//! Tests for the intake pipeline. Moved out of intake.rs verbatim
//! (std-split-intake); assertions unchanged.

use super::*;
use crate::store::migrations;

const OWNER: &str = "10001";
const BOT: &str = "9999";
/// The default quiet window the test fixture configures, in seconds.
const QUIET_SECONDS: i64 = 5;

fn limits() -> IntakeLimits {
    IntakeLimits {
        quiet_seconds: 5,
        max_age_seconds: 30,
        batch: BatchLimits::default(),
        hourly_call_budget: 60,
        flood_enabled: true,
    }
}

/// A fixture like [`intake`] but with a caller-chosen hourly call budget,
/// so the fuse can be tripped with a handful of seeded calls.
async fn intake_with_budget(budget: u32) -> (Intake, Arc<SqliteStore>, tempfile::TempDir) {
    let dir = tempfile::tempdir().expect("tmp");
    let path = dir.path().join("bot.sqlite3");
    let pool = migrations::open_and_migrate(&path).await.expect("迁移");
    let store = Arc::new(SqliteStore::new(pool));
    store.ensure_group(BOT, "500", 1).await.expect("建组");
    store
        .set_group_flags(BOT, "500", Some(true), Some(false), None, 1)
        .await
        .expect("开启");
    let mut limits = limits();
    limits.hourly_call_budget = budget;
    (
        Intake::new(
            Arc::clone(&store),
            BOT.to_string(),
            OWNER.to_string(),
            limits,
        ),
        store,
        dir,
    )
}

/// The default fixture: a group already discovered and switched on, so the tests
/// below exercise intake/batching rather than the enable gate. Tests about
/// discovery use [`bare_intake`] instead.
async fn intake() -> (Intake, Arc<SqliteStore>, tempfile::TempDir) {
    let (intake, store, dir) = bare_intake().await;
    // The row has to exist before a flag update can land on it.
    store.ensure_group(BOT, "500", 1).await.expect("建组");
    store
        .set_group_flags(BOT, "500", Some(true), Some(false), None, 1)
        .await
        .expect("开启");
    (intake, store, dir)
}

/// A fixture with no group rows: whatever the test sends is being seen for the
/// first time, so the group is created disabled.
async fn bare_intake() -> (Intake, Arc<SqliteStore>, tempfile::TempDir) {
    let dir = tempfile::tempdir().expect("tmp");
    let path = dir.path().join("bot.sqlite3");
    let pool = migrations::open_and_migrate(&path).await.expect("迁移");
    let store = Arc::new(SqliteStore::new(pool));
    (
        Intake::new(
            Arc::clone(&store),
            BOT.to_string(),
            OWNER.to_string(),
            limits(),
        ),
        store,
        dir,
    )
}

fn group_event(group: &str, user: &str, message_id: &str, text: &str) -> Value {
    serde_json::json!({
        "post_type": "message",
        "message_type": "group",
        "self_id": BOT,
        "group_id": group,
        "user_id": user,
        "message_id": message_id,
        "time": 1_700_000_000,
        "message": [{"type": "text", "data": {"text": text}}],
        "sender": {"user_id": user, "nickname": "昵称", "card": "群名片"}
    })
}

/// The same event with an `at` segment in front, so `mentions_bot` is set.
fn group_event_mentioning_bot(group: &str, user: &str, message_id: &str, text: &str) -> Value {
    serde_json::json!({
        "post_type": "message",
        "message_type": "group",
        "self_id": BOT,
        "group_id": group,
        "user_id": user,
        "message_id": message_id,
        "time": 1_700_000_000,
        "message": [
            {"type": "at", "data": {"qq": BOT}},
            {"type": "text", "data": {"text": text}}
        ],
        "sender": {"user_id": user, "nickname": "昵称", "card": "群名片"}
    })
}

/// A fixture with a fixed pacing input, so the tier logic runs end to end without a
/// quota scrape behind it.
async fn intake_with_pacing(
    input: crate::app::pacing::PacingInput,
    budget: u32,
) -> (Intake, Arc<SqliteStore>, tempfile::TempDir) {
    let (intake, store, dir) = intake_with_budget(budget).await;
    let engine = Arc::new(crate::app::pacing::PacingEngine::fixed(
        Arc::clone(&store),
        input,
        BOT.to_string(),
        budget,
    ));
    (intake.with_pacing(engine), store, dir)
}

#[tokio::test]
async fn a_new_group_is_discovered_disabled() {
    let (intake, store, _dir) = bare_intake().await;
    // First message in an unknown group is still stored in the log...
    assert_eq!(
        intake.handle(&group_event("500", "7", "1", "hi")).await,
        IntakeOutcome::Skipped(SkipReason::GroupInactive.as_str())
    );
    let group = store
        .get_group(BOT, "500")
        .await
        .expect("读")
        .expect("存在");
    // Discovery alone must never be enough to start answering.
    assert!(!group.enabled);
    assert!(!group.paused);
}

#[tokio::test]
async fn an_operator_switches_a_discovered_group_on() {
    let (intake, store, _dir) = bare_intake().await;
    // A brand new group is recorded but silent.
    assert_eq!(
        intake.handle(&group_event("500", "7", "1", "hi")).await,
        IntakeOutcome::Skipped(SkipReason::GroupInactive.as_str())
    );
    assert!(
        !store
            .get_group(BOT, "500")
            .await
            .expect("读")
            .expect("存在")
            .enabled
    );
    assert_eq!(store.pending_count(BOT, "500").await.expect("计"), 0);

    // The panel is the only switch. Once on, the group behaves normally.
    store
        .set_group_flags(BOT, "500", Some(true), Some(false), None, 2)
        .await
        .expect("开启");
    assert_eq!(
        intake
            .handle(&group_event("500", "7", "2", "hi again"))
            .await,
        IntakeOutcome::Stored
    );
    assert_eq!(store.pending_count(BOT, "500").await.expect("计"), 1);
}

#[tokio::test]
async fn duplicate_message_ids_are_stored_once() {
    let (intake, _store, _dir) = intake().await;
    assert_eq!(
        intake.handle(&group_event("500", "7", "1", "hi")).await,
        IntakeOutcome::Stored
    );
    assert_eq!(
        intake.handle(&group_event("500", "7", "1", "hi")).await,
        IntakeOutcome::Duplicate
    );
}

#[tokio::test]
async fn dedupe_survives_a_restart() {
    let dir = tempfile::tempdir().expect("tmp");
    let path = dir.path().join("bot.sqlite3");
    {
        let pool = migrations::open_and_migrate(&path).await.expect("迁移");
        let store = Arc::new(SqliteStore::new(pool));
        store.ensure_group(BOT, "500", 1).await.expect("建组");
        store
            .set_group_flags(BOT, "500", Some(true), Some(false), None, 1)
            .await
            .expect("开启");
        let intake = Intake::new(store, BOT.to_string(), OWNER.to_string(), limits());
        assert_eq!(
            intake.handle(&group_event("500", "7", "1", "hi")).await,
            IntakeOutcome::Stored
        );
    }
    // Fresh process, same database: the message must not be reprocessed.
    let pool = migrations::open_and_migrate(&path).await.expect("重开");
    let store = Arc::new(SqliteStore::new(pool));
    let intake = Intake::new(store, BOT.to_string(), OWNER.to_string(), limits());
    assert_eq!(
        intake.handle(&group_event("500", "7", "1", "hi")).await,
        IntakeOutcome::Duplicate
    );
}

#[tokio::test]
async fn private_and_self_messages_are_skipped() {
    let (intake, _store, _dir) = intake().await;
    let private = serde_json::json!({
        "post_type": "message",
        "message_type": "private",
        "self_id": BOT,
        "user_id": "7",
        "message_id": "1",
        "message": "hi"
    });
    assert_eq!(
        intake.handle(&private).await,
        IntakeOutcome::Skipped("private_message")
    );

    // The bot's own echo.
    let own = group_event("500", BOT, "2", "hi");
    assert_eq!(
        intake.handle(&own).await,
        IntakeOutcome::Skipped("self_message")
    );
}

#[tokio::test]
async fn owner_commands_are_recognized_before_enqueueing() {
    let (intake, store, _dir) = intake().await;
    let outcome = intake
        .handle(&group_event("500", OWNER, "1", "/gemini pause"))
        .await;
    assert!(matches!(outcome, IntakeOutcome::Command { .. }));
    // The group is paused and nothing was stored as a message.
    let group = store
        .get_group(BOT, "500")
        .await
        .expect("读")
        .expect("存在");
    assert!(group.paused);
    assert_eq!(store.pending_count(BOT, "500").await.expect("读"), 0);
}

#[tokio::test]
async fn a_non_owner_command_is_ordinary_chat() {
    let (intake, store, _dir) = intake().await;
    let outcome = intake
        .handle(&group_event("500", "7", "1", "/gemini pause"))
        .await;
    // Not a command: stored as a message instead.
    assert_eq!(outcome, IntakeOutcome::Stored);
    let group = store
        .get_group(BOT, "500")
        .await
        .expect("读")
        .expect("存在");
    assert!(!group.paused);
}

