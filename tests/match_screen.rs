//! マッチ画面（`/match/{id}`）の統合テスト。実際の PostgreSQL を使う。
//!
//! リポジトリは所有者ではなく**アプリ用ロール**（`assay_app`）で接続する。
//!
//! 検証すること（不変条件との対応）:
//!
//! - **ホスト打診の操作は打診中の候補本人にだけ**出る。他人が押しても状態は動かない
//! - **ルーム ID を出せるのはホストだけ**。打ち直しは**新しいイベントとして通る**
//!   （00 §5: 状態ベースの冪等にすると打ち直しを黙って捨てる）
//! - **参加者以外は報告できない**。存在も教えない（404）
//! - **他人の報告状況が HTML にもレスポンスにも出ない。件数も出ない**（00 §3-5）。
//!   DOM を見れば分かる形にもしない
//! - **相手チームのロールが HTML にもレスポンスにも出ない**
//! - 確定判定は既存のロジックのまま。全員一致で確定、割れれば disputed、
//!   全員無効で nullified。訂正時は「確定は最新・一致率は最初」
//! - σ・レート調整の内訳を出さない

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
use unite_matchsystem::lifecycle::{
    FormationEventKind as K, FormationId, FormationState as S, LifecycleConfig, PlayerId, Timestamp,
};
use unite_matchsystem::match_screen::{MatchRepository, MatchState, router};
use unite_matchsystem::persistence::{
    FormationRepository, NewEvent, NewFormation, NewFormationPlayer, Team,
};
use unite_matchsystem::registration::session::{SESSION_COOKIE, Session, set_session};
use unite_matchsystem::registration::{AuthConfig, site_base_url};
use uuid::Uuid;

const APP_ROLE: &str = "assay_app";
const APP_ROLE_PASSWORD: &str = "assay_app_test";
const COOKIE_DOMAIN: &str = "match.example.test";
/// マイグレーションが入れる 5 ロール。
const ROLES: [&str; 5] = [
    "top_carry",
    "top_support",
    "jungle",
    "bot_carry",
    "bot_tank",
];
/// 5 ロールの表示名（`roles` シードと同じ順）。
const ROLE_LABELS: [&str; 5] = ["上キャリー", "サポート", "中央", "下キャリー", "タンク"];

// ----------------------------------------------------------------------
// 道具
// ----------------------------------------------------------------------

fn player(n: u128) -> PlayerId {
    PlayerId(Uuid::from_u128(n))
}

fn fid(n: u128) -> FormationId {
    FormationId(Uuid::from_u128(n))
}

fn at(seconds: i64) -> Timestamp {
    Utc.with_ymd_and_hms(2026, 9, 12, 21, 0, 0).unwrap() + TimeDelta::seconds(seconds)
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

struct AppDb {
    pool: PgPool,
    formations: FormationRepository,
    matches: MatchRepository,
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
            matches: MatchRepository::new(pool.clone()),
            pool,
        }
    }

    fn router(&self) -> Router {
        router(MatchState::new(
            config(),
            self.formations.clone(),
            self.matches.clone(),
            LifecycleConfig::default(),
            "kyogre",
            &["大会モード".to_owned()],
        ))
    }

    async fn done(self) {
        self.pool.close().await;
    }
}

/// 署名済みのセッション Cookie。OAuth を通さずログイン済みの状態を作る。
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

/// players 1〜5 が red、6〜10 が blue。ロールは `ROLES` の順で 1 人ずつ。
fn formation(n: u128) -> NewFormation {
    NewFormation {
        id: fid(n),
        created_at: at(0),
        matchmaking_params_version: 1,
        predicted_win_prob: 0.5,
        players: (1..=10)
            .map(|i| NewFormationPlayer {
                player_id: player(i),
                team: if i <= 5 { Team::Red } else { Team::Blue },
                role: ROLES[(i as usize - 1) % 5].to_owned(),
                role_was_flexed: false,
                mu_at_match: 25.0,
                sigma_at_match: 8.33,
            })
            .collect(),
    }
}

/// 進行段階。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Stage {
    Matched,
    AwaitingHost,
    AwaitingCode,
    InProgress,
}

/// formation を作って `stage` まで進める。ホストは player 2（red）。
async fn formation_at(db: &AppDb, n: u128, stage: Stage) {
    db.formations.create_formation(&formation(n)).await.unwrap();
    let host = player(2);
    let steps = [
        (
            Stage::AwaitingHost,
            NewEvent::vc_assigned("1", 101, 102, at(0), format!("vc-{n}")),
        ),
        (
            Stage::AwaitingHost,
            NewEvent::new(K::HostAssigned, Some(host), at(1), format!("assign-{n}")),
        ),
        (
            Stage::AwaitingCode,
            NewEvent::new(K::HostAccepted, Some(host), at(2), format!("accept-{n}")),
        ),
        (
            Stage::InProgress,
            NewEvent::code_posted(host, "1234", at(3), format!("code-{n}")),
        ),
    ];
    for (needed, event) in steps {
        if stage < needed {
            break;
        }
        db.formations.append_event(fid(n), &event).await.unwrap();
    }
}

async fn send(app: &Router, request: Request<Body>) -> Response<Body> {
    app.clone().oneshot(request).await.unwrap()
}

/// マッチ画面を JSON で取る。
async fn get_json(app: &Router, n: u128, as_player: PlayerId) -> (StatusCode, Value) {
    let request = Request::get(format!("/match/{}", fid(n).0))
        .header(header::COOKIE, session_cookie(as_player))
        .body(Body::empty())
        .unwrap();
    let response = send(app, request).await;
    let status = response.status();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    let value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    (status, value)
}

/// マッチ画面を HTML で取る。
async fn get_html(app: &Router, n: u128, as_player: PlayerId) -> (StatusCode, String) {
    let request = Request::get(format!("/match/{}", fid(n).0))
        .header(header::COOKIE, session_cookie(as_player))
        .header(header::ACCEPT, "text/html")
        .body(Body::empty())
        .unwrap();
    let response = send(app, request).await;
    let status = response.status();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    (status, String::from_utf8(bytes.to_vec()).unwrap())
}

async fn post(
    app: &Router,
    n: u128,
    as_player: PlayerId,
    path: &str,
    body: Value,
) -> (StatusCode, Value) {
    let request = Request::post(format!("/match/{}{path}", fid(n).0))
        .header(header::COOKIE, session_cookie(as_player))
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(body.to_string()))
        .unwrap();
    let response = send(app, request).await;
    let status = response.status();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    let value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    (status, value)
}

async fn state_of(db: &AppDb, n: u128) -> S {
    db.formations
        .load_events(fid(n))
        .await
        .unwrap()
        .snapshot()
        .state
}

