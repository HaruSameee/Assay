//! マッチメイカの永続化。**1 回の実行が 1 トランザクション**。
//!
//! 不変条件（このモジュールで守るもの）:
//! - 実行の最初に**プール全体の `pg_advisory_xact_lock`** を取る（07 §4-1）。同時に 2 つの
//!   マッチメイカが走らない。Phase 4 の `assign_available_vc` と同じキーなので、リカバリ側の
//!   VC 割当とも直列化する。
//! - **VC の空きを確認してから formation を作る**（00 §6）。空きが無ければ formation を
//!   作らずに次の実行へ回す。「座席が無いのに formation を作って 10 人のキュー待ちを捨てる」
//!   ことをしない。
//! - VC の確保・`formations` / `formation_players` の INSERT・`vc_assigned` の追記・
//!   **キューからの除去**をすべて同じトランザクションで行う。途中で落ちれば全部無かったことに
//!   なるので、「キューから消えたが formation が無い」状態が作れない。
//! - 予約状態をメモリに持たない。占有中の VC は未終了 formation の `vc_assigned` から導出する。
//! - `SELECT ... FOR UPDATE` は使わない。追記専用テーブルの UPDATE 権限を要求するため。
//! - 追記専用テーブルへの UPDATE / DELETE は書かない。DELETE するのは可変テーブルである
//!   `queue_entries` だけ。
//! - 冪等キーは formation ごとに `vc_assigned:<label>`（Phase 4 と同じ規約）。

use std::collections::{HashMap, HashSet};

use chrono::{FixedOffset, TimeZone, Utc};
use sqlx::{PgConnection, PgPool};
use tracing::{info, warn};

use crate::admin::PauseState;
use crate::lifecycle::{FormationEventKind, FormationId, FormationState, PlayerId, Timestamp};
use crate::persistence::{
    FormationLog, FormationRepository, NewEvent, NewFormation, NewFormationPlayer, RepositoryError,
    Team, VcAssignment,
};
use crate::queue::flex::{RoleAssignment, flex_compensation_owed};
use crate::queue::model::clamp_down_range;
use crate::rating::{DEFAULT_DISPLAY_MU, DEFAULT_DISPLAY_SIGMA, DISPLAY_SCALE};

use super::select::{
    Candidate, MATCHMAKING_PARAMS_VERSION, PREDICTED_WIN_PROB, ProposedMatch, RoleWish,
    select_matches,
};

/// 1 回の実行の結果。
#[derive(Debug, Clone, PartialEq, Default)]
pub struct RunOutcome {
    /// 見たキュー中の人数。
    pub candidates: usize,
    /// 確定した formation。
    pub created: Vec<FormationId>,
    /// 確定した formation と割り当てた VC。運用ログの表示にだけ使う。
    pub assignments: Vec<AssignedFormation>,
    /// VC の空きが無くて確定を打ち切ったか。**枯渇はログに残す**（00 §6）。
    pub vc_exhausted: bool,
    /// 今回見た候補のうち成立しなかった人数。
    pub unmatched: usize,
    /// 成立しなかった候補の最長待ち時間（秒）。
    pub longest_unmatched_wait_seconds: Option<u64>,
    /// 運営がマッチメイカを止めていたか。**止まっていてもループは回る**ので、
    /// キューの人数と待ち時間は普段どおり数える（何人待たせているかが見えなくなる）。
    pub paused: bool,
}

/// 成立した formation と、そのトランザクションで永続化した VC 割当。
#[derive(Debug, Clone, PartialEq)]
pub struct AssignedFormation {
    pub formation_id: FormationId,
    pub vc: VcAssignment,
    /// 07 §4-2 の選定結果。運営ログには集計だけを出す。
    pub selection: SelectionMetrics,
}

/// 1 formation が選ばれた理由を確認するための集計値。
///
/// 個人 ID、表示名、割り当てロールは持たない。成立時の運営ログ専用で、永続化しない。
#[derive(Debug, Clone, PartialEq, Default)]
pub struct SelectionMetrics {
    pub minimum_rating: f64,
    pub maximum_rating: f64,
    pub team_average_difference: f64,
    pub total_wait_seconds: u64,
    pub longest_wait_seconds: u64,
    pub shortest_wait_seconds: u64,
    pub first_choice_count: usize,
    pub second_choice_count: usize,
    pub third_choice_count: usize,
    pub flex_compensation_count: usize,
}

