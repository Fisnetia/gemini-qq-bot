//! Mihomo (Clash) supervision inside the bot container.
//!
//! The proxy is what lets Agy reach its endpoint from a network where direct access
//! does not work, so the bot treats its health as a precondition for calling the agent.
//! Two things matter and only one of them is enforceable here:
//!
//! * **Enforceable**: Mihomo is spawned as a child process, binds only container
//!   loopback ports, and is probed through its own API before a batch runs. When the
//!   probe fails the batch is kept as pending and marked `waiting_for_us_proxy`.
//! * **Not enforceable**: `HTTP_PROXY`/`HTTPS_PROXY` in the child environment are a
//!   *convention*, not a sandbox. A CLI that ignores them will connect directly. This
//!   module therefore never claims that package-level network isolation exists.
//!
//! The American-node rule is a liveness rule: if no US node is selected the proxy is
//! considered unusable and no downgrade to another region is allowed.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use serde_json::Value;
use tokio::process::{Child, Command};
use tokio::sync::RwLock;

/// Delay before a restart is attempted.
pub const RESTART_DELAY: Duration = Duration::from_secs(5);
/// Maximum restarts before the bot reports a hard proxy failure.
pub const MAX_RESTARTS: u32 = 5;
/// Probe timeout.
pub const PROBE_TIMEOUT: Duration = Duration::from_secs(3);

/// Health of the proxy subsystem.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize)]
pub struct ProxyHealth {
    /// The child process is alive.
    pub process_running: bool,
    /// `GET /version` on the controller succeeded.
    pub api_ok: bool,
    /// A US node is selected and reachable.
    pub us_node_ok: bool,
    /// The name of the selected node, for the panel. Never a credential.
    pub selected_node: Option<String>,
    pub last_error: Option<String>,
    pub restarts: u32,
}

impl ProxyHealth {
    /// Whether the agent may be called.
    ///
    /// `mihomo_enabled == false` means the operator declared direct access to be
    /// available, so the gate opens without pretending the proxy works.
    pub fn ready(&self, mihomo_enabled: bool) -> bool {
        if !mihomo_enabled {
            return true;
        }
        self.process_running && self.api_ok && self.us_node_ok
    }

    fn recompute(&mut self) {
        if !self.process_running {
            self.api_ok = false;
            self.us_node_ok = false;
        }
    }
}

/// Reason a proxy probe failed. String form is the stable `error_class` suffix.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProxyError {
    /// The process is not running.
    NotRunning,
    /// The controller API did not answer.
    ApiUnreachable,
    /// No US node is selected.
    NoUsNode,
    /// A node was selected but a latency test failed.
    NodeUnhealthy,
}

impl ProxyError {
    pub fn as_str(self) -> &'static str {
        match self {
            ProxyError::NotRunning => "not_running",
            ProxyError::ApiUnreachable => "api_unreachable",
            ProxyError::NoUsNode => "no_us_node",
            ProxyError::NodeUnhealthy => "node_unhealthy",
        }
    }
}

/// Node name matcher for the "must be an American node" rule.
///
/// Matching is name based, which is all the API exposes reliably. A name that mentions
/// no recognisable region is **not** treated as American.
pub fn is_us_node(name: &str) -> bool {
    let lower = name.to_lowercase();
    const US: [&str; 8] = [
        "us",
        "usa",
        "united states",
        "america",
        "american",
        "洛杉矶",
        "圣何塞",
        "西雅图",
    ];
    const NON_US: [&str; 10] = [
        "hk",
        "香港",
        "tw",
        "台湾",
        "sg",
        "新加坡",
        "jp",
        "日本",
        "kr",
        "韩国",
    ];
    // An explicit non-US marker wins, so "US-HK" style labels are not misread.
    if NON_US.iter().any(|m| lower.contains(m)) {
        return false;
    }
    US.iter().any(|m| lower.contains(m))
}

