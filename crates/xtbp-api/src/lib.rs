//! xTB-Pilot 接口层。
//!
//! 协议刻意不引 RPC 框架（规划文档 §2.2）：NDJSON over TCP/UDS，
//! 一行一个 JSON 请求/响应，用 `tokio::net` + `serde_json` 手写帧与分发，
//! 约百行，少一层依赖与版本风险。

pub mod dispatch;
pub mod frame;
pub mod protocol;

pub use dispatch::{dispatch, Handler};
pub use frame::{read_frame, write_frame};
pub use protocol::{api_schema, Request, Response};
