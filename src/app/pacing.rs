//! Global quota pacing: the third layer, above the hourly fuse and the cadence gate.
//!
//! Three layers hold the reply rate down, and they answer different questions:
//!
//! * the **hourly fuse** (`hourly_call_budget` in `app/intake.rs`) answers "has this group
//!   started too many calls in the last rolling hour?" — a fixed number, blind to how much
//!   quota is actually left;
//! * the **cadence gate** (`app/preflight.rs`) answers "is this sentence finished?";
//! * **pacing** answers "can the account afford this at all?" — it reads the Gemini quota
//!   snapshot and decides how tight the hour has to be.
//!
//! The fuse is never replaced or loosened: pacing sits beside it and the *smaller* of the
//! two caps wins. A blown fuse still defers; a tight tier still tightens even when the fuse
//! would have allowed more.
//!
//! Tiers are decided by *pace*, not by the raw remaining percentage: for
//! each bucket the fair consumption line is the share of its window that
//! has already elapsed (the window ends at the bucket's reset time), and
//! the tier follows how far ahead of that line the account is burning.
//! The same 60% used is an emergency early in the window and perfectly
//! fine near its end.
//!
//! | tier | overage (points ahead of the fair line) | behaviour |
//! |------|--------|-----------|
//! | T0 | ≤ 8 | current behaviour: the fuse only |
//! | T1 | ≤ 20 | per-group cap = min(fuse, 30), plus a projection of the remaining quota over the hours left to the weekly reset |
//! | T2 | ≤ 35 | T1's cap, and only batches with an @ or an owner message are claimed; everything else is consumed now (silence beats a late unrequested reply) |
//! | T3 | > 35, or a bucket at ≤ 10% remaining | owner messages only, cap = min(fuse, 3) |
//!
//! Both buckets (weekly window 7 days, short window 5 hours) are scored
//! and the stricter tier wins. Loosening is hysteretic: the overage must
//! clear the boundary by [`HYSTERESIS_POINTS`] before the tier relaxes,
//! so a value parked on a boundary cannot flap the bot's behaviour.
//! When a bucket's reset time is unknown the pace cannot be computed for
//! it and the pre-dynamic fixed thresholds apply instead (weekly
//! remaining > 50/25/10% → T0/T1/T2/T3; the 5-hour bucket below 25%
//! tightens one step, below 10% forces T3) — a fallback, not a mode.
//!
//! One overlay sits on top of either path:
//!
//! * snapshot staleness: missing, errored or older than [`SNAPSHOT_MAX_AGE_SECONDS`] means
//!   we no longer know the account's state, so the tier escalates one step per further
//!   [`STALE_ESCALATION_SECONDS`] of blindness. It can never fall back to T0 while blind —
//!   "we could not read the quota" is not "the quota is fine".
//!
//! The operator's degradation switch (`quota_degradation_enabled`) can
//! reduce all of this to display-only: the decision is still computed
//! (and reported as `raw_tier`) but behaviour follows T0.
//!
//! Every rule here is a pure function; the [`PacingEngine`] wrapper only feeds it the
//! snapshot and remembers the last fresh tier so staleness has an anchor.

use std::sync::{Arc, Mutex};

use serde::Serialize;

use crate::integrations::quota::{QuotaService, parse_rfc3339};
use crate::pipeline::batcher::BatchEntry;
use crate::store::SqliteStore;

// ---------------------------------------------------------------------------
// Thresholds

/// Above this weekly percentage the fuse alone is enough (T0).
pub const WEEKLY_T0_FLOOR: f64 = 50.0;
/// Above this weekly percentage, T1: tighten and project.
pub const WEEKLY_T1_FLOOR: f64 = 25.0;
/// Above this weekly percentage, T2: privileged batches only.
pub const WEEKLY_T2_FLOOR: f64 = 10.0;
/// Below this 5-hour percentage, one tier stricter than the weekly tier.
pub const FIVE_HOUR_TIGHTEN_BELOW: f64 = 25.0;
/// Below this 5-hour percentage, T3 regardless of the weekly tier.
pub const FIVE_HOUR_T3_BELOW: f64 = 10.0;

/// Window lengths the pace computation divides by: the weekly bucket
/// resets every 7 days, the short bucket every 5 hours.
pub const WEEKLY_WINDOW_HOURS: f64 = 168.0;
pub const FIVE_HOUR_WINDOW_HOURS: f64 = 5.0;
/// Overage points (used% minus the fair line) at which the pace tiers
/// tighten. See the module doc's table.
pub const OVERAGE_T1_ABOVE: f64 = 8.0;
pub const OVERAGE_T2_ABOVE: f64 = 20.0;
pub const OVERAGE_T3_ABOVE: f64 = 35.0;
/// A bucket this close to empty is T3 whatever the pace says: pace is
/// about the future, an empty bucket is the present.
pub const NEAR_EMPTY_PCT: f64 = 10.0;
/// Extra overage clearance required before a tier may loosen.
pub const HYSTERESIS_POINTS: f64 = 5.0;

/// T1's hard per-group hourly ceiling.
pub const T1_CAP: u32 = 30;
/// T3's hard per-group hourly ceiling.
pub const T3_CAP: u32 = 3;
/// T1's per-group floor: even a tiny projection keeps one reply in the hour.
///
/// The projection is deliberately pessimistic (it assumes every remaining call happens
/// at the weekly bucket's marginal cost), and a floor keeps a group from being starved
/// entirely by rounding. It never lifts a group above [`T1_CAP`] or above the fuse.
pub const T1_FLOOR_PER_GROUP: u32 = 8;

