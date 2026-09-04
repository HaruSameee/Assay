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
//! - スレッド作成が N 回失敗したら `abandoned`（`thread_creation_failed`）
//! - VC は終端で解放される。プールが空なら `matched` のまま止まる

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use chrono::Utc;
use serenity::async_trait;
use serenity::model::id::{ChannelId, GuildId, InteractionId, MessageId, UserId};
use sqlx::PgPool;
use sqlx::postgres::PgPoolOptions;
use unite_matchsystem::discord::adapter::{
    Adapter, AdapterError, IncomingInteraction, IncomingKind,
};
use unite_matchsystem::discord::custom_id::{Action, CustomId, ROOM_CODE_INPUT_ID};
use unite_matchsystem::discord::gateway::{
    Gateway, GatewayError, InteractionRef, InteractionResponse, OutgoingMessage,
};
use unite_matchsystem::discord::payload::{
    ASSIGNMENT_MESSAGE_ID_KEY, CODE_MESSAGE_ID_KEY, DiscordRecord, HOST_PROMPT_MESSAGE_ID_KEY,
    VC_KEY,
};
use unite_matchsystem::discord::{DiscordConfig, VcPair};
use unite_matchsystem::lifecycle::{
    AbandonReason, DefaultHostSelector, EventPayload, FormationEventKind as K, FormationId,
    FormationState as S, LifecycleConfig, PlayerId,
};
use unite_matchsystem::persistence::{
    DISCORD_THREAD_ID_KEY, FormationLog, FormationRepository, NewEvent, NewFormation,
    NewFormationPlayer, ROOM_CODE_KEY, Team,
};
use uuid::Uuid;

const APP_ROLE: &str = "unite_app_test";
const APP_ROLE_PASSWORD: &str = "unite_app_test";
const PARENT: ChannelId = ChannelId::new(10);
const GUILD: GuildId = GuildId::new(1);

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
}

