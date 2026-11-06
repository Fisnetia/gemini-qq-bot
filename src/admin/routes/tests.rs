//! Tests moved out verbatim (std-split); assertions unchanged.

use super::groups::StateUpdate;
use super::memories::NewMemory;
use super::ops::panel_lists_call;
use super::stickers::sniff_image_type;
use super::*;

#[test]
fn a_silent_success_stays_on_the_panel_list() {
    assert!(!panel_lists_call("succeeded", None));
    assert!(panel_lists_call("succeeded", Some("silent")));
    assert!(panel_lists_call("failed", Some("agent_failed")));
}

#[test]
fn image_type_comes_from_bytes_not_from_the_file_name() {
    assert_eq!(
        sniff_image_type(&[0x89, b'P', b'N', b'G', 13, 10, 26, 10, 0]),
        Some("image/png")
    );
    assert_eq!(
        sniff_image_type(&[0xff, 0xd8, 0xff, 0xe0]),
        Some("image/jpeg")
    );
    assert_eq!(sniff_image_type(b"RIFF____WEBPVP8 "), Some("image/webp"));
    assert_eq!(sniff_image_type(b"GIF89a"), Some("image/gif"));
    // 未知格式一律拒绝，不猜类型。
    assert_eq!(sniff_image_type(b"<html>"), None);
    assert_eq!(sniff_image_type(b""), None);
}

#[test]
fn group_ids_are_validated() {
    assert!(checked_group_id("123456").is_ok());
    for bad in ["abc", "", "../etc", "12345;--", "1e5"] {
        assert!(checked_group_id(bad).is_err(), "{bad} 必须被拒绝");
    }
}

#[test]
fn route_table_has_no_catch_all_or_proxy() {
    for path in route_paths() {
        assert!(
            path.starts_with("/internal/v1/"),
            "{path} 必须固定在 /internal/v1/ 命名空间"
        );
        assert!(!path.contains('*'), "{path} 不得包含通配");
        assert!(!path.contains("proxy"), "{path} 不得是代理");
    }
}

#[test]
fn state_update_accepts_a_bare_string_or_an_object() {
    let bare = StateUpdate::from_value(json!("闲聊")).expect("裸字符串应接受");
    assert_eq!(bare.topic_state.as_deref(), Some("闲聊"));

    let object = StateUpdate::from_value(json!({
        "topic_state": "讨论咖啡",
        "mood_label": "calm",
        "mood_reason": "气氛温和"
    }))
    .expect("对象应接受");
    assert_eq!(object.mood_label.as_deref(), Some("calm"));

    assert!(StateUpdate::from_value(json!(1)).is_err());
}

#[test]
fn persona_body_schema_is_strict() {
    assert!(serde_json::from_value::<PersonaUpdate>(json!({"persona": "x"})).is_ok());
    assert!(serde_json::from_value::<PersonaUpdate>(json!({"text": "x"})).is_err());
}

#[test]
fn new_memory_schema_requires_content_and_kind() {
    assert!(serde_json::from_value::<NewMemory>(json!({"content": "x", "kind": "fact"})).is_ok());
    assert!(serde_json::from_value::<NewMemory>(json!({"content": "x"})).is_err());
    assert!(serde_json::from_value::<NewMemory>(json!({"kind": "fact"})).is_err());
}

/// A state wired to `store`, with the quota client pointed at paths that do
/// not exist so nothing in these tests touches the network or the real home.
fn state_for(dir: &tempfile::TempDir, store: Arc<SqliteStore>) -> Arc<AdminState> {
    state_for_resident(dir, store, None)
}

