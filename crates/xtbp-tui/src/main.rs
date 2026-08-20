//! xTB-Pilot 终端界面（设计文档 §3.6，v1 第一优先级交付物）。
//!
//! 架构：TUI 只是 daemon 的客户端（UDS）。事件订阅驱动渲染——
//! 订阅 `job.events` / `queue.events` 后由 daemon 主动推流；断线自动重连
//! 并恢复视图；关闭 TUI 不中断计算。Vim 键位（hjkl、/ 过滤、? 帮助、Tab 切页）。

mod chem;
mod model;
mod ui;

use anyhow::Result;
use clap::Parser;
use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use model::{
    AppModel, FAMILY_LABELS, FAMILY_NAMES, InputMode, JobView, NumberField, Page, QueueStats,
    SOLVATION_LABELS, SOLVATION_NAMES,
};
use ratatui::DefaultTerminal;
use std::time::Duration;
use tokio::net::UnixStream;
use tracing::warn;
use xtbp_api::Client;
use xtbp_api::client::connect_uds;
use xtbp_api::protocol::{Notification, methods};
use xtbp_core::config::expand_tilde;

#[derive(Parser, Debug)]
#[command(name = "xtbp-tui", version, about = "xTB-Pilot 终端界面")]
struct Args {
    /// daemon 的 UDS 路径
    #[arg(long, default_value = "~/.local/share/xtbpilot/xtbp.sock")]
    uds: String,

    /// token（UDS 默认不鉴权，与 daemon 一致）
    #[arg(long, default_value = "")]
    token: String,

    /// daemon 未运行时自动拉起（面向人类用户的即输即用；daemon 独立存活，
    /// 关闭 TUI 不中断计算）。--no-spawn 关闭
    #[arg(long, default_value_t = false, action = clap::ArgAction::SetTrue)]
    no_spawn: bool,

    /// 自动拉起用的 daemon 可执行文件（PATH 查找或绝对路径）
    #[arg(long, default_value = "xtbp-daemon")]
    daemon: String,
}

/// 主循环事件。
enum Event {
    Key(KeyEvent),
    Notif(Option<Notification>),
    Disconnected,
    Tick,
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();
    tracing_subscriber::fmt()
        .with_env_filter("xtbp_tui=info")
        .init();

    let mut terminal = ratatui::init();
    let result = run(&mut terminal, &args).await;
    ratatui::restore();
    result
}

/// 从 `~/.xtbpilot/agent.json` 自动发现 token（daemon 每次启动都会落盘）。
/// 人类用户直接输入 `xtbp-tui` 无需手动传 --token（UDS 本就不鉴权，
/// 此兜底保证未来 UDS 恢复鉴权时 TUI 仍可用）。
fn discover_token() -> Option<String> {
    let raw = std::fs::read_to_string(expand_tilde("~/.xtbpilot/agent.json")).ok()?;
    let v: serde_json::Value = serde_json::from_str(&raw).ok()?;
    v.get("token").and_then(|t| t.as_str()).map(String::from)
}

