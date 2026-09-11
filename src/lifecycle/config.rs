//! 設定値（タイムアウト、候補上限）と、それを読む純粋関数。

use std::env;
use std::time::Duration;

use chrono::TimeDelta;

use super::ids::Timestamp;
use super::state::FormationState;

/// ホスト候補の承諾期限の環境変数。**当日の校正対象。**
pub const HOST_ACCEPT_TIMEOUT_SECONDS: &str = "HOST_ACCEPT_TIMEOUT_SECONDS";
/// 打診するホスト候補の上限の環境変数。**当日の校正対象。**
pub const HOST_CANDIDATE_LIMIT: &str = "HOST_CANDIDATE_LIMIT";
/// 報告の受付時間（分）の環境変数。**当日の校正対象**（短すぎたらここで戻す）。
pub const REPORT_WINDOW_MINUTES: &str = "REPORT_WINDOW_MINUTES";
/// 早期確定に必要な一致人数の環境変数。**当日の校正対象。**
pub const REPORT_MAJORITY_THRESHOLD: &str = "REPORT_MAJORITY_THRESHOLD";

/// 既定の承諾期限（秒）。
///
/// 仕様 §1 は 90 秒だが、これは「Discord のボタンを押すだけ」を前提にした値だった。
/// 打診はサイトへ移り、候補はマッチ画面を開いて操作する（画面を見ていなければ
/// Discord の通知に気づいてから開く）ので、**180 秒**に伸ばしてある。
pub const DEFAULT_HOST_ACCEPT_TIMEOUT_SECONDS: u64 = 180;
/// 既定の候補上限（仕様 §1「上位3人まで」）。
pub const DEFAULT_HOST_CANDIDATE_LIMIT: u32 = 3;
/// 既定の報告受付時間（分）。
///
/// 30 分から **15 分**へ縮めてある。長いほど「報告しない1人」に9人が待たされる時間が
/// 伸びる（レートが動くのは確定してから）。**確定条件そのものは変えていない** —
/// `unanimous && (all_reported || report_window_elapsed)` のままで、window の長さだけ。
pub const DEFAULT_REPORT_WINDOW_MINUTES: u64 = 15;

/// 既定の早期確定の閾値（人）。
///
/// 同じ結果に 7 人以上が一致したら、残りを待たずに確定する。**従来の条件は残してある**
/// ——これは経路の追加であって、全員一致の判定を置き換えるものではない。
///
/// 7 にした根拠は本番の実測である。2026-09-09 に係争になった 4 件は
/// 8対2 / 9対1 / 9対1 / 9対1 で、いずれも 1〜2 人が「無効試合」を選んだために
/// 全員一致が崩れ、15 分待たされたうえで係争になっていた。7 人ならこの 4 件すべてが
/// 即確定する。
pub const DEFAULT_REPORT_MAJORITY_THRESHOLD: usize = 7;

/// 早期確定の閾値の下限。**過半数（10 人中 6 人）。**
///
/// 5 以下を許すと、10 人しかいないので**2 つの結果が同時に閾値へ届く**
/// （5 対 5 なら赤も青も 5 票）。そのとき `leading_outcome` は先に見つけた方を
/// 返すので、同じ報告でも並び順で結論が変わる。6 以上なら 6 × 2 = 12 > 10 で、
/// 2 つが同時に届くことはない。
const MIN_REPORT_MAJORITY_THRESHOLD: usize = crate::matchmaker::MATCH_SIZE / 2 + 1;
/// 早期確定の閾値の上限。試合の人数（10 人）を超える閾値は決して満たせない。
const MAX_REPORT_MAJORITY_THRESHOLD: usize = 10;

/// 設定の読み込みエラー。値そのものは秘密ではないので理由に載せる。
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum LifecycleConfigError {
    /// 値の形が不正。
    #[error("invalid {name}: {reason}")]
    Invalid {
        /// 環境変数名。
        name: &'static str,
        /// 理由。
        reason: String,
    },
}

/// ライフサイクルの設定値。仕様 §1 のタイムアウトと候補上限。
///
/// コードに定数として埋め込まず、ここから読む。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LifecycleConfig {
    /// `awaiting_host`: ホスト候補の承諾を待つ時間。過ぎたら `host_timeout` → 次の候補へ。
    pub host_accept_timeout: Duration,
    /// `awaiting_code`: ホストが部屋コードを貼るのを待つ時間。過ぎたら `host_timeout` → 次の候補へ。
    pub code_post_timeout: Duration,
    /// `in_progress`: 報告ゼロのまま待つ時間。過ぎたら `voided`（報告ゲートは立てない）。
    pub no_report_timeout: Duration,
    /// `in_progress`: 最初の報告からの受付時間。期限後の確定・係争評価は Phase 6 が行う。
    pub report_window: Duration,
    /// 打診するホスト候補の上限。尽きたら `abandoned`。
    pub max_host_candidates: u32,
    /// `in_progress`: 早期確定に必要な一致人数。同じ結果がこの人数に達したら即確定する。
    pub report_majority_threshold: usize,
}

