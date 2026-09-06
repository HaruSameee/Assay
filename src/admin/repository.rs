//! 管理パネルの永続化。**`rating_adjustments` への INSERT と、読み取りだけ**を書く。
//!
//! 不変条件（このモジュールで守るもの）:
//! - 追記専用（00 §3-1）。UPDATE / DELETE は書かない。DB 側でも
//!   `forbid_update_delete` トリガーとアプリ用ロールの REVOKE で弾かれる。
//! - 打ち消しは `reverts_id` の既存の仕組みを使う。DB トリガーが逆符号を強制し
//!   （不一致は 23514）、部分 unique index が二重打ち消しを弾く（23505）。
//!   **アプリ側でこの挙動を回避しない** — 制約に任せて、出たエラーを判定する。
//! - 冪等は「同一 idempotency_key のみ no-op」。キーが違う行は別操作として通す。
//!   同じキーで中身が違うものは no-op にせず衝突として返す。

use chrono::{DateTime, Utc};
use sqlx::PgPool;
use uuid::Uuid;

/// PostgreSQL の SQLSTATE。
const UNIQUE_VIOLATION: &str = "23505";
const CHECK_VIOLATION: &str = "23514";
const FOREIGN_KEY_VIOLATION: &str = "23503";
/// `reverts_id` の部分 unique index。二重打ち消しがここで落ちる。
const REVERTS_UNIQ_INDEX: &str = "rating_adjustments_reverts_uniq";

/// 検索で見つかったプレイヤー。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FoundPlayer {
    pub player_id: Uuid,
    pub discord_user_id: i64,
    pub trainer_name: Option<String>,
}

/// 調整ログ 1 行。`reverted` は「この行が誰かに打ち消されているか」。
#[derive(Debug, Clone, PartialEq)]
pub struct AdjustmentRow {
    pub id: i64,
    pub player_id: Uuid,
    pub delta_mu: f64,
    pub delta_sigma: f64,
    pub reason: String,
    pub created_by: String,
    pub created_at: DateTime<Utc>,
    pub reverts_id: Option<i64>,
    pub reverted: bool,
    /// 一覧での表示用。`find_adjustment` では `None` のまま。
    pub trainer_name: Option<String>,
}

/// 追記する 1 行。`created_by` は呼び出し側（ハンドラ）が管理者から自動で埋める。
#[derive(Debug, Clone)]
pub struct NewAdjustment {
    pub player_id: Uuid,
    pub delta_mu: f64,
    pub delta_sigma: f64,
    pub reason: String,
    pub created_by: String,
    pub created_at: DateTime<Utc>,
    pub idempotency_key: String,
    pub reverts_id: Option<i64>,
}

/// 追記の結果。
#[derive(Debug, Clone, PartialEq)]
pub enum InsertOutcome {
    /// 新しい行が入った。
    Inserted { id: i64 },
    /// 同一 `idempotency_key` の行が既にある（no-op）。
    Duplicate { existing: AdjustmentRow },
}

