//! xTB-Pilot 常驻守护进程（二进制 crate，错误用 `anyhow`，规划文档 §2.2）。
//!
//! 装配：配置/自检 → 组件登记（自动发现）→ SQLite + 文件仓 → 事件总线 →
//! 调度器（崩溃恢复）→ 工作流引擎 → RDKit helper → TCP(agent)/UDS(TUI) 服务。

mod app;
mod tail;

use anyhow::Result;
use clap::{Parser, Subcommand};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use tokio::signal;
use tokio_util::sync::CancellationToken;
use tracing::{error, info, warn};
use xtbp_api::{EventBus, ServerConfig, serve_tcp, serve_uds};
use xtbp_assemble::HelperClient;
use xtbp_core::config::expand_tilde;
use xtbp_core::registry::{ComponentEntry, InstanceRegistry};
use xtbp_core::time::now_unix;
use xtbp_core::version::ComponentVersion;
use xtbp_sched::{SchedConfig, Scheduler};
use xtbp_store::{FileRepo, Store};
use xtbp_workflow::TemplateRegistry;

use crate::app::{AppState, default_concurrency, is_under_mnt, registry_resolver};
use crate::tail::TailBuffer;

#[derive(Parser, Debug)]
#[command(name = "xtbp-daemon", version, about = "xTB-Pilot 常驻守护进程")]
struct Args {
    /// 子命令（api-schema 打印协议 schema 后退出）
    #[command(subcommand)]
    command: Option<Cmd>,

    /// 监听地址（NDJSON over TCP，agent 接口）
    #[arg(long, default_value = "127.0.0.1:7700")]
    listen: String,

    /// TUI 的 UDS 路径（空 = 不启用）
    #[arg(long, default_value = "~/.local/share/xtbpilot/xtbp.sock")]
    uds: String,

    /// 鉴权 token（空 = 从 agent.json 读取或自动生成）
    #[arg(long, default_value = "")]
    token: String,

    /// 数据目录（SQLite + 文件仓，严禁 /mnt/c）
    #[arg(long, default_value = "~/.local/share/xtbpilot")]
    data_dir: String,

    /// 工作流模板目录
    #[arg(long, default_value = "")]
    templates_dir: String,

    /// InstanceRegistry 登记表路径
    #[arg(long, default_value = "~/.local/share/xtbpilot/registry.toml")]
    registry: String,

    /// 日志目录
    #[arg(long, default_value = "~/.local/share/xtbpilot/logs")]
    log_dir: String,

    /// 全局并发槽（缺省 = 物理核心 / 每任务线程）
    #[arg(long)]
    max_concurrent: Option<usize>,

    /// 每任务默认 OMP 线程（缺省 1）
    #[arg(long, default_value_t = 1)]
    threads_per_job: u32,

    /// 内存预算 MB（0 = 不限额）
    #[arg(long, default_value_t = 0)]
    memory_budget_mb: u64,
}

#[derive(Subcommand, Debug)]
enum Cmd {
    /// 输出全部方法的 JSON-Schema（供 agent 自省）
    ApiSchema,
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();
    if let Some(Cmd::ApiSchema) = args.command {
        println!(
            "{}",
            serde_json::to_string_pretty(&xtbp_api::protocol::api_schema())?
        );
        return Ok(());
    }

    // ---- 路径展开与红线自检（§4.3：/mnt/c 严禁）----
    let data_dir = PathBuf::from(expand_tilde(&args.data_dir));
    if is_under_mnt(&data_dir) {
        error!(
            "红线：数据目录位于 /mnt（9P 文件系统），拒绝启动: {}",
            data_dir.display()
        );
        anyhow::bail!("数据目录严禁位于 /mnt: {}", data_dir.display());
    }
    std::fs::create_dir_all(&data_dir)?;
    let log_dir = PathBuf::from(expand_tilde(&args.log_dir));
    std::fs::create_dir_all(&log_dir)?;
    let templates_dir = if args.templates_dir.is_empty() {
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../templates")
    } else {
        PathBuf::from(expand_tilde(&args.templates_dir))
    };

    init_tracing(&log_dir)?;

