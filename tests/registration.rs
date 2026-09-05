//! 登録・認証トラックの統合テスト。実際の PostgreSQL と、メモリ上の偽 Discord を使う。
//!
//! リポジトリは所有者ではなく**アプリ用ロール**（`assay_app`）で接続する。所有者権限では
//! 権限バグが本番でしか出ない。権限は本番と同じ `ops/app_privileges.sql` を適用して作る。
//!
//! 検証すること:
//! - OAuth: state を検証して即座に破棄する。使い回さない。scope は identify だけ
//! - Cookie: HttpOnly / Secure / SameSite=Lax / Domain 限定。識別子と発行時刻しか入らない
//! - 申告: 再送は毎回追記される（状態ベースの冪等にしない）。現在値は最新行から導出する
//! - `player_declarations` は追記専用（アプリ用ロールの UPDATE / DELETE が 42501）
//! - キューゲート: 登録ゲートと報告ゲートが畳まれず、それぞれの理由で塞ぐ

use std::collections::HashMap;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use axum::Router;
use axum::body::Body;
use axum::http::{Request, Response, StatusCode, header};
use chrono::{TimeDelta, TimeZone, Utc};
use http_body_util::BodyExt;
use serde_json::json;
use serenity::async_trait;
use sqlx::PgPool;
use sqlx::postgres::PgPoolOptions;
use tower::ServiceExt;
use unite_matchsystem::lifecycle::{FormationEventKind as K, FormationId, PlayerId, Timestamp};
use unite_matchsystem::persistence::{
    FormationRepository, NewEvent, NewFormation, NewFormationPlayer, NewReport, Outcome, Team,
};
use unite_matchsystem::registration::{
    AuthConfig, AuthState, DiscordUser, OAuthClient, OAuthError, PlayerStatus, Rank,
    RegistrationBlock, RegistrationRepository, router,
};
use uuid::Uuid;

const APP_ROLE: &str = "assay_app";
const APP_ROLE_PASSWORD: &str = "assay_app_test";
const COOKIE_DOMAIN: &str = "match.example.test";
const SESSION_COOKIE: &str = "assay_session";
const STATE_COOKIE: &str = "assay_oauth_state";
/// マイグレーションが入れる 5 ロール。
const ROLES: [&str; 5] = [
    "top_carry",
    "top_support",
    "jungle",
    "bot_carry",
    "bot_tank",
];

// ----------------------------------------------------------------------
// 道具
// ----------------------------------------------------------------------

/// 偽の Discord。HTTP を出さずに OAuth の応答を差し替える。
struct FakeOAuth {
    user_id: Mutex<String>,
    exchanged_codes: Mutex<Vec<String>>,
    fail: AtomicBool,
}

impl FakeOAuth {
    fn new(user_id: &str) -> Self {
        Self {
            user_id: Mutex::new(user_id.to_owned()),
            exchanged_codes: Mutex::new(Vec::new()),
            fail: AtomicBool::new(false),
        }
    }

    fn codes(&self) -> Vec<String> {
        self.exchanged_codes.lock().unwrap().clone()
    }
}

#[async_trait]
impl OAuthClient for FakeOAuth {
    async fn exchange_code(&self, code: &str) -> Result<String, OAuthError> {
        if self.fail.load(Ordering::SeqCst) {
            return Err(OAuthError::Rejected(401));
        }
        self.exchanged_codes.lock().unwrap().push(code.to_owned());
        Ok(format!("access-token-for-{code}"))
    }

    async fn current_user(&self, _access_token: &str) -> Result<DiscordUser, OAuthError> {
        if self.fail.load(Ordering::SeqCst) {
            return Err(OAuthError::Transport);
        }
        Ok(DiscordUser {
            id: self.user_id.lock().unwrap().clone(),
            username: "handle".into(),
            global_name: Some("表示名".into()),
        })
    }
}

/// テスト DB にアプリ用ロールで接続したプール。
struct AppDb {
    pool: PgPool,
    repo: RegistrationRepository,
    formations: FormationRepository,
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
            repo: RegistrationRepository::new(pool.clone()),
            formations: FormationRepository::new(pool.clone()),
            pool,
        }
    }

    async fn done(self) {
        self.pool.close().await;
    }
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

