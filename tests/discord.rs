//! Discord アダプタの統合テスト。実際の PostgreSQL と、メモリ上の偽 Discord（`FakeGateway`）を使う。
//!
//! 実行方法は `tests/persistence.rs` と同じ（`DATABASE_URL` を設定して `cargo test --test discord`）。
//! リポジトリはアプリ用ロールで接続する。
//!
//! 検証すること（Phase 4 の完了条件）:
//! - `matched` から `code_posted` までが通り、スレッド・メッセージ・VC 割当の ID が全部 payload に入る
//! - 再起動（別のアダプタ）でも、Discord 側を検索し直さずにログから続きを操作できる
//! - ホストが「パスする」を 3 回押すと `abandoned` になり、打診メッセージは 1 通だけ（編集で更新）
//! - モーダルを 2 回送ると 2 回目のルーム ID が開示メッセージに反映される
//! - 失敗経路はすべて `abandoned` になり、失敗の詳細が payload に入る
//!   （VC 無し、スレッド作成失敗、参加者の追加失敗、投稿失敗）。VC は終端で解放される
//! - インタラクション応答の失敗も payload に残る
//! - `/internal/formations` はトークン無しでは到達できない

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
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
    ASSIGNMENT_MESSAGE_ID_KEY, ATTEMPTS_KEY, CODE_MESSAGE_ID_KEY, DISCORD_FAILURES_KEY,
    DISCORD_USER_ID_KEY, DiscordRecord, FAILED_PLAYERS_KEY, HOST_PROMPT_MESSAGE_ID_KEY,
    OPERATION_KEY, PLAYER_ID_KEY, VC_KEY,
};
use unite_matchsystem::discord::{DiscordConfig, VcPair};
use unite_matchsystem::lifecycle::{
    AbandonReason, DefaultHostSelector, EventPayload, FormationEventKind as K, FormationId,
    FormationState as S, LifecycleConfig, PlayerId,
};
use unite_matchsystem::persistence::{
    ABANDON_REASON_KEY, DISCORD_THREAD_ID_KEY, FormationLog, FormationRepository, NewEvent,
    NewFormation, NewFormationPlayer, ROOM_CODE_KEY, Team,
};
use uuid::Uuid;

const APP_ROLE: &str = "unite_app_test";
const APP_ROLE_PASSWORD: &str = "unite_app_test";
const PARENT: ChannelId = ChannelId::new(10);
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
    /// 作られたスレッド（親, スレッド, 名前）。
    threads: Vec<(ChannelId, ChannelId, String)>,
    members: HashMap<ChannelId, Vec<UserId>>,
    /// メッセージの現在の内容。
    messages: HashMap<(ChannelId, MessageId), OutgoingMessage>,
    /// 投稿の順（チャンネル, メッセージ）。
    sends: Vec<(ChannelId, MessageId)>,
    edits: Vec<(ChannelId, MessageId)>,
    responses: Vec<(InteractionId, InteractionResponse)>,
    thread_created_at: Vec<Instant>,
    /// このユーザーの追加だけ失敗させる。
    fail_member_add: Option<UserId>,
}

#[derive(Default)]
struct FakeGateway {
    inner: Mutex<FakeInner>,
    fail_thread_creation: AtomicBool,
    fail_sends: AtomicBool,
    fail_responses: AtomicBool,
    thread_attempts: AtomicU32,
}

