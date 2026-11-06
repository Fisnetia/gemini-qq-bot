//! lite 轮次执行：每群一个常驻 agy 会话，本文件管会话的生老
//! 病死与一轮的完整流程。
//!
//! 流程骨架照完整版的 resident 路径（app::turn）与探针轮次
//! （arm_turn → deliver → submissions_for + barrier 轮询 →
//! disarm）：武装口令防迟到提交、屏障五条件定收束、最后一份
//! 合格提交才确认。砍掉的是：插话（TurnSpec 恒无插入）、
//! salvage、节奏/相关性闸门、记忆与表情的应用、探针轮换判定
//! （轮换只按轮次计数，与完整版的固定轮次口径一致）。
//!
//! 与 agy 的耦合只经 agent::resident / agent::barrier /
//! agent::step_stream 的公开面，本文件不解析步流原始字段。

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::agent::barrier::{DEFAULT_QUIET_S, TurnSpec, Verdict};
use crate::agent::contract::{self, ValidationScope};
use crate::agent::prompt::{self, PromptImages};
use crate::agent::resident::ResidentSession;
use crate::agent::segments::filter_secrets;
use crate::memory::retrieval::MemoryContext;
use crate::onebot::connection::ConnectionHandle;
use crate::onebot::normalize::IncomingMessage;
use crate::pipeline::outbox::Outbox;
use crate::store::{CallState, PendingEventRow, SqliteStore};

use super::config::LiteConfig;
use super::sessions::LiteSessions;

/// 屏障轮询间隔（毫秒）。
const POLL_MS: u64 = 250;
/// 软顶（秒）：超过只记日志，不打断。
const SOFT_CAP_S: u64 = 45;
/// 硬顶（秒）：超过即中断本轮、按失败收场。
const HARD_CAP_S: u64 = 90;
/// 发送年龄门（秒）：本批最新消息超过这个年龄，整轮不发——
/// 迟到的回复发出去只会错位（与完整版的发送年龄门同口径）。
const SEND_GATE_S: i64 = 300;

/// 写进每群 AGENTS.md 的基座版输出契约（人格卡之后）。
/// 与完整版的差别：工具面只有 submit_reply，其余字段留空。
const LITE_CONTRACT: &str = "# 输出契约（基座版）\n\
每轮结束时，必须且只调用一次 submit_reply 作为最后一个动作，把本轮结果提交出去：\n\
- turn_token：照抄本轮提示末尾契约卡上给的口令。\n\
- should_reply：本轮是否发言。不想说话就填 false 且正文留空——静默是合法结果，不要为了发言而发言。\n\
- reply_parts：发言正文，字符串数组，一段发一条消息；也可以用 reply_text 给整段。\n\
- 需要引用某条消息时，把那条消息的 message_ref（如 m3）填进对应段的 quote_ref，或填顶层 quote_ref 表示引用对象是第一段。\n\
- memory_ops、sticker_ops、nicknames、mood_update、topic_state、poke_ref：基座版不处理，一律留空。\n\
提交被拒绝时，按返回的原因修正，在本轮内重新调用，不要等到下一轮。";

enum Disposition {
    /// 本批已处理（成功或内容性失败）：从 pending 删除。
    Consume,
    /// 基础设施失败：退回未认领，等退避/恢复后再来。
    Release,
}

struct TurnEnd {
    state: CallState,
    error_class: Option<String>,
    disposition: Disposition,
}

impl TurnEnd {
    fn succeeded() -> Self {
        Self {
            state: CallState::Succeeded,
            error_class: None,
            disposition: Disposition::Consume,
        }
    }
    fn failed(error_class: &str, disposition: Disposition) -> Self {
        Self {
            state: CallState::Failed,
            error_class: Some(error_class.to_string()),
            disposition,
        }
    }
}

pub struct TurnRunner {
    pub(super) store: Arc<SqliteStore>,
    pub(super) cfg: Arc<LiteConfig>,
    pub(super) sessions: Arc<LiteSessions>,
    pub(super) connection: ConnectionHandle,
    pub(super) outbox: Arc<Outbox>,
}

impl TurnRunner {
    pub fn new(
        store: Arc<SqliteStore>,
        cfg: Arc<LiteConfig>,
        sessions: Arc<LiteSessions>,
        connection: ConnectionHandle,
        outbox: Arc<Outbox>,
    ) -> Self {
        Self {
            store,
            cfg,
            sessions,
            connection,
            outbox,
        }
    }

