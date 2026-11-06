//! gemini-qq-bot 基座版（lite）：QQ 联系层 + 交互层 + 基座必需件。
//!
//! 与完整版同仓、共享 lib 与同一套 SQLite schema；差别只在
//! app 层：这里是一层薄装配（入站 → 攒批 → 每群一个常驻会话 →
//! 出站），没有记忆、表情、额度分档、管理台这些功能面。
//! 定位与配置见 docs/LITE.md。

pub mod config;
pub mod delivery;
pub mod intake;
pub mod sessions;
pub mod turn;

use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::{Mutex, Semaphore};

use crate::onebot::connection::Connection;
use crate::onebot::normalize::{IncomingMessage, Segment, SpeakerRole};
use crate::pipeline::batcher::{BatchEntry, BatchLimits, should_flush, split_batches};
use crate::pipeline::outbox::Outbox;
use crate::store::SqliteStore;

use config::LiteConfig;
use intake::LiteIntake;
use sessions::LiteSessions;
use turn::TurnRunner;

/// 陈旧护栏（秒）：pending 里躺超过这个年龄还没被认领的消息
/// 直接消费掉——迟到太久的回复发出去只会错位。
const STALE_S: i64 = 600;
/// flush 节拍（毫秒），与完整版一致。
const TICK_MS: u64 = 500;
/// 全局并发轮次上限，与完整版 agent_concurrency 一致。
const MAX_CONCURRENT_TURNS: usize = 2;

pub struct LiteApp {
    cfg: Arc<LiteConfig>,
    store: Arc<SqliteStore>,
    sessions: Arc<LiteSessions>,
    runner: Arc<TurnRunner>,
    intake: Arc<LiteIntake>,
    supervisor: Mutex<Option<Connection>>,
    /// 保险丝：最近一小时内已开轮次的时间戳。
    fuse: Arc<Mutex<Vec<i64>>>,
    fuse_last_audit: Arc<Mutex<i64>>,
    /// 正在跑轮次的群（每群串行）。
    running: Arc<Mutex<HashSet<String>>>,
    semaphore: Arc<Semaphore>,
    batch_seq: std::sync::atomic::AtomicU64,
}

impl LiteApp {
    pub async fn build(cfg: LiteConfig) -> Result<Self, String> {
        let cfg = Arc::new(cfg);
        let pool = crate::store::migrations::open_and_migrate(&cfg.db_path)
            .await
            .map_err(|e| e.to_string())?;
        let store = Arc::new(SqliteStore::new(pool));

        // 崩溃恢复（基座必需件，与完整版 App::build 同一套调用、
        // 同顺序）：被打断的调用标 unknown 永不重放；孤儿认领
        // 释放回池；outbox 与 jobs 对账；seen 表修剪。
        let now = crate::now_unix();
        let _ = store.reconcile_calls(now).await;
        if let Ok(released) = store.release_orphaned_claims(&cfg.bot_id).await {
            for (group_id, count) in released {
                if count == 0 {
                    continue;
                }
                let _ = store
                    .audit(
                        "system",
                        &format!("ops.startup.release_claims.g{group_id}.n{count}"),
                        Some(&cfg.bot_id),
                        Some(&group_id),
                        now,
                    )
                    .await;
            }
        }
        let _ = store.reconcile_jobs(now).await;
        let _ = store.reconcile_outbox(now).await;
        let _ = store.prune_seen(now - 7 * 86_400).await;

        // 白名单群建档并启用（基座版的群开关就是 LITE_GROUPS 本身）。
        for group_id in &cfg.groups {
            store
                .ensure_group(&cfg.bot_id, group_id, now)
                .await
                .map_err(|e| e.to_string())?;
            store
                .set_group_flags(&cfg.bot_id, group_id, Some(true), Some(false), None, now)
                .await
                .map_err(|e| e.to_string())?;
        }

        let (supervisor, handle) = Connection::new(cfg.ws_url.clone(), cfg.access_token.clone());
        let sessions = Arc::new(LiteSessions::new(Arc::clone(&store), Arc::clone(&cfg)));
        let runner = Arc::new(TurnRunner::new(
            Arc::clone(&store),
            Arc::clone(&cfg),
            Arc::clone(&sessions),
            handle,
            Arc::new(Outbox::new(Arc::clone(&store))),
        ));
        let intake = Arc::new(LiteIntake::new(Arc::clone(&store), Arc::clone(&cfg)));
        Ok(Self {
            cfg,
            store,
            sessions,
            runner,
            intake,
            supervisor: Mutex::new(Some(supervisor)),
            fuse: Arc::new(Mutex::new(Vec::new())),
            fuse_last_audit: Arc::new(Mutex::new(0)),
            running: Arc::new(Mutex::new(HashSet::new())),
            semaphore: Arc::new(Semaphore::new(MAX_CONCURRENT_TURNS)),
            batch_seq: std::sync::atomic::AtomicU64::new(1),
        })
    }

