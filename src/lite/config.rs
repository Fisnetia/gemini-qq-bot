//! lite 配置：全部来自环境变量，启动时校验。
//!
//! 与完整版的区别是刻意的：完整版是 TOML 文件 + env 覆写 + 管理台
//! 热生效三层，基座版只有 env 一层——基座的运维面越小越好，改配置
//! 就是改 env 重启。必填项缺失或格式不对直接报错退出，不带病运行
//! （口径与完整版 main 的「配置无效，拒绝启动」一致）。

use std::path::PathBuf;

/// lite 的全部配置。字段含义与默认值以 docs/LITE.md 的配置表为准，
/// 两处必须同步改。
#[derive(Debug, Clone)]
pub struct LiteConfig {
    /// 机器人 QQ 号（纯数字）。
    pub bot_id: String,
    /// 主人 QQ 号（纯数字）：发言视为触发，渲染时 role=owner。
    pub owner_id: String,
    /// OneBot 正向 WebSocket 地址。
    pub ws_url: String,
    /// OneBot 访问令牌（可空）。
    pub access_token: Option<String>,
    /// 群白名单：只有这些群的消息会被处理。
    pub groups: Vec<String>,
    /// 白名单群内是否全量触发（默认否：只应答触发消息）。
    pub respond_all: bool,
    /// 呼唤词：正文包含任一词即视为触发（不区分大小写）。
    pub call_names: Vec<String>,
    /// SQLite 库路径（与完整版共用同一套 schema，默认独立文件）。
    pub db_path: PathBuf,
    /// 数据目录：心跳文件与每群工作区放这里（默认取 db 所在目录）。
    pub data_dir: PathBuf,
    /// agy 二进制路径。
    pub agy_bin: PathBuf,
    /// agy 的 HOME（OAuth 与设置所在）。
    pub agy_home: PathBuf,
    /// 模型 id，原样透传给 agy。
    pub agy_model: String,
    /// 人设文本：写进每群工作区的 AGENTS.md（人格卡部分）。
    pub persona: String,
    /// 攒批静默窗（秒）：最后一条消息后静默这么久才成批。
    pub quiet_s: i64,
    /// 攒批最大年龄（秒）：第一条消息等太久也会成批。
    pub max_age_s: i64,
    /// 会话轮换轮次：一个 agy 会话跑满这么多轮就换新会话。
    pub turn_limit: i64,
    /// 全局小时保险丝：一小时内最多开这么多轮（只熔断，不分档）。
    pub fuse_per_hour: usize,
    /// 重生退避开关（默认开）。
    pub respawn_backoff: bool,
    /// 出站凭据过滤的已知秘密值：命中即打码。
    pub secrets: Vec<String>,
}

fn env(key: &str) -> Option<String> {
    std::env::var(key).ok().filter(|v| !v.trim().is_empty())
}

fn env_or(key: &str, default: &str) -> String {
    env(key).unwrap_or_else(|| default.to_string())
}

fn parse_i64(key: &str, default: i64) -> Result<i64, String> {
    match env(key) {
        None => Ok(default),
        Some(raw) => raw
            .trim()
            .parse::<i64>()
            .map_err(|_| format!("{key} 必须是整数，收到 {raw:?}")),
    }
}

fn is_digits(s: &str) -> bool {
    !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit())
}

