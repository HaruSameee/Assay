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

use super::disputes::{
    DisputeOutcome, FinalOutcome, NewDisputeResolution, NewReadjudication, ReadjudicationOutcome,
};
use super::matching_range::{MatchingRangeChange, MatchingRangeOutcome, MatchingRangePreview};
use crate::lifecycle::{FormationId, PlayerId};
use crate::persistence::Outcome;
use crate::queue::MatchingRanges;

use super::actions::{AdminActionKind, AdminActionRow, DEFAULT_BLOCK_ALLOWANCE, PauseState};
use super::rating_params::{
    RatingParamChange, RatingParams, RatingParamsOutcome, RatingParamsPreview,
};
use super::schedule::{MatchmakerSchedule, MatchmakerStop, ScheduleOutcome};
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
    /// 運営の報告入力も通常の経路と同じ判定にかける。**確定条件は共有する。**
    report_majority_threshold: usize,
}

impl AdminRepository {
    pub fn new(pool: PgPool) -> Self {
        Self {
            pool,
            report_majority_threshold: crate::lifecycle::DEFAULT_REPORT_MAJORITY_THRESHOLD,
        }
    }

    /// 早期確定の閾値を設定値に合わせる。運営の入力だけ別の閾値で判定されないようにする。
    pub fn with_majority_threshold(mut self, threshold: usize) -> Self {
        self.report_majority_threshold = threshold;
        self
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
    /// 既定値は 1。運営が `block_allowance_set` を追記した場合だけ、その最新行で上書きする。
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
                    v.rating as \"rating!\" \
             from queue_entries q \
             join players p on p.id = q.player_id \
             join player_current_ratings v on v.player_id = q.player_id \
             order by q.enqueued_at, q.id",
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
        // 幅は**この一覧につき 1 回だけ**読む。行ごとに読み直すと、運営が
        // 途中で変えたときに同じ表の中で丸め先が食い違う
        let ranges = self.matching_ranges().await?;
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
                down_range: ranges.clamp_down(row.down_range),
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

/// マッチメイカを止める理由を、**手動 → スケジュールの順**で見る。
///
/// 手動の停止が常に勝つ。障害で緊急停止したのに、稼働時間になったら
/// スケジュールが勝手に再開する事故を防ぐ。手動の再開は「手動停止の解除」で
/// あって「スケジュール無視」ではないので、稼働時間外ならそのまま止まっている。
///
/// **1 スキャンにつき 1 回だけ呼ぶこと。**マッチメイカは advisory lock を取った
/// 同じトランザクションでこれを読む。
pub(crate) async fn matchmaker_stop_on(
    conn: &mut PgConnection,
    now: DateTime<Utc>,
) -> Result<Option<MatchmakerStop>, sqlx::Error> {
    if let Some(pause) = matchmaker_pause_on(conn).await? {
        return Ok(Some(MatchmakerStop::Manual {
            actor_discord_user_id: pause.actor_discord_user_id,
            since: pause.since,
            reason: pause.reason,
        }));
    }
    let schedule = matchmaker_schedule_on(conn).await?;
    if schedule.is_open(now) {
        return Ok(None);
    }
    Ok(Some(MatchmakerStop::Scheduled))
}

/// 現在のスケジュール。`current_matchmaker_schedule` の 1 行。
pub(crate) async fn matchmaker_schedule_on(
    conn: &mut PgConnection,
) -> Result<MatchmakerSchedule, sqlx::Error> {
    let row = sqlx::query!(
        "select enabled as \"enabled!\", days_mask as \"days_mask!\", \
                start_minute as \"start_minute!\", end_minute as \"end_minute!\", \
                queue_open_minute as \"queue_open_minute!\" \
         from current_matchmaker_schedule"
    )
    .fetch_one(&mut *conn)
    .await?;
    Ok(MatchmakerSchedule {
        enabled: row.enabled,
        days_mask: row.days_mask,
        start_minute: row.start_minute,
        end_minute: row.end_minute,
        queue_open_minute: row.queue_open_minute,
    })
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
        let counts = self.unfinished_counts().await?;
        let (active_formations, pending_reports, disputed) = (
            counts.active_formations,
            counts.pending_reports,
            counts.disputed,
        );

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
            disputed,
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
        // **マッチメイカのスキャンと直列化する。**スキャンは 1 回まるごとが 1 トランザクションで、
        // 先頭で同じ advisory lock を取る（07 §4-1）。ロックを取らずに数えると、
        // 「スキャンは走っているがまだ commit していない」瞬間に進行中 0 件と見えてしまい、
        // 境界を入れた直後に formation が生まれる。数えるのも入れるのもロックの内側で行う。
        let mut tx = self.pool.begin().await?;
        crate::persistence::repository::lock_vc_pool(&mut tx).await?;

        // ロックを取ってから数える。ここまで来た時点で、走っていたスキャンは commit 済み
        let preview = self.season_preview(input.effective_at).await?;
        if !preview.can_start() {
            tx.commit().await?;
            return Ok(SeasonOutcome::Blocked {
                active_formations: preview.active_formations,
            });
        }

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

/// 未終了 formation の内訳。**進行中を止める判定はここ 1 か所から取る。**
///
/// シーズン境界とレート増減幅の変更が同じ条件（進行中が 1 件でもあれば拒否）を
/// 使うので、数え方を 2 か所に書かない。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct UnfinishedCounts {
    /// matched / awaiting_host / awaiting_code / in_progress。
    pub active_formations: usize,
    /// in_progress（＝まだ報告が揃っていない試合）。`active_formations` の部分集合。
    pub pending_reports: usize,
    /// disputed。進行中には数えないが、確定していないので警告として出す。
    pub disputed: usize,
}

impl AdminRepository {
    /// 未終了 formation を 1 回の走査で数える。
    pub async fn unfinished_counts(&self) -> Result<UnfinishedCounts, sqlx::Error> {
        let unfinished = crate::persistence::FormationRepository::new(self.pool.clone())
            .list_unfinished()
            .await
            .map_err(|error| match error {
                crate::persistence::RepositoryError::Database(error) => error,
                // list_unfinished は導出しかしないので、他の種別は起きない
                other => sqlx::Error::Protocol(other.to_string()),
            })?;
        let mut counts = UnfinishedCounts::default();
        for formation in &unfinished {
            match &formation.snapshot.state {
                crate::lifecycle::FormationState::Matched
                | crate::lifecycle::FormationState::AwaitingHost { .. }
                | crate::lifecycle::FormationState::AwaitingCode { .. } => {
                    counts.active_formations += 1;
                }
                crate::lifecycle::FormationState::InProgress { .. } => {
                    counts.active_formations += 1;
                    counts.pending_reports += 1;
                }
                crate::lifecycle::FormationState::Disputed => counts.disputed += 1,
                crate::lifecycle::FormationState::Confirmed
                | crate::lifecycle::FormationState::Nullified
                | crate::lifecycle::FormationState::Voided
                | crate::lifecycle::FormationState::Abandoned => {}
            }
        }
        Ok(counts)
    }
}
// ----------------------------------------------------------------------
// マッチング範囲の変更
// ----------------------------------------------------------------------
//
// **現在値のテーブルは作らない。**幅は `matching_range_changes` の最新行から
// 導出する（`current_matching_ranges`）。
//
// 増減幅（`rating_param_changes`）と違い、**過去の試合は 1 件も変わらない。**
// 幅が効くのは次のスキャン以降の選抜だけなので、「進行中の formation があれば
// 拒否する」ガードは置いていない。代わりにスキャンと同じ advisory lock を取り、
// 1 回の選抜の途中で幅が入れ替わらないようにする。

/// 追記する変更 1 件。
#[derive(Debug, Clone)]
pub struct NewMatchingRange {
    /// 新しい幅。**検証済みの値を渡すこと**（`admin::parse_ranges`）。
    pub ranges: MatchingRanges,
    /// 運営が書いた理由。
    pub reason: String,
    /// 操作した運営。
    pub actor_discord_user_id: i64,
    /// 追記時刻。
    pub created_at: DateTime<Utc>,
    /// 冪等キー。**同一キーのみ no-op**（状態ベースにしない）。
    pub idempotency_key: String,
}

impl AdminRepository {
    /// いま効いている幅。**最新行から導出する**（保存された現在値ではない）。
    pub async fn matching_ranges(&self) -> Result<MatchingRanges, sqlx::Error> {
        let mut conn = self.pool.acquire().await?;
        crate::persistence::repository::matching_ranges(&mut conn).await
    }

    /// 変更画面に出す値。**毎回数え直す**（保存しない）。
    pub async fn matching_range_preview(&self) -> Result<MatchingRangePreview, sqlx::Error> {
        let current = self.matching_ranges().await?;
        let queued = sqlx::query_scalar!("select count(*) as \"count!\" from queue_entries")
            .fetch_one(&self.pool)
            .await?;
        let history = sqlx::query!(
            "select id, up_range as \"up_range!\", down_range as \"down_range!\", \
                    reason, created_by, created_at \
             from matching_range_changes order by id desc limit 20"
        )
        .fetch_all(&self.pool)
        .await?;
        Ok(MatchingRangePreview {
            current,
            queued: usize::try_from(queued).unwrap_or(0),
            history: history
                .into_iter()
                .map(|row| MatchingRangeChange {
                    id: row.id,
                    up_range: row.up_range,
                    down_range: row.down_range,
                    reason: row.reason,
                    created_by: row.created_by,
                    created_at: row.created_at,
                })
                .collect(),
        })
    }

    /// 幅の変更を追記する。
    ///
    /// **マッチメイカのスキャンと直列化する。**スキャンは 1 回まるごとが 1 トランザクションで、
    /// 先頭で同じ advisory lock を取り、その中で幅を 1 回だけ読む（07 §4-1）。
    /// ここでも同じロックを取るので、走っているスキャンは**古い幅で最後まで走りきる**か、
    /// **新しい幅で始まる**かのどちらかになる。途中で入れ替わることはない。
    pub async fn change_matching_range(
        &self,
        input: &NewMatchingRange,
    ) -> Result<MatchingRangeOutcome, AdminStoreError> {
        let mut tx = self.pool.begin().await?;
        crate::persistence::repository::lock_vc_pool(&mut tx).await?;

        // ロックを取ってから読む。ここまで来た時点で、走っていたスキャンは commit 済み
        let previous = crate::persistence::repository::matching_ranges(&mut tx).await?;

        let inserted = sqlx::query_scalar!(
            "insert into matching_range_changes \
             (up_range, down_range, reason, created_by, idempotency_key, created_at) \
             values ($1, $2, $3, $4, $5, $6) \
             on conflict (idempotency_key) do nothing \
             returning id",
            input.ranges.up,
            input.ranges.down,
            input.reason,
            format!("discord:{}", input.actor_discord_user_id),
            input.idempotency_key,
            input.created_at,
        )
        .fetch_optional(&mut *tx)
        .await?;
        let Some(id) = inserted else {
            tx.commit().await?;
            return Ok(MatchingRangeOutcome::Duplicate);
        };

        append_action_on(
            &mut tx,
            &NewAdminAction {
                kind: AdminActionKind::MatchingRangeChanged,
                actor_discord_user_id: input.actor_discord_user_id,
                target_player_id: None,
                reason: Some(input.reason.clone()),
                payload: serde_json::json!({
                    "matching_range_change_id": id,
                    "previous_up_range": previous.up,
                    "previous_down_range": previous.down,
                    "up_range": input.ranges.up,
                    "down_range": input.ranges.down,
                }),
                occurred_at: input.created_at,
                idempotency_key: format!("matching_range:{}", input.idempotency_key),
            },
        )
        .await?;
        tx.commit().await?;
        Ok(MatchingRangeOutcome::Changed { id })
    }
}

// ----------------------------------------------------------------------
// レート増減幅の変更
// ----------------------------------------------------------------------
//
// **現在値のテーブルは作らない。**増減幅は `rating_param_changes` の最新行から
// 導出する（`current_rating_params`）。値を変えると `player_match_rating_deltas` が
// 過去の全試合を新しい値で引き直す——docs/02 の不変条件 3 そのもので、意図した挙動。

/// 追記する変更 1 件。
#[derive(Debug, Clone)]
pub struct NewRatingParams {
    /// 新しい増減幅（表示スケール）。**検証済みの値を渡すこと。**
    pub base_delta: f64,
    /// 運営が書いた理由。
    pub reason: String,
    /// 操作した運営。
    pub actor_discord_user_id: i64,
    /// 追記時刻。
    pub created_at: DateTime<Utc>,
    /// 冪等キー。**同一キーのみ no-op**（状態ベースにしない）。
    pub idempotency_key: String,
}

impl AdminRepository {
    /// いま効いている params。**最新行から導出する**（保存された現在値ではない）。
    pub async fn rating_params(&self) -> Result<RatingParams, sqlx::Error> {
        let row = sqlx::query!(
            "select base_delta as \"base_delta!\", params_version as \"params_version!\" \
             from current_rating_params"
        )
        .fetch_one(&self.pool)
        .await?;
        Ok(RatingParams {
            base_delta: row.base_delta,
            params_version: row.params_version,
        })
    }

    /// 実行前に出す件数と履歴。**毎回数え直す**（保存しない）。
    ///
    /// 数える対象は「いま導出に入っている確定済みの試合」で、これがそのまま
    /// **引き直される試合**である（`player_match_rating_deltas` を引くので、
    /// シーズン境界より前の試合は最初から入らない）。
    pub async fn rating_params_preview(&self) -> Result<RatingParamsPreview, sqlx::Error> {
        let current = self.rating_params().await?;
        let unfinished = self.unfinished_counts().await?;
        let counts = sqlx::query!(
            "select count(distinct d.formation_id) as \"matches!\", \
                    count(distinct d.player_id) as \"players!\" \
             from player_match_rating_deltas d"
        )
        .fetch_one(&self.pool)
        .await?;
        let history = sqlx::query!(
            "select id, base_delta as \"base_delta!\", params_version as \"params_version!\", \
                    reason, created_by, created_at \
             from rating_param_changes order by id desc limit 20"
        )
        .fetch_all(&self.pool)
        .await?;
        Ok(RatingParamsPreview {
            current_base_delta: current.base_delta,
            current_params_version: current.params_version,
            matches_affected: usize::try_from(counts.matches).unwrap_or(0),
            players_affected: usize::try_from(counts.players).unwrap_or(0),
            active_formations: unfinished.active_formations,
            pending_reports: unfinished.pending_reports,
            disputed: unfinished.disputed,
            history: history
                .into_iter()
                .map(|row| RatingParamChange {
                    id: row.id,
                    base_delta: row.base_delta,
                    params_version: row.params_version,
                    reason: row.reason,
                    created_by: row.created_by,
                    created_at: row.created_at,
                })
                .collect(),
        })
    }

    /// 増減幅の変更を追記する。**版数は直前の値 +1 で採番する。**
    ///
    /// 採番も導出（`current_rating_params.params_version`）から取るので、
    /// 版数の現在値を持つ列はどこにも無い。
    pub async fn change_rating_params(
        &self,
        input: &NewRatingParams,
    ) -> Result<RatingParamsOutcome, AdminStoreError> {
        // **マッチメイカのスキャンと直列化する。**スキャンは 1 回まるごとが 1 トランザクションで、
        // 先頭で同じ advisory lock を取る（07 §4-1）。ロックを取らずに数えると、
        // 「スキャンは走っているがまだ commit していない」瞬間に進行中 0 件と見えてしまう。
        // いまは凍結値が増減の計算に入らないので実害は小さいが、μ+σ へ移ると
        // 凍結値が計算に入り、原因の分からないずれになる。
        let mut tx = self.pool.begin().await?;
        crate::persistence::repository::lock_vc_pool(&mut tx).await?;

        // ロックを取ってから数える。ここまで来た時点で、走っていたスキャンは commit 済み
        let preview = self.rating_params_preview().await?;
        // **進行中の formation があるあいだは変えない。**1 行積んだ瞬間に全員のレートが
        // 動き、キュー中の人の範囲判定も同時に変わる。断ったことは記録に残す
        if !preview.can_change() {
            append_action_on(
                &mut tx,
                &NewAdminAction {
                    kind: AdminActionKind::RatingParamsRejected,
                    actor_discord_user_id: input.actor_discord_user_id,
                    target_player_id: None,
                    reason: Some(input.reason.clone()),
                    payload: serde_json::json!({
                        "requested_base_delta": input.base_delta,
                        "current_base_delta": preview.current_base_delta,
                        "active_formations": preview.active_formations,
                        "pending_reports": preview.pending_reports,
                    }),
                    occurred_at: input.created_at,
                    idempotency_key: format!("rating_params_rejected:{}", input.idempotency_key),
                },
            )
            .await?;
            tx.commit().await?;
            return Ok(RatingParamsOutcome::Blocked {
                active_formations: preview.active_formations,
            });
        }
        let params_version = preview.current_params_version + 1;

        // 同じトランザクション（＝同じロックの内側）で入れる
        let inserted = sqlx::query_scalar!(
            "insert into rating_param_changes \
             (params_version, base_delta, reason, created_by, idempotency_key, created_at) \
             values ($1, $2, $3, $4, $5, $6) \
             on conflict (idempotency_key) do nothing \
             returning id",
            params_version,
            input.base_delta,
            input.reason,
            format!("discord:{}", input.actor_discord_user_id),
            input.idempotency_key,
            input.created_at,
        )
        .fetch_optional(&mut *tx)
        .await?;
        let Some(id) = inserted else {
            tx.commit().await?;
            return Ok(RatingParamsOutcome::Duplicate);
        };

        append_action_on(
            &mut tx,
            &NewAdminAction {
                kind: AdminActionKind::RatingParamsChanged,
                actor_discord_user_id: input.actor_discord_user_id,
                target_player_id: None,
                reason: Some(input.reason.clone()),
                payload: serde_json::json!({
                    "rating_param_change_id": id,
                    "params_version": params_version,
                    "previous_base_delta": preview.current_base_delta,
                    "base_delta": input.base_delta,
                    "matches_recomputed": preview.matches_affected,
                    "players_affected": preview.players_affected,
                }),
                occurred_at: input.created_at,
                idempotency_key: format!("rating_params:{}", input.idempotency_key),
            },
        )
        .await?;
        tx.commit().await?;
        Ok(RatingParamsOutcome::Changed { id, params_version })
    }
}
// ----------------------------------------------------------------------
// 運営による報告の入力
// ----------------------------------------------------------------------
//
// **プレイヤーの報告を UPDATE しない。**運営の報告も 1 行の追記で、確定判定は
// 従来どおり「各プレイヤーの最新報告」を読む。対象は未報告の人だけで、
// 既に報告している人の内容は運営でも書き換えられない（裁定は disputed のフロー）。

/// 運営が入れる報告 1 件。
#[derive(Debug, Clone)]
pub struct NewStaffReport {
    /// 対象の formation。
    pub formation_id: FormationId,
    /// 対象のプレイヤー。**未報告の人に限る。**
    pub player_id: PlayerId,
    /// 入れる結果（`red` / `blue` / `nullified`）。
    pub outcome: Outcome,
    /// 操作した運営。
    pub actor_discord_user_id: i64,
    /// 理由（Discord 上の裁定の要約など）。空なら記録しない。
    pub reason: Option<String>,
    /// 入力時刻。
    pub occurred_at: DateTime<Utc>,
    /// 冪等キー。同一キーのみ no-op。
    pub idempotency_key: String,
}

/// 運営の報告を入れた結果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StaffReportOutcome {
    /// 追記した。
    Entered {
        /// 追記後に確定したか（`confirmed` / `nullified` / `disputed` が積まれたか）。
        settled: bool,
    },
    /// **その人は既に報告している。**運営でも上書きしない。
    AlreadyReported,
    /// **いまは報告を入れられない状態。**係争中・確定済み・まだ始まっていない、のいずれか。
    /// 断ったことは `admin_actions` に `report_rejected` として残す。
    Rejected {
        /// そのときの状態（`disputed` / `confirmed` など）。画面の文言に使う。
        state: String,
    },
    /// その formation にそのプレイヤーが居ない。
    NotAParticipant,
}

/// 未報告のプレイヤー 1 人。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnreportedPlayer {
    pub player_id: Uuid,
    pub trainer_name: Option<String>,
    pub team: String,
}

