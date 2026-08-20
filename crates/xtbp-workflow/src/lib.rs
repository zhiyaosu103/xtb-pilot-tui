//! xTB-Pilot 工作流引擎（设计文档 §3.4：TOML 声明、引擎解释为 DAG）。
//!
//! 内置模板：opt / conformer / opt-freq / excited / redox / reorg-4pt /
//! solv-series。只做机械执行与回收，不做任何科学判断。

pub mod engine;
pub mod registry;

pub use engine::{ComponentResolver, ResolvedComponent, WorkflowEngine};
pub use registry::TemplateRegistry;

use thiserror::Error;

/// 工作流错误。
#[derive(Debug, Error)]
pub enum WorkflowError {
    #[error("未知工作流模板: {0}")]
    UnknownTemplate(String),

    #[error("模板解析错误: {0}")]
    Template(String),

    #[error("IO 错误: {0}")]
    Io(#[from] std::io::Error),

    #[error("步骤失败: step={step}, {message}")]
    StepFailed { step: String, message: String },

    #[error("缺少产物: step={step}, path={path}")]
    MissingArtifact { step: String, path: String },

    #[error("组装错误: {0}")]
    Assemble(#[from] xtbp_assemble::AssembleError),

    #[error("解析错误: {0}")]
    Parse(#[from] xtbp_parse::ParseError),

    #[error("持久层错误: {0}")]
    Store(#[from] xtbp_store::StoreError),

    #[error("调度错误: {0}")]
    Sched(#[from] xtbp_sched::SchedError),

    #[error("任务状态异常: {0}")]
    State(String),
}

/// 工作流便捷 Result 别名。
pub type Result<T> = std::result::Result<T, WorkflowError>;
