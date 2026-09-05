//! キューの永続化。
//!
//! 不変条件（このモジュールで守るもの）:
//! - **キューは可変テーブル**。UPDATE / DELETE を書いてよい唯一の系統（追記専用4テーブルと
//!   `player_declarations` / `rating_adjustments` には引き続き書かない）。理由は
//!   マイグレーション `20260906140000_queue` の冒頭と `ops/app_privileges.sql` の許可リスト。
//! - `SELECT ... FOR UPDATE` は使わない。対象テーブルの UPDATE 権限を要求するため、
//!   本番でだけ 42501 で落ちる形のバグになる。ここで必要な直列化は
//!   `insert ... on conflict (player_id) do update` と `queue_entry_roles` の行ロックで
//!   足りるので、`pg_advisory_xact_lock` も取らない（不要なロックは取らない）。
//! - **冪等は「同一 idempotency_key のみ no-op」**。キューには冪等キーが無い。
//!   「既にキュー中なら設定の更新として扱う」のは可変な現在状態への upsert であって、
//!   追記ログの状態ベース冪等ではない（毎回の投入が黙って捨てられることはない）。
//! - **1プレイヤー1エントリ**は `queue_entries.player_id` の unique が保証する。
//!   アプリ側の「既にキュー中か」の判定には頼らない。
//! - 他人のエントリを触る関数を置かない。すべての操作が `player_id` を引数に取り、
//!   エンドポイントはそれをセッションからしか埋めない。

use sqlx::PgPool;

use crate::lifecycle::{PlayerId, Timestamp};
use crate::rating::DEFAULT_DISPLAY_MU;

use super::model::{QueueEntry, QueueEntryId, QueueRequest, RolePreference};
use super::range::QueuedPlayer;

/// PostgreSQL の SQLSTATE。
const FOREIGN_KEY_VIOLATION: &str = "23503";
const UNIQUE_VIOLATION: &str = "23505";

/// キュー操作のエラー。
#[derive(Debug, thiserror::Error)]
pub enum QueueError {
    /// `roles.id` に無いロールを指定した。
    #[error("unknown role")]
    UnknownRole,
    /// そのプレイヤーの `players` 行が無い。
    #[error("player {0} not found")]
    PlayerNotFound(PlayerId),
    /// DB エラー。
    #[error("database error: {0}")]
    Database(#[from] sqlx::Error),
}

/// キューの永続化。接続はアプリ用ロールを想定する。
#[derive(Debug, Clone)]
pub struct QueueRepository {
    pool: PgPool,
}

impl QueueRepository {
    /// プールを包む。
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// キューに入る / 設定を更新する。
    ///
    /// 既にキュー中なら**新規作成ではなく設定の更新**として扱う。`enqueued_at` は動かさない:
    /// 更新すると、範囲を広げた人・希望ロールを増やした人ほど待ち時間が短くリセットされ、
    /// マッチメイカの「待ち時間の合計が最大の組を選ぶ」（07 §4-2）で後回しになる。
    /// 広げる動機を潰すので、最初にキューへ入った時刻を保つ。
    pub async fn enqueue(
        &self,
        player_id: PlayerId,
        request: &QueueRequest,
        now: Timestamp,
    ) -> Result<QueueEntry, QueueError> {
        let mut tx = self.pool.begin().await?;

        let entry = sqlx::query!(
            "insert into queue_entries (id, player_id, enqueued_at, updated_at, down_range) \
             values ($1, $2, $3, $3, $4) \
             on conflict (player_id) do update \
                 set down_range = excluded.down_range, updated_at = excluded.updated_at \
             returning id, enqueued_at, updated_at, down_range",
            uuid::Uuid::new_v4(),
            player_id.0,
            now,
            request.down_range,
        )
        .fetch_one(&mut *tx)
        .await
        .map_err(|error| match constraint_violation(&error) {
            // players に行が無い（セッションは有効だが DB を入れ替えた後など）
            Some(FOREIGN_KEY_VIOLATION) => QueueError::PlayerNotFound(player_id),
            _ => QueueError::Database(error),
        })?;

        // 希望ロールは差分ではなく総入れ替え。優先度の入れ替えを部分更新で表現すると、
        // (entry_id, priority) の unique に一時的に触れる順序を考える羽目になる。
        sqlx::query!(
            "delete from queue_entry_roles where entry_id = $1",
            entry.id
        )
        .execute(&mut *tx)
        .await?;

        let roles: Vec<String> = request.roles.iter().map(|r| r.role.clone()).collect();
        let priorities: Vec<i16> = request.roles.iter().map(|r| r.priority).collect();
        sqlx::query!(
            "insert into queue_entry_roles (entry_id, role, priority) \
             select $1, role, priority from unnest($2::text[], $3::smallint[]) as w (role, priority)",
            entry.id,
            &roles,
            &priorities,
        )
        .execute(&mut *tx)
        .await
        .map_err(|error| match constraint_violation(&error) {
            Some(FOREIGN_KEY_VIOLATION) => QueueError::UnknownRole,
            _ => QueueError::Database(error),
        })?;

        tx.commit().await?;
        Ok(QueueEntry {
            id: QueueEntryId(entry.id),
            player_id,
            enqueued_at: entry.enqueued_at,
            updated_at: entry.updated_at,
            down_range: entry.down_range,
            roles: request.roles.clone(),
        })
    }