/// 報告待ちの formation 1 件。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AwaitingReports {
    pub formation_id: Uuid,
    pub created_at: DateTime<Utc>,
    /// 状態の名前（`in_progress` / `disputed`）。
    pub state: String,
    /// まだ報告していない人。
    pub unreported: Vec<UnreportedPlayer>,
}

impl AdminRepository {
    /// 報告待ちの formation と、その未報告者。
    ///
    /// 進行中の判定は `list_unfinished`（イベント列を Rust の状態機械で畳む）を使う。
    /// 状態列は無いので、最終イベントの種別だけを見て取り違えることがない。
    pub async fn formations_awaiting_reports(&self) -> Result<Vec<AwaitingReports>, sqlx::Error> {
        let unfinished = crate::persistence::FormationRepository::new(self.pool.clone())
            .list_unfinished()
            .await
            .map_err(|error| match error {
                crate::persistence::RepositoryError::Database(error) => error,
                other => sqlx::Error::Protocol(other.to_string()),
            })?;

        let mut awaiting = Vec::new();
        for formation in &unfinished {
            // **係争中は出さない。**この一覧は「未報告が原因で確定しない試合」を
            // 押し進めるためのもので、disputed は報告が割れたことが原因。未報告者ぶんを
            // 足しても状態は動かないので、載っていること自体が
            // 「ここから解決できる」という誤解になる（係争は下の一覧で見る）
            if !formation.snapshot.state.requires_reports()
                || formation.snapshot.state == crate::lifecycle::FormationState::Disputed
            {
                continue;
            }
            let rows = sqlx::query!(
                "select fp.player_id, fp.team, p.trainer_name \
                 from formation_players fp \
                 join players p on p.id = fp.player_id \
                 where fp.formation_id = $1 \
                   and not exists ( \
                       select 1 from match_reports mr \
                       where mr.formation_id = fp.formation_id and mr.player_id = fp.player_id \
                   ) \
                 order by fp.team, fp.player_id",
                formation.log.formation_id.0,
            )
            .fetch_all(&self.pool)
            .await?;
            if rows.is_empty() {
                continue;
            }
            awaiting.push(AwaitingReports {
                formation_id: formation.log.formation_id.0,
                created_at: formation.log.created_at,
                state: formation.snapshot.state.name().to_owned(),
                unreported: rows
                    .into_iter()
                    .map(|row| UnreportedPlayer {
                        player_id: row.player_id,
                        trainer_name: row.trainer_name,
                        team: row.team,
                    })
                    .collect(),
            });
        }
        awaiting.sort_by_key(|entry| entry.created_at);
        Ok(awaiting)
    }

