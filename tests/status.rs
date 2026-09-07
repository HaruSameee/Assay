//! 戦績画面（`GET /status`）の統合テスト。実際の PostgreSQL を使う。
//!
//! リポジトリは所有者ではなく**アプリ用ロール**（`assay_app`）で接続する。
//!
//! 検証すること（不変条件との対応）:
//! - 対象はセッションだけ。**クエリの `player_id` は効かない**（他人の id を入れても
//!   自分のデータしか出ない）— 他人のレート・戦績はどの経路にも出ない
//! - 未確定（in_progress / disputed / nullified）の試合は履歴にも総合にも現れない（00 §3-5）
//! - 履歴は新しい順・最大 50 件。総合成績は全期間
//! - セッションが無ければ 401（登録画面と同じ再ログイン誘導）
//!
//! セッションは `tests/common` の本物のログイン経路（偽 OAuth）で張る。

mod common;

use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use common::{
    APP_ROLE, APP_ROLE_PASSWORD, Browser, FakeOAuth, at, body_text, config, player, sign_in,
};
use sqlx::PgPool;
use sqlx::postgres::PgPoolOptions;
use unite_matchsystem::admin::parse_admin_ids;
use unite_matchsystem::lifecycle::{FormationEventKind as K, FormationId};
use unite_matchsystem::matchmaker::ROLES;
use unite_matchsystem::persistence::{
    FormationRepository, NewEvent, NewFormation, NewFormationPlayer, Team,
};
use unite_matchsystem::rating::RatingAdjustmentRepository;
use unite_matchsystem::registration::{
    AuthState, RegistrationRepository, router as registration_router,
};
use unite_matchsystem::status::{StatusRepository, StatusState, router as status_router};
use uuid::Uuid;

// ----------------------------------------------------------------------
// 道具
// ----------------------------------------------------------------------

fn fid(n: u128) -> FormationId {
    FormationId(Uuid::from_u128(n))
}

struct AppDb {
    pool: PgPool,
    formations: FormationRepository,
    ratings: RatingAdjustmentRepository,
    status: StatusRepository,
    registration: RegistrationRepository,
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
            formations: FormationRepository::new(pool.clone()),
            ratings: RatingAdjustmentRepository::new(pool.clone()),
            status: StatusRepository::new(pool.clone()),
            registration: RegistrationRepository::new(pool.clone()),
            pool,
        }
    }

    async fn done(self) {
        self.pool.close().await;
    }
}

/// 登録ルート（ログイン経路）と戦績ルートを並べたアプリ。
fn app(db: &AppDb, oauth: Arc<FakeOAuth>) -> Router {
    registration_router(AuthState::new(config(), oauth, db.registration.clone())).merge(
        status_router(StatusState::new(
            config(),
            db.ratings.clone(),
            db.status.clone(),
        )),
    )
}

/// `seed_players` が置く行（discord_user_id = 1000 + n）へのログイン。
async fn signed_in(db: &AppDb, oauth: &Arc<FakeOAuth>, discord_user_id: &str) -> Browser {
    oauth.set_user_id(discord_user_id);
    let mut browser = Browser::new(app(db, oauth.clone()));
    let id = sign_in(&mut browser, &db.registration, discord_user_id).await;
    let n: i64 = discord_user_id.parse().expect("numeric discord id");
    assert_eq!(id, player((n - 1000) as u128), "the seeded row is reused");
    browser
}

/// `players` の行を用意する（discord_user_id は 1000 + n）。
async fn seed_players(db: &AppDb, count: u128) {
    for n in 1..=count {
        sqlx::query(
            "insert into players (id, discord_user_id, created_at) values ($1, $2, now()) \
             on conflict do nothing",
        )
        .bind(player(n).0)
        .bind(1000 + n as i64)
        .execute(&db.pool)
        .await
        .expect("seed a player");
    }
}

