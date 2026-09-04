//! 設定値（タイムアウト、候補上限）と、それを読む純粋関数。

use std::time::Duration;

use chrono::TimeDelta;

use super::ids::Timestamp;
use super::state::FormationState;

/// ライフサイクルの設定値。仕様 §1 のタイムアウトと候補上限。
///
/// コードに定数として埋め込まず、ここから読む。既定値は仕様 §1 の値。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LifecycleConfig {
    /// `awaiting_host`: ホスト候補の承諾を待つ時間。過ぎたら `host_timeout` → 次の候補へ。
    pub host_accept_timeout: Duration,
    /// `awaiting_code`: ホストが部屋コードを貼るのを待つ時間。過ぎたら `host_timeout` → 次の候補へ。
    pub code_post_timeout: Duration,
    /// `in_progress`: 報告ゼロのまま待つ時間。過ぎたら `voided`（報告ゲートは立てない）。
    pub no_report_timeout: Duration,
    /// `in_progress`: 最初の報告からの受付時間。過ぎたら到着分に確定ルールを適用する。
    pub report_window: Duration,
    /// 打診するホスト候補の上限。尽きたら `abandoned`。
    pub max_host_candidates: u32,
}

impl Default for LifecycleConfig {
    /// 仕様 §1 の値: 90秒 / 180秒 / 45分 / 30分 / 上位3人。
    fn default() -> Self {
        Self {
            host_accept_timeout: Duration::from_secs(90),
            code_post_timeout: Duration::from_secs(180),
            no_report_timeout: Duration::from_secs(45 * 60),
            report_window: Duration::from_secs(30 * 60),
            max_host_candidates: 3,
        }
    }
}

/// 状態ごとの期限の種類。期限が来たときに取るべき行動を表す。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Deadline {
    /// ホスト候補の承諾期限。過ぎたら `host_timeout`（actor = 候補）を積む。
    HostAccept,
    /// 部屋コードの投稿期限。過ぎたら `host_timeout`（actor = ホスト）を積む。
    CodePost,
    /// 報告ゼロの期限。過ぎたら `voided` を積む。
    NoReports,
    /// 報告受付の期限。過ぎたら到着分に確定ルールを適用し `confirmed` / `disputed` を積む。
    ReportWindow,
}

/// 現在の状態に適用される期限と、その時刻を返す。
///
/// - `since`: 現在の状態に入った時刻（[`super::derive_snapshot`] の `since`。無ければ `formations.created_at`）。
/// - `first_report_at`: この formation への最初の報告時刻（`match_reports` から。無ければ `None`）。
///
/// 期限が無い状態（`matched`、候補を指名する前、終端、係争中）では `None`。
/// 設定値が時刻に加算できないほど大きい場合も `None`（panic しない）。
pub fn next_deadline(
    state: &FormationState,
    since: Timestamp,
    first_report_at: Option<Timestamp>,
    config: &LifecycleConfig,
) -> Option<(Deadline, Timestamp)> {
    use FormationState as S;

    let (deadline, base, wait) = match state {
        S::AwaitingHost {
            pending: Some(_), ..
        } => (Deadline::HostAccept, since, config.host_accept_timeout),
        S::AwaitingCode { .. } => (Deadline::CodePost, since, config.code_post_timeout),
        S::InProgress { .. } => match first_report_at {
            None => (Deadline::NoReports, since, config.no_report_timeout),
            Some(first) => (Deadline::ReportWindow, first, config.report_window),
        },
        S::Matched
        | S::AwaitingHost { pending: None, .. }
        | S::Confirmed
        | S::Disputed
        | S::Voided
        | S::Abandoned => return None,
    };
    let at = base.checked_add_signed(TimeDelta::from_std(wait).ok()?)?;
    Some((deadline, at))
}

/// ホスト指名フェーズで次に取る行動。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HostAction {
    /// [`super::HostSelector::candidates`] の `candidate_index` 番目を `host_assigned` で打診する。
    /// 候補リストがそれより短ければ [`HostAction::Abandon`] と同じ扱いにする。
    Assign {
        /// 候補リスト上の添字（0 始まり）。
        candidate_index: usize,
    },
    /// 候補が尽きた。`abandoned` を積み、10人をキューに戻す。
    Abandon,
}

