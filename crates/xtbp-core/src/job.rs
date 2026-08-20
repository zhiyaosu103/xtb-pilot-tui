//! 领域模型：Job 状态机与任务记录（设计文档 §3.3）。
//!
//! 生命周期：`Draft → Queued → Running → Parsing → Done | Failed |
//! Cancelled | Interrupted`。转移合法性由 [`JobStatus::can_transition_to`]
//! 强制；全部状态落 SQLite（xtbp-store）。

use crate::id::Ulid;
use crate::method::Method;
use crate::time::now_unix;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::str::FromStr;

/// Job 状态。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum JobStatus {
    /// 草稿（已入库、未入队）。
    Draft,
    /// 排队中。
    Queued,
    /// 运行中。
    Running,
    /// 解析回收中。
    Parsing,
    /// 成功完成。
    Done,
    /// 失败（不可重试或重试耗尽）。
    Failed,
    /// 用户取消。
    Cancelled,
    /// 被中断（崩溃/断电），可续算。
    Interrupted,
}

impl JobStatus {
    /// 数据库/协议中的字符串形式。
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Draft => "draft",
            Self::Queued => "queued",
            Self::Running => "running",
            Self::Parsing => "parsing",
            Self::Done => "done",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
            Self::Interrupted => "interrupted",
        }
    }

    /// 是否为终态（不再被调度器触碰）。
    pub fn is_terminal(&self) -> bool {
        matches!(self, Self::Done | Self::Failed | Self::Cancelled)
    }

    /// 转移是否合法（状态机白名单）。
    pub fn can_transition_to(&self, next: Self) -> bool {
        use JobStatus::*;
        matches!(
            (self, next),
            (Draft, Queued | Cancelled)
                | (Queued, Running | Cancelled)
                | (Running, Parsing | Failed | Cancelled | Interrupted)
                | (Parsing, Done | Failed)
                | (Interrupted, Queued) // 续算重新入队
        )
    }
}

impl FromStr for JobStatus {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "draft" => Ok(Self::Draft),
            "queued" => Ok(Self::Queued),
            "running" => Ok(Self::Running),
            "parsing" => Ok(Self::Parsing),
            "done" => Ok(Self::Done),
            "failed" => Ok(Self::Failed),
            "cancelled" => Ok(Self::Cancelled),
            "interrupted" => Ok(Self::Interrupted),
            other => Err(format!("未知任务状态: {other}")),
        }
    }
}

/// 结构化错误码（设计文档 §3.7 契约，API 与 DB 共用）。
pub mod error_codes {
    /// 收敛失败（xtb SCF/几何优化）。
    pub const XTB_CONVERGENCE_FAILED: &str = "XTB_CONVERGENCE_FAILED";
    /// 资源耗尽（并发槽/内存令牌/磁盘）。
    pub const RESOURCE_EXHAUSTED: &str = "RESOURCE_EXHAUSTED";
    /// 无效 SMILES（RDKit）。
    pub const RDKIT_INVALID_SMILES: &str = "RDKIT_INVALID_SMILES";
    /// 组件未登记/不可用。
    pub const COMPONENT_UNAVAILABLE: &str = "COMPONENT_UNAVAILABLE";
    /// 任务被取消。
    pub const CANCELLED: &str = "CANCELLED";
    /// 超时（wall-clock）。
    pub const TIMEOUT: &str = "TIMEOUT";
    /// 解析降级（结果可能缺失）。
    pub const PARSE_DEGRADED: &str = "PARSE_DEGRADED";
    /// 任务不存在。
    pub const JOB_NOT_FOUND: &str = "JOB_NOT_FOUND";
    /// 鉴权失败。
    pub const AUTH_FAILED: &str = "AUTH_FAILED";
    /// 无效参数。
    pub const INVALID_PARAMS: &str = "INVALID_PARAMS";
}