/// players 1〜5 が red、6〜10 が blue。レートは表示スケール。
fn formation_at(n: u128, red_display: f64, blue_display: f64) -> NewFormation {
    NewFormation {
        id: fid(n),
        created_at: at(n as i64),
        matchmaking_params_version: 1,
        predicted_win_prob: 0.5,
        players: (1..=10u128)
            .map(|i| NewFormationPlayer {
                player_id: player(i),
                team: if i <= 5 { Team::Red } else { Team::Blue },
                role: ROLES[(i as usize - 1) % ROLES.len()].to_owned(),
                role_was_flexed: false,
                mu_at_match: if i <= 5 { red_display } else { blue_display } / 60.0,
                sigma_at_match: 500.0 / 60.0,
            })
            .collect(),
    }
}

/// formation を作って `in_progress` まで進める（まだ確定していない）。
async fn play(db: &AppDb, n: u128, red_display: f64, blue_display: f64) {
    db.formations
        .create_formation(&formation_at(n, red_display, blue_display))
        .await
        .expect("create formation");
    let host = player(1);
    for event in [
        NewEvent::vc_assigned("1", 101, 102, at(0), format!("vc-{n}")),
        NewEvent::new(K::HostAssigned, Some(host), at(1), format!("assign-{n}")),
        NewEvent::new(K::HostAccepted, Some(host), at(2), format!("accept-{n}")),
        NewEvent::code_posted(host, "1234", at(3), format!("code-{n}")),
    ] {
        db.formations.append_event(fid(n), &event).await.unwrap();
    }
}

async fn finish(db: &AppDb, n: u128, kind: K, outcome: &str) {
    let event = NewEvent::new(kind, None, at(10), format!("finish-{n}-{}", kind.as_str()))
        .with_payload(serde_json::json!({ "outcome": outcome }));
    db.formations.append_event(fid(n), &event).await.unwrap();
}

async fn confirmed_match(db: &AppDb, n: u128, red_display: f64, blue_display: f64, winner: &str) {
    play(db, n, red_display, blue_display).await;
    finish(db, n, K::Confirmed, winner).await;
}

/// tbody の行（履歴の 1 試合ぶん）を数える。
fn history_rows(page: &str) -> usize {
    page.matches("<tr><td>").count()
}

// ----------------------------------------------------------------------
// 自分の分だけ・確定した分だけ
// ----------------------------------------------------------------------

/// 履歴に出るのは**自分の**確定試合だけ。クエリで他人の id を入れても効かない。
#[sqlx::test(fixtures("app_role"), fixtures("../ops/app_privileges.sql"))]
async fn the_status_page_shows_only_the_sessions_own_confirmed_matches(owner: PgPool) {
    let db = AppDb::connect(&owner).await;
    seed_players(&db, 10).await;
    let oauth = Arc::new(FakeOAuth::new("1001"));

    // 1 試合だけ確定。red 1500 vs blue 1500 で red 勝ち -> p1 +7 / p6 -7（params v2）
    confirmed_match(&db, 1, 1500.0, 1500.0, "red").await;
    // 2 試合目は in_progress のまま（確定していない）
    play(&db, 2, 1500.0, 1500.0).await;

    let mut mine = signed_in(&db, &oauth, "1001").await;
    let page = body_text(mine.get("/status").await).await;
    assert_eq!(history_rows(&page), 1, "未確定の試合は行にならない: {page}");
    assert!(page.contains("1507"), "現在のレート: {page}");
    assert!(page.contains(">+7<"), "増減: {page}");
    assert!(page.contains("勝ち"), "{page}");
    // 日時は JST 表示（21:00 UTC -> 翌日 06:00 JST）
    assert!(page.contains("2026-09-13 06:00"), "{page}");
    // 他人（p6）のレート 1493 は絶対に出ない
    assert!(!page.contains("1493"), "{page}");

    // クエリで他人の player_id を入れても、出るのは自分の分だけ
    let queried = body_text(
        mine.get(&format!(
            "/status?player_id={}&discord_user_id=1006",
            player(6).0.simple()
        ))
        .await,
    )
    .await;
    assert!(queried.contains("1507"), "自分のレートが出る: {queried}");
    assert!(!queried.contains("1493"), "他人のレートは出ない: {queried}");

    // 別セッション（p6）からは自分の分だけ（-7 / 負け / 1493）が見える
    let mut theirs = signed_in(&db, &oauth, "1006").await;
    let other = body_text(theirs.get("/status").await).await;
    assert!(other.contains("1493"), "{other}");
    assert!(other.contains(">-8<") || other.contains(">-7<"), "{other}");
    assert!(other.contains("負け"), "{other}");
    assert!(!other.contains("1511"), "p1 のレートは出ない: {other}");

    db.done().await;
}

