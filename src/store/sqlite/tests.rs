//! Tests for SqliteStore. Moved out of sqlite.rs verbatim
//! (std-split-sqlite); assertions unchanged.

use super::*;
use crate::store::migrations;

async fn store() -> (tempfile::TempDir, SqliteStore) {
    let dir = tempfile::tempdir().expect("临时目录");
    let pool = migrations::open_temp(dir.path()).await.expect("迁移");
    (dir, SqliteStore::new(pool))
}

#[tokio::test]
async fn seed_quarantine_lifecycle() {
    let (_d, s) = store().await;
    // No spawn recorded: nothing to quarantine, reads stay empty.
    s.mark_seed_rejected("1", "100", 10)
        .await
        .expect("无记录标记");
    assert!(
        s.get_seed_quarantine("1", "100")
            .await
            .expect("读")
            .is_none()
    );

    s.record_seed_spawn("1", "100", "hash-a", 0, 20)
        .await
        .expect("记录 spawn");
    let q = s
        .get_seed_quarantine("1", "100")
        .await
        .expect("读")
        .expect("有行");
    assert_eq!(q.last_seed_hash.as_deref(), Some("hash-a"));
    assert_eq!(q.tier, 0);
    assert!(q.rejected_hash.is_none());

    // A rejection quarantines the spawn's hash and degrades one tier.
    s.mark_seed_rejected("1", "100", 30)
        .await
        .expect("标记被拒");
    let q = s
        .get_seed_quarantine("1", "100")
        .await
        .expect("读")
        .expect("有行");
    assert_eq!(q.rejected_hash.as_deref(), Some("hash-a"));
    assert_eq!(q.tier, 1);
    assert_eq!(q.clean_rounds, 0);

    // Nine clean rounds are not enough; the tenth clears the record.
    for i in 0..9 {
        s.note_seed_clean_round("1", "100", 40 + i)
            .await
            .expect("干净轮");
    }
    let q = s
        .get_seed_quarantine("1", "100")
        .await
        .expect("读")
        .expect("有行");
    assert_eq!(q.tier, 1);
    assert_eq!(q.clean_rounds, 9);
    s.note_seed_clean_round("1", "100", 50)
        .await
        .expect("第十轮");
    let q = s
        .get_seed_quarantine("1", "100")
        .await
        .expect("读")
        .expect("有行");
    assert_eq!(q.tier, 0);
    assert!(q.rejected_hash.is_none());
    assert_eq!(q.clean_rounds, 0);

    // The tier caps at the minimal seed no matter how often it is refused.
    s.record_seed_spawn("1", "100", "hash-b", 2, 60)
        .await
        .expect("记录 spawn");
    s.mark_seed_rejected("1", "100", 61)
        .await
        .expect("标记被拒");
    let q = s
        .get_seed_quarantine("1", "100")
        .await
        .expect("读")
        .expect("有行");
    assert_eq!(q.tier, 2);
    assert_eq!(q.rejected_hash.as_deref(), Some("hash-b"));
}

#[tokio::test]
async fn arm_turn_replaces_and_reports_previous() {
    let (_d, s) = store().await;
    let first = s.arm_turn("g", "t1", "b1", 10, 100).await.expect("武装");
    assert!(first.is_none());
    let prev = s.arm_turn("g", "t2", "b2", 20, 200).await.expect("再武装");
    let prev = prev.expect("应返回上一份武装");
    assert_eq!(prev.turn_token, "t1");
    let live = s.armed_turn("g").await.expect("读武装").expect("有武装");
    assert_eq!(live.turn_token, "t2");
    // A stale disarm must not kill the newer arm.
    s.disarm_turn("g", "t1").await.expect("解除旧武装");
    assert!(s.armed_turn("g").await.expect("读武装").is_some());
    s.disarm_turn("g", "t2").await.expect("解除武装");
    assert!(s.armed_turn("g").await.expect("读武装").is_none());
}

#[tokio::test]
async fn submissions_come_back_in_call_order() {
    let (_d, s) = store().await;
    let seq1 = s
        .record_submission("g", "tok", "{}", "rejected", "r", 1)
        .await
        .expect("记录");
    let seq2 = s
        .record_submission("g", "tok", "{}", "accepted", "", 2)
        .await
        .expect("记录");
    assert_eq!((seq1, seq2), (1, 2));
    let rows = s.submissions_for("tok").await.expect("读提交");
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[1].verdict, "accepted");
}

