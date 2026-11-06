//! Antigravity account quota for the admin panel: the Gemini group's 5-hour
//! and weekly buckets.
//!
//! Both buckets come from the CLI's own `/usage` screen: a scratch-HOME
//! `agy` is driven over a PTY (`script(1)`), `/usage` is opened, and the
//! rendered page's GEMINI MODELS section is parsed by [`parse_usage`].
//! Google's Code Assist quota-summary endpoint provably never returns the
//! Gemini group to a bare API caller (verified 2026-10-26 against every
//! request variant we could construct), and while `fetchAvailableModels`
//! does serve the 5-hour bucket, the operator decided on 2026-10-26 to
//! source both buckets from the TUI for a single, uniform provenance —
//! the API client is kept below in [`parked_api`], unused, so the option
//! stays open without polluting this endpoint's data or error field.
//!
//! Secrecy rules, non-negotiable:
//!
//! * the real OAuth token file is only ever read, never written;
//! * token values and the OAuth client secret never reach logs, the HTTP
//!   response, or an error string — errors carry stage names, not payloads;
//! * the scratch HOME holds a *copy* of the token (refreshed from the real
//!   home before every scrape) and lives under `/data`, next to the real one.
//!
//! Everything degrades: a failed refresh keeps the previous snapshot, a
//! failed scrape leaves `weekly` at its last value (or `null`), and the
//! endpoint itself never errors.

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde::Serialize;

/// How long a snapshot is served before a background refresh is triggered.
pub const QUOTA_TTL: Duration = Duration::from_secs(600);
/// How soon a failed refresh may be retried. Failures must not ride the
/// full TTL: a bad-network stretch otherwise chains 90s timeouts into
/// hours of stale snapshots (observed 2026-10-31: the snapshot sat at
/// 09:51 with `error=tui_timeout` past 11:53, and pacing escalated tiers
/// on staleness the whole while).
pub const FAILED_REFRESH_RETRY: Duration = Duration::from_secs(120);

/// The delay that applies after an attempt: short after a failure, the
/// full TTL after a success.
fn retry_delay(last_failed: bool) -> Duration {
    if last_failed {
        FAILED_REFRESH_RETRY
    } else {
        QUOTA_TTL
    }
}

/// Whether a background refresh is due, given how long ago the last
/// attempt started (`None` before the first one).
fn refresh_due(elapsed: Option<Duration>, last_failed: bool) -> bool {
    match elapsed {
        None => true,
        Some(e) => e > retry_delay(last_failed),
    }
}
/// The User-Agent the Code Assist endpoints accept quota reads from. Matches
/// the CLI version pinned in the runtime image.
const QUOTA_USER_AGENT: &str = "antigravity/1.4.7 (linux; amd64)";
const MODELS_URL: &str =
    "https://daily-cloudcode-pa.googleapis.com/v1internal:fetchAvailableModels";
const TOKEN_URL: &str = "https://oauth2.googleapis.com/token";
/// The tiered Flash entry whose `quotaInfo` is the Gemini 5-hour bucket.
const TIERED_FLASH_MODEL: &str = "gemini-3.8-flash-tiered";
/// OAuth token file, relative to the Agy home (the `$HOME` of Agy children).
const TOKEN_RELATIVE_PATH: &str = ".gemini/antigravity-cli/antigravity-oauth-token";
/// Fallback location of the real CLI binary when the configured `agy_bin`
/// is the proxy wrapper script rather than the ELF itself.
const DEFAULT_AGY_BIN: &str = "/opt/agy/agy";
/// Overall budget for one TUI scrape. A cold language-server start plus
/// its quota refresh over the proxy egress can eat most of 90s on a bad
/// network stretch; 150s gives the page a fair chance to render.
const SCRAPE_TIMEOUT: Duration = Duration::from_secs(150);

// ---------------------------------------------------------------------------
// Wire shape

/// One quota bucket as the panel consumes it.
#[derive(Debug, Clone, Serialize)]
pub struct QuotaBucket {
    pub remaining_fraction: f64,
    pub reset_time: Option<String>,
}