/// 任务事件（订阅推送：`job.events`，设计文档 §3.7）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "kebab-case")]
pub enum JobEvent {
    /// 入队。
    Queued {
        /// 任务 id。
        job_id: String,
    },
    /// 开始运行。
    Started {
        /// 任务 id。
        job_id: String,
    },
    /// 输出增量（stdout 一行）。
    Output {
        /// 任务 id。
        job_id: String,
        /// 行内容。
        line: String,
    },
    /// 状态变化（通用）。
    Status {
        /// 任务 id。
        job_id: String,
        /// 新状态。
        status: String,
    },
    /// 完成。
    Finished {
        /// 任务 id。
        job_id: String,
        /// 是否成功。
        ok: bool,
        /// 结构化错误码（失败时）。
        error_code: Option<String>,
    },
    /// 队列深度变化。
    QueueDepth {
        /// 排队中的任务数。
        queued: usize,
        /// 运行中的任务数。
        running: usize,
    },
}

/// 单任务参数快照（组装进 job.toml，§3.1）。
///
/// 同一 (inchikey, 方法, 参数, 组件版本) 组合哈希为 `content_hash`，
/// 重复提交命中缓存直接复用结果（幂等）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(default)]
pub struct JobParams {
    /// 计算方法。
    pub method: Method,
    /// 电荷（与分子记录一致，但快照自带一份）。
    pub charge: i8,
    /// 多重度。
    pub multiplicity: u8,
    /// OMP 线程数。
    pub threads: u32,
    /// wall-clock 超时（秒）。
    pub wall_timeout_secs: u64,
    /// stdout 停滞检测（秒，0 = 禁用）。
    pub stall_timeout_secs: u64,
    /// 最大重试次数。
    pub max_retries: u32,
    /// 组件版本 pin（组件名 → 版本，如 "xtb" → "6.7.1"）。
    pub component_versions: BTreeMap<String, String>,
    /// 模板扩展参数（工作流特定）。
    pub extra: BTreeMap<String, serde_json::Value>,
}

impl Default for JobParams {
    fn default() -> Self {
        Self {
            method: Method::gfn2(),
            charge: 0,
            multiplicity: 1,
            threads: 1,
            wall_timeout_secs: 3600,
            stall_timeout_secs: 300,
            max_retries: 2,
            component_versions: BTreeMap::new(),
            extra: BTreeMap::new(),
        }
    }
}

/// 任务记录（`jobs` 表行 + DAG 边）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Job {
    /// 主键。
    pub id: Ulid,
    /// 所属分子。
    pub molecule_id: Ulid,
    /// 工作流模板 id（如 "opt"、"excited"）。
    pub workflow: String,
    /// 参数快照。
    pub params: JobParams,
    /// 内容哈希（幂等去重键）。
    pub content_hash: String,
    /// 当前状态。
    pub status: JobStatus,
    /// 优先级（0 = 交互，越大越后）。
    pub priority: u8,
    /// 已尝试次数（0 起）。
    pub attempt: u32,
    /// 计算目录（Linux 文件系统内，严禁 /mnt/c）。
    pub workdir: Option<String>,
    /// 创建时间（Unix 秒）。
    pub created_at: i64,
    /// 最后更新时间（Unix 秒）。
    pub updated_at: i64,
    /// 开始时间。
    pub started_at: Option<i64>,
    /// 结束时间。
    pub finished_at: Option<i64>,
    /// 子进程退出码（终态后有效）。
    pub exit_code: Option<i32>,
    /// 结构化错误码（error_codes::*）。
    pub error_code: Option<String>,
    /// 错误详情。
    pub error_message: Option<String>,
    /// DAG 父任务（无父为 None）。
    pub parent_id: Option<Ulid>,
    /// 解析降级标记（结果可能部分缺失）。
    pub parse_degraded: bool,
}

