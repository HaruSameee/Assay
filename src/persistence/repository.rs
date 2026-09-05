//! [`FormationRepository`]。書き込みは INSERT のみ。

use std::collections::HashMap;

use sqlx::{PgConnection, PgPool};
use uuid::Uuid;

use crate::lifecycle::{
    FormationEventKind, FormationId, FormationState, PlayerId, Timestamp, apply,
};

use super::codec::{EventRow, decode_row};
use super::error::RepositoryError;
use super::model::{
    AppendOutcome, EventId, FormationLog, NewEvent, NewFormation, NewReport, ReportId, RosterEntry,
    Team, UnfinishedFormation, VcAssignment,
};

/// 終端状態を作る種別。[`FormationRepository::list_unfinished`] の SQL 側の絞り込みに使う。
const TERMINAL_KINDS: [FormationEventKind; 3] = [
    FormationEventKind::Confirmed,
    FormationEventKind::Voided,
    FormationEventKind::Abandoned,
];

/// VC を解放する種別。`disputed` はスタッフ裁定待ちとしては未終了だが、VC の組を占有し続ける
/// 理由は無いのでプールからは解放する。
const VC_RELEASE_KINDS: [FormationEventKind; 4] = [
    FormationEventKind::Confirmed,
    FormationEventKind::Disputed,
    FormationEventKind::Voided,
    FormationEventKind::Abandoned,
];

/// 全 VC プールを直列化する固定の advisory-lock key。
///
/// formation 単位のロックだけでは、異なる formation が同じ空き組を同時に選べる。割当候補の
/// 選択と `vc_assigned` の INSERT は必ずこのロックを取った同一トランザクションで行う。
const VC_POOL_LOCK_KEY: i64 = 0x4153_5341_595F_5643;

/// 訂正チェーンの分岐を防ぐ部分 unique インデックス名（マイグレーションと一致させる）。
const SUPERSEDES_UNIQUE_INDEX: &str = "match_reports_supersedes_uniq";

/// プール全体のロック下での VC 割当結果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VcAssignmentOutcome {
    /// `matched` に VC を記録した。
    Assigned {
        /// 記録した組。
        assignment: VcAssignment,
        /// 追記結果。
        outcome: AppendOutcome,
    },
    /// 空きが無かったため `abandoned(vc_pool_exhausted)` を記録した。
    Exhausted {
        /// 追記結果。
        outcome: AppendOutcome,
    },
    /// プールロックを待つ間に別の処理がこの formation を進めた。
    AlreadyAdvanced {
        /// 現在状態。
        state: FormationState,
    },
}

/// formation とそのイベント・報告の永続化。
#[derive(Debug, Clone)]
pub struct FormationRepository {
    pool: PgPool,
}

impl FormationRepository {
    /// プールを包む。接続はアプリ用ロール（追記専用4テーブルの UPDATE / DELETE を持たない）を想定する。
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// formation を確定する（`formations` 1 行 + `formation_players` 10 行、1 トランザクション）。
    ///
    /// 状態は `matched`（イベント無し）。
    pub async fn create_formation(&self, formation: &NewFormation) -> Result<(), RepositoryError> {
        validate_formation(formation)?;
        let mut tx = self.pool.begin().await?;
        sqlx::query!(
            "insert into formations (id, created_at, matchmaking_params_version, predicted_win_prob) \
             values ($1, $2, $3, $4)",
            formation.id.0,
            formation.created_at,
            formation.matchmaking_params_version,
            formation.predicted_win_prob,
        )
        .execute(&mut *tx)
        .await?;
        for player in &formation.players {
            sqlx::query!(
                "insert into formation_players \
                 (formation_id, player_id, team, role, role_was_flexed, mu_at_match, sigma_at_match) \
                 values ($1, $2, $3, $4, $5, $6, $7)",
                formation.id.0,
                player.player_id.0,
                player.team.as_str(),
                player.role,
                player.role_was_flexed,
                player.mu_at_match,
                player.sigma_at_match,
            )
            .execute(&mut *tx)
            .await?;
        }
        tx.commit().await?;
        Ok(())
    }

