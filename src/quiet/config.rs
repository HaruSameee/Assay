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
/// 既定の負け側の閾値。**勝ち側と同じ 3。**
///
/// 以前は 4（対象者を除いた 4 人の満場一致）にしてあった。負け直後の報告が
/// 「戦犯選出」になるのを防ぐ意図だったが、実際には**負け側で成立しない**という
/// 別の偏りを作っていた。聞き専は勝っても負けても同じ迷惑なのに、
/// 負けた試合でだけ 1 人の反対で潰せるのは、抑止としても記録としても筋が悪い。
///
/// 戦犯選出への歯止めは、閾値の非対称ではなく
/// **下限を 3 に引き上げたこと**（[`MIN_THRESHOLD`]）と、
/// ペナルティを −1 に留めていること（1 試合の勝敗 ±7 の 1/7）で持たせる。
pub const DEFAULT_LOSER_THRESHOLD: i32 = 3;

/// 閾値の下限。**3。**
///
/// 報告できるのは対象者を除いた同チームの 4 人なので、3 票は「4 人中 3 人」。
/// これより下げると 2 人の口裏合わせで成立してしまい、聞き専の判定ではなく
/// 私怨の道具になる。0 票で成立すると報告そのものが要らなくなる。
const MIN_THRESHOLD: i32 = 3;
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

    /// 既定は**勝ち負けとも 3**。
    #[test]
    fn unset_environment_keeps_the_defaults() {
        assert_eq!(
            QuietReportConfig::parse(None, None).unwrap(),
            QuietReportConfig {
                winner_threshold: 3,
                loser_threshold: 3,
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
        let config = QuietReportConfig::parse(Some("4"), Some("3")).unwrap();
        assert_eq!(config.winner_threshold, 4);
        assert_eq!(config.loser_threshold, 3);
        // 片方だけでもよい
        let only_loser = QuietReportConfig::parse(None, Some("4")).unwrap();
        assert_eq!(only_loser.winner_threshold, 3);
        assert_eq!(only_loser.loser_threshold, 4);
    }

    /// **3 以上 4 以下だけ。**2 以下は 2 人の口裏合わせで成立し、5 以上は到達できない。
    #[test]
    fn a_meaningless_threshold_fails_at_startup() {
        for (winner, loser) in [
            (Some("0"), None),
            (Some("1"), None),
            (Some("2"), None),
            (Some("5"), None),
            (Some("-1"), None),
            (Some("たくさん"), None),
            (None, Some("0")),
            (None, Some("2")),
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
        let error = QuietReportConfig::parse(None, Some("2")).unwrap_err();
        assert!(error.to_string().contains(QUIET_REPORT_LOSER_THRESHOLD));
    }

    /// **勝ちも負けも同じ票数を要求する。**
    ///
    /// 以前は負け側だけ 4（満場一致）にしてあった。負け直後の「戦犯選出」を
    /// 防ぐ意図だったが、負け側でだけ 1 人の反対で潰せる状態になっていた。
    /// 歯止めは下限 3 の側に移してある。
    #[test]
    fn both_sides_need_the_same_number_of_votes() {
        let defaults = QuietReportConfig::default();
        assert_eq!(defaults.winner_threshold, defaults.loser_threshold);
        assert_eq!(defaults.winner_threshold, MIN_THRESHOLD);

        let team_size = crate::matchmaker::TEAM_SIZE;
        let eligible = team_size - 1;
        assert_eq!(eligible, 4, "チームは 5 人（対象者を除くと 4 人）");
        // 3 票は「4 人中 3 人」。1 人の反対は許容するが、2 人では成立しない
        assert!((defaults.winner_threshold as usize) < eligible);
        assert!(
            defaults.winner_threshold >= 3,
            "2 人の口裏合わせで成立しない"
        );
        assert_eq!(MAX_THRESHOLD as usize, eligible);
    }
}
