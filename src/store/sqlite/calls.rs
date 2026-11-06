//! Agent call ledger for SqliteStore: start/finish, resident counts, recent-call reads and startup reconciliation.
//! Moved out of sqlite.rs verbatim (std-split-sqlite); logic unchanged.

use super::*;

impl SqliteStore {
    pub async fn start_call(
        &self,
        bot_id: &str,
        group_id: &str,
        batch_id: &str,
        now: i64,
    ) -> BotResult<String> {
        self.start_call_with_priority(bot_id, group_id, batch_id, now, false)
            .await
    }

    /// `priority` marks a call whose batch carried a direct @-mention; the
    /// hourly budget gate counts the two pools separately so ambient turns
    /// cannot spend the reserve held for addressed batches.
    pub async fn start_call_with_priority(
        &self,
        bot_id: &str,
        group_id: &str,
        batch_id: &str,
        now: i64,
        priority: bool,
    ) -> BotResult<String> {
        let id = format!("call_{}", uuid_like());
        sqlx::query(
            "INSERT INTO agent_calls (id, bot_id, group_id, batch_id, state, attempt_count, started_at, priority)
             VALUES (?, ?, ?, ?, 'running', 1, ?, ?)",
        )
        .bind(&id)
        .bind(bot_id)
        .bind(group_id)
        .bind(batch_id)
        .bind(now)
        .bind(i64::from(priority))
        .execute(&self.pool)
        .await?;
        Ok(id)
    }

    pub async fn finish_call(
        &self,
        id: &str,
        state: CallState,
        error_class: Option<&str>,
        now: i64,
    ) -> BotResult<()> {
        sqlx::query(
            "UPDATE agent_calls SET state = ?, error_class = ?, finished_at = ? WHERE id = ?",
        )
        .bind(state.as_str())
        .bind(error_class)
        .bind(now)
        .bind(id)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Record the P4 counters for one resident round.
    ///
    /// These are diagnostics only: `steps_count` is how many steps the step stream
    /// held when the round finished, `insert_count` how many extra batches the turn
    /// absorbed, and `draft_count` how many agent drafts the barrier withheld. They
    /// are written after `finish_call` so a row keeps its state history intact.
    /// A row that never got past preparation leaves all three null: there was no
    /// step stream to count.
    pub async fn record_resident_counts(
        &self,
        id: &str,
        steps_count: i64,
        insert_count: i64,
        draft_count: i64,
    ) -> BotResult<()> {
        sqlx::query(
            "UPDATE agent_calls SET steps_count = ?, insert_count = ?, draft_count = ? WHERE id = ?",
        )
        .bind(steps_count)
        .bind(insert_count)
        .bind(draft_count)
        .bind(id)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn recent_calls(
        &self,
        bot_id: &str,
        group_id: &str,
        limit: i64,
    ) -> BotResult<Vec<AgentCall>> {
        let rows = sqlx::query(
            "SELECT id, bot_id, group_id, batch_id, state, attempt_count, started_at, finished_at, error_class
             FROM agent_calls WHERE bot_id = ? AND group_id = ?
             ORDER BY started_at DESC, id DESC LIMIT ?",
        )
        .bind(bot_id)
        .bind(group_id)
        .bind(limit)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .into_iter()
            .map(|r| AgentCall {
                id: r.get("id"),
                bot_id: r.get("bot_id"),
                group_id: r.get("group_id"),
                batch_id: r.get("batch_id"),
                state: r.get("state"),
                attempt_count: r.get("attempt_count"),
                started_at: r.get("started_at"),
                finished_at: r.get("finished_at"),
                error_class: r.get("error_class"),
            })
            .collect())
    }

    /// Recent agent calls across every group of one bot, for the panel overview.
    pub async fn recent_calls_all(&self, bot_id: &str, limit: i64) -> BotResult<Vec<AgentCall>> {
        self.recent_calls_page(bot_id, limit, 0).await
    }

    /// One page of model calls, newest first.
    pub async fn recent_calls_page(
        &self,
        bot_id: &str,
        limit: i64,
        offset: i64,
    ) -> BotResult<Vec<AgentCall>> {
        let rows = sqlx::query(
            "SELECT id, bot_id, group_id, batch_id, state, attempt_count, started_at, finished_at, error_class
             FROM agent_calls WHERE bot_id = ?
             ORDER BY started_at DESC, id DESC LIMIT ? OFFSET ?",
        )
        .bind(bot_id)
        .bind(limit)
        .bind(offset)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .into_iter()
            .map(|r| AgentCall {
                id: r.get("id"),
                bot_id: r.get("bot_id"),
                group_id: r.get("group_id"),
                batch_id: r.get("batch_id"),
                state: r.get("state"),
                attempt_count: r.get("attempt_count"),
                started_at: r.get("started_at"),
                finished_at: r.get("finished_at"),
                error_class: r.get("error_class"),
            })
            .collect())
    }

    pub async fn count_calls_with_error(&self, bot_id: &str, error_class: &str) -> BotResult<i64> {
        let n: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM agent_calls WHERE bot_id = ? AND error_class = ?",
        )
        .bind(bot_id)
        .bind(error_class)
        .fetch_one(&self.pool)
        .await?;
        Ok(n)
    }

    /// Agent calls started for one group at or after `since`, in any state.
    ///
    /// Backs the hourly call budget. Attempts are counted, not successes: failed
    /// and unknown calls are exactly what a runaway loop produces, so excluding
    /// them would defuse the fuse.
    pub async fn count_agent_calls_since(
        &self,
        bot_id: &str,
        group_id: &str,
        since: i64,
    ) -> BotResult<i64> {
        let n: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM agent_calls WHERE bot_id = ? AND group_id = ? AND started_at >= ?",
        )
        .bind(bot_id)
        .bind(group_id)
        .bind(since)
        .fetch_one(&self.pool)
        .await?;
        Ok(n)
    }

    /// The same window count as [`Self::count_agent_calls_since`], split by
    /// the priority pool (see [`Self::start_call_with_priority`]).
    pub async fn count_agent_calls_since_by_priority(
        &self,
        bot_id: &str,
        group_id: &str,
        since: i64,
        priority: bool,
    ) -> BotResult<i64> {
        let n: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM agent_calls WHERE bot_id = ? AND group_id = ? AND started_at >= ? AND priority = ?",
        )
        .bind(bot_id)
        .bind(group_id)
        .bind(since)
        .bind(i64::from(priority))
        .fetch_one(&self.pool)
        .await?;
        Ok(n)
    }

    /// Calls stuck in `running` after a restart are ambiguous: the session may have
    /// advanced, so they are closed as `unknown` rather than replayed.
    pub async fn reconcile_calls(&self, now: i64) -> BotResult<Vec<(String, String)>> {
        let rows = sqlx::query("SELECT id, group_id FROM agent_calls WHERE state = 'running'")
            .fetch_all(&self.pool)
            .await?;
        let mut out = Vec::new();
        for row in rows {
            let id: String = row.get("id");
            let group_id: String = row.get("group_id");
            sqlx::query(
                "UPDATE agent_calls SET state = 'unknown', error_class = 'interrupted', finished_at = ? WHERE id = ?",
            )
            .bind(now)
            .bind(&id)
            .execute(&self.pool)
            .await?;
            out.push((id, group_id));
        }
        Ok(out)
    }
}
