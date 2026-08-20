//! 领域模型：工作流模板（设计文档 §3.4：TOML 声明、引擎解释为 DAG）。
//!
//! 模板只声明「做什么」（步骤、命令骨架、产物、回收规则），
//! 不做科学判断。占位符渲染与目录生成在 xtbp-assemble。

use crate::method::Method;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::Path;

/// 工作流模板（一份 TOML 一个模板）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WorkflowTemplate {
    /// 模板 id（如 "opt"、"excited"）。
    pub id: String,
    /// 一句话描述。
    pub description: String,
    /// 步骤链（DAG 节点，按 id 互指）。
    pub steps: Vec<WorkflowStep>,
}

/// 单个步骤（DAG 节点）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WorkflowStep {
    /// 步骤 id（模板内唯一，如 "gen3d"、"opt"）。
    pub id: String,
    /// 依赖的步骤 id（DAG 边）。
    #[serde(default)]
    pub depends_on: Vec<String>,
    /// 组件名（xtb / crest / xtb4stda / stda / rdkit）。
    pub component: String,
    /// 命令骨架（参数数组；占位符如 `{input_xyz}`、`{threads}`）。
    pub command: Vec<String>,
    /// 输入文件（渲染后复制/指向，支持上游产物名映射）。
    #[serde(default)]
    pub inputs: Vec<String>,
    /// 环境变量注入（子进程，非全局）。
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    /// 资源需求。
    #[serde(default)]
    pub resources: StepResources,
    /// 产物文件（相对工作目录，回收时收集）。
    #[serde(default)]
    pub outputs: Vec<String>,
    /// 结果回收规则：产物文件 → 解析器键。
    #[serde(default)]
    pub collect: Vec<Collector>,
    /// 失败策略。
    #[serde(default)]
    pub on_failure: OnFailure,
}

/// 步骤资源需求。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct StepResources {
    /// OMP 线程数（0 = 用调度器默认）。
    pub threads: u32,
    /// 内存上限（MB，0 = 不限）。
    pub memory_mb: u64,
    /// wall-clock 超时（秒，0 = 用任务默认）。
    pub wall_timeout_secs: u64,
}

/// 结果回收规则：某产物文件按解析器键提取标量/谱。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Collector {
    /// 产物文件名（如 "xtb.out"、"tda.dat"、"crest_property.json"）。
    pub file: String,
    /// 解析器键（如 "xtb-json"、"tda-dat"、"crest-property"）。
    pub parser: String,
}

/// 步骤失败策略。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum OnFailure {
    /// 终止整链（默认）。
    #[default]
    Abort,
    /// 跳过该分支（下游继续）。
    Skip,
}

impl WorkflowTemplate {
    /// 从 TOML 文件加载模板。
    pub fn load(path: &Path) -> Result<Self, String> {
        let raw = std::fs::read_to_string(path).map_err(|e| e.to_string())?;
        Self::parse(&raw)
    }

    /// 解析 TOML 文本。
    pub fn parse(raw: &str) -> Result<Self, String> {
        let tpl: Self = toml::from_str(raw).map_err(|e| format!("模板 TOML 解析失败: {e}"))?;
        tpl.validate()?;
        Ok(tpl)
    }

    /// 校验：步骤 id 唯一、依赖存在、无环（拓扑排序检测）。
    pub fn validate(&self) -> Result<(), String> {
        let ids: Vec<&str> = self.steps.iter().map(|s| s.id.as_str()).collect();
        for (i, id) in ids.iter().enumerate() {
            if ids[..i].contains(id) {
                return Err(format!("步骤 id 重复: {id}"));
            }
        }
        for step in &self.steps {
            for dep in &step.depends_on {
                if !ids.contains(&dep.as_str()) {
                    return Err(format!("步骤 {} 依赖不存在的步骤 {dep}", step.id));
                }
            }
        }
        self.topo_order()?;
        Ok(())
    }

