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
use crate::host::HostResources;
use crate::lifecycle::{FormationEventKind, FormationId, FormationState, PlayerId, Timestamp};
use crate::persistence::{
    FormationLog, FormationRepository, NewEvent, NewFormation, NewFormationPlayer, RepositoryError,
    Team, VcAssignment,
};
use crate::queue::flex::{RoleAssignment, flex_compensation_owed};
use crate::queue::model::MatchingRanges;
use crate::rating::DISPLAY_SCALE;

use super::select::{
    Candidate, MATCHMAKING_PARAMS_VERSION, PREDICTED_WIN_PROB, ProposedMatch, RoleWish,
    ScanDirection, select_matches_scanning,
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
    /// 取り残されたうち、次のスキャンで判定用レートが底上げされる人数
    /// （実レート 1486 以下。[`super::boost`]）。ログに出す。
    pub boosted: usize,
    /// このスキャンで基準を試した順（上から / 下から）。ログに出す。
    pub direction: ScanDirection,
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
    /// 1 人ぶん数える。**第1希望のロールだけ**（希望のどこかに含まれる人ではない）。
    ///
    /// キュー画面のロール別内訳もここを通すので、運営が 15 分サマリで見る数字と
    /// 参加者がキュー画面で見る数字の定義が揃う。
    pub fn increment(&mut self, role: &str) {
        match role {
            "top_carry" => self.top_carry += 1,
            "top_support" => self.top_support += 1,
            "jungle" => self.jungle += 1,
            "bot_carry" => self.bot_carry += 1,
            "bot_tank" => self.bot_tank += 1,
            _ => warn!(role, "unknown first-choice role"),
        }
    }

    /// ロール識別子と人数の組。**5 ロールすべてを返す**（0 人でも 0 を返す。
    /// 出ないと「集計されていない」と読まれる）。並びは画面の並びに揃えてある。
    pub fn all(&self) -> [(&'static str, usize); 5] {
        [
            ("top_carry", self.top_carry),
            ("top_support", self.top_support),
            ("jungle", self.jungle),
            ("bot_carry", self.bot_carry),
            ("bot_tank", self.bot_tank),
        ]
    }

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
///
/// `Eq` は付けない（[`host::HostResources`] のロードアベレージが浮動小数点のため）。
#[derive(Debug, Clone, PartialEq, Default)]
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
    /// ホストのリソース使用状況。**読めなかった項目は `None`** で、サマリは落とさない。
    /// DB とは関係が無いので、詰めるのは [`super::runner::Matchmaker::summary`]。
    pub host: HostResources,
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
        direction: ScanDirection,
        next_id: &mut dyn FnMut() -> FormationId,
    ) -> Result<RunOutcome, RepositoryError> {
        let mut tx = self.pool.begin().await?;
        // 07 §4-1: 同時に複数のインスタンスが走らないこと。プール全体で 1 本
        crate::persistence::repository::lock_vc_pool(&mut tx).await?;

        // 07 §3: マッチング範囲は運営が変えられる（`current_matching_ranges`）。
        // **このスキャンで読むのはここ 1 回だけ。**advisory lock を取った同じ
        // トランザクションで読むので、運営の変更と選抜が同時に起きても、
        // ロックがどちらか一方に決める。ループの中で読み直すと 1 回の選抜の
        // 途中で基準がずれる。
        let ranges = crate::persistence::repository::matching_ranges(&mut tx).await?;
        let candidates = load_candidates(&mut tx, ranges).await?;
        let ally_blocks = load_active_ally_blocks(&mut tx, &candidates).await?;
        let mut outcome = RunOutcome {
            candidates: candidates.len(),
            unmatched: candidates.len(),
            longest_unmatched_wait_seconds: longest_wait(&candidates, now),
            direction,
            ..RunOutcome::default()
        };

        // 運営が止めていれば **formation を作らない**。ループ自体は回り続けるので、
        // キューの人数と最長待ち時間は数えたうえで戻る（止めている間も観測は続ける）。
        //
        // 止まる理由は 2 つあり、**手動の停止が常に勝つ**（`matchmaker_stop_on`）。
        // どちらも `admin_actions` / `matchmaker_schedule_changes` の最新行から導出する。
        // **同じトランザクション**で読むので、停止の追記とマッチ成立が同時に起きても
        // advisory lock がどちらか一方に決める。
        // プロセス内のフラグにしないのは、再起動で勝手に再開してしまうため。
        if let Some(stop) = crate::admin::repository::matchmaker_stop_on(&mut tx, now).await? {
            outcome.paused = true;
            tx.commit().await?;
            match stop {
                crate::admin::MatchmakerStop::Manual {
                    actor_discord_user_id,
                    since,
                    ..
                } => info!(
                    queued = outcome.candidates,
                    paused_by = actor_discord_user_id,
                    paused_since = %since,
                    "matchmaker is paused; forming no matches"
                ),
                crate::admin::MatchmakerStop::Scheduled => info!(
                    queued = outcome.candidates,
                    "matchmaker is outside its schedule; forming no matches"
                ),
            }
            return Ok(outcome);
        }

        let proposals = select_matches_scanning(&candidates, &ally_blocks, now, ranges, direction);
        if proposals.is_empty() {
            // 誰も組まれなかった。全員が「取り残された 1 回」
            outcome.boosted = count_unmatched_scans(&mut tx, &candidates).await?;
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
            // 配信中の申告は**キューの行から formation へ写す**（同じトランザクション。
            // 直後にキューの行は消えるので、ここで写さないと失われる）
            let streaming = streaming_players(&mut tx, proposal).await?;
            let formation = to_new_formation(formation_id, now, proposal, &streaming);
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
        // 取り残された人のカウンタを進める。成立した人の行はもう無い
        outcome.boosted = count_unmatched_scans(&mut tx, &unmatched).await?;

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
            // ホストのリソースは DB の話ではないので、ここでは既定（全項目 None）のまま。
            // 詰めるのは Matchmaker::summary（runner.rs）
            host: HostResources::default(),
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

/// red / blue のラベルを入れ替えるか。**`formation_id` だけから決める。**
///
/// 探索側（[`crate::matchmaker::select`]）は、対称な分け方を 1 回だけ数えるために
/// 「`assignment[0]` を必ず red に置く」という最適化を持っている。その `assignment[0]` は
/// レート降順に並べたプールの先頭、つまり**その試合の最高レートの人**である。
/// 結果として、探索の出力をそのまま使うと高レート側が常に red になる。
///
/// **探索は変えない。**出てきた分け方に対して、ここでラベルだけを入れ替える。
/// 誰と誰が同じ側かは 1 ミリも動かないので、「チーム平均レートの差が最小」という
/// 仕様も、凍結値（`mu_at_match` / `sigma_at_match`）もレート導出も影響を受けない。
///
/// 乱数やコイントスは使わない。同じ `formation_id` からは常に同じ結果が出る必要がある
/// ——ログを畳み直したときに再現しないと、記録と画面が食い違う。
pub fn teams_are_swapped(formation_id: FormationId) -> bool {
    // 16 バイトを XOR で畳んで最下位ビットを見る。特定のバイトだけを見ると、
    // ID の採番方式が変わったとき（v4 以外）に偏りが出る
    formation_id
        .0
        .as_bytes()
        .iter()
        .fold(0u8, |folded, byte| folded ^ byte)
        & 1
        == 1
}

/// 成立した組を永続化の形に変換する。表示スケールから内部スケールへ変換する唯一の場所。
///
/// **red / blue のラベルはここで決まる。**探索の出力は「最高レートの人が red」に
/// 固定されているので、`formation_id` から導いた向きで入れ替える。
///
/// `streaming` は成立時点で配信中と申告している人（`queue_entries.streaming`）。
/// 選抜には関与しないので `ProposedPlayer` には持たせず、ここで写すだけ。
fn to_new_formation(
    id: FormationId,
    now: Timestamp,
    proposal: &ProposedMatch,
    streaming: &HashSet<PlayerId>,
) -> NewFormation {
    let swapped = teams_are_swapped(id);
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
                team: if swapped {
                    player.team.flipped()
                } else {
                    player.team
                },
                role: player.role.clone(),
                role_was_flexed: player.role_was_flexed,
                streaming: streaming.contains(&player.player_id),
                // 凍結値は内部スケール（rating::DISPLAY_SCALE で割る）
                mu_at_match: player.rating / DISPLAY_SCALE,
                sigma_at_match: player.sigma / DISPLAY_SCALE,
            })
            .collect(),
    }
}

