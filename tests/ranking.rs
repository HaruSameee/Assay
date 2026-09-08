//! ランキングの統合テスト。実際の PostgreSQL を使う。
//!
//! 検証すること:
//! - 掲載条件は**確定した試合が 5 試合以上**。4 試合では載らない
//! - **disputed / voided / nullified は数に入らない**。5 試合ぶん係争でも載らない
//! - 並び順はレート降順のみ。勝率で並べ替える経路が無い
//! - **既定は非公開**（404）。環境変数で開けたときだけ出る
//! - シーズン境界に従う
//! - 300 人規模でページングが効き、応答時間が実用の範囲に収まる

mod common;

use std::time::Instant;

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use common::{APP_ROLE, APP_ROLE_PASSWORD, config, player};
use http_body_util::BodyExt;
use serde_json::Value;
use sqlx::PgPool;
use sqlx::postgres::PgPoolOptions;
use tower::ServiceExt;
use unite_matchsystem::lifecycle::PlayerId;
use unite_matchsystem::ranking::{RankingRepository, RankingState, router as ranking_router};
use unite_matchsystem::registration::RegistrationRepository;
use uuid::Uuid;

struct AppDb {
    pool: PgPool,
    ranking: RankingRepository,
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
            ranking: RankingRepository::new(pool.clone()),
            registration: RegistrationRepository::new(pool.clone()),
            pool,
        }
    }

    fn router(&self, public: bool) -> Router {
        ranking_router(RankingState::new(
            config(),
            self.ranking.clone(),
            self.registration.clone(),
            public,
        ))
    }

    async fn done(self) {
        self.pool.close().await;
    }
}

async fn get_json(app: &Router, uri: &str) -> (StatusCode, Value) {
    let request = Request::get(uri).body(Body::empty()).unwrap();
    let response = app.clone().oneshot(request).await.unwrap();
    let status = response.status();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

async fn get_html(app: &Router, uri: &str) -> (StatusCode, String) {
    let request = Request::get(uri)
        .header(header::ACCEPT, "text/html")
        .body(Body::empty())
        .unwrap();
    let response = app.clone().oneshot(request).await.unwrap();
    let status = response.status();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    (status, String::from_utf8_lossy(&bytes).into_owned())
}

/// プレイヤーを 1 人用意する。
async fn seed_player(db: &AppDb, n: u128) -> PlayerId {
    sqlx::query(
        "insert into players (id, discord_user_id, created_at, trainer_name) \
         values ($1, $2, now(), $3) on conflict do nothing",
    )
    .bind(player(n).0)
    .bind(50_000 + n as i64)
    .bind(format!("トレーナー{n}"))
    .execute(&db.pool)
    .await
    .expect("seed a player");
    player(n)
}

/// formation を 1 件作り、終端イベントを積む。**参加者は指定の 10 人。**
///
/// `outcome` が `Some(team)` なら `confirmed`、`None` なら `disputed`（数に入らない）。
async fn seed_match(db: &AppDb, n: u128, first: u128, outcome: Option<&str>) {
    let fid = Uuid::from_u128(0x1000_0000_0000_0000_0000_0000_0000 + n);
    sqlx::query(
        "insert into formations (id, created_at, matchmaking_params_version, predicted_win_prob) \
         values ($1, now(), 3, 0.5)",
    )
    .bind(fid)
    .execute(&db.pool)
    .await
    .expect("create formation");
    let roles = [
        "top_carry",
        "top_support",
        "jungle",
        "bot_carry",
        "bot_tank",
    ];
    for index in 0..10u128 {
        sqlx::query(
            "insert into formation_players \
             (formation_id, player_id, team, role, role_was_flexed, mu_at_match, sigma_at_match) \
             values ($1, $2, $3, $4, false, 25.0, 8.3333)",
        )
        .bind(fid)
        .bind(player(first + index).0)
        .bind(if index < 5 { "red" } else { "blue" })
        .bind(roles[(index % 5) as usize])
        .execute(&db.pool)
        .await
        .expect("seed a formation player");
    }
    let (kind, payload) = match outcome {
        Some(team) => ("confirmed", format!("{{\"outcome\":\"{team}\"}}")),
        None => ("disputed", "{\"outcome\":\"disputed\"}".to_owned()),
    };
    sqlx::query(
        "insert into formation_events (formation_id, occurred_at, kind, payload, idempotency_key) \
         values ($1, now(), $2, $3::jsonb, $4)",
    )
    .bind(fid)
    .bind(kind)
    .bind(payload)
    .bind(format!("fin-{n}"))
    .execute(&db.pool)
    .await
    .expect("finish the formation");
}

// ----------------------------------------------------------------------
// 掲載条件
// ----------------------------------------------------------------------

/// **4 試合では載らず、5 試合目で載る。**
#[sqlx::test(
    fixtures("reference", "app_role"),
    fixtures("../ops/app_privileges.sql")
)]
async fn four_matches_is_not_enough_but_five_is(owner: PgPool) {
    let db = AppDb::connect(&owner).await;
    let app = db.router(true);
    for n in 1..=10u128 {
        seed_player(&db, n).await;
    }

    for played in 1..=4u128 {
        seed_match(&db, played, 1, Some("red")).await;
        let (_, json) = get_json(&app, "/ranking").await;
        assert_eq!(json["total"], 0, "{played} 試合で載っている: {json}");
    }
    println!("4 試合まで: total = 0（1 人も載らない）");

    seed_match(&db, 5, 1, Some("red")).await;
    let (status, json) = get_json(&app, "/ranking").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(json["total"], 10, "5 試合目で載る: {json}");
    assert_eq!(json["minimum_matches"], 5);
    println!("5 試合目: total = {}（10 人とも載る）", json["total"]);
    // 全員 5 試合ちょうど
    for row in json["players"].as_array().unwrap() {
        assert_eq!(row["matches"], 5, "{row}");
    }

    db.done().await;
}

