//! daemon 应用层：全部 JSON-RPC 方法的实现（设计文档 §3.7 方法域）。
//!
//! 与订阅无关的方法全走这里；`job.events` / `queue.events` 由 xtbp-api
//! 服务器层处理。本层职责：分子管理、任务提交（幂等）、tail、取消、
//! 结果查询、导出、实例清单、健康检查。

use crate::tail::TailBuffer;
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use tracing::warn;
use xtbp_api::EventBus;
use xtbp_api::protocol::{
    ApiError, ApiHandler, JobIdParams, JobListParams, JobSubmitParams, JobTailParams,
    MolCreateParams, MolGetParams, MolListParams, ResExportParams, ResScalarParams,
    ResSpectrumParams, methods,
};
use xtbp_assemble::HelperClient;
use xtbp_core::Ulid;
use xtbp_core::hash::content_hash_json;
use xtbp_core::job::{Job, JobStatus, error_codes};
use xtbp_core::molecule::{Charge, Molecule, Multiplicity};
use xtbp_core::registry::InstanceRegistry;
use xtbp_core::time::{format_unix, now_unix};
use xtbp_sched::Scheduler;
use xtbp_store::{JobFilter, Store};
use xtbp_workflow::{ResolvedComponent, WorkflowEngine};

/// 应用状态（handler 与后台任务共享）。
#[derive(Clone)]
pub struct AppState {
    pub store: Store,
    pub bus: EventBus,
    pub sched: Scheduler,
    pub engine: WorkflowEngine,
    pub registry: Arc<Mutex<InstanceRegistry>>,
    pub helper: Arc<tokio::sync::Mutex<Option<HelperClient>>>,
    pub tail: Arc<TailBuffer>,
    pub started_at: i64,
    pub data_dir: PathBuf,
    pub templates_dir: PathBuf,
}

/// 便捷构造：把组件登记表转成引擎 resolver。
pub fn registry_resolver(
    registry: Arc<Mutex<InstanceRegistry>>,
) -> xtbp_workflow::ComponentResolver {
    let envs: BTreeMap<&str, BTreeMap<String, String>> = {
        let mut m = BTreeMap::new();
        // xtb4stda 需要 XTB4STDAHOME（参数文件目录，见 README）
        if let Ok(home) = std::env::var("XTB4STDAHOME") {
            m.insert(
                "xtb4stda",
                BTreeMap::from([("XTB4STDAHOME".to_string(), home)]),
            );
        }
        m
    };
    std::sync::Arc::new(move |name: &str| {
        let reg = registry.lock().unwrap();
        let entry = reg.latest(name)?;
        let mut env = BTreeMap::new();
        if let Some(e) = envs.get(name) {
            env = e.clone();
        }
        Some(ResolvedComponent {
            exe: entry.exe.clone(),
            version: entry.version.to_string(),
            env,
        })
    })
}

/// 经共享 helper 做一次 gen3d（必要时拉起；HelperGone 自动重启一次）。
pub async fn gen3d_via_helper(
    helper: &tokio::sync::Mutex<Option<HelperClient>>,
    smiles: &str,
    charge: i8,
    mult: u8,
) -> Result<xtbp_assemble::Gen3dOutput, ApiError> {
    let mut guard = helper.lock().await;
    if guard.is_none() {
        match HelperClient::spawn().await {
            Ok(h) => *guard = Some(h),
            Err(e) => {
                return Err(ApiError::app(
                    error_codes::COMPONENT_UNAVAILABLE,
                    format!("RDKit helper 启动失败: {e}"),
                ));
            }
        }
    }
    let client = guard.as_mut().unwrap();
    match client.gen3d(smiles, charge, mult).await {
        Ok(v) => Ok(v),
        Err(xtbp_assemble::AssembleError::HelperGone) => {
            // 崩溃自愈：重启一次
            warn!("RDKit helper 崩溃，重启中");
            if let Ok(h) = HelperClient::spawn().await {
                *guard = Some(h);
                let client = guard.as_mut().unwrap();
                client
                    .gen3d(smiles, charge, mult)
                    .await
                    .map_err(|e| ApiError::app(error_codes::COMPONENT_UNAVAILABLE, e.to_string()))
            } else {
                Err(ApiError::app(
                    error_codes::COMPONENT_UNAVAILABLE,
                    "RDKit helper 重启失败",
                ))
            }
        }
        Err(e) => Err(api_error_from_assemble(&e)),
    }
}