impl FakeGateway {
    fn lock(&self) -> std::sync::MutexGuard<'_, FakeInner> {
        self.inner.lock().unwrap()
    }

    fn threads(&self) -> Vec<ChannelId> {
        self.lock().threads.iter().map(|(_, id, _)| *id).collect()
    }

    fn members(&self, thread: ChannelId) -> Vec<UserId> {
        self.lock()
            .members
            .get(&thread)
            .cloned()
            .unwrap_or_default()
    }

    fn message(&self, thread: ChannelId, id: MessageId) -> OutgoingMessage {
        self.lock()
            .messages
            .get(&(thread, id))
            .cloned()
            .expect("message exists")
    }

    fn sends_in(&self, thread: ChannelId) -> Vec<MessageId> {
        self.lock()
            .sends
            .iter()
            .filter(|(channel, _)| *channel == thread)
            .map(|(_, id)| *id)
            .collect()
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
    async fn create_private_thread(
        &self,
        parent: ChannelId,
        name: &str,
    ) -> Result<ChannelId, GatewayError> {
        self.thread_attempts.fetch_add(1, Ordering::SeqCst);
        if self.fail_thread_creation.load(Ordering::SeqCst) {
            return Err(GatewayError("simulated failure".into()));
        }
        let mut inner = self.lock();
        inner.next_id += 1;
        let id = ChannelId::new(1000 + inner.next_id);
        inner.threads.push((parent, id, name.to_owned()));
        inner.thread_created_at.push(Instant::now());
        Ok(id)
    }

    async fn add_thread_member(&self, thread: ChannelId, user: UserId) -> Result<(), GatewayError> {
        let mut inner = self.lock();
        if inner.fail_member_add == Some(user) {
            return Err(GatewayError("Unknown Member".into()));
        }
        inner.members.entry(thread).or_default().push(user);
        Ok(())
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

/// μ は player 1 が最も高い（候補順 1, 2, 3）。
fn formation(n: u128) -> NewFormation {
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
                mu_at_match: 30.0 - i as f64,
                sigma_at_match: 8.33,
            })
            .collect(),
    }
}

fn pairs(count: u64) -> Vec<VcPair> {
    (1..=count)
        .map(|n| VcPair {
            label: format!("{n:02}"),
            red: ChannelId::new(100 + n * 2),
            blue: ChannelId::new(101 + n * 2),
        })
        .collect()
}

fn config(vc_pairs: u64, stagger: Duration) -> DiscordConfig {
    DiscordConfig {
        token: "test".into(),
        guild_id: GUILD,
        parent_channel_id: PARENT,
        vc_pool: pairs(vc_pairs),
        room_checklist: vec!["設定 A".into(), "設定 B".into()],
        thread_create_retries: 3,
        thread_create_stagger: stagger,
        retry_backoff: Duration::from_millis(1),
    }
}

