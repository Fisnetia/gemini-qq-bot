//! Conversation continuity state for SqliteStore: seed quarantine, archived chunk reads, seen-message pruning and per-group state.
//! Moved out of sqlite.rs verbatim (std-split-sqlite); logic unchanged.

use super::*;

impl SqliteStore {
    /// Newest chunks first: what the memory_search tool shows by default.
    // ---------------------------------------------------------- seed quarantine

    /// The group's seed quarantine row, if any spawn has been recorded.
    pub async fn get_seed_quarantine(
        &self,
        bot_id: &str,
        group_id: &str,
    ) -> BotResult<Option<SeedQuarantine>> {
        let row = sqlx::query(
            "SELECT bot_id, group_id, last_seed_hash, rejected_hash, tier, clean_rounds, updated_at
             FROM seed_quarantine WHERE bot_id = ? AND group_id = ?",
        )
        .bind(bot_id)
        .bind(group_id)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(|r| SeedQuarantine {
            bot_id: r.get("bot_id"),
            group_id: r.get("group_id"),
            last_seed_hash: r.get("last_seed_hash"),
            rejected_hash: r.get("rejected_hash"),
            tier: r.get("tier"),
            clean_rounds: r.get("clean_rounds"),
            updated_at: r.get("updated_at"),
        }))
    }

    /// Record the seed a fresh resident session was just born with: its
    /// fingerprint and the tier it was assembled at. Only the spawn facts
    /// move — the rejection record and the clean-round count belong to the
    /// two methods below, and a respawn must not quietly clear them.
    pub async fn record_seed_spawn(
        &self,
        bot_id: &str,
        group_id: &str,
        seed_hash: &str,
        tier: i64,
        now: i64,
    ) -> BotResult<()> {
        sqlx::query(
            "INSERT INTO seed_quarantine (bot_id, group_id, last_seed_hash, tier, updated_at)
             VALUES (?, ?, ?, ?, ?)
             ON CONFLICT(bot_id, group_id) DO UPDATE SET
                last_seed_hash = excluded.last_seed_hash,
                tier = excluded.tier,
                updated_at = excluded.updated_at",
        )
        .bind(bot_id)
        .bind(group_id)
        .bind(seed_hash)
        .bind(tier)
        .bind(now)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Quarantine the seed the current session was born with: its hash is
    /// recorded as rejected, the next spawn degrades one more tier (capped
    /// at the minimal seed), and the clean-round count restarts. A group
    /// with no recorded spawn has nothing to quarantine.
    pub async fn mark_seed_rejected(
        &self,
        bot_id: &str,
        group_id: &str,
        now: i64,
    ) -> BotResult<()> {
        sqlx::query(
            "UPDATE seed_quarantine
             SET rejected_hash = last_seed_hash,
                 tier = MIN(tier + 1, 2),
                 clean_rounds = 0,
                 updated_at = ?
             WHERE bot_id = ? AND group_id = ? AND last_seed_hash IS NOT NULL",
        )
        .bind(now)
        .bind(bot_id)
        .bind(group_id)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Count one delivered round towards seed recovery. Ten consecutive
    /// delivered rounds clear the tier and the rejection record together:
    /// the group has demonstrated its ordinary seed is no longer refused.
    /// Rounds for a group under no quarantine cost one read and no write.
    pub async fn note_seed_clean_round(
        &self,
        bot_id: &str,
        group_id: &str,
        now: i64,
    ) -> BotResult<()> {
        let Some(q) = self.get_seed_quarantine(bot_id, group_id).await? else {
            return Ok(());
        };
        if q.tier <= 0 && q.rejected_hash.is_none() {
            return Ok(());
        }
        let clean = q.clean_rounds + 1;
        if clean >= 10 {
            sqlx::query(
                "UPDATE seed_quarantine
                 SET tier = 0, rejected_hash = NULL, clean_rounds = 0, updated_at = ?
                 WHERE bot_id = ? AND group_id = ?",
            )
            .bind(now)
            .bind(bot_id)
            .bind(group_id)
            .execute(&self.pool)
            .await?;
        } else {
            sqlx::query(
                "UPDATE seed_quarantine SET clean_rounds = ?, updated_at = ?
                 WHERE bot_id = ? AND group_id = ?",
            )
            .bind(clean)
            .bind(now)
            .bind(bot_id)
            .bind(group_id)
            .execute(&self.pool)
            .await?;
        }
        Ok(())
    }

    pub async fn recent_chunks(
        &self,
        bot_id: &str,
        group_id: &str,
        limit: i64,
    ) -> BotResult<Vec<MemoryChunkRecord>> {
        let rows = sqlx::query(
            "SELECT chunk_id, bot_id, group_id, seq, from_msg_id, to_msg_id, summary, model_id, created_at
             FROM memory_chunks WHERE bot_id = ? AND group_id = ?
             ORDER BY seq DESC LIMIT ?",
        )
        .bind(bot_id)
        .bind(group_id)
        .bind(limit)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.into_iter().map(map_chunk).collect())
    }

    pub async fn count_chunks(&self, bot_id: &str, group_id: &str) -> BotResult<i64> {
        let count: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM memory_chunks WHERE bot_id = ? AND group_id = ?",
        )
        .bind(bot_id)
        .bind(group_id)
        .fetch_one(&self.pool)
        .await?;
        Ok(count)
    }

    // ------------------------------------------------------------------ seen

    /// Mark a message as seen. Returns `true` when it is new (not a duplicate).
    pub async fn mark_seen(&self, bot_id: &str, message_id: &str, now: i64) -> BotResult<bool> {
        let res = sqlx::query(
            "INSERT INTO seen_messages (bot_id, message_id, seen_at) VALUES (?, ?, ?)
             ON CONFLICT (bot_id, message_id) DO NOTHING",
        )
        .bind(bot_id)
        .bind(message_id)
        .bind(now)
        .execute(&self.pool)
        .await?;
        Ok(res.rows_affected() == 1)
    }

    pub async fn prune_seen(&self, older_than: i64) -> BotResult<u64> {
        let res = sqlx::query("DELETE FROM seen_messages WHERE seen_at < ?")
            .bind(older_than)
            .execute(&self.pool)
            .await?;
        Ok(res.rows_affected())
    }

    // ----------------------------------------------------------------- state

    pub async fn get_state(
        &self,
        bot_id: &str,
        group_id: &str,
    ) -> BotResult<Option<GroupStateRecord>> {
        let row = sqlx::query(
            "SELECT mood_json, topic_state, updated_at FROM group_state WHERE bot_id = ? AND group_id = ?",
        )
        .bind(bot_id)
        .bind(group_id)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(|r| GroupStateRecord {
            mood_json: r.get("mood_json"),
            topic_state: r.get("topic_state"),
            updated_at: r.get("updated_at"),
        }))
    }

    pub async fn put_state(
        &self,
        bot_id: &str,
        group_id: &str,
        mood_json: Option<&str>,
        topic_state: Option<&str>,
        now: i64,
    ) -> BotResult<()> {
        sqlx::query(
            "INSERT INTO group_state (bot_id, group_id, mood_json, topic_state, updated_at)
             VALUES (?, ?, ?, ?, ?)
             ON CONFLICT (bot_id, group_id) DO UPDATE SET
                mood_json = excluded.mood_json,
                topic_state = excluded.topic_state,
                updated_at = excluded.updated_at",
        )
        .bind(bot_id)
        .bind(group_id)
        .bind(mood_json)
        .bind(topic_state)
        .bind(now)
        .execute(&self.pool)
        .await?;
        Ok(())
    }
}

