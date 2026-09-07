//! Discord アダプタ本体。**通知の送出**と、イベント時刻から導出する Phase 5 のタイムアウトを扱う。
//!
//! **操作はここには無い。**ホスト打診への応答・ルーム ID の投稿・報告はすべてマッチ画面
//! （[`crate::match_screen`]）の HTTP エンドポイントで受ける。ボタンもモーダルも作らない。
//! Discord に残すのは通知だけ — サイトを開いていない人に「あなたの番です」を伝える手段が
//! 他に無く、打診は候補あたり数分、報告は 30 分の期限がある。通知が無ければ、画面を
//! 開いている人だけが進行できる仕組みになる。
//!
//! 流れ（**すべて再開ループが DB の状態から出す**。サイトはイベントを積むだけ）:
//! 1. `matched` → VC の組を LRU で確保 → 割り当て表を**チームごとの本文で**両 VC に投稿 →
//!    招集チャンネルにも**チームごとに**投稿 → `vc_assigned`
//! 2. `awaiting_host`（候補未指名）→ 候補の側の VC に打診の通知、招集チャンネルにホストの
//!    通知 → `host_assigned`
//! 3. `awaiting_code` → 承諾を VC の打診メッセージに反映（[`Adapter::ensure_host_accepted_notice`]）
//! 4. `in_progress` → ルーム ID を両 VC に開示し、報告の催促を出す
//!    （[`Adapter::ensure_code_revealed`]）。**打ち直しはコードが変わるので必ず届く**
//! 5. `disputed` → 運営専用チャンネルへ通知
//!
//! 失敗経路（すべて `abandoned` で解散し、理由と詳細を payload に残す。10 人を黙って拘束しない）:
//! - 空いている VC が無い → `vc_pool_exhausted`
//! - 割り当て表・打診・開示の投稿や編集が N 回失敗 → `discord_call_failed`（payload に操作名）
//!
//! 原則:
//! - 副作用の記録が正。Discord に投稿したら、その ID を必ず `formation_events.payload` に入れる。
//!   失敗も同じ payload に残す（stdout のログは記録ではない）。再起動後は [`DiscordRecord`] を
//!   ログから導出して続きを操作する。Discord 側を検索し直さない。
//! - **投稿済みかどうかもログから導出する。**プロセス内のフラグに持たない（再起動で消える）。
//! - VC チャットのメッセージは消さない（VC は使い回すが、過去のルーム ID は試合終了で無効）。
//!   すべての投稿に見出し（VC ラベル・時刻・試合 ID）を付ける。
//! - **相手チームの構成を載せない。**割り当て表も招集もチームごとに分ける。サイト側だけ
//!   隠しても、VC の投稿に相手のロールが載っていれば意味が無い。
//! - 履歴に残った古いボタンは Discord 側で無効化しない。押されたら、本人にだけ行き先を返す。
//!
//! マッチメイカ側への制約（Phase 4 の範囲外、03 に書き留めること）:
//! formation を確定させる前に VC プールの空きを確認する。座席が無いのに formation を作ると、
//! ここで `vc_pool_exhausted` になり、キュー待ち時間を丸ごと捨てる。

use std::collections::HashMap;
use std::future::Future;
use std::sync::Arc;

use chrono::Utc;
use serenity::model::id::{ChannelId, GuildId, InteractionId, MessageId, UserId};
use tracing::{debug, error, info, warn};

use crate::lifecycle::{
    AbandonReason, Anomaly, Deadline, Formation, FormationEventKind, FormationId, FormationPlayer,
    FormationState, HostAction, HostSelector, LifecycleConfig, PlayerId, next_deadline,
    next_host_action,
};
use crate::matchmaker::{MatchmakerSummary, RunOutcome};
use crate::persistence::{
    FormationLog, FormationRepository, NewDiscordFailure, NewEvent, ReportDecision,
    RepositoryError, RosterEntry, Team, UnfinishedFormation, VcAssignment, VcAssignmentOutcome,
};

use super::config::DiscordConfig;
use super::gateway::{Gateway, GatewayError, InteractionRef, InteractionResponse, OutgoingMessage};
use super::payload::{self, DiscordFailure, DiscordRecord};
use super::render;
use super::vc_pool::{VcPair, VcPool};

/// アダプタのエラー。Discord の失敗はここには出てこない（payload に記録して `abandoned` にする）。
#[derive(Debug, thiserror::Error)]
pub enum AdapterError {
    /// 永続化層。
    #[error(transparent)]
    Repository(#[from] RepositoryError),
    /// ログの記録と現在状態が食い違っている（VC 割当が無い、編成に居ない、など）。
    #[error("formation {0}: {1}")]
    Inconsistent(FormationId, String),
}

/// ゲートウェイから受け取ったインタラクションを正規化したもの。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IncomingInteraction {
    /// インタラクション ID（冪等キー）。
    pub id: InteractionId,
    /// 応答用トークン。
    pub token: String,
    /// 押した / 送信したユーザー。
    pub user: UserId,
    /// 発生したギルド。
    pub guild: Option<GuildId>,
    /// 種類。
    pub kind: IncomingKind,
}