    /// 運営の報告を 1 件追記する。**未報告の人に限る。**
    ///
    /// 追記したあとは通常の経路と同じ `evaluate_reports` を呼ぶので、確定条件も
    /// レートの導出も変わらない（`unanimous && (all_reported || report_window_elapsed)`）。
    pub async fn enter_report(
        &self,
        input: &NewStaffReport,
    ) -> Result<StaffReportOutcome, AdminStoreError> {
        let formations = crate::persistence::FormationRepository::new(self.pool.clone());

        // 参加者でなければ複合 FK で弾かれるが、理由を返せる形で先に見る
        let participates = sqlx::query_scalar!(
            "select exists( \
                 select 1 from formation_players where formation_id = $1 and player_id = $2 \
             ) as \"found!\"",
            input.formation_id.0,
            input.player_id.0,
        )
        .fetch_one(&self.pool)
        .await?;
        if !participates {
            return Ok(StaffReportOutcome::NotAParticipant);
        }

        // **`in_progress` 以外は断る。**画面から入力欄を外しても直接 POST は届く。
        //
        // 係争中は報告が割れたことが原因なので、未報告者ぶんを足しても状態は動かない。
        // 確定済み・無効試合・不成立も同じで、`evaluate_reports` が何もしないため
        // 「受理したのに何も起きない」応答になる（マッチ画面の訂正を 409 にしたのと同じ理由）。
        // **`match_reports` には 1 行も積まない。**断った事実だけを記録に残す。
        let state = formations
            .load_events(input.formation_id)
            .await
            .map_err(|error| match error {
                crate::persistence::RepositoryError::Database(error) => {
                    AdminStoreError::Database(error)
                }
                other => AdminStoreError::Database(sqlx::Error::Protocol(other.to_string())),
            })?
            .snapshot()
            .state;
        if !matches!(state, crate::lifecycle::FormationState::InProgress { .. }) {
            self.append_action(&NewAdminAction {
                kind: AdminActionKind::ReportRejected,
                actor_discord_user_id: input.actor_discord_user_id,
                target_player_id: Some(input.player_id.0),
                reason: input.reason.clone(),
                payload: serde_json::json!({
                    "formation_id": input.formation_id.0,
                    "outcome": input.outcome.as_str(),
                    "state": state.name(),
                }),
                occurred_at: input.occurred_at,
                idempotency_key: format!("admin_report_rejected:{}", input.idempotency_key),
            })
            .await?;
            return Ok(StaffReportOutcome::Rejected {
                state: state.name().to_owned(),
            });
        }

        // **運営の報告だと分かる形で残す。**集計は従来どおり outcome だけを読むが、
        // 誰が入れたかは raw_result から後で数えられる
        let raw_result = serde_json::json!({
            "source": "staff",
            "actor_discord_user_id": input.actor_discord_user_id,
            "reason": input.reason,
        });
        let report = crate::persistence::NewReport {
            formation_id: input.formation_id,
            player_id: input.player_id,
            reported_at: input.occurred_at,
            outcome: input.outcome,
            raw_result,
            idempotency_key: input.idempotency_key.clone(),
            supersedes_id: None,
        };
        let inserted = match formations.append_report_if_absent(&report).await {
            Ok(Some(id)) => id,
            // **既に報告している人は上書きしない。**
            Ok(None) => return Ok(StaffReportOutcome::AlreadyReported),
            // 上でロック外に読んだ状態と食い違った（競合）。ここも積まずに断る
            Err(crate::persistence::RepositoryError::ReportsNotAccepted { .. }) => {
                return Ok(StaffReportOutcome::Rejected {
                    state: state.name().to_owned(),
                });
            }
            Err(crate::persistence::RepositoryError::Database(error)) => {
                return Err(AdminStoreError::Database(error));
            }
            Err(other) => {
                return Err(AdminStoreError::Database(sqlx::Error::Protocol(
                    other.to_string(),
                )));
            }
        };

        // 通常の経路と同じ判定。確定条件は変えない
        let evaluation = formations
            .evaluate_reports(
                input.formation_id,
                false,
                self.report_majority_threshold,
                input.occurred_at,
                format!("report_evaluation:staff:{}", input.idempotency_key),
            )
            .await
            .map_err(|error| match error {
                crate::persistence::RepositoryError::Database(error) => {
                    AdminStoreError::Database(error)
                }
                other => AdminStoreError::Database(sqlx::Error::Protocol(other.to_string())),
            })?;

        self.append_action(&NewAdminAction {
            kind: AdminActionKind::ReportEntered,
            actor_discord_user_id: input.actor_discord_user_id,
            target_player_id: Some(input.player_id.0),
            reason: input.reason.clone(),
            payload: serde_json::json!({
                "formation_id": input.formation_id.0,
                "outcome": input.outcome.as_str(),
                "report_id": inserted.0,
            }),
            occurred_at: input.occurred_at,
            idempotency_key: format!("admin_report:{}", input.idempotency_key),
        })
        .await?;

        Ok(StaffReportOutcome::Entered {
            settled: evaluation.event.is_some(),
        })
    }
}

