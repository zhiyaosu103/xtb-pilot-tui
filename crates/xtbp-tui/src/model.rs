//! TUI 状态模型（设计文档 §3.6：事件订阅驱动渲染，不轮询不阻塞）。

use crate::chem::Atom;
use serde_json::Value;

/// 页面（Tab 切换，§3.6 八页）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Page {
    Dashboard,
    Molecules,
    Jobs,
    Spectra,
    Structure,
    Workflows,
    Instances,
    Settings,
}

impl Page {
    /// 全部页面（顺序即 Tab 循环序）。
    pub const ALL: [Page; 8] = [
        Page::Dashboard,
        Page::Molecules,
        Page::Jobs,
        Page::Spectra,
        Page::Structure,
        Page::Workflows,
        Page::Instances,
        Page::Settings,
    ];

    /// 页名。
    pub fn title(&self) -> &'static str {
        match self {
            Page::Dashboard => "Dashboard",
            Page::Molecules => "Molecules",
            Page::Jobs => "Jobs",
            Page::Spectra => "Spectra",
            Page::Structure => "Structure",
            Page::Workflows => "Workflows",
            Page::Instances => "Instances",
            Page::Settings => "Settings",
        }
    }

    pub fn index(&self) -> usize {
        Self::ALL.iter().position(|p| p == self).unwrap_or(0)
    }

    pub fn next(&self) -> Page {
        Self::ALL[(self.index() + 1) % Self::ALL.len()]
    }

    pub fn prev(&self) -> Page {
        Self::ALL[(self.index() + Self::ALL.len() - 1) % Self::ALL.len()]
    }
}

/// 队列快照（QueueDepth 事件）。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct QueueStats {
    pub queued: usize,
    pub running: usize,
    pub slots_free: usize,
    pub memory_in_use_mb: u64,
}

/// 任务视图（job.list 行 + 事件增量）。
#[derive(Debug, Clone)]
pub struct JobView {
    pub id: String,
    pub workflow: String,
    pub status: String,
    pub molecule_id: String,
    pub created_at: i64,
    pub finished_at: Option<i64>,
    pub error_code: Option<String>,
    pub error_message: Option<String>,
    pub parse_degraded: bool,
    pub workdir: Option<String>,
    pub exit_code: Option<i64>,
}

impl JobView {
    /// 由 job JSON（job.list / job.status 响应）构造。
    pub fn from_json(v: &Value) -> Option<Self> {
        Some(Self {
            id: v.get("id")?.as_str()?.to_string(),
            workflow: v.get("workflow")?.as_str()?.to_string(),
            status: v.get("status")?.as_str()?.to_string(),
            molecule_id: v
                .get("molecule_id")
                .and_then(|x| x.as_str())
                .unwrap_or("")
                .to_string(),
            created_at: v.get("created_at").and_then(|x| x.as_i64()).unwrap_or(0),
            finished_at: v.get("finished_at").and_then(|x| x.as_i64()),
            error_code: v
                .get("error_code")
                .and_then(|x| x.as_str())
                .map(String::from),
            error_message: v
                .get("error_message")
                .and_then(|x| x.as_str())
                .map(String::from),
            parse_degraded: v
                .get("parse_degraded")
                .and_then(|x| x.as_bool())
                .unwrap_or(false),
            workdir: v.get("workdir").and_then(|x| x.as_str()).map(String::from),
            exit_code: v.get("exit_code").and_then(|x| x.as_i64()),
        })
    }
}

/// 分子视图。
#[derive(Debug, Clone)]
pub struct MoleculeView {
    pub id: String,
    pub smiles: String,
    pub inchikey: String,
    pub charge: i8,
    pub multiplicity: u8,
    pub name: Option<String>,
    pub created_at: i64,
}

impl MoleculeView {
    pub fn from_json(v: &Value) -> Option<Self> {
        Some(Self {
            id: v.get("id")?.as_str()?.to_string(),
            smiles: v.get("smiles")?.as_str()?.to_string(),
            inchikey: v
                .get("inchikey")
                .and_then(|x| x.as_str())
                .unwrap_or("")
                .to_string(),
            charge: v.get("charge").and_then(|x| x.as_i64()).unwrap_or(0) as i8,
            multiplicity: v.get("multiplicity").and_then(|x| x.as_i64()).unwrap_or(1) as u8,
            name: v.get("name").and_then(|x| x.as_str()).map(String::from),
            created_at: v.get("created_at").and_then(|x| x.as_i64()).unwrap_or(0),
        })
    }
}

/// 输入模式（Vim 风格）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InputMode {
    /// 常规导航。
    Normal,
    /// 列表过滤（/）。
    Filter,
    /// 工作流页输入 SMILES。
    Smiles,
}

/// 工作流提交表单。
#[derive(Debug, Clone)]
pub struct SubmitForm {
    pub workflow: usize,
    pub smiles: String,
}

/// 应用模型（单线程持有，事件驱动更新）。
pub struct AppModel {
    pub page: Page,
    pub connected: bool,
    pub conn_err: Option<String>,
    pub queue: QueueStats,
    pub jobs: Vec<JobView>,
    pub molecules: Vec<MoleculeView>,
    pub instances: Vec<Value>,
    pub health: Option<Value>,
    pub today_done: usize,
    pub today_failed: usize,

