//! 戦績画面の読み取り。既存ビュー `player_match_rating_deltas` しか読まない。
//!
//! 現在レートの導出は [`crate::rating::RatingAdjustmentRepository`] の既存経路に委ねる。
//! ここで増やするのは「履歴を行として並べる」読み取りだけで、集計の再発明はしない。

use chrono::{DateTime, Utc};
use sqlx::PgPool;

use crate::lifecycle::{FormationId, PlayerId};
use crate::rating::MatchResult;

/// 履歴に並べる 1 試合。日付は `formations.created_at`。
#[derive(Debug, Clone, PartialEq)]
pub struct MatchHistoryEntry {
    /// いつの試合か。
    pub played_at: DateTime<Utc>,
    /// 自分のチーム。`red` / `blue`。
    pub team: String,
    /// 自分から見た結果。
    pub result: MatchResult,
    /// その試合での増減（表示スケール）。
    pub delta: f64,
    /// 試合開始時点の自分のレート（表示スケール、`mu_at_match * 60`）。
    pub rating_at_start: f64,
    /// どの formation か。画面には出さないが照合用に持つ。
    pub formation_id: FormationId,
}

/// 総合成績。ビューの行数から数える（別経路の集計は作らない）。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct StatusSummary {
    pub matches: i64,
    pub wins: i64,
    pub losses: i64,
    pub draws: i64,
}

/// 戦績の読み取り。接続はアプリ用ロールを想定する。
#[derive(Debug, Clone)]
pub struct StatusRepository {
    pool: PgPool,
}

impl StatusRepository {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// 確定した試合を新しい順に `limit` 件。`player_id` は呼び出し側（セッション）からしか取らない。
    ///
    /// ビューは `confirmed` イベントを持つ formation しか出さない。したがって
    /// in_progress / disputed / voided / nullified はここでは現れない（00 §3-5）。
    pub async fn history(
        &self,
        player_id: PlayerId,
        limit: i64,
    ) -> Result<Vec<MatchHistoryEntry>, sqlx::Error> {
        let rows = sqlx::query!(
            "select d.formation_id as \"formation_id!\", d.team as \"team!\", \
                    d.outcome as \"outcome!\", d.own_rating as \"own_rating!\", \
                    d.delta as \"delta!\", f.created_at as \"played_at!\" \
             from player_match_rating_deltas d \
             join formations f on f.id = d.formation_id \
             where d.player_id = $1 \
             order by f.created_at desc, d.formation_id desc \
             limit $2",
            player_id.0,
            limit,
        )
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .into_iter()
            .map(|row| {
                let team = row.team;
                let result = match row.outcome.as_str() {
                    "draw" => MatchResult::Drawn,
                    winner if winner == team => MatchResult::Won,
                    _ => MatchResult::Lost,
                };
                MatchHistoryEntry {
                    played_at: row.played_at,
                    team,
                    result,
                    delta: row.delta,
                    rating_at_start: row.own_rating,
                    formation_id: FormationId(row.formation_id),
                }
            })
            .collect())
    }

    /// 総合成績。ビューの全件を数える（履歴の表示は最新 50 件でも、成績は全期間）。
    pub async fn summary(&self, player_id: PlayerId) -> Result<StatusSummary, sqlx::Error> {
        let row = sqlx::query!(
            "select count(*) as \"matches!\", \
                    count(*) filter (where d.outcome = d.team) as \"wins!\", \
                    count(*) filter (where d.outcome <> d.team and d.outcome <> 'draw') \
                        as \"losses!\", \
                    count(*) filter (where d.outcome = 'draw') as \"draws!\" \
             from player_match_rating_deltas d \
             where d.player_id = $1",
            player_id.0,
        )
        .fetch_one(&self.pool)
        .await?;
        Ok(StatusSummary {
            matches: row.matches,
            wins: row.wins,
            losses: row.losses,
            draws: row.draws,
        })
    }
}
