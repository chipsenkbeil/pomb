//! Pomb: a small self-hosted kanban board with pomodoro time tracking.
//!
//! Configuration: command-line flags, or the matching environment variables.
//! Everything has a sane default, so `pomb` with no arguments just works.
//!
//! - `--auth` / `POMB_AUTH`: browser sign-in, `none` (default, loopback only) or `passkey`
//! - `--mode` / `POMB_MODE`: `single` (default, one account) or `multi` (the admin plus the
//!   members it invites or approves), which needs `--auth passkey`
//! - `--host` / `HOST`: bind address (default 127.0.0.1)
//! - `--port` / `PORT`: HTTP port (default 3000)
//! - `--database-path` / `DATABASE_PATH`: database file
//!   (default `~/.local/share/pomb/pomb.redb`, honoring XDG variables)
//! - `--backup-dir` / `POMB_BACKUP_DIR`: where database backups are written
//!   (default: a `backups` directory beside the database file). Whether backups run, how often and
//!   how many are kept are settings in the browser, off until the owner turns them on.
//! - `--public-url` / `POMB_PUBLIC_URL`: externally visible base URL, e.g.
//!   `https://pomb.example.com` (default: taken from the request `Host` header). `--auth passkey`
//!   needs it, and its host is the passkey relying party.
//! - `--trust-proxy` / `POMB_TRUST_PROXY`: which proxy on this host names the client, `none`
//!   (default), `cloudflare` or `forwarded`
//!
//! `pomb archive --database-path FILE --out DIR` writes every board of every account in the
//! database file to `DIR/<account handle>/<board name>-<board id>.tar.gz`, each a `board.json`
//! plus the board's attachment files. It opens the file read-only and needs no server, and it
//! stops with an error while a server holds the file.
//!
//! Without `--auth`, a public URL whose host is not loopback stops the start, since a proxy in
//! front would open the board to everyone it serves, and `--auth none` with a `--trust-proxy` other
//! than `none` stops it for the same reason. Under `--auth passkey` the owner registers the first
//! passkey on the /setup page with the setup code the server prints.

mod api_error;
mod archive;
mod auth;
mod backup;
mod csrf;
mod db;
mod disk;
mod limit;
mod markdown;
mod models;
mod passkey;
mod permissions;
mod routes;
mod taskjson;

use std::fmt;
use std::future::IntoFuture;
use std::net::{Ipv4Addr, Ipv6Addr, SocketAddr};
use std::path::{Path, PathBuf};
use std::time::Duration;

use auth::{AuthChoice, AuthMode, SignIn};
use clap::parser::ValueSource;
use clap::{ArgMatches, CommandFactory, FromArgMatches, Parser, Subcommand};
pub use db::{AppState, Db};
use db::{ModeSwitch, OfflineDb};
use directories::ProjectDirs;
use limit::TrustProxy;
use models::{InstanceMode, count_noun};
use tokio::signal::unix::{Signal, SignalKind, signal};
use tokio::sync::watch;

/// How long in-flight requests get to finish after SIGTERM or SIGINT before
/// the server drops them; systemd's 90 s stop timeout must stay above it.
const SHUTDOWN_DRAIN: Duration = Duration::from_secs(10);

/// Longest DNS name, in characters.
const MAX_DNS_NAME_LEN: usize = 253;

/// Longest label between the dots of a DNS name, in characters.
const MAX_DNS_LABEL_LEN: usize = 63;

/// The `--help` text after the options: what each sign-in mode means and how to recover.
const AFTER_LONG_HELP: &str = "\
Sign-in:
  --auth none     No sign-in. Every browser request is the owner's, so pomb refuses to start
                  unless --host is loopback (127.0.0.0/8, ::1 or localhost) and --trust-proxy is
                  none. API tokens still need to be valid. It is the default, but without --auth
                  pomb also refuses a POMB_PUBLIC_URL whose host is not loopback: name --auth none
                  to serve such a URL through a trusted network, or use --auth passkey.
  --auth passkey  The owner signs in with a passkey. POMB_PUBLIC_URL is required, and its host
                  is the passkey relying party. While no passkey exists, pomb prints a one-time
                  setup code at each start, and /setup registers the first passkey with it.

Accounts:
  --mode single   One account, the admin, which owns every board. It is the default.
  --mode multi    The admin invites members from Settings > Invites, and each account sees only
                  its own boards, tasks and tokens. It needs --auth passkey. Switching a single
                  instance to multi keeps its data with the admin. Switching back is refused while
                  more than one account exists.