fn api_error_from_assemble(e: &xtbp_assemble::AssembleError) -> ApiError {
    match e {
        xtbp_assemble::AssembleError::InvalidSmiles { smiles: _ } => ApiError::app(
            error_codes::RDKIT_INVALID_SMILES,
            "无效 SMILES（RDKit 拒绝）",
        ),
        other => ApiError::app(error_codes::COMPONENT_UNAVAILABLE, other.to_string()),
    }
}

impl ApiHandler for AppState {
    async fn handle(&self, method: &str, params: Value) -> std::result::Result<Value, ApiError> {
        match method {
            methods::MOL_CREATE => self.mol_create(&params).await,
            methods::MOL_LIST => self.mol_list(&params).await,
            methods::MOL_GET => self.mol_get(&params).await,
            methods::JOB_SUBMIT | methods::WF_RUN => self.job_submit(&params).await,
            methods::JOB_STATUS | methods::WF_STATUS => self.job_status(&params).await,
            methods::JOB_TAIL => self.job_tail(&params).await,
            methods::JOB_CANCEL => self.job_cancel(&params).await,
            methods::JOB_LIST => self.job_list(&params).await,
            methods::RES_SCALAR => self.res_scalar(&params).await,
            methods::RES_SPECTRUM => self.res_spectrum(&params).await,
            methods::RES_EXPORT => self.res_export(&params).await,
            methods::INST_LIST => self.inst_list().await,
            methods::SYS_HEALTH => self.sys_health().await,
            other => Err(ApiError::method_not_found(other)),
        }
    }
}

impl AppState {
    fn parse<T: serde::de::DeserializeOwned>(params: &Value) -> std::result::Result<T, ApiError> {
        serde_json::from_value(params.clone())
            .map_err(|e| ApiError::invalid_params(format!("参数解析失败: {e}")))
    }

    fn parse_ulid(s: &str, what: &str) -> std::result::Result<Ulid, ApiError> {
        s.parse()
            .map_err(|_| ApiError::invalid_params(format!("{what} 无效: {s}")))
    }

    // ------------------------------------------------------------------
    // 分子
    // ------------------------------------------------------------------

    async fn mol_create(&self, params: &Value) -> std::result::Result<Value, ApiError> {
        let p: MolCreateParams = Self::parse(params)?;
        let mult = Multiplicity::new(p.multiplicity).map_err(ApiError::invalid_params)?;
        let mol = Molecule::new(&p.smiles, Charge(p.charge), mult, now_unix());
        // 组装阶段校验 SMILES 有效性：dry-run 一次 gen3d（失败 → RDKIT_INVALID_SMILES）
        gen3d_via_helper(&self.helper, &mol.smiles, mol.charge.0, mol.multiplicity.0).await?;
        let kept = self.store.ensure_molecule(&mol).await.map_err(store_err)?;
        Ok(molecule_json(&kept))
    }

    async fn mol_list(&self, params: &Value) -> std::result::Result<Value, ApiError> {
        let p: MolListParams = Self::parse(params)?;
        let mols = self
            .store
            .list_molecules(p.limit.clamp(1, 1000))
            .await
            .map_err(store_err)?;
        Ok(Value::Array(mols.iter().map(molecule_json).collect()))
    }

    async fn mol_get(&self, params: &Value) -> std::result::Result<Value, ApiError> {
        let p: MolGetParams = Self::parse(params)?;
        let id = Self::parse_ulid(&p.molecule_id, "molecule_id")?;
        let mol = self
            .store
            .get_molecule(&id)
            .await
            .map_err(store_err)?
            .ok_or_else(|| ApiError::app(error_codes::JOB_NOT_FOUND, "分子不存在"))?;
        let jobs = self
            .store
            .list_jobs(&JobFilter {
                molecule_id: Some(id),
                ..JobFilter::all()
            })
            .await
            .map_err(store_err)?;
        let mut v = molecule_json(&mol);
        v["jobs"] = Value::Array(jobs.iter().map(job_summary).collect());
        Ok(v)
    }

