//! gemini-qq-bot 基座版（lite）入口。
//!
//! 模式：无参 = 服务；`--status` = 只读自查；`--health-probe` =
//! 心跳探活（给容器 healthcheck 用）；`--mcp-lite` = 给 agy 子
//! 进程的 MCP stdio 服务（只提供 submit_reply，由部署方的
//! mcp_config.json 指向本二进制）。配置与部署见 docs/LITE.md。

use std::process::ExitCode;
use std::sync::Arc;

use gemini_qq_bot::lite::LiteApp;
use gemini_qq_bot::lite::config::LiteConfig;
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().collect();

    if args.iter().any(|a| a == "--health-probe") {
        // 探活只看心跳，不要求其余配置合法。
        let path = gemini_qq_bot::lite::config::heartbeat_path_from_env();
        let alive = std::fs::read_to_string(&path)
            .ok()
            .and_then(|raw| raw.trim().parse::<i64>().ok())
            .is_some_and(|ts| gemini_qq_bot::now_unix() - ts <= 90);
        return if alive {
            ExitCode::SUCCESS
        } else {
            ExitCode::from(1)
        };
    }
    if args.iter().any(|a| a == "--mcp-lite") {
        // stdout 是协议通道：不初始化日志，诊断只走 stderr。
        return gemini_qq_bot::lite::serve_mcp().await;
    }

    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .with_target(false)
        .init();

    let config = match LiteConfig::from_env() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("配置无效，拒绝启动: {e}");
            return ExitCode::from(2);
        }
    };

    if args.iter().any(|a| a == "--status") {
        return match gemini_qq_bot::lite::print_status(&config).await {
            Ok(text) => {
                print!("{text}");
                ExitCode::SUCCESS
            }
            Err(e) => {
                eprintln!("状态读取失败: {e}");
                ExitCode::from(3)
            }
        };
    }

    let app = match LiteApp::build(config).await {
        Ok(a) => Arc::new(a),
        Err(e) => {
            tracing::error!(error = %e, "初始化失败");
            return ExitCode::from(3);
        }
    };
    if let Err(e) = app.serve().await {
        tracing::error!(error = %e, "服务退出");
        return ExitCode::from(4);
    }
    ExitCode::SUCCESS
}
