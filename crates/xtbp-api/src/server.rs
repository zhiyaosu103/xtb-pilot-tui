//! 服务器：NDJSON JSON-RPC over TCP（agent，127.0.0.1:7700）与 UDS（TUI）。
//!
//! - 仅 loopback + 请求内 token 一级鉴权（§3.7）；
//! - `job.events` / `queue.events` 订阅后主动推流（通知帧无 id）；
//! - 每连接一个任务；daemon 传 `CancellationToken` 优雅停机。

use crate::EventBus;
use crate::frame::{read_frame, write_frame};
use crate::protocol::{
    ApiError, ApiHandler, JSONRPC_VERSION, Notification, Request, Response, methods,
};
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::io::{AsyncRead, AsyncWrite, BufReader};
use tokio::net::{TcpListener, UnixListener};
use tokio_util::sync::CancellationToken;
use tracing::{debug, warn};
use xtbp_core::job::JobEvent;

/// 服务器配置（TCP 与 UDS 共用）。
#[derive(Debug, Clone)]
pub struct ServerConfig {
    /// 本地 token（空 = 不鉴权，仅限 UDS 开发场景）。
    pub token: String,
    /// 事件总线（订阅推送源）。
    pub event_bus: EventBus,
}

/// TCP 接受循环（agent 接口）。
pub async fn serve_tcp<H>(
    listener: TcpListener,
    handler: Arc<H>,
    config: Arc<ServerConfig>,
    shutdown: CancellationToken,
) -> std::io::Result<()>
where
    H: ApiHandler,
{
    loop {
        tokio::select! {
            _ = shutdown.cancelled() => return Ok(()),
            accepted = listener.accept() => {
                match accepted {
                    Ok((stream, peer)) => {
                        if !is_loopback(peer) {
                            warn!(peer = %peer, "拒绝非 loopback 连接");
                            continue;
                        }
                        let handler = Arc::clone(&handler);
                        let config = Arc::clone(&config);
                        let shutdown = shutdown.clone();
                        tokio::spawn(async move {
                            handle_connection(stream, handler, config, shutdown).await;
                        });
                    }
                    Err(e) => {
                        debug!("accept 失败: {e}");
                    }
                }
            }
        }
    }
}

/// UDS 接受循环（TUI 接口）。
pub async fn serve_uds<H>(
    listener: UnixListener,
    handler: Arc<H>,
    config: Arc<ServerConfig>,
    shutdown: CancellationToken,
) -> std::io::Result<()>
where
    H: ApiHandler,
{
    loop {
        tokio::select! {
            _ = shutdown.cancelled() => return Ok(()),
            accepted = listener.accept() => {
                match accepted {
                    Ok((stream, _)) => {
                        let handler = Arc::clone(&handler);
                        let config = Arc::clone(&config);
                        let shutdown = shutdown.clone();
                        tokio::spawn(async move {
                            handle_connection(stream, handler, config, shutdown).await;
                        });
                    }
                    Err(e) => {
                        debug!("uds accept 失败: {e}");
                    }
                }
            }
        }
    }
}

/// 仅 loopback 校验（§3.7 鉴权一级）。
fn is_loopback(peer: SocketAddr) -> bool {
    peer.ip().is_loopback()
}

