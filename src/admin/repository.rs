//! 管理パネルの永続化。**`rating_adjustments` / `admin_actions` への INSERT と、
//! 読み取りだけ**を書く。唯一の例外は強制離脱による `queue_entries` からの DELETE で、
//! これは可変テーブル（マイグレーション `20260906140000_queue` の冒頭）。
//!
//! 不変条件（このモジュールで守るもの）:
//! - 追記専用（00 §3-1）。UPDATE / DELETE は書かない。DB 側でも
//!   `forbid_update_delete` トリガーとアプリ用ロールの REVOKE で弾かれる。
//! - 打ち消しは `reverts_id` の既存の仕組みを使う。DB トリガーが逆符号を強制し
//!   （不一致は 23514）、部分 unique index が二重打ち消しを弾く（23505）。
//!   **アプリ側でこの挙動を回避しない** — 制約に任せて、出たエラーを判定する。
//! - 冪等は「同一 idempotency_key のみ no-op」。キーが違う行は別操作として通す。
//!   同じキーで中身が違うものは no-op にせず衝突として返す。

use std::collections::HashMap;

use chrono::{DateTime, Utc};
use sqlx::{PgConnection, PgPool};
use uuid::Uuid;

use crate::queue::model::clamp_down_range;
use crate::rating::DEFAULT_DISPLAY_MU;

use super::actions::{AdminActionKind, AdminActionRow, DEFAULT_BLOCK_ALLOWANCE, PauseState};
use super::season::{SeasonBoundary, SeasonOutcome, SeasonPreview};

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

// ----------------------------------------------------------------------
// 当日の運用操作（admin_actions）
// ----------------------------------------------------------------------
//
// マイグレーション `20260907120000_admin_actions` と対。ここでも **INSERT と読み取りだけ**を
// 書く。停止状態も枠数も列に持たず、最新行から導出する（[`super::actions`] の冒頭）。

/// 追記する運営操作 1 行。`actor_discord_user_id` は照合を通った管理者から自動で埋める。
#[derive(Debug, Clone)]
pub struct NewAdminAction {
    /// 操作の種別。
    pub kind: AdminActionKind,
    /// 操作した運営。
    pub actor_discord_user_id: i64,
    /// 対象プレイヤー。マッチメイカの停止・再開では `None`（DB の CHECK と対）。
    pub target_player_id: Option<Uuid>,
    /// 運営が書いた理由（任意）。
    pub reason: Option<String>,
    /// 構造化コンテキスト。`block_allowance_set` は `{"allowance": N}`。
    pub payload: serde_json::Value,
    /// 操作時刻。
    pub occurred_at: DateTime<Utc>,
    /// 冪等キー。同じフォームの二重送信は行を増やさない。
    pub idempotency_key: String,
}

/// 運営操作の追記結果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActionOutcome {
    /// 新しい行が入った。
    Recorded {
        /// 追記された行の id。
        id: i64,
    },
    /// 同一 `idempotency_key` の行が既にある（no-op）。同じフォームの二重送信。
    Duplicate,
}

/// 強制離脱の結果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RemovalOutcome {
    /// キューから外し、操作を記録した。
    Removed,
    /// そのプレイヤーはキューにいなかった。**記録も残さない**
    /// （残すと、本人のキュー画面に身に覚えのない通知が出る）。
    NotQueued,
}

/// キュー中の 1 人（管理パネルの一覧用）。
#[derive(Debug, Clone, PartialEq)]
pub struct QueuedEntry {
    /// 対象プレイヤー。
    pub player_id: Uuid,
    /// Discord ID。検索と照合の手掛かり。
    pub discord_user_id: i64,
    /// トレーナー名（未設定なら `None`）。
    pub trainer_name: Option<String>,
    /// 最初にキューへ入った時刻。待ち時間の表示に使う。
    pub enqueued_at: DateTime<Utc>,
    /// 下方向のマッチング範囲（上方向は +95 固定なので列が無い）。
    pub down_range: i32,
    /// 表示スケールの現在レート（追記ログからの導出値）。
    pub rating: f64,
    /// 希望ロール。`priority` の昇順。
    pub roles: Vec<QueuedRole>,
}

/// 希望ロール 1 つ。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QueuedRole {
    /// `roles.id`。
    pub role: String,
    /// 1 が第1希望。
    pub priority: i16,
}

impl AdminRepository {
    /// 運営操作を追記する。同一 `idempotency_key` があれば行を増やさない（no-op）。
    pub async fn append_action(
        &self,
        input: &NewAdminAction,
    ) -> Result<ActionOutcome, AdminStoreError> {
        let mut conn = self.pool.acquire().await?;
        append_action_on(&mut conn, input).await
    }

