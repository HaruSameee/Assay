//! キュー（07-マッチング仕様 §3・§4-5・§5）。
//!
//! 範囲は扱うが、**10人を選ぶアルゴリズムはここには無い**（マッチメイカは別）。
//!
//! 不変条件（このモジュールで守るもの）:
//! - **キューは可変**。エントリと離脱が頻繁に起き、残すべき履歴は formation 側にある。
//!   追記専用の4テーブルとは扱いが違う（[`repository`] の冒頭を読むこと）。
//! - **1プレイヤー1エントリ**。`queue_entries.player_id` の unique で二重キューを拒否する。
//! - **上方向の範囲 +95 は固定**（[`model::UP_RANGE`]）。列にしない。
//! - **「範囲内で N 人」は相互条件で数える**（[`range`]）。片方向で数えると、
//!   実際には組めない人数を画面に出すことになる。
//! - **フレックス補償は導出値**（[`flex`]）。プレイヤーの可変列に持たない。
//! - 他人のエントリを操作できる経路を作らない。`player_id` はセッションからしか来ない。

pub mod flex;
pub(crate) mod html;
pub mod model;
pub mod range;
pub mod repository;
pub mod routes;

pub use flex::{RoleAssignment, flex_compensation_owed, latest_assignment};
pub use model::{
    DEFAULT_DOWN_RANGE, MAX_DOWN_RANGE, MAX_ROLE_PREFERENCES, MIN_DOWN_RANGE, QueueEntry,
    QueueEntryId, QueueRequest, QueueRequestError, REMOVAL_NOTICE_WINDOW_HOURS, RolePreference,
    StaffRemoval, UP_RANGE, VoiceDestination, clamp_down_range, validate_down_range,
    validate_request,
};
pub use range::{QueuedPlayer, accepts, count_in_range, mutually_in_range};
pub use repository::{QueueError, QueueRepository};
pub use routes::{QueueState, router};
