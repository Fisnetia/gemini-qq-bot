//! Audit trail and persona storage for SqliteStore.
//! Moved out of sqlite.rs verbatim (std-split-sqlite); logic unchanged.

use super::*;

impl SqliteStore {
    pub async fn audit(
        &self,
        actor_kind: &str,
        action: &str,
        bot_id: Option<&str>,
        group_id: Option<&str>,
        now: i64,
    ) -> BotResult<()> {
        sqlx::query(
            "INSERT INTO audit_events (actor_kind, action, bot_id, group_id, created_at)
             VALUES (?, ?, ?, ?, ?)",
        )
        .bind(actor_kind)
        .bind(action)
        .bind(bot_id)
        .bind(group_id)
        .bind(now)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn recent_audit(&self, limit: i64) -> BotResult<Vec<AuditEvent>> {
        self.recent_audit_page(limit, 0).await
    }

    /// One page of audit events, newest first.
    pub async fn recent_audit_page(&self, limit: i64, offset: i64) -> BotResult<Vec<AuditEvent>> {
        let rows = sqlx::query(
            "SELECT id, actor_kind, action, bot_id, group_id, created_at
             FROM audit_events ORDER BY id DESC LIMIT ? OFFSET ?",
        )
        .bind(limit)
        .bind(offset)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .into_iter()
            .map(|r| AuditEvent {
                id: r.get("id"),
                actor_kind: r.get("actor_kind"),
                action: r.get("action"),
                bot_id: r.get("bot_id"),
                group_id: r.get("group_id"),
                created_at: r.get("created_at"),
            })
            .collect())
    }

    // ---------------------------------------------------------------- persona

    pub async fn get_persona(&self) -> BotResult<Option<String>> {
        let text: Option<String> =
            sqlx::query_scalar("SELECT text FROM persona_config WHERE id = 'default'")
                .fetch_optional(&self.pool)
                .await?;
        Ok(text)
    }

    /// Remove the stored card, restoring the compiled-in default.
    pub async fn delete_persona(&self) -> BotResult<()> {
        sqlx::query("DELETE FROM persona_config WHERE id = 'default'")
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    pub async fn put_persona(&self, text: &str, now: i64) -> BotResult<()> {
        sqlx::query(
            "INSERT INTO persona_config (id, text, updated_at) VALUES ('default', ?, ?)
             ON CONFLICT (id) DO UPDATE SET text = excluded.text, updated_at = excluded.updated_at",
        )
        .bind(text)
        .bind(now)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// The owner's private-chat card. `None` means private chat uses the group card.
    pub async fn get_private_persona(&self) -> BotResult<Option<String>> {
        let text: Option<String> =
            sqlx::query_scalar("SELECT text FROM persona_config WHERE id = 'private'")
                .fetch_optional(&self.pool)
                .await?;
        Ok(text)
    }

    pub async fn delete_private_persona(&self) -> BotResult<()> {
        sqlx::query("DELETE FROM persona_config WHERE id = 'private'")
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    pub async fn put_private_persona(&self, text: &str, now: i64) -> BotResult<()> {
        sqlx::query(
            "INSERT INTO persona_config (id, text, updated_at) VALUES ('private', ?, ?)
             ON CONFLICT (id) DO UPDATE SET text = excluded.text, updated_at = excluded.updated_at",
        )
        .bind(text)
        .bind(now)
        .execute(&self.pool)
        .await?;
        Ok(())
    }
}
