//! 聞き専報告の閾値。**環境変数が正。**
//!
//! レートの合算は `player_current_ratings` 1 か所に集約してあるので、判定に使う閾値も
//! SQL から見えなければならない。そこで起動時にこの値を `quiet_report_param_changes` へ
//! 同期する（[`crate::quiet::repository::QuietReportRepository::sync_thresholds`]）。
//! **環境変数を変えて再起動すれば、過去の試合も新しい閾値で再判定される**
//! （docs/02 不変条件 3。追記なので変更履歴も残る）。

use std::env;

/// 勝ったチームからの報告で必要な票数の環境変数。**当日の校正対象。**
pub const QUIET_REPORT_WINNER_THRESHOLD: &str = "QUIET_REPORT_WINNER_THRESHOLD";
/// 負けたチームからの報告で必要な票数の環境変数。**当日の校正対象。**
pub const QUIET_REPORT_LOSER_THRESHOLD: &str = "QUIET_REPORT_LOSER_THRESHOLD";

/// 既定の勝ち側の閾値。
///
/// 勝ちチームは 5 人で、対象者を除くと報告できるのは 4 人。3 票は「4 人中 3 人」で、
/// 1 人の反対を許容する。
pub const DEFAULT_WINNER_THRESHOLD: i32 = 3;
/// 既定の負け側の閾値。
///
/// 負けチームも 5 人で、対象者を除くと報告できるのは 4 人。したがって
/// **4 票は満場一致**である。負け直後の報告が「戦犯選出」になることを避けるため、
/// 負け側は全員一致を要求する。成立しにくいのは織り込み済みで、
/// **成立しなくても報告の記録は残る**（運営が履歴で個別に対応する）。
pub const DEFAULT_LOSER_THRESHOLD: i32 = 4;

/// 閾値の下限。0 票で成立してしまうと報告が要らなくなる。
const MIN_THRESHOLD: i32 = 1;
/// 閾値の上限。対象者を除いた同チームの人数（5 - 1 = 4）。
/// これを超えると誰も到達できない。
const MAX_THRESHOLD: i32 = 4;

/// 設定の読み込みエラー。値そのものは秘密ではないので理由に載せる。
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum QuietReportConfigError {
    /// 値の形が不正。
    #[error("invalid {name}: {reason}")]
    Invalid {
        /// 環境変数名。
        name: &'static str,
        /// 理由。
        reason: String,
    },
}

/// 聞き専報告の閾値。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct QuietReportConfig {
    /// 対象者のチームが**勝った**試合で必要な票数。
    pub winner_threshold: i32,
    /// 対象者のチームが**負けた**試合で必要な票数。
    pub loser_threshold: i32,
}

impl Default for QuietReportConfig {
    fn default() -> Self {
        Self {
            winner_threshold: DEFAULT_WINNER_THRESHOLD,
            loser_threshold: DEFAULT_LOSER_THRESHOLD,
        }
    }
}

impl QuietReportConfig {
    /// 環境変数から読む。未設定なら既定値。
    pub fn from_env() -> Result<Self, QuietReportConfigError> {
        Self::parse(
            env::var(QUIET_REPORT_WINNER_THRESHOLD).ok().as_deref(),
            env::var(QUIET_REPORT_LOSER_THRESHOLD).ok().as_deref(),
        )
    }

    /// 設定値の解釈。**純粋関数**（読み取りと解釈を分けてある）。
    ///
    /// **1 以上 4 以下だけを通す。**0 は「報告ゼロで成立」、5 以上は
    /// 「対象者を除くと 4 人しかいないので到達できない」で、どちらも
    /// 設定したつもりの挙動にならない。
    pub fn parse(
        winner: Option<&str>,
        loser: Option<&str>,
    ) -> Result<Self, QuietReportConfigError> {
        let defaults = Self::default();
        Ok(Self {
            winner_threshold: match trimmed(winner) {
                None => defaults.winner_threshold,
                Some(raw) => threshold(QUIET_REPORT_WINNER_THRESHOLD, raw)?,
            },
            loser_threshold: match trimmed(loser) {
                None => defaults.loser_threshold,
                Some(raw) => threshold(QUIET_REPORT_LOSER_THRESHOLD, raw)?,
            },
        })
    }
}