/// Weekly-bucket percentage points one started call costs, measured from the account's
/// own history (a turn is a single agent call; retries are the fuse's business).
pub const COST_PER_CALL_PCT: f64 = 0.03;
/// The share of the weekly bucket this bot is allowed to plan with.
pub const BOT_WEEKLY_SHARE: f64 = 0.5;

/// A snapshot older than this is no longer trusted.
pub const SNAPSHOT_MAX_AGE_SECONDS: i64 = 1_200;
/// Each further slice of blindness tightens the tier by one step.
pub const STALE_ESCALATION_SECONDS: i64 = 1_800;

pub const HEAVY_GROUP_WEIGHT: u32 = 2;
pub const LIGHT_GROUP_WEIGHT: u32 = 1;

/// The projection weight of one group: groups the operator listed as
/// heavy (`heavy_group_ids` in the config file) weigh double when the
/// projection is shared out.
pub fn weight_of(group_id: &str, heavy_groups: &std::collections::HashSet<String>) -> u32 {
    if heavy_groups.contains(group_id) {
        HEAVY_GROUP_WEIGHT
    } else {
        LIGHT_GROUP_WEIGHT
    }
}

// ---------------------------------------------------------------------------
// Tiers

/// How tight the hour is. Ordered from loosest to strictest so `max` is "stricter".
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
pub enum Tier {
    T0,
    T1,
    T2,
    T3,
}

impl Tier {
    pub fn as_str(self) -> &'static str {
        match self {
            Tier::T0 => "T0",
            Tier::T1 => "T1",
            Tier::T2 => "T2",
            Tier::T3 => "T3",
        }
    }

    /// One step stricter, saturating at [`Tier::T3`].
    pub fn stricter(self) -> Tier {
        match self {
            Tier::T0 => Tier::T1,
            Tier::T1 => Tier::T2,
            Tier::T2 | Tier::T3 => Tier::T3,
        }
    }

    /// `steps` steps stricter, saturating at [`Tier::T3`].
    pub fn escalate(self, steps: i64) -> Tier {
        let mut tier = self;
        for _ in 0..steps.max(0) {
            tier = tier.stricter();
        }
        tier
    }
}

/// The tier the weekly bucket alone asks for.
pub fn tier_for_weekly(weekly_pct: f64) -> Tier {
    if weekly_pct > WEEKLY_T0_FLOOR {
        Tier::T0
    } else if weekly_pct > WEEKLY_T1_FLOOR {
        Tier::T1
    } else if weekly_pct > WEEKLY_T2_FLOOR {
        Tier::T2
    } else {
        Tier::T3
    }
}

/// Apply the 5-hour overlay.
///
/// A missing 5-hour bucket changes nothing: the weekly bucket is the planning horizon, and
/// the 5-hour one is only ever allowed to make things stricter.
pub fn apply_five_hour(tier: Tier, five_hour_pct: Option<f64>) -> Tier {
    match five_hour_pct {
        Some(pct) if pct < FIVE_HOUR_T3_BELOW => Tier::T3,
        Some(pct) if pct < FIVE_HOUR_TIGHTEN_BELOW => tier.stricter(),
        _ => tier,
    }
}

/// The tier one bucket's pace asks for.
///
/// `shift` inflates the overage; the hysteresis check passes
/// [`HYSTERESIS_POINTS`] so a loosening has to clear the boundary by
/// that margin, not merely cross it.
pub fn bucket_pace_tier(
    remaining_pct: f64,
    reset_in_hours: f64,
    window_hours: f64,
    shift: f64,
) -> Tier {
    let elapsed = (1.0 - reset_in_hours / window_hours).clamp(0.0, 1.0);
    let overage = (100.0 - remaining_pct) - elapsed * 100.0 + shift;
    let tier = if overage > OVERAGE_T3_ABOVE {
        Tier::T3
    } else if overage > OVERAGE_T2_ABOVE {
        Tier::T2
    } else if overage > OVERAGE_T1_ABOVE {
        Tier::T1
    } else {
        Tier::T0
    };
    if remaining_pct <= NEAR_EMPTY_PCT {
        tier.max(Tier::T3)
    } else {
        tier
    }
}

/// Both buckets' pace tiers combined (stricter wins). The 5-hour bucket
/// falls back to the legacy overlay when its own reset time is unknown.
fn combined_pace_tier(input: &PacingInput, weekly: f64, reset_hours: f64, shift: f64) -> Tier {
    let tier = bucket_pace_tier(weekly, reset_hours, WEEKLY_WINDOW_HOURS, shift);
    match (input.five_hour_pct, input.five_hour_reset_hours) {
        (Some(pct), Some(reset)) => {
            tier.max(bucket_pace_tier(pct, reset, FIVE_HOUR_WINDOW_HOURS, shift))
        }
        _ => apply_five_hour(tier, input.five_hour_pct),
    }
}

