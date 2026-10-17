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
mod db;
mod models;
mod routes;

pub use db::{AppState, Db};

use std::future::{Future, IntoFuture};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use clap::Parser;
use directories::ProjectDirs;
use tokio::signal::unix::{signal, SignalKind};
use tokio::sync::Notify;

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
    let args = Args::parse();
    let database_path = args.database_path.unwrap_or_else(default_database_path);

    let Some(path) = database_path.to_str() else {
        eprintln!(
            "chipflow: database path {} is not valid UTF-8",
            database_path.display()
        );
        std::process::exit(1);
    };
    let db = match Db::connect(path) {
        Ok(db) => db,
        Err(err) => {
            eprintln!("chipflow: {err}");
            std::process::exit(1);
        }
    };

    let runtime = tokio::runtime::Runtime::new()?;
    runtime.block_on(serve(&args.host, args.port, &database_path, db))?;
    // Shutting the runtime down drops the connections the drain deadline
    // cut off, and with them the last `Db` handle, so redb closes the file
    // cleanly and needs no repair next start.
    runtime.shutdown_timeout(Duration::from_secs(1));
    println!("chipflow: shut down cleanly");
    Ok(())
}

/// Serve until SIGTERM or SIGINT, then let in-flight requests finish for up
/// to [`SHUTDOWN_DRAIN`].
async fn serve(
    host: &str,
    port: u16,
    database_path: &Path,
    db: Db,
) -> Result<(), Box<dyn std::error::Error>> {
    let app = routes::router(AppState { db });
    let listener = tokio::net::TcpListener::bind(format!("{host}:{port}")).await?;
    println!(
        "chipflow listening on http://{} (database: {})",
        listener.local_addr()?,
        database_path.display()
    );

    let draining = Arc::new(Notify::new());
    let signal = shutdown_signal()?;
    let server = axum::serve(listener, app).with_graceful_shutdown({
        let draining = Arc::clone(&draining);
        async move {
            signal.await;
            draining.notify_one();
        }
    });
    tokio::select! {
        result = server.into_future() => result?,
        () = async {
            draining.notified().await;
            tokio::time::sleep(SHUTDOWN_DRAIN).await;
        } => println!(
            "chipflow: requests still open after {}s; dropping them",
            SHUTDOWN_DRAIN.as_secs()
        ),
    }
    Ok(())
}

/// Resolve on SIGTERM (`systemctl stop`) or SIGINT (Ctrl-C).
fn shutdown_signal() -> std::io::Result<impl Future<Output = ()>> {
    let mut terminate = signal(SignalKind::terminate())?;
    let mut interrupt = signal(SignalKind::interrupt())?;
    Ok(async move {
        tokio::select! {
            _ = terminate.recv() => {}
            _ = interrupt.recv() => {}
        }
        println!("chipflow: shutting down");
    })
}
