//! Inbound side: event intake, dedupe, batching and the flush tick.
//!
//! The intake is deliberately synchronous per event and cheap: it authenticates nothing
//! (the transport already did), it never talks to the agent, and it stores the message
//! body only while the message is pending. Everything else happens in [`flush_tick`].
//!
//! Two invariants worth stating because they are easy to break later:
//!
//! * a group that is disabled or paused **accumulates nothing** — the tick drops its
//!   pending rows instead of letting bodies pile up,
//! * a command is recognized *before* enqueueing, so `/gemini pause` never becomes part
//!   of a prompt and is never sent to the agent.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use serde_json::Value;

use crate::config::Config;
use crate::onebot::connection::FrameSink;
use crate::onebot::event::{RawEvent, RawMessage, RawMessageSegment, SegmentData};
use crate::onebot::normalize::{
    self, Command, IncomingMessage, Normalized, SkipReason, SpeakerRole, text_len,
};
use crate::pipeline::batcher::{
    BatchEntry, BatchLimits, FlushTrigger, should_flush, split_batches,
};
use crate::pipeline::dedupe::{Dedupe, Seen, dedupe_key};
use crate::pipeline::outbox::Outbox;
use crate::pipeline::scheduler::BatchJob;
use crate::store::SqliteStore;

/// How long the owner's private chat stays quiet before it becomes a turn.
const PRIVATE_QUIET_SECONDS: i64 = 2;

/// What one inbound frame turned into, for tests and counters.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IntakeOutcome {
    /// Stored as pending.
    Stored,
    /// Already seen (dedupe or unique constraint).
    Duplicate,
    /// An owner command was executed; carries the reply text.
    Command { reply: String },
    /// Skipped, with the reason.
    Skipped(&'static str),
}

/// The batching parameters, resolved from config once.
#[derive(Debug, Clone, Copy)]
pub struct IntakeLimits {
    pub quiet_seconds: i64,
    pub max_age_seconds: i64,
    pub batch: BatchLimits,
    /// Per-group cap on agent calls started per rolling hour; 0 disables it.
    pub hourly_call_budget: u32,
    /// Flood drain kill switch (see [`FLOOD_POOL_THRESHOLD`]).
    pub flood_enabled: bool,
}

impl IntakeLimits {
    pub fn from_config(config: &Config) -> Self {
        Self {
            quiet_seconds: config.quiet_window().as_secs() as i64,
            max_age_seconds: config.max_batch_age().as_secs() as i64,
            batch: BatchLimits {
                max_messages: config.file.max_batch_messages,
                max_chars: config.file.max_batch_chars,
                max_images: config.file.max_batch_images,
            },
            hourly_call_budget: config.file.hourly_call_budget,
            flood_enabled: config.file.flood_enabled,
        }
    }
}

/// Cooldown between ambient-initiated claims at the quiet pace tier:
/// a quiet group hears from her at most once per this window unless
/// someone addresses her.
const AMBIENT_QUIET_COOLDOWN_S: i64 = 600;

/// Fixed-range context for the addressed_only tier (2026-10-30,
/// user-set): a claimed turn carries the triggering message plus
/// at most this many messages above it. Older pending rows are
/// consumed at claim time, never silently accumulated as context.
const STRICT_CONTEXT_LOOKBACK: usize = 10;

/// Inbound event handler.
pub struct Intake {
    store: Arc<SqliteStore>,
    dedupe: Dedupe,
    outbox: Outbox,
    bot_id: String,
    owner_qq_id: String,
    limits: IntakeLimits,
    /// Last gate judgement per group: `(widest_id, score, judged_at)`.
    ///
    /// Cached so a burst that produced no new message is not re-scored every tick: the
    /// score is a statement about the text, and the text has not changed. Time alone is
    /// then handled by arithmetic in [`Self::burst_may_hand_over`], never by the model.
    pace: std::sync::Mutex<
        std::collections::HashMap<String, (i64, crate::app::preflight::BurstScore, i64)>,
    >,
    /// Last cadence action written to the audit log, per group.
    ///
    /// The tick runs twice a second, so writing every judgement would bury the table in
    /// duplicates; only a change of verdict is worth a row.
    last_cadence: std::sync::Mutex<std::collections::HashMap<String, String>>,
    /// Last time a budget deferral was audited, per group. In memory on purpose:
    /// losing it on restart costs at most one duplicate audit row.
    budget_marks: std::sync::Mutex<std::collections::HashMap<String, i64>>,
    /// Last time a repeated `wait.damper_*` row was audited, keyed by
    /// `(group_id, hold reason)`.
    ///
    /// A held batch is re-judged every tick, and a tick is 500 ms: without this the
    /// audit table collects two rows a second for as long as the hold lasts (the
    /// production incident wrote >1000 rows in one eight-minute episode). In memory on
    /// purpose — losing it on restart costs one extra row.
    damper_wait_marks: std::sync::Mutex<std::collections::HashMap<(String, String), i64>>,
    /// The bot's call names: a message whose text contains one counts
    /// as addressed to her (see `text_names_bot`). Empty in tests.
    call_names: Vec<String>,
    /// Cached relevance verdicts for ambient windows: group → (widest
    /// row id, verdict). A window is re-judged only when a new row widens
    /// it — the verdict is a statement about the text, and the text has
    /// not changed. In memory on purpose: losing it on restart costs one
    /// duplicate judge call.
    relevance_cache: std::sync::Mutex<
        std::collections::HashMap<String, (i64, crate::app::relevance::Relevance)>,
    >,
    /// Last time each group claimed an ambient-only batch (the quiet
    /// tier's cooldown clock). In memory on purpose, like the other marks.
    ambient_claims: std::sync::Mutex<std::collections::HashMap<String, i64>>,
    /// Last relevance audit line written per group, so a held window
    /// does not write a row every 500 ms tick.
    relevance_marks: std::sync::Mutex<std::collections::HashMap<String, String>>,
    /// Groups whose pool was drained while no claim could be served. The
    /// mark survives until the group's next claimed job, which carries
    /// `flooded` so the prompt can tell the model the older context is
    /// background. In memory on purpose: losing it on restart costs one
    /// missing prompt notice, nothing more.
    flood_marks: std::sync::Mutex<std::collections::HashSet<String>>,
    /// Stage-4 flood backgrounds, per group: the drained overflow compressed
    /// into a short brief, waiting for the group's next claimed job. Behind
    /// an `Arc` because the summary lands from a spawned task, after the
    /// draining tick has already returned. In memory on purpose: losing it
    /// on restart costs one missing background block, nothing more.
    flood_backgrounds: std::sync::Arc<std::sync::Mutex<std::collections::HashMap<String, String>>>,
    /// The Aux (hy3) client for the stage-4 summary bypass. `None`
    /// in tests and when the gateway is unconfigured, in which case a drain
    /// stages the script-truncated rendering directly.
    aux: Option<std::sync::Arc<dyn crate::integrations::aux_llm::AuxLlm>>,
    /// Per-turn memory digest, computed *inside* the pacing gate's wait.
    ///
    /// `None` when the Aux gateway is not configured: without a model there is no
    /// digest, and the turn falls back to the raw entry list exactly as before.
    digest: Option<Arc<crate::memory::DigestEngine>>,
    /// The quota pacing layer. `None` in tests and when the quota service is absent, in
    /// which case only the hourly fuse constrains claims — exactly the old behaviour.
    pacing: Option<Arc<crate::app::pacing::PacingEngine>>,
    /// The resident manager, holder of the no-submission replay marks.
    /// `None` in tests that do not arm marks: released batches then simply
    /// merge into later ones, as before the replay existed.
    resident: Option<Arc<crate::app::resident::ResidentManager>>,
    /// The conversation-shape layer: one person and the bot trading messages.
    ///
    /// Not optional on purpose. A group with nothing to damp is at `L0`, which makes
    /// every call here a no-op, so leaving it out would only mean two code paths to
    /// reason about instead of one.
    damper: crate::app::damper::Damper,
    /// In-flight turns, so flush does not claim rows a resident insert is about
    /// to eat. `None` in tests that never start a turn.
    running_turns: Option<Arc<crate::app::turn::RunningTurns>>,
    /// OneBot action channel for the discovery-time group info lookup.
    /// `None` in tests: discovery then only creates the row.
    connection: Option<crate::onebot::connection::ConnectionHandle>,
}

