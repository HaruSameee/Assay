//! Discord アダプタの統合テスト。実際の PostgreSQL と、メモリ上の偽 Discord（`FakeGateway`）を使う。
//!
//! 実行方法は `tests/persistence.rs` と同じ（`DATABASE_URL` を設定して `cargo test --test discord`）。
//! リポジトリはアプリ用ロールで接続する。
//!
//! 検証すること（Phase 4 の完了条件）:
//! - `matched` から `code_posted` までが通り、VC 割当と投稿したメッセージの ID が全部 payload に入る
//! - 割り当て表とルーム ID は red / blue 両方の VC チャットに、ホスト打診はホスト側にだけ出る
//! - 再起動（別のアダプタ）でも、Discord 側を検索し直さずにログから続きを操作できる
//! - ホストが「パスする」を 3 回押すと `abandoned` になり、打診メッセージは編集で更新される
//! - モーダルを 2 回送ると 2 回目のルーム ID が両方の開示メッセージに反映される
//! - VC プールは起動時に名前解決し（欠落・重複で失敗）、割当は LRU
//! - 失敗経路はすべて `abandoned` になり、失敗の詳細が payload に入る
//! - `/internal/formations` はトークン無しでは到達できない

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use chrono::Utc;
use serenity::async_trait;
use serenity::model::id::{ChannelId, GuildId, InteractionId, MessageId, UserId};
use sqlx::PgPool;
use sqlx::postgres::PgPoolOptions;
use unite_matchsystem::discord::adapter::{Adapter, IncomingInteraction, IncomingKind};
use unite_matchsystem::discord::custom_id::{Action, CustomId, ROOM_CODE_INPUT_ID};
use unite_matchsystem::discord::gateway::{
    Gateway, GatewayError, InteractionRef, InteractionResponse, OutgoingMessage,
};
use unite_matchsystem::discord::payload::{
    ASSIGNMENT_MESSAGE_IDS_KEY, ATTEMPTS_KEY, CODE_MESSAGE_IDS_KEY, DISCORD_FAILURES_KEY,
    DiscordRecord, HOST_PROMPT_CHANNEL_ID_KEY, HOST_PROMPT_MESSAGE_ID_KEY, OPERATION_KEY,
};
use unite_matchsystem::discord::{
    DiscordConfig, ResolveError, VcPair, VcPoolSetupError, resolve_vc_pool,
};
use unite_matchsystem::lifecycle::{
    AbandonReason, DefaultHostSelector, EventPayload, FormationEventKind as K, FormationId,
    FormationState as S, LifecycleConfig, PlayerId,
};
use unite_matchsystem::persistence::{
    ABANDON_REASON_KEY, FormationLog, FormationRepository, NewEvent, NewFormation,
    NewFormationPlayer, ROOM_CODE_KEY, Team, VC_KEY,
};
use uuid::Uuid;

const APP_ROLE: &str = "assay_app";
const APP_ROLE_PASSWORD: &str = "assay_app_test";
const GUILD: GuildId = GuildId::new(1);
/// マイグレーションが入れる 5 ロール（各チームに 1 人ずつ）。
const ROLES: [&str; 5] = [
    "top_carry",
    "top_support",
    "jungle",
    "bot_carry",
    "bot_tank",
];

// ----------------------------------------------------------------------
// 偽 Discord
// ----------------------------------------------------------------------

#[derive(Default)]
struct FakeInner {
    next_id: u64,
    /// ギルドのボイスチャンネル（名前, ID）。
    voice_channels: Vec<(String, ChannelId)>,
    /// メッセージの現在の内容。
    messages: HashMap<(ChannelId, MessageId), OutgoingMessage>,
    /// 投稿の順（チャンネル, メッセージ）。
    sends: Vec<(ChannelId, MessageId)>,
    edits: Vec<(ChannelId, MessageId)>,
    responses: Vec<(InteractionId, InteractionResponse)>,
}

#[derive(Default)]
struct FakeGateway {
    inner: Mutex<FakeInner>,
    fail_sends: AtomicBool,
    fail_responses: AtomicBool,
}

impl FakeGateway {
    fn with_voice_channels(names: &[(&str, u64)]) -> Arc<Self> {
        let fake = Self::default();
        fake.lock().voice_channels = names
            .iter()
            .map(|(name, id)| ((*name).to_owned(), ChannelId::new(*id)))
            .collect();
        Arc::new(fake)
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, FakeInner> {
        self.inner.lock().unwrap()
    }

    fn message(&self, channel: ChannelId, id: MessageId) -> OutgoingMessage {
        self.lock()
            .messages
            .get(&(channel, id))
            .cloned()
            .expect("message exists")
    }

    fn sends_in(&self, channel: ChannelId) -> Vec<MessageId> {
        self.lock()
            .sends
            .iter()
            .filter(|(sent_to, _)| *sent_to == channel)
            .map(|(_, id)| *id)
            .collect()
    }

    fn total_sends(&self) -> usize {
        self.lock().sends.len()
    }

    fn responses(&self) -> Vec<(InteractionId, InteractionResponse)> {
        self.lock().responses.clone()
    }

    fn last_response(&self) -> InteractionResponse {
        self.responses().last().expect("a response").1.clone()
    }
}

#[async_trait]
impl Gateway for FakeGateway {
    async fn voice_channels(
        &self,
        _guild: GuildId,
    ) -> Result<Vec<(String, ChannelId)>, GatewayError> {
        Ok(self.lock().voice_channels.clone())
    }

    async fn send_message(
        &self,
        channel: ChannelId,
        message: &OutgoingMessage,
    ) -> Result<MessageId, GatewayError> {
        if self.fail_sends.load(Ordering::SeqCst) {
            return Err(GatewayError("simulated send failure".into()));
        }
        let mut inner = self.lock();
        inner.next_id += 1;
        let id = MessageId::new(5000 + inner.next_id);
        inner.messages.insert((channel, id), message.clone());
        inner.sends.push((channel, id));
        Ok(id)
    }