// ----------------------------------------------------------------------
// 係争中の試合（閲覧のみ）
// ----------------------------------------------------------------------
//
// **ここに報告を入力する経路は作らない。**未報告者の一覧から係争中を外したので、
// そのままだと運営の視界から消える。Discord 上で裁定するための材料として、
// 誰が何を報告したかを読めるようにするだけ。

/// 係争中の試合における 1 人ぶんの報告。**最新の 1 件だけ**（訂正は反映済み）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DisputedVote {
    pub player_id: Uuid,
    pub trainer_name: Option<String>,
    pub team: String,
    /// `red` / `blue` / `nullified`。まだ報告していなければ `None`。
    pub outcome: Option<String>,
    /// 運営が入れた報告なら true（`raw_result->>'source' = 'staff'`）。
    pub from_staff: bool,
}

/// 係争中の試合 1 件。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DisputedFormation {
    pub formation_id: Uuid,
    pub created_at: DateTime<Utc>,
    /// 参加者 10 人の最新報告（未報告も含む）。
    pub votes: Vec<DisputedVote>,
}

impl AdminRepository {
    /// 係争中（`disputed`）の試合と、その報告の内訳。**閲覧専用。**
    ///
    /// 件数は 15 分サマリの「係争対応待ち」と同じ判定
    /// （`list_unfinished` を畳んで `Disputed` を数える）から来る。
    pub async fn disputed_formations(&self) -> Result<Vec<DisputedFormation>, sqlx::Error> {
        let unfinished = crate::persistence::FormationRepository::new(self.pool.clone())
            .list_unfinished()
            .await
            .map_err(|error| match error {
                crate::persistence::RepositoryError::Database(error) => error,
                other => sqlx::Error::Protocol(other.to_string()),
            })?;

        let mut disputed = Vec::new();
        for formation in &unfinished {
            if formation.snapshot.state != crate::lifecycle::FormationState::Disputed {
                continue;
            }
            disputed.push(DisputedFormation {
                formation_id: formation.log.formation_id.0,
                created_at: formation.log.created_at,
                votes: self.latest_votes(formation.log.formation_id).await?,
            });
        }
        disputed.sort_by_key(|entry| entry.created_at);
        Ok(disputed)
    }

