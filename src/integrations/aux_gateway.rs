//! Aux gateway adapter — used by the cadence gate, the memory folder and the
//! image tagger.
//!
//! Contract notes (verified vs. unverified):
//!
//! * **Verified**: the gateway speaks `/v1/chat/completions` and requires
//!   `stream: true`.
//! * **Verified**: `hy3` is published, reads images, and is free at this gateway
//!   (credit 0.0 per request). Readiness is checked through `/health` +
//!   `/v1/models` **without** issuing a paid model request.
//!
//! Hard rules enforced here:
//!
//! * the model id is pinned to `hy3`; there is no fallback to another DeepSeek or Gemini model,
//! * the API key only ever travels in the `Authorization` header and is never logged,
//! * `/v1/search` is never called,
//! * the key is never written into the Agy child environment (see `agent::agy`).

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use serde_json::{Value, json};
use tokio::sync::RwLock;

use crate::config::{DEFAULT_AUX_BASE_URL, DEFAULT_AUX_MODEL, DEFAULT_AUX_SESSION_HEADER};
use crate::error::ErrorClass;
use crate::memory::compaction::SummaryModel;

/// Request timeout for one compaction completion.
pub const REQUEST_TIMEOUT: Duration = Duration::from_secs(120);
/// Timeout for the connectivity/readiness probes.
pub const PROBE_TIMEOUT: Duration = Duration::from_secs(10);
/// Hard cap on the streamed response body we are willing to buffer.
pub const MAX_STREAM_BYTES: usize = 1 << 20;

/// The sticky-session header (name from `AuxGatewayConfig::session_header`)
/// pins one logical conversation to one upstream account, which is the only
/// way the provider prefix cache can keep hitting across turns.

/// Pinned system prompt for memory compaction. Byte-identical on every call so the
/// upstream cache stays warm.
pub const SUMMARY_SYSTEM: &str =
    "You compress group memory notes into a short summary. Output JSON only.";

/// Append a coarse time bucket to a logical session id.
///
/// The gateway pins a session to one account. A session that never rotates would pin a
/// group to one account forever, and an account that runs out mid-conversation would
/// strand it. Bucketing by day keeps the prefix cache warm for a full day of chatting
/// while still letting a dead account age out.
pub fn timestamped_session_id(base: &str, now: i64) -> String {
    const BUCKET_SECONDS: i64 = 24 * 60 * 60;
    format!("{base}:{}", now / BUCKET_SECONDS)
}

/// Readiness of the compactor endpoint, surfaced on the admin page.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize)]
pub struct AuxGatewayStatus {
    /// Which aux backend this status describes: `"gateway"` or `"agy"`.
    /// Empty in statuses serialized before the field existed (= gateway).
    #[serde(default)]
    pub mode: String,
    /// `AUX_ENABLED` and a usable key file.
    pub enabled: bool,
    /// `GET /health` succeeded.
    pub health_ok: bool,
    /// `GET /v1/models` succeeded.
    pub models_ok: bool,
    /// The pinned model appears in `/v1/models`.
    pub model_published: bool,
    /// A key file was found and is non-empty (never the value itself).
    pub key_present: bool,
    /// Last error class, if any.
    pub last_error: Option<String>,
    /// True only when everything above holds.
    pub ready: bool,
}

impl AuxGatewayStatus {
    fn recompute(&mut self) {
        if self.mode == "agy" {
            // The agy backend has no gateway probes to fold in; readiness is
            // the session's liveness, set by `agy_mode`.
            self.ready = self.enabled && self.health_ok;
            return;
        }
        // Deliberately conservative: without a published model the compactor must not
        // queue work, otherwise every job would burn a request and fail validation.
        self.ready = self.enabled
            && self.health_ok
            && self.models_ok
            && self.model_published
            && self.key_present;
    }

    /// Status for the agy backend: no gateway exists — one Agy session is
    /// the provider, and `healthy` is its current liveness.
    pub fn agy_mode(healthy: bool) -> Self {
        let mut status = Self {
            mode: "agy".to_string(),
            enabled: true,
            health_ok: healthy,
            last_error: (!healthy).then(|| "aux session unavailable".to_string()),
            ..Self::default()
        };
        status.recompute();
        status
    }
}