    async fn edit_message(
        &self,
        channel: ChannelId,
        message_id: MessageId,
        message: &OutgoingMessage,
    ) -> Result<(), GatewayError> {
        let mut inner = self.lock();
        let Some(slot) = inner.messages.get_mut(&(channel, message_id)) else {
            return Err(GatewayError(format!("unknown message {message_id}")));
        };
        *slot = message.clone();
        inner.edits.push((channel, message_id));
        Ok(())
    }

    async fn respond(
        &self,
        interaction: &InteractionRef,
        response: InteractionResponse,
    ) -> Result<(), GatewayError> {
        if self.fail_responses.load(Ordering::SeqCst) {
            return Err(GatewayError("Unknown interaction".into()));
        }
        self.lock().responses.push((interaction.id, response));
        Ok(())
    }
}

// ----------------------------------------------------------------------
// 道具
// ----------------------------------------------------------------------

struct AppDb {
    pool: PgPool,
    repo: FormationRepository,
}

impl AppDb {
    async fn connect(owner: &PgPool) -> Self {
        let options = (*owner.connect_options())
            .clone()
            .username(APP_ROLE)
            .password(APP_ROLE_PASSWORD);
        let pool = PgPoolOptions::new()
            .max_connections(10)
            .connect_with(options)
            .await
            .expect("connect as the app role");
        let repo = FormationRepository::new(pool.clone());
        Self { pool, repo }
    }

