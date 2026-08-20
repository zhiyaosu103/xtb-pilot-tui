//! 工作流引擎：把模板（DAG）展开为子任务链并驱动执行（设计文档 §3.4）。
//!
//! 执行模型：每个步骤 = 一个子 Job（parent 边），每个子 Job = 一个
//! ExecUnit（xtbp-sched）。引擎订阅事件总线，依赖满足才提交下游；
//! 子任务成功 → 按 collect 规则解析产物 → 结果写入父 Job（键前缀
//! `<step>.<key>`）→ 下游就绪继续。失败按 `on_failure` 策略处理：
//! `abort` 立即终止整链，`skip` 标记下游为跳过并继续其余分支。

use crate::registry::TemplateRegistry;
use crate::{Result, WorkflowError};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use tracing::{debug, info, warn};
use xtbp_api::EventBus;
use xtbp_assemble::{Gen3dOutput, RenderCtx, render_step};
use xtbp_core::hash::sha256_file;
use xtbp_core::job::{Job, JobEvent, JobParams, JobStatus, error_codes};
use xtbp_core::result::{Broadening, METHOD_TIER_SCREENING, ScalarResult, Spectrum, Transition};
use xtbp_core::time::{format_unix, now_unix};
use xtbp_core::workflow::{OnFailure, WorkflowStep, WorkflowTemplate};
use xtbp_core::{Molecule, Ulid};
use xtbp_sched::{ExecUnit, Scheduler};
use xtbp_store::{FileRepo, Store};

/// 组件解析结果（引擎经 resolver 取组件可执行路径与环境）。
#[derive(Debug, Clone)]
pub struct ResolvedComponent {
    /// 可执行文件绝对路径。
    pub exe: PathBuf,
    /// 版本号。
    pub version: String,
    /// 组件专属环境（如 xtb4stda 的 XTB4STDAHOME）。
    pub env: BTreeMap<String, String>,
}

/// 组件名 → 解析（由 daemon 依据 InstanceRegistry 提供）。
pub type ComponentResolver = Arc<dyn Fn(&str) -> Option<ResolvedComponent> + Send + Sync>;

/// 工作流引擎（共享句柄，daemon 单实例）。
#[derive(Clone)]
pub struct WorkflowEngine {
    store: Store,
    sched: Scheduler,
    bus: EventBus,
    registry: Arc<TemplateRegistry>,
    file_repo: FileRepo,
    resolver: ComponentResolver,
}

impl WorkflowEngine {
    /// 构造引擎。
    pub fn new(
        store: Store,
        sched: Scheduler,
        bus: EventBus,
        registry: TemplateRegistry,
        file_repo: FileRepo,
        resolver: ComponentResolver,
    ) -> Self {
        Self {
            store,
            sched,
            bus,
            registry: Arc::new(registry),
            file_repo,
            resolver,
        }
    }

