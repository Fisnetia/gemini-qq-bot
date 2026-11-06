//! Outbox delivery.
//!
//! Every reply is persisted before it is sent, so a crash between `pending` and `sent`
//! is visible rather than silent. The single most important rule: when the OneBot send
//! result is ambiguous the row becomes `unknown` and **must not** be retried
//! automatically. Only an operator can move `unknown` back to `pending` from the panel.

use std::sync::Arc;

use crate::error::ErrorClass;
use crate::onebot::action::SendOutcome;
use crate::store::{OutboxRecord, OutboxState, SqliteStore};

/// What happened to one delivery attempt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DeliveryResult {
    /// The implementation acknowledged the send.
    Sent { message_id: Option<String> },
    /// A definite rejection (bad payload, muted, …). Safe to surface, not to resend
    /// blindly.
    Rejected { detail: String },
    /// The outcome is unknowable: the row goes to `unknown` and stops here.
    Unknown { detail: String },
}

/// Translate a OneBot [`SendOutcome`] into a delivery result.
pub fn classify(outcome: SendOutcome) -> DeliveryResult {
    match outcome {
        SendOutcome::Sent { message_id } => DeliveryResult::Sent { message_id },
        SendOutcome::Rejected { retcode, message } => DeliveryResult::Rejected {
            detail: format!("retcode {retcode}: {message}"),
        },
        // Not connected, timed out, or the channel closed: the message may or may not
        // have been delivered.
        SendOutcome::Unknown { reason } => DeliveryResult::Unknown { detail: reason },
    }
}

impl DeliveryResult {
    /// The state a delivery result maps to. `Rejected` becomes `failed-ish`: there is no
    /// `failed` state, so a rejection is recorded as `unknown` too — resending a rejected
    /// payload automatically would loop.
    pub fn state(&self) -> OutboxState {
        match self {
            DeliveryResult::Sent { .. } => OutboxState::Sent,
            DeliveryResult::Rejected { .. } | DeliveryResult::Unknown { .. } => {
                OutboxState::Unknown
            }
        }
    }

    pub fn error_class(&self) -> Option<ErrorClass> {
        match self {
            DeliveryResult::Sent { .. } => None,
            DeliveryResult::Rejected { .. } => Some(ErrorClass::OnebotActionFailed),
            DeliveryResult::Unknown { .. } => Some(ErrorClass::OnebotDisconnected),
        }
    }
}

/// Outbox writer/reader.
pub struct Outbox {
    store: Arc<SqliteStore>,
}

impl Outbox {
    pub fn new(store: Arc<SqliteStore>) -> Self {
        Self { store }
    }

    /// Persist a reply before attempting delivery.
    pub async fn enqueue(
        &self,
        bot_id: &str,
        group_id: &str,
        batch_id: &str,
        text: &str,
        now: i64,
    ) -> crate::error::BotResult<String> {
        self.store
            .enqueue_outbox(bot_id, group_id, batch_id, text, now)
            .await
    }

    /// Persist every part of one reply before the first send.
    pub async fn enqueue_parts(
        &self,
        bot_id: &str,
        group_id: &str,
        batch_id: &str,
        parts: &[String],
        now: i64,
    ) -> crate::error::BotResult<Vec<String>> {
        self.enqueue_mixed(bot_id, group_id, batch_id, parts, &[], &[], now)
            .await
    }