    async fn done(self) {
        self.pool.close().await;
    }
}

fn pid(n: u128) -> PlayerId {
    PlayerId(Uuid::from_u128(n))
}

fn fid(n: u128) -> FormationId {
    FormationId(Uuid::from_u128(0xd000 + n))
}

/// フィクスチャの players 1〜10。Discord ユーザー ID は 1001〜1010。
fn user(n: u64) -> UserId {
    UserId::new(1000 + n)
}

/// players 1〜5 が red、6〜10 が blue。μ は `mu(i)`。
fn formation_with_mu(n: u128, mu: impl Fn(u128) -> f64) -> NewFormation {
    NewFormation {
        id: fid(n),
        created_at: Utc::now(),
        matchmaking_params_version: 1,
        predicted_win_prob: 0.5,
        players: (1..=10)
            .map(|i| NewFormationPlayer {
                player_id: pid(i),
                team: if i <= 5 { Team::Red } else { Team::Blue },
                role: ROLES[(i as usize - 1) % 5].to_owned(),
                role_was_flexed: false,
                mu_at_match: mu(i),
                sigma_at_match: 8.33,
            })
            .collect(),
    }
}

/// μ は player 1 が最も高い（候補順 1, 2, 3。全員 red）。
fn formation(n: u128) -> NewFormation {
    formation_with_mu(n, |i| 30.0 - i as f64)
}

/// VC の組 `1` … `count`。red は `n * 100 + 1`、blue は `n * 100 + 2`。
fn pairs(count: u64) -> Vec<VcPair> {
    (1..=count)
        .map(|n| VcPair {
            label: n.to_string(),
            red: ChannelId::new(n * 100 + 1),
            blue: ChannelId::new(n * 100 + 2),
        })
        .collect()
}

fn config() -> DiscordConfig {
    DiscordConfig {
        token: "test".into(),
        guild_id: GUILD,
        vc_pool_size: 1,
        room_map: "kyogre".into(),
        room_checklist: vec!["設定 A".into(), "設定 B".into()],
        retries: 3,
        retry_backoff: Duration::from_millis(1),
    }
}

fn adapter(db: &AppDb, gateway: Arc<FakeGateway>, vc_pairs: u64) -> Adapter {
    let lifecycle = LifecycleConfig::default();
    Adapter::new(
        db.repo.clone(),
        gateway,
        config(),
        pairs(vc_pairs),
        lifecycle.clone(),
        Box::new(DefaultHostSelector::from_config(&lifecycle)),
    )
}

fn button(id: u64, user: UserId, custom_id: &str) -> IncomingInteraction {
    IncomingInteraction {
        id: InteractionId::new(id),
        token: format!("token-{id}"),
        user,
        guild: Some(GUILD),
        kind: IncomingKind::Button {
            custom_id: custom_id.to_owned(),
        },
    }
}

fn modal(id: u64, user: UserId, custom_id: &str, room_code: &str) -> IncomingInteraction {
    IncomingInteraction {
        id: InteractionId::new(id),
        token: format!("token-{id}"),
        user,
        guild: Some(GUILD),
        kind: IncomingKind::ModalSubmit {
            custom_id: custom_id.to_owned(),
            inputs: [(ROOM_CODE_INPUT_ID.to_owned(), room_code.to_owned())]
                .into_iter()
                .collect(),
        },
    }
}

fn custom(action: Action, formation: FormationId, player: PlayerId) -> String {
    CustomId::new(action, formation, player).encode()
}

async fn load(db: &AppDb, id: FormationId) -> (FormationLog, S, DiscordRecord) {
    let log = db.repo.load_events(id).await.unwrap();
    let snapshot = log.snapshot();
    assert!(snapshot.anomalies.is_empty(), "{:?}", snapshot.anomalies);
    let record = DiscordRecord::from_log(&log);
    (log, snapshot.state, record)
}

fn kinds(log: &FormationLog) -> Vec<K> {
    log.events.iter().map(|stored| stored.event.kind).collect()
}

fn abandon_reason(log: &FormationLog) -> AbandonReason {
    match &log.events.last().unwrap().event.payload {
        EventPayload::Abandon(reason) => *reason,
        other => panic!("last event is not abandoned: {other:?}"),
    }
}

/// formation を作って VC 割当〜最初の打診まで進め、（VC, 打診チャンネル, 打診メッセージ）を返す。
async fn started(db: &AppDb, adapter: &Adapter, n: u128) -> (VcPair, ChannelId, MessageId) {
    db.repo.create_formation(&formation(n)).await.unwrap();
    adapter.start_formation(fid(n)).await.unwrap();
    let (_, state, record) = load(db, fid(n)).await;
    assert_eq!(
        state,
        S::AwaitingHost {
            pending: Some(pid(1)),
            tried: 1
        }
    );
    let (channel, prompt) = record.host_prompt.unwrap();
    (record.vc.unwrap(), channel, prompt)
}

/// 打診メッセージの [部屋を作る] / [今回はパスする] の custom_id。
fn prompt_buttons(fake: &FakeGateway, channel: ChannelId, prompt: MessageId) -> (String, String) {
    let message = fake.message(channel, prompt);
    assert_eq!(message.buttons.len(), 2, "{message:?}");
    (
        message.buttons[0].custom_id.clone(),
        message.buttons[1].custom_id.clone(),
    )
}

/// 3 人の候補が全員パスして解散させる。
async fn decline_all(adapter: &Adapter, fake: &FakeGateway, n: u128, first_interaction: u64) {
    for step in 0..3u64 {
        let (_, _, record) = load_record(adapter, n).await;
        let (channel, prompt) = record.host_prompt.unwrap();
        let (_, decline) = prompt_buttons(fake, channel, prompt);
        let parsed = CustomId::parse(&decline).unwrap();
        let presser = UserId::new(1000 + parsed.player.0.as_u128() as u64);
        adapter
            .handle_interaction(button(first_interaction + step, presser, &decline))
            .await
            .unwrap();
    }
}

async fn load_record(adapter: &Adapter, n: u128) -> (FormationLog, S, DiscordRecord) {
    let log = adapter.repository().load_events(fid(n)).await.unwrap();
    let state = log.snapshot().state;
    let record = DiscordRecord::from_log(&log);
    (log, state, record)
}

// ----------------------------------------------------------------------
// 正常系
// ----------------------------------------------------------------------

#[sqlx::test(
    fixtures("reference", "app_role"),
    fixtures("../ops/app_privileges.sql")
)]
async fn matched_to_code_posted_posts_to_both_voice_chats_and_records_every_id(owner: PgPool) {
    let db = AppDb::connect(&owner).await;
    let fake = Arc::new(FakeGateway::default());
    let adapter = adapter(&db, fake.clone(), 2);

    // 1. matched → VC 割当（LRU: 未使用の 1）、割り当て表を両 VC に、vc_assigned
    let (vc, prompt_channel, prompt) = started(&db, &adapter, 1).await;
    assert_eq!(vc.label, "1");
    assert_eq!(vc.red, ChannelId::new(101));
    assert_eq!(vc.blue, ChannelId::new(102));
    let (log, _, record) = load(&db, fid(1)).await;
    assert_eq!(kinds(&log), vec![K::VcAssigned, K::HostAssigned]);
    let vc_payload = &log.events[0].payload;
    assert_eq!(vc_payload[VC_KEY]["label"], "1");
    assert_eq!(vc_payload[VC_KEY]["red_channel_id"], "101");
    assert_eq!(vc_payload[VC_KEY]["blue_channel_id"], "102");
    assert!(vc_payload.get(DISCORD_FAILURES_KEY).is_none());
    let red_assignment = record.assignment_message_ids[&Team::Red];
    let blue_assignment = record.assignment_message_ids[&Team::Blue];
    assert_eq!(
        vc_payload[ASSIGNMENT_MESSAGE_IDS_KEY]["red"],
        red_assignment.get().to_string()
    );
    assert_eq!(
        vc_payload[ASSIGNMENT_MESSAGE_IDS_KEY]["blue"],
        blue_assignment.get().to_string()
    );
    assert_eq!(fake.sends_in(vc.red)[0], red_assignment);
    assert_eq!(fake.sends_in(vc.blue)[0], blue_assignment);

    // 2. 割り当て表: 見出し、10 人、VC の案内、マップ + チェックリスト。両 VC で同じ内容
    let assignment = fake.message(vc.red, red_assignment);
    assert_eq!(assignment, fake.message(vc.blue, blue_assignment));
    assert!(assignment.content.starts_with("**[vc1] "));
    assert!(assignment.content.contains("match-"));
    assert_eq!(assignment.mention_users.len(), 10);
    assert!(assignment.content.contains("vc1-red"));
    assert!(assignment.content.contains("vc1-blue"));
    assert!(assignment.content.contains("<#101>"));
    assert!(assignment.content.contains("<#102>"));
    assert!(assignment.content.contains("上キャリー"));
    assert!(assignment.content.contains("- [ ] マップ: kyogre"));
    assert!(assignment.content.contains("- [ ] 設定 A"));
    assert!(assignment.buttons.is_empty());

    // 3. 打診: 候補 1（red）なので red の VC にだけ。host_assigned にチャンネルとメッセージ ID
    assert_eq!(prompt_channel, vc.red);
    assert_eq!(fake.sends_in(vc.red).len(), 2);
    assert_eq!(
        fake.sends_in(vc.blue).len(),
        1,
        "no prompt on the blue side"
    );
    let host_assigned = &log.events[1].payload;
    assert_eq!(host_assigned[HOST_PROMPT_CHANNEL_ID_KEY], "101");
    assert_eq!(
        host_assigned[HOST_PROMPT_MESSAGE_ID_KEY],
        prompt.get().to_string()
    );
    let prompt_message = fake.message(vc.red, prompt);
    assert!(prompt_message.content.starts_with("**[vc1] "));
    assert_eq!(prompt_message.mention_users, vec![user(1)]);
    let (accept, decline) = prompt_buttons(&fake, vc.red, prompt);
    assert_eq!(accept, custom(Action::HostAccept, fid(1), pid(1)));
    assert_eq!(decline, custom(Action::HostDecline, fid(1), pid(1)));

    // 4. 「部屋を作る」→ モーダル → 状態表示の更新 → host_accepted（冪等キー = インタラクション ID）
    adapter
        .handle_interaction(button(9001, user(1), &accept))
        .await
        .unwrap();
    let (log, state, _) = load(&db, fid(1)).await;
    assert_eq!(
        state,
        S::AwaitingCode {
            host: pid(1),
            tried: 1
        }
    );
    assert_eq!(log.events[2].event.idempotency_key, "9001");
    assert!(matches!(
        fake.last_response(),
        InteractionResponse::Modal(_)
    ));
    let prompt_message = fake.message(vc.red, prompt);
    assert_eq!(prompt_message.buttons.len(), 1);
    assert_eq!(
        prompt_message.buttons[0].custom_id,
        custom(Action::CodeOpen, fid(1), pid(1))
    );

    // 5. モーダル送信 → 両 VC に開示 → 状態表示 → code_posted（ルーム ID + 両方のメッセージ ID）
    let submit = custom(Action::CodeSubmit, fid(1), pid(1));
    adapter
        .handle_interaction(modal(9002, user(1), &submit, " 1234 "))
        .await
        .unwrap();
    let (log, state, record) = load(&db, fid(1)).await;
    assert_eq!(
        state,
        S::InProgress {
            host: pid(1),
            room_code: "1234".into()
        }
    );
    let code_event = &log.events[3];
    assert_eq!(code_event.event.kind, K::CodePosted);
    assert_eq!(code_event.event.idempotency_key, "9002");
    assert_eq!(
        code_event.event.payload,
        EventPayload::RoomCode("1234".into())
    );
    assert_eq!(code_event.payload[ROOM_CODE_KEY], "1234");
    let red_code = record.code_message_ids[&Team::Red];
    let blue_code = record.code_message_ids[&Team::Blue];
    assert_eq!(
        code_event.payload[CODE_MESSAGE_IDS_KEY]["red"],
        red_code.get().to_string()
    );
    assert_eq!(
        code_event.payload[CODE_MESSAGE_IDS_KEY]["blue"],
        blue_code.get().to_string()
    );

    // 6. 開示メッセージは両 VC で同じ内容。残り 9 人に通知し、ルーム ID と集合場所を含む
    let reveal = fake.message(vc.red, red_code);
    assert_eq!(reveal, fake.message(vc.blue, blue_code));
    let mut pinged = reveal.mention_users.clone();
    pinged.sort();
    assert_eq!(pinged, (2..=10).map(user).collect::<Vec<_>>());
    assert!(reveal.content.starts_with("**[vc1] "));
    assert!(reveal.content.contains("**1234**"));
    assert!(reveal.content.contains("vc1-red"));
    match fake.last_response() {
        InteractionResponse::UpdateMessage(status) => {
            assert!(status.content.contains("**1234**"));
            assert_eq!(status.buttons.len(), 1);
        }
        other => panic!("expected UpdateMessage, got {other:?}"),
    }

    // red: 割り当て表・打診・開示、blue: 割り当て表・開示。それ以外の投稿は無い
    assert_eq!(
        fake.sends_in(vc.red),
        vec![red_assignment, prompt, red_code]
    );
    assert_eq!(fake.sends_in(vc.blue), vec![blue_assignment, blue_code]);
    db.done().await;
}

