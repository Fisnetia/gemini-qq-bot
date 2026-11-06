//! Cadence and damper bookkeeping for Intake: verdict audits, wait
//! recording, burst shape observation and hand-over checks. Moved out
//! of intake.rs verbatim (std-split-intake); logic unchanged.

use super::*;

impl Intake {
    /// Write a cadence verdict to the audit log, but only when it changed.
    ///
    /// Without this the tick would insert two rows per group per second, which turns the
    /// audit table into a heartbeat and buries the events worth reading. With it, the log
    /// answers "why did this burst wait eight seconds" exactly once per episode.
    pub(super) async fn record_cadence(&self, group_id: &str, action: &str, now: i64) {
        let changed = match self.last_cadence.lock() {
            Ok(mut seen) => match seen.get(group_id) {
                Some(previous) if previous == action => false,
                _ => {
                    seen.insert(group_id.to_string(), action.to_string());
                    true
                }
            },
            Err(_) => false,
        };
        if changed {
            let _ = self
                .store
                .audit(
                    "system",
                    &format!("intake.cadence.{action}"),
                    Some(&self.bot_id),
                    Some(group_id),
                    now,
                )
                .await;
        }
    }

    /// Write one `wait.damper_*` row, at most once per
    /// [`DAMPER_WAIT_AUDIT_INTERVAL_SECONDS`] per `(group, reason)`.
    ///
    /// `record_cadence` alone cannot cover this: a held batch also produces a fresh
    /// `wait.*` / `hand_over.*` verdict every tick, so the per-group "last action" keeps
    /// flipping and every flip looks like a change. Throttling by time is what actually
    /// bounds the rows. Edge events (`enter.*`, `reset.*`, `cap.*`) go through
    /// [`Self::audit_damper_events`] and are never throttled: they fire once each.
    pub(super) async fn record_damper_wait(&self, group_id: &str, reason: &str, now: i64) {
        let allowed = match self.damper_wait_marks.lock() {
            Ok(mut marks) => {
                let key = (group_id.to_string(), reason.to_string());
                if should_record_damper_wait(marks.get(&key).copied(), now) {
                    marks.insert(key, now);
                    true
                } else {
                    false
                }
            }
            Err(_) => false,
        };
        if !allowed {
            return;
        }
        let _ = self
            .store
            .audit(
                "system",
                &format!("intake.cadence.wait.damper_{reason}"),
                Some(&self.bot_id),
                Some(group_id),
                now,
            )
            .await;
    }

