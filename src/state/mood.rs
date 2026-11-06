//! Mood and topic state for one group.
//!
//! Both are advisory: they only shape the prompt, never permissions. A malformed stored
//! value degrades to "unknown" instead of failing the batch.

use serde::{Deserialize, Serialize};

use crate::agent::contract::{MAX_MOOD_LABEL_CHARS, MAX_MOOD_REASON_CHARS, MAX_TOPIC_CHARS};

/// Mood as recorded in `group_state.mood_json`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct MoodState {
    /// Short label such as `calm`, `tense`, `playful`.
    pub label: String,
    /// Why the model chose it. Kept for the operator, not shown to the group.
    pub reason: String,
    /// When it was last updated.
    #[serde(default)]
    pub updated_at: i64,
}

impl MoodState {
    pub fn new(label: &str, reason: &str, updated_at: i64) -> Option<Self> {
        let label = label.trim();
        if label.is_empty() || label.chars().count() > MAX_MOOD_LABEL_CHARS {
            return None;
        }
        let reason = reason.trim();
        let reason = if reason.chars().count() > MAX_MOOD_REASON_CHARS {
            reason.chars().take(MAX_MOOD_REASON_CHARS).collect()
        } else {
            reason.to_string()
        };
        Some(Self {
            label: label.to_string(),
            reason,
            updated_at,
        })
    }

    /// Serialise for storage. Never fails: the value is a plain struct.
    pub fn to_json(&self) -> String {
        serde_json::to_string(self).unwrap_or_else(|_| "{}".to_string())
    }

    /// Parse a stored value, returning `None` for anything unusable.
    pub fn from_json(raw: &str) -> Option<Self> {
        let value: serde_json::Value = serde_json::from_str(raw).ok()?;
        // Accept both the struct form and a bare label string.
        if let Some(label) = value.as_str() {
            return Self::new(label, "", 0);
        }
        let state: MoodState = serde_json::from_value(value).ok()?;
        if state.label.trim().is_empty() {
            return None;
        }
        Some(state)
    }

    /// The label for the prompt, if any.
    pub fn label_or_none(&self) -> Option<&str> {
        if self.label.trim().is_empty() {
            None
        } else {
            Some(self.label.as_str())
        }
    }
}

/// Topic state as recorded in `group_state.topic_state`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TopicState(pub String);