fn state_for_resident(
    dir: &tempfile::TempDir,
    store: Arc<SqliteStore>,
    resident: Option<Arc<crate::app::resident::ResidentManager>>,
) -> Arc<AdminState> {
    let quota = Arc::new(QuotaService::new(crate::integrations::quota::QuotaConfig {
        agy_home: dir.path().join("absent-home"),
        agy_bin: dir.path().join("absent-agy"),
        proxy_url: None,
    }));
    let pacing = Arc::new(crate::app::pacing::PacingEngine::fixed(
        Arc::clone(&store),
        crate::app::pacing::PacingInput {
            weekly_pct: Some(40.0),
            five_hour_pct: Some(90.0),
            snapshot_age_s: Some(30),
            // Pace: 60% used at ~45% elapsed -> overage ~15 -> T1.
            weekly_reset_hours: Some(92.0),
            ..Default::default()
        },
        "1".to_string(),
        60,
    ));
    Arc::new(AdminState {
        store: Arc::clone(&store),
        health: Arc::new(HealthRegistry::new(crate::health::HealthSnapshot::default())),
        compactor: Arc::new(CompactionScheduler::new(
            Arc::clone(&store),
            crate::memory::compaction::CompactionThresholds::default(),
        )),
        summary_model: Arc::new(AuxGatewayClient::new(
            crate::integrations::aux_gateway::AuxGatewayConfig::new(
                None,
                None,
                dir.path().join("absent-key"),
                false,
            ),
        )),
        quota,
        settings: crate::app::runtime_settings::RuntimeSettings::for_tests(
            dir.path().to_path_buf(),
            Arc::clone(&pacing),
        ),
        pacing,
        scheduler: Arc::new(crate::pipeline::scheduler::Scheduler::new(
            2,
            crate::pipeline::batcher::BatchLimits::default(),
        )),
        bot_id: "1".to_string(),
        persona_default: String::new(),
        conversation_turn_limit: crate::app::turn::shared_turn_limit(None),
        resident,
    })
}

async fn get_json(state: Arc<AdminState>, uri: &str) -> serde_json::Value {
    use tower::ServiceExt;

    let response = router(state)
        .oneshot(
            axum::http::Request::builder()
                .uri(uri)
                .body(axum::body::Body::empty())
                .expect("请求"),
        )
        .await
        .expect("响应");
    assert_eq!(response.status(), 200, "{uri}");
    let bytes = http_body_util::BodyExt::collect(response.into_body())
        .await
        .expect("读主体")
        .to_bytes();
    serde_json::from_slice(&bytes).expect("解析")
}

async fn patch_json(
    state: Arc<AdminState>,
    uri: &str,
    body: serde_json::Value,
) -> (u16, serde_json::Value) {
    use tower::ServiceExt;

    let response = router(state)
        .oneshot(
            axum::http::Request::builder()
                .method("PATCH")
                .uri(uri)
                .header("content-type", "application/json")
                .body(axum::body::Body::from(body.to_string()))
                .expect("请求"),
        )
        .await
        .expect("响应");
    let status = response.status().as_u16();
    let bytes = http_body_util::BodyExt::collect(response.into_body())
        .await
        .expect("读主体")
        .to_bytes();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null),
    )
}

#[tokio::test]
async fn patch_group_sets_ambient_pace_and_rejects_unknown_values() {
    let dir = tempfile::tempdir().expect("tmp");
    let pool = crate::store::migrations::open_and_migrate(&dir.path().join("bot.sqlite3"))
        .await
        .expect("迁移");
    let store = Arc::new(SqliteStore::new(pool));
    store.ensure_group("1", "500", 1).await.expect("建组");
    let state = state_for(&dir, Arc::clone(&store));
    let (status, _) = patch_json(
        Arc::clone(&state),
        "/internal/v1/groups/500",
        json!({"ambient_pace": "quiet"}),
    )
    .await;
    assert_eq!(status, 200);
    let group = store
        .get_group("1", "500")
        .await
        .expect("读组")
        .expect("有组");
    assert_eq!(group.pace(), crate::store::AmbientPace::Quiet);
    let body = get_json(state, "/internal/v1/groups").await;
    assert_eq!(body["groups"][0]["ambient_pace"], json!("quiet"));
    let (status, _) = patch_json(
        state_for(&dir, store),
        "/internal/v1/groups/500",
        json!({"ambient_pace": "loud"}),
    )
    .await;
    assert_eq!(status, 400, "未知档位必须被拒");
}

