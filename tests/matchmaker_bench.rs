//! 300 人が同時にキューにいるときの負荷測定。**開催前の確認用。**
//!
//! 通常のテスト実行では回さない（`--ignored` で明示的に走らせる）。
//!
//! ```text
//! cargo test --release --test matchmaker_bench -- --ignored --nocapture
//! ```
//!
//! 測るのは 2 つ:
//!
//! 1. **選抜だけ**（DB も Discord も通らない純粋な計算）
//! 2. **DB を通した 1 スキャン**（advisory lock を握っている時間そのもの）
//! - フレックス補償が `formation_players` から導出されていること（07 §5）

use std::time::{Duration, Instant};

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use axum::response::IntoResponse;
use axum_extra::extract::SignedCookieJar;
use tower::ServiceExt;
use unite_matchsystem::lifecycle::LifecycleConfig;
use unite_matchsystem::match_screen::{MatchRepository, MatchState, router as match_router};
use unite_matchsystem::nullified::NullifiedReportRepository;
use unite_matchsystem::quiet::QuietReportRepository;
use unite_matchsystem::registration::AuthConfig;
use unite_matchsystem::registration::session::{SESSION_COOKIE, Session, set_session};

use chrono::{TimeDelta, TimeZone, Utc};
use sqlx::PgPool;
use sqlx::postgres::PgPoolOptions;
use unite_matchsystem::lifecycle::{FormationEventKind as K, FormationId, PlayerId, Timestamp};
use unite_matchsystem::matchmaker::{
    Candidate, Matchmaker, MatchmakerConfig, MatchmakerRepository, ROLES, RoleWish, select_matches,
};
use unite_matchsystem::persistence::{FormationRepository, NewEvent, VcAssignment};
use unite_matchsystem::queue::QueueRepository;
use unite_matchsystem::queue::model::{QueueRequest, RolePreference};
use unite_matchsystem::rating::RatingAdjustmentRepository;
use uuid::Uuid;

const APP_ROLE: &str = "assay_app";
const APP_ROLE_PASSWORD: &str = "assay_app_test";

// ----------------------------------------------------------------------
// 道具
// ----------------------------------------------------------------------

/// テストの基準時刻。**既定の稼働枠（水金土日 21:00〜翌 2:00 JST）の内側**に置く。
///
/// 2026-09-12 13:00 UTC = 土曜 22:00 JST。枠の外に置くと、スケジュールを
/// 何も設定していないマッチメイカが「稼働時間外」で止まる。
fn at(seconds: i64) -> Timestamp {
    Utc.with_ymd_and_hms(2026, 9, 12, 13, 0, 0).unwrap() + TimeDelta::seconds(seconds)
}

fn player(n: u128) -> PlayerId {
    PlayerId(Uuid::from_u128(n))
}

/// テスト DB にアプリ用ロールで接続したプール。
struct AppDb {
    pool: PgPool,
    queue: QueueRepository,
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
            formations: FormationRepository::new(pool.clone()),
            pool,
        }
    }

    /// 本番と同じマッチ画面のルータ。報告はここを通す。
    fn match_app(&self) -> Router {
        match_router(MatchState::new(
            auth_config(),
            self.formations.clone(),
            MatchRepository::new(self.pool.clone()),
            QuietReportRepository::new(self.pool.clone()),
            NullifiedReportRepository::new(self.pool.clone()),
            LifecycleConfig::default(),
            "kyogre",
            &["大会モード".to_owned()],
        ))
    }

    fn matchmaker(&self, vc_pool_size: usize) -> Matchmaker {
        Matchmaker::new(
            MatchmakerRepository::new(self.pool.clone()),
            vc_pool(vc_pool_size),
            MatchmakerConfig::default(),
        )
    }

    async fn done(self) {
        self.pool.close().await;
    }
}

/// `vc1` … `vc<size>` の組。
fn vc_pool(size: usize) -> Vec<VcAssignment> {
    (1..=size)
        .map(|n| VcAssignment {
            label: n.to_string(),
            red_channel_id: 100 + n as u64 * 2,
            blue_channel_id: 101 + n as u64 * 2,
        })
        .collect()
}

/// `players` の行を用意する（追記専用テーブルの FK 先）。
async fn seed_players(db: &AppDb, count: u128) {
    for n in 1..=count {
        sqlx::query("insert into players (id, discord_user_id, created_at) values ($1, $2, now()) on conflict do nothing")
            .bind(player(n).0)
            .bind(1000 + n as i64)
            .execute(&db.pool)
            .await
            .expect("seed a player");
    }
}