/// Cookie を持ち回るブラウザもどき。
struct Browser {
    app: Router,
    cookies: HashMap<String, String>,
}

impl Browser {
    fn new(app: Router) -> Self {
        Self {
            app,
            cookies: HashMap::new(),
        }
    }

    fn cookie_header(&self) -> Option<String> {
        if self.cookies.is_empty() {
            return None;
        }
        let mut pairs: Vec<String> = self
            .cookies
            .iter()
            .map(|(name, value)| format!("{name}={value}"))
            .collect();
        pairs.sort();
        Some(pairs.join("; "))
    }

    /// レスポンスの `Set-Cookie` を取り込む。空値 / `Max-Age=0` は削除として扱う。
    fn absorb(&mut self, response: &Response<Body>) {
        for header in response.headers().get_all(header::SET_COOKIE) {
            let Ok(raw) = header.to_str() else { continue };
            let mut parts = raw.split(';');
            let Some((name, value)) = parts.next().and_then(|pair| pair.split_once('=')) else {
                continue;
            };
            let removed = value.is_empty()
                || raw
                    .split(';')
                    .any(|attribute| attribute.trim().eq_ignore_ascii_case("Max-Age=0"));
            if removed {
                self.cookies.remove(name);
            } else {
                self.cookies.insert(name.to_owned(), value.to_owned());
            }
        }
    }

    async fn send(&mut self, request: Request<Body>) -> Response<Body> {
        let response = self.app.clone().oneshot(request).await.unwrap();
        self.absorb(&response);
        response
    }

    async fn get(&mut self, uri: &str) -> Response<Body> {
        let mut builder = Request::get(uri);
        if let Some(cookies) = self.cookie_header() {
            builder = builder.header(header::COOKIE, cookies);
        }
        self.send(builder.body(Body::empty()).unwrap()).await
    }

    async fn post_form(&mut self, uri: &str, body: &str) -> Response<Body> {
        let mut builder =
            Request::post(uri).header(header::CONTENT_TYPE, "application/x-www-form-urlencoded");
        if let Some(cookies) = self.cookie_header() {
            builder = builder.header(header::COOKIE, cookies);
        }
        self.send(builder.body(Body::from(body.to_owned())).unwrap())
            .await
    }
}

async fn body_text(response: Response<Body>) -> String {
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    String::from_utf8(bytes.to_vec()).unwrap()
}

fn location(response: &Response<Body>) -> String {
    response
        .headers()
        .get(header::LOCATION)
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default()
        .to_owned()
}

fn set_cookie_lines(response: &Response<Body>) -> Vec<String> {
    response
        .headers()
        .get_all(header::SET_COOKIE)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .map(str::to_owned)
        .collect()
}

/// 認可 URL のクエリから `state` を取る（Cookie 側は署名済みなので生の値は読めない）。
fn state_from_redirect(response: &Response<Body>) -> String {
    location(response)
        .split("state=")
        .nth(1)
        .expect("state in the authorize url")
        .split('&')
        .next()
        .expect("state value")
        .to_owned()
}

/// ログインを通してセッションを張り、`players.id` を返す。
async fn sign_in(browser: &mut Browser, db: &AppDb, discord_user_id: &str) -> PlayerId {
    let login = browser.get("/auth/login").await;
    assert_eq!(login.status(), StatusCode::SEE_OTHER);
    assert!(
        browser.cookies.contains_key(STATE_COOKIE),
        "state cookie is set"
    );
    let state = state_from_redirect(&login);
    let callback = browser
        .get(&format!("/auth/callback?code=abc&state={state}"))
        .await;
    assert_eq!(
        callback.status(),
        StatusCode::SEE_OTHER,
        "callback redirects"
    );
    assert_eq!(location(&callback), "/register");
    assert!(browser.cookies.contains_key(SESSION_COOKIE));
    assert!(
        !browser.cookies.contains_key(STATE_COOKIE),
        "state cookie is destroyed at the callback"
    );
    db.repo
        .find_by_discord_user_id(discord_user_id)
        .await
        .unwrap()
        .expect("player row exists after sign-in")
        .player_id
}

