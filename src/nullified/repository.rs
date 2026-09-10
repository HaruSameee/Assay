//! `nullified_reports` の読み書き。**INSERT と SELECT だけ。**
//!
//! UPDATE も DELETE もしない（アプリロールに権限が無く、トリガーでも拒否される）。
//! 成立しているかは票を数えて導出するので、状態列を持たない。

use sqlx::PgPool;
use uuid::Uuid;

use crate::lifecycle::{FormationId, PlayerId, Timestamp};

use super::model::{NullifiedReportError, NullifiedReportOutcome};

/// 通報できる相手 1 人。**マッチ画面の選択肢を作るためだけの型。**
///
/// 票数は持たない。持たせると画面に出す経路ができてしまう。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReportableParticipant {
    pub player_id: Uuid,
    pub trainer_name: Option<String>,
    /// `red` / `blue`。**相手チームも選べる**ので、どちらかを画面で区別する。
    pub team: String,
}

/// 運営パネルに出す通報 1 行。
#[derive(Debug, Clone, PartialEq)]
pub struct NullifiedReportRow {
    pub id: i64,
    pub formation_id: Uuid,
    pub reported_at: chrono::DateTime<chrono::Utc>,
    pub reporter_id: Uuid,
    pub reporter_name: Option<String>,
    /// 通報された人。**`None` は「誰も通報しない」を選んだ 1 票。**
    pub target_id: Option<Uuid>,
    pub target_name: Option<String>,
    /// 通報した人と対象が別チームだったか。`target_id` が `None` なら `None`。
    pub cross_team: Option<bool>,
}

/// 運営パネルに出す集計 1 行。**運営だけが票数を見られる。**
#[derive(Debug, Clone, PartialEq)]
pub struct NullifiedTally {
    pub formation_id: Uuid,
    pub player_id: Uuid,
    pub trainer_name: Option<String>,
    pub votes: i64,
    /// 閾値に届いてペナルティが確定しているか。
    pub penalised: bool,
    /// その試合が実際に `nullified` で終わっているか。
    ///
    /// **false のあいだは票が入っていてもレートに効かない。**通報は報告直後に
    /// 受け付けるので、結局 confirmed になった試合の票も残る。
    pub match_nullified: bool,
}

#[derive(Debug, Clone)]
pub struct NullifiedReportRepository {
    pool: PgPool,
}

impl NullifiedReportRepository {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// この人が、この試合を**無効試合として報告したか**。
    ///
    /// 訂正チェーンの先頭（最新の報告）だけを見る。最初は無効試合と言ったが
    /// 後から勝敗に訂正した人には、通報の入口を出さない。
    pub async fn reported_nullified(
        &self,
        formation_id: FormationId,
        player_id: PlayerId,
    ) -> Result<bool, sqlx::Error> {
        let outcome = sqlx::query_scalar!(
            "select r.outcome from match_reports r \
             where r.formation_id = $1 and r.player_id = $2 \
             order by r.id desc limit 1",
            formation_id.0,
            player_id.0,
        )
        .fetch_optional(&self.pool)
        .await?;
        Ok(outcome.as_deref() == Some("nullified"))
    }

    /// この人が既にこの試合で通報しているか（「誰も通報しない」も含む）。
    pub async fn has_reported(
        &self,
        formation_id: FormationId,
        reporter_id: PlayerId,
    ) -> Result<bool, sqlx::Error> {
        let found = sqlx::query_scalar!(
            "select 1 as \"one!\" from nullified_reports \
             where formation_id = $1 and reporter_id = $2",
            formation_id.0,
            reporter_id.0,
        )
        .fetch_optional(&self.pool)
        .await?;
        Ok(found.is_some())
    }