    /// formation のイベント列を id 順に読む。読めない行も捨てずに返す。ロックは取らない。
    pub async fn load_events(
        &self,
        formation_id: FormationId,
    ) -> Result<FormationLog, RepositoryError> {
        let mut conn = self.pool.acquire().await?;
        let created_at = fetch_created_at(&mut conn, formation_id).await?;
        let rows = fetch_rows(&mut conn, formation_id.0).await?;
        Ok(build_log(formation_id, created_at, rows))
    }

    /// formation の編成を読む（`formation_players` に `players` / `roles` を結合）。
    ///
    /// 並びはチーム（red が先）、ロールの表示順、プレイヤー ID。ロックは取らない（編成は INSERT 後不変）。
    pub async fn load_roster(
        &self,
        formation_id: FormationId,
    ) -> Result<Vec<RosterEntry>, RepositoryError> {
        let rows = sqlx::query!(
            "select fp.player_id, p.discord_user_id, fp.team, fp.role, \
                    r.display_name as role_display_name, r.sort_order as role_sort_order, \
                    fp.mu_at_match \
             from formation_players fp \
             join players p on p.id = fp.player_id \
             join roles r on r.id = fp.role \
             where fp.formation_id = $1 \
             order by fp.team = $2 desc, r.sort_order, fp.player_id",
            formation_id.0,
            Team::ALL[0].as_str(),
        )
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter()
            .map(|row| {
                let team: Team = row.team.parse().map_err(|error| {
                    RepositoryError::InvalidFormation(format!(
                        "formation {formation_id}: player {}: {error}",
                        row.player_id
                    ))
                })?;
                Ok(RosterEntry {
                    player_id: PlayerId(row.player_id),
                    discord_user_id: row.discord_user_id,
                    team,
                    role: row.role,
                    role_display_name: row.role_display_name,
                    role_sort_order: row.role_sort_order,
                    mu_at_match: row.mu_at_match,
                })
            })
            .collect()
    }