    pub(super) async fn audit(&self, group_id: &str, action: &str) {
        let _ = self
            .store
            .audit(
                "system",
                action,
                Some(&self.cfg.bot_id),
                Some(group_id),
                crate::now_unix(),
            )
            .await;
    }

    /// 跑完一整批（已认领）。pending 的去向与 call 的收尾都在这里。
    pub async fn run(&self, group_id: &str, rows: Vec<PendingEventRow>) {
        let ids: Vec<i64> = rows.iter().map(|r| r.id).collect();
        let batch_id = rows
            .first()
            .and_then(|r| r.batch_id.clone())
            .unwrap_or_default();
        let messages: Vec<IncomingMessage> = rows
            .iter()
            .filter_map(|r| serde_json::from_str(&r.normalized_payload).ok())
            .collect();
        if messages.is_empty() {
            // payload 全坏的批次留着只会反复认领，直接消费。
            let _ = self.store.delete_pending(&ids).await;
            return;
        }
        let call_id = self
            .store
            .start_call(&self.cfg.bot_id, group_id, &batch_id, crate::now_unix())
            .await
            .ok();
        let end = self.run_turn(group_id, &batch_id, &messages).await;
        match end.disposition {
            Disposition::Consume => {
                let _ = self.store.delete_pending(&ids).await;
            }
            Disposition::Release => {
                let _ = self.store.release_events(&ids).await;
            }
        }
        if let Some(call_id) = call_id {
            let _ = self
                .store
                .finish_call(
                    &call_id,
                    end.state,
                    end.error_class.as_deref(),
                    crate::now_unix(),
                )
                .await;
        }
    }

    async fn run_turn(
        &self,
        group_id: &str,
        batch_id: &str,
        messages: &[IncomingMessage],
    ) -> TurnEnd {
        self.prepare_workspace(group_id);
        let session = match self.sessions.get_or_spawn(group_id).await {
            Ok(s) => s,
            Err(e) => {
                let now = crate::now_unix();
                let (tier, until) = self.sessions.note_failure(group_id, now).await;
                if tier > 0 {
                    self.audit(
                        group_id,
                        &format!("lite.backoff.g{group_id}.tier{tier}.until{until}"),
                    )
                    .await;
                }
                self.audit(group_id, &format!("lite.turn.failed.g{group_id}.spawn"))
                    .await;
                tracing::warn!(group = %group_id, error = %e, "lite 会话启动失败");
                return TurnEnd::failed("spawn_failed", Disposition::Release);
            }
        };
        let token = lite_token();
        let now = crate::now_unix();
        let _ = self
            .store
            .arm_turn(
                group_id,
                &token,
                batch_id,
                now,
                now + HARD_CAP_S as i64 + 60,
            )
            .await;
        let end = self
            .run_armed(group_id, batch_id, messages, &session, &token)
            .await;
        let _ = self.store.disarm_turn(group_id, &token).await;
        end
    }