impl SelectionMetrics {
    fn from_proposal(proposal: &ProposedMatch) -> Self {
        let minimum_rating = proposal
            .players
            .iter()
            .map(|player| player.rating)
            .min_by(f64::total_cmp)
            .unwrap_or_default();
        let maximum_rating = proposal
            .players
            .iter()
            .map(|player| player.rating)
            .max_by(f64::total_cmp)
            .unwrap_or_default();
        let mut metrics = Self {
            minimum_rating,
            maximum_rating,
            team_average_difference: (proposal.team_average(Team::Red)
                - proposal.team_average(Team::Blue))
            .abs(),
            total_wait_seconds: proposal
                .players
                .iter()
                .map(|player| player.wait_seconds)
                .sum(),
            longest_wait_seconds: proposal
                .players
                .iter()
                .map(|player| player.wait_seconds)
                .max()
                .unwrap_or_default(),
            shortest_wait_seconds: proposal
                .players
                .iter()
                .map(|player| player.wait_seconds)
                .min()
                .unwrap_or_default(),
            flex_compensation_count: proposal
                .players
                .iter()
                .filter(|player| player.flex_compensation_applied)
                .count(),
            ..Self::default()
        };
        for player in &proposal.players {
            match player.assigned_preference {
                1 => metrics.first_choice_count += 1,
                2 => metrics.second_choice_count += 1,
                3 => metrics.third_choice_count += 1,
                priority => debug_assert!(false, "unexpected role preference: {priority}"),
            }
        }
        metrics
    }
}

/// キュー中プレイヤーの第 1 希望ロール別人数。`ROLES` と同じ固定の5ロール。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct FirstChoiceRoleCounts {
    pub top_carry: usize,
    pub top_support: usize,
    pub jungle: usize,
    pub bot_carry: usize,
    pub bot_tank: usize,
}

impl FirstChoiceRoleCounts {
    fn set(&mut self, role: &str, count: usize) {
        match role {
            "top_carry" => self.top_carry = count,
            "top_support" => self.top_support = count,
            "jungle" => self.jungle = count,
            "bot_carry" => self.bot_carry = count,
            "bot_tank" => self.bot_tank = count,
            _ => warn!(role, "unknown first-choice role in queue summary"),
        }
    }
}

/// 現在未終了の formation の内訳。係争はVCを解放済みでも裁定待ちなので別に数える。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ActiveFormationCounts {
    pub awaiting_host: usize,
    pub awaiting_code: usize,
    pub awaiting_reports: usize,
    pub awaiting_resolution: usize,
}

impl ActiveFormationCounts {
    pub fn total(self) -> usize {
        self.awaiting_host + self.awaiting_code + self.awaiting_reports + self.awaiting_resolution
    }
}

/// 直近15分に最終状態または係争へ進んだ formation の内訳。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct RecentCompletionCounts {
    pub confirmed: usize,
    pub nullified: usize,
    pub disputed: usize,
    pub abandoned: usize,
}

/// 15 分ごとの運用サマリ。
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct MatchmakerSummary {
    pub queued: usize,
    pub first_choice_roles: FirstChoiceRoleCounts,
    pub formed_last_15_minutes: usize,
    pub median_wait_seconds: Option<u64>,
    pub longest_wait_seconds: Option<u64>,
    pub vc_in_use: usize,
    pub vc_total: usize,
    pub active: ActiveFormationCounts,
    pub recent_completions: RecentCompletionCounts,
    pub pending_reporters: usize,
    pub formed_today: usize,
    pub confirmed_today: usize,
    /// マッチメイカが止められていれば、誰がいつ止めたか。
    pub paused: Option<PauseState>,
}

impl MatchmakerSummary {
    /// 開催時間外に空のサマリを流し続けない。係争もスタッフの対応待ちなので投稿対象。
    ///
    /// **停止中は必ず投稿する。**キューが空でも運営ログに定期的に出し続けないと、
    /// 止めたまま忘れる（当日いちばん危ない失敗）。
    pub fn should_publish(&self) -> bool {
        self.paused.is_some() || self.queued != 0 || self.active.total() != 0
    }
}

/// マッチメイカの永続化。接続はアプリ用ロールを想定する。
#[derive(Debug, Clone)]
pub struct MatchmakerRepository {
    pool: PgPool,
}