#[sqlx::test(
    fixtures("reference", "app_role"),
    fixtures("../ops/app_privileges.sql")
)]
async fn declining_three_times_abandons_and_edits_the_same_prompt(owner: PgPool) {
    let db = AppDb::connect(&owner).await;
    let fake = Arc::new(FakeGateway::default());
    let adapter = adapter(&db, fake.clone(), 1);
    let (vc, channel, prompt) = started(&db, &adapter, 1).await;

    // 候補 1, 2, 3 は全員 red なので、同じ打診メッセージが編集され続ける
    for (n, candidate) in [(1u64, pid(1)), (2, pid(2)), (3, pid(3))] {
        let (_, decline) = prompt_buttons(&fake, channel, prompt);
        assert_eq!(decline, custom(Action::HostDecline, fid(1), candidate));
        assert_eq!(fake.message(channel, prompt).mention_users, vec![user(n)]);
        adapter
            .handle_interaction(button(9100 + n, user(n), &decline))
            .await
            .unwrap();
    }

    let (log, state, _) = load(&db, fid(1)).await;
    assert_eq!(state, S::Abandoned);
    assert_eq!(
        kinds(&log),
        vec![
            K::VcAssigned,
            K::HostAssigned,
            K::HostDeclined,
            K::HostAssigned,
            K::HostDeclined,
            K::HostAssigned,
            K::HostDeclined,
            K::Abandoned,
        ]
    );
    assert_eq!(abandon_reason(&log), AbandonReason::HostCandidatesExhausted);
    // red: 割り当て表 + 打診 1 通（最後は解散の通知に編集）。blue: 割り当て表 + 解散の通知
    assert_eq!(fake.sends_in(vc.red).len(), 2);
    let final_prompt = fake.message(channel, prompt);
    assert!(final_prompt.buttons.is_empty());
    assert!(final_prompt.content.contains("解散"));
    let blue_sends = fake.sends_in(vc.blue);
    assert_eq!(blue_sends.len(), 2);
    assert!(
        fake.message(vc.blue, blue_sends[1])
            .content
            .contains("解散")
    );
    // 3 回とも Acknowledge で応答した（新しいメッセージは積まない）
    assert!(
        fake.responses()
            .iter()
            .all(|(_, response)| *response == InteractionResponse::Acknowledge)
    );

    // 終端に達したので VC は解放され、次の formation が同じ組を使える
    db.repo.create_formation(&formation(2)).await.unwrap();
    adapter.start_formation(fid(2)).await.unwrap();
    let (_, _, record) = load(&db, fid(2)).await;
    assert_eq!(record.vc.unwrap().label, "1");
    db.done().await;
}

