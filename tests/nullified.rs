//! 無効試合の原因になった人の通報の統合テスト。実際の PostgreSQL を使う。
//!
//! リポジトリは所有者ではなく**アプリ用ロール**（`assay_app`）で接続する。
//!
//! 検証すること:
//!
//! 1. 無効試合を報告した人だけが通報できること
//! 2. **相手チームも通報できる**こと（聞き専報告との違い）
//! 3. 5 票で −21。4 票では下がらないこと
//! 4. 「誰も通報しない」が 1 票として記録され、誰のペナルティにもならないこと
//! 5. 試合が nullified で終わらなければレートに効かないこと
//! 6. 票数も閾値も画面にも API にも出ないこと

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
    FormationRepository, NewEvent, NewFormation, NewFormationPlayer, NewReport, Outcome, Team,
};
use unite_matchsystem::quiet::QuietReportRepository;
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
    nullified: NullifiedReportRepository,
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
            nullified: NullifiedReportRepository::new(pool.clone()),
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
            self.nullified.clone(),
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

/// 本人の報告を 1 件積む（勝敗でも無効試合でも）。
async fn report_as(db: &AppDb, n: u128, who: u128, outcome: &str) {
    let outcome = match outcome {
        "red" => Outcome::Win(Team::Red),
        "blue" => Outcome::Win(Team::Blue),
        _ => Outcome::Nullified,
    };
    db.formations
        .append_latest_report(NewReport {
            formation_id: fid(n),
            player_id: player(who),
            reported_at: at(20),
            outcome,
            raw_result: json!({ "source": "web", "choice": outcome.as_str() }),
            idempotency_key: format!("rep-{n}-{who}"),
            supersedes_id: None,
        })
        .await
        .expect("record the report");
}

/// 無効試合の通報を 1 件出す（リポジトリ経由）。`target` が `None` は「誰も通報しない」。
async fn accuse(
    db: &AppDb,
    n: u128,
    reporter: u128,
    target: Option<u128>,
) -> Result<
    unite_matchsystem::nullified::NullifiedReportOutcome,
    unite_matchsystem::nullified::NullifiedReportError,
> {
    db.nullified
        .report(fid(n), player(reporter), target.map(player), at(100))
        .await
}

async fn post_accuse(
    app: &Router,
    n: u128,
    as_player: PlayerId,
    target: Option<PlayerId>,
) -> (StatusCode, Value) {
    let body = match target {
        Some(target) => json!({ "target": target.0 }),
        None => json!({ "target": "" }),
    };
    let request = Request::post(format!("/match/{}/nullified-report", fid(n).0))
        .header(header::COOKIE, session_cookie(as_player))
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(body.to_string()))
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
// 1. 通報できるのは無効試合を報告した人だけ
// ----------------------------------------------------------------------

/// **勝敗を報告した人には入口が無い。**無効試合を報告した人だけが通報できる。
#[sqlx::test(fixtures("app_role"), fixtures("../ops/app_privileges.sql"))]
async fn only_a_player_who_reported_nullified_can_accuse(owner: PgPool) {
    let db = AppDb::connect(&owner).await;
    seed_players(&db, 10).await;
    play(&db, 1).await;

    // 2 番は無効試合、3 番は赤の勝ちを報告した
    report_as(&db, 1, 2, "nullified").await;
    report_as(&db, 1, 3, "red").await;

    // 勝敗を報告した人は断られる
    let refused = accuse(&db, 1, 3, Some(4)).await.unwrap_err();
    assert_eq!(refused.code(), "not_a_nullified_reporter");
    // まだ何も報告していない人も同じ
    let refused = accuse(&db, 1, 5, Some(4)).await.unwrap_err();
    assert_eq!(refused.code(), "not_a_nullified_reporter");
    // 無効試合を報告した人は通る
    assert_eq!(
        accuse(&db, 1, 2, Some(4)).await.unwrap(),
        unite_matchsystem::nullified::NullifiedReportOutcome::Recorded
    );

    let rows: i64 = sqlx::query_scalar("select count(*) from nullified_reports")
        .fetch_one(&db.pool)
        .await
        .unwrap();
    assert_eq!(rows, 1, "断ったぶんまで積まれている");
    println!("勝敗を報告した人・未報告の人: 拒否 / 無効試合を報告した人: 受理（{rows} 行）");

    db.done().await;
}