#[tokio::test]
async fn nickname_never_confers_ownership() {
    let (intake, _store, _dir) = intake().await;
    // The nickname is copied from the owner's; the id is not.
    let mut event = group_event("500", "7", "1", "hi");
    event["sender"]["nickname"] = serde_json::json!("群主");
    event["sender"]["card"] = serde_json::json!(OWNER);
    let outcome = intake.handle(&event).await;
    assert_eq!(outcome, IntakeOutcome::Stored);
}

#[tokio::test]
async fn a_paused_group_accumulates_nothing() {
    let (intake, store, _dir) = intake().await;
    intake
        .handle(&group_event("500", OWNER, "1", "/gemini pause"))
        .await;
    assert_eq!(
        intake.handle(&group_event("500", "7", "2", "hi")).await,
        IntakeOutcome::Skipped("group_inactive")
    );
    assert_eq!(store.pending_count(BOT, "500").await.expect("读"), 0);
}

#[tokio::test]
async fn a_disabled_group_accumulates_nothing_and_is_dropped_on_tick() {
    let (intake, store, _dir) = intake().await;
    intake.handle(&group_event("500", "7", "1", "hi")).await;
    assert_eq!(store.pending_count(BOT, "500").await.expect("读"), 1);
    store
        .set_group_flags(BOT, "500", Some(false), None, None, 10)
        .await
        .expect("停用");
    let jobs = intake.flush_tick(1_000, None).await;
    assert!(jobs.is_empty());
    // The tick dropped the stored body.
    assert_eq!(store.pending_count(BOT, "500").await.expect("读"), 0);
    assert_eq!(
        intake.handle(&group_event("500", "7", "2", "hi")).await,
        IntakeOutcome::Skipped("group_inactive")
    );
}

#[tokio::test]
async fn quiet_window_then_flush() {
    let (intake, store, _dir) = intake().await;
    store.ensure_group(BOT, "500", 0).await.expect("建群");
    // Store with a controllable timestamp.
    intake.handle(&group_event("500", "7", "1", "hi")).await;
    let rows = store.pending_for_group(BOT, "500").await.expect("读");
    assert_eq!(rows.len(), 1);
    let arrived = rows[0].received_at;

    // 4s later: still inside the quiet window.
    assert!(intake.flush_tick(arrived + 4, None).await.is_empty());
    // 5s later: flush.
    let jobs = intake.flush_tick(arrived + 5, None).await;
    assert_eq!(jobs.len(), 1);
    assert_eq!(jobs[0].event_ids.len(), 1);
}

#[tokio::test]
async fn flush_skips_a_group_whose_turn_is_already_running() {
    let (intake, store, _dir) = intake().await;
    let running = Arc::new(crate::app::turn::RunningTurns::new());
    running.register("500", 1);
    let intake = intake.with_running_turns(Arc::clone(&running));
    intake.handle(&group_event("500", "7", "1", "hi")).await;
    let rows = store.pending_for_group(BOT, "500").await.expect("读");
    let arrived = rows[0].received_at;
    assert!(
        intake
            .flush_tick(arrived + QUIET_SECONDS, None)
            .await
            .is_empty(),
        "flush must not claim while a turn is running"
    );
    assert_eq!(
        store.pending_for_group(BOT, "500").await.expect("读").len(),
        1,
        "rows stay unclaimed for insert"
    );
    running.deregister("500");
    let jobs = intake.flush_tick(arrived + QUIET_SECONDS, None).await;
    assert_eq!(jobs.len(), 1);
}

#[tokio::test]
async fn at_bot_flushes_immediately_without_a_gate() {
    let (intake, store, _dir) = intake().await;
    let mut event = group_event("500", "7", "1", "看这个");
    event["message"] = serde_json::json!([
        {"type": "at", "data": {"qq": BOT}},
        {"type": "text", "data": {"text": "看这个"}}
    ]);
    intake.handle(&event).await;
    let rows = store.pending_for_group(BOT, "500").await.expect("读");
    let arrived = rows[0].received_at;
    // With no gate there is nobody to judge whether the sentence is finished, so an
    // @ is the only "send now" signal left and it is taken at once.
    let released = intake.flush_tick(arrived, None).await;
    assert_eq!(released.len(), 1);
    assert!(released[0].privileged);
}

#[tokio::test]
async fn owner_text_does_not_get_its_own_batch() {
    let (intake, store, _dir) = intake().await;
    intake
        .handle(&group_event("500", OWNER, "1", "随便说说"))
        .await;
    let rows = store.pending_for_group(BOT, "500").await.expect("读");
    let arrived = rows[0].received_at;
    // Whether she answers the owner is her decision; the batcher must not split the
    // conversation just because the speaker happens to be the owner.
    assert!(intake.flush_tick(arrived, None).await.is_empty());
    let released = intake.flush_tick(arrived + QUIET_SECONDS, None).await;
    assert_eq!(released.len(), 1);
    assert!(!released[0].privileged);
}

#[tokio::test]
async fn hard_age_flushes_even_when_messages_keep_arriving() {
    let (intake, store, _dir) = intake().await;
    intake.handle(&group_event("500", "7", "1", "a")).await;
    let rows = store.pending_for_group(BOT, "500").await.expect("读");
    let arrived = rows[0].received_at;
    // A second message arrives later, refreshing last_activity, but the batch is
    // already 30s old measured from the first message.
    store
        .insert_pending(BOT, "500", "2", "7", arrived + 29, "{}", "[]")
        .await
        .expect("插");
    let jobs = intake.flush_tick(arrived + 30, None).await;
    assert_eq!(jobs.len(), 1);
    // Both rows belong to the same batch.
    assert_eq!(jobs[0].event_ids.len(), 2);
}

#[tokio::test]
async fn batches_are_claimed_so_a_second_tick_cannot_duplicate_them() {
    let (intake, store, _dir) = intake().await;
    let mut event = group_event("500", "7", "1", "hi");
    event["message"] = serde_json::json!([
        {"type": "at", "data": {"qq": BOT}},
        {"type": "text", "data": {"text": "看看这个"}}
    ]);
    intake.handle(&event).await;
    let rows = store.pending_for_group(BOT, "500").await.expect("读");
    let now = rows[0].received_at + QUIET_SECONDS;
    assert_eq!(intake.flush_tick(now, None).await.len(), 1);
    assert!(intake.flush_tick(now, None).await.is_empty());
}

#[tokio::test]
async fn group_order_is_fifo_within_one_group() {
    let (intake, store, _dir) = intake().await;
    for i in 1..=5 {
        let mut event = group_event("500", "7", &i.to_string(), &format!("m{i}"));
        event["message"] = serde_json::json!([
            {"type": "at", "data": {"qq": BOT}},
            {"type": "text", "data": {"text": "看看这个"}}
        ]);
        intake.handle(&event).await;
    }
    let rows = store.pending_for_group(BOT, "500").await.expect("读");
    let ids: Vec<i64> = rows.iter().map(|r| r.id).collect();
    let jobs = intake
        .flush_tick(rows[0].received_at + QUIET_SECONDS, None)
        .await;
    assert_eq!(jobs.len(), 1);
    assert_eq!(jobs[0].event_ids, ids);
}

#[tokio::test]
async fn a_stored_message_also_lands_in_the_raw_group_log() {
    let (intake, store, _dir) = intake().await;
    assert_eq!(
        intake
            .handle(&group_event("500", "7", "1", "这条要留下"))
            .await,
        IntakeOutcome::Stored
    );
    let rows = store
        .recent_group_messages(BOT, "500", 10)
        .await
        .expect("读");
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].message_id, "1");
    assert_eq!(rows[0].text, "这条要留下");
    assert_eq!(rows[0].speaker_role, "member");
    assert_eq!(rows[0].display_name.as_deref(), Some("群名片"));
    assert!(!rows[0].archived);
}

#[tokio::test]
async fn an_at_of_another_member_names_its_target_and_a_bare_at_is_stored() {
    // The 2026-11-03 incident, end to end: "@小鲸鱼" vanished and
    // "@小鲸鱼 你咋还活着" reached the model as a bare "你咋还活着", so
    // she answered teasing aimed at somebody else as if it were hers.
    let (intake, store, _dir) = intake().await;
    // The target speaks once, so the log knows his name.
    let mut first = group_event("500", "42", "1", "叫我干嘛");
    first["sender"]["card"] = serde_json::json!("小鲸鱼");
    assert_eq!(intake.handle(&first).await, IntakeOutcome::Stored);
    // A bare "@42" (no text at all) is stored, not dropped as empty.
    let mut bare = group_event("500", "7", "2", "");
    bare["message"] = serde_json::json!([{"type": "at", "data": {"qq": "42"}}]);
    assert_eq!(intake.handle(&bare).await, IntakeOutcome::Stored);
    // "@42 你咋还活着": the stored text names the addressee.
    let mut aimed = group_event("500", "7", "3", "");
    aimed["message"] = serde_json::json!([
        {"type": "at", "data": {"qq": "42"}},
        {"type": "text", "data": {"text": " 你咋还活着"}}
    ]);
    assert_eq!(intake.handle(&aimed).await, IntakeOutcome::Stored);

    let log = store
        .recent_group_messages(BOT, "500", 10)
        .await
        .expect("读");
    let by_id: std::collections::HashMap<&str, &str> = log
        .iter()
        .map(|r| (r.message_id.as_str(), r.text.as_str()))
        .collect();
    assert!(
        by_id["2"].contains("@小鲸鱼"),
        "裸 @ 落库文本: {}",
        by_id["2"]
    );
    assert!(by_id["3"].contains("@小鲸鱼"), "@ 文本: {}", by_id["3"]);
    assert!(by_id["3"].contains("你咋还活着"));

    // The pending payload carries the target's alias (never the raw QQ in
    // the segment), matching the speaker_ref the target's own line got.
    let rows = store.pending_for_group(BOT, "500").await.expect("读");
    let payload_of = |mid: &str| {
        let row = rows.iter().find(|r| r.message_id == mid).expect("行");
        serde_json::from_str::<crate::onebot::normalize::IncomingMessage>(&row.normalized_payload)
            .expect("解析")
    };
    let target_ref = payload_of("1").speaker_ref;
    let aimed_msg = payload_of("3");
    assert_eq!(
        crate::onebot::normalize::mention_refs(&aimed_msg.segments),
        vec![target_ref]
    );
    assert!(!aimed_msg.mentions_bot);
}

