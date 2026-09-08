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
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use chrono::{TimeDelta, Utc};
use serenity::async_trait;
use serenity::model::id::{ChannelId, GuildId, InteractionId, MessageId, UserId};
use sqlx::PgPool;
use sqlx::postgres::PgPoolOptions;
use unite_matchsystem::discord::adapter::{Adapter, IncomingInteraction, IncomingKind};
use unite_matchsystem::discord::custom_id::{Action, CustomId};
use unite_matchsystem::discord::gateway::{
    Gateway, GatewayError, InteractionRef, InteractionResponse, OutgoingMessage,
};
use unite_matchsystem::discord::payload::{
    ASSIGNMENT_MESSAGE_IDS_KEY, ATTEMPTS_KEY, CODE_MESSAGE_IDS_KEY, DISCORD_FAILURES_KEY,
    DiscordRecord, HOST_PROMPT_CHANNEL_ID_KEY, HOST_PROMPT_MESSAGE_ID_KEY, OPERATION_KEY,
    REPORT_MESSAGE_IDS_KEY, TIMEOUT_REASON_KEY,
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
    NewFormationPlayer, NewReport, Outcome, ROOM_CODE_KEY, Team, VC_KEY, VcAssignment,
    VcAssignmentOutcome,
};
use uuid::Uuid;

const APP_ROLE: &str = "assay_app";
const APP_ROLE_PASSWORD: &str = "assay_app_test";
const GUILD: GuildId = GuildId::new(1);
/// テスト用のサイト起点。通知に載るマッチ画面 URL の前半。
const SITE: &str = "https://match.example.test";
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
    fail_send_channel: AtomicU64,
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
        if self.fail_sends.load(Ordering::SeqCst)
            || self.fail_send_channel.load(Ordering::SeqCst) == channel.get()
        {
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
        dispute_channel_id: ChannelId::new(999),
        summon_channel_id: ChannelId::new(998),
        matchmaker_log_channel_id: ChannelId::new(997),
        vc_pool_size: 1,
        room_map: "kyogre".into(),
        room_checklist: vec!["設定 A".into(), "設定 B".into()],
        retries: 3,
        retry_backoff: Duration::from_millis(1),
    }
}

fn adapter(db: &AppDb, gateway: Arc<FakeGateway>, vc_pairs: u64) -> Adapter {
    let lifecycle = LifecycleConfig::default();
    adapter_with_lifecycle(db, gateway, vc_pairs, lifecycle)
}

