//! `GET /queue` の負荷測定。**ロール別集計の追加前後を比べるためのもの。**
//!
//! 通常のテスト実行では回さない（`--ignored` で明示的に走らせる）。
//!
//! ```text
//! cargo test --release --test queue_bench -- --ignored --nocapture
//! ```
//!
//! 300 人をキューに入れ、`GET /queue` を 30 req/s で 5 分かける。
//! 環境変数で短くできる（`QUEUE_BENCH_SECONDS` / `QUEUE_BENCH_RPS` / `QUEUE_BENCH_PLAYERS`）。
//!
//! **HTTP のパースとネットワークは通らない**（`tower` のサービス呼び出し）。変わったのは
//! ハンドラとクエリなので、そこを測る。絶対値ではなく**前後の差**を見るためのもの。

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use axum::response::IntoResponse;
use axum_extra::extract::SignedCookieJar;
use sqlx::PgPool;
use sqlx::postgres::PgPoolOptions;
use tower::ServiceExt;
use unite_matchsystem::lifecycle::PlayerId;
use unite_matchsystem::queue::{QueueRepository, QueueState, router};
use unite_matchsystem::registration::session::{SESSION_COOKIE, Session, set_session};
use unite_matchsystem::registration::{AuthConfig, RegistrationRepository};
use uuid::Uuid;

const APP_ROLE: &str = "assay_app";
const APP_ROLE_PASSWORD: &str = "assay_app_test";

fn config() -> AuthConfig {
    AuthConfig::new(
        "client-id",
        "client-secret",
        "https://match.example.test/auth/callback",
        "match.example.test",
        "k".repeat(64),
        Duration::from_secs(3600),
    )
    .expect("valid test config")
}

fn player(n: u128) -> PlayerId {
    PlayerId(Uuid::from_u128(n))
}

fn session_cookie(player_id: PlayerId) -> String {
    let config = config();
    set_session(
        SignedCookieJar::new(config.cookie_key()),
        &Session::issue(player_id),
        &config,
    )
    .into_response()
    .headers()
    .get_all(header::SET_COOKIE)
    .iter()
    .filter_map(|value| value.to_str().ok())
    .find(|raw| raw.starts_with(SESSION_COOKIE))
    .and_then(|raw| raw.split(';').next())
    .expect("a signed session cookie")
    .to_owned()
}

fn env_usize(name: &str, fallback: usize) -> usize {
    std::env::var(name)
        .ok()
        .and_then(|raw| raw.parse().ok())
        .unwrap_or(fallback)
}

/// 300 人ぶんの登録とキュー投入を、まとめて 1 トランザクションで入れる。
///
/// レートは 1400〜1600 に散らす（全員が同じレートだと範囲内が常に全員になり、
/// 現実より重い側にも軽い側にも寄ってしまう）。第1希望は 5 ロールを順に割り当てる。
async fn seed_queue(pool: &PgPool, players: usize) {
    let roles = [
        "top_carry",
        "top_support",
        "jungle",
        "bot_carry",
        "bot_tank",
    ];
    for n in 1..=players {
        let id = player(n as u128).0;
        sqlx::query(
            "insert into players (id, discord_user_id, created_at, trainer_name, status, registered_at) \
             values ($1, $2, now(), $3, 'active', now()) on conflict do nothing",
        )
        .bind(id)
        .bind(100_000 + n as i64)
        .bind(format!("トレーナー{n}"))
        .execute(pool)
        .await
        .expect("seed a player");
        sqlx::query(
            "insert into player_declarations (player_id, declared_at, rank, rating, raw) \
             values ($1, now(), 'legend', 1500, '{\"rank\":\"legend\"}'::jsonb)",
        )
        .bind(id)
        .execute(pool)
        .await
        .expect("seed a declaration");
        // レートは導出値なので、追記で散らす（-100..=+100 を 201 段階で回す）
        let delta = (n % 201) as f64 - 100.0;
        if delta != 0.0 {
            sqlx::query(
                "insert into rating_adjustments \
                 (player_id, delta_mu, delta_sigma, reason, created_by, created_at, idempotency_key) \
                 values ($1, $2, 0, 'bench seed', 'staff:bench', now(), $3)",
            )
            .bind(id)
            .bind(delta)
            .bind(format!("bench-{n}"))
            .execute(pool)
            .await
            .expect("seed an adjustment");
        }
        let entry = Uuid::new_v4();
        sqlx::query(
            "insert into queue_entries (id, player_id, enqueued_at, updated_at, down_range) \
             values ($1, $2, now(), now(), 50)",
        )
        .bind(entry)
        .bind(id)
        .execute(pool)
        .await
        .expect("seed a queue entry");
        // 第1希望だけでなく第2希望も入れる（本番と同じ形。集計は第1希望だけを数える）
        for (priority, role) in [(1i16, roles[n % 5]), (2, roles[(n + 1) % 5])] {
            sqlx::query(
                "insert into queue_entry_roles (entry_id, role, priority) values ($1, $2, $3)",
            )
            .bind(entry)
            .bind(role)
            .bind(priority)
            .execute(pool)
            .await
            .expect("seed a role preference");
        }
    }
}

