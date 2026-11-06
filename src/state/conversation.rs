//! Agy conversation bookkeeping per group.
//!
//! One conversation per `(bot_id, group_id)`, stored on the group row. Two hard rules:
//!
//! * **never** `--continue`: resuming "the most recent conversation" would let one
//!   group's context bleed into another, so the id is always explicit,
//! * when a call's outcome is unknown (timeout), the id is *not* advanced and the group
//!   is paused — replaying a batch into a possibly-advanced session would duplicate the
//!   turn, and forking the id would silently split the conversation.

use std::sync::Arc;

use crate::error::BotResult;
use crate::store::SqliteStore;

/// Per-group conversation pointer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConversationState {
    pub conversation_id: Option<String>,
    /// Set when the last call's result was unknown; the pipeline must not run again
    /// until an operator resets the session.
    pub poisoned: bool,
}

impl ConversationState {
    pub fn fresh() -> Self {
        Self {
            conversation_id: None,
            poisoned: false,
        }
    }

    pub fn can_run(&self) -> bool {
        !self.poisoned
    }
}

/// Load and persist conversation ids.
pub struct ConversationStore {
    store: Arc<SqliteStore>,
}

impl ConversationStore {
    pub fn new(store: Arc<SqliteStore>) -> Self {
        Self { store }
    }

    /// Load the current pointer for a group.
    pub async fn load(&self, bot_id: &str, group_id: &str) -> BotResult<ConversationState> {
        let group = self.store.get_group(bot_id, group_id).await?;
        Ok(match group {
            None => ConversationState::fresh(),
            Some(g) => ConversationState {
                conversation_id: g.agy_conversation_id,
                // A paused group whose last call is unknown is represented by the pause
                // flag; the id itself stays as-is.
                poisoned: false,
            },
        })
    }

    /// Persist a new conversation id after a successful call.
    pub async fn store_id(
        &self,
        bot_id: &str,
        group_id: &str,
        conversation_id: Option<&str>,
        now: i64,
    ) -> BotResult<()> {
        self.store
            .set_conversation_id(bot_id, group_id, conversation_id, now)
            .await
    }

    /// Clear the pointer (operator "reset session").
    pub async fn reset(&self, bot_id: &str, group_id: &str, now: i64) -> BotResult<()> {
        self.store
            .set_conversation_id(bot_id, group_id, None, now)
            .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::migrations;

    async fn store() -> (Arc<SqliteStore>, tempfile::TempDir) {
        let dir = tempfile::tempdir().expect("tmp");
        let path = dir.path().join("bot.sqlite3");
        let pool = migrations::open_and_migrate(&path).await.expect("迁移");
        (Arc::new(SqliteStore::new(pool)), dir)
    }

    #[tokio::test]
    async fn conversation_ids_are_isolated_per_group() {
        let (store, _dir) = store().await;
        store.ensure_group("1", "100", 10).await.expect("建群");
        store.ensure_group("1", "200", 10).await.expect("建群");
        let conv = ConversationStore::new(Arc::clone(&store));

        conv.store_id("1", "100", Some("conv_a"), 20)
            .await
            .expect("写");
        conv.store_id("1", "200", Some("conv_b"), 20)
            .await
            .expect("写");

        assert_eq!(
            conv.load("1", "100")
                .await
                .unwrap()
                .conversation_id
                .as_deref(),
            Some("conv_a")
        );
        assert_eq!(
            conv.load("1", "200")
                .await
                .unwrap()
                .conversation_id
                .as_deref(),
            Some("conv_b")
        );
        // A different bot on the same group id is a separate row too.
        store.ensure_group("2", "100", 10).await.expect("建群");
        assert!(
            conv.load("2", "100")
                .await
                .unwrap()
                .conversation_id
                .is_none()
        );
    }

    #[tokio::test]
    async fn unknown_group_loads_fresh() {
        let (store, _dir) = store().await;
        let conv = ConversationStore::new(store);
        let state = conv.load("1", "999").await.expect("读");
        assert!(state.conversation_id.is_none());
        assert!(state.can_run());
    }

    #[tokio::test]
    async fn reset_clears_the_pointer() {
        let (store, _dir) = store().await;
        store.ensure_group("1", "100", 10).await.expect("建群");
        let conv = ConversationStore::new(Arc::clone(&store));
        conv.store_id("1", "100", Some("conv_a"), 20)
            .await
            .expect("写");
        conv.reset("1", "100", 30).await.expect("清");
        assert!(
            conv.load("1", "100")
                .await
                .unwrap()
                .conversation_id
                .is_none()
        );
    }
}
