//! 当日の運用機能（`/admin` の 3 つ）の統合テスト。実際の PostgreSQL を使う。
//!
//! リポジトリは所有者ではなく**アプリ用ロール**（`assay_app`）で接続する。所有者権限で
//! 走らせると、権限の配線ミスが本番でしか出ない。
//!
//! 検証すること:
//! - **停止状態がプロセス再起動をまたいで保たれる** — 状態は DB（`admin_actions`）にあり、
//!   別プロセス相当の新しいプール・新しいマッチメイカでも止まったまま
//! - 停止中は formation を作らないが、**ループは回りログとサマリは出続ける**
//! - 管理者でないセッションからは停止も強制離脱もブロック枠の変更もできない（404）
//! - 強制離脱した本人のキュー画面に理由が出る（黙って消さない）
//! - **進行中の formation を持つ人を強制離脱させても試合が壊れない**
//! - ブロック枠の既定は 1。変更は追記として記録され、誰がいつ変えたかが残る
//! - 冪等は「同一 idempotency_key のみ no-op」

use std::time::Duration;

use axum::Router;
use axum::body::Body;
use axum::http::{Request, Response, StatusCode, header};
use axum::response::IntoResponse;
use axum_extra::extract::SignedCookieJar;
use chrono::{TimeDelta, TimeZone, Utc};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use sqlx::PgPool;
use sqlx::postgres::PgPoolOptions;
use tower::ServiceExt;
use unite_matchsystem::admin::{
    AdminRepository, AdminState, DEFAULT_BLOCK_ALLOWANCE, parse_admin_ids, router as admin_router,
};
use unite_matchsystem::blocks::BlockRepository;
use unite_matchsystem::lifecycle::{
    FormationEventKind as K, FormationId, FormationState as S, PlayerId, Timestamp,
};
use unite_matchsystem::matchmaker::{
    Matchmaker, MatchmakerConfig, MatchmakerRepository, ROLES, RunOutcome,
};
use unite_matchsystem::persistence::{
    FormationRepository, NewEvent, NewFormation, NewFormationPlayer, NewReport, Outcome, Team,
    VcAssignment,
};
use unite_matchsystem::queue::model::{QueueRequest, RolePreference};
use unite_matchsystem::queue::{QueueRepository, QueueState, router as queue_router};
use unite_matchsystem::quiet::QuietReportRepository;
use unite_matchsystem::registration::session::{SESSION_COOKIE, Session, set_session};
use unite_matchsystem::registration::{AuthConfig, NewDeclaration, Rank, RegistrationRepository};
use uuid::Uuid;

const APP_ROLE: &str = "assay_app";
const APP_ROLE_PASSWORD: &str = "assay_app_test";
const COOKIE_DOMAIN: &str = "match.example.test";
/// .env.example に入れている管理者の 1 人目。
const ADMIN_ID: i64 = 900000000000000001;
/// 管理者は 1 番。キューに入るのは 2 番以降。
const ADMIN: u128 = 1;

// ----------------------------------------------------------------------
// 道具
// ----------------------------------------------------------------------

fn at(seconds: i64) -> Timestamp {
    Utc.with_ymd_and_hms(2026, 9, 12, 21, 0, 0).unwrap() + TimeDelta::seconds(seconds)
}

fn player(n: u128) -> PlayerId {
    PlayerId(Uuid::from_u128(n))
}

fn config() -> AuthConfig {
    AuthConfig::new(
        "client-id",
        "client-secret",
        "https://match.example.test/auth/callback",
        COOKIE_DOMAIN,
        "k".repeat(64),
        Duration::from_secs(3600),
    )
    .expect("valid test config")
}

/// 署名済みのセッション Cookie。OAuth を通さずにログイン済みの状態を作る
/// （Cookie に入るのは識別子と発行時刻だけなので、本物のログインと同じ材料でしかない）。
fn session_cookie(player_id: PlayerId) -> String {
    let config = config();
    let jar = set_session(
        SignedCookieJar::new(config.cookie_key()),
        &Session::issue(player_id),
        &config,
    );
    jar.into_response()
        .headers()
        .get_all(header::SET_COOKIE)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .find(|raw| raw.starts_with(SESSION_COOKIE))
        .and_then(|raw| raw.split(';').next())
        .expect("a signed session cookie")
        .to_owned()
}

/// テスト DB にアプリ用ロールで接続したプール。**別インスタンスは別プロセス相当**
/// （プロセス内の状態を持っていないことの確認に使う）。
struct AppDb {
    pool: PgPool,
    queue: QueueRepository,
    registration: RegistrationRepository,
    formations: FormationRepository,
    admin: AdminRepository,
    blocks: BlockRepository,
}

impl AppDb {
    async fn connect(owner: &PgPool) -> Self {
        let options = (*owner.connect_options())
            .clone()
            .username(APP_ROLE)
            .password(APP_ROLE_PASSWORD);
        let pool = PgPoolOptions::new()
            .max_connections(6)
            .connect_with(options)
            .await
            .expect("connect as the app role");
        Self {
            queue: QueueRepository::new(pool.clone()),
            registration: RegistrationRepository::new(pool.clone()),
            formations: FormationRepository::new(pool.clone()),
            admin: AdminRepository::new(pool.clone()),
            blocks: BlockRepository::new(pool.clone()),
            pool,
        }
    }

    /// `main.rs` と同じ組み合わせ。キュー画面と管理パネルを 1 つのアプリに載せる。
    fn router(&self) -> Router {
        let ids = parse_admin_ids(Some(&ADMIN_ID.to_string()))
            .expect("valid list")
            .expect("configured");
        queue_router(
            QueueState::new(config(), self.registration.clone(), self.queue.clone())
                .with_admins(Some(ids.clone())),
        )
        .merge(admin_router(AdminState::new(
            config(),
            ids,
            self.admin.clone(),
            self.blocks.clone(),
            QuietReportRepository::new(self.pool.clone()),
        )))
    }

    fn matchmaker(&self, vc_pool_size: usize) -> Matchmaker {
        Matchmaker::new(
            MatchmakerRepository::new(self.pool.clone()),
            (1..=vc_pool_size)
                .map(|n| VcAssignment {
                    label: n.to_string(),
                    red_channel_id: 100 + n as u64 * 2,
                    blue_channel_id: 101 + n as u64 * 2,
                })
                .collect(),
            MatchmakerConfig::default(),
        )
    }

    async fn done(self) {
        self.pool.close().await;
    }
}

async fn send(app: &Router, request: Request<Body>) -> Response<Body> {
    app.clone().oneshot(request).await.unwrap()
}

async fn body_text(response: Response<Body>) -> String {
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    String::from_utf8(bytes.to_vec()).unwrap()
}

async fn get_html(app: &Router, as_player: PlayerId, path: &str) -> (StatusCode, String) {
    let request = Request::get(path)
        .header(header::COOKIE, session_cookie(as_player))
        .header(header::ACCEPT, "text/html")
        .body(Body::empty())
        .unwrap();
    let response = send(app, request).await;
    let status = response.status();
    (status, body_text(response).await)
}

async fn get_json(app: &Router, as_player: PlayerId, path: &str) -> (StatusCode, Value) {
    let request = Request::get(path)
        .header(header::COOKIE, session_cookie(as_player))
        .body(Body::empty())
        .unwrap();
    let response = send(app, request).await;
    let status = response.status();
    let text = body_text(response).await;
    (status, serde_json::from_str(&text).expect("a JSON body"))
}

async fn post_form(
    app: &Router,
    as_player: PlayerId,
    path: &str,
    body: &str,
) -> (StatusCode, String) {
    let request = Request::post(path)
        .header(header::COOKIE, session_cookie(as_player))
        .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
        .body(Body::from(body.to_owned()))
        .unwrap();
    let response = send(app, request).await;
    let status = response.status();
    (status, body_text(response).await)
}

/// 描画されたフォームから冪等キーを読む（本物の画面と同じ値を送る）。
fn form_key(html: &str, action: &str) -> String {
    html.split(&format!("action=\"{action}\""))
        .nth(1)
        .expect("the form is rendered")
        .split("name=\"key\" value=\"")
        .nth(1)
        .expect("the hidden idempotency key")
        .split('"')
        .next()
        .expect("the key value")
        .to_owned()
}

async fn seed_player(db: &AppDb, n: u128, discord_user_id: i64) {
    sqlx::query(
        "insert into players (id, discord_user_id, created_at, trainer_name) \
         values ($1, $2, now(), $3) on conflict do nothing",
    )
    .bind(player(n).0)
    .bind(discord_user_id)
    .bind(format!("トレーナー{n}"))
    .execute(&db.pool)
    .await
    .expect("seed a player");
}

/// 1 番が管理者、2 番以降が参加者（`discord_user_id` は 1000 + n）。
async fn seed_cast(db: &AppDb, participants: u128) {
    seed_player(db, ADMIN, ADMIN_ID).await;
    for n in 2..=(participants + 1) {
        seed_player(db, n, 1000 + n as i64).await;
        register(db, player(n)).await;
    }
}