/// The status payload must carry the pacing tier the flush tick is obeying — the panel
/// and the operator read this field, so its shape is part of the contract, not a debug
/// dump.
#[tokio::test]
async fn status_reports_the_live_pacing_tier() {
    let dir = tempfile::tempdir().expect("tmp");
    let pool = crate::store::migrations::open_and_migrate(&dir.path().join("bot.sqlite3"))
        .await
        .expect("迁移");
    let store = Arc::new(SqliteStore::new(pool));
    store.ensure_group("1", "500", 1).await.expect("建组");
    store
        .set_group_flags("1", "500", Some(true), Some(false), None, 1)
        .await
        .expect("开启");
    let body = get_json(state_for(&dir, store), "/internal/v1/status").await;
    let pacing = &body["pacing"];
    assert_eq!(pacing["tier"], json!("T1"), "档位: {body}");
    assert_eq!(pacing["weekly_pct"], json!(40.0));
    assert_eq!(pacing["five_hour_pct"], json!(90.0));
    assert_eq!(pacing["snapshot_age_s"], json!(30));
    assert_eq!(pacing["stale"], json!(false));
    let caps = pacing["effective_caps"].as_array().expect("每群上限");
    assert_eq!(caps.len(), 1);
    assert_eq!(caps[0]["group_id"], json!("500"));
    assert!(caps[0]["cap"].as_u64().expect("数值") <= 30);
    assert_eq!(body["backoffs"], json!([]));
}

#[tokio::test]
async fn groups_payload_surfaces_a_live_respawn_backoff() {
    let dir = tempfile::tempdir().expect("tmp");
    let pool = crate::store::migrations::open_and_migrate(&dir.path().join("bot.sqlite3"))
        .await
        .expect("迁移");
    let store = Arc::new(SqliteStore::new(pool));
    store.ensure_group("1", "500", 1).await.expect("建组");
    let resident = Arc::new(crate::app::resident::ResidentManager::new(
        Arc::clone(&store),
        "1".into(),
        dir.path().join("agy"),
        dir.path().join("home"),
        "model".into(),
    ));
    let now = crate::now_unix();
    let (tier, until) = resident.note_respawn_failure("500", now).await;
    assert_eq!(tier, 1);
    let body = get_json(
        state_for_resident(&dir, Arc::clone(&store), Some(Arc::clone(&resident))),
        "/internal/v1/groups",
    )
    .await;
    let group = &body["groups"][0];
    assert_eq!(group["group_id"], json!("500"));
    assert_eq!(group["backoff_tier"], json!(tier));
    assert_eq!(group["backoff_until"], json!(until));
    let status = get_json(
        state_for_resident(&dir, store, Some(resident)),
        "/internal/v1/status",
    )
    .await;
    let backs = status["backoffs"].as_array().expect("backoffs");
    assert_eq!(backs.len(), 1);
    assert_eq!(backs[0]["group_id"], json!("500"));
    assert_eq!(backs[0]["tier"], json!(tier));
    assert_eq!(backs[0]["until"], json!(until));
}

async fn put_json(
    state: Arc<AdminState>,
    uri: &str,
    body: serde_json::Value,
) -> (u16, serde_json::Value) {
    use tower::ServiceExt;

    let response = router(state)
        .oneshot(
            axum::http::Request::builder()
                .method("PUT")
                .uri(uri)
                .header("content-type", "application/json")
                .body(axum::body::Body::from(body.to_string()))
                .expect("请求"),
        )
        .await
        .expect("响应");
    let status = response.status().as_u16();
    let bytes = http_body_util::BodyExt::collect(response.into_body())
        .await
        .expect("读主体")
        .to_bytes();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null),
    )
}

#[tokio::test]
async fn api_v1_mount_serves_the_shared_handlers() {
    let dir = tempfile::tempdir().expect("tmp");
    let pool = crate::store::migrations::open_and_migrate(&dir.path().join("bot.sqlite3"))
        .await
        .expect("迁移");
    let store = Arc::new(SqliteStore::new(pool));
    let state = state_for(&dir, store);
    let body = get_json(Arc::clone(&state), "/api/v1/status").await;
    assert!(body.get("health").is_some() || body.get("pacing").is_some());
    let body = get_json(state, "/api/v1/groups").await;
    assert!(body["groups"].is_array());
}