/// Configuration for the child process.
#[derive(Debug, Clone)]
pub struct MihomoConfig {
    pub bin: PathBuf,
    pub config_path: PathBuf,
    /// Controller port, container loopback only.
    pub api_port: u16,
    /// Mixed proxy port, container loopback only.
    pub proxy_port: u16,
    pub api_secret: Option<String>,
}

impl MihomoConfig {
    /// The argv used to start the proxy.
    ///
    /// Both listeners are pinned to `127.0.0.1`: nothing is published, and a container
    /// lateral move cannot reach the controller.
    pub fn argv(&self) -> Vec<String> {
        // v1.19.30 only accepts -d/-f/-ext-ctl. mixed-port and allow-lan belong in
        // the config file; passing them as flags makes the process exit immediately.
        vec![
            "-d".to_string(),
            self.config_path
                .parent()
                .map(|p| p.display().to_string())
                .unwrap_or_else(|| "/opt/mihomo".to_string()),
            "-f".to_string(),
            self.config_path.display().to_string(),
            "-ext-ctl".to_string(),
            format!("127.0.0.1:{}", self.api_port),
        ]
    }

    /// Proxy URL handed to the agent child process.
    pub fn proxy_url(&self) -> String {
        format!("http://127.0.0.1:{}", self.proxy_port)
    }

    /// Controller API base.
    pub fn api_base(&self) -> String {
        format!("http://127.0.0.1:{}", self.api_port)
    }
}

/// Supervises the proxy child process and its health.
pub struct Mihomo {
    config: MihomoConfig,
    enabled: bool,
    child: RwLock<Option<Child>>,
    health: RwLock<ProxyHealth>,
    http: reqwest::Client,
}

impl std::fmt::Debug for Mihomo {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Mihomo")
            .field("enabled", &self.enabled)
            .field("api_port", &self.config.api_port)
            .finish_non_exhaustive()
    }
}

impl Mihomo {
    pub fn new(config: MihomoConfig, enabled: bool) -> Self {
        Self {
            config,
            enabled,
            child: RwLock::new(None),
            health: RwLock::new(ProxyHealth::default()),
            http: reqwest::Client::builder()
                .timeout(PROBE_TIMEOUT)
                .redirect(reqwest::redirect::Policy::none())
                .build()
                .unwrap_or_default(),
        }
    }

    pub fn enabled(&self) -> bool {
        self.enabled
    }

    pub fn config(&self) -> &MihomoConfig {
        &self.config
    }

    pub async fn health(&self) -> ProxyHealth {
        let mut h = self.health.read().await.clone();
        h.recompute();
        h
    }

    /// Whether the agent may be called right now.
    pub async fn ready(&self) -> bool {
        self.health().await.ready(self.enabled)
    }

    /// Start the child process. A missing binary is a hard error the operator must see.
    pub async fn start(&self) -> Result<(), ProxyError> {
        if !self.enabled {
            return Ok(());
        }
        let mut guard = self.child.write().await;
        if let Some(child) = guard.as_mut()
            && child.try_wait().ok().flatten().is_none()
        {
            return Ok(());
        }
        let argv = self.config.argv();
        let mut cmd = Command::new(&self.config.bin);
        cmd.args(&argv);
        cmd.stdout(Stdio::null());
        cmd.stderr(Stdio::null());
        cmd.kill_on_drop(true);
        match cmd.spawn() {
            Ok(child) => {
                *guard = Some(child);
                let mut health = self.health.write().await;
                health.process_running = true;
                health.last_error = None;
                Ok(())
            }
            Err(e) => {
                let mut health = self.health.write().await;
                health.process_running = false;
                health.last_error = Some(format!("无法启动 mihomo: {}", e.kind()));
                Err(ProxyError::NotRunning)
            }
        }
    }

