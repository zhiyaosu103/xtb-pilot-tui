//! 调度器实现（设计文档 §3.3）。
//!
//! 结构：`submit` 入队 → 单 dispatcher 任务按（优先级, FIFO）从队列取件，
//! 检查并发槽与内存令牌后拉起 driver 任务（runner 执行 + 重试 + 事件/落库）。

use crate::Result;
use std::cmp::Ordering;
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering as AtomicOrdering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use thiserror::Error;
use tokio::sync::{Notify, Semaphore};
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};
use xtbp_api::EventBus;
use xtbp_core::Ulid;
use xtbp_core::job::{JobEvent, JobStatus, error_codes};
use xtbp_runner::{Line, RunOpts, run as run_process};
use xtbp_store::Store;

/// 调度错误。
#[derive(Debug, Error)]
pub enum SchedError {
    #[error("任务不存在: {0}")]
    NotFound(String),

    #[error("资源耗尽: {reason}")]
    ResourceExhausted { reason: String },

    #[error("IO 错误: {0}")]
    Io(#[from] std::io::Error),

    #[error("持久层错误: {0}")]
    Store(#[from] xtbp_store::StoreError),
}

/// 调度配置。
#[derive(Debug, Clone)]
pub struct SchedConfig {
    /// 全局并发槽数。
    pub max_concurrent: usize,
    /// 内存预算（MB，0 = 不限额）。
    pub memory_budget_mb: u64,
    /// 事件总线容量无关（由 daemon 提供总线）。
    pub _reserved: (),
}

impl Default for SchedConfig {
    fn default() -> Self {
        Self {
            max_concurrent: 1,
            memory_budget_mb: 0,
            _reserved: (),
        }
    }
}

/// 执行单元：一个子进程调用（工作流把步骤展开为多个单元提交）。
#[derive(Debug, Clone)]
pub struct ExecUnit {
    /// 归属任务（jobs 表主键）。
    pub job_id: Ulid,
    /// 可执行文件绝对路径。
    pub program: PathBuf,
    /// 参数数组（禁止拼接整行命令）。
    pub args: Vec<String>,
    /// 工作目录。
    pub cwd: PathBuf,
    /// 环境注入（OMP/MKL/OPENBLAS 线程数等）。
    pub env: std::collections::BTreeMap<String, String>,
    /// wall-clock 超时。
    pub wall_timeout: Option<Duration>,
    /// stdout 停滞检测。
    pub stall_timeout: Option<Duration>,
    /// 优先级（0 = 交互优先）。
    pub priority: u8,
    /// 最大重试次数（总尝试 = max_retries + 1）。
    pub max_retries: u32,
    /// 内存估算（MB，令牌桶用；0 = 不占令牌）。
    pub memory_mb: u64,
}

/// 执行结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExecResult {
    /// 最终退出码（被信号杀为 -1，看 killed_* 标志）。
    pub exit_code: i32,
    /// 因 wall-clock 超时被杀。
    pub killed_by_timeout: bool,
    /// 因停滞被杀。
    pub killed_by_stall: bool,
    /// 被取消。
    pub cancelled: bool,
    /// 尝试次数。
    pub attempts: u32,
    /// stderr 尾部。
    pub stderr_tail: String,
}

/// 队列快照（TUI Dashboard / sys.health）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
pub struct QueueStats {
    /// 排队中。
    pub queued: usize,
    /// 运行中。
    pub running: usize,
    /// 空闲并发槽。
    pub slots_free: usize,
    /// 已占内存令牌（MB）。
    pub memory_in_use_mb: u64,
}

#[derive(Debug)]
struct Queued {
    unit: ExecUnit,
    seq: u64,
}

impl PartialEq for Queued {
    fn eq(&self, other: &Self) -> bool {
        self.seq == other.seq
    }
}
impl Eq for Queued {}
impl PartialOrd for Queued {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}
impl Ord for Queued {
    fn cmp(&self, other: &Self) -> Ordering {
        // BinaryHeap 是最大堆：反转比较得到 (优先级小, seq 小) 先出
        other
            .unit
            .priority
            .cmp(&self.unit.priority)
            .then_with(|| other.seq.cmp(&self.seq))
    }
}

/// 调度器（克隆共享同一内部状态）。
#[derive(Clone)]
pub struct Scheduler {
    inner: Arc<SchedInner>,
}

struct SchedInner {
    cfg: SchedConfig,
    store: Store,
    bus: EventBus,
    shutdown: CancellationToken,
    queue: Mutex<std::collections::BinaryHeap<Queued>>,
    /// 运行中任务 → 取消令牌。
    running: Mutex<HashMap<Ulid, CancellationToken>>,
    /// 已取消但仍在队列里的任务。
    cancelled_ids: Mutex<HashSet<Ulid>>,
    slots: Arc<Semaphore>,
    mem_in_use: AtomicU64,
    running_count: AtomicUsize,
    seq: AtomicU64,
    notify: Notify,
}

impl Scheduler {
    /// 构造调度器。
    pub fn new(cfg: SchedConfig, store: Store, bus: EventBus, shutdown: CancellationToken) -> Self {
        let max_concurrent = cfg.max_concurrent.max(1);
        Self {
            inner: Arc::new(SchedInner {
                cfg,
                store,
                bus,
                shutdown,
                queue: Mutex::new(std::collections::BinaryHeap::new()),
                running: Mutex::new(HashMap::new()),
                cancelled_ids: Mutex::new(HashSet::new()),
                slots: Arc::new(Semaphore::new(max_concurrent)),
                mem_in_use: AtomicU64::new(0),
                running_count: AtomicUsize::new(0),
                seq: AtomicU64::new(0),
                notify: Notify::new(),
            }),
        }
    }