/// **自分自身は選べない。**
#[sqlx::test(fixtures("app_role"), fixtures("../ops/app_privileges.sql"))]
async fn a_player_cannot_accuse_themselves(owner: PgPool) {
    let db = AppDb::connect(&owner).await;
    seed_players(&db, 10).await;
    play(&db, 1).await;
    report_as(&db, 1, 2, "nullified").await;

    let refused = accuse(&db, 1, 2, Some(2)).await.unwrap_err();
    assert_eq!(refused.code(), "self_report");
    // 参加していない人も選べない
    let refused = accuse(&db, 1, 2, Some(99)).await.unwrap_err();
    assert_eq!(refused.code(), "not_a_participant");
    println!("自分自身・非参加者: どちらも拒否");

    db.done().await;
}

// ----------------------------------------------------------------------
// 2. 相手チームも通報できる（聞き専報告との違い）
// ----------------------------------------------------------------------

/// **相手チームも対象にできる。**部屋を壊すのは味方とは限らない。
#[sqlx::test(fixtures("app_role"), fixtures("../ops/app_privileges.sql"))]
async fn the_opposing_team_can_be_accused(owner: PgPool) {
    let db = AppDb::connect(&owner).await;
    seed_players(&db, 10).await;
    play(&db, 1).await;
    report_as(&db, 1, 2, "nullified").await;

    // 2 番は red（1〜5）。相手チームの 8 番を選ぶ
    assert_eq!(
        accuse(&db, 1, 2, Some(8)).await.unwrap(),
        unite_matchsystem::nullified::NullifiedReportOutcome::Recorded
    );

    // 選べる相手は自分以外の 9 人（両チーム）
    let reportable = db.nullified.reportable(fid(1), player(2)).await.unwrap();
    assert_eq!(reportable.len(), 9, "自分以外の 9 人が選べない");
    let red = reportable.iter().filter(|p| p.team == "red").count();
    let blue = reportable.iter().filter(|p| p.team == "blue").count();
    assert_eq!((red, blue), (4, 5), "自チーム 4 / 相手 5");
    println!("選べる相手: 自チーム {red} 人 / 相手チーム {blue} 人");

    db.done().await;
}

// ----------------------------------------------------------------------
// 3. 5 票で −21
// ----------------------------------------------------------------------

/// **5 票で −21。4 票では下がらない。**
#[sqlx::test(fixtures("app_role"), fixtures("../ops/app_privileges.sql"))]
async fn five_votes_cost_twenty_one_points(owner: PgPool) {
    let db = AppDb::connect(&owner).await;
    seed_players(&db, 10).await;
    play(&db, 1).await;
    // 9 人が無効試合を報告し、10 番が原因として通報される
    for who in 1..=9u128 {
        report_as(&db, 1, who, "nullified").await;
    }
    finish(&db, 1, K::Nullified, "nullified").await;

    let before = rating(&db, 10).await;
    assert_eq!(before, 1500.0, "無効試合なので勝敗の増減は入らない");

    // 4 票では下がらない
    for reporter in [1u128, 2, 3, 4] {
        accuse(&db, 1, reporter, Some(10)).await.unwrap();
    }
    assert_eq!(rating(&db, 10).await, before, "4 票で下がっている");
    println!("4 票: {} （変化なし）", rating(&db, 10).await);

    // 5 票目で下がる
    accuse(&db, 1, 5, Some(10)).await.unwrap();
    assert_eq!(rating(&db, 10).await, before - 21.0, "5 票で下がっていない");
    println!("5 票: {}", rating(&db, 10).await);

    // 6 票目でも −21 のまま（票数で増えない）
    accuse(&db, 1, 6, Some(10)).await.unwrap();
    assert_eq!(rating(&db, 10).await, before - 21.0, "票数で増えている");
    println!("6 票: {}", rating(&db, 10).await);

    db.done().await;
}