    /// 参加者 10 人の**最新**報告（訂正で打ち消された行は出さない）。
    ///
    /// 判定に使うのと同じ「打ち消されていない行」の条件を使う。
    async fn latest_votes(
        &self,
        formation_id: FormationId,
    ) -> Result<Vec<DisputedVote>, sqlx::Error> {
        let rows = sqlx::query!(
            "select fp.player_id, fp.team, p.trainer_name, \
                    mr.outcome as \"outcome?\", \
                    (mr.raw_result->>'source' = 'staff') as \"from_staff?\" \
             from formation_players fp \
             join players p on p.id = fp.player_id \
             left join match_reports mr \
               on mr.formation_id = fp.formation_id \
              and mr.player_id = fp.player_id \
              and not exists ( \
                  select 1 from match_reports correction \
                  where correction.supersedes_id = mr.id \
              ) \
             where fp.formation_id = $1 \
             order by fp.team, fp.player_id",
            formation_id.0,
        )
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .into_iter()
            .map(|row| DisputedVote {
                player_id: row.player_id,
                trainer_name: row.trainer_name,
                team: row.team,
                outcome: row.outcome,
                from_staff: row.from_staff.unwrap_or(false),
            })
            .collect())
    }

    /// 終わった試合（confirmed / nullified / voided）を新しい順に `limit` 件。
    ///
    /// **現在のシーズンの試合だけ。**境界より前の試合は裁定し直してもレートに
    /// 入らないので、出しても意味が無い。結果は `formation_final_outcomes`
    /// （最後の終端イベント）から読む——レートの導出が読むのと同じ行である。
    pub async fn finished_formations(
        &self,
        limit: i64,
    ) -> Result<Vec<FinishedFormation>, sqlx::Error> {
        let rows = sqlx::query!(
            "select f.id as \"formation_id!\", f.created_at as \"created_at!\", \
                    o.kind as \"kind!\", o.outcome, o.occurred_at as \"decided_at!\" \
             from formations f \
             join formation_final_outcomes o on o.formation_id = f.id \
             cross join current_season s \
             where f.created_at >= s.started_at \
             order by f.created_at desc, f.id desc \
             limit $1",
            limit,
        )
        .fetch_all(&self.pool)
        .await?;
        let mut finished = Vec::with_capacity(rows.len());
        for row in rows {
            let Some(result) = final_outcome(&row.kind, row.outcome.as_deref()) else {
                // ビューは 3 種別しか返さない。来ても一覧を落とさず、その行だけ飛ばす
                tracing::warn!(kind = %row.kind, "unknown final outcome kind");
                continue;
            };
            finished.push(FinishedFormation {
                formation_id: row.formation_id,
                created_at: row.created_at,
                decided_at: row.decided_at,
                result,
                votes: self.latest_votes(FormationId(row.formation_id)).await?,
            });
        }
        Ok(finished)
    }

    /// いまの結果を 1 件読む。終わっていなければ `None`。
    async fn final_outcome_of(
        &self,
        formation_id: FormationId,
    ) -> Result<Option<FinalOutcome>, sqlx::Error> {
        let row = sqlx::query!(
            "select o.kind as \"kind!\", o.outcome \
             from formation_final_outcomes o where o.formation_id = $1",
            formation_id.0,
        )
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.and_then(|row| final_outcome(&row.kind, row.outcome.as_deref())))
    }
}

