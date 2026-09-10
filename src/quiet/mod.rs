//! 聞き専報告。**味方からの複数票でレートを 1 下げる。**
//!
//! docs/02 の却下ログ「パフォーマンス指標をレートに入れない（全部自己申告になり、
//! 他 9 人がクロスチェックできない）」に対する、運営判断による明示的な例外である。
//!
//! - **自己申告ではない。**他者からの複数票なので、却下の理由（クロスチェックできない）は
//!   ここには当たらない。勝ち 3 票 / 負け 4 票という閾値がその代替になっている
//! - **抑止はレート値（-1）ではない。**運営が通報履歴を確認することで行う。
//!   1 試合の勝敗が ±7 なので、-1 の大きさに抑止効果は期待していない
//! - **報告を確定後に限定し、勝敗で閾値を変えている**のは、負け直後の報告が
//!   「戦犯選出」になることを避けるため。負け側を満場一致（4/4）にしているのが防波堤
//!
//! 不変条件（このモジュールで守るもの）:
//!
//! - **票数と閾値を外へ出さない。**HTML にも API にも載せない。表示しなくても JSON に
//!   入っていれば読めるので、**サーバー側でそもそも送らない**（docs/09）。
//!   本人に返すのは「自分が報告したか」だけ
//! - **誰が誰を報告したかは、他のプレイヤーの応答に一切現れない。**読めるのは運営パネルだけ
//! - **レートは導出値。**ペナルティも現在値の列を持たず、票を数えて閾値と比べるだけで、
//!   `player_current_ratings` が合算する。recompute から完全に再構築できる

pub mod config;
pub mod model;
pub mod repository;

pub use config::{
    QUIET_REPORT_LOSER_THRESHOLD, QUIET_REPORT_WINNER_THRESHOLD, QuietReportConfig,
    QuietReportConfigError,
};
pub use model::{QuietReportError, QuietReportOutcome};
pub use repository::{QuietReportRepository, QuietReportRow, QuietReporterTally};