    /// 运行一个工作流任务（阻塞至终态）。
    ///
    /// `gen3d` 由调用方提供（daemon 以短临界区持有 RDKit helper，
    /// 避免整个 DAG 执行期占用共享 helper）。
    pub async fn run<F, Fut>(&self, job: &Job, gen3d: F) -> Result<()>
    where
        F: FnOnce(Molecule) -> Fut + Send,
        Fut: Future<Output = std::result::Result<Gen3dOutput, xtbp_assemble::AssembleError>> + Send,
    {
        let tpl = self
            .registry
            .get(&job.workflow)
            .ok_or_else(|| WorkflowError::UnknownTemplate(job.workflow.clone()))?
            .clone();
        let mol = self
            .store
            .get_molecule(&job.molecule_id)
            .await?
            .ok_or_else(|| WorkflowError::State(format!("分子不存在: {}", job.molecule_id)))?;
        tpl.validate().map_err(WorkflowError::Template)?;

        // 1) gen3d + 组装输入目录（目录即真相）
        let gen_out = gen3d(mol.clone()).await?;
        let job_dir = self.file_repo.job_dir(&job.id);
        // 工作目录落库（目录即真相 + job.status 可见）
        if let Some(mut j) = self.store.get_job(&job.id).await.unwrap_or(None) {
            j.workdir = Some(job_dir.to_string_lossy().into_owned());
            let _ = self.store.update_job(&j).await;
        }
        let input_dir = job_dir.join("input");
        let work_dir = job_dir.join("work");
        let output_dir = job_dir.join("output");
        for d in [&input_dir, &work_dir, &output_dir] {
            std::fs::create_dir_all(d)?;
        }
        std::fs::write(input_dir.join("mol.xyz"), &gen_out.xyz)?;
        self.write_job_toml(&input_dir, job)?;
        self.write_cmd_txt(&input_dir, &tpl, job, &mol)?;
        if !gen_out.inchikey.is_empty() {
            self.store.set_inchikey(&mol.id, &gen_out.inchikey).await?;
        }

        // 2) 展开步骤 → 子任务（rdkit 步骤已由 helper 完成；solv-sp 按溶剂展开）
        let mut children: HashMap<String, ChildState> = HashMap::new();
        for step in tpl.steps.iter().filter(|s| s.component != "rdkit") {
            if step.id == "solv-sp" {
                let solvents = job
                    .params
                    .extra
                    .get("solvents")
                    .and_then(|v| v.as_array())
                    .map(|arr| {
                        arr.iter()
                            .filter_map(|v| v.as_str().map(String::from))
                            .collect::<Vec<_>>()
                    })
                    .unwrap_or_default();
                let solvents = if solvents.is_empty() {
                    vec![
                        job.params
                            .method
                            .solvent
                            .as_ref()
                            .map(|s| s.0.clone())
                            .unwrap_or_else(|| "water".into()),
                    ]
                } else {
                    solvents
                };
                for solv in solvents {
                    children.insert(
                        format!("solv-sp.{solv}"),
                        ChildState::new(step.clone(), format!("solv-sp.{solv}"), Some(solv)),
                    );
                }
            } else {
                children.insert(
                    step.id.clone(),
                    ChildState::new(step.clone(), step.id.clone(), None),
                );
            }
        }

        // 父任务进入运行态（Draft → Queued → Running）
        self.transition_job(job.id, JobStatus::Queued).await;
        self.transition_job(job.id, JobStatus::Running).await;

        // 3) 事件驱动 DAG 执行
        let mut rx = self.bus.subscribe();
        let total = children.len();
        let wall_deadline = tokio::time::Instant::now()
            + Duration::from_secs(job.params.wall_timeout_secs.max(600));

        self.submit_ready(&tpl, &mut children, job, &mol, &input_dir, &work_dir)
            .await?;

        loop {
            let settled = children.values().filter(|c| c.outcome.is_some()).count();
            if settled >= total {
                break;
            }
            // 父任务被取消？
            if let Some(j) = self.store.get_job(&job.id).await?
                && j.status == JobStatus::Cancelled
            {
                self.cancel_children(&children).await;
                return Ok(());
            }
            if tokio::time::Instant::now() > wall_deadline {
                self.cancel_children(&children).await;
                self.fail_job(job, error_codes::TIMEOUT, "工作流整体超时")
                    .await;
                return Err(WorkflowError::StepFailed {
                    step: job.workflow.clone(),
                    message: "工作流整体超时".into(),
                });
            }
            // 对账：事件总线可能丢事件（Lagged，批处理并发压力下实测复现）。
            // 以 store 终态为准收口子任务，避免 Finished 丢失后整链卡死
            // （子任务永远停在 Parsing、父任务永远 Running 直到整体超时）。
            if self
                .reconcile_children(&mut children, job, &work_dir, &output_dir)
                .await?
            {
                self.submit_ready(&tpl, &mut children, job, &mol, &input_dir, &work_dir)
                    .await?;
                continue;
            }
            let ev = tokio::select! {
                _ = tokio::time::sleep(Duration::from_millis(500)) => continue,
                ev = rx.recv() => match ev {
                    Ok(e) => e,
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                        return Err(WorkflowError::State("事件总线关闭".into()));
                    }
                },
            };
            let JobEvent::Finished {
                job_id,
                ok,
                error_code,
            } = ev
            else {
                continue;
            };
            let Ok(finished_id) = job_id.parse::<Ulid>() else {
                continue;
            };
            let Some(step_key) = children
                .iter()
                .find(|(_, st)| st.job_id == Some(finished_id))
                .map(|(id, _)| id.clone())
            else {
                continue;
            };

            match self
                .handle_finished(
                    &step_key,
                    &mut children,
                    ok,
                    &error_code,
                    job,
                    &work_dir,
                    &output_dir,
                )
                .await?
            {
                StepSettle::Ok => {
                    self.submit_ready(&tpl, &mut children, job, &mol, &input_dir, &work_dir)
                        .await?;
                }
                StepSettle::HardFail => {
                    self.cancel_children(&children).await;
                    self.fail_job(
                        job,
                        error_codes::XTB_CONVERGENCE_FAILED,
                        &format!("步骤 {step_key} 失败（abort 策略）"),
                    )
                    .await;
                    return Err(WorkflowError::StepFailed {
                        step: step_key,
                        message: error_code.clone().unwrap_or_else(|| "子进程失败".into()),
                    });
                }
                StepSettle::SoftFail => {
                    // skip：下游（含传递闭包）标记跳过，其余分支继续
                    self.skip_downstream(&tpl, &step_key, &mut children);
                    self.submit_ready(&tpl, &mut children, job, &mol, &input_dir, &work_dir)
                        .await?;
                }
            }
        }

