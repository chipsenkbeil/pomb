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

use std::path::PathBuf;

use clap::Parser;
use directories::ProjectDirs;

use crate::db::Db;

/// Shared application state.
#[derive(Clone)]
pub struct AppState {
    pub db: Db,
}

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

    /// HTTP port to listen on.
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

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args = Args::parse();
    let database_path = args.database_path.unwrap_or_else(default_database_path);

    let db = Db::connect(
        database_path
            .to_str()
            .ok_or("database path is not valid UTF-8")?,
    )?;

    let state = AppState { db };
    let app = routes::router(state);

    let listener = tokio::net::TcpListener::bind(format!("{}:{}", args.host, args.port)).await?;
    println!(
        "chipflow listening on http://{}:{} (database: {})",
        args.host,
        args.port,
        database_path.display()
    );
    axum::serve(listener, app).await?;
    Ok(())
}
