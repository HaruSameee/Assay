//! ランキングの読み取り。**既存の導出経路をそのまま読む。**
//!
//! 集計表もキャッシュも作らない。`player_match_rating_deltas` と
//! `season_rating_adjustments` はどちらもシーズン境界（`current_season`）で
//! 絞られているので、境界を 1 本入れれば掲載も自動的に従う。

use sqlx::PgPool;
use uuid::Uuid;

use crate::rating::DEFAULT_DISPLAY_MU;

/// 掲載条件。**確定した試合が 5 試合以上。**
///
/// `player_match_rating_deltas` は `confirmed` イベントのある試合しか返さないので、
/// disputed / voided / nullified はここに来ない（除外条件を書く必要が無い）。
pub const MINIMUM_MATCHES: i64 = 5;

/// 1 ページに出す人数。300 人規模なので分割する。
pub const PAGE_SIZE: i64 = 50;

/// ランキングの 1 行。
#[derive(Debug, Clone, PartialEq)]
pub struct RankedPlayer {
    /// 1 始まりの順位。**同率は同じ順位にせず、並びの位置をそのまま出す**
    /// （同率を作ると「何位タイ」の説明が要る割に、意味のある差がない）。
    pub rank: i64,
    /// `players.id`。プロフィールへの導線に使う。
    pub player_id: Uuid,
    /// Discord の表示名（最新のスナップショット）。無ければ `None`。
    pub display_name: Option<String>,
    /// トレーナー名（登録フォームの申告）。
    pub trainer_name: Option<String>,
    /// 表示スケールのレート（導出値）。
    pub rating: f64,
    /// 確定した試合数。
    pub matches: i64,
}

/// 1 ページ分。
#[derive(Debug, Clone, PartialEq)]
pub struct RankingPage {
    /// この頁の行。
    pub players: Vec<RankedPlayer>,
    /// 1 始まりの頁番号。
    pub page: i64,
    /// 総頁数（0 人なら 1）。
    pub pages: i64,
    /// 掲載条件を満たす総人数。
    pub total: i64,
}

/// ランキングの読み取り。
#[derive(Debug, Clone)]
pub struct RankingRepository {
    pool: PgPool,
}

impl RankingRepository {
    /// プールを包む。
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// 掲載条件を満たす人数。
    pub async fn total(&self) -> Result<i64, sqlx::Error> {
        let total = sqlx::query_scalar!(
            "select count(*) as \"count!\" from ( \
                 select player_id from player_match_rating_deltas \
                 group by player_id having count(*) >= $1 \
             ) qualified",
            MINIMUM_MATCHES,
        )
        .fetch_one(&self.pool)
        .await?;
        Ok(total)
    }

    /// 1 ページ分を読む。**並び順はレート降順のみ**（引数で変えられない）。
    ///
    /// 同率のときは `player_id` で決める。ページをまたいで同じ人が二度出たり
    /// 消えたりしないよう、順序を一意にしておく必要がある。
    pub async fn page(&self, page: i64) -> Result<RankingPage, sqlx::Error> {
        let total = self.total().await?;
        let pages = ((total + PAGE_SIZE - 1) / PAGE_SIZE).max(1);
        let page = page.clamp(1, pages);
        let offset = (page - 1) * PAGE_SIZE;

        let rows = sqlx::query!(
            "with qualified as ( \n                 select d.player_id, count(*)::bigint as matches, \n                        sum(d.delta) as match_delta \n                 from player_match_rating_deltas d \n                 group by d.player_id having count(*) >= $1 \n             ), adjusted as ( \n                 select a.player_id, sum(a.delta_mu) as adjust_delta \n                 from season_rating_adjustments a group by a.player_id \n             ) \n             select q.player_id as \"player_id!\", q.matches as \"matches!\", \n                    $2::double precision + coalesce(adj.adjust_delta, 0::double precision) \n                        + coalesce(q.match_delta, 0::double precision) as \"rating!\", \n                    p.trainer_name, \n                    (select s.display_name from player_discord_snapshots s \n                     where s.player_id = q.player_id order by s.id desc limit 1) as display_name \n             from qualified q \n             join players p on p.id = q.player_id \n             left join adjusted adj on adj.player_id = q.player_id \n             order by 3 desc, q.player_id \n             limit $3 offset $4",
            MINIMUM_MATCHES,
            DEFAULT_DISPLAY_MU,
            PAGE_SIZE,
            offset,
        )
        .fetch_all(&self.pool)
        .await?;

        let players = rows
            .into_iter()
            .enumerate()
            .map(|(index, row)| RankedPlayer {
                rank: offset + index as i64 + 1,
                player_id: row.player_id,
                display_name: row.display_name,
                trainer_name: row.trainer_name,
                rating: row.rating,
                matches: row.matches,
            })
            .collect();
        Ok(RankingPage {
            players,
            page,
            pages,
            total,
        })
    }
}
