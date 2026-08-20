//! 领域模型：计算方法（设计文档 §3.2：方法预设集中管理）。
//!
//! 方法预设（如 `gfn2+alpb(toluene)`）以 TOML 集中管理；
//! 命令行 flag 由 [`Method::xtb_flags`] 单点生成，不散落各处。

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// 计算级别（组件族）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum MethodFamily {
    /// GFN0-xTB。
    Gfn0Xtb,
    /// GFN1-xTB。
    Gfn1Xtb,
    /// GFN2-xTB。
    Gfn2Xtb,
    /// GFN-FF 力场。
    GfnFf,
    /// sTDA-xTB 激发态（stda 组件，-xtb 读 GFN 轨道）。
    StdaXtb,
    /// GFN 轨道生成（xtb4stda 组件）。
    Xtb4Stda,
}

impl MethodFamily {
    /// 方法名缩写（用于文件/结果键，如 "gfn2"）。
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Gfn0Xtb => "gfn0",
            Self::Gfn1Xtb => "gfn1",
            Self::Gfn2Xtb => "gfn2",
            Self::GfnFf => "gfnff",
            Self::StdaXtb => "stda",
            Self::Xtb4Stda => "xtb4stda",
        }
    }
}

/// 隐式溶剂（ALPB）。
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(transparent)]
pub struct Solvent(pub String);

/// 计算方法：族 + 可选溶剂。
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
pub struct Method {
    /// 计算级别。
    pub family: MethodFamily,
    /// 可选 ALPB 溶剂。
    pub solvent: Option<Solvent>,
}

impl Method {
    /// GFN2-xTB（气相）便捷构造。
    pub fn gfn2() -> Self {
        Self {
            family: MethodFamily::Gfn2Xtb,
            solvent: None,
        }
    }

    /// 生成 xtb 命令行 flag（数组传递，禁止拼接整行命令）。
    pub fn xtb_flags(&self) -> Vec<String> {
        let mut flags = Vec::new();
        match self.family {
            MethodFamily::Gfn0Xtb => {
                flags.push("--gfn".into());
                flags.push("0".into());
            }
            MethodFamily::Gfn1Xtb => {
                flags.push("--gfn".into());
                flags.push("1".into());
            }
            MethodFamily::Gfn2Xtb => {
                flags.push("--gfn".into());
                flags.push("2".into());
            }
            MethodFamily::GfnFf => flags.push("--gfnff".into()),
            _ => {}
        }
        if let Some(solv) = &self.solvent {
            flags.push("--alpb".into());
            flags.push(solv.0.clone());
        }
        flags
    }

    /// 方法全名（含溶剂），用于 job.toml 快照与结果键。
    pub fn full_name(&self) -> String {
        match &self.solvent {
            Some(s) => format!("{}+alpb({})", self.family.as_str(), s.0),
            None => self.family.as_str().to_string(),
        }
    }
}

impl Default for Method {
    fn default() -> Self {
        Self::gfn2()
    }
}

/// 方法预设（TOML 集中管理，设计文档 §3.2）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MethodPreset {
    /// 预设名，如 "gfn2+alpb(toluene)"。
    pub name: String,
    /// 方法本体。
    pub method: Method,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gfn2_flags() {
        assert_eq!(Method::gfn2().xtb_flags(), vec!["--gfn", "2"]);
    }

    #[test]
    fn gfn2_alpb_toluene_flags() {
        let m = Method {
            family: MethodFamily::Gfn2Xtb,
            solvent: Some(Solvent("toluene".into())),
        };
        assert_eq!(m.xtb_flags(), vec!["--gfn", "2", "--alpb", "toluene"]);
        assert_eq!(m.full_name(), "gfn2+alpb(toluene)");
    }

    #[test]
    fn gfnff_flag() {
        let m = Method {
            family: MethodFamily::GfnFf,
            solvent: None,
        };
        assert_eq!(m.xtb_flags(), vec!["--gfnff"]);
    }

    #[test]
    fn preset_toml_roundtrip() {
        let preset = MethodPreset {
            name: "gfn2+alpb(toluene)".into(),
            method: Method {
                family: MethodFamily::Gfn2Xtb,
                solvent: Some(Solvent("toluene".into())),
            },
        };
        let toml_str = toml::to_string(&preset).unwrap();
        let back: MethodPreset = toml::from_str(&toml_str).unwrap();
        assert_eq!(back, preset);
    }
}
