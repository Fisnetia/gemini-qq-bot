//! Tests moved out verbatim (std-split); assertions unchanged.

#[test]
fn drift_kind_classifies_the_known_shapes() {
    assert_eq!(
        drift_kind("unknown step_type 77 at idx 5 carries readable text: \"x\""),
        "unknown_type"
    );
    assert_eq!(drift_kind("model step 12: no field 20"), "no_field20");
    assert_eq!(drift_kind("user step 3: no text at 19.2"), "no_text");
    assert_eq!(drift_kind("varint truncated"), "wire");
    assert_eq!(drift_kind("length overrun"), "wire");
    assert_eq!(drift_kind("wire type 6"), "wire");
    assert_eq!(drift_kind("something entirely new"), "other");
}

use super::*;

// ---- 极小的 protobuf 编码器，只在测试里用来造夹具 ----

fn varint_bytes(mut v: u64) -> Vec<u8> {
    let mut out = Vec::new();
    loop {
        let byte = (v & 0x7F) as u8;
        v >>= 7;
        if v == 0 {
            out.push(byte);
            return out;
        }
        out.push(byte | 0x80);
    }
}

fn tag_ld(field: u32) -> Vec<u8> {
    varint_bytes((u64::from(field) << 3) | 2)
}

fn tag_vint(field: u32) -> Vec<u8> {
    varint_bytes(u64::from(field) << 3)
}

/// 一个 length-delimited 字段。
fn ld(field: u32, body: &[u8]) -> Vec<u8> {
    let mut out = tag_ld(field);
    out.extend(varint_bytes(body.len() as u64));
    out.extend_from_slice(body);
    out
}

/// 一个 varint 字段。
fn vint(field: u32, v: u64) -> Vec<u8> {
    let mut out = tag_vint(field);
    out.extend(varint_bytes(v));
    out
}

fn cat(parts: &[Vec<u8>]) -> Vec<u8> {
    parts.iter().flat_map(|p| p.iter().copied()).collect()
}

fn ts_meta(sec: u64, nanos: u64) -> Vec<u8> {
    // payload.5 = 元数据，5.1 = (sec, nanos)
    let stamp = cat(&[vint(1, sec), vint(2, nanos)]);
    ld(5, &ld(1, &stamp))
}

fn user_payload(text: &str, sec: u64) -> Vec<u8> {
    cat(&[ts_meta(sec, 500_000_000), ld(19, &ld(2, text.as_bytes()))])
}

fn model_text_payload(text: &str, sec: u64) -> Vec<u8> {
    cat(&[ts_meta(sec, 0), ld(20, &ld(1, text.as_bytes()))])
}

fn model_thought_payload(thought: &str, sec: u64) -> Vec<u8> {
    cat(&[ts_meta(sec, 0), ld(20, &ld(3, thought.as_bytes()))])
}

fn model_tool_payload(name: &str, sec: u64) -> Vec<u8> {
    let tool = cat(&[ld(1, b"call-1"), ld(2, name.as_bytes())]);
    cat(&[ts_meta(sec, 0), ld(20, &ld(7, &tool))])
}

fn tool_result_payload(task: Option<&str>, sec: u64) -> Vec<u8> {
    let mut parts = vec![ts_meta(sec, 0)];
    if let Some(task) = task {
        parts.push(ld(148, &ld(1, task.as_bytes())));
    }
    cat(&parts)
}

fn notification_payload(conv: &str, task: u32, sec: u64) -> Vec<u8> {
    let body = format!("{conv}/task-{task}");
    cat(&[ts_meta(sec, 0), ld(114, body.as_bytes())])
}

// ---- wire walking ----

#[test]
fn varint_roundtrip_and_overflow() {
    for v in [0u64, 1, 127, 128, 300, 1 << 40, u64::MAX >> 1] {
        let bytes = varint_bytes(v);
        let mut i = 0;
        assert_eq!(read_varint(&bytes, &mut i).expect("varint"), v);
        assert_eq!(i, bytes.len());
    }
    // 11 个续位字节把 shift 推过 63，必须报漂移而不是死循环。
    let overlong = vec![0xFF; 12];
    let mut i = 0;
    let err = read_varint(&overlong, &mut i).expect_err("超长 varint");
    assert!(err.to_string().contains("varint too long"), "{err}");
}

