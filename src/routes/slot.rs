use std::error::Error;
use std::fmt;
use std::pin::Pin;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::task::{Context, Poll};
use std::time::Duration;

use axum::body::{Body, Bytes, HttpBody};
use axum::http::StatusCode;
use axum::response::Response;
use http_body::{Frame, SizeHint};
use tokio::io::{AsyncRead, ReadBuf};
use tokio::sync::{OwnedSemaphorePermit, oneshot};
use tokio::time::Instant;

use crate::AppState;
use crate::api_error::{ErrorCode, coded_response};
use crate::db::AccountSlot;

/// Seconds a request waits for a slot before it is refused.
pub(crate) const QUEUE_WAIT_SECS: u64 = 10;

/// Seconds a slotted response or a streamed export may take to reach the client before the server
/// gives up on it.
pub(crate) const SEND_SECS: u64 = 120;

/// Bytes of a slotted response the server hands the connection at once, 64 KiB.
const CHUNK_BYTES: usize = 65_536;

/// The kinds of request that run a few at a time, each holding a whole board in memory or
/// streaming a file. Each names the account it counts against, and holds one of that account's
/// slots and one of the server's.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub(crate) enum Slot<'a> {
    /// An attachment upload to a task of the named account, or a board import into it, which
    /// holds its slots until its file is stored.
    Upload(&'a str),
    /// An attachment download from a task of the named account, which holds its slots until its
    /// file is sent.
    Download(&'a str),
    /// A request that reads every task of a board the named account owns, such as the board page
    /// and the board archive, which holds its slots until its response is sent.
    Board(&'a str),
    /// A calendar feed of a board the named account owns, which shares the server's whole-board
    /// slots but has its own per-account share, so a slow feed reader never holds the owner's.
    Feed(&'a str),
}

impl Slot<'_> {
    /// Waits up to [`QUEUE_WAIT_SECS`] in all for every slot this kind needs, or returns the 503
    /// `busy` that refuses the request.
    pub(crate) async fn acquire(self, state: &AppState) -> Result<SlotGuard, Box<Response>> {
        let (kind, account, server, noun) = match self {
            Self::Upload(account) => (AccountSlot::Upload, account, &state.uploads, "uploads"),
            Self::Download(account) => (
                AccountSlot::Download,
                account,
                &state.downloads,
                "downloads",
            ),
            Self::Board(account) => (
                AccountSlot::Board,
                account,
                &state.boards,
                "whole-board reads",
            ),
            Self::Feed(account) => (
                AccountSlot::Feed,
                account,
                &state.boards,
                "whole-board reads",
            ),
        };
        let queues = [state.account_slot(kind, account), Arc::clone(server)];
        let deadline = Instant::now() + Duration::from_secs(QUEUE_WAIT_SECS);
        let mut permits = Vec::with_capacity(queues.len());
        for queue in queues {
            match tokio::time::timeout_at(deadline, queue.acquire_owned()).await {
                Ok(Ok(permit)) => permits.push(permit),
                _ => {
                    return Err(Box::new(coded_response(
                        StatusCode::SERVICE_UNAVAILABLE,
                        ErrorCode::Busy,
                        format!(
                            "the server is taking as many {noun} as it can, try again in a moment"
                        ),
                    )));
                }
            }
        }
        Ok(SlotGuard { _permits: permits })
    }
}

/// The slots a request holds, freed when it is dropped.
#[derive(Debug)]
pub(crate) struct SlotGuard {
    /// One permit per slot the request took.
    _permits: Vec<OwnedSemaphorePermit>,
}

/// The waits for board events a request holds, freed when it is dropped, which includes the
/// client hanging up.
#[derive(Debug)]
pub(crate) struct WaitGuard {
    /// The permit of the token, if the request uses one, and the permit of the account.
    _permits: Vec<OwnedSemaphorePermit>,
}

impl WaitGuard {
    /// Takes a wait of the account `account` and, for a request that uses an API token, of the
    /// token `token`. Returns the slot that has none left, without queueing.
    pub(crate) fn hold(
        state: &AppState,
        account: &str,
        token: Option<&str>,
    ) -> Result<Self, AccountSlot> {
        let mut queues = Vec::with_capacity(2);
        if let Some(token) = token {
            queues.push((AccountSlot::TokenWait, token));
        }
        queues.push((AccountSlot::Wait, account));
        let mut permits = Vec::with_capacity(queues.len());
        for (kind, holder) in queues {
            let permit = state
                .account_slot(kind, holder)
                .try_acquire_owned()
                .map_err(|_| kind)?;
            permits.push(permit);
        }
        Ok(Self { _permits: permits })
    }
}

/// Where a slotted response's body stands.
enum Sending {
    /// Bytes still to send, and the slot they hold.
    Data {
        /// The bytes not yet handed to the connection.
        rest: Bytes,
        /// The slot, freed with the last chunk.
        _slot: SlotGuard,
        /// Dropped with the last chunk, which ends the task that times the send.
        _sending: oneshot::Sender<()>,
    },
    /// A file still to send, and the slot it holds.
    File {
        /// The file, read from where the last chunk ended.
        file: tokio::fs::File,
        /// The bytes of the file not yet handed to the connection.
        left: u64,
        /// The slot, freed with the last chunk.
        _slot: SlotGuard,
        /// Dropped with the last chunk, which ends the task that times the send.
        _sending: oneshot::Sender<()>,
    },
    /// Every byte went out.
    Done,
    /// [`SEND_SECS`] passed first, so the rest was dropped and the slot freed.
    TimedOut,
}

/// Why a slotted response stopped before its last byte.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub(crate) enum SendFailed {
    /// The response took longer than [`SEND_SECS`].
    TimedOut,
    /// The file the response streams could not be read to its end.
    Unreadable,
}

impl fmt::Display for SendFailed {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::TimedOut => write!(
                f,
                "the response took longer than {SEND_SECS} seconds to send"
            ),
            Self::Unreadable => write!(f, "the file could not be read to its end"),
        }
    }
}