/// What the damper says about the batch that is about to be claimed.
///
/// Assembled here because only the intake knows the pending rows: the state machine
/// itself never touches the store.
struct DamperShape {
    verdict: crate::app::damper::Verdict,
    /// The sender this batch is mostly made of, by message count (ties: lowest id, so the
    /// answer is stable from tick to tick).
    primary: String,
    /// How many pending rows came from that sender — `L2` wants six before a round opens.
    pending_from_primary: usize,
    /// Whether anything in the batch addresses the bot directly.
    addressed: bool,
    /// Whether anything in the batch strictly addresses the bot
    /// (an @ or a reply to her) — the only trigger the
    /// addressed_only tier answers to.
    strict_addressed: bool,
    /// Ids of the strictly-addressed rows: the fixed-range
    /// context trim at claim time anchors on the first of them.
    strict_ids: std::collections::HashSet<i64>,
    /// Per row: who sent it, and whether that message addresses the bot.
    senders: HashMap<i64, (String, bool)>,
}

impl Intake {
    pub fn new(
        store: Arc<SqliteStore>,
        bot_id: String,
        owner_qq_id: String,
        limits: IntakeLimits,
    ) -> Self {
        Self {
            outbox: Outbox::new(Arc::clone(&store)),
            store,
            dedupe: Dedupe::new(crate::pipeline::dedupe::DEFAULT_CAPACITY),
            bot_id,
            owner_qq_id,
            limits,
            pace: std::sync::Mutex::new(std::collections::HashMap::new()),
            last_cadence: std::sync::Mutex::new(std::collections::HashMap::new()),
            budget_marks: std::sync::Mutex::new(std::collections::HashMap::new()),
            damper_wait_marks: std::sync::Mutex::new(std::collections::HashMap::new()),
            flood_marks: std::sync::Mutex::new(std::collections::HashSet::new()),
            call_names: Vec::new(),
            relevance_cache: std::sync::Mutex::new(std::collections::HashMap::new()),
            ambient_claims: std::sync::Mutex::new(std::collections::HashMap::new()),
            relevance_marks: std::sync::Mutex::new(std::collections::HashMap::new()),
            flood_backgrounds: std::sync::Arc::new(std::sync::Mutex::new(
                std::collections::HashMap::new(),
            )),
            aux: None,
            digest: None,
            pacing: None,
            resident: None,
            damper: crate::app::damper::Damper::new(),
            running_turns: None,
            connection: None,
        }
    }

    /// Attach the OneBot action channel (discovery-time group info).
    pub fn with_connection(
        mut self,
        connection: crate::onebot::connection::ConnectionHandle,
    ) -> Self {
        self.connection = Some(connection);
        self
    }

    /// Attach the quota pacing layer. Built outside so tests can leave it off.
    pub fn with_pacing(mut self, pacing: Arc<crate::app::pacing::PacingEngine>) -> Self {
        self.pacing = Some(pacing);
        self
    }

    /// Attach the bot's call names (a text mention counts as
    /// addressed). Built outside so tests can leave it empty.
    pub fn with_call_names(mut self, call_names: Vec<String>) -> Self {
        self.call_names = call_names;
        self
    }

    /// Attach the Aux (hy3) client for the flood-summary bypass
    /// (stage 4). Built outside so tests can leave it off: without it a
    /// drain still stages a background — the script-truncated rendering —
    /// it just never becomes a summary.
    pub fn with_aux(mut self, client: Arc<dyn crate::integrations::aux_llm::AuxLlm>) -> Self {
        self.aux = Some(client);
        self
    }

    /// The pacing engine, when one is configured.
    pub fn pacing(&self) -> Option<&Arc<crate::app::pacing::PacingEngine>> {
        self.pacing.as_ref()
    }

    /// Attach the digest engine. Built outside so tests can leave it off.
    pub fn with_digest(mut self, digest: Arc<crate::memory::DigestEngine>) -> Self {
        self.digest = Some(digest);
        self
    }

    /// Attach the in-flight turn registry. Flush skips a group that already has
    /// a turn running, so the resident insert path can claim those rows.
    pub fn with_running_turns(mut self, running: Arc<crate::app::turn::RunningTurns>) -> Self {
        self.running_turns = Some(running);
        self
    }

    /// Attach the resident manager so flush ticks can consume the
    /// no-submission replay marks the executor arms (2026-10-29).
    pub fn with_resident(mut self, resident: Arc<crate::app::resident::ResidentManager>) -> Self {
        self.resident = Some(resident);
        self
    }