// ----------------------------------------------------------------------
// 4. 「誰も通報しない」
// ----------------------------------------------------------------------

/// **「誰も通報しない」は 1 票として記録され、誰のペナルティにもならない。**
///
/// 答えていない人と区別できないと、画面が催促を出し続けることになる。
#[sqlx::test(fixtures("app_role"), fixtures("../ops/app_privileges.sql"))]
async fn choosing_nobody_is_recorded_as_a_vote(owner: PgPool) {
    let db = AppDb::connect(&owner).await;
    seed_players(&db, 10).await;
    play(&db, 1).await;
    for who in 1..=9u128 {
        report_as(&db, 1, who, "nullified").await;
    }
    finish(&db, 1, K::Nullified, "nullified").await;

    // 5 人が「誰も通報しない」を選ぶ
    for reporter in 1..=5u128 {
        assert_eq!(
            accuse(&db, 1, reporter, None).await.unwrap(),
            unite_matchsystem::nullified::NullifiedReportOutcome::Recorded
        );
    }
    // 票としては 5 行入っている
    let rows: i64 = sqlx::query_scalar("select count(*) from nullified_reports")
        .fetch_one(&db.pool)
        .await
        .unwrap();
    assert_eq!(rows, 5);
    // **その人はもう催促されない**
    assert!(db.nullified.has_reported(fid(1), player(1)).await.unwrap());
    // 誰のレートも動かない
    for who in 1..=10u128 {
        assert_eq!(rating(&db, who).await, 1500.0, "p{who} が動いている");
    }
    // ペナルティのビューにも 1 行も出ない
    let penalties: i64 = sqlx::query_scalar("select count(*) from nullified_report_penalties")
        .fetch_one(&db.pool)
        .await
        .unwrap();
    assert_eq!(penalties, 0);
    println!("「誰も通報しない」5 票: ログ {rows} 行 / ペナルティ {penalties} 件");

    db.done().await;
}

/// **1 人 1 票で選び直せない。**二度目は対象が違っても no-op。
#[sqlx::test(fixtures("app_role"), fixtures("../ops/app_privileges.sql"))]
async fn one_vote_per_player_cannot_be_changed(owner: PgPool) {
    let db = AppDb::connect(&owner).await;
    seed_players(&db, 10).await;
    play(&db, 1).await;
    report_as(&db, 1, 2, "nullified").await;

    assert_eq!(
        accuse(&db, 1, 2, Some(8)).await.unwrap(),
        unite_matchsystem::nullified::NullifiedReportOutcome::Recorded
    );
    // 同じ相手も、別の相手も、「誰も通報しない」も、すべて no-op
    for second in [Some(8u128), Some(9), None] {
        assert_eq!(
            accuse(&db, 1, 2, second).await.unwrap(),
            unite_matchsystem::nullified::NullifiedReportOutcome::AlreadyReported,
            "{second:?} で増えている"
        );
    }
    let rows: i64 = sqlx::query_scalar("select count(*) from nullified_reports")
        .fetch_one(&db.pool)
        .await
        .unwrap();
    assert_eq!(rows, 1, "選び直せてしまっている");
    // 最初に選んだ相手のまま
    let target: Option<Uuid> =
        sqlx::query_scalar("select target_id from nullified_reports limit 1")
            .fetch_one(&db.pool)
            .await
            .unwrap();
    assert_eq!(target, Some(player(8).0));
    println!("1 人 1 票: 4 回送って {rows} 行、対象は最初のまま");

    db.done().await;
}

// ----------------------------------------------------------------------
// 5. 無効試合にならなければレートに効かない
// ----------------------------------------------------------------------