/// 追記がDBの制約で拒否された理由。
#[derive(Debug, thiserror::Error)]
pub enum AdminStoreError {
    /// この調整は既に打ち消されている（`rating_adjustments_reverts_uniq` の 23505）。
    #[error("this adjustment is already reverted")]
    AlreadyReverted,
    /// 打ち消し行それ自体は打ち消せない（トリガーの 23514）。
    #[error("a reversal cannot itself be reversed")]
    RevertOfRevert,
    /// 打ち消し対象の調整が存在しない（トリガーが raise する 23503）。
    #[error("the reverted adjustment does not exist")]
    OriginalMissing,
    /// 逆符号の不一致。アプリが計算を間違えたときだけ起きる（正常系では到達しない）。
    #[error("reversal deltas do not negate the original")]
    NegationMismatch,
    /// その他の DB エラー。
    #[error("database error: {0}")]
    Database(#[from] sqlx::Error),
}

/// `query!` の記録から組み立てる。列名は 4 クエリで共通。
macro_rules! adjustment_row {
    ($row:expr) => {
        AdjustmentRow {
            id: $row.id,
            player_id: $row.player_id,
            delta_mu: $row.delta_mu,
            delta_sigma: $row.delta_sigma,
            reason: $row.reason,
            created_by: $row.created_by,
            created_at: $row.created_at,
            reverts_id: $row.reverts_id,
            reverted: $row.reverted,
            trainer_name: $row.trainer_name,
        }
    };
}

/// 調整ログの読み取りと追記。接続はアプリ用ロールを想定する
/// （既定権限の SELECT / INSERT で足りる。UPDATE / DELETE はどのみち弾かれる）。
#[derive(Debug, Clone)]
pub struct AdminRepository {
    pool: PgPool,
}

impl AdminRepository {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// セッションのプレイヤーの Discord ID。ルート照合（管理者リストとの突合）に使う。
    pub async fn discord_user_id(&self, player_id: Uuid) -> Result<Option<i64>, sqlx::Error> {
        sqlx::query_scalar!(
            "select discord_user_id from players where id = $1",
            player_id
        )
        .fetch_optional(&self.pool)
        .await
    }

    /// トレーナー名（部分一致）または Discord ID（完全一致）で検索する。
    ///
    /// Discord の表示名は DB に保存していない（scope は `identify` だけで、
    /// 表示名はセッションにも入れない方針）ため、ID 照合になる。
    pub async fn search_players(
        &self,
        query: &str,
        limit: i64,
    ) -> Result<Vec<FoundPlayer>, sqlx::Error> {
        let rows = sqlx::query!(
            "select id, discord_user_id, trainer_name \
             from players \
             where trainer_name ilike '%' || $1 || '%' or discord_user_id::text = $1 \
             order by trainer_name nulls last, id \
             limit $2",
            query,
            limit,
        )
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .into_iter()
            .map(|row| FoundPlayer {
                player_id: row.id,
                discord_user_id: row.discord_user_id,
                trainer_name: row.trainer_name,
            })
            .collect())
    }