    // XTB4STDAHOME 自动探测（§3.2 回退二进制约定目录）：
    // 未显式 export 时按 ~/opt/xtb4stda-1.0 补上，sTDA 工作流开箱即用。
    // 必须在 init_tracing 之后调用（探测日志需落盘），且早于 resolver 构建。
    auto_detect_xtb4stdahome();

    info!(
        data_dir = %data_dir.display(),
        listen = %args.listen,
        "daemon 启动"
    );

    // ---- token（§3.7：单用户本地 token，agent.json 自发现）----
    let token = resolve_token(&args.token)?;
    ensure_xtb4stda_home_params();

    // ---- 组件登记（自动发现 + 版本探测）----
    let registry_path = PathBuf::from(expand_tilde(&args.registry));
    let registry = Arc::new(Mutex::new(load_or_discover_registry(&registry_path)?));

    // ---- 持久层 + 文件仓 ----
    let store = Store::open(&data_dir.join("xtbpilot.db")).await?;
    let file_repo = FileRepo::new(data_dir.clone());

    // ---- 事件总线 + 调度器（崩溃恢复）----
    let bus = EventBus::new(4096);
    let shutdown = CancellationToken::new();
    let sched = Scheduler::new(
        SchedConfig {
            max_concurrent: args
                .max_concurrent
                .unwrap_or_else(|| default_concurrency(args.threads_per_job)),
            memory_budget_mb: args.memory_budget_mb,
            _reserved: (),
        },
        store.clone(),
        bus.clone(),
        shutdown.clone(),
    );
    sched.start();
    let (interrupted, requeued) = sched.recover().await?;
    info!(interrupted, requeued, "崩溃恢复完成");

    // ---- 工作流引擎 ----
    let templates = TemplateRegistry::load_dir(&templates_dir)?;
    info!(templates = ?templates.ids(), "工作流模板已加载");
    let resolver = registry_resolver(Arc::clone(&registry));
    let engine = xtbp_workflow::WorkflowEngine::new(
        store.clone(),
        sched.clone(),
        bus.clone(),
        templates,
        file_repo.clone(),
        resolver,
    );

    // ---- RDKit helper（启动即拉起，失败可后续自愈）----
    let helper: Arc<tokio::sync::Mutex<Option<HelperClient>>> =
        Arc::new(tokio::sync::Mutex::new(None));
    match HelperClient::spawn().await {
        Ok(h) => {
            *helper.lock().await = Some(h);
            info!("RDKit helper 已就绪");
        }
        Err(e) => warn!("RDKit helper 启动失败（首次使用时重试）: {e}"),
    }

    // ---- 应用状态 ----
    let state = AppState {
        store,
        bus: bus.clone(),
        sched,
        engine,
        registry,
        helper,
        tail: Arc::new(TailBuffer::new()),
        started_at: now_unix(),
        data_dir: data_dir.clone(),
        templates_dir: templates_dir.clone(),
        shutdown: shutdown.clone(),
    };
    let state = Arc::new(state);

    // ---- 输出 tail 归档任务（Output 事件 → 内存缓冲 + stdout.log 文件）----
    {
        let state = Arc::clone(&state);
        let tail = Arc::clone(&state.tail);
        let mut rx = bus.subscribe();
        let shutdown_arc = shutdown.clone();
        tokio::spawn(async move {
            loop {
                let ev = tokio::select! {
                    _ = shutdown_arc.cancelled() => return,
                    ev = rx.recv() => match ev {
                        Ok(e) => e,
                        Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                        Err(tokio::sync::broadcast::error::RecvError::Closed) => return,
                    },
                };
                if let xtbp_core::job::JobEvent::Output { job_id, line } = ev
                    && let Ok(id) = job_id.parse::<xtbp_core::Ulid>()
                {
                    tail.append(&id, line.clone());
                    // 子任务输出镜像到父任务（工作流任务整体 tail 视图）；
                    // 子任务自身的 stdout.log 由 sched 同步写盘，此处只补父文件
                    if let Ok(Some(job)) = state.store.get_job(&id).await
                        && let Some(parent) = job.parent_id
                    {
                        tail.append(&parent, line.clone());
                        append_stdout_log(&state, &parent, &line).await;
                    }
                }
            }
        });
    }

