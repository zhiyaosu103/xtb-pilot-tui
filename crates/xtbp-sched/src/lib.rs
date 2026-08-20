//! xTB-Pilot 调度器（设计文档 §3.3：任务模型与调度）。
//!
//! 队列/并发槽/内存令牌/重试/停滞检测/崩溃恢复。
//! 调度逻辑在后续里程碑填充；本文件先固定错误契约。

use thiserror::Error;

/// 调度错误。
#[derive(Debug, Error)]
pub enum SchedError {
    #[error("任务不存在: {0}")]
    NotFound(String),

    #[error("资源耗尽: {reason}")]
    ResourceExhausted { reason: String },

    #[error("IO 错误: {0}")]
    Io(#[from] std::io::Error),
}

/// 调度器便捷 Result 别名。
pub type Result<T> = std::result::Result<T, SchedError>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resource_exhausted_carries_reason() {
        let err = SchedError::ResourceExhausted {
            reason: "memory tokens".into(),
        };
        assert!(err.to_string().contains("memory"));
    }
}