/// 未確定の試合（in_progress / disputed / nullified）は履歴にも総合にも現れない。
/// 確定した瞬間に現れる（ビューが `confirmed` を構造的に filter している）。
#[sqlx::test(fixtures("app_role"), fixtures("../ops/app_privileges.sql"))]
async fn unconfirmed_matches_stay_out_of_the_status_page(owner: PgPool) {
    let db = AppDb::connect(&owner).await;
    seed_players(&db, 10).await;
    let oauth = Arc::new(FakeOAuth::new("1001"));

    play(&db, 1, 1500.0, 1600.0).await;
    finish(&db, 1, K::Nullified, "nullified").await;
    play(&db, 2, 1500.0, 1600.0).await;
    finish(&db, 2, K::Disputed, "disputed").await;
    play(&db, 3, 1500.0, 1600.0).await; // in_progress

    let mut browser = signed_in(&db, &oauth, "1001").await;
    let page = body_text(browser.get("/status").await).await;
    assert!(page.contains("まだ確定した試合がありません"), "{page}");
    assert!(!page.contains("<table"), "{page}");
    assert!(page.contains("1500"), "レートは初期値のまま: {page}");

    // スタッフ裁定で disputed -> confirmed。この瞬間から履歴とレートに現れる
    finish(&db, 2, K::Confirmed, "red").await;
    let page = body_text(browser.get("/status").await).await;
    assert_eq!(history_rows(&page), 1, "{page}");
    assert!(page.contains(">+8<"), "格上（1600）に勝って +8: {page}");
    assert!(page.contains("1508"), "{page}");

    db.done().await;
}

/// 履歴は新しい順で最大 50 件。総合成績は全期間を数える。
#[sqlx::test(fixtures("app_role"), fixtures("../ops/app_privileges.sql"))]
async fn the_history_is_newest_first_and_capped_at_fifty(owner: PgPool) {
    let db = AppDb::connect(&owner).await;
    seed_players(&db, 10).await;

    // 51 試合を生 SQL で種を置く（ビューが読むのは formation_players と
    // confirmed イベントだけなので状態機械を通らない）。created_at を 1 分ずつずらす
    for n in 1..=51u128 {
        sqlx::query(
            "insert into formations (id, created_at, matchmaking_params_version, predicted_win_prob) \
             values ($1, $2, 1, 0.5)",
        )
        .bind(fid(0x2000 + n).0)
        .bind(at(n as i64 * 60))
        .execute(&db.pool)
        .await
        .unwrap();
        for slot in 0..10u128 {
            sqlx::query(
                "insert into formation_players \
                 (formation_id, player_id, team, role, role_was_flexed, mu_at_match, sigma_at_match) \
                 values ($1, $2, $3, $4, false, $5, 8.33)",
            )
            .bind(fid(0x2000 + n).0)
            .bind(player(slot + 1).0)
            .bind(if slot < 5 { "red" } else { "blue" })
            .bind(ROLES[(slot % 5) as usize])
            .bind(25.0)
            .execute(&db.pool)
            .await
            .unwrap();
        }
        sqlx::query(
            "insert into formation_events \
             (formation_id, occurred_at, kind, subject_player_id, payload, idempotency_key) \
             values ($1, $2, 'confirmed', null, $3, $4)",
        )
        .bind(fid(0x2000 + n).0)
        .bind(at(n as i64 * 60))
        .bind(serde_json::json!({ "outcome": if n % 2 == 0 { "red" } else { "blue" } }))
        .bind(format!("cap-{n}"))
        .execute(&db.pool)
        .await
        .unwrap();
    }

    let oauth = Arc::new(FakeOAuth::new("1001"));
    let mut browser = signed_in(&db, &oauth, "1001").await;
    let page = body_text(browser.get("/status").await).await;
    assert_eq!(history_rows(&page), 50, "直近 50 件だけ");
    // 総合成績は全期間（51 試合）
    assert!(page.contains(">51<"), "試合数は全期間: {page}");
    // 新しい順: 最新の 51 番目（51 分 = 06:51 JST）が最初、1 番古い 2 分目が最後
    let newest = "2026-09-13 06:51";
    let oldest_shown = "2026-09-13 06:02";
    let first_newest = page.find(newest).expect("newest match first");
    let last_oldest = page.rfind(oldest_shown).expect("second-oldest shown");
    assert!(first_newest < last_oldest, "新しい順で並ぶ");
    // 切られた 1 件目（06:01 JST）は出ない
    assert!(!page.contains("2026-09-13 06:01"), "51 件目は切れる");

    db.done().await;
}

