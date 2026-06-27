//! agent-collab engine. Exposed as a library so integration tests in `tests/`
//! and the future Tauri backend can reuse it unchanged (KTD1).

pub mod app;
pub mod config;
pub mod git;
pub mod message;
pub mod session;
pub mod store;
pub mod sync;
pub mod vice;