    // ---- 服务：TCP（agent）+ UDS（TUI）----
    // UDS 不鉴权（README §3.7 声明）：本地 socket 文件权限即信任边界，
    // 人类用户直接输入 `xtbp-tui` 无需 --token；TCP agent 保持 token 鉴权。
    let config = Arc::new(ServerConfig {
        token: token.clone(),
        event_bus: bus,
    });
    let tcp_listener = tokio::net::TcpListener::bind(&args.listen).await?;
    info!("TCP 监听于 {}", tcp_listener.local_addr()?);
    let tcp_task = tokio::spawn(serve_tcp(
        tcp_listener,
        Arc::clone(&state),
        Arc::clone(&config),
        shutdown.clone(),
    ));

    let uds_path = PathBuf::from(expand_tilde(&args.uds));
    let mut uds_task = None;
    if !args.uds.is_empty() {
        if let Some(parent) = uds_path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        if uds_path.exists() {
            std::fs::remove_file(&uds_path)?;
        }
        match tokio::net::UnixListener::bind(&uds_path) {
            Ok(listener) => {
                info!("UDS 监听于 {}", uds_path.display());
                let uds_config = Arc::new(ServerConfig {
                    token: String::new(),
                    event_bus: config.event_bus.clone(),
                });
                uds_task = Some(tokio::spawn(serve_uds(
                    listener,
                    Arc::clone(&state),
                    uds_config,
                    shutdown.clone(),
                )));
            }
            Err(e) => warn!("UDS 绑定失败（TUI 将不可用）: {e}"),
        }
    }

    // ---- 信号 → 优雅停机 ----
    {
        let shutdown = shutdown.clone();
        tokio::spawn(async move {
            #[cfg(unix)]
            {
                let mut sigterm = signal::unix::signal(signal::unix::SignalKind::terminate())
                    .expect("SIGTERM handler");
                tokio::select! {
                    _ = signal::ctrl_c() => {}
                    _ = sigterm.recv() => {}
                }
            }
            #[cfg(not(unix))]
            {
                let _ = signal::ctrl_c().await;
            }
            info!("收到停止信号，开始优雅停机");
            shutdown.cancel();
        });
    }

    info!(
        url = %args.listen,
        uds = %uds_path.display(),
        "xTB-Pilot daemon 就绪"
    );
    shutdown.cancelled().await;
    state.sched.shutdown();
    info!("优雅停机完成");
    if let Some(t) = uds_task {
        let _ = t.await;
    }
    if let Err(e) = tcp_task.await {
        error!("TCP 服务退出异常: {e}");
    }
    Ok(())
}

/// 把输出行追加到任务工作目录的 stdout.log（重启后仍可追查）。
async fn append_stdout_log(state: &AppState, job_id: &xtbp_core::Ulid, line: &str) {
    let Ok(Some(job)) = state.store.get_job(job_id).await else {
        return;
    };
    let Some(workdir) = job.workdir else {
        return;
    };
    let path = Path::new(&workdir).join("stdout.log");
    use std::io::Write;
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
    {
        let _ = writeln!(f, "{line}");
    }
}

// ---------------------------------------------------------------------------
// token 管理
// ---------------------------------------------------------------------------

fn resolve_token(cli_token: &str) -> Result<String> {
    let dir = PathBuf::from(expand_tilde("~/.xtbpilot"));
    std::fs::create_dir_all(&dir)?;
    let path = dir.join("agent.json");
    // CLI 显式 token 优先；否则读 agent.json；再否则生成新 token
    let token = if !cli_token.is_empty() {
        cli_token.to_string()
    } else if let Some(t) = read_token_from_agent_json(&path) {
        t
    } else {
        format!("xtbp-{:016x}", now_unix())
    };
    // 一律落盘：Windows 侧经 `wsl cat` 或 \\wsl$ 读取实现自发现
    let agent_json = serde_json::json!({
        "protocol": "xtbp-jsonrpc-v1",
        "url": "127.0.0.1:7700",
        "token": token,
        "uds": "~/.local/share/xtbpilot/xtbp.sock",
        "note": "Windows agent 直连 localhost:7700（WSL2 localhostForwarding），每个请求带顶层 token 字段",
    });
    std::fs::write(&path, serde_json::to_string_pretty(&agent_json)?)?;
    info!(path = %path.display(), "agent.json 已写入");
    Ok(token)
}