        // 4) 终态与后处理
        self.post_process(job, &children, &output_dir).await?;
        let mut j = self
            .store
            .get_job(&job.id)
            .await?
            .ok_or_else(|| WorkflowError::State("任务记录缺失".into()))?;
        if j.transition(JobStatus::Parsing).is_ok() {
            j.parse_degraded = children
                .values()
                .any(|c| c.outcome == Some(false) || c.degraded);
            let _ = j.transition(JobStatus::Done);
            let _ = self.store.update_job(&j).await;
            info!(job = %job.id, workflow = %job.workflow, "工作流完成");
        }
        self.bus.publish(JobEvent::Finished {
            job_id: job.id.to_string(),
            ok: true,
            error_code: None,
        });
        Ok(())
    }

    // ------------------------------------------------------------------

    /// 提交所有依赖已满足的子步骤。
    #[allow(clippy::too_many_arguments)]
    async fn submit_ready(
        &self,
        tpl: &WorkflowTemplate,
        children: &mut HashMap<String, ChildState>,
        job: &Job,
        mol: &Molecule,
        input_dir: &Path,
        work_dir: &Path,
    ) -> Result<()> {
        let finished_ok: HashSet<String> = children
            .iter()
            .filter(|(_, c)| c.outcome == Some(true))
            .map(|(id, _)| id.clone())
            .collect();
        let keys: Vec<String> = children
            .iter()
            .filter(|(_, c)| !c.submitted && c.outcome.is_none())
            .map(|(k, _)| k.clone())
            .collect();
        for key in keys {
            let base = base_step_id(&key);
            let Some(step) = tpl.steps.iter().find(|s| s.id == base) else {
                continue;
            };
            let deps_done = step
                .depends_on
                .iter()
                .all(|d| d == "gen3d" || finished_ok.contains(d));
            if !deps_done {
                continue;
            }
            let state = children.get_mut(&key).unwrap();
            self.submit_child(state, job, mol, input_dir, work_dir)
                .await?;
        }
        Ok(())
    }

    /// 渲染并提交一个子步骤。
    async fn submit_child(
        &self,
        state: &mut ChildState,
        job: &Job,
        mol: &Molecule,
        input_dir: &Path,
        work_dir: &Path,
    ) -> Result<()> {
        let step = &state.step;
        // 组件解析
        let comp = self.resolver.as_ref()(step.component.as_str()).ok_or_else(|| {
            WorkflowError::StepFailed {
                step: step.id.clone(),
                message: format!("组件未登记: {}", step.component),
            }
        })?;

        // 输入文件解析与复制
        let step_dir = work_dir.join(&state.key);
        std::fs::create_dir_all(&step_dir)?;
        let input_xyz = self.prepare_inputs(step, input_dir, &step_dir)?;

        // 渲染命令（extra 占位符先行替换）
        let (charge, mult) = step_charge_mult(step, &job.params);
        let step_rendered = pre_render_extra(step, &job.params);
        let solvent_name = state
            .variant
            .clone()
            .or_else(|| job.params.method.solvent.as_ref().map(|s| s.0.clone()));
        let ctx = RenderCtx {
            smiles: &mol.smiles,
            charge,
            mult,
            threads: job.params.threads,
            method: &job.params.method,
            input_xyz: &input_xyz,
            solvent: solvent_name.as_deref(),
        };
        let rendered = render_step(&step_rendered, &ctx)?;

        // 子任务记录
        let mut child = Job::new(
            mol.id,
            state.key.clone(),
            job.params.clone(),
            format!("{}:{}", job.content_hash, state.key),
            Some(job.id),
            job.priority,
        );
        child.workdir = Some(step_dir.to_string_lossy().into_owned());
        self.store.insert_job(&child).await?;
        state.job_id = Some(child.id);

        // 执行单元：模板 command 含 argv[0]（程序名，供 cmd.txt 人读），
        // ExecUnit.program 已单独给出可执行路径——剥离 argv[0] 再传参
        let args = strip_argv0(&rendered.command, &step.component);
        let unit = ExecUnit {
            job_id: child.id,
            program: comp.exe.clone(),
            args,
            cwd: step_dir,
            env: {
                let mut env = comp.env.clone();
                for (k, v) in &step.env {
                    env.insert(k.clone(), v.clone());
                }
                env.insert("OMP_NUM_THREADS".into(), job.params.threads.to_string());
                env
            },
            wall_timeout: Some(Duration::from_secs(job.params.wall_timeout_secs)),
            stall_timeout: (job.params.stall_timeout_secs > 0)
                .then(|| Duration::from_secs(job.params.stall_timeout_secs)),
            priority: job.priority,
            max_retries: job.params.max_retries,
            memory_mb: 0,
        };
        self.sched.submit(unit).await?;
        state.submitted = true;
        debug!(job = %job.id, step = %state.key, child = %child.id, "子步骤已提交");
        Ok(())
    }

    /// 解析子步骤产物并写结果/产物登记；返回结算。
    #[allow(clippy::too_many_arguments)]
    async fn handle_finished(
        &self,
        step_key: &str,
        children: &mut HashMap<String, ChildState>,
        ok: bool,
        error_code: &Option<String>,
        job: &Job,
        work_dir: &Path,
        output_dir: &Path,
    ) -> Result<StepSettle> {
        let state = children.get_mut(step_key).unwrap();
        if state.outcome.is_some() {
            // 对账已收口该步骤：忽略迟到的重复 Finished 事件（幂等）
            return Ok(StepSettle::Ok);
        }
        state.outcome = Some(ok);
        if !ok {
            warn!(job = %job.id, step = %step_key, ?error_code, "子步骤失败");
            return Ok(if state.step.on_failure == OnFailure::Skip {
                StepSettle::SoftFail
            } else {
                StepSettle::HardFail
            });
        }
        // 结果回收（§4.2：解析失败 ≠ 任务失败——标记 ParseDegraded、保留原始文件）
        let step_dir = work_dir.join(step_key);
        for rule in &state.step.collect {
            let file_path = step_dir.join(&rule.file);
            let content = match std::fs::read_to_string(&file_path) {
                Ok(c) => c,
                Err(_) => {
                    warn!(
                        job = %job.id,
                        step = %step_key,
                        path = %file_path.display(),
                        "产物缺失（ParseDegraded）"
                    );
                    state.degraded = true;
                    continue;
                }
            };
            match xtbp_parse::parse(&rule.parser, &content) {
                Ok(parsed) => {
                    for scalar in &parsed.scalars {
                        let key = format!("{}.{}", state.result_prefix, scalar.key);
                        self.store
                            .put_result(
                                &job.id,
                                &ScalarResult {
                                    key,
                                    ..scalar.clone()
                                },
                            )
                            .await?;
                    }
                    if let Some(transitions) = &parsed.transitions {
                        state.transitions = Some(transitions.clone());
                    }
                }
                Err(e) => {
                    warn!(
                        job = %job.id,
                        step = %step_key,
                        "解析失败（ParseDegraded）: {e}"
                    );
                    state.degraded = true;
                }
            }
        }
        // 产物登记 + 复制到 output/
        for out in &state.step.outputs {
            let p = step_dir.join(out);
            if p.is_file() {
                let rel = self.file_repo.store_file(&p)?;
                let sha = sha256_file(&p)?;
                if let Some(child_id) = state.job_id {
                    self.store.put_artifact(&child_id, out, &rel, &sha).await?;
                }
                let dst = output_dir.join(format!("{step_key}_{out}"));
                std::fs::copy(&p, &dst)?;
            }
        }
        // 子任务终态：Parsing → Done
        if let Some(child_id) = state.job_id
            && let Some(mut child) = self.store.get_job(&child_id).await.unwrap_or(None)
            && child.transition(JobStatus::Done).is_ok()
        {
            let _ = self.store.update_job(&child).await;
        }
        Ok(StepSettle::Ok)
    }

    /// 对账收口：事件总线丢事件（Lagged）时，以 store 终态为准推进工作流。
    ///
    /// 扫描所有尚未收口且已提交的子任务：store 里已是终态（Done/Failed/
    /// Cancelled）但本引擎还没收到 Finished 事件 → 用 handle_finished 补齐。
    /// 返回是否有推进（调用方需重新 submit_ready 推下游步骤）。
    async fn reconcile_children(
        &self,
        children: &mut HashMap<String, ChildState>,
        job: &Job,
        work_dir: &Path,
        output_dir: &Path,
    ) -> Result<bool> {
        let pending: Vec<String> = children
            .iter()
            .filter(|(_, c)| c.outcome.is_none() && c.job_id.is_some())
            .map(|(k, _)| k.clone())
            .collect();
        let mut progressed = false;
        for key in pending {
            let Some(child_id) = children.get(&key).and_then(|c| c.job_id) else {
                continue;
            };
            let Some(child) = self.store.get_job(&child_id).await? else {
                continue;
            };
            // 收口条件：终态之外，Parsing 也算已跑完——sched 是 exit 0 后才
            // 落库 Parsing 再发 Finished 事件，所以「停在 Parsing」= Finished
            // 事件丢失（Lagged）。此时以 store 为准按成功收口并补做产物回收。
            let settled = matches!(
                child.status,
                JobStatus::Done
                    | JobStatus::Parsing
                    | JobStatus::Failed
                    | JobStatus::Cancelled
                    | JobStatus::Interrupted
            );
            if !settled {
                continue;
            }
            let ok = matches!(child.status, JobStatus::Done | JobStatus::Parsing);
            match self
                .handle_finished(
                    &key,
                    children,
                    ok,
                    &child.error_code,
                    job,
                    work_dir,
                    output_dir,
                )
                .await?
            {
                StepSettle::Ok | StepSettle::SoftFail => progressed = true,
                StepSettle::HardFail => {
                    self.cancel_children(children).await;
                    self.fail_job(
                        job,
                        error_codes::XTB_CONVERGENCE_FAILED,
                        &format!("步骤 {key} 失败（abort 策略）"),
                    )
                    .await;
                    return Err(WorkflowError::StepFailed {
                        step: job.workflow.clone(),
                        message: format!("步骤 {key} 失败（abort 策略）"),
                    });
                }
            }
        }
        Ok(progressed)
    }

    /// skip 策略：把失败步骤的传递下游标记为跳过（outcome=false 且不提交）。
    fn skip_downstream(
        &self,
        tpl: &WorkflowTemplate,
        failed_key: &str,
        children: &mut HashMap<String, ChildState>,
    ) {
        let base = base_step_id(failed_key);
        // 简单传递闭包：反复扫描直到无新增
        loop {
            let mut changed = false;
            let keys: Vec<String> = children
                .iter()
                .filter(|(_, c)| c.outcome.is_none() && !c.submitted)
                .map(|(k, _)| k.clone())
                .collect();
            for key in keys {
                let b = base_step_id(&key);
                let Some(step) = tpl.steps.iter().find(|s| s.id == b) else {
                    continue;
                };
                // 依赖中含失败步骤（或其下游已被跳过）→ 跳过
                let any_dep_skipped = step.depends_on.iter().any(|d| {
                    if d == "gen3d" {
                        return false;
                    }
                    d == base
                        || children
                            .get(d)
                            .map(|c| c.outcome == Some(false))
                            .unwrap_or(false)
                });
                if any_dep_skipped {
                    children.get_mut(&key).unwrap().outcome = Some(false);
                    changed = true;
                }
            }
            if !changed {
                break;
            }
        }
    }

    /// 后处理（按工作流类型机械计算）。
    async fn post_process(
        &self,
        job: &Job,
        children: &HashMap<String, ChildState>,
        output_dir: &Path,
    ) -> Result<()> {
        match job.workflow.as_str() {
            "excited" => {
                let transitions = children
                    .values()
                    .find_map(|c| c.transitions.clone())
                    .unwrap_or_default();
                if transitions.is_empty() {
                    warn!(job = %job.id, "未解析到跃迁表（ParseDegraded）");
                }
                let sigma_ev = job
                    .params
                    .extra
                    .get("sigma_ev")
                    .and_then(|v| v.as_f64())
                    .unwrap_or(0.4);
                let spectrum = Spectrum::broaden(&transitions, Broadening::Gaussian { sigma_ev });
                self.store
                    .put_spectrum(&job.id, "stda-gaussian", &spectrum)
                    .await?;
                write_spectrum_csv(output_dir, &spectrum)?;
                write_transitions_csv(output_dir, &transitions)?;
                if let Some(first) = transitions.first() {
                    self.store
                        .put_result(
                            &job.id,
                            &ScalarResult {
                                key: "first_excitation_energy".into(),
                                value: first.energy_ev,
                                unit: "eV".into(),
                                tier: METHOD_TIER_SCREENING.into(),
                            },
                        )
                        .await?;
                }
            }
            "reorg-4pt" => {
                let rows = self.store.results_for_job(&job.id).await?;
                let energy = |step: &str| {
                    rows.iter()
                        .find(|r| r.key == format!("{step}.total_energy"))
                        .map(|r| r.value)
                };
                if let (Some(e_n), Some(e_i), Some(e_in), Some(e_ni)) = (
                    energy("opt-neutral"),
                    energy("opt-ion"),
                    energy("sp-ion-at-neutral"),
                    energy("sp-neutral-at-ion"),
                ) {
                    let (lambda_h, lambda_e) = (e_in - e_i, e_ni - e_n);
                    for (key, value) in [("lambda_h", lambda_h), ("lambda_e", lambda_e)] {
                        self.store
                            .put_result(
                                &job.id,
                                &ScalarResult {
                                    key: key.into(),
                                    value,
                                    unit: "Eh".into(),
                                    tier: METHOD_TIER_SCREENING.into(),
                                },
                            )
                            .await?;
                    }
                } else {
                    warn!(job = %job.id, "四点能量不完整，跳过 λ 计算");
                }
            }
            "solv-series" => {
                // 各溶剂能量键已为 solv-sp.<solvent>.total_energy，无需额外处理
            }
            _ => {}
        }
        Ok(())
    }

    // ------------------------------------------------------------------
    // 工具

    /// 解析并复制步骤输入文件，返回 input_xyz 文件名。
    fn prepare_inputs(
        &self,
        step: &WorkflowStep,
        input_dir: &Path,
        step_dir: &Path,
    ) -> Result<String> {
        let mut input_xyz = "mol.xyz".to_string();
        for spec in &step.inputs {
            let (src, name) = match spec.split_once(':') {
                Some((producer, file)) => {
                    let producer_dir = step_dir
                        .parent()
                        .map(|p| p.join(producer))
                        .ok_or_else(|| WorkflowError::Template("无父目录".into()))?;
                    (producer_dir.join(file), file.to_string())
                }
                None => (input_dir.join(spec), spec.clone()),
            };
            if !src.is_file() {
                return Err(WorkflowError::MissingArtifact {
                    step: step.id.clone(),
                    path: src.display().to_string(),
                });
            }
            std::fs::copy(&src, step_dir.join(&name))?;
            if name.ends_with(".xyz") {
                input_xyz = name;
            }
        }
        Ok(input_xyz)
    }

    fn write_job_toml(&self, input_dir: &Path, job: &Job) -> Result<()> {
        let toml_str = toml::to_string_pretty(&job.params)
            .map_err(|e| WorkflowError::Template(format!("params 序列化失败: {e}")))?;
        std::fs::write(input_dir.join("job.toml"), toml_str)?;
        Ok(())
    }

    fn write_cmd_txt(
        &self,
        input_dir: &Path,
        tpl: &WorkflowTemplate,
        job: &Job,
        mol: &Molecule,
    ) -> Result<()> {
        // 记录第一个可执行步骤的渲染命令（目录即真相：可手动重跑）
        let first = tpl
            .steps
            .iter()
            .find(|s| s.component != "rdkit")
            .ok_or_else(|| WorkflowError::Template("模板无执行步骤".into()))?;
        let first_rendered = pre_render_extra(first, &job.params);
        let (charge, mult) = step_charge_mult(first, &job.params);
        let solvent = job.params.method.solvent.as_ref().map(|s| s.0.clone());
        let ctx = RenderCtx {
            smiles: &mol.smiles,
            charge,
            mult,
            threads: job.params.threads,
            method: &job.params.method,
            input_xyz: "mol.xyz",
            solvent: solvent.as_deref(),
        };
        let rendered = render_step(&first_rendered, &ctx)?;
        let lines = [
            format!("# xTB-Pilot cmd 快照 · 生成于 {}", format_unix(now_unix())),
            format!("# 工作流: {} · 任务: {}", tpl.id, job.id),
            format!(
                "# 环境: OMP_NUM_THREADS={} ulimit -s unlimited",
                job.params.threads
            ),
            quote_argv(&rendered.command),
        ];
        std::fs::write(input_dir.join("cmd.txt"), lines.join("\n") + "\n")?;
        Ok(())
    }

    async fn transition_job(&self, job_id: Ulid, status: JobStatus) {
        if let Some(mut j) = self.store.get_job(&job_id).await.unwrap_or(None)
            && j.status != status
            && j.transition(status).is_ok()
        {
            let _ = self.store.update_job(&j).await;
            self.bus.publish(JobEvent::Status {
                job_id: job_id.to_string(),
                status: status.as_str().into(),
            });
        }
    }

    async fn cancel_children(&self, children: &HashMap<String, ChildState>) {
        for c in children.values() {
            if let Some(id) = c.job_id {
                let _ = self.sched.cancel(&id).await;
            }
        }
    }

    async fn fail_job(&self, job: &Job, code: &str, message: &str) {
        if let Some(mut j) = self.store.get_job(&job.id).await.unwrap_or(None) {
            j.fail(code, message);
            let _ = j.transition(JobStatus::Failed);
            let _ = self.store.update_job(&j).await;
            self.bus.publish(JobEvent::Finished {
                job_id: job.id.to_string(),
                ok: false,
                error_code: Some(code.into()),
            });
        }
    }
}