/// The Gemini group's two buckets; either may be missing when its source has
/// never succeeded.
#[derive(Debug, Clone, Default, Serialize)]
pub struct GeminiQuota {
    pub five_hour: Option<QuotaBucket>,
    pub weekly: Option<QuotaBucket>,
}

/// The `/internal/v1/quota` response.
#[derive(Debug, Clone, Default, Serialize)]
pub struct QuotaSnapshot {
    pub gemini: GeminiQuota,
    /// When the newest bucket data was fetched, RFC 3339 UTC.
    pub fetched_at: Option<String>,
    /// Where `weekly` came from: `"tui"` (the summary API never serves it).
    pub weekly_source: Option<&'static str>,
    /// Where `five_hour` came from: `"tui"` (same scrape as `weekly`).
    pub five_hour_source: Option<&'static str>,
    /// Stage name of the last failed scrape, e.g. `"tui_timeout"`.
    pub error: Option<String>,
}

// ---------------------------------------------------------------------------
// Service

/// Static wiring for [`QuotaService`].
pub struct QuotaConfig {
    pub agy_home: PathBuf,
    pub agy_bin: PathBuf,
    pub proxy_url: Option<String>,
}

#[derive(Default)]
struct State {
    five_hour: Option<QuotaBucket>,
    weekly: Option<QuotaBucket>,
    fetched_at_unix: Option<i64>,
    last_attempt: Option<Instant>,
    /// Whether the last refresh attempt failed to produce a snapshot
    /// (scrape error, or a page the weekly bucket could not be read
    /// from). Drives the shorter retry delay in [`refresh_due`].
    last_failed: bool,
    /// Access token we refreshed ourselves, with its expiry (unix seconds).
    /// Only touched by the parked API path.
    #[allow(dead_code)]
    access_token: Option<(String, i64)>,
    /// The (client id, secret) pair that a refresh succeeded with.
    /// Only touched by the parked API path.
    #[allow(dead_code)]
    oauth_pair: Option<(String, String)>,
    error: Option<String>,
}

struct Inner {
    agy_home: PathBuf,
    agy_bin: PathBuf,
    /// HTTP client for the parked API path; built (and proxied) as before
    /// but no longer used as a data source.
    #[allow(dead_code)]
    http: reqwest::Client,
    state: Mutex<State>,
    refreshing: AtomicBool,
}

/// Caching quota reader. `snapshot` never blocks on the network: it serves
/// the cache and schedules a background refresh when the cache is stale.
pub struct QuotaService {
    inner: Arc<Inner>,
}

impl QuotaService {
    pub fn new(config: QuotaConfig) -> Self {
        let build_client = |proxy: &Option<String>| {
            let mut builder = reqwest::Client::builder()
                .timeout(Duration::from_secs(25))
                .redirect(reqwest::redirect::Policy::none());
            if let Some(url) = proxy {
                // Proxy::all, not ::http — the latter only matches http://
                // targets and would send the https API calls direct (the
                // container has no direct egress, so they would all fail).
                if let Ok(p) = reqwest::Proxy::all(url) {
                    builder = builder.proxy(p);
                }
            }
            builder.build()
        };
        let http = build_client(&config.proxy_url)
            .or_else(|_| build_client(&None))
            .unwrap_or_default();
        Self {
            inner: Arc::new(Inner {
                agy_home: config.agy_home,
                agy_bin: config.agy_bin,
                http,
                state: Mutex::new(State::default()),
                refreshing: AtomicBool::new(false),
            }),
        }
    }

    /// The current snapshot; triggers a background refresh when stale.
    pub fn snapshot(&self) -> QuotaSnapshot {
        self.kick();
        let st = self.inner.state.lock().unwrap();
        QuotaSnapshot {
            gemini: GeminiQuota {
                five_hour: st.five_hour.clone(),
                weekly: st.weekly.clone(),
            },
            fetched_at: st.fetched_at_unix.map(format_rfc3339),
            weekly_source: st.weekly.as_ref().map(|_| "tui"),
            five_hour_source: st.five_hour.as_ref().map(|_| "tui"),
            error: st.error.clone(),
        }
    }

    /// Start a background refresh now (used once at startup).
    pub fn warmup(&self) {
        self.kick();
    }