/// 表示スケールのレートを `rating_adjustments` への追記で作る。
async fn adjust_rating(db: &AppDb, who: PlayerId, delta_mu: f64) {
    adjust_rating_with_key(db, who, delta_mu, "seed").await;
}

/// 同じプレイヤーに 2 回以上追記する用（冪等キーは追記ごとに変える）。
async fn adjust_rating_with_key(db: &AppDb, who: PlayerId, delta_mu: f64, key: &str) {
    sqlx::query(
        "insert into rating_adjustments \
         (player_id, delta_mu, delta_sigma, reason, created_by, created_at, idempotency_key) \
         values ($1, $2, 0, 'test seed', 'staff:test', now(), $3)",
    )
    .bind(who.0)
    .bind(delta_mu)
    .bind(format!("{key}-{}", who.0))
    .execute(&db.pool)
    .await
    .expect("append a rating adjustment");
}

/// ロールがちょうど 2 人ずつ埋まるように `count` 人をキューに入れる。
async fn enqueue_full_rosters(db: &AppDb, count: u128, enqueued_at: Timestamp) {
    for n in 1..=count {
        let role = ROLES[(n as usize - 1) % ROLES.len()];
        db.queue
            .enqueue(
                player(n),
                &QueueRequest {
                    roles: vec![RolePreference {
                        role: role.to_owned(),
                        priority: 1,
                    }],
                    streaming: false,
                },
                enqueued_at,
            )
            .await
            .expect("enqueue");
    }
}

async fn rows_in(db: &AppDb, table: &str) -> i64 {
    let sql = format!("select count(*) from {table}");
    sqlx::query_scalar(sqlx::AssertSqlSafe(sql))
        .fetch_one(&db.pool)
        .await
        .expect("count rows")
}

// ----------------------------------------------------------------------
// 300 人が同時にキューにいるときのスキャン
// ----------------------------------------------------------------------
//
// 選抜はアンカーごとに**最小費用流**を解く。ウィンドウが大きいほど 1 回が重く、
// アンカーは最大で候補数だけ回る。300 人でどうなるかを実測する。
//
// **絶対値を見る。**スキャンは advisory lock を取った 1 トランザクションなので、
// ここが長いとその間ほかの運営操作（レート増減・範囲変更・裁定）が待たされる。

/// 候補を作る。`spread` はレートの散らばり（0 なら全員同じレート）。
fn bench_candidates(count: usize, spread: f64, roles_each: usize) -> Vec<Candidate> {
    (0..count)
        .map(|index| {
            // レートは決定的にばらす（同じ入力なら毎回同じ結果になる）
            let offset = if spread == 0.0 {
                0.0
            } else {
                ((index * 37) % 100) as f64 / 100.0 * spread
            };
            let roles = (0..roles_each)
                .map(|slot| RoleWish {
                    role: ROLES[(index + slot) % ROLES.len()].to_owned(),
                    priority: slot as i16 + 1,
                })
                .collect();
            Candidate {
                player_id: PlayerId(Uuid::from_u128(index as u128 + 1)),
                rating: 1500.0 + offset,
                sigma: 500.0,
                down_range: 50,
                enqueued_at: at(-600 - (index as i64 % 300)),
                roles,
                flex_compensation: false,
                unmatched_scans: 0,
            }
        })
        .collect()
}

/// **選抜だけ**を測る（DB も Discord も通らない）。
#[test]
#[ignore = "ベンチ。--ignored で明示的に走らせる"]
fn bench_selection_at_three_hundred() {
    println!("== 選抜だけ（DB なし） ==");
    for (label, spread, roles_each) in [
        // 実際に近い形: レートが 400 点に散らばり、希望は 2 つ
        ("散らばり 400 / 希望2", 400.0, 2),
        // 全員が同じレート帯 = **ウィンドウが常に 300 人**（最小費用流が最大）
        ("全員同レート / 希望2", 0.0, 2),
        // 希望が 1 つだけ = ロールが埋まらずアンカーを回し続ける
        ("全員同レート / 希望1", 0.0, 1),
        // 幅 50 に収まらないほど散らばる = ウィンドウが小さく、組めない
        ("散らばり 2000 / 希望2", 2000.0, 2),
    ] {
        let candidates = bench_candidates(300, spread, roles_each);
        let started = Instant::now();
        let matches = select_matches(&candidates, at(0));
        let elapsed = started.elapsed();
        println!(
            "{label}: {} 組 / {:?}（1 組あたり {:?}）",
            matches.len(),
            elapsed,
            elapsed.checked_div(matches.len().max(1) as u32).unwrap(),
        );
    }
}