fn adapter(db: &AppDb, gateway: Arc<FakeGateway>, config: DiscordConfig) -> Adapter {
    let lifecycle = LifecycleConfig::default();
    Adapter::new(
        db.repo.clone(),
        gateway,
        config,
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

/// formation を作ってスレッド作成〜最初の打診まで進め、（スレッド, 打診メッセージ）を返す。
async fn started(
    db: &AppDb,
    adapter: &Adapter,
    fake: &FakeGateway,
    n: u128,
) -> (ChannelId, MessageId) {
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
    let thread = record.thread_id.unwrap();
    assert!(fake.threads().contains(&thread));
    (thread, record.host_prompt_message_id.unwrap())
}

/// 打診メッセージの [部屋を作る] / [今回はパスする] の custom_id。
fn prompt_buttons(fake: &FakeGateway, thread: ChannelId, prompt: MessageId) -> (String, String) {
    let message = fake.message(thread, prompt);
    assert_eq!(message.buttons.len(), 2, "{message:?}");
    (
        message.buttons[0].custom_id.clone(),
        message.buttons[1].custom_id.clone(),
    )
}

// ----------------------------------------------------------------------
// 正常系
// ----------------------------------------------------------------------

#[sqlx::test(fixtures("reference", "app_role"))]
async fn matched_to_code_posted_records_every_discord_id(owner: PgPool) {
    let db = AppDb::connect(&owner).await;
    let fake = Arc::new(FakeGateway::default());
    let adapter = adapter(&db, fake.clone(), config(2, Duration::ZERO));

    // 1. matched → スレッド作成、10 人追加、割り当て表、thread_created
    let (thread, prompt) = started(&db, &adapter, &fake, 1).await;
    let mut members = fake.members(thread);
    members.sort();
    assert_eq!(members, (1..=10).map(user).collect::<Vec<_>>());

    let (log, _, record) = load(&db, fid(1)).await;
    assert_eq!(kinds(&log), vec![K::ThreadCreated, K::HostAssigned]);
    let thread_payload = &log.events[0].payload;
    assert_eq!(
        thread_payload[DISCORD_THREAD_ID_KEY].as_str().unwrap(),
        thread.get().to_string()
    );
    assert_eq!(thread_payload[VC_KEY]["label"], "01");
    assert_eq!(thread_payload[VC_KEY]["red_channel_id"], "102");
    assert_eq!(thread_payload[VC_KEY]["blue_channel_id"], "103");
    assert!(thread_payload.get(DISCORD_FAILURES_KEY).is_none());
    let assignment_id = record.assignment_message_id.unwrap();
    assert_eq!(
        thread_payload[ASSIGNMENT_MESSAGE_ID_KEY].as_str().unwrap(),
        assignment_id.get().to_string()
    );
    assert_eq!(record.vc.as_ref().unwrap().label, "01");

    // 2. 割り当て表: 10 人、VC の案内、チェックリスト、ロールの表示名
    let assignment = fake.message(thread, assignment_id);
    assert_eq!(assignment.mention_users.len(), 10);
    assert!(assignment.content.contains("**red**"));
    assert!(assignment.content.contains("**blue**"));
    assert!(assignment.content.contains("VC-01-red"));
    assert!(assignment.content.contains("VC-01-blue"));
    assert!(assignment.content.contains("上キャリー"));
    assert!(assignment.content.contains("- [ ] 設定 A"));
    assert!(assignment.content.contains("- [ ] 設定 B"));
    assert!(assignment.buttons.is_empty());

    // 3. 打診: 先頭候補（μ 最大 = player 1）をメンション、ボタン 2 つ、host_assigned に message id
    assert_eq!(
        log.events[1].payload[HOST_PROMPT_MESSAGE_ID_KEY]
            .as_str()
            .unwrap(),
        prompt.get().to_string()
    );
    let prompt_message = fake.message(thread, prompt);
    assert_eq!(prompt_message.mention_users, vec![user(1)]);
    let (accept, decline) = prompt_buttons(&fake, thread, prompt);
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
    assert!(log.events[2].payload.get(DISCORD_FAILURES_KEY).is_none());
    assert!(matches!(
        fake.last_response(),
        InteractionResponse::Modal(_)
    ));
    let prompt_message = fake.message(thread, prompt);
    assert_eq!(prompt_message.buttons.len(), 1);
    assert_eq!(
        prompt_message.buttons[0].custom_id,
        custom(Action::CodeOpen, fid(1), pid(1))
    );

    // 5. モーダル送信 → 開示メッセージ → 状態表示 → code_posted（ルーム ID + 開示メッセージ ID）
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
    let code_message = record.code_message_id.unwrap();
    assert_eq!(
        code_event.payload[CODE_MESSAGE_ID_KEY].as_str().unwrap(),
        code_message.get().to_string()
    );

    // 6. 開示メッセージは残り 9 人に通知し、ルーム ID と集合場所を含む
    let reveal = fake.message(thread, code_message);
    let mut pinged = reveal.mention_users.clone();
    pinged.sort();
    assert_eq!(pinged, (2..=10).map(user).collect::<Vec<_>>());
    assert!(reveal.content.contains("**1234**"));
    assert!(reveal.content.contains("VC-01-red"));
    match fake.last_response() {
        InteractionResponse::UpdateMessage(status) => {
            assert!(status.content.contains("**1234**"));
            assert_eq!(status.buttons.len(), 1);
        }
        other => panic!("expected UpdateMessage, got {other:?}"),
    }

    // スレッドに投稿されたのは、割り当て表・打診・開示の 3 通だけ
    assert_eq!(
        fake.sends_in(thread),
        vec![assignment_id, prompt, code_message]
    );
    db.done().await;
}

#[sqlx::test(fixtures("reference", "app_role"))]
async fn declining_three_times_abandons_the_formation(owner: PgPool) {
    let db = AppDb::connect(&owner).await;
    let fake = Arc::new(FakeGateway::default());
    let adapter = adapter(&db, fake.clone(), config(1, Duration::ZERO));
    let (thread, prompt) = started(&db, &adapter, &fake, 1).await;

    for (n, candidate) in [(1u64, pid(1)), (2, pid(2)), (3, pid(3))] {
        let (_, decline) = prompt_buttons(&fake, thread, prompt);
        assert_eq!(decline, custom(Action::HostDecline, fid(1), candidate));
        assert_eq!(fake.message(thread, prompt).mention_users, vec![user(n)]);
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
            K::ThreadCreated,
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
    // 打診メッセージは 1 通だけ。候補が変わるたびに編集され、最後は解散の通知になる
    assert_eq!(fake.sends_in(thread).len(), 2, "assignment + one prompt");
    let final_prompt = fake.message(thread, prompt);
    assert!(final_prompt.buttons.is_empty());
    assert!(final_prompt.content.contains("解散"));
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
    assert_eq!(record.vc.unwrap().label, "01");
    db.done().await;
}

#[sqlx::test(fixtures("reference", "app_role"))]
async fn second_room_code_replaces_the_first_in_the_reveal_message(owner: PgPool) {
    let db = AppDb::connect(&owner).await;
    let fake = Arc::new(FakeGateway::default());
    let adapter = adapter(&db, fake.clone(), config(1, Duration::ZERO));
    let (thread, prompt) = started(&db, &adapter, &fake, 1).await;
    let (accept, _) = prompt_buttons(&fake, thread, prompt);
    adapter
        .handle_interaction(button(9201, user(1), &accept))
        .await
        .unwrap();
    let submit = custom(Action::CodeSubmit, fid(1), pid(1));
    adapter
        .handle_interaction(modal(9202, user(1), &submit, "1111"))
        .await
        .unwrap();
    let (_, _, first) = load(&db, fid(1)).await;
    let code_message = first.code_message_id.unwrap();

    // 「ルーム ID を修正」→ 現在のコードが初期値のモーダル
    let open = fake.message(thread, prompt).buttons[0].custom_id.clone();
    assert_eq!(open, custom(Action::CodeOpen, fid(1), pid(1)));
    adapter
        .handle_interaction(button(9203, user(1), &open))
        .await
        .unwrap();
    match fake.last_response() {
        InteractionResponse::Modal(modal) => assert_eq!(modal.prefill.as_deref(), Some("1111")),
        other => panic!("expected Modal, got {other:?}"),
    }

    // 別のインタラクション ID で 2 回目 → 訂正として code_posted が積まれ、同じ開示メッセージが編集される
    adapter
        .handle_interaction(modal(9204, user(1), &submit, "2222"))
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
            K::ThreadCreated,
            K::HostAssigned,
            K::HostAccepted,
            K::CodePosted,
            K::CodePosted
        ]
    );
    assert_eq!(second.code_message_id, Some(code_message));
    let reveal = fake.message(thread, code_message);
    assert!(reveal.content.contains("**2222**"));
    assert!(!reveal.content.contains("1111"));
    assert_eq!(fake.sends_in(thread).len(), 3, "no new reveal message");
    db.done().await;
}

#[sqlx::test(fixtures("reference", "app_role"))]
async fn stale_and_foreign_presses_are_rejected_without_writing(owner: PgPool) {
    let db = AppDb::connect(&owner).await;
    let fake = Arc::new(FakeGateway::default());
    let adapter = adapter(&db, fake.clone(), config(1, Duration::ZERO));
    let (thread, prompt) = started(&db, &adapter, &fake, 1).await;
    let (accept, decline) = prompt_buttons(&fake, thread, prompt);

    // 打診されていない人が押した
    adapter
        .handle_interaction(button(9301, user(5), &accept))
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
        .handle_interaction(button(9302, user(1), &decline))
        .await
        .unwrap();
    adapter
        .handle_interaction(button(9303, user(1), &accept))
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
    let (accept2, _) = prompt_buttons(&fake, thread, prompt);
    adapter
        .handle_interaction(button(9304, user(2), &accept2))
        .await
        .unwrap();
    adapter
        .handle_interaction(button(9304, user(2), &accept2))
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
        .handle_interaction(modal(9305, user(3), &submit, "9999"))
        .await
        .unwrap();
    adapter
        .handle_interaction(modal(9306, user(2), &submit, "   "))
        .await
        .unwrap();
    let (log, _, record) = load(&db, fid(1)).await;
    assert_eq!(log.events.len(), 5);
    assert_eq!(record.code_message_id, None);

    // 別ギルドや知らない custom_id は無視する
    let mut foreign = button(9307, user(2), &accept2);
    foreign.guild = Some(GuildId::new(99));
    adapter.handle_interaction(foreign).await.unwrap();
    adapter
        .handle_interaction(button(9308, user(2), "something:else"))
        .await
        .unwrap();
    assert_eq!(fake.responses().len(), 7);
    db.done().await;
}

#[sqlx::test(fixtures("reference", "app_role"))]
async fn a_restarted_process_continues_from_the_log(owner: PgPool) {
    let db = AppDb::connect(&owner).await;
    let fake = Arc::new(FakeGateway::default());
    let first = adapter(&db, fake.clone(), config(2, Duration::ZERO));
    let (thread, prompt) = started(&db, &first, &fake, 1).await;
    let (accept, _) = prompt_buttons(&fake, thread, prompt);
    drop(first);

    // 「再起動」: 新しいアダプタ。Discord 側の検索は無い（Gateway に検索操作が無い）
    let second = adapter(&db, fake.clone(), config(2, Duration::ZERO));
    second
        .handle_interaction(button(9401, user(1), &accept))
        .await
        .unwrap();
    // ログに記録された打診メッセージを編集した
    assert_eq!(fake.lock().edits.last(), Some(&(thread, prompt)));
    let submit = custom(Action::CodeSubmit, fid(1), pid(1));
    second
        .handle_interaction(modal(9402, user(1), &submit, "4242"))
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
    assert_eq!(record.thread_id, Some(thread));

    // 辞退の直後に落ちた formation（候補未指名）と、まだ何もしていない matched の formation を再開する
    let (thread2, prompt2) = started(&db, &second, &fake, 2).await;
    db.repo
        .append_event(
            fid(2),
            &NewEvent::new(K::HostDeclined, Some(pid(1)), Utc::now(), "manual-decline"),
        )
        .await
        .unwrap();
    db.repo.create_formation(&formation(3)).await.unwrap();

    let third = adapter(&db, fake.clone(), config(3, Duration::ZERO));
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
        record2.host_prompt_message_id,
        Some(prompt2),
        "same prompt, edited"
    );
    assert_eq!(fake.message(thread2, prompt2).mention_users, vec![user(2)]);

    let (_, state3, record3) = load(&db, fid(3)).await;
    assert_eq!(
        state3,
        S::AwaitingHost {
            pending: Some(pid(1)),
            tried: 1
        }
    );
    assert_eq!(
        record3.vc.unwrap().label,
        "03",
        "01 and 02 are still occupied"
    );
    db.done().await;
}

