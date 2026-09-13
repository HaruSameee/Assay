//! キューの統合テスト。実際の PostgreSQL を使う。
//!
//! リポジトリは所有者ではなく**アプリ用ロール**（`assay_app`）で接続する。所有者権限で
//! 走らせると、権限の配線ミスが本番でしか出ない。権限は本番と同じ
//! `ops/app_privileges.sql` を適用して作る（キューは可変テーブルなので、その許可リストに
//! 載っていなければここで 42501 になる）。
//!
//! 検証すること:
//! - 二重キューを `queue_entries.player_id` の unique が拒否する
//! - マッチング範囲は運営だけが変えられる（既定 50/50）。CHECK は 10〜500 を通し、範囲外の down_range を拒否する（旧仕様の値は読み出しで現在の幅に丸める）
//! - 登録ゲート・進行中ゲート・報告ゲートの拒否が**畳まれず別々に**返る
//! - 成立済みでまだ始まっていない formation を持つ人はキューに入れない（二重割当を防ぐ）
//! - 「範囲内で N 人」が差ちょうど 50 を含めて数えられる（境界は `<=`）
//! - 既にキュー中なら新規作成ではなく設定の更新。`enqueued_at` は動かない
//! - 他人のエントリを操作できない

use std::time::Duration;

use axum::Router;
use axum::body::Body;
use axum::http::{Request, Response, StatusCode, header};
use axum::response::IntoResponse;
use axum_extra::extract::SignedCookieJar;
use chrono::{TimeZone, Timelike, Utc};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use sqlx::PgPool;
use sqlx::postgres::PgPoolOptions;
use tower::ServiceExt;
use unite_matchsystem::admin::{
    AdminRepository, BanSubject, NewShadowBan, ShadowBanKind, parse_admin_ids,
};
use unite_matchsystem::lifecycle::{FormationEventKind as K, FormationId, PlayerId, Timestamp};
use unite_matchsystem::persistence::{
    FormationRepository, NewEvent, NewFormation, NewFormationPlayer, NewReport, Outcome, Team,
};
use unite_matchsystem::queue::model::{DEFAULT_DOWN_RANGE, QueueRequest, RolePreference};
use unite_matchsystem::queue::{QueueError, QueueRepository, QueueState, router};
use unite_matchsystem::registration::session::{SESSION_COOKIE, Session, set_session};
use unite_matchsystem::registration::{
    AddressSource, AddressWatch, AuthConfig, NewDeclaration, Rank, RegistrationRepository,
    WatchAlert, WatchedAccess,
};
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
        // **既定の受付枠（開催日の 20:45〜翌 2:00）を外す。**`POST /queue` は壁時計で
        // 受付時間を見るので、そのままだとテストの結果が実行時刻で変わる。
        // 受付枠そのものは `the_queue_only_accepts_entries_during_its_window` が
        // 「いまの時刻」からの相対で枠を置いて確かめる
        set_schedule(&pool, false, 0, 1, 0, "tests-ignore-the-clock").await;
        Self {
            queue: QueueRepository::new(pool.clone()),
            registration: RegistrationRepository::new(pool.clone()),
            formations: FormationRepository::new(pool.clone()),
            pool,
        }
    }

    /// 監視対象と通知先つきのルーター。
    fn router_watching(
        &self,
        watch: AddressWatch,
        alert: std::sync::Arc<dyn WatchAlert>,
    ) -> Router {
        router(
            QueueState::new(config(), self.registration.clone(), self.queue.clone())
                .with_address_watch(watch)
                .with_watch_alert(alert),
        )
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

/// スケジュールを 1 行追記する（運営パネルが書くのと同じ行）。曜日は月〜日すべて。
async fn set_schedule(
    pool: &PgPool,
    enabled: bool,
    start_minute: i32,
    end_minute: i32,
    queue_open_minute: i32,
    key: &str,
) {
    sqlx::query(
        "insert into matchmaker_schedule_changes \
         (enabled, days_mask, start_minute, end_minute, queue_open_minute, reason, \
          created_by, idempotency_key, created_at) \
         values ($1, 127, $2, $3, $4, 'test', 'test', $5, now())",
    )
    .bind(enabled)
    .bind(start_minute)
    .bind(end_minute)
    .bind(queue_open_minute)
    .bind(key)
    .execute(pool)
    .await
    .expect("append a schedule row");
}

async fn count_rows(db: &AppDb, table: &str) -> i64 {
    let sql = format!("select count(*) from {table}");
    sqlx::query_scalar(sqlx::AssertSqlSafe(sql))
        .fetch_one(&db.pool)
        .await
        .expect("count rows")
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

async fn get_queue_html(app: &Router, as_player: PlayerId, path: &str) -> (StatusCode, String) {
    let request = Request::get(path)
        .header(header::COOKIE, session_cookie(as_player))
        .header(header::ACCEPT, "text/html")
        .body(Body::empty())
        .unwrap();
    let response = send(app, request).await;
    let status = response.status();
    assert_eq!(
        response.headers().get(header::CONTENT_TYPE).unwrap(),
        "text/html; charset=utf-8"
    );
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    (status, String::from_utf8(bytes.to_vec()).unwrap())
}

async fn post_queue(app: &Router, as_player: PlayerId, body: Value) -> (StatusCode, Value) {
    post_queue_from(app, as_player, None, body).await
}

/// Cloudflare 経由を模して `CF-Connecting-IP` を付けて投入する。
async fn post_queue_from(
    app: &Router,
    as_player: PlayerId,
    ip: Option<&str>,
    body: Value,
) -> (StatusCode, Value) {
    let mut request = Request::post("/queue")
        .header(header::COOKIE, session_cookie(as_player))
        .header(header::CONTENT_TYPE, "application/json");
    if let Some(ip) = ip {
        request = request
            .header("cf-connecting-ip", ip)
            .header("x-forwarded-for", format!("{ip}, 172.64.0.1"))
            .header(header::USER_AGENT, "Mozilla/5.0 (test)");
    }
    json_body(send(app, request.body(Body::from(body.to_string())).unwrap()).await).await
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
    adjust_rating_with_key(db, player_id, delta_mu, "seed").await;
}

/// 同じプレイヤーに 2 回以上追記する用（冪等キーは追記ごとに変える）。
async fn adjust_rating_with_key(db: &AppDb, player_id: PlayerId, delta_mu: f64, key: &str) {
    sqlx::query(
        "insert into rating_adjustments \
         (player_id, delta_mu, delta_sigma, reason, created_by, created_at, idempotency_key) \
         values ($1, $2, 0, 'test seed', 'staff:test', now(), $3)",
    )
    .bind(player_id.0)
    .bind(delta_mu)
    .bind(format!("{key}-{}", player_id.0))
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
            streaming: false,
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
            streaming: false,
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

/// formation をどこまで進めるか。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Stage {
    /// 成立した直後。VC 割当待ち。
    Matched,
    /// VC 割当済み。ホストの承諾待ち。
    AwaitingHost,
    /// ホストが承諾。部屋コード待ち。
    AwaitingCode,
    /// コード開示済み。ここから報告できる。
    InProgress,
}

/// formation を作って `stage` まで進める。
async fn formation_at(db: &AppDb, n: u128, subject: PlayerId, stage: Stage) {
    db.formations
        .create_formation(&formation(n, subject))
        .await
        .unwrap();
    let host = player(2);
    let events = [
        (
            Stage::AwaitingHost,
            NewEvent::vc_assigned("1", 101, 102, at(0), format!("vc-{n}")),
        ),
        (
            Stage::AwaitingCode,
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
    for (needed, event) in events {
        if stage < needed {
            break;
        }
        db.formations.append_event(fid(n), &event).await.unwrap();
    }
}

impl PartialOrd for Stage {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.rank().cmp(&other.rank()))
    }
}

impl Stage {
    fn rank(self) -> u8 {
        match self {
            Self::Matched => 0,
            Self::AwaitingHost => 1,
            Self::AwaitingCode => 2,
            Self::InProgress => 3,
        }
    }
}

/// 報告義務のある状態（`in_progress`）まで進めて、報告ゲートを立てる。
async fn unreported_formation(db: &AppDb, n: u128, subject: PlayerId) {
    formation_at(db, n, subject, Stage::InProgress).await;
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

    sqlx::query("insert into queue_entries (id, player_id, enqueued_at, updated_at, down_range) values ($1, $2, now(), now(), 50)")
        .bind(Uuid::new_v4())
        .bind(me.0)
        .execute(&db.pool)
        .await
        .expect("the first entry is accepted");

    let error = sqlx::query("insert into queue_entries (id, player_id, enqueued_at, updated_at, down_range) values ($1, $2, now(), now(), 50)")
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

/// 下方向のマッチング範囲の CHECK（07 §3）。**運営が変えられる幅の内側だけを通す。**
///
/// 幅が 50 固定だった頃はこの CHECK が `= 50` だったが、運営が管理パネルから
/// 変えられるようになったので `10〜500` に広げてある。境界の外は依然として
/// DB が断る（狭すぎると 10 人が集まらず、広すぎると事実上の無制限になる）。
#[sqlx::test(
    fixtures("reference", "app_role"),
    fixtures("../ops/app_privileges.sql")
)]
async fn the_downward_range_is_bounded_by_a_check_constraint(owner: PgPool) {
    let db = AppDb::connect(&owner).await;

    for (index, down_range) in [10, 30, 50, 100, 200, 500].into_iter().enumerate() {
        sqlx::query("insert into queue_entries (id, player_id, enqueued_at, updated_at, down_range) values ($1, $2, now(), now(), $3)")
            .bind(Uuid::new_v4())
            .bind(player(index as u128 + 1).0)
            .bind(down_range)
            .execute(&db.pool)
            .await
            .unwrap_or_else(|error| panic!("{down_range} must be accepted: {error}"));
    }

    for down_range in [9, 501, 1000, 0, -1] {
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
    sqlx::query("insert into queue_entries (id, player_id, enqueued_at, updated_at, down_range) values ($1, $2, now(), now(), 50)")
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

/// ブラウザ向けの同じ `/queue` が、ログから導出した状態に応じて画面を切り替える。
#[sqlx::test(
    fixtures("reference", "app_role"),
    fixtures("../ops/app_privileges.sql")
)]
async fn browser_queue_page_covers_join_wait_and_voice_destination(owner: PgPool) {
    let db = AppDb::connect(&owner).await;
    let app = db.router();
    let me = player(11);
    register(&db, me, Rank::Legend).await;

    let (status, unqueued) = get_queue_html(&app, me, "/queue").await;
    assert_eq!(status, StatusCode::OK);
    assert!(unqueued.contains("data-view=\"unqueued\""), "{unqueued}");
    assert!(
        unqueued.contains("マッチング範囲は上 +50 / 下 -50 です"),
        "{unqueued}"
    );
    assert!(unqueued.contains("top_carry"), "{unqueued}");
    assert!(unqueued.contains("document.hidden"), "{unqueued}");

    let (status, joined) = post_queue(
        &app,
        me,
        json!({
            "roles": [wish("jungle", 1), wish("top_support", 2)]
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{joined}");
    let (status, queued) = get_queue_html(&app, me, "/queue").await;
    assert_eq!(status, StatusCode::OK);
    assert!(queued.contains("data-view=\"queued\""), "{queued}");
    assert!(queued.contains("第1希望: 中央"), "{queued}");
    assert!(queued.contains("第2希望: サポート"), "{queued}");
    assert!(queued.contains("上 +50 / 下 -50"), "{queued}");
    // 全体と範囲内の 2 つが並ぶ（自分ひとりなので全体 0 人）
    assert!(queued.contains("全体でキュー中"), "{queued}");
    assert!(queued.contains("同じ試合に入れる人"), "{queued}");

    let (status, _) = delete_queue(&app, me).await;
    assert_eq!(status, StatusCode::OK);
    formation_at(&db, 0xd1, me, Stage::AwaitingHost).await;
    let (status, matched) = get_queue_html(&app, me, "/queue").await;
    assert_eq!(status, StatusCode::OK);
    assert!(matched.contains("data-view=\"blocked\""), "{matched}");
    assert!(
        matched.contains("data-gate=\"active_formation\""),
        "{matched}"
    );
    assert!(matched.contains("vc1-red"), "{matched}");

    db.done().await;
}

/// **2つのゲートを畳まない。**それぞれ単独でも、両方同時でも、理由が別々に返る。
#[sqlx::test(
    fixtures("reference", "app_role"),
    fixtures("../ops/app_privileges.sql")
)]
async fn the_registration_and_report_gates_are_reported_separately(owner: PgPool) {
    let db = AppDb::connect(&owner).await;
    let app = db.router();
    let body = json!({ "roles": [wish("jungle", 1)] });

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

/// **マッチが成立していて、まだ始まっていない人はキューに入れない。**
///
/// マッチメイカは成立時に 10 人をキューから外すが、`awaiting_host`（候補 1 人あたり 90 秒 ×
/// 最大 3 人）と `awaiting_code`（180 秒）の間は本人が再投入できてしまう。マッチメイカは
/// 3 分ごとに走るので、この窓は複数回のティックをまたぐ。塞がないと同じ人が 2 つの formation に
/// 割り当てられ、VC の組も 2 つ占有する。
///
/// 報告ゲートではこれを塞げない。始まる前は報告する手段が無いためである。
#[sqlx::test(
    fixtures("reference", "app_role"),
    fixtures("../ops/app_privileges.sql")
)]
async fn a_formation_that_has_not_started_blocks_the_queue(owner: PgPool) {
    let db = AppDb::connect(&owner).await;
    let app = db.router();
    let body = json!({ "roles": [wish("jungle", 1)] });

    // fixture の players は 1〜12。formation の頭数（2〜10）と重ならない 3 人を用意する
    sqlx::query("insert into players (id, discord_user_id, created_at) values ($1, 1013, now())")
        .bind(player(13).0)
        .execute(&db.pool)
        .await
        .expect("seed a third bystander");

    // 3 状態それぞれで塞がること。プレイヤーごとに formation を 1 つ持たせる
    for (index, stage) in [Stage::Matched, Stage::AwaitingHost, Stage::AwaitingCode]
        .into_iter()
        .enumerate()
    {
        let me = player(index as u128 + 11);
        register(&db, me, Rank::Legend).await;
        formation_at(&db, 0xa0 + index as u128, me, stage).await;

        let (status, denied) = post_queue(&app, me, body.clone()).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{stage:?}: {denied}");
        assert_eq!(denied["error"], "queue_blocked");
        assert!(
            denied["active_formation"].is_string(),
            "{stage:?}: the active-formation gate must name the formation: {denied}"
        );
        // **報告ゲートとは別のフィールド。**畳まれていないこと
        assert_eq!(
            denied["pending_reports"],
            json!([]),
            "{stage:?}: nothing to report yet: {denied}"
        );
        assert_eq!(denied["registration"], Value::Null, "{stage:?}");
        let messages = denied["messages"].as_array().unwrap();
        assert_eq!(messages.len(), 1, "{stage:?}: {denied}");
        assert!(
            messages[0].as_str().unwrap().contains("マッチが成立"),
            "{stage:?}: {denied}"
        );
        assert!(
            db.queue.find(me).await.unwrap().is_none(),
            "{stage:?}: nothing was written"
        );
    }

    db.done().await;
}

/// 試合が始まったら、担当は進行中ゲートから報告ゲートへ移る。**両方が同時に塞がない。**
///
/// ここを重ねると、報告を済ませた人が formation の確定を待たされる（00 §5 の
/// 「報告ゲートの解除条件を『確定』にしない —— 他 9 人に人質を取られる」が壊れる）。
#[sqlx::test(
    fixtures("reference", "app_role"),
    fixtures("../ops/app_privileges.sql")
)]
async fn the_report_gate_takes_over_once_the_match_starts(owner: PgPool) {
    let db = AppDb::connect(&owner).await;
    let app = db.router();
    let body = json!({ "roles": [wish("jungle", 1)] });
    let me = player(11);
    register(&db, me, Rank::Legend).await;

    // 始まる前は進行中ゲート
    formation_at(&db, 0xb1, me, Stage::AwaitingCode).await;
    let (status, denied) = post_queue(&app, me, body.clone()).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert!(denied["active_formation"].is_string(), "{denied}");
    assert_eq!(denied["pending_reports"], json!([]));

    // コードが出て試合が始まると、進行中ゲートは降りて報告ゲートが立つ
    db.formations
        .append_event(
            fid(0xb1),
            &NewEvent::code_posted(player(2), "1234", at(3), "code-b1"),
        )
        .await
        .unwrap();
    let (status, denied) = post_queue(&app, me, body.clone()).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(
        denied["active_formation"],
        Value::Null,
        "the active gate must let go once reporting is possible: {denied}"
    );
    assert_eq!(denied["pending_reports"].as_array().unwrap().len(), 1);
    assert_eq!(denied["messages"].as_array().unwrap().len(), 1, "{denied}");

    // 本人が報告すれば、確定を待たずにキューへ戻れる
    db.formations
        .append_report(&NewReport {
            formation_id: fid(0xb1),
            player_id: me,
            reported_at: at(10),
            outcome: Outcome::Win(Team::Red),
            raw_result: json!({ "outcome": "red" }),
            idempotency_key: "report-b1".into(),
            supersedes_id: None,
        })
        .await
        .unwrap();
    let (status, allowed) = post_queue(&app, me, body).await;
    assert_eq!(status, StatusCode::OK, "{allowed}");
    assert_eq!(allowed["queued"], true);

    db.done().await;
}

