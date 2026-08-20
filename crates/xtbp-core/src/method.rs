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

/// 隐式溶剂模型（§3.2：ALPB 与 GBSA 二选一；None = 气相）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum Solvation {
    /// 气相（无隐式溶剂）。
    None,
    /// ALPB（解析线性化 Poisson-Boltzmann，xtb 默认）。
    Alpb,
    /// GBSA（通用 Born 表面积）。
    Gbsa,
}

/// 隐式溶剂（ALPB/GBSA 的溶剂名，如 water/toluene/thf）。
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(transparent)]
pub struct Solvent(pub String);

/// 计算方法：族 + 隐式溶剂模型 + 计算微调参数。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Method {
    /// 计算级别。
    pub family: MethodFamily,
    /// 隐式溶剂模型（缺省 Alpb——兼容旧数据：solvent 非空即 ALPB）。
    #[serde(default = "default_solvation")]
    pub solvation: Solvation,
    /// 溶剂名（solvation 非 None 时生效）。
    pub solvent: Option<Solvent>,
    /// 电子温度（K，--etemp；缺省用 xtb 默认 300）。
    #[serde(default)]
    pub etemp: Option<f64>,
    /// SCF 收敛精度（--acc；缺省 1.0）。
    #[serde(default)]
    pub accuracy: Option<f64>,
    /// SCF 最大迭代次数（--maxiter）。
    #[serde(default)]
    pub maxiter: Option<u32>,
}

fn default_solvation() -> Solvation {
    Solvation::Alpb
}

impl Method {
    /// GFN2-xTB（气相）便捷构造。
    pub fn gfn2() -> Self {
        Self {
            family: MethodFamily::Gfn2Xtb,
            solvation: Solvation::None,
            solvent: None,
            etemp: None,
            accuracy: None,
            maxiter: None,
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
        if let (Some(solv), Solvation::Alpb) = (&self.solvent, self.solvation) {
            flags.push("--alpb".into());
            flags.push(solv.0.clone());
        } else if let (Some(solv), Solvation::Gbsa) = (&self.solvent, self.solvation) {
            flags.push("--gbsa".into());
            flags.push(solv.0.clone());
        }
        if let Some(et) = self.etemp {
            flags.push("--etemp".into());
            flags.push(format!("{et}"));
        }
        if let Some(acc) = self.accuracy {
            flags.push("--acc".into());
            flags.push(format!("{acc}"));
        }
        if let Some(mi) = self.maxiter {
            flags.push("--maxiter".into());
            flags.push(mi.to_string());
        }
        flags
    }

    /// 方法全名（含溶剂与微调参数），用于 job.toml 快照与结果键。
    pub fn full_name(&self) -> String {
        let mut s = match (&self.solvent, self.solvation) {
            (Some(solv), Solvation::Alpb) => format!("{}+alpb({})", self.family.as_str(), solv.0),
            (Some(solv), Solvation::Gbsa) => format!("{}+gbsa({})", self.family.as_str(), solv.0),
            _ => self.family.as_str().to_string(),
        };
        if let Some(et) = self.etemp {
            s.push_str(&format!(";etemp={et}"));
        }
        if let Some(acc) = self.accuracy {
            s.push_str(&format!(";acc={acc}"));
        }
        if let Some(mi) = self.maxiter {
            s.push_str(&format!(";maxiter={mi}"));
        }
        s
    }
}

impl Default for Method {
    fn default() -> Self {
        Self::gfn2()
    }
}

/// 方法预设（TOML 集中管理，设计文档 §3.2）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
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
            solvation: Solvation::Alpb,
            solvent: Some(Solvent("toluene".into())),
            etemp: None,
            accuracy: None,
            maxiter: None,
        };
        assert_eq!(m.xtb_flags(), vec!["--gfn", "2", "--alpb", "toluene"]);
        assert_eq!(m.full_name(), "gfn2+alpb(toluene)");
    }

    #[test]
    fn gfnff_flag() {
        let m = Method {
            family: MethodFamily::GfnFf,
            solvation: Solvation::None,
            solvent: None,
            etemp: None,
            accuracy: None,
            maxiter: None,
        };
        assert_eq!(m.xtb_flags(), vec!["--gfnff"]);
    }

    #[test]
    fn gbsa_and_tuning_flags() {
        let m = Method {
            family: MethodFamily::Gfn1Xtb,
            solvation: Solvation::Gbsa,
            solvent: Some(Solvent("water".into())),
            etemp: Some(500.0),
            accuracy: Some(0.5),
            maxiter: Some(250),
        };
        assert_eq!(
            m.xtb_flags(),
            vec![
                "--gfn",
                "1",
                "--gbsa",
                "water",
                "--etemp",
                "500",
                "--acc",
                "0.5",
                "--maxiter",
                "250",
            ]
        );
        assert_eq!(
            m.full_name(),
            "gfn1+gbsa(water);etemp=500;acc=0.5;maxiter=250"
        );
    }

    #[test]
    fn solvent_without_solvation_is_ignored() {
        // 兼容旧数据：solvation 缺省 Alpb；显式 None + solvent → 无溶剂 flag
        let m = Method {
            family: MethodFamily::Gfn2Xtb,
            solvation: Solvation::None,
            solvent: Some(Solvent("water".into())),
            etemp: None,
            accuracy: None,
            maxiter: None,
        };
        assert_eq!(m.xtb_flags(), vec!["--gfn", "2"]);
        assert_eq!(m.full_name(), "gfn2");
    }

    #[test]
    fn serde_default_solvation_is_alpb() {
        // 旧提交只有 {family, solvent}：反序列化后 solvation 应为 Alpb（保持 ALPB 语义）
        let m: Method = serde_json::from_str(r#"{"family":"gfn2-xtb","solvent":"water"}"#).unwrap();
        assert_eq!(m.solvation, Solvation::Alpb);
        assert_eq!(m.xtb_flags(), vec!["--gfn", "2", "--alpb", "water"]);
        let m2: Method = serde_json::from_str(
            r#"{"family":"gfn2-xtb","solvation":"gbsa","solvent":"thf","etemp":500.0}"#,
        )
        .unwrap();
        assert_eq!(
            m2.xtb_flags(),
            vec!["--gfn", "2", "--gbsa", "thf", "--etemp", "500"]
        );
    }

    #[test]
    fn preset_toml_roundtrip() {
        let preset = MethodPreset {
            name: "gfn2+alpb(toluene)".into(),
            method: Method {
                family: MethodFamily::Gfn2Xtb,
                solvation: Solvation::Alpb,
                solvent: Some(Solvent("toluene".into())),
                etemp: None,
                accuracy: None,
                maxiter: None,
            },
        };
        let toml_str = toml::to_string(&preset).unwrap();
        let back: MethodPreset = toml::from_str(&toml_str).unwrap();
        assert_eq!(back, preset);
    }
}
