//! In-memory request limits: a token bucket per Bearer credential and a sliding window per client IP.
//!
//! [`Limits`] reads the time from a clock that production sets to `Instant::now` and tests
//! replace with a hand-driven one. State is per server instance, so a restart clears it. Idle
//! entries are dropped on a sweep, so a flood of distinct credentials or addresses cannot grow
//! the maps without bound.

use std::collections::{HashMap, VecDeque};
use std::fmt;
use std::net::{IpAddr, SocketAddr};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use axum::extract::{ConnectInfo, Request, State};
use axum::http::header::RETRY_AFTER;
use axum::http::{HeaderMap, HeaderValue, Method, StatusCode};
use axum::middleware::Next;
use axum::response::Response;

use crate::AppState;
use crate::api_error::{ErrorCode, coded_response};

/// A source of the current time.
pub type Clock = Arc<dyn Fn() -> Instant + Send + Sync>;

/// A refused request, with how long the caller should wait before trying again.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct Refusal {
    /// Whole seconds to wait, never less than one.
    pub retry_after_secs: u64,
}

impl Refusal {
    fn after(wait: Duration) -> Self {
        Self {
            retry_after_secs: wait.as_secs_f64().ceil().max(1.0) as u64,
        }
    }
}

/// Identifies the Bearer credential a request is made with, for the per-credential limit.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum CredentialKey {
    Token(String),
    Bootstrap,
}

#[derive(Debug)]
struct Bucket {
    tokens: f64,
    refilled_at: Instant,
}

#[derive(Debug)]
struct BucketMap {
    buckets: HashMap<CredentialKey, Bucket>,
    swept_at: Instant,
}

/// A token bucket per Bearer credential: [`Self::PER_SECOND`] requests a second up to
/// [`Self::BURST`] in one go.
#[derive(Clone, Debug)]
pub struct Buckets {
    state: Arc<Mutex<BucketMap>>,
}

impl Buckets {
    /// Requests a credential can make at once from a full bucket.
    pub const BURST: f64 = 100.0;
    /// Requests added back to a credential's bucket each second.
    pub const PER_SECOND: f64 = 20.0;
    const SWEEP_EVERY: Duration = Duration::from_secs(10);

    fn new(now: Instant) -> Self {
        Self {
            state: Arc::new(Mutex::new(BucketMap {
                buckets: HashMap::new(),
                swept_at: now,
            })),
        }
    }

    /// Spends one request from the credential's bucket, or says how long until one is free.
    pub fn check(&self, credential: &CredentialKey, now: Instant) -> Result<(), Refusal> {
        let mut state = lock(&self.state);
        if now.saturating_duration_since(state.swept_at) >= Self::SWEEP_EVERY {
            state
                .buckets
                .retain(|_, bucket| Self::refilled(bucket, now) < Self::BURST);
            state.swept_at = now;
        }
        let bucket = state
            .buckets
            .entry(credential.clone())
            .or_insert_with(|| Bucket {
                tokens: Self::BURST,
                refilled_at: now,
            });
        bucket.tokens = Self::refilled(bucket, now);
        bucket.refilled_at = now;
        if bucket.tokens >= 1.0 {
            bucket.tokens -= 1.0;
            return Ok(());
        }
        let missing = 1.0 - bucket.tokens;
        Err(Refusal::after(Duration::from_secs_f64(
            missing / Self::PER_SECOND,
        )))
    }

    fn refilled(bucket: &Bucket, now: Instant) -> f64 {
        let idle = now
            .saturating_duration_since(bucket.refilled_at)
            .as_secs_f64();
        (bucket.tokens + idle * Self::PER_SECOND).min(Self::BURST)
    }
}

#[derive(Debug)]
struct WindowMap {
    hits: HashMap<IpAddr, VecDeque<Instant>>,
    swept_at: Instant,
}

/// A sliding window per client IP: at most `max_hits` requests in any `window`.
#[derive(Clone, Debug)]
pub struct Window {
    max_hits: usize,
    window: Duration,
    state: Arc<Mutex<WindowMap>>,
}

impl Window {
    fn new(max_hits: usize, window: Duration, now: Instant) -> Self {
        Self {
            max_hits,
            window,
            state: Arc::new(Mutex::new(WindowMap {
                hits: HashMap::new(),
                swept_at: now,
            })),
        }
    }