#[sqlx::test(fixtures("reference", "app_role"))]
async fn consecutive_thread_creations_are_staggered(owner: PgPool) {
    let db = AppDb::connect(&owner).await;
    let fake = Arc::new(FakeGateway::default());
    let stagger = Duration::from_millis(200);
    let adapter = adapter(&db, fake.clone(), config(2, stagger));
    started(&db, &adapter, &fake, 1).await;
    started(&db, &adapter, &fake, 2).await;
    let created = fake.lock().thread_created_at.clone();
    assert_eq!(created.len(), 2);
    assert!(
        created[1] - created[0] >= stagger,
        "gap {:?} < {stagger:?}",
        created[1] - created[0]
    );
    db.done().await;
}

// ----------------------------------------------------------------------
// 失敗経路: すべて abandoned になり、詳細が payload に残る
// ----------------------------------------------------------------------

#[sqlx::test(fixtures("reference", "app_role"))]
async fn thread_creation_failure_abandons_after_n_attempts(owner: PgPool) {
    let db = AppDb::connect(&owner).await;
    let fake = Arc::new(FakeGateway::default());
    fake.fail_thread_creation.store(true, Ordering::SeqCst);
    let adapter = adapter(&db, fake.clone(), config(1, Duration::ZERO));
    db.repo.create_formation(&formation(1)).await.unwrap();

    adapter.start_formation(fid(1)).await.unwrap();
    assert_eq!(fake.thread_attempts.load(Ordering::SeqCst), 3);
    let (log, state, _) = load(&db, fid(1)).await;
    assert_eq!(state, S::Abandoned);
    assert_eq!(abandon_reason(&log), AbandonReason::ThreadCreationFailed);
    let failures = log.events[0].payload[DISCORD_FAILURES_KEY]
        .as_array()
        .unwrap()
        .clone();
    assert_eq!(failures.len(), 1);
    assert_eq!(failures[0][OPERATION_KEY], "create_private_thread");
    assert_eq!(failures[0][ATTEMPTS_KEY], 3);
    assert_eq!(failures[0]["error"], "simulated failure");
    assert!(fake.threads().is_empty());

    // 確保していた VC は戻っている
    fake.fail_thread_creation.store(false, Ordering::SeqCst);
    db.repo.create_formation(&formation(2)).await.unwrap();
    adapter.start_formation(fid(2)).await.unwrap();
    let (_, _, record) = load(&db, fid(2)).await;
    assert_eq!(record.vc.unwrap().label, "01");
    db.done().await;
}

