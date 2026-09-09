//! 試合ライフサイクルの状態機械。
//!
//! 出典: 04-試合ライフサイクル仕様 §1「状態機械」・§5「失敗経路」、
//! Phase 1 修正指示の「Phase 2 への申し送り」、Phase 2 レビュー（2026-09-05）。
//!
//! 設計:
//! - 現在状態は `formation_events` の列から [`derive_state`] で導出する。DB に state 列は無い。
//! - [`apply`] は純粋関数で、**INSERT 前の検証**に使う。不正な遷移は `Err`、panic しない。
//! - [`derive_state`] は永続化済みの列を読むもので、**絶対に失敗しない**。適用できなかった
//!   イベントはスキップして [`Anomaly`] に積む。追記専用のログは消せないので、壊れたデータで
//!   止まるのではなく劣化して動き続ける。
//! - 重複排除は DB の `unique (formation_id, idempotency_key)` の責務。`apply` は吸収しない。
//!   同一キーが2回来たら [`derive_state`] が no-op にして [`AnomalyCause::DuplicateKey`] に記録する。
//! - 同じ種別が違うキーで来たら重複ではなく**訂正**。正常な遷移として状態を更新する
//!   （`code_posted` の貼り直しは部屋コードを上書きする）。
//! - 順序の正は `formation_events.id`（挿入順）。`occurred_at` は期限計算にだけ使う。
//! - 申し送り 1: `in_progress` と `awaiting_reports` は [`FormationState::InProgress`] に統合した。
//! - 申し送り 2: 報告の到着はイベントにしない。積むのは結論（`confirmed` / `disputed`）だけ。
//! - タイムアウト値と候補上限は [`LifecycleConfig`] から読む。コードに定数を埋め込まない。
//! - `subject_player_id` は「このイベントが対象とするプレイヤー」。システム起因でも対象者を入れる。

mod config;
mod error;
mod host;
mod ids;
mod state;

pub use config::{
    DEFAULT_HOST_ACCEPT_TIMEOUT_SECONDS, DEFAULT_HOST_CANDIDATE_LIMIT,
    DEFAULT_REPORT_MAJORITY_THRESHOLD, Deadline, HOST_ACCEPT_TIMEOUT_SECONDS, HOST_CANDIDATE_LIMIT,
    HostAction, LifecycleConfig, LifecycleConfigError, REPORT_MAJORITY_THRESHOLD, next_deadline,
    next_host_action,
};
pub use error::{TransitionError, UnknownValue};
pub use host::{
    DefaultHostSelector, Formation, FormationPlayer, HighestRating, HostSelector,
    LeastRecentlyHosted,
};
pub use ids::{FormationId, PlayerId, Timestamp};
pub use state::{
    AbandonReason, Anomaly, AnomalyCause, EventPayload, FormationEvent, FormationEventKind,
    FormationState, Snapshot, apply, derive_snapshot, derive_snapshot_iter, derive_state,
    derive_state_iter,
};