/// 成立した 10 人のうち、配信中と申告している人。**キューの行が消える前に読む。**
async fn streaming_players(
    conn: &mut PgConnection,
    proposal: &ProposedMatch,
) -> Result<HashSet<PlayerId>, sqlx::Error> {
    let ids: Vec<uuid::Uuid> = proposal
        .players
        .iter()
        .map(|player| player.player_id.0)
        .collect();
    let rows = sqlx::query_scalar!(
        "select player_id from queue_entries where player_id = any($1) and streaming",
        &ids,
    )
    .fetch_all(&mut *conn)
    .await?;
    Ok(rows.into_iter().map(PlayerId).collect())
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
async fn load_candidates(
    conn: &mut PgConnection,
    ranges: MatchingRanges,
) -> Result<Vec<Candidate>, RepositoryError> {
    let rows = sqlx::query!(
        "select q.id, q.player_id, q.down_range, q.enqueued_at, q.unmatched_scans, \
                v.rating as \"rating!\", v.sigma as \"sigma!\", \
                latest.formation_id as \"latest_formation?\", \
                latest.created_at as \"latest_matched_at?\", \
                latest.role_was_flexed as \"latest_role_was_flexed?\" \
         from queue_entries q \
         join player_current_ratings v on v.player_id = q.player_id \
         left join lateral ( \
             select fp.formation_id, f.created_at, fp.role_was_flexed \
             from formation_players fp join formations f on f.id = fp.formation_id \
             where fp.player_id = q.player_id \
             order by f.created_at desc, f.id desc limit 1 \
         ) latest on true \
         order by q.enqueued_at, q.id",
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
                down_range: ranges.clamp_down(row.down_range),
                enqueued_at: row.enqueued_at,
                roles: by_entry.remove(&row.id).unwrap_or_default(),
                flex_compensation: flex_compensation_owed(&history),
                unmatched_scans: row.unmatched_scans,
            }
        })
        .collect())
}

