//! Webhook delivery. `Dispatcher` claims queued events from the outbox on a fixed tick and posts
//! each stored body on a blocking thread, signed with `X-Pomb-Signature` (HMAC-SHA256 keyed by the
//! webhook's secret). A webhook has at most one delivery in flight and the whole server at most
//! four. The address guard resolves the host of every try and refuses it when any address is
//! loopback, private, link-local or otherwise non-public, and passes only the vetted addresses
//! to the connector. A failed event is retried twice, after 5 and then 30 seconds, and is then
//! dropped. A claim holds an event for a lease that outlasts one try, so a second claim skips it.
//! `Prober` runs the browser-session host and request checks of the settings screen.

use std::collections::HashSet;
use std::fmt::{self, Write as _};
use std::io::{self, Read};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, ToSocketAddrs};
use std::sync::{Arc, Mutex, PoisonError, mpsc};
use std::thread;
use std::time::Duration;

use chrono::{TimeDelta, Utc};
use hmac::{Hmac, Mac};
use sha2::Sha256;
use tokio::sync::Semaphore;
use tokio::time::MissedTickBehavior;
use ureq::Agent;
use ureq::http::Uri;
use ureq::tls::{RootCerts, TlsConfig, TlsProvider};
use ureq::unversioned::resolver::{ResolvedSocketAddrs, Resolver};
use ureq::unversioned::transport::{DefaultConnector, NextTimeout};

use crate::db::{Db, DbResult, Delivery};

/// Most deliveries on the wire at once.
const MAX_IN_FLIGHT: usize = 4;

/// Most host lookups and test requests at once.
const MAX_PROBES: usize = 2;

/// Retries an event gets after its first failed try.
const RETRIES: usize = 2;

/// Tries an event gets in all, the first plus the retries.
const MAX_ATTEMPTS: u32 = RETRIES as u32 + 1;

/// Most response bytes read before the connection is dropped.
const MAX_RESPONSE_BYTES: u64 = 4096;

/// Most addresses one resolution keeps.
const MAX_ADDRESSES: usize = 16;

/// Time past a try's timeout that its claim holds the event, so a crash gives it back.
const LEASE_MARGIN: Duration = Duration::from_secs(5);

/// Prefix of the signature header's value.
const SIGNATURE_PREFIX: &str = "sha256=";

/// Whether an address may be called, where false refuses it.
pub type AddressPolicy = fn(IpAddr) -> bool;

/// Looks a host name up and returns every address it names, with the port set.
pub type Lookup = Arc<dyn Fn(&str, u16) -> io::Result<Vec<SocketAddr>> + Send + Sync>;

/// Address blocks no webhook may call, as the first address and the prefix length.
const REFUSED_V4: [([u8; 4], u32); 14] = [
    ([0, 0, 0, 0], 8),
    ([10, 0, 0, 0], 8),
    ([100, 64, 0, 0], 10),
    ([127, 0, 0, 0], 8),
    ([169, 254, 0, 0], 16),
    ([172, 16, 0, 0], 12),
    ([192, 0, 0, 0], 24),
    ([192, 0, 2, 0], 24),
    ([192, 168, 0, 0], 16),
    ([198, 18, 0, 0], 15),
    ([198, 51, 100, 0], 24),
    ([203, 0, 113, 0], 24),
    ([224, 0, 0, 0], 4),
    ([240, 0, 0, 0], 4),
];

/// IPv6 blocks no webhook may call, as the first address and the prefix length.
const REFUSED_V6: [([u16; 8], u32); 12] = [
    ([0, 0, 0, 0, 0, 0, 0, 0], 128),
    ([0, 0, 0, 0, 0, 0, 0, 1], 128),
    ([0x64, 0xff9b, 1, 0, 0, 0, 0, 0], 48),
    ([0x100, 0, 0, 0, 0, 0, 0, 0], 64),
    ([0x2001, 0, 0, 0, 0, 0, 0, 0], 32),
    ([0x2001, 2, 0, 0, 0, 0, 0, 0], 48),
    ([0x2001, 0xdb8, 0, 0, 0, 0, 0, 0], 32),
    ([0x3fff, 0, 0, 0, 0, 0, 0, 0], 20),
    ([0xfc00, 0, 0, 0, 0, 0, 0, 0], 7),
    ([0xfe80, 0, 0, 0, 0, 0, 0, 0], 10),
    ([0xfec0, 0, 0, 0, 0, 0, 0, 0], 10),
    ([0xff00, 0, 0, 0, 0, 0, 0, 0], 8),
];

