//! 渲染层：八页视图（设计文档 §3.6）。长列表虚拟滚动（只渲染可见窗口）。

use crate::chem::{element_color, fit_to_canvas, project};
use crate::model::{AppModel, InputMode, Page};
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::symbols;
use ratatui::text::{Line, Span};
use ratatui::widgets::canvas::{Canvas, Points};
use ratatui::widgets::{
    Block, Borders, List, ListItem, ListState, Paragraph, Sparkline, Tabs, Wrap,
};
use xtbp_core::time::{format_duration, format_unix};

/// 渲染一帧。
pub fn draw(frame: &mut Frame, model: &mut AppModel) {
    let area = frame.area();
    let chunks = Layout::vertical([
        Constraint::Length(1), // 顶部 Tab
        Constraint::Min(3),    // 页面主体
        Constraint::Length(1), // 状态行
    ])
    .split(area);

    draw_tabs(frame, model, chunks[0]);
    match model.page {
        Page::Dashboard => draw_dashboard(frame, model, chunks[1]),
        Page::Molecules => draw_molecules(frame, model, chunks[1]),
        Page::Jobs => draw_jobs(frame, model, chunks[1]),
        Page::Spectra => draw_spectra(frame, model, chunks[1]),
        Page::Structure => draw_structure(frame, model, chunks[1]),
        Page::Workflows => draw_workflows(frame, model, chunks[1]),
        Page::Instances => draw_instances(frame, model, chunks[1]),
        Page::Settings => draw_settings(frame, model, chunks[1]),
    }
    draw_status_line(frame, model, chunks[2]);
    if model.show_help {
        draw_help(frame, area);
    }
}

fn draw_tabs(frame: &mut Frame, model: &AppModel, area: Rect) {
    let titles: Vec<Line> = Page::ALL
        .iter()
        .map(|p| Line::from(format!(" {} ", p.title())))
        .collect();
    let tabs = Tabs::new(titles)
        .select(model.page.index())
        .style(Style::default().fg(Color::DarkGray))
        .highlight_style(Style::default().fg(Color::Black).bg(Color::LightCyan))
        .divider(symbols::line::VERTICAL);
    frame.render_widget(tabs, area);
}

fn draw_status_line(frame: &mut Frame, model: &AppModel, area: Rect) {
    let (conn, conn_color) = if model.connected {
        ("●", Color::Green)
    } else {
        ("○", Color::LightRed)
    };
    let mode_hint = match model.mode {
        InputMode::Normal => "hjkl 移动 · Tab 切页 · / 过滤 · ? 帮助 · q/Ctrl-C 退出",
        InputMode::Filter => "输入过滤词，Enter 应用，Esc 取消，Ctrl-C 退出",
        InputMode::Smiles => "输入 SMILES，Enter 提交，Esc 取消，Ctrl-C 退出",
        InputMode::SmiPath => "输入 .smi 文件路径，Enter 批量导入，Esc 取消",
        InputMode::Solvent => "输入溶剂名（如 water/toluene/thf），Enter 应用",
        InputMode::Number(field) => match field {
            crate::model::NumberField::Etemp => "输入电子温度 K（如 500），Enter 应用",
            crate::model::NumberField::Accuracy => "输入 SCF 精度（如 0.5），Enter 应用",
            crate::model::NumberField::Maxiter => "输入 SCF 最大迭代（如 250），Enter 应用",
            crate::model::NumberField::Charge => "输入电荷（如 -1/0/1），Enter 应用",
            crate::model::NumberField::Multiplicity => "输入多重度（如 1/2/3），Enter 应用",
        },
    };
    let status = if model.status_line.is_empty() {
        mode_hint.to_string()
    } else {
        model.status_line.clone()
    };
    let line = Line::from(vec![
        Span::styled(conn, Style::default().fg(conn_color)),
        Span::raw(format!(
            " q{}/r{} · {}",
            model.queue.queued, model.queue.running, status
        )),
    ]);
    frame.render_widget(Paragraph::new(line), area);
}

