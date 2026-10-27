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
use tokio::sync::{OwnedSemaphorePermit, oneshot};
use tokio::time::Instant;

use crate::AppState;
use crate::api_error::{ErrorCode, coded_response};
use crate::db::AccountSlot;

/// Seconds a request waits for a slot before it is refused.
pub(super) const QUEUE_WAIT_SECS: u64 = 10;

/// Seconds a slotted response or a streamed export may take to reach the client before the server
/// gives up on it.
pub(super) const SEND_SECS: u64 = 120;

/// Bytes of a slotted response the server hands the connection at once, 64 KiB.
const CHUNK_BYTES: usize = 65_536;

/// The kinds of request that run a few at a time, each holding a whole file or board in memory.
/// Each names the account it counts against, and holds one of that account's slots and one of the
/// server's.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub(super) enum Slot<'a> {
    /// An attachment upload to a task of the named account, which holds its slots until its file
    /// is stored.
    Upload(&'a str),
    /// An attachment download from a task of the named account, which holds its slots until its
    /// file is sent.
    Download(&'a str),
    /// A request that reads every task of a board the named account owns, which holds its slots
    /// until its response is sent.
    Board(&'a str),
    /// A calendar feed of a board the named account owns, which shares the server's whole-board
    /// slots but has its own per-account share, so a slow feed reader never holds the owner's.
    Feed(&'a str),
}

impl Slot<'_> {
    /// Waits up to [`QUEUE_WAIT_SECS`] in all for every slot this kind needs, or returns the 503
    /// `busy` that refuses the request.
    pub(super) async fn acquire(self, state: &AppState) -> Result<SlotGuard, Response> {
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
                    return Err(coded_response(
                        StatusCode::SERVICE_UNAVAILABLE,
                        ErrorCode::Busy,
                        format!(
                            "the server is taking as many {noun} as it can, try again in a moment"
                        ),
                    ));
                }
            }
        }
        Ok(SlotGuard { _permits: permits })
    }
}

/// The slots a request holds, freed when it is dropped.
#[derive(Debug)]
pub(super) struct SlotGuard {
    /// One permit per slot the request took.
    _permits: Vec<OwnedSemaphorePermit>,
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
    /// Every byte went out.
    Done,
    /// [`SEND_SECS`] passed first, so the rest was dropped and the slot freed.
    TimedOut,
}

/// Refusal to keep sending a response that took longer than [`SEND_SECS`].
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub(super) struct SendTimedOut;

impl fmt::Display for SendTimedOut {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "the response took longer than {SEND_SECS} seconds to send"
        )
    }
}

impl Error for SendTimedOut {}

/// A response body that holds its slot until the last byte goes out or [`SEND_SECS`] pass.
///
/// It hands the connection copies of at most [`CHUNK_BYTES`], so once the time is up and the
/// body is dropped, a client that stopped reading holds no more than the connection's buffer.
pub(super) struct SlotBody {
    /// The bytes left and the slot, shared with the task that ends a slow send.
    state: Arc<Mutex<Sending>>,
}

impl SlotBody {
    /// Returns the body that sends `data` while it holds `slot`, and starts the clock that ends
    /// the send after [`SEND_SECS`].
    pub(super) fn wrap(data: Bytes, slot: SlotGuard) -> Body {
        let (sending, sent) = oneshot::channel();
        let state = Arc::new(Mutex::new(Sending::Data {
            rest: data,
            _slot: slot,
            _sending: sending,
        }));
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
                if matches!(*sending, Sending::Data { .. }) {
                    *sending = Sending::TimedOut;
                }
            }
        });
        Body::new(Self { state })
    }
}

impl HttpBody for SlotBody {
    type Data = Bytes;
    type Error = SendTimedOut;

    fn poll_frame(
        self: Pin<&mut Self>,
        _: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, SendTimedOut>>> {
        let mut sending = lock(&self.state);
        let frame = match &mut *sending {
            Sending::Data { rest, .. } => {
                let chunk = Bytes::copy_from_slice(&rest.split_to(rest.len().min(CHUNK_BYTES)));
                if rest.is_empty() {
                    *sending = Sending::Done;
                }
                Some(Ok(Frame::data(chunk)))
            }
            Sending::Done => None,
            Sending::TimedOut => {
                *sending = Sending::Done;
                Some(Err(SendTimedOut))
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
            Sending::Done | Sending::TimedOut => SizeHint::with_exact(0),
        }
    }
}

/// Locks `state`, taking it over from a holder that panicked.
fn lock(state: &Mutex<Sending>) -> MutexGuard<'_, Sending> {
    state.lock().unwrap_or_else(PoisonError::into_inner)
}