/// 候補を指名する前の `awaiting_host` で、次に何をするかを返す。
///
/// それ以外の状態（候補の応答待ち、指名フェーズ以外）では `None`＝何もしない。
pub fn next_host_action(state: &FormationState, config: &LifecycleConfig) -> Option<HostAction> {
    match state {
        FormationState::AwaitingHost {
            pending: None,
            tried,
            ..
        } => Some(if *tried >= config.max_host_candidates {
            HostAction::Abandon
        } else {
            HostAction::Assign {
                candidate_index: *tried as usize,
            }
        }),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use chrono::{TimeZone, Utc};
    use uuid::Uuid;

    use super::FormationState as S;
    use super::*;
    use crate::lifecycle::PlayerId;

    fn pid(n: u128) -> PlayerId {
        PlayerId(Uuid::from_u128(n))
    }

    fn t0() -> Timestamp {
        Utc.with_ymd_and_hms(2026, 9, 12, 21, 0, 0).unwrap()
    }

    #[test]
    fn defaults_match_the_spec() {
        let config = LifecycleConfig::default();
        assert_eq!(config.host_accept_timeout, Duration::from_secs(90));
        assert_eq!(config.code_post_timeout, Duration::from_secs(180));
        assert_eq!(config.no_report_timeout, Duration::from_secs(45 * 60));
        assert_eq!(config.report_window, Duration::from_secs(30 * 60));
        assert_eq!(config.max_host_candidates, 3);
    }

    #[test]
    fn deadlines_are_read_from_config() {
        let a = pid(1);
        let config = LifecycleConfig {
            host_accept_timeout: Duration::from_secs(10),
            code_post_timeout: Duration::from_secs(20),
            no_report_timeout: Duration::from_secs(30),
            report_window: Duration::from_secs(40),
            max_host_candidates: 3,
        };
        let since = t0();
        let pending = S::AwaitingHost {
            pending: Some(a),
            tried: 1,
        };
        assert_eq!(
            next_deadline(&pending, since, None, &config),
            Some((Deadline::HostAccept, since + TimeDelta::seconds(10)))
        );
        let code = S::AwaitingCode { host: a, tried: 1 };
        assert_eq!(
            next_deadline(&code, since, None, &config),
            Some((Deadline::CodePost, since + TimeDelta::seconds(20)))
        );
        let in_progress = S::InProgress {
            host: a,
            room_code: "1234".into(),
        };
        assert_eq!(
            next_deadline(&in_progress, since, None, &config),
            Some((Deadline::NoReports, since + TimeDelta::seconds(30)))
        );
        let first_report = since + TimeDelta::seconds(5);
        assert_eq!(
            next_deadline(&in_progress, since, Some(first_report), &config),
            Some((
                Deadline::ReportWindow,
                first_report + TimeDelta::seconds(40)
            ))
        );
    }

    #[test]
    fn states_without_a_deadline_return_none() {
        let config = LifecycleConfig::default();
        let nominating = S::AwaitingHost {
            pending: None,
            tried: 1,
        };
        for state in [
            S::Matched,
            nominating,
            S::Confirmed,
            S::Disputed,
            S::Voided,
            S::Abandoned,
        ] {
            assert_eq!(
                next_deadline(&state, t0(), None, &config),
                None,
                "{state:?}"
            );
        }
    }

    #[test]
    fn absurd_config_does_not_panic() {
        let a = pid(1);
        let config = LifecycleConfig {
            host_accept_timeout: Duration::MAX,
            ..LifecycleConfig::default()
        };
        let pending = S::AwaitingHost {
            pending: Some(a),
            tried: 1,
        };
        assert_eq!(next_deadline(&pending, t0(), None, &config), None);
    }

    #[test]
    fn host_action_follows_the_candidate_limit() {
        let a = pid(1);
        let config = LifecycleConfig::default();
        let nominating = |tried: u32| S::AwaitingHost {
            pending: None,
            tried,
        };
        assert_eq!(
            next_host_action(&nominating(0), &config),
            Some(HostAction::Assign { candidate_index: 0 })
        );
        assert_eq!(
            next_host_action(&nominating(2), &config),
            Some(HostAction::Assign { candidate_index: 2 })
        );
        assert_eq!(
            next_host_action(&nominating(3), &config),
            Some(HostAction::Abandon)
        );
        let wider = LifecycleConfig {
            max_host_candidates: 5,
            ..config.clone()
        };
        assert_eq!(
            next_host_action(&nominating(3), &wider),
            Some(HostAction::Assign { candidate_index: 3 })
        );
        // 応答待ち中、および指名フェーズ以外では何もしない
        let pending = S::AwaitingHost {
            pending: Some(a),
            tried: 1,
        };
        assert_eq!(next_host_action(&pending, &config), None);
        assert_eq!(next_host_action(&S::Matched, &config), None);
        assert_eq!(
            next_host_action(
                &S::InProgress {
                    host: a,
                    room_code: "1234".into(),
                },
                &config
            ),
            None
        );
        assert_eq!(next_host_action(&S::Abandoned, &config), None);
    }
}
