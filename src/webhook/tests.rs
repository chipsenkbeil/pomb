use std::collections::HashSet;
use std::io::{Read, Write};
use std::net::{Ipv4Addr, Ipv6Addr, TcpListener, TcpStream};
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Instant;

use sha2::Digest;

use super::*;
use crate::db::{NewWebhook, WebhookColumnMissing, WebhookPatch};
use crate::models::{OutboxRow, WebhookEvent, WebhookRow};

const FAST: Schedule = Schedule {
    tick: Duration::from_millis(10),
    timeout: Duration::from_millis(400),
    backoff: [Duration::from_millis(40), Duration::from_millis(80)],
};

const SLOW_RETRY: Schedule = Schedule {
    backoff: [Duration::from_secs(60), Duration::from_secs(60)],
    ..FAST
};

const LONG: Schedule = Schedule {
    timeout: Duration::from_secs(2),
    ..SLOW_RETRY
};

const HANG: Duration = Duration::from_secs(3);

const POLL_LIMIT: u32 = 1000;

fn allow_all(_: IpAddr) -> bool {
    true
}

#[derive(Clone)]
enum Reply {
    Status(u16),
    Redirect(&'static str),
    Hang,
    Slow(u16),
    Big(usize),
    TrickleHead,
    TrickleBody,
}

#[derive(Clone, Debug)]
struct Seen {
    path: String,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

impl Seen {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value.as_str())
    }
}

struct Callback {
    port: u16,
    seen: Arc<Mutex<Vec<Seen>>>,
    written: Arc<AtomicUsize>,
}

impl Callback {
    fn start(script: Vec<Reply>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let port = listener.local_addr().expect("addr").port();
        let seen = Arc::new(Mutex::new(Vec::new()));
        let shared = seen.clone();
        let written = Arc::new(AtomicUsize::new(0));
        let counted = written.clone();
        thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                let request = read_request(&stream);
                let reply = {
                    let mut seen = shared.lock().expect("lock");
                    let index = seen.len();
                    seen.push(request);
                    script
                        .get(index)
                        .or(script.last())
                        .cloned()
                        .unwrap_or(Reply::Status(200))
                };
                let counted = counted.clone();
                thread::spawn(move || answer(stream, &reply, &counted));
            }
        });
        Self {
            port,
            seen,
            written,
        }
    }

    fn url(&self, host: &str) -> String {
        format!("http://{host}:{}/hook", self.port)
    }

    fn requests(&self) -> Vec<Seen> {
        self.seen.lock().expect("lock").clone()
    }
}

fn read_request(mut stream: &TcpStream) -> Seen {
    let mut raw = Vec::new();
    let mut chunk = [0u8; 1024];
    let head_end = loop {
        if let Some(at) = raw.windows(4).position(|window| window == b"\r\n\r\n") {
            break at + 4;
        }
        match stream.read(&mut chunk) {
            Ok(read) if read > 0 => raw.extend_from_slice(&chunk[..read]),
            _ => {
                return Seen {
                    path: String::new(),
                    headers: Vec::new(),
                    body: Vec::new(),
                };
            }
        }
    };
    let head = String::from_utf8_lossy(&raw[..head_end]).to_string();
    let mut lines = head.lines();
    let path = lines
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .unwrap_or_default()
        .to_string();
    let headers: Vec<(String, String)> = lines
        .filter_map(|line| line.split_once(':'))
        .map(|(key, value)| (key.trim().to_ascii_lowercase(), value.trim().to_string()))
        .collect();
    let length: usize = headers
        .iter()
        .find(|(key, _)| key == "content-length")
        .and_then(|(_, value)| value.parse().ok())
        .unwrap_or(0);
    while raw.len() < head_end + length {
        match stream.read(&mut chunk) {
            Ok(read) if read > 0 => raw.extend_from_slice(&chunk[..read]),
            _ => break,
        }
    }
    let end = raw.len().min(head_end + length);
    Seen {
        path,
        headers,
        body: raw[head_end..end].to_vec(),
    }
}

