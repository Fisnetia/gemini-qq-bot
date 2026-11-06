//! P4 rev1 回放集成测试：拿 p43a 的参考实现断言口径，验证 Rust 侧的步流解析器
//! 与交付屏障给出同样裁决。
//!
//! 夹具在 `/tmp/insert-lab/`（实验会话 DB e0/e2/e4 + 生产 DB 副本 top-3）。整组测试
//! 用 `#[ignore]` 保护：CI 与无夹具的机器上不跑。显式执行：
//!
//! ```sh
//! GEMINI_INSERT_LAB_DIR=/tmp/insert-lab cargo test --offline --test p4_replay -- --ignored --nocapture
//! ```
//!
//! 环境变量缺省时直接跳过（返回空断言集），不失败。
//!
//! 期望值的唯一来源是 `replay_assert.py`；这里**不允许**为了让断言过而改期望。

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use gemini_qq_bot::agent::barrier::{self, DEFAULT_QUIET_S, TurnSpec, Verdict};
use gemini_qq_bot::agent::step_stream::{self, ParseResult, StepEvent};

const ENV_LAB: &str = "GEMINI_INSERT_LAB_DIR";

fn lab_root() -> Option<PathBuf> {
    let dir = std::env::var(ENV_LAB).ok()?;
    let path = PathBuf::from(dir);
    path.is_dir().then_some(path)
}

fn conv_dir(lab: &Path) -> PathBuf {
    lab.join("home/.gemini/antigravity-cli/conversations")
}

fn replay_dir(lab: &Path) -> PathBuf {
    lab.join("p43a/replay-data")
}

/// 实验 DB 的文件名（e0#2 / e2 / e4）。
const E0_DB: &str = "10dcf27b-3303-41de-a0d7-26819556510d.db";
const E2_DB: &str = "3ae01db1-8c86-411b-a65c-7cfe2219aea2.db";
const E4_DB: &str = "8f40d104-3125-4b9c-b58a-2c423fb16789.db";

/// 生产 DB 副本（REPLAY-RESULTS §3 的三行）。
const PROD_34AB3DAF: &str = "34ab3daf-084d-4ba3-a81a-ea173ca91f06.db";
const PROD_74C2D9B0: &str = "74c2d9b0-39ee-43aa-b1ad-29f4ace3e5a9.db";
const PROD_827264B4: &str = "827264b4-2b67-4092-9781-e8571e11da33.db";

async fn parse(path: &Path) -> ParseResult {
    step_stream::parse_steps(path)
        .await
        .unwrap_or_else(|e| panic!("解析 {} 失败: {e}", path.display()))
}

fn json_contract(text: &str) -> Result<(), gemini_qq_bot::agent::contract::RejectReason> {
    match serde_json::from_str::<serde_json::Value>(text.trim()) {
        Ok(serde_json::Value::Object(map)) if map.contains_key("reply_parts") => Ok(()),
        _ => Err(gemini_qq_bot::agent::contract::RejectReason::Malformed),
    }
}

/// 与 replay_assert.now_after 同口径：片区最后 ts + 12.5s。
fn now_after(events: &[StepEvent], spec: &TurnSpec, pad: f64) -> f64 {
    let last = events
        .iter()
        .filter(|e| e.idx >= spec.start_idx && spec.end_idx.is_none_or(|end| e.idx < end))
        .filter_map(|e| e.ts)
        .fold(f64::MIN, f64::max);
    last + pad
}

fn user_idx_by_text(events: &[StepEvent], needle: &str) -> i64 {
    events
        .iter()
        .find(|e| e.is_kind("user") && e.text.contains(needle))
        .map(|e| e.idx)
        .unwrap_or_else(|| panic!("步流里找不到含 {needle:?} 的 user 步"))
}

fn kinds(parsed: &ParseResult) -> Vec<(i64, &str)> {
    parsed
        .events
        .iter()
        .map(|e| (e.idx, e.kind.as_str()))
        .collect()
}

// ---------------------------------------------------------------------------
// E0#2：确认步被未配对的后台任务拦住