    /// 拓扑排序（Kahn 算法），返回执行顺序。
    pub fn topo_order(&self) -> Result<Vec<usize>, String> {
        let n = self.steps.len();
        let index_of = |id: &str| self.steps.iter().position(|s| s.id == id);
        let mut indeg = vec![0usize; n];
        let mut children = vec![Vec::new(); n];
        for (i, step) in self.steps.iter().enumerate() {
            for dep in &step.depends_on {
                let j = index_of(dep).ok_or_else(|| format!("未知依赖: {dep}"))?;
                indeg[i] += 1;
                children[j].push(i);
            }
        }
        let mut ready: Vec<usize> = (0..n).filter(|&i| indeg[i] == 0).collect();
        let mut order = Vec::with_capacity(n);
        while let Some(i) = ready.pop() {
            order.push(i);
            for &c in &children[i] {
                indeg[c] -= 1;
                if indeg[c] == 0 {
                    ready.push(c);
                }
            }
        }
        if order.len() != n {
            return Err("工作流模板存在环".to_string());
        }
        Ok(order)
    }
}

/// 内置模板清单（id 列表，TOML 本体在仓库 templates/ 目录）。
pub const BUILTIN_TEMPLATES: &[&str] = &[
    "opt",
    "sp",
    "conformer",
    "opt-freq",
    "excited",
    "redox",
    "reorg-4pt",
    "solv-series",
];

/// 内置模板默认方法（各模板缺省参数时的起点）。
pub fn builtin_default_method(template: &str) -> Method {
    match template {
        // solv-series 默认 ALPB(水)；其余气相 GFN2。
        "solv-series" => Method {
            family: crate::method::MethodFamily::Gfn2Xtb,
            solvent: Some(crate::method::Solvent("water".into())),
        },
        _ => Method::gfn2(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const OPT_TOML: &str = r#"
id = "opt"
description = "gen3d → GFN2-xTB opt"

[[steps]]
id = "gen3d"
component = "rdkit"
command = ["gen3d", "{smiles}", "{charge}", "{mult}"]
outputs = ["mol.xyz"]
collect = [{ file = "mol.xyz", parser = "xyz" }]

[[steps]]
id = "opt"
component = "xtb"
depends_on = ["gen3d"]
command = ["xtb", "{input_xyz}", "--gfn", "2", "--opt", "tight"]
inputs = ["mol.xyz"]
outputs = ["xtbopt.xyz", "xtb.out"]
collect = [{ file = "xtb.out", parser = "xtb-json" }]
on_failure = "abort"
"#;

    #[test]
    fn parses_and_validates_template() {
        let tpl = WorkflowTemplate::parse(OPT_TOML).unwrap();
        assert_eq!(tpl.id, "opt");
        assert_eq!(tpl.steps.len(), 2);
        assert_eq!(tpl.topo_order().unwrap(), vec![0, 1]);
    }

    #[test]
    fn duplicate_step_id_rejected() {
        let raw = OPT_TOML.replace(
            "id = \"opt\"\ncomponent = \"xtb\"",
            "id = \"gen3d\"\ncomponent = \"xtb\"",
        );
        assert!(WorkflowTemplate::parse(&raw).unwrap_err().contains("重复"));
    }

    #[test]
    fn unknown_dependency_rejected() {
        let raw = OPT_TOML.replace("depends_on = [\"gen3d\"]", "depends_on = [\"nope\"]");
        assert!(WorkflowTemplate::parse(&raw).unwrap_err().contains("nope"));
    }

    #[test]
    fn cycle_detected() {
        let raw = r#"
id = "cyc"
description = "cycle"
[[steps]]
id = "a"
component = "xtb"
command = ["xtb"]
depends_on = ["b"]
[[steps]]
id = "b"
component = "xtb"
command = ["xtb"]
depends_on = ["a"]
"#;
        assert!(WorkflowTemplate::parse(raw).unwrap_err().contains("环"));
    }

    #[test]
    fn builtin_templates_all_registered() {
        assert_eq!(BUILTIN_TEMPLATES.len(), 8);
        assert!(BUILTIN_TEMPLATES.contains(&"excited"));
        assert!(BUILTIN_TEMPLATES.contains(&"sp"));
    }

    #[test]
    fn solv_series_defaults_to_alpb_water() {
        let m = builtin_default_method("solv-series");
        assert!(m.solvent.is_some());
    }
}
