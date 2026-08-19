-- 初始 schema：job 表（xTB-Pilot 持久层骨架，后续里程碑扩展 run/molecule/spectrum）。
CREATE TABLE IF NOT EXISTS job (
    id          TEXT PRIMARY KEY NOT NULL,            -- ULID 字符串
    created_at  TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    status      TEXT NOT NULL DEFAULT 'queued',       -- queued|running|done|failed|cancelled
    payload     TEXT NOT NULL DEFAULT '{}'            -- 任务描述（JSON）
);