#[sqlx::test(fixtures("reference", "app_role"))]
async fn an_exhausted_vc_pool_abandons_the_formation(owner: PgPool) {
    let db = AppDb::connect(&owner).await;
    let fake = Arc::new(FakeGateway::default());
    let adapter = adapter(&db, fake.clone(), config(1, Duration::ZERO));
    started(&db, &adapter, &fake, 1).await;

    db.repo.create_formation(&formation(2)).await.unwrap();
    adapter.start_formation(fid(2)).await.unwrap();
    let (log, state, _) = load(&db, fid(2)).await;
    assert_eq!(state, S::Abandoned, "the ten players go back to the queue");
    assert_eq!(kinds(&log), vec![K::Abandoned]);
    assert_eq!(abandon_reason(&log), AbandonReason::VcPoolExhausted);
    assert_eq!(
        log.events[0].payload[ABANDON_REASON_KEY],
        "vc_pool_exhausted"
    );
    assert_eq!(fake.threads().len(), 1, "no thread was created");
    db.done().await;
}

#[sqlx::test(fixtures("reference", "app_role"))]
async fn a_player_who_cannot_be_added_abandons_the_formation_with_the_player_recorded(
    owner: PgPool,
) {
    let db = AppDb::connect(&owner).await;
    let fake = Arc::new(FakeGateway::default());
    fake.lock().fail_member_add = Some(user(3));
    let adapter = adapter(&db, fake.clone(), config(1, Duration::ZERO));
    db.repo.create_formation(&formation(1)).await.unwrap();

    adapter.start_formation(fid(1)).await.unwrap();
    let (log, state, record) = load(&db, fid(1)).await;
    assert_eq!(state, S::Abandoned);
    assert_eq!(kinds(&log), vec![K::ThreadCreated, K::Abandoned]);
    let thread = record.thread_id.unwrap();

    // thread_created: スレッド ID と VC は残り、追加の失敗も残る。割り当て表は出していない
    let thread_payload = &log.events[0].payload;
    assert_eq!(
        thread_payload[ASSIGNMENT_MESSAGE_ID_KEY],
        serde_json::Value::Null
    );
    let failures = thread_payload[DISCORD_FAILURES_KEY].as_array().unwrap();
    assert_eq!(failures.len(), 1);
    assert_eq!(failures[0][OPERATION_KEY], "add_thread_member");
    assert_eq!(failures[0][PLAYER_ID_KEY], pid(3).to_string());
    assert_eq!(failures[0][DISCORD_USER_ID_KEY], "1003");
    assert_eq!(failures[0][ATTEMPTS_KEY], 3);

    // abandoned: 理由と対象プレイヤー
    assert_eq!(abandon_reason(&log), AbandonReason::MemberAddFailed);
    let players = log.events[1].payload[FAILED_PLAYERS_KEY]
        .as_array()
        .unwrap();
    assert_eq!(players.len(), 1);
    assert_eq!(players[0][PLAYER_ID_KEY], pid(3).to_string());
    assert!(
        players[0]["error"]
            .as_str()
            .unwrap()
            .contains("Unknown Member")
    );

    // 残りの 9 人には解散の通知が出ている
    let sends = fake.sends_in(thread);
    assert_eq!(sends.len(), 1);
    assert!(fake.message(thread, sends[0]).content.contains("解散"));
    assert_eq!(fake.members(thread).len(), 9);

    // VC は解放されている
    fake.lock().fail_member_add = None;
    db.repo.create_formation(&formation(2)).await.unwrap();
    adapter.start_formation(fid(2)).await.unwrap();
    let (_, _, record) = load(&db, fid(2)).await;
    assert_eq!(record.vc.unwrap().label, "01");
    db.done().await;
}