    async fn run_armed(
        &self,
        group_id: &str,
        batch_id: &str,
        messages: &[IncomingMessage],
        session: &Arc<ResidentSession>,
        token: &str,
    ) -> TurnEnd {
        // 批外引用目标的一句话摘要（渲染成「引用更早的消息：…」）。
        let batch_ids: HashSet<&str> = messages.iter().map(|m| m.message_id.as_str()).collect();
        let mut quote_summaries: HashMap<String, String> = HashMap::new();
        for m in messages {
            if let Some(q) = &m.quote_of
                && !batch_ids.contains(q.as_str())
                && !quote_summaries.contains_key(q)
                && let Ok(Some(text)) = self
                    .store
                    .group_message_text(&self.cfg.bot_id, group_id, q)
                    .await
            {
                quote_summaries.insert(q.clone(), text.chars().take(60).collect());
            }
        }
        // 渲染与校验作用域都用完整版的同一个纯函数组装（这是本文件
        // 唯一引用的 app 层符号：纯函数、无运行时子系统，语义与
        // 生产逐字一致，避免基座版自创格式造成行为漂移）。
        let (mut batch, scope) = crate::app::turn::build_prompt_batch(
            messages,
            MemoryContext::empty(),
            None,
            None,
            None,
            PromptImages::default(),
            Vec::new(),
            &quote_summaries,
        );
        batch.turn_token = Some(token.to_string());
        batch.now_line = Some(prompt::now_line(crate::now_unix()));
        let text = prompt::render(&batch);

        let receipt = match session.deliver(&text).await {
            Ok(r) => r,
            Err(_) => {
                // 投递未确认意味着会话多半卡死在某个界面上：弃会话、
                // 清存档 id，下一轮全新开始（完整版同口径，2026-10-30
                // 起一次即弃）。本批退回，不记退避梯。
                self.sessions.drop_session(group_id, true).await;
                self.audit(group_id, &format!("lite.turn.failed.g{group_id}.deliver"))
                    .await;
                return TurnEnd::failed("deliver_unconfirmed", Disposition::Release);
            }
        };
        let _ = self
            .store
            .set_conversation_id(
                &self.cfg.bot_id,
                group_id,
                Some(&receipt.conversation_id),
                crate::now_unix(),
            )
            .await;

        let spec = TurnSpec::new(receipt.step_idx);
        enum LoopEnd {
            Commit(String),
            BarrierFailed,
            Timeout,
            StepsFailed,
        }
        let started = Instant::now();
        let deadline = started + Duration::from_secs(HARD_CAP_S);
        let mut soft_warned = false;
        let loop_end = loop {
            // 提交先看：MCP 通道的交付物在 store 里，屏障只定收束。
            let mut accepted: Option<String> = None;
            if let Ok(rows) = self.store.submissions_for(token).await {
                accepted = rows
                    .iter()
                    .rev()
                    .find(|r| r.verdict == "accepted")
                    .map(|r| r.payload.clone());
            }
            match session
                .evaluate(&spec, crate::now_unix() as f64, DEFAULT_QUIET_S, None)
                .await
            {
                Ok((_, Verdict::Deliver { .. })) => {
                    if let Some(payload) = accepted {
                        break LoopEnd::Commit(payload);
                    }
                    // 步流已收束但还没提交：模型可能正在调工具，
                    // 继续等到硬顶（完整版 MCP 通道同处理）。
                }
                Ok((_, Verdict::Failed { .. })) => break LoopEnd::BarrierFailed,
                Ok((_, Verdict::NotReady { .. })) => {}
                Err(_) => {
                    let _ = session.interrupt().await;
                    break LoopEnd::StepsFailed;
                }
            }
            if Instant::now() >= deadline {
                let _ = session.interrupt().await;
                break LoopEnd::Timeout;
            }
            if !soft_warned && started.elapsed() > Duration::from_secs(SOFT_CAP_S) {
                soft_warned = true;
                tracing::info!(group = %group_id, "lite 轮次超过软顶，继续等到硬顶");
            }
            tokio::time::sleep(Duration::from_millis(POLL_MS)).await;
        };

        match loop_end {
            LoopEnd::Commit(payload) => {
                self.commit(group_id, batch_id, messages, &scope, &payload)
                    .await
            }
            LoopEnd::BarrierFailed => {
                self.audit(group_id, &format!("lite.turn.failed.g{group_id}.barrier"))
                    .await;
                TurnEnd::failed("barrier_failed", Disposition::Consume)
            }
            LoopEnd::Timeout => {
                self.audit(group_id, &format!("lite.turn.failed.g{group_id}.timeout"))
                    .await;
                TurnEnd::failed("timeout", Disposition::Release)
            }
            LoopEnd::StepsFailed => {
                self.audit(group_id, &format!("lite.turn.failed.g{group_id}.steps"))
                    .await;
                TurnEnd::failed("steps_failed", Disposition::Release)
            }
        }
    }