/// **報告ゲートを切っても、試合中の人はキューに入れない。**
///
/// `REPORT_GATE_DISABLED` は報告フローが壊れたときの縮退用だが、切ると `in_progress` を
/// 塞ぐものが無くなる。そのまま通すと試合中の人が二重割当され、**一番混乱している最中に
/// 一番分かりにくいバグが出る**。フラグ有効時は進行中ゲートが `in_progress` まで広がる。
#[sqlx::test(
    fixtures("reference", "app_role"),
    fixtures("../ops/app_privileges.sql")
)]
async fn a_disabled_report_gate_hands_in_progress_to_the_active_gate(owner: PgPool) {
    let db = AppDb::connect(&owner).await;
    let me = player(11);
    register(&db, me, Rank::Legend).await;
    formation_at(&db, 0xc1, me, Stage::InProgress).await;

    let body = json!({ "roles": [wish("jungle", 1)] });

    // 通常時: 報告ゲートが塞ぐ
    let (status, denied) = post_queue(&db.router(), me, body.clone()).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(denied["active_formation"], Value::Null, "{denied}");
    assert_eq!(denied["pending_reports"].as_array().unwrap().len(), 1);

    // 縮退時: 報告ゲートは空になるが、進行中ゲートが受け取る
    let degraded = QueueState::new(
        config(),
        db.registration.clone().with_report_gate_disabled(true),
        db.queue.clone(),
    );
    let degraded = router(degraded);
    let (status, denied) = post_queue(&degraded, me, body.clone()).await;
    assert_eq!(
        status,
        StatusCode::FORBIDDEN,
        "a live match must still block: {denied}"
    );
    assert_eq!(
        denied["active_formation"],
        Value::String(fid(0xc1).0.to_string()),
        "{denied}"
    );
    assert_eq!(
        denied["pending_reports"],
        json!([]),
        "the report gate is off, as asked: {denied}"
    );
    assert!(db.queue.find(me).await.unwrap().is_none());

    // 試合が終われば、縮退時は報告を待たずに入れる（フラグの目的そのもの）
    db.formations
        .append_event(
            fid(0xc1),
            &NewEvent::new(K::Voided, None, at(60), "voided-c1"),
        )
        .await
        .unwrap();
    let (status, allowed) = post_queue(&degraded, me, body).await;
    assert_eq!(status, StatusCode::OK, "{allowed}");
    assert_eq!(allowed["queued"], true);

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
        before["in_range"], 0,
        "範囲は固定なので、入る前でも人数が決まる"
    );

    // 投入
    let (status, joined) = post_queue(
        &app,
        me,
        json!({
            "roles": [wish("jungle", 1), wish("bot_tank", 2)],
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{joined}");
    assert_eq!(joined["queued"], true);
    assert_eq!(joined["entry"]["down_range"], 50);
    assert_eq!(joined["entry"]["up_range"], 50, "upward range is fixed");
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

    let (_, first) = post_queue(&app, me, json!({ "roles": [wish("jungle", 1)] })).await;
    let (_, second) = post_queue(
        &app,
        me,
        json!({
            "roles": [wish("bot_tank", 1), wish("top_support", 2)],
        }),
    )
    .await;

    assert_eq!(second["entry"]["id"], first["entry"]["id"], "same entry");
    assert_eq!(
        second["entry"]["enqueued_at"], first["entry"]["enqueued_at"],
        "widening the range must not push the player to the back of the queue"
    );
    assert_eq!(second["entry"]["down_range"], 50, "範囲は固定");
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

/// **設定の更新が途中で落ちても、エントリは元のまま残る。**
///
/// `queue_entries` の upsert と `queue_entry_roles` の入れ替え（DELETE + INSERT）が同じ
/// トランザクションに入っていないと、失敗した瞬間に「エントリはあるが希望ロールが 0 件」の
/// 行が残る。マッチメイカから見ると辺が張れないので黙って除外され、本人はキューにいるつもりで
/// 永久に待つことになる。当日いちばん原因が分かりにくい形なので、ここで固定しておく。
#[sqlx::test(
    fixtures("reference", "app_role"),
    fixtures("../ops/app_privileges.sql")
)]
async fn a_failed_update_leaves_the_entry_and_its_roles_untouched(owner: PgPool) {
    let db = AppDb::connect(&owner).await;
    let me = player(1);
    let request = |roles: Vec<RolePreference>| QueueRequest {
        roles,
        streaming: false,
    };

    db.queue
        .enqueue(
            me,
            &request(vec![
                RolePreference {
                    role: "jungle".into(),
                    priority: 1,
                },
                RolePreference {
                    role: "bot_tank".into(),
                    priority: 2,
                },
            ]),
            at(0),
        )
        .await
        .expect("the first enqueue succeeds");

    // 入れ替えの INSERT が FK で落ちる（roles に無いロール）
    let failed = db
        .queue
        .enqueue(
            me,
            &request(vec![RolePreference {
                role: "midlane".into(),
                priority: 1,
            }]),
            at(60),
        )
        .await;
    assert!(
        matches!(failed, Err(QueueError::UnknownRole)),
        "{:?}",
        failed.err()
    );

    let entry = db
        .queue
        .find(me)
        .await
        .unwrap()
        .expect("the entry survives");
    assert_eq!(entry.down_range, 50, "the upsert rolled back too");
    let roles: Vec<&str> = entry.roles.iter().map(|role| role.role.as_str()).collect();
    assert_eq!(roles, ["jungle", "bot_tank"], "the old wishes are intact");

    // 希望ロールが 0 件のエントリは存在しない
    let orphans: i64 = sqlx::query_scalar(
        "select count(*) from queue_entries q \
         where not exists (select 1 from queue_entry_roles r where r.entry_id = q.id)",
    )
    .fetch_one(&db.pool)
    .await
    .unwrap();
    assert_eq!(orphans, 0, "no entry may exist without a role preference");

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
        (json!({ "roles": [] }), "no_roles"),
        (
            json!({
                "roles": [wish("jungle", 1), wish("bot_tank", 2), wish("top_carry", 3), wish("bot_carry", 4)],
            }),
            "too_many_roles",
        ),
        (
            json!({ "roles": [wish("jungle", 1), wish("jungle", 2)] }),
            "duplicate_role",
        ),
        (
            json!({ "roles": [wish("jungle", 2)] }),
            "malformed_priorities",
        ),
        // 存在しないロールは DB の FK が拒否し、それを 400 に写す（一覧を Rust 側に複製しない）
        (json!({ "roles": [wish("midlane", 1)] }), "unknown_role"),
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

    post_queue(&app, me, json!({ "roles": [wish("jungle", 1)] })).await;

    // ボディに他人の player_id を書いても無視される（そもそも読まない）
    let (status, theirs) = post_queue(
        &app,
        someone_else,
        json!({
            "player_id": me.0.to_string(),
            "roles": [wish("bot_tank", 1)],
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(theirs["entry"]["player_id"], someone_else.0.to_string());

    // 私のエントリは無傷
    let mine = db.queue.find(me).await.unwrap().expect("still queued");
    assert_eq!(mine.down_range, 50);
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

/// **境界の実測を DB 経由で。**上下 50 固定・seed の差 100 の噛み合わせ。
///
/// 1500 / 1550 / 1600 を並べ、画面に出る「範囲内でキュー中: N人」を三者から数える。
/// 差ちょうど 50 が組める側であること（比較が `<=`）を HTTP の応答で確かめる。
#[sqlx::test(
    fixtures("reference", "app_role"),
    fixtures("../ops/app_privileges.sql")
)]
async fn exactly_fifty_apart_counts_but_one_hundred_does_not(owner: PgPool) {
    let db = AppDb::connect(&owner).await;
    let app = db.router();

    // レートは導出値。rating_adjustments への追記で作る（表示スケール、既定は 1500）
    let high = player(1); // 1600
    let middle = player(2); // 1550
    let low = player(3); // 1500
    adjust_rating(&db, high, 100.0).await;
    adjust_rating(&db, middle, 50.0).await;
    adjust_rating(&db, low, 0.0).await;
    for who in [high, middle, low] {
        register(&db, who, Rank::Legend).await;
        let (status, _) = post_queue(&app, who, json!({ "roles": [wish("jungle", 1)] })).await;
        assert_eq!(status, StatusCode::OK);
    }

    // 1600 から見えるのは 1550 だけ（1500 とは差 100）
    let (status, from_high) = get_queue(&app, Some(high)).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(from_high["in_range"], 1, "{from_high}");
    // 1500 から見えるのも 1550 だけ
    let (_, from_low) = get_queue(&app, Some(low)).await;
    assert_eq!(from_low["in_range"], 1, "{from_low}");
    // 1550 は両方と組めるが、1500 と 1600 は同じ試合に入れない（差 100）。
    // 数えるのは「自分を含む幅 50 の組み合わせ」なので、どちらか片方の 1 人
    let (_, from_middle) = get_queue(&app, Some(middle)).await;
    assert_eq!(from_middle["in_range"], 1, "{from_middle}");

    // **全体の人数はレート帯を問わない。**3 人キューにいて、自分を除く 2 人が
    // どこから見ても全体の数になる（範囲内の数と違って視点で変わらない）
    for (who, label) in [(high, "1600"), (middle, "1550"), (low, "1500")] {
        let (_, view) = get_queue(&app, Some(who)).await;
        assert_eq!(view["queue_total"], 2, "{label} から見た全体: {view}");
    }
    println!(
        "全体 2 / 範囲内: 1600→{} 1550→{} 1500→{}",
        from_high["in_range"], from_middle["in_range"], from_low["in_range"]
    );

    // 自分自身は数えない
    assert_eq!(
        db.queue
            .in_range_summary(high)
            .await
            .unwrap()
            .map(|s| s.total),
        Some(1)
    );

    // 1 だけ動かして 1551 にすると、1500 からは外れる（差 51）
    adjust_rating_with_key(&db, middle, 1.0, "nudge").await;
    let (_, nudged) = get_queue(&app, Some(low)).await;
    assert_eq!(nudged["in_range"], 0, "差 51 は範囲外: {nudged}");
    let (_, still_high) = get_queue(&app, Some(high)).await;
    assert_eq!(
        still_high["in_range"], 1,
        "1600 から 1551 は差 49: {still_high}"
    );

    db.done().await;
}

/// **「範囲内 N 人」はマッチメイカと同じ判定用レートで数える。**
///
/// 1486 の人が 2 回取り残されていれば、マッチメイカは 1514 として見る。画面の人数も
/// 同じ値で数えないと「範囲内 0 人」のまま突然組まれる。
#[sqlx::test(
    fixtures("reference", "app_role"),
    fixtures("../ops/app_privileges.sql")
)]
async fn the_in_range_count_uses_the_lifted_rating(owner: PgPool) {
    let db = AppDb::connect(&owner).await;
    let app = db.router();
    let low = player(1); // 1486
    let high = player(2); // 1550
    adjust_rating(&db, low, -14.0).await;
    adjust_rating(&db, high, 50.0).await;
    for who in [low, high] {
        register(&db, who, Rank::Legend).await;
        let (status, _) = post_queue(&app, who, json!({ "roles": [wish("jungle", 1)] })).await;
        assert_eq!(status, StatusCode::OK);
    }
    // 差 64: どちらからも見えない
    let (_, from_high) = get_queue(&app, Some(high)).await;
    assert_eq!(from_high["in_range"], 0, "{from_high}");

    // マッチメイカに 2 回取り残された（1486 → 判定用 1514、差 36）
    sqlx::query("update queue_entries set unmatched_scans = 2 where player_id = $1")
        .bind(low.0)
        .execute(&db.pool)
        .await
        .unwrap();
    let (_, from_high) = get_queue(&app, Some(high)).await;
    assert_eq!(from_high["in_range"], 1, "{from_high}");
    let (_, from_low) = get_queue(&app, Some(low)).await;
    assert_eq!(from_low["in_range"], 1, "{from_low}");

    // 逆側: 1486 は戻し（0 回）、いちばん上の 1550 が 2 回取り残された → 1522（差 36）
    sqlx::query("update queue_entries set unmatched_scans = 0 where player_id = $1")
        .bind(low.0)
        .execute(&db.pool)
        .await
        .unwrap();
    let (_, from_low) = get_queue(&app, Some(low)).await;
    assert_eq!(from_low["in_range"], 0, "{from_low}");
    sqlx::query("update queue_entries set unmatched_scans = 2 where player_id = $1")
        .bind(high.0)
        .execute(&db.pool)
        .await
        .unwrap();
    let (_, from_low) = get_queue(&app, Some(low)).await;
    assert_eq!(from_low["in_range"], 1, "{from_low}");
    let (_, from_high) = get_queue(&app, Some(high)).await;
    assert_eq!(from_high["in_range"], 1, "{from_high}");
    println!("1486 + 2 回 → 1514 / いちばん上 1550 − 2 回 → 1522 として範囲内に数える");
    db.done().await;
}

/// キュー画面のヘッダーも共通のもの。管理者リストの有無だけで管理の導線が変わる。
///
/// `player(11)` は `reference` フィクスチャの discord_user_id を持つ。判定に使うのは
/// その ID だけで、キューの状態やゲートの結果は一切見ない。
#[sqlx::test(
    fixtures("reference", "app_role"),
    fixtures("../ops/app_privileges.sql")
)]
async fn the_queue_page_carries_the_shared_header(owner: PgPool) {
    let db = AppDb::connect(&owner).await;
    let me = player(11);
    register(&db, me, Rank::Legend).await;
    let discord_user_id =
        sqlx::query_scalar!("select discord_user_id from players where id = $1", me.0)
            .fetch_one(&db.pool)
            .await
            .unwrap()
            .to_string();

    // 管理者リスト未設定: 戦績とログアウトだけ
    let (status, page) = get_queue_html(&db.router(), me, "/queue").await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        page.contains("<a class=\"brand\" href=\"/queue\">Assay</a>"),
        "{page}"
    );
    assert!(page.contains("<a href=\"/status\">戦績</a>"), "{page}");
    assert!(page.contains("action=\"/auth/logout\""), "{page}");
    assert!(!page.contains("admin"), "{page}");

    // 別人だけが管理者: 出さない
    let others = router(
        QueueState::new(config(), db.registration.clone(), db.queue.clone())
            .with_admins(parse_admin_ids(Some("900000000000000001")).unwrap()),
    );
    let (_, page) = get_queue_html(&others, me, "/queue").await;
    assert!(!page.contains("admin"), "{page}");

    let mine = router(
        QueueState::new(config(), db.registration.clone(), db.queue.clone())
            .with_admins(parse_admin_ids(Some(&discord_user_id)).unwrap()),
    );
    let (_, as_admin) = get_queue_html(&mine, me, "/queue").await;
    assert!(as_admin.contains("href=\"/admin\""), "{as_admin}");
    let header_of = |body: &str| {
        let start = body.find("<header").expect("a header");
        let end = body.find("</header>").expect("a header end") + "</header>".len();
        body[start..end].to_owned()
    };
    assert_ne!(
        header_of(&as_admin),
        header_of(&page),
        "管理者と一般参加者でヘッダーが違う"
    );

    // 未ログインの 401 画面には導線を出さない
    let response = send(
        &db.router(),
        Request::get("/queue")
            .header(header::ACCEPT, "text/html")
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    let page = String::from_utf8(bytes.to_vec()).unwrap();
    assert!(!page.contains("href=\"/status\""), "{page}");
    assert!(!page.contains("/auth/logout"), "{page}");
    db.done().await;
}