/// The pace decision with hysteresis against the last fresh tier:
/// tightening is immediate, loosening must clear the boundary by
/// [`HYSTERESIS_POINTS`] (and then lands on the cautious tier, so a
/// boundary value settles instead of oscillating).
fn pace_tier(weekly: f64, reset_hours: f64, input: &PacingInput) -> Tier {
    let raw = combined_pace_tier(input, weekly, reset_hours, 0.0);
    if raw >= input.last_fresh {
        return raw;
    }
    let cautious = combined_pace_tier(input, weekly, reset_hours, HYSTERESIS_POINTS);
    if cautious < input.last_fresh {
        cautious
    } else {
        input.last_fresh
    }
}

/// How stale the snapshot is, expressed as a tier escalation.
///
/// `last_fresh` is the tier the weekly bucket last asked for while we could read it, so
/// repeated observation of the same stale snapshot cannot compound: the escalation is a
/// function of the snapshot's own age, not of how often we looked at it. One step is always
/// charged, so a blind bot never behaves like T0.
pub fn escalate_stale(last_fresh: Tier, snapshot_age_s: Option<i64>) -> Tier {
    let overdue = snapshot_age_s
        .map(|age| (age - SNAPSHOT_MAX_AGE_SECONDS).max(0))
        .unwrap_or(0);
    last_fresh.escalate(1 + overdue / STALE_ESCALATION_SECONDS)
}

// ---------------------------------------------------------------------------
// Decision

/// What the tier decision reads. Everything is optional because every input can be missing.
#[derive(Debug, Clone, Copy)]
pub struct PacingInput {
    /// Weekly bucket remaining, in percent (0..=100).
    pub weekly_pct: Option<f64>,
    /// 5-hour bucket remaining, in percent (0..=100).
    pub five_hour_pct: Option<f64>,
    /// Age of the snapshot in seconds, `None` when we have never read one.
    pub snapshot_age_s: Option<i64>,
    /// Whether the last scrape reported an error. A failed scrape keeps the previous
    /// numbers on screen, so the numbers are not evidence any more.
    pub erroneous: bool,
    /// Hours left until the weekly bucket resets, `None` when unknown.
    pub weekly_reset_hours: Option<f64>,
    /// Hours left until the 5-hour bucket resets, `None` when unknown.
    pub five_hour_reset_hours: Option<f64>,
    /// The tier the weekly bucket last asked for while the snapshot was trustworthy.
    pub last_fresh: Tier,
}

/// What the tier decision says.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PacingDecision {
    pub tier: Tier,
    /// Whether the snapshot was unusable and the tier comes from staleness.
    pub stale: bool,
    /// Global hourly start budget from the projection, when the tier uses one.
    pub global_hourly: Option<i64>,
}

impl Default for PacingInput {
    fn default() -> Self {
        Self {
            weekly_pct: None,
            five_hour_pct: None,
            snapshot_age_s: None,
            erroneous: false,
            weekly_reset_hours: None,
            five_hour_reset_hours: None,
            last_fresh: Tier::T0,
        }
    }
}

/// Decide the tier. Pure: same input, same output, no clock, no store.
pub fn decide(input: &PacingInput) -> PacingDecision {
    let trusted = match (input.weekly_pct, input.snapshot_age_s) {
        (Some(_), Some(age)) => !input.erroneous && age <= SNAPSHOT_MAX_AGE_SECONDS,
        _ => false,
    };
    if !trusted {
        return PacingDecision {
            tier: escalate_stale(input.last_fresh, input.snapshot_age_s),
            stale: true,
            // No trustworthy percentage means no trustworthy projection; the tier's
            // structural caps and filters still apply.
            global_hourly: None,
        };
    }
    let weekly = input.weekly_pct.unwrap_or(0.0);
    let tier = match input.weekly_reset_hours {
        Some(reset_hours) => pace_tier(weekly, reset_hours, input),
        // No reset time, no fair line: the fixed thresholds are the
        // fallback (see the module doc), unchanged from the pre-dynamic
        // behaviour.
        None => apply_five_hour(tier_for_weekly(weekly), input.five_hour_pct),
    };
    let global_hourly = match (tier, input.weekly_reset_hours) {
        (Tier::T1 | Tier::T2, Some(hours)) => projected_global_hourly(weekly, hours),
        _ => None,
    };
    PacingDecision {
        tier,
        stale: false,
        global_hourly,
    }
}

/// Remaining starts for the whole bot, divided over the hours left in the week.
///
/// `weekly_pct / COST_PER_CALL_PCT` is how many calls the bucket still pays for; the bot
/// may plan with [`BOT_WEEKLY_SHARE`] of them. Spreading that over the remaining hours
/// gives an hourly quota that lands exactly on the reset with nothing to spare.
pub fn projected_global_hourly(weekly_pct: f64, reset_in_hours: f64) -> Option<i64> {
    if !reset_in_hours.is_finite() || reset_in_hours <= 0.0 {
        return None;
    }
    let starts = weekly_pct / COST_PER_CALL_PCT * BOT_WEEKLY_SHARE;
    Some((starts / reset_in_hours).floor().max(0.0) as i64)
}

