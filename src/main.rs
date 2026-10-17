//! ChipFlow: a small self-hosted kanban board with pomodoro time tracking.
//!
//! Configuration: command-line flags, or the matching environment variables.
//! Everything has a sane default, so `chipflow` with no arguments just works.
//!
//! - `--host` / `HOST`: bind address (default 0.0.0.0; use 127.0.0.1 behind a reverse proxy)
//! - `--port` / `PORT`: HTTP port (default 3000)
//! - `--database-path` / `DATABASE_PATH`: database file
//!   (default `~/.local/share/chipflow/chipflow.redb`, honoring XDG variables)
//!
//! The admin account is created on the /setup page on first run.

mod auth;
mod csrf;
mod db;
mod models;
mod routes;

pub use db::{AppState, Db};

use std::future::IntoFuture;
use std::path::{Path, PathBuf};
use std::time::Duration;

use clap::Parser;
use directories::ProjectDirs;
use tokio::signal::unix::{signal, Signal, SignalKind};
use tokio::sync::watch;

/// How long in-flight requests get to finish after SIGTERM or SIGINT before
/// the server drops them; systemd's 90 s stop timeout must stay above it.
const SHUTDOWN_DRAIN: Duration = Duration::from_secs(10);

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
    // KF-472: catch SIGTERM and SIGINT first, before the database opens, so
    // a stop during startup still closes the file cleanly.
    let runtime = tokio::runtime::Runtime::new()?;
    let signals = {
        let _context = runtime.enter();
        ShutdownSignals::install()?
    };
    let args = Args::parse();
    let database_path = args.database_path.unwrap_or_else(default_database_path);

    let Some(path) = database_path.to_str() else {
        eprintln!(
            "chipflow: database path {} is not valid UTF-8",
            database_path.display()
        );
        std::process::exit(1);
    };
    // KF-507: bind before the database opens, so a busy port exits without
    // stamping a newer schema version into the file.
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

    runtime.block_on(serve(listener, &database_path, db, signals))?;
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
    mut signals: ShutdownSignals,
) -> Result<(), Box<dyn std::error::Error>> {
    let app = routes::router(AppState::new(db));
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
    let server = axum::serve(listener, app).with_graceful_shutdown({
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
