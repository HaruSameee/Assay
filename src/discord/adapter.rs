//! Discord アダプタ本体。イベント駆動の部分だけ（時間駆動は Phase 5）。
//!
//! 流れ:
//! 1. `matched` → VC の組を LRU で確保 → 割り当て表を red / blue 両方の VC チャットに投稿 →
//!    `vc_assigned`（payload に VC のラベルと両チャンネル ID、割り当て表のメッセージ ID）
//! 2. `HostSelector` の候補を、候補の側の VC チャットにだけ打診 → `host_assigned`
//!    （payload に打診メッセージのチャンネル ID と ID）
//! 3. 「部屋を作る」→ モーダル → `host_accepted` → モーダル送信 → 両 VC に開示 → `code_posted`
//!    （payload にルーム ID・開示メッセージ ID）
//! 4. 「今回はパスする」→ `host_declined` → 次の候補（同じ側なら打診メッセージを編集、反対側なら
//!    古い打診のボタンを外して反対側に投稿）。尽きたら `abandoned`
//!
//! 失敗経路（すべて `abandoned` で解散し、理由と詳細を payload に残す。10 人を黙って拘束しない）:
//! - 空いている VC が無い → `vc_pool_exhausted`
//! - 割り当て表・打診・開示の投稿や編集が N 回失敗 → `discord_call_failed`（payload に操作名）
//!
//! 原則:
//! - 副作用の記録が正。Discord に投稿したら、その ID を必ず `formation_events.payload` に入れる。
//!   失敗も同じ payload に残す（stdout のログは記録ではない）。再起動後は [`DiscordRecord`] を
//!   ログから導出して続きを操作する。Discord 側を検索し直さない。
//! - VC チャットのメッセージは消さない（VC は使い回すが、過去のルーム ID は試合終了で無効）。
//!   すべての投稿に見出し（VC ラベル・時刻・試合 ID）を付ける。
//! - 順序は「副作用 → 記録」。インタラクションでは、状態機械で事前に検証してから応答し、
//!   応答の成否を含めて追記する。正式な検証は [`FormationRepository::append_event`]
//!   （ロック → 検証 → INSERT）が行う。
//! - インタラクション由来のイベントの冪等キーは、そのインタラクション ID。
//! - 古いボタンは Discord 側で無効化しない。状態機械の検証で弾いて、押した本人にだけ知らせる。
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
    AbandonReason, Anomaly, EventPayload, Formation, FormationEvent, FormationEventKind,
    FormationId, FormationPlayer, FormationState, HostAction, HostSelector, LifecycleConfig,
    PlayerId, apply, next_host_action,
};
use crate::persistence::{
    FormationLog, FormationRepository, NewEvent, RepositoryError, RosterEntry, Team,
};

