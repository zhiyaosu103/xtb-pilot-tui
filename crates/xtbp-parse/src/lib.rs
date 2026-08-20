//! xTB-Pilot 输出解析（设计文档 §4.2：解析脆弱性对策）。
//!
//! 原则：优先机器可读输出（`xtb --json`、`crest_property.json`），
//! 文本解析仅作回退；解析失败 ≠ job 失败——标记 `ParseDegraded`、
//! 保留原始文件。每个组件×版本配 insta 快照 fixtures。
//!
//! 具体解析器实现见 [`parsers`]；本文件固定错误契约。

pub mod parsers;

pub use parsers::*;

use thiserror::Error;

/// 解析错误。
#[derive(Debug, Error)]
pub enum ParseError {
    #[error("IO 错误: {0}")]
    Io(#[from] std::io::Error),

    #[error("JSON 错误: {0}")]
    Json(#[from] serde_json::Error),

    #[error("缺少必要字段: {field}")]
    MissingField { field: String },

    #[error("无法解释的输出: {what}")]
    Unexpected { what: String },

    /// 解析器键上下文：错误消息包含解析器键，便于上层定位退化来源。
    #[error("解析器 {parser} 解析失败: {source}")]
    Parser {
        parser: String,
        #[source]
        source: Box<ParseError>,
    },
}

/// 解析器便捷 Result 别名。
pub type Result<T> = std::result::Result<T, ParseError>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_field_error_names_field() {
        let err = ParseError::MissingField {
            field: "total_energy".into(),
        };
        assert!(err.to_string().contains("total_energy"));
    }
}
