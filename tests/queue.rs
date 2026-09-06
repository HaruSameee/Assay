//! キューの統合テスト。実際の PostgreSQL を使う。
//!
//! リポジトリは所有者ではなく**アプリ用ロール**（`assay_app`）で接続する。所有者権限で
//! 走らせると、権限の配線ミスが本番でしか出ない。権限は本番と同じ
//! `ops/app_privileges.sql` を適用して作る（キューは可変テーブルなので、その許可リストに
//! 載っていなければここで 42501 になる）。
//!
//! 検証すること:
//! - 二重キューを `queue_entries.player_id` の unique が拒否する
//! - 下方向のマッチング範囲が 95〜200 の外なら CHECK が拒否する
//! - 登録ゲートと報告ゲートの拒否が**畳まれず別々に**返る
//! - 「範囲内で N 人」が**相互条件**で数えられる（片方向なら通ってしまうケースを含む）
//! - 既にキュー中なら新規作成ではなく設定の更新。`enqueued_at` は動かない
//! - 他人のエントリを操作できない

use std::time::Duration;

use axum::Router;
use axum::body::Body;
use axum::http::{Request, Response, StatusCode, header};
use axum::response::IntoResponse;
use axum_extra::extract::SignedCookieJar;
use chrono::{TimeZone, Utc};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use sqlx::PgPool;
use sqlx::postgres::PgPoolOptions;
use tower::ServiceExt;
use unite_matchsystem::lifecycle::{FormationEventKind as K, FormationId, PlayerId, Timestamp};
use unite_matchsystem::persistence::{
    FormationRepository, NewEvent, NewFormation, NewFormationPlayer, NewReport, Outcome, Team,
};
use unite_matchsystem::queue::{QueueRepository, QueueState, router};
use unite_matchsystem::registration::session::{SESSION_COOKIE, Session, set_session};
use unite_matchsystem::registration::{AuthConfig, NewDeclaration, Rank, RegistrationRepository};
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

// ----------------------------------------------------------------------
// 道具
// ----------------------------------------------------------------------

fn at(seconds: i64) -> Timestamp {
    Utc.with_ymd_and_hms(2026, 9, 12, 21, 0, 0).unwrap() + chrono::TimeDelta::seconds(seconds)
}

fn player(n: u128) -> PlayerId {
    PlayerId(Uuid::from_u128(n))
}

fn fid(n: u128) -> FormationId {
    FormationId(Uuid::from_u128(n))
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

/// テスト DB にアプリ用ロールで接続したプール。
struct AppDb {
    pool: PgPool,
    queue: QueueRepository,
    registration: RegistrationRepository,
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
            queue: QueueRepository::new(pool.clone()),
            registration: RegistrationRepository::new(pool.clone()),
            formations: FormationRepository::new(pool.clone()),
            pool,
        }
    }

    fn router(&self) -> Router {
        router(QueueState::new(
            config(),
            self.registration.clone(),
            self.queue.clone(),
        ))
    }

    async fn done(self) {
        self.pool.close().await;
    }
}

/// 署名済みのセッション Cookie（`<名前>=<値>`）を作る。
///
/// OAuth を通さずにログイン済みの状態を作る。Cookie に入るのは識別子と発行時刻だけなので、
/// これは本物のログインと同じ材料でしかない。
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

async fn send(app: &Router, request: Request<Body>) -> Response<Body> {
    app.clone().oneshot(request).await.unwrap()
}

async fn get_queue(app: &Router, as_player: Option<PlayerId>) -> (StatusCode, Value) {
    let mut builder = Request::get("/queue");
    if let Some(player_id) = as_player {
        builder = builder.header(header::COOKIE, session_cookie(player_id));
    }
    let response = send(app, builder.body(Body::empty()).unwrap()).await;
    json_body(response).await
}

async fn post_queue(app: &Router, as_player: PlayerId, body: Value) -> (StatusCode, Value) {
    let request = Request::post("/queue")
        .header(header::COOKIE, session_cookie(as_player))
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(body.to_string()))
        .unwrap();
    json_body(send(app, request).await).await
}

