//! Library surface for integration tests. The binary target keeps its own
//! module tree in `main.rs`; this re-exports the data-layer modules so
//! `tests/` can exercise them directly.
pub mod api_error;
pub mod auth;
/// Consistent database snapshots, their rotation and the schedule that takes them.
pub mod backup;
pub mod db;
pub mod disk;
pub mod limit;
/// Domain types shared by the DB layer, routes, and templates.
pub mod models;
mod passkey;
/// The catalog of token permissions and the presets that group them.
pub mod permissions;

pub use db::AppState;
