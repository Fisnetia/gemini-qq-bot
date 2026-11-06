//! lite 出站发送：把已校验的回复段经 outbox 逐段发出。
//!
//! 镜像完整版 delivery 的文本路径：begin → 读行 → 发 →
//! classify → finish；结果 unknown 绝不自动重发（outbox 状态机
//! 保证），一段没发成则封存同批其余段（失去上下文的后段硬发
//! 只会错位）。

use std::time::Duration;

use crate::onebot::action::SendGroupMsg;
use crate::onebot::normalize::SpeakerRole;
use crate::pipeline::outbox::{DeliveryResult, classify};

use super::turn::TurnRunner;

/// 分段发送间隔（毫秒），与完整版 deliver_parts 的缺省一致。
const PART_GAP_MS: u64 = 800;

impl TurnRunner {
    pub(super) async fn deliver_parts(&self, ids: &[String], group_id: &str, batch_id: &str) {
        for (index, id) in ids.iter().enumerate() {
            if index > 0 {
                tokio::time::sleep(Duration::from_millis(PART_GAP_MS)).await;
            }
            self.deliver_one(id, group_id).await;
            let sent = self
                .store
                .get_outbox(id)
                .await
                .ok()
                .flatten()
                .is_some_and(|row| row.state == "sent");
            if !sent {
                // 一段没发成，后面的段失去上下文，一并封存
                // （完整版 deliver_parts 同口径）。
                let _ = self
                    .store
                    .seal_pending_outbox(&self.cfg.bot_id, group_id, batch_id, crate::now_unix())
                    .await;
                break;
            }
        }
    }

    /// 发一条 outbox 文本行。镜像完整版 delivery 的文本路径：
    /// begin → 读行 → 发 → classify → finish；结果 unknown
    /// 绝不自动重发（outbox 状态机保证），这里只补一条审计。
    async fn deliver_one(&self, outbox_id: &str, group_id: &str) {
        let now = crate::now_unix();
        if self.outbox.begin(outbox_id, now).await.is_err() {
            return;
        }
        let row = match self.store.get_outbox(outbox_id).await {
            Ok(Some(row)) => row,
            _ => {
                let _ = self
                    .outbox
                    .finish(
                        outbox_id,
                        &DeliveryResult::Unknown {
                            detail: "row-missing".into(),
                        },
                        crate::now_unix(),
                    )
                    .await;
                return;
            }
        };
        let frame = match row.quote_message_id.as_deref() {
            Some(target) => SendGroupMsg::text_quoted(group_id, &row.text, target),
            None => SendGroupMsg::text(group_id, &row.text),
        };
        let outcome = self
            .connection
            .call("send_group_msg", frame.params(), Duration::from_secs(10))
            .await;
        let result = classify(outcome);
        if matches!(result, DeliveryResult::Unknown { .. }) {
            self.audit(group_id, &format!("lite.send.unknown.g{group_id}"))
                .await;
        }
        let _ = self
            .outbox
            .finish(outbox_id, &result, crate::now_unix())
            .await;
        if let DeliveryResult::Sent { message_id } = &result {
            let mid = message_id
                .clone()
                .unwrap_or_else(|| format!("bot:{outbox_id}"));
            let _ = self
                .store
                .insert_group_message(
                    &self.cfg.bot_id,
                    group_id,
                    &mid,
                    &self.cfg.bot_id,
                    None,
                    SpeakerRole::Bot.as_str(),
                    &row.text,
                    false,
                    None,
                    None,
                    crate::now_unix(),
                )
                .await;
        }
    }
}
