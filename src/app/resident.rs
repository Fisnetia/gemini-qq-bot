//! Per-group registry of resident (PTY) sessions.
//!
//! The supervisor in [`crate::agent::resident`] owns one process; this module owns
//! the mapping from group to process and nothing else: lazy creation, the pause
//! reclaim sweep, and shutdown. Turn orchestration stays in
//! [`crate::app::turn::TurnExecutor`] so the barrier, the contract validator and
//! the outbox stay in one place rather than being re-implemented per lane.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::agent::resident::{
    DELIVER_CONFIRM_TIMEOUT_SECONDS, DbStepReader, ResidentConfig, ResidentError, ResidentSession,
    StepReader,
};
use crate::app::probes::{ProbeAction, ResidentProbe, RoundOutcome};
use crate::store::SqliteStore;

mod poison;

/// Suffix that marks a session as a scratch probe session rather than the group's
/// real one. The canary uses it so its transcript, its step count and its
/// conversation id stay out of the group's own session.
const SCRATCH_SUFFIX: &str = "#canary";

/// Session key for a group's scratch canary session.
pub fn canary_session_key(group_id: &str) -> String {
    format!("{group_id}{SCRATCH_SUFFIX}")
}

/// How long a group waits before another resident session is spawned for it,
/// indexed by consecutive failure tier (1-based, so tier 1 waits
/// [`RESPAWN_BACKOFF_STEPS`[0]]).
///
/// The first step is deliberately short: a session that died during onboarding is
/// usually a transient (proxy not up yet, agent binary still warming) and one
/// minute costs nothing. The ladder then widens to a ceiling of half an hour, so a
/// genuinely broken group stops burning a spawn per flush tick without ever being
/// parked long enough that an operator's fix looks ignored.
pub const RESPAWN_BACKOFF_STEPS: [i64; 4] = [60, 300, 900, 1800];

/// Delivered resident rounds that clear a group's respawn backoff entirely.
pub const RESPAWN_CLEAN_ROUNDS: u32 = 10;

/// Backoff state for one group: which rung of the ladder it is on, the deadline it
/// is waiting for, and how many clean rounds it has accumulated since.
#[derive(Debug, Clone, Copy, Default)]
struct BackoffState {
    tier: u32,
    /// Exclusive-ish deadline: the group is out while `until > now`.
    until: i64,
    clean_rounds: u32,
}

/// True for keys that name a scratch session (`"<group>#canary"`). Such a key has
/// no row in `groups`, so nothing group-shaped — pause reclaim, prewarm, the
/// status endpoint — may treat it as a group.
fn is_scratch_key(key: &str) -> bool {
    key.contains(SCRATCH_SUFFIX)
}

/// Strip the canary suffix so a scratch session still pins the real group.
fn real_group_id(key: &str) -> &str {
    key.strip_suffix(SCRATCH_SUFFIX).unwrap_or(key)
}

