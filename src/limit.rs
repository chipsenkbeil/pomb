//! In-memory request limits: a token bucket per Bearer credential, one per account shared by its
//! sessions and tokens, and a sliding window per client IP.
//!
//! [`Limits`] reads the time from a clock that production sets to `Instant::now` and tests
//! replace with a hand-driven one. Its numbers come from the instance's [`RateLimits`], swapped
//! live when the admin saves them. State is per server instance, so a restart clears it. Idle
//! entries are dropped on a sweep, so a flood of distinct credentials or addresses cannot grow
//! the maps without bound.

use std::collections::{HashMap, VecDeque};
use std::fmt;
use std::net::{IpAddr, Ipv6Addr, SocketAddr};
use std::sync::{Arc, Mutex, MutexGuard, RwLock};
use std::time::{Duration, Instant};

use axum::extract::{ConnectInfo, Request, State};
use axum::http::header::RETRY_AFTER;
use axum::http::{HeaderMap, HeaderValue, Method, StatusCode};
use axum::middleware::Next;
use axum::response::Response;

use crate::AppState;
use crate::api_error::{ErrorCode, coded_response};
use crate::models::{AccountId, RateLimits};

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

/// Identifies what a request is counted against: the Bearer credential it is made with, or the
/// account its sessions and tokens share.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum CredentialKey {
    Token(String),
    Bootstrap,
    /// Every session and token of one account.
    Account(String),
}

/// How fast one bucket refills, and how much it holds.
#[derive(Copy, Clone, Debug, PartialEq)]
pub struct Rate {
    /// Requests a full bucket allows at once.
    pub burst: f64,
    /// Requests added back each second.
    pub per_second: f64,
}

impl Rate {
    /// Returns the rate of a bucket that holds `burst` and refills `per_second`.
    #[must_use]
    pub fn new(burst: u32, per_second: u32) -> Self {
        Self {
            burst: f64::from(burst),
            per_second: f64::from(per_second),
        }
    }
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

/// A token bucket per key, each refilling at the [`Rate`] its check is given.
#[derive(Clone, Debug)]
pub struct Buckets {
    state: Arc<Mutex<BucketMap>>,
}

impl Buckets {
    const SWEEP_EVERY: Duration = Duration::from_secs(10);

    fn new(now: Instant) -> Self {
        Self {
            state: Arc::new(Mutex::new(BucketMap {
                buckets: HashMap::new(),
                swept_at: now,
            })),
        }
    }

    /// Spends one request from the key's bucket, or says how long until one is free.
    pub fn check(&self, key: &CredentialKey, now: Instant, rate: Rate) -> Result<(), Refusal> {
        let mut state = lock(&self.state);
        if now.saturating_duration_since(state.swept_at) >= Self::SWEEP_EVERY {
            state
                .buckets
                .retain(|_, bucket| Self::refilled(bucket, now, rate) < rate.burst);
            state.swept_at = now;
        }
        let bucket = state.buckets.entry(key.clone()).or_insert_with(|| Bucket {
            tokens: rate.burst,
            refilled_at: now,
        });
        bucket.tokens = Self::refilled(bucket, now, rate);
        bucket.refilled_at = now;
        if bucket.tokens >= 1.0 {
            bucket.tokens -= 1.0;
            return Ok(());
        }
        let missing = 1.0 - bucket.tokens;
        Err(Refusal::after(Duration::from_secs_f64(
            missing / rate.per_second,
        )))
    }

    fn refilled(bucket: &Bucket, now: Instant, rate: Rate) -> f64 {
        let idle = now
            .saturating_duration_since(bucket.refilled_at)
            .as_secs_f64();
        (bucket.tokens + idle * rate.per_second).min(rate.burst)
    }
}

#[derive(Debug)]
struct WindowMap {
    hits: HashMap<IpAddr, VecDeque<Instant>>,
    swept_at: Instant,
}

/// Leading bits of an IPv6 address that name one client: a /64 is the smallest network a site
/// is routinely given, so its addresses share one window.
const IPV6_CLIENT_PREFIX_BITS: u32 = 64;

/// Returns the network a client address counts against: an IPv4 address (mapped into IPv6 or
/// not) as itself, and an IPv6 address as its /64.
fn client_network(ip: IpAddr) -> IpAddr {
    match ip {
        IpAddr::V4(_) => ip,
        IpAddr::V6(v6) => match v6.to_ipv4_mapped() {
            Some(v4) => IpAddr::V4(v4),
            None => {
                let mask = u128::MAX << (128 - IPV6_CLIENT_PREFIX_BITS);
                IpAddr::V6(Ipv6Addr::from(u128::from(v6) & mask))
            }
        },
    }
}

/// A sliding window per client network: at most `max_hits` requests in any `window`. An IPv6
/// client counts by its /64.
#[derive(Clone, Debug)]
pub struct Window {
    window: Duration,
    state: Arc<Mutex<WindowMap>>,
}

impl Window {
    fn new(window: Duration, now: Instant) -> Self {
        Self {
            window,
            state: Arc::new(Mutex::new(WindowMap {
                hits: HashMap::new(),
                swept_at: now,
            })),
        }
    }

