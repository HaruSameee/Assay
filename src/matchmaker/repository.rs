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

use sqlx::{PgConnection, PgPool};
use tracing::{info, warn};

use crate::lifecycle::{FormationId, PlayerId, Timestamp};
use crate::persistence::{
    FormationLog, NewEvent, NewFormation, NewFormationPlayer, RepositoryError, VcAssignment,
};
use crate::queue::flex::{RoleAssignment, flex_compensation_owed};
use crate::rating::{DEFAULT_DISPLAY_MU, DEFAULT_DISPLAY_SIGMA, DISPLAY_SCALE};

use super::select::{
    Candidate, MATCHMAKING_PARAMS_VERSION, PREDICTED_WIN_PROB, ProposedMatch, RoleWish,
    select_matches,
};

/// 1 回の実行の結果。
#[derive(Debug, Clone, PartialEq, Eq, Default)]
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
}

/// 成立した formation と、そのトランザクションで永続化した VC 割当。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AssignedFormation {
    pub formation_id: FormationId,
    pub vc: VcAssignment,
}

/// 15 分ごとの運用サマリ。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MatchmakerSummary {
    pub queued: usize,
    pub formed_last_15_minutes: usize,
    pub median_wait_seconds: Option<u64>,
    pub longest_wait_seconds: Option<u64>,
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

    /// 現在のキューと直近 15 分の成立数を DB の時刻から集計する。
    pub async fn summary(&self, now: Timestamp) -> Result<MatchmakerSummary, RepositoryError> {
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
        Ok(MatchmakerSummary {
            queued: waits.len(),
            formed_last_15_minutes: usize::try_from(formed).unwrap_or(usize::MAX),
            median_wait_seconds,
            longest_wait_seconds,
        })
    }
}

fn wait_seconds(now: Timestamp, enqueued_at: Timestamp) -> u64 {
    u64::try_from(now.signed_duration_since(enqueued_at).num_seconds()).unwrap_or(0)
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
                down_range: row.down_range,
                enqueued_at: row.enqueued_at,
                roles: by_entry.remove(&row.id).unwrap_or_default(),
                flex_compensation: flex_compensation_owed(&history),
            }
        })
        .collect())
}
