//! unite-matchsystem
//!
//! - `lifecycle`: 試合ライフサイクルの状態機械（Phase 2、純粋関数）。
//! - `persistence`: それを sqlx / PostgreSQL に載せる永続化層（Phase 3）。
//! - `discord`: Discord アダプタ（Phase 4、イベント駆動の部分のみ）。
//! - `api`: HTTP API（axum）。Discord アダプタと同じプロセスで動く。
#![forbid(unsafe_code)]

pub mod api;
pub mod discord;
pub mod lifecycle;
pub mod persistence;