async fn delete_queue(app: &Router, as_player: PlayerId) -> (StatusCode, Value) {
    let request = Request::delete("/queue")
        .header(header::COOKIE, session_cookie(as_player))
        .body(Body::empty())
        .unwrap();
    json_body(send(app, request).await).await
}

async fn json_body(response: Response<Body>) -> (StatusCode, Value) {
    let status = response.status();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    let value = if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes).expect("a JSON body")
    };
    (status, value)
}

/// 希望ロール1つ分の JSON。
fn wish(role: &str, priority: i16) -> Value {
    json!({ "role": role, "priority": priority })
}

/// 登録を済ませる（登録ゲートを通す）。
async fn register(db: &AppDb, player_id: PlayerId, rank: Rank) {
    db.registration
        .register(&NewDeclaration {
            player_id,
            declared_at: at(0),
            trainer_name: format!("トレーナー{}", player_id.0.as_u128()),
            rank,
            rating: Some(1500),
            raw: json!({ "rank": rank.as_str() }),
        })
        .await
        .expect("record the declaration");
}

/// 表示スケールのレートを `rating_adjustments` への追記で作る。
///
/// レートは導出値なので、現在値のテーブルには書かない（書ける列も無い）。
async fn adjust_rating(db: &AppDb, player_id: PlayerId, delta_mu: f64) {
    sqlx::query(
        "insert into rating_adjustments \
         (player_id, delta_mu, delta_sigma, reason, created_by, created_at, idempotency_key) \
         values ($1, $2, 0, 'test seed', 'staff:test', now(), $3)",
    )
    .bind(player_id.0)
    .bind(delta_mu)
    .bind(format!("seed-{}", player_id.0))
    .execute(&db.pool)
    .await
    .expect("append a rating adjustment");
}

fn formation(n: u128, subject: PlayerId) -> NewFormation {
    let mut players: Vec<NewFormationPlayer> = (2..=10)
        .map(|i| NewFormationPlayer {
            player_id: player(i),
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
            player_id: subject,
            team: Team::Red,
            role: ROLES[0].to_owned(),
            role_was_flexed: false,
            mu_at_match: 25.0,
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

/// 報告義務のある状態（`in_progress`）まで進めて、報告ゲートを立てる。
async fn unreported_formation(db: &AppDb, n: u128, subject: PlayerId) {
    db.formations
        .create_formation(&formation(n, subject))
        .await
        .unwrap();
    let host = player(2);
    for event in [
        NewEvent::vc_assigned("1", 101, 102, at(0), format!("vc-{n}")),
        NewEvent::new(K::HostAssigned, Some(host), at(1), format!("assign-{n}")),
        NewEvent::new(K::HostAccepted, Some(host), at(2), format!("accept-{n}")),
        NewEvent::code_posted(host, "1234", at(3), format!("code-{n}")),
    ] {
        db.formations.append_event(fid(n), &event).await.unwrap();
    }
}

// ----------------------------------------------------------------------
// データモデルの制約
// ----------------------------------------------------------------------

/// 二重キューは DB が拒否する。アプリ側の「既にキュー中か」の判定に頼らない。
#[sqlx::test(
    fixtures("reference", "app_role"),
    fixtures("../ops/app_privileges.sql")
)]
async fn a_player_cannot_hold_two_queue_entries(owner: PgPool) {
    let db = AppDb::connect(&owner).await;
    let me = player(1);

    sqlx::query("insert into queue_entries (id, player_id, enqueued_at, updated_at, down_range) values ($1, $2, now(), now(), 95)")
        .bind(Uuid::new_v4())
        .bind(me.0)
        .execute(&db.pool)
        .await
        .expect("the first entry is accepted");

    let error = sqlx::query("insert into queue_entries (id, player_id, enqueued_at, updated_at, down_range) values ($1, $2, now(), now(), 150)")
        .bind(Uuid::new_v4())
        .bind(me.0)
        .execute(&db.pool)
        .await
        .expect_err("a second entry for the same player must be rejected");
    assert_eq!(
        error.as_database_error().unwrap().code().as_deref(),
        Some("23505"),
        "{error}"
    );

    db.done().await;
}

/// 下方向のマッチング範囲は 95〜200（07 §3）。CHECK で強制する。
#[sqlx::test(
    fixtures("reference", "app_role"),
    fixtures("../ops/app_privileges.sql")
)]
async fn the_downward_range_is_bounded_by_a_check_constraint(owner: PgPool) {
    let db = AppDb::connect(&owner).await;

    for (index, down_range) in [95, 200].into_iter().enumerate() {
        sqlx::query("insert into queue_entries (id, player_id, enqueued_at, updated_at, down_range) values ($1, $2, now(), now(), $3)")
            .bind(Uuid::new_v4())
            .bind(player(index as u128 + 1).0)
            .bind(down_range)
            .execute(&db.pool)
            .await
            .unwrap_or_else(|error| panic!("{down_range} must be accepted: {error}"));
    }

    for down_range in [94, 201, 0, -1] {
        let error = sqlx::query("insert into queue_entries (id, player_id, enqueued_at, updated_at, down_range) values ($1, $2, now(), now(), $3)")
            .bind(Uuid::new_v4())
            .bind(player(9).0)
            .bind(down_range)
            .execute(&db.pool)
            .await
            .unwrap_err();
        assert_eq!(
            error.as_database_error().unwrap().code().as_deref(),
            Some("23514"),
            "{down_range} must be rejected by the CHECK: {error}"
        );
    }

    db.done().await;
}