/// **試合が confirmed で終われば、票が集まってもレートは動かない。**
///
/// 通報は報告直後に受け付けるので、結局勝敗がついた試合の票も残る。
/// その票は記録として残るだけで、ペナルティにはならない。
#[sqlx::test(fixtures("app_role"), fixtures("../ops/app_privileges.sql"))]
async fn votes_do_not_count_unless_the_match_is_nullified(owner: PgPool) {
    let db = AppDb::connect(&owner).await;
    seed_players(&db, 10).await;
    play(&db, 1).await;
    // 6 人が無効試合を報告して 10 番を通報するが、試合は赤の勝ちで確定する
    for who in 1..=6u128 {
        report_as(&db, 1, who, "nullified").await;
        accuse(&db, 1, who, Some(10)).await.unwrap();
    }
    finish(&db, 1, K::Confirmed, "red").await;

    // 票は 6 行残っている
    let rows: i64 = sqlx::query_scalar("select count(*) from nullified_reports")
        .fetch_one(&db.pool)
        .await
        .unwrap();
    assert_eq!(rows, 6);
    // **ペナルティは出ない**（10 番は blue なので負けの −7 だけ）
    let penalties: i64 = sqlx::query_scalar("select count(*) from nullified_report_penalties")
        .fetch_one(&db.pool)
        .await
        .unwrap();
    assert_eq!(penalties, 0, "確定した試合でペナルティが出ている");
    assert_eq!(rating(&db, 10).await, 1500.0 - 7.0, "負けの増減だけのはず");
    println!("confirmed で終わった試合: 票 {rows} 行 / ペナルティ {penalties} 件");

    db.done().await;
}

// ----------------------------------------------------------------------
// 6. 画面に票数も閾値も出ない
// ----------------------------------------------------------------------

/// **票数も閾値もプレイヤーには届かない。**HTML にも JSON にも入れない。
#[sqlx::test(fixtures("app_role"), fixtures("../ops/app_privileges.sql"))]
async fn the_votes_and_threshold_never_reach_a_player(owner: PgPool) {
    let db = AppDb::connect(&owner).await;
    seed_players(&db, 10).await;
    play(&db, 1).await;
    for who in 1..=6u128 {
        report_as(&db, 1, who, "nullified").await;
    }
    // 5 票入れて成立させておく
    for reporter in 1..=5u128 {
        accuse(&db, 1, reporter, Some(10)).await.unwrap();
    }
    finish(&db, 1, K::Nullified, "nullified").await;
    let app = db.router();

    // まだ通報していない 6 番の画面
    let html = get_html(&app, 1, player(6)).await;
    assert!(html.contains("無効試合の原因"), "入口が無い: {html}");
    assert!(html.contains("誰も通報しない"), "{html}");
    assert!(html.contains("相手チーム"), "{html}");
    // **票数も閾値も書かない。**ページ全体には UUID や時刻の数字が入るので、
    // 通報の枠だけを切り出して見る
    let section = html
        .split("id=\"nullified-title\"")
        .nth(1)
        .expect("the nullified section")
        .split("</section>")
        .next()
        .expect("the end of the section");
    // 「残り」は「記録に残ります」に当たるので入れない（部分一致の誤検出）
    for leak in ["票", "閾値", "あと", "21"] {
        assert!(!section.contains(leak), "{leak} が漏れている: {section}");
    }

    // JSON にも入っていない
    let (status, view) = get_json(&app, 1, player(6)).await;
    assert_eq!(status, StatusCode::OK);
    let raw = view.to_string();
    for leak in ["votes", "threshold", "nullified_votes", "penalty"] {
        assert!(!raw.contains(leak), "{leak} が漏れている: {raw}");
    }
    assert_eq!(view["actions"]["nullified_report"], json!(true));
    assert_eq!(view["actions"]["nullified_reported"], json!(false));

    // 既に通報した 1 番には入口が出ない
    let (_, mine) = get_json(&app, 1, player(1)).await;
    assert_eq!(mine["actions"]["nullified_report"], json!(false));
    assert_eq!(mine["actions"]["nullified_reported"], json!(true));
    let mine_html = get_html(&app, 1, player(1)).await;
    assert!(mine_html.contains("受け付けました"), "{mine_html}");
    assert!(
        mine_html.contains("何票入っているかは表示されません"),
        "{mine_html}"
    );

    // 勝敗を報告した人（＝通報できない人）には入口ごと無い
    report_as(&db, 1, 7, "red").await;
    let (_, other) = get_json(&app, 1, player(7)).await;
    assert_eq!(other["actions"]["nullified_report"], json!(false));
    println!("票数・閾値: HTML にも JSON にも出ない");

    db.done().await;
}

