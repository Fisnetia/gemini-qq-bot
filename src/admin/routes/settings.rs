//! Runtime settings endpoints (`/api/v1/settings*`).
//!
//! The write path behind the web console's settings page: aux gateway
//! configuration and the two runtime switches (session rotation, quota
//! degradation). Changes persist as an overlay next to the database and
//! apply to the live consumers immediately (see
//! [`crate::app::runtime_settings`]). The aux API key is write-only:
//! reads report only whether one is set.

use std::sync::Arc;

use axum::extract::State;
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{Value, json};

use super::{AdminState, ApiError, ApiResult, ok_json};
use crate::app::runtime_settings::AuxPatch;
use crate::store::ActorKind;

pub fn routes() -> Router<Arc<AdminState>> {
    Router::new()
        .route("/settings", axum::routing::get(get_settings))
        .route("/settings/aux", axum::routing::put(put_aux))
        .route("/settings/aux/test", axum::routing::post(test_aux))
        .route("/settings/runtime", axum::routing::put(put_runtime))
        .route("/settings/engine", axum::routing::put(put_engine))
        .route("/settings/engine/models", axum::routing::get(get_models))
}

async fn get_settings(State(state): State<Arc<AdminState>>) -> ApiResult {
    ok_json(json!({
        "aux": state.settings.aux_view(),
        "runtime": state.settings.switch_view(),
        "engine": state.settings.engine_view(),
    }))
}

/// Efforts `agy --effort` knows, plus empty for the model's own default. Which
/// of them a model takes is Agy's call, checked when a choice is saved.
const EFFORTS: [&str; 6] = ["", "low", "medium", "high", "xhigh", "max"];

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct EngineUpdate {
    model: Option<String>,
    effort: Option<String>,
}

async fn get_models(State(state): State<Arc<AdminState>>) -> ApiResult {
    ok_json(json!({ "models": state.settings.list_models().await }))
}

async fn put_engine(State(state): State<Arc<AdminState>>, Json(body): Json<Value>) -> ApiResult {
    let update: EngineUpdate =
        serde_json::from_value(body).map_err(|_| ApiError::bad_request("invalid_body"))?;
    if update.model.is_none() && update.effort.is_none() {
        return Err(ApiError::bad_request("empty_update"));
    }
    let model = update.model.map(|v| v.trim().to_string());
    if let Some(model) = &model {
        let valid = !model.is_empty()
            && model.len() <= 100
            && model
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'));
        if !valid {
            return Err(ApiError::bad_request("invalid_model"));
        }
    }
    if let Some(effort) = &update.effort
        && !EFFORTS.contains(&effort.as_str())
    {
        return Err(ApiError::bad_request("invalid_effort"));
    }
    let current = state.settings.engine_view();
    let next_model = model.as_deref().unwrap_or(&current.model);
    let next_effort = update.effort.as_deref().unwrap_or(&current.effort);
    if !state
        .settings
        .engine_is_supported(next_model, next_effort)
        .await
    {
        return Err(ApiError::bad_request("unsupported_engine"));
    }
    let view = state
        .settings
        .update_engine(model, update.effort)
        .map_err(|_| ApiError::internal())?;
    let _ = state
        .store
        .audit(
            ActorKind::Panel.as_str(),
            "settings.engine.update",
            None,
            None,
            crate::now_unix(),
        )
        .await;
    ok_json(json!({ "engine": view }))
}

#[derive(Debug, Deserialize)]
struct AuxUpdate {
    enabled: Option<bool>,
    base_url: Option<String>,
    model: Option<String>,
    session_header: Option<String>,
    api_key: Option<String>,
}

impl AuxUpdate {
    /// Validate the candidate values before anything is persisted or
    /// probed, so a malformed update can neither wedge the overlay file
    /// nor aim the gateway client at a non-HTTP target.
    fn into_patch(self) -> Result<AuxPatch, ApiError> {
        if let Some(url) = &self.base_url {
            let parsed = reqwest::Url::parse(url.trim())
                .map_err(|_| ApiError::bad_request("invalid_base_url"))?;
            if !matches!(parsed.scheme(), "http" | "https") || parsed.host_str().is_none() {
                return Err(ApiError::bad_request("invalid_base_url"));
            }
        }
        if let Some(model) = &self.model
            && (model.trim().is_empty() || model.chars().count() > 100)
        {
            return Err(ApiError::bad_request("invalid_model"));
        }
        if let Some(header) = &self.session_header
            && axum::http::header::HeaderName::try_from(header.trim()).is_err()
        {
            return Err(ApiError::bad_request("invalid_session_header"));
        }
        if let Some(key) = &self.api_key
            && key.chars().count() > 512
        {
            return Err(ApiError::bad_request("invalid_api_key"));
        }
        Ok(AuxPatch {
            enabled: self.enabled,
            base_url: self.base_url.map(|v| v.trim().to_string()),
            model: self.model.map(|v| v.trim().to_string()),
            session_header: self.session_header.map(|v| v.trim().to_string()),
            api_key: self.api_key,
        })
    }
}

async fn put_aux(State(state): State<Arc<AdminState>>, Json(body): Json<Value>) -> ApiResult {
    let update: AuxUpdate =
        serde_json::from_value(body).map_err(|_| ApiError::bad_request("invalid_body"))?;
    let patch = update.into_patch()?;
    let view = state
        .settings
        .update_aux(patch)
        .map_err(|_| ApiError::internal())?;
    let _ = state
        .store
        .audit(
            ActorKind::Panel.as_str(),
            "settings.aux.update",
            None,
            None,
            crate::now_unix(),
        )
        .await;
    ok_json(json!({ "aux": view }))
}

/// Probe a candidate gateway configuration (fields default to the
/// current effective values) without applying it.
async fn test_aux(State(state): State<Arc<AdminState>>, Json(body): Json<Value>) -> ApiResult {
    let update: AuxUpdate =
        serde_json::from_value(body).map_err(|_| ApiError::bad_request("invalid_body"))?;
    let patch = update.into_patch()?;
    let status = state.settings.test_aux(patch).await;
    ok_json(json!({ "ok": status.ready, "status": status }))
}

#[derive(Debug, Deserialize)]
struct RuntimeUpdate {
    session_rotation_enabled: Option<bool>,
    quota_degradation_enabled: Option<bool>,
}

async fn put_runtime(State(state): State<Arc<AdminState>>, Json(body): Json<Value>) -> ApiResult {
    let update: RuntimeUpdate =
        serde_json::from_value(body).map_err(|_| ApiError::bad_request("invalid_body"))?;
    if update.session_rotation_enabled.is_none() && update.quota_degradation_enabled.is_none() {
        return Err(ApiError::bad_request("empty_update"));
    }
    let view = state
        .settings
        .update_switches(
            update.session_rotation_enabled,
            update.quota_degradation_enabled,
        )
        .map_err(|_| ApiError::internal())?;
    let _ = state
        .store
        .audit(
            ActorKind::Panel.as_str(),
            "settings.runtime.update",
            None,
            None,
            crate::now_unix(),
        )
        .await;
    ok_json(json!({ "runtime": view }))
}
