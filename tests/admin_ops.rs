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
//! - ブロック枠の既定は 2。変更は追記として記録され、誰がいつ変えたかが残る
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
use unite_matchsystem::lifecycle::{PlayerId, Timestamp};
use unite_matchsystem::matchmaker::{
    Matchmaker, MatchmakerConfig, MatchmakerRepository, ROLES, RunOutcome,
};
use unite_matchsystem::persistence::{FormationRepository, VcAssignment};
use unite_matchsystem::queue::model::{QueueRequest, RolePreference};
use unite_matchsystem::queue::{QueueRepository, QueueState, router as queue_router};
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

/// 既定は 2 枠。変更は追記として記録され、誰がいつ変えたかが残る。
#[sqlx::test(fixtures("app_role"), fixtures("../ops/app_privileges.sql"))]
async fn the_block_allowance_defaults_to_two_and_records_the_change(owner: PgPool) {
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
    assert!(page.contains("<dt>ブロック枠</dt><dd>2 枠</dd>"), "{page}");

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