/// Returns whether the first `prefix` bits of `ip` and `net` agree.
#[must_use]
fn in_v4(ip: Ipv4Addr, net: [u8; 4], prefix: u32) -> bool {
    let mask = u32::MAX.checked_shl(32 - prefix).unwrap_or(0);
    u32::from(ip) & mask == u32::from(Ipv4Addr::from(net)) & mask
}

/// Returns whether the first `prefix` bits of `ip` and `net` agree.
#[must_use]
fn in_v6(ip: Ipv6Addr, net: [u16; 8], prefix: u32) -> bool {
    let mask = u128::MAX.checked_shl(128 - prefix).unwrap_or(0);
    u128::from(ip) & mask == u128::from(Ipv6Addr::from(net)) & mask
}

/// Returns the IPv4 address an IPv6 address carries as an IPv4-compatible, IPv4-mapped, SIIT,
/// NAT64 or 6to4 address.
#[must_use]
fn embedded_v4(ip: Ipv6Addr) -> Option<Ipv4Addr> {
    let s = ip.segments();
    let [a, b] = s[6].to_be_bytes();
    let [c, d] = s[7].to_be_bytes();
    let low = Ipv4Addr::new(a, b, c, d);
    let zeros = |range: &[u16]| range.iter().all(|&segment| segment == 0);
    if zeros(&s[..5]) && (s[5] == 0 || s[5] == 0xffff) {
        return Some(low);
    }
    if zeros(&s[..4]) && s[4] == 0xffff && s[5] == 0 {
        return Some(low);
    }
    if s[0] == 0x64 && s[1] == 0xff9b && zeros(&s[2..6]) {
        return Some(low);
    }
    if s[0] == 0x2002 {
        let [a, b] = s[1].to_be_bytes();
        let [c, d] = s[2].to_be_bytes();
        return Some(Ipv4Addr::new(a, b, c, d));
    }
    None
}

/// Returns whether a webhook may call `ip`, which is false for loopback, unspecified, link-local,
/// private, shared, benchmarking, documentation, multicast and reserved addresses, and for an
/// IPv6 address that carries one of them as an IPv4 address.
#[must_use]
pub fn is_public_address(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(ip) => !REFUSED_V4
            .iter()
            .any(|&(net, prefix)| in_v4(ip, net, prefix)),
        IpAddr::V6(ip) => {
            if let Some(inner) = embedded_v4(ip) {
                return is_public_address(IpAddr::V4(inner));
            }
            !REFUSED_V6
                .iter()
                .any(|&(net, prefix)| in_v6(ip, net, prefix))
        }
    }
}

/// Returns whether a webhook may name `host`, as a URL spells it, which is false for `localhost`,
/// any name under `.localhost` and an IP address that [`is_public_address`] refuses.
#[must_use]
pub fn is_public_host(host: &str) -> bool {
    let name = host.trim_end_matches('.').to_ascii_lowercase();
    if name == "localhost" || name.ends_with(".localhost") {
        return false;
    }
    let literal = name.trim_start_matches('[').trim_end_matches(']');
    literal.parse::<IpAddr>().map_or(true, is_public_address)
}

/// Returns the lowercase hex HMAC-SHA256 of `body` keyed by the bytes of `secret`.
#[must_use]
fn sign(secret: &str, body: &[u8]) -> String {
    let mut mac = Hmac::<Sha256>::new_from_slice(secret.as_bytes())
        .expect("an HMAC accepts a key of any length");
    mac.update(body);
    mac.finalize()
        .into_bytes()
        .iter()
        .fold(String::new(), |mut hex, byte| {
            let _ = write!(hex, "{byte:02x}");
            hex
        })
}

