//! 係争（`disputed`）の裁定。**追記のみで、新しいテーブルは作らない。**
//!
//! これまで disputed には解決経路が無く、運営チャンネルへ通知が飛ぶだけだった。
//! 残ったままだと 2 つ困る:
//!
//! * その試合で未報告のプレイヤーが**永久にキューへ入れない**
//!   （`requires_reports()` に `disputed` が入っているため、報告ゲートが開かない）
//! * シーズン境界を引いた後に裁定しても、新シーズンのレートには入らない
//!   （境界は `formations.created_at` で切るため）
//!
//! 裁定は `formation_events` に終端イベントを 1 行積むだけで表す。状態機械が
//! `Disputed -> Confirmed | Nullified | Voided` を許しているので、通常の追記経路
//! （[`crate::persistence::FormationRepository::append_event`]）にそのまま乗る。
//! **報告は 1 行も書き換えない。**割れた報告はそのまま残り、裁定はその上に乗る
//! 別の 1 行である（docs/02 不変条件 1）。

use chrono::{DateTime, Utc};

use crate::lifecycle::{FormationEventKind, FormationId};
use crate::persistence::{Outcome, Team};

/// 運営が選べる裁定。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DisputeDecision {
    /// 勝敗を確定させる。**レートに入る。**
    Win(Team),
    /// 無効試合として確定させる。レートは動かないが、試合は「あった」ことになる。
    Nullified,
    /// 試合が成立しなかったことにする。レートは動かない。
    ///
    /// `nullified` との違いは記録上の意味だけで、どちらもレートには入らない。
    /// 部屋が立たなかった・そもそも始まっていない、のように
    /// 「無効試合の報告」ですらないものはこちら。
    Voided,
}

impl DisputeDecision {
    /// フォームの値。
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Win(team) => team.as_str(),
            Self::Nullified => "nullified",
            Self::Voided => "voided",
        }
    }

    /// 画面に出す名前。**内部の識別子を運営に読ませない。**
    pub fn label(self) -> &'static str {
        match self {
            Self::Win(Team::Red) => "赤の勝ちで確定",
            Self::Win(Team::Blue) => "青の勝ちで確定",
            Self::Nullified => "無効試合として確定",
            Self::Voided => "不成立（記録から外す）",
        }
    }

    /// 積むイベントの種別。
    pub fn kind(self) -> FormationEventKind {
        match self {
            Self::Win(_) => FormationEventKind::Confirmed,
            Self::Nullified => FormationEventKind::Nullified,
            Self::Voided => FormationEventKind::Voided,
        }
    }

    /// **レートに入るか。**確認画面の文言を分けるために使う。
    pub fn changes_ratings(self) -> bool {
        matches!(self, Self::Win(_))
    }

    /// `formation_events.payload` に載せる outcome。`voided` は勝敗の概念が無い。
    pub fn outcome(self) -> Option<Outcome> {
        match self {
            Self::Win(team) => Some(Outcome::Win(team)),
            Self::Nullified => Some(Outcome::Nullified),
            Self::Voided => None,
        }
    }

    /// 画面に出す全選択肢。**並び順は「レートに入るもの」を先に置く。**
    pub const ALL: [Self; 4] = [
        Self::Win(Team::Red),
        Self::Win(Team::Blue),
        Self::Nullified,
        Self::Voided,
    ];
}

/// フォームの値を裁定に変える。**知らない値は通さない。**
pub fn parse_decision(raw: &str) -> Option<DisputeDecision> {
    match raw.trim() {
        "red" => Some(DisputeDecision::Win(Team::Red)),
        "blue" => Some(DisputeDecision::Win(Team::Blue)),
        "nullified" => Some(DisputeDecision::Nullified),
        "voided" => Some(DisputeDecision::Voided),
        _ => None,
    }
}

/// 裁定を追記する 1 件。
#[derive(Debug, Clone)]
pub struct NewDisputeResolution {
    /// 対象の formation。
    pub formation_id: FormationId,
    /// 裁定。**検証済みの値を渡すこと**（[`parse_decision`]）。
    pub decision: DisputeDecision,
    /// 運営が書いた理由。
    pub reason: String,
    /// 操作した運営。
    pub actor_discord_user_id: i64,
    /// 追記時刻。
    pub created_at: DateTime<Utc>,
    /// 冪等キー。**同一キーのみ no-op**（状態ベースにしない）。
    pub idempotency_key: String,
}

/// 裁定の結果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DisputeOutcome {
    /// 追記した。
    Resolved {
        /// 追記後の状態（`confirmed` / `nullified` / `voided`）。
        state: String,
    },
    /// **係争中ではない。**既に誰かが裁定した後や、そもそも別の状態。
    NotDisputed {
        /// そのときの状態。画面の文言に使う。
        state: String,
    },
    /// 同じ冪等キーで既に追記済み。
    Duplicate,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_the_four_known_values_are_accepted() {
        assert_eq!(parse_decision("red"), Some(DisputeDecision::Win(Team::Red)));
        assert_eq!(
            parse_decision(" blue "),
            Some(DisputeDecision::Win(Team::Blue))
        );
        assert_eq!(
            parse_decision("nullified"),
            Some(DisputeDecision::Nullified)
        );
        assert_eq!(parse_decision("voided"), Some(DisputeDecision::Voided));
        for unknown in ["", "draw", "disputed", "abandoned", "RED", "赤"] {
            assert_eq!(parse_decision(unknown), None, "{unknown}");
        }
    }

    /// **レートに入るのは勝敗を確定させたときだけ。**
    #[test]
    fn only_a_win_reaches_the_ratings() {
        assert!(DisputeDecision::Win(Team::Red).changes_ratings());
        assert!(DisputeDecision::Win(Team::Blue).changes_ratings());
        assert!(!DisputeDecision::Nullified.changes_ratings());
        assert!(!DisputeDecision::Voided.changes_ratings());
    }

    /// 積む種別が状態機械の終端と一致している。
    ///
    /// `Disputed` から実際に積めることは統合テスト
    /// （`tests/admin_ops.rs` の裁定テスト）が本物のイベント列で確かめる。
    /// ここは種別の取り違えだけを止める。
    #[test]
    fn each_decision_appends_a_terminal_event() {
        use FormationEventKind as K;
        assert_eq!(DisputeDecision::Win(Team::Red).kind(), K::Confirmed);
        assert_eq!(DisputeDecision::Win(Team::Blue).kind(), K::Confirmed);
        assert_eq!(DisputeDecision::Nullified.kind(), K::Nullified);
        assert_eq!(DisputeDecision::Voided.kind(), K::Voided);
        // disputed / abandoned は裁定の結果にならない（前者は解決していない、
        // 後者は試合が始まる前の終わり方）
        for decision in DisputeDecision::ALL {
            assert!(
                !matches!(decision.kind(), K::Disputed | K::Abandoned),
                "{decision:?}"
            );
        }
    }

    /// 選択肢の値と表示が 1 対 1 で、往復できる。
    #[test]
    fn every_choice_round_trips_through_the_form() {
        for decision in DisputeDecision::ALL {
            assert_eq!(parse_decision(decision.as_str()), Some(decision));
            assert!(!decision.label().is_empty());
        }
        assert_eq!(DisputeDecision::ALL.len(), 4);
    }
}