#[test]
fn parse_fields_rejects_field_zero_and_bad_wire_type() {
    let err = parse_fields(&[0x00]).expect_err("field 0");
    assert!(err.to_string().contains("field 0"), "{err}");

    // tag = field 1, wire type 3（已废弃的 group 起始）→ 未注册的 wire type。
    let err = parse_fields(&[0x0B]).expect_err("wire type 3");
    assert!(err.to_string().contains("wire type 3"), "{err}");

    // 声明长度 5 但只剩 2 字节。
    let mut raw = tag_ld(1);
    raw.push(5);
    raw.extend_from_slice(b"ab");
    let err = parse_fields(&raw).expect_err("length overrun");
    assert!(err.to_string().contains("length overrun"), "{err}");
}

#[test]
fn getters_only_match_their_wire_type() {
    let payload = cat(&[vint(3, 7), ld(4, b"hi")]);
    assert_eq!(get_varint(&payload, 3).unwrap(), Some(7));
    assert_eq!(
        get_varint(&payload, 4).unwrap(),
        None,
        "wire type 2 不是 varint"
    );
    assert_eq!(get_str(&payload, 4).unwrap().as_deref(), Some("hi"));
    assert_eq!(
        get_str(&payload, 3).unwrap(),
        None,
        "wire type 0 不是字符串"
    );
    assert_eq!(get_str(&payload, 99).unwrap(), None);
}

#[test]
fn get_str_returns_none_on_invalid_utf8() {
    let payload = ld(4, &[0xFF, 0xFE]);
    assert_eq!(get_str(&payload, 4).unwrap(), None);
}

// ---- 可读文本启发式 ----

#[test]
fn readable_text_heuristic_matches_reference_cases() {
    assert!(is_readable_text("the code word is DELTA"));
    assert!(is_readable_text("第二暗号-MELON-E2"));
    assert!(is_readable_text("{\n  \"reply_parts\": []\n}"));
    assert!(!is_readable_text("abc"), "太短");
    assert!(
        !is_readable_text("3f2504e0-4f89-11d3-9a0c-0305e82c3301"),
        "UUID 不是文本"
    );
    assert!(!is_readable_text("deadbeefcafe1234"), "纯 hex 不是文本");
    assert!(!is_readable_text("run_command"), "标识符没有空格/换行/CJK");
    assert!(
        !is_readable_text("hello\u{0000}world abc"),
        "控制字符比例过高不是文本"
    );
}

#[test]
fn readable_text_walk_is_depth_limited() {
    // 深度上限是防御性措施（防止恶意/意外深嵌把递归爆掉），**不是**隐私过滤：
    // 外层包装字节自己也可能解码成可读文本（与 Python 参考实现一致）。
    // 所以这里断言的是：浅层能取到、极深层能安全终止。
    let nest = |levels: usize| {
        let mut buf = b"deep inside the buffer".to_vec();
        for _ in 0..levels {
            buf = ld(1, &buf);
        }
        buf
    };
    assert!(!payload_readable_texts(&nest(3)).is_empty());
    // 200 层：超过 MAX_LEAF_DEPTH，递归必须停下来而不是爆栈。
    let deep = nest(200);
    let _ = payload_readable_texts(&deep);
}

// ---- 任务 id 扫描 ----

#[test]
fn task_id_scan_finds_prefixed_ids() {
    let found = scan_task_ids("closing 10dcf27b-3303-41de-a0d7-26819556510d/task-2 now");
    assert_eq!(
        found,
        vec!["10dcf27b-3303-41de-a0d7-26819556510d/task-2".to_string()]
    );
}

#[test]
fn task_id_scan_requires_eight_char_prefix() {
    assert!(scan_task_ids("short/task-1").is_empty(), "前缀不足 8 字符");
    assert_eq!(
        scan_task_ids("abcdefgh/task-17"),
        vec!["abcdefgh/task-17".to_string()]
    );
}

#[test]
fn task_id_scan_takes_all_digits_and_several_ids() {
    let found =
        scan_task_ids("a: 3ae01db1-8c86-411b-a65c-7cfe2219aea2/task-1; b: xyz12345/task-22");
    assert_eq!(
        found,
        vec![
            "3ae01db1-8c86-411b-a65c-7cfe2219aea2/task-1".to_string(),
            "xyz12345/task-22".to_string(),
        ]
    );
}

// ---- 分类 ----