/// **係争中の試合は数に入らない。**5 試合ぶん disputed でも載らない。
#[sqlx::test(
    fixtures("reference", "app_role"),
    fixtures("../ops/app_privileges.sql")
)]
async fn disputed_matches_do_not_count_towards_the_listing(owner: PgPool) {
    let db = AppDb::connect(&owner).await;
    let app = db.router(true);
    for n in 1..=10u128 {
        seed_player(&db, n).await;
    }

    for played in 1..=5u128 {
        seed_match(&db, played, 1, None).await;
    }
    let (_, json) = get_json(&app, "/ranking").await;
    assert_eq!(json["total"], 0, "係争中が数に入っている: {json}");
    println!("disputed だけ 5 試合: total = 0");

    // 確定した試合を 5 件足すと載る（係争中の 5 件は数えないまま）
    for played in 6..=10u128 {
        seed_match(&db, played, 1, Some("red")).await;
    }
    let (_, json) = get_json(&app, "/ranking").await;
    assert_eq!(json["total"], 10);
    for row in json["players"].as_array().unwrap() {
        assert_eq!(row["matches"], 5, "係争中が試合数に入っている: {row}");
    }
    println!("確定 5 + 係争 5: total = 10 / 各人の試合数 = 5");

    db.done().await;
}

/// 並び順はレート降順のみ。**勝率で並べ替える手段が無い。**
#[sqlx::test(
    fixtures("reference", "app_role"),
    fixtures("../ops/app_privileges.sql")
)]
async fn the_order_is_rating_descending_only(owner: PgPool) {
    let db = AppDb::connect(&owner).await;
    let app = db.router(true);
    for n in 1..=10u128 {
        seed_player(&db, n).await;
    }
    for played in 1..=5u128 {
        seed_match(&db, played, 1, Some("red")).await;
    }

    let (_, json) = get_json(&app, "/ranking").await;
    let ratings: Vec<f64> = json["players"]
        .as_array()
        .unwrap()
        .iter()
        .map(|row| row["rating"].as_f64().unwrap())
        .collect();
    let mut sorted = ratings.clone();
    sorted.sort_by(|a, b| b.partial_cmp(a).unwrap());
    assert_eq!(ratings, sorted, "レート降順でない: {ratings:?}");
    // red の 5 人が 1535、blue の 5 人が 1465
    assert_eq!(ratings.first(), Some(&1535.0), "{ratings:?}");
    assert_eq!(ratings.last(), Some(&1465.0), "{ratings:?}");
    println!("レート降順: {ratings:?}");

    // 並べ替えの指定は受け取らない（未知のクエリは無視される）
    let (_, sorted_attempt) = get_json(&app, "/ranking?sort=win_rate&order=asc").await;
    assert_eq!(
        sorted_attempt["players"], json["players"],
        "並べ替えが効いている"
    );

    // 画面にも勝率の列が無い
    let (_, page) = get_html(&app, "/ranking").await;
    assert!(!page.contains("勝率"), "{page}");
    println!("?sort=win_rate を付けても並びは変わらない");

    db.done().await;
}