/// The timing of a worker.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct Schedule {
    /// How often the worker looks for due events.
    pub tick: Duration,
    /// The longest one try may take in all, from connecting to the last response byte.
    pub timeout: Duration,
    /// How long after the first and the second failure the next try waits.
    pub backoff: [Duration; RETRIES],
}

impl Default for Schedule {
    fn default() -> Self {
        Self {
            tick: Duration::from_secs(1),
            timeout: Duration::from_secs(10),
            backoff: [Duration::from_secs(5), Duration::from_secs(30)],
        }
    }
}

impl Schedule {
    /// Returns how long a claim holds an event.
    #[must_use]
    fn lease(&self) -> TimeDelta {
        TimeDelta::from_std(self.timeout + LEASE_MARGIN).unwrap_or(TimeDelta::MAX)
    }
}

/// The error a resolver returns for an address the policy refuses.
#[derive(Debug)]
struct Refused;

impl fmt::Display for Refused {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("the callback address is not public")
    }
}

impl std::error::Error for Refused {}

/// Returns the lookup that asks the system resolver.
fn system_lookup() -> Lookup {
    Arc::new(|host, port| Ok((host, port).to_socket_addrs()?.collect()))
}

/// The resolver of the delivery agent: it looks a host up and keeps only a set of addresses that
/// all pass the policy.
struct Vetting {
    /// Looks a host up.
    lookup: Lookup,
    /// Decides which addresses may be called.
    policy: AddressPolicy,
}

impl fmt::Debug for Vetting {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Vetting").finish_non_exhaustive()
    }
}

impl Vetting {
    /// Returns the addresses of `host`, waiting at most `wait` for the lookup.
    fn addresses(
        &self,
        host: String,
        port: u16,
        wait: Duration,
    ) -> Result<Vec<SocketAddr>, ureq::Error> {
        let (sender, receiver) = mpsc::sync_channel(1);
        let lookup = self.lookup.clone();
        thread::spawn(move || sender.send(lookup(&host, port)).ok());
        match receiver.recv_timeout(wait) {
            Ok(found) => found.map_err(ureq::Error::Io),
            Err(_) => Err(ureq::Error::Io(io::ErrorKind::TimedOut.into())),
        }
    }
}

/// Returns the host, without the brackets of an IPv6 literal, and the port that `uri` names.
fn host_port(uri: &Uri) -> Result<(String, u16), ureq::Error> {
    let authority = uri.authority().ok_or(ureq::Error::HostNotFound)?;
    let port = authority
        .port_u16()
        .or(match uri.scheme_str() {
            Some("https") => Some(443),
            Some("http") => Some(80),
            _ => None,
        })
        .ok_or(ureq::Error::HostNotFound)?;
    let host = authority
        .host()
        .trim_start_matches('[')
        .trim_end_matches(']')
        .to_string();
    Ok((host, port))
}

impl Resolver for Vetting {
    fn resolve(
        &self,
        uri: &Uri,
        _config: &ureq::config::Config,
        timeout: NextTimeout,
    ) -> Result<ResolvedSocketAddrs, ureq::Error> {
        let (host, port) = host_port(uri)?;
        let wait = if timeout.after.is_not_happening() {
            Duration::MAX
        } else {
            *timeout.after
        };
        let found = self.addresses(host, port, wait)?;
        if found.is_empty() {
            return Err(ureq::Error::HostNotFound);
        }
        if found.iter().any(|addr| !(self.policy)(addr.ip())) {
            return Err(ureq::Error::Io(io::Error::new(
                io::ErrorKind::PermissionDenied,
                Refused,
            )));
        }
        let mut vetted = self.empty();
        for addr in found.into_iter().take(MAX_ADDRESSES) {
            vetted.push(addr);
        }
        Ok(vetted)
    }
}

/// How one delivery try ended.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
enum Outcome {
    /// The callback answered with a 2xx status.
    Delivered,
    /// The callback could not be reached or answered with anything else.
    Failed,
    /// The policy refused the callback's address, so no request was sent.
    Refused,
}