#[sqlx::test(fixtures("reference", "app_role"))]
async fn a_failed_post_abandons_the_formation_with_the_operation_recorded(owner: PgPool) {
    let db = AppDb::connect(&owner).await;
    let fake = Arc::new(FakeGateway::default());
    fake.fail_sends.store(true, Ordering::SeqCst);
    let adapter = adapter(&db, fake.clone(), config(1, Duration::ZERO));
    db.repo.create_formation(&formation(1)).await.unwrap();

    adapter.start_formation(fid(1)).await.unwrap();
    let (log, state, _) = load(&db, fid(1)).await;
    assert_eq!(state, S::Abandoned);
    assert_eq!(kinds(&log), vec![K::ThreadCreated, K::Abandoned]);
    assert_eq!(abandon_reason(&log), AbandonReason::DiscordCallFailed);
    let thread_failures = log.events[0].payload[DISCORD_FAILURES_KEY]
        .as_array()
        .unwrap()
        .clone();
    assert_eq!(thread_failures[0][OPERATION_KEY], "send:assignment");
    // 解散の通知も投稿できなかったので、それも残る
    let abandon_failures = log.events[1].payload[DISCORD_FAILURES_KEY]
        .as_array()
        .unwrap()
        .clone();
    let operations: Vec<&str> = abandon_failures
        .iter()
        .map(|failure| failure[OPERATION_KEY].as_str().unwrap())
        .collect();
    assert_eq!(operations, vec!["send:assignment", "send:abandoned_notice"]);
    db.done().await;
}