impl MatchmakerRepository {
    /// プールを包む。
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// 1 回分のマッチメイキングを実行する。
    ///
    /// `next_id` は formation ID の採番。テストが決定的な ID を差し込めるように引数にしてある。
    pub async fn run_once_with(
        &self,
        vc_pool: &[VcAssignment],
        now: Timestamp,
        next_id: &mut dyn FnMut() -> FormationId,
    ) -> Result<RunOutcome, RepositoryError> {
        let mut tx = self.pool.begin().await?;
        // 07 §4-1: 同時に複数のインスタンスが走らないこと。プール全体で 1 本
        crate::persistence::repository::lock_vc_pool(&mut tx).await?;

        let candidates = load_candidates(&mut tx).await?;
        let mut outcome = RunOutcome {
            candidates: candidates.len(),
            unmatched: candidates.len(),
            longest_unmatched_wait_seconds: longest_wait(&candidates, now),
            ..RunOutcome::default()
        };

        // 運営が止めていれば **formation を作らない**。ループ自体は回り続けるので、
        // キューの人数と最長待ち時間は数えたうえで戻る（止めている間も観測は続ける）。
        //
        // 停止状態は `admin_actions` の最新行から導出する。**同じトランザクション**で読むので、
        // 停止の追記とマッチ成立が同時に起きても、advisory lock がどちらか一方に決める。
        // プロセス内のフラグにしないのは、再起動で勝手に再開してしまうため。
        if let Some(pause) = crate::admin::repository::matchmaker_pause_on(&mut tx).await? {
            outcome.paused = true;
            tx.commit().await?;
            info!(
                queued = outcome.candidates,
                paused_by = pause.actor_discord_user_id,
                paused_since = %pause.since,
                "matchmaker is paused; forming no matches"
            );
            return Ok(outcome);
        }

        let proposals = select_matches(&candidates, now);
        if proposals.is_empty() {
            tx.commit().await?;
            return Ok(outcome);
        }

        // 占有中の VC は DB から導出する（メモリ上の予約状態を持たない）。
        // このトランザクションで確保したぶんも同じ集合に足していく
        let mut occupied = crate::persistence::repository::occupied_vc_labels(&mut tx).await?;
        let last_released = crate::persistence::repository::vc_last_released(&mut tx).await?;

        for proposal in &proposals {
            let Some(assignment) = pick_vc(vc_pool, &occupied, &last_released) else {
                // 座席が無いので **formation を作らない**。次の実行に回す
                outcome.vc_exhausted = true;
                warn!(
                    pending = proposals.len() - outcome.created.len(),
                    pool = vc_pool.len(),
                    "no free voice channel pair; leaving the players in the queue"
                );
                break;
            };
            occupied.insert(assignment.label.clone());

            let formation_id = next_id();
            let formation = to_new_formation(formation_id, now, proposal);
            crate::persistence::repository::insert_formation(&mut tx, &formation).await?;

            // 新しい formation なのでイベントは空。apply() を通して検証してから追記する
            let log = FormationLog {
                formation_id,
                created_at: now,
                events: Vec::new(),
                unreadable: Vec::new(),
            };
            let event = NewEvent::vc_assigned(
                &assignment.label,
                assignment.red_channel_id,
                assignment.blue_channel_id,
                now,
                format!("vc_assigned:{}", assignment.label),
            );
            crate::persistence::repository::append_locked(&mut tx, formation_id, &event, log)
                .await?;

            // キューからの除去は formation 確定と同じトランザクション
            let players: Vec<uuid::Uuid> = proposal
                .players
                .iter()
                .map(|player| player.player_id.0)
                .collect();
            let removed = sqlx::query!(
                "delete from queue_entries where player_id = any($1)",
                &players,
            )
            .execute(&mut *tx)
            .await?;
            debug_assert_eq!(removed.rows_affected() as usize, players.len());

            info!(
                formation = %formation_id,
                vc = %assignment.label,
                "match formed"
            );
            outcome.created.push(formation_id);
            outcome.assignments.push(AssignedFormation {
                formation_id,
                vc: assignment,
                selection: SelectionMetrics::from_proposal(proposal),
            });
        }

        let matched: HashSet<PlayerId> = proposals
            .iter()
            .take(outcome.created.len())
            .flat_map(|proposal| proposal.players.iter().map(|player| player.player_id))
            .collect();
        let unmatched: Vec<Candidate> = candidates
            .into_iter()
            .filter(|candidate| !matched.contains(&candidate.player_id))
            .collect();
        outcome.unmatched = unmatched.len();
        outcome.longest_unmatched_wait_seconds = longest_wait(&unmatched, now);

        tx.commit().await?;
        Ok(outcome)
    }

