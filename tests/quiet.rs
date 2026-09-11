//! 聞き専報告の統合テスト。実際の PostgreSQL を使う。
//!
//! リポジトリは所有者ではなく**アプリ用ロール**（`assay_app`）で接続する。
//!
//! 検証すること:
//!
//! 1. 確定前に報告できないこと
//! 2. 相手チーム／自分自身を対象にできないこと
//! 3. 勝ったチームで 3 票 → −1。2 票では されないこと
//! 4. 負けたチームで 4 票 → −1。3 票では されないこと
//! 5. nullified / voided / disputed で報告できないこと
//! 6. 報告内容・票数・閾値が他プレイヤーの API レスポンスに一切含まれないこと
//! 7. recompute でレートが再構築でき、−1 が再現されること
//! 8. 二重報告が no-op になること

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
    FormationEventKind as K, FormationId, LifecycleConfig, PlayerId, Timestamp,
};
use unite_matchsystem::match_screen::{MatchRepository, MatchState, router};
use unite_matchsystem::nullified::NullifiedReportRepository;
use unite_matchsystem::persistence::{
    FormationRepository, NewEvent, NewFormation, NewFormationPlayer, Team,
};
use unite_matchsystem::quiet::{QuietReportConfig, QuietReportOutcome, QuietReportRepository};
use unite_matchsystem::rating::RatingAdjustmentRepository;
use unite_matchsystem::registration::AuthConfig;
use unite_matchsystem::registration::session::{SESSION_COOKIE, Session, set_session};
use uuid::Uuid;

const APP_ROLE: &str = "assay_app";
const APP_ROLE_PASSWORD: &str = "assay_app_test";
const COOKIE_DOMAIN: &str = "match.example.test";
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
    quiet: QuietReportRepository,
    ratings: RatingAdjustmentRepository,
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
            quiet: QuietReportRepository::new(pool.clone()),
            ratings: RatingAdjustmentRepository::new(pool.clone()),
            pool,
        }
    }

    fn router(&self) -> Router {
        router(MatchState::new(
            config(),
            self.formations.clone(),
            self.matches.clone(),
            self.quiet.clone(),
            NullifiedReportRepository::new(self.pool.clone()),
            LifecycleConfig::default(),
            "kyogre",
            &["大会モード".to_owned()],
        ))
    }

    async fn done(self) {
        self.pool.close().await;
    }
}

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

/// players 1〜5 が red、6〜10 が blue。
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
                streaming: false,
                mu_at_match: 25.0,
                sigma_at_match: 8.33,
            })
            .collect(),
    }
}

async fn seed_players(db: &AppDb, count: u128) {
    for n in 1..=count {
        sqlx::query(
            "insert into players (id, discord_user_id, created_at, trainer_name) \
             values ($1, $2, now(), $3) on conflict do nothing",
        )
        .bind(player(n).0)
        .bind(1000 + n as i64)
        .bind(format!("p{n}"))
        .execute(&db.pool)
        .await
        .expect("seed a player");
    }
}

