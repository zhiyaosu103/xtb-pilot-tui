//! 持久层：SQLite（sqlx）承载 job/run/spectrum 记录；CSV 用于导出
//! （规划文档 §2.2：导出即可，不引 polars）。

use crate::error::Result;
use sqlx::sqlite::{SqliteConnectOptions, SqlitePool};
use std::path::Path;

/// 打开（必要时创建）SQLite 数据库。
pub async fn connect(path: &Path) -> Result<SqlitePool> {
    let opts = SqliteConnectOptions::new()
        .filename(path)
        .create_if_missing(true);
    Ok(SqlitePool::connect_with(opts).await?)
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
            Row { job_id: "01J1".into(), status: "done".into() },
            Row { job_id: "01J2".into(), status: "failed".into() },
        ];
        let mut buf = Vec::new();
        export_csv(&rows, &mut buf).unwrap();
        let out = String::from_utf8(buf).unwrap();
        assert!(out.contains("job_id,status"));
        assert!(out.contains("01J1,done"));
    }
}
