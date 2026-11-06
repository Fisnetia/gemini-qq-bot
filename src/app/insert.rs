//! Whether a newly claimed batch may be folded into a running resident turn.
//!
//! A resident session is one long conversation, so a fresh batch that arrives while
//! a turn is in flight can be *inserted* into the open turn instead of starting a
//! second one. That is the whole point of residency: the agent keeps thinking about
//! the conversation as it actually went.
//!
//! Insertion is not free, though. Every inserted batch widens the turn and muddies
//! what "the answer" refers to, so it is gated twice:
//!
//! 1. **Hard limits** — the group must have opted in, the pacing tier must be T0/T1
//!    (the account can afford the extra thinking), the turn must be young enough to
//!    still be answering the same conversation, and the per-turn insert budget must
//!    not be spent.
//! 2. **Value** — something about the batch must make it worth folding in rather than
//!    leaving it for the next turn. A plain "哈哈" from a third party is not.
//!
//! Everything here is a pure function of numbers and flags so the policy can be
//! tested exhaustively without a session, a store or a clock.

use crate::app::pacing::Tier;

/// Why a batch is worth inserting into the running turn.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InsertReason {
    /// The batch addresses the bot directly.
    Mention,
    /// The owner spoke. Owner input outranks everything else in this bot.
    Owner,
    /// Retired. A picture used to be a reason to insert; describing it on the
    /// insert path stalled the running turn, and inserting the text without
    /// the picture split them. Pictures now wait for the next full turn
    /// ([`HoldReason::ImageDeferred`]).
    #[allow(dead_code)]
    Image,
    /// Same speaker as the running turn, immediately after it: a continuation.
    SameSpeaker,
    /// Same topic as the running turn, immediately after it: a continuation.
    SameTopic,
}

impl InsertReason {
    pub fn as_str(self) -> &'static str {
        match self {
            InsertReason::Mention => "mention",
            InsertReason::Owner => "owner",
            InsertReason::Image => "image",
            InsertReason::SameSpeaker => "same_speaker",
            InsertReason::SameTopic => "same_topic",
        }
    }
}

/// Why a batch was held back for the next turn instead.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HoldReason {
    /// The group has not opted into insertion.
    Disabled,
    /// The pacing tier is T2/T3: spend the call on one answer, not on widening it.
    PacingTier,
    /// This turn already absorbed its insertion budget.
    InsertLimit,
    /// The turn is past its soft cap; stop widening it so it can finish.
    SoftCap,
    /// Nothing about the batch justified widening the turn.
    Uninteresting,
    /// The batch contains a picture. The next full turn describes it; folding
    /// the text in now would either wait on vision or drop the picture.
    ImageDeferred,
}

impl HoldReason {
    pub fn as_str(self) -> &'static str {
        match self {
            HoldReason::Disabled => "disabled",
            HoldReason::PacingTier => "pacing_tier",
            HoldReason::InsertLimit => "insert_limit",
            HoldReason::SoftCap => "soft_cap",
            HoldReason::Uninteresting => "uninteresting",
            HoldReason::ImageDeferred => "image_deferred",
        }
    }
}

/// What the running turn and the candidate batch look like right now.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct InsertSignals {
    /// The batch addresses the bot.
    pub mentions_bot: bool,
    /// The batch contains a message from the owner.
    pub from_owner: bool,
    /// The batch contains at least one image.
    pub has_image: bool,
    /// Every message in the batch is from the speaker that opened the running turn.
    pub same_speaker: bool,
    /// The batch arrived within the continuation window of the running turn and
    /// shares the running turn's topic marker.
    pub same_topic: bool,
}

/// The policy inputs that are not about the batch's content.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InsertLimits {
    /// The group's `resident_insert_enabled` flag.
    pub enabled: bool,
    /// Current pacing tier for the account.
    pub tier: Tier,
    /// Inserts this turn has already absorbed.
    pub inserts_so_far: u32,
    /// Configured per-turn budget.
    pub max_inserts: u32,
    /// Age of the turn in seconds.
    pub turn_age_s: i64,
    /// Past this age the turn stops widening.
    pub soft_cap_s: i64,
}

/// The verdict.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InsertDecision {
    Accept(InsertReason),
    Hold(HoldReason),
}

impl InsertDecision {
    pub fn is_accept(self) -> bool {
        matches!(self, InsertDecision::Accept(_))
    }

    pub fn reason_str(self) -> &'static str {
        match self {
            InsertDecision::Accept(reason) => reason.as_str(),
            InsertDecision::Hold(reason) => reason.as_str(),
        }
    }
}

/// Decide whether to fold the batch into the running turn.
///
/// Order matters and is part of the contract: the cheap hard limits are checked
/// first so a T3 tier or a spent budget short-circuits before any content
/// judgement, and the value checks then run strongest-signal first so the audit
/// trail names the best reason the batch had.
pub fn decide(signals: &InsertSignals, limits: &InsertLimits) -> InsertDecision {
    if !limits.enabled {
        return InsertDecision::Hold(HoldReason::Disabled);
    }
    if limits.inserts_so_far >= limits.max_inserts {
        return InsertDecision::Hold(HoldReason::InsertLimit);
    }
    if limits.turn_age_s >= limits.soft_cap_s {
        return InsertDecision::Hold(HoldReason::SoftCap);
    }
    // T2 is "@ or owner only" and T3 is "owner only" for *starting* calls. An
    // insertion is not a new agent call, but it does extend an expensive one, so
    // the same caution applies: only the two cheap tiers may widen a turn.
    if !matches!(limits.tier, Tier::T0 | Tier::T1) {
        return InsertDecision::Hold(HoldReason::PacingTier);
    }
    // A picture has to wait even when the same batch also mentions the bot
    // or comes from the owner. Claiming it here would take it away from the
    // next full turn, which is the only path that describes it.
    if signals.has_image {
        return InsertDecision::Hold(HoldReason::ImageDeferred);
    }
    if signals.mentions_bot {
        return InsertDecision::Accept(InsertReason::Mention);
    }
    if signals.from_owner {
        return InsertDecision::Accept(InsertReason::Owner);
    }
    if signals.same_speaker {
        return InsertDecision::Accept(InsertReason::SameSpeaker);
    }
    if signals.same_topic {
        return InsertDecision::Accept(InsertReason::SameTopic);
    }
    InsertDecision::Hold(HoldReason::Uninteresting)
}

