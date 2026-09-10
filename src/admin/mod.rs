//! 運営用の管理パネル（レート調整と、当日の運用操作）。
//!
//! 設計の要点（実装前に読む場所: 00 §3 / §4、07 §2 / §4）:
//!
//! - **管理者は環境変数の ID リストだけ**（[`config::ADMIN_DISCORD_USER_IDS`]）。
//!   DB に権限の概念は作らない。未設定なら `main.rs` がルートをマウントしない
//!   （`INTERNAL_API_TOKEN` と同じ fail-closed。[`config::admin_ids_from_env`] が `None`）。
//! - **判定はセッションの Discord ID との照合だけ**。リクエスト側で管理者を
//!   名乗る経路は無い。非管理者には 404（403 を返さない = 存在を教えない）。
//! - 調整は `rating_adjustments` への**追記のみ**。`rating_logic` は触らない。
//!   打ち消しは `reverts_id` の既存機構（DB のトリガーと unique index が正で、
//!   アプリはそれを回避しない）。
//! - 理由は必須、`created_by` は行動中の管理者から自動で埋める。
//!   冪等キーで同じフォームの再送信を行にしない。
//! - **当日の運用操作も追記のみ**（[`actions`]、`admin_actions`）。マッチメイカの停止状態も
//!   ブロック枠も可変列に持たず、追記された最新行から導出する。停止状態を
//!   プロセス内に持つと、再起動で勝手に再開する。

pub mod actions;
pub mod config;
pub mod html;
pub mod rating_params;
pub mod repository;
pub mod routes;
pub mod season;

pub use actions::{
    AdminActionKind, AdminActionRow, DEFAULT_BLOCK_ALLOWANCE, MAX_BLOCK_ALLOWANCE, PauseState,
    QueueRemoval, normalize_reason, parse_block_allowance,
};
pub use config::{
    ADMIN_DISCORD_USER_IDS, AdminConfigError, AdminIds, admin_ids_from_env, parse_admin_ids,
};
pub use rating_params::{
    MAX_BASE_DELTA, RatingParamChange, RatingParams, RatingParamsOutcome, RatingParamsPreview,
    normalize_base_delta,
};
pub use repository::{
    ActionOutcome, AdjustmentRow, AdminRepository, AdminStoreError, FoundPlayer, InsertOutcome,
    NewAdjustment, NewAdminAction, NewRatingParams, NewSeasonBoundary, QueuedEntry, QueuedRole,
    RemovalOutcome,
};
pub use routes::{AdminState, router};
pub use season::{
    MAX_SEASON_LABEL_LENGTH, SeasonBoundary, SeasonOutcome, SeasonPreview, confirmation_matches,
    normalize_label,
};
