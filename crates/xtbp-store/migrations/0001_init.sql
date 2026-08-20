-- xTB-Pilot 全量初始 schema（设计文档 §3.5 双层存储之 SQLite 层）。
-- 状态字符串与 xtbp_core::job::JobStatus::as_str() 保持一致。
-- 时间戳一律 Unix 秒（整数）；params 为 JobParams 的 JSON 快照。

CREATE TABLE IF NOT EXISTS molecules (
    id           TEXT PRIMARY KEY NOT NULL,      -- ULID
    inchikey     TEXT NOT NULL DEFAULT '',       -- 未知时为空串
    smiles       TEXT NOT NULL,
    charge       INTEGER NOT NULL DEFAULT 0,
    multiplicity INTEGER NOT NULL DEFAULT 1,
    name         TEXT,
    created_at   INTEGER NOT NULL                -- Unix 秒
);
CREATE INDEX IF NOT EXISTS idx_molecules_inchikey ON molecules(inchikey);
CREATE INDEX IF NOT EXISTS idx_molecules_smiles ON molecules(smiles);

CREATE TABLE IF NOT EXISTS jobs (
    id             TEXT PRIMARY KEY NOT NULL,    -- ULID
    molecule_id    TEXT NOT NULL REFERENCES molecules(id),
    workflow       TEXT NOT NULL,                -- 模板 id（opt/conformer/...）
    params         TEXT NOT NULL,                -- JobParams JSON 快照
    content_hash   TEXT NOT NULL,                -- 幂等去重键
    status         TEXT NOT NULL DEFAULT 'draft',
    priority       INTEGER NOT NULL DEFAULT 0,   -- 0=交互优先
    attempt        INTEGER NOT NULL DEFAULT 0,
    workdir        TEXT,                         -- 计算目录（Linux 文件系统内）
    created_at     INTEGER NOT NULL,
    updated_at     INTEGER NOT NULL,
    started_at     INTEGER,
    finished_at    INTEGER,
    exit_code      INTEGER,
    error_code     TEXT,                         -- 结构化错误码
    error_message  TEXT,
    parent_id      TEXT REFERENCES jobs(id),     -- DAG 父边
    parse_degraded INTEGER NOT NULL DEFAULT 0    -- 解析降级标记
);
CREATE INDEX IF NOT EXISTS idx_jobs_status ON jobs(status);
CREATE INDEX IF NOT EXISTS idx_jobs_content_hash ON jobs(content_hash);
CREATE INDEX IF NOT EXISTS idx_jobs_molecule ON jobs(molecule_id);
CREATE INDEX IF NOT EXISTS idx_jobs_parent ON jobs(parent_id);

-- 行式标量结果（(job, key, value, unit)）
CREATE TABLE IF NOT EXISTS results (
    job_id TEXT NOT NULL REFERENCES jobs(id),
    key    TEXT NOT NULL,
    value  REAL NOT NULL,
    unit   TEXT NOT NULL,
    tier   TEXT NOT NULL DEFAULT 'screening',    -- §4.4 精度声明
    PRIMARY KEY (job_id, key)
);
CREATE INDEX IF NOT EXISTS idx_results_job ON results(job_id);

-- 光谱（跃迁表 + 展宽采样点，JSON blob）
CREATE TABLE IF NOT EXISTS spectra (
    job_id TEXT NOT NULL REFERENCES jobs(id),
    kind   TEXT NOT NULL,                        -- 如 "stda-gaussian-0.4ev"
    data   TEXT NOT NULL,                        -- Spectrum JSON
    PRIMARY KEY (job_id, kind)
);

-- 产物文件登记（路径 + sha256）
CREATE TABLE IF NOT EXISTS artifacts (
    job_id TEXT NOT NULL REFERENCES jobs(id),
    kind   TEXT NOT NULL,                        -- 如 "xyz-opt" / "stdout" / "tda"
    path   TEXT NOT NULL,
    sha256 TEXT NOT NULL,
    PRIMARY KEY (job_id, kind)
);
