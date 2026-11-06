//! lite 入站：帧 → 去重 → 白名单 → 规范化 → 落库 + 入 pending。
//!
//! 与完整版 intake 的差别只在砍掉的部分：不处理通知（不做群发现，
//! 群由 LITE_GROUPS 白名单写死）、不处理命令、不改写戳一戳事件。
//! 安全边界与完整版一致：原始 QQ 号只留在 Rust 与 SQLite 里，
//! 进 pending 的 payload 是规范化后的别名形态。

use std::sync::Arc;

use serde_json::Value;

use crate::onebot::connection::FrameSink;
use crate::onebot::event::RawEvent;
use crate::onebot::normalize::{Normalized, Segment, normalize_event};
use crate::pipeline::dedupe::{Dedupe, Seen, dedupe_key};
use crate::store::SqliteStore;

use super::config::LiteConfig;

pub struct LiteIntake {
    store: Arc<SqliteStore>,
    cfg: Arc<LiteConfig>,
    dedupe: Dedupe,
}

impl LiteIntake {
    pub fn new(store: Arc<SqliteStore>, cfg: Arc<LiteConfig>) -> Self {
        Self {
            store,
            cfg,
            dedupe: Dedupe::new(4096),
        }
    }

    async fn handle(&self, value: &Value) {
        let event: RawEvent = match serde_json::from_value(value.clone()) {
            Ok(e) => e,
            Err(_) => return,
        };
        if event.post_type.as_deref() != Some("message")
            || event.message_type.as_deref() != Some("group")
        {
            return;
        }
        let (Some(message_id), Some(group_id), Some(user_id)) = (
            event.message_id.as_deref().filter(|m| !m.is_empty()),
            event.group_id.as_deref(),
            event.user_id.as_deref(),
        ) else {
            return;
        };
        // 白名单之外：不落库、不入队、不回应。
        if !self.cfg.groups.iter().any(|g| g == group_id) {
            return;
        }
        // 自己发出的消息在发送时已由出站侧写入群日志，这里再处理
        // 会形成自问自答，直接跳过。
        if user_id == self.cfg.bot_id {
            return;
        }
        let message_id = message_id.to_string();
        let group_id = group_id.to_string();
        let user_id = user_id.to_string();

        // 双层去重：内存挡重连重放，seen 表挡重启重放（与完整版同序）。
        let key = dedupe_key(&self.cfg.bot_id, &message_id);
        if matches!(self.dedupe.observe(&key), Seen::Duplicate) {
            return;
        }
        let now = crate::now_unix();
        match self
            .store
            .mark_seen(&self.cfg.bot_id, &message_id, now)
            .await
        {
            Ok(true) => {}
            _ => return,
        }
        if self
            .store
            .ensure_group(&self.cfg.bot_id, &group_id, now)
            .await
            .is_err()
        {
            return;
        }
        // 别名必须先于规范化分配：speaker_ref 是 normalize_event 的入参。
        let speaker_ref = match self
            .store
            .ensure_member(&self.cfg.bot_id, &group_id, &user_id, now)
            .await
        {
            Ok(r) => r,
            Err(_) => return,
        };
        let normalized = normalize_event(
            &event,
            &self.cfg.bot_id,
            &self.cfg.owner_id,
            &speaker_ref,
            now,
        );
        let Normalized::Message(mut message) = normalized else {
            // Command / Skipped：基座版不处理命令，其余丢弃。
            return;
        };
        self.resolve_mentions(value, &mut message).await;

        let has_image = message
            .segments
            .iter()
            .any(|s| matches!(s, Segment::Image { .. }));
        let _ = self
            .store
            .insert_group_message(
                &self.cfg.bot_id,
                &group_id,
                &message.message_id,
                &message.sender_id,
                Some(&message.display_name),
                message.speaker_role.as_str(),
                &message.text,
                has_image,
                None,
                None,
                now,
            )
            .await;
        if let Ok(payload) = serde_json::to_string(&*message) {
            // image_refs 恒为空：基座版不下载图片，多模态一律占位。
            let _ = self
                .store
                .insert_pending(
                    &self.cfg.bot_id,
                    &group_id,
                    &message.message_id,
                    &message.sender_id,
                    message.received_at,
                    &payload,
                    "[]",
                )
                .await;
        }
    }

    /// 把消息里 @ 的成员解析成别名，填回 AtOther 段。
    ///
    /// 完整版在 intake 里做同样的事（resolve_mentions）：原始 QQ 号
    /// 只在这一层出现，进 prompt 的只有别名。原始段从帧 JSON 里读，
    /// 顺序与规范化段里的 AtOther 一一对应。
    async fn resolve_mentions(
        &self,
        frame: &Value,
        message: &mut crate::onebot::normalize::IncomingMessage,
    ) {
        let Some(raw_segments) = frame.get("message").and_then(Value::as_array) else {
            return;
        };
        let mut targets: Vec<String> = Vec::new();
        for seg in raw_segments {
            if seg.get("type").and_then(Value::as_str) != Some("at") {
                continue;
            }
            let qq = match seg.get("data").and_then(|d| d.get("qq")) {
                Some(Value::String(s)) => s.clone(),
                Some(Value::Number(n)) => n.to_string(),
                _ => continue,
            };
            if qq == "all" || qq == self.cfg.bot_id {
                continue;
            }
            targets.push(qq);
        }
        if targets.is_empty() {
            return;
        }
        let now = crate::now_unix();
        let mut resolved: Vec<(String, Option<String>)> = Vec::new();
        for qq in &targets {
            let Ok(alias) = self
                .store
                .ensure_member(&self.cfg.bot_id, &message.group_id, qq, now)
                .await
            else {
                continue;
            };
            let name = self
                .store
                .member_display_name(&self.cfg.bot_id, &message.group_id, qq)
                .await
                .ok()
                .flatten();
            resolved.push((alias, name));
        }
        let mut iter = resolved.into_iter();
        for seg in message.segments.iter_mut() {
            if let Segment::AtOther {
                target_ref,
                target_name,
            } = seg
                && target_ref.is_none()
                && let Some((alias, name)) = iter.next()
            {
                *target_ref = Some(alias);
                *target_name = name;
            }
        }
    }
}

#[async_trait::async_trait]
impl FrameSink for LiteIntake {
    async fn on_event(&self, value: Value) {
        self.handle(&value).await;
    }
}