#[tokio::test]
async fn a_first_seen_group_is_disabled_but_defaults_to_inserts_on() {
    let (_d, s) = store().await;
    s.ensure_group("bot", "100", 1).await.expect("建组");
    let g = s
        .get_group("bot", "100")
        .await
        .expect("读组")
        .expect("有组");
    assert!(!g.enabled, "首见群默认不启用，仍由面板开启");
    assert!(
        g.resident_insert_enabled,
        "首见群插入默认开（2026-11-04 拍板）：关着的热群会整轮慢一拍"
    );
}

#[tokio::test]
async fn count_agent_calls_since_counts_every_state_inside_the_window_only() {
    let (_d, s) = store().await;
    // agent_calls references groups, so every (bot, group) pair used
    // below has to exist first.
    for (bot, group) in [("bot", "100"), ("bot", "200"), ("bot2", "100")] {
        s.ensure_group(bot, group, 1).await.expect("建组");
    }
    // Inside the window, assorted states: all of them count.
    let a = s.start_call("bot", "100", "b1", 1_000).await.expect("开");
    s.finish_call(&a, CallState::Succeeded, None, 1_001)
        .await
        .expect("收");
    let b = s.start_call("bot", "100", "b2", 1_100).await.expect("开");
    s.finish_call(&b, CallState::Failed, Some("agent_failed"), 1_101)
        .await
        .expect("收");
    let _c = s.start_call("bot", "100", "b3", 1_200).await.expect("开");
    // Outside the window, another group, another bot: none of them count.
    let _old = s.start_call("bot", "100", "b0", 100).await.expect("开");
    let _other_group = s.start_call("bot", "200", "b4", 1_300).await.expect("开");
    let _other_bot = s.start_call("bot2", "100", "b5", 1_300).await.expect("开");

    assert_eq!(
        s.count_agent_calls_since("bot", "100", 1_000)
            .await
            .expect("计数"),
        3
    );
    // The window edge is inclusive and moving it forward drops aged calls.
    assert_eq!(
        s.count_agent_calls_since("bot", "100", 1_101)
            .await
            .expect("计数"),
        1
    );
}

#[tokio::test]
async fn count_agent_calls_since_by_priority_splits_the_pools() {
    let (_d, s) = store().await;
    s.ensure_group("bot", "100", 1).await.expect("建组");
    // Plain start_call rows are ambient; only the explicit priority
    // variant lands in the addressed pool.
    let _a = s.start_call("bot", "100", "b1", 1_000).await.expect("开");
    let _b = s.start_call("bot", "100", "b2", 1_100).await.expect("开");
    let _p = s
        .start_call_with_priority("bot", "100", "b3", 1_200, true)
        .await
        .expect("开");
    assert_eq!(
        s.count_agent_calls_since("bot", "100", 1_000)
            .await
            .expect("计数"),
        3
    );
    assert_eq!(
        s.count_agent_calls_since_by_priority("bot", "100", 1_000, false)
            .await
            .expect("计数"),
        2
    );
    assert_eq!(
        s.count_agent_calls_since_by_priority("bot", "100", 1_000, true)
            .await
            .expect("计数"),
        1
    );
    // The window edge applies to the split counts too.
    assert_eq!(
        s.count_agent_calls_since_by_priority("bot", "100", 1_201, true)
            .await
            .expect("计数"),
        0
    );
}

#[tokio::test]
async fn new_group_is_disabled_by_default() {
    let (_d, s) = store().await;
    s.ensure_group("bot", "100", 1).await.expect("建组");
    let g = s
        .get_group("bot", "100")
        .await
        .expect("查询")
        .expect("存在");
    // Joining a group must never be enough to start answering in it.
    assert!(!g.enabled);
    assert!(!g.paused);
    assert!(g.agy_conversation_id.is_none());
    assert_eq!(g.agy_turn_count, 0);
}