#[derive(Default)]
struct FakeGateway {
    inner: Mutex<FakeInner>,
    fail_thread_creation: AtomicBool,
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
        self.lock().members.entry(thread).or_default().push(user);
        Ok(())
    }

    async fn send_message(
        &self,
        channel: ChannelId,
        message: &OutgoingMessage,
    ) -> Result<MessageId, GatewayError> {
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
    let roles = ["top", "jungle", "bottom"];
    NewFormation {
        id: fid(n),
        created_at: Utc::now(),
        matchmaking_params_version: 1,
        predicted_win_prob: 0.5,
        players: (1..=10)
            .map(|i| NewFormationPlayer {
                player_id: pid(i),
                team: if i <= 5 { Team::Zero } else { Team::One },
                role: roles[(i as usize) % 3].to_owned(),
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
// テスト
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
    let assignment_id = record.assignment_message_id.unwrap();
    assert_eq!(
        thread_payload[ASSIGNMENT_MESSAGE_ID_KEY].as_str().unwrap(),
        assignment_id.get().to_string()
    );
    assert_eq!(record.vc.as_ref().unwrap().label, "01");

    // 2. 割り当て表: 10 人、VC の案内、チェックリスト
    let assignment = fake.message(thread, assignment_id);
    assert_eq!(assignment.mention_users.len(), 10);
    assert!(assignment.content.contains("VC-01-red"));
    assert!(assignment.content.contains("VC-01-blue"));
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

    // 4. 「部屋を作る」→ host_accepted（冪等キー = インタラクション ID）→ モーダル → 状態表示の更新
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
    let prompt_message = fake.message(thread, prompt);
    assert_eq!(prompt_message.buttons.len(), 1);
    assert_eq!(
        prompt_message.buttons[0].custom_id,
        custom(Action::CodeOpen, fid(1), pid(1))
    );

    // 5. モーダル送信 → 開示メッセージ → code_posted（ルーム ID + 開示メッセージ ID）→ 状態表示
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
    assert_eq!(
        log.events.last().unwrap().event.payload,
        EventPayload::Abandon(AbandonReason::HostCandidatesExhausted)
    );
    // 打診メッセージは 1 通だけ。候補が変わるたびに編集された
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

    // 同じインタラクション ID の再送は 1 行だけ
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
async fn thread_creation_failure_abandons_after_n_attempts(owner: PgPool) {
    let db = AppDb::connect(&owner).await;
    let fake = Arc::new(FakeGateway::default());
    fake.fail_thread_creation.store(true, Ordering::SeqCst);
    let adapter = adapter(&db, fake.clone(), config(1, Duration::ZERO));
    db.repo.create_formation(&formation(1)).await.unwrap();

    let error = adapter.start_formation(fid(1)).await.unwrap_err();
    assert!(matches!(error, AdapterError::Gateway(_)), "{error}");
    assert_eq!(fake.thread_attempts.load(Ordering::SeqCst), 3);
    let (log, state, _) = load(&db, fid(1)).await;
    assert_eq!(state, S::Abandoned);
    assert_eq!(
        log.events[0].event.payload,
        EventPayload::Abandon(AbandonReason::ThreadCreationFailed)
    );
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
async fn an_exhausted_vc_pool_leaves_the_formation_matched(owner: PgPool) {
    let db = AppDb::connect(&owner).await;
    let fake = Arc::new(FakeGateway::default());
    let adapter = adapter(&db, fake.clone(), config(1, Duration::ZERO));
    started(&db, &adapter, &fake, 1).await;

    db.repo.create_formation(&formation(2)).await.unwrap();
    let error = adapter.start_formation(fid(2)).await.unwrap_err();
    assert!(matches!(error, AdapterError::VcPoolExhausted(id) if id == fid(2)));
    let (log, state, _) = load(&db, fid(2)).await;
    assert_eq!(state, S::Matched);
    assert!(log.events.is_empty());
    assert_eq!(fake.threads().len(), 1);
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
// HTTP API（手動テストの入口）
// ----------------------------------------------------------------------

#[sqlx::test(fixtures("reference", "app_role"))]
async fn internal_formations_endpoint_creates_and_starts_a_formation(owner: PgPool) {
    use axum::body::Body;
    use axum::http::{Request, StatusCode, header};
    use http_body_util::BodyExt;
    use serde_json::json;
    use tower::ServiceExt;
    use unite_matchsystem::api::{ApiState, router};

    let db = AppDb::connect(&owner).await;
    let fake = Arc::new(FakeGateway::default());
    let adapter = Arc::new(adapter(&db, fake.clone(), config(1, Duration::ZERO)));
    let app = router(ApiState {
        adapter: adapter.clone(),
    });
    let post = |body: serde_json::Value| {
        Request::post("/internal/formations")
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(body.to_string()))
            .unwrap()
    };
    let players = |role: &str, first_user: i64| -> Vec<serde_json::Value> {
        (0..10)
            .map(|i| {
                json!({
                    "discord_user_id": first_user + i,
                    "team": if i < 5 { "red" } else { "blue" },
                    "role": role,
                    "mu_at_match": 30.0 - i as f64,
                })
            })
            .collect()
    };

    // 未登録ユーザー → 400
    let response = app
        .clone()
        .oneshot(post(json!({ "players": players("top", 5001) })))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);

    // 存在しないロール → 400（FK 違反を入力エラーとして返す）
    let response = app
        .clone()
        .oneshot(post(json!({ "players": players("no_such_role", 1001) })))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);

    // 正常系 → 202、formation が作られ、バックグラウンドでスレッド作成が進む
    let response = app
        .clone()
        .oneshot(post(json!({ "players": players("top", 1001) })))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::ACCEPTED);
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    let body: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    let id = FormationId(Uuid::parse_str(body["formation_id"].as_str().unwrap()).unwrap());

    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let (_, state, record) = load(&db, id).await;
        if state != S::Matched {
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