    /// Records a request from `ip`, or says how long until its oldest one leaves the window.
    pub fn check(&self, ip: IpAddr, now: Instant) -> Result<(), Refusal> {
        let mut state = lock(&self.state);
        if now.saturating_duration_since(state.swept_at) >= self.window {
            state.hits.retain(|_, hits| self.prune(hits, now));
            state.swept_at = now;
        }
        let hits = state.hits.entry(ip).or_default();
        self.prune(hits, now);
        if hits.len() >= self.max_hits {
            let oldest = hits.front().copied().unwrap_or(now);
            let wait = self
                .window
                .saturating_sub(now.saturating_duration_since(oldest));
            return Err(Refusal::after(wait));
        }
        hits.push_back(now);
        Ok(())
    }

    fn prune(&self, hits: &mut VecDeque<Instant>, now: Instant) -> bool {
        while hits
            .front()
            .is_some_and(|at| now.saturating_duration_since(*at) >= self.window)
        {
            hits.pop_front();
        }
        !hits.is_empty()
    }
}

/// The per-IP limit that applies to a request, by route.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum IpScope {
    Login,
    DeviceStart,
}

impl IpScope {
    fn of(method: &Method, path: &str) -> Option<Self> {
        match (method, path) {
            (&Method::POST, "/login") => Some(Self::Login),
            (&Method::POST, "/api/v1/auth/device") => Some(Self::DeviceStart),
            _ => None,
        }
    }
}

/// Every limit the server enforces, plus the clock they share.
#[derive(Clone)]
pub struct Limits {
    credentials: Buckets,
    device_start: Window,
    login: Window,
    clock: Clock,
}

impl Limits {
    /// Requests one client IP gets on each of device start and login within [`Self::WINDOW`].
    pub const PER_IP_PER_MINUTE: usize = 10;
    /// How long a per-IP request counts against the limit.
    pub const WINDOW: Duration = Duration::from_secs(60);

    /// Builds the limits around the wall clock.
    pub fn new() -> Self {
        Self::with_clock(Arc::new(Instant::now))
    }

    /// Builds the limits around `clock`.
    pub fn with_clock(clock: Clock) -> Self {
        let now = clock();
        Self {
            credentials: Buckets::new(now),
            device_start: Window::new(Self::PER_IP_PER_MINUTE, Self::WINDOW, now),
            login: Window::new(Self::PER_IP_PER_MINUTE, Self::WINDOW, now),
            clock,
        }
    }

    /// Spends one request from the Bearer credential's bucket.
    pub fn check_credential(&self, key: &CredentialKey) -> Result<(), Refusal> {
        self.credentials.check(key, (self.clock)())
    }

    /// Records a request from `ip` against the limit for `scope`.
    pub fn check_ip(&self, scope: IpScope, ip: IpAddr) -> Result<(), Refusal> {
        let window = match scope {
            IpScope::Login => &self.login,
            IpScope::DeviceStart => &self.device_start,
        };
        window.check(ip, (self.clock)())
    }
}

impl Default for Limits {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Debug for Limits {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Limits(..)")
    }
}

/// Finds the address a request really came from. A reverse proxy on the same host reports the
/// client in the last `X-Forwarded-For` entry. From any other peer that header is client-supplied
/// and ignored. A request with no peer address shares one unspecified address.
pub fn client_ip(peer: Option<IpAddr>, headers: &HeaderMap) -> IpAddr {
    let Some(peer) = peer.map(|ip| ip.to_canonical()) else {
        return IpAddr::from([0, 0, 0, 0]);
    };
    if !peer.is_loopback() {
        return peer;
    }
    headers
        .get_all("x-forwarded-for")
        .iter()
        .filter_map(|value| value.to_str().ok())
        .filter_map(|value| value.rsplit(',').next())
        .next_back()
        .and_then(|entry| entry.trim().parse::<IpAddr>().ok())
        .map_or(peer, |ip| ip.to_canonical())
}

/// Builds the coded 429 for a refused request, tagged `rate_limited` and carrying `Retry-After`.
pub fn rate_limited_response(refusal: Refusal) -> Response {
    let seconds = refusal.retry_after_secs;
    let mut response = coded_response(
        StatusCode::TOO_MANY_REQUESTS,
        ErrorCode::RateLimited,
        format!(
            "rate limit exceeded, retry in {seconds} {}",
            if seconds == 1 { "second" } else { "seconds" }
        ),
    );
    response
        .headers_mut()
        .insert(RETRY_AFTER, HeaderValue::from(seconds));
    response
}

