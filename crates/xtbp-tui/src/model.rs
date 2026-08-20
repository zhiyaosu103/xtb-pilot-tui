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
    /// 父任务 id（Some = 工作流内部子步骤，不在 TUI 任务列表展示）。
    pub parent_id: Option<String>,
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
            parent_id: v
                .get("parent_id")
                .and_then(|x| x.as_str())
                .map(String::from),
        })
    }

    /// 是否为工作流内部子步骤（列表展示与取消一律以父任务为单位）。
    pub fn is_child(&self) -> bool {
        self.parent_id.as_deref().is_some_and(|p| !p.is_empty())
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
    /// 批量导入 .smi 文件路径（i）。
    SmiPath,
    /// 编辑溶剂名（e）。
    Solvent,
    /// 编辑数值参数（t/a/m/c/n）。
    Number(NumberField),
}

/// 可编辑的数值提交参数。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NumberField {
    /// 电子温度 K（--etemp）。
    Etemp,
    /// SCF 精度（--acc）。
    Accuracy,
    /// SCF 最大迭代（--maxiter）。
    Maxiter,
    /// 电荷。
    Charge,
    /// 多重度。
    Multiplicity,
}

/// 计算水平（对应 MethodFamily 的 serde kebab-case 名）。
pub const FAMILY_NAMES: [&str; 4] = ["gfn2-xtb", "gfn1-xtb", "gfn0-xtb", "gfn-ff"];
/// 计算水平显示名。
pub const FAMILY_LABELS: [&str; 4] = ["GFN2-xTB", "GFN1-xTB", "GFN0-xTB", "GFN-FF"];
/// 隐式溶剂模型（对应 Solvation 的 serde kebab-case 名）。
pub const SOLVATION_NAMES: [&str; 3] = ["none", "alpb", "gbsa"];
/// 溶剂模型显示名。
pub const SOLVATION_LABELS: [&str; 3] = ["气相", "ALPB", "GBSA"];

/// 工作流提交表单。
#[derive(Debug, Clone)]
pub struct SubmitForm {
    pub workflow: usize,
    pub smiles: String,
    /// 计算水平索引（FAMILY_NAMES）。
    pub family: usize,
    /// 溶剂模型索引（SOLVATION_NAMES；0 = 气相）。
    pub solvation: usize,
    /// 溶剂名（solvation > 0 时生效）。
    pub solvent: String,
    /// 电子温度 K（None = xtb 默认）。
    pub etemp: Option<f64>,
    /// SCF 精度（None = xtb 默认 1.0）。
    pub accuracy: Option<f64>,
    /// SCF 最大迭代（None = xtb 默认）。
    pub maxiter: Option<u32>,
    /// 电荷。
    pub charge: i8,
    /// 多重度。
    pub multiplicity: u8,
}

impl SubmitForm {
    /// 提交参数 JSON（method/charge/multiplicity 快照，与 JobParams 对齐）。
    pub fn to_params_json(&self) -> serde_json::Value {
        let solvent = if self.solvation > 0 && !self.solvent.trim().is_empty() {
            serde_json::Value::String(self.solvent.trim().to_string())
        } else {
            serde_json::Value::Null
        };
        serde_json::json!({
            "method": {
                "family": FAMILY_NAMES[self.family.min(FAMILY_NAMES.len() - 1)],
                "solvation": SOLVATION_NAMES[self.solvation.min(SOLVATION_NAMES.len() - 1)],
                "solvent": solvent,
                "etemp": self.etemp,
                "accuracy": self.accuracy,
                "maxiter": self.maxiter,
            },
            "charge": self.charge,
            "multiplicity": self.multiplicity,
            "threads": 1,
        })
    }
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
    /// Settings 页当前选中的设置项索引（←/→ 切换其值，j/k 移动）。
    pub settings_sel: usize,

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
    /// 「退出时同时关闭 daemon」开关（Settings 页选中后 ←/→ 切换；
    /// 默认关 = 只退 TUI、daemon 独立存活并在任务全部完成后空闲自毁）。
    pub exit_shuts_daemon: bool,
}

/// Settings 页可切换设置项（j/k 移动选中，←/→ 切换值）。
/// 工程规范：所有「开关类」选项统一用 ←/→ 切换（见 docs/engineering-spec.md）。
pub const SETTINGS_ITEMS: [&str; 1] = ["退出时关闭 daemon"];

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
            settings_sel: 0,
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
                family: 0,
                solvation: 0,
                solvent: "water".into(),
                etemp: None,
                accuracy: None,
                maxiter: None,
                charge: 0,
                multiplicity: 1,
            },
            quit: false,
            exit_shuts_daemon: false,
        }
    }
}

impl AppModel {
    /// 当前页选中任务 id（经过滤列表索引——列表渲染/移动以过滤后为准，
    /// 否则过滤状态下详情/取消会指向未过滤列表里的错误任务）。
    pub fn selected_job_id(&self) -> Option<&str> {
        self.filtered_jobs()
            .get(self.jobs_sel)
            .map(|j| j.id.as_str())
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

#[cfg(test)]
mod tests {
    use super::*;

    fn job(id: &str, workflow: &str, status: &str) -> JobView {
        let v: serde_json::Value = serde_json::json!({
            "id": id, "workflow": workflow, "status": status,
            "molecule_id": "M", "created_at": 0,
        });
        JobView::from_json(&v).unwrap()
    }

    #[test]
    fn selected_job_respects_active_filter() {
        let m = AppModel {
            jobs: vec![job("AAA", "opt", "done"), job("BBB", "sp", "running")],
            filter: "running".into(),
            jobs_sel: 0,
            ..Default::default()
        };
        assert_eq!(m.selected_job_id(), Some("BBB"));
    }

    #[test]
    fn selected_job_without_filter_maps_directly() {
        let m = AppModel {
            jobs: vec![job("AAA", "opt", "done"), job("BBB", "sp", "running")],
            jobs_sel: 1,
            ..Default::default()
        };
        assert_eq!(m.selected_job_id(), Some("BBB"));
    }

    #[test]
    fn children_are_flagged_for_list_filtering() {
        let v: serde_json::Value = serde_json::json!({
            "id": "CH", "workflow": "opt", "status": "running",
            "molecule_id": "M", "created_at": 0, "parent_id": "P",
        });
        let j = JobView::from_json(&v).unwrap();
        assert!(j.is_child());
        assert!(!job("X", "opt", "done").is_child());
    }
}