impl Job {
    /// 新建草稿任务。
    pub fn new(
        molecule_id: Ulid,
        workflow: impl Into<String>,
        params: JobParams,
        content_hash: String,
        parent_id: Option<Ulid>,
        priority: u8,
    ) -> Self {
        let now = now_unix();
        Self {
            id: Ulid::new(),
            molecule_id,
            workflow: workflow.into(),
            params,
            content_hash,
            status: JobStatus::Draft,
            priority,
            attempt: 0,
            workdir: None,
            created_at: now,
            updated_at: now,
            started_at: None,
            finished_at: None,
            exit_code: None,
            error_code: None,
            error_message: None,
            parent_id,
            parse_degraded: false,
        }
    }

    /// 尝试状态转移；非法转移报错（状态机约束）。
    pub fn transition(&mut self, next: JobStatus) -> Result<(), String> {
        if !self.status.can_transition_to(next) {
            return Err(format!(
                "非法状态转移: {} → {}",
                self.status.as_str(),
                next.as_str()
            ));
        }
        let now = now_unix();
        self.status = next;
        self.updated_at = now;
        match next {
            JobStatus::Running => self.started_at = Some(now),
            JobStatus::Done | JobStatus::Failed | JobStatus::Cancelled => {
                self.finished_at = Some(now)
            }
            _ => {}
        }
        Ok(())
    }

    /// 记录失败（结构化错误码 + 消息）。
    pub fn fail(&mut self, code: impl Into<String>, message: impl Into<String>) {
        self.error_code = Some(code.into());
        self.error_message = Some(message.into());
        self.updated_at = now_unix();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::molecule::{Charge, Molecule, Multiplicity};

    fn dummy_job() -> Job {
        let mol = Molecule::new("C", Charge(0), Multiplicity(1), 0);
        Job::new(mol.id, "opt", JobParams::default(), "hash".into(), None, 0)
    }

    #[test]
    fn happy_path_transitions() {
        let mut job = dummy_job();
        assert_eq!(job.status, JobStatus::Draft);
        job.transition(JobStatus::Queued).unwrap();
        job.transition(JobStatus::Running).unwrap();
        assert!(job.started_at.is_some());
        job.transition(JobStatus::Parsing).unwrap();
        job.transition(JobStatus::Done).unwrap();
        assert!(job.finished_at.is_some());
        assert!(job.status.is_terminal());
    }

    #[test]
    fn illegal_transition_rejected() {
        let mut job = dummy_job();
        let err = job.transition(JobStatus::Done).unwrap_err();
        assert!(err.contains("非法状态转移"));
        assert_eq!(job.status, JobStatus::Draft);
    }

    #[test]
    fn interrupted_can_requeue() {
        let mut job = dummy_job();
        job.transition(JobStatus::Queued).unwrap();
        job.transition(JobStatus::Running).unwrap();
        job.transition(JobStatus::Interrupted).unwrap();
        job.transition(JobStatus::Queued).unwrap(); // 续算
        assert_eq!(job.status, JobStatus::Queued);
    }

    #[test]
    fn fail_records_structured_code() {
        let mut job = dummy_job();
        job.fail(error_codes::TIMEOUT, "wall-clock 超时");
        assert_eq!(job.error_code.as_deref(), Some("TIMEOUT"));
    }

    #[test]
    fn status_as_str_matches_protocol() {
        assert_eq!(JobStatus::Done.as_str(), "done");
        assert_eq!(JobStatus::Interrupted.as_str(), "interrupted");
    }

    #[test]
    fn status_from_str_roundtrips() {
        for s in [
            JobStatus::Draft,
            JobStatus::Queued,
            JobStatus::Running,
            JobStatus::Parsing,
            JobStatus::Done,
            JobStatus::Failed,
            JobStatus::Cancelled,
            JobStatus::Interrupted,
        ] {
            assert_eq!(s.as_str().parse::<JobStatus>().unwrap(), s);
        }
        assert!("weird".parse::<JobStatus>().is_err());
    }
}