    pub async fn serve(self: Arc<Self>) -> Result<(), String> {
        let supervisor = self
            .supervisor
            .lock()
            .await
            .take()
            .ok_or("连接监督已被取走")?;
        let intake = Arc::clone(&self.intake);
        tokio::spawn(async move {
            supervisor.run(intake).await;
        });

        // 心跳：--health-probe 的唯一依据（lite 没有 HTTP 监听）。
        let hb_path = self.cfg.heartbeat_path();
        tokio::spawn(async move {
            loop {
                let _ = std::fs::write(&hb_path, crate::now_unix().to_string());
                tokio::time::sleep(Duration::from_secs(30)).await;
            }
        });

        tracing::info!(
            bot = %self.cfg.bot_id,
            groups = self.cfg.groups.len(),
            "gemini-qq-bot-lite 已启动"
        );
        loop {
            tokio::time::sleep(Duration::from_millis(TICK_MS)).await;
            self.flush_tick(crate::now_unix()).await;
        }
    }

    async fn flush_tick(&self, now: i64) {
        let running_snapshot: HashSet<String> = self.running.lock().await.clone();
        self.sessions.tick_all(&running_snapshot).await;

        for group_id in &self.cfg.groups {
            if running_snapshot.contains(group_id) {
                continue;
            }
            if self.sessions.backoff_active(group_id, now).await {
                continue;
            }
            let rows = match self
                .store
                .pending_for_group(&self.cfg.bot_id, group_id)
                .await
            {
                Ok(rows) => rows,
                Err(_) => continue,
            };
            if rows.is_empty() {
                continue;
            }
            // 陈旧护栏先行（只看未认领行，pending_for_group 已过滤）。
            let (stale, fresh): (Vec<_>, Vec<_>) = rows
                .into_iter()
                .partition(|r| now - r.received_at > STALE_S);
            if !stale.is_empty() {
                let ids: Vec<i64> = stale.iter().map(|r| r.id).collect();
                let count = ids.len();
                let _ = self.store.delete_pending(&ids).await;
                let _ = self
                    .store
                    .audit(
                        "system",
                        &format!("lite.stale_drop.g{group_id}.n{count}"),
                        Some(&self.cfg.bot_id),
                        Some(group_id),
                        now,
                    )
                    .await;
            }
            if fresh.is_empty() {
                continue;
            }
            // payload 解析失败的行不在此处理：攒批跳过它们，
            // 陈旧护栏最终会收走。
            let parsed: Vec<(i64, IncomingMessage)> = fresh
                .iter()
                .filter_map(|r| {
                    serde_json::from_str::<IncomingMessage>(&r.normalized_payload)
                        .ok()
                        .map(|m| (r.id, m))
                })
                .collect();
            if parsed.is_empty() {
                continue;
            }
            let entries: Vec<BatchEntry> = parsed
                .iter()
                .map(|(id, m)| BatchEntry {
                    id: *id,
                    chars: m.text.chars().count(),
                    images: m
                        .segments
                        .iter()
                        .filter(|s| matches!(s, Segment::Image { .. }))
                        .count(),
                    mentions_bot: m.mentions_bot,
                    from_owner: m.speaker_role == SpeakerRole::Owner,
                    received_at: m.received_at,
                })
                .collect();

            // 只应答触发：整池没有一条触发消息就不开轮（消息留作
            // 上下文，等触发消息到来时一并带上）。
            let mut triggered = self.cfg.respond_all;
            if !triggered {
                for (_, m) in &parsed {
                    let quote_is_bot = match &m.quote_of {
                        Some(q) => self.quote_targets_bot(group_id, q).await,
                        None => false,
                    };
                    if message_triggers(m, &self.cfg.call_names, false, quote_is_bot) {
                        triggered = true;
                        break;
                    }
                }
            }
            if !triggered {
                continue;
            }

            let limits = BatchLimits::default();
            let first_at = entries.first().map(|e| e.received_at).unwrap_or(now);
            let last_at = entries.last().map(|e| e.received_at).unwrap_or(now);
            let trigger = should_flush(
                &entries,
                first_at,
                last_at,
                now,
                self.cfg.quiet_s,
                self.cfg.max_age_s,
                limits,
            );
            if !trigger.is_flush() {
                continue;
            }

            // 全局小时保险丝：超限整 tick 不认领（只熔断，不分档）。
            {
                let mut fuse = self.fuse.lock().await;
                fuse.retain(|t| now - *t < 3600);
                if fuse.len() >= self.cfg.fuse_per_hour {
                    let mut last = self.fuse_last_audit.lock().await;
                    if now - *last > 300 {
                        *last = now;
                        let _ = self
                            .store
                            .audit(
                                "system",
                                &format!("lite.fuse_tripped.g{group_id}"),
                                Some(&self.cfg.bot_id),
                                Some(group_id),
                                now,
                            )
                            .await;
                    }
                    continue;
                }
            }

            let chunks = split_batches(&entries, limits);
            let Some(chunk) = chunks.first() else {
                continue;
            };
            let ids: Vec<i64> = chunk.iter().map(|e| e.id).collect();
            let seq = self
                .batch_seq
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let batch_id = format!("lite-{group_id}-{now}-{seq}");
            let claimed = match self.store.claim_batch(&ids, &batch_id).await {
                Ok(c) => c,
                Err(_) => continue,
            };
            if claimed.is_empty() {
                continue;
            }
            let claimed_rows: Vec<_> = fresh
                .into_iter()
                .filter(|r| claimed.contains(&r.id))
                .collect();
            self.fuse.lock().await.push(now);
            self.running.lock().await.insert(group_id.clone());

            let runner = Arc::clone(&self.runner);
            let semaphore = Arc::clone(&self.semaphore);
            let running = Arc::clone(&self.running);
            let group_id_owned = group_id.clone();
            tokio::spawn(async move {
                let _permit = semaphore.acquire_owned().await;
                runner.run(&group_id_owned, claimed_rows).await;
                running.lock().await.remove(&group_id_owned);
            });
        }
    }