/// formation を作って `in_progress` まで進める。
async fn play(db: &AppDb, n: u128) {
    db.formations.create_formation(&formation(n)).await.unwrap();
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

/// 終端イベントを積む。
async fn finish(db: &AppDb, n: u128, kind: K, outcome: &str) {
    let event = NewEvent::new(kind, None, at(10), format!("finish-{n}-{}", kind.as_str()))
        .with_payload(json!({ "outcome": outcome }));
    db.formations.append_event(fid(n), &event).await.unwrap();
}

async fn confirmed(db: &AppDb, n: u128, winner: &str) {
    play(db, n).await;
    finish(db, n, K::Confirmed, winner).await;
}

/// 聞き専報告を 1 件出す（リポジトリ経由）。
async fn quiet(
    db: &AppDb,
    n: u128,
    reporter: u128,
    target: u128,
) -> Result<QuietReportOutcome, unite_matchsystem::quiet::QuietReportError> {
    db.quiet
        .report(
            fid(n),
            player(reporter),
            player(target),
            at(100),
            &format!("q-{n}-{reporter}-{target}"),
        )
        .await
}

async fn rating(db: &AppDb, who: u128) -> f64 {
    db.ratings.display_rating(player(who)).await.unwrap()
}

async fn send(app: &Router, request: Request<Body>) -> Response<Body> {
    app.clone().oneshot(request).await.unwrap()
}

async fn get_json(app: &Router, n: u128, as_player: PlayerId) -> (StatusCode, Value) {
    let request = Request::get(format!("/match/{}", fid(n).0))
        .header(header::COOKIE, session_cookie(as_player))
        .body(Body::empty())
        .unwrap();
    let response = send(app, request).await;
    let status = response.status();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

async fn get_html(app: &Router, n: u128, as_player: PlayerId) -> String {
    let request = Request::get(format!("/match/{}", fid(n).0))
        .header(header::COOKIE, session_cookie(as_player))
        .header(header::ACCEPT, "text/html")
        .body(Body::empty())
        .unwrap();
    let bytes = send(app, request)
        .await
        .into_body()
        .collect()
        .await
        .unwrap()
        .to_bytes();
    String::from_utf8(bytes.to_vec()).unwrap()
}

async fn post_quiet(
    app: &Router,
    n: u128,
    as_player: PlayerId,
    target: PlayerId,
) -> (StatusCode, Value) {
    let request = Request::post(format!("/match/{}/quiet-report", fid(n).0))
        .header(header::COOKIE, session_cookie(as_player))
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(json!({ "target": target.0 }).to_string()))
        .unwrap();
    let response = send(app, request).await;
    let status = response.status();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

// ----------------------------------------------------------------------
// 1 / 5. 受け付ける状態の検査
// ----------------------------------------------------------------------

/// **1. 確定前は報告できない。**勝敗で閾値が変わるため。
#[sqlx::test(fixtures("app_role"), fixtures("../ops/app_privileges.sql"))]
async fn a_report_before_confirmation_is_refused(owner: PgPool) {
    let db = AppDb::connect(&owner).await;
    seed_players(&db, 10).await;
    play(&db, 1).await; // in_progress のまま

    let refused = quiet(&db, 1, 2, 3).await.unwrap_err();
    assert_eq!(refused.code(), "not_confirmed");
    let rows: i64 = sqlx::query_scalar("select count(*) from quiet_reports")
        .fetch_one(&db.pool)
        .await
        .unwrap();
    assert_eq!(rows, 0, "確定前なのに積まれている");

    // HTTP でも 409
    let app = db.router();
    let (status, body) = post_quiet(&app, 1, player(2), player(3)).await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    println!("確定前: {} で拒否", refused.code());

    // 確定させれば通る
    finish(&db, 1, K::Confirmed, "red").await;
    assert_eq!(
        quiet(&db, 1, 2, 3).await.unwrap(),
        QuietReportOutcome::Recorded
    );
    println!("確定後: 受け付ける");

    db.done().await;
}

/// **5. 勝敗がつかない試合では報告できない。**閾値が決まらないため。
#[sqlx::test(fixtures("app_role"), fixtures("../ops/app_privileges.sql"))]
async fn a_match_without_a_winner_refuses_reports(owner: PgPool) {
    let db = AppDb::connect(&owner).await;
    seed_players(&db, 10).await;

    // nullified / voided / disputed / draw
    for (n, kind, outcome, expected) in [
        (1u128, K::Nullified, "nullified", "not_confirmed"),
        (2, K::Voided, "voided", "not_confirmed"),
        (3, K::Disputed, "disputed", "not_confirmed"),
        (4, K::Confirmed, "draw", "no_winner"),
    ] {
        play(&db, n).await;
        finish(&db, n, kind, outcome).await;
        let refused = quiet(&db, n, 2, 3).await.unwrap_err();
        assert_eq!(refused.code(), expected, "formation {n}");
        println!("{outcome}: {} で拒否", refused.code());
    }
    let rows: i64 = sqlx::query_scalar("select count(*) from quiet_reports")
        .fetch_one(&db.pool)
        .await
        .unwrap();
    assert_eq!(rows, 0);

    db.done().await;
}

// ----------------------------------------------------------------------
// 2. 対象の検査
// ----------------------------------------------------------------------

/// **2. 相手チームも自分自身も対象にできない。**サーバー側で弾く。
#[sqlx::test(fixtures("app_role"), fixtures("../ops/app_privileges.sql"))]
async fn only_a_teammate_can_be_reported(owner: PgPool) {
    let db = AppDb::connect(&owner).await;
    seed_players(&db, 12).await;
    confirmed(&db, 1, "red").await;

    // player 2 は red。6〜10 は blue
    let opposing = quiet(&db, 1, 2, 6).await.unwrap_err();
    assert_eq!(opposing.code(), "opposing_team");
    // 自分自身
    let myself = quiet(&db, 1, 2, 2).await.unwrap_err();
    assert_eq!(myself.code(), "self_report");
    // 参加していない人
    let outsider = quiet(&db, 1, 2, 11).await.unwrap_err();
    assert_eq!(outsider.code(), "not_a_participant");
    // **相手チームと非参加者の文言は同じ。**分けると「その人がこの試合にいるか」が分かる
    assert_eq!(opposing.message(), outsider.message());

    // 味方は通る
    assert_eq!(
        quiet(&db, 1, 2, 3).await.unwrap(),
        QuietReportOutcome::Recorded
    );

    let rows: i64 = sqlx::query_scalar("select count(*) from quiet_reports")
        .fetch_one(&db.pool)
        .await
        .unwrap();
    assert_eq!(rows, 1, "弾いたぶんが積まれている");
    println!(
        "相手チーム: {} / 自分自身: {} / 非参加者: {}（文言は相手チームと同じ）",
        opposing.code(),
        myself.code(),
        outsider.code()
    );

    db.done().await;
}

// ----------------------------------------------------------------------
// 3 / 4. 閾値
// ----------------------------------------------------------------------

/// **3. 勝ったチームは 3 票で −1。2 票では下がらない。**
#[sqlx::test(fixtures("app_role"), fixtures("../ops/app_privileges.sql"))]
async fn three_votes_from_the_winning_team_cost_one_point(owner: PgPool) {
    let db = AppDb::connect(&owner).await;
    seed_players(&db, 10).await;
    confirmed(&db, 1, "red").await; // red が勝ち。対象は player 1（red）

    let won = 1500.0 + 7.0;
    assert_eq!(rating(&db, 1).await, won);

    // 2 票では下がらない
    quiet(&db, 1, 2, 1).await.unwrap();
    quiet(&db, 1, 3, 1).await.unwrap();
    assert_eq!(rating(&db, 1).await, won, "2 票で下がっている");
    println!("勝ちチーム 2 票: {} （変化なし）", rating(&db, 1).await);

    // 3 票目で下がる
    quiet(&db, 1, 4, 1).await.unwrap();
    assert_eq!(rating(&db, 1).await, won - 1.0, "3 票で下がっていない");
    println!("勝ちチーム 3 票: {}", rating(&db, 1).await);

    // 4 票目を足しても **-1 のまま**（1 試合につき 1 回）
    quiet(&db, 1, 5, 1).await.unwrap();
    assert_eq!(
        rating(&db, 1).await,
        won - 1.0,
        "票を足すたびに下がっている"
    );
    println!("勝ちチーム 4 票: {} （-1 のまま）", rating(&db, 1).await);

    db.done().await;
}

/// **4. 負けたチームも 3 票で −1。勝ち側と同じ。**
///
/// 以前は負け側だけ 4 票（対象者を除いた 4 人の満場一致）を要求していた。
/// 負け直後の「戦犯選出」を防ぐ意図だったが、負け側でだけ 1 人の反対で
/// 潰せる状態になっていたので、勝ち負けとも 3 に統一した。
#[sqlx::test(fixtures("app_role"), fixtures("../ops/app_privileges.sql"))]
async fn three_votes_from_the_losing_team_cost_one_point(owner: PgPool) {
    let db = AppDb::connect(&owner).await;
    seed_players(&db, 10).await;
    confirmed(&db, 1, "red").await; // red が勝ち → blue（6〜10）が負け

    let lost = 1500.0 - 7.0;
    assert_eq!(rating(&db, 6).await, lost);

    // 2 票では下がらない（2 人の口裏合わせで成立させない）
    for reporter in [7u128, 8] {
        quiet(&db, 1, reporter, 6).await.unwrap();
    }
    assert_eq!(rating(&db, 6).await, lost, "2 票で下がっている");
    println!("負けチーム 2 票: {} （変化なし）", rating(&db, 6).await);

    // 3 票目で下がる。**勝ち側と同じ票数**
    quiet(&db, 1, 9, 6).await.unwrap();
    assert_eq!(rating(&db, 6).await, lost - 1.0, "3 票で下がっていない");
    println!("負けチーム 3 票: {}", rating(&db, 6).await);

    // 4 票目（＝報告できる 4 人すべて）でも −1 のまま
    quiet(&db, 1, 10, 6).await.unwrap();
    assert_eq!(rating(&db, 6).await, lost - 1.0, "票数で増えている");
    println!("負けチーム 4 票（満場一致）: {}", rating(&db, 6).await);

    // 対象を除く同チームは 4 人しかいない＝これ以上票は増やせない
    let eligible: i64 = sqlx::query_scalar(
        "select count(*) from formation_players fp \
         where fp.formation_id = $1 and fp.team = 'blue' and fp.player_id <> $2",
    )
    .bind(fid(1).0)
    .bind(player(6).0)
    .fetch_one(&db.pool)
    .await
    .unwrap();
    assert_eq!(eligible, 4, "負けチームで報告できるのは 4 人");

    db.done().await;
}

// ----------------------------------------------------------------------
// 6. 秘匿
// ----------------------------------------------------------------------

/// **6. 票数・閾値・報告内容が API にも HTML にも出ない。**
#[sqlx::test(fixtures("app_role"), fixtures("../ops/app_privileges.sql"))]
async fn the_votes_and_threshold_never_reach_a_player(owner: PgPool) {
    let db = AppDb::connect(&owner).await;
    seed_players(&db, 10).await;
    confirmed(&db, 1, "red").await;
    // player 1 に 3 票（ペナルティ成立）
    for reporter in [2u128, 3, 4] {
        quiet(&db, 1, reporter, 1).await.unwrap();
    }
    let app = db.router();

    // 報告された本人（player 1）から見た JSON
    let (status, target_view) = get_json(&app, 1, player(1)).await;
    assert_eq!(status, StatusCode::OK);
    // まったく無関係な味方（player 5）から見た JSON
    let (_, bystander) = get_json(&app, 1, player(5)).await;
    // 相手チーム（player 6）から見た JSON
    let (_, opponent) = get_json(&app, 1, player(6)).await;

    for (name, view) in [
        ("報告された本人", &target_view),
        ("同チームの第三者", &bystander),
        ("相手チーム", &opponent),
    ] {
        let json = view.to_string();
        for forbidden in [
            "votes",
            "quiet_reports",
            "threshold",
            "reporter",
            "penalty",
            "penalised",
            "quiet_report_penalties",
        ] {
            assert!(
                !json.contains(forbidden),
                "{name} に {forbidden} が漏れている: {json}"
            );
        }
        // **票数を数えられる形も無い。**対象の一覧も入っていない
        assert!(!json.contains("\"3\""), "{name}: {json}");
    }
    println!("JSON（報告された本人）: {target_view}");

    // HTML にも票数・閾値は出ない
    let html = get_html(&app, 1, player(5)).await;
    for forbidden in ["3票", "3 票", "4票", "4 票", "あと", "票入"] {
        assert!(!html.contains(forbidden), "{forbidden} が漏れている");
    }
    // 出てよいのは「一定以上」という表現だけ
    assert!(html.contains("一定以上の報告"), "{html}");
    assert!(html.contains("通報の義務はありません"), "{html}");
    assert!(html.contains("報告した側にペナルティ"), "{html}");
    println!("HTML: 票数・閾値なし。必要な 3 つの文言あり");

    // **自分が出したかどうかだけは返る**（二度押しさせないため）
    let (_, reporter_view) = get_json(&app, 1, player(2)).await;
    assert_eq!(reporter_view["actions"]["quiet_reported"], json!(true));
    assert_eq!(bystander["actions"]["quiet_reported"], json!(false));
    // 誰を報告したかは入っていない
    assert!(!reporter_view.to_string().contains("target"));

    db.done().await;
}

// ----------------------------------------------------------------------
// 7. recompute
// ----------------------------------------------------------------------

/// **7. レートは導出値のまま。**同じログから何度でも同じ値が出る。
///
/// 閾値を変えると**過去の試合も再判定される**（docs/02 不変条件 3）。
#[sqlx::test(fixtures("app_role"), fixtures("../ops/app_privileges.sql"))]
async fn the_penalty_is_rebuilt_from_the_log(owner: PgPool) {
    let db = AppDb::connect(&owner).await;
    seed_players(&db, 10).await;
    confirmed(&db, 1, "red").await;
    for reporter in [2u128, 3, 4] {
        quiet(&db, 1, reporter, 1).await.unwrap();
    }

    // 何度読んでも同じ（現在値のテーブルが無いので、毎回ログから作り直される）
    let first = rating(&db, 1).await;
    let second = rating(&db, 1).await;
    assert_eq!(first, second);
    assert_eq!(first, 1500.0 + 7.0 - 1.0);

    // **閾値を上げると、同じログでも成立しなくなる**
    db.quiet
        .sync_thresholds(
            QuietReportConfig {
                winner_threshold: 4,
                loser_threshold: 4,
            },
            at(200),
        )
        .await
        .unwrap();
    assert_eq!(
        rating(&db, 1).await,
        1500.0 + 7.0,
        "閾値を上げても再判定されていない"
    );
    println!("閾値 3 → 4: {} → {}", first, rating(&db, 1).await);

    // **下げれば戻る。**報告のログは 1 行も動いていない
    db.quiet
        .sync_thresholds(
            QuietReportConfig {
                winner_threshold: 3,
                loser_threshold: 3,
            },
            at(300),
        )
        .await
        .unwrap();
    assert_eq!(rating(&db, 1).await, 1500.0 + 7.0 - 1.0);
    let rows: i64 = sqlx::query_scalar("select count(*) from quiet_reports")
        .fetch_one(&db.pool)
        .await
        .unwrap();
    assert_eq!(rows, 3, "報告のログが動いている");
    println!(
        "閾値 4 → 3: {} / 報告のログ {rows} 行（変化なし）",
        rating(&db, 1).await
    );

    db.done().await;
}

// ----------------------------------------------------------------------
// 8. 冪等
// ----------------------------------------------------------------------

/// **8. 二重報告は no-op。**同じキーでも、違うキーの同じ組み合わせでも増えない。
#[sqlx::test(fixtures("app_role"), fixtures("../ops/app_privileges.sql"))]
async fn a_repeated_report_is_a_no_op(owner: PgPool) {
    let db = AppDb::connect(&owner).await;
    seed_players(&db, 10).await;
    confirmed(&db, 1, "red").await;

    assert_eq!(
        quiet(&db, 1, 2, 1).await.unwrap(),
        QuietReportOutcome::Recorded
    );
    // 同じキー
    assert_eq!(
        quiet(&db, 1, 2, 1).await.unwrap(),
        QuietReportOutcome::AlreadyReported
    );
    // **違うキーでも同じ組み合わせなら増えない**（unique index が弾く）
    let again = db
        .quiet
        .report(fid(1), player(2), player(1), at(500), "different-key")
        .await
        .unwrap();
    assert_eq!(again, QuietReportOutcome::AlreadyReported);

    let rows: i64 = sqlx::query_scalar("select count(*) from quiet_reports")
        .fetch_one(&db.pool)
        .await
        .unwrap();
    assert_eq!(rows, 1, "二重に積まれている");

    // 別の相手なら通る（1 試合で 1 人までではない）
    assert_eq!(
        quiet(&db, 1, 2, 3).await.unwrap(),
        QuietReportOutcome::Recorded
    );
    println!("同じ相手への二度目: no-op / 別の相手: 受け付ける");

    db.done().await;
}

// ----------------------------------------------------------------------
// 運営パネル
// ----------------------------------------------------------------------

/// 運営は「誰が誰を」「報告した数の多い順」「その試合で勝っていたか」を見られる。
#[sqlx::test(fixtures("app_role"), fixtures("../ops/app_privileges.sql"))]
async fn the_admin_view_ranks_reporters_and_shows_which_side_they_were_on(owner: PgPool) {
    let db = AppDb::connect(&owner).await;
    seed_players(&db, 10).await;
    // 1 試合目: red の勝ち。負けた blue の 7 が 6 を報告
    confirmed(&db, 1, "red").await;
    quiet(&db, 1, 7, 6).await.unwrap();
    // 2 試合目: blue の勝ち。負けた red の 2 が 1 を報告し、勝った blue の 7 も 6 を報告
    play(&db, 2).await;
    finish(&db, 2, K::Confirmed, "blue").await;
    quiet(&db, 2, 2, 1).await.unwrap();
    quiet(&db, 2, 7, 6).await.unwrap();

    let tallies = db.quiet.admin_tallies(50).await.unwrap();
    // **報告した数の多い順。**7 が 2 件で先頭
    assert_eq!(tallies[0].player_id, player(7).0);
    assert_eq!(tallies[0].reports_made, 2);
    // 7 の 1 件目は負け試合、2 件目は勝ち試合
    assert_eq!(tallies[0].made_while_losing, 1);
    for row in &tallies {
        println!(
            "{}: 報告 {} 件（うち負け {}）/ 被報告 {} 件 / ペナルティ {} 件",
            row.trainer_name.as_deref().unwrap_or("?"),
            row.reports_made,
            row.made_while_losing,
            row.reports_received,
            row.penalties
        );
    }

    let history = db.quiet.admin_history(50).await.unwrap();
    assert_eq!(history.len(), 3);
    // 新しい順。いちばん新しいのは 2 試合目の 7 → 6（blue が勝った試合）
    assert_eq!(history[0].reporter_id, player(7).0);
    assert_eq!(history[0].reporter_won, Some(true));
    // いちばん古いのは 1 試合目の 7 → 6（blue が負けた試合）
    assert_eq!(history[2].reporter_won, Some(false));
    println!(
        "履歴の勝敗: {:?}",
        history
            .iter()
            .map(|row| row.reporter_won)
            .collect::<Vec<_>>()
    );

    db.done().await;
}