async fn kinds(db: &AppDb, n: u128) -> Vec<K> {
    db.formations
        .load_events(fid(n))
        .await
        .unwrap()
        .events
        .iter()
        .map(|stored| stored.event.kind)
        .collect()
}

/// 報告を出す（サイトの経路）。
async fn report(app: &Router, n: u128, as_player: u128, outcome: &str) -> StatusCode {
    post(
        app,
        n,
        player(as_player),
        "/report",
        json!({ "outcome": outcome }),
    )
    .await
    .0
}

// ----------------------------------------------------------------------
// 画面
// ----------------------------------------------------------------------

/// **相手チームのロールは HTML にもレスポンスにも出ない。**参加者名とレートは出る。
#[sqlx::test(
    fixtures("reference", "app_role"),
    fixtures("../ops/app_privileges.sql")
)]
async fn the_opposing_roles_are_absent_from_both_the_html_and_the_json(owner: PgPool) {
    let db = AppDb::connect(&owner).await;
    formation_at(&db, 1, Stage::InProgress).await;
    let app = db.router();

    // player 1 は red
    let (status, view) = get_json(&app, 1, player(1)).await;
    assert_eq!(status, StatusCode::OK, "{view}");
    assert_eq!(view["my_team"], "red");
    assert_eq!(view["teammates"].as_array().unwrap().len(), 5);
    assert_eq!(view["opponents"].as_array().unwrap().len(), 5);
    // 自チームの行にはロールがある
    for teammate in view["teammates"].as_array().unwrap() {
        assert!(teammate["role"].is_string(), "{teammate}");
    }
    // 相手チームの行には**ロールのキーが存在しない**
    for opponent in view["opponents"].as_array().unwrap() {
        assert!(opponent.get("role").is_none(), "{opponent}");
        assert!(opponent["trainer_name"].is_string(), "{opponent}");
        assert!(opponent["rating"].is_number(), "{opponent}");
    }

    // HTML 側も同じ。blue の5人のロール表示名がどこにも無いこと
    let (status, html) = get_html(&app, 1, player(1)).await;
    assert_eq!(status, StatusCode::OK);
    let opponent_section = html
        .split("id=\"opponent-title\"")
        .nth(1)
        .and_then(|rest| rest.split("</section>").next())
        .expect("opponent section");
    for label in ROLE_LABELS {
        assert!(
            !opponent_section.contains(label),
            "{label} が相手側に出ている: {opponent_section}"
        );
    }
    // 自分の側には自分のロールが出る（player 1 は ROLES[0]）
    assert!(html.contains(ROLE_LABELS[0]), "{html}");
    db.done().await;
}

/// blue 側から見ても同じ（自分のチームだけロールが出る）。
#[sqlx::test(
    fixtures("reference", "app_role"),
    fixtures("../ops/app_privileges.sql")
)]
async fn each_side_only_sees_its_own_roles(owner: PgPool) {
    let db = AppDb::connect(&owner).await;
    formation_at(&db, 1, Stage::InProgress).await;
    let app = db.router();

    let (_, blue) = get_json(&app, 1, player(6)).await;
    assert_eq!(blue["my_team"], "blue");
    let mine: Vec<&str> = blue["teammates"]
        .as_array()
        .unwrap()
        .iter()
        .map(|entry| entry["role"].as_str().unwrap())
        .collect();
    assert_eq!(mine.len(), 5);
    for opponent in blue["opponents"].as_array().unwrap() {
        assert!(opponent.get("role").is_none(), "{opponent}");
    }
    // 相手（red）の参加者名とレートは見える
    let names: Vec<&str> = blue["opponents"]
        .as_array()
        .unwrap()
        .iter()
        .map(|entry| entry["trainer_name"].as_str().unwrap())
        .collect();
    assert_eq!(names.len(), 5);
    db.done().await;
}

/// **ホストは相手チームでも名前が表示される。**
#[sqlx::test(
    fixtures("reference", "app_role"),
    fixtures("../ops/app_privileges.sql")
)]
async fn the_host_is_visible_to_the_other_team_too(owner: PgPool) {
    let db = AppDb::connect(&owner).await;
    formation_at(&db, 1, Stage::InProgress).await;
    let app = db.router();

    // ホストは player 2（red）。blue の player 6 から見る
    let (_, blue) = get_json(&app, 1, player(6)).await;
    let host = blue["opponents"]
        .as_array()
        .unwrap()
        .iter()
        .find(|entry| entry["is_host"] == json!(true))
        .expect("相手チームのホストが見える");
    assert!(host["trainer_name"].is_string(), "{host}");
    // ただしホストのロールは出ない
    assert!(host.get("role").is_none(), "{host}");

    let (_, html) = get_html(&app, 1, player(6)).await;
    assert!(html.contains("ホスト"), "{html}");
    db.done().await;
}

/// **参加者以外には存在を教えない**（404。403 だと「その試合はある」と分かる）。
#[sqlx::test(
    fixtures("reference", "app_role"),
    fixtures("../ops/app_privileges.sql")
)]
async fn a_non_participant_cannot_even_see_the_match(owner: PgPool) {
    let db = AppDb::connect(&owner).await;
    formation_at(&db, 1, Stage::InProgress).await;
    let app = db.router();

    // players 11・12 は編成外
    let (status, _) = get_json(&app, 1, player(11)).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, _) = get_html(&app, 1, player(11)).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    // 存在しない formation も同じ応答
    let missing = Request::get(format!("/match/{}", Uuid::from_u128(0xdead)))
        .header(header::COOKIE, session_cookie(player(1)))
        .body(Body::empty())
        .unwrap();
    assert_eq!(send(&app, missing).await.status(), StatusCode::NOT_FOUND);

    // 未ログインは 401
    let anonymous = Request::get(format!("/match/{}", fid(1).0))
        .body(Body::empty())
        .unwrap();
    assert_eq!(
        send(&app, anonymous).await.status(),
        StatusCode::UNAUTHORIZED
    );
    db.done().await;
}

/// **σ・レート調整の内訳を出さない。**出すのは現在のレートだけ。
#[sqlx::test(
    fixtures("reference", "app_role"),
    fixtures("../ops/app_privileges.sql")
)]
async fn the_match_screen_never_shows_sigma_or_adjustments(owner: PgPool) {
    let db = AppDb::connect(&owner).await;
    formation_at(&db, 1, Stage::InProgress).await;
    let app = db.router();

    let (_, view) = get_json(&app, 1, player(1)).await;
    let json = view.to_string();
    let (_, html) = get_html(&app, 1, player(1)).await;
    for forbidden in [
        "sigma",
        "sigma_at_match",
        "mu_at_match",
        "delta_mu",
        "delta_sigma",
        "rating_adjust",
        "最高レート",
    ] {
        assert!(!json.contains(forbidden), "{forbidden}: {json}");
        assert!(!html.contains(forbidden), "{forbidden}");
    }
    // レートそのものは出る（公開方針）
    assert!(view["teammates"][0]["rating"].is_number(), "{view}");
    db.done().await;
}

