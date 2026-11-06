//! Sticker library persistence for SqliteStore: recognition cache upserts, phash/semantic candidates, shelf management.
//! Moved out of sqlite.rs verbatim (std-split-sqlite); logic unchanged.

use super::*;

impl SqliteStore {
    /// Insert or refresh one sticker in the recognition cache. Returns true when the row
    /// is new.
    ///
    /// The cache is bot-wide, not group-scoped: one sha256 is one row no matter how many
    /// groups posted it. A reposted sticker therefore costs zero tagging tokens, which is
    /// the entire point of caching.
    ///
    /// The conflict branch only fills in metadata that a later tagging pass may have
    /// improved. It deliberately does NOT touch `kept`: a model improving its description
    /// must never silently add something to her library, and must never resurrect a
    /// sticker she already refused.
    ///
    /// It also does not touch `seen_count` / `last_seen_at`: bumping those is the dedup
    /// gates' job ([`Self::bump_sticker_seen`]), so "this picture was posted again" has
    /// exactly one writer.
    #[allow(clippy::too_many_arguments)]
    pub async fn upsert_sticker(
        &self,
        hash: &str,
        bot_id: &str,
        group_id: &str,
        file_path: &str,
        kind: &str,
        is_self: &str,
        ocr: &str,
        description: &str,
        emotion: &str,
        search_blob: &str,
        phash: Option<i64>,
        nsfw: bool,
        enabled: bool,
        now: i64,
    ) -> BotResult<bool> {
        let res = sqlx::query(
            "INSERT INTO stickers
                (hash, bot_id, group_id, file_path, kind, is_self, ocr, description,
                 emotion, search_blob, phash, last_seen_at, seen_count, nsfw, enabled,
                 kept, created_at)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, NULL, 1, ?, ?, 0, ?)
             ON CONFLICT (hash) DO UPDATE SET
                ocr = CASE WHEN excluded.ocr <> '' THEN excluded.ocr ELSE stickers.ocr END,
                description = CASE WHEN excluded.description <> '' THEN excluded.description ELSE stickers.description END,
                emotion = CASE WHEN excluded.emotion <> '' THEN excluded.emotion ELSE stickers.emotion END,
                search_blob = CASE WHEN excluded.search_blob <> '' THEN excluded.search_blob ELSE stickers.search_blob END,
                phash = COALESCE(excluded.phash, stickers.phash)",
        )
        .bind(hash)
        .bind(bot_id)
        .bind(group_id)
        .bind(file_path)
        .bind(kind)
        .bind(is_self)
        .bind(ocr)
        .bind(description)
        .bind(emotion)
        .bind(search_blob)
        .bind(phash)
        .bind(i64::from(nsfw))
        .bind(i64::from(enabled))
        .bind(now)
        .execute(&self.pool)
        .await?;
        Ok(res.rows_affected() == 1)
    }

    /// Count one more sighting of a picture we already have.
    ///
    /// This is the whole effect of a dedup hit: no new row, no tagging call, and a
    /// counter that later tells the panel which stickers keep resurfacing. `description`
    /// is only overwritten when the incoming one is longer, which is the semantic gate's
    /// "keep the better paragraph" rule.
    pub async fn bump_sticker_seen(
        &self,
        bot_id: &str,
        hash: &str,
        description: Option<&str>,
        now: i64,
    ) -> BotResult<bool> {
        let res = sqlx::query(
            "UPDATE stickers
                SET seen_count = seen_count + 1,
                    last_seen_at = ?,
                    description = CASE
                        WHEN ? IS NOT NULL AND length(?) > length(description) THEN ?
                        ELSE description
                    END
              WHERE hash = ? AND bot_id = ?",
        )
        .bind(now)
        .bind(description)
        .bind(description)
        .bind(description)
        .bind(hash)
        .bind(bot_id)
        .execute(&self.pool)
        .await?;
        Ok(res.rows_affected() == 1)
    }

    /// Rows of one group that carry a perceptual hash, newest first: the candidate set
    /// for the byte-independent dedup gate.
    ///
    /// The stored `description` rides along because the gate no longer trusts the hash
    /// on its own: each hit is checked against the text a previous tagging call wrote,
    /// and one extra column is cheaper than a per-candidate row lookup.
    pub async fn sticker_phash_candidates(
        &self,
        bot_id: &str,
        group_id: &str,
        limit: i64,
    ) -> BotResult<Vec<(String, i64, String)>> {
        let rows: Vec<(String, i64, String)> = sqlx::query_as(
            "SELECT hash, phash, description FROM stickers
              WHERE bot_id = ? AND group_id = ? AND phash IS NOT NULL AND enabled = 1
              ORDER BY created_at DESC, hash ASC LIMIT ?",
        )
        .bind(bot_id)
        .bind(group_id)
        .bind(limit)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows)
    }

    /// Rows of one group that carry visible text, newest first: the candidate set for
    /// the semantic dedup gate.
    pub async fn sticker_semantic_candidates(
        &self,
        bot_id: &str,
        group_id: &str,
        limit: i64,
    ) -> BotResult<Vec<(String, String, String)>> {
        let rows: Vec<(String, String, String)> = sqlx::query_as(
            "SELECT hash, ocr, description FROM stickers
              WHERE bot_id = ? AND group_id = ? AND ocr <> '' AND enabled = 1
              ORDER BY created_at DESC, hash ASC LIMIT ?",
        )
        .bind(bot_id)
        .bind(group_id)
        .bind(limit)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows)
    }

    /// Look up one sticker by hash in the bot-wide cache.
    ///
    /// Not group-scoped: the cache exists to answer "did we already tag these bytes",
    /// and the answer does not depend on which group asked.
    pub async fn sticker_by_hash(
        &self,
        bot_id: &str,
        hash: &str,
    ) -> BotResult<Option<StickerRecord>> {
        let row = sqlx::query(STICKER_COLUMNS)
            .bind(hash)
            .bind(bot_id)
            .fetch_optional(&self.pool)
            .await?;
        Ok(row.map(map_sticker))
    }

    /// Resolve a short handle to its one full hash.
    ///
    /// The model sees truncated handles like `a1b2c3d4`, not 64-char hashes, so a lookup
    /// has to go the other way. Only an unambiguous single match is returned: if a prefix
    /// names two rows the caller gets `None` and must not guess.
    pub async fn find_sticker_by_prefix(
        &self,
        bot_id: &str,
        prefix: &str,
    ) -> BotResult<Option<String>> {
        let prefix = prefix.trim().to_lowercase();
        if prefix.is_empty() {
            return Ok(None);
        }
        let rows: Vec<(String,)> =
            sqlx::query_as("SELECT hash FROM stickers WHERE bot_id = ? AND hash LIKE ? LIMIT 2")
                .bind(bot_id)
                .bind(format!("{prefix}%"))
                .fetch_all(&self.pool)
                .await?;
        match rows.len() {
            1 => Ok(Some(rows[0].0.clone())),
            _ => Ok(None),
        }
    }

    /// Paged list for the panel: everything the tagger has ever seen, newest first.
    /// `kept` filters the shelf: `Some(1)` is her library, `Some(0)` unjudged,
    /// `Some(-1)` refused, `None` everything.
    pub async fn list_stickers_page(
        &self,
        bot_id: &str,
        kept: Option<i64>,
        limit: i64,
        offset: i64,
    ) -> BotResult<Vec<StickerRecord>> {
        let rows = sqlx::query(
            "SELECT hash, bot_id, group_id, file_path, kind, is_self, ocr, description,
                    scene, emotion, intent, usage_hint, search_blob, phash, last_seen_at,
                    seen_count, nsfw, enabled, kept, created_at
             FROM stickers
             WHERE bot_id = ? AND (? IS NULL OR kept = ?)
             ORDER BY created_at DESC, hash ASC LIMIT ? OFFSET ?",
        )
        .bind(bot_id)
        .bind(kept)
        .bind(kept)
        .bind(limit)
        .bind(offset)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.into_iter().map(map_sticker).collect())
    }

    /// Her actual library: kept by her and switched on. The only set she may send from.
    pub async fn list_library(&self, bot_id: &str, limit: i64) -> BotResult<Vec<StickerRecord>> {
        let rows = sqlx::query(
            "SELECT hash, bot_id, group_id, file_path, kind, is_self, ocr, description,
                    scene, emotion, intent, usage_hint, search_blob, phash, last_seen_at,
                    seen_count, nsfw, enabled, kept, created_at
             FROM stickers WHERE bot_id = ? AND kept = 1 AND enabled = 1
             ORDER BY created_at DESC LIMIT ?",
        )
        .bind(bot_id)
        .bind(limit)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.into_iter().map(map_sticker).collect())
    }

    /// Record her verdict on a cached sticker. Returns false when the hash is unknown.
    ///
    /// This is the only way a row enters her library, so a sticker she never judged stays
    /// cached-but-unofferable instead of leaking into the shelf.
    pub async fn set_sticker_kept(&self, bot_id: &str, hash: &str, kept: i64) -> BotResult<bool> {
        let res = sqlx::query("UPDATE stickers SET kept = ? WHERE hash = ? AND bot_id = ?")
            .bind(kept)
            .bind(hash)
            .bind(bot_id)
            .execute(&self.pool)
            .await?;
        Ok(res.rows_affected() == 1)
    }

    /// Flip enabled. Returns false when the hash is unknown.
    pub async fn set_sticker_enabled(
        &self,
        bot_id: &str,
        hash: &str,
        enabled: bool,
    ) -> BotResult<bool> {
        let res = sqlx::query("UPDATE stickers SET enabled = ? WHERE hash = ? AND bot_id = ?")
            .bind(i64::from(enabled))
            .bind(hash)
            .bind(bot_id)
            .execute(&self.pool)
            .await?;
        Ok(res.rows_affected() == 1)
    }

    /// Delete one sticker row. Returns the file path so the caller can unlink it.
    ///
    /// Deletion is a panel action only: an operator removing a sticker means removing the
    /// cache entry too, so a later repost re-tags it and she gets a fresh chance to judge
    /// it.
    pub async fn delete_sticker(&self, bot_id: &str, hash: &str) -> BotResult<Option<String>> {
        let row: Option<(String,)> = sqlx::query_as(
            "DELETE FROM stickers WHERE hash = ? AND bot_id = ? RETURNING file_path",
        )
        .bind(hash)
        .bind(bot_id)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(|r| r.0))
    }
}