Lost every passkey:
  1. Restart with --auth none and --trust-proxy none on a loopback --host, keeping
     POMB_PUBLIC_URL.
  2. Open Settings > Passkeys at the public URL and add a passkey. Until step 3 anyone who
     reaches that URL is the owner, so keep this window short.
  3. Restart with --auth passkey.
  Under --mode multi an account instead recovers at /recover with its handle and recovery code.";

#[derive(Parser)]
#[command(
    name = "pomb",
    about = "Pomb: a self-hosted kanban board with pomodoro time tracking",
    after_long_help = AFTER_LONG_HELP
)]
struct Args {
    /// How browsers sign in: `none` (no sign-in, loopback only) or `passkey`. API tokens work in
    /// both.
    #[arg(long, env = "POMB_AUTH", value_enum, default_value = "none")]
    auth: AuthMode,

    /// How many accounts the instance serves: `single` (one, the admin) or `multi` (the admin plus
    /// the members it invites). `multi` needs `--auth passkey`.
    #[arg(long, env = "POMB_MODE", value_enum, default_value = "single")]
    mode: InstanceMode,

    /// IP address or host name to bind to; `--auth none` allows only loopback.
    #[arg(long, env = "HOST", default_value = "127.0.0.1")]
    host: String,

    /// HTTP port to listen on; 0 picks a free port.
    #[arg(long, env = "PORT", default_value_t = 3000)]
    port: u16,

    /// Path to the database file.
    #[arg(long, env = "DATABASE_PATH", global = true)]
    database_path: Option<PathBuf>,

    /// Directory that database backups are written to, by default a `backups` directory beside
    /// the database file. The schedule is a setting in the browser, off until turned on.
    #[arg(long, env = "POMB_BACKUP_DIR")]
    backup_dir: Option<PathBuf>,

    /// Externally visible base URL: `http://` or `https://`, then host and optional port, with no
    /// path. It sets the device-flow verification link and the OpenAPI server entry. Unset, the
    /// request `Host` header is used. `--auth passkey` needs it, and its host is the passkey
    /// relying party.
    #[arg(long, env = "POMB_PUBLIC_URL")]
    public_url: Option<String>,

    /// Which proxy on this host names the client of each request, for the per-address limits:
    /// `none` (the peer address), `cloudflare` (`CF-Connecting-IP` from cloudflared) or
    /// `forwarded` (the last `X-Forwarded-For` entry from a reverse proxy). The header counts
    /// only from a loopback peer.
    #[arg(long, env = "POMB_TRUST_PROXY", value_enum, default_value = "none")]
    trust_proxy: TrustProxy,

    /// A command to run instead of the server.
    #[command(subcommand)]
    verb: Option<Verb>,
}

/// The commands that run instead of the server.
#[derive(Subcommand)]
enum Verb {
    /// Writes every board of every account in a database file as a `.tar.gz` of `board.json` and
    /// the board's attachment files, without a server.
    Archive(ArchiveArgs),
}