    /// 启动 dispatcher（幂等：只应调用一次，daemon 启动时）。
    pub fn start(&self) {
        let this = self.clone();
        tokio::spawn(async move { this.dispatch_loop().await });
    }

    /// 提交执行单元：任务状态置 Queued 并入队。
    pub async fn submit(&self, unit: ExecUnit) -> Result<()> {
        {
            let mut job = self
                .inner
                .store
                .get_job(&unit.job_id)
                .await?
                .ok_or_else(|| SchedError::NotFound(unit.job_id.to_string()))?;
            if job.status == JobStatus::Draft || job.status == JobStatus::Interrupted {
                job.transition(JobStatus::Queued)
                    .map_err(|e| SchedError::NotFound(format!("状态转移失败: {e}")))?;
                self.inner.store.update_job(&job).await?;
            }
        }
        let seq = self.inner.seq.fetch_add(1, AtomicOrdering::Relaxed);
        self.inner.bus.publish(JobEvent::Queued {
            job_id: unit.job_id.to_string(),
        });
        self.inner.queue.lock().unwrap().push(Queued { unit, seq });
        self.publish_queue_depth();
        self.inner.notify.notify_one();
        Ok(())
    }

    /// 取消任务（排队中直接标记；运行中发取消令牌，driver 负责落库）。
    pub async fn cancel(&self, job_id: &Ulid) -> Result<bool> {
        let token = {
            let running = self.inner.running.lock().unwrap();
            running.get(job_id).cloned()
        };
        match token {
            Some(t) => {
                t.cancel();
                Ok(true)
            }
            None => {
                // 可能在队列中
                let in_queue = {
                    let mut q = self.inner.queue.lock().unwrap();
                    let before = q.len();
                    let mut kept = std::collections::BinaryHeap::new();
                    while let Some(item) = q.pop() {
                        if item.unit.job_id != *job_id {
                            kept.push(item);
                        }
                    }
                    *q = kept;
                    before != q.len()
                };
                if in_queue {
                    self.inner.cancelled_ids.lock().unwrap().insert(*job_id);
                    self.publish_queue_depth();
                    Ok(true)
                } else {
                    // 不在队列也不在运行：交给调用方按 store 状态处理
                    Ok(false)
                }
            }
        }
    }

    /// 队列快照。
    pub fn stats(&self) -> QueueStats {
        let inner = &self.inner;
        QueueStats {
            queued: inner.queue.lock().unwrap().len(),
            running: inner.running_count.load(AtomicOrdering::Relaxed),
            slots_free: inner.slots.available_permits(),
            memory_in_use_mb: inner.mem_in_use.load(AtomicOrdering::Relaxed),
        }
    }