    // ------------------------------------------------------------------
    // 任务
    // ------------------------------------------------------------------

    async fn job_submit(&self, params: &Value) -> std::result::Result<Value, ApiError> {
        let p: JobSubmitParams = Self::parse(params)?;
        // 分子：已有 id 或新建（新建需先过 RDKit 校验）
        let molecule_id = match (&p.molecule_id, &p.smiles) {
            (Some(id), _) => Self::parse_ulid(id, "molecule_id")?,
            (None, Some(smiles)) => {
                let mult = Multiplicity::new(p.multiplicity).map_err(ApiError::invalid_params)?;
                let mut mol = Molecule::new(smiles, Charge(p.charge), mult, now_unix());
                let gen_out =
                    gen3d_via_helper(&self.helper, &mol.smiles, mol.charge.0, mol.multiplicity.0)
                        .await?;
                mol.inchikey = gen_out.inchikey;
                let kept = self.store.ensure_molecule(&mol).await.map_err(store_err)?;
                kept.id
            }
            (None, None) => {
                return Err(ApiError::invalid_params("molecule_id 与 smiles 必须二选一"));
            }
        };
        let mol = self
            .store
            .get_molecule(&molecule_id)
            .await
            .map_err(store_err)?
            .ok_or_else(|| ApiError::app(error_codes::JOB_NOT_FOUND, "分子不存在"))?;

        // 参数合并：请求 params 覆盖模板默认（模板级默认 nconf 等由 extra 承载）
        let job_params = p.params;

        // 内容哈希（幂等：重试不产生重复计算）
        let payload = json!({
            "inchikey_or_smiles": if mol.inchikey.is_empty() { &mol.smiles } else { &mol.inchikey },
            "workflow": &p.workflow,
            "params": &job_params,
        });
        let content_hash = content_hash_json(&payload);

        if let Some(hit) = self
            .store
            .find_done_by_content_hash(&content_hash)
            .await
            .map_err(store_err)?
        {
            return Ok(json!({
                "job_id": hit.id.to_string(),
                "reused": true,
                "status": hit.status.as_str(),
                "content_hash": content_hash,
            }));
        }

        let job = Job::new(
            mol.id,
            p.workflow.clone(),
            job_params.clone(),
            content_hash.clone(),
            None,
            p.priority,
        );

        // dry-run：只返回组装预览
        if p.dry_run {
            return Ok(json!({
                "dry_run": true,
                "workflow": p.workflow,
                "molecule": mol.smiles,
                "charge": mol.charge.0,
                "multiplicity": mol.multiplicity.0,
                "params": job_params,
                "content_hash": content_hash,
            }));
        }

        self.store.insert_job(&job).await.map_err(store_err)?;
        self.bus.publish(xtbp_core::job::JobEvent::Queued {
            job_id: job.id.to_string(),
        });

        // 引擎任务（后台驱动 DAG；helper 仅在 gen3d 短临界区占用）
        let this = self.clone();
        let job_for_task = job.clone();
        tokio::spawn(async move {
            let gen3d = {
                let helper = this.helper.clone();
                move |molecule: xtbp_core::Molecule| {
                    let helper = helper.clone();
                    async move {
                        gen3d_via_helper(
                            &helper,
                            &molecule.smiles,
                            molecule.charge.0,
                            molecule.multiplicity.0,
                        )
                        .await
                        .map_err(|e| xtbp_assemble::AssembleError::Rdkit {
                            message: e.to_string(),
                        })
                    }
                }
            };
            let result = this.engine.run(&job_for_task, gen3d).await;
            if let Err(e) = result {
                warn!(job = %job_for_task.id, "工作流驱动失败: {e}");
            }
        });

        Ok(json!({
            "job_id": job.id.to_string(),
            "reused": false,
            "status": "draft",
            "content_hash": content_hash,
        }))
    }