/// インタラクションの種類。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IncomingKind {
    /// ボタン。
    Button {
        /// ボタンの `custom_id`。
        custom_id: String,
    },
    /// モーダルの送信。
    ModalSubmit {
        /// モーダルの `custom_id`。
        custom_id: String,
        /// 入力欄の `custom_id` → 値。
        inputs: HashMap<String, String>,
    },
}

impl IncomingInteraction {
    fn reference(&self) -> InteractionRef {
        InteractionRef {
            id: self.id,
            token: self.token.clone(),
        }
    }

    fn custom_id(&self) -> &str {
        match &self.kind {
            IncomingKind::Button { custom_id } | IncomingKind::ModalSubmit { custom_id, .. } => {
                custom_id
            }
        }
    }
}

/// Discord アダプタ。
pub struct Adapter {
    repo: FormationRepository,
    gateway: Arc<dyn Gateway>,
    config: DiscordConfig,
    lifecycle: LifecycleConfig,
    selector: Box<dyn HostSelector + Send + Sync>,
    vc_pool: VcPool,
    /// サイトの起点（`https://match.aynm.dev`）。通知に添えるマッチ画面 URL を組み立てる。
    /// **必須の引数にしてある** — 操作はサイトにしか無いので、行き先の無い通知は用を成さない。
    site_base_url: String,
    /// Ready 時と定期走査が同じ副作用を二重送信しないためのプロセス内直列化。
    /// 期限や VC 占有の正はここに持たず、DB のイベント列と advisory lock にある。
    resume_lock: tokio::sync::Mutex<()>,
}

impl Adapter {
    /// 組み立てる。`vc_pool` は起動時に名前から解決した組（[`super::resolve_vc_pool`]）。
    pub fn new(
        repo: FormationRepository,
        gateway: Arc<dyn Gateway>,
        config: DiscordConfig,
        vc_pool: Vec<VcPair>,
        lifecycle: LifecycleConfig,
        selector: Box<dyn HostSelector + Send + Sync>,
        site_base_url: impl Into<String>,
    ) -> Self {
        Self {
            repo,
            gateway,
            config,
            lifecycle,
            selector,
            vc_pool: VcPool::new(vc_pool),
            site_base_url: site_base_url.into(),
            resume_lock: tokio::sync::Mutex::new(()),
        }
    }

    /// この formation のマッチ画面 URL。通知には必ず添える。
    fn match_url(&self, formation: FormationId) -> String {
        render::match_url(&self.site_base_url, formation)
    }

    /// 永続化層。
    pub fn repository(&self) -> &FormationRepository {
        &self.repo
    }

    /// VC プール。
    pub fn vc_pool(&self) -> &VcPool {
        &self.vc_pool
    }

    /// 成立があった1回分だけ、運営専用チャンネルへマッチメイカ結果を投稿する。
    ///
    /// formation は呼び出し前にコミット済み。投稿失敗は監査ログへ追記して `Ok` にし、
    /// 成立や次回のマッチメイカ実行を巻き戻さない。
    pub async fn publish_matchmaker_run_log(
        &self,
        outcome: &RunOutcome,
        occurred_at: chrono::DateTime<Utc>,
    ) -> Result<(), AdapterError> {
        if outcome.assignments.is_empty() {
            return Ok(());
        }
        let context = serde_json::json!({
            "formed": outcome.assignments.iter().map(|assigned| assigned.formation_id.to_string()).collect::<Vec<_>>(),
            "queued": outcome.candidates,
            "vc_exhausted": outcome.vc_exhausted,
        });
        self.publish_operational_log(
            "matchmaker_run_log",
            render::matchmaker_run_log(outcome, occurred_at),
            context,
            occurred_at,
        )
        .await
    }

    /// 15分ごとの運用サマリを投稿する。キューも進行中formationも無いときは投稿しない。
    pub async fn publish_matchmaker_summary(
        &self,
        summary: &MatchmakerSummary,
        occurred_at: chrono::DateTime<Utc>,
    ) -> Result<(), AdapterError> {
        if !summary.should_publish() {
            return Ok(());
        }
        self.publish_operational_log(
            "matchmaker_summary",
            render::matchmaker_summary(summary, occurred_at),
            serde_json::json!({ "window_minutes": 15 }),
            occurred_at,
        )
        .await
    }

    async fn publish_operational_log(
        &self,
        kind: &str,
        message: OutgoingMessage,
        context: serde_json::Value,
        occurred_at: chrono::DateTime<Utc>,
    ) -> Result<(), AdapterError> {
        let operation = format!("send:{kind}");
        if let Err(failure) = self
            .retry(&operation, || {
                self.gateway
                    .send_message(self.config.matchmaker_log_channel_id, &message)
            })
            .await
        {
            error!(kind, error = %failure.error, "matchmaker Discord log failed; loop continues");
            self.repo
                .append_discord_failure(&NewDiscordFailure {
                    formation_id: None,
                    kind: kind.to_owned(),
                    operation: failure.operation,
                    error: failure.error,
                    attempts: i32::try_from(failure.attempts).unwrap_or(i32::MAX),
                    context,
                    occurred_at,
                })
                .await?;
        }
        Ok(())
    }

