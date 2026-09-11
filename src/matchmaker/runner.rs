//! 3 分ごとの実行ループ（07-マッチング仕様 §4-1）。
//!
//! 状態をメモリに持たない。1 回の実行は [`MatchmakerRepository::run_once_with`] の
//! 1 トランザクションで完結するので、プロセスが再起動しても**次のティックから普通に続く**。
//! 起動時に特別な復帰処理は要らない。
//!
//! Discord への通知はここから出さない。`vc_assigned` まで書けば状態は `awaiting_host` に
//! なり、既存の `resume_unfinished`（5 秒スキャン）が割り当て表とホスト打診を出す。
//! **復帰の入口を 2 つ作らない**（00 §7 の Phase 5 申し送り）。

use std::env;
use std::time::Duration;

use uuid::Uuid;

use crate::lifecycle::{FormationId, Timestamp};
use crate::persistence::{RepositoryError, VcAssignment};

use super::repository::{MatchmakerRepository, MatchmakerSummary, RunOutcome};
use super::select::ScanDirection;

/// 実行間隔の環境変数。**当日の校正対象**（07 §4-1）。
pub const INTERVAL_SECONDS: &str = "MATCHMAKER_INTERVAL_SECONDS";

/// 既定の実行間隔（秒）。07 §4-1 の「3 分ごと」。
pub const DEFAULT_INTERVAL_SECONDS: u64 = 180;

/// 設定の読み込みエラー。
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum MatchmakerConfigError {
    /// 値の形が不正。
    #[error("invalid {name}: {reason}")]
    Invalid {
        /// 環境変数名。
        name: &'static str,
        /// 理由。
        reason: String,
    },
}

/// マッチメイカの設定。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MatchmakerConfig {
    /// 実行間隔。
    pub interval: Duration,
}

impl Default for MatchmakerConfig {
    fn default() -> Self {
        Self {
            interval: Duration::from_secs(DEFAULT_INTERVAL_SECONDS),
        }
    }
}

impl MatchmakerConfig {
    /// 環境変数から読む。未設定なら既定値。
    pub fn from_env() -> Result<Self, MatchmakerConfigError> {
        Self::parse(env::var(INTERVAL_SECONDS).ok().as_deref())
    }

    /// 設定値の解釈。**純粋関数**（`#![forbid(unsafe_code)]` の下では環境変数を
    /// テストから書き換えられないので、読み取りと解釈を分けてある）。
    pub fn parse(interval_seconds: Option<&str>) -> Result<Self, MatchmakerConfigError> {
        let Some(raw) = interval_seconds
            .map(str::trim)
            .filter(|value| !value.is_empty())
        else {
            return Ok(Self::default());
        };
        let seconds: u64 = raw.parse().map_err(|_| MatchmakerConfigError::Invalid {
            name: INTERVAL_SECONDS,
            reason: format!("expected a positive number of seconds, got {raw:?}"),
        })?;
        if seconds == 0 {
            return Err(MatchmakerConfigError::Invalid {
                name: INTERVAL_SECONDS,
                reason: "must be at least 1 second".into(),
            });
        }
        Ok(Self {
            interval: Duration::from_secs(seconds),
        })
    }
}

/// マッチメイカ。実行ごとの状態を持たない。
#[derive(Debug, Clone)]
pub struct Matchmaker {
    repo: MatchmakerRepository,
    /// 起動時に名前解決した VC プール。占有状況は DB から導出するので、これは候補一覧でしかない。
    vc_pool: Vec<VcAssignment>,
    config: MatchmakerConfig,
}

impl Matchmaker {
    /// 組み立てる。
    pub fn new(
        repo: MatchmakerRepository,
        vc_pool: Vec<VcAssignment>,
        config: MatchmakerConfig,
    ) -> Self {
        Self {
            repo,
            vc_pool,
            config,
        }
    }

    /// 実行間隔。
    pub fn interval(&self) -> Duration {
        self.config.interval
    }

    /// 1 回実行する。
    ///
    /// 走査の方向は**時刻から決める**（[`ScanDirection::for_scan`]）。実行間隔ごとに
    /// 上から / 下からが入れ替わるので、下の帯にも先に組める回が 1 回おきに来る。
    /// プロセス内に回数を持たないので、再起動しても並びは崩れない。
    pub async fn run_once(&self, now: Timestamp) -> Result<RunOutcome, RepositoryError> {
        self.run_once_with(now, &mut || FormationId(Uuid::new_v4()))
            .await
    }

    /// ID の採番を差し替えて 1 回実行する（テスト用）。方向は `now` から決める。
    pub async fn run_once_with(
        &self,
        now: Timestamp,
        next_id: &mut dyn FnMut() -> FormationId,
    ) -> Result<RunOutcome, RepositoryError> {
        let direction = ScanDirection::for_scan(now, self.config.interval);
        self.repo
            .run_once_with(&self.vc_pool, now, direction, next_id)
            .await
    }

    /// 方向を明示して 1 回実行する（テスト用）。
    pub async fn run_once_scanning(
        &self,
        now: Timestamp,
        direction: ScanDirection,
    ) -> Result<RunOutcome, RepositoryError> {
        self.repo
            .run_once_with(&self.vc_pool, now, direction, &mut || {
                FormationId(Uuid::new_v4())
            })
            .await
    }

    /// Discord の 15 分サマリ用に、永続化されたキューと formation を集計する。
    ///
    /// ホストのリソースは DB と関係が無いのでここで足す。[`crate::host::read`] は失敗せず、
    /// 読めなかった項目が `None` になるだけなので、`/proc` が無い環境でもサマリは出る。
    pub async fn summary(&self, now: Timestamp) -> Result<MatchmakerSummary, RepositoryError> {
        let mut summary = self.repo.summary(now, self.vc_pool.len()).await?;
        summary.host = crate::host::read();
        Ok(summary)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_default_interval_is_three_minutes() {
        assert_eq!(
            MatchmakerConfig::parse(None).unwrap().interval,
            Duration::from_secs(180),
            "07 §4-1: 3 分ごと"
        );
        assert_eq!(
            MatchmakerConfig::parse(Some("   ")).unwrap(),
            MatchmakerConfig::default(),
            "空白は未設定と同じ"
        );
    }

    #[test]
    fn the_interval_can_be_shortened_for_calibration() {
        assert_eq!(
            MatchmakerConfig::parse(Some("30")).unwrap().interval,
            Duration::from_secs(30)
        );
        assert_eq!(
            MatchmakerConfig::parse(Some(" 600 ")).unwrap().interval,
            Duration::from_secs(600)
        );
    }

    #[test]
    fn an_unusable_interval_stops_the_process_instead_of_guessing() {
        for invalid in ["0", "-1", "soon", "3m"] {
            assert!(
                matches!(
                    MatchmakerConfig::parse(Some(invalid)),
                    Err(MatchmakerConfigError::Invalid { .. })
                ),
                "{invalid}"
            );
        }
    }
}