/// 旧範囲（95〜200）で入った行は**一括 UPDATE せず、読み出し時に丸める**。
///
/// 新しい CHECK は `not valid` で足してあるので、既存の違反行はそのまま残る。
/// 残った値をそのままマッチングに使うと、本人が設定できない広さで組まれてしまうため、
/// 読み出し側（`queue::model::clamp_down_range`）が 50 に丸める。
#[sqlx::test(
    fixtures("reference", "app_role"),
    fixtures("../ops/app_privileges.sql")
)]
async fn a_legacy_entry_outside_the_new_range_is_clamped_on_read(owner: PgPool) {
    let db = AppDb::connect(&owner).await;
    let me = player(1);
    register(&db, me, Rank::Legend).await;

    // 旧範囲で入った行を再現する（CHECK を外して入れ、not valid で戻す）
    sqlx::raw_sql("alter table queue_entries drop constraint queue_entries_down_range_check")
        .execute(&owner)
        .await
        .unwrap();
    sqlx::query(
        "insert into queue_entries (id, player_id, enqueued_at, updated_at, down_range) \
         values ($1, $2, now(), now(), 200)",
    )
    .bind(Uuid::new_v4())
    .bind(me.0)
    .execute(&owner)
    .await
    .unwrap();
    sqlx::query("insert into queue_entry_roles (entry_id, role, priority) select id, 'jungle', 1 from queue_entries where player_id = $1")
        .bind(me.0)
        .execute(&owner)
        .await
        .unwrap();
    sqlx::raw_sql(
        "alter table queue_entries add constraint queue_entries_down_range_check \
         check (down_range between 30 and 100) not valid",
    )
    .execute(&owner)
    .await
    .expect("既存の違反行があっても not valid なら足せる");

    // 保存値はそのまま。既存行を書き換えない
    let stored: i32 =
        sqlx::query_scalar("select down_range from queue_entries where player_id = $1")
            .bind(me.0)
            .fetch_one(&owner)
            .await
            .unwrap();
    assert_eq!(stored, 200, "既存行は一括 UPDATE しない");

    // 読み出しはどの経路でも丸まる
    let entry = db.queue.find(me).await.unwrap().expect("queued");
    assert_eq!(entry.down_range, DEFAULT_DOWN_RANGE, "自分のエントリ");
    let queue = db.queue.queued_players().await.unwrap();
    assert_eq!(queue.len(), 1);
    assert_eq!(queue[0].down_range, DEFAULT_DOWN_RANGE, "範囲判定の入力");

    // 画面（JSON）にも丸めた値が出る
    let app = db.router();
    let (status, body) = get_queue(&app, Some(me)).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["entry"]["down_range"], 50, "{body}");
    assert_eq!(body["entry"]["up_range"], 50, "上方向も 50 固定: {body}");

    // 本人が入れ直せば、新しい範囲の値で上書きされる（違反行は残り続けない）
    let (status, joined) = post_queue(&app, me, json!({ "roles": [wish("jungle", 1)] })).await;
    assert_eq!(status, StatusCode::OK, "{joined}");
    let stored: i32 =
        sqlx::query_scalar("select down_range from queue_entries where player_id = $1")
            .bind(me.0)
            .fetch_one(&owner)
            .await
            .unwrap();
    assert_eq!(stored, 50);

    db.done().await;
}

