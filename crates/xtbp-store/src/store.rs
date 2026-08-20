//! 持久层基础能力：SQLite 连接/迁移与 CSV 导出（设计文档 §3.5）。
//!
//! 领域表（molecules/jobs/results/spectra/artifacts）的仓储层在后续里程碑
//! 于本 crate 内扩展；本模块只提供连接、迁移与 CSV 序列化三个基础原语。

use sqlx::sqlite::{SqliteConnectOptions, SqlitePool, SqlitePoolOptions};
use std::path::Path;
use thiserror::Error;

/// 持久层错误。
#[derive(Debug, Error)]
pub enum StoreError {
    #[error("IO 错误: {0}")]
    Io(#[from] std::io::Error),

    #[error("SQLite 错误: {0}")]
    Sqlx(#[from] sqlx::Error),

    #[error("SQLite 迁移错误: {0}")]
    Migrate(#[from] sqlx::migrate::MigrateError),

    #[error("CSV 错误: {0}")]
    Csv(#[from] csv::Error),
}

/// 持久层便捷 Result 别名。
pub type Result<T> = std::result::Result<T, StoreError>;

/// 打开（必要时创建）SQLite 数据库，WAL 模式 + busy_timeout。
pub async fn connect(path: &Path) -> Result<SqlitePool> {
    let opts = SqliteConnectOptions::new()
        .filename(path)
        .create_if_missing(true)
        .journal_mode(sqlx::sqlite::SqliteJournalMode::Wal)
        .busy_timeout(std::time::Duration::from_secs(5));
    // 连接池：单写者 + 少量读连接；WAL 下并发读不被写阻塞。
    let pool = SqlitePoolOptions::new()
        .max_connections(8)
        .connect_with(opts)
        .await?;
    Ok(pool)
}

/// 执行嵌入式迁移（`migrations/` 目录，sqlx `migrate` feature）。
pub async fn migrate(pool: &SqlitePool) -> Result<()> {
    sqlx::migrate!("./migrations").run(pool).await?;
    Ok(())
}

/// 把行集合导出为 CSV（导出即够，刻意不引 polars）。
pub fn export_csv<T: serde::Serialize, W: std::io::Write>(
    rows: &[T],
    writer: &mut W,
) -> Result<()> {
    let mut wtr = csv::Writer::from_writer(writer);
    for row in rows {
        wtr.serialize(row)?;
    }
    wtr.flush()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn sqlite_connect_and_migrate() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("test.db");
        let pool = connect(&db).await.unwrap();
        migrate(&pool).await.unwrap();
        pool.close().await;
        assert!(db.exists());
    }

    #[test]
    fn csv_export_rows() {
        #[derive(serde::Serialize)]
        struct Row {
            job_id: String,
            status: String,
        }
        let rows = vec![
            Row {
                job_id: "01J1".into(),
                status: "done".into(),
            },
            Row {
                job_id: "01J2".into(),
                status: "failed".into(),
            },
        ];
        let mut buf = Vec::new();
        export_csv(&rows, &mut buf).unwrap();
        let out = String::from_utf8(buf).unwrap();
        assert!(out.contains("job_id,status"));
        assert!(out.contains("01J1,done"));
    }
}