    async fn job_status(&self, params: &Value) -> std::result::Result<Value, ApiError> {
        let p: JobIdParams = Self::parse(params)?;
        let id = Self::parse_ulid(&p.job_id, "job_id")?;
        let job = self
            .store
            .get_job(&id)
            .await
            .map_err(store_err)?
            .ok_or_else(|| ApiError::app(error_codes::JOB_NOT_FOUND, "任务不存在"))?;
        Ok(job_json(&job))
    }

    async fn job_tail(&self, params: &Value) -> std::result::Result<Value, ApiError> {
        let p: JobTailParams = Self::parse(params)?;
        let id = Self::parse_ulid(&p.job_id, "job_id")?;
        let limit = p.limit.clamp(1, 2000);
        let (rows, next) = self.tail.read(&id, p.offset, limit);
        Ok(json!({
            "job_id": id.to_string(),
            "offset": p.offset,
            "next_offset": next,
            "lines": rows.into_iter().map(|(seq, line)| json!({"seq": seq, "line": line})).collect::<Vec<_>>(),
        }))
    }

    async fn job_cancel(&self, params: &Value) -> std::result::Result<Value, ApiError> {
        let p: JobIdParams = Self::parse(params)?;
        let id = Self::parse_ulid(&p.job_id, "job_id")?;
        let job = self
            .store
            .get_job(&id)
            .await
            .map_err(store_err)?
            .ok_or_else(|| ApiError::app(error_codes::JOB_NOT_FOUND, "任务不存在"))?;
        if job.parent_id.is_some() {
            // 子任务：调度器直接取消
            let ok = self.sched.cancel(&id).await.map_err(sched_err)?;
            Ok(json!({"cancelled": ok}))
        } else {
            // 工作流任务：置 Cancelled，引擎循环感知后取消子任务
            let cancelled = self
                .store
                .try_cancel(&id)
                .await
                .map_err(store_err)?
                .map(|j| j.status == JobStatus::Cancelled)
                .unwrap_or(false);
            Ok(json!({"cancelled": cancelled}))
        }
    }

    async fn job_list(&self, params: &Value) -> std::result::Result<Value, ApiError> {
        let p: JobListParams = Self::parse(params)?;
        let statuses = p
            .statuses
            .iter()
            .filter_map(|s| s.parse::<JobStatus>().ok())
            .collect::<Vec<_>>();
        let filter = JobFilter {
            statuses,
            molecule_id: p
                .molecule_id
                .as_deref()
                .map(Self::parse_ulid_opt)
                .transpose()?,
            workflow: p.workflow,
            limit: p.limit.clamp(1, 2000),
        };
        let jobs = self.store.list_jobs(&filter).await.map_err(store_err)?;
        Ok(Value::Array(jobs.iter().map(job_json).collect()))
    }

    // ------------------------------------------------------------------
    // 结果
    // ------------------------------------------------------------------

    async fn res_scalar(&self, params: &Value) -> std::result::Result<Value, ApiError> {
        let p: ResScalarParams = Self::parse(params)?;
        let id = Self::parse_ulid(&p.job_id, "job_id")?;
        let rows = self.store.results_for_job(&id).await.map_err(store_err)?;
        let rows: Vec<&xtbp_core::ScalarResult> = match &p.key {
            Some(k) => rows.iter().filter(|r| &r.key == k).collect(),
            None => rows.iter().collect(),
        };
        Ok(json!({
            "job_id": id.to_string(),
            "scalars": rows.iter().map(|r| json!({
                "key": r.key, "value": r.value, "unit": r.unit, "tier": r.tier,
            })).collect::<Vec<_>>(),
        }))
    }