#[tokio::test]
#[ignore = "需要 /tmp/insert-lab 夹具"]
async fn replay_e0_blocks_on_unpaired_task() {
    let Some(lab) = lab_root() else {
        eprintln!("跳过：未设置 {ENV_LAB}");
        return;
    };
    let parsed = parse(&conv_dir(&lab).join(E0_DB)).await;
    let ev = &parsed.events;
    assert!(
        parsed.other_counts.is_empty(),
        "e0 other_counts={:?}",
        parsed.other_counts
    );

    let i_task = user_idx_by_text(ev, "Run this shell command");
    let i_ins = user_idx_by_text(ev, "the code word is DELTA");
    let spec = TurnSpec::new(i_task).with_inserts([i_ins]);
    let now = now_after(ev, &spec, 12.5);
    let verdict = barrier::evaluate(ev, &spec, now, DEFAULT_QUIET_S, None).expect("求值");

    let Verdict::NotReady { reasons } = &verdict else {
        panic!("E0#2 期望 not_ready，实际 {verdict:?}");
    };
    assert_eq!(verdict.deliver_idx(), None);
    assert!(
        reasons.iter().any(|r| r.starts_with("unpaired_task:")),
        "拦截原因必须包含未配对任务：{reasons:?}"
    );
    assert!(
        reasons
            .iter()
            .any(|r| r.contains("10dcf27b-3303-41de-a0d7-26819556510d/task-2")),
        "原因里要带真实任务 id：{reasons:?}"
    );

    // 所有 agent_text 都不被交付；tool_call 步 idx==1 从不交付。
    for e in ev.iter().filter(|e| e.is_kind("agent_text")) {
        assert_ne!(verdict.deliver_idx(), Some(e.idx), "误交付 idx {}", e.idx);
    }
    assert_ne!(verdict.deliver_idx(), Some(1), "tool_call 步不是交付物");

    // 确认步在最后一个 user 步之后，所以草稿列表是空的 —— 它被条件③拦下。
    assert!(
        barrier::list_drafts(ev, &spec).is_empty(),
        "草稿列表应为空：{:?}",
        barrier::list_drafts(ev, &spec)
    );
}

// ---------------------------------------------------------------------------
// E2：插入前的完整回复是草稿，交付合并回复

#[tokio::test]
#[ignore = "需要 /tmp/insert-lab 夹具"]
async fn replay_e2_delivers_merged_and_keeps_draft() {
    let Some(lab) = lab_root() else {
        eprintln!("跳过：未设置 {ENV_LAB}");
        return;
    };
    let parsed = parse(&conv_dir(&lab).join(E2_DB)).await;
    let ev = &parsed.events;
    assert!(
        parsed.other_counts.is_empty(),
        "e2 other_counts={:?}",
        parsed.other_counts
    );

    let i_task = user_idx_by_text(ev, "逐个列出从 1 到 120");
    let inserts: Vec<i64> = ev
        .iter()
        .filter(|e| e.is_kind("user") && e.idx != i_task)
        .map(|e| e.idx)
        .collect();
    let spec = TurnSpec::new(i_task).with_inserts(inserts);
    let now = now_after(ev, &spec, 12.5);
    let verdict = barrier::evaluate(ev, &spec, now, DEFAULT_QUIET_S, None).expect("求值");

    let Verdict::Deliver { idx, text } = &verdict else {
        panic!("E2 期望 deliver，实际 {verdict:?}");
    };
    let max_idx = ev.iter().map(|e| e.idx).max().expect("非空步流");
    assert_eq!(*idx, max_idx, "交付最后一条合并步");
    assert_eq!(text.chars().count(), 4200, "E2 合并回复实测 4200 字");
    assert!(text.contains("暗号-ORCHID-E2"));
    assert!(text.contains("第二暗号-MELON-E2"));

    let mut draft_idx: Vec<i64> = ev
        .iter()
        .filter(|e| e.is_kind("agent_text") && e.idx != *idx)
        .map(|e| e.idx)
        .collect();
    draft_idx.sort_unstable();
    let drafts = barrier::list_drafts(ev, &spec);
    assert_eq!(drafts, draft_idx, "草稿列表只含插入前那条");
    assert_eq!(draft_idx.len(), 1);
    let draft_text = ev
        .iter()
        .find(|e| e.idx == draft_idx[0])
        .expect("草稿步")
        .text
        .clone();
    assert_eq!(draft_text.chars().count(), 4173, "E2 草稿实测 4173 字");
    assert_ne!(&draft_text, text, "草稿不是合并回复");
}