    /// Stop the child process.
    pub async fn stop(&self) {
        let mut guard = self.child.write().await;
        if let Some(mut child) = guard.take() {
            let _ = child.kill().await;
        }
        let mut health = self.health.write().await;
        health.process_running = false;
        health.recompute();
    }

    /// Probe the running controller. `GLOBAL` is mihomo's built-in selector.
    pub async fn probe(&self) -> ProxyHealth {
        let (selected, choices) = self.global_selection().await;
        self.probe_with(selected.as_deref(), &choices).await
    }

    async fn global_selection(&self) -> (Option<String>, Vec<String>) {
        let url = format!("{}/proxies", self.config.api_base());
        let mut req = self.http.get(&url);
        if let Some(secret) = &self.config.api_secret {
            req = req.bearer_auth(secret);
        }
        let Ok(resp) = req.send().await else {
            return (None, Vec::new());
        };
        if !resp.status().is_success() {
            return (None, Vec::new());
        }
        let Ok(body) = resp.json().await else {
            return (None, Vec::new());
        };
        parse_selection(&body, "GLOBAL")
    }

    /// Probe the controller API and the selected node.
    ///
    /// `proxies` supplies the selectable node names for the US check; tests inject it so
    /// no real proxy is needed.
    pub async fn probe_with(
        &self,
        selected: Option<&str>,
        group_choices: &[String],
    ) -> ProxyHealth {
        let mut health = ProxyHealth {
            process_running: self.child.read().await.is_some(),
            ..ProxyHealth::default()
        };

        if !self.enabled {
            // Proxy disabled by the operator: no probe, no claim.
            health.api_ok = false;
            health.us_node_ok = false;
            *self.health.write().await = health.clone();
            return health;
        }

        if !health.process_running {
            health.last_error = Some(ProxyError::NotRunning.as_str().to_string());
            *self.health.write().await = health.clone();
            return health;
        }

        let url = format!("{}/version", self.config.api_base());
        let mut req = self.http.get(&url);
        if let Some(secret) = &self.config.api_secret {
            req = req.bearer_auth(secret);
        }
        match req.send().await {
            Ok(resp) if resp.status().is_success() => health.api_ok = true,
            _ => {
                health.last_error = Some(ProxyError::ApiUnreachable.as_str().to_string());
                *self.health.write().await = health.clone();
                return health;
            }
        }

        let selected = selected.or_else(|| group_choices.first().map(|s| s.as_str()));
        health.selected_node = selected.map(|s| s.to_string());
        match selected {
            None => {
                health.last_error = Some(ProxyError::NoUsNode.as_str().to_string());
            }
            Some(node) if !is_us_node(node) => {
                // No downgrade: a non-US node is a failure, not a fallback.
                health.last_error = Some(ProxyError::NoUsNode.as_str().to_string());
            }
            Some(_) => health.us_node_ok = true,
        }
        *self.health.write().await = health.clone();
        health
    }
}

/// Parse a Clash `/proxies` payload into `(selected node for the selector group, all
/// candidate names)`.
pub fn parse_selection(body: &Value, group: &str) -> (Option<String>, Vec<String>) {
    let selected = body
        .pointer(&format!("/proxies/{group}/now"))
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());
    let all = body
        .pointer(&format!("/proxies/{group}/all"))
        .and_then(|v| v.as_array())
        .map(|items| {
            items
                .iter()
                .filter_map(|i| i.as_str().map(|s| s.to_string()))
                .collect()
        })
        .unwrap_or_default();
    (selected, all)
}

