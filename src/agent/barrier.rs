//! 交付屏障：判断常驻会话的一轮何时完成、哪条文本可以发（P4 rev1）。
//!
//! 语义逐条移植自参考实现 `p43a/delivery_barrier.py`，并做一处生产适配：
//! Python 版的 `insert_idxs` 是事后已知的（回放时插入了哪些步一清二楚）；生产里
//! 插入步的 idx 只有在它落进 DB 之后才知道，所以 [`TurnSpec`] 多一个
//! `pending_inserts`——「已经投递但还没看到落库」的插入数。它让屏障在第 1 条条件上
//! 就能把「我投了但 agy 还没接纳」和「agy 接纳了」区分开，而不是靠超时猜。
//!
//! 一轮（logical turn）从一个 user 步开始，可以在飞行中被若干*插入* user 步扩展。
//! 只有全部满足才算完成：
//!
//! 1. 本轮发出的每个插入都已被接纳（它的 user 步存在于步流中）；
//! 2. 最后一个被接纳的 user 步之后至少有一条 agent_text；
//! 3. 本轮内发起的后台任务没有仍未配对的（配对 = 之后出现引用它 id 的 101 通知）；
//! 4. 静默期：本轮最后一条步之后已经过了至少 `quiet_s` 秒；
//! 5. 交付物是最后一个被接纳 user 步之后**最后一条** agent_text，且必须过调用方
//!    注入的契约校验。
//!
//! 草稿红线：出现在最后一个被接纳 user 步**之前**的每一条 agent_text 都是草稿
//! （可能是中间确认，也可能是一条完整的插入前回复），**永不交付**。最后一条
//! agent_text 校验不过则整轮失败（[`Verdict::Failed`]），屏障**永不回退草稿**。

use std::collections::BTreeSet;

use crate::agent::step_stream::StepEvent;

/// 默认静默期（秒）。E4 实测插入落库到下一步最长 8.0s，旧 `--print` 用的 4s 窗口
/// 会误判，所以取 12s。
pub const DEFAULT_QUIET_S: f64 = 12.0;

/// 一轮的逻辑边界：起点、已观察到的插入、在途插入数与片尾。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TurnSpec {
    /// 开轮的 user 步 idx。
    pub start_idx: i64,
    /// 已观察到的插入 user 步（驱动随时登记）。
    pub insert_idxs: BTreeSet<i64>,
    /// 已投递但尚未观察到落库的插入数。
    pub pending_inserts: usize,
    /// 轮片尾（下一轮起点，独占）；活跃轮为 `None`。
    pub end_idx: Option<i64>,
}

impl TurnSpec {
    pub fn new(start_idx: i64) -> Self {
        Self {
            start_idx,
            ..Default::default()
        }
    }

    pub fn with_end(mut self, end_idx: Option<i64>) -> Self {
        self.end_idx = end_idx;
        self
    }

    pub fn with_inserts(mut self, inserts: impl IntoIterator<Item = i64>) -> Self {
        self.insert_idxs = inserts.into_iter().collect();
        self
    }

    pub fn with_pending_inserts(mut self, pending: usize) -> Self {
        self.pending_inserts = pending;
        self
    }

    /// 登记一个刚刚落库的插入步，同时把它从在途计数里扣掉。
    ///
    /// 幂等：重复登记同一个 idx 不会把 `pending_inserts` 减成负数 —— 一次投递
    /// 只对应一次销账。
    pub fn accept_insert(&mut self, idx: i64) {
        if self.insert_idxs.insert(idx) {
            self.pending_inserts = self.pending_inserts.saturating_sub(1);
        }
    }
}

/// 屏障的裁决。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    /// 这一轮可以发了：发 `text`，它来自 `idx` 这条步。
    Deliver { idx: i64, text: String },
    /// 还没完成，`reasons` 说明卡在哪。
    NotReady { reasons: Vec<String> },
    /// 完成了但交付物不合法 —— 整轮失败，不回退草稿。
    Failed { idx: i64, reason: String },
}

impl Verdict {
    pub fn is_deliver(&self) -> bool {
        matches!(self, Verdict::Deliver { .. })
    }

    pub fn is_not_ready(&self) -> bool {
        matches!(self, Verdict::NotReady { .. })
    }