/// 上方向が +95 固定である以上、-96〜-100 は候補を増やさない（承知のうえの暫定状態）。
///
/// 「-100 にしても人数が増えない」をバグとして扱わないための、意図の記録。
#[sqlx::test(
    fixtures("reference", "app_role"),
    fixtures("../ops/app_privileges.sql")
)]
async fn widening_past_the_upward_cap_does_not_add_candidates(owner: PgPool) {
    let db = AppDb::connect(&owner).await;
    let app = db.router();

    let me = player(1); // 1600
    let far = player(2); // 1502（me から見て 98 下）
    adjust_rating(&db, me, 100.0).await;
    adjust_rating(&db, far, 2.0).await;
    for who in [me, far] {
        register(&db, who, Rank::Legend).await;
    }
    let (status, _) = post_queue(&app, far, json!({ "roles": [wish("jungle", 1)] })).await;
    assert_eq!(status, StatusCode::OK);

    // 下方向を 95 → 100 に広げても、far は上方向 +95 の外（1502 + 95 = 1597 < 1600）
    for down_range in [95, 100] {
        let (status, body) = post_queue(
            &app,
            me,
            json!({ "down_range": down_range, "roles": [wish("jungle", 1)] }),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            body["in_range"], 0,
            "down_range {down_range} でも増えない: {body}"
        );
    }

    db.done().await;
}