/// The per-group hourly cap for one tier.
///
/// `fuse` is `hourly_call_budget` (0 means the fuse is disabled); `weight`/`total_weight`
/// are this group's share of the projection. The result is the strictest of the three:
/// the fuse, the tier's ceiling, and the group's projected share — with the floor applied
/// to the share *before* the ceiling so the floor can never lift a group past the ceiling.
pub fn group_cap(
    tier: Tier,
    fuse: u32,
    weight: u32,
    total_weight: u32,
    global_hourly: Option<i64>,
) -> usize {
    let fuse_cap = if fuse == 0 { usize::MAX } else { fuse as usize };
    match tier {
        Tier::T0 => fuse_cap,
        Tier::T1 | Tier::T2 => {
            let ceiling = fuse_cap.min(T1_CAP as usize);
            let Some(global) = global_hourly else {
                return ceiling;
            };
            let total = i64::from(total_weight.max(1));
            let share = global.max(0) * i64::from(weight) / total;
            share
                .max(i64::from(T1_FLOOR_PER_GROUP))
                .min(ceiling as i64)
                .max(0) as usize
        }
        Tier::T3 => fuse_cap.min(T3_CAP as usize),
    }
}

/// What a tier demands of a batch before it may be claimed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PrivilegeFloor {
    /// T2: the batch must carry an @ or an owner message.
    MentionOrOwner,
    /// T3: an owner message only.
    OwnerOnly,
}

/// The privilege floor of a tier, `None` when every batch qualifies.
pub fn privilege_floor(tier: Tier) -> Option<PrivilegeFloor> {
    match tier {
        Tier::T0 | Tier::T1 => None,
        Tier::T2 => Some(PrivilegeFloor::MentionOrOwner),
        Tier::T3 => Some(PrivilegeFloor::OwnerOnly),
    }
}

/// Whether one message clears the floor.
///
/// `mentions_bot` and `from_owner` are the batcher's own flags: the same signals the
/// cadence gate and the prompt already use, so no second definition of "asked for" exists.
pub fn entry_clears(floor: PrivilegeFloor, entry: &BatchEntry) -> bool {
    match floor {
        PrivilegeFloor::MentionOrOwner => entry.mentions_bot || entry.from_owner,
        PrivilegeFloor::OwnerOnly => entry.from_owner,
    }
}

// ---------------------------------------------------------------------------
// Engine

#[derive(Debug)]
struct Memory {
    /// The tier the weekly bucket asked for the last time the snapshot was trustworthy.
    last_fresh: Tier,
    /// The tier we last reported, so a change is audited exactly once.
    reported: Tier,
}

/// One group's live cap, as the status endpoint shows it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct GroupCap {
    pub group_id: String,
    pub cap: usize,
}

/// Everything one observation produced, ready for JSON.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct PacingReport {
    pub tier: Tier,
    /// The tier the quota decision asked for, before the degradation
    /// switch. Equal to `tier` while the switch is on.
    pub raw_tier: Tier,
    pub weekly_pct: Option<f64>,
    pub five_hour_pct: Option<f64>,
    pub snapshot_age_s: Option<i64>,
    /// Live per-group hourly caps. The fuse alone when the tier is T0.
    pub effective_caps: Vec<GroupCap>,
    /// The global hourly projection the caps were shared out from, when the tier uses one.
    pub global_hourly: Option<i64>,
    /// Whether the tier came from staleness rather than a readable snapshot.
    pub stale: bool,
    /// Whether quota-driven degradation is allowed to shape behaviour.
    pub degradation_enabled: bool,
}

impl PacingReport {
    /// The cap for one group, or `None` when the group is unknown to this observation.
    pub fn cap_for(&self, group_id: &str) -> Option<usize> {
        self.effective_caps
            .iter()
            .find(|c| c.group_id == group_id)
            .map(|c| c.cap)
    }
}

/// Reads the quota snapshot and turns it into a tier plus per-group caps.
///
/// Cheap to call every tick: [`QuotaService::snapshot`] is a mutex read that kicks a
/// background refresh at most once per its own TTL. The audit row is written here, on a
/// change only, so the flush loop and the status endpoint cannot disagree about the tier.
pub struct PacingEngine {
    store: Arc<SqliteStore>,
    source: Source,
    bot_id: String,
    fuse: u32,
    heavy_groups: std::collections::HashSet<String>,
    degradation_enabled: std::sync::atomic::AtomicBool,
    webhook: Option<Arc<crate::integrations::EventWebhook>>,
    memory: Mutex<Memory>,
}

/// Where the observation comes from. The live path reads the quota service; the fixed path
/// lets tests drive the engine without a PTY scrape behind it.
enum Source {
    Live(Arc<QuotaService>),
    #[cfg(test)]
    Fixed(PacingInput),
}

impl PacingEngine {
    pub fn new(
        store: Arc<SqliteStore>,
        quota: Arc<QuotaService>,
        bot_id: String,
        fuse: u32,
    ) -> Self {
        Self {
            store,
            source: Source::Live(quota),
            bot_id,
            fuse,
            heavy_groups: std::collections::HashSet::new(),
            degradation_enabled: std::sync::atomic::AtomicBool::new(true),
            webhook: None,
            memory: Mutex::new(Memory {
                last_fresh: Tier::T0,
                reported: Tier::T0,
            }),
        }
    }

    #[cfg(test)]
    pub fn fixed(store: Arc<SqliteStore>, input: PacingInput, bot_id: String, fuse: u32) -> Self {
        Self {
            store,
            source: Source::Fixed(input),
            bot_id,
            fuse,
            heavy_groups: std::collections::HashSet::new(),
            degradation_enabled: std::sync::atomic::AtomicBool::new(true),
            webhook: None,
            memory: Mutex::new(Memory {
                last_fresh: Tier::T0,
                reported: Tier::T0,
            }),
        }
    }