fn adapter_with_lifecycle(
    db: &AppDb,
    gateway: Arc<FakeGateway>,
    vc_pairs: u64,
    lifecycle: LifecycleConfig,
) -> Adapter {
    Adapter::new(
        db.repo.clone(),
        gateway,
        config(),
        pairs(vc_pairs),
        lifecycle.clone(),
        Box::new(DefaultHostSelector::from_config(&lifecycle)),
        SITE,
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

/// **サイトからの操作**。マッチ画面のエンドポイントが積むのと同じイベントを積む。
///
/// HTTP 層は Discord を呼ばない。投稿は後続の `resume_unfinished` が DB の状態から出す。
async fn from_the_site(db: &AppDb, n: u128, player: u128, kind: K, payload: serde_json::Value) {
    db.repo
        .append_event(
            fid(n),
            &NewEvent::new(
                kind,
                Some(pid(player)),
                Utc::now(),
                format!("web:{kind}:{n}:{player}:{payload}"),
            )
            .with_payload(payload),
        )
        .await
        .unwrap();
}

/// ホストが承諾する（サイト）→ 通知が出るところまで進める。
async fn accept_host(db: &AppDb, adapter: &Adapter, n: u128, player: u128) {
    from_the_site(
        db,
        n,
        player,
        K::HostAccepted,
        serde_json::json!({ "source": "web" }),
    )
    .await;
    adapter.resume_unfinished().await.unwrap();
}

/// ホストがルーム ID を出す（サイト）→ 両 VC への開示まで進める。
async fn post_room_code(db: &AppDb, adapter: &Adapter, n: u128, player: u128, code: &str) {
    from_the_site(
        db,
        n,
        player,
        K::CodePosted,
        serde_json::json!({ "room_code": code, "source": "web" }),
    )
    .await;
    adapter.resume_unfinished().await.unwrap();
}

/// formation を報告受付中まで進める（承諾もルーム ID もサイトから）。
async fn reporting_started(
    db: &AppDb,
    adapter: &Adapter,
    n: u128,
    room_code: &str,
) -> (VcPair, DiscordRecord) {
    let (vc, _, _) = started(db, adapter, n).await;
    accept_host(db, adapter, n, 1).await;
    post_room_code(db, adapter, n, 1, room_code).await;
    let (_, state, record) = load(db, fid(n)).await;
    assert!(matches!(state, S::InProgress { .. }), "{state:?}");
    (vc, record)
}

/// 3 人の候補が全員パスして解散させる（すべてサイトからの操作）。
async fn decline_all(db: &AppDb, adapter: &Adapter, n: u128) {
    for _ in 0..3u32 {
        let (_, state, _) = load_record(adapter, n).await;
        let S::AwaitingHost {
            pending: Some(candidate),
            ..
        } = state
        else {
            panic!("expected a pending candidate, got {state:?}");
        };
        from_the_site(
            db,
            n,
            candidate.0.as_u128(),
            K::HostDeclined,
            serde_json::json!({ "source": "web" }),
        )
        .await;
        adapter.resume_unfinished().await.unwrap();
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
    assert_eq!(
        kinds(&log),
        vec![K::VcAssigned, K::VcAssigned, K::HostAssigned]
    );
    // 最初の vc_assigned はプールロックと同じトランザクションで記録する予約。Discord の副作用を
    // 実行する前なのでメッセージ ID はまだ無い。
    let reserved_vc_payload = &log.events[0].payload;
    assert_eq!(reserved_vc_payload[VC_KEY]["label"], "1");
    assert!(
        reserved_vc_payload
            .get(ASSIGNMENT_MESSAGE_IDS_KEY)
            .is_none()
    );
    let vc_payload = &log.events[1].payload;
    assert_eq!(vc_payload[VC_KEY]["label"], "1");
    assert_eq!(vc_payload[VC_KEY]["red_channel_id"], "101");
    assert_eq!(vc_payload[VC_KEY]["blue_channel_id"], "102");
    assert!(vc_payload.get(DISCORD_FAILURES_KEY).is_none());
    let red_assignment = record.assignment_message_ids[&Team::Red];
    let blue_assignment = record.assignment_message_ids[&Team::Blue];
    assert_eq!(fake.sends_in(vc.red)[0], red_assignment);
    assert_eq!(fake.sends_in(vc.blue)[0], blue_assignment);

    // 招集は**チームごとに 1 通**。相手チームの顔ぶれと行き先が同じ投稿に並ばない
    let (summon_channel, _) = record.summon_notification.unwrap();
    assert_eq!(summon_channel, ChannelId::new(998));
    let summons: Vec<OutgoingMessage> = fake
        .sends_in(summon_channel)
        .into_iter()
        .map(|id| fake.message(summon_channel, id))
        .collect();
    assert_eq!(summons.len(), 3, "red / blue の招集とホストの通知");
    for (summon, channel) in summons[..2].iter().zip(["<#101>", "<#102>"]) {
        assert_eq!(summon.mention_users.len(), 5, "{}", summon.content);
        assert!(summon.content.contains("マッチが成立しました"));
        assert!(summon.content.contains(channel), "{}", summon.content);
        assert_eq!(
            summon.content.lines().next().unwrap().matches("<@").count(),
            5
        );
        // 相手側の VC は出てこない
        let other = if channel == "<#101>" {
            "<#102>"
        } else {
            "<#101>"
        };
        assert!(!summon.content.contains(other), "{}", summon.content);
        for forbidden in ["レート", "top_carry", "上キャリー", "報告"] {
            assert!(!summon.content.contains(forbidden), "{}", summon.content);
        }
        // 操作の行き先が必ず書いてある
        assert!(summon.content.contains(SITE), "{}", summon.content);
    }
    // 3 通目はホストの通知。ホストは相手チームにも伝えるので招集チャンネルに出す
    let announcement = &summons[2];
    assert_eq!(announcement.mention_users, vec![user(1)]);
    assert!(
        announcement.content.contains("ホストは"),
        "{}",
        announcement.content
    );
    assert!(
        announcement.content.contains(SITE),
        "{}",
        announcement.content
    );

    // 2. 割り当て表: **そのVCのチームだけ**。red の投稿に blue の割り当てが載らない
    let assignment_red = fake.message(vc.red, red_assignment);
    let assignment_blue = fake.message(vc.blue, blue_assignment);
    assert_ne!(
        assignment_red, assignment_blue,
        "チームごとに本文が違うこと"
    );
    assert!(assignment_red.content.starts_with("**[vc1] "));
    assert!(assignment_red.content.contains("match-"));
    assert!(assignment_red.mention_users.is_empty());
    assert!(!assignment_red.content.contains("<@"));
    assert!(assignment_red.content.contains("vc1-red"));
    assert!(
        !assignment_red.content.contains("vc1-blue"),
        "{}",
        assignment_red.content
    );
    assert!(assignment_red.content.contains("**red**"));
    assert!(
        !assignment_red.content.contains("**blue**"),
        "{}",
        assignment_red.content
    );
    assert!(assignment_red.content.contains("- [ ] マップ: kyogre"));
    assert!(assignment_red.content.contains("- [ ] 設定 A"));
    assert!(assignment_red.buttons.is_empty());
    assert!(assignment_blue.content.contains("**blue**"));
    assert!(
        !assignment_blue.content.contains("**red**"),
        "{}",
        assignment_blue.content
    );

    // 3. 打診: 候補 1（red）なので red の VC にだけ。ボタンは無い
    assert_eq!(prompt_channel, vc.red);
    assert_eq!(fake.sends_in(vc.red).len(), 2);
    assert_eq!(
        fake.sends_in(vc.blue).len(),
        1,
        "no prompt on the blue side"
    );
    let host_assigned = &log.events[2].payload;
    assert_eq!(host_assigned[HOST_PROMPT_CHANNEL_ID_KEY], "101");
    assert_eq!(
        host_assigned[HOST_PROMPT_MESSAGE_ID_KEY],
        prompt.get().to_string()
    );
    let prompt_message = fake.message(vc.red, prompt);
    assert!(prompt_message.content.starts_with("**[vc1] "));
    assert_eq!(prompt_message.mention_users, vec![user(1)]);
    assert!(prompt_message.buttons.is_empty(), "{prompt_message:?}");
    assert!(
        prompt_message.content.contains(SITE),
        "{}",
        prompt_message.content
    );

    // 4. サイトで「部屋を作る」→ host_accepted。VC の打診メッセージは「作成中」に編集される
    accept_host(&db, &adapter, 1, 1).await;
    let (log, state, _) = load(&db, fid(1)).await;
    assert_eq!(
        state,
        S::AwaitingCode {
            host: pid(1),
            tried: 1
        }
    );
    let accepted_message = fake.message(vc.red, prompt);
    assert!(
        accepted_message.content.contains("部屋を作成中"),
        "{}",
        accepted_message.content
    );
    assert!(accepted_message.buttons.is_empty());
    // 承諾の追記のあとに「通知済み」の追記が続く（状態は変わらない）
    assert_eq!(
        kinds(&log),
        vec![
            K::VcAssigned,
            K::VcAssigned,
            K::HostAssigned,
            K::HostAccepted,
            K::HostAccepted
        ]
    );

    // 5. サイトでルーム ID → 両 VC に開示 + 報告の催促、code_posted に全メッセージ ID
    post_room_code(&db, &adapter, 1, 1, "1234").await;
    let (log, state, record) = load(&db, fid(1)).await;
    assert_eq!(
        state,
        S::InProgress {
            host: pid(1),
            room_code: "1234".into()
        }
    );
    let code_payload = &log.events.last().unwrap().payload;
    assert_eq!(code_payload[ROOM_CODE_KEY], "1234");
    for team in Team::ALL {
        let channel = vc.channel(team);
        let reveal = fake.message(channel, record.code_message_ids[&team]);
        assert!(reveal.content.contains("**1234**"), "{}", reveal.content);
        // **参加者10人のメンションは付けない。通知はホスト自身のチーム側だけ**
        // （2通とも載せると、ホスト1人が同じ知らせで2回叩かれる）
        if team == Team::Red {
            assert_eq!(reveal.mention_users, vec![user(1)], "{reveal:?}");
        } else {
            assert!(reveal.mention_users.is_empty(), "{reveal:?}");
        }
        assert_eq!(
            reveal.content.matches("<@").count(),
            1,
            "{}",
            reveal.content
        );
        assert!(reveal.content.contains("<@1001>"), "{}", reveal.content);
        assert!(reveal.buttons.is_empty());
        // 集合先はそのチーム側だけ
        assert!(
            reveal.content.contains(&format!("vc1-{team}")),
            "{}",
            reveal.content
        );

        let report = fake.message(channel, record.report_message_ids[&team]);
        assert!(report.buttons.is_empty(), "{report:?}");
        assert!(report.mention_users.is_empty(), "{report:?}");
        assert!(report.content.contains(SITE), "{}", report.content);
        assert!(
            report.content.contains("報告してください"),
            "{}",
            report.content
        );
        assert_eq!(
            code_payload[CODE_MESSAGE_IDS_KEY][team.as_str()],
            record.code_message_ids[&team].get().to_string()
        );
        assert_eq!(
            code_payload[REPORT_MESSAGE_IDS_KEY][team.as_str()],
            record.report_message_ids[&team].get().to_string()
        );
    }
    // 開示済みのコードがログから読める（打ち直しの判定に使う）
    assert_eq!(record.revealed_code.as_deref(), Some("1234"));

    // 6. 同じ状態でもう一度走らせても、投稿は増えない
    let before = fake.total_sends();
    adapter.resume_unfinished().await.unwrap();
    assert_eq!(fake.total_sends(), before, "publishing is idempotent");
    db.done().await;
}

/// **ホストの承諾もルーム ID も、サイトから積まれたイベントで進む。**
/// Discord 側はボタンを 1 つも持たない。
#[sqlx::test(
    fixtures("reference", "app_role"),
    fixtures("../ops/app_privileges.sql")
)]
async fn no_voice_chat_message_carries_a_button(owner: PgPool) {
    let db = AppDb::connect(&owner).await;
    let fake = Arc::new(FakeGateway::default());
    let adapter = adapter(&db, fake.clone(), 2);
    let (vc, _, _) = started(&db, &adapter, 1).await;
    accept_host(&db, &adapter, 1, 1).await;
    post_room_code(&db, &adapter, 1, 1, "1234").await;

    for channel in [vc.red, vc.blue, ChannelId::new(998)] {
        for id in fake.sends_in(channel) {
            let message = fake.message(channel, id);
            assert!(
                message.buttons.is_empty(),
                "{channel} に押せるものが残っている: {message:?}"
            );
        }
    }
    db.done().await;
}

/// 履歴に残った古いボタンは**状態を変えない**。押した本人にだけ行き先を返す。
#[sqlx::test(
    fixtures("reference", "app_role"),
    fixtures("../ops/app_privileges.sql")
)]
async fn an_old_button_press_only_gets_a_link_back(owner: PgPool) {
    let db = AppDb::connect(&owner).await;
    let fake = Arc::new(FakeGateway::default());
    let adapter = adapter(&db, fake.clone(), 2);
    started(&db, &adapter, 1).await;
    let (before, _, _) = load(&db, fid(1)).await;

    // 旧版が出していた custom_id をそのまま押す
    adapter
        .handle_interaction(button(
            9001,
            user(1),
            &CustomId::new(Action::HostAccept, fid(1), pid(1)).encode(),
        ))
        .await
        .unwrap();
    let response = fake.last_response();
    match response {
        InteractionResponse::Ephemeral(text) => {
            assert!(text.contains(SITE), "{text}");
            assert!(text.contains("サイトへ移りました"), "{text}");
        }
        other => panic!("expected an ephemeral pointer to the site: {other:?}"),
    }
    // 状態もイベント列も動かない
    let (after, state, _) = load(&db, fid(1)).await;
    assert_eq!(kinds(&before), kinds(&after));
    assert_eq!(
        state,
        S::AwaitingHost {
            pending: Some(pid(1)),
            tried: 1
        }
    );
    db.done().await;
}