/// Returns whether `error` is the refusal of the address policy.
#[must_use]
fn is_refusal(error: &io::Error) -> bool {
    error.get_ref().is_some_and(|inner| inner.is::<Refused>())
}

/// Builds the agent that sends deliveries: no proxy, no redirects, no idle connections, every
/// address vetted by `resolver` and each try cut off after `timeout`.
fn build_agent(resolver: Vetting, timeout: Duration) -> Agent {
    let tls = TlsConfig::builder()
        .provider(TlsProvider::Rustls)
        .unversioned_rustls_crypto_provider(Arc::new(rustls_graviola::default_provider()))
        .root_certs(RootCerts::WebPki)
        .build();
    let config = Agent::config_builder()
        .tls_config(tls)
        .proxy(None)
        .timeout_global(Some(timeout))
        .http_status_as_error(false)
        .max_redirects(0)
        .max_idle_connections(0)
        .build();
    Agent::with_parts(config, DefaultConnector::new(), resolver)
}

/// Returns the name of the event as the payload spells it.
#[must_use]
fn event_name(delivery: &Delivery) -> String {
    serde_json::to_value(delivery.event)
        .ok()
        .and_then(|name| name.as_str().map(str::to_string))
        .unwrap_or_default()
}

/// Posts `body` to `url` through `agent`, signed when `signature` is given, and returns the status
/// the callback answered with. The answer's body is read up to a cap and dropped.
fn post(
    agent: &Agent,
    url: &str,
    event: &str,
    signature: Option<&str>,
    body: &[u8],
) -> Result<u16, ureq::Error> {
    let mut request = agent
        .post(url)
        .header("Content-Type", "application/json")
        .header("User-Agent", "Pomb")
        .header("X-Pomb-Event", event);
    if let Some(signature) = signature {
        request = request.header("X-Pomb-Signature", signature);
    }
    let mut response = request.send(body)?;
    let status = response.status().as_u16();
    let _ = io::copy(
        &mut response.body_mut().as_reader().take(MAX_RESPONSE_BYTES),
        &mut io::sink(),
    );
    Ok(status)
}

/// Posts `delivery` through `agent` and returns how the try ended.
fn send(agent: &Agent, delivery: &Delivery) -> Outcome {
    let signature = format!(
        "{SIGNATURE_PREFIX}{}",
        sign(&delivery.secret, delivery.body.as_bytes())
    );
    let sent = post(
        agent,
        &delivery.url,
        &event_name(delivery),
        Some(&signature),
        delivery.body.as_bytes(),
    );
    match sent {
        Ok(status) if (200..300).contains(&status) => Outcome::Delivered,
        Ok(_) => Outcome::Failed,
        Err(ureq::Error::Io(error)) if is_refusal(&error) => Outcome::Refused,
        Err(_) => Outcome::Failed,
    }
}

/// What a lookup of a callback host found.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum HostCheck {
    /// The host resolves and every address passes the policy.
    Resolves,
    /// The host does not resolve.
    NotFound,
    /// The host resolves and at least one address fails the policy.
    NotPublic,
}

/// Why a test got no answer.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum ProbeError {
    /// Both slots for tests are taken.
    Busy,
    /// The policy refused the callback's address, so no request was sent.
    Refused,
    /// The callback did not answer in time.
    TimedOut,
    /// The callback could not be reached.
    Unreachable,
}

/// Returns why a request through the delivery agent failed.
#[must_use]
fn probe_error(error: ureq::Error) -> ProbeError {
    match error {
        ureq::Error::Io(error) if is_refusal(&error) => ProbeError::Refused,
        ureq::Error::Timeout(_) => ProbeError::TimedOut,
        ureq::Error::Io(error) if error.kind() == io::ErrorKind::TimedOut => ProbeError::TimedOut,
        _ => ProbeError::Unreachable,
    }
}