    /// 引用目标是不是机器人自己发的（查群日志的说话人角色）。
    async fn quote_targets_bot(&self, group_id: &str, message_id: &str) -> bool {
        let row: Option<(String,)> = sqlx::query_as(
            "SELECT speaker_role FROM group_messages \
             WHERE bot_id = ? AND group_id = ? AND message_id = ? LIMIT 1",
        )
        .bind(&self.cfg.bot_id)
        .bind(group_id)
        .bind(message_id)
        .fetch_optional(self.store.pool())
        .await
        .ok()
        .flatten();
        row.is_some_and(|(role,)| role == "bot")
    }
}

/// 一条消息算不算「触发」（基座版的应答门槛）：@ 机器人、引用
/// 机器人的消息、主人发言、正文呼唤词命中，或全量模式。
/// 纯函数，`quote_is_bot` 由调用方查群日志后传入。
pub fn message_triggers(
    message: &IncomingMessage,
    call_names: &[String],
    respond_all: bool,
    quote_is_bot: bool,
) -> bool {
    respond_all
        || message.mentions_bot
        || quote_is_bot
        || message.speaker_role == SpeakerRole::Owner
        || text_names_bot(&message.text, call_names)
}

/// 正文是否喊了机器人（不区分大小写的子串匹配）。
/// 语义与完整版 app::intake::cadence::text_names_bot 相同
/// （2026-11-05 核对，含其单测用例）：lite 不依赖 app 层，
/// 这个六行谓词在此复制一份，改语义时两处同步。
pub fn text_names_bot(text: &str, names: &[String]) -> bool {
    if names.is_empty() {
        return false;
    }
    let lower = text.to_lowercase();
    names
        .iter()
        .any(|name| !name.is_empty() && lower.contains(&name.to_lowercase()))
}