    fn kick(&self) {
        let stale = {
            let st = self.inner.state.lock().unwrap();
            refresh_due(st.last_attempt.map(|t| t.elapsed()), st.last_failed)
        };
        if stale && !self.inner.refreshing.swap(true, Ordering::SeqCst) {
            let inner = Arc::clone(&self.inner);
            tokio::spawn(async move {
                refresh(&inner).await;
                inner.refreshing.store(false, Ordering::SeqCst);
            });
        }
    }
}

/// One full refresh: a single TUI scrape serves both buckets. A failed
/// scrape keeps the previous snapshot; a scrape whose page renders only
/// one bucket updates that bucket and reports the other as a parse miss.
async fn refresh(inner: &Inner) {
    {
        let mut st = inner.state.lock().unwrap();
        st.last_attempt = Some(Instant::now());
    }
    let scrape = {
        let home = inner.agy_home.clone();
        let bin = inner.agy_bin.clone();
        tokio::task::spawn_blocking(move || scrape_usage(&home, &bin)).await
    };
    let mut st = inner.state.lock().unwrap();
    match scrape {
        Ok(Ok((buckets, seen_unix))) => {
            let to_bucket = |(fraction, minutes): (f64, Option<i64>)| QuotaBucket {
                remaining_fraction: fraction,
                reset_time: minutes.map(|m| format_rfc3339(seen_unix + m * 60)),
            };
            if let Some(weekly) = buckets.weekly {
                st.weekly = Some(to_bucket(weekly));
            }
            if let Some(five_hour) = buckets.five_hour {
                st.five_hour = Some(to_bucket(five_hour));
            }
            if buckets.weekly.is_some() {
                st.fetched_at_unix = Some(crate::now_unix());
            }
            st.last_failed = buckets.weekly.is_none();
            st.error = match (buckets.weekly.is_some(), buckets.five_hour.is_some()) {
                (true, true) => None,
                (true, false) => Some("tui_parse_five_hour".to_string()),
                _ => Some("tui_parse".to_string()),
            };
        }
        Ok(Err(stage)) => {
            st.last_failed = true;
            st.error = Some(stage.to_string());
        }
        Err(_) => {
            st.last_failed = true;
            st.error = Some("tui_join".to_string());
        }
    }
}

// ---------------------------------------------------------------------------
// Parked API path (not a data source)
//
// The Code Assist API client below was the original 5-hour source. It is
// retained, compiling and unit-tested, but deliberately never called: both
// buckets come from the TUI scrape (operator decision, 2026-10-26), so this
// code can neither feed the snapshot nor pollute its error field.

#[allow(dead_code)]
mod parked_api {
    use super::*;

    /// Classify a reqwest send failure into a fixed stage name (never the
    /// error text itself: it can embed URLs, and the panel only needs the
    /// class).
    fn send_stage(base: &'static str, error: &reqwest::Error) -> &'static str {
        let kind = if error.is_timeout() {
            "timeout"
        } else if error.is_connect() {
            "connect"
        } else if error.is_request() {
            "request"
        } else {
            "send"
        };
        match (base, kind) {
            ("models", "timeout") => "models_timeout",
            ("models", "connect") => "models_connect",
            ("models", "request") => "models_request",
            ("refresh", "timeout") => "refresh_timeout",
            ("refresh", "connect") => "refresh_connect",
            ("refresh", "request") => "refresh_request",
            ("refresh", _) => "refresh_send",
            _ => "models_send",
        }
    }

    // ---------------------------------------------------------------------------
    // 5-hour bucket over the Code Assist API

    async fn fetch_five_hour(inner: &Inner) -> Result<QuotaBucket, &'static str> {
        let access = access_token(inner).await?;
        let resp = inner
            .http
            .post(MODELS_URL)
            .header("Authorization", format!("Bearer {access}"))
            .header("User-Agent", QUOTA_USER_AGENT)
            .json(&serde_json::json!({}))
            .send()
            .await
            .map_err(|e| {
                // The error chain names the failing layer (proxy CONNECT, TLS,
                // DNS); it never contains headers or token material.
                eprintln!("quota: models request failed: {e:?}");
                send_stage("models", &e)
            })?;
        if !resp.status().is_success() {
            return Err("models_status");
        }
        let value: serde_json::Value = resp.json().await.map_err(|_| "models_json")?;
        pick_gemini_bucket(&value).ok_or("models_shape")
    }