/// Middleware that limits `POST /login` and `POST /api/v1/auth/device` per client IP.
pub async fn ip_limits(State(state): State<AppState>, req: Request, next: Next) -> Response {
    let Some(scope) = IpScope::of(req.method(), req.uri().path()) else {
        return next.run(req).await;
    };
    let peer = req
        .extensions()
        .get::<ConnectInfo<SocketAddr>>()
        .map(|ConnectInfo(addr)| addr.ip());
    let ip = client_ip(peer, req.headers());
    match state.limits.check_ip(scope, ip) {
        Ok(()) => next.run(req).await,
        Err(refusal) => rate_limited_response(refusal),
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn secs(base: Instant, secs: u64) -> Instant {
        base + Duration::from_secs(secs)
    }

    fn token(id: &str) -> CredentialKey {
        CredentialKey::Token(id.to_string())
    }

    fn ip(text: &str) -> IpAddr {
        text.parse().expect("ip")
    }

    #[test]
    fn credential_should_refuse_the_request_after_the_burst() {
        let now = Instant::now();
        let limits = Buckets::new(now);
        for _ in 0..100 {
            assert_eq!(limits.check(&token("a"), now), Ok(()));
        }
        assert_eq!(
            limits.check(&token("a"), now),
            Err(Refusal {
                retry_after_secs: 1
            })
        );
        assert_eq!(limits.check(&token("b"), now), Ok(()));
    }

    #[test]
    fn credential_should_refill_twenty_requests_a_second() {
        let now = Instant::now();
        let limits = Buckets::new(now);
        for _ in 0..100 {
            limits.check(&token("a"), now).expect("burst");
        }
        let later = now + Duration::from_millis(500);
        for _ in 0..10 {
            assert_eq!(limits.check(&token("a"), later), Ok(()));
        }
        assert!(limits.check(&token("a"), later).is_err());
        assert_eq!(limits.check(&token("a"), secs(later, 5)), Ok(()));
    }

    #[test]
    fn credential_should_drop_idle_buckets_on_a_sweep() {
        let now = Instant::now();
        let limits = Buckets::new(now);
        for n in 0..50 {
            limits.check(&token(&n.to_string()), now).expect("first");
        }
        limits
            .check(&CredentialKey::Bootstrap, secs(now, 20))
            .expect("sweeps");
        assert_eq!(lock(&limits.state).buckets.len(), 1);
    }

    #[test]
    fn window_should_refuse_the_eleventh_request_in_a_minute() {
        let now = Instant::now();
        let window = Window::new(10, Duration::from_secs(60), now);
        for n in 0..10 {
            assert_eq!(window.check(ip("203.0.113.9"), secs(now, n)), Ok(()));
        }
        assert_eq!(
            window.check(ip("203.0.113.9"), secs(now, 10)),
            Err(Refusal {
                retry_after_secs: 50
            })
        );
        assert_eq!(window.check(ip("203.0.113.10"), secs(now, 10)), Ok(()));
        assert_eq!(window.check(ip("203.0.113.9"), secs(now, 60)), Ok(()));
    }

    #[test]
    fn window_should_drop_idle_addresses_on_a_sweep() {
        let now = Instant::now();
        let window = Window::new(10, Duration::from_secs(60), now);
        for n in 0..200u8 {
            window
                .check(IpAddr::from([10, 0, 0, n]), now)
                .expect("first");
        }
        window
            .check(ip("203.0.113.9"), secs(now, 61))
            .expect("sweeps");
        assert_eq!(lock(&window.state).hits.len(), 1);
    }

    #[tokio::test]
    async fn rate_limited_response_should_pluralize_the_wait() {
        let message = |retry_after_secs| async move {
            let response = rate_limited_response(Refusal { retry_after_secs });
            let bytes = axum::body::to_bytes(response.into_body(), 1024)
                .await
                .expect("body");
            String::from_utf8(bytes.to_vec()).expect("utf8")
        };
        assert_eq!(message(1).await, "rate limit exceeded, retry in 1 second");
        assert_eq!(message(2).await, "rate limit exceeded, retry in 2 seconds");
    }

    #[test]
    fn client_ip_should_trust_forwarded_for_only_from_a_loopback_peer() {
        let mut headers = HeaderMap::new();
        headers.insert(
            "x-forwarded-for",
            "1.1.1.1, 198.51.100.7".parse().expect("value"),
        );
        assert_eq!(
            client_ip(Some(ip("127.0.0.1")), &headers),
            ip("198.51.100.7")
        );
        assert_eq!(client_ip(Some(ip("::1")), &headers), ip("198.51.100.7"));
        assert_eq!(
            client_ip(Some(ip("::ffff:127.0.0.1")), &headers),
            ip("198.51.100.7")
        );
        assert_eq!(
            client_ip(Some(ip("203.0.113.9")), &headers),
            ip("203.0.113.9")
        );
        assert_eq!(
            client_ip(Some(ip("127.0.0.1")), &HeaderMap::new()),
            ip("127.0.0.1")
        );
        headers.insert("x-forwarded-for", "garbage".parse().expect("value"));
        assert_eq!(client_ip(Some(ip("127.0.0.1")), &headers), ip("127.0.0.1"));
    }
}
