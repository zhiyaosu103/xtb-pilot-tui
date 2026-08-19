//! xTB-Pilot 核心库。
//!
//! 职责：组件登记（InstanceRegistry，设计文档 §3.2）、配置加载、ID/版本/哈希、
//! SQLite 与 CSV 持久层。本 crate 只用 `thiserror` 定义库错误（规划文档 §2.2）。

pub mod config;
pub mod error;
pub mod hash;
pub mod id;
pub mod registry;
pub mod store;
pub mod version;

pub use error::{CoreError, Result};
