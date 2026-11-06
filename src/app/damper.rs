//! Single-partner damping: the deterministic layer that notices when a group has
//! turned into one person and the bot answering them.
//!
//! The observed failure is 13 minutes of one member and the bot trading 111 and 49
//! messages while every existing limiter stayed green: the hourly fuse counts calls,
//! the cadence gate judges one burst at a time, and quota pacing only knows the tier.
//! None of them can see a *conversation shape*, because a shape is a fact about a
//! window, not about a batch.
//!
//! This module is that window, and nothing else. It is a pure state machine: no
//! store, no clock of its own, no model. [`Damper::observe`] folds the pending rows
//! into a ten-minute window and returns a [`Verdict`]; the caller decides what to do
//! with it. Every input it cannot handle (poisoned lock, missing rows) degrades to
//! "no damping" rather than to an error: silence caused by a bug is worse than a
//! conversation that ran long.
//!
//! Levels:
//!
//! * `L1` — six rounds in a row with the same partner, or a very concentrated window.
//!   The quiet window before a claim grows to [`L1_QUIET_SECONDS`].
//! * `L2` — ten in a row. Quiet grows to [`L2_QUIET_SECONDS`] and the partner must
//!   have accumulated [`L2_MIN_PENDING`] pending messages before another round opens.
//! * `L3` — fourteen in a row. Only addressed batches pass; everything else stays
//!   pending until the episode resets.
//!
//! Addressing always wins: an `@`, a reply to the bot, or the owner speaking is never
//! damped and never advances the count. A third party, the owner, or ten minutes of
//! silence ends the episode and clears the streak. The per-sender soft cap is
//! deliberately *not* reset by an episode ending — it expires only by the window
//! sliding past those replies.

use std::collections::{HashMap, HashSet};

/// Sliding window everything here is measured over, in seconds.
pub const WINDOW_SECONDS: i64 = 600;

/// Consecutive same-partner rounds that open `L1`.
pub const L1_STREAK: u32 = 6;

/// ...that open `L2`.
pub const L2_STREAK: u32 = 10;

/// ...that open `L3`.
pub const L3_STREAK: u32 = 14;

/// Window concentration (percent) that opens `L1` without a streak.
pub const L1_CONCENTRATION_PERCENT: u32 = 80;

/// External messages the window must hold for the concentration rule to apply.
pub const L1_WINDOW_MESSAGES: usize = 12;

/// Bot rounds the window must hold for the concentration rule to apply.
pub const L1_BOT_ROUNDS: usize = 4;

/// Quiet window before a claim at `L1`, measured from the batch's last message.
pub const L1_QUIET_SECONDS: i64 = 30;

/// Quiet window before a claim at `L2`.
pub const L2_QUIET_SECONDS: i64 = 120;

/// Pending messages the partner must have queued for a round to open at `L2`.
pub const L2_MIN_PENDING: usize = 6;

/// Non-addressed rounds to one sender inside the window before that sender is capped.
pub const SOFT_CAP_ROUNDS: usize = 8;

/// Messages one sender must have put into the window before the soft cap applies.
///
/// Rounds alone cannot tell a machine trading lines with the bot from two people in a
/// one-to-one conversation: eight replies in ten minutes is an ordinary quiet-hour chat,
/// and capping it left the bot mute until those replies slid out of the window — which
/// reads as a dead bot, not as a brake. A sender flooding the room (the incident that
/// motivated this layer: 111 messages in thirteen minutes) is far past this floor, while
/// somebody typing measured paragraphs is not.
pub const CAP_MIN_SENDER_MESSAGES: usize = 16;

/// Concentration (percent) at which the gate prompt carries the structure hint.
pub const PROMPT_CONCENTRATION_PERCENT: u32 = 60;

/// Damping level. Ordered: `L3 > L2 > L1 > L0`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Default)]
pub enum Tier {
    #[default]
    L0,
    L1,
    L2,
    L3,
}

impl Tier {
    /// Level as a number, for the audit key and the prompt hint.
    pub fn level(self) -> u32 {
        match self {
            Tier::L0 => 0,
            Tier::L1 => 1,
            Tier::L2 => 2,
            Tier::L3 => 3,
        }
    }

    /// The level a streak justifies.
    pub fn from_streak(streak: u32) -> Tier {
        if streak >= L3_STREAK {
            Tier::L3
        } else if streak >= L2_STREAK {
            Tier::L2
        } else if streak >= L1_STREAK {
            Tier::L1
        } else {
            Tier::L0
        }
    }