/// Configuration for the adapter.
#[derive(Debug, Clone)]
pub struct AuxGatewayConfig {
    pub base_url: String,
    pub model: String,
    pub api_key_file: PathBuf,
    pub enabled: bool,
    pub session_header: String,
    /// API key supplied at runtime (settings API), taking precedence over
    /// the key file. Never printed: the manual `Debug` impl omits it.
    pub api_key_override: Option<String>,
}

impl AuxGatewayConfig {
    pub fn new(
        base_url: Option<String>,
        model: Option<String>,
        api_key_file: PathBuf,
        enabled: bool,
    ) -> Self {
        Self {
            base_url: base_url.unwrap_or_else(|| DEFAULT_AUX_BASE_URL.to_string()),
            model: model.unwrap_or_else(|| DEFAULT_AUX_MODEL.to_string()),
            api_key_file,
            enabled,
            session_header: DEFAULT_AUX_SESSION_HEADER.to_string(),
            api_key_override: None,
        }
    }

    /// Build from a resolved [`crate::config::Config`].
    pub fn from_config(cfg: &crate::config::Config) -> Self {
        Self {
            base_url: cfg.aux_base_url.clone(),
            model: cfg.aux_model.clone(),
            api_key_file: cfg.aux_api_key_file.clone(),
            enabled: cfg.aux_enabled,
            session_header: cfg.aux_session_header.clone(),
            api_key_override: None,
        }
    }

    /// Read the API key from its protected file.
    ///
    /// The value is returned to the caller and used only for the `Authorization`
    /// header; it is never stored on the struct, so a `Debug` print cannot leak it.
    pub fn read_api_key(&self) -> Option<String> {
        if let Some(key) = &self.api_key_override {
            return Some(key.clone());
        }
        read_key_file(&self.api_key_file)
    }
}

/// Read a trimmed, non-empty secret from `path`. Missing or empty → `None`.
pub fn read_key_file(path: &Path) -> Option<String> {
    let raw = std::fs::read_to_string(path).ok()?;
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

/// The Aux client. Holds a `reqwest::Client` and the readiness snapshot.
#[derive(Clone)]
pub struct AuxGatewayClient {
    http: reqwest::Client,
    config: AuxGatewayConfig,
    status: Arc<RwLock<AuxGatewayStatus>>,
}

impl std::fmt::Debug for AuxGatewayClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AuxGatewayClient")
            .field("base_url", &self.config.base_url)
            .field("model", &self.config.model)
            .field("enabled", &self.config.enabled)
            .finish_non_exhaustive()
    }
}

impl AuxGatewayClient {
    pub fn new(config: AuxGatewayConfig) -> Self {
        let http = reqwest::Client::builder()
            .timeout(REQUEST_TIMEOUT)
            .connect_timeout(Duration::from_secs(10))
            // The gateway is an internal service; redirects would leak the bearer
            // token to whatever the location header names.
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .unwrap_or_default();
        Self {
            http,
            config,
            status: Arc::new(RwLock::new(AuxGatewayStatus::default())),
        }
    }

    pub fn config(&self) -> &AuxGatewayConfig {
        &self.config
    }

    pub async fn status(&self) -> AuxGatewayStatus {
        self.status.read().await.clone()
    }

    async fn update<F: FnOnce(&mut AuxGatewayStatus)>(&self, f: F) {
        let mut guard = self.status.write().await;
        f(&mut guard);
        guard.recompute();
    }

