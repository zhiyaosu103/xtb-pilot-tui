//! xTB-Pilot 常驻守护进程（二进制 crate，错误用 `anyhow`，规划文档 §2.2）。
//!
//! - tokio 多线程运行时（唯一运行时）；
//! - `tokio-util::sync::CancellationToken` 优雅停机（Ctrl-C 或信号）；
//! - `tracing-subscriber` fmt + `tracing-appender` 按日滚动 per-job 日志；
//! - NDJSON over TCP 接口（`xtbp-api`），骨架 handler（后续里程碑装配
//!   调度器/工作流/RDKit helper）。

use anyhow::Result;
use clap::Parser;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::signal;
use tokio_util::sync::CancellationToken;
use tracing::{error, info, warn};
use xtbp_api::protocol::{ApiError, ApiHandler, methods};
use xtbp_api::{EventBus, ServerConfig, serve_tcp};

#[derive(Parser, Debug)]
#[command(name = "xtbp-daemon", version, about = "xTB-Pilot 常驻守护进程")]
struct Args {
    /// 监听地址（NDJSON over TCP，agent 接口）
    #[arg(long, default_value = "127.0.0.1:7700")]
    listen: String,

    /// TUI 的 UDS 路径（空 = 不启用）
    #[arg(long, default_value = "~/.local/share/xtbpilot/xtbp.sock")]
    uds: String,

    /// 鉴权 token（空 = 不鉴权，仅限开发）
    #[arg(long, default_value = "")]
    token: String,

    /// InstanceRegistry 登记表路径
    #[arg(long, default_value = "~/.local/share/xtbpilot/registry.toml")]
    registry: String,

    /// per-job 滚动日志目录
    #[arg(long, default_value = "~/.local/share/xtbpilot/logs")]
    log_dir: String,
}

/// 协议 handler（骨架：sys.health 即回，其余提示未实现）。
#[derive(Default)]
struct ApiHandlerImpl;

impl ApiHandler for ApiHandlerImpl {
    async fn handle(
        &self,
        method: &str,
        _params: serde_json::Value,
    ) -> std::result::Result<serde_json::Value, ApiError> {
        match method {
            methods::SYS_HEALTH => Ok(serde_json::json!({
                "ok": true,
                "daemon": env!("CARGO_PKG_VERSION"),
            })),
            other => Err(ApiError::method_not_found(other)),
        }
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();
    init_tracing(&args.log_dir)?;
    info!(listen = %args.listen, registry = %args.registry, "daemon 启动");

    // 优雅停机：Ctrl-C → cancel token
    let token = CancellationToken::new();
    {
        let token = token.clone();
        tokio::spawn(async move {
            if let Err(e) = signal::ctrl_c().await {
                warn!("ctrl_c 监听失败: {e}");
            }
            info!("收到中断信号，开始优雅停机");
            token.cancel();
        });
    }

    let listener = tokio::net::TcpListener::bind(&args.listen).await?;
    info!("TCP 监听于 {}", listener.local_addr()?);

    let bus = EventBus::new(4096);
    let config = Arc::new(ServerConfig {
        token: args.token,
        event_bus: bus,
    });
    let handler = Arc::new(ApiHandlerImpl);

    let tcp_task = tokio::spawn(serve_tcp(
        listener,
        Arc::clone(&handler),
        Arc::clone(&config),
        token.clone(),
    ));

    // UDS（TUI 接口）：路径展开 + 清理陈旧 socket
    let uds_path = PathBuf::from(xtbp_core::config::expand_tilde(&args.uds));
    let mut uds_task = None;
    if let Some(parent) = uds_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    if uds_path.exists() {
        std::fs::remove_file(&uds_path)?;
    }
    match tokio::net::UnixListener::bind(&uds_path) {
        Ok(uds_listener) => {
            info!("UDS 监听于 {}", uds_path.display());
            uds_task = Some(tokio::spawn(xtbp_api::serve_uds(
                uds_listener,
                handler,
                config,
                token.clone(),
            )));
        }
        Err(e) => warn!("UDS 绑定失败（TUI 将不可用）: {e}"),
    }

    token.cancelled().await;
    info!("优雅停机完成");
    if let Some(t) = uds_task {
        let _ = t.await;
    }
    if let Err(e) = tcp_task.await {
        error!("TCP 服务退出异常: {e}");
    }
    Ok(())
}

/// 初始化日志：控制台 fmt + 按日滚动的文件 appender。
fn init_tracing(log_dir: &str) -> Result<()> {
    let dir = PathBuf::from(xtbp_core::config::expand_tilde(log_dir));
    std::fs::create_dir_all(&dir)?;
    let file_appender = tracing_appender::rolling::daily(&dir, "daemon.log");
    let (non_blocking, _guard) = tracing_appender::non_blocking(file_appender);
    tracing_subscriber::fmt()
        .with_env_filter("xtbp_daemon=info,xtbp_api=info")
        .with_writer(non_blocking)
        .init();
    // `_guard` 需存活到进程结束：泄漏以保持 appender 工作
    std::mem::forget(_guard);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cli_parses_defaults() {
        let args = Args::try_parse_from(["xtbp-daemon"]).unwrap();
        assert_eq!(args.registry, "~/.local/share/xtbpilot/registry.toml");
        assert!(args.listen.contains("7700"));
        assert!(args.uds.contains("xtbp.sock"));
    }
}