    async fn res_spectrum(&self, params: &Value) -> std::result::Result<Value, ApiError> {
        let p: ResSpectrumParams = Self::parse(params)?;
        let id = Self::parse_ulid(&p.job_id, "job_id")?;
        let kind = p.kind.clone().unwrap_or_else(|| "stda-gaussian".into());
        let spectrum = self
            .store
            .spectrum_for_job(&id, &kind)
            .await
            .map_err(store_err)?
            .ok_or_else(|| ApiError::app(error_codes::JOB_NOT_FOUND, "光谱不存在"))?;
        serde_json::to_value(&spectrum).map_err(|e| ApiError::internal(e.to_string()))
    }

    async fn res_export(&self, params: &Value) -> std::result::Result<Value, ApiError> {
        let p: ResExportParams = Self::parse(params)?;
        let out_dir = p
            .out_dir
            .map(PathBuf::from)
            .unwrap_or_else(|| self.data_dir.join("exports"));
        std::fs::create_dir_all(&out_dir).map_err(io_err)?;
        let stamp = format_unix(now_unix()).replace([' ', ':'], "-");

        let jobs = match &p.job_id {
            Some(id) => {
                let id = Self::parse_ulid(id, "job_id")?;
                vec![
                    self.store
                        .get_job(&id)
                        .await
                        .map_err(store_err)?
                        .ok_or_else(|| ApiError::app(error_codes::JOB_NOT_FOUND, "任务不存在"))?,
                ]
            }
            None => self
                .store
                .recent_finished(10_000)
                .await
                .map_err(store_err)?,
        };

        let mut written = Vec::new();
        match p.format.as_str() {
            "csv" => {
                let path = out_dir.join(format!("scalars-{stamp}.csv"));
                let mut rows = Vec::new();
                for job in &jobs {
                    for r in self
                        .store
                        .results_for_job(&job.id)
                        .await
                        .map_err(store_err)?
                    {
                        rows.push(json!({
                            "job_id": job.id.to_string(),
                            "workflow": job.workflow,
                            "key": r.key,
                            "value": r.value,
                            "unit": r.unit,
                            "tier": r.tier,
                        }));
                    }
                }
                let mut wtr = csv::Writer::from_path(&path).map_err(csv_err)?;
                for row in &rows {
                    wtr.serialize(row).map_err(csv_err)?;
                }
                wtr.flush().map_err(io_err)?;
                written.push(path);
            }
            "json" => {
                let mut bundle = Vec::new();
                for job in &jobs {
                    let results = self
                        .store
                        .results_for_job(&job.id)
                        .await
                        .map_err(store_err)?;
                    let spectra = self
                        .store
                        .spectrum_for_job(&job.id, "stda-gaussian")
                        .await
                        .map_err(store_err)?;
                    bundle.push(json!({
                        "job": job_json(job),
                        "results": results,
                        "spectrum": spectra,
                    }));
                }
                let path = out_dir.join(format!("results-{stamp}.json"));
                std::fs::write(
                    &path,
                    serde_json::to_string_pretty(&bundle)
                        .map_err(|e| ApiError::internal(e.to_string()))?,
                )
                .map_err(io_err)?;
                written.push(path);
            }
            other => return Err(ApiError::invalid_params(format!("未知导出格式: {other}"))),
        }
        Ok(json!({
            "files": written.iter().map(|p| p.display().to_string()).collect::<Vec<_>>(),
            "count": jobs.len(),
        }))
    }

    // ------------------------------------------------------------------
    // 实例与健康
    // ------------------------------------------------------------------

    async fn inst_list(&self) -> std::result::Result<Value, ApiError> {
        let reg = self.registry.lock().unwrap();
        let mut entries = Vec::new();
        let names: Vec<String> = {
            let mut set = std::collections::BTreeSet::new();
            for e in reg.entries() {
                set.insert(e.name.clone());
            }
            set.into_iter().collect()
        };
        for name in names {
            for e in reg.all(&name) {
                entries.push(json!({
                    "name": e.name,
                    "version": e.version.to_string(),
                    "exe": e.exe.display().to_string(),
                    "sha256": e.sha256,
                    "id": e.id.to_string(),
                    "capabilities": capabilities_for(&e.name),
                }));
            }
        }
        Ok(json!({ "instances": entries }))
    }

