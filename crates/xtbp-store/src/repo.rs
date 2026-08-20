//! 仓储层：molecules / jobs / results / spectra / artifacts 的 SQLite 访问
//! （设计文档 §3.5）。领域对象与 `xtbp-core` 类型一一对应；
//! `params` 以 JobParams JSON 快照落库，状态以字符串落库。

use crate::store::{Result, StoreError};
use sqlx::{QueryBuilder, Row, Sqlite, SqlitePool};
use std::path::Path;
use xtbp_core::Ulid;
use xtbp_core::job::{Job, JobParams, JobStatus};
use xtbp_core::molecule::Molecule;
use xtbp_core::result::{ScalarResult, Spectrum};

/// 存储门面：SQLite 连接池 + 数据目录。
#[derive(Clone)]
pub struct Store {
    pool: SqlitePool,
}

impl Store {
    /// 打开（必要时创建并迁移）数据库。
    pub async fn open(db_path: &Path) -> Result<Self> {
        let pool = crate::store::connect(db_path).await?;
        crate::store::migrate(&pool).await?;
        Ok(Self { pool })
    }

    /// 裸连接池（供扩展与测试）。
    pub fn pool(&self) -> &SqlitePool {
        &self.pool
    }

    // ------------------------------------------------------------------
    // molecules
    // ------------------------------------------------------------------