/// 3 人の候補が全員パスして解散。打診メッセージは**同じものを編集し続ける**。
#[sqlx::test(
    fixtures("reference", "app_role"),
    fixtures("../ops/app_privileges.sql")
)]
async fn declining_three_times_abandons_and_edits_the_same_prompt(owner: PgPool) {
    let db = AppDb::connect(&owner).await;
    let fake = Arc::new(FakeGateway::default());
    let adapter = adapter(&db, fake.clone(), 2);
    let (vc, channel, prompt) = started(&db, &adapter, 1).await;

    for candidate in 1..=3u128 {
        let (_, state, record) = load(&db, fid(1)).await;
        assert_eq!(
            state,
            S::AwaitingHost {
                pending: Some(pid(candidate)),
                tried: candidate as u32
            }
        );
        assert_eq!(
            record.host_prompt,
            Some((channel, prompt)),
            "同じメッセージ"
        );
        from_the_site(
            &db,
            1,
            candidate,
            K::HostDeclined,
            serde_json::json!({ "source": "web" }),
        )
        .await;
        adapter.resume_unfinished().await.unwrap();
    }

    let (log, state, _) = load(&db, fid(1)).await;
    assert_eq!(state, S::Abandoned);
    assert_eq!(abandon_reason(&log), AbandonReason::HostCandidatesExhausted);
    // 打診の投稿は 1 通だけ（2人目・3人目は同じメッセージを編集）。
    // 解散の通知も、打診メッセージがある red 側はそれを編集して使い回す
    assert_eq!(fake.sends_in(vc.red).len(), 2, "割り当てと打診だけ");
    assert_eq!(fake.sends_in(vc.blue).len(), 2, "割り当てと解散");
    let final_prompt = fake.message(vc.red, prompt);
    assert!(
        final_prompt.content.contains("解散"),
        "{}",
        final_prompt.content
    );
    db.done().await;
}