/// パーセンタイル（ミリ秒）。
fn percentile(sorted: &[Duration], fraction: f64) -> f64 {
    if sorted.is_empty() {
        return 0.0;
    }
    let index = ((sorted.len() as f64 - 1.0) * fraction).round() as usize;
    sorted[index].as_secs_f64() * 1000.0
}

/// **必須の実測。**300 人をキューに入れ、`GET /queue` を 30 req/s で 5 分かける。
#[sqlx::test(fixtures("app_role"), fixtures("../ops/app_privileges.sql"))]
#[ignore = "負荷測定。--ignored で明示的に走らせる"]
async fn queue_status_under_thirty_requests_per_second(owner: PgPool) {
    let options = (*owner.connect_options())
        .clone()
        .username(APP_ROLE)
        .password(APP_ROLE_PASSWORD);
    let pool = PgPoolOptions::new()
        .max_connections(16)
        .connect_with(options)
        .await
        .expect("connect as the app role");

    let players = env_usize("QUEUE_BENCH_PLAYERS", 300);
    let rps = env_usize("QUEUE_BENCH_RPS", 30);
    let seconds = env_usize("QUEUE_BENCH_SECONDS", 300);

    seed_queue(&pool, players).await;
    let app: Router = router(QueueState::new(
        config(),
        RegistrationRepository::new(pool.clone()),
        QueueRepository::new(pool.clone()),
    ));

    // 温める（接続の確立とプランのキャッシュを測定から外す）
    for n in 1..=20usize {
        let request = Request::get("/queue")
            .header(header::COOKIE, session_cookie(player(n as u128)))
            .body(Body::empty())
            .unwrap();
        let response = app.clone().oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
    }

    let total = rps * seconds;
    let interval = Duration::from_secs_f64(1.0 / rps as f64);
    let latencies = Arc::new(tokio::sync::Mutex::new(Vec::with_capacity(total)));
    let failures = Arc::new(AtomicUsize::new(0));
    let started = Instant::now();
    let mut handles = Vec::with_capacity(total);

    for i in 0..total {
        // 出発時刻を固定してペースを守る（前のリクエストが遅れても遅延させない）
        let due = started + interval.mul_f64(i as f64);
        let now = Instant::now();
        if due > now {
            tokio::time::sleep(due - now).await;
        }
        let app = app.clone();
        let latencies = Arc::clone(&latencies);
        let failures = Arc::clone(&failures);
        let cookie = session_cookie(player((i % players + 1) as u128));
        handles.push(tokio::spawn(async move {
            let request = Request::get("/queue")
                .header(header::COOKIE, cookie)
                .body(Body::empty())
                .unwrap();
            let sent = Instant::now();
            let response = app.oneshot(request).await;
            let elapsed = sent.elapsed();
            match response {
                Ok(response) if response.status() == StatusCode::OK => {
                    latencies.lock().await.push(elapsed);
                }
                _ => {
                    failures.fetch_add(1, Ordering::Relaxed);
                }
            }
        }));
    }
    for handle in handles {
        handle.await.expect("request task");
    }
    let wall = started.elapsed();

    let mut sorted = latencies.lock().await.clone();
    sorted.sort_unstable();
    let failed = failures.load(Ordering::Relaxed);
    println!("--------------------------------------------------");
    println!("GET /queue: {players}人がキュー中 / {rps} req/s / {seconds}秒");
    println!("送信: {total} 件 / 成功: {} 件 / 失敗: {failed} 件", sorted.len());
    println!("実測の所要時間: {:.1} 秒", wall.as_secs_f64());
    println!("p50: {:.2} ms", percentile(&sorted, 0.50));
    println!("p95: {:.2} ms", percentile(&sorted, 0.95));
    println!("p99: {:.2} ms", percentile(&sorted, 0.99));
    println!(
        "max: {:.2} ms",
        sorted.last().map_or(0.0, |d| d.as_secs_f64() * 1000.0)
    );
    println!("--------------------------------------------------");
    assert_eq!(failed, 0, "失敗したリクエストがある");
    pool.close().await;
}
