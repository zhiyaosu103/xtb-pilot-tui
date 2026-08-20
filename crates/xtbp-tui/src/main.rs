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
use model::{AppModel, InputMode, JobView, Page, QueueStats};
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

async fn run(terminal: &mut DefaultTerminal, args: &Args) -> Result<()> {
    let uds = expand_tilde(&args.uds);
    let mut model = AppModel::default();
    let mut client: Option<Client<UnixStream>> = None;

    // 键盘事件 → 通道（标准输入读取，不影响主循环阻塞点）
    let (key_tx, mut key_rx) = tokio::sync::mpsc::unbounded_channel::<KeyEvent>();
    tokio::spawn(async move {
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
        // 断线重连（2s 间隔；成功后恢复订阅与视图）
        if client.is_none() {
            match connect_uds(&uds, &args.token).await {
                Ok(c) => {
                    client = Some(c);
                    on_connected(&mut model, client.as_mut().unwrap()).await;
                }
                Err(e) => {
                    model.connected = false;
                    model.status_line = format!("daemon 连接失败，2s 后重试: {e}");
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

async fn handle_key(model: &mut AppModel, client: Option<&mut Client<UnixStream>>, key: &KeyEvent) {
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
        KeyCode::Char('s') if model.page == Page::Workflows => {
            model.input_buf = model.form.smiles.clone();
            model.mode = InputMode::Smiles;
        }
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
        _ => {}
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
            .map(|a| a.iter().filter_map(JobView::from_json).collect())
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
        "charge": 0,
        "multiplicity": 1,
        "workflow": workflow,
        "priority": 0,
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
}