impl Default for LifecycleConfig {
    /// 180秒 / 180秒 / 45分 / 15分 / 上位3人 / 一致7人。
    fn default() -> Self {
        Self {
            host_accept_timeout: Duration::from_secs(DEFAULT_HOST_ACCEPT_TIMEOUT_SECONDS),
            code_post_timeout: Duration::from_secs(180),
            no_report_timeout: Duration::from_secs(45 * 60),
            report_window: Duration::from_secs(DEFAULT_REPORT_WINDOW_MINUTES * 60),
            max_host_candidates: DEFAULT_HOST_CANDIDATE_LIMIT,
            report_majority_threshold: DEFAULT_REPORT_MAJORITY_THRESHOLD,
        }
    }
}

impl LifecycleConfig {
    /// 環境変数から読む。未設定なら既定値。
    pub fn from_env() -> Result<Self, LifecycleConfigError> {
        Self::parse(
            env::var(HOST_ACCEPT_TIMEOUT_SECONDS).ok().as_deref(),
            env::var(HOST_CANDIDATE_LIMIT).ok().as_deref(),
            env::var(REPORT_WINDOW_MINUTES).ok().as_deref(),
            env::var(REPORT_MAJORITY_THRESHOLD).ok().as_deref(),
        )
    }

    /// 設定値の解釈。**純粋関数**（`#![forbid(unsafe_code)]` の下ではテストから
    /// 環境変数を書き換えられないので、読み取りと解釈を分けてある）。
    ///
    /// いずれも 0 は拒否する。候補 0 人は「成立した瞬間に必ず解散」、承諾 0 秒は
    /// 「打診した瞬間にタイムアウト」であり、設定ミスが黙って動く形になるため。
    pub fn parse(
        host_accept_timeout_seconds: Option<&str>,
        host_candidate_limit: Option<&str>,
        report_window_minutes: Option<&str>,
        report_majority_threshold: Option<&str>,
    ) -> Result<Self, LifecycleConfigError> {
        let defaults = Self::default();
        let host_accept_timeout = match trimmed(host_accept_timeout_seconds) {
            None => defaults.host_accept_timeout,
            Some(raw) => {
                Duration::from_secs(positive(HOST_ACCEPT_TIMEOUT_SECONDS, raw, "seconds")?)
            }
        };
        let max_host_candidates = match trimmed(host_candidate_limit) {
            None => defaults.max_host_candidates,
            Some(raw) => u32::try_from(positive(HOST_CANDIDATE_LIMIT, raw, "candidates")?)
                .map_err(|_| LifecycleConfigError::Invalid {
                    name: HOST_CANDIDATE_LIMIT,
                    reason: format!("{raw:?} is out of range"),
                })?,
        };
        // 0 分は「最初の報告と同時に期限切れ」= 1人の報告で確定/係争になる。
        // 確定条件を変えないための下限として、ここも 0 を拒否する
        let report_window = match trimmed(report_window_minutes) {
            None => defaults.report_window,
            Some(raw) => Duration::from_secs(positive(REPORT_WINDOW_MINUTES, raw, "minutes")? * 60),
        };
        // 閾値は 2〜10 人。1 人だと最初の報告で確定してしまい、11 人以上は決して満たせない
        // （どちらも「設定したつもりの挙動」にならないので起動で落とす）
        let report_majority_threshold = match trimmed(report_majority_threshold) {
            None => defaults.report_majority_threshold,
            Some(raw) => {
                let value = usize::try_from(positive(REPORT_MAJORITY_THRESHOLD, raw, "players")?)
                    .map_err(|_| LifecycleConfigError::Invalid {
                    name: REPORT_MAJORITY_THRESHOLD,
                    reason: format!("{raw:?} is out of range"),
                })?;
                if !(MIN_REPORT_MAJORITY_THRESHOLD..=MAX_REPORT_MAJORITY_THRESHOLD).contains(&value)
                {
                    return Err(LifecycleConfigError::Invalid {
                        name: REPORT_MAJORITY_THRESHOLD,
                        reason: format!(
                            "must be between {MIN_REPORT_MAJORITY_THRESHOLD} and {MAX_REPORT_MAJORITY_THRESHOLD} players, got {value}"
                        ),
                    });
                }
                value
            }
        };
        Ok(Self {
            host_accept_timeout,
            max_host_candidates,
            report_window,
            report_majority_threshold,
            ..defaults
        })
    }
}