/// 終わった試合 1 件。**裁定のやり直しの材料。**
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FinishedFormation {
    pub formation_id: Uuid,
    pub created_at: DateTime<Utc>,
    /// 最後の終端イベントの時刻（やり直した後はその時刻）。
    pub decided_at: DateTime<Utc>,
    /// いまの結果。
    pub result: FinalOutcome,
    /// 参加者 10 人の最新報告（未報告も含む）。
    pub votes: Vec<DisputedVote>,
}

/// `formation_final_outcomes` の 1 行を [`FinalOutcome`] にする。知らない値は `None`。
fn final_outcome(kind: &str, outcome: Option<&str>) -> Option<FinalOutcome> {
    let kind: crate::lifecycle::FormationEventKind = kind.parse().ok()?;
    let outcome = match outcome {
        Some(raw) => Some(raw.parse::<Outcome>().ok()?),
        None => None,
    };
    Some(FinalOutcome { kind, outcome })
}

impl AdminRepository {
    /// 終わった試合の裁定をやり直す。**終端イベントをもう 1 行積むだけ。**
    ///
    /// 元の裁定もプレイヤーの報告も 1 行も書き換えない。レートは導出値なので、
    /// 最後の終端イベントが変われば 10 人ぶんの増減は自動的に引き直される
    /// （`formation_final_outcomes` を見よ）。
    ///
    /// 対象は confirmed / nullified / voided のどれかで終わっている試合だけ。
    /// 係争中は [`Self::resolve_dispute`] の経路、進行中は報告の評価に任せる。
    pub async fn readjudicate(
        &self,
        input: &NewReadjudication,
    ) -> Result<ReadjudicationOutcome, AdminStoreError> {
        let formations = crate::persistence::FormationRepository::new(self.pool.clone());
        let event_key = format!("match_readjudicated:{}", input.idempotency_key);
        let log = formations
            .load_events(input.formation_id)
            .await
            .map_err(repository_error)?;

        // **冪等キーの判定を状態より先に置く**（resolve_dispute と同じ理由）
        if log
            .events
            .iter()
            .any(|stored| stored.event.idempotency_key == event_key)
        {
            return Ok(ReadjudicationOutcome::Duplicate);
        }

        let state = log.snapshot().state;
        if !matches!(
            state,
            crate::lifecycle::FormationState::Confirmed
                | crate::lifecycle::FormationState::Nullified
                | crate::lifecycle::FormationState::Voided
        ) {
            return Ok(ReadjudicationOutcome::NotFinished {
                state: state.name().to_owned(),
            });
        }
        let Some(before) = self.final_outcome_of(input.formation_id).await? else {
            // 状態機械が終端と言っているのにビューに行が無い。起きないが、
            // 起きたら積まずに止める（何を打ち消すのか分からないまま進めない）
            return Ok(ReadjudicationOutcome::NotFinished {
                state: state.name().to_owned(),
            });
        };
        if before.is_same_as(input.decision) {
            return Ok(ReadjudicationOutcome::Unchanged { current: before });
        }

        // payload は裁定と同じ形。**やり直しであることは basis で分かる**
        let mut payload = serde_json::json!({
            "decision_basis": "staff_readjudication",
            "previous_kind": before.kind.as_str(),
        });
        if let Some(outcome) = input.decision.outcome() {
            payload["outcome"] = serde_json::json!(outcome.as_str());
        }
        if let Some(previous) = before.outcome {
            payload["previous_outcome"] = serde_json::json!(previous.as_str());
        }
        let event = crate::persistence::NewEvent::new(
            input.decision.kind(),
            None,
            input.created_at,
            event_key,
        )
        .with_payload(payload);

        let appended = formations
            .append_event(input.formation_id, &event)
            .await
            .map_err(repository_error)?;
        if appended.already_recorded {
            return Ok(ReadjudicationOutcome::Duplicate);
        }

        // 運営操作ログは**別トランザクション**（resolve_dispute と同じ理由）
        let mut tx = self.pool.begin().await?;
        append_action_on(
            &mut tx,
            &NewAdminAction {
                kind: AdminActionKind::MatchReadjudicated,
                actor_discord_user_id: input.actor_discord_user_id,
                target_player_id: None,
                reason: Some(input.reason.clone()),
                payload: serde_json::json!({
                    "formation_id": input.formation_id.0,
                    "decision": input.decision.as_str(),
                    "previous_kind": before.kind.as_str(),
                    "previous_outcome": before.outcome.map(Outcome::as_str),
                    "event_id": appended.event_id.0,
                    "changes_ratings": input.decision.changes_ratings()
                        || before.kind == crate::lifecycle::FormationEventKind::Confirmed,
                }),
                occurred_at: input.created_at,
                idempotency_key: format!("match_readjudicated:{}", input.idempotency_key),
            },
        )
        .await?;
        tx.commit().await?;

        Ok(ReadjudicationOutcome::Changed {
            before,
            state: appended.state.name().to_owned(),
        })
    }