    /// Persist a reply whose parts are text, stickers, or both.
    ///
    /// Stickers come after the text: talking first and then throwing in a picture is
    /// how a person actually uses them, and it also keeps the readable sentence safe
    /// if the image send fails.
    /// `part_quotes` aligns with `parts`: each part may carry its own quote
    /// (per-part addressing). When there are no text parts at all, the
    /// first entry — the caller's single quote, if any — rides the first
    /// sticker instead, as it always has.
    pub async fn enqueue_mixed(
        &self,
        bot_id: &str,
        group_id: &str,
        batch_id: &str,
        parts: &[String],
        sticker_hashes: &[String],
        part_quotes: &[Option<&str>],
        now: i64,
    ) -> crate::error::BotResult<Vec<String>> {
        let total = parts.len() + sticker_hashes.len();
        let count = i64::try_from(total).unwrap_or(i64::MAX);
        let mut ids = Vec::with_capacity(total);
        for (index, text) in parts.iter().enumerate() {
            let id = self
                .store
                .enqueue_outbox_part(crate::store::OutboxPart {
                    bot_id,
                    group_id,
                    batch_id,
                    text,
                    part_index: i64::try_from(index).unwrap_or(i64::MAX),
                    part_count: count,
                    now,
                    sticker_hash: None,
                    quote_message_id: part_quotes.get(index).copied().flatten(),
                })
                .await?;
            ids.push(id);
        }
        let mut sticker_quote = if parts.is_empty() {
            part_quotes.first().copied().flatten()
        } else {
            None
        };
        for (offset, hash) in sticker_hashes.iter().enumerate() {
            let index = parts.len() + offset;
            let id = self
                .store
                .enqueue_outbox_part(crate::store::OutboxPart {
                    bot_id,
                    group_id,
                    batch_id,
                    text: "",
                    part_index: i64::try_from(index).unwrap_or(i64::MAX),
                    part_count: count,
                    now,
                    sticker_hash: Some(hash.as_str()),
                    quote_message_id: sticker_quote.take(),
                })
                .await?;
            ids.push(id);
        }
        Ok(ids)
    }

    /// Mark a row `sending` immediately before the network call.
    pub async fn begin(&self, id: &str, now: i64) -> crate::error::BotResult<()> {
        self.store
            .mark_outbox(id, OutboxState::Sending, None, now)
            .await
    }

    /// Record the final result.
    ///
    /// Never called with `Pending`: that would look like "never attempted".
    pub async fn finish(
        &self,
        id: &str,
        result: &DeliveryResult,
        now: i64,
    ) -> crate::error::BotResult<()> {
        let message_id = match result {
            DeliveryResult::Sent { message_id } => message_id.clone(),
            _ => None,
        };
        self.store
            .mark_outbox(id, result.state(), message_id.as_deref(), now)
            .await
    }

    /// Rows that still need an attempt (never includes `unknown` or `sending`).
    pub async fn deliverable(&self, limit: i64) -> crate::error::BotResult<Vec<OutboxRecord>> {
        self.store.outbox_batch(OutboxState::Pending, limit).await
    }

    /// Operator-only: move `unknown` back to `pending` after a manual decision.
    pub async fn retry_unknown(&self, id: &str, now: i64) -> crate::error::BotResult<bool> {
        self.store.retry_unknown(id, now).await
    }

    /// Crash recovery: `sending` rows become `unknown`.
    pub async fn reconcile(&self, now: i64) -> crate::error::BotResult<u64> {
        self.store.reconcile_outbox(now).await
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
        let store = Arc::new(SqliteStore::new(pool));
        store.ensure_group("1", "100", 10).await.expect("建群");
        (store, dir)
    }

    #[test]
    fn sent_maps_to_sent() {
        let r = classify(SendOutcome::Sent {
            message_id: Some("77".into()),
        });
        assert_eq!(r.state(), OutboxState::Sent);
        assert!(r.error_class().is_none());
    }

    #[test]
    fn rejection_and_unknown_never_map_to_pending() {
        for outcome in [
            SendOutcome::Rejected {
                retcode: 100,
                message: "bad".into(),
            },
            SendOutcome::Unknown {
                reason: "timeout".into(),
            },
        ] {
            let r = classify(outcome);
            assert_eq!(r.state(), OutboxState::Unknown);
            assert!(r.error_class().is_some());
        }
    }

    #[tokio::test]
    async fn unknown_row_is_never_returned_for_delivery() {
        let (store, _dir) = store().await;
        let outbox = Outbox::new(Arc::clone(&store));
        let id = outbox
            .enqueue("1", "100", "b1", "hello", 20)
            .await
            .expect("入队");

        // A pending row is deliverable.
        let rows = outbox.deliverable(10).await.expect("读");
        assert_eq!(rows.len(), 1);

        outbox.begin(&id, 21).await.expect("开始");
        outbox
            .finish(
                &id,
                &DeliveryResult::Unknown {
                    detail: "timeout".into(),
                },
                22,
            )
            .await
            .expect("结束");

        // Now nothing is deliverable: the ambiguous message must not be auto-resent.
        assert!(outbox.deliverable(10).await.expect("读").is_empty());
        let row = store.get_outbox(&id).await.expect("读").expect("存在");
        assert_eq!(row.state, "unknown");

        // Only the explicit operator path may requeue it.
        assert!(
            !outbox
                .retry_unknown("does-not-exist", 30)
                .await
                .expect("读")
        );
        assert!(outbox.retry_unknown(&id, 30).await.expect("读"));
        assert_eq!(outbox.deliverable(10).await.expect("读").len(), 1);
    }

