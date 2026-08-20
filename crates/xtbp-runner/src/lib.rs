//! xTB-Pilot 子进程封装（设计文档 §3.3：调度器的执行原语）。
//!
//! 职责边界：本 crate 是**唯一**允许构造外部命令的地方（开发约束 §6），
//! 参数以数组传递、禁止字符串拼接。提供：
//!
//! - spawn（独立进程组 `setsid`，kill 树一次带走全部子进程）；
//! - wall-clock 超时（单调时钟）与 stdout 停滞检测；
//! - stdout/stderr 流式逐行回调（调度器转发订阅推送）；
//! - 取消令牌优雅 kill；`ulimit -s unlimited` 经 wrapper shell 注入。

use serde::Serialize;
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::Duration;
use thiserror::Error;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::{Child, Command};
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;
use tracing::warn;

/// 子进程运行错误。
#[derive(Debug, Error)]
pub enum RunnerError {
    #[error("IO 错误: {0}")]
    Io(#[from] std::io::Error),

    #[error("启动子进程失败: {program}: {source}")]
    Spawn {
        /// 可执行文件路径。
        program: String,
        /// 底层错误。
        source: std::io::Error,
    },

    #[error("任务超时（上限 {limit_secs}s）")]
    Timeout { limit_secs: u64 },

    #[error("子进程非零退出: code={code}, stderr 尾部: {tail}")]
    NonZeroExit { code: i32, tail: String },

    #[error("stdout 流通道意外关闭")]
    StreamGone,
}

/// 运行器便捷 Result 别名。
pub type Result<T> = std::result::Result<T, RunnerError>;

/// 一行输出（stdout 或 stderr）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "stream", rename_all = "lowercase")]
pub enum Line {
    /// stdout 一行。
    Out { text: String },
    /// stderr 一行。
    Err { text: String },
}

impl Line {
    /// 行文本。
    pub fn text(&self) -> &str {
        match self {
            Self::Out { text } | Self::Err { text } => text,
        }
    }
}

/// 运行参数。
#[derive(Debug, Clone)]
pub struct RunOpts {
    /// 可执行文件绝对路径（来自 InstanceRegistry，不依赖 PATH 运气）。
    pub program: PathBuf,
    /// 参数数组（禁止拼接整行命令）。
    pub args: Vec<String>,
    /// 工作目录（组件在此 chdir 执行）。
    pub cwd: PathBuf,
    /// 环境变量注入（合并进继承环境，子进程专用非全局）。
    pub env: BTreeMap<String, String>,
    /// wall-clock 超时。
    pub wall_timeout: Option<Duration>,
    /// stdout 停滞检测（N 秒无输出 → kill + 退避重试由调度器决定）。
    pub stall_timeout: Option<Duration>,
}

/// 运行结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunOutcome {
    /// 退出码（被信号杀死时为 128+signal 约定，见 killed_* 标志）。
    pub exit_code: i32,
    /// 是否因 wall-clock 超时被杀。
    pub killed_by_timeout: bool,
    /// 是否因 stdout 停滞被杀。
    pub killed_by_stall: bool,
    /// 是否因取消令牌被杀。
    pub cancelled: bool,
    /// stderr 尾部（诊断用，最多 2KB）。
    pub stderr_tail: String,
}

/// 运行一个外部命令（wrapper shell 注入 ulimit，参数数组透传）。
///
/// `on_line` 在行到达时同步调用（同一任务内）；返回 [`RunOutcome`]。
/// 非零退出码不在此处判错——由调用方（调度器/工作流）决定策略；
/// 超时/停滞/取消以 [`RunOutcome`] 标志区分。
pub async fn run<F>(opts: &RunOpts, mut on_line: F, cancel: CancellationToken) -> Result<RunOutcome>
where
    F: FnMut(Line) + Send,
{
    let child = spawn(&opts.program, &opts.args, &opts.cwd, &opts.env)?;
    supervise(child, opts, &mut on_line, cancel).await
}

/// spawn：独立进程组 + 管道流 + ulimit wrapper。
fn spawn(
    program: &std::path::Path,
    args: &[String],
    cwd: &std::path::Path,
    env: &BTreeMap<String, String>,
) -> Result<Child> {
    // 快速失败：可执行文件必须存在（登记表已校验，此处兜底）
    if !program.is_file() {
        return Err(RunnerError::Spawn {
            program: program.display().to_string(),
            source: std::io::Error::new(std::io::ErrorKind::NotFound, "可执行文件不存在"),
        });
    }
    // wrapper：ulimit -s unlimited 后 exec "$@"（argv 透传，无用户输入拼接）
    let mut cmd = Command::new("/bin/sh");
    cmd.arg("-c")
        .arg("ulimit -s unlimited 2>/dev/null; exec \"$@\"")
        .arg("xtbp-runner")
        .arg(program)
        .args(args)
        .current_dir(cwd)
        .envs(env)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        // 独立进程组：kill(-pgid) 一次带走全部子进程
        .process_group(0)
        .kill_on_drop(true);
    tracing::debug!(
        program = %program.display(),
        cwd = %cwd.display(),
        env = ?env,
        args = ?args,
        "spawn 外部命令"
    );
    cmd.spawn().map_err(|source| RunnerError::Spawn {
        program: program.display().to_string(),
        source,
    })
}