/// Live sessions, keyed by group id.
pub struct ResidentManager {
    store: Arc<SqliteStore>,
    bot_id: String,
    bin: PathBuf,
    home: PathBuf,
    model: String,
    /// Console-selected model and effort; `None` keeps the startup defaults.
    engine: Option<crate::app::runtime_settings::SharedEngine>,
    reader: Arc<dyn StepReader>,
    sessions: tokio::sync::Mutex<HashMap<String, Arc<ResidentSession>>>,
    /// Spawns that have not finished onboarding. `sessions` stays empty until
    /// `wait_ready` returns, and the flush loop ticks every 500ms, so without
    /// this map a restart births one PTY per tick per group.
    inflight: Arc<std::sync::Mutex<HashMap<String, Arc<SpawnFlight>>>>,
    /// `(group_id, paused_since)` for groups seen paused on the last sweep.
    paused_since: tokio::sync::Mutex<HashMap<String, i64>>,
    /// Rolling health window per group. Kept here rather than in the turn path so
    /// it survives the per-batch `TurnExecutor` borrow and is visible to the
    /// canary scheduler too.
    probes: tokio::sync::Mutex<HashMap<String, ResidentProbe>>,
    /// Local day number (Unix days) of the last canary per group, so the daily
    /// probe fires at most once.
    canary_day: tokio::sync::Mutex<HashMap<String, i64>>,
    /// Consecutive delivery-confirmation failures per group. A session whose
    /// screen stopped accepting prompts (the 2026-10-27 wedge: a TUI left on
    /// a proxy-error screen after a restart) looks alive to every other
    /// check, so the turn path counts strikes here and the executor drops
    /// the session on the second one instead of failing batches forever.
    deliver_strikes: tokio::sync::Mutex<HashMap<String, u32>>,
    /// Consecutive resident retirements per group, and the deadline each one bought.
    ///
    /// Replaces the old database cooldown: nothing about a respawn has to survive a
    /// restart (the sessions themselves do not), so the ladder is pure memory state
    /// and the `groups` row stays free of scheduling bookkeeping.
    respawn_backoff: tokio::sync::Mutex<HashMap<String, BackoffState>>,
    /// Whether the respawn ladder is armed at all (config
    /// `respawn_backoff_enabled`). Off: retirements record nothing and
    /// no stored deadline defers a spawn — the kill switch for the
    /// cooldown, for groups whose failures are not theirs (a model
    /// content filter) and whose silence costs more than respawns.
    respawn_backoff_enabled: bool,
    /// One-shot replay marks for batches that died with no submission
    /// (2026-10-29): group -> the exact event ids the retired turn covered.
    /// Armed by the executor at a no-submission retirement, consumed by the
    /// intake, which must claim exactly those rows as their own job before
    /// any merging. In memory on purpose: losing a mark on restart degrades
    /// to the pre-replay release-and-merge behaviour, nothing worse.
    retry_marks: tokio::sync::Mutex<HashMap<String, Vec<i64>>>,
    /// Consecutive prompt-level content-filter rejections per group,
    /// counted over the event ids each refused turn covered. The second
    /// consecutive refusal of overlapping material drops the batch
    /// instead of releasing it for another identical submission (see
    /// [`poison`]).
    prompt_rejections: tokio::sync::Mutex<poison::PromptRejections>,
    /// Delivery-confirmation window for spawned sessions, in seconds.
    /// Defaults to the agent layer's `DELIVER_CONFIRM_TIMEOUT_SECONDS`;
    /// production overwrites it from `resident_deliver_timeout_s` at build
    /// time. The spawn path used to read the constant directly, which left
    /// the config key dead (found 2026-10-29: raising the key in
    /// config.toml changed nothing in production).
    deliver_timeout_s: u64,
}

impl std::fmt::Debug for ResidentManager {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ResidentManager")
            .field("bot_id", &self.bot_id)
            .field("model", &self.model)
            .finish_non_exhaustive()
    }
}

type SpawnResult = Result<Arc<ResidentSession>, String>;

struct SpawnFlight {
    tx: tokio::sync::watch::Sender<Option<SpawnResult>>,
}

/// Removes the in-flight entry on every exit, including task cancellation.
/// A published result is left untouched; a cancel wakes waiters with an error.
struct SpawnGuard {
    inflight: Arc<std::sync::Mutex<HashMap<String, Arc<SpawnFlight>>>>,
    group_id: String,
    flight: Arc<SpawnFlight>,
    published: bool,
}

impl Drop for SpawnGuard {
    fn drop(&mut self) {
        if !self.published && self.flight.tx.borrow().is_none() {
            let _ = self
                .flight
                .tx
                .send(Some(Err("spawn_cancelled".to_string())));
        }
        let mut inflight = lock_inflight(&self.inflight);
        if inflight
            .get(&self.group_id)
            .is_some_and(|current| Arc::ptr_eq(current, &self.flight))
        {
            inflight.remove(&self.group_id);
        }
    }
}

fn lock_inflight(
    inflight: &std::sync::Mutex<HashMap<String, Arc<SpawnFlight>>>,
) -> std::sync::MutexGuard<'_, HashMap<String, Arc<SpawnFlight>>> {
    inflight.lock().unwrap_or_else(|err| err.into_inner())
}