/// What `pomb archive` reads and writes.
#[derive(clap::Args)]
struct ArchiveArgs {
    /// Directory the archives go to, as `<account handle>/<board name>-<board id>.tar.gz`.
    #[arg(long)]
    out: PathBuf,
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
            Self::Env => "POMB_PUBLIC_URL",
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

/// Default database location: `~/.local/share/pomb/pomb.redb`
/// (or `$XDG_DATA_HOME/pomb/pomb.redb` when set).
fn default_database_path() -> PathBuf {
    let base = ProjectDirs::from("", "", "pomb")
        .map(|dirs| dirs.data_dir().to_path_buf())
        .unwrap_or_else(|| PathBuf::from("."));
    base.join("pomb.redb")
}

/// Writes every board in the database file `args` names to the directory it names, and exits with
/// status 1 and a message when the file cannot be read or an archive cannot be written.
fn run_archive(database_path: Option<PathBuf>, args: ArchiveArgs) {
    let database_path = database_path.unwrap_or_else(default_database_path);
    let Some(path) = database_path.to_str() else {
        eprintln!(
            "pomb: database path {} is not valid UTF-8",
            database_path.display()
        );
        std::process::exit(1);
    };
    let db = match OfflineDb::open(path) {
        Ok(db) => db,
        Err(err) => {
            eprintln!("pomb: {err}");
            std::process::exit(1);
        }
    };
    match archive::archive_database(&db, &args.out, chrono::Utc::now()) {
        Ok(count) => println!(
            "pomb: wrote {} to {}",
            count_noun(count as u64, "board archive"),
            args.out.display()
        ),
        Err(err) => {
            eprintln!("pomb: {err}");
            std::process::exit(1);
        }
    }
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let matches = Args::command().get_matches();
    let public_url_source = PublicUrlSource::of(&matches);
    let args = Args::from_arg_matches(&matches).unwrap_or_else(|err| err.exit());
    if let Some(Verb::Archive(archive)) = args.verb {
        run_archive(args.database_path, archive);
        return Ok(());
    }

    // Catch SIGTERM and SIGINT first, before the database opens, so a stop during startup still
    // closes the file cleanly.
    let runtime = tokio::runtime::Runtime::new()?;
    let signals = {
        let _context = runtime.enter();
        ShutdownSignals::install()?
    };
    let database_path = args.database_path.unwrap_or_else(default_database_path);
    let public_url = match normalize_public_url(args.public_url.as_deref()) {
        Ok(url) => url,
        Err(problem) => {
            eprintln!("pomb: {public_url_source} {problem}");
            std::process::exit(1);
        }
    };

    let auth = match matches.value_source("auth") {
        Some(ValueSource::DefaultValue) | None => AuthChoice::Default,
        Some(_) => AuthChoice::Chosen(args.auth),
    };
    let sign_in = match SignIn::new(auth, &args.host, public_url.as_deref())
        .and_then(|sign_in| sign_in.check_mode(args.mode).map(|()| sign_in))
        .and_then(|sign_in| sign_in.check_proxy(args.trust_proxy).map(|()| sign_in))
    {
        Ok(sign_in) => sign_in,
        Err(problem) => {
            eprintln!("pomb: {problem}");
            std::process::exit(1);
        }
    };

    let bootstrap_token = db::BootstrapToken::from_env();
    if bootstrap_token.is_some() && args.mode == InstanceMode::Multi {
        eprintln!(
            "pomb: POMB_BOOTSTRAP_TOKEN would be a standing admin credential, so --mode multi \
             refuses it; unset it"
        );
        std::process::exit(1);
    }

    let Some(path) = database_path.to_str() else {
        eprintln!(
            "pomb: database path {} is not valid UTF-8",
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
            eprintln!("pomb: cannot bind {}:{}: {err}", args.host, args.port);
            std::process::exit(1);
        }
    };
    println!("pomb: opening database {path}");
    let db = match Db::connect(path) {
        Ok(db) => db,
        Err(err) => {
            eprintln!("pomb: {err}");
            std::process::exit(1);
        }
    };
    match db.apply_mode(args.mode) {
        Ok(ModeSwitch::Kept) => {}
        Ok(ModeSwitch::Switched { from }) => {
            println!(
                "pomb: switched the instance from {from} mode to {} mode",
                args.mode
            );
        }
        Ok(ModeSwitch::Refused { accounts }) => {
            eprintln!(
                "pomb: --mode single serves one account, but this instance has {accounts}; \
                 keep --mode multi, or delete the other accounts first"
            );
            std::process::exit(1);
        }
        Err(err) => {
            eprintln!("pomb: cannot store the instance mode: {err}");
            std::process::exit(1);
        }
    }

    let config = ServeConfig {
        public_url,
        sign_in,
        mode: args.mode,
        backup_dir: args.backup_dir,
        bootstrap_token,
        trust_proxy: args.trust_proxy,
    };
    runtime.block_on(serve(listener, &database_path, db, config, signals))?;

    // Shutdown drops the last `Db` handle with the cut-off connections
    // That lets redb close the file cleanly, so the next start needs no repair
    runtime.shutdown_timeout(Duration::from_secs(1));
    println!("pomb: shut down cleanly");
    Ok(())
}

/// What the server runs with beyond its listener and database.
struct ServeConfig {
    /// The normalized public URL.
    public_url: Option<String>,
    /// The checked sign-in mode.
    sign_in: SignIn,
    /// The stored instance mode.
    mode: InstanceMode,
    /// Where backups go when `--backup-dir` names a directory.
    backup_dir: Option<PathBuf>,
    /// The token `POMB_BOOTSTRAP_TOKEN` sets, which single mode accepts.
    bootstrap_token: Option<db::BootstrapToken>,
    /// Which proxy on this host names the client.
    trust_proxy: TrustProxy,
}

/// Serve until SIGTERM or SIGINT, then let in-flight requests finish for up
/// to [`SHUTDOWN_DRAIN`]; a second signal drops them at once.
async fn serve(
    listener: tokio::net::TcpListener,
    database_path: &Path,
    db: Db,
    config: ServeConfig,
    mut signals: ShutdownSignals,
) -> Result<(), Box<dyn std::error::Error>> {
    let bootstrap_token = config.bootstrap_token;
    if bootstrap_token.is_some() {
        println!(
            "pomb: POMB_BOOTSTRAP_TOKEN is set; that token acts as the admin account with full \
             API access"
        );
    }
    let mode = config.sign_in.mode();
    if let SignIn::Passkey(relying_party) = &config.sign_in
        && db.passkey_count()? == 0
    {
        relying_party.announce_setup_code();
    }
    let mut state = AppState::new(db.clone(), bootstrap_token, config.sign_in)
        .with_mode(config.mode)
        .with_trust_proxy(config.trust_proxy);
    if let Some(dir) = config.backup_dir {
        state = state.with_backup_dir(dir);
    }
    println!("pomb: backups go to {}", state.backups.dir().display());
    tokio::spawn(backup::run_schedule(db.clone(), state.backups.clone()));
    tokio::spawn(run_upkeep(db.clone()));
    tokio::spawn(state.disk.clone().run_sampler());
    let app = routes::router(state.with_public_url(config.public_url));
    println!(
        "pomb listening on http://{} with {mode} in {} mode (database: {})",
        listener.local_addr()?,
        config.mode,
        database_path.display()
    );

    let (stops, mut stop_count) = watch::channel(0u32);
    tokio::spawn(async move {
        signals.recv().await;
        println!("pomb: shutting down");
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
                    "pomb: requests still open after {}s; dropping them",
                    SHUTDOWN_DRAIN.as_secs()
                ),
                _ = stop_count.wait_for(|count| *count >= 2) => {
                    "pomb: second stop signal; dropping open requests".to_string()
                }
            }
        } => println!("{message}"),
    }
    let flushed = tokio::task::spawn_blocking(move || {
        if let Err(error) = db.flush_activity() {
            eprintln!("pomb: could not record when accounts were last seen: {error}");
        }
    })
    .await;
    if let Err(error) = flushed {
        eprintln!("pomb: could not record when accounts were last seen: {error}");
    }
    Ok(())
}