    /// 各プレイヤーが直近にホストを承諾した時刻（`host_accepted` の最新 `occurred_at`）。
    ///
    /// [`crate::lifecycle::FormationPlayer::last_hosted_at`] の導出元。一度も無ければ含まれない。
    pub async fn last_hosted_at(
        &self,
        players: &[PlayerId],
    ) -> Result<HashMap<PlayerId, Timestamp>, RepositoryError> {
        let ids: Vec<Uuid> = players.iter().map(|player| player.0).collect();
        let rows = sqlx::query!(
            "select subject_player_id as \"player_id!\", max(occurred_at) as \"last_hosted_at!\" \
             from formation_events \
             where kind = $1 and subject_player_id = any($2) \
             group by subject_player_id",
            FormationEventKind::HostAccepted.as_str(),
            &ids[..],
        )
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .into_iter()
            .map(|row| (PlayerId(row.player_id), row.last_hosted_at))
            .collect())
    }

    /// VC の組（`vc_assigned` payload の `vc.label`）ごとの最終解放時刻。
    ///
    /// その組を使った formation を解放したイベントのうち最新の `occurred_at`。VC プールの LRU 割当の
    /// 入力。一度も使われていない組は含まれない。
    pub async fn vc_last_used(&self) -> Result<HashMap<String, Timestamp>, RepositoryError> {
        let released: Vec<String> = VC_RELEASE_KINDS
            .iter()
            .map(|kind| kind.as_str().to_owned())
            .collect();
        let rows = sqlx::query!(
            "select v.payload->'vc'->>'label' as \"label!\", max(released.occurred_at) as \"last_used!\" \
             from formation_events v \
             join formation_events released on released.formation_id = v.formation_id \
             where v.kind = $1 and released.kind = any($2) \
               and v.payload->'vc'->>'label' is not null \
             group by 1",
            FormationEventKind::VcAssigned.as_str(),
            &released[..],
        )
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .into_iter()
            .map(|row| (row.label, row.last_used))
            .collect())
    }

    /// プール全体を直列化して、空き VC を LRU で選び同じトランザクションに `vc_assigned` を追記する。
    ///
    /// Discord への投稿はこの後に行う。ここで先に割当だけを永続化するので、直後にプロセスが落ちても
    /// 再起動後の別 formation が同じ組を選ぶことはない。
    pub async fn assign_available_vc(
        &self,
        formation_id: FormationId,
        candidates: &[VcAssignment],
        occurred_at: Timestamp,
    ) -> Result<VcAssignmentOutcome, RepositoryError> {
        let mut tx = self.pool.begin().await?;
        lock_vc_pool(&mut tx).await?;
        let log = lock_and_load(&mut tx, formation_id).await?;
        let snapshot = log.snapshot();
        if snapshot.state != FormationState::Matched {
            tx.commit().await?;
            return Ok(VcAssignmentOutcome::AlreadyAdvanced {
                state: snapshot.state,
            });
        }

        let occupied = occupied_vc_labels(&mut tx).await?;
        let last_used = vc_last_released(&mut tx).await?;
        let selected = candidates
            .iter()
            .filter(|candidate| !occupied.contains(&candidate.label))
            // `None < Some`; never-used pairs first, then least recently released. Ties retain config order.
            .min_by_key(|candidate| last_used.get(&candidate.label).copied())
            .cloned();

        let outcome = match selected {
            Some(assignment) => {
                let event = NewEvent::vc_assigned(
                    &assignment.label,
                    assignment.red_channel_id,
                    assignment.blue_channel_id,
                    occurred_at,
                    format!("vc_assigned:{}", assignment.label),
                );
                let outcome = append_locked(&mut tx, formation_id, &event, log).await?;
                tx.commit().await?;
                VcAssignmentOutcome::Assigned {
                    assignment,
                    outcome,
                }
            }
            None => {
                let event = NewEvent::abandoned(
                    crate::lifecycle::AbandonReason::VcPoolExhausted,
                    occurred_at,
                    "abandoned:vc_pool_exhausted",
                );
                let outcome = append_locked(&mut tx, formation_id, &event, log).await?;
                tx.commit().await?;
                VcAssignmentOutcome::Exhausted { outcome }
            }
        };
        Ok(outcome)
    }

    /// Discord のユーザー ID からプレイヤー ID を引く。見つからないものは結果に含まれない。
    pub async fn find_players_by_discord_ids(
        &self,
        discord_user_ids: &[i64],
    ) -> Result<HashMap<i64, PlayerId>, RepositoryError> {
        let rows = sqlx::query!(
            "select id, discord_user_id from players where discord_user_id = any($1)",
            discord_user_ids,
        )
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .into_iter()
            .map(|row| (row.discord_user_id, PlayerId(row.id)))
            .collect())
    }

    /// このプレイヤーが参加していて、**本人がまだ報告していない** formation と、その導出状態。
    ///
    /// キューの報告ゲート（[`crate::registration::gate`]）の入力。解除条件は「自分が報告したこと」
    /// なので、見るのは**自分の報告の有無だけ**。`match_reports` の参照を `player_id = $1` に
    /// 限っており、他人の報告の内容も件数も読まない（報告のブラインド）。
    ///
    /// `voided` / `abandoned` を持つ formation は SQL 側で落とす（試合が成立しておらず報告義務が
    /// 生じない）。残りは状態を導出して呼び出し側が [`FormationState::requires_reports`] で絞る。
    pub async fn formations_without_report_from(
        &self,
        player: PlayerId,
    ) -> Result<Vec<(FormationId, FormationState)>, RepositoryError> {
        let never_reported: Vec<String> =
            [FormationEventKind::Voided, FormationEventKind::Abandoned]
                .iter()
                .map(|kind| kind.as_str().to_owned())
                .collect();
        let candidates = sqlx::query!(
            "select f.id, f.created_at from formation_players fp \
             join formations f on f.id = fp.formation_id \
             where fp.player_id = $1 \
               and not exists ( \
                   select 1 from match_reports mr \
                   where mr.formation_id = f.id and mr.player_id = $1 \
               ) \
               and not exists ( \
                   select 1 from formation_events e \
                   where e.formation_id = f.id and e.kind = any($2) \
               ) \
             order by f.created_at, f.id",
            player.0,
            &never_reported[..],
        )
        .fetch_all(&self.pool)
        .await?;
        if candidates.is_empty() {
            return Ok(Vec::new());
        }

        let ids: Vec<Uuid> = candidates.iter().map(|candidate| candidate.id).collect();
        let rows = sqlx::query_as!(
            EventRow,
            "select id, formation_id, occurred_at, kind, subject_player_id, payload, idempotency_key \
             from formation_events where formation_id = any($1) order by formation_id, id",
            &ids[..],
        )
        .fetch_all(&self.pool)
        .await?;
        let mut grouped: HashMap<Uuid, Vec<EventRow>> = HashMap::new();
        for row in rows {
            grouped.entry(row.formation_id).or_default().push(row);
        }
        Ok(candidates
            .into_iter()
            .map(|candidate| {
                let rows = grouped.remove(&candidate.id).unwrap_or_default();
                let log = build_log(FormationId(candidate.id), candidate.created_at, rows);
                (FormationId(candidate.id), log.snapshot().state)
            })
            .collect())
    }

    /// イベントを追記する。
    ///
    /// formation 単位のアドバイザリロックを取り、同一トランザクション内で現在状態を導出して
    /// [`apply`] で検証してから INSERT する。同じ formation への追記は直列化される。
    ///
    /// - 同じ冪等キーのイベントが既にあれば、内容が同じなら何も書かず `already_recorded` で返す。
    ///   内容が違えば [`RepositoryError::IdempotencyKeyReused`]。
    /// - 遷移が不正なら [`RepositoryError::Rejected`]。何も書かれない。
    pub async fn append_event(
        &self,
        formation_id: FormationId,
        event: &NewEvent,
    ) -> Result<AppendOutcome, RepositoryError> {
        let mut tx = self.pool.begin().await?;
        let log = lock_and_load(&mut tx, formation_id).await?;
        let outcome = append_locked(&mut tx, formation_id, event, log).await?;
        tx.commit().await?;
        Ok(outcome)
    }

    /// 未終了の formation を `created_at` 順に返す（起動時のリカバリ用）。
    ///
    /// SQL で「終端種別のイベントを 1 つも持たない formation」に絞ってから、Rust 側で状態を導出して
    /// 終端でないものだけを返す。終端種別のイベントがすべて不正（`Anomaly`）だった formation は
    /// SQL の絞り込みで漏れる。それは検証を通さず INSERT された行がある場合にしか起きない。
    ///
    /// `disputed` は終端ではないので、スタッフが裁定するまでこの一覧に残り続ける。
    /// その状態には期限が無い（[`crate::lifecycle::next_deadline`] は `None` を返す）ので、
    /// リカバリ側はタイマーを張らないこと。
    pub async fn list_unfinished(&self) -> Result<Vec<UnfinishedFormation>, RepositoryError> {
        let terminal: Vec<String> = TERMINAL_KINDS
            .iter()
            .map(|kind| kind.as_str().to_owned())
            .collect();
        let candidates = sqlx::query!(
            "select f.id, f.created_at from formations f \
             where not exists ( \
                 select 1 from formation_events e \
                 where e.formation_id = f.id and e.kind = any($1) \
             ) \
             order by f.created_at, f.id",
            &terminal[..],
        )
        .fetch_all(&self.pool)
        .await?;
        if candidates.is_empty() {
            return Ok(Vec::new());
        }

        let ids: Vec<Uuid> = candidates.iter().map(|candidate| candidate.id).collect();
        let rows = sqlx::query_as!(
            EventRow,
            "select id, formation_id, occurred_at, kind, subject_player_id, payload, idempotency_key \
             from formation_events where formation_id = any($1) order by formation_id, id",
            &ids[..],
        )
        .fetch_all(&self.pool)
        .await?;
        let mut grouped: HashMap<Uuid, Vec<EventRow>> = HashMap::new();
        for row in rows {
            grouped.entry(row.formation_id).or_default().push(row);
        }

        let mut unfinished = Vec::new();
        for candidate in candidates {
            let rows = grouped.remove(&candidate.id).unwrap_or_default();
            let log = build_log(FormationId(candidate.id), candidate.created_at, rows);
            let snapshot = log.snapshot();
            if !snapshot.state.is_terminal() {
                unfinished.push(UnfinishedFormation { log, snapshot });
            }
        }
        Ok(unfinished)
    }

    /// 報告を追記する。戻り値は受理した報告の id だけ。確定状態・係争の有無・フラグは返さない。
    ///
    /// formation をロックして現在状態を導出し、報告を受け付ける状態
    /// （[`crate::lifecycle::FormationState::requires_reports`]）でなければ拒否する。
    /// `supersedes_id` は同じプレイヤー・同じ formation の報告で、まだ打ち消されていないものに限る。
    /// 他人の報告は読まない（ブラインド）。
    pub async fn append_report(&self, report: &NewReport) -> Result<ReportId, RepositoryError> {
        let mut tx = self.pool.begin().await?;
        let snapshot = lock_and_load(&mut tx, report.formation_id)
            .await?
            .snapshot();
        if !snapshot.state.requires_reports() {
            return Err(RepositoryError::ReportsNotAccepted {
                formation_id: report.formation_id,
            });
        }
        if let Some(previous) = report.supersedes_id {
            let owner = sqlx::query!(
                "select formation_id, player_id from match_reports where id = $1",
                previous.0
            )
            .fetch_optional(&mut *tx)
            .await?;
            let same_owner = owner.is_some_and(|owner| {
                owner.formation_id == report.formation_id.0 && owner.player_id == report.player_id.0
            });
            if !same_owner {
                return Err(RepositoryError::SupersedeMismatch(previous));
            }
        }
        let inserted = sqlx::query!(
            "insert into match_reports \
             (formation_id, player_id, reported_at, outcome, raw_result, supersedes_id) \
             values ($1, $2, $3, $4, $5, $6) returning id",
            report.formation_id.0,
            report.player_id.0,
            report.reported_at,
            report.outcome.as_str(),
            report.raw_result,
            report.supersedes_id.map(|report| report.0),
        )
        .fetch_one(&mut *tx)
        .await
        .map_err(|error| map_report_insert_error(error, report.supersedes_id))?;
        tx.commit().await?;
        Ok(ReportId(inserted.id))
    }

    /// Phase 5 の期限判定専用。報告内容・報告者・件数を返さず、HTTP 層には公開しない。
    pub(crate) async fn first_report_at(
        &self,
        formation_id: FormationId,
    ) -> Result<Option<Timestamp>, RepositoryError> {
        let row = sqlx::query!(
            "select min(reported_at) as \"first_report_at?\" from match_reports where formation_id = $1",
            formation_id.0,
        )
        .fetch_one(&self.pool)
        .await?;
        Ok(row.first_report_at)
    }
}

