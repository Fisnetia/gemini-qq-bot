//! Compaction jobs for SqliteStore: create/read, run/finish and startup reconciliation.
//! Moved out of sqlite.rs verbatim (std-split-sqlite); logic unchanged.

use super::*;

impl SqliteStore {
    pub async fn create_job(
        &self,
        bot_id: &str,
        group_id: &str,
        source_revision: i64,
        requested_by: &str,
        now: i64,
    ) -> BotResult<CompactionJob> {
        let id = format!("job_{}", uuid_like());
        sqlx::query(
            "INSERT INTO memory_compaction_jobs
                (id, bot_id, group_id, source_revision, state, attempt_count, requested_by, created_at)
             VALUES (?, ?, ?, ?, 'queued', 0, ?, ?)",
        )
        .bind(&id)
        .bind(bot_id)
        .bind(group_id)
        .bind(source_revision)
        .bind(requested_by)
        .bind(now)
        .execute(&self.pool)
        .await?;
        self.get_job(&id)
            .await?
            .ok_or_else(|| BotError::Message("刚创建的压缩任务读取失败".into()))
    }

    pub async fn get_job(&self, id: &str) -> BotResult<Option<CompactionJob>> {
        let row = sqlx::query(
            "SELECT id, bot_id, group_id, source_revision, state, attempt_count, requested_by,
                    created_at, started_at, finished_at, error_class
             FROM memory_compaction_jobs WHERE id = ?",
        )
        .bind(id)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(map_job))
    }

    pub async fn jobs_for_group(
        &self,
        bot_id: &str,
        group_id: &str,
        limit: i64,
    ) -> BotResult<Vec<CompactionJob>> {
        let rows = sqlx::query(
            "SELECT id, bot_id, group_id, source_revision, state, attempt_count, requested_by,
                    created_at, started_at, finished_at, error_class
             FROM memory_compaction_jobs WHERE bot_id = ? AND group_id = ?
             ORDER BY created_at DESC LIMIT ?",
        )
        .bind(bot_id)
        .bind(group_id)
        .bind(limit)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.into_iter().map(map_job).collect())
    }

    /// Count jobs started since `since` for the daily cap. Manual runs count too.
    pub async fn jobs_since(&self, since: i64) -> BotResult<i64> {
        let n: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM memory_compaction_jobs WHERE created_at >= ?")
                .bind(since)
                .fetch_one(&self.pool)
                .await?;
        Ok(n)
    }

    /// Latest successful job time for a group, used for the 24h cooldown.
    pub async fn last_success_at(&self, bot_id: &str, group_id: &str) -> BotResult<Option<i64>> {
        let ts: Option<i64> = sqlx::query_scalar(
            "SELECT MAX(finished_at) FROM memory_compaction_jobs
             WHERE bot_id = ? AND group_id = ? AND state = 'succeeded'",
        )
        .bind(bot_id)
        .bind(group_id)
        .fetch_optional(&self.pool)
        .await?
        .flatten();
        Ok(ts)
    }

    pub async fn mark_job_running(&self, id: &str, now: i64) -> BotResult<()> {
        sqlx::query(
            "UPDATE memory_compaction_jobs
             SET state = 'running', attempt_count = attempt_count + 1, started_at = ?
             WHERE id = ?",
        )
        .bind(now)
        .bind(id)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn finish_job(
        &self,
        id: &str,
        state: JobState,
        error_class: Option<&str>,
        now: i64,
    ) -> BotResult<()> {
        sqlx::query(
            "UPDATE memory_compaction_jobs SET state = ?, error_class = ?, finished_at = ? WHERE id = ?",
        )
        .bind(state.as_str())
        .bind(error_class)
        .bind(now)
        .bind(id)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Jobs left `running` by a crash are marked failed on the next boot.
    pub async fn reconcile_jobs(&self, now: i64) -> BotResult<u64> {
        let res = sqlx::query(
            "UPDATE memory_compaction_jobs SET state = 'failed', error_class = 'interrupted', finished_at = ?
             WHERE state IN ('queued', 'running')",
        )
        .bind(now)
        .execute(&self.pool)
        .await?;
        Ok(res.rows_affected())
    }
}