#[test]
fn callback_should_record_a_connection_closed_before_the_head() {
    let callback = Callback::start(vec![Reply::Status(200)]);
    drop(TcpStream::connect(("127.0.0.1", callback.port)).expect("connect"));
    let mut stream = TcpStream::connect(("127.0.0.1", callback.port)).expect("connect");
    stream
        .write_all(b"POST /hook HTTP/1.1\r\nContent-Length: 0\r\n\r\n")
        .expect("write");
    let mut reply = Vec::new();
    stream.read_to_end(&mut reply).expect("reply");
    let seen = callback.requests();
    assert_eq!(seen.len(), 2);
    assert!(seen[0].path.is_empty() && seen[0].headers.is_empty() && seen[0].body.is_empty());
    assert_eq!(seen[1].path, "/hook");
}

fn answer(mut stream: TcpStream, reply: &Reply, written: &AtomicUsize) {
    let text = match reply {
        Reply::Status(code) => {
            format!("HTTP/1.1 {code} X\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
        }
        Reply::Redirect(to) => format!(
            "HTTP/1.1 302 Found\r\nLocation: {to}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
        ),
        Reply::Slow(code) => {
            thread::sleep(Duration::from_millis(400));
            format!("HTTP/1.1 {code} X\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
        }
        Reply::Big(total) => {
            let head =
                format!("HTTP/1.1 200 OK\r\nContent-Length: {total}\r\nConnection: close\r\n\r\n");
            if stream.write_all(head.as_bytes()).is_err() {
                return;
            }
            let chunk = [b'x'; 8192];
            let mut sent = 0;
            while sent < *total {
                let size = chunk.len().min(*total - sent);
                if stream.write_all(&chunk[..size]).is_err() {
                    return;
                }
                sent += size;
                written.store(sent, Ordering::SeqCst);
            }
            return;
        }
        Reply::TrickleHead => {
            for byte in b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n" {
                if stream.write_all(&[*byte]).is_err() {
                    return;
                }
                thread::sleep(Duration::from_millis(50));
            }
            return;
        }
        Reply::TrickleBody => {
            let head = "HTTP/1.1 200 OK\r\nContent-Length: 100000\r\nConnection: close\r\n\r\n";
            if stream.write_all(head.as_bytes()).is_err() {
                return;
            }
            for _ in 0..60 {
                if stream.write_all(b"x").is_err() {
                    return;
                }
                thread::sleep(Duration::from_millis(50));
            }
            return;
        }
        Reply::Hang => {
            thread::sleep(HANG);
            return;
        }
    };
    let _ = stream.write_all(text.as_bytes());
}

struct Fixture {
    _dir: tempfile::TempDir,
    db: Db,
    board: String,
    column: String,
}

impl Fixture {
    fn new() -> Self {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("deliver.redb");
        let db = Db::connect(path.to_str().expect("utf8")).expect("connect");
        let owner = db.create_user("admin").expect("admin");
        let board = db.create_board(&owner, "Hooks").expect("board");
        let column = db.create_column(&board, "Todo", None).expect("column");
        Self {
            _dir: dir,
            db,
            board,
            column,
        }
    }

    fn webhook(&self, url: &str) -> WebhookRow {
        self.db
            .create_webhook(NewWebhook {
                board_id: self.board.clone(),
                name: "Hook".to_string(),
                url: url.to_string(),
                events: vec![WebhookEvent::TaskCreated],
                column_id: None,
                enabled: true,
            })
            .expect("webhook")
    }

    fn queue(&self, name: &str) {
        self.db
            .create_task(&self.column, None, name, 1, None)
            .expect("task");
    }

    fn outbox(&self, hook: &str) -> Vec<OutboxRow> {
        self.db.webhook_outbox(hook).expect("outbox")
    }

    fn run(
        &self,
        policy: AddressPolicy,
        lookup: Lookup,
        schedule: Schedule,
    ) -> tokio::task::JoinHandle<()> {
        tokio::spawn(Dispatcher::with_parts(self.db.clone(), policy, lookup, schedule).run())
    }

    fn run_plain(&self, schedule: Schedule) -> tokio::task::JoinHandle<()> {
        self.run(allow_all, system_lookup(), schedule)
    }
}

async fn until(mut done: impl FnMut() -> bool) {
    for _ in 0..POLL_LIMIT {
        if done() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("the condition never held");
}

async fn settle() {
    tokio::time::sleep(Duration::from_millis(500)).await;
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn hand_hmac(key: &[u8], data: &[u8]) -> String {
    let mut block = [0u8; 64];
    if key.len() > block.len() {
        block[..32].copy_from_slice(&Sha256::digest(key));
    } else {
        block[..key.len()].copy_from_slice(key);
    }
    let inner: Vec<u8> = block
        .iter()
        .map(|byte| byte ^ 0x36)
        .chain(data.iter().copied())
        .collect();
    let outer: Vec<u8> = block
        .iter()
        .map(|byte| byte ^ 0x5c)
        .chain(Sha256::digest(&inner).iter().copied())
        .collect();
    hex(&Sha256::digest(&outer))
}

fn ip(text: &str) -> IpAddr {
    text.parse().expect("address")
}

#[test]
fn sign_should_match_the_rfc_4231_vector() {
    assert_eq!(
        sign("Jefe", b"what do ya want for nothing?"),
        "5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843"
    );
}

#[test]
fn is_public_address_should_refuse_loopback_and_unspecified() {
    for text in [
        "127.0.0.1",
        "127.255.255.254",
        "::1",
        "0.0.0.0",
        "::",
        "0.1.2.3",
    ] {
        assert!(!is_public_address(ip(text)), "{text}");
    }
}

#[test]
fn is_public_address_should_refuse_link_local() {
    for text in [
        "169.254.169.254",
        "169.254.0.1",
        "fe80::1",
        "febf::1",
        "fec0::1",
    ] {
        assert!(!is_public_address(ip(text)), "{text}");
    }
}

#[test]
fn is_public_address_should_refuse_private_ranges() {
    for text in [
        "10.0.0.1",
        "10.255.255.255",
        "172.16.0.1",
        "172.31.255.255",
        "192.168.0.1",
        "192.168.255.255",
        "fc00::1",
        "fd12:3456::1",
    ] {
        assert!(!is_public_address(ip(text)), "{text}");
    }
}

#[test]
fn is_public_address_should_refuse_shared_benchmarking_documentation_and_reserved() {
    for text in [
        "100.64.0.1",
        "100.127.255.255",
        "198.18.0.1",
        "198.19.255.255",
        "192.0.2.1",
        "198.51.100.1",
        "203.0.113.1",
        "2001:db8::1",
        "224.0.0.1",
        "239.255.255.255",
        "ff02::1",
        "255.255.255.255",
        "240.0.0.1",
    ] {
        assert!(!is_public_address(ip(text)), "{text}");
    }
}

#[test]
fn is_public_address_should_unwrap_embedded_ipv4_addresses() {
    for text in [
        "::ffff:127.0.0.1",
        "::ffff:0:127.0.0.1",
        "::ffff:0:10.0.0.1",
        "::ffff:10.1.2.3",
        "::ffff:192.168.1.1",
        "::ffff:169.254.169.254",
        "::127.0.0.1",
        "::10.0.0.1",
        "64:ff9b::7f00:1",
        "64:ff9b::a00:1",
        "2002:7f00:1::1",
    ] {
        assert!(!is_public_address(ip(text)), "{text}");
    }
    for text in [
        "::ffff:8.8.8.8",
        "::ffff:0:8.8.8.8",
        "64:ff9b::808:808",
        "2002:808:808::1",
    ] {
        assert!(is_public_address(ip(text)), "{text}");
    }
}

#[test]
fn is_public_address_should_accept_public_addresses() {
    for text in [
        "8.8.8.8",
        "1.1.1.1",
        "93.184.216.34",
        "172.15.255.255",
        "172.32.0.1",
        "100.63.255.255",
        "100.128.0.1",
        "2606:4700:4700::1111",
        "2a00:1450:4001::200e",
    ] {
        assert!(is_public_address(ip(text)), "{text}");
    }
    assert!(is_public_address(IpAddr::V4(Ipv4Addr::new(8, 8, 4, 4))));
    assert!(is_public_address(IpAddr::V6(Ipv6Addr::new(
        0x2606, 0x4700, 0, 0, 0, 0, 0, 1
    ))));
}

#[test]
fn is_public_host_should_refuse_localhost_names_and_private_literals() {
    for host in [
        "localhost",
        "LOCALHOST",
        "localhost.",
        "api.localhost",
        "a.b.localhost",
        "127.0.0.1",
        "[::1]",
        "[::ffff:7f00:1]",
        "10.0.0.5",
        "169.254.169.254",
    ] {
        assert!(!is_public_host(host), "{host}");
    }
    for host in [
        "example.com",
        "hooks.example.org",
        "8.8.8.8",
        "[2606:4700::1111]",
        "notlocalhost",
    ] {
        assert!(is_public_host(host), "{host}");
    }
}

#[tokio::test]
async fn dispatcher_should_sign_the_exact_body_and_send_the_headers() {
    let callback = Callback::start(vec![Reply::Status(200)]);
    let fx = Fixture::new();
    let hook = fx.webhook(&callback.url("127.0.0.1"));
    fx.queue("Signed");
    let stored = fx.outbox(&hook.id).remove(0).body;
    let worker = fx.run_plain(FAST);
    until(|| fx.outbox(&hook.id).is_empty()).await;
    worker.abort();

    let seen = callback.requests();
    assert_eq!(seen.len(), 1);
    assert_eq!(seen[0].path, "/hook");
    assert_eq!(seen[0].body, stored.as_bytes());
    assert_eq!(seen[0].header("content-type"), Some("application/json"));
    assert_eq!(seen[0].header("user-agent"), Some("Pomb"));
    assert_eq!(seen[0].header("x-pomb-event"), Some("taskCreated"));
    let expected = format!(
        "sha256={}",
        hand_hmac(hook.secret.as_bytes(), stored.as_bytes())
    );
    assert_eq!(seen[0].header("x-pomb-signature"), Some(expected.as_str()));
}

#[tokio::test]
async fn dispatcher_should_retry_a_failing_callback_twice_then_drop_the_event() {
    let callback = Callback::start(vec![Reply::Status(500)]);
    let fx = Fixture::new();
    let hook = fx.webhook(&callback.url("127.0.0.1"));
    fx.queue("Failing");
    let worker = fx.run_plain(FAST);
    until(|| fx.outbox(&hook.id).is_empty()).await;
    settle().await;
    worker.abort();
    assert_eq!(callback.requests().len(), 3);
}

#[tokio::test]
async fn dispatcher_should_delete_the_event_after_a_success() {
    let callback = Callback::start(vec![Reply::Status(204)]);
    let fx = Fixture::new();
    let hook = fx.webhook(&callback.url("127.0.0.1"));
    fx.queue("Fine");
    let worker = fx.run_plain(FAST);
    until(|| fx.outbox(&hook.id).is_empty()).await;
    settle().await;
    worker.abort();
    assert_eq!(callback.requests().len(), 1);
}

#[tokio::test]
async fn dispatcher_should_stop_retrying_after_a_success_on_the_second_attempt() {
    let callback = Callback::start(vec![Reply::Status(503), Reply::Status(200)]);
    let fx = Fixture::new();
    let hook = fx.webhook(&callback.url("127.0.0.1"));
    fx.queue("Second");
    let worker = fx.run_plain(FAST);
    until(|| fx.outbox(&hook.id).is_empty()).await;
    settle().await;
    worker.abort();
    assert_eq!(callback.requests().len(), 2);
}

#[tokio::test]
async fn dispatcher_should_wait_out_the_backoff_between_attempts() {
    let callback = Callback::start(vec![Reply::Status(500)]);
    let fx = Fixture::new();
    let hook = fx.webhook(&callback.url("127.0.0.1"));
    fx.queue("Waiting");
    let worker = fx.run_plain(SLOW_RETRY);
    until(|| !callback.requests().is_empty()).await;
    until(|| {
        fx.outbox(&hook.id)
            .first()
            .is_some_and(|row| row.attempts == 1)
    })
    .await;
    settle().await;
    worker.abort();
    assert_eq!(callback.requests().len(), 1);
    let row = fx.outbox(&hook.id).remove(0);
    assert!(row.next_attempt_at.is_some());
}

#[tokio::test]
async fn dispatcher_should_not_follow_a_redirect() {
    let callback = Callback::start(vec![Reply::Redirect("/other")]);
    let fx = Fixture::new();
    let hook = fx.webhook(&callback.url("127.0.0.1"));
    fx.queue("Moved");
    let worker = fx.run_plain(FAST);
    until(|| fx.outbox(&hook.id).is_empty()).await;
    settle().await;
    worker.abort();
    let seen = callback.requests();
    assert_eq!(seen.len(), 3);
    assert!(seen.iter().all(|request| request.path == "/hook"));
}

#[tokio::test]
async fn dispatcher_should_not_block_a_write_while_a_callback_never_answers() {
    let callback = Callback::start(vec![Reply::Hang]);
    let fx = Fixture::new();
    fx.webhook(&callback.url("127.0.0.1"));
    fx.queue("First");
    let worker = fx.run_plain(SLOW_RETRY);
    until(|| !callback.requests().is_empty()).await;

    let started = Instant::now();
    fx.queue("Second");
    assert!(
        started.elapsed() < Duration::from_millis(300),
        "{:?}",
        started.elapsed()
    );
    worker.abort();
}

#[tokio::test]
async fn dispatcher_should_cut_a_try_off_at_its_timeout() {
    let callback = Callback::start(vec![Reply::Hang]);
    let fx = Fixture::new();
    let hook = fx.webhook(&callback.url("127.0.0.1"));
    fx.queue("Slow");
    let worker = fx.run_plain(SLOW_RETRY);
    let started = Instant::now();
    until(|| {
        fx.outbox(&hook.id)
            .first()
            .is_some_and(|row| row.attempts == 1)
    })
    .await;
    worker.abort();
    assert!(started.elapsed() < HANG, "{:?}", started.elapsed());
    assert!(started.elapsed() >= FAST.timeout, "{:?}", started.elapsed());
}

#[tokio::test]
async fn dispatcher_should_drop_the_events_of_a_disabled_webhook_unsent() {
    let callback = Callback::start(vec![Reply::Status(200)]);
    let fx = Fixture::new();
    let hook = fx.webhook(&callback.url("127.0.0.1"));
    fx.queue("Quiet");
    assert_eq!(fx.outbox(&hook.id).len(), 1);
    fx.db
        .update_webhook(
            &hook.id,
            WebhookPatch {
                enabled: Some(false),
                ..WebhookPatch::default()
            },
        )
        .expect("update");
    let worker = fx.run_plain(FAST);
    until(|| fx.outbox(&hook.id).is_empty()).await;
    settle().await;
    worker.abort();
    assert!(callback.requests().is_empty());
}

#[tokio::test]
async fn dispatcher_should_drop_an_event_whose_name_resolves_to_a_private_address() {
    let callback = Callback::start(vec![Reply::Status(200)]);
    let port = callback.port;
    let fx = Fixture::new();
    let hook = fx.webhook(&format!("http://internal.test:{port}/hook"));
    fx.queue("Rebound");
    let lookup: Lookup = Arc::new(move |_, port| {
        Ok(vec![
            SocketAddr::from(([8, 8, 8, 8], port)),
            SocketAddr::from(([127, 0, 0, 1], port)),
        ])
    });
    let worker = fx.run(is_public_address, lookup, SLOW_RETRY);
    until(|| fx.outbox(&hook.id).is_empty()).await;
    settle().await;
    worker.abort();
    assert!(callback.requests().is_empty());
}

#[tokio::test]
async fn dispatcher_should_refuse_a_loopback_literal_under_the_real_policy() {
    let callback = Callback::start(vec![Reply::Status(200)]);
    let fx = Fixture::new();
    let hook = fx.webhook(&callback.url("127.0.0.1"));
    fx.queue("Literal");
    let worker = fx.run(is_public_address, system_lookup(), SLOW_RETRY);
    until(|| fx.outbox(&hook.id).is_empty()).await;
    settle().await;
    worker.abort();
    assert!(callback.requests().is_empty());
}

#[tokio::test]
async fn dispatcher_should_connect_to_the_vetted_address_and_keep_the_host_header() {
    let callback = Callback::start(vec![Reply::Status(200)]);
    let port = callback.port;
    let fx = Fixture::new();
    let hook = fx.webhook(&format!("http://hook.test:{port}/hook"));
    fx.queue("Named");
    let lookup: Lookup = Arc::new(|host, port| {
        assert_eq!(host, "hook.test");
        Ok(vec![SocketAddr::from(([127, 0, 0, 1], port))])
    });
    let worker = fx.run(allow_all, lookup, FAST);
    until(|| fx.outbox(&hook.id).is_empty()).await;
    worker.abort();
    let seen = callback.requests();
    assert_eq!(seen.len(), 1);
    let expected = format!("hook.test:{port}");
    assert_eq!(seen[0].header("host"), Some(expected.as_str()));
}

#[test]
fn claim_deliveries_should_hold_an_event_for_its_lease() {
    let fx = Fixture::new();
    let hook = fx.webhook("http://example.com/hook");
    fx.queue("Held");
    let now = Utc::now();
    let lease = TimeDelta::seconds(15);
    let first = fx
        .db
        .claim_deliveries(now, lease, 4, &HashSet::new())
        .expect("claim");
    assert_eq!(first.len(), 1);
    assert_eq!(first[0].url, "http://example.com/hook");
    assert_eq!(first[0].secret, hook.secret);
    assert!(
        fx.db
            .claim_deliveries(now, lease, 4, &HashSet::new())
            .expect("claim")
            .is_empty()
    );
    let later = now + TimeDelta::seconds(16);
    assert_eq!(
        fx.db
            .claim_deliveries(later, lease, 4, &HashSet::new())
            .expect("claim")
            .len(),
        1
    );
}

#[test]
fn create_webhook_should_refuse_a_column_of_another_board_in_its_transaction() {
    let fx = Fixture::new();
    let owner = fx.db.create_user("second").expect("user");
    let other = fx.db.create_board(&owner, "Other").expect("board");
    let foreign = fx
        .db
        .create_column(&other, "Elsewhere", None)
        .expect("column");
    let refused = fx
        .db
        .create_webhook(NewWebhook {
            board_id: fx.board.clone(),
            name: "Hook".to_string(),
            url: "https://example.com/hook".to_string(),
            events: vec![WebhookEvent::TaskCreated],
            column_id: Some(foreign.clone()),
            enabled: true,
        })
        .expect_err("a foreign column");
    assert!(refused.downcast_ref::<WebhookColumnMissing>().is_some());

    let hook = fx.webhook("https://example.com/hook");
    let refused = fx
        .db
        .update_webhook(
            &hook.id,
            WebhookPatch {
                column_id: Some(Some(foreign)),
                ..WebhookPatch::default()
            },
        )
        .expect_err("a foreign column");
    assert!(refused.downcast_ref::<WebhookColumnMissing>().is_some());
    let kept = fx.db.webhook(&hook.id).expect("read").expect("hook");
    assert_eq!(kept.column_id, None);
}

#[tokio::test]
async fn dispatcher_should_run_one_delivery_per_webhook_and_not_let_a_hanging_one_block_another() {
    let hanging = Callback::start(vec![Reply::Hang]);
    let healthy = Callback::start(vec![Reply::Status(200)]);
    let fx = Fixture::new();
    let stuck = fx.webhook(&hanging.url("127.0.0.1"));
    let fine = fx.webhook(&healthy.url("127.0.0.1"));
    fx.queue("One");
    fx.queue("Two");
    let worker = fx.run_plain(LONG);
    until(|| fx.outbox(&fine.id).is_empty()).await;
    settle().await;
    worker.abort();
    assert_eq!(healthy.requests().len(), 2);
    assert_eq!(hanging.requests().len(), 1);
    assert_eq!(fx.outbox(&stuck.id).len(), 2);
}

#[tokio::test]
async fn dispatcher_should_run_at_most_four_deliveries_at_once() {
    let callback = Callback::start(vec![Reply::Hang]);
    let fx = Fixture::new();
    for _ in 0..6 {
        fx.webhook(&callback.url("127.0.0.1"));
    }
    fx.queue("Many");
    let worker = fx.run_plain(LONG);
    until(|| callback.requests().len() == MAX_IN_FLIGHT).await;
    settle().await;
    worker.abort();
    assert_eq!(callback.requests().len(), MAX_IN_FLIGHT);
}

#[tokio::test]
async fn dispatcher_should_succeed_on_a_large_answer_without_reading_past_the_cap() {
    let total = 64 * 1024 * 1024;
    let callback = Callback::start(vec![Reply::Big(total)]);
    let fx = Fixture::new();
    let hook = fx.webhook(&callback.url("127.0.0.1"));
    fx.queue("Large");
    let worker = fx.run_plain(LONG);
    until(|| fx.outbox(&hook.id).is_empty()).await;
    settle().await;
    worker.abort();
    assert_eq!(callback.requests().len(), 1);
    assert!(callback.written.load(Ordering::SeqCst) < total);
}

#[tokio::test]
async fn dispatcher_should_cut_off_a_callback_that_trickles_its_status_line() {
    let callback = Callback::start(vec![Reply::TrickleHead]);
    let fx = Fixture::new();
    let hook = fx.webhook(&callback.url("127.0.0.1"));
    fx.queue("Trickle");
    let worker = fx.run_plain(SLOW_RETRY);
    let started = Instant::now();
    until(|| {
        fx.outbox(&hook.id)
            .first()
            .is_some_and(|row| row.attempts == 1)
    })
    .await;
    worker.abort();
    assert!(started.elapsed() >= FAST.timeout, "{:?}", started.elapsed());
    assert!(
        started.elapsed() < Duration::from_millis(1500),
        "{:?}",
        started.elapsed()
    );
}

#[tokio::test]
async fn dispatcher_should_not_wait_for_a_body_that_trickles_past_the_timeout() {
    let callback = Callback::start(vec![Reply::TrickleBody]);
    let fx = Fixture::new();
    let hook = fx.webhook(&callback.url("127.0.0.1"));
    fx.queue("Body");
    let worker = fx.run_plain(SLOW_RETRY);
    let started = Instant::now();
    until(|| fx.outbox(&hook.id).is_empty()).await;
    worker.abort();
    assert!(
        started.elapsed() < Duration::from_millis(2000),
        "{:?}",
        started.elapsed()
    );
}

#[tokio::test]
async fn dispatcher_should_reclaim_an_event_whose_lease_ran_out() {
    let callback = Callback::start(vec![Reply::Status(200)]);
    let fx = Fixture::new();
    let hook = fx.webhook(&callback.url("127.0.0.1"));
    fx.queue("Crashed");
    let lease = TimeDelta::milliseconds(600);
    let claimed = fx
        .db
        .claim_deliveries(Utc::now(), lease, 4, &HashSet::new())
        .expect("claim");
    assert_eq!(claimed.len(), 1);
    let worker = fx.run_plain(FAST);
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(callback.requests().is_empty());
    until(|| fx.outbox(&hook.id).is_empty()).await;
    worker.abort();
    assert_eq!(callback.requests().len(), 1);
}

#[tokio::test]
async fn dispatcher_should_not_retry_when_the_webhook_is_disabled_during_the_try() {
    let callback = Callback::start(vec![Reply::Slow(500)]);
    let fx = Fixture::new();
    let hook = fx.webhook(&callback.url("127.0.0.1"));
    fx.queue("Disabled");
    let worker = fx.run_plain(Schedule {
        timeout: Duration::from_secs(2),
        ..FAST
    });
    until(|| !callback.requests().is_empty()).await;
    fx.db
        .update_webhook(
            &hook.id,
            WebhookPatch {
                enabled: Some(false),
                ..WebhookPatch::default()
            },
        )
        .expect("update");
    until(|| fx.outbox(&hook.id).is_empty()).await;
    settle().await;
    worker.abort();
    assert_eq!(callback.requests().len(), 1);
}

#[tokio::test]
async fn dispatcher_should_leave_no_row_when_the_webhook_is_deleted_during_the_try() {
    let callback = Callback::start(vec![Reply::Slow(500)]);
    let fx = Fixture::new();
    let hook = fx.webhook(&callback.url("127.0.0.1"));
    fx.queue("Deleted");
    let worker = fx.run_plain(Schedule {
        timeout: Duration::from_secs(2),
        ..FAST
    });
    until(|| !callback.requests().is_empty()).await;
    assert!(fx.db.delete_webhook(&hook.id).expect("delete"));
    tokio::time::sleep(Duration::from_millis(1000)).await;
    worker.abort();
    assert_eq!(callback.requests().len(), 1);
    assert!(fx.outbox(&hook.id).is_empty());
}

#[test]
fn schedule_should_hold_an_event_longer_than_a_try_may_last() {
    for schedule in [Schedule::default(), FAST, SLOW_RETRY, LONG] {
        let timeout = TimeDelta::from_std(schedule.timeout).expect("timeout");
        assert!(schedule.lease() > timeout, "{schedule:?}");
    }
}

#[test]
fn webhook_event_name_should_match_the_serialized_form() {
    for event in WebhookEvent::ALL {
        assert_eq!(serde_json::to_value(event).expect("json"), event.name());
        assert!(!event.label().is_empty());
    }
    assert_eq!(WebhookEvent::ALL.len(), 7);
}