    /// 起動時と定期走査時に、未終了の formation をイベント時刻から再開する唯一の入口。
    ///
    /// プロセス内のタイマー状態は持たない。期限切れならまず追記イベントを記録し、その結果の状態を
    /// 同じ関数で再度読む。したがって再起動直後に期限切れが多数あっても、通常の遷移と同じ経路になる。
    pub async fn resume_unfinished(&self) -> Result<(), AdapterError> {
        let _scan = self.resume_lock.lock().await;
        let unfinished = self.repo.list_unfinished().await?;
        info!(count = unfinished.len(), "resuming unfinished formations");
        let now = Utc::now();
        for formation in unfinished {
            let id = formation.log.formation_id;
            let result = self.resume_one(formation, now).await;
            if let Err(error) = result {
                error!(formation = %id, %error, "failed to resume formation");
            }
        }
        Ok(())
    }

    /// `matched` の formation に VC の組を割り当て、割り当て表を投稿し、最初のホスト候補を打診する。
    ///
    /// 既に `matched` でなければ何もしない（再送に安全）。Discord 側の失敗は `abandoned` として
    /// 記録し、`Ok(())` を返す（呼び出し側が失敗として扱う必要は無い。ログを読めば分かる）。
    pub async fn start_formation(&self, id: FormationId) -> Result<(), AdapterError> {
        let _operation = self.resume_lock.lock().await;
        self.start_formation_locked(id).await
    }

    /// `resume_lock` を保持した呼び出し元専用の開始処理。
    async fn start_formation_locked(&self, id: FormationId) -> Result<(), AdapterError> {
        let assignments: Vec<VcAssignment> = self
            .vc_pool
            .pairs()
            .iter()
            .map(|pair| VcAssignment {
                label: pair.label.clone(),
                red_channel_id: pair.red.get(),
                blue_channel_id: pair.blue.get(),
            })
            .collect();
        match self
            .repo
            .assign_available_vc(id, &assignments, Utc::now())
            .await?
        {
            VcAssignmentOutcome::Assigned {
                assignment,
                outcome,
            } => {
                log_anomalies(id, &outcome.anomalies);
                info!(
                    formation = %id,
                    vc = %assignment.label,
                    red = assignment.red_channel_id,
                    blue = assignment.blue_channel_id,
                    "voice channels assigned"
                );
                let log = self.repo.load_events(id).await?;
                self.resume_vc_assignment(id, &log).await
            }
            VcAssignmentOutcome::Exhausted { outcome } => {
                log_anomalies(id, &outcome.anomalies);
                warn!(formation = %id, "no free voice channel pair; abandoned");
                Ok(())
            }
            VcAssignmentOutcome::AlreadyAdvanced { state } => {
                debug!(formation = %id, %state, "already started");
                if matches!(state, FormationState::AwaitingHost { pending: None, .. }) {
                    let log = self.repo.load_events(id).await?;
                    self.resume_vc_assignment(id, &log).await
                } else {
                    Ok(())
                }
            }
        }
    }

    async fn resume_one(
        &self,
        mut formation: UnfinishedFormation,
        now: crate::lifecycle::Timestamp,
    ) -> Result<(), AdapterError> {
        loop {
            let id = formation.log.formation_id;
            log_anomalies(id, &formation.snapshot.anomalies);
            let first_report_at =
                if matches!(formation.snapshot.state, FormationState::InProgress { .. }) {
                    self.repo.first_report_at(id).await?
                } else {
                    None
                };
            if let Some((deadline, deadline_at)) = next_deadline(
                &formation.snapshot.state,
                formation.log.since(&formation.snapshot),
                first_report_at,
                &self.lifecycle,
            ) && deadline_at <= now
            {
                if self
                    .expire(id, &formation.snapshot.state, deadline, deadline_at, now)
                    .await?
                {
                    let log = self.repo.load_events(id).await?;
                    let snapshot = log.snapshot();
                    formation = UnfinishedFormation { log, snapshot };
                    continue;
                }
                return Ok(());
            }

            // **通知の入口はここ 1 つ。**サイトはイベントを積むだけで Discord を呼ばない。
            // 何を投稿すべきかは毎回 DB の状態から決まるので、プロセスが落ちても
            // 次のスキャンで続きから進む（00 §7 Phase 5 の申し送り: 復帰の入口を 2 つ作らない）。
            return match &formation.snapshot.state {
                FormationState::Matched => self.start_formation_locked(id).await,
                FormationState::AwaitingHost { pending: None, .. } => {
                    self.resume_vc_assignment(id, &formation.log).await
                }
                FormationState::AwaitingCode { .. } => self.ensure_host_accepted_notice(id).await,
                FormationState::InProgress { .. } => self.ensure_code_revealed(id).await,
                FormationState::Disputed => self.ensure_dispute_notification(id).await,
                _ => Ok(()),
            };
        }
    }