async fn run(terminal: &mut DefaultTerminal, args: &Args) -> Result<()> {
    let uds = expand_tilde(&args.uds);
    // token：CLI 显式 > agent.json 自动发现 > 空（UDS 不鉴权）
    let token = if args.token.is_empty() {
        discover_token().unwrap_or_default()
    } else {
        args.token.clone()
    };
    let mut model = AppModel::default();
    let mut client: Option<Client<UnixStream>> = None;
    let mut spawn_tried = false;

    // 键盘事件 → 通道（标准输入读取，不影响主循环阻塞点）。
    // 必须用 std 线程而非 tokio::spawn：crossterm poll/read 是阻塞调用，
    // 挂在 tokio worker 上会让 Runtime::drop 等待该 worker 完成当前任务而
    // 永不退出（q 后进程僵住，实测复现）。std 线程随进程退出由 OS 回收。
    let (key_tx, mut key_rx) = tokio::sync::mpsc::unbounded_channel::<KeyEvent>();
    std::thread::spawn(move || {
        loop {
            if crossterm::event::poll(Duration::from_millis(50)).unwrap_or(false)
                && let Ok(crossterm::event::Event::Key(key)) = crossterm::event::read()
                && key.kind == KeyEventKind::Press
                && key_tx.send(key).is_err()
            {
                break;
            }
        }
    });

    let mut tick = tokio::time::interval(Duration::from_secs(1));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    loop {
        // 断线重连（2s 间隔；成功后恢复订阅与视图）。
        // daemon 不在时自动拉起一次（setsid 脱离会话，TUI 退出后继续存活）
        if client.is_none() {
            match connect_uds(&uds, &token).await {
                Ok(c) => {
                    client = Some(c);
                    on_connected(&mut model, client.as_mut().unwrap()).await;
                }
                Err(e) => {
                    model.connected = false;
                    if !args.no_spawn && !spawn_tried {
                        spawn_tried = true;
                        match spawn_daemon(&args.daemon) {
                            Ok(()) => {
                                model.status_line =
                                    "daemon 未运行，已自动拉起（启动约需 2-4 秒）…".into();
                            }
                            Err(se) => {
                                model.status_line = format!(
                                    "daemon 连接失败且自动拉起失败: {se}（用 --no-spawn 关闭）"
                                );
                            }
                        }
                    } else {
                        model.status_line = format!("daemon 连接失败，2s 后重试: {e}");
                    }
                }
            }
        }

        terminal.draw(|f| ui::draw(f, &mut model))?;
        if model.quit {
            break;
        }

        let ev = tokio::select! {
            key = key_rx.recv() => match key {
                Some(k) => Event::Key(k),
                None => break,
            },
            ev = async {
                match client.as_mut() {
                    Some(c) => match c.read_event().await {
                        Ok(n) => Event::Notif(n),
                        Err(e) => {
                            warn!("读事件失败: {e}");
                            Event::Disconnected
                        }
                    },
                    None => {
                        tokio::time::sleep(Duration::from_millis(500)).await;
                        Event::Tick
                    }
                }
            } => ev,
            _ = tick.tick() => Event::Tick,
        };

        match ev {
            Event::Key(k) => handle_key(&mut model, client.as_mut(), &k).await,
            Event::Notif(n) => handle_notif(&mut model, n).await,
            Event::Disconnected => {
                model.connected = false;
                client = None;
                model.status_line = "daemon 连接断开，重连中…".into();
            }
            Event::Tick => {
                // 周期增量：选中任务仍在跑 → 拉取增量 tail
                if model.detail.as_ref().is_some_and(|d| d.status == "running") {
                    fetch_tail(&mut model, client.as_mut()).await;
                }
            }
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// 事件处理
// ---------------------------------------------------------------------------

async fn handle_key(
    model: &mut AppModel,
    mut client: Option<&mut Client<UnixStream>>,
    key: &KeyEvent,
) {
    // Ctrl-C：任何时刻「全链退出」——先请求 daemon 优雅停机
    // （取消全部任务、递归杀计算进程组与 RDKit helper），再退 TUI。
    // 与 q/Esc 不同：q 只退 TUI，daemon 独立存活（设计文档 §2.1）。
    if key.modifiers.contains(KeyModifiers::CONTROL)
        && matches!(key.code, KeyCode::Char('c') | KeyCode::Char('C'))
    {
        if let Some(c) = client.as_mut()
            && c.call(methods::SYS_SHUTDOWN, serde_json::json!({}))
                .await
                .is_ok()
        {
            model.status_line = "已请求 daemon 停机（计算进程一并清理）".into();
        }
        model.quit = true;
        return;
    }

    // 输入模式优先
    match model.mode {
        InputMode::Filter => {
            match key.code {
                KeyCode::Esc => {
                    model.input_buf.clear();
                    model.mode = InputMode::Normal;
                }
                KeyCode::Backspace => {
                    model.input_buf.pop();
                }
                KeyCode::Enter => {
                    model.filter = model.input_buf.clone();
                    model.input_buf.clear();
                    model.mode = InputMode::Normal;
                }
                KeyCode::Char(c) => model.input_buf.push(c),
                _ => {}
            }
            return;
        }
        InputMode::Smiles => {
            match key.code {
                KeyCode::Esc => {
                    model.input_buf.clear();
                    model.mode = InputMode::Normal;
                }
                KeyCode::Backspace => {
                    model.input_buf.pop();
                }
                KeyCode::Enter => {
                    let smiles = model.input_buf.clone();
                    model.input_buf.clear();
                    model.mode = InputMode::Normal;
                    model.form.smiles = smiles.clone();
                    submit_job(model, client, &smiles).await;
                }
                KeyCode::Char(c) => model.input_buf.push(c),
                _ => {}
            }
            return;
        }
        InputMode::SmiPath => {
            match key.code {
                KeyCode::Esc => {
                    model.input_buf.clear();
                    model.mode = InputMode::Normal;
                }
                KeyCode::Backspace => {
                    model.input_buf.pop();
                }
                KeyCode::Enter => {
                    let path = model.input_buf.trim().to_string();
                    model.input_buf.clear();
                    model.mode = InputMode::Normal;
                    import_smi(model, client, &path).await;
                }
                KeyCode::Char(c) => model.input_buf.push(c),
                _ => {}
            }
            return;
        }
        InputMode::Solvent => {
            match key.code {
                KeyCode::Esc => {
                    model.input_buf.clear();
                    model.mode = InputMode::Normal;
                }
                KeyCode::Backspace => {
                    model.input_buf.pop();
                }
                KeyCode::Enter => {
                    let solv = model.input_buf.trim().to_string();
                    model.input_buf.clear();
                    model.mode = InputMode::Normal;
                    if solv.is_empty() {
                        model.status_line = "溶剂名不能为空（先切到 ALPB/GBSA 模型）".into();
                    } else {
                        model.form.solvent = solv;
                        model.status_line = format!("溶剂: {}", model.form.solvent);
                    }
                }
                KeyCode::Char(c) => model.input_buf.push(c),
                _ => {}
            }
            return;
        }
        InputMode::Number(field) => {
            match key.code {
                KeyCode::Esc => {
                    model.input_buf.clear();
                    model.mode = InputMode::Normal;
                }
                KeyCode::Backspace => {
                    model.input_buf.pop();
                }
                KeyCode::Enter => {
                    let text = model.input_buf.trim().to_string();
                    model.input_buf.clear();
                    model.mode = InputMode::Normal;
                    apply_number_field(model, field, &text);
                }
                KeyCode::Char(c) => model.input_buf.push(c),
                _ => {}
            }
            return;
        }
        InputMode::Normal => {}
    }

    if model.show_help {
        model.show_help = false;
        return;
    }
    match key.code {
        KeyCode::Char('q') => model.quit = true,
        KeyCode::Esc => model.quit = true,
        KeyCode::Char('?') => model.show_help = true,
        KeyCode::Char('/') => {
            model.input_buf = model.filter.clone();
            model.mode = InputMode::Filter;
        }
        KeyCode::Tab => {
            switch_page(model, client, key.modifiers.contains(KeyModifiers::SHIFT)).await
        }
        KeyCode::Char('g') => refresh_lists(model, client).await,
        // ---- Workflows 页：提交表单 ----
        KeyCode::Enter if model.page == Page::Workflows => {
            model.input_buf = model.form.smiles.clone();
            model.mode = InputMode::Smiles;
        }
        KeyCode::Char('s') if model.page == Page::Workflows => {
            model.input_buf = model.form.smiles.clone();
            model.mode = InputMode::Smiles;
        }
        KeyCode::Char('i') if model.page == Page::Workflows => {
            model.input_buf.clear();
            model.mode = InputMode::SmiPath;
        }
        KeyCode::Char('e') if model.page == Page::Workflows => {
            model.input_buf = model.form.solvent.clone();
            model.mode = InputMode::Solvent;
        }
        KeyCode::Char('t') if model.page == Page::Workflows => {
            model.input_buf = model.form.etemp.map(|v| v.to_string()).unwrap_or_default();
            model.mode = InputMode::Number(NumberField::Etemp);
        }
        KeyCode::Char('a') if model.page == Page::Workflows => {
            model.input_buf = model
                .form
                .accuracy
                .map(|v| v.to_string())
                .unwrap_or_default();
            model.mode = InputMode::Number(NumberField::Accuracy);
        }
        KeyCode::Char('m') if model.page == Page::Workflows => {
            model.input_buf = model
                .form
                .maxiter
                .map(|v| v.to_string())
                .unwrap_or_default();
            model.mode = InputMode::Number(NumberField::Maxiter);
        }
        KeyCode::Char('c') if model.page == Page::Workflows => {
            model.input_buf = model.form.charge.to_string();
            model.mode = InputMode::Number(NumberField::Charge);
        }
        KeyCode::Char('n') if model.page == Page::Workflows => {
            model.input_buf = model.form.multiplicity.to_string();
            model.mode = InputMode::Number(NumberField::Multiplicity);
        }
        KeyCode::Char('[') if model.page == Page::Workflows => {
            let n = FAMILY_NAMES.len();
            model.form.family = (model.form.family + n - 1) % n;
            model.status_line = format!("计算水平: {}", FAMILY_LABELS[model.form.family]);
        }
        KeyCode::Char(']') if model.page == Page::Workflows => {
            let n = FAMILY_NAMES.len();
            model.form.family = (model.form.family + 1) % n;
            model.status_line = format!("计算水平: {}", FAMILY_LABELS[model.form.family]);
        }
        KeyCode::Char('{') if model.page == Page::Workflows => {
            let n = SOLVATION_NAMES.len();
            model.form.solvation = (model.form.solvation + n - 1) % n;
            model.status_line = format!("溶剂模型: {}", SOLVATION_LABELS[model.form.solvation]);
        }
        KeyCode::Char('}') if model.page == Page::Workflows => {
            let n = SOLVATION_NAMES.len();
            model.form.solvation = (model.form.solvation + 1) % n;
            model.status_line = format!("溶剂模型: {}", SOLVATION_LABELS[model.form.solvation]);
        }
        // ---- 其他页面 ----
        KeyCode::Char('c') if model.page == Page::Jobs => cancel_selected(model, client).await,
        KeyCode::Char('o') if model.page == Page::Structure => {
            if let Some(p) = &model.xyz_path {
                if let Err(e) = chem::open_external_viewer(p) {
                    model.status_line = e;
                } else {
                    model.status_line = format!("已在 Windows 侧打开: {p}");
                }
            }
        }
        KeyCode::Char(' ') if model.page == Page::Jobs => select_job(model, client).await,
        KeyCode::Enter if model.page == Page::Jobs => select_job(model, client).await,
        KeyCode::Left | KeyCode::Char('h') => move_sel(model, client, -1).await,
        KeyCode::Right | KeyCode::Char('l') => move_sel(model, client, 1).await,
        KeyCode::Up | KeyCode::Char('k') => move_sel(model, client, -1).await,
        KeyCode::Down | KeyCode::Char('j') => move_sel(model, client, 1).await,
        KeyCode::Home => jump_sel(model, client, JumpTarget::First).await,
        KeyCode::End => jump_sel(model, client, JumpTarget::Last).await,
        KeyCode::PageUp => jump_sel(model, client, JumpTarget::PageUp).await,
        KeyCode::PageDown => jump_sel(model, client, JumpTarget::PageDown).await,
        _ => {}
    }
}

/// 应用数值输入到表单字段（解析失败给出状态行提示，不改值）。
fn apply_number_field(model: &mut AppModel, field: NumberField, text: &str) {
    match field {
        NumberField::Etemp => match text.parse::<f64>() {
            Ok(v) => {
                model.form.etemp = Some(v);
                model.status_line = format!("etemp: {v} K");
            }
            Err(_) => model.status_line = format!("etemp 解析失败: {text:?}（应为数字，如 500）"),
        },
        NumberField::Accuracy => match text.parse::<f64>() {
            Ok(v) => {
                model.form.accuracy = Some(v);
                model.status_line = format!("accuracy: {v}");
            }
            Err(_) => {
                model.status_line = format!("accuracy 解析失败: {text:?}（应为数字，如 0.5）")
            }
        },
        NumberField::Maxiter => match text.parse::<u32>() {
            Ok(v) => {
                model.form.maxiter = Some(v);
                model.status_line = format!("maxiter: {v}");
            }
            Err(_) => model.status_line = format!("maxiter 解析失败: {text:?}（应为整数，如 250）"),
        },
        NumberField::Charge => match text.parse::<i8>() {
            Ok(v) => {
                model.form.charge = v;
                model.status_line = format!("电荷: {v}");
            }
            Err(_) => model.status_line = format!("电荷解析失败: {text:?}（应为整数，如 -1/0/1）"),
        },
        NumberField::Multiplicity => match text.parse::<u8>() {
            Ok(v) => {
                model.form.multiplicity = v;
                model.status_line = format!("多重度: {v}");
            }
            Err(_) => model.status_line = format!("多重度解析失败: {text:?}（应为整数，如 1/2）"),
        },
    }
}

/// 跳转目标（Home/End/PgUp/PgDn）。
#[derive(Debug, Clone, Copy)]
enum JumpTarget {
    First,
    Last,
    PageUp,
    PageDown,
}

/// 一页的行数（PgUp/PgDn 跳转步长）。
const JUMP_PAGE: usize = 10;

/// 跳转选中（按页分发；Spectra 页附带重载）。
async fn jump_sel(model: &mut AppModel, client: Option<&mut Client<UnixStream>>, to: JumpTarget) {
    match model.page {
        Page::Jobs => {
            let len = model.filtered_jobs_len();
            model.jobs_sel = jump_index(model.jobs_sel, len, to);
        }
        Page::Molecules => {
            let len = model.filtered_molecules_len();
            model.mols_sel = jump_index(model.mols_sel, len, to);
        }
        Page::Spectra => {
            let len = model.spectra_candidates().len();
            model.spec_sel = jump_index(model.spec_sel, len, to);
            load_spectrum(model, client).await;
        }
        Page::Workflows => {
            let len = xtbp_core::BUILTIN_TEMPLATES.len();
            let idx = jump_index(model.form.workflow, len, to);
            model.form.workflow = idx;
            model.wf_sel = idx;
        }
        Page::Instances => {
            let len = model.instances.len();
            model.inst_sel = jump_index(model.inst_sel, len, to);
        }
        _ => {}
    }
}

/// 计算跳转后的选中索引（clamp 到 [0, len-1]）。
fn jump_index(cur: usize, len: usize, to: JumpTarget) -> usize {
    if len == 0 {
        return 0;
    }
    match to {
        JumpTarget::First => 0,
        JumpTarget::Last => len - 1,
        JumpTarget::PageUp => cur.saturating_sub(JUMP_PAGE),
        JumpTarget::PageDown => (cur + JUMP_PAGE).min(len - 1),
    }
}

/// 处理服务器推送通知。
async fn handle_notif(model: &mut AppModel, n: Option<Notification>) {
    let Some(n) = n else { return };
    if n.method == methods::QUEUE_EVENTS {
        model.queue = QueueStats {
            queued: n.params["queued"].as_u64().unwrap_or(0) as usize,
            running: n.params["running"].as_u64().unwrap_or(0) as usize,
            slots_free: model.queue.slots_free,
            memory_in_use_mb: model.queue.memory_in_use_mb,
        };
        return;
    }
    if n.method != methods::JOB_EVENTS {
        return;
    }
    let ty = n.params["type"].as_str().unwrap_or("");
    let job_id = n.params["job_id"].as_str().unwrap_or("");
    match ty {
        "queued" => {
            model.upsert_status(job_id, "queued");
            refresh_lists(model, None).await;
        }
        "started" => model.upsert_status(job_id, "running"),
        "output" => {
            if let Some(line) = n.params["line"].as_str()
                && model.detail.as_ref().is_some_and(|d| d.id == job_id)
            {
                let seq = model.tail.len() as u64;
                model.tail.push((seq, line.to_string()));
                if model.tail.len() > 2000 {
                    model.tail.drain(0..model.tail.len() - 2000);
                }
                model.energies = chem::extract_energies(&model.tail);
            }
        }
        "status" => {
            if let Some(status) = n.params["status"].as_str() {
                model.upsert_status(job_id, status);
            }
        }
        "finished" => {
            let ok = n.params["ok"].as_bool().unwrap_or(false);
            model.upsert_status(job_id, if ok { "done" } else { "failed" });
            refresh_lists(model, None).await;
        }
        "queue-depth" => {
            model.queue.queued = n.params["queued"].as_u64().unwrap_or(0) as usize;
            model.queue.running = n.params["running"].as_u64().unwrap_or(0) as usize;
        }
        _ => {}
    }
}

// ---------------------------------------------------------------------------
// API 交互
// ---------------------------------------------------------------------------

/// 连接成功：订阅 + 恢复视图。
async fn on_connected(model: &mut AppModel, client: &mut Client<UnixStream>) {
    model.connected = true;
    model.conn_err = None;
    let _ = client.subscribe(methods::JOB_EVENTS, true, None).await;
    let _ = client.subscribe(methods::QUEUE_EVENTS, true, None).await;
    if let Ok(h) = client
        .call(methods::SYS_HEALTH, serde_json::json!({}))
        .await
    {
        model.queue.slots_free = h["queue"]["slots_free"].as_u64().unwrap_or(0) as usize;
        model.queue.memory_in_use_mb = h["queue"]["memory_in_use_mb"].as_u64().unwrap_or(0);
        model.health = Some(h);
    }
    if let Ok(v) = client.call(methods::INST_LIST, serde_json::json!({})).await {
        model.instances = v["instances"].as_array().cloned().unwrap_or_default();
    }
    refresh_lists(model, Some(client)).await;
    model.status_line = "已连接 daemon".into();
}

/// 刷新任务/分子列表与今日计数。
async fn refresh_lists(model: &mut AppModel, client: Option<&mut Client<UnixStream>>) {
    let Some(client) = client else { return };
    if let Ok(v) = client.call(methods::JOB_LIST, serde_json::json!({})).await {
        model.jobs = v
            .as_array()
            .map(|a| {
                a.iter()
                    .filter_map(JobView::from_json)
                    // 只展示工作流任务（父任务）：子步骤是 DAG 内部执行单元，
                    // 其输出已镜像进父任务 tail；列表展示子任务会让 "任务数"
                    // 翻倍，且对其按 c 取消会失败（而非取消）整个工作流。
                    .filter(|j| !j.is_child())
                    .collect()
            })
            .unwrap_or_default();
        let today = xtbp_core::time::now_unix() - 86_400;
        model.today_done = model
            .jobs
            .iter()
            .filter(|j| j.status == "done" && j.created_at >= today)
            .count();
        model.today_failed = model
            .jobs
            .iter()
            .filter(|j| j.status == "failed" && j.created_at >= today)
            .count();
    }
    if let Ok(v) = client
        .call(methods::MOL_LIST, serde_json::json!({ "limit": 500 }))
        .await
    {
        model.molecules = v
            .as_array()
            .map(|a| {
                a.iter()
                    .filter_map(model::MoleculeView::from_json)
                    .collect()
            })
            .unwrap_or_default();
    }
    // 详情保持选中项
    if let Some(id) = model.selected_job_id().map(String::from)
        && let Some(j) = model.jobs.iter().find(|j| j.id == id)
    {
        model.detail = Some(j.clone());
    }
}

/// 选中任务 → 拉详情 + tail + 结果。
async fn select_job(model: &mut AppModel, client: Option<&mut Client<UnixStream>>) {
    let Some(client) = client else { return };
    let Some(id) = model.selected_job_id().map(String::from) else {
        return;
    };
    if let Ok(v) = client
        .call(methods::JOB_STATUS, serde_json::json!({ "job_id": id }))
        .await
    {
        model.detail = JobView::from_json(&v);
    }
    model.tail.clear();
    model.tail_next = 0;
    model.energies.clear();
    model.results.clear();
    fetch_tail(model, Some(client)).await;
    if let Ok(v) = client
        .call(methods::RES_SCALAR, serde_json::json!({ "job_id": id }))
        .await
    {
        model.results = v["scalars"]
            .as_array()
            .map(|a| {
                a.iter()
                    .map(|r| {
                        (
                            r["key"].as_str().unwrap_or("-").to_string(),
                            r["value"].as_f64().unwrap_or(0.0),
                            r["unit"].as_str().unwrap_or("").to_string(),
                        )
                    })
                    .collect()
            })
            .unwrap_or_default();
    }
}

/// 增量 tail。
async fn fetch_tail(model: &mut AppModel, client: Option<&mut Client<UnixStream>>) {
    let Some(client) = client else { return };
    let Some(id) = model.detail.as_ref().map(|d| d.id.clone()) else {
        return;
    };
    if let Ok(v) = client
        .call(
            methods::JOB_TAIL,
            serde_json::json!({ "job_id": id, "offset": model.tail_next, "limit": 200 }),
        )
        .await
    {
        if let Some(lines) = v["lines"].as_array() {
            for l in lines {
                let seq = l["seq"].as_u64().unwrap_or(model.tail_next);
                let text = l["line"].as_str().unwrap_or("").to_string();
                if !model.tail.iter().any(|(s, _)| *s == seq) {
                    model.tail.push((seq, text));
                }
            }
            model.tail_next = v["next_offset"].as_u64().unwrap_or(model.tail_next);
            model.tail.sort_by_key(|(s, _)| *s);
            if model.tail.len() > 2000 {
                model.tail.drain(0..model.tail.len() - 2000);
            }
            model.energies = chem::extract_energies(&model.tail);
        }
        model.tail_loaded = true;
    }
}

/// 取消选中任务。
async fn cancel_selected(model: &mut AppModel, client: Option<&mut Client<UnixStream>>) {
    let Some(client) = client else { return };
    let Some(id) = model.selected_job_id().map(String::from) else {
        return;
    };
    match client
        .call(methods::JOB_CANCEL, serde_json::json!({ "job_id": id }))
        .await
    {
        Ok(_) => model.status_line = format!("已请求取消 {id}"),
        Err(e) => model.status_line = format!("取消失败: {e}"),
    }
}

/// 工作流页提交。
async fn submit_job(model: &mut AppModel, client: Option<&mut Client<UnixStream>>, smiles: &str) {
    let Some(client) = client else { return };
    let workflow = xtbp_core::BUILTIN_TEMPLATES[model
        .form
        .workflow
        .min(xtbp_core::BUILTIN_TEMPLATES.len() - 1)];
    let params = serde_json::json!({
        "smiles": smiles,
        "charge": model.form.charge,
        "multiplicity": model.form.multiplicity,
        "workflow": workflow,
        "priority": 0,
        "params": model.form.to_params_json(),
    });
    match client.call(methods::JOB_SUBMIT, params).await {
        Ok(v) => {
            if v["reused"].as_bool().unwrap_or(false) {
                model.status_line =
                    format!("命中缓存复用: {}", v["job_id"].as_str().unwrap_or("-"));
            } else {
                model.status_line =
                    format!("已提交 {workflow}: {}", v["job_id"].as_str().unwrap_or("-"));
                refresh_lists(model, Some(client)).await;
                model.page = Page::Jobs;
            }
        }
        Err(e) => {
            let code = e.structured_code().unwrap_or("ERROR");
            model.status_line = format!("提交失败: {code}: {e}");
        }
    }
}

/// 批量导入 .smi 文件（Workflows 页 `i`）：每行一个 SMILES（可带名称），
/// 以当前表单参数逐个提交当前工作流；`#`/空行/无字母行跳过。
async fn import_smi(model: &mut AppModel, client: Option<&mut Client<UnixStream>>, path: &str) {
    let Some(client) = client else { return };
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) => {
            model.status_line = format!("读取 .smi 失败: {path}: {e}");
            return;
        }
    };
    let rows = chem::parse_smi(&text);
    if rows.is_empty() {
        model.status_line = format!("{path}: 未找到有效 SMILES 行");
        return;
    }
    let workflow = xtbp_core::BUILTIN_TEMPLATES[model
        .form
        .workflow
        .min(xtbp_core::BUILTIN_TEMPLATES.len() - 1)];
    let mut submitted = 0usize;
    let mut failed = 0usize;
    for (smiles, _name) in &rows {
        let params = serde_json::json!({
            "smiles": smiles,
            "charge": model.form.charge,
            "multiplicity": model.form.multiplicity,
            "workflow": workflow,
            "priority": 0,
            "params": model.form.to_params_json(),
        });
        match client.call(methods::JOB_SUBMIT, params).await {
            Ok(_) => submitted += 1,
            Err(_) => failed += 1,
        }
    }
    model.status_line = format!(
        "导入 {path}: {} 行 → 提交 {submitted} / 失败 {failed}（{workflow}）",
        rows.len()
    );
    refresh_lists(model, Some(client)).await;
    model.page = Page::Jobs;
}

/// 切页（附带页数据加载）。
async fn switch_page(
    model: &mut AppModel,
    client: Option<&mut Client<UnixStream>>,
    backward: bool,
) {
    model.page = if backward {
        model.page.prev()
    } else {
        model.page.next()
    };
    model.status_line = model.page.title().to_string();
    match model.page {
        Page::Spectra => load_spectrum(model, client).await,
        Page::Structure => load_structure(model, client).await,
        _ => {}
    }
}

/// 移动选中（按页分发）。
async fn move_sel(model: &mut AppModel, client: Option<&mut Client<UnixStream>>, delta: isize) {
    match model.page {
        Page::Jobs => shift(&mut model.jobs_sel, model.jobs.len(), delta),
        Page::Molecules => shift(&mut model.mols_sel, model.molecules.len(), delta),
        Page::Spectra => {
            let n = model.spectra_candidates().len();
            shift(&mut model.spec_sel, n, delta);
            load_spectrum(model, client).await;
        }
        Page::Structure => {
            if delta > 0 {
                model.struct_angle += std::f64::consts::FRAC_PI_6;
            } else {
                model.struct_angle -= std::f64::consts::FRAC_PI_6;
            }
            let _ = client;
        }
        Page::Workflows => shift(
            &mut model.form.workflow,
            xtbp_core::BUILTIN_TEMPLATES.len(),
            delta,
        ),
        Page::Instances => shift(&mut model.inst_sel, model.instances.len(), delta),
        _ => {}
    }
}

fn shift(sel: &mut usize, len: usize, delta: isize) {
    if len == 0 {
        *sel = 0;
        return;
    }
    let cur = *sel as isize;
    let next = (cur + delta).clamp(0, len as isize - 1) as usize;
    *sel = next;
}

/// 加载光谱（Spectra 页）。
async fn load_spectrum(model: &mut AppModel, client: Option<&mut Client<UnixStream>>) {
    let Some(client) = client else { return };
    model.spectrum = None;
    model.spectrum_loaded = false;
    let cands = model.spectra_candidates();
    let Some(job) = cands.get(model.spec_sel) else {
        return;
    };
    let params = serde_json::json!({
        "job_id": job.id.clone(),
        "kind": "stda-gaussian",
    });
    if let Ok(v) = client.call(methods::RES_SPECTRUM, params).await {
        model.spectrum = v["points"].as_array().map(|a| {
            a.iter()
                .filter_map(|p| {
                    let arr = p.as_array()?;
                    Some((arr.first()?.as_f64()?, arr.get(1)?.as_f64()?))
                })
                .collect()
        });
        model.spectrum_loaded = true;
    } else {
        model.status_line = "光谱加载失败（该任务可能无光谱）".into();
    }
}

/// 加载结构预览（读 daemon 侧工作目录的 xyz——仅展示，不执行计算）。
async fn load_structure(model: &mut AppModel, client: Option<&mut Client<UnixStream>>) {
    let Some(client) = client else { return };
    model.xyz = None;
    model.xyz_path = None;
    let cands = model.structure_candidates();
    let Some(job) = cands.get(model.jobs_sel.min(cands.len().saturating_sub(1))) else {
        return;
    };
    let Some(workdir) = job.workdir.clone() else {
        return;
    };
    // 优先 input/mol.xyz，其次 output 下的优化结构
    let mut candidates: Vec<String> = vec![format!("{workdir}/input/mol.xyz")];
    if let Ok(entries) = std::fs::read_dir(format!("{workdir}/output")) {
        let mut outs: Vec<String> = entries
            .filter_map(|e| e.ok())
            .map(|e| e.path().display().to_string())
            .filter(|p| p.ends_with(".xyz") && (p.contains("xtbopt") || p.contains("crest_best")))
            .collect();
        outs.sort();
        candidates.extend(outs);
    }
    for path in candidates {
        if let Ok(text) = std::fs::read_to_string(&path) {
            match chem::parse_xyz(&text) {
                Ok(atoms) => {
                    model.xyz = Some(atoms);
                    model.xyz_path = Some(path);
                    return;
                }
                Err(e) => model.status_line = format!("xyz 解析失败: {e}"),
            }
        }
    }
    let _ = client;
    model.status_line = "未找到可用 xyz（任务未运行或产物缺失）".into();
}

/// 自动拉起 daemon（面向人类用户的即输即用）。
///
/// - `setsid` 脱离会话：关闭终端/TUI 后 daemon 继续存活（§2.1 关键决策）；
/// - stdio 全空：daemon 有自己的滚动日志（~/.local/share/xtbpilot/logs）；
/// - 环境继承自 TUI；XTB4STDAHOME 由 daemon 自身自动探测。
fn spawn_daemon(binary: &str) -> std::io::Result<()> {
    use std::os::unix::process::CommandExt;
    let mut cmd = std::process::Command::new(binary);
    cmd.stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    // 子进程调用 setsid 成为新会话首进程，脱离终端
    unsafe {
        cmd.pre_exec(|| {
            libc::setsid();
            Ok(())
        });
    }
    let child = cmd.spawn().map_err(|e| {
        std::io::Error::new(e.kind(), format!("拉起 {binary} 失败（是否已安装？）: {e}"))
    })?;
    // 不 wait：daemon 由自己管理生命周期
    let _ = child;
    Ok(())
}

/// 测试辅助：状态颜色映射。
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cli_parses_uds_flag() {
        let args = Args::try_parse_from(["xtbp-tui", "--uds", "~/x.sock"]).unwrap();
        assert_eq!(args.uds, "~/x.sock");
    }

    #[test]
    fn cli_parses_spawn_flags() {
        let args = Args::try_parse_from(["xtbp-tui", "--no-spawn", "--daemon", "/tmp/xd"]).unwrap();
        assert!(args.no_spawn);
        assert_eq!(args.daemon, "/tmp/xd");
    }

    #[test]
    fn page_cycle_roundtrips() {
        let mut p = Page::Dashboard;
        for _ in 0..8 {
            p = p.next();
        }
        assert_eq!(p, Page::Dashboard);
        assert_eq!(Page::Dashboard.prev(), Page::Settings);
    }

    #[test]
    fn job_view_parses_minimal_json() {
        let v: serde_json::Value = serde_json::json!({
            "id": "01X", "workflow": "opt", "status": "running",
            "molecule_id": "01M", "created_at": 100
        });
        let j = JobView::from_json(&v).unwrap();
        assert_eq!(j.status, "running");
    }

    fn key(code: KeyCode, modifiers: KeyModifiers) -> KeyEvent {
        KeyEvent::new(code, modifiers)
    }

    #[tokio::test]
    async fn ctrl_c_quits_in_normal_mode() {
        let mut m = AppModel::default();
        handle_key(
            &mut m,
            None,
            &key(KeyCode::Char('c'), KeyModifiers::CONTROL),
        )
        .await;
        assert!(m.quit, "Ctrl-C 在 Normal 模式应退出");
    }

    #[tokio::test]
    async fn ctrl_c_quits_in_filter_and_smiles_modes() {
        for mode in [InputMode::Filter, InputMode::Smiles] {
            let mut m = AppModel {
                mode,
                ..Default::default()
            };
            handle_key(
                &mut m,
                None,
                &key(KeyCode::Char('c'), KeyModifiers::CONTROL),
            )
            .await;
            assert!(m.quit, "Ctrl-C 在 {mode:?} 模式应退出");
        }
    }

    #[tokio::test]
    async fn plain_c_on_jobs_page_cancels_not_quits() {
        let mut m = AppModel {
            page: Page::Jobs,
            ..Default::default()
        };
        handle_key(&mut m, None, &key(KeyCode::Char('c'), KeyModifiers::NONE)).await;
        assert!(!m.quit, "普通 c 是取消而非退出");
    }

    #[tokio::test]
    async fn plain_q_quits() {
        let mut m = AppModel::default();
        handle_key(&mut m, None, &key(KeyCode::Char('q'), KeyModifiers::NONE)).await;
        assert!(m.quit);
    }

    #[test]
    fn jump_index_bounds() {
        assert_eq!(jump_index(0, 0, JumpTarget::First), 0);
        assert_eq!(jump_index(0, 0, JumpTarget::Last), 0);
        assert_eq!(jump_index(5, 20, JumpTarget::First), 0);
        assert_eq!(jump_index(5, 20, JumpTarget::Last), 19);
        assert_eq!(jump_index(5, 20, JumpTarget::PageUp), 0);
        assert_eq!(jump_index(5, 20, JumpTarget::PageDown), 15);
        assert_eq!(jump_index(18, 20, JumpTarget::PageDown), 19);
        assert_eq!(jump_index(0, 3, JumpTarget::PageUp), 0);
    }
}
