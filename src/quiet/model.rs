//! 聞き専報告の受理結果と拒否理由。
//!
//! **票数も閾値もこの型に入っていない。**外へ出す形（HTTP のレスポンス）は
//! この型からしか作らないので、うっかり載せる余地が無い。

/// 報告を受け付けた結果。**票数は返さない。**
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QuietReportOutcome {
    /// 受け付けた。
    Recorded,
    /// 同じ組み合わせが既にある（no-op）。二度押しても増えない。
    AlreadyReported,
}

/// 受け付けられない理由。
///
/// **どれも票数・閾値を含まない。**画面に出す文言も、そこから逆算できない形にしてある。
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum QuietReportError {
    /// まだ結果が確定していない。**確定前は受け付けない**（勝敗で閾値が変わるため）。
    #[error("the match is not confirmed yet")]
    NotConfirmed,
    /// 無効試合・不成立・係争中。勝敗が無いので閾値が決まらない。
    #[error("this match has no winner")]
    NoWinner,
    /// 対象が同じ formation にいない。
    #[error("the target is not in this match")]
    NotAParticipant,
    /// 対象が相手チーム。**自チームのメンバーだけが対象。**
    #[error("the target is on the opposing team")]
    OpposingTeam,
    /// 自分自身は対象にできない。
    #[error("a player cannot report themselves")]
    Self_,
    /// DB エラー。
    #[error("database error")]
    Database,
}

impl QuietReportError {
    /// 機械可読な理由。
    pub fn code(self) -> &'static str {
        match self {
            Self::NotConfirmed => "not_confirmed",
            Self::NoWinner => "no_winner",
            Self::NotAParticipant => "not_a_participant",
            Self::OpposingTeam => "opposing_team",
            Self::Self_ => "self_report",
            Self::Database => "internal_error",
        }
    }

    /// 画面に出す説明。**票数にも閾値にも触れない。**
    pub fn message(self) -> &'static str {
        match self {
            Self::NotConfirmed => "結果が確定してから報告してください。",
            Self::NoWinner => "この試合は勝敗がついていないため、報告を受け付けません。",
            // 相手チーム・非参加者は同じ文言にする。
            // 分けると「その人がこの試合にいるか」が分かってしまう
            Self::NotAParticipant | Self::OpposingTeam => {
                "この試合の味方チームのメンバーだけを報告できます。"
            }
            Self::Self_ => "自分自身は報告できません。",
            Self::Database => "処理に失敗しました。時間をおいてやり直してください。",
        }
    }
}
