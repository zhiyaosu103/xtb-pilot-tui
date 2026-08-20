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

        // 父任务进入运行态
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
            j.parse_degraded = children.values().any(|c| c.outcome == Some(false));
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
        let mut command = step.command.clone();
        for arg in &mut command {
            for (key, default) in [("nconf", "20"), ("sigma_ev", "0.4")] {
                let v = job
                    .params
                    .extra
                    .get(key)
                    .and_then(|v| v.as_str())
                    .unwrap_or(default);
                *arg = arg.replace(&format!("{{{key}}}"), v);
            }
        }
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
        let rendered = render_step(step, &ctx)?;

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

        // 执行单元
        let unit = ExecUnit {
            job_id: child.id,
            program: comp.exe.clone(),
            args: rendered.command,
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
        state.outcome = Some(ok);
        if !ok {
            warn!(job = %job.id, step = %step_key, ?error_code, "子步骤失败");
            return Ok(if state.step.on_failure == OnFailure::Skip {
                StepSettle::SoftFail
            } else {
                StepSettle::HardFail
            });
        }
        // 结果回收
        let step_dir = work_dir.join(step_key);
        for rule in &state.step.collect {
            let file_path = step_dir.join(&rule.file);
            let content = std::fs::read_to_string(&file_path).map_err(|_| {
                WorkflowError::MissingArtifact {
                    step: step_key.to_string(),
                    path: file_path.display().to_string(),
                }
            })?;
            let parsed = xtbp_parse::parse(&rule.parser, &content)?;
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
        Ok(StepSettle::Ok)
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
        let mut command = first.command.clone();
        for arg in &mut command {
            for (key, default) in [("nconf", "20"), ("sigma_ev", "0.4")] {
                let v = job
                    .params
                    .extra
                    .get(key)
                    .and_then(|v| v.as_str())
                    .unwrap_or(default);
                *arg = arg.replace(&format!("{{{key}}}"), v);
            }
        }
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
        let rendered = render_step(first, &ctx)?;
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