    pub async fn find_player(&self, player_id: Uuid) -> Result<Option<FoundPlayer>, sqlx::Error> {
        let row = sqlx::query!(
            "select id, discord_user_id, trainer_name from players where id = $1",
            player_id,
        )
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(|row| FoundPlayer {
            player_id: row.id,
            discord_user_id: row.discord_user_id,
            trainer_name: row.trainer_name,
        }))
    }

    /// 直近の調整一覧（新しい順）。
    pub async fn recent_adjustments(&self, limit: i64) -> Result<Vec<AdjustmentRow>, sqlx::Error> {
        let rows = sqlx::query!(
            "select r.id, r.player_id, r.delta_mu, r.delta_sigma, r.reason, r.created_by, \
                    r.created_at, r.reverts_id, \
                    exists (select 1 from rating_adjustments rv where rv.reverts_id = r.id) \
                        as \"reverted!\", \
                    p.trainer_name as \"trainer_name?\" \
             from rating_adjustments r \
             left join players p on p.id = r.player_id \
             order by r.id desc \
             limit $1",
            limit,
        )
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.into_iter().map(|row| adjustment_row!(row)).collect())
    }

    /// 1 プレイヤーの調整一覧（新しい順）。
    pub async fn adjustments_for(
        &self,
        player_id: Uuid,
    ) -> Result<Vec<AdjustmentRow>, sqlx::Error> {
        let rows = sqlx::query!(
            "select r.id, r.player_id, r.delta_mu, r.delta_sigma, r.reason, r.created_by, \
                    r.created_at, r.reverts_id, \
                    exists (select 1 from rating_adjustments rv where rv.reverts_id = r.id) \
                        as \"reverted!\", \
                    p.trainer_name as \"trainer_name?\" \
             from rating_adjustments r \
             left join players p on p.id = r.player_id \
             where r.player_id = $1 \
             order by r.id desc",
            player_id,
        )
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.into_iter().map(|row| adjustment_row!(row)).collect())
    }

    pub async fn find_adjustment(&self, id: i64) -> Result<Option<AdjustmentRow>, sqlx::Error> {
        let row = sqlx::query!(
            "select r.id, r.player_id, r.delta_mu, r.delta_sigma, r.reason, r.created_by, \
                    r.created_at, r.reverts_id, \
                    exists (select 1 from rating_adjustments rv where rv.reverts_id = r.id) \
                        as \"reverted!\", \
                    p.trainer_name as \"trainer_name?\" \
             from rating_adjustments r \
             left join players p on p.id = r.player_id \
             where r.id = $1",
            id,
        )
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(|row| adjustment_row!(row)))
    }

    pub async fn find_by_idempotency_key(
        &self,
        key: &str,
    ) -> Result<Option<AdjustmentRow>, sqlx::Error> {
        let row = sqlx::query!(
            "select r.id, r.player_id, r.delta_mu, r.delta_sigma, r.reason, r.created_by, \
                    r.created_at, r.reverts_id, \
                    exists (select 1 from rating_adjustments rv where rv.reverts_id = r.id) \
                        as \"reverted!\", \
                    p.trainer_name as \"trainer_name?\" \
             from rating_adjustments r \
             left join players p on p.id = r.player_id \
             where r.idempotency_key = $1",
            key,
        )
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(|row| adjustment_row!(row)))
    }

    /// 調整を追記する。同一 `idempotency_key` があれば新しい行は作らない（no-op）。
    ///
    /// 打ち消しの整合（逆符号・打ち消し行の再打ち消し禁止・二重打ち消し禁止）は
    /// すべて DB 側（トリガーと unique index）が正。ここでは出た制約違反を
    /// 判別して返すだけで、先に検査して回避はしない。
    pub async fn insert(&self, input: &NewAdjustment) -> Result<InsertOutcome, AdminStoreError> {
        let inserted = sqlx::query_scalar::<_, i64>(
            "insert into rating_adjustments \
             (player_id, delta_mu, delta_sigma, reason, created_by, created_at, idempotency_key, reverts_id) \
             values ($1, $2, $3, $4, $5, $6, $7, $8) \
             on conflict (idempotency_key) do nothing \
             returning id",
        )
        .bind(input.player_id)
        .bind(input.delta_mu)
        .bind(input.delta_sigma)
        .bind(&input.reason)
        .bind(&input.created_by)
        .bind(input.created_at)
        .bind(&input.idempotency_key)
        .bind(input.reverts_id)
        .fetch_optional(&self.pool)
        .await
        .map_err(classify_insert_error)?;

        if let Some(id) = inserted {
            return Ok(InsertOutcome::Inserted { id });
        }
        // idempotency_key が重複している。既存行を返す（中身が同じなら no-op、
        // 違うなら衝突として扱うのは呼び出し側）
        match self.find_by_idempotency_key(&input.idempotency_key).await? {
            Some(existing) => Ok(InsertOutcome::Duplicate { existing }),
            // INSERT が 0 行で既存検索にも無いのは競合直後くらい。DB エラーとして扱う
            None => Err(sqlx::Error::RowNotFound.into()),
        }
    }
}

/// 追記で出た制約違反を、運用の意味に翻訳する。
fn classify_insert_error(error: sqlx::Error) -> AdminStoreError {
    let Some(db) = error.as_database_error() else {
        return AdminStoreError::Database(error);
    };
    let code = db.code().unwrap_or_default().to_string();
    if code == UNIQUE_VIOLATION {
        if db.constraint() == Some(REVERTS_UNIQ_INDEX) {
            return AdminStoreError::AlreadyReverted;
        }
        return AdminStoreError::Database(error);
    }
    if code == CHECK_VIOLATION {
        // トリガーが raise する 23514 は 2 種類。メッセージで分ける
        if db.message().contains("cannot itself be reversed") {
            return AdminStoreError::RevertOfRevert;
        }
        return AdminStoreError::NegationMismatch;
    }
    if code == FOREIGN_KEY_VIOLATION {
        return AdminStoreError::OriginalMissing;
    }
    AdminStoreError::Database(error)
}