/// How often the server writes when accounts and boards were last used, ends expired sessions,
/// deletes expired requests to join, and checks whether the daily cleanup of unused boards is due.
const UPKEEP_INTERVAL_SECS: u64 = 300;

/// Writes when accounts and boards were last used, ends the sessions past their age, deletes the
/// requests to join that waited longer than [`Db::PENDING_REQUEST_DAYS`] and runs the daily
/// cleanup of unused boards, every [`UPKEEP_INTERVAL_SECS`] until the runtime stops. The work runs
/// on a blocking thread, since it holds the database writer.
async fn run_upkeep(db: Db) {
    let mut ticker = tokio::time::interval(Duration::from_secs(UPKEEP_INTERVAL_SECS));
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        ticker.tick().await;
        let db = db.clone();
        if let Err(error) = tokio::task::spawn_blocking(move || upkeep(&db)).await {
            eprintln!("pomb: upkeep failed: {error}");
        }
    }
}

/// Writes when accounts and boards were last used, ends expired sessions, deletes the expired
/// requests to join and runs the cleanup of unused boards when it is due, once.
fn upkeep(db: &Db) {
    if let Err(error) = db.flush_activity() {
        eprintln!("pomb: could not record when accounts were last seen: {error}");
    }
    let now = chrono::Utc::now().timestamp();
    match db.prune_sessions(now) {
        Ok(0) => {}
        Ok(ended) => println!("pomb: ended {}", count_noun(ended, "expired session")),
        Err(error) => eprintln!("pomb: could not end expired sessions: {error}"),
    }
    let expired = db.expire_pending_requests(now, |handle| {
        println!("pomb: deleted the expired request to join from {handle}");
    });
    if let Err(error) = expired {
        eprintln!("pomb: could not delete expired requests to join: {error}");
    }
    match db.prune_unused_boards(now) {
        Ok(Some(status)) => println!(
            "pomb: deleted {} and {} left without a board",
            count_noun(status.boards_deleted, "unused board"),
            count_noun(status.accounts_deleted, "account")
        ),
        Ok(None) => {}
        Err(error) => eprintln!("pomb: could not delete unused boards: {error}"),
    }
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
            normalize_public_url(Some(" https://pomb.example.com:8443/ ")),
            Ok(Some("https://pomb.example.com:8443".to_string()))
        );
    }

    #[test]
    fn normalize_public_url_should_accept_dns_names_and_ip_addresses() {
        for good in [
            "http://localhost",
            "https://pomb.example.com",
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
        assert_eq!(problem("pomb.example.com"), PublicUrlProblem::Scheme);
        assert_eq!(problem("ftp://pomb.example.com"), PublicUrlProblem::Scheme);
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
            "POMB_PUBLIC_URL has port `notaport`, which is not a number from 1 to 65535"
        );
        assert_eq!(
            message("https://<x>"),
            "POMB_PUBLIC_URL has host `<x>`, which is not a valid DNS name or IP address"
        );
        assert_eq!(
            format!("{} {}", PublicUrlSource::Flag, PublicUrlProblem::EmptyHost),
            "--public-url has an empty host"
        );
    }
}