/// 登録ゲートを通す。
async fn register(db: &AppDb, player_id: PlayerId) {
    db.registration
        .register(&NewDeclaration {
            player_id,
            declared_at: at(0),
            trainer_name: format!("トレーナー{}", player_id.0.as_u128()),
            rank: Rank::Legend,
            rating: Some(1500),
            raw: json!({ "rank": "legend" }),
        })
        .await
        .expect("record the declaration");
}

/// ロールがちょうど 2 人ずつ埋まるように 2..=11 をキューに入れる。
async fn enqueue_full_roster(db: &AppDb, enqueued_at: Timestamp) {
    for n in 2..=11u128 {
        enqueue(
            db,
            player(n),
            ROLES[(n as usize - 2) % ROLES.len()],
            enqueued_at,
        )
        .await;
    }
}

async fn enqueue(db: &AppDb, who: PlayerId, role: &str, enqueued_at: Timestamp) {
    db.queue
        .enqueue(
            who,
            &QueueRequest {
                roles: vec![RolePreference {
                    role: role.to_owned(),
                    priority: 1,
                }],
            },
            enqueued_at,
        )
        .await
        .expect("enqueue");
}

async fn count(db: &AppDb, table: &str) -> i64 {
    let sql = format!("select count(*) from {table}");
    sqlx::query_scalar(sqlx::AssertSqlSafe(sql))
        .fetch_one(&db.pool)
        .await
        .expect("count rows")
}