fn at(seconds: i64) -> Timestamp {
    Utc.with_ymd_and_hms(2026, 9, 12, 21, 0, 0).unwrap() + TimeDelta::seconds(seconds)
}

fn fid(n: u128) -> FormationId {
    FormationId(Uuid::from_u128(0xe000 + n))
}

/// フィクスチャの players 1〜10 で 5v5 を作る。`player` は red の 1 人目に据える。
fn formation(n: u128, player: PlayerId) -> NewFormation {
    let mut players: Vec<NewFormationPlayer> = (2..=10)
        .map(|i| NewFormationPlayer {
            player_id: PlayerId(Uuid::from_u128(i)),
            team: if i <= 5 { Team::Red } else { Team::Blue },
            role: ROLES[(i as usize - 1) % 5].to_owned(),
            role_was_flexed: false,
            mu_at_match: 25.0,
            sigma_at_match: 8.33,
        })
        .collect();
    players.insert(
        0,
        NewFormationPlayer {
            player_id: player,
            team: Team::Red,
            role: ROLES[0].to_owned(),
            role_was_flexed: false,
            mu_at_match: 30.0,
            sigma_at_match: 8.33,
        },
    );
    NewFormation {
        id: fid(n),
        created_at: at(0),
        matchmaking_params_version: 1,
        predicted_win_prob: 0.5,
        players,
    }
}

/// formation を `in_progress`（報告義務あり）まで進める。
async fn formation_in_progress(db: &AppDb, n: u128, player: PlayerId) {
    db.formations
        .create_formation(&formation(n, player))
        .await
        .unwrap();
    let host = PlayerId(Uuid::from_u128(2));
    for event in [
        NewEvent::vc_assigned("1", 101, 102, at(0), format!("vc-{n}")),
        NewEvent::new(K::HostAssigned, Some(host), at(1), format!("assign-{n}")),
        NewEvent::new(K::HostAccepted, Some(host), at(2), format!("accept-{n}")),
        NewEvent::code_posted(host, "1234", at(3), format!("code-{n}")),
    ] {
        db.formations.append_event(fid(n), &event).await.unwrap();
    }
}

/// 登録フォームを送る（トレーナー名・ランク・レート）。
fn form_body(trainer: &str, rank: &str, rating: &str) -> String {
    format!("trainer_name={trainer}&rank={rank}&rating={rating}")
}

async fn declaration_count(db: &AppDb, player: PlayerId) -> i64 {
    sqlx::query_scalar!(
        "select count(*) as \"count!\" from player_declarations where player_id = $1",
        player.0
    )
    .fetch_one(&db.pool)
    .await
    .unwrap()
}

// ----------------------------------------------------------------------
// OAuth とセッション
// ----------------------------------------------------------------------

#[sqlx::test(
    fixtures("reference", "app_role"),
    fixtures("../ops/app_privileges.sql")
)]
async fn login_sets_a_scoped_state_cookie_and_redirects_to_discord(owner: PgPool) {
    let db = AppDb::connect(&owner).await;
    let oauth = std::sync::Arc::new(FakeOAuth::new("900000000000000005"));
    let mut browser = Browser::new(router(AuthState::new(config(), oauth, db.repo.clone())));

    let response = browser.get("/auth/login").await;
    assert_eq!(response.status(), StatusCode::SEE_OTHER);

    // scope は identify だけ。email も guilds も要求しない
    let target = location(&response);
    assert!(
        target.starts_with("https://discord.com/oauth2/authorize?"),
        "{target}"
    );
    assert!(target.contains("scope=identify"), "{target}");
    assert!(!target.contains("email"), "{target}");
    assert!(!target.contains("guilds"), "{target}");
    assert!(target.contains("response_type=code"), "{target}");
    // client_secret は URL に出ない
    assert!(!target.contains("client-secret"), "{target}");

    // state Cookie の属性
    let cookie_line = set_cookie_lines(&response)
        .into_iter()
        .find(|line| line.starts_with(STATE_COOKIE))
        .expect("state cookie");
    for attribute in ["HttpOnly", "Secure", "SameSite=Lax"] {
        assert!(
            cookie_line.contains(attribute),
            "{attribute}: {cookie_line}"
        );
    }
    assert!(
        cookie_line.contains(&format!("Domain={COOKIE_DOMAIN}")),
        "{cookie_line}"
    );
    // 親ドメインにしない
    assert!(
        !cookie_line.contains("Domain=.").to_owned(),
        "{cookie_line}"
    );
    assert!(!cookie_line.contains("SameSite=Strict"), "{cookie_line}");
    db.done().await;
}