/// 取り残された人の `queue_entries.unmatched_scans` を 1 進める。
///
/// **このスキャンで見た人だけ**（`candidates` はスキャン開始時に読んだ行）。スキャン中に
/// 入ってきた人は次回から数える。止まっている間は呼ばない（呼び出し側が
/// `matchmaker_stop_on` の後ろに置く）。戻り値は次のスキャンで判定用レートが
/// 動く人数（1486 以下と、残った中でいちばん上。ログ用）。
async fn count_unmatched_scans(
    conn: &mut PgConnection,
    unmatched: &[Candidate],
) -> Result<usize, RepositoryError> {
    if unmatched.is_empty() {
        return Ok(0);
    }
    let player_ids: Vec<uuid::Uuid> = unmatched
        .iter()
        .map(|candidate| candidate.player_id.0)
        .collect();
    sqlx::query!(
        "update queue_entries set unmatched_scans = unmatched_scans + 1 \
         where player_id = any($1)",
        &player_ids,
    )
    .execute(&mut *conn)
    .await?;
    let top = super::boost::top_rating(unmatched.iter().map(|candidate| &candidate.rating));
    Ok(unmatched
        .iter()
        .filter(|candidate| super::boost::is_shifted(candidate.rating, top))
        .count())
}

async fn load_active_ally_blocks(
    conn: &mut PgConnection,
    candidates: &[Candidate],
) -> Result<super::select::AllyBlocks, RepositoryError> {
    let player_ids: Vec<uuid::Uuid> = candidates
        .iter()
        .map(|candidate| candidate.player_id.0)
        .collect();
    if player_ids.is_empty() {
        return Ok(super::select::AllyBlocks::new());
    }
    let rows = sqlx::query!(
        "select latest.player_id, latest.blocked_player_id \
         from ( \
             select distinct on (player_id, blocked_player_id) \
                    player_id, blocked_player_id, kind \
             from player_blocks \
             where player_id = any($1) \
             order by player_id, blocked_player_id, id desc \
         ) latest \
         where latest.kind = 'blocked'",
        &player_ids,
    )
    .fetch_all(&mut *conn)
    .await?;
    let mut blocks = super::select::AllyBlocks::new();
    for row in rows {
        blocks
            .entry(PlayerId(row.player_id))
            .or_default()
            .insert(PlayerId(row.blocked_player_id));
    }
    Ok(blocks)
}

#[cfg(test)]
mod tests {
    use chrono::{TimeDelta, TimeZone, Utc};
    use uuid::Uuid;

    use super::*;
    use crate::matchmaker::select::{Candidate, MATCH_SIZE, ROLES, RoleWish, select_matches};
    use crate::queue::model::DEFAULT_DOWN_RANGE;

    fn now() -> Timestamp {
        Utc.with_ymd_and_hms(2026, 9, 12, 21, 30, 0).unwrap()
    }