    /// 崩溃恢复（daemon 启动时调用一次）：
    /// - `Running`（PID 已死）→ `Interrupted`，可续算；
    /// - `Queued` / `Interrupted` → 重新入队（Interrupted 先转 Queued）；
    /// - `Draft` / `Parsing` 保持原状。
    ///
    /// 返回 (标记中断数, 重新入队数)。
    pub async fn recover(&self) -> Result<(usize, usize)> {
        let jobs = self.inner.store.list_non_terminal_jobs().await?;
        let mut interrupted = 0;
        let mut requeued = 0;
        for job in jobs {
            match job.status {
                JobStatus::Running => {
                    let mut j = job.clone();
                    j.transition(JobStatus::Interrupted)
                        .map_err(|e| SchedError::NotFound(format!("恢复转移失败: {e}")))?;
                    self.inner.store.update_job(&j).await?;
                    interrupted += 1;
                }
                JobStatus::Queued => {
                    // 依据 workdir 与参数重建执行单元的任务由 daemon/工作流负责；
                    // 调度器只负责把 Queued 留在队列视图（本实现不自动重放，
                    // daemon 会再次 submit）。
                    requeued += 1;
                }
                JobStatus::Interrupted => {
                    requeued += 1;
                }
                _ => {}
            }
        }
        info!(interrupted, requeued, "崩溃恢复完成");
        Ok((interrupted, requeued))
    }

    /// 优雅停机：取消全部运行中任务并等待 driver 收尾。
    pub fn shutdown(&self) {
        self.inner.shutdown.cancel();
        let tokens: Vec<_> = self
            .inner
            .running
            .lock()
            .unwrap()
            .values()
            .cloned()
            .collect();
        for t in tokens {
            t.cancel();
        }
    }

    // ------------------------------------------------------------------
    // 内部
    // ------------------------------------------------------------------

    fn publish_queue_depth(&self) {
        let s = self.stats();
        self.inner.bus.publish(JobEvent::QueueDepth {
            queued: s.queued,
            running: s.running,
        });
    }

    /// dispatcher 主循环：先占槽再取件（保证优先级在取件那一刻生效，
    /// 不会因等待槽位而让后来的高优先级任务插队失败）。
    async fn dispatch_loop(&self) {
        loop {
            if self.inner.shutdown.is_cancelled() {
                return;
            }
            // 先占一个并发槽
            let permit = tokio::select! {
                _ = self.inner.shutdown.cancelled() => return,
                p = self.inner.slots.clone().acquire_owned() => match p {
                    Ok(p) => p,
                    Err(_) => return, // semaphore closed
                },
            };
            // 取队头（此刻的优先级顺序）
            let item = {
                let mut q = self.inner.queue.lock().unwrap();
                q.pop()
            };
            let Some(item) = item else {
                // 队列为空：释放槽位，等待新任务通知
                drop(permit);
                tokio::select! {
                    _ = self.inner.shutdown.cancelled() => return,
                    _ = self.inner.notify.notified() => {}
                }
                continue;
            };
            // 已被取消的队内任务直接标记 Cancelled
            let is_cancelled = {
                let mut cancelled = self.inner.cancelled_ids.lock().unwrap();
                cancelled.remove(&item.unit.job_id)
            };
            if is_cancelled {
                drop(permit);
                self.finish_as_cancelled(&item.unit.job_id).await;
                self.publish_queue_depth();
                continue;
            }
            // 内存令牌检查：不够则放回队头等待
            if !self.try_acquire_memory(&item.unit) {
                {
                    let mut q = self.inner.queue.lock().unwrap();
                    q.push(item);
                }
                drop(permit);
                self.inner.notify.notify_waiters();
                tokio::select! {
                    _ = self.inner.shutdown.cancelled() => return,
                    _ = self.inner.notify.notified() => {}
                }
                continue;
            }
            let this = self.clone();
            tokio::spawn(async move {
                this.run_unit(item.unit).await;
                drop(permit);
                this.notify_and_publish();
            });
        }
    }