    /// 係争を裁定して終端イベントを 1 行積む。
    ///
    /// **報告は 1 行も書き換えない。**割れた報告はそのまま残り、裁定はその上に乗る
    /// 別の 1 行である（docs/02 不変条件 1）。レートに入るのは
    /// `confirmed` を積んだときだけで、`nullified` / `voided` は増減を生まない
    /// （`player_match_rating_deltas` は `confirmed` しか読まない）。
    ///
    /// 追記は通常の経路（[`crate::persistence::FormationRepository::append_event`]）を
    /// 通すので、**formation 単位のロック・冪等キー・状態機械の検証がそのまま効く**。
    /// 係争中でない試合に積もうとすると状態機械が弾くが、その前に状態を読んで
    /// [`DisputeOutcome::NotDisputed`] で返す（画面に何が起きたかを書くため）。
    pub async fn resolve_dispute(
        &self,
        input: &NewDisputeResolution,
    ) -> Result<DisputeOutcome, AdminStoreError> {
        let formations = crate::persistence::FormationRepository::new(self.pool.clone());
        let event_key = format!("dispute_resolved:{}", input.idempotency_key);
        let log = formations
            .load_events(input.formation_id)
            .await
            .map_err(repository_error)?;

        // **冪等キーの判定を状態より先に置く。**同じフォームを 2 回送ると、
        // 1 回目で状態が disputed でなくなる。状態を先に見ると、ただの二重送信が
        // 「他の運営が先に裁定した」と表示されてしまう。
        // 判定は**キーだけ**で行う（docs/02: 状態ベースにしない）。
        if log
            .events
            .iter()
            .any(|stored| stored.event.idempotency_key == event_key)
        {
            return Ok(DisputeOutcome::Duplicate);
        }

        let state = log.snapshot().state;
        if state != crate::lifecycle::FormationState::Disputed {
            return Ok(DisputeOutcome::NotDisputed {
                state: state.name().to_owned(),
            });
        }

        // payload は評価器が積む confirmed と同じ形にする。**裁定であることは
        // basis で分かる**（一致率は運営の判断なので載せない。載せると
        // 「10 人中 N 人が一致」という指標に運営の 1 票が混ざる）
        let mut payload = serde_json::json!({ "decision_basis": "staff_adjudication" });
        if let Some(outcome) = input.decision.outcome() {
            payload["outcome"] = serde_json::json!(outcome.as_str());
        }
        let event = crate::persistence::NewEvent::new(
            input.decision.kind(),
            None,
            input.created_at,
            event_key,
        )
        .with_payload(payload);

        let appended = formations
            .append_event(input.formation_id, &event)
            .await
            .map_err(repository_error)?;
        if appended.already_recorded {
            return Ok(DisputeOutcome::Duplicate);
        }

        // 運営操作ログは**別トランザクション**で積む。イベントの追記と 1 つの
        // トランザクションにまとめるには append_event を割る必要があり、そちらの
        // ロックと冪等の保証を薄くする方が高くつく。両方とも冪等キーを持つので、
        // ここで落ちても同じ操作をやり直せば揃う
        let mut tx = self.pool.begin().await?;
        append_action_on(
            &mut tx,
            &NewAdminAction {
                kind: AdminActionKind::DisputeResolved,
                actor_discord_user_id: input.actor_discord_user_id,
                target_player_id: None,
                reason: Some(input.reason.clone()),
                payload: serde_json::json!({
                    "formation_id": input.formation_id.0,
                    "decision": input.decision.as_str(),
                    "event_id": appended.event_id.0,
                    "changes_ratings": input.decision.changes_ratings(),
                }),
                occurred_at: input.created_at,
                idempotency_key: format!("dispute_resolved:{}", input.idempotency_key),
            },
        )
        .await?;
        tx.commit().await?;

        Ok(DisputeOutcome::Resolved {
            state: appended.state.name().to_owned(),
        })
    }
}

/// `RepositoryError` を `AdminStoreError` に寄せる。DB 以外の種別は導出の失敗なので、
/// そのまま文字列で運ぶ（この経路では起きない）。
fn repository_error(error: crate::persistence::RepositoryError) -> AdminStoreError {
    match error {
        crate::persistence::RepositoryError::Database(error) => AdminStoreError::from(error),
        other => AdminStoreError::from(sqlx::Error::Protocol(other.to_string())),
    }
}

// ----------------------------------------------------------------------
// 登録者の一覧
// ----------------------------------------------------------------------
//
// 検索（`/admin`）は名前か Discord ID を知っている前提の入口で、「誰が居るのか」を
// 見る場所が無かった。運営が把握したいのは、登録済みか・停止されていないか・
// どれくらい試合しているか、の 3 つ。**すべて導出値**で、ここでは保存しない。

/// 1 ページあたりの人数。**運営画面なのでランキングより多く出す。**
pub const ROSTER_PAGE_SIZE: i64 = 50;

/// 一覧に出す 1 人。
#[derive(Debug, Clone, PartialEq)]
pub struct RosterEntry {
    pub player_id: Uuid,
    pub discord_user_id: i64,
    /// 登録フォームで申告したトレーナー名。未登録なら `None`。
    pub trainer_name: Option<String>,
    /// Discord の表示名（最新のスナップショット）。取れていなければ `None`。
    pub display_name: Option<String>,
    /// `players.status`（`active` / `suspended`）。停止処分がかかっているかを見る。
    pub status: String,
    /// 登録を済ませた時刻。まだなら `None`。
    pub registered_at: Option<DateTime<Utc>>,
    /// 表示スケールのレート（導出値）。
    pub rating: f64,
    /// 確定した試合数。
    pub matches: i64,
    /// **いまキューに入っているか。**運営が「誰が待っているか」を一覧から読めるようにする。
    pub queued: bool,
}

/// 一覧 1 ページ分。
#[derive(Debug, Clone, PartialEq)]
pub struct RosterPage {
    pub players: Vec<RosterEntry>,
    /// 1 始まりのページ番号。
    pub page: i64,
    /// 総ページ数（0 人でも 1）。
    pub pages: i64,
    /// 絞り込み後の総人数。
    pub total: i64,
    /// 絞り込みに使った文字列（空なら全員）。
    pub query: String,
}

