//! xTB-Pilot 子进程封装（设计文档 §3.3：调度器的执行原语）。
//!
//! 职责边界：本 crate 是唯一允许构造外部命令的地方（开发约束 §6），
//! 参数以数组传递、禁止字符串拼接。提供：spawn、wall-clock 超时、
//! stdout/stderr 流式读取、进程树 kill（`setsid` 进程组）。
//!
//! 计算的实际执行在后续里程碑填充；本文件先固定错误契约。

use thiserror::Error;

/// 子进程运行错误。
#[derive(Debug, Error)]
pub enum RunnerError {
    #[error("IO 错误: {0}")]
    Io(#[from] std::io::Error),

    #[error("启动子进程失败: {0}")]
    Spawn(String),

    #[error("任务超时（上限 {limit_secs}s）")]
    Timeout { limit_secs: u64 },

    #[error("子进程非零退出: code={code}, stderr 尾部: {tail}")]
    NonZeroExit { code: i32, tail: String },

    #[error("stdout 流通道意外关闭")]
    StdoutGone,
}

/// 运行器便捷 Result 别名。
pub type Result<T> = std::result::Result<T, RunnerError>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn timeout_error_carries_limit() {
        let err = RunnerError::Timeout { limit_secs: 42 };
        assert!(err.to_string().contains("42"));
    }
}