/// 希望ロールは同一エントリ内で role も priority も一意。
#[sqlx::test(
    fixtures("reference", "app_role"),
    fixtures("../ops/app_privileges.sql")
)]
async fn role_preferences_are_unique_per_entry(owner: PgPool) {
    let db = AppDb::connect(&owner).await;
    let entry = Uuid::new_v4();
    sqlx::query("insert into queue_entries (id, player_id, enqueued_at, updated_at, down_range) values ($1, $2, now(), now(), 95)")
        .bind(entry)
        .bind(player(1).0)
        .execute(&db.pool)
        .await
        .unwrap();
    sqlx::query(
        "insert into queue_entry_roles (entry_id, role, priority) values ($1, 'jungle', 1)",
    )
    .bind(entry)
    .execute(&db.pool)
    .await
    .unwrap();

    // 同じロールを2回
    let duplicate_role = sqlx::query(
        "insert into queue_entry_roles (entry_id, role, priority) values ($1, 'jungle', 2)",
    )
    .bind(entry)
    .execute(&db.pool)
    .await
    .unwrap_err();
    assert_eq!(
        duplicate_role
            .as_database_error()
            .unwrap()
            .code()
            .as_deref(),
        Some("23505")
    );

    // 同じ優先度を2回
    let duplicate_priority = sqlx::query(
        "insert into queue_entry_roles (entry_id, role, priority) values ($1, 'bot_tank', 1)",
    )
    .bind(entry)
    .execute(&db.pool)
    .await
    .unwrap_err();
    assert_eq!(
        duplicate_priority
            .as_database_error()
            .unwrap()
            .code()
            .as_deref(),
        Some("23505")
    );

    db.done().await;
}

// ----------------------------------------------------------------------
// エンドポイント
// ----------------------------------------------------------------------