    /// Consume a no-submission replay mark for this group, if one is armed
    /// and its rows are still waiting (2026-10-29). Claims exactly the
    /// marked rows as one exclusive job and returns the rows/entries the
    /// normal judgement should still see. The mark is cleared once its
    /// rows are claimed or proven gone (answered elsewhere, aged out,
    /// floored) — fail-closed: a mark never fires twice and never
    /// invents a batch. A lost claim race keeps the mark for the next
    /// tick and leaves the rows to the normal judgement.
    async fn take_retry_job(
        &self,
        group_id: &str,
        rows: Vec<crate::store::PendingEventRow>,
        entries: Vec<BatchEntry>,
        now: i64,
    ) -> (
        Vec<crate::store::PendingEventRow>,
        Vec<BatchEntry>,
        Option<BatchJob>,
    ) {
        let Some(resident) = &self.resident else {
            return (rows, entries, None);
        };
        let Some(marked) = resident.retry_mark(group_id).await else {
            return (rows, entries, None);
        };
        let marked_set: std::collections::HashSet<i64> = marked.iter().copied().collect();
        let marked_ids: Vec<i64> = entries
            .iter()
            .filter(|e| marked_set.contains(&e.id))
            .map(|e| e.id)
            .collect();
        if marked_ids.is_empty() {
            resident.clear_retry(group_id).await;
            let _ = self
                .store
                .audit(
                    "system",
                    &format!("resident.retry.missed.g{group_id}"),
                    Some(&self.bot_id),
                    Some(group_id),
                    now,
                )
                .await;
            return (rows, entries, None);
        }
        let batch_id = format!("b{group_id}-{now}-retry");
        let claimed = match self.store.claim_batch(&marked_ids, &batch_id).await {
            Ok(claimed) if !claimed.is_empty() => claimed,
            _ => return (rows, entries, None),
        };
        resident.clear_retry(group_id).await;
        let taken: std::collections::HashSet<i64> = claimed.iter().copied().collect();
        let _ = self
            .store
            .audit(
                "system",
                &format!("resident.retry.claimed.g{group_id}.n{}", taken.len()),
                Some(&self.bot_id),
                Some(group_id),
                now,
            )
            .await;
        let job = BatchJob {
            group_id: group_id.to_string(),
            batch_id,
            event_ids: claimed,
            privileged: entries
                .iter()
                .filter(|e| taken.contains(&e.id))
                .any(|e| e.is_privileged()),
            chars: entries
                .iter()
                .filter(|e| taken.contains(&e.id))
                .map(|e| e.chars)
                .sum(),
            images: entries
                .iter()
                .filter(|e| taken.contains(&e.id))
                .map(|e| e.images)
                .sum(),
            flooded: false,
            flood_background: None,
            retry: true,
        };
        let rows = rows
            .into_iter()
            .filter(|r| !taken.contains(&r.id))
            .collect();
        let entries = entries
            .into_iter()
            .filter(|e| !taken.contains(&e.id))
            .collect();
        (rows, entries, Some(job))
    }

    /// The digest engine, when one is configured. The turn picks its slots from here.
    pub fn digest_engine(&self) -> Option<&Arc<crate::memory::DigestEngine>> {
        self.digest.as_ref()
    }

    pub fn limits(&self) -> IntakeLimits {
        self.limits
    }

    /// Handle one raw frame value.
    ///
    /// Ordering matters: dedupe first (cheap, in memory), then the persistent unique
    /// constraint (survives restarts), then normalization, then the store.
    pub async fn handle(&self, value: &Value) -> IntakeOutcome {
        let event: RawEvent = match serde_json::from_value(value.clone()) {
            Ok(e) => e,
            Err(_) => return IntakeOutcome::Skipped(SkipReason::MissingField.as_str()),
        };
        // A poke of the bot is a notice, not a message; rewrite it into the
        // equivalent message event so it flows down the one path that exists.
        let event = rewrite_bot_poke(event, &self.bot_id);
        let event = crate::onebot::private::rewrite_owner_private(event, &self.owner_qq_id);

        // Membership notices are the second discovery channel: a group the
        // bot was just added to may not produce a message for a long time,
        // and a group it was removed from should be marked, not forgotten.
        if event.post_type.as_deref() == Some("notice") {
            self.handle_notice(&event).await;
            return IntakeOutcome::Skipped(SkipReason::NotMessage.as_str());
        }

        // Only group messages can be interesting; skip early without touching the store.
        if event.post_type.as_deref() != Some("message")
            || event.message_type.as_deref() != Some("group")
        {
            let skip = if event.post_type.as_deref() != Some("message") {
                SkipReason::NotMessage
            } else {
                SkipReason::PrivateMessage
            };
            return IntakeOutcome::Skipped(skip.as_str());
        }

        let message_id = match event.message_id.as_deref() {
            Some(m) if !m.is_empty() => m.to_string(),
            _ => return IntakeOutcome::Skipped(SkipReason::MissingField.as_str()),
        };
        let key = dedupe_key(&self.bot_id, &message_id);
        match self.dedupe.observe(&key) {
            Seen::Duplicate => return IntakeOutcome::Duplicate,
            Seen::New => {}
        }

        // Persistent dedupe: a restart must not re-process a message.
        let now = crate::now_unix();
        match self.store.mark_seen(&self.bot_id, &message_id, now).await {
            Ok(true) => {}
            Ok(false) => return IntakeOutcome::Duplicate,
            Err(_) => return IntakeOutcome::Skipped(SkipReason::MissingField.as_str()),
        }

        let (Some(group_id), Some(user_id)) = (event.group_id.clone(), event.user_id.clone())
        else {
            return IntakeOutcome::Skipped(SkipReason::MissingField.as_str());
        };

        // The group must exist before an alias can be allocated. A first-seen group is
        // created disabled and recorded once, so an operator can see it in the panel and
        // switch it on; nothing is stored or answered until they do.
        match self.store.ensure_group(&self.bot_id, &group_id, now).await {
            Ok(true) => {
                let _ = self
                    .store
                    .audit(
                        "system",
                        "group.discovered",
                        Some(&self.bot_id),
                        Some(&group_id),
                        now,
                    )
                    .await;
                if crate::onebot::private::private_user_id(&group_id).is_some() {
                    // The owner chose to talk here: nothing to discover or approve.
                    let _ = self
                        .store
                        .set_group_flags(
                            &self.bot_id,
                            &group_id,
                            Some(true),
                            Some(false),
                            None,
                            now,
                        )
                        .await;
                    let _ = self
                        .store
                        .set_group_info(&self.bot_id, &group_id, Some("主人私聊"), Some(2), now)
                        .await;
                } else {
                    self.spawn_group_info_fetch(group_id.clone());
                }
            }
            Ok(false) => {}
            Err(_) => return IntakeOutcome::Skipped(SkipReason::MissingField.as_str()),
        }
        let group = match self.store.get_group(&self.bot_id, &group_id).await {
            Ok(Some(g)) => g,
            _ => return IntakeOutcome::Skipped(SkipReason::MissingField.as_str()),
        };

        let speaker_ref = match self
            .store
            .ensure_member(&self.bot_id, &group_id, &user_id, now)
            .await
        {
            Ok(r) => r,
            Err(_) => return IntakeOutcome::Skipped(SkipReason::MissingField.as_str()),
        };

        let normalized =
            normalize::normalize_event(&event, &self.bot_id, &self.owner_qq_id, &speaker_ref, now);

        match normalized {
            Normalized::Skipped(reason) => IntakeOutcome::Skipped(reason.as_str()),
            Normalized::Command {
                command,
                speaker_role,
                ..
            } => {
                // Belt and braces: normalize already gated on owner, but the executor
                // must not rely on that.
                if speaker_role != SpeakerRole::Owner {
                    return IntakeOutcome::Skipped(SkipReason::Empty.as_str());
                }
                self.run_command(&group_id, command, &group).await
            }
            Normalized::Message(mut message) => {
                // A disabled or paused group keeps nothing.
                if !group.enabled || group.paused {
                    return IntakeOutcome::Skipped(SkipReason::GroupInactive.as_str());
                }
                self.resolve_mentions(&event, &mut message).await;
                self.store_message(&message).await
            }
        }
    }