#[sqlx::test(
    fixtures("reference", "app_role"),
    fixtures("../ops/app_privileges.sql")
)]
async fn a_candidate_on_the_other_team_gets_the_prompt_in_their_own_voice_chat(owner: PgPool) {
    let db = AppDb::connect(&owner).await;
    let fake = Arc::new(FakeGateway::default());
    let adapter = adapter(&db, fake.clone(), 1);
    // 候補順: player 1（red, 29）, player 6（blue, 28.5）, player 2（red, 28）
    let custom_mu = |i: u128| if i == 6 { 28.5 } else { 30.0 - i as f64 };
    db.repo
        .create_formation(&formation_with_mu(1, custom_mu))
        .await
        .unwrap();
    adapter.start_formation(fid(1)).await.unwrap();
    let (_, _, record) = load(&db, fid(1)).await;
    let vc = record.vc.unwrap();
    let (channel, prompt) = record.host_prompt.unwrap();
    assert_eq!(channel, vc.red);

    // 候補 1 が辞退 → 候補 6 は blue なので、blue の VC に新しい打診。red の古い打診はボタンを外す
    let (_, decline) = prompt_buttons(&fake, channel, prompt);
    adapter
        .handle_interaction(button(9201, user(1), &decline))
        .await
        .unwrap();
    let (log, state, record) = load(&db, fid(1)).await;
    assert_eq!(
        state,
        S::AwaitingHost {
            pending: Some(pid(6)),
            tried: 2
        }
    );
    let (channel2, prompt2) = record.host_prompt.unwrap();
    assert_eq!(channel2, vc.blue);
    assert_ne!(prompt2, prompt);
    assert_eq!(
        log.events.last().unwrap().payload[HOST_PROMPT_CHANNEL_ID_KEY],
        "102"
    );
    let moved = fake.message(vc.red, prompt);
    assert!(moved.buttons.is_empty());
    assert!(moved.content.contains("blue"));
    let new_prompt = fake.message(vc.blue, prompt2);
    assert_eq!(new_prompt.mention_users, vec![user(6)]);
    assert_eq!(new_prompt.buttons.len(), 2);

    // 候補 6 が承諾 → blue 側の打診が状態表示に。ルーム ID は両 VC に出る
    let (accept, _) = prompt_buttons(&fake, vc.blue, prompt2);
    adapter
        .handle_interaction(button(9202, user(6), &accept))
        .await
        .unwrap();
    assert_eq!(fake.message(vc.blue, prompt2).buttons.len(), 1);
    let submit = custom(Action::CodeSubmit, fid(1), pid(6));
    adapter
        .handle_interaction(modal(9203, user(6), &submit, "6666"))
        .await
        .unwrap();
    let (_, state, record) = load(&db, fid(1)).await;
    assert!(matches!(state, S::InProgress { host, .. } if host == pid(6)));
    assert_eq!(record.code_message_ids.len(), 2);
    db.done().await;
}

#[sqlx::test(
    fixtures("reference", "app_role"),
    fixtures("../ops/app_privileges.sql")
)]
async fn second_room_code_replaces_the_first_in_both_reveal_messages(owner: PgPool) {
    let db = AppDb::connect(&owner).await;
    let fake = Arc::new(FakeGateway::default());
    let adapter = adapter(&db, fake.clone(), 1);
    let (vc, channel, prompt) = started(&db, &adapter, 1).await;
    let (accept, _) = prompt_buttons(&fake, channel, prompt);
    adapter
        .handle_interaction(button(9301, user(1), &accept))
        .await
        .unwrap();
    let submit = custom(Action::CodeSubmit, fid(1), pid(1));
    adapter
        .handle_interaction(modal(9302, user(1), &submit, "1111"))
        .await
        .unwrap();
    let (_, _, first) = load(&db, fid(1)).await;
    let sends_before = fake.total_sends();

    // 「ルーム ID を修正」→ 現在のコードが初期値のモーダル
    let open = fake.message(channel, prompt).buttons[0].custom_id.clone();
    assert_eq!(open, custom(Action::CodeOpen, fid(1), pid(1)));
    adapter
        .handle_interaction(button(9303, user(1), &open))
        .await
        .unwrap();
    match fake.last_response() {
        InteractionResponse::Modal(modal) => assert_eq!(modal.prefill.as_deref(), Some("1111")),
        other => panic!("expected Modal, got {other:?}"),
    }

    // 別のインタラクション ID で 2 回目 → 訂正として code_posted が積まれ、両方の開示メッセージが編集される
    adapter
        .handle_interaction(modal(9304, user(1), &submit, "2222"))
        .await
        .unwrap();
    let (log, state, second) = load(&db, fid(1)).await;
    assert_eq!(
        state,
        S::InProgress {
            host: pid(1),
            room_code: "2222".into()
        }
    );
    assert_eq!(
        kinds(&log),
        vec![
            K::VcAssigned,
            K::HostAssigned,
            K::HostAccepted,
            K::CodePosted,
            K::CodePosted
        ]
    );
    assert_eq!(second.code_message_ids, first.code_message_ids);
    for team in Team::ALL {
        let reveal = fake.message(vc.channel(team), second.code_message_ids[&team]);
        assert!(reveal.content.contains("**2222**"), "{team}");
        assert!(!reveal.content.contains("1111"), "{team}");
    }
    assert_eq!(fake.total_sends(), sends_before, "no new reveal messages");
    db.done().await;
}

