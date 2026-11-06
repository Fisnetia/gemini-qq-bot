//! Outbox persistence for SqliteStore: enqueue/seal, speaker aliases, delivery marks, counts and crash reconciliation.
//! Moved out of sqlite.rs verbatim (std-split-sqlite); logic unchanged.

use super::*;

impl SqliteStore {
    pub async fn enqueue_outbox(
        &self,
        bot_id: &str,
        group_id: &str,
        batch_id: &str,
        text: &str,
        now: i64,
    ) -> BotResult<String> {
        let id = format!("out_{}", uuid_like());
        sqlx::query(
            "INSERT INTO outbox (id, bot_id, group_id, batch_id, text, state, created_at, updated_at)
             VALUES (?, ?, ?, ?, ?, 'pending', ?, ?)",
        )
        .bind(&id)
        .bind(bot_id)
        .bind(group_id)
        .bind(batch_id)
        .bind(text)
        .bind(now)
        .bind(now)
        .execute(&self.pool)
        .await?;
        Ok(id)
    }

    pub async fn enqueue_outbox_part(
        &self,
        part: crate::store::OutboxPart<'_>,
    ) -> BotResult<String> {
        let id = format!("out_{}", uuid_like());
        let kind = if part.sticker_hash.is_some() {
            "sticker"
        } else {
            "text"
        };
        sqlx::query(
            "INSERT INTO outbox
                (id, bot_id, group_id, batch_id, text, kind, sticker_hash, quote_message_id, state, part_index, part_count, created_at, updated_at)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, 'pending', ?, ?, ?, ?)",
        )
        .bind(&id)
        .bind(part.bot_id)
        .bind(part.group_id)
        .bind(part.batch_id)
        .bind(part.text)
        .bind(kind)
        .bind(part.sticker_hash)
        .bind(part.quote_message_id)
        .bind(part.part_index)
        .bind(part.part_count)
        .bind(part.now)
        .bind(part.now)
        .execute(&self.pool)
        .await?;
        Ok(id)
    }

    /// Mark every not-yet-sent row in a batch unknown. Sent rows stay sent.
    pub async fn seal_pending_outbox(
        &self,
        bot_id: &str,
        group_id: &str,
        batch_id: &str,
        now: i64,
    ) -> BotResult<u64> {
        let result = sqlx::query(
            "UPDATE outbox SET state = 'unknown', updated_at = ?
             WHERE bot_id = ? AND group_id = ? AND batch_id = ? AND state IN ('pending', 'sending')",
        )
        .bind(now)
        .bind(bot_id)
        .bind(group_id)
        .bind(batch_id)
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected())
    }

    pub async fn list_speaker_aliases(
        &self,
        bot_id: &str,
        group_id: &str,
    ) -> BotResult<Vec<(String, String)>> {
        let rows = sqlx::query(
            "SELECT user_id, alias FROM speaker_aliases WHERE bot_id = ? AND group_id = ?",
        )
        .bind(bot_id)
        .bind(group_id)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .into_iter()
            .map(|row| (row.get("user_id"), row.get("alias")))
            .collect())
    }

    pub async fn upsert_speaker_alias(
        &self,
        bot_id: &str,
        group_id: &str,
        user_id: &str,
        alias: &str,
        now: i64,
    ) -> BotResult<()> {
        sqlx::query(
            "INSERT INTO speaker_aliases (bot_id, group_id, user_id, alias, updated_at)
             VALUES (?, ?, ?, ?, ?)
             ON CONFLICT (bot_id, group_id, user_id) DO UPDATE SET alias = excluded.alias, updated_at = excluded.updated_at",
        )
        .bind(bot_id)
        .bind(group_id)
        .bind(user_id)
        .bind(alias)
        .bind(now)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn outbox_batch(
        &self,
        state: OutboxState,
        limit: i64,
    ) -> BotResult<Vec<OutboxRecord>> {
        let rows = sqlx::query(
            "SELECT id, bot_id, group_id, batch_id, text, kind, sticker_hash, quote_message_id, state, message_id, part_index, part_count, created_at, updated_at
             FROM outbox WHERE state = ? ORDER BY created_at, part_index, id LIMIT ?",
        )
        .bind(state.as_str())
        .bind(limit)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.into_iter().map(map_outbox).collect())
    }

    pub async fn get_outbox(&self, id: &str) -> BotResult<Option<OutboxRecord>> {
        let row = sqlx::query(
            "SELECT id, bot_id, group_id, batch_id, text, kind, sticker_hash, quote_message_id, state, message_id, part_index, part_count, created_at, updated_at
             FROM outbox WHERE id = ?",
        )
        .bind(id)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(map_outbox))
    }

    pub async fn mark_outbox(
        &self,
        id: &str,
        state: OutboxState,
        message_id: Option<&str>,
        now: i64,
    ) -> BotResult<()> {
        sqlx::query("UPDATE outbox SET state = ?, message_id = COALESCE(?, message_id), updated_at = ? WHERE id = ?")
            .bind(state.as_str())
            .bind(message_id)
            .bind(now)
            .bind(id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// Stickers this group recently sent, newest first: `(hash, batch_id, created_at)`.
    ///
    /// Feeds the offer damping in `turn.rs`. `batch_id` is there because one bot round
    /// can deliver several parts, so "the last N rounds" is a distinct-batch view of
    /// the same rows, and reading them once beats two queries that could disagree.
    pub async fn recent_sent_stickers(
        &self,
        bot_id: &str,
        group_id: &str,
        limit: i64,
    ) -> BotResult<Vec<(String, String, i64)>> {
        let rows = sqlx::query(
            "SELECT sticker_hash, batch_id, created_at FROM outbox \
             WHERE bot_id = ? AND group_id = ? AND state = 'sent' AND sticker_hash IS NOT NULL \
             ORDER BY created_at DESC, id DESC LIMIT ?",
        )
        .bind(bot_id)
        .bind(group_id)
        .bind(limit)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .into_iter()
            .map(|r| {
                (
                    r.get::<String, _>("sticker_hash"),
                    r.get::<String, _>("batch_id"),
                    r.get::<i64, _>("created_at"),
                )
            })
            .collect())
    }

    pub async fn outbox_counts(&self, bot_id: &str) -> BotResult<Vec<(String, i64)>> {
        let rows =
            sqlx::query("SELECT state, COUNT(*) AS n FROM outbox WHERE bot_id = ? GROUP BY state")
                .bind(bot_id)
                .fetch_all(&self.pool)
                .await?;
        Ok(rows
            .into_iter()
            .map(|r| (r.get::<String, _>("state"), r.get::<i64, _>("n")))
            .collect())
    }

    /// Operator-only retry of an `unknown` message. Never automatic.
    pub async fn retry_unknown(&self, id: &str, now: i64) -> BotResult<bool> {
        let res = sqlx::query(
            "UPDATE outbox SET state = 'pending', updated_at = ? WHERE id = ? AND state = 'unknown'",
        )
        .bind(now)
        .bind(id)
        .execute(&self.pool)
        .await?;
        Ok(res.rows_affected() == 1)
    }

    /// Messages stuck in `sending` after a crash have an unknown delivery status.
    pub async fn reconcile_outbox(&self, now: i64) -> BotResult<u64> {
        let res = sqlx::query(
            "UPDATE outbox SET state = 'unknown', updated_at = ? WHERE state = 'sending'",
        )
        .bind(now)
        .execute(&self.pool)
        .await?;
        Ok(res.rows_affected())
    }
}
