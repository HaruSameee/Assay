//! 聞き専報告の永続化。**追記だけ。**
//!
//! 不変条件（このモジュールで守るもの）:
//!
//! - **票数を返す関数を、プレイヤー向けの経路に置かない。**票数を返すのは
//!   [`QuietReportRepository::admin_history`] と [`QuietReportRepository::admin_tallies`]
//!   だけで、どちらも運営パネルからしか呼ばれない
//! - 本人に返せるのは [`QuietReportRepository::has_reported`]（自分が報告したか）だけ。
//!   `reporter_id` を条件から外せない形にしてある
//! - **冪等は「同一 idempotency_key のみ no-op」。**「もう報告したか」を先に見る
//!   状態ベースの判定はしない。同じ組み合わせの二度目は unique index が弾く

use sqlx::PgPool;
use uuid::Uuid;

use crate::lifecycle::{FormationId, PlayerId, Timestamp};

use super::config::QuietReportConfig;
use super::model::{QuietReportError, QuietReportOutcome};

/// PostgreSQL の SQLSTATE。
const UNIQUE_VIOLATION: &str = "23505";

/// 運営パネルに出す 1 行。**運営だけが読む。**
#[derive(Debug, Clone, PartialEq)]
pub struct QuietReportRow {
    pub id: i64,
    pub formation_id: Uuid,
    pub reported_at: Timestamp,
    pub reporter_id: Uuid,
    pub reporter_name: Option<String>,
    pub target_id: Uuid,
    pub target_name: Option<String>,
    /// **報告者がその試合で勝っていたか。**負け試合でしか報告していない人を見つけるため。
    /// 確定していない・勝敗が無い試合では `None`。
    pub reporter_won: Option<bool>,
    /// この報告でペナルティが成立したか（閾値に達している組み合わせか）。
    pub penalised: bool,
}

/// プレイヤーごとの集計。**運営だけが読む。**
#[derive(Debug, Clone, PartialEq)]
pub struct QuietReporterTally {
    pub player_id: Uuid,
    pub trainer_name: Option<String>,
    /// **報告した数。**乱用の検出に使う（多い順に並べる）。
    pub reports_made: i64,
    /// **報告された数。**
    pub reports_received: i64,
    /// 報告した数のうち、負け試合でのもの。
    pub made_while_losing: i64,
    /// 実際にペナルティが成立した回数（被報告側）。
    pub penalties: i64,
}

/// 聞き専報告の読み書き。接続はアプリ用ロールを想定する。
#[derive(Debug, Clone)]
pub struct QuietReportRepository {
    pool: PgPool,
}

impl QuietReportRepository {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// 環境変数の閾値を DB へ同期する。**起動時に 1 回だけ呼ぶ。**
    ///
    /// 正は環境変数だが、レートの合算は `player_current_ratings` 1 か所に集約してあり、
    /// 判定に使う閾値も SQL から見えなければならない。現在の導出値と同じなら何も書かない
    /// （毎回の起動で行が増えない）。変わっていれば 1 行追記し、**過去の試合も
    /// 新しい閾値で再判定される**（docs/02 不変条件 3）。
    ///
    /// 追記したかどうかを返す（起動ログに出す）。
    pub async fn sync_thresholds(
        &self,
        config: QuietReportConfig,
        now: Timestamp,
    ) -> Result<bool, sqlx::Error> {
        let current = sqlx::query!(
            "select winner_threshold as \"winner!\", loser_threshold as \"loser!\" \
             from current_quiet_report_params"
        )
        .fetch_one(&self.pool)
        .await?;
        if current.winner == config.winner_threshold && current.loser == config.loser_threshold {
            return Ok(false);
        }
        sqlx::query!(
            "insert into quiet_report_param_changes \
             (winner_threshold, loser_threshold, source, created_at) \
             values ($1, $2, 'environment', $3)",
            config.winner_threshold,
            config.loser_threshold,
            now,
        )
        .execute(&self.pool)
        .await?;
        Ok(true)
    }