/// `append_event` とプール割当が共用する、ロック取得済みトランザクション内の追記。
async fn append_locked(
    conn: &mut PgConnection,
    formation_id: FormationId,
    event: &NewEvent,
    log: FormationLog,
) -> Result<AppendOutcome, RepositoryError> {
    let candidate = event.to_core();
    if let Some(existing) = log
        .events
        .iter()
        .find(|stored| stored.event.idempotency_key == candidate.idempotency_key)
    {
        let same = existing.event.kind == candidate.kind
            && existing.event.subject == candidate.subject
            && existing.event.payload == candidate.payload;
        if !same {
            return Err(RepositoryError::IdempotencyKeyReused {
                formation_id,
                idempotency_key: candidate.idempotency_key,
            });
        }
        let snapshot = log.snapshot();
        return Ok(AppendOutcome {
            event_id: existing.id,
            state: snapshot.state,
            already_recorded: true,
            anomalies: snapshot.anomalies,
        });
    }
    if log
        .unreadable
        .iter()
        .any(|row| row.idempotency_key == candidate.idempotency_key)
    {
        return Err(RepositoryError::IdempotencyKeyReused {
            formation_id,
            idempotency_key: candidate.idempotency_key,
        });
    }

    let snapshot = log.snapshot();
    let next = apply(&snapshot.state, &candidate)?;
    let inserted = sqlx::query!(
        "insert into formation_events \
         (formation_id, occurred_at, kind, subject_player_id, payload, idempotency_key) \
         values ($1, $2, $3, $4, $5, $6) returning id",
        formation_id.0,
        event.occurred_at,
        event.kind.as_str(),
        event.subject.map(|player| player.0),
        event.payload,
        event.idempotency_key,
    )
    .fetch_one(&mut *conn)
    .await?;
    Ok(AppendOutcome {
        event_id: EventId(inserted.id),
        state: next,
        already_recorded: false,
        anomalies: snapshot.anomalies,
    })
}