// ----------------------------------------------------------------------
// HTTP 経路
// ----------------------------------------------------------------------

/// **画面から送っても同じ。**「誰も通報しない」も HTTP で送れる。
#[sqlx::test(fixtures("app_role"), fixtures("../ops/app_privileges.sql"))]
async fn the_http_route_records_both_a_target_and_nobody(owner: PgPool) {
    let db = AppDb::connect(&owner).await;
    seed_players(&db, 10).await;
    play(&db, 1).await;
    report_as(&db, 1, 2, "nullified").await;
    report_as(&db, 1, 3, "nullified").await;
    let app = db.router();

    // 相手チームを名指しする
    let (status, body) = post_accuse(&app, 1, player(2), Some(player(8))).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["nullified_reported"], json!(true));
    // **応答に票数が入っていない**
    assert!(body.get("votes").is_none(), "{body}");

    // 「誰も通報しない」
    let (status, body) = post_accuse(&app, 1, player(3), None).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["nullified_reported"], json!(true));

    // 勝敗を報告した人は 409
    report_as(&db, 1, 4, "red").await;
    let (status, body) = post_accuse(&app, 1, player(4), Some(player(8))).await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(body["error"], json!("not_a_nullified_reporter"));

    let rows: i64 = sqlx::query_scalar("select count(*) from nullified_reports")
        .fetch_one(&db.pool)
        .await
        .unwrap();
    assert_eq!(rows, 2);
    let nobody: i64 =
        sqlx::query_scalar("select count(*) from nullified_reports where target_id is null")
            .fetch_one(&db.pool)
            .await
            .unwrap();
    assert_eq!(nobody, 1);
    println!("HTTP: 名指し 1 件 / 誰も通報しない {nobody} 件 / 拒否 1 件");

    db.done().await;
}

// ----------------------------------------------------------------------
// 運営パネル
// ----------------------------------------------------------------------

/// **票数が見えるのは運営だけ。**成立の有無と、試合が無効化されたかも分かる。
#[sqlx::test(fixtures("app_role"), fixtures("../ops/app_privileges.sql"))]
async fn the_admin_view_shows_the_votes_and_whether_they_count(owner: PgPool) {
    let db = AppDb::connect(&owner).await;
    seed_players(&db, 10).await;
    play(&db, 1).await;
    for who in 1..=6u128 {
        report_as(&db, 1, who, "nullified").await;
    }
    for reporter in 1..=5u128 {
        accuse(&db, 1, reporter, Some(10)).await.unwrap();
    }
    accuse(&db, 1, 6, None).await.unwrap();

    // まだ試合が終わっていないので、票はあってもペナルティにならない
    let tallies = db.nullified.admin_tallies(100).await.unwrap();
    assert_eq!(tallies.len(), 1);
    assert_eq!(tallies[0].votes, 5, "「誰も通報しない」を数えている");
    assert!(!tallies[0].penalised);
    assert!(!tallies[0].match_nullified);
    println!(
        "無効化前: {} 票 / 成立 {} / 無効試合 {}",
        tallies[0].votes, tallies[0].penalised, tallies[0].match_nullified
    );

    // 無効試合として終わると成立する
    finish(&db, 1, K::Nullified, "nullified").await;
    let tallies = db.nullified.admin_tallies(100).await.unwrap();
    assert!(tallies[0].penalised);
    assert!(tallies[0].match_nullified);
    println!(
        "無効化後: {} 票 / 成立 {} / 無効試合 {}",
        tallies[0].votes, tallies[0].penalised, tallies[0].match_nullified
    );

    // 履歴には「誰も通報しない」も出る
    let history = db.nullified.admin_history(100).await.unwrap();
    assert_eq!(history.len(), 6);
    assert_eq!(
        history.iter().filter(|row| row.target_id.is_none()).count(),
        1
    );
    // 相手チームへの通報が分かる（1〜5 は red、10 は blue）
    assert_eq!(
        history
            .iter()
            .filter(|row| row.cross_team == Some(true))
            .count(),
        5
    );
    println!("履歴 {} 行（うち「誰も通報しない」1 行）", history.len());

    db.done().await;
}