    /// 按去重键查已有分子；不存在则插入。返回库中记录（id 以库为准）。
    pub async fn ensure_molecule(&self, mol: &Molecule) -> Result<Molecule> {
        if let Some(existing) = self.find_molecule_by_dedup(&mol.dedup_key()).await? {
            return Ok(existing);
        }
        sqlx::query(
            "INSERT INTO molecules (id, inchikey, smiles, charge, multiplicity, name, created_at)
             VALUES (?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(mol.id.to_string())
        .bind(&mol.inchikey)
        .bind(&mol.smiles)
        .bind(mol.charge.0)
        .bind(mol.multiplicity.0)
        .bind(&mol.name)
        .bind(mol.created_at)
        .execute(&self.pool)
        .await?;
        Ok(mol.clone())
    }

    /// 去重键查询：inchikey 优先，否则 (smiles, charge, multiplicity)。
    async fn find_molecule_by_dedup(&self, dedup_key: &str) -> Result<Option<Molecule>> {
        if dedup_key.contains('|') {
            // "smiles|charge|mult" 形式
            let parts: Vec<&str> = dedup_key.split('|').collect();
            let row = sqlx::query(
                "SELECT * FROM molecules WHERE smiles = ? AND charge = ? AND multiplicity = ? LIMIT 1",
            )
            .bind(parts[0])
            .bind(parts[1].parse::<i64>().unwrap_or(0))
            .bind(parts[2].parse::<i64>().unwrap_or(1))
            .fetch_optional(&self.pool)
            .await?;
            Ok(row.map(molecule_from_row))
        } else {
            let row = sqlx::query("SELECT * FROM molecules WHERE inchikey = ? LIMIT 1")
                .bind(dedup_key)
                .fetch_optional(&self.pool)
                .await?;
            Ok(row.map(molecule_from_row))
        }
    }

    /// 取分子。
    pub async fn get_molecule(&self, id: &Ulid) -> Result<Option<Molecule>> {
        let row = sqlx::query("SELECT * FROM molecules WHERE id = ?")
            .bind(id.to_string())
            .fetch_optional(&self.pool)
            .await?;
        Ok(row.map(molecule_from_row))
    }

    /// 分子列表（新→旧，限 limit 条）。
    pub async fn list_molecules(&self, limit: i64) -> Result<Vec<Molecule>> {
        let rows = sqlx::query("SELECT * FROM molecules ORDER BY created_at DESC LIMIT ?")
            .bind(limit)
            .fetch_all(&self.pool)
            .await?;
        Ok(rows.into_iter().map(molecule_from_row).collect())
    }

    /// 回填 inchikey（组装阶段由 RDKit 计算）。
    pub async fn set_inchikey(&self, id: &Ulid, inchikey: &str) -> Result<()> {
        sqlx::query("UPDATE molecules SET inchikey = ? WHERE id = ?")
            .bind(inchikey)
            .bind(id.to_string())
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    // ------------------------------------------------------------------
    // jobs
    // ------------------------------------------------------------------

    /// 插入新任务（Draft）。
    pub async fn insert_job(&self, job: &Job) -> Result<()> {
        let params = serde_json::to_string(&job.params)?;
        sqlx::query(
            "INSERT INTO jobs (id, molecule_id, workflow, params, content_hash, status,
                               priority, attempt, workdir, created_at, updated_at,
                               started_at, finished_at, exit_code, error_code,
                               error_message, parent_id, parse_degraded)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(job.id.to_string())
        .bind(job.molecule_id.to_string())
        .bind(&job.workflow)
        .bind(params)
        .bind(&job.content_hash)
        .bind(job.status.as_str())
        .bind(job.priority)
        .bind(job.attempt)
        .bind(&job.workdir)
        .bind(job.created_at)
        .bind(job.updated_at)
        .bind(job.started_at)
        .bind(job.finished_at)
        .bind(job.exit_code)
        .bind(&job.error_code)
        .bind(&job.error_message)
        .bind(job.parent_id.map(|p| p.to_string()))
        .bind(job.parse_degraded)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// 取任务。
    pub async fn get_job(&self, id: &Ulid) -> Result<Option<Job>> {
        let row = sqlx::query("SELECT * FROM jobs WHERE id = ?")
            .bind(id.to_string())
            .fetch_optional(&self.pool)
            .await?;
        Ok(row.map(job_from_row))
    }

    /// 幂等命中：同一内容哈希且已 Done 的任务（直接复用结果）。
    pub async fn find_done_by_content_hash(&self, hash: &str) -> Result<Option<Job>> {
        let row = sqlx::query(
            "SELECT * FROM jobs WHERE content_hash = ? AND status = 'done' ORDER BY finished_at DESC LIMIT 1",
        )
        .bind(hash)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(job_from_row))
    }

    /// 全量更新任务行（状态机变更后落库）。
    pub async fn update_job(&self, job: &Job) -> Result<()> {
        sqlx::query(
            "UPDATE jobs SET status = ?, priority = ?, attempt = ?, workdir = ?,
                             updated_at = ?, started_at = ?, finished_at = ?,
                             exit_code = ?, error_code = ?, error_message = ?,
                             parse_degraded = ?
             WHERE id = ?",
        )
        .bind(job.status.as_str())
        .bind(job.priority)
        .bind(job.attempt)
        .bind(&job.workdir)
        .bind(job.updated_at)
        .bind(job.started_at)
        .bind(job.finished_at)
        .bind(job.exit_code)
        .bind(&job.error_code)
        .bind(&job.error_message)
        .bind(job.parse_degraded)
        .bind(job.id.to_string())
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// 任务列表（按过滤器；默认按优先级升序、创建时间升序）。
    pub async fn list_jobs(&self, filter: &JobFilter) -> Result<Vec<Job>> {
        let mut qb = QueryBuilder::<Sqlite>::new("SELECT * FROM jobs");
        let mut first = true;
        if !filter.statuses.is_empty() {
            qb.push(" WHERE status IN (");
            let mut sep = qb.separated(", ");
            for s in &filter.statuses {
                sep.push_bind(s.as_str());
            }
            qb.push(")");
            first = false;
        }
        if let Some(mol) = &filter.molecule_id {
            qb.push(if first { " WHERE " } else { " AND " });
            qb.push("molecule_id = ").push_bind(mol.to_string());
            first = false;
        }
        if let Some(wf) = &filter.workflow {
            qb.push(if first { " WHERE " } else { " AND " });
            qb.push("workflow = ").push_bind(wf);
        }
        qb.push(" ORDER BY priority ASC, created_at ASC LIMIT ")
            .push_bind(filter.limit.max(1));
        let rows = qb.build().fetch_all(&self.pool).await?;
        Ok(rows.into_iter().map(job_from_row).collect())
    }

    /// 非终态任务（daemon 启动时核对：崩溃恢复）。
    pub async fn list_non_terminal_jobs(&self) -> Result<Vec<Job>> {
        let rows = sqlx::query(
            "SELECT * FROM jobs WHERE status NOT IN ('done','failed','cancelled') ORDER BY priority ASC, created_at ASC",
        )
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.into_iter().map(job_from_row).collect())
    }

    /// 某任务的子任务（DAG 边）。
    pub async fn children_of(&self, id: &Ulid) -> Result<Vec<Job>> {
        let rows = sqlx::query("SELECT * FROM jobs WHERE parent_id = ? ORDER BY created_at ASC")
            .bind(id.to_string())
            .fetch_all(&self.pool)
            .await?;
        Ok(rows.into_iter().map(job_from_row).collect())
    }

    /// 尝试取消：非终态任务 → Cancelled。
    pub async fn try_cancel(&self, id: &Ulid) -> Result<Option<Job>> {
        let mut job = match self.get_job(id).await? {
            Some(j) => j,
            None => return Ok(None),
        };
        if job.status.is_terminal() {
            return Ok(Some(job));
        }
        job.transition(JobStatus::Cancelled)
            .map_err(StoreError::Invalid)?;
        self.update_job(&job).await?;
        Ok(Some(job))
    }

    // ------------------------------------------------------------------
    // results
    // ------------------------------------------------------------------

    /// 写入/覆盖标量结果。
    pub async fn put_result(&self, job_id: &Ulid, r: &ScalarResult) -> Result<()> {
        sqlx::query(
            "INSERT INTO results (job_id, key, value, unit, tier) VALUES (?, ?, ?, ?, ?)
             ON CONFLICT(job_id, key) DO UPDATE SET value = excluded.value,
             unit = excluded.unit, tier = excluded.tier",
        )
        .bind(job_id.to_string())
        .bind(&r.key)
        .bind(r.value)
        .bind(&r.unit)
        .bind(&r.tier)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// 某任务全部标量结果。
    pub async fn results_for_job(&self, job_id: &Ulid) -> Result<Vec<ScalarResult>> {
        let rows =
            sqlx::query("SELECT key, value, unit, tier FROM results WHERE job_id = ? ORDER BY key")
                .bind(job_id.to_string())
                .fetch_all(&self.pool)
                .await?;
        Ok(rows
            .into_iter()
            .map(|row| ScalarResult {
                key: row.get("key"),
                value: row.get("value"),
                unit: row.get("unit"),
                tier: row.get("tier"),
            })
            .collect())
    }

    // ------------------------------------------------------------------
    // spectra
    // ------------------------------------------------------------------

    /// 写入/覆盖光谱。
    pub async fn put_spectrum(&self, job_id: &Ulid, kind: &str, s: &Spectrum) -> Result<()> {
        let data = serde_json::to_string(s)?;
        sqlx::query(
            "INSERT INTO spectra (job_id, kind, data) VALUES (?, ?, ?)
             ON CONFLICT(job_id, kind) DO UPDATE SET data = excluded.data",
        )
        .bind(job_id.to_string())
        .bind(kind)
        .bind(data)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// 某任务某类光谱。
    pub async fn spectrum_for_job(&self, job_id: &Ulid, kind: &str) -> Result<Option<Spectrum>> {
        let row = sqlx::query("SELECT data FROM spectra WHERE job_id = ? AND kind = ?")
            .bind(job_id.to_string())
            .bind(kind)
            .fetch_optional(&self.pool)
            .await?;
        row.map(|r| {
            let data: String = r.get("data");
            Ok(serde_json::from_str(&data)?)
        })
        .transpose()
    }

    // ------------------------------------------------------------------
    // artifacts
    // ------------------------------------------------------------------

    /// 登记产物文件。
    pub async fn put_artifact(
        &self,
        job_id: &Ulid,
        kind: &str,
        path: &str,
        sha256: &str,
    ) -> Result<()> {
        sqlx::query(
            "INSERT INTO artifacts (job_id, kind, path, sha256) VALUES (?, ?, ?, ?)
             ON CONFLICT(job_id, kind) DO UPDATE SET path = excluded.path,
             sha256 = excluded.sha256",
        )
        .bind(job_id.to_string())
        .bind(kind)
        .bind(path)
        .bind(sha256)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// 某任务全部产物登记。
    pub async fn artifacts_for_job(&self, job_id: &Ulid) -> Result<Vec<(String, String, String)>> {
        let rows =
            sqlx::query("SELECT kind, path, sha256 FROM artifacts WHERE job_id = ? ORDER BY kind")
                .bind(job_id.to_string())
                .fetch_all(&self.pool)
                .await?;
        Ok(rows
            .into_iter()
            .map(|r| (r.get("kind"), r.get("path"), r.get("sha256")))
            .collect())
    }

    /// 最近完成的 N 个任务（Dashboard 用）。
    pub async fn recent_finished(&self, limit: i64) -> Result<Vec<Job>> {
        let rows = sqlx::query(
            "SELECT * FROM jobs WHERE status IN ('done','failed') ORDER BY finished_at DESC LIMIT ?",
        )
        .bind(limit)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.into_iter().map(job_from_row).collect())
    }
}

/// 任务列表过滤器。
#[derive(Debug, Clone, Default)]
pub struct JobFilter {
    /// 状态白名单（空 = 全部）。
    pub statuses: Vec<JobStatus>,
    /// 按分子过滤。
    pub molecule_id: Option<Ulid>,
    /// 按模板过滤。
    pub workflow: Option<String>,
    /// 条数上限。
    pub limit: i64,
}

impl JobFilter {
    /// 默认过滤器（全部状态，200 条）。
    pub fn all() -> Self {
        Self {
            statuses: vec![],
            molecule_id: None,
            workflow: None,
            limit: 200,
        }
    }

    /// 只看活动任务（非终态）。
    pub fn active() -> Self {
        Self {
            statuses: vec![
                JobStatus::Draft,
                JobStatus::Queued,
                JobStatus::Running,
                JobStatus::Parsing,
                JobStatus::Interrupted,
            ],
            molecule_id: None,
            workflow: None,
            limit: 500,
        }
    }
}

fn molecule_from_row(row: sqlx::sqlite::SqliteRow) -> Molecule {
    Molecule {
        id: parse_ulid(&row.get::<String, _>("id")),
        inchikey: row.get("inchikey"),
        smiles: row.get("smiles"),
        charge: xtbp_core::Charge(row.get("charge")),
        multiplicity: xtbp_core::Multiplicity(row.get::<i64, _>("multiplicity") as u8),
        name: row.get("name"),
        created_at: row.get("created_at"),
    }
}

fn job_from_row(row: sqlx::sqlite::SqliteRow) -> Job {
    let params_json: String = row.get("params");
    let params: JobParams = serde_json::from_str(&params_json).unwrap_or_default();
    let status: String = row.get("status");
    let status = status.parse().unwrap_or(JobStatus::Interrupted);
    Job {
        id: parse_ulid(&row.get::<String, _>("id")),
        molecule_id: parse_ulid(&row.get::<String, _>("molecule_id")),
        workflow: row.get("workflow"),
        params,
        content_hash: row.get("content_hash"),
        status,
        priority: row.get::<i64, _>("priority") as u8,
        attempt: row.get::<i64, _>("attempt") as u32,
        workdir: row.get("workdir"),
        created_at: row.get("created_at"),
        updated_at: row.get("updated_at"),
        started_at: row.get("started_at"),
        finished_at: row.get("finished_at"),
        exit_code: row.get("exit_code"),
        error_code: row.get("error_code"),
        error_message: row.get("error_message"),
        parent_id: row
            .get::<Option<String>, _>("parent_id")
            .map(|s| parse_ulid(&s)),
        parse_degraded: row.get::<i64, _>("parse_degraded") != 0,
    }
}

/// 解析库内 ULID 字符串（存储层内部辅助；数据损坏时视为内部错误）。
fn parse_ulid(s: &str) -> Ulid {
    s.parse()
        .unwrap_or_else(|e| panic!("数据库中的 ULID 无效: {s}: {e}"))
}
