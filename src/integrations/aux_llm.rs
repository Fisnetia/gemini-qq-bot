//! The auxiliary-LLM seam.
//!
//! Every side job — cadence and relevance judging, image tagging, memory
//! summarising and extraction, flood summaries — talks to this trait and
//! never to a concrete backend. Two backends implement it:
//!
//! * **gateway**: an OpenAI-compatible HTTP gateway
//!   ([`crate::integrations::aux_gateway::AuxGatewayClient`]).
//! * **agy**: one dedicated Agy session
//!   ([`crate::integrations::agy_aux::AgyAuxClient`]) that takes the same
//!   jobs over when no gateway is configured.
//!
//! Which backend runs is decided at startup (gateway enabled →
//! gateway, otherwise agy) and can be swapped at runtime through the
//! settings API via [`crate::integrations::aux_slot::AuxSlot`]. There is
//! deliberately no third "off" state:
//! the aux layer always has a provider, and every caller keeps its
//! fail-open behaviour when the provider errors, whichever it is.

use async_trait::async_trait;

use crate::error::ErrorClass;
use crate::integrations::aux_gateway::AuxGatewayClient;

#[async_trait]
pub trait AuxLlm: Send + Sync + std::fmt::Debug {
    /// One text completion. `session` is the gateway's sticky-session id;
    /// the agy backend ignores it (it has exactly one session).
    async fn complete_streaming(
        &self,
        system: &str,
        prompt: &str,
        session: Option<&str>,
    ) -> Result<String, ErrorClass>;

    /// One multimodal completion; `images` are data URLs. `model` is the
    /// gateway-side model choice (the tagger fans out over model ids);
    /// the agy backend ignores it.
    async fn complete_vision_on(
        &self,
        model: &str,
        system: &str,
        prompt: &str,
        images: &[String],
        session: Option<&str>,
    ) -> Result<String, ErrorClass>;

    /// Memory compaction's pinned-prompt completion.
    async fn complete_summary(&self, prompt: &str) -> Result<String, ErrorClass>;

    /// `"gateway"` or `"agy"`, for status surfaces.
    fn mode_name(&self) -> &'static str;

    /// Cheap liveness for the health snapshot. Must not issue a paid
    /// model request.
    async fn healthy(&self) -> bool;
}

#[async_trait]
impl AuxLlm for AuxGatewayClient {
    async fn complete_streaming(
        &self,
        system: &str,
        prompt: &str,
        session: Option<&str>,
    ) -> Result<String, ErrorClass> {
        AuxGatewayClient::complete_streaming(self, system, prompt, session).await
    }

    async fn complete_vision_on(
        &self,
        model: &str,
        system: &str,
        prompt: &str,
        images: &[String],
        session: Option<&str>,
    ) -> Result<String, ErrorClass> {
        AuxGatewayClient::complete_vision_on(self, model, system, prompt, images, session).await
    }

    async fn complete_summary(&self, prompt: &str) -> Result<String, ErrorClass> {
        AuxGatewayClient::complete_summary(self, prompt).await
    }

    fn mode_name(&self) -> &'static str {
        "gateway"
    }

    async fn healthy(&self) -> bool {
        self.status().await.ready
    }
}