    pub fn is_failed(&self) -> bool {
        matches!(self, Verdict::Failed { .. })
    }

    /// 裁决给出的交付文本（非 Deliver 时为 `None`）。
    pub fn deliver_text(&self) -> Option<&str> {
        match self {
            Verdict::Deliver { text, .. } => Some(text),
            _ => None,
        }
    }

    /// 裁决给出的交付步 idx（NotReady 时为 `None`）。
    pub fn deliver_idx(&self) -> Option<i64> {
        match self {
            Verdict::Deliver { idx, .. } | Verdict::Failed { idx, .. } => Some(*idx),
            Verdict::NotReady { .. } => None,
        }
    }

    pub fn reasons(&self) -> &[String] {
        match self {
            Verdict::NotReady { reasons } => reasons,
            Verdict::Failed { reason, .. } => std::slice::from_ref(reason),
            Verdict::Deliver { .. } => &[],
        }
    }
}

fn slice<'a>(events: &'a [StepEvent], spec: &TurnSpec) -> Vec<&'a StepEvent> {
    events
        .iter()
        .filter(|e| e.idx >= spec.start_idx && spec.end_idx.is_none_or(|end| e.idx < end))
        .collect()
}

/// 本轮所有被草稿规则标记为「永不交付」的 agent_text（最后一个被接纳 user 步之前）。
pub fn list_drafts(events: &[StepEvent], spec: &TurnSpec) -> Vec<i64> {
    let sl = slice(events, spec);
    let users: Vec<i64> = sl
        .iter()
        .filter(|e| e.is_kind("user"))
        .map(|e| e.idx)
        .collect();
    if users.is_empty() {
        return Vec::new();
    }
    let last_user = *users.iter().max().expect("users 非空");
    sl.iter()
        .filter(|e| e.is_kind("agent_text") && e.idx < last_user)
        .map(|e| e.idx)
        .collect()
}