    /// マッチメイカの停止状態。停止中なら誰がいつ止めたかを返す。
    ///
    /// **DB が正**なので、プロセスを再起動しても止まったままになる。
    pub async fn matchmaker_pause(&self) -> Result<Option<PauseState>, sqlx::Error> {
        let mut conn = self.pool.acquire().await?;
        matchmaker_pause_on(&mut conn).await
    }

    /// そのプレイヤーのブロック枠。行が無ければ [`DEFAULT_BLOCK_ALLOWANCE`]。
    ///
    /// ブロック機能そのものは未実装で、いまは枠数を持つだけ（機能側が入ったらここを読む）。
    pub async fn block_allowance(&self, player_id: Uuid) -> Result<i32, sqlx::Error> {
        let row = sqlx::query_scalar!(
            "select (payload->>'allowance')::int as \"allowance!\" \
             from admin_actions \
             where kind = $1 and target_player_id = $2 \
             order by id desc limit 1",
            AdminActionKind::BlockAllowanceSet.as_str(),
            player_id,
        )
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.unwrap_or(DEFAULT_BLOCK_ALLOWANCE))
    }

    /// キュー中の一覧（最初に入った順 = 待ち時間の長い順）。
    ///
    /// レートは導出値なので現在値のテーブルを持たない。`queue::repository::queued_players` や
    /// マッチメイカの候補読み込みと同じ式（初期値 + `rating_adjustments` + 確定した試合の増減）。
    pub async fn queued_entries(&self) -> Result<Vec<QueuedEntry>, sqlx::Error> {
        let rows = sqlx::query!(
            "select q.id as entry_id, q.player_id, q.enqueued_at, q.down_range, \
                    p.discord_user_id, p.trainer_name, \
                    $1::double precision \
                        + coalesce((select sum(a.delta_mu) from season_rating_adjustments a \
                                    where a.player_id = q.player_id), 0::double precision) \
                        + coalesce(m.delta, 0::double precision) \
                        as \"rating!\" \
             from queue_entries q \
             join players p on p.id = q.player_id \
             left join player_match_ratings m on m.player_id = q.player_id \
             order by q.enqueued_at, q.id",
            DEFAULT_DISPLAY_MU,
        )
        .fetch_all(&self.pool)
        .await?;

        let entry_ids: Vec<Uuid> = rows.iter().map(|row| row.entry_id).collect();
        let wishes = sqlx::query!(
            "select entry_id, role, priority from queue_entry_roles \
             where entry_id = any($1) order by entry_id, priority",
            &entry_ids,
        )
        .fetch_all(&self.pool)
        .await?;
        let mut by_entry: HashMap<Uuid, Vec<QueuedRole>> = HashMap::new();
        for wish in wishes {
            by_entry.entry(wish.entry_id).or_default().push(QueuedRole {
                role: wish.role,
                priority: wish.priority,
            });
        }

        Ok(rows
            .into_iter()
            .map(|row| QueuedEntry {
                player_id: row.player_id,
                discord_user_id: row.discord_user_id,
                trainer_name: row.trainer_name,
                enqueued_at: row.enqueued_at,
                down_range: clamp_down_range(row.down_range),
                rating: row.rating,
                roles: by_entry.remove(&row.entry_id).unwrap_or_default(),
            })
            .collect())
    }

    /// キューから外し、同じトランザクションで操作を記録する。
    ///
    /// マッチメイカと同じ**プール全体の advisory lock** を取る。マッチメイカは 1 回の実行を
    /// 1 トランザクションで行い、その中で成立した 10 人をキューから消す。ロックを共有しないと
    /// 「formation に入った直後の人をキューから外した」ように見える競合が起きる。
    /// `SELECT ... FOR UPDATE` は使わない（00 §5）。
    ///
    /// **キューにいなければ記録も残さない。**身に覚えのない通知を本人に出さないため。
    /// formation には触らない。キューと formation は別の系なので、進行中の試合は影響を受けない。
    pub async fn remove_from_queue(
        &self,
        input: &NewAdminAction,
    ) -> Result<RemovalOutcome, AdminStoreError> {
        let Some(player_id) = input.target_player_id else {
            // 呼び出し側が対象を入れ忘れた場合。DB の CHECK と同じ判断をここでも下す
            return Err(AdminStoreError::Database(sqlx::Error::RowNotFound));
        };
        let mut tx = self.pool.begin().await?;
        crate::persistence::repository::lock_vc_pool(&mut tx).await?;
        let deleted = sqlx::query!("delete from queue_entries where player_id = $1", player_id)
            .execute(&mut *tx)
            .await?;
        if deleted.rows_affected() == 0 {
            tx.commit().await?;
            return Ok(RemovalOutcome::NotQueued);
        }
        if append_action_on(&mut tx, input).await? == ActionOutcome::Duplicate {
            // 同じフォームの二重送信。1 回目の記録が既にあるので行は増やさない
            tracing::warn!(%player_id, "queue removal replayed with the same idempotency key");
        }
        tx.commit().await?;
        Ok(RemovalOutcome::Removed)
    }

    /// 直近の運営操作（新しい順）。管理パネルの履歴表に出す。
    pub async fn recent_actions(&self, limit: i64) -> Result<Vec<AdminActionRow>, sqlx::Error> {
        let rows = sqlx::query!(
            "select a.id, a.kind, a.actor_discord_user_id, a.target_player_id, a.reason, \
                    a.occurred_at, \
                    case when a.kind = 'block_allowance_set' \
                         then (a.payload->>'allowance')::int end as \"allowance?\", \
                    p.trainer_name as \"trainer_name?\" \
             from admin_actions a \
             left join players p on p.id = a.target_player_id \
             order by a.id desc limit $1",
            limit,
        )
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .into_iter()
            .map(|row| AdminActionRow {
                id: row.id,
                kind: AdminActionKind::parse(&row.kind),
                raw_kind: row.kind,
                actor_discord_user_id: row.actor_discord_user_id,
                target_player_id: row.target_player_id,
                target_trainer_name: row.trainer_name,
                reason: row.reason,
                allowance: row.allowance,
                occurred_at: row.occurred_at,
            })
            .collect())
    }
}

