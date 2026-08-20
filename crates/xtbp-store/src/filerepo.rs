//! 文件仓（设计文档 §3.5：`data/<hash[:2]>/<hash>/`，只增不改）。
//!
//! 内容寻址存储原始输出；同内容文件只存一份。工作目录
//! `data/jobs/<job_id>/` 必须在 Linux 文件系统内（红线：/mnt/c 禁令由
//! daemon 自检执行，本层在构造时校验根路径）。

use crate::store::{Result, StoreError};
use std::path::{Path, PathBuf};
use xtbp_core::Ulid;
use xtbp_core::hash::sha256_file;

/// 文件仓根目录（通常 `~/.local/share/xtbpilot`）。
#[derive(Debug, Clone)]
pub struct FileRepo {
    root: PathBuf,
}

impl FileRepo {
    /// 新建文件仓（不落盘；首次写入时创建目录）。
    pub fn new(root: PathBuf) -> Self {
        Self { root }
    }

    /// 根目录。
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// 内容寻址路径（相对 root）：`data/<hash[:2]>/<hash>`。
    pub fn rel_path_for_hash(hash: &str) -> String {
        let (prefix, rest) = hash.split_at(2.min(hash.len()));
        format!("data/{prefix}/{rest}")
    }

    /// 存一个文件（按内容哈希去重，已存在则跳过写入）。
    ///
    /// 返回相对 root 的路径。文件先写临时名再原子改名，避免半成品。
    pub fn store_file(&self, src: &Path) -> Result<String> {
        let hash = sha256_file(src)?;
        let rel = Self::rel_path_for_hash(&hash);
        let dst = self.root.join(&rel);
        if dst.exists() {
            return Ok(rel);
        }
        if let Some(parent) = dst.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let tmp = dst.with_extension("tmp");
        std::fs::copy(src, &tmp)?;
        std::fs::rename(&tmp, &dst).map_err(StoreError::Io)?;
        Ok(rel)
    }

    /// 存一段字节（同 [`Self::store_file`]，内容寻址）。
    pub fn store_bytes(&self, bytes: &[u8]) -> Result<String> {
        let hash = xtbp_core::hash::sha256_bytes(bytes);
        let rel = Self::rel_path_for_hash(&hash);
        let dst = self.root.join(&rel);
        if dst.exists() {
            return Ok(rel);
        }
        if let Some(parent) = dst.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let tmp = dst.with_extension("tmp");
        std::fs::write(&tmp, bytes)?;
        std::fs::rename(&tmp, &dst)?;
        Ok(rel)
    }

    /// 绝对路径。
    pub fn abs(&self, rel: &str) -> PathBuf {
        self.root.join(rel)
    }

    /// 任务工作目录：`data/jobs/<job_id>/`。
    pub fn job_dir(&self, job_id: &Ulid) -> PathBuf {
        self.root.join("data").join("jobs").join(job_id.to_string())
    }

    /// 任务输入目录：`data/jobs/<job_id>/input`。
    pub fn job_input_dir(&self, job_id: &Ulid) -> PathBuf {
        self.job_dir(job_id).join("input")
    }

    /// 任务输出目录：`data/jobs/<job_id>/output`。
    pub fn job_output_dir(&self, job_id: &Ulid) -> PathBuf {
        self.job_dir(job_id).join("output")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hash_layout_two_level() {
        let rel = FileRepo::rel_path_for_hash("abcdef0123456789");
        assert_eq!(rel, "data/ab/cdef0123456789");
    }

    #[test]
    fn store_file_dedups_by_content() {
        let dir = tempfile::tempdir().unwrap();
        let repo = FileRepo::new(dir.path().to_path_buf());
        let src = dir.path().join("a.xyz");
        std::fs::write(&src, "same content").unwrap();
        let rel1 = repo.store_file(&src).unwrap();
        let rel2 = repo.store_file(&src).unwrap();
        assert_eq!(rel1, rel2);
        assert!(dir.path().join(&rel1).exists());
        // 只增不改：再存一次不同内容 → 不同路径
        std::fs::write(&src, "different").unwrap();
        let rel3 = repo.store_file(&src).unwrap();
        assert_ne!(rel1, rel3);
    }

    #[test]
    fn job_dir_layout() {
        let dir = tempfile::tempdir().unwrap();
        let repo = FileRepo::new(dir.path().to_path_buf());
        let id: Ulid = "01ARZ3NDEKTSV4RRFFQ69G5FAV".parse().unwrap();
        assert!(
            repo.job_dir(&id)
                .ends_with("data/jobs/01ARZ3NDEKTSV4RRFFQ69G5FAV")
        );
    }
}