/// 从 agent.json 读 token（不存在/损坏 → None）。
fn read_token_from_agent_json(path: &Path) -> Option<String> {
    let raw = std::fs::read_to_string(path).ok()?;
    let v = serde_json::from_str::<serde_json::Value>(&raw).ok()?;
    v.get("token").and_then(|t| t.as_str()).map(String::from)
}

/// XTB4STDAHOME 自动探测：未设置且 `~/opt/xtb4stda-1.0` 存在（回退二进制
/// 约定目录，见 README）→ 注入进程环境。后续 resolver、参数文件同步、
/// 子进程注入全部依赖它。
fn auto_detect_xtb4stdahome() {
    if std::env::var_os("XTB4STDAHOME").is_some() {
        return;
    }
    let Ok(home) = std::env::var("HOME") else {
        return;
    };
    let guess = Path::new(&home).join("opt/xtb4stda-1.0");
    if guess.is_dir() {
        // SAFETY：启动早期调用，进程内尚无任何并发环境读取者
        // （resolver/参数同步均在本次写入之后才读取）。
        unsafe {
            std::env::set_var("XTB4STDAHOME", &guess);
        }
        info!(path = %guess.display(), "XTB4STDAHOME 自动探测并启用");
    }
}

/// xtb4stda 参数文件兜底：它的默认路径是 `~/.param_stda{1,2}.xtb`（而非
/// XTB4STDAHOME），HOME 侧缺失或空文件时从 XTB4STDAHOME 同步（实测踩坑：
/// 空文件会报 `no basis found for atom 1 Z=6`）。
fn ensure_xtb4stda_home_params() {
    let Ok(home) = std::env::var("XTB4STDAHOME") else {
        return;
    };
    let Ok(home_dir) = std::env::var("HOME") else {
        return;
    };
    for name in [".param_stda1.xtb", ".param_stda2.xtb"] {
        let src = Path::new(&home).join(name);
        let dst = Path::new(&home_dir).join(name);
        let dst_empty_or_missing = match std::fs::metadata(&dst) {
            Ok(m) => m.len() == 0,
            Err(_) => true,
        };
        if dst_empty_or_missing && src.is_file() {
            if let Err(e) = std::fs::copy(&src, &dst) {
                warn!(src = %src.display(), dst = %dst.display(), "xtb4stda 参数同步失败: {e}");
            } else {
                info!(path = %dst.display(), "xtb4stda 参数文件已同步到 HOME");
            }
        }
    }
}

// ---------------------------------------------------------------------------
// 组件发现
// ---------------------------------------------------------------------------

/// 加载登记表；缺失组件自动发现（PATH / conda env / ~/opt 回退二进制）。
fn load_or_discover_registry(path: &Path) -> Result<InstanceRegistry> {
    let mut reg = if path.exists() {
        match InstanceRegistry::load(path) {
            Ok(r) => r,
            Err(e) => {
                warn!(path = %path.display(), "登记表解析失败，从空表重建: {e}");
                InstanceRegistry::new()
            }
        }
    } else {
        InstanceRegistry::new()
    };
    let mut changed = false;
    for name in ["xtb", "crest", "xtb4stda", "stda", "qcg", "aiss"] {
        if reg.latest(name).is_none()
            && let Some(exe) = find_component(name)
        {
            // 版本探测失败（如 xtb4stda 只打 banner）→ 登记 0.0.0，仍可用
            let version = match probe_version(&exe) {
                Some(v) => v,
                None => {
                    warn!(name, exe = %exe.display(), "版本探测失败，登记为 0.0.0");
                    ComponentVersion::zero()
                }
            };
            match reg.register(name, version, exe.clone()) {
                Ok(entry) => {
                    info!(
                        name,
                        exe = %exe.display(),
                        version = %entry.version,
                        "组件已登记"
                    );
                    changed = true;
                }
                Err(e) => warn!(name, "登记失败: {e}"),
            }
        }
    }
    if changed && let Err(e) = reg.save(path) {
        warn!("登记表保存失败: {e}");
    }
    Ok(reg)
}

