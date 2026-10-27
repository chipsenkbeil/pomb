use std::error::Error;
use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::Duration;
use std::{fmt, vec};

use axum::body::{Body, Bytes, HttpBody};
use http_body::Frame;
use serde::Serialize;
use tokio::time::Instant;

use super::slot::SEND_SECS;
use crate::db::AccountDb;

/// Refusal of an export whose head could not be built, whose next task could not be read, or
/// that took longer than [`SEND_SECS`] to send.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct ExportFailed(String);

impl fmt::Display for ExportFailed {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "the export stopped: {}", self.0)
    }
}

impl Error for ExportFailed {}

/// A JSON body that ends in an array of tasks, written one task at a time as the connection takes
/// it, so the board is never held whole.
pub(super) struct TaskStream {
    /// Reads each task as its turn comes.
    account: AccountDb,
    /// The JSON before the first task, sent first.
    head: Option<Bytes>,
    /// The tasks still to send, in order.
    ids: vec::IntoIter<String>,
    /// Whether a task went out already, so the next one needs a comma.
    sent_task: bool,
    /// Whether the closing bytes went out.
    closed: bool,
    /// When the export gives up on a client that has not read it all.
    deadline: Instant,
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
        let head = serde_json::to_string(head).map_err(|error| ExportFailed(error.to_string()))?;
        let head = head
            .strip_suffix("[]}")
            .filter(|open| open.ends_with("\"tasks\":"))
            .ok_or_else(|| ExportFailed("the head must end in an empty tasks array".to_string()))?;
        Ok(Body::new(Self {
            account,
            head: Some(Bytes::from(format!("{head}["))),
            ids: ids.into_iter(),
            sent_task: false,
            closed: false,
            deadline: Instant::now() + Duration::from_secs(SEND_SECS),
        }))
    }

    /// Returns the next task as JSON, led by a comma after the first, or [`None`] once every task
    /// is sent.
    fn next_task(&mut self) -> Result<Option<Bytes>, ExportFailed> {
        for id in self.ids.by_ref() {
            let task = self
                .account
                .board_task(&id)
                .map_err(|error| ExportFailed(error.to_string()))?;
            let Some(task) = task else {
                continue;
            };
            let mut chunk = Vec::new();
            if self.sent_task {
                chunk.push(b',');
            }
            serde_json::to_writer(&mut chunk, &task)
                .map_err(|error| ExportFailed(error.to_string()))?;
            self.sent_task = true;
            return Ok(Some(Bytes::from(chunk)));
        }
        Ok(None)
    }
}

impl HttpBody for TaskStream {
    type Data = Bytes;
    type Error = ExportFailed;

    fn poll_frame(
        mut self: Pin<&mut Self>,
        _: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, ExportFailed>>> {
        if let Some(head) = self.head.take() {
            return Poll::Ready(Some(Ok(Frame::data(head))));
        }
        if self.closed {
            return Poll::Ready(None);
        }
        if Instant::now() >= self.deadline {
            self.closed = true;
            return Poll::Ready(Some(Err(ExportFailed(format!(
                "it took longer than {SEND_SECS} seconds to send"
            )))));
        }
        let frame = match self.next_task() {
            Ok(Some(task)) => Ok(Frame::data(task)),
            Ok(None) => {
                self.closed = true;
                Ok(Frame::data(Bytes::from_static(b"]}")))
            }
            Err(error) => {
                self.closed = true;
                Err(error)
            }
        };
        Poll::Ready(Some(frame))
    }

    fn is_end_stream(&self) -> bool {
        self.closed && self.head.is_none()
    }
}