    /// `vc_assigned` を記録済みの formation で、割り当て表と最初の打診を完了する。
    async fn resume_vc_assignment(
        &self,
        id: FormationId,
        log: &FormationLog,
    ) -> Result<(), AdapterError> {
        let snapshot = log.snapshot();
        if !matches!(
            snapshot.state,
            FormationState::AwaitingHost { pending: None, .. }
        ) {
            return Ok(());
        }
        let record = DiscordRecord::from_log(log);
        if record.assignment_message_ids.len() != Team::ALL.len()
            && !self.publish_assignment(id, log, &record).await?
        {
            return Ok(());
        }
        self.nominate_next(id).await
    }

    /// 期限切れをイベントとして記録する。報告期限は同じ再開ループから評価器へ渡す。
    async fn expire(
        &self,
        id: FormationId,
        state: &FormationState,
        deadline: Deadline,
        deadline_at: crate::lifecycle::Timestamp,
        now: crate::lifecycle::Timestamp,
    ) -> Result<bool, AdapterError> {
        let (kind, subject, reason) = match (deadline, state) {
            (
                Deadline::HostAccept,
                FormationState::AwaitingHost {
                    pending: Some(host),
                    ..
                },
            ) => (
                FormationEventKind::HostTimeout,
                Some(*host),
                "host_no_response",
            ),
            (Deadline::CodePost, FormationState::AwaitingCode { host, .. }) => (
                FormationEventKind::HostTimeout,
                Some(*host),
                "room_code_not_posted",
            ),
            (Deadline::NoReports, FormationState::InProgress { .. }) => {
                let reason = "no_reports";
                let event = NewEvent::new(
                    FormationEventKind::Voided,
                    None,
                    now,
                    format!("timeout:{reason}:{}", deadline_at.timestamp_micros()),
                )
                .with_payload(payload::timeout(reason, deadline_at));
                let Some(outcome) = self.repo.append_if_no_reports(id, &event).await? else {
                    debug!(formation = %id, "no-report timeout lost a race to a report or state transition");
                    return Ok(false);
                };
                log_anomalies(id, &outcome.anomalies);
                info!(formation = %id, %reason, %deadline_at, "formation timeout advanced");
                return Ok(true);
            }
            (Deadline::ReportWindow, FormationState::InProgress { .. }) => {
                let evaluation = self
                    .repo
                    .evaluate_reports(
                        id,
                        true,
                        now,
                        format!("report_evaluation:{}", deadline_at.timestamp_micros()),
                    )
                    .await?;
                if evaluation.event.is_some() {
                    info!(formation = %id, decision = ?evaluation.decision, %deadline_at, "report window evaluated");
                }
                if evaluation.decision == ReportDecision::Disputed {
                    self.ensure_dispute_notification(id).await?;
                }
                return Ok(evaluation.event.is_some());
            }
            _ => return Ok(false),
        };
        let event = NewEvent::new(
            kind,
            subject,
            now,
            format!("timeout:{reason}:{}", deadline_at.timestamp_micros()),
        )
        .with_payload(payload::timeout(reason, deadline_at));
        let recorded = self.record(id, &event).await?;
        if recorded {
            info!(formation = %id, %reason, %deadline_at, "formation timeout advanced");
        }
        Ok(recorded)
    }

    /// VC 割当を記録済みで、割り当て表の投稿だけが未完了の formation を再開する。
    async fn publish_assignment(
        &self,
        id: FormationId,
        log: &FormationLog,
        record: &DiscordRecord,
    ) -> Result<bool, AdapterError> {
        let vc = record
            .vc
            .as_ref()
            .ok_or_else(|| AdapterError::Inconsistent(id, "no voice channels recorded".into()))?;
        let roster = self.repo.load_roster(id).await?;
        if roster.len() != 10 {
            return Err(AdapterError::Inconsistent(
                id,
                format!("roster has {} players", roster.len()),
            ));
        }
        let heading = render::heading(vc, log.created_at, id);
        let match_url = self.match_url(id);
        let mut ids = HashMap::new();
        let mut failures = Vec::new();
        // **チームごとに別の本文。**そのVCには、そのチームの構成しか載せない
        for team in Team::ALL {
            let message = render::assignment(
                &heading,
                &roster,
                vc,
                team,
                &self.config.room_map,
                &self.config.room_checklist,
            );
            match self
                .post_or_edit(
                    vc.channel(team),
                    record.assignment_message_ids.get(&team).copied(),
                    &format!("assignment:{team}"),
                    &message,
                )
                .await
            {
                Ok(message_id) => {
                    ids.insert(team, message_id);
                }
                Err(failure) => failures.push(failure),
            }
        }
        let assignment_failed = !failures.is_empty();
        // 招集もチームごとに分ける。1 通に両チームを入れると相手の顔ぶれと行き先が
        // 同じ場所に並ぶので、割り当てを VC ごとに分けた意味が薄れる
        let summon = if assignment_failed {
            None
        } else if let Some(existing) = record.summon_notification {
            Some(existing)
        } else {
            let mut first = None;
            for team in Team::ALL {
                let message = render::summon(&roster, vc, team, &match_url);
                match self
                    .retry(&format!("send:summon:{team}"), || {
                        self.gateway
                            .send_message(self.config.summon_channel_id, &message)
                    })
                    .await
                {
                    Ok(message_id) => {
                        info!(
                            formation = %id,
                            channel = %self.config.summon_channel_id,
                            %message_id,
                            %team,
                            "match summon posted"
                        );
                        first.get_or_insert((self.config.summon_channel_id, message_id));
                    }
                    Err(failure) => {
                        error!(formation = %id, %team, error = %failure.error, "match summon failed; formation continues");
                        failures.push(failure);
                    }
                }
            }
            first
        };
        let event = NewEvent::new(
            FormationEventKind::VcAssigned,
            None,
            Utc::now(),
            format!(
                "vc_assigned_messages:{}:{}",
                vc.label,
                Utc::now().timestamp_micros()
            ),
        )
        .with_payload(payload::vc_assigned(vc, &ids, summon, &failures));
        if !self.record(id, &event).await? {
            return Ok(false);
        }
        if !assignment_failed {
            return Ok(true);
        }
        error!(formation = %id, count = failures.len(), "assignment table could not be posted; abandoning");
        let updated = self.repo.load_events(id).await?;
        let updated_record = DiscordRecord::from_log(&updated);
        self.abandon_in_vc(
            id,
            vc,
            &updated_record,
            AbandonReason::DiscordCallFailed,
            Vec::new(),
        )
        .await?;
        Ok(false)
    }