    /// 通報できる相手（自分以外の 9 人）。**相手チームも含む。**
    ///
    /// 聞き専報告は同チームに限っていたが、部屋を壊すのは味方とは限らない。
    pub async fn reportable(
        &self,
        formation_id: FormationId,
        reporter_id: PlayerId,
    ) -> Result<Vec<ReportableParticipant>, sqlx::Error> {
        let rows = sqlx::query!(
            "select fp.player_id, fp.team, p.trainer_name \
             from formation_players fp \
             join players p on p.id = fp.player_id \
             where fp.formation_id = $1 and fp.player_id <> $2 \
             order by fp.team, p.trainer_name nulls last, fp.player_id",
            formation_id.0,
            reporter_id.0,
        )
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .into_iter()
            .map(|row| ReportableParticipant {
                player_id: row.player_id,
                trainer_name: row.trainer_name,
                team: row.team,
            })
            .collect())
    }

    /// 通報を 1 行積む。`target` が `None` なら「誰も通報しない」の 1 票。
    ///
    /// **1 人 1 票で選び直せない。**「原因は誰か」は 1 つの答えなので、
    /// 二度目は対象が違っても no-op にする（`nullified_reports_once` が弾く）。
    pub async fn report(
        &self,
        formation_id: FormationId,
        reporter_id: PlayerId,
        target: Option<PlayerId>,
        now: Timestamp,
    ) -> Result<NullifiedReportOutcome, NullifiedReportError> {
        if target == Some(reporter_id) {
            return Err(NullifiedReportError::Self_);
        }

        // **無効試合を報告した人だけ。**ここを通さないと、勝敗を報告した人が
        // 相手チームを通報できてしまう
        if !self
            .reported_nullified(formation_id, reporter_id)
            .await
            .map_err(|_| NullifiedReportError::Database)?
        {
            return Err(NullifiedReportError::NotANullifiedReporter);
        }

        // 対象が同じ formation にいること。**参加者かどうかだけを見る**
        // （チームは問わない）
        if let Some(target) = target {
            let found = sqlx::query_scalar!(
                "select 1 as \"one!\" from formation_players \
                 where formation_id = $1 and player_id = $2",
                formation_id.0,
                target.0,
            )
            .fetch_optional(&self.pool)
            .await
            .map_err(|_| NullifiedReportError::Database)?;
            if found.is_none() {
                return Err(NullifiedReportError::NotAParticipant);
            }
        }

        let inserted = sqlx::query!(
            "insert into nullified_reports (formation_id, reporter_id, target_id, created_at) \
             values ($1, $2, $3, $4) \
             on conflict (formation_id, reporter_id) do nothing \
             returning id",
            formation_id.0,
            reporter_id.0,
            target.map(|target| target.0),
            now,
        )
        .fetch_optional(&self.pool)
        .await
        .map_err(|_| NullifiedReportError::Database)?;
        Ok(match inserted {
            Some(_) => NullifiedReportOutcome::Recorded,
            None => NullifiedReportOutcome::AlreadyReported,
        })
    }

    /// 無効試合の通報の閾値とペナルティ幅。**運営画面に出すためだけ。**
    ///
    /// `current_nullified_report_params` の 1 行で、プレイヤー側には出さない。
    pub async fn nullified_params(&self) -> Result<(i32, f64), sqlx::Error> {
        let row = sqlx::query!(
            "select threshold as \"threshold!\", penalty as \"penalty!\" \
             from current_nullified_report_params"
        )
        .fetch_one(&self.pool)
        .await?;
        Ok((row.threshold, row.penalty))
    }

    /// 運営パネル向けの履歴。**運営だけが読む。**
    pub async fn admin_history(&self, limit: i64) -> Result<Vec<NullifiedReportRow>, sqlx::Error> {
        let rows = sqlx::query!(
            "select r.id, r.formation_id, r.created_at, \
                    r.reporter_id, reporter.trainer_name as reporter_name, \
                    r.target_id, target.trainer_name as target_name, \
                    case when r.target_id is null then null else \
                        (select rfp.team <> tfp.team \
                         from formation_players rfp, formation_players tfp \
                         where rfp.formation_id = r.formation_id \
                           and rfp.player_id = r.reporter_id \
                           and tfp.formation_id = r.formation_id \
                           and tfp.player_id = r.target_id) \
                    end as \"cross_team?\" \
             from nullified_reports r \
             join players reporter on reporter.id = r.reporter_id \
             left join players target on target.id = r.target_id \
             order by r.id desc limit $1",
            limit,
        )
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .into_iter()
            .map(|row| NullifiedReportRow {
                id: row.id,
                formation_id: row.formation_id,
                reported_at: row.created_at,
                reporter_id: row.reporter_id,
                reporter_name: row.reporter_name,
                target_id: row.target_id,
                target_name: row.target_name,
                cross_team: row.cross_team,
            })
            .collect())
    }

    /// 運営パネル向けの集計。**票数が見えるのはここだけ。**
    pub async fn admin_tallies(&self, limit: i64) -> Result<Vec<NullifiedTally>, sqlx::Error> {
        let rows = sqlx::query!(
            "select r.formation_id, r.target_id as \"target_id!\", \
                    p.trainer_name, count(*) as \"votes!\", \
                    exists (select 1 from nullified_report_penalties n \
                            where n.formation_id = r.formation_id \
                              and n.player_id = r.target_id) as \"penalised!\", \
                    exists (select 1 from formation_events e \
                            where e.formation_id = r.formation_id \
                              and e.kind = 'nullified') as \"match_nullified!\" \
             from nullified_reports r \
             join players p on p.id = r.target_id \
             where r.target_id is not null \
             group by r.formation_id, r.target_id, p.trainer_name \
             order by count(*) desc, r.formation_id limit $1",
            limit,
        )
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .into_iter()
            .map(|row| NullifiedTally {
                formation_id: row.formation_id,
                player_id: row.target_id,
                trainer_name: row.trainer_name,
                votes: row.votes,
                penalised: row.penalised,
                match_nullified: row.match_nullified,
            })
            .collect())
    }
}
