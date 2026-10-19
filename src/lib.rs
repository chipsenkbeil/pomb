//! Library surface for integration tests. The binary target keeps its own
//! module tree in `main.rs`; this re-exports the data-layer modules so
//! `tests/` can exercise them directly.
pub mod api_error;
pub mod auth;
pub mod db;
pub mod limit;
/// Domain types shared by the DB layer, routes, and templates.
pub mod models;

pub use db::AppState;
