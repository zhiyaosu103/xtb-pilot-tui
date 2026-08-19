//! 配置文件：TOML（规划文档 §2.2：serde + toml 足够，不引 config/figment）。
//!
//! 默认数据目录：`~/.local/share/xtbpilot`（`directories` 的 XDG 解析）。
//! 配置与路径中的 `~` 由 `shellexpand` 展开。

use crate::error::{CoreError, Result};
use directories::ProjectDirs;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// 应用数据目录名（规划文档 §2.2：`~/.local/share/xtbpilot` 等）。
pub const APP_DIR_NAME: &str = "xtbpilot";

/// 顶层配置。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    /// 组件登记表路径。
    pub registry_path: PathBuf,
    /// SQLite 数据库路径。
    pub db_path: PathBuf,
    /// per-job 日志目录。
    pub log_dir: PathBuf,
    /// rdkit helper 常驻进程入口（`conda run -n xtbp python -m <module>`）。
    pub helper_module: String,
    /// 子进程注入的环境变量（规划文档 §1.3：OMP/MKL/OPENBLAS 线程数等）。
    pub env: BTreeMap<String, String>,
}

impl Default for Config {
    fn default() -> Self {
        let data = default_data_dir();
        Self {
            registry_path: data.join("registry.toml"),
            db_path: data.join("xtbpilot.db"),
            log_dir: data.join("logs"),
            helper_module: "rdkit_helper".to_string(),
            env: BTreeMap::from([
                ("OMP_NUM_THREADS".into(), "1".into()),
                ("MKL_NUM_THREADS".into(), "1".into()),
                ("OPENBLAS_NUM_THREADS".into(), "1".into()),
            ]),
        }
    }
}

/// XDG 数据目录（`directories`）。
pub fn default_data_dir() -> PathBuf {
    ProjectDirs::from("", "", APP_DIR_NAME)
        .map(|d| d.data_dir().to_path_buf())
        .unwrap_or_else(|| PathBuf::from("."))
}

/// 展开路径中的 `~`（`shellexpand`）。
pub fn expand_tilde(path: &str) -> String {
    shellexpand::tilde(path).into_owned()
}

impl Config {
    /// 加载配置文件（展开 `~` 后返回）。
    pub fn load(path: &Path) -> Result<Self> {
        let raw = std::fs::read_to_string(path)
            .map_err(|e| CoreError::Config(format!("读取 {} 失败: {e}", path.display())))?;
        let mut cfg: Config = toml::from_str(&raw)?;
        cfg.expand();
        Ok(cfg)
    }

    /// 就地展开所有路径字段的 `~`。
    pub fn expand(&mut self) {
        self.registry_path = PathBuf::from(expand_tilde(&self.registry_path.to_string_lossy()));
        self.db_path = PathBuf::from(expand_tilde(&self.db_path.to_string_lossy()));
        self.log_dir = PathBuf::from(expand_tilde(&self.log_dir.to_string_lossy()));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_config_uses_xdg_data_dir() {
        let cfg = Config::default();
        assert!(cfg.registry_path.to_string_lossy().contains(APP_DIR_NAME));
        assert_eq!(cfg.helper_module, "rdkit_helper");
        assert!(cfg.env.contains_key("OMP_NUM_THREADS"));
    }

    #[test]
    fn load_expands_tilde() {
        let dir = tempfile::tempdir().unwrap();
        let cfg_path = dir.path().join("config.toml");
        std::fs::write(
            &cfg_path,
            "registry_path = \"~/.local/share/xtbpilot/registry.toml\"\n",
        )
        .unwrap();
        let cfg = Config::load(&cfg_path).unwrap();
        let s = cfg.registry_path.to_string_lossy();
        assert!(!s.starts_with('~'), "~ 未展开: {s}");
        assert!(s.starts_with('/'));
    }

    #[test]
    fn expand_tilde_helper() {
        assert_eq!(expand_tilde("~/x"), format!("{}/x", std::env::var("HOME").unwrap()));
        assert_eq!(expand_tilde("/abs/path"), "/abs/path");
    }
}
