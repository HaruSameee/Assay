//! 無効試合の原因になった人の通報（4/5/6）。
//!
//! 「無効試合」を報告した人に、原因のプレイヤーを 1 人選ばせる。
//! 聞き専報告（[`crate::quiet`]）と形は同じだが、3 つ違う:
//!
//! * **相手チームも通報できる。**部屋を壊すのは味方とは限らない
//! * **1 人 1 票で選び直せない。**「原因は誰か」は 1 つの答えなので、
//!   `(formation, reporter)` で一意にする
//! * **「誰も通報しない」を票として記録する。**答えていない人と区別できないと、
//!   画面が催促を出し続ける
//!
//! **票数も閾値もプレイヤー側には出さない。**「あと何票で成立するか」を
//! 逆算させないため、サーバー側でそもそも送らない（docs/09）。
//!
//! 効くのは**その試合が `nullified` で終わったときだけ**。通報は報告直後に
//! 受け付けるので、結局 confirmed になった試合の票も残るが、そちらは
//! `nullified_report_penalties` に現れない。

pub mod model;
pub mod repository;

pub use model::{NullifiedReportError, NullifiedReportOutcome};
pub use repository::{
    NullifiedReportRepository, NullifiedReportRow, NullifiedTally, ReportableParticipant,
};
