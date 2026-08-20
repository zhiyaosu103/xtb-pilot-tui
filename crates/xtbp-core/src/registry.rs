//! 组件登记表（InstanceRegistry，设计文档 §3.2 的多版本机制）。
//!
//! 所有 xtb 工具链组件（xtb / crest / xtb4stda / stda / rdkit helper 等）以
//! （名称, 版本, 可执行路径, sha256）登记；daemon 以登记表为准拉起子进程，
//! 不依赖 PATH 运气（规划文档 §1.2）。conda-forge 缺失的 xtb4stda/stda 二进制
//! 即按此机制手动登记（见 README「xtb4stda / stda 二进制注册」）。

use crate::error::{CoreError, Result};
use crate::hash::sha256_file;
use crate::id::Ulid;
use crate::version::ComponentVersion;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// 一条组件登记记录。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ComponentEntry {
    /// 组件名，如 "xtb"、"crest"、"stda"、"rdkit-helper"。
    pub name: String,
    /// 语义化版本。
    pub version: ComponentVersion,
    /// 可执行文件绝对路径。
    pub exe: PathBuf,
    /// 内容 sha256，启动前自检用。
    pub sha256: String,
    /// 登记序号。
    pub id: Ulid,
}

/// 多版本组件登记表。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct InstanceRegistry {
    entries: Vec<ComponentEntry>,
}

impl InstanceRegistry {
    /// 新建空登记表。
    pub fn new() -> Self {
        Self::default()
    }

    /// 登记一个组件：校验文件存在并计算 sha256。
    pub fn register(
        &mut self,
        name: &str,
        version: ComponentVersion,
        exe: PathBuf,
    ) -> Result<ComponentEntry> {
        if !exe.is_file() {
            return Err(CoreError::Config(format!(
                "组件可执行文件不存在: {}",
                exe.display()
            )));
        }
        let sha256 = sha256_file(&exe)?;
        let entry = ComponentEntry {
            name: name.to_string(),
            version,
            exe,
            sha256,
            id: Ulid::new(),
        };
        self.entries.push(entry.clone());
        Ok(entry)
    }

    /// 取某组件最新版本条目。
    pub fn latest(&self, name: &str) -> Option<&ComponentEntry> {
        self.entries
            .iter()
            .filter(|e| e.name == name)
            .max_by_key(|e| &e.version)
    }

    /// 取某组件全部条目（多版本并存）。
    pub fn all(&self, name: &str) -> impl Iterator<Item = &ComponentEntry> {
        self.entries.iter().filter(move |e| e.name == name)
    }

    /// 条目总数。
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// 是否为空。
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// 从 TOML 文件加载。
    pub fn load(path: &Path) -> Result<Self> {
        let raw = std::fs::read_to_string(path)?;
        let reg: Self = toml::from_str(&raw)?;
        Ok(reg)
    }

    /// 保存为 TOML 文件。
    pub fn save(&self, path: &Path) -> Result<()> {
        let raw = toml::to_string(self)
            .map_err(|e| CoreError::Other(format!("登记表序列化失败: {e}")))?;
        std::fs::write(path, raw)?;
        Ok(())
    }

    /// 组件自动发现：在 PATH 中查找可执行文件（`which`）。
    pub fn discover(name: &str) -> Option<PathBuf> {
        which::which(name).ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fake_exe(dir: &Path, content: &[u8]) -> PathBuf {
        let p = dir.join("fake-xtb");
        std::fs::write(&p, content).unwrap();
        p
    }

    #[test]
    fn register_validates_and_hashes() {
        let dir = tempfile::tempdir().unwrap();
        let exe = fake_exe(dir.path(), b"#!/bin/sh\necho fake\n");
        let mut reg = InstanceRegistry::new();
        let entry = reg
            .register("xtb", "6.7.1".parse().unwrap(), exe.clone())
            .unwrap();
        assert_eq!(entry.exe, exe);
        assert_eq!(entry.sha256.len(), 64);
        assert_eq!(entry.version.to_string(), "6.7.1");
        assert_eq!(reg.latest("xtb").unwrap().sha256, entry.sha256);
    }

    #[test]
    fn register_rejects_missing_file() {
        let mut reg = InstanceRegistry::new();
        let err = reg
            .register(
                "xtb",
                "6.7.1".parse().unwrap(),
                PathBuf::from("/nonexistent"),
            )
            .unwrap_err();
        assert!(err.to_string().contains("不存在"));
    }

    #[test]
    fn multi_version_latest_picks_max() {
        let dir = tempfile::tempdir().unwrap();
        let exe = fake_exe(dir.path(), b"v1");
        let mut reg = InstanceRegistry::new();
        reg.register("xtb", "6.6.1".parse().unwrap(), exe.clone())
            .unwrap();
        reg.register("xtb", "6.7.1".parse().unwrap(), exe.clone())
            .unwrap();
        assert_eq!(reg.latest("xtb").unwrap().version.to_string(), "6.7.1");
        assert_eq!(reg.all("xtb").count(), 2);
    }

    #[test]
    fn registry_toml_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let exe = fake_exe(dir.path(), b"roundtrip");
        let mut reg = InstanceRegistry::new();
        reg.register("stda", "1.6.1".parse().unwrap(), exe.clone())
            .unwrap();
        let path = dir.path().join("registry.toml");
        reg.save(&path).unwrap();
        let loaded = InstanceRegistry::load(&path).unwrap();
        assert_eq!(loaded.latest("stda").unwrap().version.to_string(), "1.6.1");
        assert_eq!(loaded.latest("stda").unwrap().exe, exe);
    }

    #[test]
    fn registry_snapshot() {
        let dir = tempfile::tempdir().unwrap();
        let exe = fake_exe(dir.path(), b"content");
        let mut reg = InstanceRegistry::new();
        reg.register("xtb", "6.7.1".parse().unwrap(), exe).unwrap();
        let entry = reg.latest("xtb").unwrap();
        // 只快照确定性字段：临时目录路径与 ULID 随机，不进入快照
        let stable = serde_json::json!({
            "name": entry.name,
            "version": entry.version.to_string(),
            "sha256": entry.sha256,
        });
        insta::assert_snapshot!(serde_json::to_string_pretty(&stable).unwrap());
    }
}