#[sqlx::test(
    fixtures("reference", "app_role"),
    fixtures("../ops/app_privileges.sql")
)]
async fn the_callback_destroys_the_state_cookie_and_rejects_a_mismatch(owner: PgPool) {
    let db = AppDb::connect(&owner).await;
    let oauth = std::sync::Arc::new(FakeOAuth::new("900000000000000005"));
    let mut browser = Browser::new(router(AuthState::new(
        config(),
        oauth.clone(),
        db.repo.clone(),
    )));

    browser.get("/auth/login").await;
    assert!(browser.cookies.contains_key(STATE_COOKIE));

    // 違う state ではセッションを張らない
    let response = browser.get("/auth/callback?code=abc&state=forged").await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert!(!browser.cookies.contains_key(SESSION_COOKIE));
    // 失敗しても state は破棄される（使い回せない）
    assert!(!browser.cookies.contains_key(STATE_COOKIE));
    assert!(oauth.codes().is_empty(), "no code is exchanged on mismatch");

    // 破棄済みなので、正しい state を後から出しても通らない
    let response = browser.get("/auth/callback?code=abc&state=whatever").await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert!(!browser.cookies.contains_key(SESSION_COOKIE));
    db.done().await;
}

#[sqlx::test(
    fixtures("reference", "app_role"),
    fixtures("../ops/app_privileges.sql")
)]
async fn a_state_token_cannot_be_replayed(owner: PgPool) {
    let db = AppDb::connect(&owner).await;
    let oauth = std::sync::Arc::new(FakeOAuth::new("900000000000000005"));
    let mut browser = Browser::new(router(AuthState::new(
        config(),
        oauth.clone(),
        db.repo.clone(),
    )));

    let login = browser.get("/auth/login").await;
    let state = state_from_redirect(&login);
    let first = browser
        .get(&format!("/auth/callback?code=one&state={state}"))
        .await;
    assert_eq!(first.status(), StatusCode::SEE_OTHER);

    // 同じ state をもう一度: Cookie はもう無いので通らない
    let replay = browser
        .get(&format!("/auth/callback?code=two&state={state}"))
        .await;
    assert_eq!(replay.status(), StatusCode::BAD_REQUEST);
    assert_eq!(
        oauth.codes(),
        ["one"],
        "the replayed code is never exchanged"
    );
    db.done().await;
}

#[sqlx::test(
    fixtures("reference", "app_role"),
    fixtures("../ops/app_privileges.sql")
)]
async fn the_session_cookie_carries_only_an_identifier_and_a_timestamp(owner: PgPool) {
    let db = AppDb::connect(&owner).await;
    let discord_id = "900000000000000005";
    let oauth = std::sync::Arc::new(FakeOAuth::new(discord_id));
    let mut browser = Browser::new(router(AuthState::new(config(), oauth, db.repo.clone())));

    let player = sign_in(&mut browser, &db, discord_id).await;

    // 署名付きなので値そのものは読めないが、権限やロールの語が混ざっていないことは見える
    let raw = browser.cookies.get(SESSION_COOKIE).unwrap();
    for forbidden in ["admin", "role", "suspended", "legend", "trainer"] {
        assert!(!raw.contains(forbidden), "{forbidden} in {raw}");
    }

    // ログイン直後は未登録。行はあるが registered_at は NULL
    let profile = db.repo.find_by_player_id(player).await.unwrap().unwrap();
    assert_eq!(profile.discord_user_id, discord_id);
    assert!(!profile.is_registered());
    assert_eq!(profile.status, Some(PlayerStatus::Active));
    assert_eq!(profile.latest_declaration, None);

    // ログアウトで Cookie が消え、フォームは入口に戻される
    let logout = browser.post_form("/auth/logout", "").await;
    assert_eq!(logout.status(), StatusCode::SEE_OTHER);
    assert!(!browser.cookies.contains_key(SESSION_COOKIE));
    let after = browser.get("/register").await;
    assert_eq!(after.status(), StatusCode::SEE_OTHER);
    assert_eq!(location(&after), "/");
    db.done().await;
}