/// プール共有資源を直列化する。formation ロックより先に取り、ロック順を一意にする。
async fn lock_vc_pool(conn: &mut PgConnection) -> Result<(), sqlx::Error> {
    sqlx::query!("select pg_advisory_xact_lock($1)", VC_POOL_LOCK_KEY)
        .execute(&mut *conn)
        .await?;
    Ok(())
}

async fn occupied_vc_labels(
    conn: &mut PgConnection,
) -> Result<std::collections::HashSet<String>, sqlx::Error> {
    let released: Vec<String> = VC_RELEASE_KINDS
        .iter()
        .map(|kind| kind.as_str().to_owned())
        .collect();
    let rows = sqlx::query!(
        "select distinct on (assigned.formation_id) \
                assigned.payload->'vc'->>'label' as \"label?\" \
         from formation_events assigned \
         where assigned.kind = $1 \
           and not exists ( \
               select 1 from formation_events released \
               where released.formation_id = assigned.formation_id and released.kind = any($2) \
           ) \
         order by assigned.formation_id, assigned.id desc",
        FormationEventKind::VcAssigned.as_str(),
        &released[..],
    )
    .fetch_all(&mut *conn)
    .await?;
    Ok(rows.into_iter().filter_map(|row| row.label).collect())
}

async fn vc_last_released(
    conn: &mut PgConnection,
) -> Result<HashMap<String, Timestamp>, sqlx::Error> {
    let released: Vec<String> = VC_RELEASE_KINDS
        .iter()
        .map(|kind| kind.as_str().to_owned())
        .collect();
    let rows = sqlx::query!(
        "select assigned.payload->'vc'->>'label' as \"label!\", \
                max(released.occurred_at) as \"last_used!\" \
         from formation_events assigned \
         join formation_events released on released.formation_id = assigned.formation_id \
         where assigned.kind = $1 and released.kind = any($2) \
           and assigned.payload->'vc'->>'label' is not null \
         group by 1",
        FormationEventKind::VcAssigned.as_str(),
        &released[..],
    )
    .fetch_all(&mut *conn)
    .await?;
    Ok(rows
        .into_iter()
        .map(|row| (row.label, row.last_used))
        .collect())
}