#[sqlx::test(
    fixtures("reference", "app_role"),
    fixtures("../ops/app_privileges.sql")
)]
async fn stale_and_foreign_presses_are_rejected_without_writing(owner: PgPool) {
    let db = AppDb::connect(&owner).await;
    let fake = Arc::new(FakeGateway::default());
    let adapter = adapter(&db, fake.clone(), 1);
    let (_, channel, prompt) = started(&db, &adapter, 1).await;
    let (accept, decline) = prompt_buttons(&fake, channel, prompt);

    // 打診されていない人が押した
    adapter
        .handle_interaction(button(9401, user(5), &accept))
        .await
        .unwrap();
    assert!(matches!(
        fake.last_response(),
        InteractionResponse::Ephemeral(_)
    ));
    let (log, _, _) = load(&db, fid(1)).await;
    assert_eq!(log.events.len(), 2);

    // 候補 1 が辞退 → 候補 2 へ。古い（候補 1 の）ボタンはもう無効
    adapter
        .handle_interaction(button(9402, user(1), &decline))
        .await
        .unwrap();
    adapter
        .handle_interaction(button(9403, user(1), &accept))
        .await
        .unwrap();
    assert!(matches!(
        fake.last_response(),
        InteractionResponse::Ephemeral(_)
    ));
    let (log, state, _) = load(&db, fid(1)).await;
    assert_eq!(
        state,
        S::AwaitingHost {
            pending: Some(pid(2)),
            tried: 2
        }
    );
    assert_eq!(log.events.len(), 4);

    // 同じインタラクション ID の再送は 1 行だけ（モーダルは再度開く）
    let (accept2, _) = prompt_buttons(&fake, channel, prompt);
    adapter
        .handle_interaction(button(9404, user(2), &accept2))
        .await
        .unwrap();
    adapter
        .handle_interaction(button(9404, user(2), &accept2))
        .await
        .unwrap();
    let (log, state, _) = load(&db, fid(1)).await;
    assert_eq!(
        state,
        S::AwaitingCode {
            host: pid(2),
            tried: 2
        }
    );
    assert_eq!(log.events.len(), 5);
    assert!(matches!(
        fake.last_response(),
        InteractionResponse::Modal(_)
    ));

    // ホスト以外はルーム ID を入力できない。空のルーム ID も受け付けない
    let submit = custom(Action::CodeSubmit, fid(1), pid(2));
    adapter
        .handle_interaction(modal(9405, user(3), &submit, "9999"))
        .await
        .unwrap();
    adapter
        .handle_interaction(modal(9406, user(2), &submit, "   "))
        .await
        .unwrap();
    let (log, _, record) = load(&db, fid(1)).await;
    assert_eq!(log.events.len(), 5);
    assert!(record.code_message_ids.is_empty());

    // 別ギルドや知らない custom_id は無視する
    let mut foreign = button(9407, user(2), &accept2);
    foreign.guild = Some(GuildId::new(99));
    adapter.handle_interaction(foreign).await.unwrap();
    adapter
        .handle_interaction(button(9408, user(2), "something:else"))
        .await
        .unwrap();
    assert_eq!(fake.responses().len(), 7);
    db.done().await;
}

#[sqlx::test(
    fixtures("reference", "app_role"),
    fixtures("../ops/app_privileges.sql")
)]
async fn a_restarted_process_continues_from_the_log(owner: PgPool) {
    let db = AppDb::connect(&owner).await;
    let fake = Arc::new(FakeGateway::default());
    let first = adapter(&db, fake.clone(), 3);
    let (vc, channel, prompt) = started(&db, &first, 1).await;
    let (accept, _) = prompt_buttons(&fake, channel, prompt);
    drop(first);

    // 「再起動」: 新しいアダプタ。Discord 側の検索は無い（Gateway に検索操作が無い）
    let second = adapter(&db, fake.clone(), 3);
    second
        .handle_interaction(button(9501, user(1), &accept))
        .await
        .unwrap();
    // ログに記録された打診メッセージを編集した
    assert_eq!(fake.lock().edits.last(), Some(&(channel, prompt)));
    let submit = custom(Action::CodeSubmit, fid(1), pid(1));
    second
        .handle_interaction(modal(9502, user(1), &submit, "4242"))
        .await
        .unwrap();
    let (_, state, record) = load(&db, fid(1)).await;
    assert_eq!(
        state,
        S::InProgress {
            host: pid(1),
            room_code: "4242".into()
        }
    );
    assert_eq!(record.vc.as_ref(), Some(&vc));

    // 辞退の直後に落ちた formation（候補未指名）と、まだ何もしていない matched の formation を再開する
    let (_, channel2, prompt2) = started(&db, &second, 2).await;
    db.repo
        .append_event(
            fid(2),
            &NewEvent::new(K::HostDeclined, Some(pid(1)), Utc::now(), "manual-decline"),
        )
        .await
        .unwrap();
    db.repo.create_formation(&formation(3)).await.unwrap();

    let third = adapter(&db, fake.clone(), 3);
    third.resume_unfinished().await.unwrap();

    let (_, state2, record2) = load(&db, fid(2)).await;
    assert_eq!(
        state2,
        S::AwaitingHost {
            pending: Some(pid(2)),
            tried: 2
        }
    );
    assert_eq!(
        record2.host_prompt,
        Some((channel2, prompt2)),
        "same prompt, edited"
    );
    assert_eq!(fake.message(channel2, prompt2).mention_users, vec![user(2)]);

    let (_, state3, record3) = load(&db, fid(3)).await;
    assert_eq!(
        state3,
        S::AwaitingHost {
            pending: Some(pid(1)),
            tried: 1
        }
    );
    assert_eq!(record3.vc.unwrap().label, "3", "1 and 2 are still occupied");
    db.done().await;
}

// ----------------------------------------------------------------------
// VC プール: 名前解決と LRU
// ----------------------------------------------------------------------

