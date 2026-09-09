//! マッチメイカ（07-マッチング仕様 §4）。
//!
//! 3 分ごとにキューから 10 人を選び、VC を確保して formation を確定させる。
//!
//! 不変条件（このモジュールで守るもの）:
//! - **選択は純粋関数**（[`select::select_matches`]）。DB にも Discord にも触らず、時刻は
//!   引数で受ける。Phase 2 の状態機械と同じ設計。
//! - **総当たりを書かない**（07 §4-3）。スライディングウィンドウ＋最小費用流。
//! - **VC の空きを確認してから formation を作る**（00 §6）。座席が無いのに formation を
//!   作ると 10 人のキュー待ちを捨てることになる。確保と確定は同一トランザクション。
//! - **キューからの除去も同一トランザクション**。「キューから消えたが formation が無い」
//!   状態を作らない。
//! - 予約状態をメモリに持たない。占有は DB から導出する（プロセス再起動で消えないため）。
//! - Discord への通知経路をここに作らない。`vc_assigned` まで書けば状態は
//!   `awaiting_host` になり、既存の `resume_unfinished`（5 秒スキャン）が拾って
//!   割り当て表とホスト打診を出す。**復帰の入口を 2 つ作らない**（00 §7 の申し送り）。

pub mod flow;
pub mod repository;
pub mod runner;
pub mod select;

pub use repository::{
    ActiveFormationCounts, AssignedFormation, FirstChoiceRoleCounts, MatchmakerRepository,
    MatchmakerSummary, RecentCompletionCounts, RunOutcome, SelectionMetrics, teams_are_swapped,
};
pub use runner::{Matchmaker, MatchmakerConfig, MatchmakerConfigError};
pub use select::{
    AllyBlocks, Candidate, MATCH_SIZE, MATCHMAKING_PARAMS_VERSION, PREDICTED_WIN_PROB,
    ProposedMatch, ProposedPlayer, ROLES, RoleWish, TEAM_SIZE, select_matches,
    select_matches_with_ally_blocks,
};
