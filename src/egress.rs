//! The egress counter: how many requests each route served and how many response body bytes it
//! sent, to name in the journal the routes that send the most.
//!
//! [`count_egress`] files each request under its method and matched route pattern, and wraps the
//! response body so every chunk is counted as it is handed to the connection. A streamed export
//! or a long wait is counted as it goes, and a body the client abandons counts only what was sent.
//! [`Egress::run_report`] prints the biggest routes to stderr every [`REPORT_EVERY`].

use std::collections::HashMap;
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::task::{Context, Poll};
use std::time::Duration;

use axum::body::{Body, Bytes};
use axum::extract::{MatchedPath, Request, State};
use axum::http::Method;
use axum::middleware::Next;
use axum::response::Response;
use http_body::{Frame, SizeHint};
use tokio::time::MissedTickBehavior;

use crate::AppState;

/// How often the biggest routes are printed.
pub const REPORT_EVERY: Duration = Duration::from_secs(600);

/// How many routes each report lists.
const REPORT_ROUTES: usize = 10;

/// The methods filed under their own name, where any other method shares one label so a client
/// cannot grow the map.
const STANDARD_METHODS: [&str; 9] = [
    "GET", "POST", "PUT", "PATCH", "DELETE", "HEAD", "OPTIONS", "TRACE", "CONNECT",
];

/// What a request that matched no route is filed under.
const UNMATCHED: &str = "unmatched";

/// The requests and body bytes of one method and route pattern.
#[derive(Debug, Default)]
struct Tally {
    /// Requests that reached the route.
    requests: AtomicU64,
    /// Response body bytes handed to the connection.
    bytes: AtomicU64,
}

/// One route's totals.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RouteTotal {
    /// The method and route pattern, such as `GET /b/:board_id`.
    pub route: String,
    /// Requests that reached the route.
    pub requests: u64,
    /// Response body bytes sent.
    pub bytes: u64,
}

/// Request counts and response body bytes per method and route pattern since the server started.
#[derive(Clone, Debug, Default)]
pub struct Egress {
    /// The tally of each method and route pattern.
    routes: Arc<Mutex<HashMap<String, Arc<Tally>>>>,
}

impl Egress {
    /// Counts one request to `method` and `pattern`, or to the unmatched label when it matched no
    /// route, and returns the tally its response bytes are added to.
    fn start(&self, method: &Method, pattern: Option<&str>) -> Arc<Tally> {
        let method = if STANDARD_METHODS.contains(&method.as_str()) {
            method.as_str()
        } else {
            "OTHER"
        };
        let key = format!("{method} {}", pattern.unwrap_or(UNMATCHED));
        let tally = Arc::clone(
            self.routes
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .entry(key)
                .or_default(),
        );
        tally.requests.fetch_add(1, Ordering::Relaxed);
        tally
    }

    /// Returns the `limit` routes with the most bytes, biggest first, ties by route.
    #[must_use]
    pub fn top(&self, limit: usize) -> Vec<RouteTotal> {
        let mut totals: Vec<RouteTotal> = self
            .routes
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .iter()
            .map(|(route, tally)| RouteTotal {
                route: route.clone(),
                requests: tally.requests.load(Ordering::Relaxed),
                bytes: tally.bytes.load(Ordering::Relaxed),
            })
            .collect();
        totals.sort_by(|a, b| b.bytes.cmp(&a.bytes).then_with(|| a.route.cmp(&b.route)));
        totals.truncate(limit);
        totals
    }

    /// Prints the routes with the most bytes to stderr every [`REPORT_EVERY`] until the runtime
    /// stops, and prints nothing while no request has come.
    pub async fn run_report(self) {
        let mut ticker =
            tokio::time::interval_at(tokio::time::Instant::now() + REPORT_EVERY, REPORT_EVERY);
        ticker.set_missed_tick_behavior(MissedTickBehavior::Skip);
        loop {
            ticker.tick().await;
            let top = self.top(REPORT_ROUTES);
            if top.is_empty() {
                continue;
            }
            eprintln!("pomb: egress, the routes with the most response bytes since start:");
            for total in top {
                eprintln!(
                    "pomb: egress {} bytes in {} requests: {}",
                    total.bytes, total.requests, total.route
                );
            }
        }
    }
}