#[sqlx::test(fixtures("reference", "app_role"))]
async fn failed_interaction_responses_are_recorded_in_the_event(owner: PgPool) {
    let db = AppDb::connect(&owner).await;
    let fake = Arc::new(FakeGateway::default());
    let adapter = adapter(&db, fake.clone(), config(1, Duration::ZERO));
    let (thread, prompt) = started(&db, &adapter, &fake, 1).await;
    let (accept, _) = prompt_buttons(&fake, thread, prompt);

    fake.fail_responses.store(true, Ordering::SeqCst);
    adapter
        .handle_interaction(button(9501, user(1), &accept))
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
    assert_eq!(fake.message(thread, prompt).buttons.len(), 1);

    // ホストは「ルーム ID を入力」から続けられる
    fake.fail_responses.store(false, Ordering::SeqCst);
    let submit = custom(Action::CodeSubmit, fid(1), pid(1));
    adapter
        .handle_interaction(modal(9502, user(1), &submit, "7777"))
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

#[sqlx::test(fixtures("reference", "app_role"))]
async fn internal_formations_endpoint_requires_the_token_and_starts_a_formation(owner: PgPool) {
    use axum::body::Body;
    use axum::http::{Request, StatusCode, header};
    use http_body_util::BodyExt;
    use serde_json::json;
    use tower::ServiceExt;
    use unite_matchsystem::api::{ApiState, router};

    let db = AppDb::connect(&owner).await;
    let fake = Arc::new(FakeGateway::default());
    let adapter = Arc::new(adapter(&db, fake.clone(), config(1, Duration::ZERO)));
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

    // 正常系 → 202、formation が作られ、バックグラウンドでスレッド作成が進む
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
        // thread_created と host_assigned の間で観測することがあるので、打診まで待つ
        if !matches!(state, S::Matched | S::AwaitingHost { pending: None, .. }) {
            assert_eq!(
                state,
                S::AwaitingHost {
                    pending: Some(pid(1)),
                    tried: 1
                }
            );
            assert!(fake.threads().contains(&record.thread_id.unwrap()));
            break;
        }
        assert!(Instant::now() < deadline, "start_formation did not run");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    db.done().await;
}
