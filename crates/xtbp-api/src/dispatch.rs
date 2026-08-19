//! 请求分发（daemon 侧使用）：按 `Request` 分派到 handler。
//!
//! 不引 `async_trait`：edition 2024 原生支持 trait 中的 async fn。

use crate::protocol::{Request, Response};

/// 请求处理器 trait。
///
/// RPITIT（edition 2024）而非裸 `async fn`：显式声明返回 Future 为 `Send`，
/// 满足 daemon 里 `tokio::spawn` 的要求。
pub trait Handler: Send + Sync {
    /// 处理一个请求，返回响应。
    fn handle(&self, req: Request) -> impl std::future::Future<Output = Response> + Send;
}

/// 分发入口（泛型：async fn in trait 非 dyn-compatible，故不用 trait object）。
pub async fn dispatch<H: Handler>(handler: &H, req: Request) -> Response {
    handler.handle(req).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::Response;

    struct PingHandler;

    impl Handler for PingHandler {
        async fn handle(&self, req: Request) -> Response {
            match req {
                Request::Ping => Response::Ok {
                    job_id: String::new(),
                    data: serde_json::json!({ "pong": true }),
                },
                other => Response::Err {
                    message: format!("unhandled: {other:?}"),
                },
            }
        }
    }

    #[tokio::test]
    async fn dispatch_routes_ping() {
        let handler = PingHandler;
        let resp = dispatch(&handler, Request::Ping).await;
        match resp {
            Response::Ok { data, .. } => assert_eq!(data["pong"], true),
            Response::Err { message } => panic!("unexpected error: {message}"),
        }
    }
}