/// 子步骤执行状态。
struct ChildState {
    step: WorkflowStep,
    /// 状态键（= 步骤 id；solv-series 为 `solv-sp.<溶剂>`）。
    key: String,
    /// 溶剂变体（solv-series）。
    variant: Option<String>,
    /// 子任务 id（提交后回填）。
    job_id: Option<Ulid>,
    /// 是否已提交。
    submitted: bool,
    /// 终局结果（None = 未完成）。
    outcome: Option<bool>,
    /// 解析出的跃迁表（excited 用）。
    transitions: Option<Vec<Transition>>,
    /// 解析降级标记（产物缺失/解析失败，§4.2）。
    degraded: bool,
    /// 结果键前缀（如 `solv-sp.toluene`）。
    result_prefix: String,
}

impl ChildState {
    fn new(step: WorkflowStep, key: String, variant: Option<String>) -> Self {
        let result_prefix = key.clone();
        Self {
            step,
            key,
            variant,
            job_id: None,
            submitted: false,
            outcome: None,
            transitions: None,
            degraded: false,
            result_prefix,
        }
    }
}

/// 子步骤结算。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StepSettle {
    /// 成功。
    Ok,
    /// 失败但 Skip（下游不再等待）。
    SoftFail,
    /// 失败且 Abort（整链终止）。
    HardFail,
}