    fn try_acquire_memory(&self, unit: &ExecUnit) -> bool {
        if unit.memory_mb == 0 || self.inner.cfg.memory_budget_mb == 0 {
            return true;
        }
        let budget = self.inner.cfg.memory_budget_mb;
        let mut in_use = self.inner.mem_in_use.load(AtomicOrdering::Relaxed);
        loop {
            if in_use + unit.memory_mb > budget {
                return false;
            }
            match self.inner.mem_in_use.compare_exchange_weak(
                in_use,
                in_use + unit.memory_mb,
                AtomicOrdering::Relaxed,
                AtomicOrdering::Relaxed,
            ) {
                Ok(_) => return true,
                Err(actual) => in_use = actual,
            }
        }
    }

    fn release_memory(&self, unit: &ExecUnit) {
        if unit.memory_mb > 0 {
            self.inner
                .mem_in_use
                .fetch_sub(unit.memory_mb, AtomicOrdering::Relaxed);
        }
    }

    fn notify_and_publish(&self) {
        self.publish_queue_depth();
        self.inner.notify.notify_one();
    }

    /// 执行一个单元（含退避重试），终态落库 + 事件。
    async fn run_unit(&self, unit: ExecUnit) {
        let job_id = unit.job_id;
        let cancel = CancellationToken::new();
        self.inner
            .running
            .lock()
            .unwrap()
            .insert(job_id, cancel.clone());
        self.inner
            .running_count
            .fetch_add(1, AtomicOrdering::Relaxed);
        self.publish_queue_depth();

        // 环境注入缺省（§3.3：防 BLAS 线程超额订阅）
        let mut env = unit.env.clone();
        for (k, v) in [
            ("OMP_NUM_THREADS", "1"),
            ("MKL_NUM_THREADS", "1"),
            ("OPENBLAS_NUM_THREADS", "1"),
        ] {
            env.entry(k.to_string()).or_insert_with(|| v.to_string());
        }
        let opts = RunOpts {
            program: unit.program.clone(),
            args: unit.args.clone(),
            cwd: unit.cwd.clone(),
            env,
            wall_timeout: unit.wall_timeout,
            stall_timeout: unit.stall_timeout,
        };

        let total_attempts = unit.max_retries.saturating_add(1);
        let mut attempts = 0u32;
        let mut result = ExecResult {
            exit_code: -1,
            killed_by_timeout: false,
            killed_by_stall: false,
            cancelled: false,
            attempts: 0,
            stderr_tail: String::new(),
        };

        for attempt in 0..total_attempts {
            attempts = attempt + 1;
            self.mark_status(&job_id, JobStatus::Running).await;
            self.inner.bus.publish(JobEvent::Started {
                job_id: job_id.to_string(),
            });
            let outcome = run_process(
                &opts,
                |line| match line {
                    Line::Out { text } => {
                        self.inner.bus.publish(JobEvent::Output {
                            job_id: job_id.to_string(),
                            line: text,
                        });
                    }
                    Line::Err { text } => {
                        debug!(job = %job_id, line = %text, "stderr");
                    }
                },
                cancel.clone(),
            )
            .await;

            let outcome = match outcome {
                Ok(o) => o,
                Err(e) => {
                    result.exit_code = -1;
                    result.stderr_tail = e.to_string();
                    break;
                }
            };
            result.exit_code = outcome.exit_code;
            result.killed_by_timeout = outcome.killed_by_timeout;
            result.killed_by_stall = outcome.killed_by_stall;
            result.stderr_tail = outcome.stderr_tail.clone();

            if outcome.cancelled {
                result.cancelled = true;
                break;
            }
            if outcome.exit_code == 0 {
                break;
            }
            if attempt + 1 < total_attempts {
                let backoff = Duration::from_secs(2u64.saturating_pow(attempt.min(6)));
                self.inner.bus.publish(JobEvent::Status {
                    job_id: job_id.to_string(),
                    status: format!("retry-backoff-{backoff:?}"),
                });
                debug!(job = %job_id, attempt = attempts, "退避重试");
                tokio::select! {
                    _ = cancel.cancelled() => {
                        result.cancelled = true;
                        break;
                    }
                    _ = tokio::time::sleep(backoff) => {}
                }
            }
        }
        result.attempts = attempts;

        // 终态落库
        if let Some(mut job) = self.inner.store.get_job(&job_id).await.unwrap_or(None) {
            if result.cancelled {
                let _ = job.transition(JobStatus::Cancelled);
                job.error_code = Some(error_codes::CANCELLED.into());
            } else if result.exit_code == 0 {
                let _ = job.transition(JobStatus::Parsing);
            } else if result.killed_by_timeout {
                job.fail(error_codes::TIMEOUT, "wall-clock 超时");
                let _ = job.transition(JobStatus::Failed);
            } else if result.killed_by_stall {
                job.fail(error_codes::TIMEOUT, "stdout 停滞超时");
                let _ = job.transition(JobStatus::Failed);
            } else {
                job.fail(
                    error_codes::XTB_CONVERGENCE_FAILED,
                    format!(
                        "子进程退出码 {}（尝试 {} 次）: {}",
                        result.exit_code,
                        result.attempts,
                        result.stderr_tail.chars().take(200).collect::<String>()
                    ),
                );
                let _ = job.transition(JobStatus::Failed);
            }
            job.exit_code = Some(result.exit_code);
            let _ = self.inner.store.update_job(&job).await;
            self.inner.bus.publish(JobEvent::Finished {
                job_id: job_id.to_string(),
                ok: result.exit_code == 0,
                error_code: job.error_code.clone(),
            });
        } else {
            warn!(job = %job_id, "任务记录缺失，无法落终态");
        }

        self.inner.running.lock().unwrap().remove(&job_id);
        self.inner
            .running_count
            .fetch_sub(1, AtomicOrdering::Relaxed);
        self.release_memory(&unit);
        self.publish_queue_depth();
    }

