//! Raw group-message log for SqliteStore: append, reads, paging and archive trimming (plus the two recent-outbox text reads used for freshness checks).
//! Moved out of sqlite.rs verbatim (std-split-sqlite); logic unchanged.

use super::*;

impl SqliteStore {
    /// Append one group message to the raw log.
    ///
    /// Idempotent on `(bot_id, message_id)` so a re-delivered frame cannot double
    /// count. The log outlives pending_events: a turn deletes its pending rows, the
    /// log row stays and is only ever flagged archived.
    #[allow(clippy::too_many_arguments)]
    pub async fn insert_group_message(
        &self,
        bot_id: &str,
        group_id: &str,
        message_id: &str,
        sender_id: &str,
        display_name: Option<&str>,
        speaker_role: &str,
        text: &str,
        has_image: bool,
        sticker_hash: Option<&str>,
        sub_type: Option<&str>,
        now: i64,
    ) -> BotResult<bool> {
        let res = sqlx::query(
            "INSERT INTO group_messages
                (bot_id, group_id, message_id, sender_id, display_name, speaker_role,
                 text, has_image, sticker_hash, sub_type, created_at, archived)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, 0)
             ON CONFLICT (bot_id, message_id) DO NOTHING",
        )
        .bind(bot_id)
        .bind(group_id)
        .bind(message_id)
        .bind(sender_id)
        .bind(display_name)
        .bind(speaker_role)
        .bind(text)
        .bind(i64::from(has_image))
        .bind(sticker_hash)
        .bind(sub_type)
        .bind(now)
        .execute(&self.pool)
        .await?;
        Ok(res.rows_affected() == 1)
    }

    /// The most recent display name the log has for one member of a group.
    ///
    /// Used to name an @ target: the member table keeps only the alias, so
    /// the name has to come from the member's own logged lines. `None` when
    /// the member has never been logged speaking in this group.
    pub async fn member_display_name(
        &self,
        bot_id: &str,
        group_id: &str,
        user_id: &str,
    ) -> BotResult<Option<String>> {
        let row = sqlx::query(
            "SELECT display_name FROM group_messages
             WHERE bot_id = ? AND group_id = ? AND sender_id = ?
               AND display_name IS NOT NULL AND display_name <> ''
             ORDER BY id DESC LIMIT 1",
        )
        .bind(bot_id)
        .bind(group_id)
        .bind(user_id)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(|r| r.get::<String, _>("display_name")))
    }

    /// Attach the tagger's descriptions to an already logged message.
    ///
    /// `insert_group_message` runs in the intake hot path, before any image is
    /// downloaded, so the column cannot be filled there. A missing row is silently
    /// ignored: the tag is a cache for `memory_search`, not a fact about the message.
    pub async fn set_message_image_tags(
        &self,
        bot_id: &str,
        group_id: &str,
        message_id: &str,
        tags_json: &str,
    ) -> BotResult<bool> {
        let res = sqlx::query(
            "UPDATE group_messages SET image_tags = ?
             WHERE bot_id = ? AND group_id = ? AND message_id = ?",
        )
        .bind(tags_json)
        .bind(bot_id)
        .bind(group_id)
        .bind(message_id)
        .execute(&self.pool)
        .await?;
        Ok(res.rows_affected() == 1)
    }

    /// The logged text of one message, by its OneBot message id.
    ///
    /// Used to show what an incoming quote points at: the id comes from the
    /// wire, so the summary is only ever read back from this group's own log
    /// — an id that was never logged here resolves to nothing.
    pub async fn group_message_text(
        &self,
        bot_id: &str,
        group_id: &str,
        message_id: &str,
    ) -> BotResult<Option<String>> {
        let row: Option<(String,)> = sqlx::query_as(
            "SELECT text FROM group_messages
             WHERE bot_id = ? AND group_id = ? AND message_id = ? LIMIT 1",
        )
        .bind(bot_id)
        .bind(group_id)
        .bind(message_id)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(|(text,)| text))
    }

    /// Send-time freshness gate: how many non-bot group messages were
    /// logged strictly after `after_ts` (unix seconds).
    pub async fn count_group_messages_after(
        &self,
        bot_id: &str,
        group_id: &str,
        after_ts: f64,
    ) -> BotResult<i64> {
        let row: (i64,) = sqlx::query_as(
            "SELECT COUNT(*) FROM group_messages
             WHERE bot_id = ? AND group_id = ? AND speaker_role != 'bot' AND created_at > ?",
        )
        .bind(bot_id)
        .bind(group_id)
        .bind(after_ts)
        .fetch_one(&self.pool)
        .await?;
        Ok(row.0)
    }

    /// When a logged group message was sent (unix seconds), if known.
    pub async fn group_message_created_at(
        &self,
        bot_id: &str,
        group_id: &str,
        message_id: &str,
    ) -> BotResult<Option<f64>> {
        let row: Option<(f64,)> = sqlx::query_as(
            "SELECT CAST(created_at AS REAL) FROM group_messages
             WHERE bot_id = ? AND group_id = ? AND message_id = ? LIMIT 1",
        )
        .bind(bot_id)
        .bind(group_id)
        .bind(message_id)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(|(ts,)| ts))
    }

    /// Texts this bot recently sent (or is about to send) in a group, newest
    /// first. Backs the send-time replay guard: a reply segment repeating one
    /// of these is the model echoing itself, not answering anyone.
    pub async fn recent_outbox_texts(
        &self,
        bot_id: &str,
        group_id: &str,
        since_ts: i64,
    ) -> BotResult<Vec<String>> {
        let rows: Vec<(String,)> = sqlx::query_as(
            "SELECT text FROM outbox
             WHERE bot_id = ? AND group_id = ? AND kind = 'text'
               AND state IN ('sent', 'pending') AND created_at >= ?
             ORDER BY created_at DESC LIMIT 200",
        )
        .bind(bot_id)
        .bind(group_id)
        .bind(since_ts)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.into_iter().map(|(text,)| text).collect())
    }

    /// When the bot last sent anything in a group strictly before
    /// `before_ts` (unix seconds), if it ever did. The resident quote guard
    /// uses the gap between this and a new batch's first message to spot the
    /// one-beat content lag behind a fresh quote.
    pub async fn last_outbox_sent_at(
        &self,
        bot_id: &str,
        group_id: &str,
        before_ts: i64,
    ) -> BotResult<Option<i64>> {
        let row: Option<(Option<i64>,)> = sqlx::query_as(
            "SELECT MAX(created_at) FROM outbox
             WHERE bot_id = ? AND group_id = ? AND state = 'sent' AND created_at < ?",
        )
        .bind(bot_id)
        .bind(group_id)
        .bind(before_ts)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.and_then(|(ts,)| ts))
    }

    /// Newest messages of a group, oldest-last, for the panel and for prompt context.
    pub async fn recent_group_messages(
        &self,
        bot_id: &str,
        group_id: &str,
        limit: i64,
    ) -> BotResult<Vec<GroupMessageRecord>> {
        let rows = sqlx::query(
            "SELECT id, bot_id, group_id, message_id, sender_id, display_name, speaker_role,
                    text, has_image, sticker_hash, sub_type, image_tags, created_at, archived
             FROM group_messages WHERE bot_id = ? AND group_id = ?
             ORDER BY id DESC LIMIT ?",
        )
        .bind(bot_id)
        .bind(group_id)
        .bind(limit)
        .fetch_all(&self.pool)
        .await?;
        let mut out: Vec<GroupMessageRecord> = rows.into_iter().map(map_group_message).collect();
        out.reverse();
        Ok(out)
    }

    /// Unarchived tail only, oldest-first. Used by memory_search L1 and the cadence gate.
    pub async fn recent_unarchived_messages(
        &self,
        bot_id: &str,
        group_id: &str,
        limit: i64,
    ) -> BotResult<Vec<GroupMessageRecord>> {
        let rows = sqlx::query(
            "SELECT id, bot_id, group_id, message_id, sender_id, display_name, speaker_role,
                    text, has_image, sticker_hash, sub_type, image_tags, created_at, archived
             FROM group_messages WHERE bot_id = ? AND group_id = ? AND archived = 0
             ORDER BY id DESC LIMIT ?",
        )
        .bind(bot_id)
        .bind(group_id)
        .bind(limit)
        .fetch_all(&self.pool)
        .await?;
        let mut out: Vec<GroupMessageRecord> = rows.into_iter().map(map_group_message).collect();
        out.reverse();
        Ok(out)
    }

    /// One page of the raw log, newest first, plus the total for the pager.
    pub async fn group_messages_page(
        &self,
        bot_id: &str,
        group_id: &str,
        limit: i64,
        offset: i64,
    ) -> BotResult<Vec<GroupMessageRecord>> {
        let rows = sqlx::query(
            "SELECT id, bot_id, group_id, message_id, sender_id, display_name, speaker_role,
                    text, has_image, sticker_hash, sub_type, image_tags, created_at, archived
             FROM group_messages WHERE bot_id = ? AND group_id = ?
             ORDER BY id DESC LIMIT ? OFFSET ?",
        )
        .bind(bot_id)
        .bind(group_id)
        .bind(limit)
        .bind(offset)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.into_iter().map(map_group_message).collect())
    }

    pub async fn count_group_messages(&self, bot_id: &str, group_id: &str) -> BotResult<i64> {
        let count: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM group_messages WHERE bot_id = ? AND group_id = ?",
        )
        .bind(bot_id)
        .bind(group_id)
        .fetch_one(&self.pool)
        .await?;
        Ok(count)
    }

    /// How many rows are not yet folded into a chunk. This is the compaction trigger.
    pub async fn unarchived_message_count(&self, bot_id: &str, group_id: &str) -> BotResult<i64> {
        let count: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM group_messages
             WHERE bot_id = ? AND group_id = ? AND archived = 0",
        )
        .bind(bot_id)
        .bind(group_id)
        .fetch_one(&self.pool)
        .await?;
        Ok(count)
    }

    /// The oldest `limit` unarchived rows: exactly the window handed to Aux.
    pub async fn oldest_unarchived_messages(
        &self,
        bot_id: &str,
        group_id: &str,
        limit: i64,
    ) -> BotResult<Vec<GroupMessageRecord>> {
        let rows = sqlx::query(
            "SELECT id, bot_id, group_id, message_id, sender_id, display_name, speaker_role,
                    text, has_image, sticker_hash, sub_type, image_tags, created_at, archived
             FROM group_messages WHERE bot_id = ? AND group_id = ? AND archived = 0
             ORDER BY id LIMIT ?",
        )
        .bind(bot_id)
        .bind(group_id)
        .bind(limit)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.into_iter().map(map_group_message).collect())
    }

    /// Drop archived rows beyond the newest `keep` in one group.
    ///
    /// Archived rows already have their content in `memory_chunks`, so the raw text is
    /// redundant once it is old enough. The recent tail is never touched: it is what the
    /// cadence gate and the folder both read. Returns how many rows went.
    pub async fn trim_archived_messages(
        &self,
        bot_id: &str,
        group_id: &str,
        keep: i64,
        now: i64,
    ) -> BotResult<u64> {
        if keep < 0 {
            return Ok(0);
        }
        let res = sqlx::query(
            "DELETE FROM group_messages
             WHERE bot_id = ? AND group_id = ? AND archived = 1
               AND id NOT IN (
                   SELECT id FROM group_messages
                   WHERE bot_id = ? AND group_id = ? AND archived = 1
                   ORDER BY id DESC LIMIT ?
               )",
        )
        .bind(bot_id)
        .bind(group_id)
        .bind(bot_id)
        .bind(group_id)
        .bind(keep)
        .execute(&self.pool)
        .await?;
        let _ = now;
        Ok(res.rows_affected())
    }

    /// Flag a window as folded. Never deletes: a failed summary must stay retryable.
    pub async fn archive_group_messages(&self, ids: &[i64]) -> BotResult<u64> {
        if ids.is_empty() {
            return Ok(0);
        }
        let mut tx = self.pool.begin().await?;
        let mut total = 0u64;
        for id in ids {
            let res = sqlx::query("UPDATE group_messages SET archived = 1 WHERE id = ?")
                .bind(id)
                .execute(&mut *tx)
                .await?;
            total += res.rows_affected();
        }
        tx.commit().await?;
        Ok(total)
    }
}