/// セッションが無ければ 401。理由は再ログインに落とす画面を出す。
#[sqlx::test(fixtures("app_role"), fixtures("../ops/app_privileges.sql"))]
async fn the_status_page_requires_a_session(owner: PgPool) {
    let db = AppDb::connect(&owner).await;
    seed_players(&db, 1).await;
    let mut browser = Browser::new(app(&db, Arc::new(FakeOAuth::new("1001"))));

    let response = browser.get("/status").await;
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    let page = body_text(response).await;
    assert!(page.contains("セッションの有効期限が切れました"), "{page}");

    // 署名の通らない Cookie も同じ扱い
    let forged = browser
        .send(
            Request::get("/status")
                .header(header::COOKIE, "assay_session=deadbeef.deadbeef")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
    assert_eq!(forged.status(), StatusCode::UNAUTHORIZED);

    db.done().await;
}

/// 戦績画面も共通ヘッダーを出す。管理者リストに載っていなければ管理の導線は出さない。
#[sqlx::test(fixtures("app_role"), fixtures("../ops/app_privileges.sql"))]
async fn the_status_page_carries_the_shared_header(owner: PgPool) {
    let db = AppDb::connect(&owner).await;
    seed_players(&db, 1).await;
    let oauth = Arc::new(FakeOAuth::new("1001"));

    let mut browser = signed_in(&db, &oauth, "1001").await;
    let page = body_text(browser.get("/status").await).await;
    assert!(page.contains("<header class=\"site-header\">"), "{page}");
    assert!(
        page.contains("<a class=\"brand\" href=\"/queue\">Assay</a>"),
        "{page}"
    );
    assert!(page.contains("action=\"/auth/logout\""), "{page}");
    // 管理者リストを渡していない構成では誰にも出さない
    assert!(!page.contains("href=\"/admin\""), "{page}");

    // 401 の画面は未ログイン扱い。導線を出さない
    let mut anonymous = Browser::new(app(&db, oauth));
    let denied = anonymous.get("/status").await;
    assert_eq!(denied.status(), StatusCode::UNAUTHORIZED);
    let page = body_text(denied).await;
    assert!(!page.contains("href=\"/status\""), "{page}");
    assert!(!page.contains("/auth/logout"), "{page}");
    db.done().await;
}

/// 管理者リストに載っているセッションには、戦績画面のヘッダーにも管理の導線が出る。
#[sqlx::test(fixtures("app_role"), fixtures("../ops/app_privileges.sql"))]
async fn an_admin_sees_the_panel_link_on_the_status_page(owner: PgPool) {
    let db = AppDb::connect(&owner).await;
    seed_players(&db, 1).await;
    let oauth = Arc::new(FakeOAuth::new("1001"));
    let admins = parse_admin_ids(Some("1001")).unwrap();
    let app = registration_router(AuthState::new(
        config(),
        oauth.clone(),
        db.registration.clone(),
    ))
    .merge(status_router(
        StatusState::new(config(), db.ratings.clone(), db.status.clone()).with_admins(admins),
    ));

    let mut browser = Browser::new(app);
    sign_in(&mut browser, &db.registration, "1001").await;
    let page = body_text(browser.get("/status").await).await;
    assert!(page.contains("<a href=\"/admin\">管理</a>"), "{page}");
    db.done().await;
}
