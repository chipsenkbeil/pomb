//! ChipFlow: a small self-hosted kanban board with pomodoro time tracking.
//!
//! Configuration: command-line flags, or the matching environment variables.
//! Everything has a sane default, so `chipflow` with no arguments just works.
//!
//! - `--host` / `HOST`: bind address (default 0.0.0.0; use 127.0.0.1 behind a reverse proxy)
//! - `--port` / `PORT`: HTTP port (default 3000)
//! - `--database-path` / `DATABASE_PATH`: database file
//!   (default `~/.local/share/chipflow/chipflow.redb`, honoring XDG variables)
//! - `--public-url` / `CHIPFLOW_PUBLIC_URL`: externally visible base URL, e.g.
//!   `https://chipflow.example.com` (default: taken from the request `Host` header)
//!
//! The admin account is created on the /setup page on first run.

mod api_error;
mod auth;
mod csrf;
mod db;
mod limit;
mod models;
mod routes;

use std::fmt;
use std::future::IntoFuture;
use std::net::{Ipv4Addr, Ipv6Addr, SocketAddr};
use std::path::{Path, PathBuf};
use std::time::Duration;

use clap::parser::ValueSource;
use clap::{ArgMatches, CommandFactory, FromArgMatches, Parser};
pub use db::{AppState, Db};
use directories::ProjectDirs;
use tokio::signal::unix::{Signal, SignalKind, signal};
use tokio::sync::watch;

/// How long in-flight requests get to finish after SIGTERM or SIGINT before
/// the server drops them; systemd's 90 s stop timeout must stay above it.
const SHUTDOWN_DRAIN: Duration = Duration::from_secs(10);

/// Longest DNS name, in characters.
const MAX_DNS_NAME_LEN: usize = 253;

/// Longest label between the dots of a DNS name, in characters.
const MAX_DNS_LABEL_LEN: usize = 63;

#[derive(Parser)]
#[command(
    name = "chipflow",
    about = "ChipFlow: a self-hosted kanban board with pomodoro time tracking"
)]
struct Args {
    /// IP address to bind to. Default 0.0.0.0 (all interfaces); set to
    /// 127.0.0.1 when running behind a reverse proxy like Caddy.
    #[arg(long, env = "HOST", default_value = "0.0.0.0")]
    host: String,

    /// HTTP port to listen on; 0 picks a free port.
    #[arg(long, env = "PORT", default_value_t = 3000)]
    port: u16,

    /// Path to the database file.
    #[arg(long, env = "DATABASE_PATH")]
    database_path: Option<PathBuf>,

    /// Externally visible base URL: `http://` or `https://`, then host and optional port, with no
    /// path. It sets the device-flow verification link and the OpenAPI server entry. Unset, the
    /// request `Host` header is used.
    #[arg(long, env = "CHIPFLOW_PUBLIC_URL")]
    public_url: Option<String>,
}

/// What is wrong with a public URL.
#[derive(Debug, PartialEq, Eq)]
enum PublicUrlProblem {
    Scheme,
    EmptyHost,
    UnbracketedIpv6,
    Userinfo,
    Path,
    Whitespace,
    BadHost(String),
    BadPort(String),
}

impl fmt::Display for PublicUrlProblem {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Scheme => f.write_str("must start with http:// or https://"),
            Self::EmptyHost => f.write_str("has an empty host"),
            Self::UnbracketedIpv6 => {
                f.write_str("has an IPv6 host without brackets, write it like [::1]")
            }
            Self::Userinfo => f.write_str("must not contain user information (user@)"),
            Self::Path => f.write_str("must not have a path, query or fragment"),
            Self::Whitespace => f.write_str("must not contain whitespace"),
            Self::BadHost(host) => {
                write!(
                    f,
                    "has host `{host}`, which is not a valid DNS name or IP address"
                )
            }
            Self::BadPort(port) => {
                write!(
                    f,
                    "has port `{port}`, which is not a number from 1 to 65535"
                )
            }
        }
    }
}

/// Where the public URL setting came from, for the startup error.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
enum PublicUrlSource {
    Flag,
    Env,
}

impl fmt::Display for PublicUrlSource {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Flag => "--public-url",
            Self::Env => "CHIPFLOW_PUBLIC_URL",
        })
    }
}