/// 单连接会话：鉴权、分发、订阅推送。
async fn handle_connection<S, H>(
    stream: S,
    handler: Arc<H>,
    config: Arc<ServerConfig>,
    shutdown: CancellationToken,
) where
    S: AsyncRead + AsyncWrite + Unpin,
    H: ApiHandler,
{
    let (read_half, mut write_half) = tokio::io::split(stream);
    let mut reader = BufReader::new(read_half);
    let mut subs = Subscriptions::default();
    let mut event_rx = config.event_bus.subscribe();

    loop {
        tokio::select! {
            _ = shutdown.cancelled() => return,
            ev = event_rx.recv() => {
                match ev {
                    Ok(event) => {
                        if let Some(channel) = subs.matches(&event) {
                            let notif = Notification {
                                jsonrpc: JSONRPC_VERSION.into(),
                                method: channel.into(),
                                params: serde_json::to_value(&event).unwrap_or(serde_json::Value::Null),
                            };
                            if write_frame(&mut write_half, &notif).await.is_err() {
                                return;
                            }
                        }
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                        warn!(skipped = n, "事件订阅落后，跳过一批");
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => return,
                }
            }
            frame = read_frame::<_, Request>(&mut reader) => {
                match frame {
                    Err(e) => {
                        debug!("帧解析失败: {e}");
                        return;
                    }
                    Ok(None) => return, // 对端关闭
                    Ok(Some(req)) => {
                        // 鉴权：请求内 token 一级
                        if !config.token.is_empty() && req.token.as_deref() != Some(config.token.as_str()) {
                            let resp = Response::err(req.id, ApiError::auth_failed());
                            if write_frame(&mut write_half, &resp).await.is_err() {
                                return;
                            }
                            return; // 鉴权失败即断开
                        }
                        if req.jsonrpc != JSONRPC_VERSION {
                            let resp = Response::err(
                                req.id,
                                ApiError::invalid_params("jsonrpc 必须为 \"2.0\""),
                            );
                            if write_frame(&mut write_half, &resp).await.is_err() {
                                return;
                            }
                            continue;
                        }
                        match req.method.as_str() {
                            methods::JOB_EVENTS | methods::QUEUE_EVENTS => {
                                let reply = handle_subscribe(req.method.as_str(), &req.params, &mut subs);
                                let resp = match reply {
                                    Ok(v) => Response::ok(req.id, v),
                                    Err(e) => Response::err(req.id, e),
                                };
                                if write_frame(&mut write_half, &resp).await.is_err() {
                                    return;
                                }
                            }
                            _ => {
                                if req.id.is_none() {
                                    continue; // 未知通知：忽略
                                }
                                let result = handler.handle(&req.method, req.params).await;
                                let resp = match result {
                                    Ok(v) => Response::ok(req.id, v),
                                    Err(e) => Response::err(req.id, e),
                                };
                                if write_frame(&mut write_half, &resp).await.is_err() {
                                    return;
                                }
                            }
                        }
                    }
                }
            }
        }
    }
}

/// 处理订阅/退订请求。
fn handle_subscribe(
    method: &str,
    params: &serde_json::Value,
    subs: &mut Subscriptions,
) -> std::result::Result<serde_json::Value, ApiError> {
    let subscribe = params
        .get("subscribe")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let job_id = params
        .get("job_id")
        .and_then(|v| v.as_str())
        .map(String::from);
    if method == methods::JOB_EVENTS {
        subs.job_events = subscribe.then_some(job_id);
    } else {
        subs.queue_events = subscribe;
    }
    Ok(serde_json::json!({
        "subscribed": subscribe,
        "method": method,
    }))
}

/// 连接级订阅状态。
#[derive(Debug, Clone, Default)]
struct Subscriptions {
    /// None = 未订阅；Some(None) = 全部 job 事件；Some(Some(id)) = 过滤。
    job_events: Option<Option<String>>,
    /// 队列深度推送。
    queue_events: bool,
}

impl Subscriptions {
    /// 事件是否命中订阅；命中则返回推送通道名。
    fn matches(&self, event: &JobEvent) -> Option<&'static str> {
        match event {
            JobEvent::QueueDepth { .. } => self.queue_events.then_some(methods::QUEUE_EVENTS),
            other => {
                let job_id = job_event_id(other);
                match &self.job_events {
                    None => None,
                    Some(None) => Some(methods::JOB_EVENTS),
                    Some(Some(id)) => (job_id == Some(id.as_str())).then_some(methods::JOB_EVENTS),
                }
            }
        }
    }
}

/// 从事件中提取 job_id。
fn job_event_id(event: &JobEvent) -> Option<&str> {
    match event {
        JobEvent::Queued { job_id }
        | JobEvent::Started { job_id }
        | JobEvent::Output { job_id, .. }
        | JobEvent::Status { job_id, .. }
        | JobEvent::Finished { job_id, .. } => Some(job_id),
        JobEvent::QueueDepth { .. } => None,
    }
}

/// 测试辅助：极简 handler 与随机 token（集成测试复用）。
pub mod test_support {
    use super::*;

    /// 回显 handler：把 params 原样包一层返回。
    pub struct EchoHandler;

    impl ApiHandler for EchoHandler {
        async fn handle(
            &self,
            method: &str,
            params: serde_json::Value,
        ) -> std::result::Result<serde_json::Value, ApiError> {
            match method {
                methods::SYS_HEALTH => Ok(serde_json::json!({"ok": true, "echo": params})),
                other => Err(ApiError::method_not_found(other)),
            }
        }
    }

    /// 生成随机 token。
    pub fn random_token() -> String {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        format!("tok-{nanos:016x}")
    }
}