/// 状态键 → 模板步骤 id（去掉 `.变体` 后缀）。
fn base_step_id(key: &str) -> &str {
    key.split_once('.').map(|(b, _)| b).unwrap_or(key)
}

/// 步骤的电荷/多重度覆盖（redox/reorg-4pt 分支在模板里硬编码 flag，
/// 此处处理模板使用 {charge}/{mult} 占位符的步骤）。
fn step_charge_mult(step: &WorkflowStep, params: &JobParams) -> (i8, u8) {
    match step.id.as_str() {
        "opt-cation" => (1, 2),
        "opt-anion" => (-1, 2),
        "opt-ion" | "sp-ion-at-neutral" => (1, 2),
        _ => (params.charge, params.multiplicity),
    }
}

/// extra 占位符预替换（{nconf}/{sigma_ev}）→ 返回替换后的步骤副本。
fn pre_render_extra(step: &WorkflowStep, params: &JobParams) -> WorkflowStep {
    let mut s = step.clone();
    for arg in &mut s.command {
        for (key, default) in [("nconf", "20"), ("sigma_ev", "0.4")] {
            let v = params
                .extra
                .get(key)
                .and_then(|v| v.as_str())
                .unwrap_or(default);
            *arg = arg.replace(&format!("{{{key}}}"), v);
        }
    }
    s
}