/// 候補が反対チームに移ったら、打診は**その候補の側の VC**に出す。
#[sqlx::test(
    fixtures("reference", "app_role"),
    fixtures("../ops/app_privileges.sql")
)]
async fn a_candidate_on_the_other_team_gets_the_prompt_in_their_own_voice_chat(owner: PgPool) {
    let db = AppDb::connect(&owner).await;
    let fake = Arc::new(FakeGateway::default());
    // player 1 が red、player 6 が blue。μ は 1 が最上位、次が 6（＝候補順 1, 6, 2）
    db.repo
        .create_formation(&formation_with_mu(1, |i| match i {
            1 => 30.0,
            6 => 29.0,
            other => 28.0 - other as f64,
        }))
        .await
        .unwrap();
    let adapter = adapter(&db, fake.clone(), 2);
    adapter.start_formation(fid(1)).await.unwrap();
    let (_, _, record) = load(&db, fid(1)).await;
    let vc = record.vc.clone().unwrap();
    let (first_channel, first_prompt) = record.host_prompt.unwrap();
    assert_eq!(first_channel, vc.red);

    // 1 人目がパス → 2 人目は blue 側
    from_the_site(
        &db,
        1,
        1,
        K::HostDeclined,
        serde_json::json!({ "source": "web" }),
    )
    .await;
    adapter.resume_unfinished().await.unwrap();
    let (_, state, record) = load(&db, fid(1)).await;
    assert_eq!(
        state,
        S::AwaitingHost {
            pending: Some(pid(6)),
            tried: 2
        }
    );
    let (second_channel, second_prompt) = record.host_prompt.unwrap();
    assert_eq!(second_channel, vc.blue, "候補の側の VC に出る");
    // 古い方は「移りました」に書き換わる
    let moved = fake.message(vc.red, first_prompt);
    assert!(moved.content.contains("移りました"), "{}", moved.content);
    assert!(moved.buttons.is_empty());

    // blue 側のホストが承諾してルーム ID を出す
    accept_host(&db, &adapter, 1, 6).await;
    let accepted = fake.message(vc.blue, second_prompt);
    assert!(
        accepted.content.contains("部屋を作成中"),
        "{}",
        accepted.content
    );
    post_room_code(&db, &adapter, 1, 6, "6666").await;
    let (_, state, record) = load(&db, fid(1)).await;
    assert_eq!(
        state,
        S::InProgress {
            host: pid(6),
            room_code: "6666".into()
        }
    );
    // 開示は両チームに出るが、通知が飛ぶのはホスト（blue の人）の側の1通だけ
    for team in Team::ALL {
        let reveal = fake.message(vc.channel(team), record.code_message_ids[&team]);
        assert!(reveal.content.contains("**6666**"), "{}", reveal.content);
        // 表示はどちらにも出る（誰に聞けばよいかは相手チームにも要る）
        assert!(reveal.content.contains("<@1006>"), "{}", reveal.content);
        if team == Team::Blue {
            assert_eq!(reveal.mention_users, vec![user(6)]);
        } else {
            assert!(reveal.mention_users.is_empty(), "{reveal:?}");
        }
    }
    db.done().await;
}

/// **ルーム ID の打ち直しは新しいイベントとして通り、両 VC の開示が更新される。**
///
/// 00 §5: 状態ベースの冪等にすると、部屋コードの打ち直しを黙って捨てる。
#[sqlx::test(
    fixtures("reference", "app_role"),
    fixtures("../ops/app_privileges.sql")
)]
async fn second_room_code_replaces_the_first_in_both_reveal_messages(owner: PgPool) {
    let db = AppDb::connect(&owner).await;
    let fake = Arc::new(FakeGateway::default());
    let adapter = adapter(&db, fake.clone(), 2);
    let (vc, record) = reporting_started(&db, &adapter, 1, "1111").await;
    let first_ids = record.code_message_ids.clone();
    let sends_before = fake.total_sends();

    post_room_code(&db, &adapter, 1, 1, "2222").await;
    let (log, state, record) = load(&db, fid(1)).await;
    assert_eq!(
        state,
        S::InProgress {
            host: pid(1),
            room_code: "2222".into()
        },
        "打ち直しが状態に反映される"
    );
    // 同じメッセージを編集する（新しい投稿を積まない）
    assert_eq!(record.code_message_ids, first_ids);
    assert_eq!(fake.total_sends(), sends_before, "編集であって投稿ではない");
    for team in Team::ALL {
        let reveal = fake.message(vc.channel(team), first_ids[&team]);
        assert!(reveal.content.contains("**2222**"), "{}", reveal.content);
        assert!(!reveal.content.contains("1111"), "{}", reveal.content);
    }
    assert_eq!(record.revealed_code.as_deref(), Some("2222"));
    // code_posted が積み上がっている（打ち消しでも上書きでもない）
    assert_eq!(
        kinds(&log)
            .iter()
            .filter(|kind| **kind == K::CodePosted)
            .count(),
        4,
        "サイトからの 2 件と、開示を記録した 2 件"
    );
    db.done().await;
}