    /// いま効いている閾値。**起動ログと運営パネル以外から呼ばないこと。**
    pub async fn thresholds(&self) -> Result<QuietReportConfig, sqlx::Error> {
        let row = sqlx::query!(
            "select winner_threshold as \"winner!\", loser_threshold as \"loser!\" \
             from current_quiet_report_params"
        )
        .fetch_one(&self.pool)
        .await?;
        Ok(QuietReportConfig {
            winner_threshold: row.winner,
            loser_threshold: row.loser,
        })
    }

    /// **自分が**この試合で聞き専報告をしたか。
    ///
    /// `reporter_id` を条件から外せない形にしてある。件数も対象も返さないので、
    /// この経路から秘匿は壊れない。
    pub async fn has_reported(
        &self,
        formation_id: FormationId,
        reporter_id: PlayerId,
    ) -> Result<bool, sqlx::Error> {
        let row = sqlx::query!(
            "select exists( \
                 select 1 from quiet_reports \
                 where formation_id = $1 and reporter_id = $2 \
             ) as \"reported!\"",
            formation_id.0,
            reporter_id.0,
        )
        .fetch_one(&self.pool)
        .await?;
        Ok(row.reported)
    }

    /// 報告を 1 件追記する。**検証はすべてサーバー側。**
    ///
    /// - 確定していなければ受け付けない（勝敗で閾値が変わるため）
    /// - 勝敗が無い試合（無効試合・不成立・係争中・引き分け）も受け付けない
    /// - 対象が同じ formation の**自チーム**でなければ受け付けない
    /// - 自分自身は対象にできない
    pub async fn report(
        &self,
        formation_id: FormationId,
        reporter_id: PlayerId,
        target_id: PlayerId,
        now: Timestamp,
        idempotency_key: &str,
    ) -> Result<QuietReportOutcome, QuietReportError> {
        if reporter_id == target_id {
            return Err(QuietReportError::Self_);
        }

        // 確定と勝敗を 1 回で見る。**最新の confirmed イベントだけを見る**
        let outcome = sqlx::query!(
            "select e.payload->>'outcome' as \"outcome?\" \
             from formation_events e \
             where e.formation_id = $1 and e.kind = 'confirmed' \
             order by e.id desc limit 1",
            formation_id.0,
        )
        .fetch_optional(&self.pool)
        .await
        .map_err(|_| QuietReportError::Database)?;
        let Some(outcome) = outcome else {
            return Err(QuietReportError::NotConfirmed);
        };
        // 引き分けは勝敗が無いので閾値が決まらない
        if !matches!(outcome.outcome.as_deref(), Some("red") | Some("blue")) {
            return Err(QuietReportError::NoWinner);
        }

        // 両者のチーム。**同じ formation の同じチームだけ**
        let teams = sqlx::query!(
            "select \
                 (select fp.team from formation_players fp \
                  where fp.formation_id = $1 and fp.player_id = $2) as \"reporter?\", \
                 (select fp.team from formation_players fp \
                  where fp.formation_id = $1 and fp.player_id = $3) as \"target?\"",
            formation_id.0,
            reporter_id.0,
            target_id.0,
        )
        .fetch_one(&self.pool)
        .await
        .map_err(|_| QuietReportError::Database)?;
        let (Some(reporter_team), Some(target_team)) = (teams.reporter, teams.target) else {
            return Err(QuietReportError::NotAParticipant);
        };
        if reporter_team != target_team {
            return Err(QuietReportError::OpposingTeam);
        }

        // **同一 idempotency_key のみ no-op。**同じ組み合わせの二度目は unique index が弾く
        let inserted = sqlx::query!(
            "insert into quiet_reports \
             (formation_id, reporter_id, target_id, reported_at, idempotency_key) \
             values ($1, $2, $3, $4, $5) \
             on conflict (idempotency_key) do nothing \
             returning id",
            formation_id.0,
            reporter_id.0,
            target_id.0,
            now,
            idempotency_key,
        )
        .fetch_optional(&self.pool)
        .await;
        match inserted {
            Ok(Some(_)) => Ok(QuietReportOutcome::Recorded),
            Ok(None) => Ok(QuietReportOutcome::AlreadyReported),
            // 同じ組み合わせを別のキーで送ってきた（二度押し・再読み込み）。
            // エラーにせず no-op として扱う
            Err(error) if constraint_violation(&error) == Some(UNIQUE_VIOLATION) => {
                Ok(QuietReportOutcome::AlreadyReported)
            }
            Err(_) => Err(QuietReportError::Database),
        }
    }