// ----------------------------------------------------------------------
// ホスト打診
// ----------------------------------------------------------------------

/// **打診の操作は候補本人にだけ。**他の9人には出ないし、押しても通らない。
#[sqlx::test(
    fixtures("reference", "app_role"),
    fixtures("../ops/app_privileges.sql")
)]
async fn only_the_pending_candidate_can_answer_the_host_prompt(owner: PgPool) {
    let db = AppDb::connect(&owner).await;
    formation_at(&db, 1, Stage::AwaitingHost).await;
    let app = db.router();

    // 候補は player 2
    let (_, candidate) = get_json(&app, 1, player(2)).await;
    assert_eq!(candidate["stage"], "awaiting_host");
    assert_eq!(candidate["i_am_candidate"], json!(true));
    assert_eq!(candidate["actions"]["host_response"], json!(true));
    let (_, html) = get_html(&app, 1, player(2)).await;
    assert!(html.contains("id=\"host-accept\""), "{html}");
    assert!(html.contains("id=\"host-decline\""), "{html}");

    // 他の9人には操作が出ない
    for other in [1u128, 3, 6, 10] {
        let (_, view) = get_json(&app, 1, player(other)).await;
        assert_eq!(view["i_am_candidate"], json!(false), "player {other}");
        assert_eq!(view["actions"]["host_response"], json!(false));
        let (_, html) = get_html(&app, 1, player(other)).await;
        assert!(
            !html.contains("id=\"host-accept\""),
            "player {other}: {html}"
        );
        // 誰に打診しているかは全員に見える（進行状況なので隠さない）
        assert!(html.contains("部屋の作成をお願いしています"), "{html}");
    }

    // 押しても通らない。状態は動かない
    let (status, body) = post(&app, 1, player(3), "/host", json!({ "action": "accept" })).await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(body["error"], "not_the_candidate");
    assert!(matches!(
        state_of(&db, 1).await,
        S::AwaitingHost {
            pending: Some(_),
            ..
        }
    ));

    // 本人が承諾すると進む
    let (status, _) = post(&app, 1, player(2), "/host", json!({ "action": "accept" })).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        state_of(&db, 1).await,
        S::AwaitingCode {
            host: player(2),
            tried: 1
        }
    );
    db.done().await;
}

/// パスすると次の候補へ進む（候補の切り替えは状態機械の既存の動き）。
#[sqlx::test(
    fixtures("reference", "app_role"),
    fixtures("../ops/app_privileges.sql")
)]
async fn declining_returns_the_formation_to_the_nomination_phase(owner: PgPool) {
    let db = AppDb::connect(&owner).await;
    formation_at(&db, 1, Stage::AwaitingHost).await;
    let app = db.router();

    let (status, _) = post(&app, 1, player(2), "/host", json!({ "action": "decline" })).await;
    assert_eq!(status, StatusCode::OK);
    // 次の候補を指名する前の状態に戻る（誰を次にするかはアダプタが決める）
    assert_eq!(
        state_of(&db, 1).await,
        S::AwaitingHost {
            pending: None,
            tried: 1
        }
    );
    assert!(kinds(&db, 1).await.contains(&K::HostDeclined));

    // 打診が無い状態で送っても通らない
    let (status, body) = post(&app, 1, player(2), "/host", json!({ "action": "accept" })).await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(body["error"], "not_awaiting_host");
    db.done().await;
}

// ----------------------------------------------------------------------
// ルーム ID
// ----------------------------------------------------------------------

