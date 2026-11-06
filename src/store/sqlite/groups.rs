//! Groups and members for SqliteStore: construction, group records and flags, member registry.
//! Moved out of sqlite.rs verbatim (std-split-sqlite); logic unchanged.

use super::*;

impl SqliteStore {
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }

    pub fn pool(&self) -> &SqlitePool {
        &self.pool
    }

    // ---------------------------------------------------------------- groups

    /// Ensure the group exists. A first-seen group is **disabled**: joining a group must
    /// not silently start answering in it. An operator enables it from the panel.
    ///
    /// Inserts default **on** for a first-seen group (2026-11-04, a newly joined group):
    /// with inserts off, a busy group the operator has just enabled replies a full
    /// turn behind — every mid-turn arrival waits for the next batch — which reads
    /// as a broken reply queue. Opting a group out stays available via
    /// `set_group_flags`.
    ///
    /// Returns `true` when the row was created by this call, so the caller can record
    /// the discovery exactly once.
    pub async fn ensure_group(&self, bot_id: &str, group_id: &str, now: i64) -> BotResult<bool> {
        let done = sqlx::query(
            "INSERT INTO groups (bot_id, group_id, enabled, paused, resident_insert_enabled, prompt_version, memory_revision, created_at, updated_at)
             VALUES (?, ?, 0, 0, 1, 1, 0, ?, ?)
             ON CONFLICT (bot_id, group_id) DO NOTHING",
        )
        .bind(bot_id)
        .bind(group_id)
        .bind(now)
        .bind(now)
        .execute(&self.pool)
        .await?;
        Ok(done.rows_affected() == 1)
    }

    /// Record the descriptive fields fetched from OneBot at discovery
    /// time. Also clears the departure marker: a group the bot can be
    /// seen in is not departed.
    pub async fn set_group_info(
        &self,
        bot_id: &str,
        group_id: &str,
        name: Option<&str>,
        member_count: Option<i64>,
        now: i64,
    ) -> BotResult<()> {
        sqlx::query(
            "UPDATE groups SET name = ?, member_count = ?, departed_at = NULL, updated_at = ?
             WHERE bot_id = ? AND group_id = ?",
        )
        .bind(name)
        .bind(member_count)
        .bind(now)
        .bind(bot_id)
        .bind(group_id)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Mark (or clear, with `None`) the group's departure.
    pub async fn set_group_departed(
        &self,
        bot_id: &str,
        group_id: &str,
        departed_at: Option<i64>,
        now: i64,
    ) -> BotResult<()> {
        sqlx::query(
            "UPDATE groups SET departed_at = ?, updated_at = ? WHERE bot_id = ? AND group_id = ?",
        )
        .bind(departed_at)
        .bind(now)
        .bind(bot_id)
        .bind(group_id)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn get_group(&self, bot_id: &str, group_id: &str) -> BotResult<Option<GroupRecord>> {
        let row = sqlx::query(
            "SELECT bot_id, group_id, enabled, paused, agy_conversation_id, agy_turn_count,
                    resident_insert_enabled, ambient_pace, prompt_version, memory_revision,
                    name, member_count, departed_at, created_at, updated_at
             FROM groups WHERE bot_id = ? AND group_id = ?",
        )
        .bind(bot_id)
        .bind(group_id)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(|r| GroupRecord {
            bot_id: r.get("bot_id"),
            group_id: r.get("group_id"),
            enabled: r.get::<i64, _>("enabled") != 0,
            paused: r.get::<i64, _>("paused") != 0,
            agy_conversation_id: r.get("agy_conversation_id"),
            agy_turn_count: r.get("agy_turn_count"),
            resident_insert_enabled: r.get::<i64, _>("resident_insert_enabled") != 0,
            ambient_pace: r.get("ambient_pace"),
            prompt_version: r.get("prompt_version"),
            memory_revision: r.get("memory_revision"),
            name: r.get("name"),
            member_count: r.get("member_count"),
            departed_at: r.get("departed_at"),
            created_at: r.get("created_at"),
            updated_at: r.get("updated_at"),
        }))
    }

    pub async fn list_groups(&self, bot_id: &str) -> BotResult<Vec<GroupRecord>> {
        let rows = sqlx::query(
            "SELECT bot_id, group_id, enabled, paused, agy_conversation_id, agy_turn_count,
                    resident_insert_enabled, ambient_pace, prompt_version, memory_revision,
                    name, member_count, departed_at, created_at, updated_at
             FROM groups WHERE bot_id = ? ORDER BY group_id",
        )
        .bind(bot_id)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .into_iter()
            .map(|r| GroupRecord {
                bot_id: r.get("bot_id"),
                group_id: r.get("group_id"),
                enabled: r.get::<i64, _>("enabled") != 0,
                paused: r.get::<i64, _>("paused") != 0,
                agy_conversation_id: r.get("agy_conversation_id"),
                agy_turn_count: r.get("agy_turn_count"),
                resident_insert_enabled: r.get::<i64, _>("resident_insert_enabled") != 0,
                ambient_pace: r.get("ambient_pace"),
                prompt_version: r.get("prompt_version"),
                memory_revision: r.get("memory_revision"),
                name: r.get("name"),
                member_count: r.get("member_count"),
                departed_at: r.get("departed_at"),
                created_at: r.get("created_at"),
                updated_at: r.get("updated_at"),
            })
            .collect())
    }

    /// Update only the four mutable flags. Anything else is rejected by the caller.
    ///
    /// `resident_insert_enabled` is the remaining per-group switch (on by default
    /// at discovery, see `ensure_group`): the resident
    /// session itself has no switch any more (it is the only execution path, so a
    /// group cannot be taken off it), but a running turn may still be told not to
    /// absorb newly claimed batches. It stays on this audited surface so the opt-out
    /// cannot be flipped outside the admin API.
    #[allow(clippy::too_many_arguments)]
    pub async fn set_group_flags(
        &self,
        bot_id: &str,
        group_id: &str,
        enabled: Option<bool>,
        paused: Option<bool>,
        resident_insert_enabled: Option<bool>,
        now: i64,
    ) -> BotResult<bool> {
        let mut tx = self.pool.begin().await?;
        if let Some(v) = enabled {
            sqlx::query(
                "UPDATE groups SET enabled = ?, updated_at = ? WHERE bot_id = ? AND group_id = ?",
            )
            .bind(i64::from(v))
            .bind(now)
            .bind(bot_id)
            .bind(group_id)
            .execute(&mut *tx)
            .await?;
        }
        if let Some(v) = paused {
            sqlx::query(
                "UPDATE groups SET paused = ?, updated_at = ? WHERE bot_id = ? AND group_id = ?",
            )
            .bind(i64::from(v))
            .bind(now)
            .bind(bot_id)
            .bind(group_id)
            .execute(&mut *tx)
            .await?;
        }
        if let Some(v) = resident_insert_enabled {
            sqlx::query(
                "UPDATE groups SET resident_insert_enabled = ?, updated_at = ?
                 WHERE bot_id = ? AND group_id = ?",
            )
            .bind(i64::from(v))
            .bind(now)
            .bind(bot_id)
            .bind(group_id)
            .execute(&mut *tx)
            .await?;
        }
        tx.commit().await?;
        let exists = sqlx::query("SELECT 1 FROM groups WHERE bot_id = ? AND group_id = ?")
            .bind(bot_id)
            .bind(group_id)
            .fetch_optional(&self.pool)
            .await?
            .is_some();
        Ok(exists)
    }

    /// Set the group's ambient pace tier (the panel's reply-frequency
    /// control). Returns false when the group does not exist. The value
    /// is stored verbatim: the admin layer validates it against the
    /// tier list, and readers fall back to normal on unknown values.
    pub async fn set_group_ambient_pace(
        &self,
        bot_id: &str,
        group_id: &str,
        pace: &str,
        now: i64,
    ) -> BotResult<bool> {
        let done = sqlx::query(
            "UPDATE groups SET ambient_pace = ?, updated_at = ?
             WHERE bot_id = ? AND group_id = ?",
        )
        .bind(pace)
        .bind(now)
        .bind(bot_id)
        .bind(group_id)
        .execute(&self.pool)
        .await?;
        Ok(done.rows_affected() == 1)
    }

    pub async fn set_conversation_id(
        &self,
        bot_id: &str,
        group_id: &str,
        conversation_id: Option<&str>,
        now: i64,
    ) -> BotResult<()> {
        if conversation_id.is_none() {
            sqlx::query(
                "UPDATE groups SET agy_conversation_id = NULL, agy_turn_count = 0, updated_at = ?
                 WHERE bot_id = ? AND group_id = ?",
            )
            .bind(now)
            .bind(bot_id)
            .bind(group_id)
            .execute(&self.pool)
            .await?;
        } else {
            sqlx::query(
                "UPDATE groups SET agy_conversation_id = ?, updated_at = ? WHERE bot_id = ? AND group_id = ?",
            )
            .bind(conversation_id)
            .bind(now)
            .bind(bot_id)
            .bind(group_id)
            .execute(&self.pool)
            .await?;
        }
        Ok(())
    }

    /// Increment the successful-turn counter. Returns the new count.
    pub async fn bump_agy_turn_count(
        &self,
        bot_id: &str,
        group_id: &str,
        now: i64,
    ) -> BotResult<i64> {
        let count: i64 = sqlx::query_scalar(
            "UPDATE groups SET agy_turn_count = agy_turn_count + 1, updated_at = ?
             WHERE bot_id = ? AND group_id = ?
             RETURNING agy_turn_count",
        )
        .bind(now)
        .bind(bot_id)
        .bind(group_id)
        .fetch_one(&self.pool)
        .await?;
        Ok(count)
    }

    // --------------------------------------------------------------- members

    /// Return the stable per-group alias for `user_id`, creating it if needed.
    ///
    /// Aliases are `u1`, `u2`, … and are unique inside one group only: two groups
    /// may both contain `u1` and they refer to different people.
    pub async fn ensure_member(
        &self,
        bot_id: &str,
        group_id: &str,
        user_id: &str,
        now: i64,
    ) -> BotResult<String> {
        let mut tx = self.pool.begin().await?;
        if let Some(existing) = sqlx::query(
            "SELECT speaker_ref FROM group_members WHERE bot_id = ? AND group_id = ? AND user_id = ?",
        )
        .bind(bot_id)
        .bind(group_id)
        .bind(user_id)
        .fetch_optional(&mut *tx)
        .await?
        {
            let speaker_ref: String = existing.get("speaker_ref");
            sqlx::query(
                "UPDATE group_members SET last_seen_at = ? WHERE bot_id = ? AND group_id = ? AND user_id = ?",
            )
            .bind(now)
            .bind(bot_id)
            .bind(group_id)
            .bind(user_id)
            .execute(&mut *tx)
            .await?;
            tx.commit().await?;
            return Ok(speaker_ref);
        }

        let refs: Vec<String> = sqlx::query_scalar(
            "SELECT speaker_ref FROM group_members WHERE bot_id = ? AND group_id = ?",
        )
        .bind(bot_id)
        .bind(group_id)
        .fetch_all(&mut *tx)
        .await?;
        let next = refs
            .iter()
            .filter_map(|r| r.strip_prefix('u').and_then(|n| n.parse::<u32>().ok()))
            .max()
            .unwrap_or(0)
            + 1;
        let speaker_ref = format!("u{next}");

        sqlx::query(
            "INSERT INTO group_members (bot_id, group_id, user_id, speaker_ref, first_seen_at, last_seen_at)
             VALUES (?, ?, ?, ?, ?, ?)",
        )
        .bind(bot_id)
        .bind(group_id)
        .bind(user_id)
        .bind(&speaker_ref)
        .bind(now)
        .bind(now)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(speaker_ref)
    }

    pub async fn member_by_ref(
        &self,
        bot_id: &str,
        group_id: &str,
        speaker_ref: &str,
    ) -> BotResult<Option<MemberRecord>> {
        let row = sqlx::query(
            "SELECT bot_id, group_id, user_id, speaker_ref, first_seen_at, last_seen_at
             FROM group_members WHERE bot_id = ? AND group_id = ? AND speaker_ref = ?",
        )
        .bind(bot_id)
        .bind(group_id)
        .bind(speaker_ref)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(|r| MemberRecord {
            bot_id: r.get("bot_id"),
            group_id: r.get("group_id"),
            user_id: r.get("user_id"),
            speaker_ref: r.get("speaker_ref"),
            first_seen_at: r.get("first_seen_at"),
            last_seen_at: r.get("last_seen_at"),
        }))
    }

    pub async fn list_members(&self, bot_id: &str, group_id: &str) -> BotResult<Vec<MemberRecord>> {
        let rows = sqlx::query(
            "SELECT bot_id, group_id, user_id, speaker_ref, first_seen_at, last_seen_at
             FROM group_members WHERE bot_id = ? AND group_id = ? ORDER BY first_seen_at, speaker_ref",
        )
        .bind(bot_id)
        .bind(group_id)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .into_iter()
            .map(|r| MemberRecord {
                bot_id: r.get("bot_id"),
                group_id: r.get("group_id"),
                user_id: r.get("user_id"),
                speaker_ref: r.get("speaker_ref"),
                first_seen_at: r.get("first_seen_at"),
                last_seen_at: r.get("last_seen_at"),
            })
            .collect())
    }
}