impl LiteConfig {
    pub fn from_env() -> Result<Self, String> {
        let bot_id = env("LITE_BOT_ID").ok_or("缺少 LITE_BOT_ID（机器人 QQ 号）")?;
        if !is_digits(&bot_id) {
            return Err(format!("LITE_BOT_ID 必须是纯数字，收到 {bot_id:?}"));
        }
        let owner_id = env("LITE_OWNER_ID").ok_or("缺少 LITE_OWNER_ID（主人 QQ 号）")?;
        if !is_digits(&owner_id) {
            return Err(format!("LITE_OWNER_ID 必须是纯数字，收到 {owner_id:?}"));
        }
        let ws_url = env("ONEBOT_WS_URL").ok_or("缺少 ONEBOT_WS_URL（OneBot 正向 WS 地址）")?;
        if !ws_url.starts_with("ws://") && !ws_url.starts_with("wss://") {
            return Err(format!(
                "ONEBOT_WS_URL 必须以 ws:// 或 wss:// 开头，收到 {ws_url:?}"
            ));
        }
        let groups: Vec<String> = env("LITE_GROUPS")
            .ok_or("缺少 LITE_GROUPS（群白名单，逗号分隔）")?
            .split(',')
            .map(|g| g.trim().to_string())
            .filter(|g| !g.is_empty())
            .collect();
        if groups.is_empty() || !groups.iter().all(|g| is_digits(g)) {
            return Err("LITE_GROUPS 必须是非空的纯数字群号列表（逗号分隔）".to_string());
        }
        let call_names: Vec<String> = env_or("LITE_CALL_NAMES", "Gemini")
            .split(',')
            .map(|n| n.trim().to_string())
            .filter(|n| !n.is_empty())
            .collect();

        let db_path = PathBuf::from(env_or("LITE_DB_PATH", "./lite.sqlite3"));
        let data_dir = env("LITE_DATA_DIR")
            .map(PathBuf::from)
            .or_else(|| db_path.parent().map(|p| p.to_path_buf()))
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or_else(|| PathBuf::from("."));

        let persona = match env("LITE_PERSONA_FILE") {
            Some(path) => std::fs::read_to_string(&path)
                .map_err(|e| format!("读取 LITE_PERSONA_FILE {path} 失败: {e}"))?,
            None => include_str!("persona.txt").to_string(),
        };
        if persona.trim().is_empty() {
            return Err("人设文本为空".to_string());
        }

        let agy_home = env("AGY_HOME")
            .map(PathBuf::from)
            .or_else(|| std::env::var("HOME").ok().map(PathBuf::from))
            .ok_or("缺少 AGY_HOME 且无法从 HOME 推导")?;

        let mut secrets: Vec<String> = Vec::new();
        if let Some(token) = env("ONEBOT_ACCESS_TOKEN") {
            secrets.push(token);
        }
        if let Some(extra) = env("LITE_SECRETS") {
            secrets.extend(
                extra
                    .split(',')
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty()),
            );
        }

        Ok(Self {
            bot_id,
            owner_id,
            ws_url,
            access_token: env("ONEBOT_ACCESS_TOKEN"),
            groups,
            respond_all: env("LITE_RESPOND_ALL").is_some_and(|v| v.trim() == "1"),
            call_names,
            db_path,
            data_dir,
            agy_bin: PathBuf::from(env_or("AGY_BIN", "agy")),
            agy_home,
            agy_model: env_or("AGY_MODEL", crate::config::DEFAULT_AGY_MODEL),
            persona,
            quiet_s: parse_i64("LITE_QUIET_S", 5)?,
            max_age_s: parse_i64("LITE_MAX_AGE_S", 180)?,
            turn_limit: parse_i64("LITE_TURN_LIMIT", 35)?,
            fuse_per_hour: parse_i64("LITE_FUSE_PER_HOUR", 60)?.max(0) as usize,
            respawn_backoff: !env("LITE_RESPAWN_BACKOFF").is_some_and(|v| v.trim() == "0"),
            secrets,
        })
    }

    /// 每群的 agy 工作区目录（AGENTS.md 与 scope.json 在这里）。
    pub fn project_dir(&self, group_id: &str) -> PathBuf {
        self.data_dir.join("projects").join(group_id)
    }

    /// 心跳文件路径：serve 每 30 秒刷新，--health-probe 读它。
    pub fn heartbeat_path(&self) -> PathBuf {
        self.data_dir.join("lite-heartbeat")
    }
}

/// 只靠 env 推导心跳路径（--health-probe 用）：探活不该要求
/// 全部配置合法，数据目录的推导规则与 from_env 保持一致。
pub fn heartbeat_path_from_env() -> PathBuf {
    let data_dir = env("LITE_DATA_DIR")
        .map(PathBuf::from)
        .or_else(|| {
            PathBuf::from(env_or("LITE_DB_PATH", "./lite.sqlite3"))
                .parent()
                .map(|p| p.to_path_buf())
        })
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| PathBuf::from("."));
    data_dir.join("lite-heartbeat")
}