/// **ホスト以外はルーム ID を出せない。**他人の formation も書き換えられない。
#[sqlx::test(
    fixtures("reference", "app_role"),
    fixtures("../ops/app_privileges.sql")
)]
async fn only_the_host_can_post_the_room_code(owner: PgPool) {
    let db = AppDb::connect(&owner).await;
    formation_at(&db, 1, Stage::AwaitingCode).await;
    // 別の formation（同じ10人）を用意して、越境できないことも見る
    formation_at(&db, 2, Stage::AwaitingCode).await;
    let app = db.router();

    // ホストは player 2。他の9人は拒否される
    for other in [1u128, 3, 6, 10] {
        let (status, body) = post(
            &app,
            1,
            player(other),
            "/room-code",
            json!({ "room_code": "9999" }),
        )
        .await;
        assert_eq!(status, StatusCode::CONFLICT, "player {other}: {body}");
        assert_eq!(body["error"], "not_the_host");
    }
    // 編成外は 404（存在も教えない）
    let (status, _) = post(
        &app,
        1,
        player(11),
        "/room-code",
        json!({ "room_code": "9999" }),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(
        state_of(&db, 1).await,
        S::AwaitingCode {
            host: player(2),
            tried: 1
        },
        "拒否された送信は状態を動かさない"
    );

    // 入力欄が出るのはホストだけ
    let (_, host_html) = get_html(&app, 1, player(2)).await;
    assert!(host_html.contains("id=\"room-code\""), "{host_html}");
    let (_, other_html) = get_html(&app, 1, player(3)).await;
    assert!(!other_html.contains("id=\"room-code\""), "{other_html}");

    // ホスト本人は通る
    let (status, _) = post(
        &app,
        1,
        player(2),
        "/room-code",
        json!({ "room_code": " 1234 " }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        state_of(&db, 1).await,
        S::InProgress {
            host: player(2),
            room_code: "1234".into()
        },
        "前後の空白は落ちる"
    );
    // formation 2 は動いていない
    assert_eq!(
        state_of(&db, 2).await,
        S::AwaitingCode {
            host: player(2),
            tried: 1
        }
    );
    db.done().await;
}

/// **打ち直しは新しいイベントとして通る**（00 §5）。
#[sqlx::test(
    fixtures("reference", "app_role"),
    fixtures("../ops/app_privileges.sql")
)]
async fn a_retyped_room_code_is_recorded_as_a_new_event(owner: PgPool) {
    let db = AppDb::connect(&owner).await;
    formation_at(&db, 1, Stage::AwaitingCode).await;
    let app = db.router();

    for code in ["1111", "2222", "3333"] {
        let (status, body) = post(
            &app,
            1,
            player(2),
            "/room-code",
            json!({ "room_code": code }),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{code}: {body}");
    }
    assert_eq!(
        state_of(&db, 1).await,
        S::InProgress {
            host: player(2),
            room_code: "3333".into()
        }
    );
    // 3 回とも別のイベントとして残る（上書きでも打ち消しでもない）
    let posted = kinds(&db, 1)
        .await
        .into_iter()
        .filter(|kind| *kind == K::CodePosted)
        .count();
    assert_eq!(posted, 3, "打ち直しが黙って捨てられていない");

    // 同じ内容の連打は 1 件に畳まれる（同一 idempotency_key）
    let before = kinds(&db, 1).await.len();
    for _ in 0..3 {
        let (status, _) = post(
            &app,
            1,
            player(2),
            "/room-code",
            json!({ "room_code": "3333" }),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
    }
    assert_eq!(kinds(&db, 1).await.len(), before, "同じ内容は増えない");

    // 空・長すぎは 400
    for bad in ["", "   ", &"9".repeat(33)] {
        let (status, body) = post(
            &app,
            1,
            player(2),
            "/room-code",
            json!({ "room_code": bad }),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    }
    db.done().await;
}

// ----------------------------------------------------------------------
// 報告
// ----------------------------------------------------------------------

/// **参加者以外は報告できない。**他人の代わりにも報告できない。
#[sqlx::test(
    fixtures("reference", "app_role"),
    fixtures("../ops/app_privileges.sql")
)]
async fn only_participants_can_report(owner: PgPool) {
    let db = AppDb::connect(&owner).await;
    formation_at(&db, 1, Stage::InProgress).await;
    let app = db.router();

    for outsider in [11u128, 12] {
        assert_eq!(
            report(&app, 1, outsider, "red").await,
            StatusCode::NOT_FOUND,
            "player {outsider}"
        );
    }
    // 報告は 1 件も入っていない
    let reports: i64 = sqlx::query_scalar!(
        "select count(*) as \"count!\" from match_reports where formation_id = $1",
        fid(1).0
    )
    .fetch_one(&db.pool)
    .await
    .unwrap();
    assert_eq!(reports, 0);

    // 参加者は通る。**player_id はセッションからしか来ない**ので、他人を名乗る経路が無い
    assert_eq!(report(&app, 1, 1, "red").await, StatusCode::OK);
    let mine: i64 = sqlx::query_scalar!(
        "select count(*) as \"count!\" from match_reports \
         where formation_id = $1 and player_id = $2",
        fid(1).0,
        player(1).0
    )
    .fetch_one(&db.pool)
    .await
    .unwrap();
    assert_eq!(mine, 1);

    // draw は選択肢に入れない（無効試合に寄せる）
    let (status, body) = post(&app, 1, player(1), "/report", json!({ "outcome": "draw" })).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    db.done().await;
}

/// **他人の報告状況は HTML にもレスポンスにも出ない。件数も出ない**（00 §3-5）。
#[sqlx::test(
    fixtures("reference", "app_role"),
    fixtures("../ops/app_privileges.sql")
)]
async fn nobody_can_see_how_many_others_have_reported(owner: PgPool) {
    let db = AppDb::connect(&owner).await;
    formation_at(&db, 1, Stage::InProgress).await;
    let app = db.router();

    // 9 人が報告して、10 人目（player 10）だけが未報告
    for reporter in 1..=9u128 {
        assert_eq!(report(&app, 1, reporter, "red").await, StatusCode::OK);
    }

    let (_, view) = get_json(&app, 1, player(10)).await;
    let json = view.to_string();
    // **件数のフィールドが存在しない**
    for key in [
        "reports",
        "reported_count",
        "pending_reports",
        "agreement",
        "votes",
        "outcome",
    ] {
        assert!(!json.contains(key), "{key} が漏れている: {json}");
    }
    // 自分の分だけは分かる
    assert_eq!(view["i_reported"], json!(false));
    assert_eq!(view["actions"]["report"], json!(true));
    assert_eq!(view["stage"], "in_progress");

    let (_, html) = get_html(&app, 1, player(10)).await;
    for forbidden in ["9人", "9 人", "人が報告", "報告済み:", "件の報告"] {
        assert!(!html.contains(forbidden), "{forbidden}: {html}");
    }
    // 報告の入口は出る
    assert!(html.contains("id=\"report-red\""), "{html}");

    // 報告した本人には「受け付けました」だけ
    assert_eq!(report(&app, 1, 10, "red").await, StatusCode::OK);
    let (_, mine) = get_json(&app, 1, player(3)).await;
    assert_eq!(mine["i_reported"], json!(true));
    db.done().await;
}

/// 10 人一致で即確定。**確定後の画面には自分の増減だけが出る。**
#[sqlx::test(
    fixtures("reference", "app_role"),
    fixtures("../ops/app_privileges.sql")
)]
async fn ten_unanimous_reports_confirm_and_show_my_own_delta(owner: PgPool) {
    let db = AppDb::connect(&owner).await;
    formation_at(&db, 1, Stage::InProgress).await;
    let app = db.router();

    for reporter in 1..=10u128 {
        assert_eq!(report(&app, 1, reporter, "red").await, StatusCode::OK);
    }
    assert_eq!(state_of(&db, 1).await, S::Confirmed);

    // red の勝ち。params v2 で差 50 未満なので player 1 は +7、player 6 は -7
    let (_, red) = get_json(&app, 1, player(1)).await;
    assert_eq!(red["stage"], "confirmed");
    assert_eq!(red["result"], "won");
    assert_eq!(red["my_delta"], json!(7.0));
    let (_, blue) = get_json(&app, 1, player(6)).await;
    assert_eq!(blue["result"], "lost");
    assert_eq!(blue["my_delta"], json!(-7.0));

    let (_, html) = get_html(&app, 1, player(1)).await;
    assert!(html.contains(">+7<"), "{html}");
    assert!(html.contains("href=\"/queue\""), "キューへ戻る導線");
    // 他人の増減は出ない
    assert!(!html.contains(">-7<"), "{html}");
    db.done().await;
}