/// --status 的只读自查：库里每个白名单群的状态 + 阻塞项。
/// 进程外的视角：会话是否活着只能看心跳新鲜度，逐群会话细节
/// 在 serve 进程内，这里如实只报库里能查到的。
pub async fn print_status(cfg: &LiteConfig) -> Result<String, String> {
    let pool = crate::store::migrations::open_and_migrate(&cfg.db_path)
        .await
        .map_err(|e| e.to_string())?;
    let store = SqliteStore::new(pool);
    let mut out = String::new();
    out.push_str("gemini-qq-bot-lite 状态\n");
    out.push_str(&format!("bot: {}\n", cfg.bot_id));
    out.push_str(&format!("库: {}\n", cfg.db_path.display()));
    match read_heartbeat(cfg) {
        Some(age) if age <= 90 => out.push_str(&format!("serve: 在跑（心跳 {age} 秒前）\n")),
        Some(age) => out.push_str(&format!("serve: 疑似不在跑（心跳 {age} 秒前）\n")),
        None => out.push_str("serve: 无心跳（未启动或数据目录不一致）\n"),
    }
    let mut blockers: Vec<String> = Vec::new();
    if !cfg.agy_bin.exists() && cfg.agy_bin.to_string_lossy() != "agy" {
        blockers.push(format!("agy 二进制不存在: {}", cfg.agy_bin.display()));
    }
    for group_id in &cfg.groups {
        let pending = store
            .pending_count(&cfg.bot_id, group_id)
            .await
            .unwrap_or(-1);
        match store.get_group(&cfg.bot_id, group_id).await {
            Ok(Some(g)) => out.push_str(&format!(
                "群 {group_id}: enabled={} paused={} 会话id={} pending={pending}\n",
                g.enabled,
                g.paused,
                if g.agy_conversation_id.is_some() {
                    "有"
                } else {
                    "无"
                },
            )),
            Ok(None) => out.push_str(&format!("群 {group_id}: 未建档\n")),
            Err(e) => out.push_str(&format!("群 {group_id}: 读失败 {e}\n")),
        }
    }
    if blockers.is_empty() {
        out.push_str("blockers: 无\n");
    } else {
        for b in &blockers {
            out.push_str(&format!("blocker: {b}\n"));
        }
    }
    Ok(out)
}

/// 心跳年龄（秒）；文件不存在或内容坏了返回 None。
pub fn read_heartbeat(cfg: &LiteConfig) -> Option<i64> {
    let raw = std::fs::read_to_string(cfg.heartbeat_path()).ok()?;
    let ts: i64 = raw.trim().parse().ok()?;
    Some(crate::now_unix() - ts)
}

/// --health-probe：心跳 90 秒内算活（与完整版探活的宽松口径
/// 一致：只断言还活着，不断言就绪）。
pub fn health_probe_ok(cfg: &LiteConfig) -> bool {
    read_heartbeat(cfg).is_some_and(|age| age <= 90)
}

/// --mcp-lite：给 agy 子进程用的 MCP stdio 服务，只提供
/// submit_reply。库路径与 serve 保持一致（LITE_DB_PATH），
/// 也认 GEMINI_MEMORY_DB（完整版部署里 MCP 配置用的变量名）。
pub async fn serve_mcp() -> std::process::ExitCode {
    let db_path = std::env::var("LITE_DB_PATH")
        .ok()
        .filter(|v| !v.trim().is_empty())
        .or_else(|| std::env::var("GEMINI_MEMORY_DB").ok())
        .unwrap_or_else(|| "./lite.sqlite3".to_string());
    let pool =
        match crate::store::migrations::open_and_migrate(std::path::Path::new(&db_path)).await {
            Ok(p) => p,
            Err(e) => {
                eprintln!("打开 lite 库失败: {e}");
                return std::process::ExitCode::from(3);
            }
        };
    let store = Arc::new(SqliteStore::new(pool));
    let session = Arc::new(crate::mcp::Session::submit_only());
    match session.run(store).await {
        Ok(()) | Err(crate::mcp::SessionError::Closed) => std::process::ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("lite MCP 会话中断: {err:?}");
            std::process::ExitCode::from(4)
        }
    }
}

#[cfg(test)]
mod tests;
