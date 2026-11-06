//! Relevance judgement for ambient chatter: is this burst about her?
//!
//! Deliberately a separate judge from the burst gate in
//! [`crate::app::preflight`]. The burst gate scores how *unfinished* a
//! burst looks and, by design, has no authority over whether she
//! replies. This judge answers exactly that other question — and only
//! for windows nothing in which is addressed to her, only as an intake
//! claim gate at the group's [`AmbientPace`] tier. Once a batch is
//! claimed she still decides `should_reply` herself: a Relevant
//! verdict never forces a reply, and an Unrelated one only holds the
//! batch at the gate (it stays pending and can still be claimed
//! later, e.g. when an addressed message lands in the same window).

use std::sync::Arc;
use std::time::Duration;

use serde_json::Value;

use crate::integrations::aux_llm::AuxLlm;
use crate::onebot::normalize::IncomingMessage;
use crate::store::AmbientPace;

/// The judge has this long before intake treats it as unavailable;
/// aligned with the burst gate's timeout so a slow model cannot stall
/// a tick either way.
pub const JUDGE_TIMEOUT: Duration = Duration::from_secs(15);

/// Whether an ambient burst is about her, as judged by the model.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Relevance {
    Relevant,
    Unrelated,
    /// The judge could not answer. Intake treats it as Relevant
    /// (fail open): a broken judge must never silence her.
    Unavailable,
}

impl Relevance {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Relevant => "relevant",
            Self::Unrelated => "unrelated",
            Self::Unavailable => "unavailable",
        }
    }
}

/// The claim decision for one window, as a pure function so the whole
/// truth table is unit-testable. `addressed` windows always claim;
/// `cooldown_ready` is only consulted by the quiet tier.
pub fn gate_decision(
    pace: AmbientPace,
    addressed: bool,
    relevance: Relevance,
    cooldown_ready: bool,
) -> bool {
    if addressed {
        return true;
    }
    match pace {
        AmbientPace::Lively => true,
        AmbientPace::AddressedOnly => false,
        AmbientPace::Normal => relevance != Relevance::Unrelated,
        AmbientPace::Quiet => relevance != Relevance::Unrelated && cooldown_ready,
    }
}

/// The stable judging rules, passed as the call's system side. Byte
/// identical on every call, like the burst gate's rules.
pub const RELEVANCE_SYSTEM: &str = "\
You judge relevance for a QQ group chat. One member of the group is a bot \
named Gemini (in the log her lines are marked \"me\"). You are shown recent \
group log and a new burst of messages that nobody addressed to her directly \
(no @, no reply quoting her, not from the group owner). Decide whether the \
burst is RELEVANT to her: the members are talking about her, continuing a \
thread she is part of, reacting to something she said or did, or clearly \
leaving an opening that a present group member would naturally take. \
UNRELATED means the members are having their own exchange that has nothing \
to do with her. Answer with {\"relevant\":true} or {\"relevant\":false} only. \
When unsure, answer true.";

/// The relevance judge: one Aux (hy3) call per ambient window,
/// on its own per-group session.
pub struct RelevanceJudge {
    client: Arc<dyn AuxLlm>,
}

impl RelevanceJudge {
    pub fn new(client: Arc<dyn AuxLlm>) -> Self {
        Self { client }
    }

    /// The judge's own session per group, separate from the burst
    /// gate's: its context is the relevance question, not timing.
    pub fn session_id(bot_id: &str, group_id: &str) -> String {
        format!("relevance:{bot_id}:{group_id}")
    }

    pub async fn judge(
        &self,
        bot_id: &str,
        group_id: &str,
        messages: &[IncomingMessage],
        recent: &[(String, String)],
    ) -> Relevance {
        let prompt = build_relevance_prompt(messages, recent);
        let session = Self::session_id(bot_id, group_id);
        let raw = match tokio::time::timeout(
            JUDGE_TIMEOUT,
            self.client
                .complete_streaming(RELEVANCE_SYSTEM, &prompt, Some(&session)),
        )
        .await
        {
            Ok(Ok(raw)) => raw,
            // Timeout and upstream error take the same path: no answer
            // is not evidence of irrelevance.
            Ok(Err(_)) | Err(_) => return Relevance::Unavailable,
        };
        parse_relevance(&raw).unwrap_or(Relevance::Unavailable)
    }
}

/// Build the user-side prompt: recent log first, then the burst, in
/// the same rendering the burst gate uses.
pub fn build_relevance_prompt(messages: &[IncomingMessage], recent: &[(String, String)]) -> String {
    let mut out = String::with_capacity(1024);
    if !recent.is_empty() {
        out.push_str("Recent group log:\n");
        for (name, text) in recent {
            out.push_str("- ");
            out.push_str(name);
            out.push_str(": ");
            out.push_str(text);
            out.push('\n');
        }
        out.push('\n');
    }
    out.push_str(&format!("{} message(s) in this burst.\n", messages.len()));
    out.push_str("Burst messages:\n");
    for message in messages {
        out.push_str("- ");
        if !message.display_name.is_empty() {
            out.push_str(&message.display_name);
        } else {
            out.push_str(&message.speaker_ref);
        }
        out.push_str(": ");
        let text = message.text.trim();
        if text.is_empty() {
            out.push_str("(no text)");
        } else {
            out.push_str(text);
        }
        out.push('\n');
    }
    out
}

/// Parse the judge's JSON. Accepts a bare object or one wrapped in
/// prose or fences, like the burst gate's parser.
pub fn parse_relevance(raw: &str) -> Option<Relevance> {
    let trimmed = raw.trim();
    let candidate = if trimmed.starts_with('{') {
        trimmed
    } else {
        let start = trimmed.find('{')?;
        let end = trimmed.rfind('}')?;
        if end <= start {
            return None;
        }
        &trimmed[start..=end]
    };
    let value: Value = serde_json::from_str(candidate).ok()?;
    let relevant = value.get("relevant")?.as_bool()?;
    Some(if relevant {
        Relevance::Relevant
    } else {
        Relevance::Unrelated
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_relevance_covers_shapes() {
        assert_eq!(
            parse_relevance("{\"relevant\":true}"),
            Some(Relevance::Relevant)
        );
        assert_eq!(
            parse_relevance("结论：{\"relevant\": false}。"),
            Some(Relevance::Unrelated)
        );
        assert_eq!(parse_relevance("{\"relevant\":\"yes\"}"), None);
        assert_eq!(parse_relevance("没有 JSON"), None);
    }

    #[test]
    fn gate_decision_truth_table() {
        use AmbientPace::*;
        // Addressed always claims, at every tier.
        for pace in [AddressedOnly, Quiet, Normal, Lively] {
            assert!(gate_decision(pace, true, Relevance::Unrelated, false));
        }
        assert!(!gate_decision(
            AddressedOnly,
            false,
            Relevance::Relevant,
            true
        ));
        assert!(gate_decision(Lively, false, Relevance::Unrelated, false));
        assert!(gate_decision(Normal, false, Relevance::Relevant, false));
        assert!(gate_decision(Normal, false, Relevance::Unavailable, false));
        assert!(!gate_decision(Normal, false, Relevance::Unrelated, true));
        assert!(gate_decision(Quiet, false, Relevance::Relevant, true));
        assert!(!gate_decision(Quiet, false, Relevance::Relevant, false));
        assert!(!gate_decision(Quiet, false, Relevance::Unrelated, true));
    }
}
