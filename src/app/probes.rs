//! Health probes for the resident (P4) path.
//!
//! Everything here is a pure state machine over outcomes the turn path already
//! produces, so the decision "rotate the session" can be unit tested without a
//! PTY, an agent or a clock. The turn path feeds [`ResidentProbe::record`] once per
//! finished round and acts on the returned [`ProbeAction`]; the canary feeds
//! [`ResidentProbe::canary`] on its own schedule.
//!
//! One rule shapes the design: every signal here resolves to a *rotation*. The
//! resident path has no fallback of its own — a session that keeps misbehaving is
//! retired and replaced, and how long the group then waits is decided by the
//! respawn backoff in the resident manager, not by a cooling-off deadline stored
//! next to the group. Draft and step inflation are slow degradations and, like a
//! broken contract, only ever ask for the next session.

use std::collections::VecDeque;

/// How many delivered rounds the rolling windows look at.
pub const WINDOW: usize = 20;
/// Below this contract pass rate the next rotation is forced.
pub const CONTRACT_ROTATE_BELOW: f64 = 0.95;
/// Below this one the rotation is forced regardless of how close to the floor the
/// rate is: the window is no longer describing a working session.
pub const CONTRACT_ROTATE_HARD_BELOW: f64 = 0.90;
/// Consecutive contract failures that trigger a rotation regardless of the rate.
pub const CONSECUTIVE_CONTRACT_FAILS: usize = 3;
/// Step-count p50 growth over the baseline that forces an early rotation.
pub const STEP_INFLATION_RATIO: f64 = 0.50;
/// Rolling mean of withheld drafts above which the surge is audited.
pub const DRAFT_SURGE_MEAN: f64 = 1.5;
/// Rounds in the window that may end on a repair or a silence before the session
/// is rotated anyway.
///
/// Six of twenty is the point where "the output needed mending" stops looking like
/// bad luck. The 2026-10-28 session in group 943 ran for about three hours on
/// salvaged and declined rounds while the contract rate stayed high — no rule here
/// looked at quality at all, so the probe saw a healthy session the whole time.
pub const DEGRADED_ROTATE_COUNT: usize = 6;

/// The fixed canary prompt for the text output channel.
///
/// It asks for a contract-shaped reply and one fact whose correct value is given
/// in the same prompt, so a well-behaved agent has nothing to look up and the
/// check is deterministic. The canary turn's output is never enqueued to the
/// outbox and never written to `group_messages`: it is validated and dropped.
pub const CANARY_TEXT: &str = "内部自检，请只回复一个 JSON 对象，不要执行任何工具：\
{\"should_reply\": true, \"reply_parts\": [\"CANARY-9F3A-OK\"], \"memory_ops\": []}。\
自检事实：本条的校验码是 CANARY-9F3A-OK。";

/// The token the canary reply must contain for the fact check to pass.
pub const CANARY_TOKEN: &str = "CANARY-9F3A-OK";

/// Prompt for one canary attempt. Text keeps the JSON-body contract; MCP asks
/// for `submit_reply` with the armed token so the check reads `turn_submissions`
/// instead of PTY text.
pub fn canary_text(channel: crate::config::OutputChannel, turn_token: Option<&str>) -> String {
    match channel {
        crate::config::OutputChannel::Text => CANARY_TEXT.to_string(),
        crate::config::OutputChannel::Mcp => {
            let token = turn_token.unwrap_or("");
            format!(
                "内部自检，请调用 submit_reply 提交本轮结果，turn_token 用「{token}」，\
should_reply 为 true，reply_parts 含「{CANARY_TOKEN}」。\
自检事实：本条的校验码是 {CANARY_TOKEN}。"
            )
        }
    }
}

/// Local hours the canary is allowed to run: 04:00–04:59.
///
/// Deliberately one hour wide and in the small hours: the canary is a real turn
/// against the account, so it must never compete with a live conversation, and a
/// one-hour window keeps "did it run today" answerable without a scheduler.
pub const CANARY_HOUR_START: u32 = 4;
/// Exclusive end of the canary window.
pub const CANARY_HOUR_END: u32 = 5;

/// Whether `local_hour` falls inside the canary window.
pub fn canary_window_open(local_hour: u32) -> bool {
    (CANARY_HOUR_START..CANARY_HOUR_END).contains(&local_hour)
}

/// What one finished resident round looked like.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct RoundOutcome {
    /// The contract validated.
    pub contract_ok: bool,
    /// Steps the stream held when the round finished.
    pub steps: i64,
    /// Agent drafts the barrier withheld.
    pub drafts: i64,
    /// The round held together, but only just: its text needed the salvage ladder,
    /// or it ended on a silence the turn path had to work around.
    ///
    /// `contract_ok` alone cannot see this — a repaired document and a declined
    /// answer both pass validation. A few such rounds are normal (an ambient batch
    /// may be declined freely); a window full of them is a session that is talking
    /// less and less and should not be trusted with the next batch.
    pub degraded: bool,
}