/// **2つのゲートを畳まない。**それぞれ単独でも、両方同時でも、理由が別々に返る。
#[sqlx::test(
    fixtures("reference", "app_role"),
    fixtures("../ops/app_privileges.sql")
)]
async fn the_registration_and_report_gates_are_reported_separately(owner: PgPool) {
    let db = AppDb::connect(&owner).await;
    let app = db.router();
    let body = json!({ "down_range": 95, "roles": [wish("jungle", 1)] });

    // 登録ゲートだけ: ログイン済みだが未登録
    let unregistered = player(1);
    let (status, denied) = post_queue(&app, unregistered, body.clone()).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(denied["error"], "queue_blocked");
    assert_eq!(denied["registration"]["code"], "not_registered");
    assert_eq!(denied["pending_reports"], json!([]), "report gate is clear");

    // 報告ゲートだけ: 登録済みのレジェンドに未報告の formation がある
    let reporter = player(11);
    register(&db, reporter, Rank::Legend).await;
    unreported_formation(&db, 0xf1, reporter).await;
    db.formations
        .append_report(&NewReport {
            formation_id: fid(0xf1),
            player_id: player(2),
            reported_at: at(10),
            outcome: Outcome::Win(Team::Blue),
            raw_result: json!({ "source": "other-player" }),
            idempotency_key: "queue-blind-other-report".into(),
            supersedes_id: None,
        })
        .await
        .unwrap();
    let (status, denied) = post_queue(&app, reporter, body.clone()).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(
        denied["registration"],
        Value::Null,
        "registration gate is clear"
    );
    assert_eq!(
        denied["pending_reports"].as_array().unwrap().len(),
        1,
        "{denied}"
    );
    let response = denied.to_string();
    for private in [
        "outcome",
        "blue",
        "other-player",
        "report_count",
        "reported_by",
    ] {
        assert!(
            !response.contains(private),
            "private report data leaked: {response}"
        );
    }

    // 両方: 理由が2つとも残る（1つの真偽値に畳まれない）
    let both = player(12);
    register(&db, both, Rank::Master).await;
    unreported_formation(&db, 0xf2, both).await;
    let (status, denied) = post_queue(&app, both, body).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(denied["registration"]["code"], "rank_below_entry");
    assert_eq!(denied["registration"]["declared"], "master");
    assert_eq!(denied["pending_reports"].as_array().unwrap().len(), 1);
    assert_eq!(
        denied["messages"].as_array().unwrap().len(),
        2,
        "both reasons are explained: {denied}"
    );

    // どのケースでもキューには入っていない
    for blocked in [unregistered, reporter, both] {
        assert!(db.queue.find(blocked).await.unwrap().is_none());
    }

    db.done().await;
}