    /// Find the Gemini bucket in a `fetchAvailableModels` response. Every Gemini
    /// model shares one bucket; the tiered Flash entry is the canonical handle,
    /// with the response's own `tieredModelIds` as the fallback pointer.
    pub(super) fn pick_gemini_bucket(value: &serde_json::Value) -> Option<QuotaBucket> {
        let models = value.get("models")?;
        let quota_of = |key: &str| -> Option<QuotaBucket> {
            let info = models.get(key)?.get("quotaInfo")?;
            Some(QuotaBucket {
                remaining_fraction: info.get("remainingFraction")?.as_f64()?,
                reset_time: info
                    .get("resetTime")
                    .and_then(|v| v.as_str())
                    .map(str::to_string),
            })
        };
        if let Some(bucket) = quota_of(TIERED_FLASH_MODEL) {
            return Some(bucket);
        }
        if let Some(key) = value
            .get("tieredModelIds")
            .and_then(|t| t.get("flash"))
            .and_then(|f| f.as_array())
            .and_then(|a| a.first())
            .and_then(|v| v.as_str())
            && let Some(bucket) = quota_of(key)
        {
            return Some(bucket);
        }
        for (key, _) in models.as_object()? {
            if key.starts_with("gemini-")
                && let Some(bucket) = quota_of(key)
                && bucket.reset_time.is_some()
            {
                return Some(bucket);
            }
        }
        None
    }

    /// A usable access token: ours from cache, the token file's own (Agy keeps
    /// it fresh on its own runs, so this usually costs no refresh call), or a
    /// fresh one from the refresh token.
    async fn access_token(inner: &Inner) -> Result<String, &'static str> {
        let now = crate::now_unix();
        {
            let st = inner.state.lock().unwrap();
            if let Some((token, expiry)) = &st.access_token
                && *expiry > now + 120
            {
                return Ok(token.clone());
            }
        }
        let text = tokio::fs::read_to_string(inner.agy_home.join(TOKEN_RELATIVE_PATH))
            .await
            .map_err(|_| "token_read")?;
        let parsed: serde_json::Value = serde_json::from_str(&text).map_err(|_| "token_json")?;
        let token_obj = parsed.get("token").cloned().unwrap_or(parsed);
        if let (Some(access), Some(expiry)) = (
            token_obj.get("access_token").and_then(|v| v.as_str()),
            token_obj
                .get("expiry")
                .and_then(|v| v.as_str())
                .and_then(parse_rfc3339),
        ) && expiry > now + 120
        {
            inner.state.lock().unwrap().access_token = Some((access.to_string(), expiry));
            return Ok(access.to_string());
        }
        let refresh_token = token_obj
            .get("refresh_token")
            .and_then(|v| v.as_str())
            .ok_or("token_shape")?
            .to_string();