/// 報告が割れても、期限までは確定しない（既存の判定のまま）。**画面に内訳は出ない。**
///
/// 5-4 のような非対称な報告数は正常系で、多数決は採らない（00 §5）。割れたときの
/// `disputed` は最初の報告から30分後に決まる（その経路は `tests/discord.rs` の
/// `an_elapsed_report_window_is_evaluated_by_the_existing_resume_loop`）。
#[sqlx::test(
    fixtures("reference", "app_role"),
    fixtures("../ops/app_privileges.sql")
)]
async fn split_reports_do_not_confirm_and_never_show_the_breakdown(owner: PgPool) {
    let db = AppDb::connect(&owner).await;
    formation_at(&db, 1, Stage::InProgress).await;
    let app = db.router();

    for reporter in 1..=5u128 {
        assert_eq!(report(&app, 1, reporter, "red").await, StatusCode::OK);
    }
    for reporter in 6..=10u128 {
        assert_eq!(report(&app, 1, reporter, "blue").await, StatusCode::OK);
    }
    // 全員が出していても、どちらの結果も早期確定の閾値（7人）に届かないので期限まで待つ
    assert!(
        matches!(state_of(&db, 1).await, S::InProgress { .. }),
        "{:?}",
        state_of(&db, 1).await
    );

    let (_, view) = get_json(&app, 1, player(1)).await;
    let json = view.to_string();
    for forbidden in [
        "red_votes",
        "blue_votes",
        "breakdown",
        "voters",
        "agreement",
    ] {
        assert!(!json.contains(forbidden), "{forbidden}: {json}");
    }
    // 自分の分だけ分かる
    assert_eq!(view["i_reported"], json!(true));
    assert_eq!(view["actions"]["report"], json!(false));

    // 係争になった後の画面にも内訳を出さない
    db.formations
        .append_event(
            fid(1),
            &NewEvent::new(K::Disputed, None, at(100), "dispute-1"),
        )
        .await
        .unwrap();
    let (_, view) = get_json(&app, 1, player(1)).await;
    assert_eq!(view["stage"], "disputed");
    let (_, html) = get_html(&app, 1, player(1)).await;
    assert!(html.contains("運営が確認しています"), "{html}");
    assert!(!html.contains("赤の勝ち"), "他人の報告内容: {html}");
    assert!(!html.contains("青の勝ち"), "他人の報告内容: {html}");
    db.done().await;
}

/// 全員が無効試合で一致したら nullified（増減なし）。
#[sqlx::test(
    fixtures("reference", "app_role"),
    fixtures("../ops/app_privileges.sql")
)]
async fn unanimous_invalid_reports_end_as_nullified(owner: PgPool) {
    let db = AppDb::connect(&owner).await;
    formation_at(&db, 1, Stage::InProgress).await;
    let app = db.router();

    for reporter in 1..=10u128 {
        assert_eq!(report(&app, 1, reporter, "nullified").await, StatusCode::OK);
    }
    assert_eq!(state_of(&db, 1).await, S::Nullified);
    let (_, view) = get_json(&app, 1, player(1)).await;
    assert_eq!(view["stage"], "nullified");
    // 増減のフィールド自体が無い
    assert!(view.get("my_delta").is_none(), "{view}");
    db.done().await;
}

/// **訂正は追記。確定判定は最新、一致率は最初の報告**（00 §5）。
#[sqlx::test(
    fixtures("reference", "app_role"),
    fixtures("../ops/app_privileges.sql")
)]
async fn a_correction_moves_the_confirmation_but_not_the_agreement_metric(owner: PgPool) {
    let db = AppDb::connect(&owner).await;
    formation_at(&db, 1, Stage::InProgress).await;
    let app = db.router();

    // player 1 が最初 blue と報告し、後から red に訂正する
    assert_eq!(report(&app, 1, 1, "blue").await, StatusCode::OK);
    assert_eq!(report(&app, 1, 1, "red").await, StatusCode::OK);
    for reporter in 2..=10u128 {
        assert_eq!(report(&app, 1, reporter, "red").await, StatusCode::OK);
    }
    // 最新の報告で全員一致 → confirmed
    assert_eq!(state_of(&db, 1).await, S::Confirmed);

    // 一致率は**最初の報告**で測る。player 1 の最初は blue なので 9/10
    let log = db.formations.load_events(fid(1)).await.unwrap();
    let confirmed = log
        .events
        .iter()
        .rev()
        .find(|stored| stored.event.kind == K::Confirmed)
        .expect("confirmed event");
    assert_eq!(
        confirmed.payload["first_report_agreement_matching"],
        json!(9)
    );
    assert_eq!(confirmed.payload["first_report_agreement_total"], json!(10));

    // 訂正で行が消えていない（追記のまま）
    let rows: i64 = sqlx::query_scalar!(
        "select count(*) as \"count!\" from match_reports \
         where formation_id = $1 and player_id = $2",
        fid(1).0,
        player(1).0
    )
    .fetch_one(&db.pool)
    .await
    .unwrap();
    assert_eq!(rows, 2, "訂正は打ち消しではなく追記");
    db.done().await;
}

/// 確定しても、まだ報告していない人には報告の入口が残る。
///
/// 00 §5「報告ゲートの解除条件を『確定』にしない」。ここで入口を閉じると、
/// 未報告のままキューのゲートが開かなくなる。
///
/// **早期確定（閾値 7 人）を入れてから、これは例外ではなく通常になった。**
/// 7 人で確定した時点で 3 人が未報告のまま残るので、その 3 人の入口を閉じると
/// 毎試合 3 人がキューへ戻れなくなる。
#[sqlx::test(
    fixtures("reference", "app_role"),
    fixtures("../ops/app_privileges.sql")
)]
async fn an_unreported_player_can_still_report_after_confirmation(owner: PgPool) {
    let db = AppDb::connect(&owner).await;
    formation_at(&db, 1, Stage::InProgress).await;
    let app = db.router();

    // 6 人では閾値に届かない
    for reporter in 1..=6u128 {
        assert_eq!(report(&app, 1, reporter, "red").await, StatusCode::OK);
    }
    assert!(matches!(state_of(&db, 1).await, S::InProgress { .. }));

    // 7 人目で確定する（残り 3 人を待たない）
    assert_eq!(report(&app, 1, 7, "red").await, StatusCode::OK);
    assert_eq!(state_of(&db, 1).await, S::Confirmed);

    // **残り 3 人は確定後も報告できる。**受理はするが確定は動かさない
    for reporter in 8..=10u128 {
        assert_eq!(
            report(&app, 1, reporter, "blue").await,
            StatusCode::OK,
            "未報告の {reporter} 人目が報告できない"
        );
        assert_eq!(state_of(&db, 1).await, S::Confirmed, "確定が覆っている");
    }
    let (_, view) = get_json(&app, 1, player(10)).await;
    assert_eq!(view["i_reported"], json!(true));
    assert_eq!(view["actions"]["report"], json!(false), "もう出さない");
    db.done().await;
}

