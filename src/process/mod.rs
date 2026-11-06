//! Child process supervision (Mihomo).

pub mod mihomo;
pub mod reaper;

pub use mihomo::{Mihomo, MihomoConfig, ProxyError, ProxyHealth};
pub use reaper::spawn_reaper;