async fn follow_spawn(
    mut rx: tokio::sync::watch::Receiver<Option<SpawnResult>>,
) -> Result<Arc<ResidentSession>, ResidentError> {
    loop {
        if let Some(result) = rx.borrow().clone() {
            return result.map_err(ResidentError::Blocked);
        }
        if rx.changed().await.is_err() {
            return Err(ResidentError::Blocked("spawn_cancelled".to_string()));
        }
    }
}

impl ResidentManager {
    pub fn new(
        store: Arc<SqliteStore>,
        bot_id: String,
        bin: PathBuf,
        home: PathBuf,
        model: String,
    ) -> Self {
        Self {
            store,
            bot_id,
            bin,
            home,
            model,
            engine: None,
            reader: Arc::new(DbStepReader),
            sessions: tokio::sync::Mutex::new(HashMap::new()),
            inflight: Arc::new(std::sync::Mutex::new(HashMap::new())),
            paused_since: tokio::sync::Mutex::new(HashMap::new()),
            probes: tokio::sync::Mutex::new(HashMap::new()),
            canary_day: tokio::sync::Mutex::new(HashMap::new()),
            deliver_strikes: tokio::sync::Mutex::new(HashMap::new()),
            respawn_backoff: tokio::sync::Mutex::new(HashMap::new()),
            respawn_backoff_enabled: true,
            retry_marks: tokio::sync::Mutex::new(HashMap::new()),
            prompt_rejections: tokio::sync::Mutex::new(poison::PromptRejections::new()),
            deliver_timeout_s: DELIVER_CONFIRM_TIMEOUT_SECONDS,
        }
    }

    /// Overwrite the delivery-confirmation window (config
    /// `resident_deliver_timeout_s`).
    pub fn with_deliver_timeout_s(mut self, secs: u64) -> Self {
        self.deliver_timeout_s = secs.max(1);
        self
    }

    /// Overwrite the respawn-cooldown switch (config
    /// `respawn_backoff_enabled`).
    pub fn with_engine(mut self, engine: crate::app::runtime_settings::SharedEngine) -> Self {
        self.engine = Some(engine);
        self
    }

    /// The model and effort for a session starting now.
    fn engine_choice(&self) -> (String, String) {
        match &self.engine {
            Some(engine) => {
                let engine = engine.read().unwrap_or_else(|e| e.into_inner());
                (engine.model.clone(), engine.effort.clone())
            }
            None => (
                self.model.clone(),
                crate::agent::agy::AGY_EFFORT.to_string(),
            ),
        }
    }

    pub fn with_respawn_backoff_enabled(mut self, enabled: bool) -> Self {
        self.respawn_backoff_enabled = enabled;
        self
    }

    /// Record a delivery-confirmation failure; returns the strike count.
    pub async fn note_deliver_failure(&self, group_id: &str) -> u32 {
        let mut strikes = self.deliver_strikes.lock().await;
        let entry = strikes.entry(group_id.to_string()).or_default();
        *entry += 1;
        *entry
    }

    /// A confirmed delivery clears the strike count.
    pub async fn note_deliver_success(&self, group_id: &str) {
        self.deliver_strikes.lock().await.remove(group_id);
    }

    /// Arm the one-shot no-submission replay for a group (2026-10-29).
    pub async fn arm_retry(&self, group_id: &str, event_ids: Vec<i64>) {
        self.retry_marks
            .lock()
            .await
            .insert(group_id.to_string(), event_ids);
    }

    /// The armed replay mark for a group, if any (peek: the intake clears
    /// it only once the rows are claimed or proven gone).
    pub async fn retry_mark(&self, group_id: &str) -> Option<Vec<i64>> {
        self.retry_marks.lock().await.get(group_id).cloned()
    }

    /// Drop a group's replay mark without claiming it.
    pub async fn clear_retry(&self, group_id: &str) {
        self.retry_marks.lock().await.remove(group_id);
    }