/// 求值一轮。
///
/// `events` 是**整条**步流（后台任务的配对要在全流上解析：通知落在哪里都算数，
/// 不限于本轮片内）。`now` 与事件 `ts` 同一个时钟。`validator` 是契约校验（Rev3 里
/// 注入 `contract::validate` 的包装），`None` 表示不校验（回放与单测用）。
pub fn evaluate(
    events: &[StepEvent],
    spec: &TurnSpec,
    now: f64,
    quiet_s: f64,
    validator: Option<
        &(dyn Fn(&str) -> Result<(), crate::agent::contract::RejectReason> + Send + Sync),
    >,
) -> Result<Verdict, String> {
    let start = events
        .iter()
        .find(|e| e.idx == spec.start_idx)
        .ok_or_else(|| format!("turn start {} is not a user step", spec.start_idx))?;
    if !start.is_kind("user") {
        return Err(format!(
            "turn start {} is not a user step (kind={})",
            spec.start_idx, start.kind
        ));
    }

    let sl = slice(events, spec);
    let users: Vec<i64> = {
        let mut v: Vec<i64> = sl
            .iter()
            .filter(|e| e.is_kind("user"))
            .map(|e| e.idx)
            .collect();
        v.sort_unstable();
        v
    };
    let accepted: Vec<i64> = users
        .iter()
        .copied()
        .filter(|u| *u == spec.start_idx || spec.insert_idxs.contains(u))
        .collect();
    let mut reasons: Vec<String> = Vec::new();

    // 条件 1：本轮发出的插入全部被接纳。
    let missing: Vec<i64> = spec
        .insert_idxs
        .iter()
        .copied()
        .filter(|i| !users.contains(i))
        .collect();
    if !missing.is_empty() || spec.pending_inserts > 0 {
        reasons.push(format!(
            "insert_not_accepted:pending={}missing={missing:?}",
            spec.pending_inserts
        ));
    }

    let last_user = accepted.iter().copied().max().unwrap_or(spec.start_idx);
    let candidates: Vec<&StepEvent> = sl
        .iter()
        .copied()
        .filter(|e| e.is_kind("agent_text") && e.idx > last_user)
        .collect();

    // 条件 2：最后一个被接纳 user 步之后有 agent_text。
    if candidates.is_empty() {
        reasons.push("no_agent_text_after_last_user".to_string());
    }

    // 条件 3：本轮发起的后台任务全部被关闭。
    let mut launched: Vec<(String, i64)> = Vec::new();
    for e in sl.iter().filter(|e| e.is_kind("tool_result")) {
        if !e.task_id.is_empty() && !launched.iter().any(|(tid, _)| tid == &e.task_id) {
            launched.push((e.task_id.clone(), e.idx));
        }
    }
    if !launched.is_empty() {
        let mut closed: BTreeSet<String> = BTreeSet::new();
        for e in events.iter().filter(|e| e.is_kind("task_notification")) {
            if e.task_id.is_empty() {
                continue;
            }
            for tid in e.task_id.split(',') {
                if let Some((_, launch_idx)) = launched.iter().find(|(id, _)| id == tid)
                    && e.idx > *launch_idx
                {
                    closed.insert(tid.to_string());
                }
            }
        }
        let mut unpaired: Vec<&String> = launched
            .iter()
            .map(|(tid, _)| tid)
            .filter(|tid| !closed.contains(*tid))
            .collect();
        unpaired.sort();
        for tid in unpaired {
            reasons.push(format!("unpaired_task:{tid}"));
        }
    }

    // 条件 4：静默期。
    let last_ts = sl.iter().filter_map(|e| e.ts).fold(None::<f64>, |acc, ts| {
        Some(acc.map_or(ts, |prev: f64| prev.max(ts)))
    });
    match last_ts {
        Some(last_ts) => {
            let remaining = quiet_s - (now - last_ts);
            if remaining > 0.0 {
                reasons.push(format!("quiet_not_elapsed:{remaining:.1}s_remaining"));
            }
        }
        None => reasons.push("no_timestamps".to_string()),
    }

    if !reasons.is_empty() {
        return Ok(Verdict::NotReady { reasons });
    }

    // 条件 5：最后一条 agent_text，过契约校验，永不回退草稿。
    let deliverable = candidates.last().expect("条件 2 已保证非空");
    if let Some(validate) = validator
        && let Err(reject) = validate(&deliverable.text)
    {
        // The reject kind rides along: an undifferentiated "contract_invalid"
        // hid for a day whether the deep-step failures were malformed JSON,
        // empty replies or forbidden markup (2026-10-27).
        return Ok(Verdict::Failed {
            idx: deliverable.idx,
            reason: format!("contract_invalid.{}", reject.as_str()),
        });
    }
    Ok(Verdict::Deliver {
        idx: deliverable.idx,
        text: deliverable.text.clone(),
    })
}