/// **DB を通した 1 スキャン**を測る。300 人を投入して `run_once` を 1 回。
#[sqlx::test(
    fixtures("reference", "app_role"),
    fixtures("../ops/app_privileges.sql")
)]
#[ignore = "ベンチ。--ignored で明示的に走らせる"]
async fn bench_one_scan_with_three_hundred_queued(owner: PgPool) {
    let db = AppDb::connect(&owner).await;
    let count = 300u128;

    let seeding = Instant::now();
    seed_players(&db, count).await;
    // レートをばらす（1400〜1700 くらい）
    for n in 1..=count {
        adjust_rating(&db, player(n), ((n % 30) as f64) * 10.0 - 100.0).await;
    }
    enqueue_full_rosters(&db, count, at(-600)).await;
    println!("== 300 人の投入: {:?} ==", seeding.elapsed());
    assert_eq!(count as i64, rows_in(&db, "queue_entries").await);

    // VC は本番と同じ 40 組
    let matchmaker = db.matchmaker(40);
    let started = Instant::now();
    let outcome = matchmaker.run_once(at(0)).await.unwrap();
    let elapsed = started.elapsed();

    println!(
        "1 スキャン: {:?} / 候補 {} 人 → {} 組成立 / 残り {} 人 / VC 枯渇 {}",
        elapsed,
        outcome.candidates,
        outcome.created.len(),
        outcome.unmatched,
        outcome.vc_exhausted,
    );
    println!(
        "キューに残った人数: {} / 作られた formation: {}",
        rows_in(&db, "queue_entries").await,
        rows_in(&db, "formations").await,
    );

    // **advisory lock を握っている時間**がこの値。ここが長いと運営操作が待つ
    assert!(
        elapsed < Duration::from_secs(60),
        "1 スキャンに 60 秒以上かかっている: {elapsed:?}"
    );

    db.done().await;
}

/// **VC の数が足りているか。**300 人 = 30 組で、本番のプールは 40 組。
#[sqlx::test(
    fixtures("reference", "app_role"),
    fixtures("../ops/app_privileges.sql")
)]
#[ignore = "ベンチ。--ignored で明示的に走らせる"]
async fn bench_vc_pool_headroom_at_three_hundred(owner: PgPool) {
    let db = AppDb::connect(&owner).await;
    let count = 300u128;
    seed_players(&db, count).await;
    enqueue_full_rosters(&db, count, at(-600)).await;

    let outcome = db.matchmaker(40).run_once(at(0)).await.unwrap();
    println!(
        "== VC 40 組 / 300 人 ==\n成立: {} 組 / VC 枯渇: {} / 残り: {} 人",
        outcome.created.len(),
        outcome.vc_exhausted,
        outcome.unmatched,
    );
    assert!(!outcome.vc_exhausted, "40 組で足りていない");

    db.done().await;
}

// ----------------------------------------------------------------------
// 成立から報告・確定・レート反映まで
// ----------------------------------------------------------------------
//
// スキャンだけ速くても、そのあと 300 人が一斉に報告してくる。
// **そこまで通して測る。**
/// 本番と同じ HTTP 経路で報告する（追記 → 評価 → 確定まで通る）。
async fn post_report(
    app: &Router,
    formation: FormationId,
    who: PlayerId,
    outcome: &str,
) -> StatusCode {
    let request = Request::post(format!("/match/{}/report", formation.0))
        .header(header::COOKIE, session_cookie(who))
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(
            serde_json::json!({ "outcome": outcome }).to_string(),
        ))
        .unwrap();
    app.clone().oneshot(request).await.unwrap().status()
}

