//! 無効試合の通報の受理結果と拒否理由。
//!
//! **票数も閾値もこの型に入っていない。**外へ出す形（HTTP のレスポンス）は
//! この型からしか作らないので、うっかり載せる余地が無い
//! （`quiet::model` と同じ作り方）。

/// 通報を受け付けた結果。**票数は返さない。**
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NullifiedReportOutcome {
    /// 受け付けた。
    Recorded,
    /// **既にこの試合で 1 票入れている**（no-op）。
    ///
    /// 「原因は誰か」は 1 つの答えなので選び直せない。二度押しても増えないし、
    /// 別の相手に付け替えることもできない。
    AlreadyReported,
}

/// 受け付けられない理由。
///
/// **どれも票数・閾値を含まない。**画面に出す文言も、そこから逆算できない形にしてある。
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum NullifiedReportError {
    /// この試合で「無効試合」を報告していない。
    ///
    /// 通報できるのは無効試合を報告した人だけ。勝敗を報告した人にも、
    /// まだ報告していない人にも入口を出さない。
    #[error("the reporter did not report this match as nullified")]
    NotANullifiedReporter,
    /// 対象が同じ formation にいない。
    #[error("the target is not in this match")]
    NotAParticipant,
    /// 自分自身は対象にできない。
    #[error("a player cannot report themselves")]
    Self_,
    /// DB エラー。
    #[error("database error")]
    Database,
}

impl NullifiedReportError {
    /// 機械可読な理由。
    pub fn code(self) -> &'static str {
        match self {
            Self::NotANullifiedReporter => "not_a_nullified_reporter",
            Self::NotAParticipant => "not_a_participant",
            Self::Self_ => "self_report",
            Self::Database => "internal_error",
        }
    }

    /// 画面に出す説明。**票数にも閾値にも触れない。**
    pub fn message(self) -> &'static str {
        match self {
            Self::NotANullifiedReporter => "この試合を無効試合として報告した人だけが通報できます。",
            Self::NotAParticipant => "この試合の参加者を選んでください。",
            Self::Self_ => "自分自身は選べません。",
            Self::Database => "受け付けられませんでした。時間をおいて試してください。",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// **どの文言にも票数・閾値が出てこない。**
    ///
    /// 「あと何票で成立するか」を逆算させないため、エラーの理由にも数字を入れない。
    #[test]
    fn no_message_leaks_a_vote_count() {
        for error in [
            NullifiedReportError::NotANullifiedReporter,
            NullifiedReportError::NotAParticipant,
            NullifiedReportError::Self_,
            NullifiedReportError::Database,
        ] {
            let text = format!("{} {}", error.code(), error.message());
            assert!(
                !text.chars().any(|c| c.is_ascii_digit()),
                "数字が漏れている: {text}"
            );
            assert!(!text.contains("票"), "票数に触れている: {text}");
        }
    }

    /// 理由ごとに別のコードが付く（画面が取り違えない）。
    #[test]
    fn every_reason_has_its_own_code() {
        let all = [
            NullifiedReportError::NotANullifiedReporter,
            NullifiedReportError::NotAParticipant,
            NullifiedReportError::Self_,
            NullifiedReportError::Database,
        ];
        let mut codes: Vec<&str> = all.iter().map(|error| error.code()).collect();
        codes.sort_unstable();
        codes.dedup();
        assert_eq!(codes.len(), all.len());
    }
}
