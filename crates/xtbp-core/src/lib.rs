//! xTB-Pilot 核心库（设计文档 §2.2：领域模型 + 跨 crate 基础设施）。
//!
//! 职责：领域模型（Molecule/Job/Workflow/Result/Method，后续里程碑扩展）、
//! 组件登记（InstanceRegistry，§3.2）、配置加载、ID/版本/哈希。
//! 本 crate 只用 `thiserror` 定义库错误（规划文档 §2.2）；
//! SQLite 持久层在独立的 `xtbp-store` crate（单一职责）。

pub mod config;
pub mod error;
pub mod hash;
pub mod id;
pub mod registry;
pub mod version;

pub use error::{CoreError, Result};