/// A batch arriving this soon after the running turn's last message is treated as
/// a continuation of it, not as a new topic.
pub const CONTINUATION_WINDOW_SECONDS: i64 = 60;

/// Whether a message arrived close enough behind the running turn to count as a
/// continuation of it.
pub fn is_continuation(gap_seconds: i64) -> bool {
    (0..=CONTINUATION_WINDOW_SECONDS).contains(&gap_seconds)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn limits(tier: Tier) -> InsertLimits {
        InsertLimits {
            enabled: true,
            tier,
            inserts_so_far: 0,
            max_inserts: 3,
            turn_age_s: 10,
            soft_cap_s: 120,
        }
    }

    fn plain() -> InsertSignals {
        InsertSignals::default()
    }

    #[test]
    fn decide_covers_each_signal_and_hold() {
        let mut disabled = limits(Tier::T0);
        disabled.enabled = false;
        let mut spent = limits(Tier::T0);
        spent.inserts_so_far = 3;
        let mut soft = limits(Tier::T0);
        soft.turn_age_s = 120;
        let all_signals = InsertSignals {
            mentions_bot: true,
            from_owner: true,
            has_image: true,
            same_speaker: true,
            same_topic: true,
        };
        let cases = [
            (
                "mention",
                InsertSignals {
                    mentions_bot: true,
                    ..plain()
                },
                limits(Tier::T0),
                InsertDecision::Accept(InsertReason::Mention),
            ),
            (
                "owner",
                InsertSignals {
                    from_owner: true,
                    ..plain()
                },
                limits(Tier::T1),
                InsertDecision::Accept(InsertReason::Owner),
            ),
            (
                "image deferred",
                InsertSignals {
                    has_image: true,
                    mentions_bot: true,
                    from_owner: true,
                    ..plain()
                },
                limits(Tier::T0),
                InsertDecision::Hold(HoldReason::ImageDeferred),
            ),
            (
                "same speaker",
                InsertSignals {
                    same_speaker: true,
                    ..plain()
                },
                limits(Tier::T0),
                InsertDecision::Accept(InsertReason::SameSpeaker),
            ),
            (
                "same topic",
                InsertSignals {
                    same_topic: true,
                    ..plain()
                },
                limits(Tier::T0),
                InsertDecision::Accept(InsertReason::SameTopic),
            ),
            (
                "strongest signal",
                InsertSignals {
                    mentions_bot: true,
                    from_owner: true,
                    has_image: false,
                    same_speaker: true,
                    same_topic: true,
                },
                limits(Tier::T0),
                InsertDecision::Accept(InsertReason::Mention),
            ),
            (
                "unrelated",
                plain(),
                limits(Tier::T0),
                InsertDecision::Hold(HoldReason::Uninteresting),
            ),
            (
                "disabled",
                InsertSignals {
                    mentions_bot: true,
                    ..plain()
                },
                disabled,
                InsertDecision::Hold(HoldReason::Disabled),
            ),
            (
                "insert limit",
                InsertSignals {
                    mentions_bot: true,
                    ..plain()
                },
                spent,
                InsertDecision::Hold(HoldReason::InsertLimit),
            ),
            (
                "soft cap",
                InsertSignals {
                    from_owner: true,
                    ..plain()
                },
                soft,
                InsertDecision::Hold(HoldReason::SoftCap),
            ),
            (
                "t2 pacing",
                all_signals.clone(),
                limits(Tier::T2),
                InsertDecision::Hold(HoldReason::PacingTier),
            ),
            (
                "t3 pacing",
                all_signals,
                limits(Tier::T3),
                InsertDecision::Hold(HoldReason::PacingTier),
            ),
        ];
        for (label, signals, limits, expected) in cases {
            assert_eq!(decide(&signals, &limits), expected, "{label}");
        }
    }

    #[test]
    fn tier_ordering_does_not_leak_into_the_tier_check() {
        // T0 and T1 accept; T2 and T3 hold. Asserted as a table so a future tier
        // inserted between them fails here instead of silently widening turns.
        let signals = InsertSignals {
            mentions_bot: true,
            ..plain()
        };
        let accepted: Vec<Tier> = [Tier::T0, Tier::T1, Tier::T2, Tier::T3]
            .into_iter()
            .filter(|tier| decide(&signals, &limits(*tier)).is_accept())
            .collect();
        assert_eq!(accepted, vec![Tier::T0, Tier::T1]);
    }

    #[test]
    fn continuation_window_boundaries() {
        assert!(is_continuation(0));
        assert!(is_continuation(CONTINUATION_WINDOW_SECONDS));
        assert!(!is_continuation(CONTINUATION_WINDOW_SECONDS + 1));
        assert!(!is_continuation(-1));
    }
}