/// **プロセスが落ちても続きから進む。**投稿済みかどうかはログから導出する。
#[sqlx::test(
    fixtures("reference", "app_role"),
    fixtures("../ops/app_privileges.sql")
)]
async fn a_restarted_process_continues_from_the_log(owner: PgPool) {
    let db = AppDb::connect(&owner).await;
    let fake = Arc::new(FakeGateway::default());
    let adapter = adapter(&db, fake.clone(), 2);
    let (vc, _, prompt) = started(&db, &adapter, 1).await;

    // サイトで承諾したところでプロセスが落ちた（通知はまだ出ていない）
    from_the_site(
        &db,
        1,
        1,
        K::HostAccepted,
        serde_json::json!({ "source": "web" }),
    )
    .await;
    drop(adapter);

    // 新しいプロセス。ログを読み直して、出せていない通知を出す
    let restarted = adapter_with_lifecycle(&db, fake.clone(), 2, LifecycleConfig::default());
    restarted.resume_unfinished().await.unwrap();
    let accepted = fake.message(vc.red, prompt);
    assert!(
        accepted.content.contains("部屋を作成中"),
        "{}",
        accepted.content
    );

    // 続けてサイトでルーム ID。ここでも落ちる
    from_the_site(
        &db,
        1,
        1,
        K::CodePosted,
        serde_json::json!({ "room_code": "1234", "source": "web" }),
    )
    .await;
    drop(restarted);

    let restarted_again = adapter_with_lifecycle(&db, fake.clone(), 2, LifecycleConfig::default());
    restarted_again.resume_unfinished().await.unwrap();
    let (_, state, record) = load(&db, fid(1)).await;
    assert_eq!(
        state,
        S::InProgress {
            host: pid(1),
            room_code: "1234".into()
        }
    );
    for team in Team::ALL {
        let reveal = fake.message(vc.channel(team), record.code_message_ids[&team]);
        assert!(reveal.content.contains("**1234**"), "{}", reveal.content);
    }

    // 何度再開しても二重に投稿しない
    let sends = fake.total_sends();
    restarted_again.resume_unfinished().await.unwrap();
    restarted_again.resume_unfinished().await.unwrap();
    assert_eq!(fake.total_sends(), sends);
    db.done().await;
}
// Phase 5: 時間駆動の再開
// ----------------------------------------------------------------------

#[sqlx::test(
    fixtures("reference", "app_role"),
    fixtures("../ops/app_privileges.sql")
)]
async fn booting_with_an_expired_third_host_candidate_abandons_the_formation(owner: PgPool) {
    let db = AppDb::connect(&owner).await;
    let fake = Arc::new(FakeGateway::default());
    let lifecycle = LifecycleConfig {
        host_accept_timeout: Duration::from_secs(1),
        ..LifecycleConfig::default()
    };
    let adapter = adapter_with_lifecycle(&db, fake, 1, lifecycle);
    let old = Utc::now() - TimeDelta::seconds(10);
    db.repo.create_formation(&formation(1)).await.unwrap();
    for event in [
        NewEvent::vc_assigned("1", 101, 102, old, "vc"),
        NewEvent::new(K::HostAssigned, Some(pid(1)), old, "host-1"),
        NewEvent::new(K::HostTimeout, Some(pid(1)), old, "timeout-1"),
        NewEvent::new(K::HostAssigned, Some(pid(2)), old, "host-2"),
        NewEvent::new(K::HostTimeout, Some(pid(2)), old, "timeout-2"),
        NewEvent::new(K::HostAssigned, Some(pid(3)), old, "host-3"),
    ] {
        db.repo.append_event(fid(1), &event).await.unwrap();
    }

    adapter.resume_unfinished().await.unwrap();
    let (log, state, _) = load(&db, fid(1)).await;
    assert_eq!(state, S::Abandoned);
    assert_eq!(abandon_reason(&log), AbandonReason::HostCandidatesExhausted);
    let timeout = log
        .events
        .iter()
        .find(|event| {
            event.event.kind == K::HostTimeout
                && event.payload[TIMEOUT_REASON_KEY] == "host_no_response"
        })
        .expect("the expired third candidate is recorded");
    assert_eq!(timeout.event.subject, Some(pid(3)));
    db.done().await;
}