    /// Record one resident retirement and arm the next spawn deadline.
    ///
    /// Returns the tier (1-based) and the deadline it bought. The tier climbs on
    /// every consecutive retirement and is the only thing that widens the wait;
    /// [`Self::note_respawn_clean`] is what walks it back down.
    pub async fn note_respawn_failure(&self, group_id: &str, now: i64) -> (u32, i64) {
        if !self.respawn_backoff_enabled {
            // Cooldown switched off: the retirement buys no wait, and
            // any rung the group was sitting on is dropped with it.
            // Tier 0 tells the caller nothing was armed.
            self.respawn_backoff.lock().await.remove(group_id);
            return (0, now);
        }
        let mut backoff = self.respawn_backoff.lock().await;
        let entry = backoff.entry(group_id.to_string()).or_default();
        entry.tier = entry.tier.saturating_add(1);
        entry.clean_rounds = 0;
        let step = RESPAWN_BACKOFF_STEPS
            [(entry.tier - 1).min(RESPAWN_BACKOFF_STEPS.len() as u32 - 1) as usize];
        entry.until = now + step;
        (entry.tier, entry.until)
    }

    /// Arm a short fixed wait after a content-filter refusal, without climbing
    /// the ladder.
    ///
    /// A refusal says something about the chat, not the session: a fresh session
    /// is healthy, so walking the ladder for it put a group in a 30-minute
    /// silence for what people happened to say (2026-11-08). Repeated refusals
    /// of the same batch are already dropped by the poison check.
    pub async fn note_content_filter(&self, group_id: &str, now: i64) -> (u32, i64) {
        if !self.respawn_backoff_enabled {
            self.respawn_backoff.lock().await.remove(group_id);
            return (0, now);
        }
        let mut backoff = self.respawn_backoff.lock().await;
        let entry = backoff.entry(group_id.to_string()).or_default();
        entry.until = now + RESPAWN_BACKOFF_STEPS[0];
        (entry.tier.max(1), entry.until)
    }

    /// Whether the group is still waiting out a respawn deadline.
    ///
    /// The deadline is inclusive in the same direction the probe used to be: the
    /// group may spawn again at `now == until`, not one tick later.
    pub async fn backoff_active(&self, group_id: &str, now: i64) -> bool {
        if !self.respawn_backoff_enabled {
            return false;
        }
        let backoff = self.respawn_backoff.lock().await;
        backoff.get(group_id).is_some_and(|state| state.until > now)
    }

    /// The deadline the group is waiting for, if any. Only for the audit row.
    pub async fn backoff_until(&self, group_id: &str, now: i64) -> Option<i64> {
        self.backoff_status(group_id, now)
            .await
            .map(|(_, until)| until)
    }

    /// Live respawn cooldown for one group: `(tier, until)` while `until > now`.
    pub async fn backoff_status(&self, group_id: &str, now: i64) -> Option<(u32, i64)> {
        if !self.respawn_backoff_enabled {
            return None;
        }
        let backoff = self.respawn_backoff.lock().await;
        backoff
            .get(group_id)
            .filter(|state| state.until > now)
            .map(|state| (state.tier, state.until))
    }

    /// Every group still sitting out a respawn deadline. The panel reads this;
    /// expired rungs are omitted so a restart that dropped the map shows empty.
    pub async fn active_backoffs(&self, now: i64) -> Vec<(String, u32, i64)> {
        if !self.respawn_backoff_enabled {
            return Vec::new();
        }
        let backoff = self.respawn_backoff.lock().await;
        backoff
            .iter()
            .filter(|(_, state)| state.until > now)
            .map(|(group_id, state)| (group_id.clone(), state.tier, state.until))
            .collect()
    }

    /// Record one delivered resident round for the backoff ladder.
    ///
    /// A run of clean rounds is the only evidence that clears the penalty, and it
    /// clears it completely: the next retirement starts again at the bottom rung,
    /// which is what stops a group that had one bad week from carrying the ceiling
    /// forever. Rounds that did not require a retirement call this; nothing else
    /// does, so nothing outside a completed round can rehabilitate a session.
    pub async fn note_respawn_clean(&self, group_id: &str) {
        let mut backoff = self.respawn_backoff.lock().await;
        let Some(entry) = backoff.get_mut(group_id) else {
            return;
        };
        // Still waiting out the deadline: rounds from a session that is about to be
        // retired anyway are not evidence, and counting them would clear the wait
        // early.
        if entry.until > crate::now_unix() {
            return;
        }
        entry.clean_rounds += 1;
        if entry.clean_rounds >= RESPAWN_CLEAN_ROUNDS {
            backoff.remove(group_id);
        }
    }