impl SqliteStore {
    #[allow(clippy::too_many_arguments)]
    pub async fn insert_chunk(
        &self,
        chunk_id: &str,
        bot_id: &str,
        group_id: &str,
        seq: i64,
        from_msg_id: i64,
        to_msg_id: i64,
        summary: &str,
        model_id: &str,
        now: i64,
    ) -> BotResult<()> {
        sqlx::query(
            "INSERT INTO memory_chunks
                (chunk_id, bot_id, group_id, seq, from_msg_id, to_msg_id, summary, model_id, created_at)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(chunk_id)
        .bind(bot_id)
        .bind(group_id)
        .bind(seq)
        .bind(from_msg_id)
        .bind(to_msg_id)
        .bind(summary)
        .bind(model_id)
        .bind(now)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Next chunk sequence number for a group (1-based, monotonic).
    pub async fn next_chunk_seq(&self, bot_id: &str, group_id: &str) -> BotResult<i64> {
        let max: Option<i64> = sqlx::query_scalar(
            "SELECT MAX(seq) FROM memory_chunks WHERE bot_id = ? AND group_id = ?",
        )
        .bind(bot_id)
        .bind(group_id)
        .fetch_one(&self.pool)
        .await?;
        Ok(max.unwrap_or(0) + 1)
    }

    /// Write one chunk and flag its source window as archived in a single transaction.
    ///
    /// Both halves must land together: a chunk without the archive flag would be folded
    /// twice, and an archive without the chunk would lose the messages' content.
    #[allow(clippy::too_many_arguments)]
    pub async fn commit_chunk(
        &self,
        chunk_id: &str,
        bot_id: &str,
        group_id: &str,
        seq: i64,
        from_msg_id: i64,
        to_msg_id: i64,
        summary: &str,
        model_id: &str,
        now: i64,
        archived_ids: &[i64],
    ) -> BotResult<()> {
        let mut tx = self.pool.begin().await?;
        sqlx::query(
            "INSERT INTO memory_chunks
                (chunk_id, bot_id, group_id, seq, from_msg_id, to_msg_id, summary, model_id, created_at)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(chunk_id)
        .bind(bot_id)
        .bind(group_id)
        .bind(seq)
        .bind(from_msg_id)
        .bind(to_msg_id)
        .bind(summary)
        .bind(model_id)
        .bind(now)
        .execute(&mut *tx)
        .await?;
        for id in archived_ids {
            sqlx::query("UPDATE group_messages SET archived = 1 WHERE id = ?")
                .bind(id)
                .execute(&mut *tx)
                .await?;
        }
        tx.commit().await?;
        Ok(())
    }
}