/// 追記の実体。プールからでもトランザクションからでも同じ経路を通す。
pub(crate) async fn append_action_on(
    conn: &mut PgConnection,
    input: &NewAdminAction,
) -> Result<ActionOutcome, AdminStoreError> {
    let inserted = sqlx::query_scalar!(
        "insert into admin_actions \
         (kind, actor_discord_user_id, target_player_id, reason, payload, occurred_at, \
          idempotency_key) \
         values ($1, $2, $3, $4, $5, $6, $7) \
         on conflict (idempotency_key) do nothing \
         returning id",
        input.kind.as_str(),
        input.actor_discord_user_id,
        input.target_player_id,
        input.reason.as_deref(),
        input.payload,
        input.occurred_at,
        input.idempotency_key,
    )
    .fetch_optional(&mut *conn)
    .await?;
    match inserted {
        Some(id) => Ok(ActionOutcome::Recorded { id }),
        None => Ok(ActionOutcome::Duplicate),
    }
}

/// マッチメイカの停止状態を、停止・再開の**最新行**から導出する。
///
/// マッチメイカは自分のトランザクション（advisory lock 取得済み）の中でこれを読む。
/// 実行の判断と同じトランザクションで読むので、走り出してから止められることがない。
pub(crate) async fn matchmaker_pause_on(
    conn: &mut PgConnection,
) -> Result<Option<PauseState>, sqlx::Error> {
    let kinds = [
        AdminActionKind::MatchmakerPaused.as_str().to_owned(),
        AdminActionKind::MatchmakerResumed.as_str().to_owned(),
    ];
    let row = sqlx::query!(
        "select kind, actor_discord_user_id, reason, occurred_at \
         from admin_actions \
         where kind = any($1) \
         order by id desc limit 1",
        &kinds[..],
    )
    .fetch_optional(&mut *conn)
    .await?;
    Ok(row
        .filter(|row| row.kind == AdminActionKind::MatchmakerPaused.as_str())
        .map(|row| PauseState {
            actor_discord_user_id: row.actor_discord_user_id,
            since: row.occurred_at,
            reason: row.reason,
        }))
}

// ----------------------------------------------------------------------
// シーズン境界（導出の起点）
// ----------------------------------------------------------------------
//
// マイグレーション `20260909090000_season_boundaries` と対。ここでも **INSERT と
// 読み取りだけ**。境界を適用する条件は SQL の `current_season` ビューにあり、
// Rust 側には条件式を置かない。

/// 追記する境界 1 行。`created_by` は照合を通った管理者から自動で埋める。
#[derive(Debug, Clone)]
pub struct NewSeasonBoundary {
    /// この時刻以降を新しいシーズンにする。
    pub effective_at: DateTime<Utc>,
    /// シーズン名。2 段目でこの文字列を手入力させる。
    pub label: String,
    /// 操作した運営。
    pub actor_discord_user_id: i64,
    /// 追記時刻。
    pub created_at: DateTime<Utc>,
    /// 冪等キー。同一キーのみ no-op（状態ベースにしない）。
    pub idempotency_key: String,
}