/// Runs the checks of the webhook settings: it looks a callback host up and sends one unsigned
/// test request, through the same resolver, policy and agent that deliveries use, with at most
/// [`MAX_PROBES`] of either at once.
#[derive(Clone)]
pub struct Prober {
    /// The agent that sends, whose resolver holds the address policy.
    agent: Agent,
    /// The resolver that looks a host up for a check.
    resolver: Arc<Vetting>,
    /// One permit per check that may run at once.
    slots: Arc<Semaphore>,
    /// The longest a lookup or a request may take.
    timeout: Duration,
}

impl fmt::Debug for Prober {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Prober")
            .field("timeout", &self.timeout)
            .finish_non_exhaustive()
    }
}

impl Prober {
    /// Builds the prober that calls only addresses `policy` accepts, resolved through the system
    /// resolver and cut off at the default delivery timeout.
    #[must_use]
    pub fn new(policy: AddressPolicy) -> Self {
        Self::with_parts(policy, system_lookup(), Schedule::default().timeout)
    }

    /// Builds the prober that resolves names through `lookup`, calls only addresses `policy`
    /// accepts and cuts a check off after `timeout`.
    #[must_use]
    pub fn with_parts(policy: AddressPolicy, lookup: Lookup, timeout: Duration) -> Self {
        let agent = build_agent(
            Vetting {
                lookup: lookup.clone(),
                policy,
            },
            timeout,
        );
        Self {
            agent,
            resolver: Arc::new(Vetting { lookup, policy }),
            slots: Arc::new(Semaphore::new(MAX_PROBES)),
            timeout,
        }
    }

    /// Looks up the host of `url` and says whether it resolves to public addresses only. The
    /// addresses themselves stay inside.
    pub async fn check_host(&self, url: &str) -> Result<HostCheck, ProbeError> {
        let permit = self
            .slots
            .clone()
            .try_acquire_owned()
            .map_err(|_| ProbeError::Busy)?;
        let Some((host, port)) = url.parse::<Uri>().ok().and_then(|uri| host_port(&uri).ok())
        else {
            return Ok(HostCheck::NotFound);
        };
        let resolver = self.resolver.clone();
        let wait = self.timeout;
        let checked = tokio::task::spawn_blocking(move || {
            let _permit = permit;
            match resolver.addresses(host, port, wait) {
                Ok(found) if found.is_empty() => HostCheck::NotFound,
                Ok(found) if found.iter().any(|addr| !(resolver.policy)(addr.ip())) => {
                    HostCheck::NotPublic
                }
                Ok(_) => HostCheck::Resolves,
                Err(_) => HostCheck::NotFound,
            }
        })
        .await;
        Ok(checked.unwrap_or(HostCheck::NotFound))
    }

    /// Posts one unsigned test event for board `board_id` to `url` and returns the status the
    /// callback answered with.
    pub async fn send_test(&self, url: &str, board_id: &str) -> Result<u16, ProbeError> {
        let permit = self
            .slots
            .clone()
            .try_acquire_owned()
            .map_err(|_| ProbeError::Busy)?;
        let body = serde_json::json!({
            "eventType": "test",
            "timestamp": Utc::now().to_rfc3339(),
            "boardId": board_id,
        })
        .to_string();
        let agent = self.agent.clone();
        let url = url.to_string();
        tokio::task::spawn_blocking(move || {
            let _permit = permit;
            post(&agent, &url, "test", None, body.as_bytes())
        })
        .await
        .map_err(|_| ProbeError::Unreachable)?
        .map_err(probe_error)
    }
}

/// The delivery worker of one server.
#[derive(Clone)]
pub struct Dispatcher {
    /// The database the outbox lives in.
    db: Db,
    /// The agent that sends, whose resolver holds the address policy.
    agent: Agent,
    /// The timing of the worker.
    schedule: Schedule,
}

impl fmt::Debug for Dispatcher {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Dispatcher")
            .field("schedule", &self.schedule)
            .finish_non_exhaustive()
    }
}

impl Dispatcher {
    /// Builds the worker of `db` that calls only addresses `policy` accepts, resolved through the
    /// system resolver and timed by [`Schedule::default`].
    #[must_use]
    pub fn new(db: Db, policy: AddressPolicy) -> Self {
        Self::with_parts(db, policy, system_lookup(), Schedule::default())
    }