#[sqlx::test(
    fixtures("reference", "app_role"),
    fixtures("../ops/app_privileges.sql")
)]
async fn expired_code_post_moves_to_the_next_host_candidate(owner: PgPool) {
    let db = AppDb::connect(&owner).await;
    let fake = Arc::new(FakeGateway::default());
    let lifecycle = LifecycleConfig {
        code_post_timeout: Duration::from_secs(1),
        ..LifecycleConfig::default()
    };
    let adapter = adapter_with_lifecycle(&db, fake, 1, lifecycle);
    let old = Utc::now() - TimeDelta::seconds(10);
    db.repo.create_formation(&formation(1)).await.unwrap();
    for event in [
        NewEvent::vc_assigned("1", 101, 102, old, "vc"),
        NewEvent::new(K::HostAssigned, Some(pid(1)), old, "host-1"),
        NewEvent::new(K::HostAccepted, Some(pid(1)), old, "accepted-1"),
    ] {
        db.repo.append_event(fid(1), &event).await.unwrap();
    }

    adapter.resume_unfinished().await.unwrap();
    let (log, state, _) = load(&db, fid(1)).await;
    assert_eq!(
        state,
        S::AwaitingHost {
            pending: Some(pid(2)),
            tried: 2,
        }
    );
    assert!(log.events.iter().any(|event| {
        event.event.kind == K::HostTimeout
            && event.event.subject == Some(pid(1))
            && event.payload[TIMEOUT_REASON_KEY] == "room_code_not_posted"
    }));
    db.done().await;
}

#[sqlx::test(
    fixtures("reference", "app_role"),
    fixtures("../ops/app_privileges.sql")
)]
async fn booting_with_no_reports_past_the_deadline_voids_the_formation(owner: PgPool) {
    let db = AppDb::connect(&owner).await;
    let fake = Arc::new(FakeGateway::default());
    let lifecycle = LifecycleConfig {
        no_report_timeout: Duration::from_secs(1),
        ..LifecycleConfig::default()
    };
    let adapter = adapter_with_lifecycle(&db, fake, 1, lifecycle);
    let old = Utc::now() - TimeDelta::seconds(10);
    db.repo.create_formation(&formation(1)).await.unwrap();
    for event in [
        NewEvent::vc_assigned("1", 101, 102, old, "vc"),
        NewEvent::new(K::HostAssigned, Some(pid(1)), old, "host"),
        NewEvent::new(K::HostAccepted, Some(pid(1)), old, "accepted"),
        NewEvent::code_posted(pid(1), "1234", old, "code"),
    ] {
        db.repo.append_event(fid(1), &event).await.unwrap();
    }

    adapter.resume_unfinished().await.unwrap();
    let (log, state, _) = load(&db, fid(1)).await;
    assert_eq!(state, S::Voided);
    assert_eq!(log.events.last().unwrap().event.kind, K::Voided);
    assert_eq!(
        log.events.last().unwrap().payload[TIMEOUT_REASON_KEY],
        "no_reports"
    );
    db.done().await;
}

#[sqlx::test(
    fixtures("reference", "app_role"),
    fixtures("../ops/app_privileges.sql")
)]
async fn an_elapsed_report_window_is_evaluated_by_the_existing_resume_loop(owner: PgPool) {
    let db = AppDb::connect(&owner).await;
    let fake = Arc::new(FakeGateway::default());
    let lifecycle = LifecycleConfig {
        no_report_timeout: Duration::from_secs(1),
        report_window: Duration::from_secs(1),
        ..LifecycleConfig::default()
    };
    let adapter = adapter_with_lifecycle(&db, fake, 1, lifecycle);
    let old = Utc::now() - TimeDelta::seconds(10);
    db.repo.create_formation(&formation(1)).await.unwrap();
    for event in [
        NewEvent::vc_assigned("1", 101, 102, old, "vc"),
        NewEvent::new(K::HostAssigned, Some(pid(1)), old, "host"),
        NewEvent::new(K::HostAccepted, Some(pid(1)), old, "accepted"),
        NewEvent::code_posted(pid(1), "1234", old, "code"),
    ] {
        db.repo.append_event(fid(1), &event).await.unwrap();
    }
    db.repo
        .append_report(&NewReport {
            formation_id: fid(1),
            player_id: pid(1),
            reported_at: old,
            outcome: Outcome::Win(Team::Red),
            raw_result: serde_json::json!({ "source": "test" }),
            supersedes_id: None,
            idempotency_key: "phase5-report".into(),
        })
        .await
        .unwrap();

    adapter.resume_unfinished().await.unwrap();
    let (log, state, _) = load(&db, fid(1)).await;
    assert_eq!(state, S::Confirmed);
    assert_eq!(log.events.last().unwrap().event.kind, K::Confirmed);
    assert_eq!(log.events.last().unwrap().payload["outcome"], "red");
    assert!(log.events.iter().all(|event| event.event.kind != K::Voided));

    // **評価器が書いた confirmed が、そのままレート導出の入力になること**（07 §2-2）。
    // ここを繋いでおかないと、ビュー側のテストは自分で作った payload しか見ていない。
    // formation(1) は red の平均 μ が 27（表示 1620）、blue が 22（表示 1320）。
    // params v3: 差に関わらず red は +7、blue は −7（分岐なし）。
    let ratings = unite_matchsystem::rating::RatingAdjustmentRepository::new(db.pool.clone());
    assert_eq!(ratings.display_rating(pid(1)).await.unwrap(), 1507.0);
    assert_eq!(ratings.display_rating(pid(6)).await.unwrap(), 1493.0);

    db.done().await;
}

#[sqlx::test(
    fixtures("reference", "app_role"),
    fixtures("../ops/app_privileges.sql")
)]
async fn recorded_vc_assignment_survives_a_restart_without_double_allocating(owner: PgPool) {
    let db = AppDb::connect(&owner).await;
    let fake = Arc::new(FakeGateway::default());
    let available = vec![VcAssignment {
        label: "1".into(),
        red_channel_id: 101,
        blue_channel_id: 102,
    }];
    db.repo.create_formation(&formation(1)).await.unwrap();
    assert!(matches!(
        db.repo
            .assign_available_vc(fid(1), &available, Utc::now())
            .await
            .unwrap(),
        VcAssignmentOutcome::Assigned { .. }
    ));

    // ここがプロセス停止点に相当する。新しい Adapter はメモリ予約を持たないが、DB にある
    // vc_assigned を占有として扱うため、別 formation に同じ組を割り当てない。
    let restarted = adapter(&db, fake, 1);
    db.repo.create_formation(&formation(2)).await.unwrap();
    restarted.start_formation(fid(2)).await.unwrap();
    let (second_log, second_state, _) = load(&db, fid(2)).await;
    assert_eq!(second_state, S::Abandoned);
    assert_eq!(abandon_reason(&second_log), AbandonReason::VcPoolExhausted);

    restarted.resume_unfinished().await.unwrap();
    let (_, first_state, first_record) = load(&db, fid(1)).await;
    assert_eq!(
        first_state,
        S::AwaitingHost {
            pending: Some(pid(1)),
            tried: 1,
        }
    );
    assert_eq!(first_record.vc.unwrap().label, "1");
    db.done().await;
}