impl PublicUrlSource {
    fn of(matches: &ArgMatches) -> Self {
        match matches.value_source("public_url") {
            Some(ValueSource::EnvVariable) => Self::Env,
            _ => Self::Flag,
        }
    }
}

/// Trims the public URL and drops trailing slashes. A blank value counts as unset. Anything but
/// `http://` or `https://` followed by a DNS name, IPv4 or bracketed IPv6 address and an optional
/// port from 1 to 65535 is an error.
fn normalize_public_url(raw: Option<&str>) -> Result<Option<String>, PublicUrlProblem> {
    let Some(raw) = raw.map(str::trim) else {
        return Ok(None);
    };
    if raw.trim_end_matches('/').is_empty() {
        return Ok(None);
    }
    let Some((scheme, rest)) = raw
        .strip_prefix("https://")
        .map(|rest| ("https://", rest))
        .or_else(|| raw.strip_prefix("http://").map(|rest| ("http://", rest)))
    else {
        return Err(PublicUrlProblem::Scheme);
    };
    let authority = rest.trim_end_matches('/');
    if authority.contains(char::is_whitespace) {
        return Err(PublicUrlProblem::Whitespace);
    }
    if authority.contains(['/', '?', '#']) {
        return Err(PublicUrlProblem::Path);
    }
    if authority.contains('@') {
        return Err(PublicUrlProblem::Userinfo);
    }
    if !authority.starts_with('[') && authority.matches(':').count() > 1 {
        return Err(PublicUrlProblem::UnbracketedIpv6);
    }
    let (host, port) = split_host_port(authority);
    if host.is_empty() {
        return Err(PublicUrlProblem::EmptyHost);
    }
    if !is_valid_host(host) {
        return Err(PublicUrlProblem::BadHost(host.to_string()));
    }
    if let Some(port) = port
        && !is_valid_port(port)
    {
        return Err(PublicUrlProblem::BadPort(port.to_string()));
    }
    Ok(Some(format!("{scheme}{authority}")))
}

/// Splits `host`, `host:port` and `[v6]:port`; the host keeps its brackets.
fn split_host_port(authority: &str) -> (&str, Option<&str>) {
    let colon = if authority.starts_with('[') {
        authority
            .find(']')
            .and_then(|close| authority[close..].find(':').map(|at| close + at))
    } else {
        authority.find(':')
    };
    match colon {
        Some(at) => (&authority[..at], Some(&authority[at + 1..])),
        None => (authority, None),
    }
}

fn is_valid_port(port: &str) -> bool {
    port.bytes().all(|byte| byte.is_ascii_digit())
        && port.parse::<u16>().is_ok_and(|port| port >= 1)
}

fn is_valid_host(host: &str) -> bool {
    if let Some(inner) = host
        .strip_prefix('[')
        .and_then(|rest| rest.strip_suffix(']'))
    {
        return inner.parse::<Ipv6Addr>().is_ok();
    }
    if host.parse::<Ipv4Addr>().is_ok() {
        return true;
    }
    let labels: Vec<&str> = host.split('.').collect();
    let numeric_tld = labels
        .last()
        .is_some_and(|tld| tld.bytes().all(|b| b.is_ascii_digit()));
    host.len() <= MAX_DNS_NAME_LEN && !numeric_tld && labels.iter().all(|label| is_dns_label(label))
}

fn is_dns_label(label: &str) -> bool {
    (1..=MAX_DNS_LABEL_LEN).contains(&label.len())
        && !label.starts_with('-')
        && !label.ends_with('-')
        && label
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
}