#[sqlx::test(
    fixtures("reference", "app_role"),
    fixtures("../ops/app_privileges.sql")
)]
async fn signing_in_twice_reuses_the_same_player_row(owner: PgPool) {
    let db = AppDb::connect(&owner).await;
    let discord_id = "900000000000000005";
    let oauth = std::sync::Arc::new(FakeOAuth::new(discord_id));
    let app = router(AuthState::new(config(), oauth, db.repo.clone()));

    let mut first = Browser::new(app.clone());
    let player = sign_in(&mut first, &db, discord_id).await;
    let mut second = Browser::new(app);
    let again = sign_in(&mut second, &db, discord_id).await;
    assert_eq!(
        player, again,
        "the same discord user maps to one player row"
    );

    let rows: i64 = sqlx::query_scalar!(
        "select count(*) as \"count!\" from players where discord_user_id = $1",
        900_000_000_000_000_005_i64
    )
    .fetch_one(&db.pool)
    .await
    .unwrap();
    assert_eq!(rows, 1);
    db.done().await;
}

// ----------------------------------------------------------------------
// 登録フォーム
// ----------------------------------------------------------------------

#[sqlx::test(
    fixtures("reference", "app_role"),
    fixtures("../ops/app_privileges.sql")
)]
async fn registering_appends_a_declaration_and_every_resubmit_appends_another(owner: PgPool) {
    let db = AppDb::connect(&owner).await;
    let discord_id = "900000000000000005";
    let oauth = std::sync::Arc::new(FakeOAuth::new(discord_id));
    let mut browser = Browser::new(router(AuthState::new(config(), oauth, db.repo.clone())));
    let player = sign_in(&mut browser, &db, discord_id).await;

    // フォームには全ランクが並ぶ
    let form = body_text(browser.get("/register").await).await;
    for rank in Rank::ALL {
        assert!(form.contains(rank.display_name()), "{rank} missing");
    }

    let response = browser
        .post_form("/register", &form_body("トレーナー", "legend", "1500"))
        .await;
    assert_eq!(response.status(), StatusCode::OK);
    let page = body_text(response).await;
    assert!(page.contains("キューに参加できます"), "{page}");

    let profile = db.repo.find_by_player_id(player).await.unwrap().unwrap();
    assert!(profile.is_registered());
    assert_eq!(profile.trainer_name.as_deref(), Some("トレーナー"));
    let declaration = profile.latest_declaration.clone().unwrap();
    assert_eq!(declaration.rank, Some(Rank::Legend));
    assert_eq!(declaration.rating, Some(1500));
    assert_eq!(declaration_count(&db, player).await, 1);
    let registered_at = profile.registered_at.unwrap();

    // 申告し直し: 内容が同じでも新しい行になる（状態ベースの冪等にしない）
    browser
        .post_form("/register", &form_body("トレーナー", "legend", "1500"))
        .await;
    assert_eq!(declaration_count(&db, player).await, 2);

    // 内容を変えた申告も追記。現在値は最新行から導出される
    browser
        .post_form("/register", &form_body("別名", "master", "1400"))
        .await;
    assert_eq!(declaration_count(&db, player).await, 3);
    let profile = db.repo.find_by_player_id(player).await.unwrap().unwrap();
    assert_eq!(profile.trainer_name.as_deref(), Some("別名"));
    let declaration = profile.latest_declaration.clone().unwrap();
    assert_eq!(declaration.rank, Some(Rank::Master));
    assert_eq!(declaration.rating, Some(1400));
    // 最初の登録時刻は動かない
    assert_eq!(profile.registered_at, Some(registered_at));

    // 過去の申告は消えていない
    let ranks: Vec<String> = sqlx::query_scalar!(
        "select rank from player_declarations where player_id = $1 order by id",
        player.0
    )
    .fetch_all(&db.pool)
    .await
    .unwrap();
    assert_eq!(ranks, ["legend", "legend", "master"]);
    db.done().await;
}