    /// Whether a session handle exists for the group (warm-pool checks).
    pub async fn has_session(&self, group_id: &str) -> bool {
        self.sessions.lock().await.contains_key(group_id)
    }

    /// Feed one finished round into the group's rolling health window.
    pub async fn record_round(&self, group_id: &str, outcome: RoundOutcome) -> ProbeAction {
        let mut probes = self.probes.lock().await;
        probes
            .entry(group_id.to_string())
            .or_default()
            .record(outcome)
    }

    /// Report a step-stream parse drift. Rotates the session immediately.
    pub async fn record_drift(&self, group_id: &str) -> ProbeAction {
        let mut probes = self.probes.lock().await;
        probes.entry(group_id.to_string()).or_default().drift()
    }

    /// Feed one canary result into the group's streak.
    pub async fn record_canary(&self, group_id: &str, ok: bool) -> ProbeAction {
        let mut probes = self.probes.lock().await;
        probes.entry(group_id.to_string()).or_default().canary(ok)
    }

    /// Whether the group's canary is due: it has not run today, the local clock is
    /// inside the off-peak window, and nothing else is using the session.
    ///
    /// Marking the day happens when the canary actually starts, so a group that is
    /// mid-turn at 04:00 simply waits for the next tick inside the window.
    pub async fn canary_due(&self, group_id: &str, day: i64, local_hour: u32) -> bool {
        if !crate::app::probes::canary_window_open(local_hour) {
            return false;
        }
        let days = self.canary_day.lock().await;
        days.get(group_id).copied() != Some(day)
    }

    /// Mark today's canary as started for this group.
    pub async fn canary_started(&self, group_id: &str, day: i64) {
        self.canary_day
            .lock()
            .await
            .insert(group_id.to_string(), day);
    }

    /// Return the group's session, spawning it on first use.
    ///
    /// `conversation_id` is only consulted when a new process is started, so a
    /// rotation performed by the turn logic takes effect on the next spawn rather
    /// than forcing a restart mid-turn.
    pub async fn session(
        &self,
        group_id: &str,
        workspace: &Path,
        conversation_id: Option<String>,
    ) -> Result<Arc<ResidentSession>, ResidentError> {
        if let Some(live) = self.live_session(group_id).await {
            return Ok(live);
        }
        // One spawn per group. Prewarm and the turn path both call this while
        // onboarding is still running; the old code let every caller fork a PTY
        // and only closed the losers after they had finished booting.
        let (flight, leader) = self.claim_spawn(group_id);
        if !leader {
            return follow_spawn(flight.tx.subscribe()).await;
        }
        let mut guard = SpawnGuard {
            inflight: Arc::clone(&self.inflight),
            group_id: group_id.to_string(),
            flight: Arc::clone(&flight),
            published: false,
        };
        let result = self
            .start_session(group_id, workspace, conversation_id)
            .await;
        let published = match &result {
            Ok(session) => Ok(Arc::clone(session)),
            Err(err) => Err(err.to_string()),
        };
        let _ = flight.tx.send(Some(published));
        guard.published = true;
        result
    }

    /// True while this group has a spawn that has not yet published a result.
    /// Prewarm uses it to avoid stacking a join-task on every flush tick.
    pub fn spawn_in_flight(&self, group_id: &str) -> bool {
        lock_inflight(&self.inflight).contains_key(group_id)
    }

    async fn live_session(&self, group_id: &str) -> Option<Arc<ResidentSession>> {
        let existing = {
            let sessions = self.sessions.lock().await;
            sessions.get(group_id).map(Arc::clone)
        };
        match existing {
            Some(session)
                if !matches!(
                    session.state().await,
                    crate::agent::resident::SessionState::Dead(_)
                        | crate::agent::resident::SessionState::Blocked(_)
                ) =>
            {
                Some(session)
            }
            _ => None,
        }
    }