#[tokio::test]
async fn conversation_turn_count_resets_with_the_session() {
    let (_d, s) = store().await;
    s.ensure_group("bot", "100", 1).await.expect("建组");
    s.set_conversation_id("bot", "100", Some("conv-1"), 2)
        .await
        .expect("写入会话");
    assert_eq!(
        s.bump_agy_turn_count("bot", "100", 3).await.expect("计数"),
        1
    );
    assert_eq!(
        s.bump_agy_turn_count("bot", "100", 4).await.expect("计数"),
        2
    );
    s.set_conversation_id("bot", "100", None, 5)
        .await
        .expect("重置");
    let g = s
        .get_group("bot", "100")
        .await
        .expect("查询")
        .expect("存在");
    assert!(g.agy_conversation_id.is_none());
    assert_eq!(g.agy_turn_count, 0);
}

#[tokio::test]
async fn speaker_refs_are_stable_and_per_group() {
    let (_d, s) = store().await;
    s.ensure_group("bot", "100", 1).await.expect("建组");
    s.ensure_group("bot", "200", 1).await.expect("建组");
    let a = s.ensure_member("bot", "100", "555", 1).await.expect("成员");
    let b = s.ensure_member("bot", "100", "666", 1).await.expect("成员");
    let a2 = s.ensure_member("bot", "100", "555", 2).await.expect("成员");
    assert_eq!(a, "u1");
    assert_eq!(b, "u2");
    assert_eq!(a2, "u1", "同一用户必须复用别名");

    // Same numeric alias in another group refers to a different person.
    let other = s.ensure_member("bot", "200", "777", 1).await.expect("成员");
    assert_eq!(other, "u1");
    let m = s
        .member_by_ref("bot", "200", "u1")
        .await
        .expect("查询")
        .expect("存在");
    assert_eq!(m.user_id, "777");
}

#[tokio::test]
async fn pending_insert_is_idempotent_and_batchable() {
    let (_d, s) = store().await;
    s.ensure_group("bot", "100", 1).await.expect("建组");
    assert!(
        s.insert_pending("bot", "100", "m1", "u", 1, "{}", "[]")
            .await
            .expect("插入")
    );
    assert!(
        !s.insert_pending("bot", "100", "m1", "u", 1, "{}", "[]")
            .await
            .expect("重复插入")
    );
    let rows = s.pending_for_group("bot", "100").await.expect("查询");
    assert_eq!(rows.len(), 1);
    s.claim_batch(&[rows[0].id], "batch1").await.expect("认领");
    assert!(
        s.pending_for_group("bot", "100")
            .await
            .expect("查询")
            .is_empty()
    );
    s.release_batch("batch1").await.expect("释放");
    assert_eq!(
        s.pending_for_group("bot", "100").await.expect("查询").len(),
        1
    );
    s.delete_pending(&[rows[0].id]).await.expect("删除");
    assert_eq!(s.pending_count("bot", "100").await.expect("计数"), 0);
}

#[tokio::test]
async fn claim_batch_returns_only_the_rows_this_caller_took() {
    let (_d, s) = store().await;
    s.ensure_group("bot", "100", 1).await.expect("建组");
    s.insert_pending("bot", "100", "m1", "u", 1, "{}", "[]")
        .await
        .expect("插入");
    let rows = s.pending_for_group("bot", "100").await.expect("查询");
    let id = rows[0].id;
    let store = std::sync::Arc::new(s);
    let a = std::sync::Arc::clone(&store);
    let b = std::sync::Arc::clone(&store);
    let ids_a = [id];
    let ids_b = [id];
    let (left, right) = tokio::join!(a.claim_batch(&ids_a, "b1"), b.claim_batch(&ids_b, "b2"));
    let left = left.expect("认领 a");
    let right = right.expect("认领 b");
    assert_eq!(
        left.len() + right.len(),
        1,
        "exactly one caller takes the row"
    );
    assert!(left.is_empty() ^ right.is_empty());
    assert!(
        store
            .claim_batch(&[id], "b3")
            .await
            .expect("再认领")
            .is_empty()
    );
}