use super::config::DiscordConfig;
use super::custom_id::{Action, CustomId, ROOM_CODE_INPUT_ID};
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
    ) -> Self {
        Self {
            repo,
            gateway,
            config,
            lifecycle,
            selector,
            vc_pool: VcPool::new(vc_pool),
        }
    }

    /// 永続化層。
    pub fn repository(&self) -> &FormationRepository {
        &self.repo
    }

    /// VC プール。
    pub fn vc_pool(&self) -> &VcPool {
        &self.vc_pool
    }

    /// 起動時: 未終了の formation のうち、イベント駆動で続きができるものを進める。
    ///
    /// - `matched`（VC 未割当）→ VC の確保から
    /// - `awaiting_host` で候補未指名（辞退の直後に落ちた）→ 次の候補を打診
    ///
    /// 期限切れの判定や、応答待ちのままの候補への催促はしない（時間駆動は Phase 5）。
    /// Phase 5 はこの関数を拡張する形で実装する。復帰の入口を 2 つ作らない。
    pub async fn resume_unfinished(&self) -> Result<(), AdapterError> {
        let unfinished = self.repo.list_unfinished().await?;
        info!(count = unfinished.len(), "resuming unfinished formations");
        for formation in unfinished {
            let id = formation.log.formation_id;
            log_anomalies(id, &formation.snapshot.anomalies);
            let result = match &formation.snapshot.state {
                FormationState::Matched => self.start_formation(id).await,
                FormationState::AwaitingHost { pending: None, .. } => self.nominate_next(id).await,
                _ => Ok(()),
            };
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
        let log = self.repo.load_events(id).await?;
        let snapshot = log.snapshot();
        log_anomalies(id, &snapshot.anomalies);
        if snapshot.state != FormationState::Matched {
            debug!(formation = %id, state = %snapshot.state, "already started");
            return Ok(());
        }
        let roster = self.repo.load_roster(id).await?;
        if roster.len() != 10 {
            return Err(AdapterError::Inconsistent(
                id,
                format!("roster has {} players", roster.len()),
            ));
        }

        // VC の確保。占有状態と最終使用時刻は DB（vc_assigned の payload）から導出する
        let unfinished = self.repo.list_unfinished().await?;
        let occupied = VcPool::occupied_labels(&unfinished);
        let last_used = self.repo.vc_last_used().await?;
        let Some(reservation) = self.vc_pool.reserve(&occupied, &last_used) else {
            warn!(formation = %id, "no free voice channel pair; abandoning");
            self.abandon(id, AbandonReason::VcPoolExhausted, &[])
                .await?;
            return Ok(());
        };
        let vc = reservation.pair().clone();
        info!(formation = %id, vc = %vc.label, red = %vc.red, blue = %vc.blue, "voice channels assigned");

        // 割り当て表を両方の VC チャットに投稿する（副作用 → 記録）
        let heading = render::heading(&vc, log.created_at, id);
        let assignment = render::assignment(
            &heading,
            &roster,
            &vc,
            &self.config.room_map,
            &self.config.room_checklist,
        );
        let (assignment_ids, mut failures) =
            self.post_to_both(&vc, "assignment", &assignment).await;
        let event = NewEvent::new(
            FormationEventKind::VcAssigned,
            None,
            Utc::now(),
            format!("vc_assigned:{}", vc.label),
        )
        .with_payload(payload::vc_assigned(&vc, &assignment_ids, &failures));
        let outcome = self.repo.append_event(id, &event).await?;
        log_anomalies(id, &outcome.anomalies);
        reservation.commit();

        if !failures.is_empty() {
            error!(formation = %id, count = failures.len(), "assignment table could not be posted; abandoning");
            failures.clear();
            let record = DiscordRecord::from_log(&self.repo.load_events(id).await?);
            self.abandon_in_vc(id, &vc, &record, AbandonReason::DiscordCallFailed, failures)
                .await?;
            return Ok(());
        }
        self.nominate_next(id).await
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

        let message = render::host_prompt(
            &heading,
            user,
            id,
            candidate,
            candidate_index + 1,
            candidates.len(),
        );
        let mut failures = Vec::new();
        // 同じ側の VC に打診メッセージがあれば編集する。反対側にあればボタンを外して、こちらに投稿する
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

    /// ゲートウェイから来たインタラクションを処理する。
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
        let Some(custom_id) = CustomId::parse(incoming.custom_id()) else {
            debug!(
                custom_id = incoming.custom_id(),
                "ignoring foreign custom id"
            );
            return Ok(());
        };
        match (custom_id.action, &incoming.kind) {
            (Action::HostAccept, IncomingKind::Button { .. }) => {
                self.on_host_accept(&incoming, custom_id).await
            }
            (Action::HostDecline, IncomingKind::Button { .. }) => {
                self.on_host_decline(&incoming, custom_id).await
            }
            (Action::CodeOpen, IncomingKind::Button { .. }) => {
                self.on_code_open(&incoming, custom_id).await
            }
            (Action::CodeSubmit, IncomingKind::ModalSubmit { inputs, .. }) => {
                self.on_code_submit(&incoming, custom_id, inputs.get(ROOM_CODE_INPUT_ID))
                    .await
            }
            (action, kind) => {
                debug!(?action, ?kind, "interaction kind does not match the action");
                Ok(())
            }
        }
    }

    /// 「部屋を作る」: 検証 → モーダルを開く → 状態表示を「作成中」に → `host_accepted` を追記。
    async fn on_host_accept(
        &self,
        incoming: &IncomingInteraction,
        custom_id: CustomId,
    ) -> Result<(), AdapterError> {
        let CustomId {
            formation, player, ..
        } = custom_id;
        let Some((entry, _)) = self.presser_is(incoming, formation, player).await? else {
            return Ok(());
        };
        let Some(log) = self
            .prevalidate(
                incoming,
                formation,
                FormationEventKind::HostAccepted,
                player,
                None,
            )
            .await?
        else {
            return Ok(());
        };

        let mut failures = Vec::new();
        self.respond(
            incoming,
            "respond:modal",
            InteractionResponse::Modal(render::code_modal(formation, player, None)),
            &mut failures,
        )
        .await;
        // 状態表示を「作成中」に。同じ VC の全員に承諾が分かる
        let record = DiscordRecord::from_log(&log);
        if let (Some(vc), Some((channel, prompt))) = (&record.vc, record.host_prompt) {
            let heading = render::heading(vc, log.created_at, formation);
            let message = render::host_accepted(&heading, incoming.user, formation, player);
            if let Err(failure) = self
                .retry("edit:host_prompt", || {
                    self.gateway.edit_message(channel, prompt, &message)
                })
                .await
            {
                failures.push(failure);
            }
        }

        let event = NewEvent::new(
            FormationEventKind::HostAccepted,
            Some(player),
            Utc::now(),
            incoming.id.to_string(),
        )
        .with_payload(payload::host_response(entry.discord_user_id, &failures));
        if self.record(formation, &event).await? {
            info!(formation = %formation, %player, "host accepted");
        }
        Ok(())
    }

    /// 「今回はパスする」: 検証 → 受領 → `host_declined` を追記 → 次の候補を打診。
    async fn on_host_decline(
        &self,
        incoming: &IncomingInteraction,
        custom_id: CustomId,
    ) -> Result<(), AdapterError> {
        let CustomId {
            formation, player, ..
        } = custom_id;
        let Some((entry, _)) = self.presser_is(incoming, formation, player).await? else {
            return Ok(());
        };
        if self
            .prevalidate(
                incoming,
                formation,
                FormationEventKind::HostDeclined,
                player,
                None,
            )
            .await?
            .is_none()
        {
            return Ok(());
        }

        // 打診メッセージは nominate_next が編集する（新しいメッセージは積まない）
        let mut failures = Vec::new();
        self.respond(
            incoming,
            "respond:acknowledge",
            InteractionResponse::Acknowledge,
            &mut failures,
        )
        .await;
        let event = NewEvent::new(
            FormationEventKind::HostDeclined,
            Some(player),
            Utc::now(),
            incoming.id.to_string(),
        )
        .with_payload(payload::host_response(entry.discord_user_id, &failures));
        if !self.record(formation, &event).await? {
            return Ok(());
        }
        info!(formation = %formation, %player, "host declined");
        self.nominate_next(formation).await
    }

    /// 「ルーム ID を入力 / 修正」: ホスト本人ならモーダルを開く。イベントは積まない。
    async fn on_code_open(
        &self,
        incoming: &IncomingInteraction,
        custom_id: CustomId,
    ) -> Result<(), AdapterError> {
        let CustomId {
            formation, player, ..
        } = custom_id;
        let Some(_) = self.presser_is(incoming, formation, player).await? else {
            return Ok(());
        };
        let log = self.repo.load_events(formation).await?;
        let snapshot = log.snapshot();
        let (host, current) = match &snapshot.state {
            FormationState::AwaitingCode { host, .. } => (*host, None),
            FormationState::InProgress { host, room_code } => (*host, Some(room_code.as_str())),
            _ => {
                self.ephemeral(incoming, render::stale()).await;
                return Ok(());
            }
        };
        if host != player {
            self.ephemeral(incoming, render::not_the_host()).await;
            return Ok(());
        }
        // 記録するイベントが無いので、この応答の失敗は payload に残らない（本人が押し直せば済む）
        let mut failures = Vec::new();
        self.respond(
            incoming,
            "respond:modal",
            InteractionResponse::Modal(render::code_modal(formation, player, current)),
            &mut failures,
        )
        .await;
        Ok(())
    }

    /// モーダル送信: 検証 → 両 VC に開示（投稿または編集）→ 状態表示を更新 → `code_posted` を追記。
    async fn on_code_submit(
        &self,
        incoming: &IncomingInteraction,
        custom_id: CustomId,
        input: Option<&String>,
    ) -> Result<(), AdapterError> {
        let CustomId {
            formation, player, ..
        } = custom_id;
        let Some((entry, roster)) = self.presser_is(incoming, formation, player).await? else {
            return Ok(());
        };
        let room_code = input.map(|value| value.trim()).unwrap_or_default();
        if room_code.is_empty() {
            self.ephemeral(incoming, render::code_invalid()).await;
            return Ok(());
        }
        let Some(log) = self
            .prevalidate(
                incoming,
                formation,
                FormationEventKind::CodePosted,
                player,
                Some(room_code),
            )
            .await?
        else {
            return Ok(());
        };
        let record = DiscordRecord::from_log(&log);
        let vc = record.vc.clone().ok_or_else(|| {
            AdapterError::Inconsistent(formation, "no voice channels recorded".into())
        })?;
        let heading = render::heading(&vc, log.created_at, formation);

        // 副作用（両 VC に開示）→ 記録（code_posted に開示メッセージ ID）
        let others: Vec<UserId> = roster
            .iter()
            .filter(|other| other.player_id != player)
            .filter_map(render::user_of)
            .collect();
        let reveal = render::code_reveal(&heading, incoming.user, &others, room_code, &vc);
        let mut code_message_ids = HashMap::new();
        let mut failures = Vec::new();
        for team in Team::ALL {
            let what = format!("code_reveal:{team}");
            match self
                .post_or_edit(
                    vc.channel(team),
                    record.code_message_ids.get(&team).copied(),
                    &what,
                    &reveal,
                )
                .await
            {
                Ok(message_id) => {
                    code_message_ids.insert(team, message_id);
                }
                Err(failure) => failures.push(failure),
            }
        }
        if !failures.is_empty() {
            // 片側でも見えなければ試合にならない
            error!(formation = %formation, count = failures.len(), "room code could not be revealed; abandoning");
            self.ephemeral(incoming, render::reveal_failed()).await;
            return self
                .abandon_in_vc(
                    formation,
                    &vc,
                    &record,
                    AbandonReason::DiscordCallFailed,
                    failures,
                )
                .await;
        }
        self.respond(
            incoming,
            "respond:update_status",
            InteractionResponse::UpdateMessage(render::code_status(
                &heading,
                incoming.user,
                room_code,
                formation,
                player,
            )),
            &mut failures,
        )
        .await;
        let event = NewEvent::new(
            FormationEventKind::CodePosted,
            Some(player),
            Utc::now(),
            incoming.id.to_string(),
        )
        .with_payload(payload::code_posted(
            room_code,
            &code_message_ids,
            entry.discord_user_id,
            &failures,
        ));
        if self.record(formation, &event).await? {
            info!(formation = %formation, %player, "room code posted to both voice chats");
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
        self.vc_pool.release(&vc.label);
        Ok(())
    }

    /// 両方の VC チャットに投稿する。成功した側の ID と、失敗の一覧を返す。
    async fn post_to_both(
        &self,
        vc: &VcPair,
        what: &str,
        message: &OutgoingMessage,
    ) -> (HashMap<Team, MessageId>, Vec<DiscordFailure>) {
        let mut ids = HashMap::new();
        let mut failures = Vec::new();
        for team in Team::ALL {
            match self
                .retry(&format!("send:{what}:{team}"), || {
                    self.gateway.send_message(vc.channel(team), message)
                })
                .await
            {
                Ok(message_id) => {
                    ids.insert(team, message_id);
                }
                Err(failure) => failures.push(failure),
            }
        }
        (ids, failures)
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

    /// 押した人が `player` 本人か。本人なら編成の行と編成全体を返す。
    /// 違えば本人にだけ理由を返して `None`。
    async fn presser_is(
        &self,
        incoming: &IncomingInteraction,
        formation: FormationId,
        player: PlayerId,
    ) -> Result<Option<(RosterEntry, Vec<RosterEntry>)>, AdapterError> {
        let roster = match self.repo.load_roster(formation).await {
            Ok(roster) => roster,
            Err(RepositoryError::FormationNotFound(_)) => {
                self.ephemeral(incoming, render::stale()).await;
                return Ok(None);
            }
            Err(error) => return Err(error.into()),
        };
        let Some(entry) = roster
            .iter()
            .find(|entry| entry.player_id == player)
            .cloned()
        else {
            self.ephemeral(incoming, render::stale()).await;
            return Ok(None);
        };
        let addressee = render::user_of(&entry);
        if addressee != Some(incoming.user) {
            self.ephemeral(incoming, render::not_addressed_to_you(addressee))
                .await;
            return Ok(None);
        }
        Ok(Some((entry, roster)))
    }

    /// 応答する前の検証（ロック無し。正式な検証は `append_event` が同じ `apply` で行う）。
    /// 通れば現在のログを返す。弾かれたら本人にだけ知らせて `None`。
    async fn prevalidate(
        &self,
        incoming: &IncomingInteraction,
        formation: FormationId,
        kind: FormationEventKind,
        player: PlayerId,
        room_code: Option<&str>,
    ) -> Result<Option<FormationLog>, AdapterError> {
        let log = self.repo.load_events(formation).await?;
        let snapshot = log.snapshot();
        log_anomalies(formation, &snapshot.anomalies);
        let candidate = FormationEvent {
            kind,
            subject: Some(player),
            occurred_at: Utc::now(),
            idempotency_key: incoming.id.to_string(),
            payload: room_code.map_or(EventPayload::Empty, |code| {
                EventPayload::RoomCode(code.to_owned())
            }),
        };
        // 同じインタラクションの再送は、状態がもう進んでいても受け付ける（append が no-op にする）
        let already = log
            .events
            .iter()
            .any(|stored| stored.event.idempotency_key == candidate.idempotency_key);
        if !already && let Err(error) = apply(&snapshot.state, &candidate) {
            info!(formation = %formation, %error, "interaction rejected by the state machine");
            self.ephemeral(incoming, render::stale()).await;
            return Ok(None);
        }
        Ok(Some(log))
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

    /// インタラクションに応答する。失敗は `failures` に積む（呼び出し側が payload に入れる）。
    async fn respond(
        &self,
        incoming: &IncomingInteraction,
        what: &str,
        response: InteractionResponse,
        failures: &mut Vec<DiscordFailure>,
    ) {
        if let Err(error) = self.gateway.respond(&incoming.reference(), response).await {
            warn!(interaction = %incoming.id, what, %error, "interaction response failed");
            failures.push(DiscordFailure {
                operation: what.to_owned(),
                error: error.0,
                attempts: 1,
            });
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
