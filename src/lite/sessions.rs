//! lite 会话管理：每群一个常驻 agy 会话的登记、拉起、关闭，
//! 与重生退避梯。
//!
//! 与 agy 的耦合只经 agent::resident 的公开面（ResidentSession、
//! ResidentConfig），本文件不解析步流原始字段。退避梯的值与
//! 完整版 app::resident 的 RESPAWN_BACKOFF_STEPS 相同（2026-11-05
//! 核对）：基座版不依赖 app 层，常量在此复制一份，改梯子时两处同步。

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use tokio::sync::Mutex;

use crate::agent::resident::{DbStepReader, ResidentConfig, ResidentSession, SessionState};
use crate::store::SqliteStore;

use super::config::LiteConfig;

/// 投递确认窗口（秒）：prompt 多久没落成 user 步算投递失败。
const DELIVER_TIMEOUT_S: u64 = 30;
/// 重生退避梯（秒）。
const BACKOFF_STEPS: [i64; 4] = [60, 300, 900, 1800];
/// 连续干净轮达到这个数，重生退避计数清零（完整版同口径）。
const CLEAN_ROUNDS_TO_RESET: u32 = 10;

#[derive(Default)]
struct GroupState {
    session: Option<Arc<ResidentSession>>,
    /// 当前会话（按 conversation 计）已完成的轮数，驱动轮换。
    turns: i64,
    clean_rounds: u32,
    backoff_tier: u32,
    backoff_until: i64,
}

/// 每群会话与重生退避的登记处。
pub struct LiteSessions {
    store: Arc<SqliteStore>,
    cfg: Arc<LiteConfig>,
    groups: Mutex<HashMap<String, GroupState>>,
}

impl LiteSessions {
    pub fn new(store: Arc<SqliteStore>, cfg: Arc<LiteConfig>) -> Self {
        Self {
            store,
            cfg,
            groups: Mutex::new(HashMap::new()),
        }
    }

    async fn audit(&self, group_id: &str, action: &str) {
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

    pub async fn backoff_active(&self, group_id: &str, now: i64) -> bool {
        if !self.cfg.respawn_backoff {
            return false;
        }
        let groups = self.groups.lock().await;
        groups.get(group_id).is_some_and(|g| g.backoff_until > now)
    }

    /// 记一次重生失败，走一档退避梯；返回 (tier, until)。
    /// 退避关闭时返回 (0, 0)，调用方据此不写 backoff 审计。
    pub async fn note_failure(&self, group_id: &str, now: i64) -> (u32, i64) {
        if !self.cfg.respawn_backoff {
            return (0, 0);
        }
        let mut groups = self.groups.lock().await;
        let g = groups.entry(group_id.to_string()).or_default();
        g.backoff_tier = (g.backoff_tier + 1).min(BACKOFF_STEPS.len() as u32);
        g.backoff_until = now + BACKOFF_STEPS[(g.backoff_tier - 1) as usize];
        g.clean_rounds = 0;
        (g.backoff_tier, g.backoff_until)
    }

    pub async fn note_clean(&self, group_id: &str) {
        let mut groups = self.groups.lock().await;
        let g = groups.entry(group_id.to_string()).or_default();
        g.clean_rounds += 1;
        if g.clean_rounds >= CLEAN_ROUNDS_TO_RESET {
            g.backoff_tier = 0;
            g.backoff_until = 0;
            g.clean_rounds = 0;
        }
    }

    pub async fn bump_turns(&self, group_id: &str) -> i64 {
        let mut groups = self.groups.lock().await;
        let g = groups.entry(group_id.to_string()).or_default();
        g.turns += 1;
        g.turns
    }

    pub async fn reset_turns(&self, group_id: &str) {
        let mut groups = self.groups.lock().await;
        if let Some(g) = groups.get_mut(group_id) {
            g.turns = 0;
        }
    }

    async fn live_session(&self, group_id: &str) -> Option<Arc<ResidentSession>> {
        let session = {
            let groups = self.groups.lock().await;
            groups.get(group_id).and_then(|g| g.session.clone())
        }?;
        match session.state().await {
            SessionState::Dead(_) | SessionState::Blocked(_) => None,
            _ => Some(session),
        }
    }

    /// 取活着的会话，没有就按存档的 conversation id 拉起一个。
    pub async fn get_or_spawn(&self, group_id: &str) -> Result<Arc<ResidentSession>, String> {
        if let Some(session) = self.live_session(group_id).await {
            return Ok(session);
        }
        let conversation_id = self
            .store
            .get_group(&self.cfg.bot_id, group_id)
            .await
            .ok()
            .flatten()
            .and_then(|g| g.agy_conversation_id);
        let config = ResidentConfig {
            bin: self.cfg.agy_bin.clone(),
            home: self.cfg.agy_home.clone(),
            workspace: self.cfg.project_dir(group_id),
            model: self.cfg.agy_model.clone(),
            conversation_id,
            deliver_timeout_s: DELIVER_TIMEOUT_S,
            memory_scope: Some(format!("{}:{}", self.cfg.bot_id, group_id)),
        };
        let session = ResidentSession::spawn(&config, group_id, Arc::new(DbStepReader))
            .await
            .map_err(|e| e.to_string())?;
        let session = Arc::new(session);
        for action in session.drain_audit() {
            self.audit(group_id, &action).await;
        }
        let mut groups = self.groups.lock().await;
        groups.entry(group_id.to_string()).or_default().session = Some(Arc::clone(&session));
        Ok(session)
    }

    /// 关掉会话；`clear_conversation` 时连存档的会话 id 一并清掉，
    /// 下一轮必然是全新会话（轮换与弃会话都走这里）。
    pub async fn drop_session(&self, group_id: &str, clear_conversation: bool) {
        let session = {
            let mut groups = self.groups.lock().await;
            groups.get_mut(group_id).and_then(|g| g.session.take())
        };
        if let Some(session) = session {
            session.close().await;
            for action in session.drain_audit() {
                self.audit(group_id, &action).await;
            }
        }
        if clear_conversation {
            let _ = self
                .store
                .set_conversation_id(&self.cfg.bot_id, group_id, None, crate::now_unix())
                .await;
        }
    }

    /// 会话看门狗：由 flush 循环周期调用（完整版的 sweep 在
    /// lite 只剩 tick 这一件真事）。
    pub async fn tick_all(&self, active: &HashSet<String>) {
        let sessions: Vec<(String, Arc<ResidentSession>)> = {
            let groups = self.groups.lock().await;
            groups
                .iter()
                .filter_map(|(g, s)| s.session.clone().map(|sess| (g.clone(), sess)))
                .collect()
        };
        for (group_id, session) in sessions {
            let _ = session.tick(active.contains(&group_id)).await;
            for action in session.drain_audit() {
                self.audit(&group_id, &action).await;
            }
        }
    }
}
