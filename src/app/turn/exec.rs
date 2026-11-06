//! Batch entry point for TurnExecutor: run_batch and its failure bookkeeping.
//! Moved out of turn.rs verbatim (std-split-turn-executor); logic unchanged.

use super::*;

impl TurnExecutor {
    /// Execute one batch. Never returns an error: failures are encoded in
    /// [`TurnResult`] so the scheduler can decide whether to continue the lane.
    pub async fn run_batch(&self, job: &BatchJob) -> TurnResult {
        let turn_started = std::time::Instant::now();
        let now = crate::now_unix();

        // 1. The group may have been disabled while the batch waited.
        let group = match self.store.get_group(&self.bot_id, &job.group_id).await {
            Ok(Some(g)) => g,
            _ => return TurnResult::SkippedInactive,
        };
        if !group.enabled || group.paused {
            return TurnResult::SkippedInactive;
        }

        // 1.5 The hourly fuse, re-checked where the call actually starts.
        //
        // The intake checks the same fuse before it claims, but claiming and
        // starting are different instants: a batch can queue behind the
        // scheduler's permits for a while, and a deferred batch is re-claimed
        // by a later tick. The claim-time count therefore says nothing about
        // how many calls started since (one observation window saw 64 starts in
        // a rolling hour against a budget of 60). Nothing is sent to the agent
        // here; the batch goes back to the pool and the lane defers, so the next
        // tick re-claims it once calls age out.
        if let Some(remaining) = self.hourly_budget_remaining(&job.group_id, now).await
            && remaining == 0
        {
            let _ = self.store.release_events(&job.event_ids).await;
            let ages = self.pending_ages(&job.event_ids).await;
            let oldest_age = ages.values().min().map(|at| now - at).unwrap_or(0);
            let _ = self
                .store
                .audit(
                    "system",
                    &format!(
                        "scheduler.budget.exec_denied.g{}.age{oldest_age}",
                        job.group_id
                    ),
                    Some(&self.bot_id),
                    Some(&job.group_id),
                    now,
                )
                .await;
            return TurnResult::BudgetDeferred;
        }

        // 2. Proxy gate. A closed gate keeps the batch pending and defers the lane.
        if !self.mihomo.ready().await {
            let _ = self.store.release_events(&job.event_ids).await;
            let _ = self
                .store
                .audit(
                    "system",
                    "agent.waiting_for_us_proxy",
                    Some(&self.bot_id),
                    Some(&job.group_id),
                    now,
                )
                .await;
            return TurnResult::DeferredProxy;
        }

        // 2.5 Backoff gate. The resident session is the only execution path, so a
        // group whose last session was retired waits for its respawn deadline here.
        // The batch goes back to the pool and the lane defers — waiting *is* the
        // behaviour. There is no print lane behind this gate: an engine that cannot
        // produce a session leaves the whole group silent until it is fixed.
        if let Some(until) = self.resident.backoff_until(&job.group_id, now).await {
            let _ = self.store.release_events(&job.event_ids).await;
            let _ = self
                .store
                .audit(
                    "system",
                    &format!(
                        "resident.turn.deferred_backoff.g{}.until{until}",
                        job.group_id
                    ),
                    Some(&self.bot_id),
                    Some(&job.group_id),
                    now,
                )
                .await;
            return TurnResult::DeferredProxy;
        }
        let result = self
            .run_resident_batch(job, &group, now, turn_started)
            .await;
        // A consumed batch ends any run of prompt rejections for the
        // group: the refused material has left the pool, so the next
        // refusal starts a fresh count instead of dropping a batch that
        // was never refused before.
        if matches!(
            result,
            TurnResult::Replied | TurnResult::Silent | TurnResult::DroppedStale
        ) {
            self.resident.clear_prompt_rejections(&job.group_id).await;
        }
        result
    }

    /// Close a round whose prompt an upstream content filter refused. The
    /// prompt never ran, so this is not a contract failure and the session
    /// is not at fault: the call closes under the dedicated error class, no
    /// probe window (contract validity or degraded) hears about the round,
    /// the seed the session was born with is quarantined so the next spawn
    /// degrades instead of resubmitting it, and the session retires through
    /// the ordinary backoff ladder.
    pub(super) async fn retire_for_content_filter(
        &self,
        manager: &Arc<crate::app::resident::ResidentManager>,
        job: &BatchJob,
        call_id: &str,
        coverage: &ResidentCoverage,
    ) -> TurnResult {
        let _ = self
            .store
            .mark_seed_rejected(&self.bot_id, &job.group_id, crate::now_unix())
            .await;
        manager
            .audit(
                &job.group_id,
                &format!("agent.content_filtered.g{}", job.group_id),
            )
            .await;
        // The filter's answer to a prompt is deterministic, so a batch
        // refused twice running will be refused forever: drop the
        // covered events instead of handing them back to the pool for
        // another identical submission (2026-11-06, group 943: thirteen
        // refusals in five minutes, one per respawned session). The
        // retirement below still runs; its release is a no-op for rows
        // that are already gone.
        let covered = coverage.event_ids();
        if manager.note_prompt_rejection(&job.group_id, &covered).await {
            let n = covered.len();
            let _ = self.store.delete_pending(&covered).await;
            manager
                .audit(
                    &job.group_id,
                    &format!("scheduler.poison_drop.g{}.n{n}", job.group_id),
                )
                .await;
        }
        self.retire_resident(
            manager,
            job,
            call_id,
            "content_filtered",
            ErrorClass::ContentFiltered.as_str(),
            coverage,
        )
        .await
    }

    /// Why a turn's answer was unusable. The preview is a flattened head of the
    /// raw contract text the session produced. It stays in the audit and is never
    /// handed back to the model.
    pub(super) async fn audit_bad_output(&self, group_id: &str, detail: &str, preview: &str) {
        let now = crate::now_unix();
        let kind = crate::agent::agy::output_preview(detail, 160);
        let text = crate::agent::agy::output_preview(preview, 80);
        let _ = self
            .store
            .audit(
                "system",
                &format!("agent.bad_output.g{group_id}.kind.{kind}"),
                Some(&self.bot_id),
                Some(group_id),
                now,
            )
            .await;
        let _ = self
            .store
            .audit(
                "system",
                &format!("agent.bad_output.g{group_id}.text.{text}"),
                Some(&self.bot_id),
                Some(group_id),
                now,
            )
            .await;
    }
}