#[sqlx::test(
    fixtures("reference", "app_role"),
    fixtures("../ops/app_privileges.sql")
)]
async fn a_declaration_below_the_entry_requirement_is_recorded_but_blocked(owner: PgPool) {
    let db = AppDb::connect(&owner).await;
    let discord_id = "900000000000000005";
    let oauth = std::sync::Arc::new(FakeOAuth::new(discord_id));
    let mut browser = Browser::new(router(AuthState::new(config(), oauth, db.repo.clone())));
    let player = sign_in(&mut browser, &db, discord_id).await;

    let response = browser
        .post_form("/register", &form_body("トレーナー", "elite", ""))
        .await;
    assert_eq!(response.status(), StatusCode::OK);
    let page = body_text(response).await;
    assert!(page.contains("いまはキューに入れません"), "{page}");
    assert!(page.contains("参加資格は"), "{page}");

    // 資格未満でも記録は残る（生データを捨てない）
    assert_eq!(declaration_count(&db, player).await, 1);
    let eligibility = db.repo.queue_eligibility(player).await.unwrap();
    assert!(!eligibility.is_allowed());
    assert_eq!(
        eligibility.registration,
        Some(RegistrationBlock::RankBelowEntry {
            declared: Some(Rank::Elite)
        })
    );
    assert!(!eligibility.is_report_blocked());
    db.done().await;
}

#[sqlx::test(
    fixtures("reference", "app_role"),
    fixtures("../ops/app_privileges.sql")
)]
async fn an_invalid_form_is_rejected_without_writing_anything(owner: PgPool) {
    let db = AppDb::connect(&owner).await;
    let discord_id = "900000000000000005";
    let oauth = std::sync::Arc::new(FakeOAuth::new(discord_id));
    let mut browser = Browser::new(router(AuthState::new(config(), oauth, db.repo.clone())));
    let player = sign_in(&mut browser, &db, discord_id).await;

    for (body, expected) in [
        (form_body("", "legend", "1500"), "トレーナー名"),
        (form_body("トレーナー", "", "1500"), "ランク"),
        (form_body("トレーナー", "legend", ""), "本家レート"),
        (form_body("トレーナー", "legend", "つよい"), "本家レート"),
    ] {
        let response = browser.post_form("/register", &body).await;
        assert_eq!(response.status(), StatusCode::BAD_REQUEST, "{body}");
        let page = body_text(response).await;
        assert!(page.contains(expected), "{body}: {page}");
    }
    assert_eq!(declaration_count(&db, player).await, 0);
    let profile = db.repo.find_by_player_id(player).await.unwrap().unwrap();
    assert!(!profile.is_registered());
    db.done().await;
}

#[sqlx::test(
    fixtures("reference", "app_role"),
    fixtures("../ops/app_privileges.sql")
)]
async fn the_form_requires_a_session(owner: PgPool) {
    let db = AppDb::connect(&owner).await;
    let oauth = std::sync::Arc::new(FakeOAuth::new("900000000000000005"));
    let mut browser = Browser::new(router(AuthState::new(config(), oauth, db.repo.clone())));

    let form = browser.get("/register").await;
    assert_eq!(form.status(), StatusCode::SEE_OTHER);
    assert_eq!(location(&form), "/");

    let submit = browser
        .post_form("/register", &form_body("トレーナー", "legend", "1500"))
        .await;
    assert_eq!(submit.status(), StatusCode::SEE_OTHER);
    assert_eq!(location(&submit), "/");

    // 入口はログインページ
    let index = body_text(browser.get("/").await).await;
    assert!(index.contains("Discord でログイン"), "{index}");
    assert!(index.contains("identify"), "{index}");
    db.done().await;
}

// ----------------------------------------------------------------------
// 追記専用の担保
// ----------------------------------------------------------------------

