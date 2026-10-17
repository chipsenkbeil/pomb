//! SIGTERM closes the database cleanly, so the next start needs no redb
//! crash repair; SIGKILL is the control that does.

use std::io::{BufRead, BufReader, ErrorKind, Read, Write};
use std::net::TcpStream;
use std::process::{Child, ChildStdout, Command, ExitStatus, Stdio};
use std::time::{Duration, Instant};

/// `SHUTDOWN_DRAIN` in src/main.rs.
const DRAIN: Duration = Duration::from_secs(10);

/// A running server on an OS-assigned port.
struct Server {
    child: Child,
    port: u16,
    stdout: BufReader<ChildStdout>,
}

impl Server {
    /// Start on `db_path` with `--port 0` and read the bound port from the
    /// startup line.
    fn start(db_path: &str) -> Self {
        let mut child = Command::new(env!("CARGO_BIN_EXE_chipflow"))
            .args(["--host", "127.0.0.1", "--port", "0"])
            .args(["--database-path", db_path])
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn chipflow");
        let mut stdout = BufReader::new(child.stdout.take().expect("child stdout"));
        let mut line = String::new();
        let port = loop {
            line.clear();
            let read = stdout.read_line(&mut line).expect("read child stdout");
            assert!(read > 0, "chipflow exited before listening");
            if let Some(rest) = line.strip_prefix("chipflow listening on http://127.0.0.1:") {
                let port = rest.split_whitespace().next().expect("port");
                break port.parse().expect("numeric port");
            }
        };
        Server {
            child,
            port,
            stdout,
        }
    }

    fn signal(&self, signal: &str) {
        let sent = Command::new("kill")
            .args([signal, &self.child.id().to_string()])
            .status()
            .expect("run kill");
        assert!(sent.success(), "kill {signal} failed");
    }

    /// Send `signal` and wait for exit; returns the status and the rest of
    /// the server's stdout.
    fn signal_and_wait(&mut self, signal: &str, limit: Duration) -> (ExitStatus, String) {
        self.signal(signal);
        self.wait(limit)
    }

    fn wait(&mut self, limit: Duration) -> (ExitStatus, String) {
        let deadline = Instant::now() + limit;
        let status = loop {
            if let Some(status) = self.child.try_wait().expect("poll child") {
                break status;
            }
            if Instant::now() >= deadline {
                self.child.kill().ok();
                panic!("chipflow did not exit within {limit:?}");
            }
            std::thread::sleep(Duration::from_millis(50));
        };
        let mut rest = String::new();
        self.stdout.read_to_string(&mut rest).expect("drain stdout");
        (status, rest)
    }
}

#[test]
fn sigterm_leaves_a_cleanly_closed_database() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("chipflow.redb");
    let path = path.to_str().expect("utf8 path");

    let mut server = Server::start(path);
    assert_ne!(server.port, 0);
    let (status, log) = server.signal_and_wait("-TERM", Duration::from_secs(30));
    assert!(status.success(), "SIGTERM exit status: {status}\n{log}");
    assert!(
        redb::ReadOnlyDatabase::open(path).is_ok(),
        "a SIGTERM stop must not leave the file needing repair"
    );
}

#[test]
fn sigkill_leaves_a_database_needing_repair() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("chipflow.redb");
    let path = path.to_str().expect("utf8 path");

    let mut server = Server::start(path);
    server.signal_and_wait("-KILL", Duration::from_secs(30));
    assert!(matches!(
        redb::ReadOnlyDatabase::open(path),
        Err(redb::DatabaseError::RepairAborted)
    ));
}

/// Connect and send `bytes`, leaving the connection open.
fn send(port: u16, bytes: &[u8]) -> TcpStream {
    let mut stream = TcpStream::connect(("127.0.0.1", port)).expect("connect");
    stream.write_all(bytes).expect("write request");
    stream
}

#[test]
fn shutdown_drain_finishes_live_requests_and_drops_stalled_ones() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("chipflow.redb");
    let path = path.to_str().expect("utf8 path");
    let mut server = Server::start(path);

    let body = "user=nobody&pass=wrong";
    let head = |len: usize| {
        format!(
            "POST /login HTTP/1.1\r\nHost: 127.0.0.1\r\n\
             Content-Type: application/x-www-form-urlencoded\r\nContent-Length: {len}\r\n\r\n"
        )
    };
    let _stalled_body = send(
        server.port,
        format!("{}{}", head(100), &body[..10]).as_bytes(),
    );
    let _stalled_head = send(server.port, b"POST /login HTTP/1.1\r\nHost: 127.0");
    let mut live = send(
        server.port,
        format!("{}{}", head(body.len()), &body[..10]).as_bytes(),
    );

    std::thread::sleep(Duration::from_millis(300));
    live.set_read_timeout(Some(Duration::from_millis(300)))
        .expect("read timeout");
    let early = live.read(&mut [0u8; 1]).map_err(|err| err.kind());
    assert!(
        matches!(early, Err(ErrorKind::WouldBlock | ErrorKind::TimedOut)),
        "the live request must still be waiting for its body: {early:?}"
    );

    let signalled = Instant::now();
    server.signal("-TERM");
    std::thread::sleep(Duration::from_secs(1));
    live.write_all(&body.as_bytes()[10..]).expect("finish body");
    live.set_read_timeout(Some(Duration::from_secs(5)))
        .expect("read timeout");
    let mut status_line = [0u8; 12];
    live.read_exact(&mut status_line)
        .expect("live request gets a response during the drain");
    assert_eq!(&status_line, b"HTTP/1.1 303");

    let (status, log) = server.wait(DRAIN + Duration::from_secs(20));
    let elapsed = signalled.elapsed();
    assert!(status.success(), "exit status: {status}\n{log}");
    assert!(
        elapsed < DRAIN + Duration::from_secs(5),
        "took {elapsed:?} to exit\n{log}"
    );
    assert!(log.contains("requests still open"), "{log}");
    assert!(
        redb::ReadOnlyDatabase::open(path).is_ok(),
        "the drain deadline must still close the file cleanly"
    );
}