fn trimmed(value: Option<&str>) -> Option<&str> {
    value.map(str::trim).filter(|value| !value.is_empty())
}

fn threshold(name: &'static str, raw: &str) -> Result<i32, QuietReportConfigError> {
    let value: i32 = raw.parse().map_err(|_| QuietReportConfigError::Invalid {
        name,
        reason: format!("expected a number of votes, got {raw:?}"),
    })?;
    if !(MIN_THRESHOLD..=MAX_THRESHOLD).contains(&value) {
        return Err(QuietReportConfigError::Invalid {
            name,
            reason: format!("must be between {MIN_THRESHOLD} and {MAX_THRESHOLD}, got {value}"),
        });
    }
    Ok(value)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unset_environment_keeps_the_defaults() {
        assert_eq!(
            QuietReportConfig::parse(None, None).unwrap(),
            QuietReportConfig {
                winner_threshold: 3,
                loser_threshold: 4,
            }
        );
        // 空文字・空白だけも「未設定」と同じ
        assert_eq!(
            QuietReportConfig::parse(Some(""), Some("  ")).unwrap(),
            QuietReportConfig::default()
        );
    }

    #[test]
    fn the_thresholds_come_from_the_environment() {
        let config = QuietReportConfig::parse(Some("2"), Some("3")).unwrap();
        assert_eq!(config.winner_threshold, 2);
        assert_eq!(config.loser_threshold, 3);
        // 片方だけでもよい
        let only_loser = QuietReportConfig::parse(None, Some("1")).unwrap();
        assert_eq!(only_loser.winner_threshold, 3);
        assert_eq!(only_loser.loser_threshold, 1);
    }

    /// **1 以上 4 以下だけ。**0 は報告ゼロで成立、5 以上は到達できない。
    #[test]
    fn a_meaningless_threshold_fails_at_startup() {
        for (winner, loser) in [
            (Some("0"), None),
            (Some("5"), None),
            (Some("-1"), None),
            (Some("たくさん"), None),
            (None, Some("0")),
            (None, Some("5")),
            (None, Some("10")),
            (None, Some("")),
        ] {
            // 空文字は「未設定」扱いなので、そこだけは通る
            let parsed = QuietReportConfig::parse(winner, loser);
            if loser == Some("") {
                assert!(parsed.is_ok());
                continue;
            }
            assert!(parsed.is_err(), "{winner:?} / {loser:?} must be rejected");
        }
        let error = QuietReportConfig::parse(Some("5"), None).unwrap_err();
        assert!(error.to_string().contains(QUIET_REPORT_WINNER_THRESHOLD));
        let error = QuietReportConfig::parse(None, Some("0")).unwrap_err();
        assert!(error.to_string().contains(QUIET_REPORT_LOSER_THRESHOLD));
    }

    /// **負け側の 4 は満場一致にあたる。**5 人から対象者を除くと 4 人しか報告できない。
    #[test]
    fn the_losing_threshold_is_unanimous_among_those_who_can_report() {
        let team_size = crate::matchmaker::TEAM_SIZE;
        let eligible = team_size - 1;
        assert_eq!(eligible, 4, "チームは 5 人（対象者を除くと 4 人）");
        assert_eq!(
            QuietReportConfig::default().loser_threshold as usize,
            eligible,
            "負け側の既定値は満場一致"
        );
        assert!(
            (QuietReportConfig::default().winner_threshold as usize) < eligible,
            "勝ち側は 1 人の反対を許容する"
        );
        assert_eq!(MAX_THRESHOLD as usize, eligible);
    }
}