#[sqlx::test(
    fixtures("reference", "app_role"),
    fixtures("../ops/app_privileges.sql")
)]
async fn player_declarations_is_append_only_for_the_app_role(owner: PgPool) {
    let db = AppDb::connect(&owner).await;
    let player = PlayerId(Uuid::from_u128(1));
    db.repo
        .register(&unite_matchsystem::registration::NewDeclaration {
            player_id: player,
            declared_at: at(0),
            trainer_name: "トレーナー".into(),
            rank: Rank::Legend,
            rating: Some(1500),
            raw: json!({ "rank": "legend" }),
        })
        .await
        .unwrap();

    // アプリ用ロールは権限で弾かれる（42501）。トリガー（0A000）より前に止まる
    for sql in [
        "update player_declarations set rating = 1",
        "delete from player_declarations",
    ] {
        let error = sqlx::query(sql).execute(&db.pool).await.unwrap_err();
        assert_eq!(
            error.as_database_error().unwrap().code().as_deref(),
            Some("42501"),
            "{sql}"
        );
        // 所有者でもトリガーが拒否する（0A000）
        let error = sqlx::query(sql).execute(&owner).await.unwrap_err();
        assert_eq!(
            error.as_database_error().unwrap().code().as_deref(),
            Some("0A000"),
            "{sql}"
        );
    }

    // players は可変なので UPDATE してよい
    db.repo
        .set_status(player, PlayerStatus::Suspended)
        .await
        .unwrap();
    let profile = db.repo.find_by_player_id(player).await.unwrap().unwrap();
    assert_eq!(profile.status, Some(PlayerStatus::Suspended));
    db.done().await;
}

// ----------------------------------------------------------------------
// キューゲート
// ----------------------------------------------------------------------

#[sqlx::test(
    fixtures("reference", "app_role"),
    fixtures("../ops/app_privileges.sql")
)]
async fn the_report_gate_opens_when_the_player_reports(owner: PgPool) {
    let db = AppDb::connect(&owner).await;
    let player = PlayerId(Uuid::from_u128(1));
    db.repo
        .register(&unite_matchsystem::registration::NewDeclaration {
            player_id: player,
            declared_at: at(0),
            trainer_name: "トレーナー".into(),
            rank: Rank::Legend,
            rating: Some(1500),
            raw: json!({}),
        })
        .await
        .unwrap();
    assert!(
        db.repo
            .queue_eligibility(player)
            .await
            .unwrap()
            .is_allowed()
    );

    // コード開示済み（報告義務あり）の formation を作る
    formation_in_progress(&db, 1, player).await;
    let eligibility = db.repo.queue_eligibility(player).await.unwrap();
    assert!(!eligibility.is_allowed());
    assert_eq!(eligibility.registration, None, "registration gate is open");
    assert_eq!(eligibility.pending_reports, vec![fid(1)]);

    // 自分が報告すれば解除される。確定を待たない
    db.formations
        .append_report(&NewReport {
            formation_id: fid(1),
            player_id: player,
            reported_at: at(10),
            outcome: Outcome::Win(Team::Red),
            raw_result: json!({}),
            supersedes_id: None,
        })
        .await
        .unwrap();
    let eligibility = db.repo.queue_eligibility(player).await.unwrap();
    assert!(eligibility.is_allowed(), "{eligibility:?}");

    // 他人が報告しても、自分の未報告は解除されない
    formation_in_progress(&db, 2, player).await;
    db.formations
        .append_report(&NewReport {
            formation_id: fid(2),
            player_id: PlayerId(Uuid::from_u128(2)),
            reported_at: at(11),
            outcome: Outcome::Win(Team::Blue),
            raw_result: json!({}),
            supersedes_id: None,
        })
        .await
        .unwrap();
    let eligibility = db.repo.queue_eligibility(player).await.unwrap();
    assert_eq!(eligibility.pending_reports, vec![fid(2)]);
    db.done().await;
}