    // 选择与滚动
    pub jobs_sel: usize,
    pub jobs_scroll: usize,
    pub mols_sel: usize,
    pub mols_scroll: usize,
    pub spec_sel: usize,
    pub inst_sel: usize,
    pub wf_sel: usize,
    pub struct_angle: f64,

    // 详情（选中任务）
    pub detail: Option<JobView>,
    pub tail: Vec<(u64, String)>,
    pub tail_next: u64,
    pub tail_loaded: bool,
    pub energies: Vec<f64>,
    pub results: Vec<(String, f64, String)>,
    pub spectrum: Option<Vec<(f64, f64)>>,
    pub spectrum_loaded: bool,

    // 结构预览
    pub xyz: Option<Vec<Atom>>,
    pub xyz_path: Option<String>,

    // 交互
    pub mode: InputMode,
    pub input_buf: String,
    pub filter: String,
    pub show_help: bool,
    pub status_line: String,
    pub form: SubmitForm,
    pub quit: bool,
}

impl Default for AppModel {
    fn default() -> Self {
        Self {
            page: Page::Dashboard,
            connected: false,
            conn_err: None,
            queue: QueueStats::default(),
            jobs: Vec::new(),
            molecules: Vec::new(),
            instances: Vec::new(),
            health: None,
            today_done: 0,
            today_failed: 0,
            jobs_sel: 0,
            jobs_scroll: 0,
            mols_sel: 0,
            mols_scroll: 0,
            spec_sel: 0,
            inst_sel: 0,
            wf_sel: 0,
            struct_angle: 0.0,
            detail: None,
            tail: Vec::new(),
            tail_next: 0,
            tail_loaded: false,
            energies: Vec::new(),
            results: Vec::new(),
            spectrum: None,
            spectrum_loaded: false,
            xyz: None,
            xyz_path: None,
            mode: InputMode::Normal,
            input_buf: String::new(),
            filter: String::new(),
            show_help: false,
            status_line: "未连接 daemon".into(),
            form: SubmitForm {
                workflow: 0,
                smiles: "C1=CC=CC=C1".into(),
            },
            quit: false,
        }
    }
}

impl AppModel {
    /// 当前页选中任务 id。
    pub fn selected_job_id(&self) -> Option<&str> {
        self.jobs.get(self.jobs_sel).map(|j| j.id.as_str())
    }

    /// 过滤后的任务列表（Jobs 页）。
    pub fn filtered_jobs(&self) -> Vec<&JobView> {
        if self.filter.is_empty() {
            self.jobs.iter().collect()
        } else {
            self.jobs
                .iter()
                .filter(|j| {
                    j.id.contains(&self.filter)
                        || j.workflow.contains(&self.filter)
                        || j.status.contains(&self.filter)
                })
                .collect()
        }
    }

    /// 过滤后的任务数（避免渲染层借用冲突）。
    pub fn filtered_jobs_len(&self) -> usize {
        if self.filter.is_empty() {
            self.jobs.len()
        } else {
            self.jobs
                .iter()
                .filter(|j| {
                    j.id.contains(&self.filter)
                        || j.workflow.contains(&self.filter)
                        || j.status.contains(&self.filter)
                })
                .count()
        }
    }

    /// 过滤后的分子数。
    pub fn filtered_molecules_len(&self) -> usize {
        if self.filter.is_empty() {
            self.molecules.len()
        } else {
            self.molecules
                .iter()
                .filter(|m| {
                    m.id.contains(&self.filter)
                        || m.smiles.contains(&self.filter)
                        || m.inchikey.contains(&self.filter)
                })
                .count()
        }
    }

    /// 过滤后的分子列表。
    pub fn filtered_molecules(&self) -> Vec<&MoleculeView> {
        if self.filter.is_empty() {
            self.molecules.iter().collect()
        } else {
            self.molecules
                .iter()
                .filter(|m| {
                    m.id.contains(&self.filter)
                        || m.smiles.contains(&self.filter)
                        || m.inchikey.contains(&self.filter)
                })
                .collect()
        }
    }

    /// 有光谱的任务（Spectra 页候选）。
    pub fn spectra_candidates(&self) -> Vec<&JobView> {
        self.jobs
            .iter()
            .filter(|j| j.status == "done" && j.workflow == "excited")
            .collect()
    }

    /// 有 workdir 的已运行任务（Structure 页候选）。
    pub fn structure_candidates(&self) -> Vec<&JobView> {
        self.jobs.iter().filter(|j| j.workdir.is_some()).collect()
    }

    /// 任务状态更新（事件增量）。
    pub fn upsert_status(&mut self, job_id: &str, status: &str) {
        if let Some(j) = self.jobs.iter_mut().find(|j| j.id == job_id) {
            j.status = status.to_string();
        }
        if let Some(d) = self.detail.as_mut()
            && d.id == job_id
        {
            d.status = status.to_string();
        }
    }

    /// 状态 → 颜色。
    pub fn status_color(status: &str) -> ratatui::style::Color {
        match status {
            "done" => ratatui::style::Color::Green,
            "failed" => ratatui::style::Color::LightRed,
            "running" => ratatui::style::Color::LightBlue,
            "parsing" => ratatui::style::Color::LightCyan,
            "cancelled" => ratatui::style::Color::DarkGray,
            "interrupted" => ratatui::style::Color::Yellow,
            "queued" => ratatui::style::Color::Gray,
            _ => ratatui::style::Color::White,
        }
    }
}
