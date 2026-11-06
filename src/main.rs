//! Entry point.
//!
//! Startup is intentionally boring: load and validate config, build the app, serve. Any
//! configuration problem is fatal here — the process must not run with a weak internal
//! token or an unset owner id. Unverified *external contracts* are not fatal: they show
//! up as readiness blockers instead, because refusing to boot would hide the very
//! diagnostics the operator needs.

use std::process::ExitCode;

use gemini_qq_bot::app::App;
use gemini_qq_bot::config::Config;
use gemini_qq_bot::mcp;
use gemini_qq_bot::store::sqlite::SqliteStore;
use tracing_subscriber::EnvFilter;

/// Serve the read-only memory tool over MCP stdio.
///
/// The group scope is resolved per call: the server asks the client which directories
/// this turn works in, then reads `scope.json` from that workspace. It is never taken
/// from the request arguments, so the model cannot address another group, and the shared
/// config file is never rewritten, so two concurrent groups cannot collide.
///
/// stdout carries protocol frames only; every diagnostic goes to stderr, because a
/// stray line on stdout would corrupt the transport.
async fn serve_memory_mcp() -> ExitCode {
    let db_path = std::env::var("GEMINI_MEMORY_DB").unwrap_or_else(|_| "/data/bot.sqlite3".into());
    let pool =
        match gemini_qq_bot::store::migrations::open_and_migrate(std::path::Path::new(&db_path))
            .await
        {
            Ok(p) => p,
            Err(e) => {
                eprintln!("打开记忆库失败: {e}");
                return ExitCode::from(3);
            }
        };
    let store = std::sync::Arc::new(SqliteStore::new(pool));

    let session = std::sync::Arc::new(mcp::Session::new());
    match session.run(store).await {
        Ok(()) | Err(mcp::SessionError::Closed) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("MCP 会话中断: {err:?}");
            ExitCode::from(4)
        }
    }
}

/// Minimal TCP probe used by the container health check.
///
/// It deliberately does **not** authenticate: the only thing a liveness probe should
/// assert is that the admin listener accepts connections. Reading the internal token
/// to call `/internal/v1/status` would copy a secret into a process that has no other
/// reason to hold it. Deep checks stay behind the token, where an operator inspects
/// them.
async fn health_probe() -> ExitCode {
    let bind = std::env::var("ADMIN_BIND").unwrap_or_else(|_| "0.0.0.0:8080".to_string());
    let target = if bind.starts_with("0.0.0.0:") {
        bind.replacen("0.0.0.0", "127.0.0.1", 1)
    } else if bind.starts_with("[::]:") {
        bind.replacen("[::]", "[::1]", 1)
    } else {
        bind
    };
    match tokio::time::timeout(
        std::time::Duration::from_secs(3),
        tokio::net::TcpStream::connect(&target),
    )
    .await
    {
        Ok(Ok(_)) => ExitCode::SUCCESS,
        _ => ExitCode::from(1),
    }
}

#[tokio::main]
async fn main() -> ExitCode {
    if std::env::args().any(|a| a == "--health-probe") {
        return health_probe().await;
    }
    if std::env::args().any(|a| a == "--mcp-memory") {
        return serve_memory_mcp().await;
    }

    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        // Never log request bodies or headers: message text and tokens live there.
        .with_target(false)
        .init();

    let config = match Config::from_env() {
        Ok(c) => c,
        Err(e) => {
            tracing::error!(error = %e, "配置无效，拒绝启动");
            return ExitCode::from(2);
        }
    };

    tracing::info!(
        bot_id = %config.bot_id,
        proxy_enabled = config.mihomo_enabled,
        aux_enabled = config.aux_enabled,
        image_transport_verified = gemini_qq_bot::config::image_transport_verified(),
        "启动 gemini-qq-bot"
    );

    let app = match App::build(config).await {
        Ok(a) => std::sync::Arc::new(a),
        Err(e) => {
            tracing::error!(error = %e, "初始化失败");
            return ExitCode::from(3);
        }
    };

    let snapshot = app.health.snapshot().await;
    if snapshot.production_ready {
        tracing::info!("所有依赖已验证，生产就绪");
    } else {
        tracing::warn!(
            blockers = snapshot.blockers.len(),
            "未达到生产就绪，回复链路受闸门限制"
        );
        for blocker in &snapshot.blockers {
            tracing::warn!(blocker = %blocker, "阻塞项");
        }
    }

    if let Err(e) = app.serve().await {
        tracing::error!(error = %e, "服务退出");
        return ExitCode::from(4);
    }
    ExitCode::SUCCESS
}