#[tokio::test]
async fn the_log_keeps_the_message_after_the_batch_is_consumed() {
    let (intake, store, _dir) = intake().await;
    let mut event = group_event("500", "7", "1", "吃过饭了吗");
    event["message"] = serde_json::json!([
        {"type": "at", "data": {"qq": BOT}},
        {"type": "text", "data": {"text": "吃过饭了吗"}}
    ]);
    intake.handle(&event).await;
    let rows = store.pending_for_group(BOT, "500").await.expect("读");
    let batch = intake
        .flush_tick(rows[0].received_at + QUIET_SECONDS, None)
        .await;
    assert_eq!(batch.len(), 1);
    store.delete_pending(&batch[0].event_ids).await.expect("删");
    // pending_events is gone; the log must still be there.
    let log = store
        .recent_group_messages(BOT, "500", 10)
        .await
        .expect("读");
    assert_eq!(log.len(), 1);
    assert!(log[0].text.contains("吃过饭了吗"));
}

#[tokio::test]
async fn duplicate_delivery_does_not_double_log() {
    let (intake, store, _dir) = intake().await;
    assert_eq!(
        intake.handle(&group_event("500", "7", "1", "第一次")).await,
        IntakeOutcome::Stored
    );
    // Same message id, different body: the unique index wins.
    let _ = intake.handle(&group_event("500", "7", "1", "第二次")).await;
    let count = store.count_group_messages(BOT, "500").await.expect("读");
    assert_eq!(count, 1);
}

#[tokio::test]
async fn the_snowluma_status_trigger_is_never_stored() {
    let (intake, store, _dir) = intake().await;
    assert_eq!(
        intake.handle(&group_event("500", "7", "1", "#sl")).await,
        IntakeOutcome::Skipped("snowluma_command")
    );
    assert_eq!(store.count_group_messages(BOT, "500").await.expect("读"), 0);
}

#[tokio::test]
async fn groups_are_batched_independently() {
    let (intake, store, _dir) = intake().await;
    store.ensure_group(BOT, "600", 1).await.expect("建组");
    store
        .set_group_flags(BOT, "600", Some(true), Some(false), None, 1)
        .await
        .expect("开启");
    for (group, msg) in [("500", "1"), ("600", "2")] {
        let mut event = group_event(group, "7", msg, "hi");
        event["message"] = serde_json::json!([
            {"type": "at", "data": {"qq": BOT}},
            {"type": "text", "data": {"text": "看看这个"}}
        ]);
        intake.handle(&event).await;
    }
    let jobs = intake
        .flush_tick(crate::now_unix() + QUIET_SECONDS, None)
        .await;
    assert_eq!(jobs.len(), 2);
    let mut groups: Vec<&str> = jobs.iter().map(|j| j.group_id.as_str()).collect();
    groups.sort();
    assert_eq!(groups, vec!["500", "600"]);
    assert_eq!(store.pending_count(BOT, "500").await.expect("读"), 0);
}

/// A gate that cannot answer must not hold the burst: `Unavailable` maps to zero
/// hold time, so the first tick after the floor expires releases the batch.
#[tokio::test]
async fn an_unavailable_gate_releases_the_burst_immediately() {
    let (intake, store, dir) = intake().await;
    let gate = disabled_gate(&dir);
    intake.handle(&group_event("500", "7", "1", "在吗")).await;
    let rows = store.pending_for_group(BOT, "500").await.expect("读");
    let arrived = rows[0].received_at;
    // The floor still applies: a burst is never released on the tick it arrives.
    assert!(intake.flush_tick(arrived, Some(&gate)).await.is_empty());
    // Past the floor, a broken gate is not evidence of a pause, so it lets go.
    let jobs = intake
        .flush_tick(
            arrived + crate::app::preflight::MIN_BURST_SECONDS,
            Some(&gate),
        )
        .await;
    assert_eq!(jobs.len(), 1);
}

#[test]
fn budget_exhaustion_is_a_pure_threshold() {
    assert!(!budget_exhausted(0, 60));
    assert!(!budget_exhausted(59, 60));
    assert!(budget_exhausted(60, 60));
    assert!(budget_exhausted(61, 60));
    // Zero disables the fuse outright, however hot the group is running.
    assert!(!budget_exhausted(10_000, 0));
}

#[test]
fn a_repeated_damper_wait_is_audited_once_a_minute() {
    // The first hold is always recorded.
    assert!(should_record_damper_wait(None, 1_000));
    // Then not again inside the interval, however many ticks ask.
    assert!(!should_record_damper_wait(Some(1_000), 1_000));
    assert!(!should_record_damper_wait(Some(1_000), 1_059));
    assert!(should_record_damper_wait(
        Some(1_000),
        1_000 + DAMPER_WAIT_AUDIT_INTERVAL_SECONDS
    ));
    // A clock that runs backwards records nothing new rather than spamming.
    assert!(!should_record_damper_wait(Some(1_000), 999));
}

#[tokio::test]
async fn a_held_batch_does_not_rewrite_its_audit_row_every_tick() {
    let (intake, store, _dir) = intake().await;
    let base = crate::now_unix();
    seed_volume(&intake, "500", PARTNER, base);
    for round in 0..crate::app::damper::SOFT_CAP_ROUNDS {
        intake
            .damper
            .note_reply("500", base - 300 + round as i64, PARTNER, false);
    }
    intake
        .handle(&group_event("500", PARTNER, "1", "还在吗"))
        .await;
    let arrived = store.pending_for_group(BOT, "500").await.expect("读")[0].received_at;

    // The cap is a hard hold, so every tick below is held for the same reason.
    for offset in [5, 10, 55] {
        assert!(intake.flush_tick(arrived + offset, None).await.is_empty());
    }
    let rows = |events: Vec<crate::store::AuditEvent>| {
        events
            .into_iter()
            .filter(|e| e.action == "intake.cadence.wait.damper_cap")
            .count()
    };
    assert_eq!(rows(store.recent_audit(50).await.expect("读审计")), 1);

    // Past the interval the hold states itself again: one row, not one per tick.
    assert!(
        intake
            .flush_tick(arrived + DAMPER_WAIT_AUDIT_INTERVAL_SECONDS + 10, None)
            .await
            .is_empty()
    );
    assert_eq!(rows(store.recent_audit(50).await.expect("读审计")), 2);
}

#[tokio::test]
async fn an_exhausted_budget_defers_the_flush_and_keeps_the_batch_pending() {
    let (intake, store, _dir) = intake_with_budget(2).await;
    intake.handle(&group_event("500", "7", "1", "hi")).await;
    let rows = store.pending_for_group(BOT, "500").await.expect("读");
    let arrived = rows[0].received_at;
    let now = arrived + QUIET_SECONDS;
    // Two calls already started inside the window: the fuse is blown, so
    // even a privileged batch would wait — and a plain one certainly does.
    store
        .start_call(BOT, "500", "b1", arrived)
        .await
        .expect("记调用");
    store
        .start_call(BOT, "500", "b2", arrived)
        .await
        .expect("记调用");

    assert!(intake.flush_tick(now, None).await.is_empty());
    // Nothing was claimed and nothing was dropped: the batch stays pending.
    assert_eq!(store.pending_count(BOT, "500").await.expect("读"), 1);

    // A second tick defers again but must not audit a second row: the
    // throttle is ten minutes and no time has passed.
    assert!(intake.flush_tick(now, None).await.is_empty());
    let events = store.recent_audit(50).await.expect("读审计");
    let defers: Vec<&str> = events
        .iter()
        .map(|e| e.action.as_str())
        .filter(|a| *a == "scheduler.budget.defer.g500.age5")
        .collect();
    assert_eq!(defers.len(), 1, "审计动作: {events:?}");
}

#[tokio::test]
async fn one_call_below_the_budget_still_flushes() {
    let (intake, store, _dir) = intake_with_budget(2).await;
    intake.handle(&group_event("500", "7", "1", "hi")).await;
    let rows = store.pending_for_group(BOT, "500").await.expect("读");
    let arrived = rows[0].received_at;
    store
        .start_call(BOT, "500", "b1", arrived)
        .await
        .expect("记调用");
    let jobs = intake.flush_tick(arrived + QUIET_SECONDS, None).await;
    assert_eq!(jobs.len(), 1);
}