/// formation 単位の直列化。トランザクション終了時に自動解放されるアドバイザリロック。
///
/// `SELECT ... FOR UPDATE` と `LOCK TABLE` は対象テーブルの UPDATE 権限を要求するので、
/// UPDATE を REVOKE したアプリ用ロールでは `permission denied` になる。アドバイザリロックは
/// テーブル権限を要求しない。64bit ハッシュの衝突は無関係な formation 同士が一瞬直列化するだけで、
/// 正しさには影響しない。
async fn lock_formation(
    conn: &mut PgConnection,
    formation_id: FormationId,
) -> Result<(), sqlx::Error> {
    sqlx::query!(
        "select pg_advisory_xact_lock(hashtextextended($1::uuid::text, 0))",
        formation_id.0
    )
    .execute(&mut *conn)
    .await?;
    Ok(())
}

/// ロックを取ってから formation の存在を確認し、イベント列を読む。
/// **順序を逆にしないこと。**先に読むと、ロック待ちの間に積まれたイベントを見ずに検証してしまう。
async fn lock_and_load(
    conn: &mut PgConnection,
    formation_id: FormationId,
) -> Result<FormationLog, RepositoryError> {
    lock_formation(conn, formation_id).await?;
    let created_at = fetch_created_at(conn, formation_id).await?;
    let rows = fetch_rows(conn, formation_id.0).await?;
    Ok(build_log(formation_id, created_at, rows))
}