/// **7 人で確定した後、既に報告した人の訂正は 409 で断る。**
///
/// 確定は追記された事実であり、後から引き直さない（docs/02 §4）。
/// 「7 人で確定 → 残り 3 人が違う結果 → 6 対 4 だった」という記録はそのまま残す。
#[sqlx::test(
    fixtures("reference", "app_role"),
    fixtures("../ops/app_privileges.sql")
)]
async fn a_correction_after_an_early_confirmation_is_refused(owner: PgPool) {
    let db = AppDb::connect(&owner).await;
    formation_at(&db, 1, Stage::InProgress).await;
    let app = db.router();

    for reporter in 1..=7u128 {
        assert_eq!(report(&app, 1, reporter, "red").await, StatusCode::OK);
    }
    assert_eq!(state_of(&db, 1).await, S::Confirmed, "7 人で確定していない");

    // 報告済みの人が引き直そうとしても 409。行は消さない
    let before = report_rows(&db, 1, player(1)).await;
    let (status, payload) = post(&app, 1, player(1), "/report", json!({ "outcome": "blue" })).await;
    assert_eq!(status, StatusCode::CONFLICT, "{payload}");
    assert_eq!(payload["error"], json!("already_settled"), "{payload}");
    assert_eq!(
        report_rows(&db, 1, player(1)).await,
        before,
        "行が増えている"
    );
    assert_eq!(state_of(&db, 1).await, S::Confirmed);

    // 残り 3 人が逆の結果を出しても確定は動かない（6 対 4 の記録が残るだけ）
    for reporter in 8..=10u128 {
        assert_eq!(report(&app, 1, reporter, "blue").await, StatusCode::OK);
    }
    assert_eq!(state_of(&db, 1).await, S::Confirmed, "確定が覆っている");
    assert_eq!(
        kinds(&db, 1)
            .await
            .iter()
            .filter(|k| **k == K::Confirmed)
            .count(),
        1
    );
    db.done().await;
}

/// 成立直後（VC 割当前）でも画面が出る。
#[sqlx::test(
    fixtures("reference", "app_role"),
    fixtures("../ops/app_privileges.sql")
)]
async fn the_screen_works_from_the_moment_the_match_forms(owner: PgPool) {
    let db = AppDb::connect(&owner).await;
    formation_at(&db, 1, Stage::Matched).await;
    let app = db.router();

    let (status, view) = get_json(&app, 1, player(1)).await;
    assert_eq!(status, StatusCode::OK, "{view}");
    assert_eq!(view["stage"], "matched");
    assert_eq!(view["my_voice"], Value::Null, "VC 未割当");
    assert_eq!(view["actions"]["host_response"], json!(false));
    assert_eq!(view["actions"]["report"], json!(false));
    let (_, html) = get_html(&app, 1, player(1)).await;
    assert!(html.contains("data-stage=\"matched\""), "{html}");
    assert!(html.contains("VC 割り当て中"), "{html}");
    db.done().await;
}

/// 通知に載る URL の起点は、リダイレクト URI から導出する。
#[test]
fn the_site_base_url_matches_the_login_host() {
    assert_eq!(
        site_base_url(&config().redirect_uri),
        "https://match.example.test"
    );
}

// ----------------------------------------------------------------------
// 確定後の訂正
// ----------------------------------------------------------------------

/// **確定した後の訂正は 409 で断る。**受理したのに何も起きない応答を返さない。
///
/// 確定・無効試合は追記された事実であって、後から引き直さない（docs/02 §4）。
/// `evaluate_reports` は `in_progress` 以外では何もしないので、200 で通すと
/// 「受理したのに確定もレートも動かない」状態になる。画面はボタンを隠していたが、
/// API を直接叩けば通っていた。
///
/// **既に追記された行は消さない。**追記専用のまま、新しい訂正を断るだけ。
#[sqlx::test(
    fixtures("reference", "app_role"),
    fixtures("../ops/app_privileges.sql")
)]
async fn a_correction_after_confirmation_is_refused(owner: PgPool) {
    let db = AppDb::connect(&owner).await;
    formation_at(&db, 1, Stage::InProgress).await;
    let app = db.router();

    for reporter in 1..=10u128 {
        assert_eq!(report(&app, 1, reporter, "red").await, StatusCode::OK);
    }
    assert_eq!(state_of(&db, 1).await, S::Confirmed);
    let rows_before = report_rows(&db, 1, player(1)).await;
    assert_eq!(rows_before, 1);

    // 逆の内容を追記しようとしても 409
    let (status, payload) = post(&app, 1, player(1), "/report", json!({ "outcome": "blue" })).await;
    assert_eq!(status, StatusCode::CONFLICT, "{payload}");
    assert_eq!(payload["error"], json!("already_settled"), "{payload}");
    println!("確定後の訂正: {status} {payload}");

    // 同じ内容でも通さない（訂正かどうかで判定しない）
    let (status, _) = post(&app, 1, player(1), "/report", json!({ "outcome": "red" })).await;
    assert_eq!(status, StatusCode::CONFLICT);

    // **行は増えていないし、減ってもいない**
    assert_eq!(report_rows(&db, 1, player(1)).await, rows_before);
    assert_eq!(state_of(&db, 1).await, S::Confirmed);

    db.done().await;
}

/// **まだ報告していない人は、確定した後でも報告できる。**
///
/// ここを閉じると、未報告のままキューのゲートが開かなくなる
/// （00 §5「報告ゲートの解除条件を『確定』にしない」）。
#[sqlx::test(
    fixtures("reference", "app_role"),
    fixtures("../ops/app_privileges.sql")
)]
async fn an_unreported_player_is_still_accepted_after_confirmation(owner: PgPool) {
    let db = AppDb::connect(&owner).await;
    formation_at(&db, 1, Stage::InProgress).await;
    let app = db.router();

    // 9 人が報告 → 期限を待たずには確定しないので、確定イベントを直接積む
    for reporter in 1..=9u128 {
        assert_eq!(report(&app, 1, reporter, "red").await, StatusCode::OK);
    }
    db.formations
        .append_event(
            fid(1),
            &NewEvent::new(K::Confirmed, None, at(30), "confirmed-1")
                .with_payload(json!({ "outcome": "red" })),
        )
        .await
        .unwrap();
    assert_eq!(state_of(&db, 1).await, S::Confirmed);

    // 10 人目（未報告）は通る
    assert_eq!(report(&app, 1, 10, "red").await, StatusCode::OK);
    assert_eq!(report_rows(&db, 1, player(10)).await, 1);
    println!("未報告の10人目は確定後でも 200 で受理される");

    // その 10 人目が続けて訂正しようとすると、今度は 409
    let (status, payload) =
        post(&app, 1, player(10), "/report", json!({ "outcome": "blue" })).await;
    assert_eq!(status, StatusCode::CONFLICT, "{payload}");
    assert_eq!(report_rows(&db, 1, player(10)).await, 1);

    db.done().await;
}