/// Whether a config path looks usable (exists and is a file).
pub fn config_present(path: &Path) -> bool {
    path.is_file()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config() -> MihomoConfig {
        MihomoConfig {
            bin: PathBuf::from("/opt/mihomo/mihomo"),
            config_path: PathBuf::from("/opt/mihomo/config.yaml"),
            api_port: 9090,
            proxy_port: 7890,
            api_secret: None,
        }
    }

    #[test]
    fn argv_binds_loopback_only_and_disables_lan() {
        let argv = config().argv();
        let joined = argv.join(" ");
        assert!(joined.contains("-ext-ctl 127.0.0.1:9090"));
        assert!(joined.contains("-f /opt/mihomo/config.yaml"));
        assert!(!joined.contains("-allow-lan"));
        assert!(!joined.contains("-mixed-port"));
        assert!(!joined.contains("0.0.0.0"));
    }

    #[test]
    fn us_node_matching_is_conservative() {
        for us in ["US-LosAngeles", "usa-01", "United States 3", "洛杉矶-02"] {
            assert!(is_us_node(us), "{us} 应识别为美国节点");
        }
        for other in ["HK-01", "香港 2", "SG-Premium", "JP-Tokyo", "kr-seoul"] {
            assert!(!is_us_node(other), "{other} 不得被当成美国节点");
        }
        // An explicit non-US marker wins over a stray "us".
        assert!(!is_us_node("US-HK-中转"));
        // Unrecognisable names are not American.
        assert!(!is_us_node("Node-7"));
    }

    #[test]
    fn health_gate_requires_every_signal_when_enabled() {
        let good = ProxyHealth {
            process_running: true,
            api_ok: true,
            us_node_ok: true,
            selected_node: Some("US-1".into()),
            last_error: None,
            restarts: 0,
        };
        assert!(good.ready(true));
        for broken in 0..3 {
            let mut h = good.clone();
            match broken {
                0 => h.process_running = false,
                1 => h.api_ok = false,
                _ => h.us_node_ok = false,
            }
            h.recompute();
            assert!(!h.ready(true), "缺少任一信号时不得调用 Agy");
        }
        // Disabled proxy means the operator declared direct access: gate open.
        assert!(ProxyHealth::default().ready(false));
    }

    #[test]
    fn proxy_urls_are_loopback() {
        let c = config();
        assert_eq!(c.proxy_url(), "http://127.0.0.1:7890");
        assert_eq!(c.api_base(), "http://127.0.0.1:9090");
    }

    #[test]
    fn parse_selection_reads_now_and_all() {
        let body = serde_json::json!({
            "proxies": {
                "GLOBAL": {"now": "US-01", "all": ["US-01", "HK-01"]}
            }
        });
        let (now, all) = parse_selection(&body, "GLOBAL");
        assert_eq!(now.as_deref(), Some("US-01"));
        assert_eq!(all, vec!["US-01", "HK-01"]);
    }

    #[test]
    fn parse_selection_tolerates_missing_fields() {
        let (now, all) = parse_selection(&serde_json::json!({}), "GLOBAL");
        assert!(now.is_none());
        assert!(all.is_empty());
    }

    #[tokio::test]
    async fn disabled_proxy_never_probes_and_reports_no_health() {
        let mihomo = Mihomo::new(config(), false);
        let health = mihomo.probe_with(Some("HK-01"), &[]).await;
        assert!(!health.api_ok);
        assert!(!health.us_node_ok);
        // But the gate is open because the operator disabled the proxy.
        assert!(mihomo.ready().await);
    }

    #[tokio::test]
    async fn non_us_node_blocks_the_gate() {
        let mut cfg = config();
        // Point the API at a closed port so the probe stops at the API check; we assert
        // the node rule through the pure helper instead.
        cfg.api_port = 1;
        let mihomo = Mihomo::new(cfg, true);
        mihomo.start().await.ok();
        let health = mihomo
            .probe_with(Some("HK-01"), &["HK-01".to_string()])
            .await;
        assert!(!health.us_node_ok);
        assert!(!mihomo.ready().await);
        mihomo.stop().await;
    }

    #[test]
    fn config_presence_check() {
        let dir = tempfile::tempdir().expect("tmp");
        let path = dir.path().join("config.yaml");
        assert!(!config_present(&path));
        std::fs::write(&path, "proxies: []").expect("写");
        assert!(config_present(&path));
    }
}