fn block(title: impl Into<Line<'static>>) -> Block<'static> {
    Block::default().borders(Borders::ALL).title(title)
}

// ---------------------------------------------------------------------------
// Dashboard
// ---------------------------------------------------------------------------

fn draw_dashboard(frame: &mut Frame, model: &AppModel, area: Rect) {
    let cols = Layout::horizontal([
        Constraint::Percentage(40),
        Constraint::Percentage(30),
        Constraint::Percentage(30),
    ])
    .split(area);

    // 左：队列与健康
    let health = model.health.clone().unwrap_or_default();
    let lines = vec![
        Line::from(format!(
            "daemon 版本: {}",
            health["daemon"].as_str().unwrap_or("-")
        )),
        Line::from(format!(
            "运行时长: {}",
            format_duration(health["uptime_secs"].as_u64().unwrap_or(0))
        )),
        Line::from(format!(
            "并发槽: 运行 {} / 排队 {} / 空闲 {}",
            model.queue.running, model.queue.queued, model.queue.slots_free
        )),
        Line::from(format!("内存令牌占用: {} MB", model.queue.memory_in_use_mb)),
        Line::from(format!(
            "RDKit helper: {}",
            if health["helper_alive"].as_bool().unwrap_or(false) {
                "在线"
            } else {
                "未拉起"
            }
        )),
        Line::from(format!(
            "数据目录: {}（Linux FS: {}）",
            health["data_dir"].as_str().unwrap_or("-"),
            if health["data_dir_on_linux_fs"].as_bool().unwrap_or(true) {
                "✓"
            } else {
                "✗ /mnt 红线"
            }
        )),
        Line::from(""),
        Line::from(format!("今日完成: {}", model.today_done)),
        Line::from(format!("今日失败: {}", model.today_failed)),
    ];
    frame.render_widget(Paragraph::new(lines).block(block("系统状态")), cols[0]);

    // 中：运行中任务
    let running: Vec<&crate::model::JobView> = model
        .jobs
        .iter()
        .filter(|j| j.status == "running")
        .collect();
    let items: Vec<ListItem> = running
        .iter()
        .take(20)
        .map(|j| {
            ListItem::new(Line::from(format!(" {}  {}", j.workflow, short_id(&j.id))))
                .style(Style::default().fg(Color::LightBlue))
        })
        .collect();
    frame.render_widget(
        List::new(items)
            .block(block(format!("运行中 ({})", running.len())))
            .highlight_style(Style::default().fg(Color::Black).bg(Color::LightBlue)),
        cols[1],
    );

    // 右：排队任务
    let queued: Vec<&crate::model::JobView> =
        model.jobs.iter().filter(|j| j.status == "queued").collect();
    let items: Vec<ListItem> = queued
        .iter()
        .take(20)
        .map(|j| ListItem::new(format!(" {}  {}", j.workflow, short_id(&j.id))))
        .collect();
    frame.render_widget(
        List::new(items).block(block(format!("排队中 ({})", queued.len()))),
        cols[2],
    );
}

// ---------------------------------------------------------------------------
// Molecules
// ---------------------------------------------------------------------------

fn draw_molecules(frame: &mut Frame, model: &mut AppModel, area: Rect) {
    let cols =
        Layout::horizontal([Constraint::Percentage(50), Constraint::Percentage(50)]).split(area);
    let h = cols[0].height as usize - 2;
    clamp_sel(
        model.filtered_molecules_len(),
        &mut model.mols_sel,
        &mut model.mols_scroll,
        h,
    );
    let mols = model.filtered_molecules();
    let mut state = ListState::default();
    state.select(Some(model.mols_sel));
    let items: Vec<ListItem> =
        visible_window(&mols, model.mols_scroll, cols[0].height as usize - 2)
            .iter()
            .enumerate()
            .map(|(i, m)| {
                let idx = model.mols_scroll + i;
                ListItem::new(Line::from(vec![
                    Span::styled(
                        if idx == model.mols_sel { "▸ " } else { "  " },
                        Style::default().fg(Color::LightCyan),
                    ),
                    Span::raw(format!(
                        "{:<12} [q={} m={}] {}",
                        short_id(&m.id),
                        m.charge,
                        m.multiplicity,
                        truncate(&m.smiles, 40)
                    )),
                ]))
            })
            .collect();
    frame.render_widget(
        List::new(items)
            .block(block(format!("分子 ({})", mols.len())))
            .highlight_style(Style::default().fg(Color::Black).bg(Color::LightCyan)),
        cols[0],
    );

    // 右：详情
    let detail = model
        .molecules
        .get(model.mols_sel.min(mols.len().saturating_sub(1)))
        .cloned();
    let mut lines = vec![Line::from("（无）")];
    if let Some(m) = detail {
        lines = vec![
            Line::from(format!("id: {}", m.id)),
            Line::from(format!("SMILES: {}", m.smiles)),
            Line::from(format!(
                "InChIKey: {}",
                if m.inchikey.is_empty() {
                    "（未计算）"
                } else {
                    &m.inchikey
                }
            )),
            Line::from(format!("电荷 {} · 多重度 {}", m.charge, m.multiplicity)),
            Line::from(format!("名称: {}", m.name.as_deref().unwrap_or("-"))),
            Line::from(format!("创建: {}", format_unix(m.created_at))),
            Line::from(""),
            Line::from(format!(
                "历史任务: {}",
                model.jobs.iter().filter(|j| j.molecule_id == m.id).count()
            )),
        ];
    }
    frame.render_widget(
        Paragraph::new(lines)
            .block(block("详情"))
            .wrap(Wrap { trim: true }),
        cols[1],
    );
}

// ---------------------------------------------------------------------------
// Jobs
// ---------------------------------------------------------------------------

fn draw_jobs(frame: &mut Frame, model: &mut AppModel, area: Rect) {
    let rows =
        Layout::vertical([Constraint::Percentage(50), Constraint::Percentage(50)]).split(area);
    let h = rows[0].height as usize - 2;
    clamp_sel(
        model.filtered_jobs_len(),
        &mut model.jobs_sel,
        &mut model.jobs_scroll,
        h,
    );
    let jobs = model.filtered_jobs();
    let items: Vec<ListItem> = visible_window(&jobs, model.jobs_scroll, h)
        .iter()
        .enumerate()
        .map(|(i, j)| {
            let idx = model.jobs_scroll + i;
            ListItem::new(Line::from(vec![
                Span::styled(
                    if idx == model.jobs_sel { "▸ " } else { "  " },
                    Style::default().fg(Color::LightCyan),
                ),
                Span::styled(
                    format!("{:<11}", j.status),
                    Style::default().fg(AppModel::status_color(&j.status)),
                ),
                Span::raw(format!("{:<10} {}", j.workflow, short_id(&j.id))),
                if j.parse_degraded {
                    Span::styled(" ⚠", Style::default().fg(Color::Yellow))
                } else {
                    Span::raw("")
                },
            ]))
        })
        .collect();
    frame.render_widget(
        List::new(items)
            .block(block(format!("任务 ({}) · g 刷新 · c 取消", jobs.len())))
            .highlight_style(Style::default().fg(Color::Black).bg(Color::LightCyan)),
        rows[0],
    );

    // 下：选中任务详情 + tail + 收敛曲线
    let detail_rows = Layout::vertical([Constraint::Min(6), Constraint::Length(6)]).split(rows[1]);
    let mut lines = vec![Line::from("（未选择）")];
    if let Some(d) = &model.detail {
        lines = vec![
            Line::from(format!("{} · {} · {}", d.id, d.workflow, d.status)),
            Line::from(format!(
                "创建 {} · 结束 {} · 退出码 {}",
                format_unix(d.created_at),
                d.finished_at.map(format_unix).unwrap_or_else(|| "-".into()),
                d.exit_code
                    .map(|c| c.to_string())
                    .unwrap_or_else(|| "-".into())
            )),
            Line::from(format!(
                "错误: {} {}",
                d.error_code.as_deref().unwrap_or("-"),
                d.error_message.as_deref().unwrap_or("")
            )),
            Line::from(format!("workdir: {}", d.workdir.as_deref().unwrap_or("-"))),
            Line::from(format!("结果标量: {} 项", model.results.len())),
        ];
        // tail 尾部 5 行
        for (_, l) in model.tail.iter().rev().take(5).rev() {
            lines.push(Line::from(truncate(l, 110)));
        }
    }
    frame.render_widget(
        Paragraph::new(lines)
            .block(block("详情 / tail"))
            .wrap(Wrap { trim: true }),
        detail_rows[0],
    );

    // 收敛曲线（Braille Sparkline，§3.6）
    let data: Vec<u64> = normalize_energies(&model.energies, 100);
    let spark = Sparkline::default()
        .block(block("SCF/优化能量收敛"))
        .data(if data.is_empty() {
            &[0u64][..]
        } else {
            &data[..]
        })
        .style(Style::default().fg(Color::LightGreen))
        .max(100);
    frame.render_widget(spark, detail_rows[1]);
}

/// 能量序列 → sparkline 高度（0..=100，越收敛越平）。
fn normalize_energies(energies: &[f64], max_h: u64) -> Vec<u64> {
    if energies.len() < 2 {
        return Vec::new();
    }
    let last = *energies.last().unwrap();
    let max_delta = energies
        .iter()
        .map(|e| (e - last).abs())
        .fold(1e-12, f64::max);
    energies
        .iter()
        .map(|e| {
            let d = (e - last).abs() / max_delta;
            (d * max_h as f64) as u64
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Spectra
// ---------------------------------------------------------------------------

fn draw_spectra(frame: &mut Frame, model: &mut AppModel, area: Rect) {
    let rows = Layout::vertical([Constraint::Length(3), Constraint::Min(4)]).split(area);
    let n = model.spectra_candidates().len();
    if n == 0 {
        frame.render_widget(
            Paragraph::new("尚无 completed 的 excited 任务。").block(block("光谱")),
            rows[1],
        );
        return;
    }
    model.spec_sel = model.spec_sel.min(n - 1);
    let cands = model.spectra_candidates();
    let label = cands
        .get(model.spec_sel)
        .map(|j| format!("{} · {}", j.workflow, short_id(&j.id)))
        .unwrap_or_default();
    frame.render_widget(
        Paragraph::new(format!("◂ ► 选择任务: {label}")).block(block("sTDA 光谱")),
        rows[0],
    );

    if let Some(points) = &model.spectrum {
        let spark_data: Vec<u64> = downscale(points, rows[1].width as usize * 2);
        let max_i = points.iter().map(|(_, i)| *i).fold(1e-12, f64::max);
        let (wl_min, wl_max) = points
            .iter()
            .fold((f64::INFINITY, 0.0f64), |(lo, hi), (w, _)| {
                (lo.min(*w), hi.max(*w))
            });
        let spark = Sparkline::default()
            .block(block(format!(
                "波长 {wl_min:.0}–{wl_max:.0} nm · 峰值 {max_i:.3}",
            )))
            .data(&spark_data)
            .style(Style::default().fg(Color::LightMagenta))
            .max(spark_data.iter().copied().max().unwrap_or(1).max(1));
        frame.render_widget(spark, rows[1]);
    } else {
        frame.render_widget(
            Paragraph::new(if model.spectrum_loaded {
                "该任务无光谱数据。"
            } else {
                "加载中…"
            })
            .block(block("光谱")),
            rows[1],
        );
    }
}

/// 下采样到 sparkline 宽度。
fn downscale(points: &[(f64, f64)], width: usize) -> Vec<u64> {
    if points.is_empty() || width == 0 {
        return Vec::new();
    }
    let max_i = points.iter().map(|(_, i)| *i).fold(1e-12, f64::max);
    let bucket = points.len().div_ceil(width);
    let mut out = Vec::with_capacity(width);
    for start in (0..points.len()).step_by(bucket) {
        let seg = &points[start..(start + bucket).min(points.len())];
        let peak = seg.iter().map(|(_, i)| *i).fold(0.0, f64::max);
        out.push(((peak / max_i) * 100.0) as u64);
    }
    out
}

// ---------------------------------------------------------------------------
// Structure（Braille 点云）
// ---------------------------------------------------------------------------

fn draw_structure(frame: &mut Frame, model: &mut AppModel, area: Rect) {
    let cols =
        Layout::horizontal([Constraint::Percentage(30), Constraint::Percentage(70)]).split(area);
    let cands = model.structure_candidates();
    let items: Vec<ListItem> = cands
        .iter()
        .take(cols[0].height as usize)
        .map(|j| ListItem::new(format!(" {}  {}", j.workflow, short_id(&j.id))))
        .collect();
    let mut state = ListState::default();
    state.select(Some(model.jobs_sel.min(cands.len().saturating_sub(1))));
    frame.render_widget(
        List::new(items)
            .block(block("结构来源"))
            .highlight_style(Style::default().fg(Color::Black).bg(Color::LightCyan)),
        cols[0],
    );

    let canvas = Canvas::default()
        .block(block("Braille 点云 · ◂ ► 旋转 · o 外部查看器"))
        .marker(ratatui::symbols::Marker::Braille)
        .x_bounds([0.0, 1.0])
        .y_bounds([0.0, 1.0])
        .paint(|ctx| {
            if let Some(atoms) = &model.xyz {
                let pts = project(atoms, model.struct_angle);
                let fitted = fit_to_canvas(&pts, 0.0, 1.0, 0.0, 1.0);
                // 按元素分组着色
                let mut by_element: std::collections::BTreeMap<&str, Vec<(f64, f64)>> =
                    std::collections::BTreeMap::new();
                for (x, y, e) in fitted {
                    by_element.entry(e).or_default().push((x, y));
                }
                for (e, coords) in by_element {
                    ctx.draw(&Points {
                        coords: &coords,
                        color: element_color(e),
                    });
                }
            }
        });
    frame.render_widget(canvas, cols[1]);
}

// ---------------------------------------------------------------------------
// Workflows
// ---------------------------------------------------------------------------

fn draw_workflows(frame: &mut Frame, model: &mut AppModel, area: Rect) {
    let cols =
        Layout::horizontal([Constraint::Percentage(45), Constraint::Percentage(55)]).split(area);
    let wfs = xtbp_core::BUILTIN_TEMPLATES;
    // 提交表单的选中与列表高亮保持同步（move_sel 走 form.workflow）
    model.wf_sel = model.form.workflow.min(wfs.len() - 1);
    let items: Vec<ListItem> = wfs
        .iter()
        .enumerate()
        .map(|(i, w)| {
            ListItem::new(Line::from(vec![
                Span::styled(
                    if i == model.wf_sel { "▸ " } else { "  " },
                    Style::default().fg(Color::LightCyan),
                ),
                Span::raw(*w),
            ]))
        })
        .collect();
    frame.render_widget(
        List::new(items)
            .block(block("内置模板"))
            .highlight_style(Style::default().fg(Color::Black).bg(Color::LightCyan)),
        cols[0],
    );

    let desc = match wfs.get(model.wf_sel) {
        Some(&"opt") => "gen3d → GFN2-xTB 优化（tight）：能量、HL gap、偶极矩",
        Some(&"sp") => "gen3d → GFN2-xTB 单点能（--sp，gas）",
        Some(&"conformer") => "gen3d → CREST 构象搜索（能量、布居）",
        Some(&"opt-freq") => "opt → --ohess 频率（热力学校正、虚频计数）",
        Some(&"excited") => "opt → xtb4stda → stda：垂直激发能、振子强度、展宽谱",
        Some(&"redox") => "中性/阳离子/阴离子三态 opt（三点能量与结构）",
        Some(&"reorg-4pt") => "四点法重组能：λ_h / λ_e 直接算出",
        Some(&"solv-series") => "同一结构多溶剂 ALPB 单点能量表",
        _ => "",
    };
    let smi = match model.mode {
        InputMode::Smiles => format!("{}▏", model.input_buf),
        _ => model.form.smiles.clone(),
    };
    let fam = crate::model::FAMILY_LABELS[model.form.family.min(3)];
    let solv = crate::model::SOLVATION_LABELS[model.form.solvation.min(2)];
    let solv_name = if model.form.solvation > 0 {
        model.form.solvent.as_str()
    } else {
        "-"
    };
    let etemp = model
        .form
        .etemp
        .map(|v| v.to_string())
        .unwrap_or_else(|| "缺省(300K)".into());
    let acc = model
        .form
        .accuracy
        .map(|v| v.to_string())
        .unwrap_or_else(|| "缺省(1.0)".into());
    let maxiter = model
        .form
        .maxiter
        .map(|v| v.to_string())
        .unwrap_or_else(|| "缺省".into());
    let lines = vec![
        Line::from(""),
        Line::from(desc),
        Line::from(""),
        Line::from(Span::styled(
            "SMILES（Enter/s 输入 · 提交）",
            Style::default().fg(Color::DarkGray),
        )),
        Line::from(Span::styled(smi, Style::default().fg(Color::LightYellow))),
        Line::from(""),
        Line::from(Span::styled(
            format!("计算水平 [ ]: {fam}"),
            Style::default().fg(Color::LightCyan),
        )),
        Line::from(Span::styled(
            format!("溶剂模型 {{ }}: {solv} · 溶剂 e: {solv_name}"),
            Style::default().fg(Color::LightCyan),
        )),
        Line::from(Span::styled(
            format!("etemp t: {etemp} · acc a: {acc} · maxiter m: {maxiter}"),
            Style::default().fg(Color::LightCyan),
        )),
        Line::from(Span::styled(
            format!(
                "电荷 c: {} · 多重度 n: {}",
                model.form.charge, model.form.multiplicity
            ),
            Style::default().fg(Color::LightCyan),
        )),
        Line::from(""),
        Line::from(Span::styled(
            "i: 批量导入 .smi（当前参数逐行提交）",
            Style::default().fg(Color::DarkGray),
        )),
        Line::from("提交后 Jobs 页实时监控；sTDA 谱在 Spectra 页"),
    ];
    frame.render_widget(
        Paragraph::new(lines)
            .block(block("提交表单"))
            .wrap(Wrap { trim: true }),
        cols[1],
    );
}

// ---------------------------------------------------------------------------
// Instances
// ---------------------------------------------------------------------------

fn draw_instances(frame: &mut Frame, model: &mut AppModel, area: Rect) {
    let cols =
        Layout::horizontal([Constraint::Percentage(70), Constraint::Percentage(30)]).split(area);
    let items: Vec<ListItem> = model
        .instances
        .iter()
        .take(cols[0].height as usize)
        .map(|v| {
            ListItem::new(Line::from(format!(
                " {:<10} v{:<10} {}",
                v["name"].as_str().unwrap_or("-"),
                v["version"].as_str().unwrap_or("-"),
                v["exe"].as_str().unwrap_or("-"),
            )))
        })
        .collect();
    let mut state = ListState::default();
    state.select(Some(
        model.inst_sel.min(model.instances.len().saturating_sub(1)),
    ));
    frame.render_widget(
        List::new(items)
            .block(block(format!("组件登记 ({})", model.instances.len())))
            .highlight_style(Style::default().fg(Color::Black).bg(Color::LightCyan)),
        cols[0],
    );

    let detail = model
        .instances
        .get(model.inst_sel)
        .cloned()
        .unwrap_or_default();
    let caps: Vec<&str> = detail["capabilities"]
        .as_array()
        .map(|a| a.iter().filter_map(|v| v.as_str()).collect())
        .unwrap_or_default();
    let lines = vec![
        Line::from(format!("名称: {}", detail["name"].as_str().unwrap_or("-"))),
        Line::from(format!(
            "版本: {}",
            detail["version"].as_str().unwrap_or("-")
        )),
        Line::from(format!("路径: {}", detail["exe"].as_str().unwrap_or("-"))),
        Line::from(format!(
            "sha256: {}",
            detail["sha256"].as_str().unwrap_or("-")
        )),
        Line::from(""),
        Line::from("能力:"),
        Line::from(caps.join(" · ")),
    ];
    frame.render_widget(
        Paragraph::new(lines)
            .block(block("实例详情"))
            .wrap(Wrap { trim: true }),
        cols[1],
    );
}

// ---------------------------------------------------------------------------
// Settings / Help
// ---------------------------------------------------------------------------

fn draw_settings(frame: &mut Frame, model: &AppModel, area: Rect) {
    let health = model.health.clone().unwrap_or_default();
    let lines = vec![
        Line::from("连接: UDS（daemon 为 TUI 保留的本地通道）"),
        Line::from("协议: xtbp-jsonrpc-v1（NDJSON，事件订阅推送）"),
        Line::from("数据目录（daemon 侧）:"),
        Line::from(Span::styled(
            format!("  {}", health["data_dir"].as_str().unwrap_or("-")),
            Style::default().fg(Color::LightCyan),
        )),
        Line::from("模板目录（daemon 侧）:"),
        Line::from(Span::styled(
            format!("  {}", health["templates_dir"].as_str().unwrap_or("-")),
            Style::default().fg(Color::LightCyan),
        )),
        Line::from(""),
        Line::from("配置修改：编辑 ~/.local/share/xtbpilot/registry.toml 后重启 daemon"),
        Line::from("（配置热重载为后续里程碑；方法学边界：全部结果为筛选级 screening）"),
    ];
    frame.render_widget(
        Paragraph::new(lines)
            .block(block("设置"))
            .wrap(Wrap { trim: true }),
        area,
    );
}

fn draw_help(frame: &mut Frame, area: Rect) {
    let popup = centered_rect(area, 66, 20);
    let text = vec![
        Line::from(Span::styled(
            "xTB-Pilot 键位帮助",
            Style::default().add_modifier(Modifier::BOLD),
        )),
        Line::from(""),
        Line::from("Tab / Shift+Tab   页面切换        g        刷新（重新拉取列表）"),
        Line::from("j k / ↑ ↓         列表移动        Space    选中任务看详情"),
        Line::from("Home / End         列表首 / 尾     PgUp/PgDn 翻页"),
        Line::from("/                 过滤（Enter 应用，Esc 取消）"),
        Line::from(""),
        Line::from(Span::styled(
            "Workflows 页提交表单：",
            Style::default().add_modifier(Modifier::BOLD),
        )),
        Line::from("Enter / s     输入 SMILES 并提交      i      批量导入 .smi 文件"),
        Line::from(
            "[ ]            切换计算水平（GFN2/1/0/FF）  { }  切换溶剂模型（气相/ALPB/GBSA）",
        ),
        Line::from("e              溶剂名        t/a/m     etemp/accuracy/maxiter"),
        Line::from("c / n          电荷 / 多重度"),
        Line::from(""),
        Line::from("c（Jobs 页）   取消选中任务        o（Structure 页）外部查看器"),
        Line::from("?                 本帮助        q / Esc / Ctrl-C  退出"),
        Line::from(""),
        Line::from(Span::styled(
            "任何时刻 Ctrl-C 或 q 退出；daemon 独立运行，关 TUI 不中断计算。",
            Style::default().fg(Color::DarkGray),
        )),
    ];
    frame.render_widget(
        Paragraph::new(text).block(Block::default().borders(Borders::ALL).title("帮助")),
        popup,
    );
}

fn centered_rect(area: Rect, width: u16, height: u16) -> Rect {
    let x = area.x + area.width.saturating_sub(width) / 2;
    let y = area.y + area.height.saturating_sub(height) / 2;
    Rect::new(x, y, width.min(area.width), height.min(area.height))
}

// ---------------------------------------------------------------------------
// 辅助
// ---------------------------------------------------------------------------

fn short_id(id: &str) -> &str {
    id.get(..8).unwrap_or(id)
}

fn truncate(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        s.to_string()
    } else {
        let cut: String = s.chars().take(n.saturating_sub(1)).collect();
        format!("{cut}…")
    }
}

/// 虚拟滚动窗口（只返回可见切片）。
fn visible_window<T>(items: &[T], scroll: usize, height: usize) -> &[T] {
    let start = scroll.min(items.len().saturating_sub(1));
    let end = (start + height.max(1)).min(items.len());
    items.get(start..end).unwrap_or_default()
}

/// 选择与滚动校正（按条数，避免渲染层借用冲突）。
fn clamp_sel(len: usize, sel: &mut usize, scroll: &mut usize, height: usize) {
    if len == 0 {
        *sel = 0;
        *scroll = 0;
        return;
    }
    *sel = (*sel).min(len - 1);
    let h = height.max(1);
    if *sel < *scroll {
        *scroll = *sel;
    } else if *sel >= *scroll + h {
        *scroll = *sel + 1 - h;
    }
}