/// `formations` の行の存在確認（SELECT 権限だけで足りる）。無ければ [`RepositoryError::FormationNotFound`]。
async fn fetch_created_at(
    conn: &mut PgConnection,
    formation_id: FormationId,
) -> Result<Timestamp, RepositoryError> {
    sqlx::query!(
        "select created_at from formations where id = $1",
        formation_id.0
    )
    .fetch_optional(&mut *conn)
    .await?
    .map(|row| row.created_at)
    .ok_or(RepositoryError::FormationNotFound(formation_id))
}

async fn fetch_rows(
    conn: &mut PgConnection,
    formation_id: Uuid,
) -> Result<Vec<EventRow>, sqlx::Error> {
    sqlx::query_as!(
        EventRow,
        "select id, formation_id, occurred_at, kind, subject_player_id, payload, idempotency_key \
         from formation_events where formation_id = $1 order by id",
        formation_id
    )
    .fetch_all(&mut *conn)
    .await
}

fn build_log(
    formation_id: FormationId,
    created_at: Timestamp,
    rows: Vec<EventRow>,
) -> FormationLog {
    let mut events = Vec::with_capacity(rows.len());
    let mut unreadable = Vec::new();
    for row in rows {
        match decode_row(row) {
            Ok(stored) => events.push(stored),
            Err(row) => unreadable.push(row),
        }
    }
    FormationLog {
        formation_id,
        created_at,
        events,
        unreadable,
    }
}

/// 打ち消し先が既に打ち消されていた場合（部分 unique 違反）を専用のエラーにする。
fn map_report_insert_error(error: sqlx::Error, supersedes_id: Option<ReportId>) -> RepositoryError {
    if let (sqlx::Error::Database(database), Some(previous)) = (&error, supersedes_id)
        && database.is_unique_violation()
        && database.constraint() == Some(SUPERSEDES_UNIQUE_INDEX)
    {
        return RepositoryError::AlreadySuperseded(previous);
    }
    RepositoryError::Database(error)
}

/// 5v5 の構成か（10人、各チーム5人、重複なし）。
fn validate_formation(formation: &NewFormation) -> Result<(), RepositoryError> {
    let total = formation.players.len();
    if total != 10 {
        return Err(RepositoryError::InvalidFormation(format!(
            "expected 10 players, got {total}"
        )));
    }
    let red = formation
        .players
        .iter()
        .filter(|player| player.team == Team::Red)
        .count();
    if red != 5 {
        return Err(RepositoryError::InvalidFormation(format!(
            "expected 5 players per team, got {red} in team red"
        )));
    }
    let mut ids: Vec<Uuid> = formation
        .players
        .iter()
        .map(|player| player.player_id.0)
        .collect();
    ids.sort();
    ids.dedup();
    if ids.len() != total {
        return Err(RepositoryError::InvalidFormation(
            "the same player appears more than once".into(),
        ));
    }
    Ok(())
}
