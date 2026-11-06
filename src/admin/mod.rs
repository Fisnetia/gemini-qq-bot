//! Internal admin API: routes plus bearer authentication.

pub mod auth;
pub mod routes;

pub use auth::{AdminAuth, require_bearer};
pub use routes::{AdminState, router};