    /// 運営用の15分サマリを、永続化されたキューと formation ログから導出する。
    ///
    /// 状態カラムやメモリ上の予約を参照しない。各値は短い読み取りクエリで取得するため、
    /// 実行中に別の遷移があれば隣接する値がわずかにずれることはあるが、マッチ成立や
    /// Discord送信を待たせない運用観測値として十分である。
    pub async fn summary(
        &self,
        now: Timestamp,
        vc_total: usize,
    ) -> Result<MatchmakerSummary, RepositoryError> {
        let enqueued_at = sqlx::query_scalar!("select enqueued_at from queue_entries")
            .fetch_all(&self.pool)
            .await?;
        let mut waits: Vec<u64> = enqueued_at
            .into_iter()
            .map(|at| wait_seconds(now, at))
            .collect();
        waits.sort_unstable();
        let median_wait_seconds = median(&waits);
        let longest_wait_seconds = waits.last().copied();
        let formed = sqlx::query_scalar!(
            "select count(*)::bigint as \"count!\" from formations \
             where created_at > $1::timestamptz - interval '15 minutes' \
               and created_at <= $1::timestamptz",
            now,
        )
        .fetch_one(&self.pool)
        .await?;
        let first_choice_rows = sqlx::query!(
            "select role, count(*)::bigint as \"count!\" \
             from queue_entry_roles where priority = 1 group by role"
        )
        .fetch_all(&self.pool)
        .await?;
        let mut first_choice_roles = FirstChoiceRoleCounts::default();
        for row in first_choice_rows {
            first_choice_roles.set(&row.role, count_as_usize(row.count));
        }

        // `list_unfinished` は formation_events を Rust の状態機械で畳み込む。
        // `vc_assigned` の再送やホスト辞退を最終イベント種別だけで取り違えない。
        let unfinished = FormationRepository::new(self.pool.clone())
            .list_unfinished()
            .await?;
        let mut active = ActiveFormationCounts::default();
        for formation in &unfinished {
            match &formation.snapshot.state {
                FormationState::Matched | FormationState::AwaitingHost { .. } => {
                    active.awaiting_host += 1;
                }
                FormationState::AwaitingCode { .. } => active.awaiting_code += 1,
                FormationState::InProgress { .. } => active.awaiting_reports += 1,
                FormationState::Disputed => active.awaiting_resolution += 1,
                FormationState::Confirmed
                | FormationState::Nullified
                | FormationState::Voided
                | FormationState::Abandoned => {}
            }
        }

        let mut conn = self.pool.acquire().await?;
        let vc_in_use = crate::persistence::repository::occupied_vc_labels(&mut conn)
            .await?
            .len();
        // 停止中は運営ログへ定期的に出し続ける（止めたまま忘れない）
        let paused = crate::admin::repository::matchmaker_pause_on(&mut conn).await?;
        drop(conn);

        let final_kinds: Vec<String> = [
            FormationEventKind::Confirmed,
            FormationEventKind::Nullified,
            FormationEventKind::Disputed,
            FormationEventKind::Abandoned,
        ]
        .iter()
        .map(|kind| kind.as_str().to_owned())
        .collect();
        // formationごとの最新イベントだけを見る。例えば係争から裁定済みなら、係争と確定を
        // 同じ15分に二重計上しない。voided は依頼された4分類に含めない。
        let recent_rows = sqlx::query!(
            "select latest.kind, count(*)::bigint as \"count!\" \
             from ( \
                 select distinct on (formation_id) formation_id, kind, occurred_at \
                 from formation_events order by formation_id, id desc \
             ) latest \
             where latest.kind = any($1) \
               and latest.occurred_at > $2::timestamptz - interval '15 minutes' \
               and latest.occurred_at <= $2::timestamptz \
             group by latest.kind",
            &final_kinds[..],
            now,
        )
        .fetch_all(&self.pool)
        .await?;
        let mut recent_completions = RecentCompletionCounts::default();
        for row in recent_rows {
            match row.kind.as_str() {
                "confirmed" => recent_completions.confirmed = count_as_usize(row.count),
                "nullified" => recent_completions.nullified = count_as_usize(row.count),
                "disputed" => recent_completions.disputed = count_as_usize(row.count),
                "abandoned" => recent_completions.abandoned = count_as_usize(row.count),
                _ => unreachable!("query limits the event kinds"),
            }
        }

        // 報告ゲートと同じ意味: コード開示済みで、本人がまだ1件も報告しておらず、
        // voided / abandoned で義務が消えていないプレイヤーを重複なしで数える。
        let report_exempt: Vec<String> =
            [FormationEventKind::Voided, FormationEventKind::Abandoned]
                .iter()
                .map(|kind| kind.as_str().to_owned())
                .collect();
        let pending_reporters = sqlx::query_scalar!(
            "select count(distinct fp.player_id)::bigint as \"count!\" \
             from formation_players fp \
             where exists ( \
                 select 1 from formation_events code_posted \
                 where code_posted.formation_id = fp.formation_id and code_posted.kind = $1 \
             ) \
             and not exists ( \
                 select 1 from match_reports mr \
                 where mr.formation_id = fp.formation_id and mr.player_id = fp.player_id \
             ) \
             and not exists ( \
                 select 1 from formation_events exempt \
                 where exempt.formation_id = fp.formation_id and exempt.kind = any($2) \
             )",
            FormationEventKind::CodePosted.as_str(),
            &report_exempt[..],
        )
        .fetch_one(&self.pool)
        .await?;

        let day_started_at = start_of_jst_day(now);
        let formed_today = sqlx::query_scalar!(
            "select count(*)::bigint as \"count!\" from formations \
             where created_at >= $1 and created_at <= $2",
            day_started_at,
            now,
        )
        .fetch_one(&self.pool)
        .await?;
        let confirmed_today = sqlx::query_scalar!(
            "select count(distinct formation_id)::bigint as \"count!\" \
             from formation_events \
             where kind = $1 and occurred_at >= $2 and occurred_at <= $3",
            FormationEventKind::Confirmed.as_str(),
            day_started_at,
            now,
        )
        .fetch_one(&self.pool)
        .await?;
        Ok(MatchmakerSummary {
            queued: waits.len(),
            first_choice_roles,
            formed_last_15_minutes: usize::try_from(formed).unwrap_or(usize::MAX),
            median_wait_seconds,
            longest_wait_seconds,
            vc_in_use,
            vc_total,
            active,
            recent_completions,
            pending_reporters: count_as_usize(pending_reporters),
            formed_today: count_as_usize(formed_today),
            confirmed_today: count_as_usize(confirmed_today),
            paused,
        })
    }
}