    /// 決定的な擬似乱数（splitmix64）。テストを安定させたまま、
    /// 連番ではない現実的な UUID を作るために使う。
    fn mix(mut seed: u64) -> u64 {
        seed = seed.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = seed;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    /// 試行番号から formation_id を作る。連番 UUID では偏りの検証にならない。
    fn formation_id(trial: u64) -> FormationId {
        let high = mix(trial * 2);
        let low = mix(trial * 2 + 1);
        FormationId(Uuid::from_u128(u128::from(high) << 64 | u128::from(low)))
    }

    /// ロールがちょうど 2 人ずつ、レートがばらけた 10 人。
    fn roster(trial: u64) -> Vec<Candidate> {
        (0..MATCH_SIZE)
            .map(|index| {
                let offset = ((index * 7 + trial as usize * 3) % 10) as f64 * 5.0;
                Candidate {
                    player_id: PlayerId(Uuid::from_u128(
                        u128::from(trial) * 100 + index as u128 + 1,
                    )),
                    rating: 1500.0 + offset,
                    sigma: 500.0,
                    down_range: DEFAULT_DOWN_RANGE,
                    enqueued_at: now() - TimeDelta::seconds(600),
                    roles: vec![RoleWish {
                        role: ROLES[index % ROLES.len()].to_owned(),
                        priority: 1,
                    }],
                    flex_compensation: false,
                    unmatched_scans: 0,
                }
            })
            .collect()
    }

    /// **同じ formation_id からは常に同じ向きが出る。**
    #[test]
    fn the_side_is_derived_from_the_formation_id_alone() {
        for trial in 0..50 {
            let id = formation_id(trial);
            let first = teams_are_swapped(id);
            for _ in 0..5 {
                assert_eq!(teams_are_swapped(id), first, "{id} で結果が揺れている");
            }
        }
        // 既知の値で固定する（採番方式を変えても気づけるように）
        assert!(!teams_are_swapped(FormationId(Uuid::from_u128(0))));
        assert!(teams_are_swapped(FormationId(Uuid::from_u128(1))));
    }

    /// **入れ替えても、誰と誰が同じ側かは変わらない。**
    ///
    /// チーム分割そのもの（＝仕様「チーム平均レートの差が最小」）と凍結値に
    /// 影響しないことを、同じ提案を 2 通りのラベルで永続化して確かめる。
    #[test]
    fn swapping_the_labels_keeps_the_split_and_the_frozen_values() {
        let proposal = select_matches(&roster(0), now()).remove(0);

        // 入れ替えが起きる ID と起きない ID を 1 つずつ選ぶ
        let plain = (0..200)
            .map(formation_id)
            .find(|id| !teams_are_swapped(*id))
            .expect("入れ替えない ID");
        let swapped = (0..200)
            .map(formation_id)
            .find(|id| teams_are_swapped(*id))
            .expect("入れ替える ID");

        let a = to_new_formation(plain, now(), &proposal, &HashSet::new());
        let b = to_new_formation(swapped, now(), &proposal, &HashSet::new());

        // 凍結値は 1 ミリも動かない
        for (left, right) in a.players.iter().zip(&b.players) {
            assert_eq!(left.player_id, right.player_id);
            assert_eq!(left.role, right.role);
            assert_eq!(left.mu_at_match, right.mu_at_match);
            assert_eq!(left.sigma_at_match, right.sigma_at_match);
            assert_eq!(left.team, right.team.flipped(), "ラベルが反転していない");
        }

        // 「同じ側にいる集合」も変わらない（ラベルが入れ替わるだけ）
        let side = |formation: &NewFormation, team: Team| {
            let mut ids: Vec<Uuid> = formation
                .players
                .iter()
                .filter(|player| player.team == team)
                .map(|player| player.player_id.0)
                .collect();
            ids.sort_unstable();
            ids
        };
        assert_eq!(side(&a, Team::Red), side(&b, Team::Blue));
        assert_eq!(side(&a, Team::Blue), side(&b, Team::Red));
    }

    /// **実測: 20 組で、高レート側が red になる回数。**
    ///
    /// 修正前は探索の最適化がそのまま出ていて 20/20 だった。
    #[test]
    fn the_higher_rated_side_is_red_about_half_the_time() {
        let mut red_is_higher = 0usize;
        let mut lines = Vec::new();

        for trial in 0..20u64 {
            let proposal = select_matches(&roster(trial), now()).remove(0);
            let id = formation_id(trial);
            let formation = to_new_formation(id, now(), &proposal, &HashSet::new());

            let mean = |team: Team| {
                let picked: Vec<f64> = formation
                    .players
                    .iter()
                    .filter(|player| player.team == team)
                    .map(|player| player.mu_at_match)
                    .collect();
                picked.iter().sum::<f64>() / picked.len() as f64
            };
            let red = mean(Team::Red);
            let blue = mean(Team::Blue);
            if red > blue {
                red_is_higher += 1;
            }
            lines.push(format!(
                "trial {trial:>2}: 高い側 = {:<4} / 入れ替え = {}",
                if red > blue { "red" } else { "blue" },
                if teams_are_swapped(id) {
                    "した"
                } else {
                    "しない"
                },
            ));
        }

        for line in &lines {
            println!("{line}");
        }
        println!("---");
        println!("高レート側が red になった回数: {red_is_higher} / 20");
        assert!(
            (6..=14).contains(&red_is_higher),
            "偏っている: {red_is_higher} / 20"
        );
    }
}