/// 管理パネルからマッチメイカを止める（画面のフォームと同じ経路）。
async fn pause_from_the_panel(app: &Router, reason: &str) -> String {
    let (status, page) = get_html(app, player(ADMIN), "/admin/queue").await;
    assert_eq!(status, StatusCode::OK);
    let key = form_key(&page, "/admin/matchmaker");
    let (status, page) = post_form(
        app,
        player(ADMIN),
        "/admin/matchmaker",
        &format!("action=pause&key={key}&reason={reason}"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    page
}

// ----------------------------------------------------------------------
// A. マッチメイカの停止・再開
// ----------------------------------------------------------------------

/// 停止状態は DB にあり、**プロセスを再起動しても保たれる**。
///
/// 「止めたまま再起動したら動き出した」が当日いちばん危ない。別プールで作った
/// マッチメイカ（＝プロセス内の状態を何も引き継いでいない）でも formation を作らない。
#[sqlx::test(fixtures("app_role"), fixtures("../ops/app_privileges.sql"))]
async fn the_pause_survives_a_process_restart(owner: PgPool) {
    let db = AppDb::connect(&owner).await;
    seed_cast(&db, 10).await;
    enqueue_full_roster(&db, at(0)).await;
    let app = db.router();

    let page = pause_from_the_panel(&app, "テスト").await;
    assert!(page.contains("マッチメイカを停止しました"), "{page}");
    assert!(page.contains("state-paused"), "{page}");

    // ここが「再起動」。新しいプール・新しいリポジトリ・新しいマッチメイカ
    let restarted = AppDb::connect(&owner).await;
    let outcome = restarted.matchmaker(2).run_once(at(60)).await.unwrap();
    assert!(outcome.paused, "停止状態は DB から読み直される");
    assert!(outcome.created.is_empty(), "formation を作らない");
    assert_eq!(count(&db, "formations").await, 0);
    assert_eq!(count(&db, "queue_entries").await, 10, "キューは触らない");
    restarted.done().await;
    db.done().await;
}

/// 停止中でもループは回り、キューの人数と待ち時間は数え続ける（ログとサマリを出すため）。
#[sqlx::test(fixtures("app_role"), fixtures("../ops/app_privileges.sql"))]
async fn a_paused_matchmaker_keeps_counting_and_publishing(owner: PgPool) {
    let db = AppDb::connect(&owner).await;
    seed_cast(&db, 10).await;
    enqueue_full_roster(&db, at(0)).await;
    let app = db.router();
    pause_from_the_panel(&app, "VC不調").await;

    let outcome = db.matchmaker(2).run_once(at(600)).await.unwrap();
    assert_eq!(
        outcome,
        RunOutcome {
            candidates: 10,
            unmatched: 10,
            longest_unmatched_wait_seconds: Some(600),
            paused: true,
            ..RunOutcome::default()
        },
        "止めていても、何人を待たせているかは見えなくならない"
    );

    let summary = db.matchmaker(2).summary(at(600)).await.unwrap();
    let pause = summary.paused.as_ref().expect("停止中とわかる");
    assert_eq!(pause.actor_discord_user_id, ADMIN_ID, "誰が止めたか");
    assert_eq!(pause.reason.as_deref(), Some("VC不調"));
    assert_eq!(summary.queued, 10);
    assert!(summary.should_publish());
    db.done().await;
}

/// キューが空でも、停止中は運営ログへ出し続ける（止めたまま忘れるのを防ぐ）。
#[sqlx::test(fixtures("app_role"), fixtures("../ops/app_privileges.sql"))]
async fn an_empty_queue_still_publishes_the_summary_while_paused(owner: PgPool) {
    let db = AppDb::connect(&owner).await;
    seed_cast(&db, 0).await;
    let app = db.router();

    let running = db.matchmaker(2).summary(at(0)).await.unwrap();
    assert!(
        !running.should_publish(),
        "稼働中で誰も居なければ投稿しない（既存の挙動）"
    );

    pause_from_the_panel(&app, "調整中").await;
    let paused = db.matchmaker(2).summary(at(0)).await.unwrap();
    assert_eq!(paused.queued, 0);
    assert!(
        paused.should_publish(),
        "停止中は空でも投稿する（止めたまま忘れない）"
    );
    db.done().await;
}

/// 再開すると次の実行から普通に成立する。
#[sqlx::test(fixtures("app_role"), fixtures("../ops/app_privileges.sql"))]
async fn resuming_forms_matches_again(owner: PgPool) {
    let db = AppDb::connect(&owner).await;
    seed_cast(&db, 10).await;
    enqueue_full_roster(&db, at(0)).await;
    let app = db.router();

    let page = pause_from_the_panel(&app, "点検").await;
    assert!(db.matchmaker(2).run_once(at(60)).await.unwrap().paused);

    let key = form_key(&page, "/admin/matchmaker");
    let (status, page) = post_form(
        &app,
        player(ADMIN),
        "/admin/matchmaker",
        &format!("action=resume&key={key}&reason="),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(page.contains("マッチメイカを再開しました"), "{page}");
    assert!(page.contains("state-running"), "{page}");

    let outcome = db.matchmaker(2).run_once(at(120)).await.unwrap();
    assert!(!outcome.paused);
    assert_eq!(outcome.created.len(), 1);
    assert_eq!(count(&db, "queue_entries").await, 0);
    db.done().await;
}

/// 停止と再開は**追記**として残る。誰がいつ止めたかが上書きで消えない。
#[sqlx::test(fixtures("app_role"), fixtures("../ops/app_privileges.sql"))]
async fn pausing_and_resuming_leaves_an_append_only_trail(owner: PgPool) {
    let db = AppDb::connect(&owner).await;
    seed_cast(&db, 0).await;
    let app = db.router();

    let page = pause_from_the_panel(&app, "1回目").await;
    let key = form_key(&page, "/admin/matchmaker");
    post_form(
        &app,
        player(ADMIN),
        "/admin/matchmaker",
        &format!("action=resume&key={key}&reason="),
    )
    .await;

    assert_eq!(count(&db, "admin_actions").await, 2, "2 行とも残る");
    assert!(
        db.admin.matchmaker_pause().await.unwrap().is_none(),
        "最新行が再開なので稼働中"
    );
    let (_, page) = get_html(&app, player(ADMIN), "/admin/queue").await;
    assert!(page.contains("マッチメイカ停止"), "履歴に残る: {page}");
    assert!(page.contains("マッチメイカ再開"), "履歴に残る: {page}");
    assert!(page.contains("1回目"), "理由も残る: {page}");
    db.done().await;
}

/// 同じフォームの二重送信は行を増やさない（冪等は同一キーのみ）。
#[sqlx::test(fixtures("app_role"), fixtures("../ops/app_privileges.sql"))]
async fn the_same_control_form_submitted_twice_records_one_row(owner: PgPool) {
    let db = AppDb::connect(&owner).await;
    seed_cast(&db, 0).await;
    let app = db.router();

    let (_, page) = get_html(&app, player(ADMIN), "/admin/queue").await;
    let key = form_key(&page, "/admin/matchmaker");
    let body = format!("action=pause&key={key}&reason=");
    post_form(&app, player(ADMIN), "/admin/matchmaker", &body).await;
    let (_, second) = post_form(&app, player(ADMIN), "/admin/matchmaker", &body).await;

    assert!(second.contains("既に記録されています"), "{second}");
    assert_eq!(count(&db, "admin_actions").await, 1);
    db.done().await;
}

/// 起動時のログ相当。停止状態は 1 本のクエリで読める（`main.rs` が起動時に出す値）。
#[sqlx::test(fixtures("app_role"), fixtures("../ops/app_privileges.sql"))]
async fn the_pause_state_is_readable_without_the_http_layer(owner: PgPool) {
    let db = AppDb::connect(&owner).await;
    seed_cast(&db, 0).await;
    assert!(db.admin.matchmaker_pause().await.unwrap().is_none());
    pause_from_the_panel(&db.router(), "起動ログの確認").await;

    let restarted = AppDb::connect(&owner).await;
    let pause = restarted
        .admin
        .matchmaker_pause()
        .await
        .unwrap()
        .expect("起動時に停止中とわかる");
    assert_eq!(pause.actor_discord_user_id, ADMIN_ID);
    assert_eq!(pause.reason.as_deref(), Some("起動ログの確認"));
    restarted.done().await;
    db.done().await;
}

// ----------------------------------------------------------------------
// B. キュー中の一覧と強制離脱
// ----------------------------------------------------------------------

/// 一覧に出す 5 項目（トレーナー名 / 待ち時間 / ロール希望 / 下方向の範囲 / レート）。
#[sqlx::test(fixtures("app_role"), fixtures("../ops/app_privileges.sql"))]
async fn the_queue_screen_lists_the_players_with_their_settings(owner: PgPool) {
    let db = AppDb::connect(&owner).await;
    seed_cast(&db, 2).await;
    enqueue(&db, player(2), "jungle", at(0)).await;
    sqlx::query(
        "insert into rating_adjustments \
         (player_id, delta_mu, delta_sigma, reason, created_by, created_at, idempotency_key) \
         values ($1, 100, 0, 'test seed', 'staff:test', now(), 'seed-2')",
    )
    .bind(player(2).0)
    .execute(&db.pool)
    .await
    .expect("append a rating adjustment");

    let (status, page) = get_html(&db.router(), player(ADMIN), "/admin/queue").await;
    assert_eq!(status, StatusCode::OK);
    assert!(page.contains("キュー中（1人）"), "{page}");
    assert!(page.contains("トレーナー2"));
    assert!(page.contains("1. 中央"), "ロール希望を表示名で出す");
    assert!(page.contains(">-50</td>"), "下方向の範囲（50 固定）");
    assert!(page.contains(">1600</td>"), "調整が乗ったレート");
    assert!(page.contains("action=\"/admin/queue/remove\""));
    db.done().await;
}

/// 強制離脱すると本人のキュー画面に理由が出る。**黙って消さない。**
#[sqlx::test(fixtures("app_role"), fixtures("../ops/app_privileges.sql"))]
async fn a_removed_player_is_told_why_on_their_own_queue_screen(owner: PgPool) {
    let db = AppDb::connect(&owner).await;
    seed_cast(&db, 2).await;
    enqueue(&db, player(2), "jungle", at(0)).await;
    let app = db.router();

    let (_, panel) = get_html(&app, player(ADMIN), "/admin/queue").await;
    let key = form_key(&panel, "/admin/queue/remove");
    let (status, page) = post_form(
        &app,
        player(ADMIN),
        "/admin/queue/remove",
        &format!(
            "player={}&key={key}&reason=別の卓に移動してもらうため",
            player(2).0.simple()
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(page.contains("キューから外しました"), "{page}");
    assert_eq!(count(&db, "queue_entries").await, 0);

    // 本人の画面。ポーリングは 10 秒でここへ落ちてくる
    let (status, mine) = get_html(&app, player(2), "/queue").await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        mine.contains("運営によってキューから外されました"),
        "{mine}"
    );
    assert!(mine.contains("別の卓に移動してもらうため"), "{mine}");
    assert!(
        mine.contains("data-view=\"unqueued\""),
        "再度入れる: {mine}"
    );

    // JSON でも同じことが返る（画面はこれをポーリングしている）
    let (_, status_json) = get_json(&app, player(2), "/queue").await;
    assert_eq!(
        status_json["removed_by_staff"]["reason"],
        json!("別の卓に移動してもらうため")
    );
    db.done().await;
}

/// 理由は任意。書かなくても「運営に外された」ことは伝える。
#[sqlx::test(fixtures("app_role"), fixtures("../ops/app_privileges.sql"))]
async fn the_reason_is_optional_but_the_notice_is_not(owner: PgPool) {
    let db = AppDb::connect(&owner).await;
    seed_cast(&db, 2).await;
    enqueue(&db, player(2), "jungle", at(0)).await;
    let app = db.router();

    post_form(
        &app,
        player(ADMIN),
        "/admin/queue/remove",
        &format!("player={}&key=remove-1&reason=", player(2).0.simple()),
    )
    .await;

    let (_, mine) = get_html(&app, player(2), "/queue").await;
    assert!(
        mine.contains("運営によってキューから外されました"),
        "{mine}"
    );
    assert!(mine.contains("理由は記載されていません"), "{mine}");
    db.done().await;
}

/// 入り直せば通知は消える（古い通知を出し続けない）。
#[sqlx::test(fixtures("app_role"), fixtures("../ops/app_privileges.sql"))]
async fn requeueing_clears_the_notice(owner: PgPool) {
    let db = AppDb::connect(&owner).await;
    seed_cast(&db, 2).await;
    enqueue(&db, player(2), "jungle", at(0)).await;
    let app = db.router();
    post_form(
        &app,
        player(ADMIN),
        "/admin/queue/remove",
        &format!("player={}&key=remove-1&reason=様子見", player(2).0.simple()),
    )
    .await;

    enqueue(&db, player(2), "jungle", at(120)).await;
    let (_, mine) = get_html(&app, player(2), "/queue").await;
    assert!(
        !mine.contains("運営によってキューから外されました"),
        "{mine}"
    );
    let (_, status_json) = get_json(&app, player(2), "/queue").await;
    assert_eq!(status_json["removed_by_staff"], Value::Null);
    db.done().await;
}

/// キューにいない人を外しても**記録を残さない**（身に覚えのない通知を出さない）。
#[sqlx::test(fixtures("app_role"), fixtures("../ops/app_privileges.sql"))]
async fn removing_someone_who_is_not_queued_records_nothing(owner: PgPool) {
    let db = AppDb::connect(&owner).await;
    seed_cast(&db, 2).await;
    let app = db.router();

    let (status, page) = post_form(
        &app,
        player(ADMIN),
        "/admin/queue/remove",
        &format!("player={}&key=remove-1&reason=", player(2).0.simple()),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(page.contains("既にキューにいません"), "{page}");
    assert_eq!(count(&db, "admin_actions").await, 0);

    let (_, mine) = get_html(&app, player(2), "/queue").await;
    assert!(
        !mine.contains("運営によってキューから外されました"),
        "{mine}"
    );
    db.done().await;
}

/// **進行中の formation を持つ人を外しても試合は壊れない。**
///
/// キューと formation は別の系。強制離脱が触るのは `queue_entries` だけで、
/// `formations` / `formation_players` / `formation_events` には一切書かない。
#[sqlx::test(fixtures("app_role"), fixtures("../ops/app_privileges.sql"))]
async fn removing_a_player_does_not_disturb_their_running_match(owner: PgPool) {
    let db = AppDb::connect(&owner).await;
    seed_cast(&db, 10).await;
    enqueue_full_roster(&db, at(0)).await;
    let outcome = db.matchmaker(2).run_once(at(60)).await.unwrap();
    let formation = *outcome.created.first().expect("a match was formed");
    let before = db.formations.load_events(formation).await.unwrap();

    // 試合中の人がなぜかキューにも入っている状態を作る（ゲートを通さない経路）
    enqueue(&db, player(2), "jungle", at(120)).await;
    let (status, page) = post_form(
        &db.router(),
        player(ADMIN),
        "/admin/queue/remove",
        &format!(
            "player={}&key=remove-1&reason=二重登録",
            player(2).0.simple()
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(page.contains("キューから外しました"), "{page}");

    let after = db.formations.load_events(formation).await.unwrap();
    assert_eq!(
        after.events.len(),
        before.events.len(),
        "イベントを足さない"
    );
    assert_eq!(count(&db, "formations").await, 1);
    assert_eq!(count(&db, "formation_players").await, 10);
    assert_eq!(count(&db, "queue_entries").await, 0);
    let unfinished = db.formations.list_unfinished().await.unwrap();
    assert_eq!(unfinished.len(), 1, "試合はそのまま進行中");
    assert_eq!(unfinished[0].log.formation_id, formation);
    db.done().await;
}

// ----------------------------------------------------------------------
// C. ブロック枠
// ----------------------------------------------------------------------

/// 既定は 1 枠。変更は追記として記録され、誰がいつ変えたかが残る。
#[sqlx::test(fixtures("app_role"), fixtures("../ops/app_privileges.sql"))]
async fn the_block_allowance_defaults_to_one_and_records_the_change(owner: PgPool) {
    let db = AppDb::connect(&owner).await;
    seed_cast(&db, 2).await;
    let app = db.router();
    assert_eq!(
        db.admin.block_allowance(player(2).0).await.unwrap(),
        DEFAULT_BLOCK_ALLOWANCE
    );

    let path = format!("/admin/player?player={}", player(2).0);
    let (status, page) = get_html(&app, player(ADMIN), &path).await;
    assert_eq!(status, StatusCode::OK);
    assert!(page.contains("<dt>ブロック枠</dt><dd>1 枠</dd>"), "{page}");

    let key = form_key(&page, "/admin/blocks");
    let (status, page) = post_form(
        &app,
        player(ADMIN),
        "/admin/blocks",
        &format!(
            "player={}&key={key}&allowance=4&reason=大会前の調整",
            player(2).0.simple()
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(page.contains("ブロック枠を 4 枠に変更しました"), "{page}");
    assert!(page.contains("<dt>ブロック枠</dt><dd>4 枠</dd>"), "{page}");
    assert_eq!(db.admin.block_allowance(player(2).0).await.unwrap(), 4);

    // 誰がいつ変えたか
    let actions = db.admin.recent_actions(10).await.unwrap();
    assert_eq!(actions.len(), 1);
    assert_eq!(actions[0].actor_discord_user_id, ADMIN_ID);
    assert_eq!(actions[0].target_player_id, Some(player(2).0));
    assert_eq!(actions[0].allowance, Some(4));
    assert_eq!(actions[0].reason.as_deref(), Some("大会前の調整"));
    db.done().await;
}

/// 誤ブロックは 5 日間残るため、管理者は画面からだけ即時解除できる。
/// 解除そのものと管理操作は、ともに追記行として残る。
#[sqlx::test(fixtures("app_role"), fixtures("../ops/app_privileges.sql"))]
async fn an_admin_can_override_an_active_block_and_the_action_is_recorded(owner: PgPool) {
    let db = AppDb::connect(&owner).await;
    seed_cast(&db, 2).await;
    db.blocks
        .block(player(2), player(3), "player-block", at(0))
        .await
        .unwrap();
    let app = db.router();
    let path = format!("/admin/player?player={}", player(2).0);
    let (status, page) = get_html(&app, player(ADMIN), &path).await;
    assert_eq!(status, StatusCode::OK);
    let key = form_key(&page, "/admin/blocks/override");

    let (status, page) = post_form(
        &app,
        player(ADMIN),
        "/admin/blocks/override",
        &format!(
            "player={}&target={}&key={key}&reason=mistake",
            player(2).0,
            player(3).0,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(page.contains("ブロックを管理解除しました"), "{page}");
    assert!(db.blocks.current_for(player(2)).await.unwrap().is_empty());

    let events: Vec<String> =
        sqlx::query_scalar("select kind from player_blocks where player_id = $1 order by id")
            .bind(player(2).0)
            .fetch_all(&db.pool)
            .await
            .unwrap();
    assert_eq!(events, ["blocked", "unblocked"]);
    let actions = db.admin.recent_actions(10).await.unwrap();
    assert_eq!(actions.len(), 1);
    assert_eq!(actions[0].raw_kind, "block_override_removed");
    assert_eq!(actions[0].target_player_id, Some(player(2).0));

    let (status, _) = post_form(
        &app,
        player(2),
        "/admin/blocks/override",
        &format!(
            "player={}&target={}&key=non-admin-override&reason=mistake",
            player(2).0,
            player(3).0,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    db.done().await;
}

/// 枠数は最新行が正。過去の行は消えない（追記専用）。
#[sqlx::test(fixtures("app_role"), fixtures("../ops/app_privileges.sql"))]
async fn the_latest_row_wins_and_the_old_ones_stay(owner: PgPool) {
    let db = AppDb::connect(&owner).await;
    seed_cast(&db, 2).await;
    let app = db.router();
    for (index, allowance) in [3, 0, 5].into_iter().enumerate() {
        post_form(
            &app,
            player(ADMIN),
            "/admin/blocks",
            &format!(
                "player={}&key=blocks-{index}&allowance={allowance}&reason=",
                player(2).0.simple()
            ),
        )
        .await;
    }
    assert_eq!(db.admin.block_allowance(player(2).0).await.unwrap(), 5);
    assert_eq!(count(&db, "admin_actions").await, 3);
    db.done().await;
}

/// 範囲外・非数は行にしない。
#[sqlx::test(fixtures("app_role"), fixtures("../ops/app_privileges.sql"))]
async fn an_unusable_allowance_is_refused_before_it_becomes_a_row(owner: PgPool) {
    let db = AppDb::connect(&owner).await;
    seed_cast(&db, 2).await;
    let app = db.router();
    for (index, invalid) in ["-1", "100", "", "three"].into_iter().enumerate() {
        let (status, page) = post_form(
            &app,
            player(ADMIN),
            "/admin/blocks",
            &format!(
                "player={}&key=blocks-{index}&allowance={invalid}&reason=",
                player(2).0.simple()
            ),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{invalid}");
        assert!(
            page.contains("ブロック枠は 0〜99 の整数"),
            "{invalid}: {page}"
        );
    }
    assert_eq!(count(&db, "admin_actions").await, 0);
    assert_eq!(
        db.admin.block_allowance(player(2).0).await.unwrap(),
        DEFAULT_BLOCK_ALLOWANCE
    );
    db.done().await;
}

// ----------------------------------------------------------------------
// 権限: 管理者リストとの照合だけが正
// ----------------------------------------------------------------------

/// 管理者でないセッションからは、どの運用操作もできない（404 で、存在も教えない）。
#[sqlx::test(fixtures("app_role"), fixtures("../ops/app_privileges.sql"))]
async fn a_non_admin_cannot_touch_any_of_the_operational_controls(owner: PgPool) {
    let db = AppDb::connect(&owner).await;
    seed_cast(&db, 2).await;
    enqueue(&db, player(2), "jungle", at(0)).await;
    let app = db.router();

    let target = player(2).0.simple().to_string();
    for (path, body) in [
        (
            "/admin/matchmaker",
            "action=pause&key=k1&reason=".to_owned(),
        ),
        (
            "/admin/queue/remove",
            format!("player={target}&key=k2&reason="),
        ),
        (
            "/admin/blocks",
            format!("player={target}&key=k3&allowance=9&reason="),
        ),
    ] {
        let (status, _) = post_form(&app, player(2), path, &body).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{path}");
    }
    let (status, _) = get_html(&app, player(2), "/admin/queue").await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    // 何も起きていない
    assert_eq!(count(&db, "admin_actions").await, 0);
    assert_eq!(count(&db, "queue_entries").await, 1);
    assert!(db.admin.matchmaker_pause().await.unwrap().is_none());
    assert_eq!(
        db.admin.block_allowance(player(2).0).await.unwrap(),
        DEFAULT_BLOCK_ALLOWANCE
    );
    db.done().await;
}

/// 知らない操作名は 404。画面に無い操作を作らせない。
#[sqlx::test(fixtures("app_role"), fixtures("../ops/app_privileges.sql"))]
async fn an_unknown_matchmaker_action_is_not_found(owner: PgPool) {
    let db = AppDb::connect(&owner).await;
    seed_cast(&db, 0).await;
    let (status, _) = post_form(
        &db.router(),
        player(ADMIN),
        "/admin/matchmaker",
        "action=stop&key=k1&reason=",
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(count(&db, "admin_actions").await, 0);
    db.done().await;
}

/// 運営操作ログは追記専用。アプリ用ロールは UPDATE / DELETE で 42501 になる。
#[sqlx::test(fixtures("app_role"), fixtures("../ops/app_privileges.sql"))]
async fn the_operation_log_is_append_only_for_the_app_role(owner: PgPool) {
    let db = AppDb::connect(&owner).await;
    seed_cast(&db, 0).await;
    pause_from_the_panel(&db.router(), "権限の確認").await;

    for sql in [
        "update admin_actions set reason = 'rewritten'",
        "delete from admin_actions",
    ] {
        let error = sqlx::query(sqlx::AssertSqlSafe(sql.to_owned()))
            .execute(&db.pool)
            .await
            .unwrap_err();
        assert_eq!(
            error.as_database_error().unwrap().code().as_deref(),
            Some("42501"),
            "{sql}"
        );
    }
    assert_eq!(count(&db, "admin_actions").await, 1);
    db.done().await;
}

/// レビュー用に運用画面を吐く。`cargo test --test admin_ops -- --ignored` で使う。
#[sqlx::test(fixtures("app_role"), fixtures("../ops/app_privileges.sql"))]
#[ignore = "レビュー用の出力。通常のテスト実行では回さない"]
async fn dump_the_operational_pages(owner: PgPool) {
    let db = AppDb::connect(&owner).await;
    seed_cast(&db, 3).await;
    for n in 2..=4u128 {
        enqueue(
            &db,
            player(n),
            ROLES[(n as usize - 2) % ROLES.len()],
            at(-600),
        )
        .await;
    }
    let app = db.router();
    let (_, running) = get_html(&app, player(ADMIN), "/admin/queue").await;
    println!("===== /admin/queue（稼働中）\n{running}");
    pause_from_the_panel(&app, "VCが落ちたため").await;
    let (_, paused) = get_html(&app, player(ADMIN), "/admin/queue").await;
    println!("===== /admin/queue（停止中）\n{paused}");
    let (_, search) = get_html(&app, player(ADMIN), "/admin").await;
    println!("===== /admin\n{search}");
    let (_, player_page) = get_html(
        &app,
        player(ADMIN),
        &format!("/admin/player?player={}", player(2).0),
    )
    .await;
    println!("===== /admin/player\n{player_page}");
    post_form(
        &app,
        player(ADMIN),
        "/admin/queue/remove",
        &format!(
            "player={}&key=dump&reason=通信不良のため",
            player(3).0.simple()
        ),
    )
    .await;
    let (_, removed) = get_html(&app, player(3), "/queue").await;
    println!("===== /queue（外された本人）\n{removed}");
    db.done().await;
}

// ----------------------------------------------------------------------
// 運営による報告の入力
// ----------------------------------------------------------------------

/// 10 人の formation を `in_progress` まで進める（参加者は player(2)..player(11)）。
async fn playable_formation(db: &AppDb, n: u128) {
    let fid = FormationId(Uuid::from_u128(0xf00 + n));
    let players: Vec<NewFormationPlayer> = (0..10u128)
        .map(|i| NewFormationPlayer {
            player_id: player(i + 2),
            team: if i < 5 { Team::Red } else { Team::Blue },
            role: ROLES[(i % 5) as usize].to_owned(),
            role_was_flexed: false,
            mu_at_match: 25.0,
            sigma_at_match: 8.3333,
        })
        .collect();
    db.formations
        .create_formation(&NewFormation {
            id: fid,
            created_at: at(0),
            matchmaking_params_version: 3,
            predicted_win_prob: 0.5,
            players,
        })
        .await
        .unwrap();
    let host = player(2);
    for event in [
        NewEvent::vc_assigned("1", 101, 102, at(0), format!("vc-{n}")),
        NewEvent::new(K::HostAssigned, Some(host), at(1), format!("as-{n}")),
        NewEvent::new(K::HostAccepted, Some(host), at(2), format!("ac-{n}")),
        NewEvent::code_posted(host, "1234", at(3), format!("code-{n}")),
    ] {
        db.formations.append_event(fid, &event).await.unwrap();
    }
}

fn fid(n: u128) -> FormationId {
    FormationId(Uuid::from_u128(0xf00 + n))
}

/// プレイヤー本人の報告を 1 件入れる。
async fn player_report(db: &AppDb, n: u128, who: u128, outcome: &str) {
    let outcome = match outcome {
        "red" => Outcome::Win(Team::Red),
        "blue" => Outcome::Win(Team::Blue),
        _ => Outcome::Nullified,
    };
    db.formations
        .append_latest_report(NewReport {
            formation_id: fid(n),
            player_id: player(who),
            reported_at: at(10),
            outcome,
            raw_result: json!({ "source": "web", "choice": outcome.as_str() }),
            idempotency_key: format!("player-{n}-{who}"),
            supersedes_id: None,
        })
        .await
        .unwrap();
}

async fn state_of(db: &AppDb, n: u128) -> S {
    db.formations
        .load_events(fid(n))
        .await
        .unwrap()
        .snapshot()
        .state
}

/// 画面のその行のフォームから、そのプレイヤーの冪等キーを読む。
fn report_key(html: &str, player_id: PlayerId) -> String {
    html.split(&format!("name=\"player\" value=\"{}\"", player_id.0))
        .nth(1)
        .expect("the player row is rendered")
        .split("name=\"key\" value=\"")
        .nth(1)
        .expect("the hidden idempotency key")
        .split('"')
        .next()
        .expect("a key value")
        .to_owned()
}

/// 運営の報告を送る。
async fn enter_report(
    app: &Router,
    page: &str,
    n: u128,
    who: PlayerId,
    outcome: &str,
) -> (StatusCode, String) {
    let key = report_key(page, who);
    post_form(
        app,
        player(ADMIN),
        "/admin/reports",
        &format!(
            "formation={}&player={}&outcome={outcome}&reason=Discordで裁定&key={key}",
            fid(n).0,
            who.0,
        ),
    )
    .await
}

/// 画面にその人の入力欄が出ているか。
fn listed(page: &str, who: PlayerId) -> bool {
    page.contains(&format!("name=\"player\" value=\"{}\"", who.0))
}

/// **運営の報告はプレイヤーの報告と区別でき、確定判定は通常どおり動く。**
#[sqlx::test(fixtures("app_role"), fixtures("../ops/app_privileges.sql"))]
async fn a_staff_report_is_appended_and_settles_the_match(owner: PgPool) {
    let db = AppDb::connect(&owner).await;
    seed_cast(&db, 10).await;
    playable_formation(&db, 1).await;
    let app = db.router();

    // 9 人が red で報告。10 人目（player(11)）だけ未報告
    for who in 2..=10u128 {
        player_report(&db, 1, who, "red").await;
    }
    assert!(
        matches!(state_of(&db, 1).await, S::InProgress { .. }),
        "まだ確定していないこと"
    );

    // 画面には未報告の 1 人だけが出る
    let (status, page) = get_html(&app, player(ADMIN), "/admin/reports").await;
    assert_eq!(status, StatusCode::OK);
    assert!(listed(&page, player(11)), "未報告の人が出ていない");
    for reported in 2..=10u128 {
        assert!(
            !listed(&page, player(reported)),
            "報告済みの player {reported} が一覧に出ている"
        );
    }
    println!("未報告の1人だけが一覧に出る（報告済みの9人は出ない）");

    let (status, after) = enter_report(&app, &page, 1, player(11), "red").await;
    assert_eq!(status, StatusCode::OK);
    assert!(after.contains("結果が確定しました"), "{after}");

    // **確定している。**確定条件は変えていない（全員一致 + 全員報告）
    assert_eq!(state_of(&db, 1).await, S::Confirmed);

    // 運営の報告とプレイヤーの報告が区別できる
    let rows: Vec<(Uuid, String, Option<String>)> = sqlx::query_as(
        "select player_id, outcome, raw_result->>'source' as source \
         from match_reports order by id",
    )
    .fetch_all(&db.pool)
    .await
    .unwrap();
    let staff: Vec<&(Uuid, String, Option<String>)> = rows
        .iter()
        .filter(|row| row.2.as_deref() == Some("staff"))
        .collect();
    println!("=== match_reports（raw_result->>'source' で区別できる）");
    println!(
        "全 {} 件 / うち運営が入れたもの {} 件",
        rows.len(),
        staff.len()
    );
    for row in &staff {
        println!("  player={} outcome={} source={:?}", row.0, row.1, row.2);
    }
    assert_eq!(rows.len(), 10);
    assert_eq!(staff.len(), 1);
    assert_eq!(staff[0].0, player(11).0);

    // 誰が入れたかも残っている
    let actions: Vec<(String, Option<Uuid>, i64, Value)> = sqlx::query_as(
        "select kind, target_player_id, actor_discord_user_id, payload \
         from admin_actions where kind = 'report_entered' order by id",
    )
    .fetch_all(&db.pool)
    .await
    .unwrap();
    println!("=== admin_actions");
    for action in &actions {
        println!(
            "  kind={} target={:?} actor={} payload={}",
            action.0, action.1, action.2, action.3
        );
    }
    assert_eq!(actions.len(), 1);
    assert_eq!(actions[0].1, Some(player(11).0));
    assert_eq!(actions[0].2, ADMIN_ID);

    // レートも通常の経路と同じく導出される
    let deltas: Vec<(Uuid, f64)> = sqlx::query_as(
        "select player_id, delta from player_match_rating_deltas order by delta desc",
    )
    .fetch_all(&db.pool)
    .await
    .unwrap();
    assert_eq!(deltas.len(), 10);
    assert_eq!(deltas[0].1, 7.0);
    assert_eq!(deltas[9].1, -7.0);
    println!(
        "レートの導出: +7 が {} 人 / -7 が {} 人",
        deltas.iter().filter(|row| row.1 > 0.0).count(),
        deltas.iter().filter(|row| row.1 < 0.0).count()
    );

    db.done().await;
}

/// **報告済みのプレイヤーは上書きできない。**
#[sqlx::test(fixtures("app_role"), fixtures("../ops/app_privileges.sql"))]
async fn a_reported_player_cannot_be_overwritten_by_staff(owner: PgPool) {
    let db = AppDb::connect(&owner).await;
    seed_cast(&db, 10).await;
    playable_formation(&db, 1).await;
    let app = db.router();

    player_report(&db, 1, 2, "red").await;
    let before: i64 = sqlx::query_scalar("select count(*) from match_reports")
        .fetch_one(&db.pool)
        .await
        .unwrap();

    // 画面には出ない
    let (status, page) = get_html(&app, player(ADMIN), "/admin/reports").await;
    assert_eq!(status, StatusCode::OK);
    assert!(!listed(&page, player(2)), "報告済みの人が一覧に出ている");

    // 直接 POST しても通らない
    let (status, after) = post_form(
        &app,
        player(ADMIN),
        "/admin/reports",
        &format!(
            "formation={}&player={}&outcome=blue&reason=上書き&key=force-1",
            fid(1).0,
            player(2).0,
        ),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::CONFLICT,
        "上書きの試みが 409 で断られていない"
    );
    assert!(after.contains("既に報告しています"), "{after}");
    println!("報告済みの人への上書き: 409 で拒否された");

    // **行は増えていない。**元の報告も書き換わっていない
    let rows: i64 = sqlx::query_scalar("select count(*) from match_reports")
        .fetch_one(&db.pool)
        .await
        .unwrap();
    assert_eq!(rows, before);
    let outcome: String =
        sqlx::query_scalar("select outcome from match_reports where player_id = $1")
            .bind(player(2).0)
            .fetch_one(&db.pool)
            .await
            .unwrap();
    assert_eq!(outcome, "red", "プレイヤーの報告が書き換わっている");
    println!("player(2) の報告は red のまま / match_reports の行数は {rows}");

    // 操作ログも増えない
    let actions: i64 =
        sqlx::query_scalar("select count(*) from admin_actions where kind = 'report_entered'")
            .fetch_one(&db.pool)
            .await
            .unwrap();
    assert_eq!(actions, 0);

    db.done().await;
}

/// **係争中（disputed）の試合に運営が入れたときの挙動。**現在の姿を記録する（修正しない）。
#[sqlx::test(fixtures("app_role"), fixtures("../ops/app_privileges.sql"))]
async fn what_happens_when_staff_reports_into_a_disputed_match(owner: PgPool) {
    let db = AppDb::connect(&owner).await;
    seed_cast(&db, 10).await;
    playable_formation(&db, 1).await;
    let app = db.router();

    // 8 人が red、1 人が blue。割れたまま disputed にする
    for who in 2..=9u128 {
        player_report(&db, 1, who, "red").await;
    }
    player_report(&db, 1, 10, "blue").await;
    db.formations
        .append_event(
            fid(1),
            &NewEvent::new(K::Disputed, None, at(20), "disputed-1")
                .with_payload(json!({ "outcome": "disputed" })),
        )
        .await
        .unwrap();
    assert_eq!(state_of(&db, 1).await, S::Disputed);

    // **係争中は一覧に出ない。**報告が割れたことが原因なので、未報告者ぶんを
    // 足しても状態は動かない（下で実測する）。出ていること自体が誤解を生む
    let (_, page) = get_html(&app, player(ADMIN), "/admin/reports").await;
    assert!(
        !listed(&page, player(11)),
        "係争中の未報告者が一覧に出ている"
    );
    println!("係争中の試合の未報告者が入力欄に出るか: false");

    // **エンドポイント自体も塞ぐ。**直接 POST しても 409 で断る
    let before: i64 = sqlx::query_scalar("select count(*) from match_reports")
        .fetch_one(&db.pool)
        .await
        .unwrap();
    let (status, after) = post_form(
        &app,
        player(ADMIN),
        "/admin/reports",
        &format!(
            "formation={}&player={}&outcome=red&reason=直接POST&key=disputed-direct",
            fid(1).0,
            player(11).0,
        ),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::CONFLICT,
        "係争中が 409 で断られていない"
    );
    assert!(after.contains("係争中です"), "{after}");
    assert!(
        after.contains("Discord の係争チャンネルで対応してください"),
        "{after}"
    );

    // **match_reports の行は増えていない。**
    let rows: i64 = sqlx::query_scalar("select count(*) from match_reports")
        .fetch_one(&db.pool)
        .await
        .unwrap();
    assert_eq!(rows, before, "係争中なのに報告が積まれている");

    // 断った事実は admin_actions に残る
    let rejected: Vec<(Option<Uuid>, i64, Value)> = sqlx::query_as(
        "select target_player_id, actor_discord_user_id, payload \n         from admin_actions where kind = 'report_rejected' order by id",
    )
    .fetch_all(&db.pool)
    .await
    .unwrap();
    println!("=== admin_actions（report_rejected）");
    for row in &rejected {
        println!("  target={:?} actor={} payload={}", row.0, row.1, row.2);
    }
    assert_eq!(rejected.len(), 1);
    assert_eq!(rejected[0].0, Some(player(11).0));
    assert_eq!(rejected[0].2["state"], "disputed");

    let state = state_of(&db, 1).await;
    let kinds: Vec<&str> = db
        .formations
        .load_events(fid(1))
        .await
        .unwrap()
        .events
        .iter()
        .map(|stored| stored.event.kind.as_str())
        .collect();
    let staff: i64 = sqlx::query_scalar(
        "select count(*) from match_reports where raw_result->>'source' = 'staff'",
    )
    .fetch_one(&db.pool)
    .await
    .unwrap();
    println!("=== 係争中に運営が報告を入れようとした結果");
    println!("HTTP: 409 Conflict");
    println!("状態: {state:?}（動いていない）");
    println!("イベント列: {kinds:?}");
    println!("match_reports の行数: {rows}（送信前と同じ）");
    println!("運営が積めた報告の行数: {staff}");

    db.done().await;
}

/// **係争中の試合は未報告者の一覧から外れ、裁定できる一覧に出る。**
///
/// 件数が 15 分サマリの「係争対応待ち」と一致することも同時に測る。
#[sqlx::test(fixtures("app_role"), fixtures("../ops/app_privileges.sql"))]
async fn a_disputed_match_leaves_the_entry_list_and_appears_read_only(owner: PgPool) {
    let db = AppDb::connect(&owner).await;
    seed_cast(&db, 10).await;
    // 1 件は普通に報告待ち、2 件目は係争中にする
    playable_formation(&db, 1).await;
    playable_formation(&db, 2).await;
    let app = db.router();

    // formation 1: 9 人が報告して 1 人が未報告（一覧に出る側）
    for who in 2..=10u128 {
        player_report(&db, 1, who, "red").await;
    }
    // formation 2: 報告が割れたまま disputed（一覧から外れる側）
    for who in 2..=9u128 {
        player_report(&db, 2, who, "red").await;
    }
    player_report(&db, 2, 10, "blue").await;
    db.formations
        .append_event(
            fid(2),
            &NewEvent::new(K::Disputed, None, at(20), "disputed-2")
                .with_payload(json!({ "outcome": "disputed" })),
        )
        .await
        .unwrap();
    assert_eq!(state_of(&db, 2).await, S::Disputed);

    let (status, page) = get_html(&app, player(ADMIN), "/admin/reports").await;
    assert_eq!(status, StatusCode::OK);

    // **入力欄が出るのは formation 1 の未報告者だけ。**
    // 係争中の formation 2 の未報告者（player(11)）には入力欄が無い
    let entry_forms = page.matches("name=\"player\" value=\"").count();
    assert_eq!(entry_forms, 1, "入力欄の数が違う: {entry_forms}");
    assert!(
        page.contains(&format!("match-{}", &fid(1).0.simple().to_string()[..8])),
        "報告待ちの試合が出ていない"
    );
    println!("入力欄が出る試合: 1 件（報告待ちのみ）");

    // 係争中の試合は**裁定できる枠**に出る
    assert!(page.contains("係争中の試合（1件）"), "{page}");
    assert!(
        page.contains("裁定しないと、その試合で未報告の人はキューに入れないままになります"),
        "{page}"
    );
    let disputed_section = page
        .split("id=\"disputed-title\"")
        .nth(1)
        .expect("the disputed section");
    // **裁定の 4 択がそろっている。**レートが動くかどうかも書いてある
    for label in [
        "赤の勝ちで確定",
        "青の勝ちで確定",
        "無効試合として確定",
        "不成立（記録から外す）",
    ] {
        assert!(
            disputed_section.contains(label),
            "{label}: {disputed_section}"
        );
    }
    assert!(
        disputed_section.contains("（レートが動きます）"),
        "{disputed_section}"
    );
    assert!(
        disputed_section.contains("（レートは動きません）"),
        "{disputed_section}"
    );
    assert!(
        disputed_section.contains("action=\"/admin/reports/dispute\""),
        "{disputed_section}"
    );
    // 理由は必須
    assert!(
        disputed_section.contains("name=\"reason\""),
        "{disputed_section}"
    );
    println!("係争中の枠: 4 択の裁定フォームが出ている");

    // 誰が何を報告したかが読める
    assert!(disputed_section.contains("赤の勝ち"), "{disputed_section}");
    assert!(disputed_section.contains("青の勝ち"), "{disputed_section}");
    assert!(disputed_section.contains("未報告"), "{disputed_section}");
    let votes = db.admin.disputed_formations().await.unwrap();
    assert_eq!(votes.len(), 1);
    let breakdown = &votes[0];
    let red = breakdown
        .votes
        .iter()
        .filter(|vote| vote.outcome.as_deref() == Some("red"))
        .count();
    let blue = breakdown
        .votes
        .iter()
        .filter(|vote| vote.outcome.as_deref() == Some("blue"))
        .count();
    let unreported = breakdown
        .votes
        .iter()
        .filter(|vote| vote.outcome.is_none())
        .count();
    println!("係争の内訳: 赤 {red} / 青 {blue} / 未報告 {unreported}");
    assert_eq!((red, blue, unreported), (8, 1, 1));

    // **15 分サマリの「係争対応待ち」と件数が一致する。**
    let summary = db.matchmaker(2).summary(Utc::now()).await.expect("summary");
    println!(
        "15分サマリの係争対応待ち: {} 件 / 一覧の件数: {} 件",
        summary.active.awaiting_resolution,
        votes.len()
    );
    assert_eq!(summary.active.awaiting_resolution, votes.len());

    db.done().await;
}

/// **確定済み・無効試合・不成立も 409。**どれも `match_reports` に積まない。
#[sqlx::test(fixtures("app_role"), fixtures("../ops/app_privileges.sql"))]
async fn a_settled_match_refuses_the_staff_report(owner: PgPool) {
    let db = AppDb::connect(&owner).await;
    seed_cast(&db, 10).await;
    let app = db.router();

    // 3 件を、確定 / 無効試合 / 不成立 にする
    for (n, kind, outcome) in [
        (1u128, K::Confirmed, "red"),
        (2, K::Nullified, "nullified"),
        (3, K::Voided, "voided"),
    ] {
        playable_formation(&db, n).await;
        db.formations
            .append_event(
                fid(n),
                &NewEvent::new(kind, None, at(20), format!("fin-{n}"))
                    .with_payload(json!({ "outcome": outcome })),
            )
            .await
            .unwrap();
    }

    let before: i64 = sqlx::query_scalar("select count(*) from match_reports")
        .fetch_one(&db.pool)
        .await
        .unwrap();
    assert_eq!(before, 0);

    for (n, expected) in [
        (1u128, "もう結果が出ています"),
        (2, "もう結果が出ています"),
        (3, "不成立で終わっています"),
    ] {
        let (status, after) = post_form(
            &app,
            player(ADMIN),
            "/admin/reports",
            &format!(
                "formation={}&player={}&outcome=red&reason=直接POST&key=settled-{n}",
                fid(n).0,
                player(11).0,
            ),
        )
        .await;
        assert_eq!(status, StatusCode::CONFLICT, "formation {n} が 409 でない");
        assert!(after.contains(expected), "formation {n}: {after}");
        println!("formation {n}: 409 / 「{expected}」");
    }

    // **1 行も積まれていない。**
    let rows: i64 = sqlx::query_scalar("select count(*) from match_reports")
        .fetch_one(&db.pool)
        .await
        .unwrap();
    assert_eq!(rows, before, "確定済みなのに報告が積まれている");
    println!("match_reports の行数: {rows}（3 回断っても増えない）");

    // 断った記録は 3 件残る
    let rejected: Vec<(Option<Uuid>, Value)> = sqlx::query_as(
        "select target_player_id, payload from admin_actions \
         where kind = 'report_rejected' order by id",
    )
    .fetch_all(&db.pool)
    .await
    .unwrap();
    assert_eq!(rejected.len(), 3);
    let states: Vec<&str> = rejected
        .iter()
        .map(|row| row.1["state"].as_str().unwrap())
        .collect();
    assert_eq!(states, vec!["confirmed", "nullified", "voided"]);
    println!("admin_actions の report_rejected: {states:?}");
    // 追記した側（report_entered）は 1 件も無い
    let entered: i64 =
        sqlx::query_scalar("select count(*) from admin_actions where kind = 'report_entered'")
            .fetch_one(&db.pool)
            .await
            .unwrap();
    assert_eq!(entered, 0);

    db.done().await;
}

/// **通常の入力（`in_progress`）が回帰していないこと。**
#[sqlx::test(fixtures("app_role"), fixtures("../ops/app_privileges.sql"))]
async fn the_normal_entry_path_still_works(owner: PgPool) {
    let db = AppDb::connect(&owner).await;
    seed_cast(&db, 10).await;
    playable_formation(&db, 1).await;
    let app = db.router();

    // 9 人が報告済み、1 人が未報告（in_progress のまま）
    for who in 2..=10u128 {
        player_report(&db, 1, who, "red").await;
    }
    assert!(matches!(state_of(&db, 1).await, S::InProgress { .. }));

    let (status, page) = get_html(&app, player(ADMIN), "/admin/reports").await;
    assert_eq!(status, StatusCode::OK);
    assert!(listed(&page, player(11)));

    let (status, after) = enter_report(&app, &page, 1, player(11), "red").await;
    assert_eq!(status, StatusCode::OK, "通常の入力が 200 で通らない");
    assert!(after.contains("結果が確定しました"), "{after}");
    assert_eq!(state_of(&db, 1).await, S::Confirmed);

    let rows: i64 = sqlx::query_scalar("select count(*) from match_reports")
        .fetch_one(&db.pool)
        .await
        .unwrap();
    assert_eq!(rows, 10, "運営の報告が積まれていない");
    let entered: i64 =
        sqlx::query_scalar("select count(*) from admin_actions where kind = 'report_entered'")
            .fetch_one(&db.pool)
            .await
            .unwrap();
    let rejected: i64 =
        sqlx::query_scalar("select count(*) from admin_actions where kind = 'report_rejected'")
            .fetch_one(&db.pool)
            .await
            .unwrap();
    assert_eq!((entered, rejected), (1, 0));
    println!(
        "通常の入力: HTTP 200 / match_reports {rows} 行 / report_entered {entered} 件 / report_rejected {rejected} 件"
    );

    db.done().await;
}

// ----------------------------------------------------------------------
// 係争の裁定
// ----------------------------------------------------------------------
//
// 係争にはこれまで解決経路が無く、運営チャンネルへ通知が飛ぶだけだった。
// 残ったままだと、その試合で未報告のプレイヤーが**永久にキューへ入れない**。

/// レートを読む（表示スケール）。裁定がレートに入ったかを実測する。
async fn rating_of(db: &AppDb, who: PlayerId) -> f64 {
    sqlx::query_scalar!(
        "select rating as \"rating!\" from player_current_ratings where player_id = $1",
        who.0
    )
    .fetch_one(&db.pool)
    .await
    .unwrap()
}

/// 10 人ぶんのレートをまとめて読む。
async fn ratings(db: &AppDb) -> Vec<f64> {
    let mut all = Vec::new();
    for who in 2..=11u128 {
        all.push(rating_of(db, player(who)).await);
    }
    all
}

/// 係争中の試合を 1 件作る。8 人が red、1 人が blue、1 人が未報告。
async fn disputed_formation(db: &AppDb, n: u128) {
    playable_formation(db, n).await;
    for who in 2..=9u128 {
        player_report(db, n, who, "red").await;
    }
    player_report(db, n, 10, "blue").await;
    db.formations
        .append_event(
            fid(n),
            &NewEvent::new(K::Disputed, None, at(20), format!("disputed-{n}"))
                .with_payload(json!({ "outcome": "disputed" })),
        )
        .await
        .unwrap();
    assert_eq!(state_of(db, n).await, S::Disputed);
}

/// **勝敗で裁定すると確定し、レートが動く。**報告は 1 行も書き換わらない。
#[sqlx::test(fixtures("app_role"), fixtures("../ops/app_privileges.sql"))]
async fn adjudicating_a_win_confirms_the_match_and_moves_the_ratings(owner: PgPool) {
    let db = AppDb::connect(&owner).await;
    seed_cast(&db, 11).await;
    disputed_formation(&db, 1).await;
    let app = db.router();

    let reports_before = count(&db, "match_reports").await;
    let before = ratings(&db).await;
    println!("裁定前のレート: {before:?}");

    let (_, page) = get_html(&app, player(ADMIN), "/admin/reports").await;
    let key = form_key(&page, "/admin/reports/dispute");
    let (status, after_page) = post_form(
        &app,
        player(ADMIN),
        "/admin/reports/dispute",
        &format!(
            "formation={}&decision=red&reason=recorded&key={key}",
            fid(1).0
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(after_page.contains("裁定を記録しました"), "{after_page}");
    assert!(
        after_page.contains("この試合の増減がレートに入りました"),
        "{after_page}"
    );

    // 状態が confirmed になり、係争の一覧から消える
    assert_eq!(state_of(&db, 1).await, S::Confirmed);
    assert!(db.admin.disputed_formations().await.unwrap().is_empty());

    // **報告は 1 行も増えていないし、書き換わってもいない**
    assert_eq!(
        count(&db, "match_reports").await,
        reports_before,
        "報告の行数が変わった"
    );

    // レートが動いた。参加した 10 人全員
    let after = ratings(&db).await;
    println!("裁定後のレート: {after:?}");
    let moved = before
        .iter()
        .zip(&after)
        .filter(|(b, a)| (**b - **a).abs() > 0.5)
        .count();
    assert_eq!(moved, 10, "10 人全員のレートが動いていない");

    // 運営操作ログに残る
    let logged =
        sqlx::query!("select reason, payload from admin_actions where kind = 'dispute_resolved'")
            .fetch_all(&db.pool)
            .await
            .unwrap();
    assert_eq!(logged.len(), 1);
    assert_eq!(logged[0].reason.as_deref(), Some("recorded"));
    assert_eq!(logged[0].payload["decision"], json!("red"));
    assert_eq!(logged[0].payload["changes_ratings"], json!(true));
    println!("admin_actions: {} 行", logged.len());

    db.done().await;
}

/// **不成立で裁定するとレートは動かず、詰まっていた人のキューが開く。**
#[sqlx::test(fixtures("app_role"), fixtures("../ops/app_privileges.sql"))]
async fn voiding_a_dispute_releases_the_player_who_never_reported(owner: PgPool) {
    let db = AppDb::connect(&owner).await;
    seed_cast(&db, 11).await;
    disputed_formation(&db, 1).await;
    let app = db.router();

    // player(11) だけが未報告。**disputed のあいだは報告ゲートで塞がれている**
    let stuck = player(11);
    let before = db.registration.queue_eligibility(stuck).await.unwrap();
    assert!(
        !before.pending_reports.is_empty(),
        "塞がっていない: {before:?}"
    );
    println!(
        "裁定前: pending_reports={} 件",
        before.pending_reports.len()
    );

    let ratings_before = ratings(&db).await;

    let (_, page) = get_html(&app, player(ADMIN), "/admin/reports").await;
    let key = form_key(&page, "/admin/reports/dispute");
    let (status, after_page) = post_form(
        &app,
        player(ADMIN),
        "/admin/reports/dispute",
        &format!(
            "formation={}&decision=voided&reason=no+room&key={key}",
            fid(1).0
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        after_page.contains("レートは動いていません"),
        "{after_page}"
    );
    assert_eq!(state_of(&db, 1).await, S::Voided);

    // **キューが開いた**
    let after = db.registration.queue_eligibility(stuck).await.unwrap();
    assert!(
        after.pending_reports.is_empty(),
        "まだ塞がっている: {after:?}"
    );
    println!("裁定後: pending_reports={} 件", after.pending_reports.len());

    // レートは 1 人も動いていない
    assert_eq!(ratings_before, ratings(&db).await, "レートが動いた");

    db.done().await;
}

/// **同じフォームを 2 回送っても 1 行しか積まない。**判定は冪等キーだけで行う。
#[sqlx::test(fixtures("app_role"), fixtures("../ops/app_privileges.sql"))]
async fn resolving_the_same_dispute_twice_records_one_row(owner: PgPool) {
    let db = AppDb::connect(&owner).await;
    seed_cast(&db, 11).await;
    disputed_formation(&db, 1).await;
    let app = db.router();

    let (_, page) = get_html(&app, player(ADMIN), "/admin/reports").await;
    let key = form_key(&page, "/admin/reports/dispute");
    let body = format!(
        "formation={}&decision=nullified&reason=double+submit&key={key}",
        fid(1).0
    );
    let (_, first) = post_form(&app, player(ADMIN), "/admin/reports/dispute", &body).await;
    assert!(first.contains("裁定を記録しました"), "{first}");
    let (_, second) = post_form(&app, player(ADMIN), "/admin/reports/dispute", &body).await;
    assert!(
        second.contains("既に記録されています"),
        "2 回目が no-op になっていない: {second}"
    );

    assert_eq!(state_of(&db, 1).await, S::Nullified);
    let events = sqlx::query_scalar!(
        "select count(*) as \"count!\" from formation_events \
         where formation_id = $1 and kind = 'nullified'",
        fid(1).0
    )
    .fetch_one(&db.pool)
    .await
    .unwrap();
    assert_eq!(events, 1, "nullified が 2 行積まれた");
    let logged = sqlx::query_scalar!(
        "select count(*) as \"count!\" from admin_actions where kind = 'dispute_resolved'"
    )
    .fetch_one(&db.pool)
    .await
    .unwrap();
    assert_eq!(logged, 1, "操作ログが 2 行積まれた");
    println!("nullified イベント: {events} 行 / dispute_resolved: {logged} 行");

    db.done().await;
}

/// **既に裁定済みの試合には積まない。**別の運営が先に裁定していた場合。
#[sqlx::test(fixtures("app_role"), fixtures("../ops/app_privileges.sql"))]
async fn a_match_that_is_no_longer_disputed_is_refused(owner: PgPool) {
    let db = AppDb::connect(&owner).await;
    seed_cast(&db, 11).await;
    disputed_formation(&db, 1).await;
    let app = db.router();

    // 画面を開いた後に、別の経路で裁定が済んでしまう
    let (_, page) = get_html(&app, player(ADMIN), "/admin/reports").await;
    let key = form_key(&page, "/admin/reports/dispute");
    db.formations
        .append_event(
            fid(1),
            &NewEvent::new(K::Voided, None, at(30), "someone-else")
                .with_payload(json!({ "decision_basis": "staff_adjudication" })),
        )
        .await
        .unwrap();

    let (status, after) = post_form(
        &app,
        player(ADMIN),
        "/admin/reports/dispute",
        &format!("formation={}&decision=red&reason=late&key={key}", fid(1).0),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(after.contains("係争中ではありません"), "{after}");
    assert!(
        after.contains("voided"),
        "現在の状態を書いていない: {after}"
    );
    // 状態は変わっていない
    assert_eq!(state_of(&db, 1).await, S::Voided);
    let logged = sqlx::query_scalar!(
        "select count(*) as \"count!\" from admin_actions where kind = 'dispute_resolved'"
    )
    .fetch_one(&db.pool)
    .await
    .unwrap();
    assert_eq!(logged, 0, "断ったのに操作ログを積んだ");
    println!("係争中でない試合への裁定: 拒否・イベントも操作ログも増えない");

    db.done().await;
}

// ----------------------------------------------------------------------
// 登録者一覧
// ----------------------------------------------------------------------
//
// 検索（/admin）は名前か Discord ID を知っている前提の入口だった。
// 「誰が居るのか」を端から見る場所として別に置く。

/// **一覧は全員を出す。**検索と違って、何も入力しなくても並ぶ。
#[sqlx::test(fixtures("app_role"), fixtures("../ops/app_privileges.sql"))]
async fn the_roster_lists_everyone_without_a_query(owner: PgPool) {
    let db = AppDb::connect(&owner).await;
    // 管理者 1 人 + 参加者 12 人
    seed_cast(&db, 12).await;
    let app = db.router();

    let (status, page) = get_html(&app, player(ADMIN), "/admin/players").await;
    assert_eq!(status, StatusCode::OK);
    let rows = page.matches("/admin/player?player=").count();
    assert_eq!(rows, 13, "全員が出ていない: {rows}");
    assert!(page.contains("13人"), "総人数が出ていない: {page}");
    println!("一覧に出た人数: {rows}");

    // レート・試合数・登録日・Discord ID の列がある
    for header in ["プレイヤー", "レート", "試合", "登録", "Discord ID"] {
        assert!(page.contains(header), "{header}: {page}");
    }

    db.done().await;
}

/// **名前でも Discord ID でも絞り込める。**
#[sqlx::test(fixtures("app_role"), fixtures("../ops/app_privileges.sql"))]
async fn the_roster_can_be_filtered(owner: PgPool) {
    let db = AppDb::connect(&owner).await;
    seed_cast(&db, 12).await;

    // Discord ID は完全一致（参加者 n の ID は 1000 + n）
    let by_id = db.admin.roster("1011", 1).await.unwrap();
    assert_eq!(by_id.total, 1, "Discord ID で 1 人に絞れていない");
    assert_eq!(by_id.players[0].discord_user_id, 1011);

    // 名前は部分一致。全員が「トレーナーN」なので 13 人に当たる
    let by_name = db.admin.roster("トレーナー", 1).await.unwrap();
    assert_eq!(by_name.total, 13, "名前の部分一致が効いていない");

    // 当たらない文字列なら 0 件で、その旨が画面に出る
    let app = db.router();
    let (status, empty) = get_html(&app, player(ADMIN), "/admin/players?q=zzzznotfound").await;
    assert_eq!(status, StatusCode::OK);
    assert!(empty.contains("該当する登録者がいません"), "{empty}");
    println!(
        "絞り込み: ID 1 件 / 名前 {} 件 / 該当なし 0 件",
        by_name.total
    );

    db.done().await;
}

/// **未登録の人を名前で埋めない。**登録が済んでいないことが一覧から読める。
///
/// `seed_cast` は管理者を `register` しないので、管理者がその 1 人になる。
#[sqlx::test(fixtures("app_role"), fixtures("../ops/app_privileges.sql"))]
async fn a_player_who_never_registered_is_marked_as_such(owner: PgPool) {
    let db = AppDb::connect(&owner).await;
    seed_cast(&db, 2).await;
    let app = db.router();

    let roster = db.admin.roster("", 1).await.unwrap();
    let unregistered = roster
        .players
        .iter()
        .filter(|entry| entry.registered_at.is_none())
        .count();
    assert_eq!(unregistered, 1, "未登録が 1 人のはず");
    assert_eq!(roster.total, 3);

    // 未登録でも players 行は残っているので、一覧からは消えない
    let (status, page) = get_html(&app, player(ADMIN), "/admin/players").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(page.matches("/admin/player?player=").count(), 3);
    // 登録済みの人はトレーナー名で出る
    assert!(page.contains("トレーナー2"), "{page}");
    println!("未登録: {unregistered} 人 / 全体: {} 人", roster.total);

    db.done().await;
}

/// **キュー中の人には印が付く。**運営が「いま誰が待っているか」を一覧から読める。
#[sqlx::test(fixtures("app_role"), fixtures("../ops/app_privileges.sql"))]
async fn the_roster_marks_who_is_queued(owner: PgPool) {
    let db = AppDb::connect(&owner).await;
    seed_cast(&db, 12).await;
    let app = db.router();

    let before = db.admin.roster("", 1).await.unwrap();
    assert_eq!(
        before.players.iter().filter(|entry| entry.queued).count(),
        0,
        "誰もキューに入っていないはず"
    );

    enqueue(&db, player(2), "jungle", at(0)).await;
    enqueue(&db, player(3), "top_carry", at(0)).await;

    let after = db.admin.roster("", 1).await.unwrap();
    let queued = after.players.iter().filter(|entry| entry.queued).count();
    assert_eq!(queued, 2, "キュー中の印が合わない");
    let (_, page) = get_html(&app, player(ADMIN), "/admin/players").await;
    assert_eq!(page.matches("キュー中").count(), 2, "{page}");
    println!("キュー中の印: {queued} 人");

    db.done().await;
}