fn trimmed(value: Option<&str>) -> Option<&str> {
    value.map(str::trim).filter(|value| !value.is_empty())
}

fn positive(name: &'static str, raw: &str, unit: &str) -> Result<u64, LifecycleConfigError> {
    let value: u64 = raw.parse().map_err(|_| LifecycleConfigError::Invalid {
        name,
        reason: format!("expected a positive number of {unit}, got {raw:?}"),
    })?;
    if value == 0 {
        return Err(LifecycleConfigError::Invalid {
            name,
            reason: format!("must be at least 1 {}", unit.trim_end_matches('s')),
        });
    }
    Ok(value)
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
    /// 報告受付の期限。過ぎた後の確定・係争評価は Phase 6 が行う。
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
        | S::Nullified
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
    fn unset_environment_keeps_the_defaults() {
        assert_eq!(
            LifecycleConfig::parse(None, None, None, None).unwrap(),
            LifecycleConfig::default()
        );
        // 空文字・空白だけも「未設定」と同じ（.env に空行を残しても既定に戻る）
        assert_eq!(
            LifecycleConfig::parse(Some(""), Some("  "), Some(""), Some(" ")).unwrap(),
            LifecycleConfig::default()
        );
    }

    #[test]
    fn the_host_timeout_and_candidate_limit_come_from_the_environment() {
        let config = LifecycleConfig::parse(Some("45"), Some("5"), None, None).unwrap();
        assert_eq!(config.host_accept_timeout, Duration::from_secs(45));
        assert_eq!(config.max_host_candidates, 5);
        // 他の期限は動かない
        let defaults = LifecycleConfig::default();
        assert_eq!(config.code_post_timeout, defaults.code_post_timeout);
        assert_eq!(config.no_report_timeout, defaults.no_report_timeout);
        assert_eq!(config.report_window, defaults.report_window);

        // 片方だけ設定してもよい
        let only_limit = LifecycleConfig::parse(None, Some("1"), None, None).unwrap();
        assert_eq!(only_limit.host_accept_timeout, defaults.host_accept_timeout);
        assert_eq!(only_limit.max_host_candidates, 1);
    }

    #[test]
    fn a_meaningless_setting_fails_at_startup() {
        // 0 秒は「打診した瞬間にタイムアウト」、0 人は「成立した瞬間に解散」、
        // 0 分は「最初の報告と同時に期限切れ」＝ 1 人の報告で確定してしまう。
        // 閾値 1 人も同じ（最初の報告で確定する）、11 人以上は決して満たせない。
        // どれも黙って動かず起動で落とす
        for (timeout, limit, window, threshold) in [
            (Some("0"), None, None, None),
            (None, Some("0"), None, None),
            (None, None, Some("0"), None),
            (Some("-1"), None, None, None),
            (Some("soon"), None, None, None),
            (None, Some("たくさん"), None, None),
            (None, Some("4294967296"), None, None),
            (None, None, Some("しばらく"), None),
            (None, None, Some("-5"), None),
            (None, None, None, Some("0")),
            (None, None, None, Some("1")),
            (None, None, None, Some("11")),
            (None, None, None, Some("100")),
            (None, None, None, Some("-3")),
            (None, None, None, Some("ほとんど")),
        ] {
            assert!(
                LifecycleConfig::parse(timeout, limit, window, threshold).is_err(),
                "{timeout:?} / {limit:?} / {window:?} / {threshold:?} must be rejected"
            );
        }
        let error = LifecycleConfig::parse(Some("0"), None, None, None).unwrap_err();
        assert!(error.to_string().contains(HOST_ACCEPT_TIMEOUT_SECONDS));
        let error = LifecycleConfig::parse(None, None, Some("0"), None).unwrap_err();
        assert!(error.to_string().contains(REPORT_WINDOW_MINUTES));
        let error = LifecycleConfig::parse(None, None, None, Some("11")).unwrap_err();
        assert!(error.to_string().contains(REPORT_MAJORITY_THRESHOLD));
    }

    /// 早期確定の閾値は環境変数で変えられる。**当日の校正対象。**
    ///
    /// **下限は過半数（6）。**5 以下は 5 対 5 で 2 つの結果が同時に届き、
    /// 結論が報告の並び順に依存してしまうので設定させない。
    #[test]
    fn the_majority_threshold_comes_from_the_environment() {
        for raw in ["6", "7", "8", "10"] {
            let config = LifecycleConfig::parse(None, None, None, Some(raw)).unwrap();
            assert_eq!(
                config.report_majority_threshold,
                raw.parse::<usize>().unwrap()
            );
            // 他の設定は動かない
            let defaults = LifecycleConfig::default();
            assert_eq!(config.report_window, defaults.report_window);
            assert_eq!(config.host_accept_timeout, defaults.host_accept_timeout);
            assert_eq!(config.max_host_candidates, defaults.max_host_candidates);
        }
    }

    /// **過半数に満たない閾値は設定できない。**
    ///
    /// 10 人しかいないので、5 以下だと 5 対 5 で赤も青も閾値に届く。
    /// どちらを採るかが報告の並び順で決まってしまうため、設定の時点で断る。
    #[test]
    fn a_threshold_below_a_majority_is_refused() {
        for raw in ["1", "2", "3", "4", "5"] {
            let error = LifecycleConfig::parse(None, None, None, Some(raw)).unwrap_err();
            assert!(
                error.to_string().contains(REPORT_MAJORITY_THRESHOLD),
                "{raw} が通ってしまう"
            );
        }
        // 6（＝10 人の過半数）から通る
        assert_eq!(
            LifecycleConfig::parse(None, None, None, Some("6"))
                .unwrap()
                .report_majority_threshold,
            6
        );
    }

    /// 報告の受付時間は環境変数で変えられる。**当日に戻せること**が目的。
    #[test]
    fn the_report_window_comes_from_the_environment() {
        let config = LifecycleConfig::parse(None, None, Some("30"), None).unwrap();
        assert_eq!(config.report_window, Duration::from_secs(30 * 60));
        // 他の期限は動かない
        let defaults = LifecycleConfig::default();
        assert_eq!(config.host_accept_timeout, defaults.host_accept_timeout);
        assert_eq!(config.code_post_timeout, defaults.code_post_timeout);
        assert_eq!(config.no_report_timeout, defaults.no_report_timeout);
        assert_eq!(config.max_host_candidates, defaults.max_host_candidates);
    }

    #[test]
    fn defaults_match_the_spec() {
        let config = LifecycleConfig::default();
        assert_eq!(config.host_accept_timeout, Duration::from_secs(180));
        assert_eq!(config.code_post_timeout, Duration::from_secs(180));
        assert_eq!(config.no_report_timeout, Duration::from_secs(45 * 60));
        assert_eq!(config.report_window, Duration::from_secs(15 * 60));
        assert_eq!(config.max_host_candidates, 3);
        assert_eq!(config.report_majority_threshold, 7);
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
            report_majority_threshold: DEFAULT_REPORT_MAJORITY_THRESHOLD,
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
            S::Nullified,
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

    /// **キュー画面からマッチ画面への遷移の遅れが、打診の 180 秒を食わないこと。**
    ///
    /// キュー画面は10秒ポーリングなので、成立から最大10秒遅れてマッチ画面へ移る。
    /// 承諾期限は `host_assigned` を起点に測るので、遷移の遅れは期限の**外側**にある。
    /// ここではホストが最悪ケース（10秒）で画面に着いたときの残り時間を出す。
    #[test]
    fn a_ten_second_transition_does_not_eat_the_host_window() {
        let config = LifecycleConfig::default();
        let assigned_at = t0();
        let (deadline, at) = next_deadline(
            &FormationState::AwaitingHost {
                pending: Some(super::super::PlayerId(uuid::Uuid::nil())),
                tried: 1,
            },
            assigned_at,
            None,
            &config,
        )
        .expect("a host accept deadline");
        assert_eq!(deadline, Deadline::HostAccept);
        // 期限は host_assigned から 180 秒
        assert_eq!(at, assigned_at + TimeDelta::seconds(180));

        // キュー画面のポーリング間隔（最悪ケース）
        let polling_delay = TimeDelta::seconds(10);
        let arrives_at = assigned_at + polling_delay;
        let remaining = at - arrives_at;
        assert_eq!(remaining, TimeDelta::seconds(170));
        assert!(
            remaining >= TimeDelta::seconds(120),
            "遷移の遅れが打診の窓を食っている: 残り {remaining}"
        );
        println!(
            "打診の窓 {}秒 / 遷移の最悪遅れ {}秒 / 画面に着いた時点の残り {}秒",
            (at - assigned_at).num_seconds(),
            polling_delay.num_seconds(),
            remaining.num_seconds()
        );
    }
}