/// 係争中（`disputed`）は訂正を受け付ける。運営の裁定の材料になる。
#[sqlx::test(
    fixtures("reference", "app_role"),
    fixtures("../ops/app_privileges.sql")
)]
async fn a_correction_is_still_accepted_while_disputed(owner: PgPool) {
    let db = AppDb::connect(&owner).await;
    formation_at(&db, 1, Stage::InProgress).await;
    let app = db.router();

    assert_eq!(report(&app, 1, 1, "red").await, StatusCode::OK);
    db.formations
        .append_event(
            fid(1),
            &NewEvent::new(K::Disputed, None, at(30), "disputed-1")
                .with_payload(json!({ "outcome": "disputed" })),
        )
        .await
        .unwrap();
    assert_eq!(state_of(&db, 1).await, S::Disputed);

    assert_eq!(report(&app, 1, 1, "blue").await, StatusCode::OK);
    assert_eq!(report_rows(&db, 1, player(1)).await, 2, "訂正は追記");
    println!("係争中の訂正は受理される（追記され、裁定の材料になる）");

    db.done().await;
}

/// あるプレイヤーの `match_reports` の行数。
async fn report_rows(db: &AppDb, n: u128, who: PlayerId) -> i64 {
    sqlx::query_scalar!(
        "select count(*) as \"count!\" from match_reports \
         where formation_id = $1 and player_id = $2",
        fid(n).0,
        who.0
    )
    .fetch_one(&db.pool)
    .await
    .unwrap()
}

// ----------------------------------------------------------------------
// 報告完了 → キュー画面（行き先はサーバが返す）
// ----------------------------------------------------------------------

/// **本人の報告が追記された時点で**キュー画面へ戻す。確定は待たない。
///
/// 行き先はサーバの応答（`next`）で返す。クライアントに遷移条件もタイマーも持たせない。
/// 戻した後も結果を追えるよう、`reported` を付けてマッチ画面への導線を残す。
#[sqlx::test(
    fixtures("reference", "app_role"),
    fixtures("../ops/app_privileges.sql")
)]
async fn a_report_sends_the_player_back_to_the_queue(owner: PgPool) {
    let db = AppDb::connect(&owner).await;
    formation_at(&db, 1, Stage::InProgress).await;
    let app = db.router();

    let (status, payload) = post(&app, 1, player(1), "/report", json!({ "outcome": "red" })).await;
    assert_eq!(status, StatusCode::OK, "{payload}");
    assert_eq!(payload["ok"], json!(true));
    assert_eq!(
        payload["next"],
        json!(format!("/queue?reported={}", fid(1).0)),
        "行き先をサーバが返していない: {payload}"
    );
    println!("報告の応答: {payload}");

    // この時点で試合はまだ未確定（他の9人が未報告）
    assert!(matches!(state_of(&db, 1).await, S::InProgress { .. }));

    // **戻った後もマッチ画面は開ける。**確定・disputed・裁定はここで確認する
    let (status, view) = get_json(&app, 1, player(1)).await;
    assert_eq!(status, StatusCode::OK, "{view}");
    assert_eq!(view["i_reported"], json!(true));
    assert_eq!(view["actions"]["report"], json!(false));

    // 残り9人が報告して確定 → 同じ URL で結果が読める
    for reporter in 2..=10u128 {
        assert_eq!(report(&app, 1, reporter, "red").await, StatusCode::OK);
    }
    assert_eq!(state_of(&db, 1).await, S::Confirmed);
    let (status, view) = get_json(&app, 1, player(1)).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(view["stage"], "confirmed", "{view}");
    println!("遷移後に開き直したマッチ画面: stage={} ", view["stage"]);

    // disputed も同じ経路で読める（/status には確定した試合しか出ない）
    formation_at(&db, 2, Stage::InProgress).await;
    db.formations
        .append_event(
            fid(2),
            &NewEvent::new(K::Disputed, None, at(20), "disputed-2")
                .with_payload(json!({ "outcome": "disputed" })),
        )
        .await
        .unwrap();
    assert_eq!(state_of(&db, 2).await, S::Disputed);
    let (status, view) = get_json(&app, 2, player(1)).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(view["stage"], "disputed", "{view}");
    println!(
        "係争になった試合も同じ URL で読める: stage={}",
        view["stage"]
    );

    db.done().await;
}

// ----------------------------------------------------------------------
// カードのアイコンと、相手チームの送信順
// ----------------------------------------------------------------------

/// レートを追記で作る（レートは導出値なので現在値のテーブルには書かない）。
async fn adjust(db: &AppDb, who: PlayerId, delta_mu: f64) {
    sqlx::query(
        "insert into rating_adjustments \
         (player_id, delta_mu, delta_sigma, reason, created_by, created_at, idempotency_key) \
         values ($1, $2, 0, 'test seed', 'staff:test', now(), $3)",
    )
    .bind(who.0)
    .bind(delta_mu)
    .bind(format!("card-{}", who.0))
    .execute(&db.pool)
    .await
    .expect("append a rating adjustment");
}

/// Discord のスナップショットを積む。
async fn snapshot(db: &AppDb, who: PlayerId, name: &str, avatar: Option<&str>) {
    sqlx::query(
        "insert into player_discord_snapshots (player_id, display_name, avatar_hash, captured_at) \
         values ($1, $2, $3, now())",
    )
    .bind(who.0)
    .bind(name)
    .bind(avatar)
    .execute(&db.pool)
    .await
    .expect("record a snapshot");
}

