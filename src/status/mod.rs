//! 自分の戦績画面（プレイヤーの戦績）。
//!
//! 表示するのは **自分のレートと自分の試合履歴だけ**。他人の `player_id` を指定して
//! 見る経路を作らない — 対象は [`CurrentSession`] からしか取らない。
//!
//! 不変条件（このモジュールで守るもの）:
//! - **レーティングは導出値**（00 §3-2）。レートは [`crate::rating`] の既存経路
//!   （[`crate::rating::RatingAdjustmentRepository::display_rating`]）を読むだけで、
//!   現在値を保存も再計算もしない。
//! - **既存ビューだけが入力**。`player_match_rating_deltas` 以外の集計経路を作らない
//!   （00 §3-3）。日付は `formations.created_at` を join するだけで、これは
//!   実体の参照であって集計の追加ではない。
//! - **確定していない試合を出さない**（00 §3-5）。ビューは `confirmed` イベントを持つ
//!   formation しか出さないため、in_progress / disputed は構造的に現れない。
//!   逆に言えば、この画面から他人の報告内容が漏れる経路は無い。
//! - 読み取り専用。追記専用テーブルへ書き込む SQL を持たない。

pub mod html;
pub mod repository;
pub mod routes;

pub use repository::{MatchHistoryEntry, StatusRepository, StatusSummary};
pub use routes::{StatusState, router};