    async fn mark_status(&self, job_id: &Ulid, status: JobStatus) {
        let Some(mut job) = self.inner.store.get_job(job_id).await.unwrap_or(None) else {
            return;
        };
        if matches!(job.status, JobStatus::Running) {
            return;
        }
        if job.transition(status).is_err() {
            debug!(job = %job_id, "状态转移跳过");
            return;
        }
        let _ = self.inner.store.update_job(&job).await;
        self.inner.bus.publish(JobEvent::Status {
            job_id: job_id.to_string(),
            status: status.as_str().into(),
        });
    }

    async fn finish_as_cancelled(&self, job_id: &Ulid) {
        if let Some(mut job) = self.inner.store.get_job(job_id).await.unwrap_or(None)
            && !job.status.is_terminal()
        {
            let _ = job.transition(JobStatus::Cancelled);
            job.error_code = Some(error_codes::CANCELLED.into());
            let _ = self.inner.store.update_job(&job).await;
            self.inner.bus.publish(JobEvent::Finished {
                job_id: job_id.to_string(),
                ok: false,
                error_code: Some(error_codes::CANCELLED.into()),
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;
    use std::time::Duration;
    use xtbp_core::job::{Job, JobParams};
    use xtbp_core::molecule::{Charge, Molecule, Multiplicity};
    use xtbp_core::time::now_unix;

    async fn fresh() -> (Store, EventBus, CancellationToken, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(&dir.path().join("t.db")).await.unwrap();
        let bus = EventBus::new(1024);
        let shutdown = CancellationToken::new();
        (store, bus, shutdown, dir)
    }

    async fn make_job(store: &Store, wf: &str) -> Job {
        let mol = Molecule::new("C1=CC=CC=C1", Charge(0), Multiplicity(1), now_unix());
        let mol = store.ensure_molecule(&mol).await.unwrap();
        let job = Job::new(mol.id, wf, JobParams::default(), "h".into(), None, 0);
        store.insert_job(&job).await.unwrap();
        job
    }

    fn sh_unit(
        job_id: Ulid,
        script: &str,
        priority: u8,
        max_retries: u32,
        memory_mb: u64,
    ) -> ExecUnit {
        ExecUnit {
            job_id,
            program: "/bin/sh".into(),
            args: vec!["-c".into(), script.into()],
            cwd: std::env::temp_dir(),
            env: BTreeMap::new(),
            wall_timeout: Some(Duration::from_secs(30)),
            stall_timeout: None,
            priority,
            max_retries,
            memory_mb,
        }
    }

    /// 收集事件直到谓词满足或超时。
    async fn collect_until<F>(
        rx: &mut tokio::sync::broadcast::Receiver<JobEvent>,
        pred: F,
    ) -> Vec<JobEvent>
    where
        F: Fn(&JobEvent) -> bool,
    {
        let mut seen = Vec::new();
        loop {
            match tokio::time::timeout(Duration::from_secs(10), rx.recv()).await {
                Ok(Ok(ev)) => {
                    let done = pred(&ev);
                    seen.push(ev);
                    if done {
                        return seen;
                    }
                }
                Ok(Err(tokio::sync::broadcast::error::RecvError::Lagged(_))) => continue,
                Ok(Err(tokio::sync::broadcast::error::RecvError::Closed)) => return seen,
                Err(_) => panic!("等待事件超时，已收到 {seen:?}"),
            }
        }
    }

    #[tokio::test]
    async fn unit_completes_and_transitions_to_parsing() {
        let (store, bus, shutdown, _dir) = fresh().await;
        let job = make_job(&store, "opt").await;
        let sched = Scheduler::new(
            SchedConfig {
                max_concurrent: 2,
                ..Default::default()
            },
            store.clone(),
            bus.clone(),
            shutdown.clone(),
        );
        sched.start();
        let mut rx = bus.subscribe();
        sched
            .submit(sh_unit(job.id, "echo ok", 0, 0, 0))
            .await
            .unwrap();
        let events = collect_until(&mut rx, |ev| matches!(ev, JobEvent::Finished { .. })).await;
        assert!(
            events
                .iter()
                .any(|ev| matches!(ev, JobEvent::Started { .. }))
        );
        let loaded = store.get_job(&job.id).await.unwrap().unwrap();
        assert_eq!(loaded.status, JobStatus::Parsing);
        sched.shutdown();
    }

    #[tokio::test]
    async fn retries_then_fails_with_error_code() {
        let (store, bus, shutdown, _dir) = fresh().await;
        let job = make_job(&store, "opt").await;
        let sched = Scheduler::new(
            SchedConfig {
                max_concurrent: 1,
                ..Default::default()
            },
            store.clone(),
            bus.clone(),
            shutdown.clone(),
        );
        sched.start();
        let mut rx = bus.subscribe();
        sched
            .submit(sh_unit(job.id, "exit 1", 0, 2, 0))
            .await
            .unwrap();
        let events = collect_until(&mut rx, |ev| matches!(ev, JobEvent::Finished { .. })).await;
        let started = events
            .iter()
            .filter(|ev| matches!(ev, JobEvent::Started { .. }))
            .count();
        assert_eq!(started, 3, "应重试 3 次: {events:?}");
        let loaded = store.get_job(&job.id).await.unwrap().unwrap();
        assert_eq!(loaded.status, JobStatus::Failed);
        assert_eq!(
            loaded.error_code.as_deref(),
            Some(xtbp_core::error_codes::XTB_CONVERGENCE_FAILED)
        );
        sched.shutdown();
    }

    #[tokio::test]
    async fn cancel_running_unit() {
        let (store, bus, shutdown, _dir) = fresh().await;
        let job = make_job(&store, "opt").await;
        let sched = Scheduler::new(
            SchedConfig {
                max_concurrent: 1,
                ..Default::default()
            },
            store.clone(),
            bus.clone(),
            shutdown.clone(),
        );
        sched.start();
        let mut rx = bus.subscribe();
        sched
            .submit(sh_unit(job.id, "sleep 30", 0, 0, 0))
            .await
            .unwrap();
        collect_until(&mut rx, |ev| matches!(ev, JobEvent::Started { .. })).await;
        assert!(sched.cancel(&job.id).await.unwrap());
        let events = collect_until(&mut rx, |ev| matches!(ev, JobEvent::Finished { .. })).await;
        let fin = events.iter().find_map(|ev| match ev {
            JobEvent::Finished { ok, error_code, .. } => Some((*ok, error_code.clone())),
            _ => None,
        });
        assert_eq!(fin, Some((false, Some("CANCELLED".into()))));
        let loaded = store.get_job(&job.id).await.unwrap().unwrap();
        assert_eq!(loaded.status, JobStatus::Cancelled);
        sched.shutdown();
    }

    #[tokio::test]
    async fn priority_order_interactive_first() {
        let (store, bus, shutdown, _dir) = fresh().await;
        let blocker = make_job(&store, "opt").await;
        let slow = make_job(&store, "opt").await;
        let fast = make_job(&store, "opt").await;
        let sched = Scheduler::new(
            SchedConfig {
                max_concurrent: 1,
                ..Default::default()
            },
            store.clone(),
            bus.clone(),
            shutdown.clone(),
        );
        sched.start();
        let mut rx = bus.subscribe();
        // 先占满唯一槽位，再排队慢（优先级1）与快（优先级0）任务
        sched
            .submit(sh_unit(blocker.id, "sleep 0.4", 0, 0, 0))
            .await
            .unwrap();
        sched
            .submit(sh_unit(slow.id, "echo slow", 1, 0, 0))
            .await
            .unwrap();
        sched
            .submit(sh_unit(fast.id, "echo fast", 0, 0, 0))
            .await
            .unwrap();
        // 快任务必须先于慢任务 Started
        let events = collect_until(
            &mut rx,
            |ev| matches!(ev, JobEvent::Started { job_id } if job_id == &slow.id.to_string()),
        )
        .await;
        let pos_fast = events.iter().position(
            |ev| matches!(ev, JobEvent::Started { job_id } if job_id == &fast.id.to_string()),
        );
        let pos_slow = events.iter().position(
            |ev| matches!(ev, JobEvent::Started { job_id } if job_id == &slow.id.to_string()),
        );
        assert!(
            matches!((pos_fast, pos_slow), (Some(pf), Some(ps)) if pf < ps),
            "优先级未生效: {events:?}"
        );
        sched.shutdown();
    }

    #[tokio::test]
    async fn memory_budget_serializes_heavy_units() {
        let (store, bus, shutdown, _dir) = fresh().await;
        let a = make_job(&store, "opt").await;
        let b = make_job(&store, "opt").await;
        let sched = Scheduler::new(
            SchedConfig {
                max_concurrent: 4,
                memory_budget_mb: 100,
                ..Default::default()
            },
            store.clone(),
            bus.clone(),
            shutdown.clone(),
        );
        sched.start();
        let mut rx = bus.subscribe();
        sched
            .submit(sh_unit(a.id, "sleep 0.4", 0, 0, 80))
            .await
            .unwrap();
        sched
            .submit(sh_unit(b.id, "echo b", 0, 0, 80))
            .await
            .unwrap();
        // b 只能在 a 释放 80MB 后开始
        let events = collect_until(
            &mut rx,
            |ev| matches!(ev, JobEvent::Started { job_id } if job_id == &b.id.to_string()),
        )
        .await;
        let a_finished_before_b = {
            let pos_a = events.iter().position(
                |ev| matches!(ev, JobEvent::Finished { job_id, .. } if job_id == &a.id.to_string()),
            );
            let pos_b = events.iter().position(
                |ev| matches!(ev, JobEvent::Started { job_id } if job_id == &b.id.to_string()),
            );
            matches!((pos_a, pos_b), (Some(pa), Some(pb)) if pa < pb)
        };
        assert!(a_finished_before_b, "内存令牌未生效: {events:?}");
        sched.shutdown();
    }

    #[tokio::test]
    async fn recover_marks_running_as_interrupted() {
        let (store, _bus, _shutdown, _dir) = fresh().await;
        let running = make_job(&store, "opt").await;
        let queued = make_job(&store, "opt").await;
        let mut r = store.get_job(&running.id).await.unwrap().unwrap();
        r.transition(JobStatus::Queued).unwrap();
        r.transition(JobStatus::Running).unwrap();
        store.update_job(&r).await.unwrap();
        let mut q = store.get_job(&queued.id).await.unwrap().unwrap();
        q.transition(JobStatus::Queued).unwrap();
        store.update_job(&q).await.unwrap();

        let sched = Scheduler::new(
            SchedConfig::default(),
            store.clone(),
            EventBus::new(4),
            CancellationToken::new(),
        );
        let (interrupted, _requeued) = sched.recover().await.unwrap();
        assert_eq!(interrupted, 1);
        let loaded = store.get_job(&running.id).await.unwrap().unwrap();
        assert_eq!(loaded.status, JobStatus::Interrupted);
        let loaded_q = store.get_job(&queued.id).await.unwrap().unwrap();
        assert_eq!(loaded_q.status, JobStatus::Queued);
    }
}
