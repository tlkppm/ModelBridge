//! Wires the Cursor-facing API routes.

pub mod bidi;
mod diagnostics;
mod handlers;
mod health;
pub mod proxy;
mod run_sse;

pub use handlers::router;