    /// Handle a membership notice: `group_increase` discovers (or
    /// re-discovers) the group, `group_decrease` of the bot herself marks
    /// it departed. Everything else is ignored. Data is never deleted.
    async fn handle_notice(&self, event: &RawEvent) {
        let Some(group_id) = event.group_id.clone() else {
            return;
        };
        let now = crate::now_unix();
        match event.notice_type.as_deref() {
            Some("group_increase") => {
                match self.store.ensure_group(&self.bot_id, &group_id, now).await {
                    Ok(true) => {
                        let _ = self
                            .store
                            .audit(
                                "system",
                                "group.discovered",
                                Some(&self.bot_id),
                                Some(&group_id),
                                now,
                            )
                            .await;
                    }
                    Ok(false) => {
                        // Already known: rejoining after a departure
                        // clears the marker.
                        let _ = self
                            .store
                            .set_group_departed(&self.bot_id, &group_id, None, now)
                            .await;
                    }
                    Err(_) => return,
                }
                self.spawn_group_info_fetch(group_id);
            }
            Some("group_decrease") => {
                if event.user_id.as_deref() == Some(self.bot_id.as_str()) {
                    let _ = self
                        .store
                        .set_group_departed(&self.bot_id, &group_id, Some(now), now)
                        .await;
                    let _ = self
                        .store
                        .audit(
                            "system",
                            "group.departed",
                            Some(&self.bot_id),
                            Some(&group_id),
                            now,
                        )
                        .await;
                }
            }
            _ => {}
        }
    }

    /// Fetch the group's name and member count from OneBot, off the
    /// intake path: discovery never waits on the socket.
    fn spawn_group_info_fetch(&self, group_id: String) {
        let Some(connection) = self.connection.clone() else {
            return;
        };
        let store = Arc::clone(&self.store);
        let bot_id = self.bot_id.clone();
        tokio::spawn(async move {
            let Ok(group_num) = group_id.parse::<i64>() else {
                return;
            };
            let data = connection
                .call_data(
                    "get_group_info",
                    serde_json::json!({ "group_id": group_num }),
                    std::time::Duration::from_secs(10),
                )
                .await;
            if let Ok(data) = data {
                let name = data
                    .get("group_name")
                    .and_then(|v| v.as_str())
                    .map(str::to_string);
                let member_count = data.get("member_count").and_then(|v| v.as_i64());
                if name.is_some() || member_count.is_some() {
                    let _ = store
                        .set_group_info(
                            &bot_id,
                            &group_id,
                            name.as_deref(),
                            member_count,
                            crate::now_unix(),
                        )
                        .await;
                }
            }
        });
    }

    /// Fill in who each @ of another member points at.
    ///
    /// Normalize deliberately keeps raw QQ numbers out of the segment
    /// stream, so the targets are re-read from the raw event here, where
    /// the store can turn each into the alias + display name the agent is
    /// allowed to see. Alignment is positional: normalize never drops an
    /// `at` segment, so the unresolved `AtOther` segments line up
    /// one-to-one with the non-bot, non-`all` targets on the wire (@全体
    /// carries its name already and consumes no target). Afterwards the
    /// plain text is rendered again, so the stored text names the
    /// addressee (`@小鲸鱼 你咋还活着`) instead of the blank an unresolved
    /// segment leaves — the missing addressee is what made the model
    /// answer other people's teasing as if it were aimed at her
    /// (2026-11-03, a production group).
    async fn resolve_mentions(&self, event: &RawEvent, message: &mut IncomingMessage) {
        let mut targets: Vec<&str> = Vec::new();
        if let Some(RawMessage::Segments(raw)) = &event.message {
            for seg in raw {
                if seg.seg_type == "at"
                    && let Some(qq) = seg.data.as_ref().and_then(|d| d.qq.as_deref())
                    && qq != self.bot_id.as_str()
                    && qq != "all"
                    && !qq.is_empty()
                    && qq.bytes().all(|b| b.is_ascii_digit())
                {
                    targets.push(qq);
                }
            }
        }
        if targets.is_empty() {
            return;
        }
        let now = crate::now_unix();
        let mut next = targets.iter();
        let mut touched = false;
        for segment in message.segments.iter_mut() {
            let normalize::Segment::AtOther {
                target_ref,
                target_name,
            } = segment
            else {
                continue;
            };
            if target_ref.is_some() || target_name.is_some() {
                continue;
            }
            let Some(qq) = next.next() else { break };
            if let Ok(r) = self
                .store
                .ensure_member(&self.bot_id, &message.group_id, qq, now)
                .await
            {
                *target_ref = Some(r);
                touched = true;
            }
            if let Ok(Some(name)) = self
                .store
                .member_display_name(&self.bot_id, &message.group_id, qq)
                .await
            {
                *target_name = Some(name);
            }
        }
        if touched {
            message.text = normalize::plain_text(&message.segments);
        }
    }

    async fn store_message(&self, message: &IncomingMessage) -> IntakeOutcome {
        let payload = match serde_json::to_string(message) {
            Ok(p) => p,
            Err(_) => return IntakeOutcome::Skipped(SkipReason::MissingField.as_str()),
        };
        // Image references stay in a separate column so the prompt builder cannot
        // accidentally inline a URL.
        let image_refs: Vec<&str> = message
            .segments
            .iter()
            .filter_map(|s| match s {
                crate::onebot::normalize::Segment::Image { url: Some(u), .. } => Some(u.as_str()),
                _ => None,
            })
            .collect();
        let image_json = serde_json::to_string(&image_refs).unwrap_or_else(|_| "[]".to_string());
        let has_image = !image_refs.is_empty();
        // QQ flags a sticker with sub_type 1 and a picture with 0. The raw log keeps it
        // so the panel and the tagger can tell them apart without re-reading pixels.
        let sub_type = message.segments.iter().find_map(|s| match s {
            crate::onebot::normalize::Segment::Image { sub_type, .. } => sub_type.clone(),
            _ => None,
        });

        // The raw log is written before the queue: pending_events is deleted at the end
        // of a turn, so without this line nothing about the conversation would survive.
        // A failure here must not swallow the message, hence the ignored result.
        let _ = self
            .store
            .insert_group_message(
                &self.bot_id,
                &message.group_id,
                &message.message_id,
                &message.sender_id,
                Some(message.display_name.as_str()),
                message.speaker_role.as_str(),
                &message.text,
                has_image,
                None,
                sub_type.as_deref(),
                message.received_at,
            )
            .await;

        match self
            .store
            .insert_pending(
                &self.bot_id,
                &message.group_id,
                &message.message_id,
                &message.sender_id,
                message.received_at,
                &payload,
                &image_json,
            )
            .await
        {
            Ok(true) => IntakeOutcome::Stored,
            Ok(false) => IntakeOutcome::Duplicate,
            Err(_) => IntakeOutcome::Skipped(SkipReason::MissingField.as_str()),
        }
    }

