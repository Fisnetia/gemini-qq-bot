//! Operator settings that can change at runtime (settings API).
//!
//! The config file stays the base layer. Changes made through
//! `PUT /api/v1/settings/*` are kept as an overlay in
//! `runtime-settings.json` next to the database (the deployed config file
//! is a read-only mount, so it is never rewritten), applied to the live
//! consumers immediately, and re-applied on top of the file config at the
//! next startup. The aux API key is write-only over the API: views report
//! only whether one is set.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Arc, RwLock};

use serde::{Deserialize, Serialize};

use crate::app::pacing::PacingEngine;
use crate::integrations::aux_gateway::{AuxGatewayClient, AuxGatewayConfig, AuxGatewayStatus};
use crate::integrations::aux_slot::{AgyAuxParams, AuxSlot, build_provider};

/// The persisted overlay. Every field is optional: absent means "the file
/// config decides".
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct SettingsOverlay {
    pub aux_enabled: Option<bool>,
    pub aux_base_url: Option<String>,
    pub aux_model: Option<String>,
    pub aux_session_header: Option<String>,
    pub aux_api_key: Option<String>,
    pub session_rotation_enabled: Option<bool>,
    pub quota_degradation_enabled: Option<bool>,
    pub main_model: Option<String>,
    pub main_effort: Option<String>,
}

impl SettingsOverlay {
    /// Load the overlay from `path`; a missing or malformed file is the
    /// empty overlay (the file config then decides everything).
    pub fn load(path: Option<&Path>) -> Self {
        let Some(path) = path else {
            return Self::default();
        };
        std::fs::read_to_string(path)
            .ok()
            .and_then(|raw| serde_json::from_str(&raw).ok())
            .unwrap_or_default()
    }
}

/// The model and effort main group sessions start with.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct MainEngine {
    pub model: String,
    pub effort: String,
}

/// Shared with the resident manager, which reads it at each session start.
pub type SharedEngine = Arc<RwLock<MainEngine>>;

/// One line of `agy models`.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct ModelOption {
    pub id: String,
    pub label: String,
}

/// Parse `agy models` output; lines that are not `id<TAB>label` are skipped.
pub fn parse_models(output: &str) -> Vec<ModelOption> {
    output
        .lines()
        .filter_map(|line| {
            let (id, label) = line.split_once('\t')?;
            let id = id.trim();
            let valid = !id.is_empty()
                && id
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'));
            valid.then(|| ModelOption {
                id: id.to_string(),
                label: label.trim().to_string(),
            })
        })
        .collect()
}

/// The gateway config with the overlay applied.
pub fn effective_aux_config(
    base: &AuxGatewayConfig,
    overlay: &SettingsOverlay,
) -> AuxGatewayConfig {
    let mut config = base.clone();
    if let Some(enabled) = overlay.aux_enabled {
        config.enabled = enabled;
    }
    if let Some(base_url) = &overlay.aux_base_url {
        config.base_url = base_url.clone();
    }
    if let Some(model) = &overlay.aux_model {
        config.model = model.clone();
    }
    if let Some(header) = &overlay.aux_session_header {
        config.session_header = header.clone();
    }
    config.api_key_override = overlay.aux_api_key.clone();
    config
}

/// A partial aux update from the settings API. `None` leaves a field
/// alone; for the API key, `Some("")` clears the overlay key.
#[derive(Debug, Clone, Default)]
pub struct AuxPatch {
    pub enabled: Option<bool>,
    pub base_url: Option<String>,
    pub model: Option<String>,
    pub session_header: Option<String>,
    pub api_key: Option<String>,
}

/// What the settings API reports about the aux layer.
#[derive(Debug, Clone, Serialize)]
pub struct AuxSettingsView {
    pub mode: &'static str,
    pub enabled: bool,
    pub base_url: String,
    pub model: String,
    pub session_header: String,
    pub api_key_set: bool,
}

/// What the settings API reports about the two runtime switches.
#[derive(Debug, Clone, Serialize)]
pub struct SwitchView {
    pub session_rotation_enabled: bool,
    pub quota_degradation_enabled: bool,
}

/// Live settings: the overlay plus the handles it steers.
pub struct RuntimeSettings {
    path: Option<PathBuf>,
    overlay: RwLock<SettingsOverlay>,
    base_aux: AuxGatewayConfig,
    agy: AgyAuxParams,
    slot: Arc<AuxSlot>,
    pacing: Arc<PacingEngine>,
    rotation: Arc<AtomicI64>,
    /// The cap that applies while rotation is on (already resolved).
    rotation_cap: Option<i64>,
    file_rotation_enabled: bool,
    file_degradation_enabled: bool,
    engine: SharedEngine,
}

