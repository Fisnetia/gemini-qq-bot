//! Optional outbound event webhook: lifecycle events POSTed as JSON to
//! an operator-configured URL, for external systems to subscribe to.
//!
//! Sender only, best-effort by design: sends are spawned and time-boxed,
//! failures are logged and dropped. A dead or slow receiver must never
//! stall a turn or a pacing decision — that is the whole contract, and
//! it is also why there are no retries: a subscriber that missed an
//! event reconciles from the admin API, not from us.
//!
//! When a secret is configured, each body is signed with HMAC-SHA256
//! and sent as `X-Webhook-Signature: sha256=<hex>`, so receivers can
//! authenticate deliveries. The full contract is in docs/EXTENDING.md.

use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde_json::{Value, json};
use sha2::{Digest, Sha256};

/// Event names on the wire. Subscribers filter on these.
pub mod events {
    /// A batch finished: the agent replied (or deliberately stayed silent).
    pub const TURN_COMPLETED: &str = "turn.completed";
    /// A batch failed in a way that is safe to retry later.
    pub const TURN_DEFERRED: &str = "turn.deferred";
    /// A batch ended with unknown conversation state; the lane halted.
    pub const TURN_FAILED: &str = "turn.failed";
    /// The pacing tier actually in force changed.
    pub const PACING_TIER_CHANGED: &str = "pacing.tier_changed";
}

#[derive(Clone)]
pub struct EventWebhook {
    client: reqwest::Client,
    url: String,
    secret: Option<String>,
}

impl EventWebhook {
    /// Build from config values; `None` when the URL is unset or blank
    /// (the feature is off — the URL being set is the switch).
    pub fn from_parts(url: Option<&str>, secret: Option<String>) -> Option<Arc<Self>> {
        let url = url?.trim();
        if url.is_empty() {
            return None;
        }
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(5))
            .build()
            .ok()?;
        Some(Arc::new(Self {
            client,
            url: url.to_string(),
            secret: secret.filter(|s| !s.trim().is_empty()),
        }))
    }

    /// Queue one event for delivery and return immediately.
    pub fn fire(&self, event: &str, group_id: Option<&str>, detail: Value) {
        let payload = json!({
            "event": event,
            "ts": unix_now(),
            "group_id": group_id,
            "detail": detail,
        });
        let body = payload.to_string();
        let signature = self
            .secret
            .as_deref()
            .map(|s| hmac_sha256_hex(s.as_bytes(), body.as_bytes()));
        let this = self.clone();
        let event = event.to_string();
        tokio::spawn(async move {
            let mut req = this
                .client
                .post(&this.url)
                .header("Content-Type", "application/json")
                .header("X-Webhook-Event", &event)
                .body(body);
            if let Some(sig) = signature {
                req = req.header("X-Webhook-Signature", format!("sha256={sig}"));
            }
            match req.send().await {
                Ok(resp) if resp.status().is_success() => {}
                Ok(resp) => {
                    tracing::debug!(event, status = %resp.status(), "webhook 交付未成功")
                }
                Err(err) => tracing::debug!(event, %err, "webhook 交付失败"),
            }
        });
    }
}

fn unix_now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// HMAC-SHA256 (RFC 2104) over the raw body, hex-encoded.
fn hmac_sha256_hex(key: &[u8], msg: &[u8]) -> String {
    const BLOCK: usize = 64;
    let mut key_block = [0u8; BLOCK];
    if key.len() > BLOCK {
        key_block[..32].copy_from_slice(&Sha256::digest(key));
    } else {
        key_block[..key.len()].copy_from_slice(key);
    }
    let mut inner = Sha256::new();
    inner.update(key_block.iter().map(|b| b ^ 0x36).collect::<Vec<u8>>());
    inner.update(msg);
    let inner_hash = inner.finalize();
    let mut outer = Sha256::new();
    outer.update(key_block.iter().map(|b| b ^ 0x5c).collect::<Vec<u8>>());
    outer.update(inner_hash);
    outer
        .finalize()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hmac_matches_rfc4231_case_1() {
        let key = [0x0b_u8; 20];
        assert_eq!(
            hmac_sha256_hex(&key, b"Hi There"),
            "b0344c61d8db38535ca8afceaf0bf12b881dc200c9833da726e9376c2e32cff7"
        );
    }

    #[test]
    fn blank_url_disables() {
        assert!(EventWebhook::from_parts(None, None).is_none());
        assert!(EventWebhook::from_parts(Some("  "), None).is_none());
        assert!(EventWebhook::from_parts(Some("http://127.0.0.1:9/hook"), None).is_some());
    }

    #[tokio::test]
    async fn fire_posts_the_event_json_with_signature() {
        use tokio::io::AsyncReadExt as _;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let addr = listener.local_addr().expect("addr");
        let hook = EventWebhook::from_parts(
            Some(&format!("http://{addr}/hook")),
            Some("s3cret".to_string()),
        )
        .expect("webhook");
        hook.fire(
            events::TURN_COMPLETED,
            Some("900000001"),
            json!({"batch_id": "b1"}),
        );
        let (mut stream, _) = tokio::time::timeout(Duration::from_secs(5), listener.accept())
            .await
            .expect("a connection arrives")
            .expect("accept");
        let mut request = Vec::new();
        let mut chunk = [0u8; 4096];
        for _ in 0..4 {
            let n = tokio::time::timeout(Duration::from_secs(2), stream.read(&mut chunk))
                .await
                .expect("read within timeout")
                .expect("read");
            if n == 0 {
                break;
            }
            request.extend_from_slice(&chunk[..n]);
            if String::from_utf8_lossy(&request).contains("\"group_id\"") {
                break;
            }
        }
        let request = String::from_utf8_lossy(&request).to_lowercase();
        assert!(request.contains("post /hook"), "{request}");
        assert!(
            request.contains("x-webhook-event: turn.completed"),
            "{request}"
        );
        assert!(
            request.contains("x-webhook-signature: sha256="),
            "{request}"
        );
        assert!(
            request.contains("\"event\":\"turn.completed\""),
            "{request}"
        );
        assert!(request.contains("\"group_id\":\"900000001\""), "{request}");
    }
}