// ----------------------------------------------------------------------
// 報告完了 → キュー画面 → 再投入（実測）
// ----------------------------------------------------------------------

/// **9人が報告して未確定のままでも、9人ともキュー画面へ戻って再投入できる。**
///
/// 報告ゲートはプレイヤー単位なので、確定を待たせない。10人目が報告するまで
/// マッチ画面に留めると、最大で報告受付時間ぶん拘束することになる。
#[sqlx::test(
    fixtures("reference", "app_role"),
    fixtures("../ops/app_privileges.sql")
)]
async fn nine_reporters_all_get_back_to_the_queue_before_confirmation(owner: PgPool) {
    let db = AppDb::connect(&owner).await;
    let app = db.router();
    let body = json!({ "roles": [wish("jungle", 1)] });

    // formation の参加者は player(1)..player(10)。全員を登録しておく
    for n in 1..=10u128 {
        register(&db, player(n), Rank::Legend).await;
    }
    formation_at(&db, 0xc1, player(1), Stage::InProgress).await;

    // 報告前は 10 人ともキューに入れない
    for n in 1..=10u128 {
        let (status, denied) = post_queue(&app, player(n), body.clone()).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "player {n}: {denied}");
    }

    // 9 人が報告する。10 人目は報告しないので**確定しない**
    for n in 1..=9u128 {
        db.formations
            .append_latest_report(NewReport {
                formation_id: fid(0xc1),
                player_id: player(n),
                reported_at: at(10),
                outcome: Outcome::Win(Team::Red),
                raw_result: json!({ "outcome": "red" }),
                idempotency_key: format!("nine-{n}"),
                supersedes_id: None,
            })
            .await
            .unwrap();
    }
    let state = db
        .formations
        .load_events(fid(0xc1))
        .await
        .unwrap()
        .snapshot()
        .state;
    assert!(
        matches!(
            state,
            unite_matchsystem::lifecycle::FormationState::InProgress { .. }
        ),
        "未確定であること: {state:?}"
    );

    // 9 人ともキュー画面へ戻り、そのまま再投入できる
    for n in 1..=9u128 {
        let (status, page) = get_queue_html(
            &app,
            player(n),
            &format!("/queue?reported={}", fid(0xc1).0.simple()),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "player {n}");
        assert!(
            page.contains("報告を受け付けました"),
            "player {n} に導線が出ていない"
        );
        assert!(
            page.contains(&format!("/match/{}", fid(0xc1).0)),
            "player {n} からマッチ画面へ戻れない"
        );
        let (status, allowed) = post_queue(&app, player(n), body.clone()).await;
        assert_eq!(status, StatusCode::OK, "player {n}: {allowed}");
        assert_eq!(allowed["queued"], true, "player {n}");
    }

    // 10 人目だけは報告していないので、まだ塞がれたまま
    let (status, denied) = post_queue(&app, player(10), body).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{denied}");
    assert_eq!(denied["pending_reports"].as_array().unwrap().len(), 1);

    println!("9人が報告 → 未確定のまま9人ともキューへ再投入できた（10人目だけ未報告で停止）");
    db.done().await;
}