// ---------------------------------------------------------------------------
// E4：三轮各自交付唯一合法 JSON；T2 的 776 字插入前 JSON 是草稿

#[tokio::test]
#[ignore = "需要 /tmp/insert-lab 夹具"]
async fn replay_e4_three_turns() {
    let Some(lab) = lab_root() else {
        eprintln!("跳过：未设置 {ENV_LAB}");
        return;
    };
    let parsed = parse(&conv_dir(&lab).join(E4_DB)).await;
    let ev = &parsed.events;
    assert!(
        parsed.other_counts.is_empty(),
        "e4 other_counts={:?}",
        parsed.other_counts
    );

    let i_t1 = user_idx_by_text(ev, "今晚有人去滨江骑行吗");
    let i_t2 = user_idx_by_text(ev, "请逐个回应下面每个人");
    let i_ins = user_idx_by_text(ev, "新增消息，并入本轮");
    let i_t3 = user_idx_by_text(ev, "大家晚安");

    let turns: [(&str, TurnSpec, i64); 3] = [
        ("T1", TurnSpec::new(i_t1).with_end(Some(i_t2)), 13),
        (
            "T2",
            TurnSpec::new(i_t2)
                .with_inserts([i_ins])
                .with_end(Some(i_t3)),
            17,
        ),
        ("T3", TurnSpec::new(i_t3), 19),
    ];

    for (label, spec, expect_idx) in turns {
        let now = now_after(ev, &spec, 12.5);
        let verdict =
            barrier::evaluate(ev, &spec, now, DEFAULT_QUIET_S, Some(&json_contract)).expect("求值");
        let Verdict::Deliver { idx, text } = &verdict else {
            panic!("E4 {label} 期望 deliver，实际 {verdict:?}");
        };
        assert_eq!(*idx, expect_idx, "E4 {label} 交付步");
        assert!(
            json_contract(text).is_ok(),
            "E4 {label} 交付物必须是合法契约 JSON"
        );
    }

    // T2 专项：交付文本覆盖插入的图片消息，插入前那条 776 字 JSON 是草稿。
    let spec = TurnSpec::new(i_t2)
        .with_inserts([i_ins])
        .with_end(Some(i_t3));
    let now = now_after(ev, &spec, 12.5);
    let verdict =
        barrier::evaluate(ev, &spec, now, DEFAULT_QUIET_S, Some(&json_contract)).expect("求值");
    let text = verdict.deliver_text().expect("交付文本");
    assert!(
        text.contains("5880") || text.contains("红色"),
        "T2 交付文本要覆盖插入的图片消息"
    );
    assert_eq!(barrier::list_drafts(ev, &spec), vec![15]);
    let draft_len = ev
        .iter()
        .find(|e| e.idx == 15)
        .expect("草稿步")
        .text
        .chars()
        .count();
    assert_eq!(draft_len, 776, "E4 T2 插入前 JSON 实测 776 字");
}

// ---------------------------------------------------------------------------
// 三个实验 DB 的形状自检：与 REPLAY-RESULTS 记录的步数/kinds 一致

#[tokio::test]
#[ignore = "需要 /tmp/insert-lab 夹具"]
async fn lab_dbs_parse_without_drift_and_expected_shapes() {
    let Some(lab) = lab_root() else {
        eprintln!("跳过：未设置 {ENV_LAB}");
        return;
    };
    let dir = conv_dir(&lab);

    let e0 = parse(&dir.join(E0_DB)).await;
    assert_eq!(
        kinds(&e0),
        vec![
            (0, "user"),
            (1, "tool_call"),
            (2, "tool_result"),
            (3, "user"),
            (4, "agent_text"),
        ]
    );
    assert_eq!(
        e0.events[2].task_id,
        "10dcf27b-3303-41de-a0d7-26819556510d/task-2"
    );

    let e2 = parse(&dir.join(E2_DB)).await;
    assert_eq!(
        kinds(&e2),
        vec![
            (0, "user"),
            (1, "agent_text"),
            (2, "user"),
            (3, "user"),
            (4, "agent_text"),
        ]
    );

    let e4 = parse(&dir.join(E4_DB)).await;
    assert_eq!(e4.events.len(), 20);
    let tool_calls = e4.events.iter().filter(|e| e.is_kind("tool_call")).count();
    let tool_results = e4
        .events
        .iter()
        .filter(|e| e.is_kind("tool_result"))
        .count();
    assert_eq!(tool_calls, 6, "E4 六个 MCP 工具步");
    assert_eq!(tool_results, 6);
    // E4 的工具结果全是同步的（没有 field 148）。
    assert!(
        e4.events
            .iter()
            .filter(|e| e.is_kind("tool_result"))
            .all(|e| e.task_id.is_empty()),
        "E4 无后台任务"
    );
}