    /// One flush pass over every known group.
    ///
    /// Returns the batches that were claimed and are ready for the scheduler. Each
    /// returned job's rows are already tagged with its `batch_id`, so a crash between
    /// here and the executor leaves them claimed but recoverable.
    /// Script-only release decision for the addressed_only tier
    /// (2026-10-30): no hy3 cadence score and no relevance judge
    /// may trigger or hold a turn at this tier. A burst is released
    /// once it has settled inside the quiet window, or when a hard
    /// cap forces release — the same ceilings as the gated path,
    /// so a burst in a continuously busy group cannot starve.
    /// Returns `(ready, ceiling)` like the gated decision.
    fn strict_release(
        &self,
        entries: &[BatchEntry],
        first_at: i64,
        last_activity: i64,
        now: i64,
    ) -> (bool, bool) {
        let total_chars: usize = entries.iter().map(|e| e.chars).sum();
        let total_images: usize = entries.iter().map(|e| e.images).sum();
        let forced = now - first_at >= self.limits.max_age_seconds
            || entries.len() >= self.limits.batch.max_messages
            || total_chars >= self.limits.batch.max_chars
            || total_images >= self.limits.batch.max_images;
        (
            forced || now - last_activity >= self.limits.quiet_seconds,
            forced,
        )
    }

    /// Trim a strict (addressed_only) claim to its fixed context
    /// range: the first strictly-addressed row plus up to
    /// [`STRICT_CONTEXT_LOOKBACK`] rows above it. Rows older than
    /// the range are deleted from the pending pool and audited —
    /// consumed, not carried. Without a strict row (unreachable
    /// through the gate) the entries pass through untouched.
    async fn trim_strict_context(
        &self,
        group_id: &str,
        entries: Vec<BatchEntry>,
        strict_ids: &std::collections::HashSet<i64>,
        now: i64,
    ) -> Vec<BatchEntry> {
        let Some(first_strict) = entries.iter().position(|e| strict_ids.contains(&e.id)) else {
            return entries;
        };
        let keep_from = first_strict.saturating_sub(STRICT_CONTEXT_LOOKBACK);
        if keep_from == 0 {
            return entries;
        }
        let dropped: Vec<i64> = entries[..keep_from].iter().map(|e| e.id).collect();
        let count = dropped.len();
        if self.store.delete_pending(&dropped).await.is_ok() {
            let _ = self
                .store
                .audit(
                    "system",
                    &format!("intake.addressed_only.context_trim.g{group_id}.n{count}"),
                    Some(&self.bot_id),
                    Some(group_id),
                    now,
                )
                .await;
        }
        entries.into_iter().skip(keep_from).collect()
    }

    /// The ambient gate: may this window become a turn at the group's
    /// pace tier? Addressed windows always may. Ambient-only windows
    /// follow [`crate::app::relevance::gate_decision`]: lively always,
    /// addressed_only never, normal/quiet by the relevance judge. The
    /// verdict is cached per widest row, and no judge configured (or a
    /// judge that cannot answer) reads as Unavailable, which claims —
    /// fail open, like every other gate in this pipeline.
    async fn ambient_gate(
        &self,
        group: &crate::store::GroupRecord,
        addressed: bool,
        rows: &[crate::store::PendingEventRow],
        widest: i64,
        now: i64,
    ) -> bool {
        let pace = group.pace();
        if addressed || pace == crate::store::AmbientPace::Lively {
            return true;
        }
        let group_id = &group.group_id;
        if pace == crate::store::AmbientPace::AddressedOnly {
            self.audit_relevance(group_id, widest, "deferred.addressed_only", now)
                .await;
            return false;
        }
        let relevance = match self
            .relevance_cache
            .lock()
            .ok()
            .and_then(|cache| cache.get(group_id).copied())
        {
            Some((w, verdict)) if w == widest => verdict,
            _ => {
                let verdict = self.judge_relevance(group_id, rows).await;
                if let Ok(mut cache) = self.relevance_cache.lock() {
                    cache.insert(group_id.clone(), (widest, verdict));
                }
                self.audit_relevance(
                    group_id,
                    widest,
                    &format!("judged.{}", verdict.as_str()),
                    now,
                )
                .await;
                verdict
            }
        };
        let cooldown_ready = match self.ambient_claims.lock() {
            Ok(claims) => claims
                .get(group_id)
                .is_none_or(|last| now - last >= AMBIENT_QUIET_COOLDOWN_S),
            Err(_) => true,
        };
        let claim = crate::app::relevance::gate_decision(pace, false, relevance, cooldown_ready);
        if claim {
            if let Ok(mut claims) = self.ambient_claims.lock() {
                claims.insert(group_id.clone(), now);
            }
        } else {
            let reason = if relevance == crate::app::relevance::Relevance::Unrelated {
                "deferred.unrelated"
            } else {
                "deferred.quiet_cooldown"
            };
            self.audit_relevance(group_id, widest, reason, now).await;
        }
        claim
    }

    /// Ask the relevance judge about one ambient window. Without a
    /// Aux client (tests, gateway unconfigured) the answer is
    /// Unavailable, which the gate treats as claimable.
    async fn judge_relevance(
        &self,
        group_id: &str,
        rows: &[crate::store::PendingEventRow],
    ) -> crate::app::relevance::Relevance {
        let Some(client) = self.aux.as_ref() else {
            return crate::app::relevance::Relevance::Unavailable;
        };
        let messages: Vec<crate::onebot::normalize::IncomingMessage> = rows
            .iter()
            .filter_map(|r| serde_json::from_str(&r.normalized_payload).ok())
            .collect();
        if messages.is_empty() {
            return crate::app::relevance::Relevance::Unavailable;
        }
        let pending_ids: std::collections::HashSet<String> =
            messages.iter().map(|m| m.message_id.clone()).collect();
        let members = self
            .store
            .list_members(&self.bot_id, group_id)
            .await
            .unwrap_or_default();
        let recent_rows = self
            .store
            .recent_unarchived_messages(
                &self.bot_id,
                group_id,
                crate::app::preflight::PREFLIGHT_CONTEXT_MESSAGES,
            )
            .await
            .unwrap_or_default();
        let recent: Vec<(String, String)> = recent_rows
            .into_iter()
            .filter(|m| !pending_ids.contains(&m.message_id))
            .map(|m| {
                let who = if m.speaker_role == "bot" {
                    "me".to_string()
                } else {
                    members
                        .iter()
                        .find(|mem| mem.user_id == m.sender_id)
                        .map(|mem| mem.speaker_ref.clone())
                        .unwrap_or_else(|| "群".to_string())
                };
                (who, m.text)
            })
            .collect();
        crate::app::relevance::RelevanceJudge::new(std::sync::Arc::clone(client))
            .judge(&self.bot_id, group_id, &messages, &recent)
            .await
    }

    /// Write a relevance audit row at most once per (group, widest,
    /// line): a held window is re-evaluated every tick and must not
    /// flood the table (the same reasoning as the damper wait marks).
    async fn audit_relevance(&self, group_id: &str, widest: i64, line: &str, now: i64) {
        let key = format!("{widest}.{line}");
        if let Ok(mut marks) = self.relevance_marks.lock() {
            if marks.get(group_id) == Some(&key) {
                return;
            }
            marks.insert(group_id.to_string(), key);
        }
        let _ = self
            .store
            .audit(
                "system",
                &format!("intake.relevance.g{group_id}.{line}"),
                Some(&self.bot_id),
                Some(group_id),
                now,
            )
            .await;
    }