// ----------------------------------------------------------------------
// ロール希望の保持（追記専用・最新行から導出）
// ----------------------------------------------------------------------

/// **離脱しても、次にキュー画面を開くと前回の希望で埋まっている。**
///
/// 保持先は `players` ではなく追記専用の `queue_role_preferences`。現在値の列は
/// 持たず、最新行から導出する（登録フォームのプリフィルと同じ方針）。
#[sqlx::test(
    fixtures("reference", "app_role"),
    fixtures("../ops/app_privileges.sql")
)]
async fn the_role_choices_survive_leaving_the_queue(owner: PgPool) {
    let db = AppDb::connect(&owner).await;
    let app = db.router();
    let me = player(11);
    register(&db, me, Rank::Legend).await;

    // 何も選んでいないうちは埋まらない
    let (_, page) = get_queue_html(&app, me, "/queue").await;
    assert!(!page.contains(" selected>"), "{page}");

    // 第1〜第3希望を選んでキューへ
    let body = json!({
        "roles": [wish("jungle", 1), wish("bot_tank", 2), wish("top_carry", 3)]
    });
    let (status, joined) = post_queue(&app, me, body).await;
    assert_eq!(status, StatusCode::OK, "{joined}");

    // 離脱する（queue_entries とその子行は消える）
    let request = Request::delete("/queue")
        .header(header::COOKIE, session_cookie(me))
        .body(Body::empty())
        .unwrap();
    assert_eq!(send(&app, request).await.status(), StatusCode::OK);
    let remaining: i64 = sqlx::query_scalar("select count(*) from queue_entries")
        .fetch_one(&db.pool)
        .await
        .unwrap();
    assert_eq!(remaining, 0, "エントリは消えている");

    // それでも次に開いたときは前回の希望で埋まっている
    let (_, page) = get_queue_html(&app, me, "/queue").await;
    assert!(
        page.contains("<option value=\"jungle\" selected>"),
        "第1希望が埋まっていない: {page}"
    );
    assert!(
        page.contains("<option value=\"bot_tank\" selected>"),
        "第2希望が埋まっていない: {page}"
    );
    assert!(
        page.contains("<option value=\"top_carry\" selected>"),
        "第3希望が埋まっていない: {page}"
    );
    println!("離脱後に開き直したキュー画面で、第1〜第3希望が復元された");

    // 選び直すと最新行が正になる。**古い行は残る**（追記専用）
    let (status, _) = post_queue(&app, me, json!({ "roles": [wish("bot_carry", 1)] })).await;
    assert_eq!(status, StatusCode::OK);
    let rows: i64 = sqlx::query_scalar("select count(*) from queue_role_preferences")
        .fetch_one(&db.pool)
        .await
        .unwrap();
    assert_eq!(rows, 2, "追記されている（上書きではない）");

    let request = Request::delete("/queue")
        .header(header::COOKIE, session_cookie(me))
        .body(Body::empty())
        .unwrap();
    send(&app, request).await;
    let (_, page) = get_queue_html(&app, me, "/queue").await;
    assert!(
        page.contains("<option value=\"bot_carry\" selected>"),
        "最新行が使われていない: {page}"
    );
    assert!(
        !page.contains("<option value=\"jungle\" selected>"),
        "古い行が使われている: {page}"
    );
    println!("選び直すと最新行が正になり、古い行は残ったまま（行数 {rows}）");

    db.done().await;
}

/// **アプリ用ロールからは書き換えも削除もできない**（追記専用）。
#[sqlx::test(
    fixtures("reference", "app_role"),
    fixtures("../ops/app_privileges.sql")
)]
async fn the_role_preference_log_is_append_only(owner: PgPool) {
    let db = AppDb::connect(&owner).await;
    let app = db.router();
    let me = player(11);
    register(&db, me, Rank::Legend).await;
    let (status, _) = post_queue(&app, me, json!({ "roles": [wish("jungle", 1)] })).await;
    assert_eq!(status, StatusCode::OK);

    for sql in [
        "update queue_role_preferences set roles = '[]'::jsonb",
        "delete from queue_role_preferences",
    ] {
        let error = sqlx::query(sql).execute(&db.pool).await.unwrap_err();
        assert_eq!(
            error.as_database_error().and_then(|e| e.code()).as_deref(),
            Some("42501"),
            "{sql}"
        );
    }
    db.done().await;
}

// ----------------------------------------------------------------------
// 「報告の反映が遅い」の切り分け（実測）
// ----------------------------------------------------------------------

/// **報告ゲートは本人の報告と同じ HTTP リクエストで解除される。**確定を待たない。
///
/// 「報告してもすぐ次のキューに入れない」という報告の切り分けに使う。ここで測るのは
/// サーバ側の解除までの時間で、画面に見えるまでの遅れ（キュー画面の10秒ポーリング）は
/// 含まない。両者を混ぜると、直す場所を取り違える。
#[sqlx::test(
    fixtures("reference", "app_role"),
    fixtures("../ops/app_privileges.sql")
)]
async fn the_report_gate_opens_within_the_same_request(owner: PgPool) {
    let db = AppDb::connect(&owner).await;
    let app = db.router();
    let body = json!({ "roles": [wish("jungle", 1)] });
    let me = player(11);
    register(&db, me, Rank::Legend).await;
    formation_at(&db, 0xf1, me, Stage::InProgress).await;

    let (status, denied) = post_queue(&app, me, body.clone()).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(denied["pending_reports"].as_array().unwrap().len(), 1);

    let reported_at = std::time::Instant::now();
    db.formations
        .append_latest_report(NewReport {
            formation_id: fid(0xf1),
            player_id: me,
            reported_at: at(10),
            outcome: Outcome::Win(Team::Red),
            raw_result: json!({ "outcome": "red" }),
            idempotency_key: "latency-report".into(),
            supersedes_id: None,
        })
        .await
        .unwrap();
    let append_elapsed = reported_at.elapsed();

    let gate_checked = std::time::Instant::now();
    let (status, allowed) = post_queue(&app, me, body).await;
    let gate_elapsed = gate_checked.elapsed();
    assert_eq!(status, StatusCode::OK, "{allowed}");
    assert_eq!(allowed["queued"], true);

    let state = db
        .formations
        .load_events(fid(0xf1))
        .await
        .unwrap()
        .snapshot()
        .state;
    assert!(
        matches!(
            state,
            unite_matchsystem::lifecycle::FormationState::InProgress { .. }
        ),
        "確定を待たずに解除されること: {state:?}"
    );

    println!("報告の追記: {append_elapsed:?}");
    println!("報告 → キュー再投入が通るまで: {gate_elapsed:?}");
    println!("この時点の formation の状態: {state:?}（未確定）");
    db.done().await;
}

// ----------------------------------------------------------------------
// 受付時間
// ----------------------------------------------------------------------

