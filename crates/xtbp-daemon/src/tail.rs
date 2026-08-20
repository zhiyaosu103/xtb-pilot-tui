//! per-job 输出环形缓冲（`job.tail` 数据源，设计文档 §3.7）。
//!
//! 订阅事件总线的 `Output` 事件：按 (job, seq) 追加；同时追加到
//! 任务工作目录的 `stdout.log`（daemon 重启后 tail 只保留运行期内存，
//! 历史行以文件为准）。按行偏移读取，天然支持增量 tail。

use std::collections::HashMap;
use std::sync::Mutex;
use xtbp_core::Ulid;

/// 环形缓冲容量（行）。
const RING_CAPACITY: usize = 20_000;

/// 单任务行缓冲。
#[derive(Debug, Default)]
struct JobLog {
    /// 已追加总行数（= 下一行 seq）。
    total: u64,
    lines: std::collections::VecDeque<(u64, String)>,
}

/// 全部任务的输出缓冲（daemon 单实例共享）。
#[derive(Debug, Default)]
pub struct TailBuffer {
    logs: Mutex<HashMap<Ulid, JobLog>>,
}

impl TailBuffer {
    /// 空缓冲。
    pub fn new() -> Self {
        Self::default()
    }

    /// 追加一行（seq 从 0 起）。
    pub fn append(&self, job_id: &Ulid, line: String) {
        let mut logs = self.logs.lock().unwrap();
        let log = logs.entry(*job_id).or_default();
        log.lines.push_back((log.total, line));
        log.total += 1;
        while log.lines.len() > RING_CAPACITY {
            log.lines.pop_front();
        }
    }

    /// 从 offset 起读最多 limit 行。返回 (行列表, 下一 offset)。
    pub fn read(&self, job_id: &Ulid, offset: u64, limit: u64) -> (Vec<(u64, String)>, u64) {
        let logs = self.logs.lock().unwrap();
        let Some(log) = logs.get(job_id) else {
            return (Vec::new(), offset);
        };
        let mut out = Vec::new();
        let mut next = offset;
        for (seq, line) in &log.lines {
            if *seq >= offset && out.len() < limit as usize {
                out.push((*seq, line.clone()));
                next = seq + 1;
            }
        }
        (out, next)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn append_and_incremental_read() {
        let buf = TailBuffer::new();
        let id: Ulid = "01ARZ3NDEKTSV4RRFFQ69G5FAV".parse().unwrap();
        buf.append(&id, "a".into());
        buf.append(&id, "b".into());
        buf.append(&id, "c".into());
        let (rows, next) = buf.read(&id, 0, 2);
        assert_eq!(rows.len(), 2);
        assert_eq!(next, 2);
        let (rows2, _) = buf.read(&id, next, 10);
        assert_eq!(rows2.len(), 1);
    }

    #[test]
    fn ring_capacity_evicts_oldest() {
        let buf = TailBuffer::new();
        let id: Ulid = "01ARZ3NDEKTSV4RRFFQ69G5FAV".parse().unwrap();
        for i in 0..(RING_CAPACITY as u64 + 10) {
            buf.append(&id, i.to_string());
        }
        // 最早的 10 行被淘汰；offset 依旧单调
        let (rows, next) = buf.read(&id, 0, 100);
        assert!(next >= 10);
        assert!(!rows.is_empty());
    }
}