impl RuntimeSettings {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        path: Option<PathBuf>,
        overlay: SettingsOverlay,
        base_aux: AuxGatewayConfig,
        agy: AgyAuxParams,
        slot: Arc<AuxSlot>,
        pacing: Arc<PacingEngine>,
        rotation: Arc<AtomicI64>,
        rotation_cap: Option<i64>,
        file_rotation_enabled: bool,
        file_degradation_enabled: bool,
        engine: SharedEngine,
    ) -> Arc<Self> {
        Arc::new(Self {
            path,
            overlay: RwLock::new(overlay),
            base_aux,
            agy,
            slot,
            pacing,
            rotation,
            rotation_cap,
            file_rotation_enabled,
            file_degradation_enabled,
            engine,
        })
    }

    /// A settings handle over a scratch directory, for admin route tests.
    #[cfg(test)]
    pub fn for_tests(dir: PathBuf, pacing: Arc<PacingEngine>) -> Arc<Self> {
        let base_aux = AuxGatewayConfig::new(None, None, dir.join("absent-key"), false);
        let agy = AgyAuxParams {
            bin: dir.join("absent-agy"),
            home: dir.join("absent-home"),
            workspace: dir.join("aux-ws"),
            model: "test-model".to_string(),
            turn_limit: 35,
        };
        let provider = build_provider(base_aux.clone(), &agy);
        Self::new(
            Some(dir.join("runtime-settings.json")),
            SettingsOverlay::default(),
            base_aux,
            agy,
            Arc::new(AuxSlot::new(provider)),
            pacing,
            Arc::new(AtomicI64::new(-1)),
            Some(35),
            true,
            true,
            Arc::new(RwLock::new(MainEngine {
                model: "test-model".to_string(),
                effort: "low".to_string(),
            })),
        )
    }

    fn overlay(&self) -> SettingsOverlay {
        self.overlay
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    pub fn aux_view(&self) -> AuxSettingsView {
        let config = effective_aux_config(&self.base_aux, &self.overlay());
        let api_key_set = config.read_api_key().is_some();
        AuxSettingsView {
            mode: self.slot.provider().llm.mode_name(),
            enabled: config.enabled,
            base_url: config.base_url,
            model: config.model,
            session_header: config.session_header,
            api_key_set,
        }
    }

    pub fn engine_view(&self) -> MainEngine {
        self.engine
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    /// Persist and apply the model and effort. Sessions that are already
    /// running keep theirs until they next start.
    pub fn update_engine(
        &self,
        model: Option<String>,
        effort: Option<String>,
    ) -> Result<MainEngine, String> {
        let mut overlay = self.overlay();
        let mut engine = self.engine_view();
        if let Some(model) = model {
            overlay.main_model = Some(model.clone());
            engine.model = model;
        }
        if let Some(effort) = effort {
            overlay.main_effort = Some(effort.clone());
            engine.effort = effort;
        }
        self.persist(&overlay)?;
        *self.overlay.write().unwrap_or_else(|e| e.into_inner()) = overlay;
        *self.engine.write().unwrap_or_else(|e| e.into_inner()) = engine.clone();
        Ok(engine)
    }

    /// An Agy child with the same minimal environment for each query.
    fn agy_command(&self) -> tokio::process::Command {
        let mut command = tokio::process::Command::new(&self.agy.bin);
        command
            .current_dir(&self.agy.home)
            .env_clear()
            .env("HOME", &self.agy.home)
            .stdin(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .kill_on_drop(true);
        if let Ok(path) = std::env::var("PATH") {
            command.env("PATH", path);
        }
        command
    }

    /// Whether Agy accepts this model and effort. `/effort` is answered locally,
    /// so the check spends no quota. If Agy cannot be asked, the choice stands:
    /// only a definite refusal is an error.
    pub async fn engine_is_supported(&self, model: &str, effort: &str) -> bool {
        let mut command = self.agy_command();
        command.arg("--model").arg(model);
        if !effort.is_empty() {
            command.arg("--effort").arg(effort);
        }
        command.arg("-p").arg("/effort");
        match tokio::time::timeout(std::time::Duration::from_secs(30), command.output()).await {
            Ok(Ok(output)) => output.status.success(),
            _ => true,
        }
    }

    /// The models this account can use; empty when `agy models` fails.
    pub async fn list_models(&self) -> Vec<ModelOption> {
        let mut command = self.agy_command();
        command.arg("models");
        match tokio::time::timeout(std::time::Duration::from_secs(30), command.output()).await {
            Ok(Ok(output)) if output.status.success() => {
                parse_models(&String::from_utf8_lossy(&output.stdout))
            }
            _ => Vec::new(),
        }
    }

    pub fn switch_view(&self) -> SwitchView {
        let overlay = self.overlay();
        SwitchView {
            session_rotation_enabled: overlay
                .session_rotation_enabled
                .unwrap_or(self.file_rotation_enabled),
            quota_degradation_enabled: overlay
                .quota_degradation_enabled
                .unwrap_or(self.file_degradation_enabled),
        }
    }

    /// Apply an aux patch: persist first, then swap the live provider, so
    /// a failed write changes nothing.
    pub fn update_aux(&self, patch: AuxPatch) -> Result<AuxSettingsView, String> {
        let mut overlay = self.overlay();
        if let Some(enabled) = patch.enabled {
            overlay.aux_enabled = Some(enabled);
        }
        if let Some(base_url) = patch.base_url {
            overlay.aux_base_url = Some(base_url);
        }
        if let Some(model) = patch.model {
            overlay.aux_model = Some(model);
        }
        if let Some(header) = patch.session_header {
            overlay.aux_session_header = Some(header);
        }
        if let Some(key) = patch.api_key {
            overlay.aux_api_key = if key.is_empty() { None } else { Some(key) };
        }
        self.persist(&overlay)?;
        let config = effective_aux_config(&self.base_aux, &overlay);
        self.slot.swap(build_provider(config, &self.agy));
        *self.overlay.write().unwrap_or_else(|e| e.into_inner()) = overlay;
        Ok(self.aux_view())
    }

    /// Probe a candidate gateway config without swapping it in.
    pub async fn test_aux(&self, patch: AuxPatch) -> AuxGatewayStatus {
        let mut overlay = self.overlay();
        if let Some(base_url) = patch.base_url {
            overlay.aux_base_url = Some(base_url);
        }
        if let Some(model) = patch.model {
            overlay.aux_model = Some(model);
        }
        if let Some(header) = patch.session_header {
            overlay.aux_session_header = Some(header);
        }
        if let Some(key) = patch.api_key {
            overlay.aux_api_key = if key.is_empty() { None } else { Some(key) };
        }
        let mut config = effective_aux_config(&self.base_aux, &overlay);
        config.enabled = true;
        AuxGatewayClient::new(config).probe().await
    }

    /// Apply the runtime switches: persist first, then flip the live
    /// handles (pacing degradation flag, shared rotation cap).
    pub fn update_switches(
        &self,
        rotation_enabled: Option<bool>,
        degradation_enabled: Option<bool>,
    ) -> Result<SwitchView, String> {
        let mut overlay = self.overlay();
        if let Some(enabled) = rotation_enabled {
            overlay.session_rotation_enabled = Some(enabled);
        }
        if let Some(enabled) = degradation_enabled {
            overlay.quota_degradation_enabled = Some(enabled);
        }
        self.persist(&overlay)?;
        *self.overlay.write().unwrap_or_else(|e| e.into_inner()) = overlay;
        if let Some(enabled) = degradation_enabled {
            self.pacing.set_degradation_enabled(enabled);
        }
        if let Some(enabled) = rotation_enabled {
            let raw = if enabled {
                self.rotation_cap.unwrap_or(-1)
            } else {
                -1
            };
            self.rotation.store(raw, Ordering::Relaxed);
        }
        Ok(self.switch_view())
    }

    fn persist(&self, overlay: &SettingsOverlay) -> Result<(), String> {
        let Some(path) = &self.path else {
            return Ok(());
        };
        let raw = serde_json::to_string_pretty(overlay).map_err(|e| e.to_string())?;
        std::fs::write(path, raw).map_err(|e| e.to_string())?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn model_list_keeps_only_id_and_label_lines() {
        let parsed = parse_models(
            "Fetching available models...\n\
             gemini-3.8-flash-high\tGemini 3.8 Flash (High)\n\
             bad id\tBad\n\
             gpt-oss-120b-medium\tGPT-OSS 120B (Medium)\n",
        );
        assert_eq!(parsed.len(), 2);
        assert_eq!(parsed[0].id, "gemini-3.8-flash-high");
        assert_eq!(parsed[1].label, "GPT-OSS 120B (Medium)");
    }

    #[test]
    fn overlay_round_trips_through_json() {
        let overlay = SettingsOverlay {
            aux_enabled: Some(true),
            aux_model: Some("m".to_string()),
            quota_degradation_enabled: Some(false),
            ..Default::default()
        };
        let raw = serde_json::to_string(&overlay).expect("serialize");
        let back: SettingsOverlay = serde_json::from_str(&raw).expect("parse");
        assert_eq!(back, overlay);
    }

    #[test]
    fn missing_overlay_file_loads_as_default() {
        let overlay = SettingsOverlay::load(Some(Path::new("/absent/runtime-settings.json")));
        assert_eq!(overlay, SettingsOverlay::default());
        assert_eq!(SettingsOverlay::load(None), SettingsOverlay::default());
    }

    #[test]
    fn effective_config_prefers_overlay_and_key_override() {
        let base = AuxGatewayConfig::new(
            Some("http://base:1".to_string()),
            Some("base-model".to_string()),
            PathBuf::from("/absent/key"),
            false,
        );
        let overlay = SettingsOverlay {
            aux_enabled: Some(true),
            aux_base_url: Some("http://override:2".to_string()),
            aux_api_key: Some("secret".to_string()),
            ..Default::default()
        };
        let config = effective_aux_config(&base, &overlay);
        assert!(config.enabled);
        assert_eq!(config.base_url, "http://override:2");
        assert_eq!(config.model, "base-model");
        assert_eq!(config.read_api_key().as_deref(), Some("secret"));
    }
}