    async fn sys_health(&self) -> std::result::Result<Value, ApiError> {
        let stats = self.sched.stats();
        let helper_alive = self.helper.try_lock().map(|g| g.is_some()).unwrap_or(false);
        let data_ok = !is_under_mnt(&self.data_dir);
        Ok(json!({
            "ok": true,
            "daemon": env!("CARGO_PKG_VERSION"),
            "uptime_secs": now_unix() - self.started_at,
            "queue": stats,
            "helper_alive": helper_alive,
            "data_dir": self.data_dir.display().to_string(),
            "data_dir_on_linux_fs": data_ok,
            "templates_dir": self.templates_dir.display().to_string(),
            "db": "sqlite",
        }))
    }

    // ------------------------------------------------------------------
    // 辅助
    // ------------------------------------------------------------------

    fn parse_ulid_opt(s: &str) -> std::result::Result<Ulid, ApiError> {
        Self::parse_ulid(s, "molecule_id")
    }
}

// ---------------------------------------------------------------------------
// 序列化辅助
// ---------------------------------------------------------------------------

fn molecule_json(mol: &Molecule) -> Value {
    json!({
        "id": mol.id.to_string(),
        "inchikey": mol.inchikey,
        "smiles": mol.smiles,
        "charge": mol.charge.0,
        "multiplicity": mol.multiplicity.0,
        "name": mol.name,
        "created_at": mol.created_at,
    })
}

fn job_summary(job: &Job) -> Value {
    json!({
        "id": job.id.to_string(),
        "workflow": job.workflow,
        "status": job.status.as_str(),
    })
}

fn job_json(job: &Job) -> Value {
    json!({
        "id": job.id.to_string(),
        "molecule_id": job.molecule_id.to_string(),
        "workflow": job.workflow,
        "params": job.params,
        "content_hash": job.content_hash,
        "status": job.status.as_str(),
        "priority": job.priority,
        "attempt": job.attempt,
        "workdir": job.workdir,
        "created_at": job.created_at,
        "updated_at": job.updated_at,
        "started_at": job.started_at,
        "finished_at": job.finished_at,
        "exit_code": job.exit_code,
        "error_code": job.error_code,
        "error_message": job.error_message,
        "parent_id": job.parent_id.map(|p| p.to_string()),
        "parse_degraded": job.parse_degraded,
    })
}

/// 组件能力声明（设计文档 §3.2）。
fn capabilities_for(name: &str) -> Vec<&'static str> {
    match name {
        "xtb" => vec![
            "GFN0-xTB", "GFN1-xTB", "GFN2-xTB", "GFN-FF", "ALPB", "opt", "freq", "sp",
        ],
        "crest" => vec!["conformer-search"],
        "xtb4stda" => vec!["GFN-orbitals"],
        "stda" => vec!["sTDA-xTB"],
        "qcg" | "aiss" => vec!["registered-only"],
        _ => vec![],
    }
}

// ---------------------------------------------------------------------------
// 错误映射
// ---------------------------------------------------------------------------

fn store_err(e: xtbp_store::StoreError) -> ApiError {
    ApiError::internal(format!("持久层错误: {e}"))
}

fn sched_err(e: xtbp_sched::SchedError) -> ApiError {
    ApiError::app(error_codes::RESOURCE_EXHAUSTED, e.to_string())
}

fn io_err(e: std::io::Error) -> ApiError {
    ApiError::internal(format!("IO 错误: {e}"))
}

fn csv_err(e: csv::Error) -> ApiError {
    ApiError::internal(format!("CSV 错误: {e}"))
}

/// /mnt/c 红线自检（设计文档 §4.3：工作目录严禁 9P 盘）。
pub fn is_under_mnt(path: &std::path::Path) -> bool {
    path.starts_with("/mnt")
}

/// 物理核心数（并发槽默认值依据，§3.3）。
pub fn physical_cores() -> usize {
    std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1)
}

/// 默认并发槽：核心数 / 每任务线程（至少 1）。
pub fn default_concurrency(default_threads: u32) -> usize {
    (physical_cores() / default_threads.max(1) as usize).max(1)
}