#[tokio::test]
async fn release_events_releases_exactly_those_rows_across_batches() {
    let (_d, s) = store().await;
    s.ensure_group("bot", "100", 1).await.expect("建组");
    for m in ["m1", "m2", "m3"] {
        s.insert_pending("bot", "100", m, "u", 1, "{}", "[]")
            .await
            .expect("插入");
    }
    let rows = s.pending_for_group("bot", "100").await.expect("查询");
    assert_eq!(rows.len(), 3);
    // Two batches claimed separately, as two flush ticks would.
    s.claim_batch(&[rows[0].id], "batch1").await.expect("认领");
    s.claim_batch(&[rows[1].id, rows[2].id], "batch2")
        .await
        .expect("认领");
    // Releasing the union by id (what a merged job does) frees every row,
    // including the ones whose tag names the other batch.
    s.release_events(&[rows[0].id, rows[1].id, rows[2].id])
        .await
        .expect("按 id 释放");
    assert_eq!(
        s.pending_for_group("bot", "100").await.expect("查询").len(),
        3
    );
    // A partial release frees only the named rows.
    s.claim_batch(&[rows[0].id], "batch1").await.expect("认领");
    s.claim_batch(&[rows[1].id], "batch2").await.expect("认领");
    s.release_events(&[rows[0].id]).await.expect("释放一行");
    let left = s.pending_for_group("bot", "100").await.expect("查询");
    assert_eq!(left.len(), 2);
    assert!(left.iter().all(|r| r.id != rows[1].id));
}

/// A difference-set release: the rows *not* named stay claimed, and only the
/// named ones come back as pending. This is the shape a retired resident turn
/// relies on — it releases the union of its main batch and the batches it
/// inserted mid-turn, and nothing else may leak back into the pool.
#[tokio::test]
async fn releasing_a_difference_set_frees_only_the_named_rows() {
    let (_d, s) = store().await;
    s.ensure_group("bot", "100", 1).await.expect("建组");
    for m in ["m1", "m2", "m3", "m4"] {
        s.insert_pending("bot", "100", m, "u", 1, "{}", "[]")
            .await
            .expect("插入");
    }
    let rows = s.pending_for_group("bot", "100").await.expect("查询");
    assert_eq!(rows.len(), 4);
    // One turn: the main batch plus an insertion claimed after it.
    s.claim_batch(&[rows[0].id, rows[1].id], "batch-main")
        .await
        .expect("认领主批");
    s.claim_batch(&[rows[2].id], "batch-insert")
        .await
        .expect("认领插入批");
    assert!(
        s.pending_for_group("bot", "100").await.expect("查询").len() == 1,
        "全部认领后只剩未入批的那一行"
    );

    // The turn retires and releases its coverage, minus the row it never held.
    s.release_events(&[rows[0].id, rows[1].id, rows[2].id])
        .await
        .expect("释放本轮覆盖");
    let pending = s.pending_for_group("bot", "100").await.expect("查询");
    assert_eq!(pending.len(), 4, "本轮覆盖的行必须全部回到 pending");
    assert!(
        pending.iter().any(|r| r.id == rows[3].id),
        "从没入批的那一行本来就在池子里"
    );

    // A second turn claims a different, overlapping set; releasing only part of
    // it leaves the rest claimed.
    s.claim_batch(&[rows[1].id, rows[2].id, rows[3].id], "batch-next")
        .await
        .expect("认领");
    s.release_events(&[rows[1].id, rows[3].id])
        .await
        .expect("按差集释放");
    let left = s.pending_for_group("bot", "100").await.expect("查询");
    let ids: Vec<i64> = left.iter().map(|r| r.id).collect();
    assert!(ids.contains(&rows[0].id), "未被认领的行一直可见");
    assert!(ids.contains(&rows[1].id), "被释放的行回到池子");
    assert!(ids.contains(&rows[3].id), "被释放的行回到池子");
    assert!(!ids.contains(&rows[2].id), "没被释放的认领行必须保持不可见");
}

#[tokio::test]
async fn trim_archived_messages_keeps_the_tail_and_the_newest_archive() {
    let (_d, s) = store().await;
    s.ensure_group("bot", "g", 100).await.expect("建组");
    for i in 0..5 {
        s.insert_group_message(
            "bot",
            "g",
            &format!("m{i}"),
            "u",
            Some("u"),
            "member",
            "几句话",
            false,
            None,
            None,
            100 + i,
        )
        .await
        .expect("消息");
    }
    // 前两行已完成折叠，后三行还是近期尾巴。
    s.archive_group_messages(&[1, 2]).await.expect("折叠");
    let removed = s
        .trim_archived_messages("bot", "g", 1, 200)
        .await
        .expect("裁剪");
    assert_eq!(removed, 1, "只该删掉最旧的一行已折叠记录");
    assert_eq!(
        s.unarchived_message_count("bot", "g").await.expect("计数"),
        3,
        "未折叠的尾巴一条都不能动"
    );
}