fn wait_seconds(now: Timestamp, enqueued_at: Timestamp) -> u64 {
    u64::try_from(now.signed_duration_since(enqueued_at).num_seconds()).unwrap_or(0)
}

fn count_as_usize(value: i64) -> usize {
    usize::try_from(value).unwrap_or(usize::MAX)
}

fn start_of_jst_day(now: Timestamp) -> Timestamp {
    let jst = FixedOffset::east_opt(9 * 3600).expect("valid JST offset");
    let midnight = now
        .with_timezone(&jst)
        .date_naive()
        .and_hms_opt(0, 0, 0)
        .expect("valid midnight");
    jst.from_local_datetime(&midnight)
        .single()
        .expect("fixed offset has no ambiguous local time")
        .with_timezone(&Utc)
}

fn longest_wait(candidates: &[Candidate], now: Timestamp) -> Option<u64> {
    candidates
        .iter()
        .map(|candidate| wait_seconds(now, candidate.enqueued_at))
        .max()
}

fn median(sorted: &[u64]) -> Option<u64> {
    let upper = *sorted.get(sorted.len() / 2)?;
    if sorted.len() % 2 == 1 {
        Some(upper)
    } else {
        Some((sorted[sorted.len() / 2 - 1] + upper) / 2)
    }
}

/// 空いている組を LRU で選ぶ（Phase 4 の `assign_available_vc` と同じ基準）。
fn pick_vc(
    vc_pool: &[VcAssignment],
    occupied: &HashSet<String>,
    last_released: &HashMap<String, Timestamp>,
) -> Option<VcAssignment> {
    vc_pool
        .iter()
        .filter(|candidate| !occupied.contains(&candidate.label))
        // `None < Some`。一度も使っていない組が先、次に解放が古い順。同点は設定順
        .min_by_key(|candidate| last_released.get(&candidate.label).copied())
        .cloned()
}