/// 组件可执行文件探测：PATH → conda env → ~/opt/<name>-*/bin/<name>。
fn find_component(name: &str) -> Option<PathBuf> {
    if let Ok(p) = which::which(name) {
        return Some(p);
    }
    let conda_bins = [
        "/opt/miniforge3/envs/xtbp/bin",
        "/opt/miniconda3/envs/xtbp/bin",
        "/home/black/miniforge3/envs/xtbp/bin",
        "/home/black/miniconda3/envs/xtbp/bin",
    ];
    for dir in conda_bins {
        let p = Path::new(dir).join(name);
        if p.is_file() {
            return Some(p);
        }
    }
    // ~/opt/<name>-*/bin/<name>（回退二进制，如 xtb4stda/stda）
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
    let opt_dir = Path::new(&home).join("opt");
    if let Ok(entries) = std::fs::read_dir(&opt_dir) {
        let mut candidates: Vec<PathBuf> = entries
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .filter(|p| {
                p.is_dir()
                    && p.file_name()
                        .and_then(|n| n.to_str())
                        .is_some_and(|n| n.starts_with(name))
            })
            .map(|p| p.join("bin").join(name))
            .filter(|p| p.is_file())
            .collect();
        candidates.sort();
        if let Some(p) = candidates.into_iter().next() {
            return Some(p);
        }
    }
    None
}

/// 探测组件版本（`<exe> --version` 输出中扫描 semver token）。
fn probe_version(exe: &Path) -> Option<ComponentVersion> {
    let output = std::process::Command::new(exe)
        .arg("--version")
        .output()
        .ok()?;
    let text = String::from_utf8_lossy(&output.stdout);
    for token in text.split_whitespace() {
        if let Some(v) = extract_semver(token) {
            return Some(v);
        }
    }
    None
}

/// 从 token 中提取首个 `数字(.数字)*` 片段（容忍 "5.2.21(1)-release" 等噪声）。
fn extract_semver(token: &str) -> Option<ComponentVersion> {
    let bytes = token.as_bytes();
    let start = bytes.iter().position(|b| b.is_ascii_digit())?;
    let mut end = start;
    while end < bytes.len() && (bytes[end].is_ascii_digit() || bytes[end] == b'.') {
        end += 1;
    }
    let candidate = &token[start..end];
    let candidate = candidate.trim_start_matches('v').trim_start_matches('V');
    candidate.parse().ok()
}

// ---------------------------------------------------------------------------
// 日志
// ---------------------------------------------------------------------------

fn init_tracing(log_dir: &Path) -> Result<()> {
    let file_appender = tracing_appender::rolling::daily(log_dir, "daemon.log");
    let (non_blocking, _guard) = tracing_appender::non_blocking(file_appender);
    tracing_subscriber::fmt()
        .with_env_filter("xtbp=info")
        .with_writer(non_blocking)
        .init();
    // `_guard` 需存活到进程结束：泄漏以保持 appender 工作
    std::mem::forget(_guard);
    Ok(())
}

// 登记表条目辅助（测试用）
#[allow(dead_code)]
fn entry_of(reg: &InstanceRegistry, name: &str) -> Option<ComponentEntry> {
    reg.latest(name).cloned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cli_parses_defaults() {
        let args = Args::try_parse_from(["xtbp-daemon"]).unwrap();
        assert_eq!(args.registry, "~/.local/share/xtbpilot/registry.toml");
        assert!(args.listen.contains("7700"));
        assert_eq!(args.threads_per_job, 1);
    }

    #[test]
    fn api_schema_subcommand_parses() {
        let args = Args::try_parse_from(["xtbp-daemon", "api-schema"]).unwrap();
        assert!(matches!(args.command, Some(Cmd::ApiSchema)));
    }

    #[test]
    fn version_probe_scans_semver_token() {
        // bash --version 输出含 semver（如 "GNU bash, version 5.2.x"）
        let v = probe_version(Path::new("/bin/bash"));
        assert!(v.is_some(), "bash --version 应含 semver");
    }

    #[test]
    fn mnt_redline_detects_9p() {
        assert!(is_under_mnt(Path::new("/mnt/c/Users/x")));
        assert!(!is_under_mnt(Path::new(
            "/home/black/.local/share/xtbpilot"
        )));
    }
}