/// What the caller should do next.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProbeAction {
    /// Nothing unusual; keep going.
    None,
    /// Rotate the session before the next round.
    Rotate(&'static str),
}

impl ProbeAction {
    /// Audit action suffix, or `None` for [`ProbeAction::None`].
    pub fn audit_reason(&self) -> Option<&'static str> {
        match self {
            Self::None => None,
            Self::Rotate(reason) => Some(reason),
        }
    }
}

/// Rolling health state for one group.
#[derive(Debug, Default)]
pub struct ResidentProbe {
    rounds: VecDeque<RoundOutcome>,
    /// Step counts from before the current window, used as the inflation baseline.
    baseline_steps: Vec<i64>,
    consecutive_contract_fails: usize,
}

impl ResidentProbe {
    pub fn new() -> Self {
        Self::default()
    }

    /// Record one delivered round and decide what to do about it.
    ///
    /// Order matters: a broken streak outranks a rate rule, and a drift signal
    /// (handled by the caller through [`Self::drift`]) outranks everything.
    pub fn record(&mut self, outcome: RoundOutcome) -> ProbeAction {
        self.rounds.push_back(outcome);
        if self.rounds.len() > WINDOW {
            // Whatever falls out of the window becomes baseline history.
            if let Some(old) = self.rounds.pop_front() {
                self.baseline_steps.push(old.steps);
                if self.baseline_steps.len() > WINDOW {
                    self.baseline_steps.remove(0);
                }
            }
        }
        if outcome.contract_ok {
            self.consecutive_contract_fails = 0;
        } else {
            self.consecutive_contract_fails += 1;
        }

        if self.consecutive_contract_fails >= CONSECUTIVE_CONTRACT_FAILS {
            // The counter restarts with the replacement session: the rotation below
            // already retired the conversation these failures belong to.
            self.consecutive_contract_fails = 0;
            return ProbeAction::Rotate("contract_streak");
        }
        // Rate rules wait for a full window. Judging a rate off two or three
        // rounds would rotate on the first hiccup; the consecutive-failure rule
        // above is what catches a broken session immediately.
        if self.rounds.len() >= WINDOW
            && let Some(rate) = self.contract_rate()
        {
            if rate < CONTRACT_ROTATE_HARD_BELOW {
                return ProbeAction::Rotate("contract_rate_hard");
            }
            if rate < CONTRACT_ROTATE_BELOW {
                return ProbeAction::Rotate("contract_rate");
            }
        }
        if self.steps_inflated() {
            return ProbeAction::Rotate("step_inflation");
        }
        // Quality of the rounds, not just their contract: a session can pass every
        // check while mostly mending or declining what it produces. Judged on a full
        // window like the contract rate, so a short run of bad luck cannot rotate a
        // session that has no history yet.
        if self.rounds.len() >= WINDOW && self.degraded_count() >= DEGRADED_ROTATE_COUNT {
            return ProbeAction::Rotate("degraded_window");
        }
        if outcome.degraded {
            // A repaired or declined round did pass, so it keeps the contract numbers
            // honest, but it is not evidence of health: only the degraded-window rule
            // above is allowed to act on them.
            return ProbeAction::None;
        }
        if self.draft_surge() {
            // Advisory only: drafts are withheld safely, so this never rotates.
            return ProbeAction::None;
        }
        ProbeAction::None
    }

    /// A step-stream parse failure. Drift is a format change, not a slow
    /// degradation, so it rotates immediately and without a window.
    pub fn drift(&mut self) -> ProbeAction {
        ProbeAction::Rotate("drift")
    }

    /// Record one canary result. A failure always rotates.
    pub fn canary(&mut self, ok: bool) -> ProbeAction {
        if ok {
            return ProbeAction::None;
        }
        ProbeAction::Rotate("canary")
    }

    /// Pass rate over the rolling window, or `None` before the first round.
    pub fn contract_rate(&self) -> Option<f64> {
        if self.rounds.is_empty() {
            return None;
        }
        let ok = self.rounds.iter().filter(|r| r.contract_ok).count();
        Some(ok as f64 / self.rounds.len() as f64)
    }

    /// Mean drafts withheld over the window.
    pub fn draft_mean(&self) -> f64 {
        if self.rounds.is_empty() {
            return 0.0;
        }
        let total: i64 = self.rounds.iter().map(|r| r.drafts).sum();
        total as f64 / self.rounds.len() as f64
    }