    /// Quiet window this level imposes before a claim.
    pub fn quiet_seconds(self) -> i64 {
        match self {
            Tier::L0 => 0,
            Tier::L1 => L1_QUIET_SECONDS,
            Tier::L2 | Tier::L3 => L2_QUIET_SECONDS,
        }
    }

    /// Pending messages required to open a round at this level.
    pub fn min_pending(self) -> usize {
        match self {
            Tier::L0 | Tier::L1 => 1,
            Tier::L2 | Tier::L3 => L2_MIN_PENDING,
        }
    }
}

/// Why an episode ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResetReason {
    /// Someone other than the partner spoke.
    ThirdParty,
    /// The owner spoke.
    Owner,
    /// The partner went quiet for a whole window.
    Silence,
}

impl ResetReason {
    pub fn as_str(self) -> &'static str {
        match self {
            ResetReason::ThirdParty => "third_party",
            ResetReason::Owner => "owner",
            ResetReason::Silence => "silence",
        }
    }
}

/// Something the caller should audit. The state machine stays pure;
/// writing these is the caller's job.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    Enter { tier: Tier, sender: String },
    Reset { reason: ResetReason },
    Cap { sender: String },
}

/// One pending message, as the damper sees it.
///
/// `id` is the pending row id and is the dedupe key: the flush tick offers the same
/// rows over and over until they are claimed, and each row must enter the window once.
#[derive(Debug, Clone)]
pub struct Observation<'a> {
    pub id: i64,
    pub sender: &'a str,
    /// When the message arrived, not when it was observed.
    pub at: i64,
    /// Whether the batch answers this sender directly: an `@`, a reply to a message the
    /// bot sent, or the owner speaking.
    pub addressed: bool,
    /// Owner messages end an episode whatever else they are.
    pub from_owner: bool,
}

/// Why a claim was held back.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Hold {
    /// Short slug for the audit key.
    pub reason: &'static str,
    /// Hard holds survive the batch ceilings; soft ones (quiet, thin) do not, because
    /// the ceilings exist so nothing can starve the lane forever.
    pub hard: bool,
}

/// What the window says about the batch that is about to be claimed.
#[derive(Debug, Clone)]
pub struct Verdict {
    pub tier: Tier,
    /// The sender this window is mostly made of, if it has one.
    pub dominant: Option<String>,
    /// Dominant sender's share of the window, in `0.0..=1.0`.
    pub concentration: f64,
    /// Consecutive same-partner rounds so far.
    pub streak: u32,
    /// Senders currently over the soft cap.
    pub capped: Vec<String>,
    pub hold_seconds: i64,
    pub min_pending: usize,
    pub addressed_only: bool,
    pub window_messages: usize,
    pub bot_rounds: usize,
}

impl Verdict {
    /// The no-op verdict: no damping at all.
    pub fn neutral() -> Self {
        Self {
            tier: Tier::L0,
            dominant: None,
            concentration: 0.0,
            streak: 0,
            capped: Vec::new(),
            hold_seconds: 0,
            min_pending: 1,
            addressed_only: false,
            window_messages: 0,
            bot_rounds: 0,
        }
    }

    /// Whether a claim by `sender` at `waited` seconds after its last message, with
    /// `pending` rows queued, must be held back.
    pub fn hold(&self, sender: &str, addressed: bool, waited: i64, pending: usize) -> Option<Hold> {
        if addressed {
            return None;
        }
        // `L3` is checked before the per-sender cap even though both are hard holds: a
        // partner who reached `L3` by streak alone has also crossed the cap (fourteen
        // non-addressed rounds is more than eight), so the tier is the honest reason to
        // report. "cap" then speaks only for senders capped at a lower tier.
        if self.addressed_only && self.dominant.as_deref() == Some(sender) {
            return Some(Hold {
                reason: "l3",
                hard: true,
            });
        }
        if self.capped.iter().any(|capped| capped == sender) {
            return Some(Hold {
                reason: "cap",
                hard: true,
            });
        }
        if waited < self.hold_seconds {
            return Some(Hold {
                reason: match self.tier {
                    Tier::L2 | Tier::L3 => "l2_quiet",
                    _ => "l1_quiet",
                },
                hard: false,
            });
        }
        if pending < self.min_pending {
            return Some(Hold {
                reason: "thin",
                hard: false,
            });
        }
        None
    }
}