/// Default database location: `~/.local/share/chipflow/chipflow.redb`
/// (or `$XDG_DATA_HOME/chipflow/chipflow.redb` when set).
fn default_database_path() -> PathBuf {
    let base = ProjectDirs::from("", "", "chipflow")
        .map(|dirs| dirs.data_dir().to_path_buf())
        .unwrap_or_else(|| PathBuf::from("."));
    base.join("chipflow.redb")
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Catch SIGTERM and SIGINT first, before the database opens, so a stop during startup still
    // closes the file cleanly.
    let runtime = tokio::runtime::Runtime::new()?;
    let signals = {
        let _context = runtime.enter();
        ShutdownSignals::install()?
    };
    let matches = Args::command().get_matches();
    let public_url_source = PublicUrlSource::of(&matches);
    let args = Args::from_arg_matches(&matches).unwrap_or_else(|err| err.exit());
    let database_path = args.database_path.unwrap_or_else(default_database_path);
    let public_url = match normalize_public_url(args.public_url.as_deref()) {
        Ok(url) => url,
        Err(problem) => {
            eprintln!("chipflow: {public_url_source} {problem}");
            std::process::exit(1);
        }
    };

    let Some(path) = database_path.to_str() else {
        eprintln!(
            "chipflow: database path {} is not valid UTF-8",
            database_path.display()
        );
        std::process::exit(1);
    };

    // Bind before the database opens, so a busy port exits without stamping a newer schema version
    // into the file.
    let listener = match runtime.block_on(tokio::net::TcpListener::bind(format!(
        "{}:{}",
        args.host, args.port
    ))) {
        Ok(listener) => listener,
        Err(err) => {
            eprintln!("chipflow: cannot bind {}:{}: {err}", args.host, args.port);
            std::process::exit(1);
        }
    };
    println!("chipflow: opening database {path}");
    let db = match Db::connect(path) {
        Ok(db) => db,
        Err(err) => {
            eprintln!("chipflow: {err}");
            std::process::exit(1);
        }
    };

    runtime.block_on(serve(listener, &database_path, db, public_url, signals))?;

    // Shutting the runtime down drops the connections the drain deadline
    // cut off, and with them the last `Db` handle, so redb closes the file
    // cleanly and needs no repair next start.
    runtime.shutdown_timeout(Duration::from_secs(1));
    println!("chipflow: shut down cleanly");
    Ok(())
}

/// Serve until SIGTERM or SIGINT, then let in-flight requests finish for up
/// to [`SHUTDOWN_DRAIN`]; a second signal drops them at once.
async fn serve(
    listener: tokio::net::TcpListener,
    database_path: &Path,
    db: Db,
    public_url: Option<String>,
    mut signals: ShutdownSignals,
) -> Result<(), Box<dyn std::error::Error>> {
    let bootstrap_token = db::BootstrapToken::from_env();
    if bootstrap_token.is_some() {
        println!("chipflow: CHIPFLOW_BOOTSTRAP_TOKEN is set; that token grants admin API access");
    }
    let app = routes::router(AppState::new(db, bootstrap_token).with_public_url(public_url));
    println!(
        "chipflow listening on http://{} (database: {})",
        listener.local_addr()?,
        database_path.display()
    );

    let (stops, mut stop_count) = watch::channel(0u32);
    tokio::spawn(async move {
        signals.recv().await;
        println!("chipflow: shutting down");
        stops.send_replace(1);
        signals.recv().await;
        stops.send_replace(2);
    });
    let server = axum::serve(
        listener,
        app.into_make_service_with_connect_info::<SocketAddr>(),
    )
    .with_graceful_shutdown({
        let mut stop_count = stop_count.clone();
        async move {
            let _ = stop_count.wait_for(|count| *count >= 1).await;
        }
    });
    tokio::select! {
        result = server.into_future() => result?,
        message = async {
            let _ = stop_count.wait_for(|count| *count >= 1).await;
            tokio::select! {
                () = tokio::time::sleep(SHUTDOWN_DRAIN) => format!(
                    "chipflow: requests still open after {}s; dropping them",
                    SHUTDOWN_DRAIN.as_secs()
                ),
                _ = stop_count.wait_for(|count| *count >= 2) => {
                    "chipflow: second stop signal; dropping open requests".to_string()
                }
            }
        } => println!("{message}"),
    }
    Ok(())
}

/// SIGTERM (`systemctl stop`) and SIGINT (Ctrl-C), registered up front so
/// none is lost before the server runs.
struct ShutdownSignals {
    terminate: Signal,
    interrupt: Signal,
}

impl ShutdownSignals {
    /// Registers both handlers; must run inside a Tokio runtime context.
    fn install() -> std::io::Result<Self> {
        Ok(Self {
            terminate: signal(SignalKind::terminate())?,
            interrupt: signal(SignalKind::interrupt())?,
        })
    }