    /// True when the window's median step count grew more than
    /// [`STEP_INFLATION_RATIO`] over the baseline window.
    pub fn steps_inflated(&self) -> bool {
        let (Some(now), Some(base)) = (self.median_steps(false), self.median_steps(true)) else {
            return false;
        };
        if base <= 0 {
            return false;
        }
        (now - base) as f64 / base as f64 > STEP_INFLATION_RATIO
    }

    fn draft_surge(&self) -> bool {
        self.rounds.len() >= WINDOW && self.draft_mean() > DRAFT_SURGE_MEAN
    }

    /// Rounds in the window that ended on a repair or a workaround for a silence.
    pub fn degraded_count(&self) -> usize {
        self.rounds.iter().filter(|r| r.degraded).count()
    }

    fn median_steps(&self, baseline: bool) -> Option<i64> {
        let mut values: Vec<i64> = if baseline {
            self.baseline_steps.clone()
        } else {
            self.rounds.iter().map(|r| r.steps).collect()
        };
        if values.is_empty() {
            return None;
        }
        values.sort_unstable();
        Some(values[values.len() / 2])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ok(steps: i64) -> RoundOutcome {
        RoundOutcome {
            contract_ok: true,
            steps,
            drafts: 0,
            degraded: false,
        }
    }

    fn bad(steps: i64) -> RoundOutcome {
        RoundOutcome {
            contract_ok: false,
            steps,
            drafts: 0,
            degraded: false,
        }
    }

    fn degraded(steps: i64) -> RoundOutcome {
        RoundOutcome {
            contract_ok: true,
            steps,
            drafts: 0,
            degraded: true,
        }
    }

    #[test]
    fn a_healthy_window_stays_quiet() {
        let mut probe = ResidentProbe::new();
        for _ in 0..WINDOW {
            assert_eq!(probe.record(ok(10)), ProbeAction::None);
        }
        assert_eq!(probe.contract_rate(), Some(1.0));
    }

    #[test]
    fn a_wobble_below_the_rotate_floor_only_ever_rotates() {
        let mut probe = ResidentProbe::new();
        for _ in 0..WINDOW {
            assert_eq!(probe.record(ok(10)), ProbeAction::None);
        }
        // 19/20 = 0.95 is exactly at the rotate floor: still quiet.
        assert_eq!(probe.record(bad(10)), ProbeAction::None);
        assert_eq!(probe.record(ok(10)), ProbeAction::None);
        // 18/20 = 0.90 sits between the two floors.
        assert_eq!(probe.record(bad(10)), ProbeAction::Rotate("contract_rate"));
        // The failures are not consecutive, so the streak rule stays out of it.
        probe.record(ok(10));
        // 17/20 = 0.85 is under the hard floor: the rotation is forced, and it is
        // still a rotation — the resident path has nothing to fall back to.
        assert_eq!(
            probe.record(bad(10)),
            ProbeAction::Rotate("contract_rate_hard")
        );
    }

    #[test]
    fn a_collapsed_contract_rate_rotates_without_waiting_for_the_rate_rule() {
        let mut probe = ResidentProbe::new();
        let mut action = ProbeAction::None;
        for _ in 0..WINDOW {
            action = probe.record(bad(10));
            if matches!(action, ProbeAction::Rotate(_)) {
                break;
            }
        }
        assert_eq!(action, ProbeAction::Rotate("contract_streak"));
    }

    #[test]
    fn three_consecutive_contract_failures_rotate_without_waiting_for_a_window() {
        let mut probe = ResidentProbe::new();
        for _ in 0..(WINDOW - 3) {
            probe.record(ok(10));
        }
        // Two failures inside a window that is still filling: the rate rule needs
        // a full window, so nothing fires yet.
        assert_eq!(probe.record(bad(10)), ProbeAction::None);
        assert_eq!(probe.record(bad(10)), ProbeAction::None);
        // The third consecutive failure rotates without waiting for the window.
        assert_eq!(
            probe.record(bad(10)),
            ProbeAction::Rotate("contract_streak")
        );
        // The streak restarted, so the very next failure is not a second streak —
        // whatever it hits, it is not `contract_streak` fired off the old counter.
        assert_ne!(
            probe.record(bad(10)),
            ProbeAction::Rotate("contract_streak")
        );
    }

    #[test]
    fn step_inflation_rotates_once_the_baseline_exists() {
        let mut probe = ResidentProbe::new();
        for _ in 0..WINDOW {
            probe.record(ok(10));
        }
        // Sliding 20 rounds at 10 steps each fills the baseline with 10s.
        for _ in 0..WINDOW {
            probe.record(ok(10));
        }
        assert!(!probe.steps_inflated(), "baseline 与窗口一致时不应判膨胀");
        // 16 vs 10 is +60%: over the 50% threshold.
        let mut action = ProbeAction::None;
        for _ in 0..WINDOW {
            action = probe.record(ok(16));
        }
        assert_eq!(action, ProbeAction::Rotate("step_inflation"));
    }

    #[test]
    fn draft_surge_is_advisory_only() {
        let mut probe = ResidentProbe::new();
        for _ in 0..WINDOW {
            probe.record(RoundOutcome {
                contract_ok: true,
                steps: 10,
                drafts: 3,
                degraded: false,
            });
        }
        assert!(probe.draft_mean() > DRAFT_SURGE_MEAN);
        assert_eq!(probe.record(ok(10)), ProbeAction::None);
    }

    #[test]
    fn a_degraded_window_rotates_without_touching_the_contract_rate() {
        let mut probe = ResidentProbe::new();
        // Every round validates, so the contract rate is a perfect 1.0 and no rule
        // above looks at them. This is the 2026-10-28 shape: three hours of rounds
        // that passed validation by being repaired, with the probe seeing health.
        for _ in 0..(WINDOW - DEGRADED_ROTATE_COUNT) {
            assert_eq!(probe.record(ok(10)), ProbeAction::None);
        }
        assert_eq!(probe.contract_rate(), Some(1.0));
        let mut action = ProbeAction::None;
        for _ in 0..DEGRADED_ROTATE_COUNT {
            action = probe.record(degraded(10));
        }
        assert_eq!(action, ProbeAction::Rotate("degraded_window"));
        assert_eq!(probe.degraded_count(), DEGRADED_ROTATE_COUNT);
    }

    #[test]
    fn a_few_degraded_rounds_stay_quiet() {
        let mut probe = ResidentProbe::new();
        // One short of the threshold: an ambient group may decline a batch here and
        // there, and a session is not rotated for it.
        for _ in 0..(DEGRADED_ROTATE_COUNT - 1) {
            assert_eq!(probe.record(degraded(10)), ProbeAction::None);
        }
        assert_eq!(probe.degraded_count(), DEGRADED_ROTATE_COUNT - 1);
    }

    #[test]
    fn a_short_degraded_run_does_not_rotate() {
        let mut probe = ResidentProbe::new();
        // The window has to fill first, for the same reason the contract rate waits:
        // rotating a session that has no history would rotate on its first bad day.
        for _ in 0..(WINDOW - 1) {
            assert_eq!(probe.record(degraded(10)), ProbeAction::None);
        }
        assert!(probe.degraded_count() > DEGRADED_ROTATE_COUNT);
    }

    #[test]
    fn the_degraded_window_does_not_touch_the_hard_failure_ladder() {
        let mut probe = ResidentProbe::new();
        // A contract failure is still a failure: three in a row rotate immediately,
        // regardless of how many rounds were merely degraded. The quality rule only
        // ever adds another rotation, never a different verdict.
        for _ in 0..(CONSECUTIVE_CONTRACT_FAILS - 1) {
            assert_eq!(probe.record(bad(10)), ProbeAction::None);
        }
        assert_eq!(
            probe.record(bad(10)),
            ProbeAction::Rotate("contract_streak")
        );
    }

    #[test]
    fn drift_rotates_immediately() {
        let mut probe = ResidentProbe::new();
        assert_eq!(probe.drift(), ProbeAction::Rotate("drift"));
    }

    #[test]
    fn a_failed_canary_rotates() {
        let mut probe = ResidentProbe::new();
        assert_eq!(probe.canary(false), ProbeAction::Rotate("canary"));
        // A second failure the next day is the same verdict, not a different one.
        assert_eq!(probe.canary(false), ProbeAction::Rotate("canary"));
    }

    #[test]
    fn a_passing_canary_is_quiet() {
        let mut probe = ResidentProbe::new();
        assert_eq!(probe.canary(false), ProbeAction::Rotate("canary"));
        assert_eq!(probe.canary(true), ProbeAction::None);
        assert_eq!(probe.canary(false), ProbeAction::Rotate("canary"));
    }

    #[test]
    fn the_canary_prompt_carries_its_own_answer() {
        assert!(CANARY_TEXT.contains(CANARY_TOKEN));
        assert!(CANARY_TEXT.contains("reply_parts"));
        let mcp = canary_text(crate::config::OutputChannel::Mcp, Some("tok-1"));
        assert!(
            !mcp.contains("不要执行任何工具"),
            "MCP canary must call submit_reply"
        );
        assert!(mcp.contains("submit_reply"));
        assert!(mcp.contains("tok-1"));
        assert!(mcp.contains(CANARY_TOKEN));
        assert_eq!(
            canary_text(crate::config::OutputChannel::Text, None),
            CANARY_TEXT
        );
    }
}