/// 投入・状態取得・離脱の一周。
#[sqlx::test(
    fixtures("reference", "app_role"),
    fixtures("../ops/app_privileges.sql")
)]
async fn joining_reading_and_leaving_the_queue(owner: PgPool) {
    let db = AppDb::connect(&owner).await;
    let app = db.router();
    let me = player(1);
    register(&db, me, Rank::Legend).await;

    // 入る前
    let (status, before) = get_queue(&app, Some(me)).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(before["queued"], false);
    assert_eq!(before["entry"], Value::Null);
    assert_eq!(
        before["in_range"],
        Value::Null,
        "the range is the player's own setting, so it is undefined before joining"
    );

    // 投入
    let (status, joined) = post_queue(
        &app,
        me,
        json!({
            "down_range": 150,
            "roles": [wish("jungle", 1), wish("bot_tank", 2)],
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{joined}");
    assert_eq!(joined["queued"], true);
    assert_eq!(joined["entry"]["down_range"], 150);
    assert_eq!(joined["entry"]["up_range"], 95, "upward range is fixed");
    assert_eq!(joined["entry"]["player_id"], me.0.to_string());
    assert_eq!(joined["entry"]["roles"][0]["role"], "jungle");
    assert_eq!(joined["entry"]["roles"][1]["role"], "bot_tank");
    assert_eq!(joined["in_range"], 0, "alone in the queue");

    // 状態取得
    let (status, read) = get_queue(&app, Some(me)).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(read["entry"]["id"], joined["entry"]["id"]);

    // 離脱。従属する希望ロールも消える
    let (status, left) = delete_queue(&app, me).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(left["was_queued"], true);
    let remaining_roles: i64 = sqlx::query_scalar("select count(*) from queue_entry_roles")
        .fetch_one(&db.pool)
        .await
        .unwrap();
    assert_eq!(remaining_roles, 0, "role rows cascade with the entry");

    // 離脱の再送は「既に抜けている」で成功
    let (status, again) = delete_queue(&app, me).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(again["was_queued"], false);

    db.done().await;
}

/// 既にキュー中なら新規作成ではなく**設定の更新**。待ち時間はリセットしない。
#[sqlx::test(
    fixtures("reference", "app_role"),
    fixtures("../ops/app_privileges.sql")
)]
async fn queueing_again_updates_the_settings_without_resetting_the_wait(owner: PgPool) {
    let db = AppDb::connect(&owner).await;
    let app = db.router();
    let me = player(1);
    register(&db, me, Rank::Legend).await;

    let (_, first) = post_queue(
        &app,
        me,
        json!({ "down_range": 95, "roles": [wish("jungle", 1)] }),
    )
    .await;
    let (_, second) = post_queue(
        &app,
        me,
        json!({
            "down_range": 200,
            "roles": [wish("bot_tank", 1), wish("top_support", 2)],
        }),
    )
    .await;

    assert_eq!(second["entry"]["id"], first["entry"]["id"], "same entry");
    assert_eq!(
        second["entry"]["enqueued_at"], first["entry"]["enqueued_at"],
        "widening the range must not push the player to the back of the queue"
    );
    assert_eq!(second["entry"]["down_range"], 200);
    let roles: Vec<&str> = second["entry"]["roles"]
        .as_array()
        .unwrap()
        .iter()
        .map(|role| role["role"].as_str().unwrap())
        .collect();
    assert_eq!(roles, ["bot_tank", "top_support"], "roles are replaced");

    let entries: i64 = sqlx::query_scalar("select count(*) from queue_entries")
        .fetch_one(&db.pool)
        .await
        .unwrap();
    assert_eq!(entries, 1, "an update, not a second entry");

    db.done().await;
}

/// 不正な設定は DB を触る前に 400 で落ちる。
#[sqlx::test(
    fixtures("reference", "app_role"),
    fixtures("../ops/app_privileges.sql")
)]
async fn malformed_settings_are_rejected(owner: PgPool) {
    let db = AppDb::connect(&owner).await;
    let app = db.router();
    let me = player(1);
    register(&db, me, Rank::Legend).await;

    for (body, expected) in [
        (
            json!({ "down_range": 94, "roles": [wish("jungle", 1)] }),
            "down_range_out_of_bounds",
        ),
        (
            json!({ "down_range": 201, "roles": [wish("jungle", 1)] }),
            "down_range_out_of_bounds",
        ),
        (json!({ "down_range": 95, "roles": [] }), "no_roles"),
        (
            json!({
                "down_range": 95,
                "roles": [wish("jungle", 1), wish("bot_tank", 2), wish("top_carry", 3), wish("bot_carry", 4)],
            }),
            "too_many_roles",
        ),
        (
            json!({ "down_range": 95, "roles": [wish("jungle", 1), wish("jungle", 2)] }),
            "duplicate_role",
        ),
        (
            json!({ "down_range": 95, "roles": [wish("jungle", 2)] }),
            "malformed_priorities",
        ),
        // 存在しないロールは DB の FK が拒否し、それを 400 に写す（一覧を Rust 側に複製しない）
        (
            json!({ "down_range": 95, "roles": [wish("midlane", 1)] }),
            "unknown_role",
        ),
    ] {
        let (status, error) = post_queue(&app, me, body.clone()).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
        assert_eq!(error["error"], expected, "{body}");
    }

    assert!(
        db.queue.find(me).await.unwrap().is_none(),
        "nothing was written"
    );
    db.done().await;
}