#[sqlx::test(
    fixtures("reference", "app_role"),
    fixtures("../ops/app_privileges.sql")
)]
async fn a_disputed_formation_releases_its_voice_channel_pair(owner: PgPool) {
    let db = AppDb::connect(&owner).await;
    let fake = Arc::new(FakeGateway::default());
    let available = vec![VcAssignment {
        label: "1".into(),
        red_channel_id: 101,
        blue_channel_id: 102,
    }];
    db.repo.create_formation(&formation(1)).await.unwrap();
    db.repo
        .assign_available_vc(fid(1), &available, Utc::now())
        .await
        .unwrap();
    for event in [
        NewEvent::new(K::HostAssigned, Some(pid(1)), Utc::now(), "host"),
        NewEvent::new(K::HostAccepted, Some(pid(1)), Utc::now(), "accepted"),
        NewEvent::code_posted(pid(1), "1234", Utc::now(), "code"),
        NewEvent::new(K::Disputed, None, Utc::now(), "disputed"),
    ] {
        db.repo.append_event(fid(1), &event).await.unwrap();
    }

    let adapter = adapter(&db, fake, 1);
    db.repo.create_formation(&formation(2)).await.unwrap();
    adapter.start_formation(fid(2)).await.unwrap();
    let (_, _, record) = load(&db, fid(2)).await;
    assert_eq!(record.vc.unwrap().label, "1");
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
        SITE,
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
    decline_all(&db, &adapter, 1).await;
    let (vc2, _, _) = started(&db, &adapter, 2).await;
    assert_eq!(vc2.label, "2", "never used comes first");
    decline_all(&db, &adapter, 2).await;
    let (vc3, _, _) = started(&db, &adapter, 3).await;
    assert_eq!(vc3.label, "1", "released earliest comes first");
    decline_all(&db, &adapter, 3).await;
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
    assert_eq!(
        kinds(&log),
        vec![K::VcAssigned, K::VcAssigned, K::Abandoned]
    );
    assert_eq!(abandon_reason(&log), AbandonReason::DiscordCallFailed);
    // vc_assigned: VC 割当は残り、割り当て表の失敗（両側）も残る
    assert_eq!(record.vc.unwrap().label, "1");
    assert!(record.assignment_message_ids.is_empty());
    let vc_failures = log.events[1].payload[DISCORD_FAILURES_KEY]
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
    let abandon_failures = log.events[2].payload[DISCORD_FAILURES_KEY]
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
async fn a_failed_summon_is_recorded_without_abandoning_the_formation(owner: PgPool) {
    let db = AppDb::connect(&owner).await;
    let fake = Arc::new(FakeGateway::default());
    fake.fail_send_channel.store(998, Ordering::SeqCst);
    let adapter = adapter(&db, fake.clone(), 1);
    db.repo.create_formation(&formation(1)).await.unwrap();

    adapter.start_formation(fid(1)).await.unwrap();
    let (log, state, record) = load(&db, fid(1)).await;
    assert!(
        matches!(
            state,
            S::AwaitingHost {
                pending: Some(_),
                ..
            }
        ),
        "summon failure must not discard the match: {state}"
    );
    assert_eq!(
        kinds(&log),
        vec![K::VcAssigned, K::VcAssigned, K::HostAssigned]
    );
    assert_eq!(record.assignment_message_ids.len(), 2);
    assert_eq!(record.summon_notification, None);
    // 招集はチームごとに 1 通ずつ出すので、失敗も 2 件残る
    let failures = log.events[1].payload[DISCORD_FAILURES_KEY]
        .as_array()
        .unwrap();
    assert_eq!(failures.len(), 2, "{failures:?}");
    let operations: Vec<&str> = failures
        .iter()
        .map(|failure| failure[OPERATION_KEY].as_str().unwrap())
        .collect();
    assert_eq!(operations, ["send:summon:red", "send:summon:blue"]);
    assert_eq!(failures[0][ATTEMPTS_KEY], 3);
    let indexed = sqlx::query!(
        "select kind, operation from discord_failures where formation_id = $1 order by id",
        fid(1).0,
    )
    .fetch_all(&db.pool)
    .await
    .unwrap();
    // 招集チャンネルへの投稿はどれも失敗する（ホストの通知も同じチャンネル）。
    // どれも監査ログに残り、どれも formation を落とさない
    assert!(
        indexed
            .iter()
            .any(|row| row.kind == "summon_notification" && row.operation == "send:summon:red"),
        "{indexed:?}"
    );
    assert!(
        indexed
            .iter()
            .any(|row| row.kind == "summon_notification" && row.operation == "send:summon:blue"),
        "{indexed:?}"
    );
    assert!(
        indexed
            .iter()
            .any(|row| row.operation == "send:host_announcement"),
        "{indexed:?}"
    );
    assert!(
        !kinds(&log).contains(&K::Abandoned),
        "notification failure is not a formation failure"
    );
    db.done().await;
}

#[sqlx::test(
    fixtures("reference", "app_role"),
    fixtures("../ops/app_privileges.sql")
)]
async fn matchmaker_logs_skip_empty_runs_and_failures_do_not_discard_formations(owner: PgPool) {
    use unite_matchsystem::matchmaker::{
        ActiveFormationCounts, AssignedFormation, MatchmakerSummary, RunOutcome, SelectionMetrics,
    };

    let db = AppDb::connect(&owner).await;
    let fake = Arc::new(FakeGateway::default());
    let adapter = adapter(&db, fake.clone(), 1);
    let now = Utc::now();

    adapter
        .publish_matchmaker_run_log(&RunOutcome::default(), now)
        .await
        .unwrap();
    assert_eq!(fake.total_sends(), 0, "成立0の回は投稿しない");
    adapter
        .publish_matchmaker_summary(&MatchmakerSummary::default(), now)
        .await
        .unwrap();
    assert_eq!(
        fake.total_sends(),
        0,
        "キューも進行中formationも無いサマリは投稿しない"
    );

    db.repo.create_formation(&formation(1)).await.unwrap();
    fake.fail_send_channel.store(997, Ordering::SeqCst);
    let outcome = RunOutcome {
        candidates: 10,
        created: vec![fid(1)],
        assignments: vec![AssignedFormation {
            formation_id: fid(1),
            vc: VcAssignment {
                label: "1".into(),
                red_channel_id: 101,
                blue_channel_id: 102,
            },
            selection: SelectionMetrics::default(),
        }],
        vc_exhausted: false,
        unmatched: 0,
        longest_unmatched_wait_seconds: None,
        paused: false,
    };
    adapter
        .publish_matchmaker_run_log(&outcome, now)
        .await
        .unwrap();
    adapter
        .publish_matchmaker_summary(
            &MatchmakerSummary {
                formed_last_15_minutes: 1,
                active: ActiveFormationCounts {
                    awaiting_reports: 1,
                    ..ActiveFormationCounts::default()
                },
                ..MatchmakerSummary::default()
            },
            now,
        )
        .await
        .unwrap();

    let formation_count: i64 = sqlx::query_scalar!("select count(*) from formations")
        .fetch_one(&db.pool)
        .await
        .unwrap()
        .unwrap_or_default();
    assert_eq!(formation_count, 1, "Discordログ失敗でformationを捨てない");
    let kinds = sqlx::query_scalar!(
        "select kind from discord_failures where formation_event_id is null order by id"
    )
    .fetch_all(&db.pool)
    .await
    .unwrap();
    assert_eq!(kinds, ["matchmaker_run_log", "matchmaker_summary"]);
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

/// **同じ状態から何度再開しても、ホストへの通知はちょうど 1 回だけ。**
///
/// 本番で「ロビー ID を入れると自チーム VC と相手チーム VC の両方でメンションが飛ぶ」
/// という報告があった。切り分けた結果、経路も冪等判定も正しく、原因は開示の 2 通とも
/// `mention_users` にホストを載せていたことだった（`allowed_mentions` に載った側だけが
/// 通知される）。ここでは投稿経路の冪等と、通知が 1 回であることを同時に測る。
#[sqlx::test(
    fixtures("reference", "app_role"),
    fixtures("../ops/app_privileges.sql")
)]
async fn revealing_the_room_code_notifies_the_host_exactly_once(owner: PgPool) {
    let db = AppDb::connect(&owner).await;
    let fake = Arc::new(FakeGateway::default());
    let adapter = adapter(&db, fake.clone(), 2);

    let (vc, _, _) = started(&db, &adapter, 1).await;
    accept_host(&db, &adapter, 1, 1).await;
    let sends_before_reveal = fake.total_sends();
    post_room_code(&db, &adapter, 1, 1, "1234").await;
    let sends_after_reveal = fake.total_sends();

    // 開示 2 通 + 催促 2 通が出る
    assert_eq!(sends_after_reveal - sends_before_reveal, 4);

    // **同じ状態のまま再開ループを何度も回す。**投稿は 1 通も増えない
    for _ in 0..5 {
        adapter.resume_unfinished().await.unwrap();
    }
    assert_eq!(
        fake.total_sends(),
        sends_after_reveal,
        "同じ状態から再投稿されている"
    );

    // ホストは red（pid(1)）。開示は両 VC に出るが、通知は red 側の 1 通だけ
    let (_, _, record) = load(&db, fid(1)).await;
    let mut notified = 0;
    for team in Team::ALL {
        let reveal = fake.message(vc.channel(team), record.code_message_ids[&team]);
        assert!(reveal.content.contains("<@1001>"), "{}", reveal.content);
        if reveal.mention_users.contains(&user(1)) {
            notified += 1;
            assert_eq!(team, Team::Red, "通知は自チーム側だけ");
        }
    }
    assert_eq!(notified, 1, "ホストが 2 回叩かれている");

    // 催促にはメンションを付けない（全員分の通知にならない）
    for team in Team::ALL {
        let report = fake.message(vc.channel(team), record.report_message_ids[&team]);
        assert!(report.mention_users.is_empty(), "{report:?}");
    }

    // 打ち直しは別の内容なので必ず届く。それでも通知は 1 回のまま
    post_room_code(&db, &adapter, 1, 1, "9999").await;
    let (_, _, record) = load(&db, fid(1)).await;
    let mut notified = 0;
    for team in Team::ALL {
        let reveal = fake.message(vc.channel(team), record.code_message_ids[&team]);
        assert!(reveal.content.contains("**9999**"), "{}", reveal.content);
        if reveal.mention_users.contains(&user(1)) {
            notified += 1;
        }
    }
    assert_eq!(notified, 1, "打ち直しでもホストは 1 回だけ");

    db.done().await;
}
