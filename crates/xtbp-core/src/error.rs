//! 库错误类型（`thiserror`，规划文档 §2.2：仅库 crate 使用）。

use thiserror::Error;

/// 核心库统一错误。
#[derive(Debug, Error)]
pub enum CoreError {
    #[error("配置错误: {0}")]
    Config(String),

    #[error("组件未登记: {0}")]
    ComponentNotFound(String),

    #[error("IO 错误: {0}")]
    Io(#[from] std::io::Error),

    #[error("TOML 解析错误: {0}")]
    Toml(#[from] toml::de::Error),

    #[error("其他错误: {0}")]
    Other(String),
}

/// 核心库便捷 Result 别名。
pub type Result<T> = std::result::Result<T, CoreError>;