/// 他人のエントリは触れない。`player_id` はセッションからしか来ない。
#[sqlx::test(
    fixtures("reference", "app_role"),
    fixtures("../ops/app_privileges.sql")
)]
async fn a_session_can_only_touch_its_own_entry(owner: PgPool) {
    let db = AppDb::connect(&owner).await;
    let app = db.router();
    let me = player(1);
    let someone_else = player(2);
    register(&db, me, Rank::Legend).await;
    register(&db, someone_else, Rank::Legend).await;

    post_queue(
        &app,
        me,
        json!({ "down_range": 95, "roles": [wish("jungle", 1)] }),
    )
    .await;

    // ボディに他人の player_id を書いても無視される（そもそも読まない）
    let (status, theirs) = post_queue(
        &app,
        someone_else,
        json!({
            "player_id": me.0.to_string(),
            "down_range": 200,
            "roles": [wish("bot_tank", 1)],
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(theirs["entry"]["player_id"], someone_else.0.to_string());

    // 私のエントリは無傷
    let mine = db.queue.find(me).await.unwrap().expect("still queued");
    assert_eq!(mine.down_range, 95);
    assert_eq!(mine.roles[0].role, "jungle");

    // 他人が離脱しても私は残る
    delete_queue(&app, someone_else).await;
    assert!(db.queue.find(me).await.unwrap().is_some());

    // セッションが無ければ 401（リダイレクトではなく JSON）
    let (status, error) = get_queue(&app, None).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(error["error"], "unauthenticated");

    db.done().await;
}

// ----------------------------------------------------------------------
// 「範囲内で N 人」
// ----------------------------------------------------------------------

/// **相互条件で数える**（07 §3-1）。片方向なら通ってしまうケースを含む。
#[sqlx::test(
    fixtures("reference", "app_role"),
    fixtures("../ops/app_privileges.sql")
)]
async fn the_in_range_count_requires_both_directions(owner: PgPool) {
    let db = AppDb::connect(&owner).await;
    let app = db.router();

    // レートは導出値。rating_adjustments への追記で作る（表示スケール、既定は 1500）
    let wide = player(1); // 1600、下方向 200 まで広げた
    let below = player(2); // 1450、下方向は既定の 95
    let near = player(3); // 1520、下方向は既定の 95
    adjust_rating(&db, wide, 100.0).await;
    adjust_rating(&db, below, -50.0).await;
    adjust_rating(&db, near, 20.0).await;
    for (who, down_range) in [(wide, 200), (below, 95), (near, 95)] {
        register(&db, who, Rank::Legend).await;
        let (status, _) = post_queue(
            &app,
            who,
            json!({ "down_range": down_range, "roles": [wish("jungle", 1)] }),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
    }

    // wide から見れば below は下方向 200 の内（1600 - 200 = 1400 ≦ 1450）。
    // だが below から見ると wide は上方向 +95 の外（1450 + 95 = 1545 < 1600）。
    // 片方向で数えると 2 になるところを、相互条件では 1 にしなければならない。
    let (status, status_body) = get_queue(&app, Some(wide)).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        status_body["in_range"], 1,
        "only the player who also accepts us counts: {status_body}"
    );

    // 相互条件は対称。below から見ても wide は数えない（near だけ）
    let (_, from_below) = get_queue(&app, Some(below)).await;
    assert_eq!(from_below["in_range"], 1, "{from_below}");
    // near は両方と両立する
    let (_, from_near) = get_queue(&app, Some(near)).await;
    assert_eq!(from_near["in_range"], 2, "{from_near}");

    // 自分自身は数えない
    assert_eq!(db.queue.in_range_count(wide).await.unwrap(), Some(1));

    // below が下方向を広げても、上方向 +95 は固定なので wide とは両立しない
    post_queue(
        &app,
        below,
        json!({ "down_range": 200, "roles": [wish("jungle", 1)] }),
    )
    .await;
    let (_, still) = get_queue(&app, Some(wide)).await;
    assert_eq!(
        still["in_range"], 1,
        "the upward cap cannot be widened by either side: {still}"
    );

    db.done().await;
}