impl AdminRepository {
    /// 登録者の一覧。**レートも試合数もキュー在席も導出値**で、保存しない。
    ///
    /// 並びは「登録の新しい順」。運営が見たいのは直近に入ってきた人なので、
    /// レート順にはしない（レート順はランキングが担う）。
    ///
    /// `query` が空でなければ、トレーナー名・Discord 表示名・Discord ID で絞る
    /// （`search_players` と同じ当たり方に、表示名ぶんを足したもの）。
    pub async fn roster(&self, query: &str, page: i64) -> Result<RosterPage, sqlx::Error> {
        let query = query.trim().to_owned();
        let filter = if query.is_empty() {
            None
        } else {
            Some(query.clone())
        };
        let total = sqlx::query_scalar!(
            "select count(*) as \"count!\" \
             from players p \
             where $1::text is null \
                or p.trainer_name ilike '%' || $1 || '%' \
                or p.discord_user_id::text = $1 \
                or exists ( \
                    select 1 from player_discord_snapshots s \
                    where s.player_id = p.id \
                      and s.display_name ilike '%' || $1 || '%' \
                )",
            filter.as_deref(),
        )
        .fetch_one(&self.pool)
        .await?;

        let pages = ((total + ROSTER_PAGE_SIZE - 1) / ROSTER_PAGE_SIZE).max(1);
        let page = page.clamp(1, pages);
        let offset = (page - 1) * ROSTER_PAGE_SIZE;

        let rows = sqlx::query!(
            "select p.id, p.discord_user_id, p.trainer_name, p.status, p.registered_at, \
                    v.rating as \"rating!\", v.matches as \"matches!\", \
                    (select s.display_name from player_discord_snapshots s \
                     where s.player_id = p.id order by s.id desc limit 1) as display_name, \
                    exists (select 1 from queue_entries q where q.player_id = p.id) as \"queued!\" \
             from players p \
             join player_current_ratings v on v.player_id = p.id \
             where $1::text is null \
                or p.trainer_name ilike '%' || $1 || '%' \
                or p.discord_user_id::text = $1 \
                or exists ( \
                    select 1 from player_discord_snapshots s \
                    where s.player_id = p.id \
                      and s.display_name ilike '%' || $1 || '%' \
                ) \
             order by p.registered_at desc nulls last, p.created_at desc, p.id \
             limit $2 offset $3",
            filter.as_deref(),
            ROSTER_PAGE_SIZE,
            offset,
        )
        .fetch_all(&self.pool)
        .await?;

        Ok(RosterPage {
            players: rows
                .into_iter()
                .map(|row| RosterEntry {
                    player_id: row.id,
                    discord_user_id: row.discord_user_id,
                    trainer_name: row.trainer_name,
                    display_name: row.display_name,
                    status: row.status,
                    registered_at: row.registered_at,
                    rating: row.rating,
                    matches: row.matches,
                    queued: row.queued,
                })
                .collect(),
            page,
            pages,
            total,
            query,
        })
    }
}

/// 追記するスケジュール 1 件。
#[derive(Debug, Clone)]
pub struct NewMatchmakerSchedule {
    /// 新しいスケジュール。**検証済みの値を渡すこと**（`admin::parse_time` / `parse_days`）。
    pub schedule: MatchmakerSchedule,
    /// 運営が書いた理由。
    pub reason: String,
    /// 操作した運営。
    pub actor_discord_user_id: i64,
    /// 追記時刻。
    pub created_at: DateTime<Utc>,
    /// 冪等キー。**同一キーのみ no-op**（状態ベースにしない）。
    pub idempotency_key: String,
}

impl AdminRepository {
    /// いま効いているスケジュール。**最新行から導出する。**
    pub async fn matchmaker_schedule(&self) -> Result<MatchmakerSchedule, sqlx::Error> {
        let mut conn = self.pool.acquire().await?;
        matchmaker_schedule_on(&mut conn).await
    }

    /// いまマッチメイカが止まっているか、その理由。**手動が先。**
    pub async fn matchmaker_stop(
        &self,
        now: DateTime<Utc>,
    ) -> Result<Option<MatchmakerStop>, sqlx::Error> {
        let mut conn = self.pool.acquire().await?;
        matchmaker_stop_on(&mut conn, now).await
    }

    /// スケジュールの変更を追記する。
    ///
    /// **マッチメイカのスキャンと直列化する。**スキャンは 1 回まるごとが 1 トランザクションで、
    /// 先頭で同じ advisory lock を取り、その中でスケジュールを 1 回だけ読む（07 §4-1）。
    /// 走っているスキャンは古い枠で走りきるか、新しい枠で始まるかのどちらかになる。
    pub async fn change_matchmaker_schedule(
        &self,
        input: &NewMatchmakerSchedule,
    ) -> Result<ScheduleOutcome, AdminStoreError> {
        let mut tx = self.pool.begin().await?;
        crate::persistence::repository::lock_vc_pool(&mut tx).await?;

        let inserted = sqlx::query_scalar!(
            "insert into matchmaker_schedule_changes \
             (enabled, days_mask, start_minute, end_minute, queue_open_minute, reason, \
              created_by, idempotency_key, created_at) \
             values ($1, $2, $3, $4, $5, $6, $7, $8, $9) \
             on conflict (idempotency_key) do nothing \
             returning id",
            input.schedule.enabled,
            input.schedule.days_mask,
            input.schedule.start_minute,
            input.schedule.end_minute,
            input.schedule.queue_open_minute,
            input.reason,
            format!("discord:{}", input.actor_discord_user_id),
            input.idempotency_key,
            input.created_at,
        )
        .fetch_optional(&mut *tx)
        .await?;
        let Some(id) = inserted else {
            tx.commit().await?;
            return Ok(ScheduleOutcome::Duplicate);
        };

        append_action_on(
            &mut tx,
            &NewAdminAction {
                kind: AdminActionKind::MatchmakerScheduled,
                actor_discord_user_id: input.actor_discord_user_id,
                target_player_id: None,
                reason: Some(input.reason.clone()),
                payload: serde_json::json!({
                    "matchmaker_schedule_change_id": id,
                    "enabled": input.schedule.enabled,
                    "days_mask": input.schedule.days_mask,
                    "start_minute": input.schedule.start_minute,
                    "end_minute": input.schedule.end_minute,
                    "queue_open_minute": input.schedule.queue_open_minute,
                }),
                occurred_at: input.created_at,
                idempotency_key: format!("matchmaker_schedule:{}", input.idempotency_key),
            },
        )
        .await?;
        tx.commit().await?;
        Ok(ScheduleOutcome::Changed { id })
    }
}
