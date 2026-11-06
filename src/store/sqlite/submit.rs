//! MCP submit channel persistence: armed turns and turn submissions for SqliteStore.
//! Moved out of sqlite.rs verbatim (std-split-sqlite); logic unchanged.

use super::*;

// ------------------------------------------------------------ MCP submit

impl SqliteStore {
    /// Arm a turn for `submit_reply`. One live arm per group; re-arming
    /// replaces the previous row and returns it, so the caller can audit an
    /// overwritten arm whose submissions were never consumed.
    pub async fn arm_turn(
        &self,
        group_id: &str,
        turn_token: &str,
        batch_id: &str,
        armed_at: i64,
        expires_at: i64,
    ) -> BotResult<Option<ArmedTurn>> {
        let previous = self.armed_turn(group_id).await?;
        sqlx::query(
            "INSERT INTO armed_turns (group_id, turn_token, batch_id, armed_at, expires_at)
             VALUES (?, ?, ?, ?, ?)
             ON CONFLICT(group_id) DO UPDATE SET
                turn_token = excluded.turn_token,
                batch_id = excluded.batch_id,
                armed_at = excluded.armed_at,
                expires_at = excluded.expires_at",
        )
        .bind(group_id)
        .bind(turn_token)
        .bind(batch_id)
        .bind(armed_at)
        .bind(expires_at)
        .execute(&self.pool)
        .await?;
        Ok(previous)
    }

    /// The group's live arm, if any.
    pub async fn armed_turn(&self, group_id: &str) -> BotResult<Option<ArmedTurn>> {
        let row = sqlx::query(
            "SELECT group_id, turn_token, batch_id, armed_at, expires_at
             FROM armed_turns WHERE group_id = ?",
        )
        .bind(group_id)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(|r| ArmedTurn {
            group_id: r.get("group_id"),
            turn_token: r.get("turn_token"),
            batch_id: r.get("batch_id"),
            armed_at: r.get("armed_at"),
            expires_at: r.get("expires_at"),
        }))
    }

    /// Record one `submit_reply` call; returns its per-token sequence number.
    /// Sequence allocation and insert run in one statement pair on the pool;
    /// calls under one token come from a single agent session and are
    /// sequential, so MAX+1 cannot interleave in practice.
    pub async fn record_submission(
        &self,
        group_id: &str,
        turn_token: &str,
        payload: &str,
        verdict: &str,
        reason: &str,
        now: i64,
    ) -> BotResult<i64> {
        let seq: i64 = sqlx::query_scalar(
            "SELECT COALESCE(MAX(seq), 0) + 1 FROM turn_submissions WHERE turn_token = ?",
        )
        .bind(turn_token)
        .fetch_one(&self.pool)
        .await?;
        sqlx::query(
            "INSERT INTO turn_submissions
                (group_id, turn_token, seq, payload, verdict, reason, created_at)
             VALUES (?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(group_id)
        .bind(turn_token)
        .bind(seq)
        .bind(payload)
        .bind(verdict)
        .bind(reason)
        .bind(now)
        .execute(&self.pool)
        .await?;
        Ok(seq)
    }

    /// Every recorded call under a token, in call order.
    pub async fn submissions_for(&self, turn_token: &str) -> BotResult<Vec<TurnSubmission>> {
        let rows = sqlx::query(
            "SELECT seq, payload, verdict, reason, created_at
             FROM turn_submissions WHERE turn_token = ? ORDER BY seq ASC",
        )
        .bind(turn_token)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .iter()
            .map(|r| TurnSubmission {
                seq: r.get("seq"),
                payload: r.get("payload"),
                verdict: r.get("verdict"),
                reason: r.get("reason"),
                created_at: r.get("created_at"),
            })
            .collect())
    }

    /// Drop a group's arm once its turn is over. Token-checked, so a stale
    /// disarm can never kill a newer turn's arm.
    pub async fn disarm_turn(&self, group_id: &str, turn_token: &str) -> BotResult<()> {
        sqlx::query("DELETE FROM armed_turns WHERE group_id = ? AND turn_token = ?")
            .bind(group_id)
            .bind(turn_token)
            .execute(&self.pool)
            .await?;
        Ok(())
    }
}