fn session_cookie(player_id: PlayerId) -> String {
    let config = auth_config();
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

fn auth_config() -> AuthConfig {
    AuthConfig::new(
        "client-id",
        "client-secret",
        "https://match.example.test/auth/callback",
        "match.example.test",
        "k".repeat(64),
        std::time::Duration::from_secs(3600),
    )
    .expect("valid config")
}

/// 1 組ぶんを in_progress まで進める（ホスト打診 → 承諾 → ルーム ID）。
async fn start_match(db: &AppDb, formation: FormationId, host: PlayerId, seq: usize) {
    for event in [
        NewEvent::new(K::HostAssigned, Some(host), at(1), format!("assign-{seq}")),
        NewEvent::new(K::HostAccepted, Some(host), at(2), format!("accept-{seq}")),
        NewEvent::code_posted(host, "1234", at(3), format!("code-{seq}")),
    ] {
        db.formations.append_event(formation, &event).await.unwrap();
    }
}

/// **300 人 → 30 組 → 300 報告 → 確定 → レート反映**を通して測る。
#[sqlx::test(
    fixtures("reference", "app_role"),
    fixtures("../ops/app_privileges.sql")
)]
#[ignore = "ベンチ。--ignored で明示的に走らせる"]
async fn bench_three_hundred_from_queue_to_confirmed(owner: PgPool) {
    let db = AppDb::connect(&owner).await;
    let ratings = RatingAdjustmentRepository::new(db.pool.clone());
    let count = 300u128;

    let seeding = Instant::now();
    seed_players(&db, count).await;
    for n in 1..=count {
        adjust_rating(&db, player(n), ((n % 30) as f64) * 10.0 - 100.0).await;
    }
    enqueue_full_rosters(&db, count, at(-600)).await;
    println!("== 投入 300 人: {:?} ==", seeding.elapsed());

    // 1. スキャン（advisory lock を握っている時間）
    let scan = Instant::now();
    let outcome = db.matchmaker(40).run_once(at(0)).await.unwrap();
    let scan_elapsed = scan.elapsed();
    println!(
        "1. スキャン: {scan_elapsed:?} / {} 組成立 / 残り {} 人 / VC 枯渇 {}",
        outcome.created.len(),
        outcome.unmatched,
        outcome.vc_exhausted,
    );
    assert!(!outcome.created.is_empty(), "1 組も成立していない");

    // 2. 全組を in_progress まで進める
    let starting = Instant::now();
    for (seq, formation) in outcome.created.iter().enumerate() {
        let roster = db.formations.load_roster(*formation).await.unwrap();
        let host = roster.first().expect("a roster").player_id;
        start_match(&db, *formation, host, seq).await;
    }
    println!(
        "2. ホスト決定 → ルーム ID（{} 組）: {:?}",
        outcome.created.len(),
        starting.elapsed()
    );

    // 3. **全員が一斉に報告する。**本番と同じ HTTP 経路（追記 → 評価 → 確定）
    let app = db.match_app();
    let reporting = Instant::now();
    let mut reports = 0usize;
    let mut latencies = Vec::new();
    for formation in &outcome.created {
        let roster = db.formations.load_roster(*formation).await.unwrap();
        for entry in &roster {
            let one = Instant::now();
            let status = post_report(&app, *formation, entry.player_id, "red").await;
            latencies.push(one.elapsed());
            assert_eq!(status, StatusCode::OK, "報告が通らない");
            reports += 1;
        }
    }
    let reporting_elapsed = reporting.elapsed();
    latencies.sort();
    println!(
        "3. 報告 {reports} 件（HTTP 経路）: {reporting_elapsed:?}\n   \
         1 件あたり平均 {:?} / 中央値 {:?} / p95 {:?} / 最遅 {:?}",
        reporting_elapsed / reports as u32,
        latencies[latencies.len() / 2],
        latencies[latencies.len() * 95 / 100],
        latencies.last().unwrap(),
    );
    // 4. 確定していること
    let confirmed = rows_in(&db, "formation_events where kind = 'confirmed'").await;
    println!("4. 確定した試合: {confirmed} / {}", outcome.created.len());
    assert_eq!(
        confirmed as usize,
        outcome.created.len(),
        "確定していない組がある"
    );

    // 5. **レートの導出**（1 人ぶんと、ランキング全体）
    let one = Instant::now();
    let rating = ratings.display_rating(player(1)).await.unwrap();
    let one_elapsed = one.elapsed();
    let all = Instant::now();
    let rows: i64 = sqlx::query_scalar("select count(*) from player_current_ratings")
        .fetch_one(&db.pool)
        .await
        .unwrap();
    let all_elapsed = all.elapsed();
    println!("5. レート導出: 1 人 {one_elapsed:?}（{rating}） / 全員 {rows} 行 {all_elapsed:?}");

    // 6. キューが空いていること（成立した人は外れる）
    let left = rows_in(&db, "queue_entries").await;
    println!(
        "6. キューに残った人: {left} 人（{} 人が試合へ）",
        count as i64 - left
    );

    println!(
        "== 合計（投入を除く）: {:?} ==",
        scan_elapsed + starting.elapsed() + reporting_elapsed
    );

    db.done().await;
}