impl AdminRepository {
    /// いま効いている境界。まだ 1 本も無ければ `None`。
    pub async fn current_season(&self) -> Result<Option<SeasonBoundary>, sqlx::Error> {
        let row = sqlx::query!(
            "select id, effective_at, label, created_by, created_at \
             from season_boundaries order by id desc limit 1"
        )
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(|row| SeasonBoundary {
            id: row.id,
            effective_at: row.effective_at,
            label: row.label,
            created_by: row.created_by,
            created_at: row.created_at,
        }))
    }

    /// 実行前に画面へ出す件数。**すべて導出値**で、どこにも保存しない。
    ///
    /// 進行中の判定は `list_unfinished`（イベント列を Rust の状態機械で畳む）を使う。
    /// 状態列は無いので、最終イベントの種別だけを見て取り違えることがない。
    pub async fn season_preview(
        &self,
        effective_at: DateTime<Utc>,
    ) -> Result<SeasonPreview, sqlx::Error> {
        let current = self.current_season().await?;

        // 進行中（matched / awaiting_host / awaiting_code / in_progress）と、
        // 報告待ち（in_progress）を 1 回の走査で数える
        let unfinished = crate::persistence::FormationRepository::new(self.pool.clone())
            .list_unfinished()
            .await
            .map_err(|error| match error {
                crate::persistence::RepositoryError::Database(error) => error,
                // list_unfinished は導出しかしないので、他の種別は起きない
                other => sqlx::Error::Protocol(other.to_string()),
            })?;
        let mut active_formations = 0;
        let mut pending_reports = 0;
        for formation in &unfinished {
            match &formation.snapshot.state {
                crate::lifecycle::FormationState::Matched
                | crate::lifecycle::FormationState::AwaitingHost { .. }
                | crate::lifecycle::FormationState::AwaitingCode { .. } => active_formations += 1,
                crate::lifecycle::FormationState::InProgress { .. } => {
                    active_formations += 1;
                    pending_reports += 1;
                }
                crate::lifecycle::FormationState::Disputed
                | crate::lifecycle::FormationState::Confirmed
                | crate::lifecycle::FormationState::Nullified
                | crate::lifecycle::FormationState::Voided
                | crate::lifecycle::FormationState::Abandoned => {}
            }
        }

        // 集計から外れる試合と、その参加者。いま導出に入っているものだけを数える
        // （`player_match_rating_deltas` が現在の境界で既に絞られている）
        let leaving = sqlx::query!(
            "select count(distinct d.formation_id)::bigint as \"matches!\", \
                    count(distinct d.player_id)::bigint as \"players!\" \
             from player_match_rating_deltas d \
             join formations f on f.id = d.formation_id \
             where f.created_at < $1",
            effective_at,
        )
        .fetch_one(&self.pool)
        .await?;

        Ok(SeasonPreview {
            active_formations,
            pending_reports,
            matches_leaving: usize::try_from(leaving.matches).unwrap_or(usize::MAX),
            players_affected: usize::try_from(leaving.players).unwrap_or(usize::MAX),
            current,
        })
    }

    /// 境界を追記する。**進行中の formation が 1 件でもあれば何も書かない。**
    ///
    /// 境界の追記と `admin_actions` への記録を同じトランザクションで行う。
    /// 冪等は「同一 `idempotency_key` のみ no-op」。
    pub async fn start_season(
        &self,
        input: &NewSeasonBoundary,
    ) -> Result<SeasonOutcome, AdminStoreError> {
        let preview = self.season_preview(input.effective_at).await?;
        if !preview.can_start() {
            return Ok(SeasonOutcome::Blocked {
                active_formations: preview.active_formations,
            });
        }

        let mut tx = self.pool.begin().await?;
        let inserted = sqlx::query_scalar!(
            "insert into season_boundaries \
             (effective_at, label, created_by, idempotency_key, created_at) \
             values ($1, $2, $3, $4, $5) \
             on conflict (idempotency_key) do nothing \
             returning id",
            input.effective_at,
            input.label,
            format!("discord:{}", input.actor_discord_user_id),
            input.idempotency_key,
            input.created_at,
        )
        .fetch_optional(&mut *tx)
        .await?;
        let Some(id) = inserted else {
            tx.commit().await?;
            return Ok(SeasonOutcome::Duplicate);
        };

        append_action_on(
            &mut tx,
            &NewAdminAction {
                kind: AdminActionKind::SeasonStarted,
                actor_discord_user_id: input.actor_discord_user_id,
                target_player_id: None,
                reason: Some(input.label.clone()),
                payload: serde_json::json!({
                    "season_boundary_id": id,
                    "effective_at": input.effective_at.to_rfc3339(),
                    "matches_leaving": preview.matches_leaving,
                    "players_affected": preview.players_affected,
                }),
                occurred_at: input.created_at,
                idempotency_key: format!("season:{}", input.idempotency_key),
            },
        )
        .await?;
        tx.commit().await?;
        Ok(SeasonOutcome::Started { id })
    }
}