impl Error for SendFailed {}

/// A response body that holds its slot until the last byte goes out or [`SEND_SECS`] pass.
///
/// It hands the connection copies of at most [`CHUNK_BYTES`], so once the time is up and the
/// body is dropped, a client that stopped reading holds no more than the connection's buffer.
pub(crate) struct SlotBody {
    /// The bytes left and the slot, shared with the task that ends a slow send.
    state: Arc<Mutex<Sending>>,
}

impl SlotBody {
    /// Returns the body that sends `data` while it holds `slot`, and starts the clock that ends
    /// the send after [`SEND_SECS`].
    pub(crate) fn wrap(data: Bytes, slot: SlotGuard) -> Body {
        Self::start(|sending| Sending::Data {
            rest: data,
            _slot: slot,
            _sending: sending,
        })
    }

    /// Returns the body that sends the `len` bytes of `file` from where it stands while it holds
    /// `slot`, and starts the clock that ends the send after [`SEND_SECS`]. The file is read a
    /// chunk at a time as the connection takes them.
    pub(crate) fn stream(file: tokio::fs::File, len: u64, slot: SlotGuard) -> Body {
        Self::start(|sending| Sending::File {
            file,
            left: len,
            _slot: slot,
            _sending: sending,
        })
    }

    /// Returns the body that sends `begin`'s state, and starts the clock that ends the send.
    fn start(begin: impl FnOnce(oneshot::Sender<()>) -> Sending) -> Body {
        let (sending, sent) = oneshot::channel();
        let state = Arc::new(Mutex::new(begin(sending)));
        let watched = Arc::downgrade(&state);
        tokio::spawn(async move {
            if tokio::time::timeout(Duration::from_secs(SEND_SECS), sent)
                .await
                .is_ok()
            {
                return;
            }
            if let Some(state) = watched.upgrade() {
                let mut sending = lock(&state);
                if matches!(*sending, Sending::Data { .. } | Sending::File { .. }) {
                    *sending = Sending::TimedOut;
                }
            }
        });
        Body::new(Self { state })
    }
}

impl HttpBody for SlotBody {
    type Data = Bytes;
    type Error = SendFailed;

    fn poll_frame(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, SendFailed>>> {
        let mut sending = lock(&self.state);
        let frame = match &mut *sending {
            Sending::Data { rest, .. } => {
                let chunk = Bytes::copy_from_slice(&rest.split_to(rest.len().min(CHUNK_BYTES)));
                if rest.is_empty() {
                    *sending = Sending::Done;
                }
                Some(Ok(Frame::data(chunk)))
            }
            Sending::File { left: 0, .. } => {
                *sending = Sending::Done;
                None
            }
            Sending::File { file, left, .. } => {
                let mut chunk =
                    vec![0; CHUNK_BYTES.min(usize::try_from(*left).unwrap_or(CHUNK_BYTES))];
                let mut buf = ReadBuf::new(&mut chunk);
                match Pin::new(file).poll_read(cx, &mut buf) {
                    Poll::Pending => return Poll::Pending,
                    Poll::Ready(Ok(())) if !buf.filled().is_empty() => {
                        let read = buf.filled().len();
                        *left -= read as u64;
                        chunk.truncate(read);
                        if *left == 0 {
                            *sending = Sending::Done;
                        }
                        Some(Ok(Frame::data(Bytes::from(chunk))))
                    }
                    Poll::Ready(_) => {
                        *sending = Sending::Done;
                        Some(Err(SendFailed::Unreadable))
                    }
                }
            }
            Sending::Done => None,
            Sending::TimedOut => {
                *sending = Sending::Done;
                Some(Err(SendFailed::TimedOut))
            }
        };
        Poll::Ready(frame)
    }

    fn is_end_stream(&self) -> bool {
        matches!(*lock(&self.state), Sending::Done)
    }

    fn size_hint(&self) -> SizeHint {
        match &*lock(&self.state) {
            Sending::Data { rest, .. } => SizeHint::with_exact(rest.len() as u64),
            Sending::File { left, .. } => SizeHint::with_exact(*left),
            Sending::Done | Sending::TimedOut => SizeHint::with_exact(0),
        }
    }
}

/// Locks `state`, taking it over from a holder that panicked.
fn lock(state: &Mutex<Sending>) -> MutexGuard<'_, Sending> {
    state.lock().unwrap_or_else(PoisonError::into_inner)
}