    /// Operator-listed heavy groups (see `weight_of`).
    pub fn with_heavy_groups(mut self, heavy_groups: std::collections::HashSet<String>) -> Self {
        self.heavy_groups = heavy_groups;
        self
    }

    /// The degradation switch (`quota_degradation_enabled`): off means
    /// the decision is computed and reported (`raw_tier`) but behaviour
    /// follows T0 — the quota becomes display-only.
    pub fn with_degradation_enabled(self, enabled: bool) -> Self {
        self.set_degradation_enabled(enabled);
        self
    }

    /// Attach the optional event webhook (tier changes are reported).
    pub fn with_webhook(mut self, webhook: Option<Arc<crate::integrations::EventWebhook>>) -> Self {
        self.webhook = webhook;
        self
    }

    /// Flip the degradation switch at runtime (settings API).
    pub fn set_degradation_enabled(&self, enabled: bool) {
        self.degradation_enabled
            .store(enabled, std::sync::atomic::Ordering::Relaxed);
    }

    /// Whether quota degradation currently shapes behaviour.
    pub fn degradation_enabled(&self) -> bool {
        self.degradation_enabled
            .load(std::sync::atomic::Ordering::Relaxed)
    }

    /// The configured fuse, so callers do not need a second copy of it.
    pub fn fuse(&self) -> u32 {
        self.fuse
    }

    /// Observe every group the bot may answer — the same set the flush tick walks, so a
    /// paused group never takes weight away from a live one.
    pub async fn observe_known_groups(&self, now: i64) -> PacingReport {
        let group_ids: Vec<String> = self
            .store
            .list_groups(&self.bot_id)
            .await
            .map(|groups| {
                groups
                    .into_iter()
                    .filter(|g| g.enabled && !g.paused)
                    .map(|g| g.group_id)
                    .collect()
            })
            .unwrap_or_default();
        self.observe(now, &group_ids).await
    }

    /// Observe the quota now and produce the live report for these groups.
    pub async fn observe(&self, now: i64, group_ids: &[String]) -> PacingReport {
        let input = self.read_input(now);
        self.observe_input(now, group_ids, input).await
    }

    /// Turn one quota snapshot into a [`PacingInput`]. Reading only; the tier decision and
    /// the memory update live in [`Self::observe_input`] so a fixed input takes the same road.
    fn read_input(&self, now: i64) -> PacingInput {
        let last_fresh = {
            let memory = self.memory.lock().unwrap_or_else(|e| e.into_inner());
            memory.last_fresh
        };
        match &self.source {
            Source::Live(quota) => {
                let snapshot = quota.snapshot();
                let weekly_pct = snapshot
                    .gemini
                    .weekly
                    .as_ref()
                    .map(|bucket| (bucket.remaining_fraction * 100.0).clamp(0.0, 100.0));
                let five_hour_pct = snapshot
                    .gemini
                    .five_hour
                    .as_ref()
                    .map(|bucket| (bucket.remaining_fraction * 100.0).clamp(0.0, 100.0));
                let snapshot_age_s = snapshot
                    .fetched_at
                    .as_deref()
                    .and_then(parse_rfc3339)
                    .map(|at| now - at);
                let weekly_reset_hours = snapshot
                    .gemini
                    .weekly
                    .as_ref()
                    .and_then(|bucket| bucket.reset_time.as_deref())
                    .and_then(parse_rfc3339)
                    .map(|reset| (reset - now) as f64 / 3_600.0);
                let five_hour_reset_hours = snapshot
                    .gemini
                    .five_hour
                    .as_ref()
                    .and_then(|bucket| bucket.reset_time.as_deref())
                    .and_then(parse_rfc3339)
                    .map(|reset| (reset - now) as f64 / 3_600.0);
                PacingInput {
                    weekly_pct,
                    five_hour_pct,
                    snapshot_age_s,
                    erroneous: snapshot.error.is_some(),
                    weekly_reset_hours,
                    five_hour_reset_hours,
                    last_fresh,
                }
            }
            #[cfg(test)]
            Source::Fixed(input) => PacingInput {
                last_fresh,
                ..*input
            },
        }
    }