    /// 候補未指名の `awaiting_host` で、次の候補を打診する（または候補が尽きて解散する）。
    ///
    /// 打診は候補の側の VC チャットにだけ出す。それ以外の状態では何もしない。
    pub async fn nominate_next(&self, id: FormationId) -> Result<(), AdapterError> {
        let log = self.repo.load_events(id).await?;
        let snapshot = log.snapshot();
        log_anomalies(id, &snapshot.anomalies);
        let Some(action) = next_host_action(&snapshot.state, &self.lifecycle) else {
            debug!(formation = %id, state = %snapshot.state, "nothing to nominate");
            return Ok(());
        };
        let record = DiscordRecord::from_log(&log);
        let vc = record
            .vc
            .clone()
            .ok_or_else(|| AdapterError::Inconsistent(id, "no voice channels recorded".into()))?;
        let heading = render::heading(&vc, log.created_at, id);

        let candidate_index = match action {
            HostAction::Abandon => {
                return self
                    .abandon_in_vc(
                        id,
                        &vc,
                        &record,
                        AbandonReason::HostCandidatesExhausted,
                        Vec::new(),
                    )
                    .await;
            }
            HostAction::Assign { candidate_index } => candidate_index,
        };
        let roster = self.repo.load_roster(id).await?;
        let candidates = self.candidates(id, &roster).await?;
        let Some(candidate) = candidates.get(candidate_index).copied() else {
            // 候補リストが上限より短い。HostAction の規約どおり解散と同じ扱い
            return self
                .abandon_in_vc(
                    id,
                    &vc,
                    &record,
                    AbandonReason::HostCandidatesExhausted,
                    Vec::new(),
                )
                .await;
        };
        let entry = roster
            .iter()
            .find(|entry| entry.player_id == candidate)
            .ok_or_else(|| {
                AdapterError::Inconsistent(
                    id,
                    format!("candidate {candidate} is not in the roster"),
                )
            })?;
        let user = render::user_of(entry).ok_or_else(|| {
            AdapterError::Inconsistent(id, format!("candidate {candidate} has no discord user"))
        })?;
        let channel = vc.channel(entry.team);

        let match_url = self.match_url(id);
        let message = render::host_prompt(
            &heading,
            user,
            candidate_index + 1,
            candidates.len(),
            &match_url,
        );
        let mut failures = Vec::new();
        // 同じ側の VC に打診メッセージがあれば編集する。反対側にあれば「移りました」に書き換える
        let existing = match record.host_prompt {
            Some((prompt_channel, prompt_message)) if prompt_channel == channel => {
                Some(prompt_message)
            }
            Some((prompt_channel, prompt_message)) => {
                let moved = render::host_prompt_moved(&heading, entry.team);
                if let Err(failure) = self
                    .retry("edit:host_prompt_moved", || {
                        self.gateway
                            .edit_message(prompt_channel, prompt_message, &moved)
                    })
                    .await
                {
                    failures.push(failure);
                }
                None
            }
            None => None,
        };
        let prompt_message_id = match self
            .post_or_edit(channel, existing, "host_prompt", &message)
            .await
        {
            Ok(message_id) => message_id,
            Err(failure) => {
                error!(formation = %id, error = %failure.error, "host prompt could not be posted; abandoning");
                failures.push(failure);
                return self
                    .abandon_in_vc(id, &vc, &record, AbandonReason::DiscordCallFailed, failures)
                    .await;
            }
        };
        // **招集チャンネルにもホストを知らせる。**相手チームの人にも伝える（誰が部屋を
        // 建てたか分からないと、入れなかったときに聞く相手が分からない）。候補が次に
        // 移ったときも同じ経路で投稿するので、交代したことが公開の場に残る
        let announcement =
            render::host_announcement(id, user, candidate_index + 1, candidates.len(), &match_url);
        if let Err(failure) = self
            .retry("send:host_announcement", || {
                self.gateway
                    .send_message(self.config.summon_channel_id, &announcement)
            })
            .await
        {
            // 打診自体は VC に出ている。招集の通知が落ちても試合は続ける
            error!(formation = %id, error = %failure.error, "host announcement failed; formation continues");
            failures.push(failure);
        }
        let event = NewEvent::new(
            FormationEventKind::HostAssigned,
            Some(candidate),
            Utc::now(),
            format!("host_assigned:{prompt_message_id}:{candidate_index}"),
        )
        .with_payload(payload::host_assigned(
            channel,
            prompt_message_id,
            entry.discord_user_id,
            &failures,
        ));
        let outcome = self.repo.append_event(id, &event).await?;
        log_anomalies(id, &outcome.anomalies);
        info!(formation = %id, %candidate, index = candidate_index, %channel, "host candidate nominated");
        Ok(())
    }