#[test]
fn classify_user_model_and_thought() {
    let mut counts = BTreeMap::new();
    let ev = classify(0, STEP_USER, &user_payload("你好 世界", 100), &mut counts).unwrap();
    assert_eq!(ev.kind, "user");
    assert_eq!(ev.text, "你好 世界");
    assert_eq!(ev.ts, Some(100.5));
    assert_eq!(ev.raw_type, STEP_USER);

    let ev = classify(
        1,
        STEP_MODEL,
        &model_text_payload("正文文本", 101),
        &mut counts,
    )
    .unwrap();
    assert_eq!(ev.kind, "agent_text");

    let ev = classify(
        2,
        STEP_MODEL,
        &model_thought_payload("先想一想", 102),
        &mut counts,
    )
    .unwrap();
    assert_eq!(ev.kind, "thought");
    assert_eq!(ev.text, "先想一想");

    let ev = classify(
        3,
        STEP_MODEL,
        &model_tool_payload("run_command", 103),
        &mut counts,
    )
    .unwrap();
    assert_eq!(ev.kind, "tool_call");
    assert_eq!(ev.tool, "run_command");

    let ev = classify(
        4,
        STEP_MODEL,
        &cat(&[ts_meta(104, 0), ld(20, &[])]),
        &mut counts,
    )
    .unwrap();
    assert_eq!(ev.kind, "model_empty");
    assert!(counts.is_empty(), "已知类型不进 other_counts：{counts:?}");
}

#[test]
fn classify_user_without_text_is_drift() {
    let mut counts = BTreeMap::new();
    // field 19 只有元数据，没有 19.2。
    let payload = cat(&[ts_meta(1, 0), ld(19, &ld(3, b"nothing here"))]);
    let err = classify(7, STEP_USER, &payload, &mut counts).expect_err("user 无文本");
    assert!(err.is_drift());
    assert!(
        err.to_string().contains("user step 7: no text at 19.2"),
        "{err}"
    );
}

#[test]
fn classify_model_without_field_20_is_drift() {
    let mut counts = BTreeMap::new();
    let payload = cat(&[ts_meta(1, 0), ld(21, b"elsewhere")]);
    let err = classify(3, STEP_MODEL, &payload, &mut counts).expect_err("model 无 20");
    assert!(
        err.to_string().contains("model step 3: no field 20"),
        "{err}"
    );
}

#[test]
fn classify_error_step_is_known_and_counted_not_drift() {
    let mut counts = BTreeMap::new();
    // 24 = 错误记录，24.3 = 子记录，24.3.2 = 可读信息。
    let payload = cat(&[
        ts_meta(9, 0),
        ld(24, &ld(3, &ld(2, b"API error 503 unavailable"))),
    ]);
    let ev = classify(11, STEP_ERROR, &payload, &mut counts).unwrap();
    assert_eq!(ev.kind, "error");
    assert_eq!(ev.text, "API error 503 unavailable");
    assert_eq!(counts.get(&17), Some(&1));
}

#[test]
fn classify_tool_result_carries_task_id_only_when_launched() {
    let mut counts = BTreeMap::new();
    let ev = classify(
        2,
        STEP_TOOL_RESULT,
        &tool_result_payload(Some("conv/task-1"), 5),
        &mut counts,
    )
    .unwrap();
    assert_eq!(ev.kind, "tool_result");
    assert_eq!(ev.task_id, "conv/task-1");

    let ev = classify(
        3,
        STEP_TOOL_RESULT,
        &tool_result_payload(None, 6),
        &mut counts,
    )
    .unwrap();
    assert_eq!(ev.task_id, "");
    assert!(counts.is_empty());
}

#[test]
fn classify_unknown_type_without_text_is_counted() {
    let mut counts = BTreeMap::new();
    let payload = cat(&[ts_meta(1, 0), ld(9, &[0x01, 0x02, 0x03])]);
    let ev = classify(4, 77, &payload, &mut counts).unwrap();
    assert_eq!(ev.kind, "other:77");
    assert_eq!(counts.get(&77), Some(&1));
}

#[test]
fn classify_unknown_type_with_readable_text_is_drift() {
    let mut counts = BTreeMap::new();
    let payload = cat(&[ts_meta(1, 0), ld(9, &ld(1, b"a whole sentence of prose"))]);
    let err = classify(5, 77, &payload, &mut counts).expect_err("漂移");
    assert!(err.is_drift());
    assert!(
        err.to_string().contains("unknown step_type 77 at idx 5"),
        "{err}"
    );
    assert!(counts.is_empty(), "漂移的步不计数");
}