#[tokio::test]
async fn settings_round_trip_updates_aux_and_switches() {
    let dir = tempfile::tempdir().expect("tmp");
    let pool = crate::store::migrations::open_and_migrate(&dir.path().join("bot.sqlite3"))
        .await
        .expect("迁移");
    let store = Arc::new(SqliteStore::new(pool));
    let state = state_for(&dir, store);

    // Starts in agy mode (fixture gateway disabled), both switches on.
    let body = get_json(Arc::clone(&state), "/api/v1/settings").await;
    assert_eq!(body["aux"]["mode"], json!("agy"));
    assert_eq!(body["aux"]["enabled"], json!(false));
    assert_eq!(body["runtime"]["session_rotation_enabled"], json!(true));
    assert_eq!(body["runtime"]["quota_degradation_enabled"], json!(true));

    // Aux update: values land, the key is stored but never echoed.
    let (status, body) = put_json(
        Arc::clone(&state),
        "/api/v1/settings/aux",
        json!({"base_url": "http://127.0.0.1:18088", "model": "m1", "api_key": "k-secret"}),
    )
    .await;
    assert_eq!(status, 200);
    assert_eq!(body["aux"]["base_url"], json!("http://127.0.0.1:18088"));
    assert_eq!(body["aux"]["model"], json!("m1"));
    assert_eq!(body["aux"]["api_key_set"], json!(true));
    assert!(
        !body.to_string().contains("k-secret"),
        "api key must never be echoed"
    );

    // A malformed base URL is rejected and changes nothing.
    let (status, _) = put_json(
        Arc::clone(&state),
        "/api/v1/settings/aux",
        json!({"base_url": "notaurl"}),
    )
    .await;
    assert_eq!(status, 400);

    // Switches flip live and persist to the overlay file.
    let (status, body) = put_json(
        Arc::clone(&state),
        "/api/v1/settings/runtime",
        json!({"quota_degradation_enabled": false}),
    )
    .await;
    assert_eq!(status, 200);
    assert_eq!(body["runtime"]["quota_degradation_enabled"], json!(false));
    assert_eq!(
        state.settings.switch_view().quota_degradation_enabled,
        false
    );
    let overlay_raw = std::fs::read_to_string(dir.path().join("runtime-settings.json"))
        .expect("overlay persisted");
    assert!(overlay_raw.contains("\"quota_degradation_enabled\": false"));

    // An empty switch update is a client error.
    let (status, _) = put_json(Arc::clone(&state), "/api/v1/settings/runtime", json!({})).await;
    assert_eq!(status, 400);

    // Settings routes do not exist on the legacy mount.
    let (status, _) = put_json(
        state,
        "/internal/v1/settings/runtime",
        json!({"quota_degradation_enabled": true}),
    )
    .await;
    assert_eq!(status, 404);
}

#[tokio::test]
async fn api_v1_persona_edit_round_trips() {
    let dir = tempfile::tempdir().expect("tmp");
    let pool = crate::store::migrations::open_and_migrate(&dir.path().join("bot.sqlite3"))
        .await
        .expect("迁移");
    let store = Arc::new(SqliteStore::new(pool));
    let state = state_for(&dir, store);
    let (status, _) = put_json(
        Arc::clone(&state),
        "/api/v1/persona",
        json!({"persona": "你是一只冷静的猫"}),
    )
    .await;
    assert_eq!(status, 200);
    let body = get_json(Arc::clone(&state), "/api/v1/persona").await;
    assert_eq!(body["persona"], json!("你是一只冷静的猫"));
    assert_eq!(body["is_default"], json!(false));

    // DELETE restores the compiled-in default card.
    use tower::ServiceExt;
    let response = router(state.clone())
        .oneshot(
            axum::http::Request::builder()
                .method("DELETE")
                .uri("/api/v1/persona")
                .body(axum::body::Body::empty())
                .expect("请求"),
        )
        .await
        .expect("响应");
    assert_eq!(response.status(), 200);
    let body = get_json(state, "/api/v1/persona").await;
    assert_eq!(body["is_default"], json!(true));
}