    /// 過去に投稿したボタンへの応答。**状態は一切変えない。**
    ///
    /// 新しいボタンはもう作らない（操作はサイトへ移した）。ただしチャット履歴に残った
    /// 古いメッセージは押せてしまうので、黙って無視せず、本人にだけ行き先を返す。
    /// ここでイベントを積んだり報告を受けたりはしない — Discord に操作を残さない。
    pub async fn handle_interaction(
        &self,
        incoming: IncomingInteraction,
    ) -> Result<(), AdapterError> {
        if let Some(guild) = incoming.guild
            && guild != self.config.guild_id
        {
            debug!(%guild, "ignoring interaction from another guild");
            return Ok(());
        }
        debug!(
            custom_id = incoming.custom_id(),
            "interaction received after the move to the site; answering with the link only"
        );
        self.ephemeral(&incoming, render::moved_to_the_site(&self.site_base_url))
            .await;
        Ok(())
    }

    /// ホストが承諾した後の状態表示を VC に出す（サイトからの `host_accepted` を受けて）。
    ///
    /// 二重投稿を避けるため、通知済みかどうかはログから導出する。既に通知済みなら何もしない。
    /// 通知したことは `host_accepted` の再追記で残す（同じホストの再送は状態を変えないので、
    /// 状態機械に新しい遷移を足さずに済む）。
    async fn ensure_host_accepted_notice(&self, id: FormationId) -> Result<(), AdapterError> {
        let log = self.repo.load_events(id).await?;
        let snapshot = log.snapshot();
        let FormationState::AwaitingCode { host, .. } = snapshot.state else {
            return Ok(());
        };
        let record = DiscordRecord::from_log(&log);
        if record.host_accept_notified {
            return Ok(());
        }
        let (Some(vc), Some((channel, prompt))) = (record.vc.clone(), record.host_prompt) else {
            return Ok(());
        };
        let roster = self.repo.load_roster(id).await?;
        let Some(user) = roster
            .iter()
            .find(|entry| entry.player_id == host)
            .and_then(render::user_of)
        else {
            return Ok(());
        };
        let heading = render::heading(&vc, log.created_at, id);
        let message = render::host_accepted(&heading, user, &self.match_url(id));
        let mut failures = Vec::new();
        if let Err(failure) = self
            .retry("edit:host_accepted", || {
                self.gateway.edit_message(channel, prompt, &message)
            })
            .await
        {
            failures.push(failure);
        }
        let discord_user_id = roster
            .iter()
            .find(|entry| entry.player_id == host)
            .map(|entry| entry.discord_user_id)
            .unwrap_or_default();
        let event = NewEvent::new(
            FormationEventKind::HostAccepted,
            Some(host),
            Utc::now(),
            format!("host_accepted_notified:{id}"),
        )
        .with_payload(payload::host_accept_notified(discord_user_id, &failures));
        if self.record(id, &event).await? {
            info!(formation = %id, %host, "host acceptance announced in the voice chat");
        }
        Ok(())
    }