    /// Records a request from `ip`, or says how long until its oldest one leaves the window when
    /// it already holds `max_hits`.
    pub fn check(&self, ip: IpAddr, now: Instant, max_hits: usize) -> Result<(), Refusal> {
        let mut state = lock(&self.state);
        if now.saturating_duration_since(state.swept_at) >= self.window {
            state.hits.retain(|_, hits| self.prune(hits, now));
            state.swept_at = now;
        }
        let hits = state.hits.entry(client_network(ip)).or_default();
        self.prune(hits, now);
        if hits.len() >= max_hits {
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
    /// Registering an account through an invite.
    Register,
    /// Recovering an account with its recovery code.
    Recover,
}

impl IpScope {
    fn of(method: &Method, path: &str) -> Option<Self> {
        match (method, path) {
            (
                &Method::POST,
                "/api/v1/auth/sign-in/options"
                | "/api/v1/auth/sign-in"
                | "/api/v1/auth/setup/options"
                | "/api/v1/auth/setup",
            ) => Some(Self::Login),
            (&Method::POST, "/api/v1/auth/device") => Some(Self::DeviceStart),
            (&Method::POST, "/api/v1/auth/register/options" | "/api/v1/auth/register") => {
                Some(Self::Register)
            }
            (&Method::POST, "/api/v1/auth/recover/options" | "/api/v1/auth/recover") => {
                Some(Self::Recover)
            }
            _ => None,
        }
    }
}

/// Every limit the server enforces, the numbers they follow and the clock they share.
#[derive(Clone)]
pub struct Limits {
    credentials: Buckets,
    accounts: Buckets,
    device_start: Window,
    login: Window,
    register: Window,
    recover: Window,
    settings: Arc<RwLock<RateLimits>>,
    clock: Clock,
}

impl Limits {
    /// How long a per-IP request counts against the limit.
    pub const WINDOW: Duration = Duration::from_secs(60);

    /// Builds the default limits around the wall clock.
    pub fn new() -> Self {
        Self::with_clock(Arc::new(Instant::now))
    }

    /// Builds the default limits around `clock`.
    pub fn with_clock(clock: Clock) -> Self {
        let now = clock();
        Self {
            credentials: Buckets::new(now),
            accounts: Buckets::new(now),
            device_start: Window::new(Self::WINDOW, now),
            login: Window::new(Self::WINDOW, now),
            register: Window::new(Self::WINDOW, now),
            recover: Window::new(Self::WINDOW, now),
            settings: Arc::new(RwLock::new(RateLimits::default())),
            clock,
        }
    }

    /// Returns the numbers the limits follow.
    #[must_use]
    pub fn settings(&self) -> RateLimits {
        *self
            .settings
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Makes every clone of these limits follow `settings` from the next request on.
    pub fn apply(&self, settings: &RateLimits) {
        *self
            .settings
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = *settings;
    }

    /// Spends one request from the Bearer credential's bucket.
    pub fn check_credential(&self, key: &CredentialKey) -> Result<(), Refusal> {
        let settings = self.settings();
        let rate = Rate::new(settings.token_burst, settings.token_per_second);
        self.credentials.check(key, (self.clock)(), rate)
    }

    /// Spends one request from the bucket `account`'s sessions and tokens share.
    pub fn check_account(&self, account: &AccountId) -> Result<(), Refusal> {
        let settings = self.settings();
        let rate = Rate::new(settings.account_burst, settings.account_per_second);
        let key = CredentialKey::Account(account.as_str().to_string());
        self.accounts.check(&key, (self.clock)(), rate)
    }

    /// Records a request from `ip` against the limit for `scope`.
    pub fn check_ip(&self, scope: IpScope, ip: IpAddr) -> Result<(), Refusal> {
        let window = match scope {
            IpScope::Login => &self.login,
            IpScope::DeviceStart => &self.device_start,
            IpScope::Register => &self.register,
            IpScope::Recover => &self.recover,
        };
        let max_hits = self.settings().ip_per_minute as usize;
        window.check(ip, (self.clock)(), max_hits)
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

/// Middleware that limits the passkey sign-in, setup, registration and recovery endpoints and
/// `POST /api/v1/auth/device` per client IP.
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

    /// Requests a full token bucket allows by default.
    const BURST: usize = RateLimits::DEFAULT_TOKEN_BURST as usize;
    /// Requests one address gets per window by default.
    const PER_IP: usize = RateLimits::DEFAULT_IP_PER_MINUTE as usize;
    /// A window that takes one request per address.
    const ONE_HIT: usize = 1;
    /// Seconds a per-IP request counts.
    const WINDOW_SECS: u64 = Limits::WINDOW.as_secs();

    fn secs(base: Instant, secs: u64) -> Instant {
        base + Duration::from_secs(secs)
    }

    fn token(id: &str) -> CredentialKey {
        CredentialKey::Token(id.to_string())
    }

    fn rate() -> Rate {
        Rate::new(
            RateLimits::DEFAULT_TOKEN_BURST,
            RateLimits::DEFAULT_TOKEN_PER_SECOND,
        )
    }

    fn ip(text: &str) -> IpAddr {
        text.parse().expect("ip")
    }

    #[test]
    fn credential_should_refuse_the_request_after_the_burst() {
        let now = Instant::now();
        let limits = Buckets::new(now);
        for _ in 0..BURST {
            assert_eq!(limits.check(&token("a"), now, rate()), Ok(()));
        }
        assert_eq!(
            limits.check(&token("a"), now, rate()),
            Err(Refusal {
                retry_after_secs: 1
            })
        );
        assert_eq!(limits.check(&token("b"), now, rate()), Ok(()));
    }

    #[test]
    fn credential_should_refill_twenty_requests_a_second() {
        let now = Instant::now();
        let limits = Buckets::new(now);
        for _ in 0..BURST {
            limits.check(&token("a"), now, rate()).expect("burst");
        }
        let later = now + Duration::from_millis(500);
        for _ in 0..10 {
            assert_eq!(limits.check(&token("a"), later, rate()), Ok(()));
        }
        assert!(limits.check(&token("a"), later, rate()).is_err());
        assert_eq!(limits.check(&token("a"), secs(later, 5), rate()), Ok(()));
    }

    #[test]
    fn credential_should_drop_idle_buckets_on_a_sweep() {
        let now = Instant::now();
        let limits = Buckets::new(now);
        for n in 0..50 {
            limits
                .check(&token(&n.to_string()), now, rate())
                .expect("first");
        }
        limits
            .check(&CredentialKey::Bootstrap, secs(now, 20), rate())
            .expect("sweeps");
        assert_eq!(lock(&limits.state).buckets.len(), 1);
    }

    #[test]
    fn window_should_refuse_the_eleventh_request_in_a_minute() {
        let now = Instant::now();
        let window = Window::new(Limits::WINDOW, now);
        for n in 0..PER_IP as u64 {
            assert_eq!(
                window.check(ip("203.0.113.9"), secs(now, n), PER_IP),
                Ok(())
            );
        }
        assert_eq!(
            window.check(ip("203.0.113.9"), secs(now, PER_IP as u64), PER_IP),
            Err(Refusal {
                retry_after_secs: WINDOW_SECS - PER_IP as u64
            })
        );
        assert_eq!(
            window.check(ip("203.0.113.10"), secs(now, PER_IP as u64), PER_IP),
            Ok(())
        );
        assert_eq!(
            window.check(ip("203.0.113.9"), secs(now, WINDOW_SECS), PER_IP),
            Ok(())
        );
    }

    #[test]
    fn window_should_count_an_ipv6_client_by_its_slash_64() {
        let now = Instant::now();
        let window = Window::new(Limits::WINDOW, now);
        for n in 0..PER_IP as u16 {
            let rotated = IpAddr::from([0x2001, 0xdb8, 0, 1, n, n, n, n]);
            assert_eq!(window.check(rotated, now, PER_IP), Ok(()));
        }
        assert!(
            window
                .check(ip("2001:db8:0:1:ffff::1"), now, PER_IP)
                .is_err()
        );
        assert_eq!(window.check(ip("2001:db8:0:2::1"), now, PER_IP), Ok(()));
        let single = Window::new(Limits::WINDOW, now);
        assert_eq!(single.check(ip("::ffff:203.0.113.9"), now, ONE_HIT), Ok(()));
        assert!(
            single.check(ip("203.0.113.9"), now, ONE_HIT).is_err(),
            "a mapped address counts as its IPv4 address"
        );
        assert_eq!(single.check(ip("203.0.113.10"), now, ONE_HIT), Ok(()));
    }

    #[test]
    fn window_should_drop_idle_addresses_on_a_sweep() {
        let now = Instant::now();
        let window = Window::new(Limits::WINDOW, now);
        for n in 0..200u8 {
            window
                .check(IpAddr::from([10, 0, 0, n]), now, PER_IP)
                .expect("first");
        }
        window
            .check(ip("203.0.113.9"), secs(now, WINDOW_SECS + 1), PER_IP)
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