    async fn observe_input(
        &self,
        now: i64,
        group_ids: &[String],
        input: PacingInput,
    ) -> PacingReport {
        let weekly_pct = input.weekly_pct;
        let five_hour_pct = input.five_hour_pct;
        let snapshot_age_s = input.snapshot_age_s;
        let decision = decide(&input);

        // Remember what the weekly bucket asked for while it was readable; staleness is
        // then a pure function of the snapshot's age and cannot compound per observation.
        if !decision.stale {
            let mut memory = self.memory.lock().unwrap_or_else(|e| e.into_inner());
            memory.last_fresh = decision.tier;
        }

        let reported = {
            let memory = self.memory.lock().unwrap_or_else(|e| e.into_inner());
            memory.reported
        };

        // The degradation switch gates behaviour, not knowledge: the
        // decision above is the quota's real ask (kept as raw_tier and
        // as the hysteresis/staleness anchor), while caps, floors and
        // the reported tier follow the effective tier.
        let effective_tier = if self.degradation_enabled() {
            decision.tier
        } else {
            Tier::T0
        };
        let effective_hourly = if self.degradation_enabled() {
            decision.global_hourly
        } else {
            None
        };

        let total_weight: u32 = group_ids
            .iter()
            .map(|id| weight_of(id, &self.heavy_groups))
            .sum();
        let effective_caps: Vec<GroupCap> = group_ids
            .iter()
            .map(|group_id| GroupCap {
                group_id: group_id.clone(),
                cap: group_cap(
                    effective_tier,
                    self.fuse,
                    weight_of(group_id, &self.heavy_groups),
                    total_weight,
                    effective_hourly,
                ),
            })
            .collect();

        if reported != effective_tier {
            let weekly = weekly_pct
                .map(|p| format!("{p:.0}"))
                .unwrap_or_else(|| "na".into());
            let five = five_hour_pct
                .map(|p| format!("{p:.0}"))
                .unwrap_or_else(|| "na".into());
            let age = snapshot_age_s
                .map(|a| a.to_string())
                .unwrap_or_else(|| "na".into());
            let stale = if decision.stale { ".stale" } else { "" };
            let _ = self
                .store
                .audit(
                    "system",
                    &format!(
                        "pacing.tier.from{}.to{}.w{weekly}.h{five}.age{age}{stale}",
                        reported.as_str(),
                        effective_tier.as_str()
                    ),
                    Some(&self.bot_id),
                    None,
                    now,
                )
                .await;
            let mut memory = self.memory.lock().unwrap_or_else(|e| e.into_inner());
            memory.reported = effective_tier;
            drop(memory);
            if let Some(hook) = &self.webhook {
                hook.fire(
                    crate::integrations::webhook::events::PACING_TIER_CHANGED,
                    None,
                    serde_json::json!({
                        "from": reported.as_str(),
                        "to": effective_tier.as_str(),
                        "weekly_pct": weekly_pct,
                        "five_hour_pct": five_hour_pct,
                        "stale": decision.stale,
                    }),
                );
            }
        }

        PacingReport {
            tier: effective_tier,
            raw_tier: decision.tier,
            weekly_pct,
            five_hour_pct,
            snapshot_age_s,
            effective_caps,
            global_hourly: effective_hourly,
            stale: decision.stale,
            degradation_enabled: self.degradation_enabled(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(mentions_bot: bool, from_owner: bool) -> BatchEntry {
        BatchEntry {
            id: 1,
            chars: 2,
            images: 0,
            mentions_bot,
            from_owner,
            received_at: 1_000,
        }
    }

    fn fresh(weekly: f64) -> PacingInput {
        PacingInput {
            weekly_pct: Some(weekly),
            snapshot_age_s: Some(60),
            weekly_reset_hours: Some(48.0),
            ..PacingInput::default()
        }
    }

    #[test]
    fn the_weekly_boundaries_are_exclusive_on_the_loose_side() {
        assert_eq!(tier_for_weekly(100.0), Tier::T0);
        assert_eq!(tier_for_weekly(50.01), Tier::T0);
        assert_eq!(tier_for_weekly(50.0), Tier::T1);
        assert_eq!(tier_for_weekly(25.01), Tier::T1);
        assert_eq!(tier_for_weekly(25.0), Tier::T2);
        assert_eq!(tier_for_weekly(10.01), Tier::T2);
        assert_eq!(tier_for_weekly(10.0), Tier::T3);
        assert_eq!(tier_for_weekly(0.0), Tier::T3);
    }

    #[test]
    fn tiers_rank_from_loose_to_strict() {
        assert!(Tier::T0 < Tier::T1 && Tier::T1 < Tier::T2 && Tier::T2 < Tier::T3);
        assert_eq!(Tier::T3.escalate(5), Tier::T3);
        assert_eq!(Tier::T0.escalate(2), Tier::T2);
    }

    #[test]
    fn the_five_hour_bucket_can_only_tighten() {
        assert_eq!(apply_five_hour(Tier::T0, Some(90.0)), Tier::T0);
        assert_eq!(apply_five_hour(Tier::T0, Some(25.0)), Tier::T0);
        assert_eq!(apply_five_hour(Tier::T0, Some(24.9)), Tier::T1);
        assert_eq!(apply_five_hour(Tier::T1, Some(24.9)), Tier::T2);
        assert_eq!(apply_five_hour(Tier::T1, Some(9.9)), Tier::T3);
        assert_eq!(apply_five_hour(Tier::T2, Some(24.9)), Tier::T3);
        // Missing 5-hour data is not evidence of trouble.
        assert_eq!(apply_five_hour(Tier::T1, None), Tier::T1);
    }

    fn legacy(weekly: f64) -> PacingInput {
        PacingInput {
            weekly_reset_hours: None,
            ..fresh(weekly)
        }
    }

    fn paced(weekly: f64, reset_hours: f64) -> PacingInput {
        PacingInput {
            weekly_pct: Some(weekly),
            snapshot_age_s: Some(60),
            weekly_reset_hours: Some(reset_hours),
            ..PacingInput::default()
        }
    }

    #[test]
    fn the_five_hour_overlay_applies_after_the_weekly_tier() {
        // Legacy fallback (no reset times): the fixed thresholds and the
        // absolute overlay, exactly as before the pace decision existed.
        let mut input = legacy(30.0);
        assert_eq!(decide(&input).tier, Tier::T1);
        input.five_hour_pct = Some(20.0);
        assert_eq!(decide(&input).tier, Tier::T2);
        input.five_hour_pct = Some(5.0);
        assert_eq!(decide(&input).tier, Tier::T3);
        // T3 from the weekly bucket plus a healthy 5-hour bucket stays T3.
        let weekly_floor = PacingInput {
            five_hour_pct: Some(80.0),
            ..legacy(8.0)
        };
        assert_eq!(decide(&weekly_floor).tier, Tier::T3);
    }

    #[test]
    fn staleness_escalates_one_step_and_never_falls_back_to_t0() {
        assert_eq!(escalate_stale(Tier::T0, Some(0)), Tier::T1);
        assert_eq!(escalate_stale(Tier::T0, Some(1_200)), Tier::T1);
        assert_eq!(escalate_stale(Tier::T0, Some(1_201)), Tier::T1);
        assert_eq!(escalate_stale(Tier::T0, Some(3_000)), Tier::T2);
        assert_eq!(escalate_stale(Tier::T1, Some(3_000)), Tier::T3);
        assert_eq!(escalate_stale(Tier::T3, Some(99_999)), Tier::T3);
        // No snapshot at all is one step, not a free pass.
        assert_eq!(escalate_stale(Tier::T0, None), Tier::T1);
    }

    #[test]
    fn a_missing_or_errored_snapshot_never_reads_as_t0() {
        let missing = PacingInput::default();
        let decision = decide(&missing);
        assert!(decision.stale);
        assert_eq!(decision.tier, Tier::T1);

        let errored = PacingInput {
            erroneous: true,
            ..fresh(90.0)
        };
        let decision = decide(&errored);
        assert!(decision.stale);
        assert_eq!(decision.tier, Tier::T1, "读不到不等于额度充足");

        let old = PacingInput {
            snapshot_age_s: Some(SNAPSHOT_MAX_AGE_SECONDS + 1),
            ..fresh(90.0)
        };
        assert_eq!(decide(&old).tier, Tier::T1);
        // Two slices of blindness past the line is two steps.
        let very_old = PacingInput {
            snapshot_age_s: Some(SNAPSHOT_MAX_AGE_SECONDS + STALE_ESCALATION_SECONDS),
            ..fresh(90.0)
        };
        assert_eq!(decide(&very_old).tier, Tier::T2);
    }

    #[test]
    fn a_stale_snapshot_keeps_the_last_fresh_tier_as_its_anchor() {
        let stale = PacingInput {
            snapshot_age_s: Some(10_000),
            last_fresh: Tier::T2,
            ..PacingInput::default()
        };
        let decision = decide(&stale);
        assert!(decision.stale);
        assert_eq!(decision.tier, Tier::T3, "最后一档是 T2，失明只会更严");
        assert_eq!(decision.global_hourly, None);
    }

    #[test]
    fn the_projection_spends_the_week_exactly() {
        // 30% left at 0.03/call = 1000 calls, half of them ours = 500,
        // over 50 remaining hours = 10/hour.
        assert_eq!(projected_global_hourly(30.0, 50.0), Some(10));
        // Nothing left, nothing to spend.
        assert_eq!(projected_global_hourly(0.0, 5.0), Some(0));
        // A reset already behind us is not a division by zero.
        assert_eq!(projected_global_hourly(30.0, 0.0), None);
        assert_eq!(projected_global_hourly(30.0, -1.0), None);
        assert_eq!(projected_global_hourly(30.0, f64::NAN), None);
    }

    #[test]
    fn t0_is_the_fuse_and_nothing_else() {
        assert_eq!(group_cap(Tier::T0, 60, 2, 3, Some(10)), 60);
        assert_eq!(group_cap(Tier::T0, 7, 1, 3, Some(10)), 7);
        // A disabled fuse stays disabled.
        assert_eq!(group_cap(Tier::T0, 0, 1, 1, None), usize::MAX);
    }

    #[test]
    fn t1_takes_the_strictest_of_fuse_ceiling_and_share() {
        // Share (10/h for weight 2 of 3 → 6) is below the floor, so the floor holds
        // at 8 — below the ceiling and the fuse.
        assert_eq!(group_cap(Tier::T1, 60, 2, 3, Some(10)), 8);
        // A share above the ceiling is cut to the ceiling.
        assert_eq!(group_cap(Tier::T1, 60, 1, 3, Some(1_000)), T1_CAP as usize);
        // A fuse below the ceiling wins.
        assert_eq!(group_cap(Tier::T1, 5, 1, 3, Some(1_000)), 5);
        // No projection: the ceiling still applies.
        assert_eq!(group_cap(Tier::T1, 60, 1, 1, None), T1_CAP as usize);
        // Two groups, weights 2 and 1, a 30/hour global quota: 20 and 10.
        assert_eq!(group_cap(Tier::T1, 60, 2, 3, Some(30)), 20);
        assert_eq!(group_cap(Tier::T1, 60, 1, 3, Some(30)), 10);
    }

    #[test]
    fn t3_is_three_per_group_and_never_more_than_the_fuse() {
        assert_eq!(group_cap(Tier::T3, 60, 2, 3, None), T3_CAP as usize);
        assert_eq!(group_cap(Tier::T3, 2, 2, 3, None), 2);
        assert_eq!(group_cap(Tier::T3, 0, 1, 1, None), T3_CAP as usize);
    }

    #[test]
    fn only_a_mention_or_an_owner_message_clears_the_floor() {
        let plain = entry(false, false);
        let mentioned = entry(true, false);
        let owner = entry(false, true);
        let both = entry(true, true);

        assert!(entry_clears(PrivilegeFloor::MentionOrOwner, &mentioned));
        assert!(entry_clears(PrivilegeFloor::MentionOrOwner, &owner));
        assert!(!entry_clears(PrivilegeFloor::MentionOrOwner, &plain));
        assert!(entry_clears(PrivilegeFloor::OwnerOnly, &owner));
        assert!(entry_clears(PrivilegeFloor::OwnerOnly, &both));
        assert!(!entry_clears(PrivilegeFloor::OwnerOnly, &mentioned));
        assert!(!entry_clears(PrivilegeFloor::OwnerOnly, &plain));

        assert_eq!(privilege_floor(Tier::T0), None);
        assert_eq!(privilege_floor(Tier::T1), None);
        assert_eq!(
            privilege_floor(Tier::T2),
            Some(PrivilegeFloor::MentionOrOwner)
        );
        assert_eq!(privilege_floor(Tier::T3), Some(PrivilegeFloor::OwnerOnly));
    }

    #[test]
    fn sixty_percent_used_is_strict_early_and_loose_late() {
        // The acceptance pair from the dynamic-quota design: the same
        // consumption judged against different clocks.
        // Early: ~5% of the week elapsed, 60% used — far ahead of the line.
        assert_eq!(decide(&paced(40.0, 160.0)).tier, Tier::T3);
        // Late: ~90% elapsed, 60% used — behind the line, quota to spare.
        assert_eq!(decide(&paced(40.0, 17.0)).tier, Tier::T0);
    }

    #[test]
    fn on_pace_consumption_stays_t0() {
        // Half the week gone, half the bucket used: exactly on the line.
        assert_eq!(decide(&paced(50.0, 84.0)).tier, Tier::T0);
        // Ahead of the line tightens by overage: 15 points T1, 30 T2.
        assert_eq!(decide(&paced(35.0, 84.0)).tier, Tier::T1);
        assert_eq!(decide(&paced(20.0, 84.0)).tier, Tier::T2);
    }

    #[test]
    fn a_nearly_empty_bucket_is_t3_even_behind_pace() {
        // 92% used with 90% of the window gone is behind pace, but 8%
        // remaining is the present, not the future.
        assert_eq!(decide(&paced(8.0, 17.0)).tier, Tier::T3);
    }

    #[test]
    fn the_five_hour_bucket_paces_on_its_own_clock() {
        let mut input = paced(90.0, 84.0);
        input.five_hour_pct = Some(30.0);
        input.five_hour_reset_hours = Some(4.5);
        // The weekly bucket is loafing (overage -40) but the 5-hour
        // bucket burned 70% in the first tenth of its window.
        assert_eq!(decide(&input).tier, Tier::T3);
    }

    #[test]
    fn loosening_needs_hysteresis_clearance() {
        // Half the window gone. Remaining 45 -> overage 5: raw T0, but
        // shifted by the hysteresis margin the overage is 10 -> still
        // T1, so a T1 anchor holds instead of flapping.
        let mut input = paced(45.0, 84.0);
        input.last_fresh = Tier::T1;
        assert_eq!(decide(&input).tier, Tier::T1);
        // Remaining 49 -> overage 1, shifted 6: T0 clears the anchor.
        let mut input = paced(49.0, 84.0);
        input.last_fresh = Tier::T1;
        assert_eq!(decide(&input).tier, Tier::T0);
    }

    #[test]
    fn tightening_needs_no_clearance() {
        let mut input = paced(20.0, 84.0);
        input.last_fresh = Tier::T0;
        assert_eq!(decide(&input).tier, Tier::T2);
    }

    #[tokio::test]
    async fn degradation_off_reports_the_ask_but_behaves_t0() {
        let dir = tempfile::tempdir().expect("tmp");
        let path = dir.path().join("bot.sqlite3");
        let pool = crate::store::migrations::open_and_migrate(&path)
            .await
            .expect("migrate");
        let store = Arc::new(SqliteStore::new(pool));
        // Legacy path (no reset times): 20% weekly remaining asks T2.
        let engine = PacingEngine::fixed(Arc::clone(&store), legacy(20.0), "2000000001".into(), 60)
            .with_degradation_enabled(false);
        let report = engine.observe(1_000, &["900000001".to_string()]).await;
        assert_eq!(report.raw_tier, Tier::T2);
        assert_eq!(report.tier, Tier::T0);
        assert!(!report.degradation_enabled);
        // T0 behaviour: the fuse alone caps the group.
        assert_eq!(report.cap_for("900000001"), Some(60));
    }

    #[test]
    fn the_heavy_group_weighs_two() {
        let heavy: std::collections::HashSet<String> =
            ["900000001".to_string()].into_iter().collect();
        assert_eq!(weight_of("900000001", &heavy), 2);
        assert_eq!(weight_of("123456", &heavy), 1);
        assert_eq!(weight_of("900000001", &Default::default()), 1);
    }
}