    /// ルーム ID を両 VC に開示し、報告の催促を出す（サイトからの `code_posted` を受けて）。
    ///
    /// 開示済みかどうかは「最後に開示したコード」をログから導出して判定する。**打ち直しは
    /// コードが変わるので必ず再投稿（編集）される**。同じコードで何度呼ばれても 1 回で済む。
    async fn ensure_code_revealed(&self, id: FormationId) -> Result<(), AdapterError> {
        let log = self.repo.load_events(id).await?;
        let snapshot = log.snapshot();
        let FormationState::InProgress { host, room_code } = snapshot.state.clone() else {
            return Ok(());
        };
        let record = DiscordRecord::from_log(&log);
        if record.revealed_code.as_deref() == Some(room_code.as_str()) {
            return Ok(());
        }
        let Some(vc) = record.vc.clone() else {
            return Ok(());
        };
        let roster = self.repo.load_roster(id).await?;
        let Some(host_entry) = roster.iter().find(|entry| entry.player_id == host) else {
            return Ok(());
        };
        let Some(host_user) = render::user_of(host_entry) else {
            return Ok(());
        };
        let heading = render::heading(&vc, log.created_at, id);
        let match_url = self.match_url(id);
        let report_prompt = render::report_prompt(&heading, &match_url);

        let mut code_message_ids = HashMap::new();
        let mut report_message_ids = HashMap::new();
        let mut failures = Vec::new();
        for team in Team::ALL {
            // 開示もチーム側の文面にする（集合先はそのチームの VC だけを書く）
            let reveal = render::code_reveal(&heading, host_user, &room_code, &vc, team);
            match self
                .post_or_edit(
                    vc.channel(team),
                    record.code_message_ids.get(&team).copied(),
                    &format!("code_reveal:{team}"),
                    &reveal,
                )
                .await
            {
                Ok(message_id) => {
                    code_message_ids.insert(team, message_id);
                }
                Err(failure) => failures.push(failure),
            }
            match self
                .post_or_edit(
                    vc.channel(team),
                    record.report_message_ids.get(&team).copied(),
                    &format!("report_prompt:{team}"),
                    &report_prompt,
                )
                .await
            {
                Ok(message_id) => {
                    report_message_ids.insert(team, message_id);
                }
                Err(failure) => failures.push(failure),
            }
        }
        if !failures.is_empty() {
            // 片側でも見えなければ試合にならない
            error!(formation = %id, count = failures.len(), "room code could not be revealed; abandoning");
            return self
                .abandon_in_vc(id, &vc, &record, AbandonReason::DiscordCallFailed, failures)
                .await;
        }
        // 開示したコードと、投稿したメッセージ ID を同じ種別の追記で残す。
        // 同じホスト・同じコードの再追記なので状態は変わらない（状態機械はそのまま）
        let event = NewEvent::new(
            FormationEventKind::CodePosted,
            Some(host),
            Utc::now(),
            format!("code_revealed:{id}:{}", short_key(&room_code)),
        )
        .with_payload(payload::code_posted(
            &room_code,
            &code_message_ids,
            &report_message_ids,
            host_entry.discord_user_id,
            &failures,
        ));
        if self.record(id, &event).await? {
            info!(formation = %id, %host, "room code revealed in both voice chats");
        }
        Ok(())
    }

    /// `disputed` の通知を再開可能にする。通知済み ID がログにあれば Discord を検索しない。
    async fn ensure_dispute_notification(
        &self,
        formation: FormationId,
    ) -> Result<(), AdapterError> {
        let log = self.repo.load_events(formation).await?;
        let record = DiscordRecord::from_log(&log);
        if record.dispute_notification.is_some() {
            return Ok(());
        }
        let roster = self.repo.load_roster(formation).await?;
        let evidence = self.repo.report_evidence(formation).await?;
        let message =
            render::dispute_notification(formation, &roster, &evidence.latest_for_confirmation);
        let channel = self.config.dispute_channel_id;
        let mut failures = Vec::new();
        let sent = match self
            .retry("send:dispute_notification", || {
                self.gateway.send_message(channel, &message)
            })
            .await
        {
            Ok(message_id) => Some(message_id),
            Err(failure) => {
                failures.push(failure);
                None
            }
        };
        let key = sent.map_or_else(
            || {
                format!(
                    "dispute_notification_failure:{}",
                    Utc::now().timestamp_micros()
                )
            },
            |message_id| format!("dispute_notification:{message_id}"),
        );
        let event = NewEvent::new(FormationEventKind::Disputed, None, Utc::now(), key)
            .with_payload(payload::dispute_notification(channel, sent, &failures));
        self.repo.append_event(formation, &event).await?;
        if let Some(message_id) = sent {
            info!(formation = %formation, %channel, %message_id, "dispute notification posted");
        } else {
            error!(formation = %formation, %channel, "dispute notification failed; will retry from the log");
        }
        Ok(())
    }

    /// VC の無い段階での解散（VC 無し）。
    async fn abandon(
        &self,
        id: FormationId,
        reason: AbandonReason,
        failures: &[DiscordFailure],
    ) -> Result<(), AdapterError> {
        let event = NewEvent::new(
            FormationEventKind::Abandoned,
            None,
            Utc::now(),
            format!("abandoned:{reason}"),
        )
        .with_payload(payload::abandoned(reason, failures));
        let outcome = self.repo.append_event(id, &event).await?;
        log_anomalies(id, &outcome.anomalies);
        info!(formation = %id, %reason, "abandoned");
        Ok(())
    }