/// **キューは受付時間内にしか入れない。**受付は稼働開始より前に開き、
/// 稼働の終了と同時に閉じる。
///
/// `POST /queue` は壁時計で判定するので、枠は「いまの JST 時刻」からの相対で置く
/// （曜日は全部）。1 時間の余裕を取っているので、テストが分をまたいでも結果は変わらない。
#[sqlx::test(
    fixtures("reference", "app_role"),
    fixtures("../ops/app_privileges.sql")
)]
async fn the_queue_only_accepts_entries_during_its_window(owner: PgPool) {
    let db = AppDb::connect(&owner).await;
    let app = db.router();
    let me = player(1);
    register(&db, me, Rank::Legend).await;

    let jst_now = Utc::now().with_timezone(&chrono::FixedOffset::east_opt(9 * 3600).unwrap());
    let minute_now = i32::try_from(jst_now.hour() * 60 + jst_now.minute()).unwrap();
    let minute_at = |offset: i32| (minute_now + offset).rem_euclid(24 * 60);

    // 受付は 1 時間後、稼働は 2 時間後から 3 時間後まで → いまは受付時間外
    set_schedule(
        &db.pool,
        true,
        minute_at(120),
        minute_at(180),
        minute_at(60),
        "closed",
    )
    .await;
    let (status, closed) = get_queue(&app, Some(me)).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(closed["queue_open"], false, "{closed}");
    assert_eq!(closed["matchmaker_open"], false, "{closed}");
    // 受付開始と稼働開始は別々のフィールド（1 つの枠にまとめない）
    assert_eq!(
        closed["queue_schedule"]["days"], "月火水木金土日",
        "{closed}"
    );
    let queue_open = closed["queue_schedule"]["queue_open"].as_str().unwrap();
    let start = closed["queue_schedule"]["matchmaker_start"]
        .as_str()
        .unwrap();
    assert_ne!(queue_open, start, "{closed}");
    let (status, refused) = post_queue(&app, me, json!({ "roles": [wish("jungle", 1)] })).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{refused}");
    assert_eq!(refused["error"], "queue_closed");
    let message = refused["message"].as_str().unwrap();
    assert!(
        message.contains(&format!("キュー受付は 月火水木金土日 {queue_open} から"))
            && message.contains(&format!("マッチメイキングは {start} から")),
        "受付開始と稼働開始を分けて書く: {refused}"
    );
    assert!(!message.contains(&format!("{queue_open}〜")), "{refused}");
    let (_, page) = get_queue_html(&app, me, "/queue").await;
    assert!(page.contains("data-notice=\"queue-closed\""), "{page}");
    assert!(page.contains("いまはキューの受付時間外です"), "{page}");
    assert!(
        page.contains("<button type=\"submit\" disabled>キューに入る</button>"),
        "{page}"
    );
    assert!(!page.contains("matchmaker-stopped"), "{page}");
    assert_eq!(count_rows(&db, "queue_entries").await, 0);

    // 受付は 1 時間前から、稼働は 1 時間後から → 受付中だがマッチメイカはまだ
    set_schedule(
        &db.pool,
        true,
        minute_at(60),
        minute_at(180),
        minute_at(-60),
        "open",
    )
    .await;
    let (_, early) = get_queue(&app, Some(me)).await;
    assert_eq!(early["queue_open"], true, "{early}");
    assert_eq!(early["matchmaker_open"], false, "{early}");
    let (_, page) = get_queue_html(&app, me, "/queue").await;
    assert!(!page.contains("queue-closed"), "{page}");
    assert!(page.contains("matchmaker-stopped"), "稼働前の告知: {page}");
    assert!(
        page.contains("<button type=\"submit\">キューに入る</button>"),
        "{page}"
    );
    let (status, joined) = post_queue(&app, me, json!({ "roles": [wish("jungle", 1)] })).await;
    assert_eq!(status, StatusCode::OK, "{joined}");
    assert_eq!(joined["queued"], true);
    assert_eq!(joined["queue_open"], true);

    // 受付が閉じても、入っている人はそのまま（外さない）
    set_schedule(
        &db.pool,
        true,
        minute_at(120),
        minute_at(180),
        minute_at(60),
        "closed-again",
    )
    .await;
    let (_, still) = get_queue(&app, Some(me)).await;
    assert_eq!(still["queued"], true, "{still}");
    assert_eq!(still["queue_open"], false, "{still}");
    assert_eq!(count_rows(&db, "queue_entries").await, 1);

    // スケジュールを外せばいつでも入れる
    delete_queue(&app, me).await;
    set_schedule(&db.pool, false, 0, 1, 0, "manual-only").await;
    let (_, unscheduled) = get_queue(&app, Some(me)).await;
    assert_eq!(unscheduled["queue_open"], true, "{unscheduled}");
    assert_eq!(unscheduled["queue_schedule"], Value::Null, "{unscheduled}");
    let (status, _) = post_queue(&app, me, json!({ "roles": [wish("jungle", 1)] })).await;
    assert_eq!(status, StatusCode::OK);
    println!("受付時間外: 403 queue_closed / 受付中（稼働前）: 200 / スケジュールなし: 200");

    db.done().await;
}

// ----------------------------------------------------------------------
// IP のシャドウバンと監視対象の記録
// ----------------------------------------------------------------------

/// 通知を受け取るだけの偽の通知先。
#[derive(Default)]
struct RecordingAlert {
    seen: std::sync::Mutex<Vec<WatchedAccess>>,
}

#[serenity::async_trait]
impl WatchAlert for RecordingAlert {
    async fn watched_access(&self, access: &WatchedAccess) -> Result<(), String> {
        self.seen.lock().unwrap().push(access.clone());
        Ok(())
    }
}