#[test]
fn step_ts_absence_is_none_not_an_error() {
    let mut counts = BTreeMap::new();
    let payload = ld(19, &ld(2, b"no timestamp here"));
    let ev = classify(0, STEP_USER, &payload, &mut counts).unwrap();
    assert_eq!(ev.ts, None);
}

// ---- DB 解析 ----

async fn fixture_db(dir: &Path, rows: &[(i64, i64, Vec<u8>)]) -> std::path::PathBuf {
    let path = dir.join("conv.db");
    let options = SqliteConnectOptions::new()
        .filename(&path)
        .create_if_missing(true);
    let pool = SqlitePoolOptions::new()
        .max_connections(1)
        .connect_with(options)
        .await
        .expect("建库");
    sqlx::query(
        "CREATE TABLE steps (idx INTEGER PRIMARY KEY, step_type INTEGER NOT NULL DEFAULT 0,
                                 step_payload BLOB)",
    )
    .execute(&pool)
    .await
    .expect("建表");
    for (idx, stype, payload) in rows {
        sqlx::query("INSERT INTO steps (idx, step_type, step_payload) VALUES (?, ?, ?)")
            .bind(idx)
            .bind(stype)
            .bind(payload)
            .execute(&pool)
            .await
            .expect("写行");
    }
    pool.close().await;
    path
}

#[tokio::test]
async fn parse_steps_resolves_notification_task_ids() {
    let dir = tempfile::tempdir().expect("临时目录");
    let conv = "10dcf27b-3303-41de-a0d7-26819556510d";
    let db = fixture_db(
        dir.path(),
        &[
            (0, STEP_USER, user_payload("Run this shell command", 100)),
            (1, STEP_MODEL, model_tool_payload("run_command", 101)),
            (
                2,
                STEP_TOOL_RESULT,
                tool_result_payload(Some("conv/task-2"), 102),
            ),
            (
                3,
                STEP_TASK_NOTIFICATION,
                notification_payload(conv, 2, 103),
            ),
        ],
    )
    .await;

    let parsed = parse_steps(&db).await.expect("解析");
    assert_eq!(parsed.events.len(), 4);
    assert_eq!(parsed.events[3].kind, "task_notification");
    assert_eq!(
        parsed.events[3].task_id,
        format!("{conv}/task-2"),
        "101 事件第二遍填上被关闭的任务 id"
    );
    assert_eq!(parsed.events[2].task_id, "conv/task-2");
    assert!(parsed.other_counts.is_empty());
}

#[tokio::test]
async fn parse_steps_rereads_mutated_rows() {
    let dir = tempfile::tempdir().expect("临时目录");
    let db = fixture_db(
        dir.path(),
        &[
            (0, STEP_USER, user_payload("提问", 100)),
            (1, STEP_MODEL, model_thought_payload("思考中", 101)),
        ],
    )
    .await;
    let first = parse_steps(&db).await.expect("首读");
    assert_eq!(first.events[1].kind, "thought");

    // 同一个 idx 被原地改写：thought 变成正文。
    let options = SqliteConnectOptions::new().filename(&db);
    let pool = SqlitePoolOptions::new()
        .max_connections(1)
        .connect_with(options)
        .await
        .expect("重开");
    sqlx::query("UPDATE steps SET step_payload = ? WHERE idx = 1")
        .bind(model_text_payload("最终正文", 102))
        .execute(&pool)
        .await
        .expect("改写");
    pool.close().await;

    let second = parse_steps(&db).await.expect("重读");
    assert_eq!(second.events[1].kind, "agent_text");
    assert_eq!(second.events[1].text, "最终正文");
    assert_eq!(second.events[1].ts, Some(102.0));
}

#[tokio::test]
async fn parse_steps_reports_missing_db_as_storage_error() {
    let dir = tempfile::tempdir().expect("临时目录");
    let err = parse_steps(&dir.path().join("nope.db"))
        .await
        .expect_err("不存在的库");
    assert!(!err.is_drift(), "读不到不是漂移：{err}");
    assert!(matches!(err, ParseError::Storage(_)), "{err}");
}
