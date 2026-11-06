//! Bearer-token authentication for the internal admin API.
//!
//! The internal API is reachable only from the container's own loopback interface (the
//! host maps `127.0.0.1:18081` → container `8080`), and it additionally requires a
//! random bearer token that the 8002 gateway reads from a protected file and never
//! exposes to the browser. Two independent properties, both cheap:
//!
//! * the token is compared in constant time, so a timing oracle cannot recover it,
//! * the token is never logged, echoed or serialised into a response body.

use std::path::Path;

use axum::body::Body;
use axum::extract::State;
use axum::http::{Request, StatusCode, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use sha2::{Digest, Sha256};

use crate::config::validate_internal_token;
use crate::error::BotError;

/// Configuration for the admin surface.
#[derive(Debug, Clone)]
pub struct AdminAuth {
    /// SHA-256 of the expected token. The plaintext is dropped at construction so a
    /// `Debug` dump cannot leak it.
    expected_hash: [u8; 32],
}

impl AdminAuth {
    /// Build from a plaintext token, rejecting anything too short to be random.
    pub fn new(token: &str) -> Result<Self, BotError> {
        validate_internal_token(token)?;
        Ok(Self {
            expected_hash: hash(token),
        })
    }

    /// Build from a file, which is how the deployed bot receives it.
    pub fn from_file(path: &Path) -> Result<Self, BotError> {
        let raw = std::fs::read_to_string(path).map_err(BotError::Io)?;
        Self::new(raw.trim())
    }

    /// Constant-time comparison against the expected token.
    pub fn verify(&self, presented: &str) -> bool {
        let presented_hash = hash(presented);
        let mut diff = 0u8;
        for (a, b) in presented_hash.iter().zip(self.expected_hash.iter()) {
            diff |= a ^ b;
        }
        diff == 0
    }
}

fn hash(token: &str) -> [u8; 32] {
    let mut out = [0u8; 32];
    out.copy_from_slice(&Sha256::digest(token.as_bytes()));
    out
}

/// Extract a bearer token from the `Authorization` header.
pub fn bearer_token(headers: &axum::http::HeaderMap) -> Option<&str> {
    let value = headers.get(header::AUTHORIZATION)?.to_str().ok()?;
    let rest = value.strip_prefix("Bearer ").or_else(|| {
        // Some clients lowercase the scheme.
        value.strip_prefix("bearer ")
    })?;
    let token = rest.trim();
    if token.is_empty() { None } else { Some(token) }
}

/// axum middleware enforcing the bearer token on every internal route.
pub async fn require_bearer(
    State(auth): State<AdminAuth>,
    req: Request<Body>,
    next: Next,
) -> Response {
    match bearer_token(req.headers()) {
        Some(token) if auth.verify(token) => next.run(req).await,
        _ => (
            StatusCode::UNAUTHORIZED,
            [(header::CONTENT_TYPE, "application/json")],
            r#"{"error":"unauthorized"}"#,
        )
            .into_response(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const GOOD: &str = "0123456789abcdef0123456789abcdef";

    #[test]
    fn accepts_the_exact_token() {
        let auth = AdminAuth::new(GOOD).expect("应构造成功");
        assert!(auth.verify(GOOD));
    }

    #[test]
    fn rejects_near_misses() {
        let auth = AdminAuth::new(GOOD).expect("应构造成功");
        for bad in [
            "0123456789abcdef0123456789abcdee",
            "0123456789abcdef0123456789abcdef ",
            "",
            "short",
            "0123456789abcdef0123456789abcdeF",
        ] {
            assert!(!auth.verify(bad), "{bad:?} 必须被拒绝");
        }
    }

    #[test]
    fn refuses_weak_tokens() {
        assert!(AdminAuth::new("short").is_err());
        assert!(AdminAuth::new("").is_err());
        // Whitespace means it was probably read badly; refuse rather than guess.
        assert!(AdminAuth::new("0123456789abcdef 0123456789abcdef").is_err());
    }

    #[test]
    fn debug_output_does_not_leak_the_token() {
        let auth = AdminAuth::new(GOOD).expect("应构造成功");
        let rendered = format!("{auth:?}");
        assert!(!rendered.contains(GOOD));
    }

    #[test]
    fn bearer_parsing_is_strict() {
        let mut headers = axum::http::HeaderMap::new();
        headers.insert(header::AUTHORIZATION, "Bearer abcdef".parse().expect("值"));
        assert_eq!(bearer_token(&headers), Some("abcdef"));

        headers.insert(header::AUTHORIZATION, "Basic abcdef".parse().expect("值"));
        assert_eq!(bearer_token(&headers), None);

        headers.insert(header::AUTHORIZATION, "Bearer    ".parse().expect("值"));
        assert_eq!(bearer_token(&headers), None);
    }

    #[test]
    fn token_file_is_trimmed() {
        let dir = tempfile::tempdir().expect("tmp");
        let path = dir.path().join("token");
        std::fs::write(&path, format!("{GOOD}\n")).expect("写");
        let auth = AdminAuth::from_file(&path).expect("应成功");
        assert!(auth.verify(GOOD));
    }
}