/// **報告が割れて係争になる場合**も測る。確定しない経路が詰まらないこと。
#[sqlx::test(
    fixtures("reference", "app_role"),
    fixtures("../ops/app_privileges.sql")
)]
#[ignore = "ベンチ。--ignored で明示的に走らせる"]
async fn bench_disputed_reports_at_scale(owner: PgPool) {
    let db = AppDb::connect(&owner).await;
    let count = 300u128;
    seed_players(&db, count).await;
    enqueue_full_rosters(&db, count, at(-600)).await;
    let outcome = db.matchmaker(40).run_once(at(0)).await.unwrap();

    for (seq, formation) in outcome.created.iter().enumerate() {
        let roster = db.formations.load_roster(*formation).await.unwrap();
        let host = roster.first().expect("a roster").player_id;
        start_match(&db, *formation, host, seq).await;
    }

    // **半々に割れる報告。**過半数（7）に届かないので確定しない
    let app = db.match_app();
    let started = Instant::now();
    for formation in &outcome.created {
        let roster = db.formations.load_roster(*formation).await.unwrap();
        for (index, entry) in roster.iter().enumerate() {
            let side = if index % 2 == 0 { "red" } else { "blue" };
            let status = post_report(&app, *formation, entry.player_id, side).await;
            assert_eq!(status, StatusCode::OK);
        }
    }
    println!(
        "割れた報告 {} 件（HTTP 経路）: {:?}",
        outcome.created.len() * 10,
        started.elapsed()
    );
    let disputed = rows_in(&db, "formation_events where kind = 'disputed'").await;
    println!("係争になった試合: {disputed} / {}", outcome.created.len());

    db.done().await;
}

/// **500 人ちょうど。**VC プール 50 組 = 500 人ぶんで、ぴったり埋まる。
#[sqlx::test(
    fixtures("reference", "app_role"),
    fixtures("../ops/app_privileges.sql")
)]
#[ignore = "ベンチ。--ignored で明示的に走らせる"]
async fn bench_five_hundred_fills_the_pool_exactly(owner: PgPool) {
    let db = AppDb::connect(&owner).await;
    seed_players(&db, 500).await;
    enqueue_full_rosters(&db, 500, at(-600)).await;

    let started = Instant::now();
    let outcome = db.matchmaker(50).run_once(at(0)).await.unwrap();
    println!(
        "500 人: {:?} / {} 組成立 / 残り {} 人 / VC 枯渇 {}",
        started.elapsed(),
        outcome.created.len(),
        outcome.unmatched,
        outcome.vc_exhausted,
    );
    println!(
        "   キューに残った人数: {}",
        rows_in(&db, "queue_entries").await
    );
    assert_eq!(outcome.created.len(), 50);
    db.done().await;
}

/// **600 人。**VC が 50 組しかないので、50 組だけ組んで 100 人はキューに残る。
///
/// **組めるだけ組んで残りは次回に回す**こと（0 組になったり、キューが壊れたりしない）。
#[sqlx::test(
    fixtures("reference", "app_role"),
    fixtures("../ops/app_privileges.sql")
)]
#[ignore = "ベンチ。--ignored で明示的に走らせる"]
async fn bench_six_hundred_forms_what_the_pool_allows(owner: PgPool) {
    let db = AppDb::connect(&owner).await;
    seed_players(&db, 600).await;
    enqueue_full_rosters(&db, 600, at(-600)).await;

    let started = Instant::now();
    let outcome = db.matchmaker(50).run_once(at(0)).await.unwrap();
    let left = rows_in(&db, "queue_entries").await;
    println!(
        "600 人: {:?} / {} 組成立 / VC 枯渇 {} / キューに残った {} 人",
        started.elapsed(),
        outcome.created.len(),
        outcome.vc_exhausted,
        left,
    );
    assert_eq!(outcome.created.len(), 50, "組めるだけ組んでいない");
    assert!(outcome.vc_exhausted, "枯渇が報告されていない");
    assert_eq!(left, 100, "残りが次回に回っていない");

    // **2 回目のスキャンでは 1 組も増えない**（VC が空いていないので）
    let second = db.matchmaker(50).run_once(at(60)).await.unwrap();
    println!(
        "   2 回目: {} 組成立 / VC 枯渇 {} / キュー {} 人",
        second.created.len(),
        second.vc_exhausted,
        rows_in(&db, "queue_entries").await
    );
    assert_eq!(second.created.len(), 0);
    db.done().await;
}

/// **選抜だけ**を 500 / 1000 人で。CPU がどこまで持つか。
#[test]
#[ignore = "ベンチ。--ignored で明示的に走らせる"]
fn bench_selection_beyond_three_hundred() {
    for count in [300usize, 500, 800, 1000] {
        // 全員同レート = ウィンドウ最大（最小費用流がいちばん重い）
        let candidates = bench_candidates(count, 0.0, 2);
        let started = Instant::now();
        let matches = select_matches(&candidates, at(0));
        println!(
            "{count} 人（全員同レート）: {} 組 / {:?}",
            matches.len(),
            started.elapsed()
        );
    }
}