    /// VC 割当後の解散。通知（打診メッセージがあればそれを編集、無い側には投稿）→ `abandoned` の順。
    /// 通知の失敗も payload に残す。
    async fn abandon_in_vc(
        &self,
        id: FormationId,
        vc: &VcPair,
        record: &DiscordRecord,
        reason: AbandonReason,
        mut failures: Vec<DiscordFailure>,
    ) -> Result<(), AdapterError> {
        let log = self.repo.load_events(id).await?;
        let heading = render::heading(vc, log.created_at, id);
        let notice = render::abandoned_notice(&heading, reason);
        for team in Team::ALL {
            let channel = vc.channel(team);
            let existing = match record.host_prompt {
                Some((prompt_channel, prompt_message)) if prompt_channel == channel => {
                    Some(prompt_message)
                }
                _ => None,
            };
            if let Err(failure) = self
                .post_or_edit(
                    channel,
                    existing,
                    &format!("abandoned_notice:{team}"),
                    &notice,
                )
                .await
            {
                failures.push(failure);
            }
        }
        self.abandon(id, reason, &failures).await?;
        Ok(())
    }

    /// メッセージがあれば編集、無ければ投稿。どちらもリトライ付き。
    async fn post_or_edit(
        &self,
        channel: ChannelId,
        existing: Option<MessageId>,
        what: &str,
        message: &OutgoingMessage,
    ) -> Result<MessageId, DiscordFailure> {
        match existing {
            Some(message_id) => {
                self.retry(&format!("edit:{what}"), || {
                    self.gateway.edit_message(channel, message_id, message)
                })
                .await?;
                Ok(message_id)
            }
            None => {
                self.retry(&format!("send:{what}"), || {
                    self.gateway.send_message(channel, message)
                })
                .await
            }
        }
    }
    /// 追記する。事前検証を通っているので、拒否されるのは競合だけ（エラーとして記録し `false`）。
    async fn record(&self, formation: FormationId, event: &NewEvent) -> Result<bool, AdapterError> {
        match self.repo.append_event(formation, event).await {
            Ok(outcome) => {
                log_anomalies(formation, &outcome.anomalies);
                if outcome.already_recorded {
                    debug!(formation = %formation, key = %event.idempotency_key, "already recorded");
                }
                Ok(true)
            }
            Err(RepositoryError::Rejected(error)) => {
                error!(formation = %formation, %error, "event rejected after the side effect (race)");
                Ok(false)
            }
            Err(RepositoryError::IdempotencyKeyReused { .. }) => {
                warn!(formation = %formation, key = %event.idempotency_key, "idempotency key reused");
                Ok(false)
            }
            Err(error) => Err(error.into()),
        }
    }

    /// 本人にだけ見える通知。記録するイベントが無いので、失敗はログに出すだけ。
    async fn ephemeral(&self, incoming: &IncomingInteraction, content: String) {
        if let Err(error) = self
            .gateway
            .respond(
                &incoming.reference(),
                InteractionResponse::Ephemeral(content),
            )
            .await
        {
            warn!(interaction = %incoming.id, %error, "ephemeral response failed");
        }
    }

    /// `HostSelector` に渡す形を作って候補を得る。
    async fn candidates(
        &self,
        id: FormationId,
        roster: &[RosterEntry],
    ) -> Result<Vec<PlayerId>, AdapterError> {
        let ids: Vec<PlayerId> = roster.iter().map(|entry| entry.player_id).collect();
        let last_hosted = self.repo.last_hosted_at(&ids).await?;
        let formation = Formation {
            id,
            players: roster
                .iter()
                .map(|entry| FormationPlayer {
                    player_id: entry.player_id,
                    mu_at_match: entry.mu_at_match,
                    last_hosted_at: last_hosted.get(&entry.player_id).copied(),
                })
                .collect(),
        };
        Ok(self.selector.candidates(&formation))
    }

    /// Discord API 呼び出しを最大 N 回試す。間隔は試行回数に比例して延びる。
    /// 使い切ったら最後のエラーを [`DiscordFailure`] にして返す（payload に入れる形）。
    async fn retry<T, F, Fut>(&self, what: &str, mut operation: F) -> Result<T, DiscordFailure>
    where
        F: FnMut() -> Fut,
        Fut: Future<Output = Result<T, GatewayError>>,
    {
        let attempts = self.config.retries.max(1);
        let mut last_error = None;
        for attempt in 1..=attempts {
            match operation().await {
                Ok(value) => return Ok(value),
                Err(error) => {
                    warn!(what, attempt, attempts, %error, "discord call failed");
                    last_error = Some(error);
                    if attempt < attempts {
                        tokio::time::sleep(self.config.retry_backoff * attempt).await;
                    }
                }
            }
        }
        Err(DiscordFailure {
            operation: what.to_owned(),
            error: last_error.map_or_else(|| "no attempts".into(), |error| error.0),
            attempts,
        })
    }
}

fn log_anomalies(id: FormationId, anomalies: &[Anomaly]) {
    for anomaly in anomalies {
        warn!(formation = %id, %anomaly, "event log anomaly");
    }
}

/// ルーム ID を冪等キーに載せるための短い印。**同じコードなら同じキー**になり、
/// 打ち直しでは必ず変わる（00 §5: 内容の違う打ち直しを黙って捨てない）。
fn short_key(value: &str) -> String {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in value.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x1000_0000_01b3);
    }
    format!("{hash:016x}")
}
