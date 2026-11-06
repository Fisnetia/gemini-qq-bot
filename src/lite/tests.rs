//! lite 的契约测试（从 mod.rs 搬出，独立成文件）。

use super::*;
use crate::pipeline::outbox::DeliveryResult;

fn msg(text: &str, mentions_bot: bool, role: SpeakerRole) -> IncomingMessage {
    IncomingMessage {
        bot_id: "1".into(),
        group_id: "9".into(),
        message_id: "m1".into(),
        sender_id: "2".into(),
        speaker_ref: "u1".into(),
        speaker_role: role,
        display_name: "群友".into(),
        known_alias: None,
        received_at: 0,
        segments: Vec::new(),
        text: text.into(),
        mentions_bot,
        quote_of: None,
    }
}

#[test]
fn trigger_rules_cover_the_four_gates() {
    let names = vec!["Gemini".to_string()];
    // 普通群聊：不触发。
    assert!(!message_triggers(
        &msg("今天吃啥", false, SpeakerRole::Member),
        &names,
        false,
        false
    ));
    // @ 机器人：触发。
    assert!(message_triggers(
        &msg("看这个", true, SpeakerRole::Member),
        &names,
        false,
        false
    ));
    // 主人发言：触发。
    assert!(message_triggers(
        &msg("在吗", false, SpeakerRole::Owner),
        &names,
        false,
        false
    ));
    // 呼唤词：触发。
    assert!(message_triggers(
        &msg("Gemini 在吗", false, SpeakerRole::Member),
        &names,
        false,
        false
    ));
    // 引用机器人的消息：触发。
    assert!(message_triggers(
        &msg("这个呢", false, SpeakerRole::Member),
        &names,
        false,
        true
    ));
    // 全量模式：都触发。
    assert!(message_triggers(
        &msg("闲聊", false, SpeakerRole::Member),
        &names,
        true,
        false
    ));
}

#[test]
fn call_name_matching_mirrors_full_version() {
    // 用例与完整版 cadence::text_names_bot 的单测对齐。
    let names = vec!["Gemini".to_string(), "小双".to_string()];
    assert!(text_names_bot("Gemini 在吗", &names));
    assert!(text_names_bot("gemini 快看这个", &names));
    assert!(text_names_bot("小双又睡着了", &names));
    assert!(!text_names_bot("今天 gem 好闪", &names));
    assert!(!text_names_bot("", &names));
    assert!(!text_names_bot("Gemini", &[]));
}

#[test]
fn split_batches_respects_message_cap() {
    let entries: Vec<BatchEntry> = (0..60)
        .map(|i| BatchEntry {
            id: i,
            chars: 1,
            images: 0,
            mentions_bot: false,
            from_owner: false,
            received_at: i,
        })
        .collect();
    let chunks = split_batches(&entries, BatchLimits::default());
    assert_eq!(chunks.len(), 2);
    assert_eq!(chunks[0].len(), 50);
    assert_eq!(chunks[1].len(), 10);
}

async fn temp_store() -> (tempfile::TempDir, Arc<SqliteStore>) {
    let dir = tempfile::tempdir().expect("临时目录");
    let pool = crate::store::migrations::open_temp(dir.path())
        .await
        .expect("迁移");
    (dir, Arc::new(SqliteStore::new(pool)))
}

#[tokio::test]
async fn late_submission_after_disarm_is_rejected() {
    // 迟到提交契约：轮次结束（disarm）后，上一轮的口令再提交
    // 必须被拒——迟到的草稿绝不能落进新一轮。
    let (_dir, store) = temp_store().await;
    let scope = crate::mcp::Scope {
        bot_id: "b".into(),
        group_id: "g".into(),
    };
    let now = crate::now_unix();
    store
        .arm_turn("g", "tok1", "batch", now, now + 600)
        .await
        .expect("武装");
    let stale = serde_json::json!({
        "turn_token": "tok1", "should_reply": true, "reply_parts": ["迟到的"]
    });
    assert!(
        !crate::mcp::submit::call_submit(&store, &scope, &stale)
            .await
            .get("isError")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false),
        "武装期内本轮口令应被接受"
    );
    store.disarm_turn("g", "tok1").await.expect("解除武装");
    let result = crate::mcp::submit::call_submit(&store, &scope, &stale).await;
    assert!(
        result
            .get("isError")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false),
        "解除武装后迟到提交必须被拒"
    );
}

#[tokio::test]
async fn unknown_outbox_row_is_never_deliverable_again() {
    // unknown 不重发契约：发送结果不明朗的行落 unknown 后，
    // 待发列表里永远不再出现，只能由运维人工处置。
    let (_dir, store) = temp_store().await;
    let outbox = Outbox::new(Arc::clone(&store));
    let now = crate::now_unix();
    // outbox 行有指向 groups 的外键，先建档（serve 启动时
    // build() 对白名单群做同样的事）。
    store.ensure_group("b", "g", now).await.expect("建群档");
    let id = outbox
        .enqueue("b", "g", "batch", "你好", now)
        .await
        .expect("入队");
    let deliverable = outbox.deliverable(10).await.expect("待发列表");
    assert!(deliverable.iter().any(|r| r.id == id));
    outbox.begin(&id, now).await.expect("开始发送");
    outbox
        .finish(
            &id,
            &DeliveryResult::Unknown {
                detail: "timeout".into(),
            },
            now,
        )
        .await
        .expect("落 unknown");
    let deliverable = outbox.deliverable(10).await.expect("待发列表");
    assert!(
        !deliverable.iter().any(|r| r.id == id),
        "unknown 行不得再次出现在待发列表"
    );
}