    pub async fn flush_tick(
        &self,
        now: i64,
        gate: Option<&crate::app::preflight::Preflight>,
    ) -> Vec<BatchJob> {
        let groups = match self.store.list_groups(&self.bot_id).await {
            Ok(g) => g,
            Err(_) => return Vec::new(),
        };
        let mut jobs = Vec::new();
        // The quota tier is global (one account for every group), so it is observed once per
        // tick and then handed to each group as a cap. The group list is the one this tick
        // already walks, so a paused group cannot take weight away from a live one and the
        // engine does not need a second query. `None` when pacing is off: the fuse is then the
        // only constraint, which is the pre-pacing behaviour.
        let pacing_report = match &self.pacing {
            Some(engine) => {
                let live: Vec<String> = groups
                    .iter()
                    .filter(|g| g.enabled && !g.paused)
                    .map(|g| g.group_id.clone())
                    .collect();
                Some(engine.observe(now, &live).await)
            }
            None => None,
        };
        for group in groups {
            // Inactive groups hold nothing at all.
            if !group.enabled || group.paused {
                let _ = self.store.drop_pending(&self.bot_id, &group.group_id).await;
                continue;
            }
            let rows = match self
                .store
                .pending_for_group(&self.bot_id, &group.group_id)
                .await
            {
                Ok(r) if !r.is_empty() => r,
                _ => continue,
            };
            // A turn is already eating this group's unclaimed rows via insert.
            // Claiming them here would enqueue a follow-up job on the same
            // events (claim_batch used to return Ok even when it took 0 rows).
            if self
                .running_turns
                .as_ref()
                .is_some_and(|rt| rt.contains(&group.group_id))
            {
                continue;
            }

            // Age guardrail first: rows too old to answer are consumed here,
            // before the budget or the gate spend anything on them. What
            // survives is fresh enough that claiming it cannot produce a
            // minutes-late reply to a topic the group already left behind.
            let rows = self.apply_age_guardrail(&group.group_id, rows, now).await;
            if rows.is_empty() {
                continue;
            }

            // Quota pacing's privilege floor (T2: @ or owner only, T3: owner only). Rows
            // below the floor are consumed here without a reply — silence is the intended
            // outcome when the account cannot pay, not a failure — and audited the same way
            // a stale drop is: delete first, then record, so the row never exists in both.
            let floor = pacing_report
                .as_ref()
                .and_then(|r| crate::app::pacing::privilege_floor(r.tier).map(|f| (f, r.tier)));
            let (rows, entries) = match floor {
                None => {
                    let entries = entries_from_rows(&rows);
                    (rows, entries)
                }
                Some((floor, tier)) => {
                    let entries = entries_from_rows(&rows);
                    let (keep, drop): (Vec<_>, Vec<_>) = entries
                        .into_iter()
                        .partition(|e| crate::app::pacing::entry_clears(floor, e));
                    if !drop.is_empty() {
                        let ids: Vec<i64> = drop.iter().map(|e| e.id).collect();
                        let count = ids.len();
                        if self.store.delete_pending(&ids).await.is_ok() {
                            let _ = self
                                .store
                                .audit(
                                    "system",
                                    &format!(
                                        "pacing.drop.g{}.n{count}.tier{}",
                                        group.group_id,
                                        tier.as_str()
                                    ),
                                    Some(&self.bot_id),
                                    Some(&group.group_id),
                                    now,
                                )
                                .await;
                        }
                    }
                    if keep.is_empty() {
                        continue;
                    }
                    let kept: std::collections::HashSet<i64> = keep.iter().map(|e| e.id).collect();
                    let rows = rows.into_iter().filter(|r| kept.contains(&r.id)).collect();
                    (rows, keep)
                }
            };

            // The hourly budget is checked before anything else spends effort on
            // this group: a deferred batch is never scored by the gate and never
            // claimed — it stays pending until calls age out of the window.
            let oldest_age = now - rows.first().map(|r| r.received_at).unwrap_or(now);
            let pool_privileged = entries.iter().any(|e| e.is_privileged());
            let Some((fuse_claims, calls_in_window)) = self
                .budget_gate(&group.group_id, now, oldest_age, pool_privileged)
                .await
            else {
                // The fuse is blown outright — the main flood case. The
                // pool still drains (see `drain_pool_if_flooded`).
                self.drain_pool_if_flooded(&group.group_id, rows, now).await;
                continue;
            };
            // Two layers, strictest wins: the fuse is a safety limit on calls per rolling
            // hour, pacing is an affordance limit on the same hour. Pacing can only
            // tighten. The tier speaks in caps, so subtract what the hour already spent —
            // otherwise a group at its cap would still get one batch per tick.
            let max_claims = match pacing_report
                .as_ref()
                .and_then(|r| r.cap_for(&group.group_id))
            {
                Some(cap) => fuse_claims.min(cap.saturating_sub(calls_in_window.max(0) as usize)),
                None => fuse_claims,
            };
            if max_claims == 0 {
                // Pacing capped this group to zero claims this tick: same
                // drain duty as the blown-fuse path above.
                self.drain_pool_if_flooded(&group.group_id, rows, now).await;
                continue;
            }

            // No-submission replay (2026-10-29): an armed mark claims its
            // rows as one exclusive job ahead of the cadence judgement —
            // they already waited through a whole turn that never started,
            // and folding them into a later batch is how they used to lose
            // their reply. The replay spends from the same claim budget.
            let (rows, entries, retry_job) = self
                .take_retry_job(&group.group_id, rows, entries, now)
                .await;
            let mut claims_spent = 0usize;
            if let Some(job) = retry_job {
                jobs.push(job);
                claims_spent = 1;
            }
            if rows.is_empty() {
                continue;
            }
            let max_claims = max_claims.saturating_sub(claims_spent);
            if max_claims == 0 {
                continue;
            }

            let first_at = rows.first().map(|r| r.received_at).unwrap_or(now);
            let last_activity = rows.iter().map(|r| r.received_at).max().unwrap_or(now);
            let widest = rows.last().map(|r| r.id).unwrap_or(0);

            // The conversation-shape layer sees the same rows every tick until they are
            // claimed, which is why it dedupes by row id. It runs before the release
            // decision because a shape is a fact about the window, not about this batch.
            let shape = self.observe_shape(&group.group_id, now, &rows).await;

            // `ceiling` says whether this release was forced by a hard cap rather than
            // earned from the gate or the quiet window; the damper's soft holds defer to
            // it, exactly as a cadence wait does.
            //
            // The addressed_only tier runs on script alone (2026-10-30):
            // its release decision, its damper exemption and its
            // ambient gate all read the strict trigger (@ / reply to
            // her), and neither the hy3 cadence gate nor the relevance
            // judge takes part.
            let strict_mode = group.pace() == crate::store::AmbientPace::AddressedOnly;
            let effective_addressed = if strict_mode {
                shape.strict_addressed
            } else {
                shape.addressed
            };
            let (ready, ceiling) = if strict_mode {
                self.strict_release(&entries, first_at, last_activity, now)
            } else if crate::onebot::private::private_user_id(&group.group_id).is_some() {
                // One speaker and no room to read: a short fixed window is all the
                // pacing this needs, and it saves the cadence model's round trip.
                let trigger = should_flush(
                    &entries,
                    first_at,
                    last_activity,
                    now,
                    PRIVATE_QUIET_SECONDS,
                    self.limits.max_age_seconds,
                    self.limits.batch,
                );
                (
                    trigger.is_flush(),
                    matches!(trigger, FlushTrigger::Full | FlushTrigger::MaxAge),
                )
            } else {
                match gate {
                    // Without a gate there is nothing to pace on, so the fixed windows still
                    // apply: quiet, hard age, privileged, or a full batch.
                    None => {
                        let trigger = should_flush(
                            &entries,
                            first_at,
                            last_activity,
                            now,
                            self.limits.quiet_seconds,
                            self.limits.max_age_seconds,
                            self.limits.batch,
                        );
                        // Full and aged-out are this path's ceilings: the same promise that
                        // nothing can be starved forever, expressed without a model.
                        (
                            trigger.is_flush(),
                            matches!(trigger, FlushTrigger::Full | FlushTrigger::MaxAge),
                        )
                    }
                    Some(gate) => {
                        // A burst younger than the floor is never released, whatever the score
                        // says: two messages typed back to back must land in one batch, and a
                        // score of 0 on the first one cannot know that the second is coming.
                        // See `MIN_BURST_SECONDS` for the replay numbers behind the value.
                        if now - first_at < crate::app::preflight::MIN_BURST_SECONDS {
                            self.record_cadence(&group.group_id, "warmup", now).await;
                            (false, false)
                        } else {
                            // Hard ceilings run first and never consult the model. They exist
                            // only so a gate that is down, slow or confused cannot starve the
                            // agent forever; on a healthy day the model answers long before.
                            //
                            // Every batch dimension is checked, not just the message count:
                            // four pictures and a few words are already over budget long
                            // before the fifty-message limit, and missing that would let the
                            // gate hand over a batch that `split_batches` then has to cut.
                            let total_chars: usize = entries.iter().map(|e| e.chars).sum();
                            let total_images: usize = entries.iter().map(|e| e.images).sum();
                            let at_ceiling = now - first_at >= self.limits.max_age_seconds
                                || entries.len() >= self.limits.batch.max_messages
                                || total_chars >= self.limits.batch.max_chars
                                || total_images >= self.limits.batch.max_images;
                            if at_ceiling {
                                self.record_cadence(&group.group_id, "ceiling", now).await;
                                (true, true)
                            } else {
                                let (ready, reason) = self
                                    .burst_may_hand_over(gate, &group.group_id, &rows, widest, now)
                                    .await;
                                let verb = if ready { "hand_over" } else { "wait" };
                                self.record_cadence(
                                    &group.group_id,
                                    &format!("{verb}.{reason}"),
                                    now,
                                )
                                .await;
                                (ready, false)
                            }
                        }
                    }
                }
            };
            if !ready {
                continue;
            }
            // Damping never drops a message and never turns a batch into a failure: a
            // held batch stays pending and the next tick asks again, which is exactly the
            // cadence-wait contract. Hard holds (the per-sender cap, `L3`) survive the
            // ceilings because they *are* the cap; soft ones (quiet, thin) do not, because
            // the ceilings exist so nothing can starve the lane forever.
            //
            // No observed sender means no shape to judge (a payload that would not parse):
            // fail open and claim, because a rule that cannot see its input must not hold.
            if !shape.senders.is_empty()
                && let Some(hold) = shape.verdict.hold(
                    &shape.primary,
                    effective_addressed,
                    now - last_activity,
                    shape.pending_from_primary,
                )
                && (hold.hard || !ceiling)
            {
                self.record_damper_wait(&group.group_id, hold.reason, now)
                    .await;
                continue;
            }
            // Ambient gate: a window nothing in which is addressed to
            // her becomes a turn only at the group's pace tier (see
            // [`Self::ambient_gate`]). Addressed windows skip it entirely.
            // A deferral keeps the rows pending, exactly like a cadence
            // wait, so an addressed message arriving later claims the
            // window — at the addressed_only tier only its fixed-range
            // tail (see the trim below), not the whole pool.
            if !self
                .ambient_gate(&group, effective_addressed, &rows, widest, now)
                .await
            {
                continue;
            }
            // Once handed over, this group starts a fresh judgement next time.
            if let Ok(mut pace) = self.pace.lock() {
                pace.remove(&group.group_id);
            }

            // addressed_only: the claimed context is a fixed range —
            // the triggering message and the messages above it, never
            // the whole silently-accumulated pool.
            let entries = if strict_mode {
                self.trim_strict_context(&group.group_id, entries, &shape.strict_ids, now)
                    .await
            } else {
                entries
            };

            let chunks = split_batches(&entries, self.limits.batch);
            // Budget truncation: when the fuse leaves room for fewer batches
            // than the pool splits into, serve the newest segments and consume
            // the older ones (audited) — a group with one call left must answer
            // its freshest topic, not stall in silence behind a backlog it can
            // never afford to work through oldest-first.
            let claim_from = chunks.len().saturating_sub(max_claims);
            if claim_from > 0 {
                let dropped: Vec<i64> = chunks[..claim_from]
                    .iter()
                    .flat_map(|chunk| chunk.iter().map(|e| e.id))
                    .collect();
                let count = dropped.len();
                if self.store.delete_pending(&dropped).await.is_ok() {
                    let _ = self
                        .store
                        .audit(
                            "system",
                            &format!("scheduler.budget.trim.g{}.n{count}", group.group_id),
                            Some(&self.bot_id),
                            Some(&group.group_id),
                            now,
                        )
                        .await;
                }
            }
            // Jobs claimed after a drain (this tick's budget trim, or a
            // drain on an earlier tick while the fuse was blown) carry the
            // flood flag: older messages were consumed as background, and
            // the prompt must say so or she will try to answer them all.
            let mut flood_background = self.take_flood_background(&group.group_id);
            if strict_mode {
                // A summary of drained ambient chatter is unbounded
                // lurking context; the strict tier's fixed range
                // replaces it, so the brief is consumed and dropped.
                flood_background = None;
            }
            let flooded = self.take_flood_mark(&group.group_id)
                || claim_from > 0
                || flood_background.is_some();
            for (index, chunk) in chunks.into_iter().enumerate().skip(claim_from) {
                let ids: Vec<i64> = chunk.iter().map(|e| e.id).collect();
                let batch_id = format!("b{}-{}-{}", group.group_id, now, index);
                let ids = match self.store.claim_batch(&ids, &batch_id).await {
                    Ok(claimed) if !claimed.is_empty() => claimed,
                    _ => continue,
                };
                // The round is recorded per claimed chunk, against whoever that chunk was
                // mostly made of: that is what "the bot has answered this person n times in
                // a row" counts, and it is only true once the batch is actually claimed.
                let mut reply_counts: HashMap<&str, usize> = HashMap::new();
                let mut chunk_addressed = false;
                for id in &ids {
                    if let Some((sender, addressed)) = shape.senders.get(id) {
                        *reply_counts.entry(sender.as_str()).or_insert(0) += 1;
                        chunk_addressed |= *addressed;
                    }
                }
                let best = reply_counts.values().copied().max().unwrap_or(0);
                if let Some(sender) = reply_counts
                    .iter()
                    .filter(|(_, count)| **count == best)
                    .map(|(sender, _)| (*sender).to_string())
                    .min()
                {
                    let events =
                        self.damper
                            .note_reply(&group.group_id, now, &sender, chunk_addressed);
                    self.audit_damper_events(&group.group_id, now, &events)
                        .await;
                }
                jobs.push(BatchJob {
                    group_id: group.group_id.clone(),
                    batch_id,
                    event_ids: ids,
                    privileged: chunk.iter().any(|e| e.is_privileged()),
                    chars: chunk.iter().map(|e| e.chars).sum(),
                    images: chunk.iter().map(|e| e.images).sum(),
                    flooded,
                    flood_background: flood_background.take(),
                    retry: false,
                });
            }
        }
        jobs
    }
}