    /// キューを抜ける。エントリが有ったかどうかを返す。
    ///
    /// `queue_entry_roles` は `on delete cascade` で消える。
    pub async fn leave(&self, player_id: PlayerId) -> Result<bool, QueueError> {
        let deleted = sqlx::query!(
            "delete from queue_entries where player_id = $1",
            player_id.0
        )
        .execute(&self.pool)
        .await?;
        Ok(deleted.rows_affected() > 0)
    }

    /// 自分のエントリ。キュー中でなければ `None`。
    pub async fn find(&self, player_id: PlayerId) -> Result<Option<QueueEntry>, QueueError> {
        let Some(entry) = sqlx::query!(
            "select id, enqueued_at, updated_at, down_range \
             from queue_entries where player_id = $1",
            player_id.0,
        )
        .fetch_optional(&self.pool)
        .await?
        else {
            return Ok(None);
        };
        let roles = sqlx::query!(
            "select role, priority from queue_entry_roles where entry_id = $1 order by priority",
            entry.id,
        )
        .fetch_all(&self.pool)
        .await?;
        Ok(Some(QueueEntry {
            id: QueueEntryId(entry.id),
            player_id,
            enqueued_at: entry.enqueued_at,
            updated_at: entry.updated_at,
            down_range: entry.down_range,
            roles: roles
                .into_iter()
                .map(|row| RolePreference {
                    role: row.role,
                    priority: row.priority,
                })
                .collect(),
        }))
    }

    /// キュー中の全員の、範囲判定に必要な値。
    ///
    /// **レートは導出値**なので現在値のテーブルを持たない。表示スケールの初期値
    /// （[`crate::rating::DEFAULT_DISPLAY_MU`]）に `rating_adjustments` の差分を足したものが、
    /// 現時点での導出経路の全体である（`match_results` は未実装で、試合結果による増減は
    /// まだどこにも入っていない。07 §2-2 の増減式を入れるときは、この 1 本のクエリを
    /// 伸ばす形で追加し、現在値の保存テーブルを作らないこと）。
    ///
    /// 規模はキュー100人程度（07 §6）なので、全件読んで Rust 側で数える。
    pub async fn queued_players(&self) -> Result<Vec<QueuedPlayer>, QueueError> {
        let rows = sqlx::query!(
            "select q.player_id, q.down_range, \
                    $1::double precision \
                        + coalesce((select sum(a.delta_mu) from rating_adjustments a \
                                    where a.player_id = q.player_id), 0::double precision) \
                        as \"rating!\" \
             from queue_entries q",
            DEFAULT_DISPLAY_MU,
        )
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .into_iter()
            .map(|row| QueuedPlayer {
                player_id: PlayerId(row.player_id),
                rating: row.rating,
                down_range: row.down_range,
            })
            .collect())
    }

    /// 自分の範囲内で、かつ相手からも受け入れられるキュー中の人数（07 §3-1）。
    ///
    /// 自分がキュー中でなければ `None`（範囲は本人の設定なので、入る前は決まらない）。
    pub async fn in_range_count(&self, player_id: PlayerId) -> Result<Option<usize>, QueueError> {
        let queue = self.queued_players().await?;
        let Some(me) = queue.iter().find(|player| player.player_id == player_id) else {
            return Ok(None);
        };
        Ok(Some(super::range::count_in_range(me, &queue)))
    }
}

/// 制約違反の SQLSTATE を取り出す。
fn constraint_violation(error: &sqlx::Error) -> Option<&'static str> {
    let code = error.as_database_error()?.code()?;
    match code.as_ref() {
        FOREIGN_KEY_VIOLATION => Some(FOREIGN_KEY_VIOLATION),
        UNIQUE_VIOLATION => Some(UNIQUE_VIOLATION),
        _ => None,
    }
}