#[sqlx::test(
    fixtures("reference", "app_role"),
    fixtures("../ops/app_privileges.sql")
)]
async fn a_disputed_formation_does_not_re_block_a_player_who_reported(owner: PgPool) {
    let db = AppDb::connect(&owner).await;
    let player = PlayerId(Uuid::from_u128(1));
    db.repo
        .register(&unite_matchsystem::registration::NewDeclaration {
            player_id: player,
            declared_at: at(0),
            trainer_name: "トレーナー".into(),
            rank: Rank::Legend,
            rating: Some(1500),
            raw: json!({}),
        })
        .await
        .unwrap();
    formation_in_progress(&db, 1, player).await;
    db.formations
        .append_report(&NewReport {
            formation_id: fid(1),
            player_id: player,
            reported_at: at(10),
            outcome: Outcome::Win(Team::Red),
            raw_result: json!({}),
            supersedes_id: None,
        })
        .await
        .unwrap();
    db.formations
        .append_event(
            fid(1),
            &NewEvent::new(K::Disputed, None, at(20), "disputed-1"),
        )
        .await
        .unwrap();

    // 係争入りしてもゲートは解除済みのまま（他 9 人に人質を取られない）
    assert!(
        db.repo
            .queue_eligibility(player)
            .await
            .unwrap()
            .is_allowed()
    );
    db.done().await;
}

#[sqlx::test(
    fixtures("reference", "app_role"),
    fixtures("../ops/app_privileges.sql")
)]
async fn formations_that_never_happened_do_not_block_the_queue(owner: PgPool) {
    let db = AppDb::connect(&owner).await;
    let player = PlayerId(Uuid::from_u128(1));
    db.repo
        .register(&unite_matchsystem::registration::NewDeclaration {
            player_id: player,
            declared_at: at(0),
            trainer_name: "トレーナー".into(),
            rank: Rank::Legend,
            rating: Some(1500),
            raw: json!({}),
        })
        .await
        .unwrap();

    // 解散した formation（試合が成立していない）
    db.formations
        .create_formation(&formation(1, player))
        .await
        .unwrap();
    db.formations
        .append_event(
            fid(1),
            &NewEvent::abandoned(
                unite_matchsystem::lifecycle::AbandonReason::VcPoolExhausted,
                at(1),
                "abandoned-1",
            ),
        )
        .await
        .unwrap();

    // まだホストを待っている formation（コード未開示）
    db.formations
        .create_formation(&formation(2, player))
        .await
        .unwrap();
    db.formations
        .append_event(fid(2), &NewEvent::vc_assigned("2", 201, 202, at(0), "vc-2"))
        .await
        .unwrap();

    let eligibility = db.repo.queue_eligibility(player).await.unwrap();
    assert!(eligibility.is_allowed(), "{eligibility:?}");
    db.done().await;
}

#[sqlx::test(
    fixtures("reference", "app_role"),
    fixtures("../ops/app_privileges.sql")
)]
async fn the_two_gates_are_reported_separately(owner: PgPool) {
    let db = AppDb::connect(&owner).await;
    let player = PlayerId(Uuid::from_u128(1));
    db.repo
        .register(&unite_matchsystem::registration::NewDeclaration {
            player_id: player,
            declared_at: at(0),
            trainer_name: "トレーナー".into(),
            rank: Rank::Master,
            rating: Some(1400),
            raw: json!({}),
        })
        .await
        .unwrap();
    formation_in_progress(&db, 1, player).await;

    let eligibility = db.repo.queue_eligibility(player).await.unwrap();
    assert!(!eligibility.is_allowed());
    // 畳まれていない: 両方の理由が残る
    assert_eq!(
        eligibility.registration,
        Some(RegistrationBlock::RankBelowEntry {
            declared: Some(Rank::Master)
        })
    );
    assert_eq!(eligibility.pending_reports, vec![fid(1)]);
    assert_eq!(eligibility.messages().len(), 2);

    // 停止処分は登録ゲート側の理由として出る
    db.repo
        .set_status(player, PlayerStatus::Suspended)
        .await
        .unwrap();
    let eligibility = db.repo.queue_eligibility(player).await.unwrap();
    assert_eq!(eligibility.registration, Some(RegistrationBlock::Suspended));
    assert_eq!(eligibility.pending_reports, vec![fid(1)]);
    db.done().await;
}