#[tokio::test]
async fn image_tags_land_on_their_own_message_row() {
    let (_d, s) = store().await;
    s.ensure_group("bot", "g", 100).await.expect("建组");
    s.insert_group_message(
        "bot",
        "g",
        "m1",
        "u",
        Some("u"),
        "member",
        "[图片]",
        true,
        None,
        Some("0"),
        100,
    )
    .await
    .expect("消息");
    assert!(
        s.set_message_image_tags("bot", "g", "m1", r#"[{"text":"photo"}]"#)
            .await
            .expect("回写")
    );
    let rows = s.recent_group_messages("bot", "g", 10).await.expect("读");
    assert_eq!(rows[0].image_tags.as_deref(), Some(r#"[{"text":"photo"}]"#));
    // A message that does not exist is a miss, not an error.
    assert!(
        !s.set_message_image_tags("bot", "g", "nope", "[]")
            .await
            .expect("未命中")
    );
}

#[tokio::test]
async fn seen_messages_dedupe() {
    let (_d, s) = store().await;
    assert!(s.mark_seen("bot", "1", 100).await.expect("首次"));
    assert!(!s.mark_seen("bot", "1", 101).await.expect("重复"));
    assert!(s.mark_seen("bot", "2", 100).await.expect("另一条"));
    let removed = s.prune_seen(150).await.expect("清理");
    assert_eq!(removed, 2);
    assert!(
        s.mark_seen("bot", "1", 200)
            .await
            .expect("清理后可再次记录")
    );
}

#[tokio::test]
async fn memories_bump_revision_and_are_group_scoped() {
    let (_d, s) = store().await;
    s.ensure_group("bot", "100", 1).await.expect("建组");
    s.ensure_group("bot", "200", 1).await.expect("建组");
    let r1 = s
        .insert_memory("bot", "100", None, "喜欢猫", "topic", "[]", 0.8, 1)
        .await
        .expect("新增");
    assert_eq!(s.memory_revision("bot", "100").await.expect("修订号"), 1);
    s.insert_memory("bot", "200", None, "喜欢狗", "topic", "[]", 0.8, 1)
        .await
        .expect("新增");
    let list = s.list_memories("bot", "100", 50).await.expect("查询");
    assert_eq!(list.len(), 1);
    assert_eq!(list[0].content, "喜欢猫");
    assert!(
        s.update_memory("bot", "100", &r1, "喜欢猫和鸟", 0.9, 3)
            .await
            .expect("更新")
    );
    assert_eq!(s.memory_revision("bot", "100").await.expect("修订号"), 2);
    // Cross-group update must not touch the other group's row.
    assert!(
        !s.update_memory("bot", "200", &r1, "x", 0.5, 4)
            .await
            .expect("跨群")
    );
    assert!(s.delete_memory("bot", "100", &r1, 5).await.expect("删除"));
    assert_eq!(s.memory_stats("bot", "100").await.expect("统计").0, 0);
}

#[tokio::test]
async fn memory_create_and_update_write_audit_rows_with_the_real_actor() {
    let (_d, s) = store().await;
    s.ensure_group("bot", "100", 1).await.expect("建组");
    let r1 = s
        .insert_memory("bot", "100", None, "喜欢猫", "topic", "[]", 0.8, 10)
        .await
        .expect("新增");
    s.update_memory("bot", "100", &r1, "喜欢猫和鸟", 0.9, 20)
        .await
        .expect("更新");
    s.insert_memory_with_origin(
        "bot",
        "100",
        None,
        "群里约好周五联机",
        "event",
        "[]",
        0.6,
        crate::store::origin::HY3,
        30,
    )
    .await
    .expect("提取写入");

    let events: Vec<(String, String)> =
        sqlx::query_as("SELECT actor_kind, action FROM audit_events ORDER BY id")
            .fetch_all(&s.pool)
            .await
            .expect("审计查询");
    let got: Vec<(String, String)> = events
        .into_iter()
        .filter(|(_, a)| a.starts_with("memory."))
        .collect();
    assert_eq!(
        got,
        vec![
            ("agy".into(), "memory.create".into()),
            ("agy".into(), "memory.update".into()),
            ("hy3_extract".into(), "memory.create".into()),
        ]
    );
}

#[tokio::test]
async fn audit_rolls_back_with_its_memory_write() {
    let (_d, s) = store().await;
    s.ensure_group("bot", "100", 1).await.expect("建组");
    let r1 = s
        .insert_memory("bot", "100", None, "喜欢猫", "topic", "[]", 0.8, 10)
        .await
        .expect("新增");
    s.update_memory("bot", "100", &r1, "喜欢猫和鸟", 0.9, 20)
        .await
        .expect("更新");
    let before: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM audit_events")
        .fetch_one(&s.pool)
        .await
        .expect("计数");
    // A miss writes nothing at all: no phantom `memory.update` row.
    assert!(
        !s.update_memory("bot", "100", "mem_missing", "x", 0.5, 30)
            .await
            .expect("未命中")
    );
    let after: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM audit_events")
        .fetch_one(&s.pool)
        .await
        .expect("计数");
    assert_eq!(before, after);
}

#[tokio::test]
async fn origin_defaults_to_agy_and_is_recorded_per_writer() {
    let (_d, s) = store().await;
    s.ensure_group("bot", "100", 1).await.expect("建组");
    s.insert_memory("bot", "100", None, "喜欢猫", "topic", "[]", 0.8, 10)
        .await
        .expect("新增");
    s.insert_memory_with_origin(
        "bot",
        "100",
        None,
        "喜欢下雨天",
        "preference",
        "[]",
        0.7,
        crate::store::origin::HY3,
        11,
    )
    .await
    .expect("提取写入");
    let list = s.list_memories("bot", "100", 50).await.expect("查询");
    let mut origins: Vec<String> = list.into_iter().map(|m| m.origin).collect();
    origins.sort();
    assert_eq!(origins, vec!["agy".to_string(), "hy3".to_string()]);
}

#[tokio::test]
async fn archived_rows_leave_the_active_list_and_refuse_updates() {
    let (_d, s) = store().await;
    s.ensure_group("bot", "100", 1).await.expect("建组");
    let live = s
        .insert_memory("bot", "100", None, "喜欢猫", "topic", "[]", 0.8, 10)
        .await
        .expect("新增");
    let dead = s
        .insert_memory("bot", "100", None, "喜欢狗", "topic", "[]", 0.8, 11)
        .await
        .expect("新增");
    sqlx::query("UPDATE memories SET archived_at = ? WHERE memory_ref = ?")
        .bind(12_i64)
        .bind(&dead)
        .execute(&s.pool)
        .await
        .expect("归档");

    let active = s.list_memories("bot", "100", 50).await.expect("查询");
    assert_eq!(active.len(), 1);
    assert_eq!(active[0].memory_ref, live);
    // The operator view still sees both.
    assert_eq!(
        s.list_memories_page("bot", "100", 50, 0)
            .await
            .expect("面板查询")
            .len(),
        2
    );

    let err = s
        .update_memory("bot", "100", &dead, "喜欢猫", 0.9, 20)
        .await
        .expect_err("归档行必须拒写");
    assert!(matches!(err, crate::error::BotError::MemoryArchived(ref r) if r == &dead));
    // The refusal must not have bumped the revision or written an audit row.
    assert_eq!(s.memory_revision("bot", "100").await.expect("修订号"), 2);
    let updates: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM audit_events WHERE action = 'memory.update'")
            .fetch_one(&s.pool)
            .await
            .expect("计数");
    assert_eq!(updates, 0);
}

#[tokio::test]
async fn outbox_unknown_is_not_retried_automatically() {
    let (_d, s) = store().await;
    s.ensure_group("bot", "100", 1).await.expect("建组");
    let id = s
        .enqueue_outbox("bot", "100", "b1", "hi", 10)
        .await
        .expect("入队");
    s.mark_outbox(&id, OutboxState::Sending, None, 11)
        .await
        .expect("sending");
    // Simulate a crash mid-send.
    assert_eq!(s.reconcile_outbox(12).await.expect("收敛"), 1);
    let rec = s.get_outbox(&id).await.expect("查询").expect("存在");
    assert_eq!(rec.state, "unknown");
    assert!(
        s.outbox_batch(OutboxState::Pending, 10)
            .await
            .expect("查询")
            .is_empty()
    );
    assert!(s.retry_unknown(&id, 13).await.expect("重试"));
    assert_eq!(
        s.outbox_batch(OutboxState::Pending, 10)
            .await
            .expect("查询")
            .len(),
        1
    );
}

#[tokio::test]
async fn jobs_are_reconciled_after_restart() {
    let (_d, s) = store().await;
    s.ensure_group("bot", "100", 1).await.expect("建组");
    let job = s
        .create_job("bot", "100", 1, "panel", 10)
        .await
        .expect("建任务");
    s.mark_job_running(&job.id, 11).await.expect("运行");
    assert_eq!(s.reconcile_jobs(12).await.expect("收敛"), 1);
    let after = s.get_job(&job.id).await.expect("查询").expect("存在");
    assert_eq!(after.state, "failed");
    assert_eq!(after.error_class.as_deref(), Some("interrupted"));
}

#[tokio::test]
async fn orphaned_claims_are_released_back_to_the_pool() {
    let (_d, s) = store().await;
    s.ensure_group("bot", "100", 1).await.expect("建组");
    s.ensure_group("bot", "200", 1).await.expect("建组");
    // Two claimed rows in group 100, one unclaimed; one claimed in group 200.
    for (group, mid) in [("100", "a"), ("100", "b"), ("100", "c"), ("200", "d")] {
        s.insert_pending("bot", group, mid, "7", 10, "{}", "[]")
            .await
            .expect("入队");
    }
    let rows100 = s.pending_for_group("bot", "100").await.expect("读");
    s.claim_batch(&[rows100[0].id, rows100[1].id], "b-dead")
        .await
        .expect("认领");
    let rows200 = s.pending_for_group("bot", "200").await.expect("读");
    s.claim_batch(&[rows200[0].id], "b-dead-2")
        .await
        .expect("认领");
    // The flush tick can no longer see the claimed rows: that is the swallow.
    assert_eq!(s.pending_count("bot", "100").await.expect("计"), 1);

    let released = s.release_orphaned_claims("bot").await.expect("释放");
    assert_eq!(
        released,
        vec![("100".to_string(), 2), ("200".to_string(), 1)]
    );
    // Everything is visible to the tick again, and a second pass finds nothing.
    assert_eq!(s.pending_count("bot", "100").await.expect("计"), 3);
    assert_eq!(s.pending_count("bot", "200").await.expect("计"), 1);
    assert!(
        s.release_orphaned_claims("bot")
            .await
            .expect("再释放")
            .is_empty()
    );
}

#[tokio::test]
async fn state_round_trips() {
    let (_d, s) = store().await;
    s.ensure_group("bot", "100", 1).await.expect("建组");
    s.put_state("bot", "100", Some(r#"{"label":"calm"}"#), Some("闲聊中"), 5)
        .await
        .expect("写入");
    let st = s
        .get_state("bot", "100")
        .await
        .expect("查询")
        .expect("存在");
    assert_eq!(st.topic_state.as_deref(), Some("闲聊中"));
    assert_eq!(st.updated_at, 5);
}

#[tokio::test]
async fn data_survives_reopen() {
    let dir = tempfile::tempdir().expect("临时目录");
    let path = dir.path().join("bot.sqlite3");
    {
        let pool = migrations::open_and_migrate(&path).await.expect("迁移");
        let s = SqliteStore::new(pool);
        s.ensure_group("bot", "100", 1).await.expect("建组");
        s.insert_memory("bot", "100", None, "持久化事实", "fact", "[]", 0.7, 1)
            .await
            .expect("新增");
    }
    let pool = migrations::open_and_migrate(&path).await.expect("重新打开");
    let s = SqliteStore::new(pool);
    let list = s.list_memories("bot", "100", 10).await.expect("查询");
    assert_eq!(list.len(), 1);
    assert_eq!(list[0].content, "持久化事实");
}

/// A cached sticker is shared by every group: one sha256, one row. The group recorded
/// on the row is provenance for the panel, not a scope, so group "200" must not get a
/// second copy of the same bytes.
#[tokio::test]
async fn sticker_cache_is_shared_across_groups() {
    let (_d, s) = store().await;
    s.upsert_sticker(
        "h1",
        "bot",
        "100",
        "/p/h1.jpg",
        "sticker",
        "other",
        "字",
        "画面",
        "怒",
        "blob",
        None,
        false,
        true,
        1,
    )
    .await
    .expect("入库");
    assert!(
        s.sticker_by_hash("bot", "h1")
            .await
            .expect("查询")
            .is_some()
    );
    assert!(
        s.sticker_by_hash("bot", "nope")
            .await
            .expect("查询")
            .is_none()
    );
}

/// The whole point of the split: caching never puts a sticker in her library. Only
/// `set_sticker_kept` does, and only `kept = 1` is searchable.
#[tokio::test]
async fn only_kept_stickers_are_in_the_library() {
    let (_d, s) = store().await;
    for (hash, kept) in [("keep", 1), ("unjudged", 0), ("refused", -1)] {
        s.upsert_sticker(
            hash, "bot", "100", "/p/x.jpg", "sticker", "other", "", "", "", "", None, false, true,
            1,
        )
        .await
        .expect("入库");
        if kept != 0 {
            assert!(s.set_sticker_kept("bot", hash, kept).await.expect("判定"));
        }
    }
    let library = s.list_library("bot", 100).await.expect("读取库");
    assert_eq!(library.len(), 1, "只有她收下的才在库里");
    assert_eq!(library[0].hash, "keep");

    // A fresh row is unjudged, never auto-kept: the tagger describes, she decides.
    assert_eq!(
        s.sticker_by_hash("bot", "unjudged")
            .await
            .expect("查询")
            .map(|r| r.kept),
        Some(super::super::KEPT_UNJUDGED)
    );
}

/// A refused sticker must stay refused even if the tagger runs again with a better
/// description; otherwise her "no" gets overwritten by a later model pass.
#[tokio::test]
async fn a_refused_sticker_is_never_promoted_by_a_retag() {
    let (_d, s) = store().await;
    s.upsert_sticker(
        "h", "bot", "100", "/p/h.jpg", "sticker", "other", "旧", "", "", "", None, false, true, 1,
    )
    .await
    .expect("入库");
    s.set_sticker_kept("bot", "h", super::super::KEPT_REFUSED)
        .await
        .expect("判定");
    s.upsert_sticker(
        "h",
        "bot",
        "100",
        "/p/h.jpg",
        "sticker",
        "other",
        "新描述",
        "",
        "",
        "",
        None,
        false,
        true,
        2,
    )
    .await
    .expect("重标");
    let row = s
        .sticker_by_hash("bot", "h")
        .await
        .expect("查询")
        .expect("存在");
    assert_eq!(row.kept, super::super::KEPT_REFUSED);
    assert_eq!(row.ocr, "新描述", "描述可以刷新");
}

/// The model only sees an 8-char handle, so a prefix has to resolve. An ambiguous or
/// unknown prefix must resolve to nothing rather than to a guess.
#[tokio::test]
async fn short_handles_resolve_only_when_unambiguous() {
    let (_d, s) = store().await;
    for hash in ["aabbccdd1111", "aabbccdd2222", "ffee0000"] {
        s.upsert_sticker(
            hash, "bot", "100", "/p/x.jpg", "sticker", "other", "", "", "", "", None, false, true,
            1,
        )
        .await
        .expect("入库");
    }
    assert_eq!(
        s.find_sticker_by_prefix("bot", "ffee").await.expect("查询"),
        Some("ffee0000".to_string())
    );
    assert_eq!(
        s.find_sticker_by_prefix("bot", "aabbccdd")
            .await
            .expect("查询"),
        None,
        "前缀命中两张就不能猜"
    );
    assert_eq!(
        s.find_sticker_by_prefix("bot", "zzzz").await.expect("查询"),
        None
    );
}

#[tokio::test]
async fn ambient_pace_defaults_to_normal_and_roundtrips() {
    let (_dir, s) = store().await;
    s.ensure_group("bot", "100", 1).await.expect("建组");
    let group = s
        .get_group("bot", "100")
        .await
        .expect("读组")
        .expect("有组");
    assert_eq!(group.ambient_pace, "normal");
    assert_eq!(group.pace(), crate::store::AmbientPace::Normal);
    assert!(
        s.set_group_ambient_pace("bot", "100", "quiet", 2)
            .await
            .expect("写档位")
    );
    let group = s
        .get_group("bot", "100")
        .await
        .expect("读组")
        .expect("有组");
    assert_eq!(group.pace(), crate::store::AmbientPace::Quiet);
    assert!(
        !s.set_group_ambient_pace("bot", "404", "quiet", 2)
            .await
            .expect("写不存在的组")
    );
}