#[sqlx::test(
    fixtures("reference", "app_role"),
    fixtures("../ops/app_privileges.sql")
)]
async fn vc_pool_is_resolved_by_name_at_startup_and_fails_on_gaps_or_duplicates(owner: PgPool) {
    let db = AppDb::connect(&owner).await;

    let complete = FakeGateway::with_voice_channels(&[
        ("lobby", 1),
        ("vc2-blue", 202),
        ("vc1-red", 101),
        ("vc1-blue", 102),
        ("vc2-red", 201),
    ]);
    let pairs = resolve_vc_pool(&*complete, GUILD, 2).await.unwrap();
    assert_eq!(pairs, self::pairs(2));

    let missing = FakeGateway::with_voice_channels(&[("vc1-red", 101), ("vc1-blue", 102)]);
    let error = resolve_vc_pool(&*missing, GUILD, 2).await.unwrap_err();
    assert!(
        matches!(
            &error,
            VcPoolSetupError::Resolve(ResolveError::Missing(names))
                if names == &vec!["vc2-red".to_owned(), "vc2-blue".to_owned()]
        ),
        "{error}"
    );

    let duplicated =
        FakeGateway::with_voice_channels(&[("vc1-red", 101), ("vc1-blue", 102), ("vc1-blue", 103)]);
    let error = resolve_vc_pool(&*duplicated, GUILD, 1).await.unwrap_err();
    assert!(
        matches!(
            &error,
            VcPoolSetupError::Resolve(ResolveError::Duplicated(names))
                if names == &vec!["vc1-blue".to_owned()]
        ),
        "{error}"
    );

    // 解決した組でそのまま動く
    let adapter = Adapter::new(
        db.repo.clone(),
        complete.clone(),
        config(),
        pairs,
        LifecycleConfig::default(),
        Box::new(DefaultHostSelector::default()),
    );
    let (vc, _, _) = started(&db, &adapter, 1).await;
    assert_eq!(vc.red, ChannelId::new(101));
    db.done().await;
}

#[sqlx::test(
    fixtures("reference", "app_role"),
    fixtures("../ops/app_privileges.sql")
)]
async fn voice_channels_are_assigned_least_recently_used_first(owner: PgPool) {
    let db = AppDb::connect(&owner).await;
    let fake = Arc::new(FakeGateway::default());
    let adapter = adapter(&db, fake.clone(), 2);

    // 1 を使って解散 → 次は未使用の 2 → 解散 → 次は最も古く解放された 1（設定順なら 1 のまま）
    let (vc1, _, _) = started(&db, &adapter, 1).await;
    assert_eq!(vc1.label, "1");
    decline_all(&adapter, &fake, 1, 9600).await;
    let (vc2, _, _) = started(&db, &adapter, 2).await;
    assert_eq!(vc2.label, "2", "never used comes first");
    decline_all(&adapter, &fake, 2, 9610).await;
    let (vc3, _, _) = started(&db, &adapter, 3).await;
    assert_eq!(vc3.label, "1", "released earliest comes first");
    decline_all(&adapter, &fake, 3, 9620).await;
    let (vc4, _, _) = started(&db, &adapter, 4).await;
    assert_eq!(vc4.label, "2");

    // 4 が使用中なので、次は 1 しか無い。その次は無い → abandoned
    let (vc5, _, _) = started(&db, &adapter, 5).await;
    assert_eq!(vc5.label, "1");
    db.repo.create_formation(&formation(6)).await.unwrap();
    adapter.start_formation(fid(6)).await.unwrap();
    let (log, state, _) = load(&db, fid(6)).await;
    assert_eq!(state, S::Abandoned, "the ten players go back to the queue");
    assert_eq!(kinds(&log), vec![K::Abandoned]);
    assert_eq!(abandon_reason(&log), AbandonReason::VcPoolExhausted);
    assert_eq!(
        log.events[0].payload[ABANDON_REASON_KEY],
        "vc_pool_exhausted"
    );
    db.done().await;
}

// ----------------------------------------------------------------------
// 失敗経路: すべて abandoned になり、詳細が payload に残る
// ----------------------------------------------------------------------

#[sqlx::test(
    fixtures("reference", "app_role"),
    fixtures("../ops/app_privileges.sql")
)]
async fn a_failed_post_abandons_the_formation_with_the_operation_recorded(owner: PgPool) {
    let db = AppDb::connect(&owner).await;
    let fake = Arc::new(FakeGateway::default());
    fake.fail_sends.store(true, Ordering::SeqCst);
    let adapter = adapter(&db, fake.clone(), 1);
    db.repo.create_formation(&formation(1)).await.unwrap();

    adapter.start_formation(fid(1)).await.unwrap();
    let (log, state, record) = load(&db, fid(1)).await;
    assert_eq!(state, S::Abandoned);
    assert_eq!(kinds(&log), vec![K::VcAssigned, K::Abandoned]);
    assert_eq!(abandon_reason(&log), AbandonReason::DiscordCallFailed);
    // vc_assigned: VC 割当は残り、割り当て表の失敗（両側）も残る
    assert_eq!(record.vc.unwrap().label, "1");
    assert!(record.assignment_message_ids.is_empty());
    let vc_failures = log.events[0].payload[DISCORD_FAILURES_KEY]
        .as_array()
        .unwrap()
        .clone();
    let operations: Vec<&str> = vc_failures
        .iter()
        .map(|failure| failure[OPERATION_KEY].as_str().unwrap())
        .collect();
    assert_eq!(
        operations,
        vec!["send:assignment:red", "send:assignment:blue"]
    );
    assert_eq!(vc_failures[0][ATTEMPTS_KEY], 3);
    assert_eq!(vc_failures[0]["error"], "simulated send failure");
    // 解散の通知も投稿できなかったので、それも残る
    let abandon_failures = log.events[1].payload[DISCORD_FAILURES_KEY]
        .as_array()
        .unwrap()
        .clone();
    let operations: Vec<&str> = abandon_failures
        .iter()
        .map(|failure| failure[OPERATION_KEY].as_str().unwrap())
        .collect();
    assert_eq!(
        operations,
        vec!["send:abandoned_notice:red", "send:abandoned_notice:blue"]
    );

    // VC は解放されている
    fake.fail_sends.store(false, Ordering::SeqCst);
    db.repo.create_formation(&formation(2)).await.unwrap();
    adapter.start_formation(fid(2)).await.unwrap();
    let (_, _, record) = load(&db, fid(2)).await;
    assert_eq!(record.vc.unwrap().label, "1");
    db.done().await;
}

