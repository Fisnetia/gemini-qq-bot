//! The hot-swappable aux provider slot.
//!
//! The aux layer always has exactly one provider (the operator's dual-mode
//! design): the gateway when it is enabled, otherwise one dedicated Agy
//! session. The settings API can swap the provider at runtime: consumers
//! hold the slot (as [`AuxLlm`] / [`SummaryModel`]) and never rewire.

use std::path::PathBuf;
use std::sync::{Arc, RwLock};

use async_trait::async_trait;

use crate::error::ErrorClass;
use crate::integrations::agy_aux::{AgyAuxClient, AgyAuxConfig};
use crate::integrations::aux_gateway::{AuxGatewayClient, AuxGatewayConfig, AuxGatewayStatus};
use crate::integrations::aux_llm::AuxLlm;
use crate::memory::compaction::SummaryModel;

/// One concrete aux backend plus the fold bookkeeping that belongs to it.
pub struct AuxProvider {
    pub llm: Arc<dyn AuxLlm>,
    pub summary: Arc<dyn SummaryModel>,
    /// The concrete gateway client in gateway mode (for status/probe).
    pub gateway: Option<Arc<AuxGatewayClient>>,
    /// Memory origin tag recorded for this backend ('hy3' / 'agy').
    pub origin: &'static str,
    /// Model label recorded on fold chunks by this backend.
    pub model_label: String,
}

impl std::fmt::Debug for AuxProvider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AuxProvider")
            .field("mode", &self.llm.mode_name())
            .field("model_label", &self.model_label)
            .finish_non_exhaustive()
    }
}

impl AuxProvider {
    /// The health row from the cached gateway snapshot (startup path).
    pub async fn status_cached(&self) -> AuxGatewayStatus {
        match &self.gateway {
            Some(gateway) => gateway.status().await,
            None => AuxGatewayStatus::agy_mode(self.llm.healthy().await),
        }
    }

    /// The health row, probing the gateway (refresh-loop path).
    pub async fn status_probed(&self) -> AuxGatewayStatus {
        match &self.gateway {
            Some(gateway) => gateway.probe().await,
            None => AuxGatewayStatus::agy_mode(self.llm.healthy().await),
        }
    }
}

/// Inputs for the agy fallback backend, captured at startup.
#[derive(Debug, Clone)]
pub struct AgyAuxParams {
    pub bin: PathBuf,
    pub home: PathBuf,
    pub workspace: PathBuf,
    pub model: String,
    pub turn_limit: i64,
}

/// Build the provider for an effective gateway config: gateway mode when
/// the config is enabled, the dedicated Agy session otherwise.
pub fn build_provider(config: AuxGatewayConfig, agy: &AgyAuxParams) -> AuxProvider {
    if config.enabled {
        let model_label = config.model.clone();
        let client = Arc::new(AuxGatewayClient::new(config));
        AuxProvider {
            llm: Arc::clone(&client) as Arc<dyn AuxLlm>,
            summary: Arc::clone(&client) as Arc<dyn SummaryModel>,
            gateway: Some(client),
            origin: crate::store::origin::HY3,
            model_label,
        }
    } else {
        let client = Arc::new(AgyAuxClient::new(AgyAuxConfig {
            bin: agy.bin.clone(),
            home: agy.home.clone(),
            workspace: agy.workspace.clone(),
            model: agy.model.clone(),
            turn_limit: agy.turn_limit,
        }));
        AuxProvider {
            llm: Arc::clone(&client) as Arc<dyn AuxLlm>,
            summary: client as Arc<dyn SummaryModel>,
            gateway: None,
            origin: crate::store::origin::AGY,
            model_label: agy.model.clone(),
        }
    }
}

/// The shared, swappable provider slot.
#[derive(Debug)]
pub struct AuxSlot {
    current: RwLock<Arc<AuxProvider>>,
}

impl AuxSlot {
    pub fn new(provider: AuxProvider) -> Self {
        Self {
            current: RwLock::new(Arc::new(provider)),
        }
    }

    pub fn provider(&self) -> Arc<AuxProvider> {
        self.current
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    pub fn swap(&self, provider: AuxProvider) {
        *self.current.write().unwrap_or_else(|e| e.into_inner()) = Arc::new(provider);
    }

    pub fn origin(&self) -> &'static str {
        self.provider().origin
    }

    pub fn model_label(&self) -> String {
        self.provider().model_label.clone()
    }

    pub async fn status_cached(&self) -> AuxGatewayStatus {
        self.provider().status_cached().await
    }

    pub async fn status_probed(&self) -> AuxGatewayStatus {
        self.provider().status_probed().await
    }
}

#[async_trait]
impl AuxLlm for AuxSlot {
    async fn complete_streaming(
        &self,
        system: &str,
        prompt: &str,
        session: Option<&str>,
    ) -> Result<String, ErrorClass> {
        self.provider()
            .llm
            .complete_streaming(system, prompt, session)
            .await
    }

    async fn complete_vision_on(
        &self,
        model: &str,
        system: &str,
        prompt: &str,
        images: &[String],
        session: Option<&str>,
    ) -> Result<String, ErrorClass> {
        self.provider()
            .llm
            .complete_vision_on(model, system, prompt, images, session)
            .await
    }

    async fn complete_summary(&self, prompt: &str) -> Result<String, ErrorClass> {
        self.provider().llm.complete_summary(prompt).await
    }

    fn mode_name(&self) -> &'static str {
        self.provider().llm.mode_name()
    }

    async fn healthy(&self) -> bool {
        self.provider().llm.healthy().await
    }
}

#[async_trait]
impl SummaryModel for AuxSlot {
    async fn summarise(&self, prompt: String) -> Result<String, ErrorClass> {
        self.provider().summary.summarise(prompt).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn agy_params() -> AgyAuxParams {
        AgyAuxParams {
            bin: PathBuf::from("/absent/agy"),
            home: PathBuf::from("/absent/home"),
            workspace: PathBuf::from("/absent/ws"),
            model: "agy-model".to_string(),
            turn_limit: 35,
        }
    }

    #[test]
    fn disabled_config_builds_the_agy_provider() {
        let config = AuxGatewayConfig::new(None, None, PathBuf::from("/absent/key"), false);
        let provider = build_provider(config, &agy_params());
        assert_eq!(provider.llm.mode_name(), "agy");
        assert_eq!(provider.origin, crate::store::origin::AGY);
        assert!(provider.gateway.is_none());
    }

    #[test]
    fn enabled_config_builds_the_gateway_provider() {
        let config = AuxGatewayConfig::new(None, None, PathBuf::from("/absent/key"), true);
        let provider = build_provider(config, &agy_params());
        assert_eq!(provider.llm.mode_name(), "gateway");
        assert_eq!(provider.origin, crate::store::origin::HY3);
        assert!(provider.gateway.is_some());
    }

    #[test]
    fn swap_changes_the_active_provider() {
        let agy = agy_params();
        let slot = AuxSlot::new(build_provider(
            AuxGatewayConfig::new(None, None, PathBuf::from("/absent/key"), false),
            &agy,
        ));
        assert_eq!(slot.provider().llm.mode_name(), "agy");
        slot.swap(build_provider(
            AuxGatewayConfig::new(None, None, PathBuf::from("/absent/key"), true),
            &agy,
        ));
        assert_eq!(slot.provider().llm.mode_name(), "gateway");
    }
}