        let pair = {
            let st = inner.state.lock().unwrap();
            st.oauth_pair.clone()
        };
        let (client_id, secrets) = match pair {
            Some((id, secret)) => (id, vec![secret]),
            None => {
                let bin = inner.agy_bin.clone();
                tokio::task::spawn_blocking(move || extract_oauth_candidates(&bin))
                    .await
                    .map_err(|_| "oauth_join")?
                    .ok_or("oauth_extract")?
            }
        };
        for secret in &secrets {
            let resp = inner
                .http
                .post(TOKEN_URL)
                .form(&[
                    ("client_id", client_id.as_str()),
                    ("client_secret", secret.as_str()),
                    ("refresh_token", refresh_token.as_str()),
                    ("grant_type", "refresh_token"),
                ])
                .send()
                .await
                .map_err(|e| {
                    eprintln!("quota: token refresh failed: {e:?}");
                    send_stage("refresh", &e)
                })?;
            if !resp.status().is_success() {
                continue;
            }
            let value: serde_json::Value = resp.json().await.map_err(|_| "refresh_json")?;
            let access = value
                .get("access_token")
                .and_then(|v| v.as_str())
                .ok_or("refresh_shape")?
                .to_string();
            let ttl = value
                .get("expires_in")
                .and_then(|v| v.as_i64())
                .unwrap_or(3600);
            let mut st = inner.state.lock().unwrap();
            st.access_token = Some((access.clone(), crate::now_unix() + ttl));
            st.oauth_pair = Some((client_id.clone(), secret.clone()));
            return Ok(access);
        }
        Err("refresh_status")
    }

    /// Extract the embedded consumer OAuth client from the CLI binary: the
    /// client id (identified by its well-known prefix) and every 35-byte
    /// `GOCSPX-…` secret candidate. The binary carries more than one client;
    /// only one secret pairs with this id, and the refresh call is the arbiter,
    /// so all candidates are returned for the caller to try. Nothing extracted
    /// here is ever logged or persisted.
    fn extract_oauth_candidates(agy_bin: &Path) -> Option<(String, Vec<String>)> {
        let mut paths = vec![agy_bin.to_path_buf()];
        let fallback = PathBuf::from(DEFAULT_AGY_BIN);
        if !paths.contains(&fallback) {
            paths.push(fallback);
        }
        for path in paths {
            let Ok(bytes) = std::fs::read(&path) else {
                continue;
            };
            // The wrapper script is a few hundred bytes of shell; the ELF is
            // hundreds of megabytes. Only scan real binaries.
            if bytes.len() < 1_000_000 || !bytes.starts_with(b"\x7fELF") {
                continue;
            }
            if let Some(found) = scan_oauth_candidates(&bytes) {
                return Some(found);
            }
        }
        None
    }

    pub(super) fn scan_oauth_candidates(bytes: &[u8]) -> Option<(String, Vec<String>)> {
        const ID_PREFIX: &[u8] = b"1071006060591";
        const ID_SUFFIX: &[u8] = b".apps.googleusercontent.com";
        let mut client_id = None;
        for at in find_all(bytes, ID_PREFIX) {
            let rest = &bytes[at..];
            let consumed = rest
                .iter()
                .take_while(|&&b| b.is_ascii_alphanumeric() || b == b'-' || b == b'.')
                .count();
            let run = &rest[..consumed];
            // The id is not NUL-terminated in the binary: in-charset bytes trail
            // it (measured: a 160-byte run), so the suffix must be located
            // inside the run and the candidate cut at the suffix's end.
            if let Some(end) = find_all(run, ID_SUFFIX)
                .first()
                .map(|start| start + ID_SUFFIX.len())
                && end > ID_PREFIX.len() + ID_SUFFIX.len()
            {
                client_id = Some(String::from_utf8_lossy(&run[..end]).into_owned());
                break;
            }
        }
        let client_id = client_id?;
        let mut secrets: Vec<String> = Vec::new();
        for at in find_all(bytes, b"GOCSPX-") {
            if at + 35 > bytes.len() {
                continue;
            }
            let candidate = &bytes[at..at + 35];
            if candidate
                .iter()
                .all(|&b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
            {
                let secret = String::from_utf8_lossy(candidate).into_owned();
                if !secrets.contains(&secret) {
                    secrets.push(secret);
                }
            }
        }
        if secrets.is_empty() {
            return None;
        }
        Some((client_id, secrets))
    }

    fn find_all(haystack: &[u8], needle: &[u8]) -> Vec<usize> {
        let mut out = Vec::new();
        if needle.is_empty() || haystack.len() < needle.len() {
            return out;
        }
        let mut i = 0;
        while i + needle.len() <= haystack.len() {
            if &haystack[i..i + needle.len()] == needle {
                out.push(i);
                i += needle.len();
            } else {
                i += 1;
            }
        }
        out
    }
}

// ---------------------------------------------------------------------------
// Both buckets via the TUI

// ---------------------------------------------------------------------------

mod parse;
mod scrape;

pub use self::parse::{
    UsageBuckets, format_rfc3339, parse_rfc3339, parse_usage, parse_usage_weekly, strip_ansi,
};

use self::parse::{
    civil_from_days, days_from_civil, floor_boundary, parse_bucket_at, parse_refresh_minutes,
};
use self::scrape::{prepare_scratch_home, scrape_usage, shell_quote};

#[cfg(test)]
mod tests;