impl TopicState {
    /// Accept only a non-empty, in-budget topic string.
    pub fn new(raw: &str) -> Option<Self> {
        let trimmed = raw.trim();
        if trimmed.is_empty() || trimmed.chars().count() > MAX_TOPIC_CHARS {
            return None;
        }
        // A topic must not smuggle CQ codes into the prompt.
        if trimmed.contains("[CQ:") {
            return None;
        }
        Some(Self(trimmed.to_string()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn is_empty(&self) -> bool {
        self.0.trim().is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mood_round_trips() {
        let m = MoodState::new("calm", "话题温和", 1_700_000_000).expect("应构造成功");
        let json = m.to_json();
        let back = MoodState::from_json(&json).expect("应解析成功");
        assert_eq!(back.label, "calm");
        assert_eq!(back.reason, "话题温和");
        assert_eq!(back.updated_at, 1_700_000_000);
    }

    #[test]
    fn bare_label_string_is_accepted() {
        let back = MoodState::from_json("\"playful\"").expect("应解析成功");
        assert_eq!(back.label, "playful");
        assert!(back.reason.is_empty());
    }

    #[test]
    fn invalid_mood_json_degrades_to_none() {
        let overlong = format!("\"{}\"", "字".repeat(MAX_MOOD_LABEL_CHARS + 1));
        for raw in [
            "not json",
            "{}",
            r#"{"label":""}"#,
            "null",
            overlong.as_str(),
            "\"\"",
            "\"   \"",
        ] {
            assert!(MoodState::from_json(raw).is_none(), "应拒绝: {raw}");
        }
    }

    #[test]
    fn overlong_mood_label_is_refused() {
        assert!(MoodState::new(&"长".repeat(MAX_MOOD_LABEL_CHARS + 1), "", 0).is_none());
    }

    #[test]
    fn overlong_reason_is_truncated_not_dropped() {
        let m = MoodState::new("calm", &"长".repeat(MAX_MOOD_REASON_CHARS + 50), 0)
            .expect("应构造成功");
        assert_eq!(m.reason.chars().count(), MAX_MOOD_REASON_CHARS);
    }

    #[test]
    fn topic_state_validates() {
        assert_eq!(TopicState::new("  闲聊中 ").unwrap().as_str(), "闲聊中");
        assert!(TopicState::new("").is_none());
        assert!(TopicState::new(&"长".repeat(MAX_TOPIC_CHARS + 1)).is_none());
        // CQ injection is refused outright.
        assert!(TopicState::new("[CQ:at,qq=all]").is_none());
    }

    #[test]
    fn mood_label_at_the_budget_boundary_is_accepted() {
        let exact = "字".repeat(MAX_MOOD_LABEL_CHARS);
        let m = MoodState::new(&exact, "", 0).expect("恰好达上限应当接受");
        assert_eq!(m.label.chars().count(), MAX_MOOD_LABEL_CHARS);
        // Trimmed length, not raw length, is what the budget measures.
        let padded = format!("  {}  ", "字".repeat(MAX_MOOD_LABEL_CHARS));
        assert!(MoodState::new(&padded, "", 0).is_some());
    }

    #[test]
    fn mood_reason_at_the_budget_boundary_is_kept_intact() {
        let exact = "因".repeat(MAX_MOOD_REASON_CHARS);
        let m = MoodState::new("calm", &exact, 0).expect("应构造成功");
        assert_eq!(m.reason.chars().count(), MAX_MOOD_REASON_CHARS);
        assert_eq!(m.reason, exact);
    }

    #[test]
    fn a_blank_label_is_never_a_mood() {
        assert!(MoodState::new("", "有理由", 0).is_none());
        assert!(MoodState::new("   \t ", "有理由", 0).is_none());
        assert!(MoodState::new("\n", "", 0).is_none());
    }

    #[test]
    fn stored_timestamp_survives_the_round_trip_and_defaults_to_zero() {
        let m = MoodState::new("tense", "争吵", 1_700_000_123).expect("应构造成功");
        assert_eq!(
            MoodState::from_json(&m.to_json()).unwrap().updated_at,
            1_700_000_123
        );
        // A row written before `updated_at` existed must still parse.
        let legacy =
            MoodState::from_json(r#"{"label":"calm","reason":"平稳"}"#).expect("旧行应可读");
        assert_eq!(legacy.updated_at, 0);
        assert_eq!(legacy.label, "calm");
    }

    #[test]
    fn label_or_none_matches_what_the_prompt_would_see() {
        assert_eq!(
            MoodState::new("playful", "", 0).unwrap().label_or_none(),
            Some("playful")
        );
        // Default is the empty struct, which is what a group without mood looks like.
        assert!(MoodState::default().label_or_none().is_none());
        let padded = MoodState {
            label: "  ".into(),
            ..MoodState::default()
        };
        assert!(padded.label_or_none().is_none());
    }

    #[test]
    fn to_json_never_fails_and_stays_valid_json() {
        // Any value must serialise: a group must not brick on a mood write.
        for m in [
            MoodState::default(),
            MoodState::new("calm", "", 0).unwrap(),
            MoodState::new("calm", &"长".repeat(MAX_MOOD_REASON_CHARS + 10), 5).unwrap(),
        ] {
            let json = m.to_json();
            assert!(
                serde_json::from_str::<serde_json::Value>(&json).is_ok(),
                "非法 JSON: {json}"
            );
            assert!(MoodState::from_json(&json).is_some() || m.label.trim().is_empty());
        }
    }

    #[test]
    fn topic_exactly_at_the_budget_is_kept() {
        let exact = "题".repeat(MAX_TOPIC_CHARS);
        let t = TopicState::new(&exact).expect("恰好达上限应当接受");
        assert_eq!(t.as_str().chars().count(), MAX_TOPIC_CHARS);
        assert!(!t.is_empty());
    }

    #[test]
    fn topic_is_trimmed_and_blank_is_empty() {
        assert_eq!(
            TopicState::new("  周末盘游戏  ").unwrap().as_str(),
            "周末盘游戏"
        );
        assert!(TopicState::new("\n\t ").is_none());
        assert!(TopicState::default().is_empty());
        assert!(TopicState("   ".into()).is_empty());
        assert!(!TopicState("有内容".into()).is_empty());
    }
}