/// 监督循环：超时/停滞/取消/退出四路竞争，逐行回调。
async fn supervise<F>(
    mut child: Child,
    opts: &RunOpts,
    on_line: &mut F,
    cancel: CancellationToken,
) -> Result<RunOutcome>
where
    F: FnMut(Line) + Send,
{
    let pid = child.id().ok_or_else(|| RunnerError::Spawn {
        program: "?".into(),
        source: std::io::Error::other("子进程无 pid"),
    })?;
    let started = Instant::now();
    let wall_deadline = opts.wall_timeout.map(|d| started + d);
    let mut last_output = started;
    let mut stderr_tail = String::new();

    // stdout / stderr 逐行泵（独立任务，避免缓冲阻塞子进程）
    let (line_tx, mut line_rx) = tokio::sync::mpsc::unbounded_channel::<Line>();
    let stdout = child.stdout.take();
    let stderr = child.stderr.take();
    if let Some(pipe) = stdout {
        let line_tx = line_tx.clone();
        tokio::spawn(async move {
            let mut reader = BufReader::new(pipe).lines();
            while let Ok(Some(line)) = reader.next_line().await {
                if line_tx.send(Line::Out { text: line }).is_err() {
                    break;
                }
            }
        });
    }
    if let Some(pipe) = stderr {
        let line_tx = line_tx.clone();
        tokio::spawn(async move {
            let mut reader = BufReader::new(pipe).lines();
            while let Ok(Some(line)) = reader.next_line().await {
                if line_tx.send(Line::Err { text: line }).is_err() {
                    break;
                }
            }
        });
    }
    drop(line_tx);

    let mut outcome = RunOutcome {
        exit_code: -1,
        killed_by_timeout: false,
        killed_by_stall: false,
        cancelled: false,
        stderr_tail: String::new(),
    };

    loop {
        // 下一个到期的监督时限
        let stall_deadline = opts.stall_timeout.map(|d| last_output + d);
        let next_deadline = match (wall_deadline, stall_deadline) {
            (Some(w), Some(s)) => Some(w.min(s)),
            (Some(w), None) => Some(w),
            (None, Some(s)) => Some(s),
            (None, None) => None,
        };
        let which = match next_deadline {
            Some(d) if Some(d) == wall_deadline => DeadlineKind::Wall,
            Some(_) => DeadlineKind::Stall,
            None => DeadlineKind::None,
        };

        tokio::select! {
            _ = cancel.cancelled() => {
                kill_tree(pid);
                let _ = child.wait().await;
                outcome.cancelled = true;
                outcome.exit_code = -1;
                break;
            }
            _ = async {
                match next_deadline {
                    Some(d) => tokio::time::sleep_until(d).await,
                    None => std::future::pending::<()>().await,
                }
            } => {
                kill_tree(pid);
                let _ = child.wait().await;
                match which {
                    DeadlineKind::Wall => {
                        outcome.killed_by_timeout = true;
                        outcome.exit_code = -1;
                    }
                    DeadlineKind::Stall => {
                        outcome.killed_by_stall = true;
                        outcome.exit_code = -1;
                    }
                    DeadlineKind::None => unreachable!(),
                }
                break;
            }
            status = child.wait() => {
                match status {
                    Ok(st) => {
                        outcome.exit_code = st.code().unwrap_or(-1);
                        break;
                    }
                    Err(e) => return Err(RunnerError::Io(e)),
                }
            }
            line = line_rx.recv() => {
                if let Some(l) = line {
                    if matches!(l, Line::Err { .. }) {
                        push_tail(&mut stderr_tail, l.text());
                    }
                    last_output = Instant::now();
                    on_line(l);
                }
                // 泵已停（EOF）：继续等待 status 或时限
            }
        }
    }

    // 排空残余行（子进程退出后管道内可能还有缓冲）
    while let Ok(line) = line_rx.try_recv() {
        if matches!(line, Line::Err { .. }) {
            push_tail(&mut stderr_tail, line.text());
        }
        on_line(line);
    }
    outcome.stderr_tail = stderr_tail;
    Ok(outcome)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DeadlineKind {
    Wall,
    Stall,
    None,
}

/// 杀整个进程组（setsid 后 kill(-pgid)）。
fn kill_tree(pid: u32) {
    // 子进程在独立进程组（pgid == pid），负数 pid = 向进程组发信号
    let r = unsafe { libc::kill(-(pid as i32), libc::SIGKILL) };
    if r != 0 {
        warn!(pid, "kill 进程组失败: {}", std::io::Error::last_os_error());
    }
}

/// 追加 stderr 尾部（最多保留 2KB，新行在前截断）。
fn push_tail(buf: &mut String, line: &str) {
    buf.push_str(line);
    buf.push('\n');
    const MAX: usize = 2048;
    if buf.len() > MAX {
        let keep = buf
            .char_indices()
            .rev()
            .nth(MAX)
            .map(|(i, _)| i + 1)
            .unwrap_or(0);
        *buf = buf[keep..].to_string();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn sh_opts(cwd: &std::path::Path) -> RunOpts {
        RunOpts {
            program: "/bin/sh".into(),
            args: vec![],
            cwd: cwd.to_path_buf(),
            env: BTreeMap::new(),
            wall_timeout: Some(Duration::from_secs(30)),
            stall_timeout: None,
        }
    }

    #[tokio::test]
    async fn streams_lines_and_captures_exit() {
        let dir = tempfile::tempdir().unwrap();
        let mut opts = sh_opts(dir.path());
        opts.args = vec![
            "-c".into(),
            "echo one; echo two; echo err-line >&2; exit 0".into(),
        ];
        let mut lines = Vec::new();
        let outcome = run(&opts, |l| lines.push(l), CancellationToken::new())
            .await
            .unwrap();
        assert_eq!(outcome.exit_code, 0);
        let outs: Vec<&str> = lines
            .iter()
            .filter(|l| matches!(l, Line::Out { .. }))
            .map(|l| l.text())
            .collect();
        assert_eq!(outs, vec!["one", "two"]);
        assert!(outcome.stderr_tail.contains("err-line"));
    }

    #[tokio::test]
    async fn nonzero_exit_reported_not_errored() {
        let dir = tempfile::tempdir().unwrap();
        let mut opts = sh_opts(dir.path());
        opts.args = vec!["-c".into(), "exit 7".into()];
        let outcome = run(&opts, |_| {}, CancellationToken::new()).await.unwrap();
        assert_eq!(outcome.exit_code, 7);
    }

    #[tokio::test]
    async fn wall_timeout_kills_process_tree() {
        let dir = tempfile::tempdir().unwrap();
        let mut opts = sh_opts(dir.path());
        // 子进程继续睡：验证进程组 kill 带走孙进程
        opts.args = vec!["-c".into(), "sleep 30 & sleep 30; wait".into()];
        opts.wall_timeout = Some(Duration::from_millis(300));
        let start = Instant::now();
        let outcome = run(&opts, |_| {}, CancellationToken::new()).await.unwrap();
        assert!(outcome.killed_by_timeout);
        assert!(start.elapsed() < Duration::from_secs(10));
    }

    #[tokio::test]
    async fn stall_detection_kills_silent_process() {
        let dir = tempfile::tempdir().unwrap();
        let mut opts = sh_opts(dir.path());
        opts.args = vec!["-c".into(), "echo hello; sleep 30".into()];
        opts.stall_timeout = Some(Duration::from_millis(300));
        let mut saw_hello = false;
        let outcome = run(
            &opts,
            |l| {
                if l.text() == "hello" {
                    saw_hello = true;
                }
            },
            CancellationToken::new(),
        )
        .await
        .unwrap();
        assert!(saw_hello);
        assert!(outcome.killed_by_stall);
    }

    #[tokio::test]
    async fn cancel_token_kills() {
        let dir = tempfile::tempdir().unwrap();
        let mut opts = sh_opts(dir.path());
        opts.args = vec!["-c".into(), "sleep 30".into()];
        opts.wall_timeout = None;
        let cancel = CancellationToken::new();
        let cancel2 = cancel.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(200)).await;
            cancel2.cancel();
        });
        let outcome = run(&opts, |_| {}, cancel).await.unwrap();
        assert!(outcome.cancelled);
    }

    #[tokio::test]
    async fn env_injection_reaches_child() {
        let dir = tempfile::tempdir().unwrap();
        let mut opts = sh_opts(dir.path());
        opts.args = vec!["-c".into(), "echo $XTBP_TEST_VAR".into()];
        opts.env
            .insert("XTBP_TEST_VAR".into(), "injected-42".into());
        let mut got = None;
        run(
            &opts,
            |l| {
                if let Line::Out { text } = l {
                    got = Some(text);
                }
            },
            CancellationToken::new(),
        )
        .await
        .unwrap();
        assert_eq!(got.as_deref(), Some("injected-42"));
    }

    #[tokio::test]
    async fn missing_program_yields_spawn_error() {
        let dir = tempfile::tempdir().unwrap();
        let opts = RunOpts {
            program: "/nonexistent/xtb".into(),
            args: vec![],
            cwd: dir.path().to_path_buf(),
            env: BTreeMap::new(),
            wall_timeout: None,
            stall_timeout: None,
        };
        let err = run(&opts, |_| {}, CancellationToken::new())
            .await
            .unwrap_err();
        assert!(matches!(err, RunnerError::Spawn { .. }));
    }
}