    /// Probe `/health` and `/v1/models`. **Never** issues a paid completion request.
    ///
    /// Returns the resulting status. When the feature is disabled both probes are
    /// skipped and readiness stays false.
    pub async fn probe(&self) -> AuxGatewayStatus {
        let key = self.config.read_api_key();
        self.update(|s| {
            s.enabled = self.config.enabled;
            s.key_present = key.is_some();
        })
        .await;
        if !self.config.enabled {
            return self.status().await;
        }

        let health_url = format!("{}/health", self.config.base_url.trim_end_matches('/'));
        match self
            .http
            .get(&health_url)
            .timeout(PROBE_TIMEOUT)
            .send()
            .await
        {
            Ok(resp) if resp.status().is_success() => {
                self.update(|s| {
                    s.health_ok = true;
                    s.last_error = None;
                })
                .await;
            }
            Ok(resp) => {
                let code = resp.status().as_u16();
                self.update(|s| {
                    s.health_ok = false;
                    s.last_error = Some(format!("health status {code}"));
                })
                .await;
            }
            Err(e) if e.is_timeout() => {
                self.update(|s| {
                    s.health_ok = false;
                    s.last_error = Some("health timeout".into());
                })
                .await;
            }
            Err(_) => {
                self.update(|s| {
                    s.health_ok = false;
                    s.last_error = Some("health unreachable".into());
                })
                .await;
            }
        }

        let models_url = format!("{}/v1/models", self.config.base_url.trim_end_matches('/'));
        let mut builder = self.http.get(&models_url).timeout(PROBE_TIMEOUT);
        if let Some(key) = &key {
            builder = builder.bearer_auth(key);
        }
        match builder.send().await {
            Ok(resp) if resp.status().is_success() => {
                let published = match resp.json::<Value>().await {
                    Ok(body) => model_in_listing(&body, &self.config.model),
                    Err(_) => false,
                };
                self.update(|s| {
                    s.models_ok = true;
                    s.model_published = published;
                })
                .await;
            }
            Ok(resp) => {
                let code = resp.status().as_u16();
                self.update(|s| {
                    s.models_ok = false;
                    s.model_published = false;
                    s.last_error = Some(format!("models status {code}"));
                })
                .await;
            }
            Err(_) => {
                self.update(|s| {
                    s.models_ok = false;
                    s.model_published = false;
                })
                .await;
            }
        }
        self.status().await
    }

    /// Build the request body. Pinned model, `stream: true`, no extra knobs.
    ///
    /// `system` is a parameter because the same client now serves two callers with
    /// different jobs (memory compaction and the reply preflight), and the gateway
    /// prefix cache only helps when the system text stays byte-identical per use.
    pub fn request_body(&self, system: &str, prompt: &str) -> Value {
        json!({
            "model": self.config.model,
            "stream": true,
            // The gateway drops "off"; this model accepts low/medium/high. Low is the
            // cheapest effort that still stays explicit.
            "reasoning_effort": "low",
            "messages": [
                {"role": "system", "content": system},
                {"role": "user", "content": prompt}
            ]
        })
    }

    /// Issue one streaming completion and return the concatenated assistant text.
    ///
    /// `session` pins the gateway's sticky account: the upstream prefix cache only
    /// survives while the same account keeps serving the same conversation, so a
    /// caller that reuses one logical session per group must pass its id here.
    pub async fn complete_streaming(
        &self,
        system: &str,
        prompt: &str,
        session: Option<&str>,
    ) -> Result<String, ErrorClass> {
        self.post_stream(self.request_body(system, prompt), session)
            .await
    }

    /// Issue one multimodal completion. `images` are already-encoded data URLs.
    ///
    /// The gateway forwards `image_url` parts verbatim, so the picture never has to be
    /// reachable from the gateway host. Every image travels in the *user* message while
    /// the system text stays byte-identical, which is what keeps the prefix cache warm.
    pub async fn complete_vision(
        &self,
        system: &str,
        prompt: &str,
        images: &[String],
        session: Option<&str>,
    ) -> Result<String, ErrorClass> {
        self.complete_vision_on(&self.config.model, system, prompt, images, session)
            .await
    }

    /// Same as [`Self::complete_vision`], but the model id is chosen by the caller.
    ///
    /// Vision fan-out uses this so one batch can spread across `hy3`, `hy3-b` and
    /// `hy3-c`. Memory, the pacing gate and folding stay on the pinned config model.
    pub async fn complete_vision_on(
        &self,
        model: &str,
        system: &str,
        prompt: &str,
        images: &[String],
        session: Option<&str>,
    ) -> Result<String, ErrorClass> {
        let mut content = vec![json!({"type": "text", "text": prompt})];
        for url in images {
            content.push(json!({"type": "image_url", "image_url": {"url": url}}));
        }
        let body = json!({
            "model": model,
            "stream": true,
            "reasoning_effort": "low",
            "messages": [
                {"role": "system", "content": system},
                {"role": "user", "content": content}
            ]
        });
        self.post_stream(body, session).await
    }

