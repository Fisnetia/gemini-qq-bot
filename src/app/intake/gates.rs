//! Claim gates for Intake: the stale-row age guardrail and the hourly budget fuse.
//! Moved out of intake.rs verbatim (std-split-intake); logic unchanged.

use super::*;

impl Intake {
    /// Consume pending rows that are too old to be worth an answer, returning
    /// the survivors oldest-first.
    ///
    /// A row older than [`STALE_ROW_SECONDS`] is dropped (and audited) when the
    /// pool also holds anything newer: answering it would produce exactly the
    /// minutes-late reply the misalignment investigation kept finding. When
    /// the *whole* pool is stale — a quiet group waking up — only the newest
    /// segment survives, so the group still gets an answer to its latest topic
    /// instead of total silence. Message bodies survive in the raw group log;
    /// dropping a pending row only ends its candidacy for a reply.
    pub(super) async fn apply_age_guardrail(
        &self,
        group_id: &str,
        rows: Vec<crate::store::PendingEventRow>,
        now: i64,
    ) -> Vec<crate::store::PendingEventRow> {
        let cutoff = now - STALE_ROW_SECONDS;
        let newest = rows.iter().map(|r| r.received_at).max().unwrap_or(now);
        if newest < cutoff {
            // Whole pool stale: keep only the newest segment.
            let entries = entries_from_rows(&rows);
            let chunks = split_batches(&entries, self.limits.batch);
            if chunks.len() <= 1 {
                return rows;
            }
            let keep_from: usize = chunks[..chunks.len() - 1]
                .iter()
                .map(|chunk| chunk.len())
                .sum();
            let (drop_rows, keep_rows) = rows.split_at(keep_from);
            self.drop_stale_rows(group_id, drop_rows, now).await;
            return keep_rows.to_vec();
        }
        let (stale, fresh): (Vec<_>, Vec<_>) =
            rows.into_iter().partition(|r| r.received_at < cutoff);
        if stale.is_empty() {
            return fresh;
        }
        self.drop_stale_rows(group_id, &stale, now).await;
        fresh
    }

    /// How many batches this group may still claim under its hourly budget, and how many
    /// calls are already in the window.
    ///
    /// `None` means the fuse is blown: defer (audited, throttled). `Some((n, calls))`
    /// caps this tick's claims at `n` batches; `n` is `usize::MAX` when the fuse is
    /// disabled, and `calls` is what the pacing layer needs to turn its own *cap* into a
    /// remaining allowance — the fuse already speaks in remaining terms, the tier cap is
    /// an absolute ceiling, and mixing the two would let a capped group claim one more
    /// batch every tick.
    /// The budget is a fuse, not a pacing tool: it counts every call
    /// started in the last hour, whatever state that call ended in, and a
    /// privileged (@) batch does **not** bypass it — a fuse an @ can jump is
    /// not a fuse. The deferral is audited at most once per
    /// [`BUDGET_AUDIT_THROTTLE_SECONDS`] per group, so a capped group cannot
    /// flood the audit table at two rows a second for as long as it stays
    /// capped; the action carries the pool's oldest message age, so a long
    /// deferral is visible as data rather than as silence.
    pub(super) async fn budget_gate(
        &self,
        group_id: &str,
        now: i64,
        oldest_age: i64,
        pool_privileged: bool,
    ) -> Option<(usize, i64)> {
        let budget = self.limits.hourly_call_budget;
        let calls = self
            .store
            .count_agent_calls_since(&self.bot_id, group_id, now - BUDGET_WINDOW_SECONDS)
            .await
            .unwrap_or(0);
        if budget == 0 {
            return Some((usize::MAX, calls));
        }
        // Budget split: ambient batches stop at the ambient cap so the
        // reserve stays available for batches carrying a direct mention.
        // Without the split, a hot group burns the whole hour on ambient
        // turns and an addressed question queues behind the blown fuse
        // (2026-10-27: 900000003 at 60/60 with fresh questions pending).
        // The reserve only exists on budgets large enough to hold one;
        // small budgets keep the single-pool behaviour.
        let total_remaining = i64::from(budget) - calls;
        let remaining = if pool_privileged {
            total_remaining
        } else {
            let reserve = if i64::from(budget) > BUDGET_PRIORITY_RESERVE {
                BUDGET_PRIORITY_RESERVE
            } else {
                0
            };
            if reserve == 0 {
                total_remaining
            } else {
                let ambient_calls = self
                    .store
                    .count_agent_calls_since_by_priority(
                        &self.bot_id,
                        group_id,
                        now - BUDGET_WINDOW_SECONDS,
                        false,
                    )
                    .await
                    .unwrap_or(0);
                total_remaining.min(i64::from(budget) - reserve - ambient_calls)
            }
        };
        if remaining > 0 {
            return Some((remaining.max(0) as usize, calls));
        }
        // A deferral that still had total headroom was caused by the
        // ambient cap alone; the audit suffix keeps the two pools'
        // pressure readable separately in the observation log.
        let ambient_suffix = if !pool_privileged && !budget_exhausted(calls, budget) {
            ".ambient"
        } else {
            ""
        };
        let should_audit = match self.budget_marks.lock() {
            Ok(mut marks) => match marks.get(group_id) {
                Some(&at) if now - at < BUDGET_AUDIT_THROTTLE_SECONDS => false,
                _ => {
                    marks.insert(group_id.to_string(), now);
                    true
                }
            },
            Err(_) => false,
        };
        if should_audit {
            let _ = self
                .store
                .audit(
                    "system",
                    &format!("scheduler.budget.defer.g{group_id}.age{oldest_age}{ambient_suffix}"),
                    Some(&self.bot_id),
                    Some(group_id),
                    now,
                )
                .await;
        }
        None
    }
}