    /// Fold this tick's pending rows into the damper's window and report the shape.
    ///
    /// The same rows are offered again every tick until they are claimed, which is why the
    /// state machine dedupes by row id: calling this unconditionally is safe, even for a
    /// group nowhere near a claim.
    ///
    /// Every input it cannot make sense of (a payload that will not parse, a failed query)
    /// counts as "not addressed" rather than as an error: the damper must never be able to
    /// hold a batch because something broke.
    pub(super) async fn observe_shape(
        &self,
        group_id: &str,
        now: i64,
        rows: &[crate::store::PendingEventRow],
    ) -> DamperShape {
        // One query per distinct quoted id per tick: a burst that quotes the same bot
        // message twice must not pay for it twice.
        let mut quoted_by_bot: HashMap<String, bool> = HashMap::new();
        let mut senders: HashMap<i64, (String, bool)> = HashMap::new();
        let mut spans: Vec<(i64, i64, bool, bool)> = Vec::new();
        let mut any_addressed = false;
        let mut any_strict = false;
        let mut strict_ids: std::collections::HashSet<i64> = std::collections::HashSet::new();
        for row in rows {
            let Ok(message) = serde_json::from_str::<IncomingMessage>(&row.normalized_payload)
            else {
                continue;
            };
            let from_owner = message.speaker_role == SpeakerRole::Owner;
            // Strict addressing first: an @, or a reply to something
            // the bot wrote. Both are structural — no model decides
            // them — which is what the addressed_only tier requires.
            let mut strict = message.mentions_bot;
            if !strict && let Some(quoted) = message.quote_of.as_deref() {
                let hit = match quoted_by_bot.get(quoted) {
                    Some(hit) => *hit,
                    None => {
                        let hit = self.quote_targets_bot(group_id, quoted).await;
                        quoted_by_bot.insert(quoted.to_string(), hit);
                        hit
                    }
                };
                strict = hit;
            }
            // Full addressing widens strict with the owner and a
            // text name-call; it is what exempts a batch at the
            // other tiers.
            let mut addressed = strict || from_owner;
            if !addressed && text_names_bot(&message.text, &self.call_names) {
                addressed = true;
            }
            any_addressed |= addressed;
            any_strict |= strict;
            if strict {
                strict_ids.insert(row.id);
            }
            senders.insert(row.id, (message.sender_id, addressed));
            spans.push((row.id, row.received_at, addressed, from_owner));
        }
        let observations: Vec<crate::app::damper::Observation<'_>> = spans
            .iter()
            .filter_map(|(id, at, addressed, from_owner)| {
                let (sender, _) = senders.get(id)?;
                Some(crate::app::damper::Observation {
                    id: *id,
                    sender: sender.as_str(),
                    at: *at,
                    addressed: *addressed,
                    from_owner: *from_owner,
                })
            })
            .collect();
        let (verdict, events) = self.damper.observe(group_id, now, &observations);
        self.audit_damper_events(group_id, now, &events).await;

        // Whoever this batch is mostly made of; ties resolve to the lowest sender id so
        // identical input cannot flip the answer between ticks.
        let mut counts: HashMap<&str, usize> = HashMap::new();
        for (sender, _) in senders.values() {
            *counts.entry(sender.as_str()).or_insert(0) += 1;
        }
        let pending_from_primary = counts.values().copied().max().unwrap_or(0);
        let primary = counts
            .iter()
            .filter(|(_, count)| **count == pending_from_primary)
            .map(|(sender, _)| (*sender).to_string())
            .min()
            .unwrap_or_default();
        DamperShape {
            verdict,
            primary,
            pending_from_primary,
            addressed: any_addressed,
            strict_addressed: any_strict,
            strict_ids,
            senders,
        }
    }

    /// Whether a quoted message id was written by the bot.
    ///
    /// Only this group's own log is consulted, so a quote that points at nothing resolves
    /// to `false` and cannot talk the damper into exempting a batch.
    pub(super) async fn quote_targets_bot(&self, group_id: &str, message_id: &str) -> bool {
        let row: Option<(String,)> = sqlx::query_as(
            "SELECT speaker_role FROM group_messages
             WHERE bot_id = ? AND group_id = ? AND message_id = ? LIMIT 1",
        )
        .bind(&self.bot_id)
        .bind(group_id)
        .bind(message_id)
        .fetch_optional(self.store.pool())
        .await
        .ok()
        .flatten();
        row.map(|(role,)| role == "bot").unwrap_or(false)
    }

    /// Write the damper's edge-triggered events to the audit log.
    ///
    /// The state machine emits a transition once (entering a level, ending an episode,
    /// capping a sender), so these are already deduplicated upstream and cannot turn the
    /// table into a per-tick heartbeat the way a raw dump would.
    pub(super) async fn audit_damper_events(
        &self,
        group_id: &str,
        now: i64,
        events: &[crate::app::damper::Event],
    ) {
        for event in events {
            let action = match event {
                crate::app::damper::Event::Enter { tier, sender } => format!(
                    "intake.damper.enter.l{}.g{group_id}.s{sender}",
                    tier.level()
                ),
                crate::app::damper::Event::Reset { reason } => {
                    format!("intake.damper.reset.g{group_id}.{}", reason.as_str())
                }
                crate::app::damper::Event::Cap { sender } => {
                    format!("intake.damper.cap.g{group_id}.s{sender}")
                }
            };
            let _ = self
                .store
                .audit("system", &action, Some(&self.bot_id), Some(group_id), now)
                .await;
        }
    }