/// **BAN 中の IP からの投入は、断られずに入り、組まれない行になる。**
///
/// 本人の画面は「キュー中」のまま。他の人の「範囲内 N 人」には数えない。
/// BAN 一覧を変えても、いまキューにいる行は変わらない（次の投入で判定し直す）。
#[sqlx::test(
    fixtures("reference", "app_role"),
    fixtures("../ops/app_privileges.sql")
)]
async fn a_banned_ip_is_let_in_but_never_counted(owner: PgPool) {
    let db = AppDb::connect(&owner).await;
    let app = db.router();
    let admin = AdminRepository::new(db.pool.clone());
    let culprit = player(1);
    let bystander = player(2);
    register(&db, culprit, Rank::Legend).await;
    register(&db, bystander, Rank::Legend).await;

    admin
        .change_shadow_ban(&NewShadowBan {
            subject: BanSubject::Ip("198.51.100.7".to_owned()),
            kind: ShadowBanKind::Banned,
            reason: "荒らし".to_owned(),
            actor_discord_user_id: 42,
            created_at: Utc::now(),
            idempotency_key: "ban-1".to_owned(),
        })
        .await
        .unwrap();

    // BAN 中の IP から投入 → 200。本人には何も出ない
    let (status, joined) = post_queue_from(
        &app,
        culprit,
        Some("198.51.100.7"),
        json!({ "roles": [wish("jungle", 1)] }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{joined}");
    assert_eq!(joined["queued"], true);
    assert!(
        joined.get("shadow_banned").is_none(),
        "本人には見せない: {joined}"
    );
    let shadow: bool =
        sqlx::query_scalar("select shadow_banned from queue_entries where player_id = $1")
            .bind(culprit.0)
            .fetch_one(&db.pool)
            .await
            .unwrap();
    assert!(shadow, "投入時点の判定が写る");

    // 別の IP の人からは見えない（範囲内にも全体にも数えない）
    let (_, other) = post_queue_from(
        &app,
        bystander,
        Some("203.0.113.5"),
        json!({ "roles": [wish("jungle", 1)] }),
    )
    .await;
    assert_eq!(other["in_range"], 0, "{other}");
    assert_eq!(other["queue_total"], 0, "{other}");
    // 本人からは相手が見える（ふつうの画面）
    let (_, mine) = get_queue(&app, Some(culprit)).await;
    assert_eq!(mine["queued"], true);
    assert_eq!(mine["in_range"], 1, "{mine}");

    // 解除しても、いまの行は変わらない。入り直せば判定し直される
    admin
        .change_shadow_ban(&NewShadowBan {
            subject: BanSubject::Ip("198.51.100.7".to_owned()),
            kind: ShadowBanKind::Unbanned,
            reason: "解除".to_owned(),
            actor_discord_user_id: 42,
            created_at: Utc::now(),
            idempotency_key: "unban-1".to_owned(),
        })
        .await
        .unwrap();
    let still: bool =
        sqlx::query_scalar("select shadow_banned from queue_entries where player_id = $1")
            .bind(culprit.0)
            .fetch_one(&db.pool)
            .await
            .unwrap();
    assert!(still, "既存の行は書き換えない");
    post_queue_from(
        &app,
        culprit,
        Some("198.51.100.7"),
        json!({ "roles": [wish("jungle", 1)] }),
    )
    .await;
    let cleared: bool =
        sqlx::query_scalar("select shadow_banned from queue_entries where player_id = $1")
            .bind(culprit.0)
            .fetch_one(&db.pool)
            .await
            .unwrap();
    assert!(!cleared, "入り直せば解除が効く");
    let (_, other) = get_queue(&app, Some(bystander)).await;
    assert_eq!(other["in_range"], 1, "{other}");
    println!("BAN 中の IP: 投入 200 / 相手から見えない / 解除後の入り直しで戻る");
    db.done().await;
}

/// **Discord ユーザー ID で BAN すれば、IP を変えても組まれない。**
/// 他の人（別の ID）は同じ IP からでもふつうに数えられる。
#[sqlx::test(
    fixtures("reference", "app_role"),
    fixtures("../ops/app_privileges.sql")
)]
async fn a_banned_discord_id_is_shadow_banned_from_any_ip(owner: PgPool) {
    let db = AppDb::connect(&owner).await;
    let app = db.router();
    let admin = AdminRepository::new(db.pool.clone());
    // reference fixture: player(1) の Discord ID は 1001
    let culprit = player(1);
    let bystander = player(2);
    register(&db, culprit, Rank::Legend).await;
    register(&db, bystander, Rank::Legend).await;

    admin
        .change_shadow_ban(&NewShadowBan {
            subject: BanSubject::DiscordUser(1001),
            kind: ShadowBanKind::Banned,
            reason: "サブ垢".to_owned(),
            actor_discord_user_id: 42,
            created_at: Utc::now(),
            idempotency_key: "ban-user-1001".to_owned(),
        })
        .await
        .unwrap();

    // どの IP から入っても組まれない行になる
    for ip in ["198.51.100.7", "203.0.113.200"] {
        let (status, joined) = post_queue_from(
            &app,
            culprit,
            Some(ip),
            json!({ "roles": [wish("jungle", 1)] }),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{joined}");
        assert_eq!(joined["queued"], true);
        let shadow: bool =
            sqlx::query_scalar("select shadow_banned from queue_entries where player_id = $1")
                .bind(culprit.0)
                .fetch_one(&db.pool)
                .await
                .unwrap();
        assert!(shadow, "{ip} からでも組まれない");
    }
    // 同じ IP の別の人はふつう
    let (_, other) = post_queue_from(
        &app,
        bystander,
        Some("198.51.100.7"),
        json!({ "roles": [wish("jungle", 1)] }),
    )
    .await;
    assert_eq!(other["in_range"], 0, "BAN 中の人は数えない: {other}");
    let shadow: bool =
        sqlx::query_scalar("select shadow_banned from queue_entries where player_id = $1")
            .bind(bystander.0)
            .fetch_one(&db.pool)
            .await
            .unwrap();
    assert!(!shadow, "ID の BAN は同じ IP の他人に及ばない");
    // 運営操作ログには対象が payload で残る
    let payload: serde_json::Value = sqlx::query_scalar(
        "select payload from admin_actions where kind = 'shadow_banned' order by id desc limit 1",
    )
    .fetch_one(&db.pool)
    .await
    .unwrap();
    assert_eq!(payload["subject_kind"], "discord_user");
    assert_eq!(payload["subject"], "1001");
    println!("Discord ID の BAN: どの IP からでも組まれない、同じ IP の他人は無関係");
    db.done().await;
}

/// **記録対象の人のキュー投入だけ、接続元が記録され、運営に通知される。**
#[sqlx::test(
    fixtures("reference", "app_role"),
    fixtures("../ops/app_privileges.sql")
)]
async fn a_watched_account_has_its_queue_join_recorded_and_reported(owner: PgPool) {
    let db = AppDb::connect(&owner).await;
    let alert = std::sync::Arc::new(RecordingAlert::default());
    // reference fixture: player(1) の Discord ID は 1001
    let app = db.router_watching(AddressWatch::of([1001]), alert.clone());
    let watched = player(1);
    let other = player(2);
    register(&db, watched, Rank::Legend).await;
    register(&db, other, Rank::Legend).await;

    post_queue_from(
        &app,
        other,
        Some("203.0.113.5"),
        json!({ "roles": [wish("jungle", 1)] }),
    )
    .await;
    let recorded: i64 = sqlx::query_scalar("select count(*) from player_address_records")
        .fetch_one(&db.pool)
        .await
        .unwrap();
    assert_eq!(recorded, 0, "対象外は記録しない");

    let (status, _) = post_queue_from(
        &app,
        watched,
        Some("198.51.100.7"),
        json!({ "roles": [wish("jungle", 1)] }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let rows: Vec<(String, Option<String>)> = sqlx::query_as(
        "select source, cf_connecting_ip from player_address_records where player_id = $1",
    )
    .bind(watched.0)
    .fetch_all(&db.pool)
    .await
    .unwrap();
    assert_eq!(
        rows,
        vec![("queue".to_owned(), Some("198.51.100.7".to_owned()))]
    );
    let seen = alert.seen.lock().unwrap().clone();
    assert_eq!(seen.len(), 1);
    assert_eq!(seen[0].discord_user_id, "1001");
    assert_eq!(seen[0].source, AddressSource::Queue);
    assert_eq!(seen[0].address.best_guess(), "198.51.100.7");
    println!("監視対象のキュー投入: 記録 1 行、通知 1 回");
    db.done().await;
}

// ----------------------------------------------------------------------
// 配信中の申告
// ----------------------------------------------------------------------

/// **チェックボックス 1 つで「配信中」を申告できる。**未指定は false。
/// 設定の更新で付け外しでき、キュー画面にも出る。
#[sqlx::test(
    fixtures("reference", "app_role"),
    fixtures("../ops/app_privileges.sql")
)]
async fn a_player_can_declare_that_they_are_streaming(owner: PgPool) {
    let db = AppDb::connect(&owner).await;
    let app = db.router();
    let me = player(1);
    register(&db, me, Rank::Legend).await;

    // 画面にチェックボックスがある
    let (_, page) = get_queue_html(&app, me, "/queue").await;
    assert!(page.contains("id=\"streaming\""), "{page}");
    assert!(page.contains("配信中（YouTube など）"), "{page}");

    // 指定しなければ false
    let (status, joined) = post_queue(&app, me, json!({ "roles": [wish("jungle", 1)] })).await;
    assert_eq!(status, StatusCode::OK, "{joined}");
    assert_eq!(joined["entry"]["streaming"], false);

    // 設定の更新で申告する
    let (status, updated) = post_queue(
        &app,
        me,
        json!({ "roles": [wish("jungle", 1)], "streaming": true }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{updated}");
    assert_eq!(updated["entry"]["streaming"], true);
    let stored: bool =
        sqlx::query_scalar("select streaming from queue_entries where player_id = $1")
            .bind(me.0)
            .fetch_one(&db.pool)
            .await
            .unwrap();
    assert!(stored, "DB に書かれていない");
    let (_, page) = get_queue_html(&app, me, "/queue").await;
    assert!(page.contains("配信中として登録しています"), "{page}");

    // 外すこともできる
    let (_, cleared) = post_queue(
        &app,
        me,
        json!({ "roles": [wish("jungle", 1)], "streaming": false }),
    )
    .await;
    assert_eq!(cleared["entry"]["streaming"], false);
    println!("配信中: 未指定 false → true → false。画面にも出る");

    db.done().await;
}