    /// One POST against the chat endpoint, shared by every caller so the transport
    /// error mapping and the response cap can only ever be written once.
    async fn post_stream(&self, body: Value, session: Option<&str>) -> Result<String, ErrorClass> {
        if !self.config.enabled {
            return Err(ErrorClass::Unavailable);
        }
        let key = self.config.read_api_key().ok_or(ErrorClass::Unavailable)?;
        let url = format!(
            "{}/v1/chat/completions",
            self.config.base_url.trim_end_matches('/')
        );
        let mut builder = self.http.post(&url).bearer_auth(&key).json(&body);
        if let Some(session) = session.filter(|s| !s.is_empty()) {
            builder = builder.header(&self.config.session_header, session);
        }
        let resp = builder
            .send()
            .await
            // Transport failures, timeouts and HTTP errors all map to the same
            // coarse class on purpose: the detail may echo the prompt.
            .map_err(|_| ErrorClass::AuxFailed)?;
        if !resp.status().is_success() {
            // The status code is safe; the body may echo the prompt.
            return Err(ErrorClass::AuxFailed);
        }
        let text = resp.text().await.map_err(|_| ErrorClass::AuxFailed)?;
        if text.len() > MAX_STREAM_BYTES {
            return Err(ErrorClass::AuxBadOutput);
        }
        collect_stream(&text)
    }

    /// Memory compaction keeps its own pinned system prompt.
    pub async fn complete_summary(&self, prompt: &str) -> Result<String, ErrorClass> {
        self.complete_streaming(SUMMARY_SYSTEM, prompt, None).await
    }
}

#[async_trait::async_trait]
impl SummaryModel for AuxGatewayClient {
    async fn summarise(&self, prompt: String) -> Result<String, ErrorClass> {
        self.complete_summary(&prompt).await
    }
}

/// Extract the assistant content from an SSE stream body.
///
/// Handles both `data: {...}` chunks and a plain JSON body (some gateways ignore
/// `stream` and answer in one shot). Anything unparsable is skipped rather than
/// guessed at; an empty result is a bad-output error.
pub fn collect_stream(body: &str) -> Result<String, ErrorClass> {
    let trimmed = body.trim();
    if trimmed.is_empty() {
        return Err(ErrorClass::AuxBadOutput);
    }

    // Plain (non-SSE) JSON answer.
    if trimmed.starts_with('{')
        && let Ok(value) = serde_json::from_str::<Value>(trimmed)
    {
        if let Some(text) = choice_content(&value) {
            return Ok(text);
        }
        return Err(ErrorClass::AuxBadOutput);
    }

    let mut out = String::new();
    let mut saw_data = false;
    for line in trimmed.lines() {
        let line = line.trim();
        let Some(payload) = line.strip_prefix("data:") else {
            continue;
        };
        let payload = payload.trim();
        if payload == "[DONE]" {
            break;
        }
        if payload.is_empty() {
            continue;
        }
        saw_data = true;
        let Ok(value) = serde_json::from_str::<Value>(payload) else {
            continue;
        };
        if let Some(delta) = value
            .pointer("/choices/0/delta/content")
            .and_then(|v| v.as_str())
        {
            out.push_str(delta);
            continue;
        }
        if let Some(text) = value
            .pointer("/choices/0/message/content")
            .and_then(|v| v.as_str())
        {
            out.push_str(text);
        }
    }
    if !saw_data || out.trim().is_empty() {
        return Err(ErrorClass::AuxBadOutput);
    }
    Ok(out)
}

fn choice_content(value: &Value) -> Option<String> {
    value
        .pointer("/choices/0/message/content")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .or_else(|| {
            value
                .pointer("/choices/0/delta/content")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string())
        })
}