    /// Builds the worker of `db` that resolves names through `lookup`, calls only addresses
    /// `policy` accepts and keeps the time `schedule` sets.
    #[must_use]
    pub fn with_parts(db: Db, policy: AddressPolicy, lookup: Lookup, schedule: Schedule) -> Self {
        let agent = build_agent(Vetting { lookup, policy }, schedule.timeout);
        Self {
            db,
            agent,
            schedule,
        }
    }

    /// Delivers queued events every [`Schedule::tick`], for as long as the runtime runs.
    pub async fn run(self) {
        let permits = Arc::new(Semaphore::new(MAX_IN_FLIGHT));
        let in_flight: Arc<Mutex<HashSet<String>>> = Arc::default();
        let mut ticker = tokio::time::interval(self.schedule.tick);
        ticker.set_missed_tick_behavior(MissedTickBehavior::Skip);
        loop {
            ticker.tick().await;
            let free = permits.available_permits();
            if free == 0 {
                continue;
            }
            let batch = {
                let db = self.db.clone();
                let lease = self.schedule.lease();
                let busy = in_flight
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .clone();
                tokio::task::spawn_blocking(move || {
                    db.claim_deliveries(Utc::now(), lease, free, &busy)
                        .map_err(|error| error.to_string())
                })
                .await
            };
            let batch = match batch {
                Ok(Ok(batch)) => batch,
                Ok(Err(error)) => {
                    eprintln!("pomb: could not claim webhook events: {error}");
                    continue;
                }
                Err(error) => {
                    eprintln!("pomb: the webhook claim failed: {error}");
                    continue;
                }
            };
            for delivery in batch {
                let Ok(permit) = permits.clone().try_acquire_owned() else {
                    break;
                };
                in_flight
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .insert(delivery.webhook_id.clone());
                let worker = self.clone();
                let in_flight = in_flight.clone();
                tokio::spawn(async move {
                    let webhook_id = delivery.webhook_id.clone();
                    worker.deliver(delivery).await;
                    in_flight
                        .lock()
                        .unwrap_or_else(PoisonError::into_inner)
                        .remove(&webhook_id);
                    drop(permit);
                });
            }
        }
    }

    /// Sends `delivery` on a blocking thread and records how it ended.
    async fn deliver(&self, delivery: Delivery) {
        let key = delivery.key.clone();
        let attempts = delivery.attempts;
        let agent = self.agent.clone();
        let outcome = tokio::task::spawn_blocking(move || send(&agent, &delivery))
            .await
            .unwrap_or(Outcome::Failed);
        if outcome == Outcome::Refused {
            eprintln!("pomb: dropped a webhook event, its callback address is not public");
        }
        let db = self.db.clone();
        let backoff = self.schedule.backoff;
        let recorded = tokio::task::spawn_blocking(move || {
            record(&db, &key, attempts, outcome, backoff).map_err(|error| error.to_string())
        })
        .await;
        match recorded {
            Ok(Ok(())) => {}
            Ok(Err(error)) => eprintln!("pomb: could not record a webhook delivery: {error}"),
            Err(error) => eprintln!("pomb: the webhook record failed: {error}"),
        }
    }
}

/// Records how a try of event `key` ended, where `attempts` is the count of failed tries.
fn record(
    db: &Db,
    key: &str,
    attempts: u32,
    outcome: Outcome,
    backoff: [Duration; RETRIES],
) -> DbResult<()> {
    let next = usize::try_from(attempts).ok().and_then(|n| backoff.get(n));
    match (outcome, next) {
        (Outcome::Failed, Some(wait)) if attempts + 1 < MAX_ATTEMPTS => {
            let wait = TimeDelta::from_std(*wait).unwrap_or(TimeDelta::MAX);
            db.retry_delivery(key, Utc::now() + wait)
        }
        _ => db.finish_delivery(key),
    }
}

#[cfg(test)]
mod tests;