// ---------------------------------------------------------------------------
// 生产副本：other 计数与 REPLAY-RESULTS §3 表格一致

#[tokio::test]
#[ignore = "需要 /tmp/insert-lab 夹具"]
async fn production_copies_match_the_scan_table() {
    let Some(lab) = lab_root() else {
        eprintln!("跳过：未设置 {ENV_LAB}");
        return;
    };
    let dir = replay_dir(&lab);

    // 34ab3daf：73 步 / other {17: 3}
    let p = parse(&dir.join(PROD_34AB3DAF)).await;
    assert_eq!(p.events.len(), 73, "34ab3daf 步数");
    assert_eq!(
        p.other_counts,
        BTreeMap::from([(17, 3)]),
        "34ab3daf other_counts"
    );
    // 3 条 error 步带可读 API 错误信息，且**不是**漂移。
    let errors: Vec<&StepEvent> = p.events.iter().filter(|e| e.is_kind("error")).collect();
    assert_eq!(errors.len(), 3);
    assert!(errors.iter().all(|e| !e.text.is_empty()), "error 步带文本");

    // 827264b4：172 步 / other {}（无 error、无未知类型）
    let p = parse(&dir.join(PROD_827264B4)).await;
    assert_eq!(p.events.len(), 172, "827264b4 步数");
    assert!(p.other_counts.is_empty(), "827264b4 other_counts");

    // 74c2d9b0：137 步 / other {}
    let p = parse(&dir.join(PROD_74C2D9B0)).await;
    assert_eq!(p.events.len(), 137, "74c2d9b0 步数");
    assert!(p.other_counts.is_empty(), "74c2d9b0 other_counts");
}

// ---------------------------------------------------------------------------
// 生产副本的轮结构：把每个 user 步当作潜在轮起点，逐轮求值必须不 panic，
// 且已完成的历史轮都应当落在一个 deliver 上（草稿永不交付）。

#[tokio::test]
#[ignore = "需要 /tmp/insert-lab 夹具"]
async fn production_turns_all_settle() {
    let Some(lab) = lab_root() else {
        eprintln!("跳过：未设置 {ENV_LAB}");
        return;
    };
    let dir = replay_dir(&lab);

    for name in [PROD_34AB3DAF, PROD_827264B4, PROD_74C2D9B0] {
        let parsed = parse(&dir.join(name)).await;
        let ev = &parsed.events;
        let users: Vec<i64> = ev
            .iter()
            .filter(|e| e.is_kind("user"))
            .map(|e| e.idx)
            .collect();
        assert!(!users.is_empty(), "{name} 至少要有一个 user 步");

        for (pos, &start) in users.iter().enumerate() {
            let end = users.get(pos + 1).copied();
            let spec = TurnSpec::new(start).with_end(end);
            let now = now_after(ev, &spec, 12.5);
            let verdict = barrier::evaluate(ev, &spec, now, DEFAULT_QUIET_S, Some(&json_contract))
                .unwrap_or_else(|e| panic!("{name} idx {start} 求值失败: {e}"));
            // 历史轮：要么交付，要么因为契约不合法失败；不允许「草稿被交付」。
            if let Verdict::Deliver { idx, .. } = &verdict {
                assert!(
                    !barrier::list_drafts(ev, &spec).contains(idx),
                    "{name} idx {start}: 交付了草稿 idx {idx}"
                );
            }
        }
    }
}
