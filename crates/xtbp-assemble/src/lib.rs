//! xTB-Pilot 计算文件组装（设计文档 §3.1：SMILES → 自洽计算目录）。
//!
//! 3D 生成经常驻 `rdkit_helper` 进程（stdio 换行 JSON 协议，§4.1）；
//! 目录即真相：即使 daemon 宕机，人也可以 `cd` 进去手动重跑 `cmd.txt`。
//!
//! 模块分工：
//! - [`helper`]：RDKit helper 常驻进程客户端（协议 v1）；
//! - [`template`]：工作流模板占位符渲染；
//! - [`assemble`]：计算目录生成。

pub mod assemble;
pub mod helper;
pub mod template;

pub use assemble::{Assembled, assemble};
pub use helper::{Gen3dOutput, HelperClient};
pub use template::{RenderCtx, RenderOutput, render_step};

use thiserror::Error;

/// 组装错误。
#[derive(Debug, Error)]
pub enum AssembleError {
    #[error("IO 错误: {0}")]
    Io(#[from] std::io::Error),

    #[error("RDKit 错误: {message}")]
    Rdkit { message: String },

    #[error("无效 SMILES: {smiles}")]
    InvalidSmiles { smiles: String },

    #[error("RDKit helper 进程不可用")]
    HelperGone,

    #[error("模板渲染错误: {message}")]
    Template { message: String },

    #[error("序列化错误: {message}")]
    Serialize { message: String },
}

/// 组装器便捷 Result 别名。
pub type Result<T> = std::result::Result<T, AssembleError>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn invalid_smiles_error_echoes_input() {
        let err = AssembleError::InvalidSmiles {
            smiles: "C(C".into(),
        };
        assert!(err.to_string().contains("C(C"));
    }
}
