//! xTB-Pilot 接口层（设计文档 §3.7 / §2.1：TUI 与 agent 共用同一套分发）。
//!
//! - 协议：JSON-RPC 2.0，NDJSON over TCP（127.0.0.1:7700，agent）与
//!   UDS（TUI），一行一个完整请求/响应；
//! - 鉴权：仅 loopback + 请求内 token 一级，单用户无角色；
//! - 订阅：`job.events` / `queue.events`，订阅后服务器主动推流，无需轮询；
//! - 契约：结构化错误码（XTB_CONVERGENCE_FAILED 等）；`api_schema()`
//!   输出全部方法的 JSON-Schema 供 agent 自省。
//!
//! 刻意不引 jsonrpsee/tarpc（规划文档 §2.2）：协议只有「一行一消息」。

pub mod bus;
pub mod client;
pub mod frame;
pub mod protocol;
pub mod server;

pub use bus::EventBus;
pub use client::Client;
pub use frame::{read_frame, write_frame};
pub use protocol::{ApiError, ApiHandler, Params, Request, Response, api_schema, methods};
pub use server::{ServerConfig, serve_tcp, serve_uds};