#[sqlx::test(
    fixtures("reference", "app_role"),
    fixtures("../ops/app_privileges.sql")
)]
async fn failed_interaction_responses_are_recorded_in_the_event(owner: PgPool) {
    let db = AppDb::connect(&owner).await;
    let fake = Arc::new(FakeGateway::default());
    let adapter = adapter(&db, fake.clone(), 1);
    let (_, channel, prompt) = started(&db, &adapter, 1).await;
    let (accept, _) = prompt_buttons(&fake, channel, prompt);

    fake.fail_responses.store(true, Ordering::SeqCst);
    adapter
        .handle_interaction(button(9701, user(1), &accept))
        .await
        .unwrap();
    let (log, state, _) = load(&db, fid(1)).await;
    // 状態は進み、応答の失敗が host_accepted の payload に残る。状態表示の編集は成功している
    assert_eq!(
        state,
        S::AwaitingCode {
            host: pid(1),
            tried: 1
        }
    );
    let failures = log.events[2].payload[DISCORD_FAILURES_KEY]
        .as_array()
        .unwrap();
    assert_eq!(failures.len(), 1);
    assert_eq!(failures[0][OPERATION_KEY], "respond:modal");
    assert_eq!(fake.message(channel, prompt).buttons.len(), 1);

    // ホストは「ルーム ID を入力」から続けられる
    fake.fail_responses.store(false, Ordering::SeqCst);
    let submit = custom(Action::CodeSubmit, fid(1), pid(1));
    adapter
        .handle_interaction(modal(9702, user(1), &submit, "7777"))
        .await
        .unwrap();
    let (log, state, _) = load(&db, fid(1)).await;
    assert!(matches!(state, S::InProgress { .. }));
    assert!(log.events[3].payload.get(DISCORD_FAILURES_KEY).is_none());
    db.done().await;
}

// ----------------------------------------------------------------------
// HTTP API（手動テストの入口）
// ----------------------------------------------------------------------

#[sqlx::test(
    fixtures("reference", "app_role"),
    fixtures("../ops/app_privileges.sql")
)]
async fn internal_formations_endpoint_requires_the_token_and_starts_a_formation(owner: PgPool) {
    use axum::body::Body;
    use axum::http::{Request, StatusCode, header};
    use http_body_util::BodyExt;
    use serde_json::json;
    use tower::ServiceExt;
    use unite_matchsystem::api::{ApiState, router};

    let db = AppDb::connect(&owner).await;
    let fake = Arc::new(FakeGateway::default());
    let adapter = Arc::new(adapter(&db, fake.clone(), 1));
    let players = |role_of: fn(usize) -> &'static str, first_user: i64| -> Vec<serde_json::Value> {
        (0..10)
            .map(|i| {
                json!({
                    "discord_user_id": first_user + i as i64,
                    "team": if i < 5 { "red" } else { "blue" },
                    "role": role_of(i),
                    "mu_at_match": 30.0 - i as f64,
                })
            })
            .collect()
    };
    let post = |token: Option<&str>, body: serde_json::Value| {
        let mut request =
            Request::post("/internal/formations").header(header::CONTENT_TYPE, "application/json");
        if let Some(token) = token {
            request = request.header(header::AUTHORIZATION, format!("Bearer {token}"));
        }
        request.body(Body::from(body.to_string())).unwrap()
    };
    let valid = json!({ "players": players(|i| ROLES[i % 5], 1001) });

    // トークン未設定 → エンドポイント自体が無い
    let unmounted = router(ApiState {
        adapter: adapter.clone(),
        internal_token: None,
    });
    let response = unmounted
        .clone()
        .oneshot(post(Some("secret"), valid.clone()))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    let response = unmounted
        .oneshot(Request::get("/healthz").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    // トークン設定あり → Bearer が無い / 違う → 401
    let app = router(ApiState {
        adapter: adapter.clone(),
        internal_token: Some(Arc::from("secret")),
    });
    for token in [None, Some("wrong"), Some("secre")] {
        let response = app
            .clone()
            .oneshot(post(token, valid.clone()))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED, "{token:?}");
    }

    // 未登録ユーザー → 400
    let response = app
        .clone()
        .oneshot(post(
            Some("secret"),
            json!({ "players": players(|i| ROLES[i % 5], 5001) }),
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);

    // 存在しないロール → 400（FK 違反を入力エラーとして返す）
    let response = app
        .clone()
        .oneshot(post(
            Some("secret"),
            json!({ "players": players(|_| "no_such_role", 1001) }),
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);

    // 同一チーム同一ロール → 400（unique 違反を入力エラーとして返す）
    let response = app
        .clone()
        .oneshot(post(
            Some("secret"),
            json!({ "players": players(|_| "jungle", 1001) }),
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert!(db.repo.list_unfinished().await.unwrap().is_empty());

    // 正常系 → 202、formation が作られ、バックグラウンドで VC 割当が進む
    let response = app
        .clone()
        .oneshot(post(Some("secret"), valid))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::ACCEPTED);
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    let body: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    let id = FormationId(Uuid::parse_str(body["formation_id"].as_str().unwrap()).unwrap());

    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let (_, state, record) = load(&db, id).await;
        // vc_assigned と host_assigned の間で観測することがあるので、打診まで待つ
        if !matches!(state, S::Matched | S::AwaitingHost { pending: None, .. }) {
            assert_eq!(
                state,
                S::AwaitingHost {
                    pending: Some(pid(1)),
                    tried: 1
                }
            );
            assert_eq!(record.vc.unwrap().label, "1");
            break;
        }
        assert!(Instant::now() < deadline, "start_formation did not run");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    db.done().await;
}