/// One external message that made it into the window.
#[derive(Debug, Clone)]
struct Windowed {
    id: i64,
    sender: String,
    at: i64,
}

/// One round the bot has answered.
#[derive(Debug, Clone)]
struct Round {
    sender: String,
    at: i64,
    addressed: bool,
}

#[derive(Debug, Default)]
struct GroupState {
    messages: Vec<Windowed>,
    ids: HashSet<i64>,
    rounds: Vec<Round>,
    streak: u32,
    streak_sender: Option<String>,
    tier: Tier,
    last_round_at: Option<i64>,
    /// Senders already audited as capped in the current window.
    cap_announced: HashSet<String>,
    /// Last rendered prompt hint, if any.
    hint: Option<String>,
}

/// The per-group damping state. Cheap to construct; holds no I/O.
#[derive(Debug, Default)]
pub struct Damper {
    groups: std::sync::Mutex<HashMap<String, GroupState>>,
}

impl Damper {
    pub fn new() -> Self {
        Self::default()
    }

    /// Fold the pending rows into the window and report what it says.
    ///
    /// Called once per group per tick. Idempotent for rows already folded in.
    pub fn observe(
        &self,
        group_id: &str,
        now: i64,
        batch: &[Observation<'_>],
    ) -> (Verdict, Vec<Event>) {
        let Ok(mut groups) = self.groups.lock() else {
            // A poisoned lock means another thread panicked mid-update. Damping is a
            // courtesy, not a safety property: fail open and let the batch through.
            return (Verdict::neutral(), Vec::new());
        };
        let state = groups.entry(group_id.to_string()).or_default();
        let mut events = Vec::new();
        let mut reset: Option<ResetReason> = None;

        state.messages.retain(|m| m.at > now - WINDOW_SECONDS);
        let live: HashSet<i64> = state.messages.iter().map(|m| m.id).collect();
        state.ids = live;
        state.rounds.retain(|r| r.at > now - WINDOW_SECONDS);

        for observation in batch {
            if !state.ids.insert(observation.id) {
                continue;
            }
            state.messages.push(Windowed {
                id: observation.id,
                sender: observation.sender.to_string(),
                at: observation.at,
            });
            // The first message that breaks the episode decides the audit reason;
            // later ones in the same tick are the same news.
            if reset.is_none() {
                if observation.from_owner {
                    reset = Some(ResetReason::Owner);
                } else if let Some(partner) = &state.streak_sender
                    && partner != observation.sender
                {
                    reset = Some(ResetReason::ThirdParty);
                }
            }
        }

        if state.tier > Tier::L0
            && let Some(last) = state.last_round_at
            && now - last >= WINDOW_SECONDS
            && reset.is_none()
        {
            reset = Some(ResetReason::Silence);
        }

        if let Some(reason) = reset {
            state.streak = 0;
            state.streak_sender = None;
            state.tier = Tier::L0;
            state.last_round_at = None;
            state.hint = None;
            events.push(Event::Reset { reason });
        }

        // Window statistics.
        let mut counts: HashMap<&str, usize> = HashMap::new();
        for message in &state.messages {
            *counts.entry(message.sender.as_str()).or_insert(0) += 1;
        }
        let dominant_total = counts.values().copied().max().unwrap_or(0);
        let dominant = counts
            .iter()
            .filter(|(_, count)| **count == dominant_total)
            .map(|(sender, _)| (*sender).to_string())
            .min();
        let window_messages = state.messages.len();
        let concentration = if window_messages == 0 {
            0.0
        } else {
            dominant_total as f64 / window_messages as f64
        };
        let bot_rounds = state.rounds.len();

        // The concentration rule can open L1 on its own, without a streak: six bursts
        // can all be over before anyone counts them, but 12 of 13 messages from one
        // person is already visible in the window.
        let concentration_l1 = window_messages >= L1_WINDOW_MESSAGES
            && bot_rounds >= L1_BOT_ROUNDS
            && concentration * 100.0 >= f64::from(L1_CONCENTRATION_PERCENT);
        let streak_tier = Tier::from_streak(state.streak);
        let raised = if concentration_l1 {
            streak_tier.max(Tier::L1)
        } else {
            streak_tier
        };
        let before = state.tier;
        state.tier = state.tier.max(raised);
        if state.tier > before
            && let Some(sender) = state.dominant_sender()
        {
            events.push(Event::Enter {
                tier: state.tier,
                sender,
            });
        }

        // The soft cap is per sender and independent of the streak: eight non-addressed
        // rounds to one person inside the window is already a lot of attention — but only
        // when that person is also *filling* the window with messages. Both counts are
        // read from state already folded above (`rounds` for (a), `messages` for (b)); no
        // second accounting channel exists.
        let mut capped: Vec<String> = Vec::new();
        let mut per_sender: HashMap<&str, usize> = HashMap::new();
        for round in &state.rounds {
            if !round.addressed {
                *per_sender.entry(round.sender.as_str()).or_insert(0) += 1;
            }
        }
        for (sender, rounds) in per_sender {
            if rounds < SOFT_CAP_ROUNDS {
                continue;
            }
            if counts.get(sender).copied().unwrap_or(0) >= CAP_MIN_SENDER_MESSAGES {
                capped.push(sender.to_string());
            }
        }
        capped.sort();
        for sender in &capped {
            if state.cap_announced.insert(sender.clone()) {
                events.push(Event::Cap {
                    sender: sender.clone(),
                });
            }
        }
        // A sender whose replies slid out of the window may be announced again later;
        // re-crossing the cap is a new episode even though it is the same person.
        state
            .cap_announced
            .retain(|sender| capped.iter().any(|c| c == sender));

        let percent = (concentration * 100.0).round() as u32;
        state.hint = (state.tier >= Tier::L1 || percent >= PROMPT_CONCENTRATION_PERCENT)
            .then(|| {
                format!(
                    "结构提示：近10分钟发言集中度 {percent}%，主要来自同一人，本机器人已连续回复 {} 轮，当前降温等级 L{}。",
                    state.streak,
                    state.tier.level()
                )
            });

        let verdict = Verdict {
            tier: state.tier,
            dominant,
            concentration,
            streak: state.streak,
            capped,
            hold_seconds: state.tier.quiet_seconds(),
            min_pending: state.tier.min_pending(),
            addressed_only: state.tier >= Tier::L3,
            window_messages,
            bot_rounds,
        };
        (verdict, events)
    }

    /// Record that the bot answered `sender` just now.
    ///
    /// Addressed rounds are recorded but change nothing else: an `@` neither extends the
    /// streak nor breaks it.
    pub fn note_reply(
        &self,
        group_id: &str,
        now: i64,
        sender: &str,
        addressed: bool,
    ) -> Vec<Event> {
        let Ok(mut groups) = self.groups.lock() else {
            return Vec::new();
        };
        let state = groups.entry(group_id.to_string()).or_default();
        state.rounds.push(Round {
            sender: sender.to_string(),
            at: now,
            addressed,
        });
        state.last_round_at = Some(now);
        if addressed {
            return Vec::new();
        }
        if state.streak_sender.as_deref() == Some(sender) {
            state.streak += 1;
        } else {
            state.streak = 1;
            state.streak_sender = Some(sender.to_string());
        }
        let before = state.tier;
        state.tier = state.tier.max(Tier::from_streak(state.streak));
        if state.tier > before {
            vec![Event::Enter {
                tier: state.tier,
                sender: sender.to_string(),
            }]
        } else {
            Vec::new()
        }
    }

    /// The structure line for the gate prompt, when this group warrants one.
    ///
    /// The gate is told the shape of the conversation and nothing else: not what to
    /// answer, and not that a hold exists. Its authority is unchanged.
    pub fn hint(&self, group_id: &str) -> Option<String> {
        self.groups
            .lock()
            .ok()
            .and_then(|groups| groups.get(group_id).and_then(|state| state.hint.clone()))
    }

    /// Current level, for tests and the audit trail.
    pub fn tier(&self, group_id: &str) -> Tier {
        self.groups
            .lock()
            .ok()
            .and_then(|groups| groups.get(group_id).map(|state| state.tier))
            .unwrap_or(Tier::L0)
    }
}

impl GroupState {
    fn dominant_sender(&self) -> Option<String> {
        let mut counts: HashMap<&str, usize> = HashMap::new();
        for message in &self.messages {
            *counts.entry(message.sender.as_str()).or_insert(0) += 1;
        }
        let best = counts.values().copied().max()?;
        counts
            .iter()
            .filter(|(_, count)| **count == best)
            .map(|(sender, _)| (*sender).to_string())
            .min()
    }
}

#[cfg(test)]
mod tests;