    fn claim_spawn(&self, group_id: &str) -> (Arc<SpawnFlight>, bool) {
        let mut inflight = lock_inflight(&self.inflight);
        if let Some(flight) = inflight.get(group_id) {
            return (Arc::clone(flight), false);
        }
        let (tx, _rx) = tokio::sync::watch::channel(None);
        let flight = Arc::new(SpawnFlight { tx });
        inflight.insert(group_id.to_string(), Arc::clone(&flight));
        (flight, true)
    }

    async fn start_session(
        &self,
        group_id: &str,
        workspace: &Path,
        conversation_id: Option<String>,
    ) -> Result<Arc<ResidentSession>, ResidentError> {
        if let Some(live) = self.live_session(group_id).await {
            return Ok(live);
        }
        let existing = {
            let sessions = self.sessions.lock().await;
            sessions.get(group_id).map(Arc::clone)
        };
        let stale = match existing {
            Some(session) => {
                let mut sessions = self.sessions.lock().await;
                match sessions.get(group_id) {
                    Some(current) if Arc::ptr_eq(current, &session) => {
                        sessions.remove(group_id);
                        Some(session)
                    }
                    _ => {
                        let winner = sessions.get(group_id).map(Arc::clone);
                        return match winner {
                            Some(winner) => Ok(winner),
                            None => Err(ResidentError::Blocked("session_vanished".to_string())),
                        };
                    }
                }
            }
            None => None,
        };
        if let Some(dead) = stale {
            dead.close().await;
            for action in dead.drain_audit() {
                self.audit(group_id, &action).await;
            }
        }

        let (model, effort) = self.engine_choice();
        let config = ResidentConfig {
            bin: self.bin.clone(),
            home: self.home.clone(),
            workspace: workspace.to_path_buf(),
            model,
            effort,
            conversation_id,
            deliver_timeout_s: self.deliver_timeout_s,
            memory_scope: Some(format!("{}:{}", self.bot_id, real_group_id(group_id))),
        };
        let fresh =
            Arc::new(ResidentSession::spawn(&config, group_id, Arc::clone(&self.reader)).await?);
        for action in fresh.drain_audit() {
            self.audit(group_id, &action).await;
        }
        let installed = {
            let mut sessions = self.sessions.lock().await;
            match sessions.get(group_id) {
                Some(winner) => Err(Arc::clone(winner)),
                None => {
                    sessions.insert(group_id.to_string(), Arc::clone(&fresh));
                    Ok(())
                }
            }
        };
        match installed {
            Ok(()) => Ok(fresh),
            Err(winner) => {
                fresh.close().await;
                Ok(winner)
            }
        }
    }

    /// Replace the group's session with a rebuilt one, resuming the same
    /// conversation when `agy` allows it and flagging a cold start otherwise.
    pub async fn rebuild(
        &self,
        group_id: &str,
    ) -> Result<(Arc<ResidentSession>, crate::agent::resident::RebuildKind), ResidentError> {
        // Same rule as `session`: the rebuild walks a PTY boot, so it happens with
        // the lock released and only the installed handle is written back under it.
        let existing = {
            let mut sessions = self.sessions.lock().await;
            sessions.remove(group_id)
        };
        let Some(existing) = existing else {
            return Err(ResidentError::Blocked(
                "rebuild_without_session".to_string(),
            ));
        };
        match existing.rebuild().await {
            Ok((fresh, kind)) => {
                let action = match kind {
                    crate::agent::resident::RebuildKind::Resume => "session.rebuild_resume",
                    crate::agent::resident::RebuildKind::NeedsSeed => "session.rebuild_seed",
                };
                let fresh = Arc::new(fresh);
                for line in fresh.drain_audit() {
                    self.audit(group_id, &line).await;
                }
                self.sessions
                    .lock()
                    .await
                    .insert(group_id.to_string(), Arc::clone(&fresh));
                self.audit(group_id, &format!("resident.{action}.g{group_id}"))
                    .await;
                Ok((fresh, kind))
            }
            Err(err) => {
                existing.close().await;
                for line in existing.drain_audit() {
                    self.audit(group_id, &line).await;
                }
                Err(err)
            }
        }
    }

