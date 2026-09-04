//! unite-matchsystem
//!
//! - `lifecycle`: 試合ライフサイクルの状態機械（Phase 2、純粋関数）。
//! - `persistence`: それを sqlx / PostgreSQL に載せる永続化層（Phase 3）。
#![forbid(unsafe_code)]

pub mod lifecycle;
pub mod persistence;