    #[tokio::test]
    async fn a_quote_rides_only_on_the_first_part() {
        let (store, _dir) = store().await;
        let outbox = Outbox::new(Arc::clone(&store));
        let ids = outbox
            .enqueue_mixed(
                "1",
                "100",
                "b1",
                &["回你了".into(), "第二句".into()],
                &["a1b2c3d4".to_string()],
                &[Some("4242"), None],
                10,
            )
            .await
            .expect("入队");
        assert_eq!(ids.len(), 3);
        let rows = outbox.deliverable(10).await.expect("读");
        let by_index: std::collections::HashMap<i64, Option<String>> = rows
            .iter()
            .map(|r| (r.part_index, r.quote_message_id.clone()))
            .collect();
        assert_eq!(by_index[&0].as_deref(), Some("4242"));
        assert_eq!(by_index[&1], None);
        assert_eq!(by_index[&2], None);

        // Sticker-only reply: the quote lands on the sticker part instead.
        let ids = outbox
            .enqueue_mixed(
                "1",
                "100",
                "b2",
                &[],
                &["e5f6a7b8".to_string()],
                &[Some("99")],
                20,
            )
            .await
            .expect("入队");
        let row = store.get_outbox(&ids[0]).await.expect("读").expect("存在");
        assert_eq!(row.quote_message_id.as_deref(), Some("99"));
    }

    #[tokio::test]
    async fn per_part_quotes_ride_their_own_parts() {
        let (store, _dir) = store().await;
        let outbox = Outbox::new(Arc::clone(&store));
        let ids = outbox
            .enqueue_mixed(
                "1",
                "100",
                "b3",
                &["先接话".into(), "这句答你".into()],
                &[],
                &[None, Some("777")],
                30,
            )
            .await
            .expect("入队");
        assert_eq!(ids.len(), 2);
        let first = store.get_outbox(&ids[0]).await.expect("读").expect("存在");
        let second = store.get_outbox(&ids[1]).await.expect("读").expect("存在");
        assert_eq!(first.quote_message_id, None);
        assert_eq!(second.quote_message_id.as_deref(), Some("777"));
    }

    #[tokio::test]
    async fn in_flight_rows_are_recovered_as_unknown() {
        let (store, _dir) = store().await;
        let outbox = Outbox::new(Arc::clone(&store));
        let id = outbox
            .enqueue("1", "100", "b1", "hello", 20)
            .await
            .expect("入队");
        outbox.begin(&id, 21).await.expect("开始");
        // Simulate a crash: the row stays `sending`.
        assert_eq!(outbox.reconcile(30).await.expect("收敛"), 1);
        let row = store.get_outbox(&id).await.expect("读").expect("存在");
        assert_eq!(row.state, "unknown");
        assert!(outbox.deliverable(10).await.expect("读").is_empty());
    }

    #[tokio::test]
    async fn partial_success_does_not_resend_the_batch() {
        let (store, _dir) = store().await;
        let outbox = Outbox::new(Arc::clone(&store));
        let ids = outbox
            .enqueue_parts(
                "1",
                "100",
                "b1",
                &["一".into(), "二".into(), "三".into()],
                10,
            )
            .await
            .expect("入队");
        outbox.begin(&ids[0], 11).await.expect("开始");
        outbox
            .finish(
                &ids[0],
                &DeliveryResult::Sent {
                    message_id: Some("m1".into()),
                },
                12,
            )
            .await
            .expect("第一条成功");
        outbox.begin(&ids[1], 13).await.expect("开始");
        outbox
            .finish(
                &ids[1],
                &DeliveryResult::Unknown {
                    detail: "timeout".into(),
                },
                14,
            )
            .await
            .expect("第二条不明");
        store
            .seal_pending_outbox("1", "100", "b1", 15)
            .await
            .expect("封存");

        assert!(outbox.deliverable(10).await.expect("读").is_empty());
        let first = store.get_outbox(&ids[0]).await.expect("读").expect("存在");
        assert_eq!(first.state, "sent");
        let third = store.get_outbox(&ids[2]).await.expect("读").expect("存在");
        assert_eq!(third.state, "unknown");
    }
}