/// A response body that adds each data chunk's length to a tally as the chunk is polled out.
struct Counted {
    /// The wrapped response body.
    inner: Body,
    /// The tally that each data chunk's length is added to.
    tally: Arc<Tally>,
}

impl http_body::Body for Counted {
    type Data = Bytes;
    type Error = axum::Error;

    fn poll_frame(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Self::Data>, Self::Error>>> {
        let polled = Pin::new(&mut self.inner).poll_frame(cx);
        if let Poll::Ready(Some(Ok(frame))) = &polled
            && let Some(data) = frame.data_ref()
        {
            self.tally
                .bytes
                .fetch_add(data.len() as u64, Ordering::Relaxed);
        }
        polled
    }

    fn is_end_stream(&self) -> bool {
        self.inner.is_end_stream()
    }

    fn size_hint(&self) -> SizeHint {
        self.inner.size_hint()
    }
}

/// Files the request under its method and matched route pattern and counts the bytes of its
/// response body as they are sent.
pub async fn count_egress(State(state): State<AppState>, req: Request, next: Next) -> Response {
    let tally = state.egress.start(
        req.method(),
        req.extensions()
            .get::<MatchedPath>()
            .map(MatchedPath::as_str),
    );
    let (parts, body) = next.run(req).await.into_parts();
    Response::from_parts(parts, Body::new(Counted { inner: body, tally }))
}

#[cfg(test)]
mod tests {
    use axum::Router;
    use axum::body::to_bytes;
    use axum::http::StatusCode;
    use axum::middleware::from_fn_with_state;
    use axum::routing::get;
    use tower::ServiceExt;

    use super::*;
    use crate::auth::{AuthChoice, AuthMode, SignIn};
    use crate::db::Db;

    async fn fetch(app: &Router, uri: &str) -> usize {
        let request = Request::builder()
            .uri(uri)
            .body(Body::empty())
            .expect("request");
        let response = app.clone().oneshot(request).await.expect("oneshot");
        to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("body")
            .len()
    }

    #[tokio::test]
    async fn egress_counts_response_bytes_per_route() {
        // Serve two routes behind the counting layer
        let dir = tempfile::tempdir().expect("tempdir");
        let db = Db::connect(dir.path().join("egress.redb").to_str().expect("utf8")).expect("db");
        let sign_in =
            SignIn::new(AuthChoice::Chosen(AuthMode::None), "127.0.0.1", None).expect("sign in");
        let state = AppState::new(db, None, sign_in);
        let app = Router::new()
            .route("/small/:id", get(|| async { "x".repeat(10) }))
            .route("/big", get(|| async { "y".repeat(1000) }))
            .layer(from_fn_with_state(state.clone(), count_egress))
            .with_state(state.clone());

        // Request each route and one unmatched path
        assert_eq!(fetch(&app, "/small/a").await, 10);
        assert_eq!(fetch(&app, "/small/b").await, 10);
        assert_eq!(fetch(&app, "/big").await, 1000);
        let missing = Request::builder()
            .uri("/nope")
            .body(Body::empty())
            .expect("request");
        let answer = app.clone().oneshot(missing).await.expect("oneshot");
        assert_eq!(answer.status(), StatusCode::NOT_FOUND);
        to_bytes(answer.into_body(), usize::MAX)
            .await
            .expect("body");

        // Check the totals, biggest first
        let total = |route: &str, requests, bytes| RouteTotal {
            route: route.to_string(),
            requests,
            bytes,
        };
        assert_eq!(
            state.egress.top(10),
            vec![
                total("GET /big", 1, 1000),
                total("GET /small/:id", 2, 20),
                total("GET unmatched", 1, 0),
            ]
        );
        assert_eq!(state.egress.top(1), vec![total("GET /big", 1, 1000)]);
    }
}