/// Build flush inputs from stored rows.
fn entries_from_rows(rows: &[crate::store::PendingEventRow]) -> Vec<BatchEntry> {
    rows.iter()
        .map(|row| {
            let message: Option<IncomingMessage> =
                serde_json::from_str(&row.normalized_payload).ok();
            let (chars, mentions_bot, from_owner) = match &message {
                Some(m) => (
                    text_len(&m.segments),
                    m.mentions_bot,
                    m.speaker_role == SpeakerRole::Owner,
                ),
                None => (0, false, false),
            };
            let images = message
                .as_ref()
                .map(|m| {
                    m.segments
                        .iter()
                        .filter(|s| matches!(s, crate::onebot::normalize::Segment::Image { .. }))
                        .count()
                })
                .unwrap_or(0);
            BatchEntry {
                id: row.id,
                chars,
                images,
                mentions_bot,
                from_owner,
                received_at: row.received_at,
            }
        })
        .collect()
}

#[async_trait::async_trait]
impl FrameSink for Intake {
    async fn on_event(&self, value: Value) {
        let _ = self.handle(&value).await;
    }
}

/// Rolling window the hourly call budget counts over, in seconds.
pub const BUDGET_WINDOW_SECONDS: i64 = 3_600;

/// Calls per window held back for batches that carry a direct @-mention:
/// ambient batches may spend at most `budget − reserve`, so a direct
/// question can still be claimed after ambient traffic has had its fill.
/// Only applies when the budget is larger than the reserve itself.
pub const BUDGET_PRIORITY_RESERVE: i64 = 15;