#[tokio::test]
async fn stale_rows_drop_when_the_pool_has_fresh_ones() {
    let (intake, store, _dir) = intake().await;
    // One row 5000s old, one fresh: the old row must be consumed without
    // ever reaching a batch, and the fresh one flushes on its own.
    store
        .insert_pending(BOT, "500", "old", "7", 10_000, "{}", "[]")
        .await
        .expect("插老");
    store
        .insert_pending(BOT, "500", "new", "7", 15_000, "{}", "[]")
        .await
        .expect("插新");
    let rows = store.pending_for_group(BOT, "500").await.expect("读");
    let fresh_id = rows[1].id;
    let jobs = intake.flush_tick(15_000 + QUIET_SECONDS, None).await;
    assert_eq!(jobs.len(), 1);
    assert_eq!(jobs[0].event_ids, vec![fresh_id], "老行不得进批次");
    // The stale row is gone for good, not merely left unclaimed.
    store
        .release_events(&jobs[0].event_ids)
        .await
        .expect("释放");
    let left = store.pending_for_group(BOT, "500").await.expect("读");
    assert_eq!(left.len(), 1);
    assert_eq!(left[0].id, fresh_id);
    let events = store.recent_audit(50).await.expect("读审计");
    assert!(
        events
            .iter()
            .any(|e| e.action == "scheduler.stale_drop.g500.n1.age5005"),
        "审计动作: {events:?}"
    );
}

#[tokio::test]
async fn a_fully_stale_pool_keeps_only_its_newest_segment() {
    let (intake, store, _dir) = intake().await;
    // 60 rows, all far past the stale line: two batch segments. Only the
    // newest segment may be claimed; the older one is consumed.
    for i in 0..60 {
        store
            .insert_pending(BOT, "500", &format!("m{i}"), "7", 10_000 + i, "{}", "[]")
            .await
            .expect("插");
    }
    let rows = store.pending_for_group(BOT, "500").await.expect("读");
    let newest_ids: Vec<i64> = rows[50..].iter().map(|r| r.id).collect();
    let jobs = intake.flush_tick(20_000, None).await;
    assert_eq!(jobs.len(), 1);
    assert_eq!(jobs[0].event_ids, newest_ids, "只保留最新一段");
    let events = store.recent_audit(50).await.expect("读审计");
    assert!(
        events
            .iter()
            .any(|e| e.action.starts_with("scheduler.stale_drop.g500.n50.age")),
        "审计动作: {events:?}"
    );
}

#[tokio::test]
async fn budget_truncation_claims_only_the_newest_segment() {
    let (intake, store, _dir) = intake_with_budget(2).await;
    // One call already spent: exactly one claim remains, but the pool
    // splits into two segments. The newest segment is claimed, the older
    // one is consumed with an audit row — never a silent full-group stall.
    store
        .start_call(BOT, "500", "b0", 50_000)
        .await
        .expect("记调用");
    for i in 0..60 {
        store
            .insert_pending(BOT, "500", &format!("m{i}"), "7", 50_000 + i, "{}", "[]")
            .await
            .expect("插");
    }
    let rows = store.pending_for_group(BOT, "500").await.expect("读");
    let newest_ids: Vec<i64> = rows[50..].iter().map(|r| r.id).collect();
    let jobs = intake.flush_tick(50_100, None).await;
    assert_eq!(jobs.len(), 1);
    assert_eq!(jobs[0].event_ids, newest_ids, "截断时只领最新一段");
    assert_eq!(store.pending_count(BOT, "500").await.expect("计"), 0);
    let events = store.recent_audit(50).await.expect("读审计");
    assert!(
        events
            .iter()
            .any(|e| e.action == "scheduler.budget.trim.g500.n50"),
        "审计动作: {events:?}"
    );
}

#[tokio::test]
async fn flood_drain_fires_only_when_no_claim_can_be_served() {
    let (intake, store, _dir) = intake_with_budget(60).await;
    // The fuse is blown for this window: 60 calls already started.
    for i in 0..60 {
        store
            .start_call(BOT, "500", &format!("seed{i}"), 100_000)
            .await
            .expect("记调用");
    }
    // 25 fresh rows: over the flood threshold, none of them stale.
    for i in 0..25 {
        store
            .insert_pending(BOT, "500", &format!("m{i}"), "7", 100_000 + i, "{}", "[]")
            .await
            .expect("插");
    }
    let rows = store.pending_for_group(BOT, "500").await.expect("读");
    let live_ids: Vec<i64> = rows[10..].iter().map(|r| r.id).collect();
    // Nothing can be claimed, yet the pool still drains to its live
    // tail instead of sitting at 25 behind the blown fuse.
    let jobs = intake.flush_tick(100_100, None).await;
    assert!(jobs.is_empty(), "额度烧穿时不该有领走的批次");
    let kept = store.pending_for_group(BOT, "500").await.expect("读");
    let kept_ids: Vec<i64> = kept.iter().map(|r| r.id).collect();
    assert_eq!(kept_ids, live_ids, "泄洪后只剩最新一段待发");
    let events = store.recent_audit(50).await.expect("读审计");
    assert!(
        events
            .iter()
            .any(|e| e.action == "intake.flood.drain.g500.overflow10.live15"),
        "审计动作: {events:?}"
    );
    // Once the window slides past the seeded calls, the first claim is
    // exactly the live tail — and it carries the flood flag so the
    // prompt can demote the drained context to background.
    let jobs = intake.flush_tick(103_601, None).await;
    assert_eq!(jobs.len(), 1);
    assert!(jobs[0].flooded, "泄洪后的首领要带标记");
    assert_eq!(jobs[0].event_ids, live_ids);
}

#[tokio::test]
async fn a_large_pool_with_budget_claims_normally_and_unflagged() {
    let (intake, store, _dir) = intake().await;
    // Same 25 rows, but the fuse has room: the pool is claimed as one
    // ordinary batch. The drain must not pre-empt the normal path —
    // an earlier version did, and broke budget truncation's segment
    // semantics (see the flood fix commit).
    for i in 0..25 {
        store
            .insert_pending(BOT, "500", &format!("m{i}"), "7", 100_000 + i, "{}", "[]")
            .await
            .expect("插");
    }
    let jobs = intake.flush_tick(100_100, None).await;
    assert_eq!(jobs.len(), 1);
    assert_eq!(jobs[0].event_ids.len(), 25);
    assert!(!jobs[0].flooded, "有额度时不许带泄洪标记");
    let events = store.recent_audit(50).await.expect("读审计");
    assert!(
        !events
            .iter()
            .any(|e| e.action.starts_with("intake.flood.drain")),
        "有额度时不许泄洪: {events:?}"
    );
}

#[test]
fn flood_background_resolution_fails_open() {
    let fallback = "截断版背景".to_string();
    // A real summary wins and is trimmed.
    let (text, kind) = resolve_flood_background(Some("  摘要正文  ".to_string()), fallback.clone());
    assert_eq!(text, "摘要正文");
    assert_eq!(kind, "ok");
    // Empty or missing summaries keep the truncated rendering.
    let (text, kind) = resolve_flood_background(Some("   ".to_string()), fallback.clone());
    assert_eq!(text, "截断版背景");
    assert_eq!(kind, "fallback");
    let (text, kind) = resolve_flood_background(None, fallback);
    assert_eq!(text, "截断版背景");
    assert_eq!(kind, "fallback");
    // A rambling summary is hard-capped at the block budget.
    let long = "长".repeat(2000);
    let (text, kind) = resolve_flood_background(Some(long), "x".to_string());
    assert_eq!(kind, "ok");
    assert_eq!(text.chars().count(), FLOOD_BACKGROUND_MAX_CHARS);
    // The truncated fallback respects the same budget.
    let many = (0..50)
        .map(|i| format!("第{i}行积压消息内容"))
        .collect::<Vec<_>>()
        .join("\n");
    let truncated = truncate_background(&many);
    assert!(truncated.chars().count() <= FLOOD_BACKGROUND_MAX_CHARS);
}

