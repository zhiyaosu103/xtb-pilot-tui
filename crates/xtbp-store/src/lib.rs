//! xTB-Pilot 持久层（设计文档 §3.5 / 规划文档 §2.2）。
//!
//! 双层存储：SQLite（`~/.local/share/xtbpilot/xtbp.db`，WAL 模式）+ 文件仓
//! （`data/<hash[:2]>/<hash>/`，只增不改）。迁移一律走 `sqlx migrate`。

pub mod filerepo;
pub mod repo;
pub mod store;

pub use filerepo::FileRepo;
pub use repo::{JobFilter, Store};
pub use store::{Result, StoreError, connect, export_csv, migrate};