/// **相手チームは必ずレート降順で送る。**
///
/// `roster` はロールの表示順（上キャリー → サポート → 中央 → 下キャリー → タンク）で
/// 返すので、そのまま送るとラベルが無くても並び順から構成が読める。
#[sqlx::test(
    fixtures("reference", "app_role"),
    fixtures("../ops/app_privileges.sql")
)]
async fn the_opposing_team_is_sent_in_rating_order(owner: PgPool) {
    let db = AppDb::connect(&owner).await;
    formation_at(&db, 1, Stage::InProgress).await;
    let app = db.router();

    // blue（相手）は player(6)..player(10)。**ロールの表示順とレートの順を食い違わせる**
    // ロール順:  6=サポート 7=中央 8=下キャリー 9=タンク 10=上キャリー
    // レート順: 10 > 7 > 9 > 6 > 8 になるように振る
    adjust(&db, player(6), -20.0).await; // 1480
    adjust(&db, player(7), 30.0).await; //  1530
    adjust(&db, player(8), -40.0).await; // 1460
    adjust(&db, player(9), 10.0).await; //  1510
    adjust(&db, player(10), 60.0).await; // 1560
    for n in 6..=10u128 {
        snapshot(&db, player(n), &format!("相手{n}"), Some("abc123")).await;
    }

    let (status, view) = get_json(&app, 1, player(1)).await;
    assert_eq!(status, StatusCode::OK);

    println!("=== 相手チームのカード（サーバが返した JSON そのまま）");
    println!("{:#}", view["opponents"]);

    let opponents = view["opponents"].as_array().unwrap();
    let ratings: Vec<f64> = opponents
        .iter()
        .map(|row| row["rating"].as_f64().unwrap())
        .collect();
    assert_eq!(
        ratings,
        vec![1560.0, 1530.0, 1510.0, 1480.0, 1460.0],
        "レート降順で送っていない"
    );
    // ロールの表示順（6,7,8,9,10）とは違う並びになっている
    let names: Vec<&str> = opponents
        .iter()
        .map(|row| row["discord_name"].as_str().unwrap())
        .collect();
    assert_eq!(names, vec!["相手10", "相手7", "相手9", "相手6", "相手8"]);
    println!("送信順: {names:?}（ロールの表示順は 相手6,7,8,9,10）");

    // **ロールに相当するキーが1つも無い**
    for row in opponents {
        for key in row.as_object().unwrap().keys() {
            assert!(!key.contains("role"), "ロールのキーがある: {row}");
        }
        assert!(
            row["avatar_url"]
                .as_str()
                .unwrap()
                .contains("cdn.discordapp.com")
        );
    }
    // 相手のカードにプロフィールへのリンクを張らない
    let (_, page) = get_html(&app, 1, player(1)).await;
    let opponent_section = page
        .split("id=\"opponent-title\"")
        .nth(1)
        .expect("opponent section")
        .split("</section>")
        .next()
        .unwrap();
    assert!(
        !opponent_section.contains("/players/"),
        "相手のカードにプロフィールへの導線がある: {opponent_section}"
    );

    db.done().await;
}

/// 自チームはロールが出る。アイコンと表示名も両チームに出る。
#[sqlx::test(
    fixtures("reference", "app_role"),
    fixtures("../ops/app_privileges.sql")
)]
async fn both_teams_carry_an_icon_and_a_name(owner: PgPool) {
    let db = AppDb::connect(&owner).await;
    formation_at(&db, 1, Stage::InProgress).await;
    let app = db.router();
    // 5 番だけアバター未設定にして、既定アバターに落ちることを見る
    for n in 1..=10u128 {
        let avatar = if n == 5 { None } else { Some("abc123") };
        snapshot(&db, player(n), &format!("名前{n}"), avatar).await;
    }

    let (_, view) = get_json(&app, 1, player(1)).await;
    println!("=== 自チームのカード");
    println!("{:#}", view["teammates"]);

    for row in view["teammates"].as_array().unwrap() {
        assert!(row["role"].is_string(), "自チームにロールが無い: {row}");
        assert!(row["discord_name"].is_string(), "{row}");
        assert!(row["avatar_url"].as_str().unwrap().starts_with("https://"));
    }
    // アバター未設定の人は既定アバター
    let fallback = view["teammates"]
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["discord_name"] == "名前5")
        .expect("player 5");
    assert!(
        fallback["avatar_url"]
            .as_str()
            .unwrap()
            .starts_with("https://cdn.discordapp.com/embed/avatars/"),
        "{fallback}"
    );
    println!("アバター未設定の人の URL: {}", fallback["avatar_url"]);

    // 「Best（最高レート）」の列は足さない
    let body = serde_json::to_string(&view).unwrap();
    for forbidden in ["best", "highest", "peak"] {
        assert!(
            !body.contains(forbidden),
            "{forbidden} が入っている: {body}"
        );
    }
    // チーム名は red / blue のまま
    assert!(["red", "blue"].contains(&view["my_team"].as_str().unwrap()));

    db.done().await;
}

/// **同レートの並びが安定していること。**
///
/// 開幕直後は 1500 と 1600 の 2 値しかないので、相手チームのほぼ全員が同レートになる。
/// 10 秒ポーリングのたびに並びが変わると読みづらいので、同率は `player_id` で決める。
#[sqlx::test(
    fixtures("reference", "app_role"),
    fixtures("../ops/app_privileges.sql")
)]
async fn equal_ratings_keep_a_stable_order(owner: PgPool) {
    let db = AppDb::connect(&owner).await;
    formation_at(&db, 1, Stage::InProgress).await;
    let app = db.router();
    // blue（相手）の 5 人を全員同じレートにする（開幕直後の 1500 揃いと同じ形）
    for n in 6..=10u128 {
        snapshot(&db, player(n), &format!("相手{n}"), Some("abc123")).await;
    }

    let (_, first) = get_json(&app, 1, player(1)).await;
    let order: Vec<String> = first["opponents"]
        .as_array()
        .unwrap()
        .iter()
        .map(|row| row["player_id"].as_str().unwrap().to_owned())
        .collect();
    let ratings: Vec<f64> = first["opponents"]
        .as_array()
        .unwrap()
        .iter()
        .map(|row| row["rating"].as_f64().unwrap())
        .collect();
    assert!(
        ratings.windows(2).all(|pair| pair[0] == pair[1]),
        "全員同レートの前提が崩れている: {ratings:?}"
    );
    println!("全員同レート（{}）のときの送信順:", ratings[0]);
    for id in &order {
        println!("  {id}");
    }

    // **10 回取り直しても同じ順**（ポーリングのたびに揺れない）
    for attempt in 1..=10 {
        let (_, again) = get_json(&app, 1, player(1)).await;
        let repeated: Vec<String> = again["opponents"]
            .as_array()
            .unwrap()
            .iter()
            .map(|row| row["player_id"].as_str().unwrap().to_owned())
            .collect();
        assert_eq!(repeated, order, "{attempt} 回目で並びが変わった");
    }
    // player_id の昇順（同率のタイブレーク）
    let mut sorted = order.clone();
    sorted.sort();
    assert_eq!(order, sorted, "同率のタイブレークが player_id 順でない");
    println!("10 回取り直しても同じ順（player_id 昇順）");

    // 自チームは**ロール順のまま**（レート順にしない）
    let roles: Vec<&str> = first["teammates"]
        .as_array()
        .unwrap()
        .iter()
        .map(|row| row["role"].as_str().unwrap())
        .collect();
    assert_eq!(
        roles,
        vec!["上キャリー", "サポート", "中央", "下キャリー", "タンク"],
        "自チームがロール順でない"
    );
    println!("自チームの並び: {roles:?}（ロール順のまま）");

    db.done().await;
}