    /// 運営パネル向けの履歴。**運営だけが読む。**
    pub async fn admin_history(&self, limit: i64) -> Result<Vec<QuietReportRow>, sqlx::Error> {
        let rows = sqlx::query!(
            "select q.id, q.formation_id, q.reported_at, \
                    q.reporter_id, reporter.trainer_name as reporter_name, \
                    q.target_id, target.trainer_name as target_name, \
                    case when c.outcome in ('red', 'blue') \
                         then c.outcome = fp.team else null end as \"reporter_won?\", \
                    exists ( \
                        select 1 from quiet_report_penalties p \
                        where p.formation_id = q.formation_id and p.player_id = q.target_id \
                    ) as \"penalised!\" \
             from quiet_reports q \
             join players reporter on reporter.id = q.reporter_id \
             join players target on target.id = q.target_id \
             join formation_players fp \
                 on fp.formation_id = q.formation_id and fp.player_id = q.reporter_id \
             left join lateral ( \
                 select e.payload->>'outcome' as outcome from formation_events e \
                 where e.formation_id = q.formation_id and e.kind = 'confirmed' \
                 order by e.id desc limit 1 \
             ) c on true \
             order by q.id desc limit $1",
            limit,
        )
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .into_iter()
            .map(|row| QuietReportRow {
                id: row.id,
                formation_id: row.formation_id,
                reported_at: row.reported_at,
                reporter_id: row.reporter_id,
                reporter_name: row.reporter_name,
                target_id: row.target_id,
                target_name: row.target_name,
                reporter_won: row.reporter_won,
                penalised: row.penalised,
            })
            .collect())
    }

    /// プレイヤーごとの集計。**報告した数の多い順**（報告者の乱用を検出するため）。
    pub async fn admin_tallies(&self, limit: i64) -> Result<Vec<QuietReporterTally>, sqlx::Error> {
        let rows = sqlx::query!(
            "with made as ( \
                 select q.reporter_id as player_id, count(*)::bigint as made, \
                        count(*) filter ( \
                            where c.outcome in ('red', 'blue') and c.outcome <> fp.team \
                        )::bigint as made_while_losing \
                 from quiet_reports q \
                 join formation_players fp \
                     on fp.formation_id = q.formation_id and fp.player_id = q.reporter_id \
                 left join lateral ( \
                     select e.payload->>'outcome' as outcome from formation_events e \
                     where e.formation_id = q.formation_id and e.kind = 'confirmed' \
                     order by e.id desc limit 1 \
                 ) c on true \
                 group by q.reporter_id \
             ), received as ( \
                 select q.target_id as player_id, count(*)::bigint as received \
                 from quiet_reports q group by q.target_id \
             ), penalised as ( \
                 select p.player_id, count(*)::bigint as penalties \
                 from quiet_report_penalties p group by p.player_id \
             ) \
             select p.id as \"player_id!\", p.trainer_name, \
                    coalesce(made.made, 0)::bigint as \"reports_made!\", \
                    coalesce(made.made_while_losing, 0)::bigint as \"made_while_losing!\", \
                    coalesce(received.received, 0)::bigint as \"reports_received!\", \
                    coalesce(penalised.penalties, 0)::bigint as \"penalties!\" \
             from players p \
             left join made on made.player_id = p.id \
             left join received on received.player_id = p.id \
             left join penalised on penalised.player_id = p.id \
             where made.made is not null or received.received is not null \
             order by coalesce(made.made, 0) desc, coalesce(received.received, 0) desc, p.id \
             limit $1",
            limit,
        )
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .into_iter()
            .map(|row| QuietReporterTally {
                player_id: row.player_id,
                trainer_name: row.trainer_name,
                reports_made: row.reports_made,
                reports_received: row.reports_received,
                made_while_losing: row.made_while_losing,
                penalties: row.penalties,
            })
            .collect())
    }
}

fn constraint_violation(error: &sqlx::Error) -> Option<&str> {
    error
        .as_database_error()
        .and_then(|error| error.code())
        .map(|code| match code {
            std::borrow::Cow::Borrowed(code) => code,
            std::borrow::Cow::Owned(_) => "",
        })
}