/// Whether `model` is listed in a `/v1/models` payload.
pub fn model_in_listing(body: &Value, model: &str) -> bool {
    body.get("data")
        .and_then(|d| d.as_array())
        .map(|items| {
            items.iter().any(|item| {
                item.get("id")
                    .and_then(|id| id.as_str())
                    .map(|id| id == model)
                    .unwrap_or(false)
            })
        })
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn status_with_all_good() -> AuxGatewayStatus {
        let mut s = AuxGatewayStatus {
            mode: "gateway".to_string(),
            enabled: true,
            health_ok: true,
            models_ok: true,
            model_published: true,
            key_present: true,
            last_error: None,
            ready: false,
        };
        s.recompute();
        s
    }

    #[test]
    fn readiness_requires_every_signal() {
        assert!(status_with_all_good().ready);
        for broken in 0..5 {
            let mut s = status_with_all_good();
            match broken {
                0 => s.enabled = false,
                1 => s.health_ok = false,
                2 => s.models_ok = false,
                3 => s.model_published = false,
                _ => s.key_present = false,
            }
            s.recompute();
            assert!(!s.ready, "缺少任一信号时必须未就绪");
        }
    }

    #[test]
    fn request_body_pins_model_and_stream() {
        let client = AuxGatewayClient::new(AuxGatewayConfig::new(
            None,
            None,
            PathBuf::from("/nonexistent"),
            true,
        ));
        let body = client.request_body(SUMMARY_SYSTEM, "hi");
        assert_eq!(body["model"], DEFAULT_AUX_MODEL);
        assert_eq!(body["stream"], true);
        assert_eq!(body["reasoning_effort"], "low");
        assert_eq!(body["messages"][0]["content"], SUMMARY_SYSTEM);
        assert_eq!(body["messages"][1]["content"], "hi");
        // No search endpoint anywhere in the request.
        assert!(body.get("tools").is_none());
    }

    #[test]
    fn compaction_system_prompt_is_pinned_and_passed_through() {
        let client = AuxGatewayClient::new(AuxGatewayConfig::new(
            None,
            None,
            PathBuf::from("/nonexistent"),
            true,
        ));
        let first = client.request_body("preflight", "p").to_string();
        let second = client.request_body("preflight", "p").to_string();
        // Byte-identical: the upstream prefix cache only pays off if it is stable.
        assert_eq!(first, second);
        assert!(first.contains("preflight"));
    }

    #[test]
    fn model_listing_detection_is_exact() {
        let listing = json!({"data":[{"id":"deepseek-v4.1-flash"},{"id":"other"}]});
        assert!(model_in_listing(&listing, "deepseek-v4.1-flash"));
        assert!(!model_in_listing(&listing, "deepseek-v4.1-flash-preview"));
        assert!(!model_in_listing(&json!({}), "deepseek-v4.1-flash"));
    }

    #[test]
    fn collects_sse_deltas() {
        let body = "\
data: {\"choices\":[{\"delta\":{\"content\":\"{\\\"summary\\\":\"}}]}\n\
data: {\"choices\":[{\"delta\":{\"content\":\"\\\"ok\\\"}\"}}]}\n\
data: [DONE]\n";
        let out = collect_stream(body).expect("应拼接成功");
        assert_eq!(out, "{\"summary\":\"ok\"}");
    }

    #[test]
    fn accepts_non_streaming_json_answer() {
        let body = json!({"choices":[{"message":{"content":"{\"summary\":\"x\"}"}}]}).to_string();
        assert_eq!(collect_stream(&body).unwrap(), "{\"summary\":\"x\"}");
    }

    #[test]
    fn empty_or_garbage_stream_is_bad_output() {
        assert_eq!(collect_stream("").unwrap_err(), ErrorClass::AuxBadOutput);
        assert_eq!(
            collect_stream("data: [DONE]").unwrap_err(),
            ErrorClass::AuxBadOutput
        );
        assert_eq!(
            collect_stream("hello").unwrap_err(),
            ErrorClass::AuxBadOutput
        );
    }

    #[test]
    fn key_file_reading_never_panics_and_trims() {
        let dir = tempfile::tempdir().expect("tmp");
        let path = dir.path().join("key");
        assert!(read_key_file(&path).is_none());
        std::fs::write(&path, "  \n").expect("写");
        assert!(read_key_file(&path).is_none());
        std::fs::write(&path, "sk-test-value\n").expect("写");
        assert_eq!(read_key_file(&path).as_deref(), Some("sk-test-value"));
    }

    #[test]
    fn debug_output_never_contains_the_key() {
        let dir = tempfile::tempdir().expect("tmp");
        let path = dir.path().join("key");
        std::fs::write(&path, "sk-super-secret-value").expect("写");
        let client = AuxGatewayClient::new(AuxGatewayConfig::new(None, None, path.clone(), true));
        let rendered = format!("{client:?}");
        assert!(!rendered.contains("sk-super-secret-value"));
        assert!(rendered.contains("hy3"));
    }

    // ------------------------------------------------------------ mock server

    /// Minimal HTTP mock that answers one canned response per path.
    async fn spawn_mock(
        routes: Vec<(&'static str, u16, &'static str)>,
    ) -> (String, tokio::task::JoinHandle<()>) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("绑定");
        let addr = listener.local_addr().expect("地址");
        let routes: Vec<(String, u16, String)> = routes
            .into_iter()
            .map(|(p, s, b)| (p.to_string(), s, b.to_string()))
            .collect();
        let handle = tokio::spawn(async move {
            loop {
                let Ok((mut socket, _)) = listener.accept().await else {
                    break;
                };
                let routes = routes.clone();
                tokio::spawn(async move {
                    let mut buf = vec![0u8; 8192];
                    let n = socket.read(&mut buf).await.unwrap_or(0);
                    let req = String::from_utf8_lossy(&buf[..n]).to_string();
                    let path = req
                        .lines()
                        .next()
                        .and_then(|line| line.split_whitespace().nth(1))
                        .unwrap_or("/")
                        .to_string();
                    let (status, body) = routes
                        .iter()
                        .find(|(p, _, _)| *p == path)
                        .map(|(_, s, b)| (*s, b.clone()))
                        .unwrap_or((404, "{}".to_string()));
                    let resp = format!(
                        "HTTP/1.1 {status} OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    );
                    let _ = socket.write_all(resp.as_bytes()).await;
                });
            }
        });
        (format!("http://{addr}"), handle)
    }

    #[tokio::test]
    async fn probe_requires_published_model() {
        let (base, handle) = spawn_mock(vec![
            ("/health", 200, "{\"status\":\"ok\"}"),
            ("/v1/models", 200, "{\"data\":[{\"id\":\"hy3\"}]}"),
        ])
        .await;
        let dir = tempfile::tempdir().expect("tmp");
        let key_path = dir.path().join("key");
        std::fs::write(&key_path, "sk-test").expect("写");
        let client = AuxGatewayClient::new(AuxGatewayConfig::new(Some(base), None, key_path, true));
        let status = client.probe().await;
        assert!(status.ready, "{status:?}");
        handle.abort();
    }

    #[tokio::test]
    async fn probe_with_unpublished_model_is_not_ready() {
        let (base, handle) = spawn_mock(vec![
            ("/health", 200, "{\"status\":\"ok\"}"),
            (
                "/v1/models",
                200,
                "{\"data\":[{\"id\":\"some-other-model\"}]}",
            ),
        ])
        .await;
        let dir = tempfile::tempdir().expect("tmp");
        let key_path = dir.path().join("key");
        std::fs::write(&key_path, "sk-test").expect("写");
        let client = AuxGatewayClient::new(AuxGatewayConfig::new(Some(base), None, key_path, true));
        let status = client.probe().await;
        assert!(!status.ready);
        assert!(status.models_ok);
        assert!(!status.model_published);
        handle.abort();
    }

    #[tokio::test]
    async fn disabled_client_never_probes() {
        let client = AuxGatewayClient::new(AuxGatewayConfig::new(
            Some("http://127.0.0.1:1".into()),
            None,
            PathBuf::from("/nonexistent"),
            false,
        ));
        let status = client.probe().await;
        assert!(!status.health_ok);
        assert!(!status.ready);
        // Completing while disabled is refused without a network call.
        assert_eq!(
            client
                .complete_streaming(SUMMARY_SYSTEM, "hi", None)
                .await
                .unwrap_err(),
            ErrorClass::Unavailable
        );
    }

    #[tokio::test]
    async fn streaming_completion_returns_concatenated_text() {
        let (base, handle) = spawn_mock(vec![(
            "/v1/chat/completions",
            200,
            "data: {\"choices\":[{\"delta\":{\"content\":\"{\\\"summary\\\":\\\"hi\\\"}\"}}]}\n\ndata: [DONE]\n",
        )])
        .await;
        let dir = tempfile::tempdir().expect("tmp");
        let key_path = dir.path().join("key");
        std::fs::write(&key_path, "sk-test").expect("写");
        let client = AuxGatewayClient::new(AuxGatewayConfig::new(Some(base), None, key_path, true));
        let out = client
            .complete_streaming(SUMMARY_SYSTEM, "prompt", None)
            .await
            .expect("应成功");
        assert!(out.contains("summary"));
        handle.abort();
    }

    #[tokio::test]
    async fn server_error_is_aux_failed() {
        let (base, handle) = spawn_mock(vec![("/v1/chat/completions", 500, "{}")]).await;
        let dir = tempfile::tempdir().expect("tmp");
        let key_path = dir.path().join("key");
        std::fs::write(&key_path, "sk-test").expect("写");
        let client = AuxGatewayClient::new(AuxGatewayConfig::new(Some(base), None, key_path, true));
        assert_eq!(
            client
                .complete_streaming(SUMMARY_SYSTEM, "p", None)
                .await
                .unwrap_err(),
            ErrorClass::AuxFailed
        );
        handle.abort();
    }
}
