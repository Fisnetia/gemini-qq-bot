//! Migration runner.
//!
//! Migrations are embedded so the container image never needs the source tree at
//! runtime. SQLx tracks applied versions in `_sqlx_migrations`.

use std::path::Path;

use sqlx::SqlitePool;
use sqlx::migrate::Migrator;
use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions, SqliteSynchronous};

use crate::error::BotResult;

/// Embedded migrations from `migrations/`.
pub static MIGRATOR: Migrator = sqlx::migrate!("./migrations");

/// Open (creating if needed) the SQLite database with the operational pragmas the
/// design requires: WAL, foreign keys, a busy timeout and normal sync.
pub async fn open_pool(path: &Path) -> BotResult<SqlitePool> {
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        std::fs::create_dir_all(parent)?;
    }

    let options = SqliteConnectOptions::new()
        .filename(path)
        .create_if_missing(true)
        .journal_mode(SqliteJournalMode::Wal)
        .synchronous(SqliteSynchronous::Normal)
        .foreign_keys(true)
        .busy_timeout(std::time::Duration::from_secs(10));

    let pool = SqlitePoolOptions::new()
        .max_connections(8)
        .connect_with(options)
        .await?;
    Ok(pool)
}

/// Run all pending migrations.
pub async fn run(pool: &SqlitePool) -> BotResult<()> {
    MIGRATOR
        .run(pool)
        .await
        .map_err(|e| crate::error::BotError::Message(format!("数据库迁移失败: {e}")))?;
    Ok(())
}

/// Open and migrate in one step.
pub async fn open_and_migrate(path: &Path) -> BotResult<SqlitePool> {
    let pool = open_pool(path).await?;
    run(&pool).await?;
    Ok(pool)
}

/// Convenience for tests: an in-memory database is not usable here because SQLx
/// pools hand out different connections, so tests use a temp file instead.
pub async fn open_temp(dir: &Path) -> BotResult<SqlitePool> {
    open_and_migrate(&dir.join("bot.sqlite3")).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn migrations_apply_cleanly() {
        let dir = tempfile::tempdir().expect("临时目录");
        let pool = open_temp(dir.path()).await.expect("迁移应当成功");
        let count: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name NOT LIKE '_sqlx%'",
        )
        .fetch_one(&pool)
        .await
        .expect("查询表数量");
        assert!(count >= 11, "期望至少 11 张业务表，实际 {count}");
    }

    #[tokio::test]
    async fn migrations_are_idempotent() {
        let dir = tempfile::tempdir().expect("临时目录");
        let pool = open_temp(dir.path()).await.expect("首次迁移");
        run(&pool).await.expect("二次迁移应当幂等");
    }

    #[tokio::test]
    async fn foreign_keys_are_enforced() {
        let dir = tempfile::tempdir().expect("临时目录");
        let pool = open_temp(dir.path()).await.expect("迁移应当成功");
        let err = sqlx::query(
            "INSERT INTO group_state (bot_id, group_id, updated_at) VALUES ('b', 'g', 0)",
        )
        .execute(&pool)
        .await;
        assert!(err.is_err(), "缺失父行时外键应当拒绝插入");
    }
}
