//! Gemini QQ group bot.
//!
//! The crate exposes a library so that integration tests can drive the store, the
//! batching pipeline and the adapters without starting the network supervisor.

pub mod admin;
pub mod agent;
pub mod app;
pub mod config;
pub mod error;
pub mod health;
pub mod integrations;
pub mod lite;
pub mod mcp;
pub mod media;
pub mod memory;
pub mod onebot;
pub mod pipeline;
pub mod process;
pub mod state;
pub mod store;

/// Current unix time in whole seconds.
pub fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}