/// A pending row older than this is stale: when anything newer exists in the
/// pool it is consumed without a reply, because a reply to it would land
/// minutes after the group moved on (the stale-batch failure mode).
pub const STALE_ROW_SECONDS: i64 = 600;

/// Hard cap on one stage-4 hy3 summary call: past this the background
/// falls open to the script-truncated rendering.
pub const FLOOD_SUMMARY_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(3);

/// Pinned system prompt for the flood-summary bypass. Byte-identical on
/// every call so the gateway prefix cache stays warm (the same discipline
/// as the memory-compaction prompt). The model only ever compresses here.

/// Minimum seconds between two `scheduler.budget.defer` audit rows per group.
pub const BUDGET_AUDIT_THROTTLE_SECONDS: i64 = 600;

/// Minimum seconds between two `intake.cadence.wait.damper_*` audit rows for the same
/// `(group, reason)` pair.
///
/// A hold lasts until its window opens (tens of seconds to minutes) and the tick asks
/// twice a second, so unthrottled the rows describe nothing new. One per minute keeps
/// "still held, still for this reason" visible while leaving the edge rows readable.
pub const DAMPER_WAIT_AUDIT_INTERVAL_SECONDS: i64 = 60;

/// Whether a repeated `wait.damper_*` row may be written at `now`, given the last time
/// this `(group, reason)` was written.
///
/// The first observation is always recorded; afterwards a row is due once
/// [`DAMPER_WAIT_AUDIT_INTERVAL_SECONDS`] has passed. A clock that stands still or runs
/// backwards simply records nothing new.
/// Rewrite a group poke of the bot into a synthetic message event.
///
/// OneBot delivers pokes as `notice`/`notify` frames with `sub_type="poke"`,
/// a `user_id` (the poker) and a `target_id` (the poked). Only pokes of the
/// bot in a group are rewritten — poking another member is ambient noise,
/// and private pokes have no group to answer in. The synthetic message is
/// an @ plus the marker text, so downstream it is indistinguishable from
/// someone @-ing the bot and writing "戳了戳你": mention flag, gate,
/// batching and dedupe (via the synthetic id) all come for free.
fn rewrite_bot_poke(event: RawEvent, bot_id: &str) -> RawEvent {
    let is_poke = event.post_type.as_deref() == Some("notice")
        && event.notice_type.as_deref() == Some("notify")
        && event.sub_type.as_deref() == Some("poke");
    if !is_poke {
        return event;
    }
    let (Some(group_id), Some(user_id), Some(target_id)) = (
        event.group_id.clone(),
        event.user_id.clone(),
        event.target_id.clone(),
    ) else {
        return event;
    };
    if target_id != bot_id || user_id == bot_id {
        return event;
    }
    let stamp = event.time.unwrap_or_default();
    RawEvent {
        post_type: Some("message".to_string()),
        message_type: Some("group".to_string()),
        message_id: Some(format!("poke-{group_id}-{user_id}-{stamp}")),
        message: Some(RawMessage::Segments(vec![
            RawMessageSegment {
                seg_type: "at".to_string(),
                data: Some(SegmentData {
                    qq: Some(bot_id.to_string()),
                    ..Default::default()
                }),
            },
            RawMessageSegment {
                seg_type: "text".to_string(),
                data: Some(SegmentData {
                    text: Some("戳了戳你".to_string()),
                    ..Default::default()
                }),
            },
        ])),
        sender: None,
        ..event
    }
}

pub fn should_record_damper_wait(last: Option<i64>, now: i64) -> bool {
    match last {
        None => true,
        Some(at) => now - at >= DAMPER_WAIT_AUDIT_INTERVAL_SECONDS,
    }
}

/// Whether `calls` calls inside the window meet or exceed `budget`. A budget
/// of 0 disables the fuse, whatever the count.
pub fn budget_exhausted(calls: i64, budget: u32) -> bool {
    budget > 0 && calls >= i64::from(budget)
}

/// Flush triggers, exposed for tests.
pub fn trigger_for(
    entries: &[BatchEntry],
    first_at: i64,
    last_activity: i64,
    now: i64,
    limits: IntakeLimits,
) -> FlushTrigger {
    should_flush(
        entries,
        first_at,
        last_activity,
        now,
        limits.quiet_seconds,
        limits.max_age_seconds,
        limits.batch,
    )
}

mod cadence;
mod commands;
mod flood;
mod gates;

use self::flood::{
    FLOOD_BACKGROUND_MAX_CHARS, FLOOD_LINE_CHARS, FLOOD_LIVE_MAX, FLOOD_OVERFLOW_MAX_CHARS,
    FLOOD_POOL_THRESHOLD, FLOOD_SUMMARY_SYSTEM, flood_summary_prompt, render_overflow_text,
    resolve_flood_background, truncate_background,
};

#[cfg(test)]
mod tests;
