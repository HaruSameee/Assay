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

use crate::lifecycle::{FormationId, PlayerId, Timestamp};
use crate::matchmaker::FirstChoiceRoleCounts;

use super::model::{
    MatchingRanges, QueueEntry, QueueEntryId, QueueRequest, REMOVAL_NOTICE_WINDOW_HOURS,
    RolePreference, StaffRemoval, VoiceDestination, validate_request,
};
use super::range::QueuedPlayer;

/// PostgreSQL の SQLSTATE。
const FOREIGN_KEY_VIOLATION: &str = "23503";
const UNIQUE_VIOLATION: &str = "23505";

/// 範囲内の待機状況。**人数と内訳を同じ判定・同じクエリから作る。**
///
/// 内訳は「第1希望がそのロールの人」の数で、15 分サマリと同じ定義。
/// 「希望のどこかに含まれる人」ではないので、内訳の合計は `total` と一致する
/// （希望を 1 つも持たない行があれば、そのぶんだけ内訳のほうが少なくなる）。
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct InRangeSummary {
    /// 自分の範囲内で、かつ相手からも受け入れられる人数（07 §3-1）。
    pub total: usize,
    /// **キュー全体の人数。**レート帯を問わず、いまキューにいる全員
    /// (自分は含めない。`total` と分母を揃える)。
    ///
    /// `total` と**同じ 1 本のクエリの結果から数える**。別に count を投げると、
    /// 2 つの数字がわずかに違う瞬間のキューを指すことがある。
    pub queue_total: usize,
    /// その内訳（5 ロールすべて。0 人でも 0）。
    pub by_first_choice: FirstChoiceRoleCounts,
}

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
        self.enqueue_from(player_id, request, false, now).await
    }

    /// 接続元の判定つきで投入する。`shadow_banned` は**ルートが接続元の IP を
    /// `current_ip_bans` と突き合わせた結果**で、リクエストからは来ない。真なら
    /// マッチメイカは候補から外す（本人には何も告げない）。再投入では判定し直した値で上書きする。
    pub async fn enqueue_from(
        &self,
        player_id: PlayerId,
        request: &QueueRequest,
        shadow_banned: bool,
        now: Timestamp,
    ) -> Result<QueueEntry, QueueError> {
        let mut tx = self.pool.begin().await?;

        let entry = sqlx::query!(
            "insert into queue_entries \
                 (id, player_id, enqueued_at, updated_at, down_range, streaming, shadow_banned) \
             values ($1, $2, $3, $3, \
                     (select down_range from current_matching_ranges), $4, $5) \
             on conflict (player_id) do update \
                 set down_range = excluded.down_range, updated_at = excluded.updated_at, \
                     streaming = excluded.streaming, shadow_banned = excluded.shadow_banned \
             returning id, enqueued_at, updated_at, down_range, \
                       (select up_range from current_matching_ranges) \
                           as \"up_range!\"",
            uuid::Uuid::new_v4(),
            player_id.0,
            now,
            request.streaming,
            shadow_banned,
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

        // 選んだ内容を追記して残す。**次にキュー画面を開いたときのプリフィル用。**
        // `queue_entry_roles` は離脱で消える可変の状態なので、そこに履歴は残らない。
        // マッチングは従来どおり `queue_entry_roles` を読む（この表は表示にしか使わない）。
        sqlx::query!(
            "insert into queue_role_preferences (player_id, chosen_at, roles) values ($1, $2, $3)",
            player_id.0,
            now,
            serde_json::to_value(&request.roles).unwrap_or_else(|_| serde_json::json!([])),
        )
        .execute(&mut *tx)
        .await?;

        tx.commit().await?;
        Ok(QueueEntry {
            id: QueueEntryId(entry.id),
            player_id,
            enqueued_at: entry.enqueued_at,
            updated_at: entry.updated_at,
            // 入れたばかりの行なので、`down_range` は投入時点の現在値そのもの
            up_range: entry.up_range,
            down_range: entry.down_range,
            roles: request.roles.clone(),
            streaming: request.streaming,
        })
    }

    /// 前回選んだロール希望。**最新行から導出する**（現在値の列は持たない）。
    ///
    /// キュー画面のプリフィルにしか使わない。読めない行や、いまは存在しないロールが
    /// 混ざっていれば黙って空を返す — 中途半端に埋まった画面より、空のほうが誤解が無い。
    pub async fn last_role_preferences(
        &self,
        player_id: PlayerId,
    ) -> Result<Vec<RolePreference>, QueueError> {
        let Some(row) = sqlx::query!(
            "select roles from queue_role_preferences \
             where player_id = $1 order by id desc limit 1",
            player_id.0,
        )
        .fetch_optional(&self.pool)
        .await?
        else {
            return Ok(Vec::new());
        };
        let Ok(mut roles) = serde_json::from_value::<Vec<RolePreference>>(row.roles) else {
            return Ok(Vec::new());
        };
        roles.sort_by_key(|preference| preference.priority);
        if validate_request(&roles).is_err() {
            return Ok(Vec::new());
        }
        Ok(roles)
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
            "select id, enqueued_at, updated_at, down_range, streaming \
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
        let ranges = self.ranges().await?;
        Ok(Some(QueueEntry {
            id: QueueEntryId(entry.id),
            player_id,
            enqueued_at: entry.enqueued_at,
            updated_at: entry.updated_at,
            up_range: ranges.up,
            down_range: ranges.clamp_down(entry.down_range),
            roles: roles
                .into_iter()
                .map(|row| RolePreference {
                    role: row.role,
                    priority: row.priority,
                })
                .collect(),
            streaming: entry.streaming,
        }))
    }

    /// キュー中の全員の、範囲判定に必要な値。
    ///
    /// **レートは導出値**なので現在値のテーブルを持たない。表示スケールの初期値
    /// （[`crate::rating::DEFAULT_DISPLAY_MU`]）に `rating_adjustments` の差分と、
    /// 確定した試合の増減（07 §2-2。`player_match_ratings` ビュー）を足す。ビューは実体を
    /// 持たないので、読むたびに追記ログから計算し直される。
    ///
    /// 集計は**1 回だけ**行って join する。プレイヤーごとの相関サブクエリにすると、
    /// キューの人数だけビュー全体を走査することになる。
    /// 規模はキュー100人程度（07 §6）なので、全件読んで Rust 側で数える。
    /// 現在のマッチング範囲。`current_matching_ranges` の 1 行。
    ///
    /// **1 回の画面描画につき 1 回だけ呼ぶこと。**同じ描画の中で 2 回読むと、
    /// 運営が間に幅を変えたときに「自分の幅」と「相手の幅」が食い違う。
    pub async fn ranges(&self) -> Result<MatchingRanges, QueueError> {
        let mut conn = self.pool.acquire().await?;
        Ok(crate::persistence::repository::matching_ranges(&mut conn).await?)
    }

    /// **いまマッチメイカが止まっているか、その理由。**
    ///
    /// キュー画面に出すためだけに読む。止まっていることを黙っていると、
    /// 稼働時間外に入った人が理由も分からず待ち続ける（待ち時間だけが増える）。
    pub async fn matchmaker_stop(
        &self,
        now: Timestamp,
    ) -> Result<Option<crate::admin::MatchmakerStop>, QueueError> {
        let mut conn = self.pool.acquire().await?;
        Ok(crate::admin::repository::matchmaker_stop_on(&mut conn, now).await?)
    }

    /// この接続元がいま BAN 中か（`current_ip_bans`。完全一致）。
    pub async fn is_ip_banned(&self, ip: &str) -> Result<bool, QueueError> {
        let banned = sqlx::query_scalar!(
            "select exists(select 1 from current_ip_bans where ip = $1) as \"banned!\"",
            ip,
        )
        .fetch_one(&self.pool)
        .await?;
        Ok(banned)
    }

    /// 稼働スケジュール（画面に「次はいつ動くか」を書くため）。
    pub async fn schedule(&self) -> Result<crate::admin::MatchmakerSchedule, QueueError> {
        let mut conn = self.pool.acquire().await?;
        Ok(crate::admin::repository::matchmaker_schedule_on(&mut conn).await?)
    }

    /// キュー中の全員を、現在の幅で読む。
    pub async fn queued_players(&self) -> Result<Vec<QueuedPlayer>, QueueError> {
        let ranges = self.ranges().await?;
        self.queued_players_within(ranges).await
    }

    /// 幅を明示して読む。**同じ描画の中で幅を 2 回読まないための入口。**
    async fn queued_players_within(
        &self,
        ranges: MatchingRanges,
    ) -> Result<Vec<QueuedPlayer>, QueueError> {
        // 第1希望のロールは**同じクエリに畳む**。範囲内の人数もロール別の内訳も
        // この1本の結果から数えるので、10秒ポーリングで往復が増えない。
        // レートは**判定用**（低レート帯の底上げ込み。`matchmaker::boost`）。マッチメイカが
        // 見るのと同じ値で数えないと、「範囲内 N 人」と実際に組まれる相手がずれる
        let rows = sqlx::query!(
            "select q.player_id, q.down_range, \
                    (select r.role from queue_entry_roles r \
                     where r.entry_id = q.id and r.priority = 1) as first_choice, \
                    v.rating as \"rating!\", q.unmatched_scans, q.shadow_banned \
             from queue_entries q \
             join player_current_ratings v on v.player_id = q.player_id",
        )
        .fetch_all(&self.pool)
        .await?;
        // 「いちばん上」はキュー全員の実レートから決める（マッチメイカと同じ）
        let top = crate::matchmaker::top_rating(rows.iter().map(|row| &row.rating));
        Ok(rows
            .into_iter()
            .map(|row| QueuedPlayer {
                player_id: PlayerId(row.player_id),
                rating: crate::matchmaker::matching_rating(row.rating, row.unmatched_scans, top),
                up_range: ranges.up,
                down_range: ranges.clamp_down(row.down_range),
                first_choice: row.first_choice,
                shadow_banned: row.shadow_banned,
            })
            .collect())
    }

    /// 範囲内の人数と、その**第1希望のロール別内訳**。
    ///
    /// 人数と内訳は `queued_players` の**同じ 1 本のクエリ**の結果から数える
    /// （画面のポーリングで往復を増やさない）。自分がキュー中でなければ `None`。
    pub async fn in_range_summary(
        &self,
        player_id: PlayerId,
    ) -> Result<Option<InRangeSummary>, QueueError> {
        let queue = self.queued_players().await?;
        let Some(me) = queue.iter().find(|player| player.player_id == player_id) else {
            return Ok(None);
        };
        // シャドウバン中の人は数えない（組まれないので）。**自分は残す**（本人の画面は
        // ふつうに見せる。自分は count 側で除かれる）
        let queue: Vec<QueuedPlayer> = queue
            .iter()
            .filter(|player| !player.shadow_banned || player.player_id == player_id)
            .cloned()
            .collect();
        Ok(Some(InRangeSummary {
            total: super::range::count_in_range(me, &queue),
            // 自分を除く（`count_in_range` も自分は数えない）
            queue_total: queue.len().saturating_sub(1),
            by_first_choice: super::range::count_by_first_choice(me, &queue),
        }))
    }

    /// 未投入時のプレビュー。固定幅（上下 50）で相互に成立する人数を返す。
    ///
    /// 範囲はユーザーが調整できないので引数に取らない。本人のレートもキュー中のレートも
    /// 追記ログから毎回導出する。プレビューのために `queue_entries` を一時作成したり、
    /// 現在レートを保存したりしない。
    pub async fn preview_in_range_summary(
        &self,
        player_id: PlayerId,
    ) -> Result<InRangeSummary, QueueError> {
        // 幅の読み出しは**この 1 回だけ**。`queued_players_within` にも同じ 1 個を
        // 渡すので、自分の幅と相手の幅が食い違わない
        let ranges = self.ranges().await?;
        let queue: Vec<QueuedPlayer> = self
            .queued_players_within(ranges)
            .await?
            .into_iter()
            .filter(|player| !player.shadow_banned)
            .collect();
        let Some(rating) = self.player_rating(player_id).await? else {
            return Err(QueueError::PlayerNotFound(player_id));
        };
        let me = QueuedPlayer {
            player_id,
            rating,
            up_range: ranges.up,
            down_range: ranges.down,
            // 未投入なので希望はまだ無い。内訳の分母には入らない
            first_choice: None,
            shadow_banned: false,
        };
        Ok(InRangeSummary {
            total: super::range::count_in_range(&me, &queue),
            // 未投入なので自分はキューに入っていない。そのまま全員が「自分以外」
            queue_total: queue.len(),
            by_first_choice: super::range::count_by_first_choice(&me, &queue),
        })
    }

    /// `vc_assigned` の永久ログから、成立済み formation で本人が向かう VC を読む。
    /// Discord 側のチャンネル検索は行わない。
    pub async fn voice_destination(
        &self,
        formation_id: FormationId,
        player_id: PlayerId,
    ) -> Result<Option<VoiceDestination>, QueueError> {
        let row = sqlx::query!(
            "select fp.team, latest.payload->'vc'->>'label' as \"label?\", \
                    case fp.team \
                        when 'red' then latest.payload->'vc'->>'red_channel_id' \
                        when 'blue' then latest.payload->'vc'->>'blue_channel_id' \
                    end as \"channel_id?\" \
             from formation_players fp \
             left join lateral ( \
                 select e.payload \
                 from formation_events e \
                 where e.formation_id = fp.formation_id and e.kind = 'vc_assigned' \
                   and e.payload ? 'vc' \
                 order by e.id desc \
                 limit 1 \
             ) latest on true \
             where fp.formation_id = $1 and fp.player_id = $2",
            formation_id.0,
            player_id.0,
        )
        .fetch_optional(&self.pool)
        .await?;
        let Some(row) = row else {
            return Ok(None);
        };
        let (Some(label), Some(channel_id)) = (row.label, row.channel_id) else {
            return Ok(None);
        };
        let label = label.trim();
        let channel_id = channel_id.trim();
        if label.is_empty()
            || channel_id
                .parse::<u64>()
                .ok()
                .filter(|id| *id != 0)
                .is_none()
        {
            return Ok(None);
        }
        Ok(Some(VoiceDestination {
            formation_id,
            team: row.team,
            label: label.to_owned(),
            channel_id: channel_id.to_owned(),
        }))
    }

    /// 「運営によってキューから外された」通知。`admin_actions` の最新行から導出する。
    ///
    /// **黙って消さない。**キューから消えた理由が分からないと「バグでキューから外れた」と
    /// 受け取られる。運営が理由を書いていれば、それも本人にそのまま見せる。
    ///
    /// 出す条件は「いまキューにいない」ことと、外されたのが
    /// [`REMOVAL_NOTICE_WINDOW_HOURS`] 以内であること。窓を切るのは、キューの出入りに履歴が
    /// 無い（可変テーブル）ため「外された後に自分で入り直して自分で抜けた」を区別できず、
    /// 窓が無いと何日も前の通知が残り続けるから。開催は 21:00〜25:00 なので、1 回の開催と
    /// その後の確認に足りる長さにしてある。
    pub async fn staff_removal(
        &self,
        player_id: PlayerId,
        now: Timestamp,
    ) -> Result<Option<StaffRemoval>, QueueError> {
        let since = now - chrono::TimeDelta::hours(REMOVAL_NOTICE_WINDOW_HOURS);
        let row = sqlx::query!(
            "select occurred_at, reason from admin_actions \
             where kind = 'queue_removed' and target_player_id = $1 and occurred_at > $2 \
             order by id desc limit 1",
            player_id.0,
            since,
        )
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(|row| StaffRemoval {
            removed_at: row.occurred_at,
            reason: row.reason,
        }))
    }

    /// 表示スケールの現在レート。現在値テーブルを作らず追記ログから導出する。
    async fn player_rating(&self, player_id: PlayerId) -> Result<Option<f64>, QueueError> {
        let row = sqlx::query!(
            "select v.rating as \"rating!\" \
             from player_current_ratings v where v.player_id = $1",
            player_id.0,
        )
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(|row| row.rating))
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
