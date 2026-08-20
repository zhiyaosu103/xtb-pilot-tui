//! 客户端（TUI / agent / CLI 共用）：NDJSON JSON-RPC 调用与事件流。
//!
//! 单连接上请求/响应与服务器推送通知交错：`call` 内部把通知缓存在
//! 队列，调用方用 `read_event` 排空（TUI 事件循环不轮询、不阻塞）。

use crate::frame::{read_frame, write_frame};
use crate::protocol::{ApiError, JSONRPC_VERSION, Notification, Request, Response};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use std::collections::VecDeque;
use tokio::io::{AsyncRead, AsyncWrite, BufStream};

/// 线上帧：响应（有 id）或通知（无 id，有 method）。
#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum WireFrame {
    Resp(Response),
    Notif(Notification),
}

/// JSON-RPC 客户端。
pub struct Client<S> {
    stream: BufStream<S>,
    next_id: u64,
    token: String,
    pending: VecDeque<Notification>,
}

impl<S> Client<S>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    /// 在既有流上构造客户端。
    pub fn new(stream: S, token: impl Into<String>) -> Self {
        Self {
            stream: BufStream::new(stream),
            next_id: 1,
            token: token.into(),
            pending: VecDeque::new(),
        }
    }

    /// 同步调用：发请求、收响应（途中通知入队）。
    pub async fn call(
        &mut self,
        method: &str,
        params: serde_json::Value,
    ) -> Result<serde_json::Value, ApiError> {
        let id = serde_json::json!(self.next_id);
        self.next_id += 1;
        let req = Request {
            jsonrpc: JSONRPC_VERSION.into(),
            id: Some(id.clone()),
            method: method.into(),
            params,
            token: (!self.token.is_empty()).then(|| self.token.clone()),
        };
        write_frame(&mut self.stream, &req)
            .await
            .map_err(|e| ApiError::internal(format!("写请求失败: {e}")))?;
        loop {
            let frame: Option<WireFrame> = read_frame(&mut self.stream)
                .await
                .map_err(|e| ApiError::internal(format!("读响应失败: {e}")))?;
            match frame {
                None => return Err(ApiError::internal("连接被对端关闭")),
                Some(WireFrame::Notif(notif)) => self.pending.push_back(notif),
                Some(WireFrame::Resp(resp)) if resp.id != req.id => {
                    // 错序响应（单连接顺序协议不应出现）：忽略
                    debug_assert!(false, "响应 id 与请求不匹配");
                }
                Some(WireFrame::Resp(resp)) => {
                    return match resp.error {
                        Some(err) => Err(err),
                        None => Ok(resp.result.unwrap_or(serde_json::Value::Null)),
                    };
                }
            }
        }
    }

    /// 类型化调用。
    pub async fn call_typed<Req, Resp>(
        &mut self,
        method: &str,
        params: &Req,
    ) -> Result<Resp, ApiError>
    where
        Req: Serialize,
        Resp: DeserializeOwned,
    {
        let value = serde_json::to_value(params)
            .map_err(|e| ApiError::invalid_params(format!("参数序列化失败: {e}")))?;
        let result = self.call(method, value).await?;
        serde_json::from_value(result)
            .map_err(|e| ApiError::internal(format!("响应反序列化失败: {e}")))
    }

    /// 取一个已缓存或新到的通知（事件订阅用；None = 连接关闭）。
    pub async fn read_event(&mut self) -> Result<Option<Notification>, ApiError> {
        if let Some(n) = self.pending.pop_front() {
            return Ok(Some(n));
        }
        let frame: Option<WireFrame> = read_frame(&mut self.stream)
            .await
            .map_err(|e| ApiError::internal(format!("读事件失败: {e}")))?;
        match frame {
            None => Ok(None),
            Some(WireFrame::Notif(notif)) => Ok(Some(notif)),
            Some(WireFrame::Resp(_)) => {
                // 未预期的响应（无 pending 调用时）——丢弃并继续
                Box::pin(self.read_event()).await
            }
        }
    }

    /// 发送订阅/退订请求（job.events / queue.events）。
    pub async fn subscribe(
        &mut self,
        events_method: &str,
        subscribe: bool,
        job_id: Option<&str>,
    ) -> Result<serde_json::Value, ApiError> {
        self.call(
            events_method,
            serde_json::json!({
                "subscribe": subscribe,
                "job_id": job_id,
            }),
        )
        .await
    }
}

/// 便捷构造：TCP 客户端。
pub async fn connect_tcp(
    addr: &str,
    token: impl Into<String>,
) -> std::io::Result<Client<tokio::net::TcpStream>> {
    let stream = tokio::net::TcpStream::connect(addr).await?;
    Ok(Client::new(stream, token))
}

/// 便捷构造：UDS 客户端。
pub async fn connect_uds(
    path: &str,
    token: impl Into<String>,
) -> std::io::Result<Client<tokio::net::UnixStream>> {
    let stream = tokio::net::UnixStream::connect(path).await?;
    Ok(Client::new(stream, token))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

    /// 极简回显服务器（测试对端）。
    async fn echo_server(mut stream: tokio::net::TcpStream) {
        let mut buf = Vec::new();
        let mut tmp = [0u8; 4096];
        loop {
            match stream.try_read(&mut tmp) {
                Ok(0) => break,
                Ok(n) => buf.extend_from_slice(&tmp[..n]),
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
                Err(_) => break,
            }
        }
        // 回应一帧：ok(id=1, result=42)
        let resp = Response::ok(Some(serde_json::json!(1)), serde_json::json!(42));
        let line = serde_json::to_string(&resp).unwrap() + "\n";
        stream.write_all(line.as_bytes()).await.unwrap();
    }

    #[tokio::test]
    async fn client_call_roundtrip() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            echo_server(stream).await;
        });
        let mut client = connect_tcp(&addr.to_string(), "").await.unwrap();
        let result = client
            .call("sys.health", serde_json::json!({}))
            .await
            .unwrap();
        assert_eq!(result, serde_json::json!(42));
        server.await.unwrap();
    }

    #[tokio::test]
    async fn notification_buffered_during_call() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let (read_half, mut write_half) = stream.into_split();
            let mut reader = BufReader::new(read_half);
            let mut line = String::new();
            let _ = reader.read_line(&mut line).await; // 读请求
            // 先推一个通知，再回响应
            let notif = serde_json::json!({
                "jsonrpc": "2.0",
                "method": "job.events",
                "params": { "type": "queue-depth", "queued": 3, "running": 1 }
            });
            write_half
                .write_all((serde_json::to_string(&notif).unwrap() + "\n").as_bytes())
                .await
                .unwrap();
            let resp = Response::ok(Some(serde_json::json!(1)), serde_json::json!("pong"));
            write_half
                .write_all((serde_json::to_string(&resp).unwrap() + "\n").as_bytes())
                .await
                .unwrap();
            write_half.flush().await.unwrap();
        });
        let mut client = connect_tcp(&addr.to_string(), "").await.unwrap();
        let result = client
            .call("sys.health", serde_json::json!({}))
            .await
            .unwrap();
        assert_eq!(result, serde_json::json!("pong"));
        let event = client.read_event().await.unwrap().unwrap();
        assert_eq!(event.method, "job.events");
        assert_eq!(event.params["queued"], 3);
        server.await.unwrap();
    }
}
