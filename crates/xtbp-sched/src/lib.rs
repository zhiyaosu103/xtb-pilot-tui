//! xTB-Pilot 调度器（设计文档 §3.3：任务模型与调度）。
//!
//! 提供：优先级队列、全局并发槽、内存令牌、退避重试、取消、崩溃恢复。
//! 执行原语来自 `xtbp-runner`，状态落库走 `xtbp-store`，事件走 `xtbp-api` 总线。

mod scheduler;

pub use scheduler::{ExecResult, ExecUnit, QueueStats, SchedConfig, SchedError, Scheduler};

/// 调度器便捷 Result 别名。
pub type Result<T> = std::result::Result<T, SchedError>;