    /// Close one group's session, if any.
    pub async fn close(&self, group_id: &str) {
        self.close_as(group_id, group_id).await;
    }

    /// Close session `key`, attributing the audit lines it drained to `group_id`.
    ///
    /// A scratch canary session has its own key but belongs to a real group, so
    /// its `resident.session.*` lines must land on the group an operator reads
    /// rather than on a `"<group>#canary"` id that no group row backs.
    pub async fn close_as(&self, key: &str, group_id: &str) {
        let mut sessions = self.sessions.lock().await;
        let removed = sessions.remove(key);
        drop(sessions);
        if let Some(session) = removed {
            session.close().await;
            for action in session.drain_audit() {
                self.audit(group_id, &action).await;
            }
        }
        self.paused_since.lock().await.remove(key);
    }

    /// Close every session. Used on shutdown only.
    pub async fn close_all(&self) {
        let drained: Vec<(String, Arc<ResidentSession>)> = {
            let mut sessions = self.sessions.lock().await;
            sessions.drain().collect()
        };
        for (group_id, session) in drained {
            session.close().await;
            for action in session.drain_audit() {
                self.audit(&group_id, &action).await;
            }
        }
    }

    /// Sweep: tick every live child, reclaim sessions whose group has been paused
    /// longer than [`crate::agent::resident::PAUSED_RECLAIM_SECONDS`], and drain
    /// the audit queue into `audit_events`.
    pub async fn sweep(&self, now: i64) {
        let snapshot: Vec<(String, Arc<ResidentSession>)> = {
            let sessions = self.sessions.lock().await;
            sessions
                .iter()
                .map(|(k, v)| (k.clone(), Arc::clone(v)))
                .collect()
        };
        let mut reclaim: Vec<String> = Vec::new();
        for (group_id, session) in snapshot {
            // A scratch canary session is not a group and is closed by the canary
            // itself; sweeping it would only try to look up a `groups` row that
            // cannot exist.
            if is_scratch_key(&group_id) {
                continue;
            }
            let paused = matches!(
                self.store.get_group(&self.bot_id, &group_id).await,
                Ok(Some(ref group)) if group.paused
            );
            if paused {
                let since = {
                    let mut marks = self.paused_since.lock().await;
                    *marks.entry(group_id.clone()).or_insert(now)
                };
                if session.reclaim_if_idle(Some(since), now).await {
                    reclaim.push(group_id.clone());
                }
            } else {
                self.paused_since.lock().await.remove(&group_id);
            }
            for action in session.drain_audit() {
                self.audit(&group_id, &action).await;
            }
        }
        for group_id in reclaim {
            self.close(&group_id).await;
            self.audit(&group_id, "resident.session.reclaimed").await;
        }
    }

    /// Number of live sessions. Used by tests and the status endpoint.
    pub async fn live_count(&self) -> usize {
        self.sessions.lock().await.len()
    }