/// 成立した組を永続化の形に変換する。表示スケールから内部スケールへ変換する唯一の場所。
fn to_new_formation(id: FormationId, now: Timestamp, proposal: &ProposedMatch) -> NewFormation {
    NewFormation {
        id,
        created_at: now,
        matchmaking_params_version: MATCHMAKING_PARAMS_VERSION,
        predicted_win_prob: PREDICTED_WIN_PROB,
        players: proposal
            .players
            .iter()
            .map(|player| NewFormationPlayer {
                player_id: player.player_id,
                team: player.team,
                role: player.role.clone(),
                role_was_flexed: player.role_was_flexed,
                // 凍結値は内部スケール（rating::DISPLAY_SCALE で割る）
                mu_at_match: player.rating / DISPLAY_SCALE,
                sigma_at_match: player.sigma / DISPLAY_SCALE,
            })
            .collect(),
    }
}

/// キュー中の全員を、マッチメイカの入力の形で読む。
///
/// レートは導出値なので現在値のテーブルを持たない。表示スケールの初期値に
/// `rating_adjustments` の差分と、確定した試合の増減（07 §2-2。`player_match_ratings`
/// ビュー）を足す。ビューは実体を持たないので、読むたびに追記ログから計算し直される。
///
/// σ は固定増減方式では動かない（07 §2 は μ しか触らない）ので、調整ログだけから作る。
///
/// フレックス補償は `formation_players.role_was_flexed` から導出する
/// （[`crate::queue::flex`]）。プレイヤーの可変列には持たない。
async fn load_candidates(conn: &mut PgConnection) -> Result<Vec<Candidate>, RepositoryError> {
    let rows = sqlx::query!(
        "select q.id, q.player_id, q.down_range, q.enqueued_at, \
                $1::double precision \
                    + coalesce((select sum(a.delta_mu) from rating_adjustments a \
                                where a.player_id = q.player_id), 0::double precision) \
                    + coalesce(m.delta, 0::double precision) \
                    as \"rating!\", \
                $2::double precision \
                    + coalesce((select sum(a.delta_sigma) from rating_adjustments a \
                                where a.player_id = q.player_id), 0::double precision) \
                    as \"sigma!\", \
                latest.formation_id as \"latest_formation?\", \
                latest.created_at as \"latest_matched_at?\", \
                latest.role_was_flexed as \"latest_role_was_flexed?\" \
         from queue_entries q \
         left join player_match_ratings m on m.player_id = q.player_id \
         left join lateral ( \
             select fp.formation_id, f.created_at, fp.role_was_flexed \
             from formation_players fp join formations f on f.id = fp.formation_id \
             where fp.player_id = q.player_id \
             order by f.created_at desc, f.id desc limit 1 \
         ) latest on true \
         order by q.enqueued_at, q.id",
        DEFAULT_DISPLAY_MU,
        DEFAULT_DISPLAY_SIGMA,
    )
    .fetch_all(&mut *conn)
    .await?;

    let entry_ids: Vec<uuid::Uuid> = rows.iter().map(|row| row.id).collect();
    let wishes = sqlx::query!(
        "select entry_id, role, priority from queue_entry_roles \
         where entry_id = any($1) order by entry_id, priority",
        &entry_ids,
    )
    .fetch_all(&mut *conn)
    .await?;
    let mut by_entry: HashMap<uuid::Uuid, Vec<RoleWish>> = HashMap::new();
    for wish in wishes {
        by_entry.entry(wish.entry_id).or_default().push(RoleWish {
            role: wish.role,
            priority: wish.priority,
        });
    }

    Ok(rows
        .into_iter()
        .map(|row| {
            // 直近 1 件だけを渡す。導出の規則は queue::flex 側にある（累積しない）
            let history: Vec<RoleAssignment> = match (
                row.latest_formation,
                row.latest_matched_at,
                row.latest_role_was_flexed,
            ) {
                (Some(formation_id), Some(matched_at), Some(role_was_flexed)) => {
                    vec![RoleAssignment {
                        formation_id: FormationId(formation_id),
                        matched_at,
                        role_was_flexed,
                    }]
                }
                _ => Vec::new(),
            };
            Candidate {
                player_id: PlayerId(row.player_id),
                rating: row.rating,
                sigma: row.sigma,
                down_range: clamp_down_range(row.down_range),
                enqueued_at: row.enqueued_at,
                roles: by_entry.remove(&row.id).unwrap_or_default(),
                flex_compensation: flex_compensation_owed(&history),
            }
        })
        .collect())
}
