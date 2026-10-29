use std::error::Error;
use std::fmt;
use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::Duration;

use axum::body::{Body, Bytes, HttpBody};
use http_body::Frame;
use serde::Serialize;
use tokio::time::Instant;

use super::slot::{SEND_SECS, SlotGuard};
use crate::archive::Archive;
use crate::db::AccountDb;
use crate::taskjson::TaskArray;

/// Refusal of an export whose head could not be built, whose next piece could not be read, or
/// that took longer than [`SEND_SECS`] to send.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct ExportFailed(String);

impl fmt::Display for ExportFailed {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "the export stopped: {}", self.0)
    }
}

impl Error for ExportFailed {}

/// The content of an export, produced one piece at a time as the connection takes it.
pub(super) trait Pieces: Send {
    /// Returns the next piece, or [`None`] once every piece is out.
    fn next_piece(&mut self) -> Result<Option<Bytes>, ExportFailed>;
}

/// A body that sends the pieces of an export, holds its slot until the last goes out, and fails
/// once [`SEND_SECS`] pass.
pub(super) struct Streamed {
    /// Produces each piece as its turn comes.
    pieces: Box<dyn Pieces>,
    /// The slot the export holds, freed once the body ends.
    _slot: Option<SlotGuard>,
    /// Whether the last piece went out or a piece failed.
    closed: bool,
    /// When the export gives up on a client that has not read it all.
    deadline: Instant,
}

impl Streamed {
    /// Returns the body that sends `pieces` while it holds `slot`, if any.
    pub(super) fn body(pieces: impl Pieces + 'static, slot: Option<SlotGuard>) -> Body {
        Body::new(Self {
            pieces: Box::new(pieces),
            _slot: slot,
            closed: false,
            deadline: Instant::now() + Duration::from_secs(SEND_SECS),
        })
    }

    /// Ends the body and frees its slot.
    fn close(&mut self) {
        self.closed = true;
        self._slot = None;
    }
}

impl HttpBody for Streamed {
    type Data = Bytes;
    type Error = ExportFailed;

    fn poll_frame(
        mut self: Pin<&mut Self>,
        _: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, ExportFailed>>> {
        if self.closed {
            return Poll::Ready(None);
        }
        if Instant::now() >= self.deadline {
            self.close();
            return Poll::Ready(Some(Err(ExportFailed(format!(
                "it took longer than {SEND_SECS} seconds to send"
            )))));
        }
        let frame = match self.pieces.next_piece() {
            Ok(Some(piece)) => Some(Ok(Frame::data(piece))),
            Ok(None) => {
                self.close();
                None
            }
            Err(error) => {
                self.close();
                Some(Err(error))
            }
        };
        Poll::Ready(frame)
    }

    fn is_end_stream(&self) -> bool {
        self.closed
    }
}

/// A JSON body that ends in an array of tasks, written one task at a time as the connection takes
/// it, so the board is never held whole.
pub(super) struct TaskStream {
    /// Reads each task as its turn comes.
    account: AccountDb,
    /// The JSON of the head, the tasks and the closing bytes.
    array: TaskArray,
}

impl TaskStream {
    /// Returns the body that sends `head`, serialized with an empty `tasks` array as its last
    /// field, then the tasks `ids` names, read through `account` and skipped once gone. The send
    /// fails once [`SEND_SECS`] pass.
    ///
    /// # Errors
    ///
    /// Returns [`ExportFailed`] when `head` does not serialize to an object that ends in
    /// `"tasks":[]`.
    pub(super) fn wrap(
        head: &impl Serialize,
        account: AccountDb,
        ids: Vec<String>,
    ) -> Result<Body, ExportFailed> {
        let array = TaskArray::new(head, ids).map_err(|error| ExportFailed(error.to_string()))?;
        Ok(Streamed::body(Self { account, array }, None))
    }
}

impl Pieces for TaskStream {
    fn next_piece(&mut self) -> Result<Option<Bytes>, ExportFailed> {
        let Self { account, array } = self;
        array
            .next_piece(|id| account.board_task(id))
            .map(|piece| piece.map(Bytes::from))
            .map_err(|error| ExportFailed(error.to_string()))
    }
}

/// A board archive, sent as the gzipped chunks it produces.
pub(super) struct ArchiveStream {
    /// Produces each chunk as its turn comes.
    archive: Archive,
}

impl ArchiveStream {
    /// Returns the body that sends `archive` while it holds `slot`. The send fails once
    /// [`SEND_SECS`] pass.
    pub(super) fn wrap(archive: Archive, slot: SlotGuard) -> Body {
        Streamed::body(Self { archive }, Some(slot))
    }
}

impl Pieces for ArchiveStream {
    fn next_piece(&mut self) -> Result<Option<Bytes>, ExportFailed> {
        self.archive
            .next_chunk()
            .map(|chunk| chunk.map(Bytes::from))
            .map_err(|error| ExportFailed(error.to_string()))
    }
}