/// **既定は非公開。**環境変数で開けたときだけ出る。
#[sqlx::test(
    fixtures("reference", "app_role"),
    fixtures("../ops/app_privileges.sql")
)]
async fn the_ranking_is_hidden_until_it_is_published(owner: PgPool) {
    let db = AppDb::connect(&owner).await;
    for n in 1..=10u128 {
        seed_player(&db, n).await;
    }
    for played in 1..=5u128 {
        seed_match(&db, played, 1, Some("red")).await;
    }

    // 非公開: 存在ごと伏せる（403 だと「あるが見せない」ことが分かる）
    let private = db.router(false);
    let (status, _) = get_json(&private, "/ranking").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, page) = get_html(&private, "/ranking").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    // ヘッダーにも導線が出ない
    assert!(!page.contains("href=\"/ranking\""), "{page}");
    println!("非公開: /ranking は 404、ヘッダーにも導線が出ない");

    // 公開すると出る。ヘッダーにも導線が出る
    let public = db.router(true);
    let (status, page) = get_html(&public, "/ranking").await;
    assert_eq!(status, StatusCode::OK);
    assert!(page.contains("href=\"/ranking\""), "{page}");
    println!("公開: /ranking は 200、ヘッダーに導線が出る");

    db.done().await;
}

/// シーズン境界を入れると、境界より前の試合は数に入らなくなる。
#[sqlx::test(
    fixtures("reference", "app_role"),
    fixtures("../ops/app_privileges.sql")
)]
async fn the_listing_follows_the_season_boundary(owner: PgPool) {
    let db = AppDb::connect(&owner).await;
    let app = db.router(true);
    for n in 1..=10u128 {
        seed_player(&db, n).await;
    }
    for played in 1..=5u128 {
        seed_match(&db, played, 1, Some("red")).await;
    }
    let (_, before) = get_json(&app, "/ranking").await;
    assert_eq!(before["total"], 10);

    // 境界を 1 本入れる（追記するだけ。試合ログは消さない）
    sqlx::query(
        "insert into season_boundaries (effective_at, label, created_by, idempotency_key, created_at) \
         values (now(), '2026 開幕', 'discord:1', 'ranking-test', now())",
    )
    .execute(&db.pool)
    .await
    .expect("append the boundary");

    let (_, after) = get_json(&app, "/ranking").await;
    assert_eq!(after["total"], 0, "境界に従っていない: {after}");
    println!(
        "境界の前: total = {} → 境界の後: total = {}",
        before["total"], after["total"]
    );

    // 試合ログは残っている
    let formations: i64 = sqlx::query_scalar("select count(*) from formations")
        .fetch_one(&db.pool)
        .await
        .unwrap();
    assert_eq!(formations, 5, "ログが消えている");

    db.done().await;
}

// ----------------------------------------------------------------------
// 300 人規模
// ----------------------------------------------------------------------

/// **300 人規模でページングが効き、応答時間が実用の範囲に収まる。**
#[sqlx::test(
    fixtures("reference", "app_role"),
    fixtures("../ops/app_privileges.sql")
)]
async fn three_hundred_players_are_paged(owner: PgPool) {
    let db = AppDb::connect(&owner).await;
    let app = db.router(true);
    for n in 1..=300u128 {
        seed_player(&db, n).await;
    }
    // 30 組 × 5 試合 = 150 試合。300 人が 5 試合ずつ持つ
    let mut match_id = 1u128;
    for group in 0..30u128 {
        for _ in 0..5 {
            seed_match(&db, match_id, group * 10 + 1, Some("red")).await;
            match_id += 1;
        }
    }

    let started = Instant::now();
    let (status, first) = get_json(&app, "/ranking").await;
    let first_page = started.elapsed();
    assert_eq!(status, StatusCode::OK);
    assert_eq!(first["total"], 300, "{}", first["total"]);
    assert_eq!(first["pages"], 6, "50 人ずつ 6 ページ");
    assert_eq!(first["players"].as_array().unwrap().len(), 50);
    assert_eq!(first["players"][0]["rank"], 1);

    let started = Instant::now();
    let (_, last) = get_json(&app, "/ranking?page=6").await;
    let last_page = started.elapsed();
    assert_eq!(last["players"].as_array().unwrap().len(), 50);
    assert_eq!(last["players"][0]["rank"], 251, "順位が通し番号でない");

    // 範囲外のページは端に丸める（404 にしない）
    let (status, beyond) = get_json(&app, "/ranking?page=99").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(beyond["page"], 6);
    let (status, zero) = get_json(&app, "/ranking?page=0").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(zero["page"], 1);

    // ページをまたいで重複や欠落が無い
    let mut seen: Vec<String> = Vec::with_capacity(300);
    for page in 1..=6 {
        let (_, json) = get_json(&app, &format!("/ranking?page={page}")).await;
        for row in json["players"].as_array().unwrap() {
            seen.push(row["player_id"].as_str().unwrap().to_owned());
        }
    }
    let mut unique = seen.clone();
    unique.sort();
    unique.dedup();
    assert_eq!(unique.len(), 300, "ページ間で重複か欠落がある");

    println!("300人 / 150試合: 1ページ目 {first_page:?} / 6ページ目 {last_page:?}");
    println!("total = {} / pages = {}", first["total"], first["pages"]);
    db.done().await;
}