/// 剥离模板命令中的 argv[0]（首个元素为组件名时）。
fn strip_argv0(command: &[String], component: &str) -> Vec<String> {
    match command.first() {
        Some(first) if first == component => command[1..].to_vec(),
        _ => command.to_vec(),
    }
}

/// argv 转一行命令（含空格转义）。
fn quote_argv(args: &[String]) -> String {
    args.iter()
        .map(|a| {
            if a.contains(char::is_whitespace) {
                format!("\"{a}\"")
            } else {
                a.clone()
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// 写 (nm, intensity) 展宽谱 CSV。
fn write_spectrum_csv(output_dir: &Path, spectrum: &Spectrum) -> Result<()> {
    let mut wtr = csv::Writer::from_path(output_dir.join("spectrum.csv"))
        .map_err(|e| WorkflowError::Io(e.into()))?;
    wtr.write_record(["wavelength_nm", "intensity"])
        .map_err(|e| WorkflowError::Io(e.into()))?;
    for (nm, i) in &spectrum.points {
        wtr.write_record([format!("{nm:.3}"), format!("{i:.6}")])
            .map_err(|e| WorkflowError::Io(e.into()))?;
    }
    wtr.flush().map_err(WorkflowError::Io)?;
    Ok(())
}

/// 写跃迁表 CSV。
fn write_transitions_csv(output_dir: &Path, transitions: &[Transition]) -> Result<()> {
    let mut wtr = csv::Writer::from_path(output_dir.join("transitions.csv"))
        .map_err(|e| WorkflowError::Io(e.into()))?;
    wtr.write_record(["state", "energy_ev", "wavelength_nm", "f", "assignment"])
        .map_err(|e| WorkflowError::Io(e.into()))?;
    for t in transitions {
        wtr.write_record([
            t.state.to_string(),
            format!("{:.6}", t.energy_ev),
            format!("{:.2}", t.wavelength_nm),
            format!("{:.6}", t.oscillator_strength),
            t.assignment.clone().unwrap_or_default(),
        ])
        .map_err(|e| WorkflowError::Io(e.into()))?;
    }
    wtr.flush().map_err(WorkflowError::Io)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;
    use tokio_util::sync::CancellationToken;
    use xtbp_core::molecule::{Charge, Multiplicity};
    use xtbp_sched::SchedConfig;

    fn conformer_step() -> WorkflowStep {
        WorkflowTemplate::parse(
            r#"
id = "conformer"
description = "x"
[[steps]]
id = "crest"
component = "crest"
command = ["crest", "{input_xyz}", "--nconf", "{nconf}"]
"#,
        )
        .unwrap()
        .steps
        .pop()
        .unwrap()
    }

    #[test]
    fn pre_render_replaces_nconf_from_extra() {
        let step = conformer_step();
        let params = JobParams {
            extra: BTreeMap::from([
                ("nconf".into(), serde_json::json!("42")),
                ("batch_tag".into(), serde_json::json!(7)),
            ]),
            ..JobParams::default()
        };
        let rendered = pre_render_extra(&step, &params);
        assert!(rendered.command.contains(&"42".to_string()));
        assert!(!rendered.command.iter().any(|a| a.contains("{nconf}")));
    }

    #[test]
    fn pre_render_defaults_nconf_to_20() {
        let step = conformer_step();
        let rendered = pre_render_extra(&step, &JobParams::default());
        assert!(rendered.command.contains(&"20".to_string()));
    }

    // ------------------------------------------------------------------
    // 对账（reconcile_children）：事件总线丢 Finished 时以 store 收口
    // ------------------------------------------------------------------

    /// 组装引擎 + store + 已终态子任务（无任何事件送达）的最小环境。
    async fn reconcile_fixture(
        child_status: JobStatus,
    ) -> (
        WorkflowEngine,
        Job,
        Job,
        HashMap<String, ChildState>,
        tempfile::TempDir,
    ) {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(&dir.path().join("t.db")).await.unwrap();
        let bus = EventBus::new(16);
        let shutdown = CancellationToken::new();
        let sched = Scheduler::new(SchedConfig::default(), store.clone(), bus.clone(), shutdown);
        let templates = TemplateRegistry::new();
        let file_repo = FileRepo::new(dir.path().to_path_buf());
        let resolver: ComponentResolver = std::sync::Arc::new(|_| None);
        let engine = WorkflowEngine::new(store.clone(), sched, bus, templates, file_repo, resolver);

        let mol = Molecule::new("C1=CC=CC=C1", Charge(0), Multiplicity(1), now_unix());
        let mol = store.ensure_molecule(&mol).await.unwrap();
        let mut parent = Job::new(mol.id, "opt", JobParams::default(), "h".into(), None, 0);
        store.insert_job(&parent).await.unwrap();
        // 父任务进入运行态（真实流程中步骤执行时父任务已是 Running）
        for st in [JobStatus::Queued, JobStatus::Running] {
            parent.transition(st).unwrap();
            store.update_job(&parent).await.unwrap();
        }

        // 子任务直接造到终态（模拟：sched 已跑完并落库，但 Finished 事件丢了）
        let mut child = Job::new(
            mol.id,
            "sp",
            JobParams::default(),
            "c".into(),
            Some(parent.id),
            0,
        );
        store.insert_job(&child).await.unwrap();
        for st in [JobStatus::Queued, JobStatus::Running] {
            child.transition(st).unwrap();
            store.update_job(&child).await.unwrap();
        }
        if child_status != JobStatus::Running {
            child.transition(JobStatus::Parsing).unwrap();
            store.update_job(&child).await.unwrap();
        }
        match child_status {
            JobStatus::Done => {
                child.transition(JobStatus::Done).unwrap();
                store.update_job(&child).await.unwrap();
            }
            JobStatus::Failed => {
                child.fail(error_codes::XTB_CONVERGENCE_FAILED, "测试失败");
                child.transition(JobStatus::Failed).unwrap();
                store.update_job(&child).await.unwrap();
            }
            JobStatus::Parsing => {} // 卡在 Parsing：Finished 事件丢失的典型状态
            JobStatus::Running => {} // 非终态：对账应跳过
            other => panic!("fixture 只支持 Done/Failed/Parsing/Running: {other:?}"),
        }

        let step = WorkflowTemplate::parse(
            r#"
id = "sp"
description = "x"
[[steps]]
id = "sp"
component = "xtb"
command = ["xtb", "{input_xyz}", "--sp"]
"#,
        )
        .unwrap()
        .steps
        .pop()
        .unwrap();
        let mut children = HashMap::new();
        let mut cs = ChildState::new(step, "sp".into(), None);
        cs.job_id = Some(child.id);
        cs.submitted = true;
        children.insert("sp".into(), cs);

        let work_dir = dir.path().join("work");
        let output_dir = dir.path().join("output");
        std::fs::create_dir_all(&work_dir).unwrap();
        std::fs::create_dir_all(&output_dir).unwrap();
        // 返回 dir 守卫：TempDir 必须活到测试结束，否则 SQLite 文件被删、
        // 连接池新建连接时偶发失败（全量并行跑时暴露）
        (engine, parent, child, children, dir)
    }

    #[tokio::test]
    async fn reconcile_settles_child_when_finished_event_lost() {
        let (engine, parent, _child, mut children, _dir) = reconcile_fixture(JobStatus::Done).await;
        let work_dir = engine.file_repo.job_dir(&parent.id).join("work");
        let output_dir = engine.file_repo.job_dir(&parent.id).join("output");
        let progressed = engine
            .reconcile_children(&mut children, &parent, &work_dir, &output_dir)
            .await
            .unwrap();
        assert!(progressed, "Done 子任务应被对账收口");
        assert_eq!(children["sp"].outcome, Some(true));
    }

    #[tokio::test]
    async fn reconcile_hardfails_when_child_failed() {
        let (engine, parent, _child, mut children, _dir) =
            reconcile_fixture(JobStatus::Failed).await;
        let work_dir = engine.file_repo.job_dir(&parent.id).join("work");
        let output_dir = engine.file_repo.job_dir(&parent.id).join("output");
        let res = engine
            .reconcile_children(&mut children, &parent, &work_dir, &output_dir)
            .await;
        assert!(res.is_err(), "Failed 子任务应对账触发 abort 失败");
        assert_eq!(children["sp"].outcome, Some(false));
        let loaded = engine.store.get_job(&parent.id).await.unwrap().unwrap();
        assert_eq!(loaded.status, JobStatus::Failed);
    }

    #[tokio::test]
    async fn reconcile_settles_child_stuck_in_parsing() {
        // Finished 事件丢失的典型状态：子任务 exit 0 后停在 Parsing
        let (engine, parent, _child, mut children, _dir) =
            reconcile_fixture(JobStatus::Parsing).await;
        let work_dir = engine.file_repo.job_dir(&parent.id).join("work");
        let output_dir = engine.file_repo.job_dir(&parent.id).join("output");
        let progressed = engine
            .reconcile_children(&mut children, &parent, &work_dir, &output_dir)
            .await
            .unwrap();
        assert!(progressed, "卡在 Parsing 的子任务应收口为成功");
        assert_eq!(children["sp"].outcome, Some(true));
        let loaded = engine
            .store
            .get_job(&children["sp"].job_id.unwrap())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(loaded.status, JobStatus::Done, "子任务应被补到 Done");
    }

    #[tokio::test]
    async fn reconcile_skips_non_terminal_children() {
        let (engine, parent, _child, mut children, _dir) =
            reconcile_fixture(JobStatus::Running).await;
        let work_dir = engine.file_repo.job_dir(&parent.id).join("work");
        let output_dir = engine.file_repo.job_dir(&parent.id).join("output");
        let progressed = engine
            .reconcile_children(&mut children, &parent, &work_dir, &output_dir)
            .await
            .unwrap();
        assert!(!progressed, "非终态子任务不应推进");
        assert_eq!(children["sp"].outcome, None);
    }
}

#[cfg(test)]
mod strip_tests {
    use super::strip_argv0;

    #[test]
    fn strips_component_argv0() {
        let cmd = vec!["xtb4stda".to_string(), "xtbopt.xyz".to_string()];
        assert_eq!(strip_argv0(&cmd, "xtb4stda"), vec!["xtbopt.xyz"]);
    }

    #[test]
    fn keeps_when_first_is_not_component() {
        let cmd = vec!["xtbopt.xyz".to_string()];
        assert_eq!(strip_argv0(&cmd, "xtb4stda"), cmd);
    }
}