    /// Resolves on the next SIGTERM or SIGINT, including one that arrived
    /// before the first call.
    async fn recv(&mut self) {
        tokio::select! {
            _ = self.terminate.recv() => {}
            _ = self.interrupt.recv() => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{PublicUrlProblem, PublicUrlSource, normalize_public_url};

    #[test]
    fn normalize_public_url_should_trim_and_drop_trailing_slashes() {
        assert_eq!(normalize_public_url(None), Ok(None));
        assert_eq!(normalize_public_url(Some("  ")), Ok(None));
        assert_eq!(
            normalize_public_url(Some(" https://chipflow.example.com:8443/ ")),
            Ok(Some("https://chipflow.example.com:8443".to_string()))
        );
    }

    #[test]
    fn normalize_public_url_should_accept_dns_names_and_ip_addresses() {
        for good in [
            "http://localhost",
            "https://chipflow.example.com",
            "https://a-b.example.com:65535",
            "http://192.168.1.10:3000",
            "http://[::1]:8080",
            "https://[2001:db8::1]",
            "https://xn--bcher-kva.example",
        ] {
            assert_eq!(
                normalize_public_url(Some(good)),
                Ok(Some(good.to_string())),
                "{good}"
            );
        }
    }

    #[test]
    fn normalize_public_url_should_name_the_actual_problem() {
        let problem = |raw| normalize_public_url(Some(raw)).expect_err(raw);
        assert_eq!(problem("chipflow.example.com"), PublicUrlProblem::Scheme);
        assert_eq!(
            problem("ftp://chipflow.example.com"),
            PublicUrlProblem::Scheme
        );
        assert_eq!(problem("https://"), PublicUrlProblem::EmptyHost);
        assert_eq!(problem("https:///"), PublicUrlProblem::EmptyHost);
        assert_eq!(problem("http:////"), PublicUrlProblem::EmptyHost);
        assert_eq!(problem("https://::1"), PublicUrlProblem::UnbracketedIpv6);
        assert_eq!(
            problem("https://2001:db8::1:8080"),
            PublicUrlProblem::UnbracketedIpv6
        );
        assert_eq!(problem("https://:8080"), PublicUrlProblem::EmptyHost);
        assert_eq!(problem("https://example.com/app"), PublicUrlProblem::Path);
        assert_eq!(problem("https://example.com?x=1"), PublicUrlProblem::Path);
        assert_eq!(problem("https://example.com#top"), PublicUrlProblem::Path);
        assert_eq!(
            problem("https://user@example.com"),
            PublicUrlProblem::Userinfo
        );
        assert_eq!(
            problem("https://exa mple.com"),
            PublicUrlProblem::Whitespace
        );
        for bad_host in [
            "<x>",
            "-a.example",
            "a-.example",
            "a..example",
            "under_score.example",
            "example.123",
            "999.1.1.1",
            "[::1",
            "[not-v6]",
            "a.b.",
            &"a".repeat(64),
        ] {
            let raw = format!("https://{bad_host}");
            assert_eq!(
                normalize_public_url(Some(&raw)),
                Err(PublicUrlProblem::BadHost(bad_host.to_string())),
                "{bad_host}"
            );
        }
        for bad_port in ["notaport", "0", "65536", "-1", "+80", "", "80a"] {
            let raw = format!("https://example.com:{bad_port}");
            assert_eq!(
                normalize_public_url(Some(&raw)),
                Err(PublicUrlProblem::BadPort(bad_port.to_string())),
                "{bad_port}"
            );
        }
        let long_name = format!("https://{}", ["a".repeat(60).as_str(); 5].join("."));
        assert!(normalize_public_url(Some(&long_name)).is_err());
    }

    #[test]
    fn public_url_problem_should_read_as_a_sentence_after_the_source() {
        let message = |raw| {
            let problem = normalize_public_url(Some(raw)).expect_err(raw);
            format!("{} {problem}", PublicUrlSource::Env)
        };
        assert_eq!(
            message("https://host:notaport"),
            "CHIPFLOW_PUBLIC_URL has port `notaport`, which is not a number from 1 to 65535"
        );
        assert_eq!(
            message("https://<x>"),
            "CHIPFLOW_PUBLIC_URL has host `<x>`, which is not a valid DNS name or IP address"
        );
        assert_eq!(
            format!("{} {}", PublicUrlSource::Flag, PublicUrlProblem::EmptyHost),
            "--public-url has an empty host"
        );
    }
}