    /// 屏障放行且有合格提交：校验 → 年龄门 → 入队 → 逐段发送。
    async fn commit(
        &self,
        group_id: &str,
        batch_id: &str,
        messages: &[IncomingMessage],
        scope: &ValidationScope,
        payload: &str,
    ) -> TurnEnd {
        let now = crate::now_unix();
        let validated = match contract::validate(payload, scope) {
            Ok(v) => v,
            Err(reason) => {
                self.audit(
                    group_id,
                    &format!("lite.turn.failed.g{group_id}.contract.{}", reason.as_str()),
                )
                .await;
                return TurnEnd::failed("contract_invalid", Disposition::Consume);
            }
        };
        self.sessions.note_clean(group_id).await;
        let turns = self.sessions.bump_turns(group_id).await;
        let output = validated.output;

        if !output.should_reply {
            self.finish_success(group_id, turns).await;
            self.audit(group_id, &format!("lite.turn.completed.g{group_id}.silent"))
                .await;
            return TurnEnd::succeeded();
        }
        // 发送年龄门：本批最新一条都已经太旧，回复发出去只会错位。
        let newest = messages.iter().map(|m| m.received_at).max().unwrap_or(now);
        if now - newest > SEND_GATE_S {
            self.audit(group_id, &format!("lite.send_gate_drop.g{group_id}"))
                .await;
            self.finish_success(group_id, turns).await;
            return TurnEnd::succeeded();
        }

        let mut parts: Vec<String> = if output.reply_parts.is_empty() {
            vec![output.reply_text.clone()]
        } else {
            output.reply_parts.clone()
        };
        parts = parts
            .iter()
            .map(|p| filter_secrets(p, &self.cfg.secrets).trim().to_string())
            .filter(|p| !p.is_empty())
            .collect();
        if parts.is_empty() {
            self.finish_success(group_id, turns).await;
            self.audit(group_id, &format!("lite.turn.completed.g{group_id}.silent"))
                .await;
            return TurnEnd::succeeded();
        }

        // 引用解析：quote_ref 是批内别名（mN），落到真实 message_id。
        let resolve = |r: &str| -> Option<String> {
            let n: usize = r.strip_prefix('m')?.parse().ok()?;
            messages
                .get(n.checked_sub(1)?)
                .map(|m| m.message_id.clone())
        };
        let mut part_quotes: Vec<Option<String>> = vec![None; parts.len()];
        for pq in &output.part_quotes {
            if let Some(slot) = part_quotes.get_mut(pq.part_index) {
                *slot = resolve(&pq.quote_ref);
            }
        }
        if part_quotes[0].is_none()
            && let Some(q) = &output.quote_ref
        {
            part_quotes[0] = resolve(q);
        }
        let part_quote_refs: Vec<Option<&str>> = part_quotes.iter().map(|o| o.as_deref()).collect();

        let ids = match self
            .outbox
            .enqueue_mixed(
                &self.cfg.bot_id,
                group_id,
                batch_id,
                &parts,
                &[],
                &part_quote_refs,
                now,
            )
            .await
        {
            Ok(ids) => ids,
            Err(_) => {
                return TurnEnd::failed("outbox_enqueue", Disposition::Consume);
            }
        };
        self.deliver_parts(&ids, group_id, batch_id).await;
        self.finish_success(group_id, turns).await;
        self.audit(group_id, &format!("lite.turn.completed.g{group_id}"))
            .await;
        TurnEnd::succeeded()
    }

    /// 轮次成功后的会话处置：到轮次上限就轮换（关会话 + 清 id，
    /// 无交接纸条，新会话靠 AGENTS.md 与后续批次重新建立上下文）。
    async fn finish_success(&self, group_id: &str, turns: i64) {
        if turns >= self.cfg.turn_limit {
            self.sessions.drop_session(group_id, true).await;
            self.sessions.reset_turns(group_id).await;
            self.audit(group_id, &format!("lite.session.rotated.g{group_id}"))
                .await;
        }
    }

    /// 工作区文件：AGENTS.md（人设 + 基座契约）与 scope.json，
    /// 内容没变就不重写（agy 每次启动自己读，口径同完整版）。
    fn prepare_workspace(&self, group_id: &str) {
        let dir = self.cfg.project_dir(group_id);
        let _ = std::fs::create_dir_all(&dir);
        let agents = format!("{}\n\n{}", self.cfg.persona.trim(), LITE_CONTRACT);
        write_if_changed(&dir.join("AGENTS.md"), &agents);
        let scope = crate::mcp::render_scope_json(&self.cfg.bot_id, group_id);
        write_if_changed(&dir.join("scope.json"), &scope);
    }
}

fn write_if_changed(path: &std::path::Path, content: &str) {
    if std::fs::read_to_string(path).is_ok_and(|old| old == content) {
        return;
    }
    let _ = std::fs::write(path, content);
}

/// 一轮的提交口令（16 位 hex）。配方与完整版 app::turn 的
/// new_turn_token 相同（2026-11-05 核对）：只是相关性标记，
/// 不是凭据，唯一性足够。
fn lite_token() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(1);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);
    format!("{:016x}", nanos ^ n.wrapping_mul(0x9E37_79B9_7F4A_7C15))
}