    /// Write one audit row for this group. Public so the turn path can record the
    /// `resident.*` actions it owns (`turn.delivered`, `insert.*`, `probe.*`).
    pub async fn audit(&self, group_id: &str, action: &str) {
        let _ = self
            .store
            .audit(
                "system",
                action,
                Some(&self.bot_id),
                Some(group_id),
                crate::now_unix(),
            )
            .await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn a_canary_key_pins_the_real_group_id() {
        assert_eq!(real_group_id("g1"), "g1");
        assert_eq!(real_group_id("g1#canary"), "g1");
        assert_eq!(real_group_id(&canary_session_key("900000002")), "900000002");
    }

    #[tokio::test]
    async fn deliver_timeout_defaults_to_the_constant_and_takes_the_config_override() {
        let dir = tempfile::tempdir().expect("temp");
        let pool = crate::store::migrations::open_temp(dir.path())
            .await
            .expect("migrate");
        let manager = ResidentManager::new(
            Arc::new(SqliteStore::new(pool)),
            "bot".into(),
            dir.path().join("agy"),
            dir.path().to_path_buf(),
            "model".into(),
        );
        assert_eq!(manager.deliver_timeout_s, DELIVER_CONFIRM_TIMEOUT_SECONDS);
        let manager = manager.with_deliver_timeout_s(30);
        assert_eq!(manager.deliver_timeout_s, 30);
    }

    // The fake agy is a #!/bin/sh script: unix-only by construction.
    #[cfg(unix)]
    #[tokio::test]
    async fn concurrent_callers_share_one_pty() {
        let dir = tempfile::tempdir().expect("temp");
        let bin = dir.path().join("agy");
        std::fs::write(
            &bin,
            "#!/bin/sh\nsleep 0.4\nprintf '? for shortcuts\\n'\nwhile true; do sleep 1; done\n",
        )
        .unwrap();
        let mut perms = std::fs::metadata(&bin).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&bin, perms).unwrap();
        let pool = crate::store::migrations::open_temp(dir.path())
            .await
            .expect("migrate");
        let manager = ResidentManager::new(
            Arc::new(SqliteStore::new(pool)),
            "bot".into(),
            bin,
            dir.path().to_path_buf(),
            "model".into(),
        );
        let workspace = dir.path().join("ws");
        std::fs::create_dir_all(&workspace).unwrap();
        let (first, second) = tokio::join!(
            manager.session("g1", &workspace, None),
            manager.session("g1", &workspace, None),
        );
        let first = first.expect("leader");
        let second = second.expect("follower");
        assert_eq!(first.pid().await, second.pid().await);
        assert_eq!(manager.live_count().await, 1);
        assert!(!manager.spawn_in_flight("g1"));
        manager.close("g1").await;
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_canary_session_never_touches_the_groups_session() {
        let dir = tempfile::tempdir().expect("temp");
        let bin = dir.path().join("agy");
        std::fs::write(
            &bin,
            "#!/bin/sh\nsleep 0.4\nprintf '? for shortcuts\\n'\nwhile true; do sleep 1; done\n",
        )
        .unwrap();
        let mut perms = std::fs::metadata(&bin).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&bin, perms).unwrap();
        let pool = crate::store::migrations::open_temp(dir.path())
            .await
            .expect("migrate");
        let manager = ResidentManager::new(
            Arc::new(SqliteStore::new(pool)),
            "bot".into(),
            bin,
            dir.path().to_path_buf(),
            "model".into(),
        );
        let workspace = dir.path().join("ws");
        std::fs::create_dir_all(&workspace).unwrap();

        // The group's own session, resuming a stored conversation.
        let real = manager
            .session("g1", &workspace, Some("conv-g1".into()))
            .await
            .expect("real");
        let real_pid = real.pid().await.expect("pid");

        // The canary asks for a scratch key with no conversation. It must fork its
        // own process rather than reuse the group's.
        let key = canary_session_key("g1");
        let scratch = manager
            .session(&key, &workspace, None)
            .await
            .expect("scratch");
        assert_ne!(
            scratch.pid().await,
            Some(real_pid),
            "canary must not reuse the group's session"
        );
        assert_eq!(scratch.conversation_id().await, None);
        assert_eq!(manager.live_count().await, 2);

        // Closing the scratch key leaves the real session alive and untouched.
        manager.close_as(&key, "g1").await;
        assert_eq!(manager.live_count().await, 1);
        assert_eq!(real.pid().await, Some(real_pid));
        assert_eq!(real.conversation_id().await.as_deref(), Some("conv-g1"));

        // The sweep ignores scratch keys, so a live one is never looked up as a
        // group (no `groups` row could exist) and never reclaimed.
        let scratch = manager
            .session(&key, &workspace, None)
            .await
            .expect("scratch");
        assert!(
            scratch.state().await != crate::agent::resident::SessionState::Dead(String::new()),
            "scratch session should be live before the sweep"
        );
        manager.sweep(crate::now_unix()).await;
        assert_eq!(manager.live_count().await, 2);

        manager.close(&key).await;
        manager.close("g1").await;
        assert_eq!(manager.live_count().await, 0);
    }
}
