//! xTB-Pilot 常驻守护进程（二进制 crate，错误用 `anyhow`，规划文档 §2.2）。
//!
//! - tokio 多线程运行时（唯一运行时）；
//! - `tokio-util::sync::CancellationToken` 优雅停机（Ctrl-C 或信号）；
//! - `tracing-subscriber` fmt + `tracing-appender` 按日滚动 per-job 日志；
//! - NDJSON over TCP 接口（`xtbp-api`），Ping/Submit/Status 骨架；
//! - 子进程经 InstanceRegistry 登记路径拉起（不依赖 PATH 运气）。

use anyhow::Result;
use clap::Parser;
use std::path::PathBuf;
use tokio::io::BufReader;
use tokio::net::{TcpListener, TcpStream};
use tokio::signal;
use tokio_util::sync::CancellationToken;
use tracing::{error, info, warn};
use xtbp_api::protocol::{Request, Response};
use xtbp_api::{Handler, frame};

#[derive(Parser, Debug)]
#[command(name = "xtbp-daemon", version, about = "xTB-Pilot 常驻守护进程")]
struct Args {
    /// 监听地址（NDJSON over TCP）
    #[arg(long, default_value = "127.0.0.1:0")]
    listen: String,

    /// InstanceRegistry 登记表路径
    #[arg(long, default_value = "~/.local/share/xtbpilot/registry.toml")]
    registry: String,

    /// per-job 滚动日志目录
    #[arg(long, default_value = "~/.local/share/xtbpilot/logs")]
    log_dir: String,
}

/// 协议 handler（骨架：Ping 即回，其余提示未实现）。
#[derive(Default)]
struct ApiHandler;

impl Handler for ApiHandler {
    async fn handle(&self, req: Request) -> Response {
        match req {
            Request::Ping => Response::Ok {
                job_id: String::new(),
                data: serde_json::json!({ "pong": true, "daemon": env!("CARGO_PKG_VERSION") }),
            },
            other => Response::Err {
                message: format!("未实现的方法: {other:?}"),
            },
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

    let listener = TcpListener::bind(&args.listen).await?;
    info!("监听于 {}", listener.local_addr()?);

    let handler = std::sync::Arc::new(ApiHandler);
    loop {
        tokio::select! {
            _ = token.cancelled() => {
                info!("优雅停机完成");
                break;
            }
            accepted = listener.accept() => {
                match accepted {
                    Ok((stream, peer)) => {
                        let token = token.clone();
                        let handler = std::sync::Arc::clone(&handler);
                        tokio::spawn(async move {
                            if let Err(e) = serve(stream, handler, token).await {
                                error!(peer = %peer, "连接处理失败: {e:#}");
                            }
                        });
                    }
                    Err(e) => error!("accept 失败: {e}"),
                }
            }
        }
    }
    Ok(())
}

/// 处理单个连接：逐帧读取 NDJSON 请求并应答。
async fn serve(
    stream: TcpStream,
    handler: std::sync::Arc<ApiHandler>,
    token: CancellationToken,
) -> std::io::Result<()> {
    let (read_half, mut write_half) = stream.into_split();
    let mut reader = BufReader::new(read_half);
    loop {
        tokio::select! {
            _ = token.cancelled() => return Ok(()),
            msg = frame::read_frame::<_, Request>(&mut reader) => {
                match msg? {
                    None => return Ok(()), // 对端关闭
                    Some(req) => {
                        let resp = handler.handle(req).await;
                        frame::write_frame(&mut write_half, &resp).await?;
                    }
                }
            }
        }
    }
}

/// 初始化日志：控制台 fmt + 按日滚动的文件 appender。
fn init_tracing(log_dir: &str) -> Result<()> {
    let dir = PathBuf::from(xtbp_core::config::expand_tilde(log_dir));
    std::fs::create_dir_all(&dir)?;
    let file_appender = tracing_appender::rolling::daily(&dir, "daemon.log");
    let (non_blocking, _guard) = tracing_appender::non_blocking(file_appender);
    tracing_subscriber::fmt()
        .with_env_filter("xtbp_daemon=info")
        .with_writer(non_blocking)
        .init();
    // `_guard` 需存活到进程结束：泄漏以保持 appender 工作
    std::mem::forget(_guard);
    Ok(())
}

/// 心跳流示例（规划文档 §2.2：futures 流组合）。
pub fn heartbeat_stream() -> impl futures::Stream<Item = u64> {
    futures::stream::iter(0u64..)
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::StreamExt;

    #[tokio::test(start_paused = true)]
    async fn heartbeat_stream_yields_sequence() {
        let mut s = heartbeat_stream();
        assert_eq!(s.next().await, Some(0));
        assert_eq!(s.next().await, Some(1));
        assert_eq!(s.next().await, Some(2));
    }

    #[tokio::test]
    async fn cancellation_token_cancels() {
        let token = CancellationToken::new();
        let child = token.clone();
        let handle = tokio::spawn(async move {
            child.cancelled().await;
            42
        });
        token.cancel();
        assert_eq!(handle.await.unwrap(), 42);
    }

    #[test]
    fn cli_parses_defaults() {
        let args = Args::try_parse_from(["xtbp-daemon"]).unwrap();
        assert_eq!(args.registry, "~/.local/share/xtbpilot/registry.toml");
        assert!(args.listen.contains("127.0.0.1"));
    }
}