    /// Whether this burst is complete enough to send on.
    ///
    /// The gate scores how unfinished the text looks; this function turns that score into
    /// a waiting time. Splitting the two is the whole point: a finished burst (score 0)
    /// is released on the very next tick instead of sitting out a fixed grace period,
    /// while a half-sentence gets up to [`HOLD_MAX_SECONDS`] of patience.
    ///
    /// Returns the verdict plus the gate's reason, so the caller can put the reason in the
    /// audit trail instead of throwing it away.
    pub(super) async fn burst_may_hand_over(
        &self,
        gate: &crate::app::preflight::Preflight,
        group_id: &str,
        rows: &[crate::store::PendingEventRow],
        widest: i64,
        now: i64,
    ) -> (bool, &'static str) {
        let judged = self
            .pace
            .lock()
            .ok()
            .and_then(|pace| pace.get(group_id).copied());
        let score = match judged {
            // Something new arrived: the text changed, so the old score is stale.
            Some((id, _, _)) if id != widest => None,
            // Nothing new: the cached score still describes exactly this text, so only
            // the clock has moved, and the clock is arithmetic rather than a model call.
            Some((_, score, judged_at)) => {
                return (now - judged_at >= score.hold_seconds(), score.reason());
            }
            None => None,
        };
        let messages: Vec<IncomingMessage> = rows
            .iter()
            .filter_map(|r| serde_json::from_str::<IncomingMessage>(&r.normalized_payload).ok())
            .collect();
        let pending_ids: HashSet<String> = messages.iter().map(|m| m.message_id.clone()).collect();
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
        let score = match score {
            Some(score) => score,
            None => {
                // The digest is launched when a burst arrives, but no longer
                // inside the gate's window: the summary model routinely needs
                // longer than the gate can lend, and a digest abandoned at the
                // gate's deadline helped no one (2026-10-27: 100% fallback).
                // It finishes in the background on its own budget and stashes
                // a slot for the group's next turn; if none is ready (or the
                // model failed) the turn falls back to the raw entries.
                if let Some(engine) = self.digest.clone() {
                    engine.prepare_background(group_id, messages.clone());
                }
                // The damper's structure line rides along when it has one. It is context
                // about the window, nothing more: the gate keeps its advisory role and
                // still cannot silence anything on its own.
                let hint = self.damper.hint(group_id);
                gate.score_with_hint(
                    &self.bot_id,
                    group_id,
                    &messages,
                    &recent,
                    &[],
                    hint.as_deref(),
                )
                .await
            }
        };
        if let Ok(mut pace) = self.pace.lock() {
            pace.insert(group_id.to_string(), (widest, score, now));
        }
        // A score is a statement about the text, never a hold forever: the highest score
        // buys only [`HOLD_MAX_SECONDS`], and an unavailable gate buys nothing at all.
        (score.hold_seconds() == 0, score.reason())
    }
}

/// A message names the bot: any configured call name appears in its
/// text. Members address her by name far more often than they @ her,
/// and a name in the text is as structural a signal as the @ itself.
/// ASCII case-insensitive; CJK names match as plain substrings.
pub(crate) fn text_names_bot(text: &str, names: &[String]) -> bool {
    if text.is_empty() || names.is_empty() {
        return false;
    }
    let lowered = text.to_lowercase();
    names.iter().any(|name| {
        let name = name.trim();
        !name.is_empty() && lowered.contains(&name.to_lowercase())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_names_bot_matches_names_case_insensitively() {
        let names = vec!["Gemini".to_string(), "小双".to_string()];
        assert!(text_names_bot("Gemini 在吗", &names));
        assert!(text_names_bot("gemini 快看这个", &names));
        assert!(text_names_bot("小双又睡着了", &names));
        assert!(!text_names_bot("今天 gem 好闪", &names));
        assert!(!text_names_bot("", &names));
        assert!(!text_names_bot("Gemini", &[]));
    }
}
