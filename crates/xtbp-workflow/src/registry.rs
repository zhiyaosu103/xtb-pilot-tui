//! 模板注册表：加载 `templates/` 目录的全部工作流 TOML，
//! 校验（重复 id / 依赖存在 / 无环）后按 id 索引（设计文档 §3.4）。

use crate::WorkflowError;
use std::collections::BTreeMap;
use std::path::Path;
use xtbp_core::workflow::WorkflowTemplate;

/// 模板注册表。
#[derive(Debug, Clone, Default)]
pub struct TemplateRegistry {
    templates: BTreeMap<String, WorkflowTemplate>,
}

impl TemplateRegistry {
    /// 空注册表。
    pub fn new() -> Self {
        Self::default()
    }

    /// 加载一个目录下的全部 `*.toml` 模板（非法文件报错并指名）。
    pub fn load_dir(dir: &Path) -> crate::Result<Self> {
        let mut reg = Self::new();
        let mut entries: Vec<_> = std::fs::read_dir(dir)?
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .filter(|p| p.extension().is_some_and(|x| x == "toml"))
            .collect();
        entries.sort();
        for path in entries {
            let tpl = WorkflowTemplate::load(&path)
                .map_err(|e| WorkflowError::Template(format!("{}: {e}", path.display())))?;
            if reg.templates.contains_key(&tpl.id) {
                return Err(WorkflowError::Template(format!("模板 id 重复: {}", tpl.id)));
            }
            reg.templates.insert(tpl.id.clone(), tpl);
        }
        Ok(reg)
    }

    /// 注册单个模板。
    pub fn register(&mut self, tpl: WorkflowTemplate) -> crate::Result<()> {
        if self.templates.contains_key(&tpl.id) {
            return Err(WorkflowError::Template(format!("模板 id 重复: {}", tpl.id)));
        }
        self.templates.insert(tpl.id.clone(), tpl);
        Ok(())
    }

    /// 取模板。
    pub fn get(&self, id: &str) -> Option<&WorkflowTemplate> {
        self.templates.get(id)
    }

    /// 全部模板 id。
    pub fn ids(&self) -> Vec<&str> {
        self.templates.keys().map(String::as_str).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 仓库根 templates/ 目录（7 个内置模板全部可解析、可校验）。
    #[test]
    fn builtin_templates_are_valid() {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../templates");
        let reg = TemplateRegistry::load_dir(&dir).unwrap();
        let ids = reg.ids();
        assert_eq!(ids.len(), 7, "内置模板应恰好 7 个: {ids:?}");
        for id in xtbp_core::BUILTIN_TEMPLATES {
            assert!(ids.contains(id), "缺少内置模板: {id}");
        }
        // DAG 校验：excited 为 4 步链
        let excited = reg.get("excited").unwrap();
        assert_eq!(excited.topo_order().unwrap().len(), 4);
        // redox 三条分支共享 gen3d
        let redox = reg.get("redox").unwrap();
        assert_eq!(redox.steps.len(), 4);
        for s in &redox.steps {
            assert!(s.id == "gen3d" || s.depends_on.as_slice() == ["gen3d"]);
        }
    }

    #[test]
    fn duplicate_template_id_rejected() {
        let tpl = WorkflowTemplate::parse(
            r#"
id = "opt"
description = "x"
[[steps]]
id = "a"
component = "xtb"
command = ["xtb"]
"#,
        )
        .unwrap();
        let mut reg = TemplateRegistry::new();
        reg.register(tpl.clone()).unwrap();
        assert!(reg.register(tpl).is_err());
    }
}