#[tokio::test]
async fn flood_summary_bypass_fails_open_and_rides_the_recovery_claim() {
    let (intake, store, _dir) = intake_with_budget(60).await;
    // A Aux client that is present but disabled: the summary
    // call fails immediately, so the bypass must fall open to the
    // script-truncated rendering and say so in the audit.
    let client = crate::integrations::aux_gateway::AuxGatewayClient::new(
        crate::integrations::aux_gateway::AuxGatewayConfig::new(
            None,
            None,
            std::path::PathBuf::from("/nonexistent-key"),
            false,
        ),
    );
    let intake = intake.with_aux(std::sync::Arc::new(client));
    for i in 0..10 {
        intake
            .handle(&group_event(
                "500",
                "7",
                &format!("o{i}"),
                &format!("溢流话题{i}"),
            ))
            .await;
    }
    for i in 0..15 {
        intake
            .handle(&group_event(
                "500",
                "7",
                &format!("n{i}"),
                &format!("最新话题{i}"),
            ))
            .await;
    }
    let rows = store.pending_for_group(BOT, "500").await.expect("读");
    assert_eq!(rows.len(), 25);
    let arrived = rows[0].received_at;
    for i in 0..60 {
        store
            .start_call(BOT, "500", &format!("seed{i}"), arrived)
            .await
            .expect("记调用");
    }
    // The fuse is blown: nothing is claimed, but the pool drains and
    // the overflow is staged as a background.
    let jobs = intake.flush_tick(arrived + QUIET_SECONDS, None).await;
    assert!(jobs.is_empty());
    // The summary task is async; wait for its audit row.
    let mut saw_fallback = false;
    for _ in 0..60 {
        let events = store.recent_audit(50).await.expect("读审计");
        if events
            .iter()
            .any(|e| e.action == "intake.flood.summary.g500.fallback")
        {
            saw_fallback = true;
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    assert!(saw_fallback, "摘要旁路失败必须回落并留审计");
    // The recovery claim carries the truncated overflow as its
    // background: drained content present, live tail not duplicated.
    let jobs = intake
        .flush_tick(arrived + QUIET_SECONDS + 3601, None)
        .await;
    assert_eq!(jobs.len(), 1);
    assert!(jobs[0].flooded);
    let background = jobs[0]
        .flood_background
        .clone()
        .expect("恢复批次应带积压背景");
    assert!(
        background.contains("溢流话题0"),
        "背景含溢流首条: {background}"
    );
    assert!(
        background.contains("溢流话题9"),
        "背景含溢流末条: {background}"
    );
    assert!(
        !background.contains("最新话题"),
        "活尾巴不进背景: {background}"
    );
    // Consumed: nothing is staged any more.
    assert!(intake.take_flood_background("500").is_none());
}

#[tokio::test]
async fn flood_drain_never_demotes_the_latest_addressed() {
    let (intake, store, _dir) = intake_with_budget(60).await;
    // 10 fillers, one @, then 14 more fillers: the @ sits exactly at
    // the live-tail boundary, and the drain must stop there — the
    // direct question and everything after it stay live.
    for i in 0..10 {
        intake
            .handle(&group_event("500", "7", &format!("f{i}"), "闲聊"))
            .await;
    }
    let mut at_event = group_event("500", "7", "at1", "看这个");
    at_event["message"] = serde_json::json!([
        {"type": "at", "data": {"qq": BOT}},
        {"type": "text", "data": {"text": "看这个"}}
    ]);
    intake.handle(&at_event).await;
    for i in 0..14 {
        intake
            .handle(&group_event("500", "7", &format!("g{i}"), "继续聊"))
            .await;
    }
    let rows = store.pending_for_group(BOT, "500").await.expect("读");
    assert_eq!(rows.len(), 25);
    let arrived = rows[0].received_at;
    // Blow the fuse inside this window so no claim can be served.
    for i in 0..60 {
        store
            .start_call(BOT, "500", &format!("seed{i}"), arrived)
            .await
            .expect("记调用");
    }
    let live_ids: Vec<i64> = rows[10..].iter().map(|r| r.id).collect();
    let jobs = intake.flush_tick(arrived + QUIET_SECONDS, None).await;
    assert!(jobs.is_empty());
    let kept = store.pending_for_group(BOT, "500").await.expect("读");
    let kept_ids: Vec<i64> = kept.iter().map(|r| r.id).collect();
    assert_eq!(kept_ids, live_ids, "活尾巴必须从 @ 那条开始");
}

#[tokio::test]
async fn flood_drain_extends_back_to_an_early_addressed() {
    let (intake, store, _dir) = intake_with_budget(60).await;
    // The only @ is early (index 3): the live set is the union of the
    // newest 15 and everything from that @ on — 22 rows survive.
    for i in 0..3 {
        intake
            .handle(&group_event("500", "7", &format!("f{i}"), "闲聊"))
            .await;
    }
    let mut at_event = group_event("500", "7", "at1", "问你个事");
    at_event["message"] = serde_json::json!([
        {"type": "at", "data": {"qq": BOT}},
        {"type": "text", "data": {"text": "问你个事"}}
    ]);
    intake.handle(&at_event).await;
    for i in 0..21 {
        intake
            .handle(&group_event("500", "7", &format!("g{i}"), "继续聊"))
            .await;
    }
    let rows = store.pending_for_group(BOT, "500").await.expect("读");
    assert_eq!(rows.len(), 25);
    let arrived = rows[0].received_at;
    for i in 0..60 {
        store
            .start_call(BOT, "500", &format!("seed{i}"), arrived)
            .await
            .expect("记调用");
    }
    let jobs = intake.flush_tick(arrived + QUIET_SECONDS, None).await;
    assert!(jobs.is_empty());
    let kept = store.pending_for_group(BOT, "500").await.expect("读");
    assert_eq!(kept.len(), 22, "活尾巴应从 @ 那条起算");
    assert_eq!(kept[0].id, rows[3].id);
}

#[tokio::test]
async fn ambient_pool_defers_at_the_ambient_cap_but_addressed_still_claims() {
    let (intake, store, _dir) = intake_with_budget(60).await;
    intake.handle(&group_event("500", "7", "a0", "锚点")).await;
    let rows = store.pending_for_group(BOT, "500").await.expect("读");
    let arrived = rows[0].received_at;
    // 45 ambient calls: the ambient cap (60 - 15 reserve) is spent,
    // but the total still has headroom.
    for i in 0..45 {
        store
            .start_call(BOT, "500", &format!("seed{i}"), arrived)
            .await
            .expect("记调用");
    }
    for i in 0..3 {
        store
            .insert_pending(
                BOT,
                "500",
                &format!("f{i}"),
                "7",
                arrived + 1 + i,
                "{}",
                "[]",
            )
            .await
            .expect("插");
    }
    // A plain pool defers on the ambient cap alone, audited as such.
    let jobs = intake.flush_tick(arrived + 35, None).await;
    assert!(jobs.is_empty(), "普通池子撞普通上限要让路");
    let events = store.recent_audit(50).await.expect("读审计");
    assert!(
        events
            .iter()
            .any(|e| e.action.starts_with("scheduler.budget.defer.g500.age")
                && e.action.ends_with(".ambient")),
        "审计动作: {events:?}"
    );
    // A direct mention draws on the reserve and claims everything
    // pending, fillers included.
    let mut at_event = group_event("500", "7", "at1", "问你个事");
    at_event["message"] = serde_json::json!([
        {"type": "at", "data": {"qq": BOT}},
        {"type": "text", "data": {"text": "问你个事"}}
    ]);
    intake.handle(&at_event).await;
    let rows = store.pending_for_group(BOT, "500").await.expect("读");
    let at_arrived = rows.last().map(|r| r.received_at).unwrap_or(arrived);
    let jobs = intake.flush_tick(at_arrived + QUIET_SECONDS, None).await;
    assert_eq!(jobs.len(), 1, "点名池子要能动用预留额度");
    assert!(jobs[0].privileged);
    assert_eq!(jobs[0].event_ids.len(), 5);
}

#[tokio::test]
async fn ambient_pool_under_the_cap_still_claims() {
    let (intake, store, _dir) = intake_with_budget(60).await;
    intake.handle(&group_event("500", "7", "a0", "锚点")).await;
    let rows = store.pending_for_group(BOT, "500").await.expect("读");
    let arrived = rows[0].received_at;
    // 44 ambient calls: one ambient slot remains.
    for i in 0..44 {
        store
            .start_call(BOT, "500", &format!("seed{i}"), arrived)
            .await
            .expect("记调用");
    }
    for i in 0..2 {
        store
            .insert_pending(
                BOT,
                "500",
                &format!("f{i}"),
                "7",
                arrived + 1 + i,
                "{}",
                "[]",
            )
            .await
            .expect("插");
    }
    let jobs = intake.flush_tick(arrived + 35, None).await;
    assert_eq!(jobs.len(), 1);
    assert!(!jobs[0].privileged);
    assert_eq!(jobs[0].event_ids.len(), 3);
}

#[tokio::test]
async fn calls_older_than_the_window_do_not_count_against_the_budget() {
    let (intake, store, _dir) = intake_with_budget(1).await;
    intake.handle(&group_event("500", "7", "1", "hi")).await;
    let rows = store.pending_for_group(BOT, "500").await.expect("读");
    let arrived = rows[0].received_at;
    // One call against a budget of one — but it started over an hour
    // before this flush, so it has aged out of the window.
    store
        .start_call(BOT, "500", "b0", arrived - BUDGET_WINDOW_SECONDS - 1)
        .await
        .expect("记调用");
    let jobs = intake.flush_tick(arrived + QUIET_SECONDS, None).await;
    assert_eq!(jobs.len(), 1);
}

/// The tick runs twice a second, so an unchanging verdict must not be re-audited.
#[tokio::test]
async fn cadence_is_only_audited_when_it_changes() {
    let (intake, store, _dir) = intake().await;
    intake
        .record_cadence("500", "wait.unfinished_thought", 1)
        .await;
    intake
        .record_cadence("500", "wait.unfinished_thought", 2)
        .await;
    intake
        .record_cadence("500", "wait.unfinished_thought", 3)
        .await;
    let events = store.recent_audit(50).await.expect("读");
    let cadence: Vec<&str> = events
        .iter()
        .map(|e| e.action.as_str())
        .filter(|a| a.starts_with("intake.cadence."))
        .collect();
    assert_eq!(
        cadence,
        vec!["intake.cadence.wait.unfinished_thought"],
        "只有状态变化才写审计，否则 tick 会把表刷成心跳"
    );
    // A different verdict is a new episode and does get recorded.
    intake.record_cadence("500", "hand_over.settled", 4).await;
    let events = store.recent_audit(50).await.expect("读");
    assert_eq!(
        events
            .iter()
            .filter(|e| e.action.starts_with("intake.cadence."))
            .count(),
        2
    );
}

/// A gate that is switched off always answers `Unavailable`, which is exactly the
/// failure path these tests need: no network, no key, deterministic.
fn disabled_gate(dir: &tempfile::TempDir) -> crate::app::preflight::Preflight {
    let client = crate::integrations::aux_gateway::AuxGatewayClient::new(
        crate::integrations::aux_gateway::AuxGatewayConfig::new(
            None,
            None,
            dir.path().join("absent-key"),
            false,
        ),
    );
    crate::app::preflight::Preflight::new(std::sync::Arc::new(client))
}

/// The whole point of the floor: a person typing one thought as two messages must not
/// get cut. Even a score of 0 (a picture, a finished sentence) cannot release the
/// burst before the second message has had a chance to arrive.
#[tokio::test]
async fn the_floor_keeps_back_to_back_messages_in_one_batch() {
    let (intake, store, dir) = intake().await;
    let gate = disabled_gate(&dir);
    intake
        .handle(&group_event("500", "7", "1", "我和你说个事"))
        .await;
    let arrived = store.pending_for_group(BOT, "500").await.expect("读")[0].received_at;
    // Nothing may leave inside the floor, even though this gate would answer at once.
    assert!(intake.flush_tick(arrived, Some(&gate)).await.is_empty());
    // The second message lands inside the floor and joins the same burst.
    intake
        .handle(&group_event("500", "7", "2", "是件大事"))
        .await;
    assert!(intake.flush_tick(arrived + 1, Some(&gate)).await.is_empty());
    let jobs = intake
        .flush_tick(
            arrived + crate::app::preflight::MIN_BURST_SECONDS,
            Some(&gate),
        )
        .await;
    assert_eq!(jobs.len(), 1);
    assert_eq!(jobs[0].event_ids.len(), 2, "两句话必须落在同一批");
}

// ---- C2: quota pacing -------------------------------------------------------

fn t1_input() -> crate::app::pacing::PacingInput {
    crate::app::pacing::PacingInput {
        weekly_pct: Some(40.0),
        five_hour_pct: Some(80.0),
        snapshot_age_s: Some(60),
        // Pace: 60% used at ~45% of the week elapsed -> overage ~15 -> T1.
        weekly_reset_hours: Some(92.0),
        ..Default::default()
    }
}

/// At T1 the fuse alone would allow 60 an hour; pacing hands over at most 30, so the
/// capped tick must claim strictly less than the fuse would have.
#[tokio::test]
async fn t1_tightens_the_claim_count_below_the_fuse() {
    // Fuse wide open, budget 60: only pacing can be what limits the claim.
    let (intake, store, _dir) = intake_with_pacing(t1_input(), 60).await;
    for index in 0..40 {
        intake
            .handle(&group_event("500", "7", &format!("m{index}"), "一句话"))
            .await;
    }
    // 2026-10-29: the 40 handle calls can straddle a real second boundary, so
    // the quiet window is measured from the *newest* row. Measuring from the
    // first row made this test fail whenever ingestion crossed a second
    // (timing flake, six sightings during the 2026-10-29 split merges).
    let rows = store.pending_for_group(BOT, "500").await.expect("读");
    let arrived = rows.iter().map(|r| r.received_at).max().expect("40 行");
    let now = arrived + QUIET_SECONDS;
    let jobs = intake.flush_tick(now, None).await;

    let claimed: usize = jobs.iter().map(|j| j.event_ids.len()).sum();
    let limit = intake.limits().batch.max_messages;
    // The fuse would have allowed everything (40 < 60); T1 caps at 30 calls, i.e.
    // exactly one batch of the default size, and the rest is trimmed and audited.
    assert_eq!(limit, 50);
    assert_eq!(jobs.len(), 1, "T1 只放行一个批");
    assert_eq!(claimed, 40, "一个批里仍有 40 条");
    // The cap is visible in the engine's own report, which is what the panel reads.
    let report = intake
        .pacing()
        .expect("已接入")
        .observe_known_groups(now)
        .await;
    assert_eq!(report.tier, crate::app::pacing::Tier::T1);
    assert!(report.cap_for("500").expect("有档") < 60);
}

/// T2 answers only a mention or an owner message; everything else is consumed
/// without a reply and recorded as a pacing drop, not as a failure.
#[tokio::test]
async fn t2_drops_batches_below_the_privilege_floor() {
    let input = crate::app::pacing::PacingInput {
        weekly_pct: Some(20.0),
        five_hour_pct: Some(80.0),
        snapshot_age_s: Some(60),
        // Pace: 80% used at ~52% elapsed -> overage ~28 -> T2.
        weekly_reset_hours: Some(80.0),
        ..Default::default()
    };
    let (intake, store, _dir) = intake_with_pacing(input, 60).await;
    // A plain remark: below the floor at T2.
    intake
        .handle(&group_event("500", "7", "1", "随便说一句"))
        .await;
    let arrived = store.pending_for_group(BOT, "500").await.expect("读")[0].received_at;
    let now = arrived + QUIET_SECONDS;
    assert!(intake.flush_tick(now, None).await.is_empty());
    // Dropped, not deferred: it never comes back.
    assert_eq!(store.pending_count(BOT, "500").await.expect("读"), 0);
    let events = store.recent_audit(50).await.expect("读审计");
    assert!(
        events
            .iter()
            .any(|e| e.action == "pacing.drop.g500.n1.tierT2"),
        "审计动作: {events:?}"
    );

    // A mention clears the same floor and is answered normally.
    intake
        .handle(&group_event_mentioning_bot("500", "7", "2", " 看这个"))
        .await;
    let rows = store.pending_for_group(BOT, "500").await.expect("读");
    let at = rows[0].received_at + QUIET_SECONDS;
    let jobs = intake.flush_tick(at, None).await;
    assert_eq!(jobs.len(), 1, "@ 发言必须照常应答");
    assert!(jobs[0].privileged);
}

/// T3 keeps only the owner's own words, and the cap is three an hour.
#[tokio::test]
async fn t3_keeps_only_the_owner_and_never_exceeds_three() {
    let input = crate::app::pacing::PacingInput {
        weekly_pct: Some(5.0),
        five_hour_pct: Some(5.0),
        snapshot_age_s: Some(60),
        weekly_reset_hours: Some(30.0),
        ..Default::default()
    };
    let (intake, store, _dir) = intake_with_pacing(input, 60).await;
    // A member's @ still does not clear T3: only the owner does.
    intake
        .handle(&group_event_mentioning_bot("500", "7", "1", " 在吗"))
        .await;
    // The owner's own message does.
    intake.handle(&group_event("500", OWNER, "2", "在吗")).await;
    let rows = store.pending_for_group(BOT, "500").await.expect("读");
    let arrived = rows[0].received_at;
    let now = arrived + QUIET_SECONDS;
    // Three calls already started inside the rolling hour: the T3 cap of three is spent.
    for seed in 0..3 {
        store
            .start_call(BOT, "500", &format!("seed-{seed}"), arrived)
            .await
            .expect("记");
    }
    assert!(intake.flush_tick(now, None).await.is_empty(), "T3 三次已满");
    let events = store.recent_audit(50).await.expect("读审计");
    assert!(
        events
            .iter()
            .any(|e| e.action == "pacing.drop.g500.n1.tierT3"),
        "审计动作: {events:?}"
    );
}

/// The tier is audited on a change only: two observations at the same tier leave one row,
/// so the log reads as a history of tier transitions rather than a heartbeat.
#[tokio::test]
async fn the_tier_is_audited_once_per_change() {
    let input = crate::app::pacing::PacingInput {
        weekly_pct: Some(20.0),
        five_hour_pct: Some(90.0),
        snapshot_age_s: Some(30),
        // Pace: 80% used at ~52% elapsed -> overage ~28 -> T2.
        weekly_reset_hours: Some(80.0),
        ..Default::default()
    };
    let (intake, store, _dir) = intake_with_pacing(input, 60).await;
    let engine = intake.pacing().expect("已接入");
    engine.observe_known_groups(1_700_000_000).await;
    assert!(
        store
            .recent_audit(50)
            .await
            .expect("读审计")
            .iter()
            .any(|e| e.action.starts_with("pacing.tier.fromT0.toT2")),
        "首次观察必须记档位变化"
    );
    let before = store.recent_audit(200).await.expect("读审计").len();
    engine.observe_known_groups(1_700_000_001).await;
    let after = store.recent_audit(200).await.expect("读审计").len();
    assert_eq!(after, before, "同一档位的第二次观察不得再写一条");
}

/// A stale snapshot escalates rather than relaxing: the last fresh tier is the anchor.
#[tokio::test]
async fn a_stale_snapshot_never_relaxes_the_tier() {
    let input = crate::app::pacing::PacingInput {
        weekly_pct: Some(40.0),
        five_hour_pct: Some(80.0),
        snapshot_age_s: Some(5_000),
        weekly_reset_hours: Some(48.0),
        ..Default::default()
    };
    let (intake, store, dir) = intake_with_pacing(input, 60).await;
    let report = intake
        .pacing()
        .expect("已接入")
        .observe_known_groups(1_700_000_000)
        .await;
    assert!(report.stale);
    assert!(
        report.tier > crate::app::pacing::Tier::T0,
        "失效快照绝不能回 T0: {:?}",
        report.tier
    );
    // And the same engine keeps the anchor across a second observation.
    let again = intake
        .pacing()
        .expect("已接入")
        .observe_known_groups(1_700_001_000)
        .await;
    assert_eq!(again.tier, report.tier);
    drop(dir);
    drop(store);
}

/// Someone who is not the owner and not the bot, used as the partner in the
/// damping tests below.
const PARTNER: &str = "20001";

/// Give `sender` the message volume a real flood carries (see
/// `CAP_MIN_SENDER_MESSAGES`), straight into the damper's window: the soft cap now
/// needs volume as well as rounds, and these tests are about the hold that follows,
/// not about how the rows got there. The ids sit far above anything the store hands
/// out in one test, so they cannot collide with a real pending row.
fn seed_volume(intake: &Intake, group: &str, sender: &str, base: i64) {
    let batch: Vec<crate::app::damper::Observation<'_>> = (0
        ..crate::app::damper::CAP_MIN_SENDER_MESSAGES)
        .map(|index| crate::app::damper::Observation {
            id: 10_000 + index as i64,
            sender,
            at: base - 250 + index as i64,
            addressed: false,
            from_owner: false,
        })
        .collect();
    intake.damper.observe(group, base - 240, &batch);
}

/// An `L1` quiet hold delays the claim and loses nothing: the same row is still
/// pending afterwards, and the next tick past the window claims it as one batch.
#[tokio::test]
async fn a_damped_batch_stays_pending_until_the_window_opens() {
    let (intake, store, _dir) = intake().await;
    let base = crate::now_unix();
    // Six consecutive non-addressed rounds to the same person is what opens L1.
    for round in 0..crate::app::damper::L1_STREAK {
        intake
            .damper
            .note_reply("500", base - 120 + round as i64, PARTNER, false);
    }
    assert_eq!(intake.damper.tier("500"), crate::app::damper::Tier::L1);

    intake
        .handle(&group_event("500", PARTNER, "1", "所以呢"))
        .await;
    let arrived = store.pending_for_group(BOT, "500").await.expect("读")[0].received_at;

    // Inside the L1 quiet window: held, and the audit says which rule held it.
    assert!(
        intake
            .flush_tick(arrived + QUIET_SECONDS, None)
            .await
            .is_empty()
    );
    assert_eq!(store.pending_count(BOT, "500").await.expect("读"), 1);
    let events = store.recent_audit(50).await.expect("读审计");
    assert!(
        events
            .iter()
            .any(|e| e.action == "intake.cadence.wait.damper_l1_quiet"),
        "审计动作: {events:?}"
    );

    // Past the window the same message is claimed — nothing was dropped.
    let jobs = intake
        .flush_tick(arrived + crate::app::damper::L1_QUIET_SECONDS, None)
        .await;
    assert_eq!(jobs.len(), 1, "jobs: {jobs:?}");
    assert_eq!(jobs[0].event_ids.len(), 1);
    assert_eq!(store.pending_count(BOT, "500").await.expect("读"), 0);
}

/// At `L3` the partner only gets through when the batch is addressed: an `@` is a
/// request, a bare continuation is not.
#[tokio::test]
async fn l3_lets_only_addressed_batches_through() {
    let (intake, store, _dir) = intake().await;
    let base = crate::now_unix();
    for round in 0..crate::app::damper::L3_STREAK {
        intake
            .damper
            .note_reply("500", base - 200 + round as i64, PARTNER, false);
    }
    assert_eq!(intake.damper.tier("500"), crate::app::damper::Tier::L3);

    intake
        .handle(&group_event("500", PARTNER, "1", "继续"))
        .await;
    let arrived = store.pending_for_group(BOT, "500").await.expect("读")[0].received_at;
    // Even well past every quiet window: L3 is a hard hold, not a delay.
    assert!(
        intake
            .flush_tick(arrived + crate::app::damper::L2_QUIET_SECONDS + 60, None)
            .await
            .is_empty()
    );
    assert_eq!(store.pending_count(BOT, "500").await.expect("读"), 1);
    let events = store.recent_audit(50).await.expect("读审计");
    assert!(
        events
            .iter()
            .any(|e| e.action == "intake.cadence.wait.damper_l3"),
        "审计动作: {events:?}"
    );

    // An @ from the same person is a direct request and passes at once.
    intake
        .handle(&group_event_mentioning_bot("500", PARTNER, "2", "那你说呢"))
        .await;
    let jobs = intake
        .flush_tick(arrived + crate::app::damper::L2_QUIET_SECONDS + 61, None)
        .await;
    assert_eq!(jobs.len(), 1, "jobs: {jobs:?}");
    // Both messages ride along: the hold never discarded the first one.
    assert_eq!(jobs[0].event_ids.len(), 2);
}

/// The per-sender soft cap holds one person's non-addressed rounds and leaves
/// everybody else — and that person's `@`s — untouched.
#[tokio::test]
async fn the_soft_cap_holds_only_the_sender_it_names() {
    let (capped_intake, capped_store, _dir) = intake().await;
    let base = crate::now_unix();
    seed_volume(&capped_intake, "500", PARTNER, base);
    for round in 0..crate::app::damper::SOFT_CAP_ROUNDS {
        capped_intake
            .damper
            .note_reply("500", base - 300 + round as i64, PARTNER, false);
    }

    capped_intake
        .handle(&group_event("500", PARTNER, "1", "还在吗"))
        .await;
    let arrived = capped_store
        .pending_for_group(BOT, "500")
        .await
        .expect("读")[0]
        .received_at;
    assert!(
        capped_intake
            .flush_tick(arrived + crate::app::damper::L2_QUIET_SECONDS + 60, None)
            .await
            .is_empty()
    );
    let events = capped_store.recent_audit(50).await.expect("读审计");
    assert!(
        events
            .iter()
            .any(|e| e.action == "intake.cadence.wait.damper_cap"),
        "审计动作: {events:?}"
    );

    // Somebody else in the same room is not covered by that cap.
    let (other_intake, other_store, _other_dir) = intake().await;
    seed_volume(&other_intake, "500", PARTNER, base);
    for round in 0..crate::app::damper::SOFT_CAP_ROUNDS {
        other_intake
            .damper
            .note_reply("500", base - 300 + round as i64, PARTNER, false);
    }
    other_intake
        .handle(&group_event("500", "20002", "1", "我插一句"))
        .await;
    let other_arrived = other_store.pending_for_group(BOT, "500").await.expect("读")[0].received_at;
    let jobs = other_intake
        .flush_tick(
            other_arrived + crate::app::damper::L2_QUIET_SECONDS + 61,
            None,
        )
        .await;
    assert_eq!(jobs.len(), 1, "jobs: {jobs:?}");
}

/// A room with several people taking turns never trips anything: the window is
/// spread out, the streak never forms and no sender reaches the cap.
#[tokio::test]
async fn an_ordinary_multi_person_chat_is_never_damped() {
    let (intake, store, _dir) = intake().await;
    let crowd = ["20001", "20002", "20003"];
    for (index, sender) in crowd.iter().enumerate() {
        for turn in 0..2 {
            intake
                .handle(&group_event(
                    "500",
                    sender,
                    &format!("{index}{turn}"),
                    "随便聊聊",
                ))
                .await;
        }
    }
    let arrived = store.pending_for_group(BOT, "500").await.expect("读")[0].received_at;
    let jobs = intake.flush_tick(arrived + QUIET_SECONDS, None).await;
    assert_eq!(jobs.len(), 1, "jobs: {jobs:?}");
    assert_eq!(jobs[0].event_ids.len(), 6);
    assert_eq!(intake.damper.tier("500"), crate::app::damper::Tier::L0);
}

#[tokio::test]
async fn retry_mark_claims_an_exclusive_retry_job() {
    let (intake, store, _dir) = intake().await;
    store.ensure_group(BOT, "500", 0).await.expect("建群");
    let resident = Arc::new(crate::app::resident::ResidentManager::new(
        Arc::clone(&store),
        BOT.to_string(),
        std::path::PathBuf::new(),
        std::path::PathBuf::new(),
        String::new(),
    ));
    let intake = intake.with_resident(Arc::clone(&resident));
    intake.handle(&group_event("500", "7", "1", "hi")).await;
    intake.handle(&group_event("500", "8", "2", "later")).await;
    let rows = store.pending_for_group(BOT, "500").await.expect("读");
    assert_eq!(rows.len(), 2);
    let dead_id = rows[0].id;
    // Only the first row is the dead batch; the second arrived afterwards
    // and must not be folded into the replay.
    resident.arm_retry("500", vec![dead_id]).await;
    let arrived = rows[0].received_at;
    let jobs = intake.flush_tick(arrived + 1, None).await;
    let retry_jobs: Vec<_> = jobs.iter().filter(|j| j.retry).collect();
    assert_eq!(retry_jobs.len(), 1, "exactly one replay job: {jobs:?}");
    assert_eq!(retry_jobs[0].event_ids, vec![dead_id]);
    assert!(
        resident.retry_mark("500").await.is_none(),
        "the mark is consumed by the claim"
    );
    // The other row was not claimed by the replay.
    let remaining = store.pending_for_group(BOT, "500").await.expect("读");
    assert_eq!(remaining.len(), 1);
    assert_ne!(remaining[0].id, dead_id);
}

#[tokio::test]
async fn retry_mark_for_gone_rows_is_cleared_without_a_job() {
    let (intake, store, _dir) = intake().await;
    store.ensure_group(BOT, "500", 0).await.expect("建群");
    let resident = Arc::new(crate::app::resident::ResidentManager::new(
        Arc::clone(&store),
        BOT.to_string(),
        std::path::PathBuf::new(),
        std::path::PathBuf::new(),
        String::new(),
    ));
    let intake = intake.with_resident(Arc::clone(&resident));
    intake.handle(&group_event("500", "7", "1", "hi")).await;
    let rows = store.pending_for_group(BOT, "500").await.expect("读");
    let arrived = rows[0].received_at;
    // The mark names a row that does not exist: it must clear quietly and
    // the live row must flow through the normal judgement untouched.
    resident.arm_retry("500", vec![999_999]).await;
    let jobs = intake.flush_tick(arrived + QUIET_SECONDS, None).await;
    assert!(jobs.iter().all(|j| !j.retry), "no replay job: {jobs:?}");
    assert_eq!(jobs.len(), 1, "the live row still flushes normally");
    assert!(
        resident.retry_mark("500").await.is_none(),
        "a mark for gone rows is cleared, not left to fire later"
    );
}

#[tokio::test]
async fn addressed_only_pace_holds_ambient_but_addressed_claims() {
    let (intake, store, _dir) = intake_with_budget(60).await;
    store
        .set_group_ambient_pace(BOT, "500", "addressed_only", 1)
        .await
        .expect("设档位");
    intake.handle(&group_event("500", "7", "a0", "锚点")).await;
    let rows = store.pending_for_group(BOT, "500").await.expect("读");
    let arrived = rows[0].received_at;
    // Ambient chatter alone never becomes a turn at this tier.
    let jobs = intake.flush_tick(arrived + 35, None).await;
    assert!(jobs.is_empty(), "只应答档位下 ambient 不该成轮");
    let events = store.recent_audit(50).await.expect("读审计");
    assert!(
        events.iter().any(|e| e
            .action
            .contains("intake.relevance.g500.deferred.addressed_only")),
        "审计动作: {events:?}"
    );
    // An addressed message claims the whole window, ambient included.
    let mut at_event = group_event("500", "7", "at1", "问你个事");
    at_event["message"] = serde_json::json!([
        {"type": "at", "data": {"qq": BOT}},
        {"type": "text", "data": {"text": "问你个事"}}
    ]);
    intake.handle(&at_event).await;
    let jobs = intake.flush_tick(arrived + 70, None).await;
    assert!(!jobs.is_empty(), "被 @ 后整窗都应被认领");
}

#[tokio::test]
async fn addressed_only_owner_and_name_call_do_not_trigger() {
    let (intake, store, _dir) = intake_with_budget(60).await;
    store
        .set_group_ambient_pace(BOT, "500", "addressed_only", 1)
        .await
        .expect("设档位");
    let intake = intake.with_call_names(vec!["Gemini".to_string()]);
    // The owner's own message and a text name-call both counted as
    // addressed before 2026-10-30; at this tier neither may open a
    // turn on its own — only an @ or a reply to her may.
    intake
        .handle(&group_event("500", OWNER, "o1", "我说一句"))
        .await;
    intake
        .handle(&group_event("500", "7", "n1", "Gemini 在吗"))
        .await;
    let rows = store.pending_for_group(BOT, "500").await.expect("读");
    let arrived = rows[0].received_at;
    let jobs = intake.flush_tick(arrived + 70, None).await;
    assert!(
        jobs.is_empty(),
        "只应答档位下主人发言和喊名字不该成轮: {jobs:?}"
    );
}

#[tokio::test]
async fn addressed_only_claim_keeps_only_fixed_range_context() {
    let (intake, store, _dir) = intake_with_budget(60).await;
    store
        .set_group_ambient_pace(BOT, "500", "addressed_only", 1)
        .await
        .expect("设档位");
    for i in 0..13 {
        intake
            .handle(&group_event("500", "7", &format!("a{i}"), "闲聊"))
            .await;
    }
    intake
        .handle(&group_event_mentioning_bot("500", "7", "at1", "问你个事"))
        .await;
    let rows = store.pending_for_group(BOT, "500").await.expect("读");
    assert_eq!(rows.len(), 14);
    let arrived = rows[0].received_at;
    let jobs = intake.flush_tick(arrived + 70, None).await;
    assert_eq!(jobs.len(), 1, "被 @ 应认领一轮: {jobs:?}");
    // Fixed range: the trigger plus the ten messages above it; the
    // three oldest ambient rows are consumed, not carried.
    let expected: Vec<i64> = rows[3..].iter().map(|r| r.id).collect();
    assert_eq!(jobs[0].event_ids, expected);
    let remaining = store.pending_for_group(BOT, "500").await.expect("读");
    assert!(remaining.is_empty(), "裁掉的行应被消费: {remaining:?}");
    let events = store.recent_audit(50).await.expect("读审计");
    assert!(
        events.iter().any(|e| e
            .action
            .contains("intake.addressed_only.context_trim.g500.n3")),
        "审计动作: {events:?}"
    );
}

#[tokio::test]
async fn group_increase_notice_discovers_the_group() {
    let dir = tempfile::tempdir().expect("tmp");
    let pool = migrations::open_and_migrate(&dir.path().join("bot.sqlite3"))
        .await
        .expect("迁移");
    let store = Arc::new(SqliteStore::new(pool));
    let intake = Intake::new(
        Arc::clone(&store),
        BOT.to_string(),
        OWNER.to_string(),
        limits(),
    );
    let frame = serde_json::json!({
        "post_type": "notice",
        "notice_type": "group_increase",
        "sub_type": "invite",
        "group_id": "777",
        "user_id": BOT,
        "self_id": BOT,
        "time": 1_700_000_000,
    });
    let outcome = intake.handle(&frame).await;
    assert!(matches!(outcome, IntakeOutcome::Skipped(_)));
    let group = store
        .get_group(BOT, "777")
        .await
        .expect("读组")
        .expect("已发现");
    assert!(!group.enabled, "发现的群默认不启用");
    assert!(group.departed_at.is_none());
    let discovered: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM audit_events WHERE action = 'group.discovered'")
            .fetch_one(store.pool())
            .await
            .expect("审计计数");
    assert_eq!(discovered, 1);

    // A second increase writes no second discovery row.
    let _ = intake.handle(&frame).await;
    let discovered: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM audit_events WHERE action = 'group.discovered'")
            .fetch_one(store.pool())
            .await
            .expect("审计计数");
    assert_eq!(discovered, 1);
}

#[tokio::test]
async fn group_decrease_of_the_bot_marks_departed_and_rejoin_clears_it() {
    let dir = tempfile::tempdir().expect("tmp");
    let pool = migrations::open_and_migrate(&dir.path().join("bot.sqlite3"))
        .await
        .expect("迁移");
    let store = Arc::new(SqliteStore::new(pool));
    store.ensure_group(BOT, "888", 1).await.expect("建组");
    let intake = Intake::new(
        Arc::clone(&store),
        BOT.to_string(),
        OWNER.to_string(),
        limits(),
    );
    let decrease = serde_json::json!({
        "post_type": "notice",
        "notice_type": "group_decrease",
        "sub_type": "kick_me",
        "group_id": "888",
        "user_id": BOT,
        "self_id": BOT,
        "time": 1_700_000_000,
    });
    let _ = intake.handle(&decrease).await;
    let group = store
        .get_group(BOT, "888")
        .await
        .expect("读组")
        .expect("有组");
    assert!(group.departed_at.is_some(), "机器人被移出应标记 departed");

    // Another member leaving does not touch the group.
    let other = serde_json::json!({
        "post_type": "notice",
        "notice_type": "group_decrease",
        "sub_type": "leave",
        "group_id": "888",
        "user_id": "4242",
        "self_id": BOT,
        "time": 1_700_000_001,
    });
    store
        .set_group_departed(BOT, "888", None, 2)
        .await
        .expect("清标记");
    let _ = intake.handle(&other).await;
    let group = store
        .get_group(BOT, "888")
        .await
        .expect("读组")
        .expect("有组");
    assert!(group.departed_at.is_none());

    // Rejoining (group_increase) clears the marker again.
    store
        .set_group_departed(BOT, "888", Some(3), 3)
        .await
        .expect("置标记");
    let increase = serde_json::json!({
        "post_type": "notice",
        "notice_type": "group_increase",
        "sub_type": "invite",
        "group_id": "888",
        "user_id": BOT,
        "self_id": BOT,
        "time": 1_700_000_002,
    });
    let _ = intake.handle(&increase).await;
    let group = store
        .get_group(BOT, "888")
        .await
        .expect("读组")
        .expect("有组");
    assert!(group.departed_at.is_none(), "重新入群应清 departed 标记");
}
