//! xTB-Pilot 核心库（设计文档 §2.2：领域模型 + 跨 crate 基础设施）。
//!
//! 职责：领域模型（Molecule/Job/Workflow/Result/Method）、
//! 组件登记（InstanceRegistry，§3.2）、配置加载、ID/版本/哈希、时间工具。
//! 本 crate 只用 `thiserror` 定义库错误（规划文档 §2.2）；
//! SQLite 持久层在独立的 `xtbp-store` crate（单一职责）。

pub mod config;
pub mod error;
pub mod hash;
pub mod id;
pub mod job;
pub mod method;
pub mod molecule;
pub mod registry;
pub mod result;
pub mod time;
pub mod version;
pub mod workflow;

pub use error::{CoreError, Result};
pub use id::Ulid;
pub use job::{Job, JobParams, JobStatus, error_codes};
pub use method::{Method, MethodFamily, MethodPreset, Solvent};
pub use molecule::{Charge, Molecule, Multiplicity};
pub use result::{Broadening, METHOD_TIER_SCREENING, ScalarResult, Spectrum, Transition};
pub use workflow::{BUILTIN_TEMPLATES, OnFailure, StepResources, WorkflowStep, WorkflowTemplate};