/// `evaluate` 的草稿视图：一次求值同时给出裁决和草稿列表。
///
/// 生产里审计要记「这轮有几条草稿被丢掉」，分开调用会重复扫一遍片区。
pub fn evaluate_with_drafts(
    events: &[StepEvent],
    spec: &TurnSpec,
    now: f64,
    quiet_s: f64,
    validator: Option<
        &(dyn Fn(&str) -> Result<(), crate::agent::contract::RejectReason> + Send + Sync),
    >,
) -> Result<(Verdict, Vec<i64>), String> {
    let verdict = evaluate(events, spec, now, quiet_s, validator)?;
    Ok((verdict, list_drafts(events, spec)))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ev(idx: i64, kind: &str, text: &str, ts: f64) -> StepEvent {
        StepEvent {
            idx,
            kind: kind.to_string(),
            text: text.to_string(),
            tool: String::new(),
            task_id: String::new(),
            ts: Some(ts),
            raw_type: 15,
        }
    }

    fn user(idx: i64, text: &str, ts: f64) -> StepEvent {
        StepEvent {
            raw_type: 14,
            ..ev(idx, "user", text, ts)
        }
    }

    fn tool_result(idx: i64, task: &str, ts: f64) -> StepEvent {
        StepEvent {
            task_id: task.to_string(),
            raw_type: 132,
            ..ev(idx, "tool_result", "", ts)
        }
    }

    fn notification(idx: i64, closes: &[&str], ts: f64) -> StepEvent {
        StepEvent {
            task_id: closes.join(","),
            raw_type: 101,
            ..ev(idx, "task_notification", "", ts)
        }
    }

    fn json_contract(text: &str) -> Result<(), crate::agent::contract::RejectReason> {
        match serde_json::from_str::<serde_json::Value>(text.trim()) {
            Ok(serde_json::Value::Object(map)) if map.contains_key("reply_parts") => Ok(()),
            _ => Err(crate::agent::contract::RejectReason::Malformed),
        }
    }

    fn last_ts(events: &[StepEvent]) -> f64 {
        events.iter().filter_map(|e| e.ts).fold(f64::MIN, f64::max)
    }

    /// 与 replay_assert.now_after 同口径：片区最后 ts + 12.5s。
    fn now_after(events: &[StepEvent], spec: &TurnSpec, pad: f64) -> f64 {
        let sl = slice(events, spec);
        let ts = sl
            .iter()
            .filter_map(|e| e.ts)
            .fold(f64::MIN, |a, b| a.max(b));
        ts + pad
    }

    // ---- E2 型：插入前完整回复 + 双插入 + 合并回复 ----

    #[test]
    fn e2_shape_delivers_merged_and_keeps_the_pre_insert_draft() {
        let events = vec![
            user(0, "逐个列出从 1 到 120", 100.0),
            ev(1, "agent_text", "1. 一\n2. 二\n…（全文 4173 字）", 100.1),
            user(2, "暗号-ORCHID-E2", 121.0),
            user(3, "第二暗号-MELON-E2", 139.0),
            ev(
                4,
                "agent_text",
                "1. 一 暗号-ORCHID-E2\n…\n120. 一百二十 第二暗号-MELON-E2",
                144.0,
            ),
        ];
        let spec = TurnSpec::new(0).with_inserts([2, 3]);
        let now = now_after(&events, &spec, 12.5);
        let (verdict, drafts) =
            evaluate_with_drafts(&events, &spec, now, DEFAULT_QUIET_S, None).expect("求值");

        assert!(verdict.is_deliver(), "{verdict:?}");
        let Verdict::Deliver { idx, text } = &verdict else {
            unreachable!()
        };
        assert_eq!(*idx, 4, "交付合并回复");
        assert!(text.contains("暗号-ORCHID-E2"));
        assert!(text.contains("第二暗号-MELON-E2"));
        assert_eq!(drafts, vec![1], "插入前那条是草稿");
    }

    // ---- E4-T2 型：插入前合法 JSON 草稿 + 带图片的插入 + 合并 JSON ----

    #[test]
    fn e4_t2_shape_delivers_merged_json_and_never_the_draft_json() {
        let draft_json = format!(
            "{{\"reply_parts\":[\"{}\"],\"should_reply\":true}}",
            "啊".repeat(700)
        );
        let merged = "{\"reply_parts\":[\"骑行 5880 元押金已收到\",\"红色那辆是我的\"],\"should_reply\":true}";
        let events = vec![
            user(14, "请逐个回应下面每个人", 100.0),
            ev(15, "agent_text", &draft_json, 100.1),
            user(16, "新增消息，并入本轮", 105.0),
            ev(17, "agent_text", merged, 123.0),
        ];
        let spec = TurnSpec::new(14).with_inserts([16]).with_end(Some(18));
        let now = now_after(&events, &spec, 12.5);
        let (verdict, drafts) =
            evaluate_with_drafts(&events, &spec, now, DEFAULT_QUIET_S, Some(&json_contract))
                .expect("求值");

        let Verdict::Deliver { idx, text } = &verdict else {
            panic!("期望交付，实际 {verdict:?}")
        };
        assert_eq!(*idx, 17);
        assert!(text.contains("5880") || text.contains("红色"));
        assert_eq!(drafts, vec![15], "插入前那条 JSON 是草稿");
        assert_ne!(*idx, 15, "永不交付草稿");
    }

    #[test]
    fn e4_t2_shape_fails_when_the_merged_json_is_invalid() {
        let draft_json = "{\"reply_parts\":[\"插入前的合法回复\"],\"should_reply\":true}";
        let events = vec![
            user(14, "请逐个回应下面每个人", 100.0),
            ev(15, "agent_text", draft_json, 100.1),
            user(16, "新增消息，并入本轮", 105.0),
            ev(17, "agent_text", "抱歉，我不太确定……", 123.0),
        ];
        let spec = TurnSpec::new(14).with_inserts([16]).with_end(Some(18));
        let now = now_after(&events, &spec, 12.5);
        let verdict =
            evaluate(&events, &spec, now, DEFAULT_QUIET_S, Some(&json_contract)).expect("求值");

        let Verdict::Failed { idx, reason } = &verdict else {
            panic!("期望失败，实际 {verdict:?}")
        };
        assert_eq!(*idx, 17);
        assert_eq!(reason, "contract_invalid.malformed");
        assert!(
            verdict.deliver_text().is_none(),
            "草稿红线：校验失败绝不回退到插入前那条合法 JSON"
        );
    }

    // ---- E0#2 型：最后 user 步之后有 agent_text，但本轮有未配对后台任务 ----

    #[test]
    fn e0_shape_blocks_on_unpaired_background_task() {
        let task = "10dcf27b-3303-41de-a0d7-26819556510d/task-2";
        let events = vec![
            user(0, "Run this shell command and tell me the code word", 100.0),
            ev(1, "tool_call", "", 100.7),
            tool_result(2, task, 108.6),
            user(3, "the code word is DELTA", 115.3),
            ev(4, "agent_text", "收到，DELTA。任务仍在后台运行。", 118.7),
        ];
        let spec = TurnSpec::new(0).with_inserts([3]);
        let now = now_after(&events, &spec, 12.5);
        let verdict = evaluate(&events, &spec, now, DEFAULT_QUIET_S, None).expect("求值");

        let Verdict::NotReady { reasons } = &verdict else {
            panic!("期望未就绪，实际 {verdict:?}")
        };
        assert!(
            reasons.iter().any(|r| r.starts_with("unpaired_task:")),
            "未配对任务是拦下它的原因：{reasons:?}"
        );
        assert!(
            reasons.iter().any(|r| r.contains(task)),
            "原因里要带任务 id：{reasons:?}"
        );
        assert_eq!(verdict.deliver_idx(), None);
        assert_eq!(
            list_drafts(&events, &spec),
            Vec::<i64>::new(),
            "确认步在最后一个 user 步之后，被条件③拦下而不是被草稿规则拦下"
        );
    }

    #[test]
    fn e0_shape_delivers_once_the_task_is_closed() {
        let task = "10dcf27b-3303-41de-a0d7-26819556510d/task-2";
        let events = vec![
            user(0, "Run this shell command", 100.0),
            tool_result(2, task, 108.6),
            user(3, "the code word is DELTA", 115.3),
            notification(4, &[task], 120.0),
            ev(5, "agent_text", "任务已结束，暗号 DELTA。", 121.0),
        ];
        let spec = TurnSpec::new(0).with_inserts([3]);
        let now = now_after(&events, &spec, 12.5);
        let verdict = evaluate(&events, &spec, now, DEFAULT_QUIET_S, None).expect("求值");
        assert!(verdict.is_deliver(), "{verdict:?}");
        assert_eq!(verdict.deliver_idx(), Some(5));
    }

    #[test]
    fn notification_that_predates_the_launch_does_not_close_the_task() {
        let task = "conv-aaaa/task-7";
        let events = vec![
            notification(0, &[task], 90.0),
            user(1, "开轮", 100.0),
            tool_result(2, task, 101.0),
            ev(3, "agent_text", "处理中", 102.0),
        ];
        let spec = TurnSpec::new(1);
        let now = now_after(&events, &spec, 12.5);
        let verdict = evaluate(&events, &spec, now, DEFAULT_QUIET_S, None).expect("求值");
        assert!(
            verdict
                .reasons()
                .iter()
                .any(|r| r.starts_with("unpaired_task:")),
            "关闭必须发生在发起之后：{verdict:?}"
        );
    }

    // ---- 正常单轮 / 静默期 ----

    #[test]
    fn single_turn_with_one_json_delivers() {
        let events = vec![
            user(0, "在吗", 100.0),
            ev(1, "thought", "先想想", 100.2),
            ev(
                2,
                "agent_text",
                "{\"reply_parts\":[\"在\"],\"should_reply\":true}",
                104.0,
            ),
        ];
        let spec = TurnSpec::new(0);
        let now = now_after(&events, &spec, 12.5);
        let verdict =
            evaluate(&events, &spec, now, DEFAULT_QUIET_S, Some(&json_contract)).expect("求值");
        assert_eq!(verdict.deliver_idx(), Some(2));
        assert!(json_contract(verdict.deliver_text().expect("文本")).is_ok());
    }

    #[test]
    fn quiet_window_not_elapsed_blocks_delivery() {
        let events = vec![
            user(0, "在吗", 100.0),
            ev(
                1,
                "agent_text",
                "{\"reply_parts\":[\"在\"],\"should_reply\":true}",
                104.0,
            ),
        ];
        let spec = TurnSpec::new(0);
        let verdict =
            evaluate(&events, &spec, 108.0, DEFAULT_QUIET_S, Some(&json_contract)).expect("求值");
        let Verdict::NotReady { reasons } = &verdict else {
            panic!("期望未就绪，实际 {verdict:?}")
        };
        assert!(
            reasons.iter().any(|r| r.starts_with("quiet_not_elapsed:")),
            "{reasons:?}"
        );
    }

    #[test]
    fn missing_timestamps_block_delivery() {
        let mut events = vec![user(0, "在吗", 100.0), ev(1, "agent_text", "在", 104.0)];
        for e in &mut events {
            e.ts = None;
        }
        let spec = TurnSpec::new(0);
        let verdict = evaluate(&events, &spec, 1_000.0, DEFAULT_QUIET_S, None).expect("求值");
        assert_eq!(verdict.reasons(), ["no_timestamps"]);
    }

    #[test]
    fn no_agent_text_after_last_user_blocks_delivery() {
        // 草稿/候选的划分只看 idx，不看 ts：idx 2 在 start 之后但在 `补充一句`（idx 5）
        // 之前，因此是草稿；最后一个 user 步之后一条文本也没有。
        let events = [
            user(0, "在吗", 100.0),
            ev(2, "agent_text", "早期回复", 101.0),
            user(5, "补充一句", 102.0),
        ];
        let spec = TurnSpec::new(0).with_inserts([5]);
        let now = now_after(&events, &spec, 12.5);
        let verdict = evaluate(&events, &spec, now, DEFAULT_QUIET_S, None).expect("求值");
        assert_eq!(verdict.reasons(), ["no_agent_text_after_last_user"]);
        assert_eq!(list_drafts(&events, &spec), vec![2]);
    }

    #[test]
    fn pending_insert_blocks_delivery_until_it_lands() {
        let events = vec![user(0, "开轮", 100.0), ev(1, "agent_text", "好的", 101.0)];
        let spec = TurnSpec::new(0).with_pending_inserts(1);
        let verdict = evaluate(&events, &spec, 200.0, DEFAULT_QUIET_S, None).expect("求值");
        let Verdict::NotReady { reasons } = &verdict else {
            panic!("期望未就绪，实际 {verdict:?}")
        };
        assert!(
            reasons
                .iter()
                .any(|r| r.starts_with("insert_not_accepted:")),
            "{reasons:?}"
        );

        // 插入落库后同一份步流就能交付。
        let mut spec = spec;
        spec.accept_insert(1);
        let events = vec![
            user(0, "开轮", 100.0),
            user(1, "插入", 101.0),
            ev(2, "agent_text", "好的", 102.0),
        ];
        let verdict = evaluate(&events, &spec, 200.0, DEFAULT_QUIET_S, None).expect("求值");
        assert!(verdict.is_deliver(), "{verdict:?}");
        assert_eq!(spec.pending_inserts, 0);
    }

    #[test]
    fn accept_insert_is_idempotent() {
        let mut spec = TurnSpec::new(0).with_pending_inserts(1);
        spec.accept_insert(3);
        spec.accept_insert(3);
        assert_eq!(spec.pending_inserts, 0);
        assert_eq!(
            spec.insert_idxs.iter().copied().collect::<Vec<_>>(),
            vec![3]
        );
    }

    #[test]
    fn unregistered_insert_user_step_does_not_raise_the_draft_boundary() {
        // 参考实现的 accepted 只看 start + insert_idxs：步流里存在一个没登记的 user 步
        // *不会*抬高 last_user。这正是生产必须「先登记 pending_inserts、落库后再
        // accept_insert」的原因 —— 否则插入前的文本会被当成候选而不是草稿。
        let events = vec![
            user(0, "开轮", 100.0),
            ev(1, "agent_text", "早期文本", 101.0),
            user(3, "未登记的插入", 102.0),
        ];
        let spec = TurnSpec::new(0);
        let now = now_after(&events, &spec, 12.5);
        let (verdict, drafts) =
            evaluate_with_drafts(&events, &spec, now, DEFAULT_QUIET_S, None).expect("求值");
        assert_eq!(verdict.deliver_idx(), Some(1), "没登记的插入不构成草稿边界");
        // list_drafts 与参考实现一致，用**全部** user 步的 max（不是 accepted 的 max），
        // 所以这里它已经认为 idx 1 是草稿；evaluate 的候选边界则看不到这个未登记插入。
        // 两者故意不一致：草稿列表是保守的告警视图，交付裁决才是权威。
        assert_eq!(drafts, vec![1]);

        // 登记以后它就变成边界，同一份步流变成 not_ready（插入后还没有文本）。
        let spec = TurnSpec::new(0).with_inserts([3]);
        let verdict = evaluate(&events, &spec, 300.0, DEFAULT_QUIET_S, None).expect("求值");
        assert!(verdict.is_not_ready(), "{verdict:?}");
        assert_eq!(verdict.reasons(), ["no_agent_text_after_last_user"]);
        assert_eq!(list_drafts(&events, &spec), vec![1]);
    }

    #[test]
    fn start_idx_must_be_a_user_step() {
        let events = vec![user(0, "开轮", 100.0), ev(1, "agent_text", "好的", 101.0)];
        let err = evaluate(&events, &TurnSpec::new(1), 200.0, DEFAULT_QUIET_S, None)
            .expect_err("起点不是 user 步");
        assert!(err.contains("is not a user step"), "{err}");

        let err = evaluate(&events, &TurnSpec::new(42), 200.0, DEFAULT_QUIET_S, None)
            .expect_err("起点不存在");
        assert!(err.contains("is not a user step"), "{err}");
    }

    #[test]
    fn end_idx_excludes_the_next_turn() {
        let events = vec![
            user(0, "第一轮", 100.0),
            ev(1, "agent_text", "回复一", 101.0),
            user(5, "第二轮", 200.0),
            ev(6, "agent_text", "回复二", 201.0),
        ];
        let spec = TurnSpec::new(0).with_end(Some(5));
        let verdict = evaluate(&events, &spec, 300.0, DEFAULT_QUIET_S, None).expect("求值");
        assert_eq!(verdict.deliver_idx(), Some(1));

        // 活跃轮（end=None）会看到第二轮的文本作为候选 —— 所以生产里一轮结束后
        // 必须立刻把片尾钉到下一轮起点。
        let open = TurnSpec::new(0);
        let verdict = evaluate(&events, &open, 300.0, DEFAULT_QUIET_S, None).expect("求值");
        assert_eq!(verdict.deliver_idx(), Some(6));
    }

    #[test]
    fn list_drafts_is_empty_without_user_steps() {
        let events = vec![ev(1, "agent_text", "孤立的文本", 101.0)];
        assert!(list_drafts(&events, &TurnSpec::new(1)).is_empty());
    }

    #[test]
    fn drafts_are_never_the_deliverable_even_when_candidates_exist() {
        let events = vec![
            user(0, "开轮", 100.0),
            ev(1, "agent_text", "草稿甲", 101.0),
            user(2, "插入", 102.0),
            ev(3, "agent_text", "草稿乙", 103.0),
            user(4, "再插入", 104.0),
            ev(5, "agent_text", "正式回复", 105.0),
        ];
        let spec = TurnSpec::new(0).with_inserts([2, 4]);
        let verdict = evaluate(&events, &spec, 200.0, DEFAULT_QUIET_S, None).expect("求值");
        assert_eq!(verdict.deliver_idx(), Some(5));
        assert_eq!(list_drafts(&events, &spec), vec![1, 3]);
        let drafts = list_drafts(&events, &spec);
        assert!(!drafts.contains(&verdict.deliver_idx().expect("idx")));
    }

    #[test]
    fn last_ts_helper_ignores_none() {
        let events = vec![ev(0, "user", "", 100.0), {
            let mut e = ev(1, "agent_text", "", 50.0);
            e.ts = None;
            e
        }];
        assert_eq!(last_ts(&events), 100.0);
    }
}
