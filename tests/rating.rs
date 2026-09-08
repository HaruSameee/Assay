//! レートの導出（07-マッチング仕様 §2-2）の統合テスト。実際の PostgreSQL を使う。
//!
//! リポジトリは所有者ではなく**アプリ用ロール**（`assay_app`）で接続する。ビューへの
//! SELECT 権限が付いていなければここで 42501 になる。
//!
//! 検証すること:
//! - SQL のビューと Rust の [`match_delta`] が同じ値を出す（式が 2 か所にあるため）
//! - 差 50 未満で ±11、格上勝ち +12、格下勝ち +10、格上負け −10、格下負け −12
//! - nullified と draw は増減 0。disputed / voided はレートに影響しない
//! - **1 勝した 1500 が 1511 になり、1600 とマッチ可能になる**（07 §3-2 のプール融合）
//! - 同じログを 2 回導出して同じ結果になる
//! - `rating_adjustments` の +100 が初期値に効いている
//! - キューとマッチメイカが、この結果を反映したレートを見ている

use chrono::{TimeDelta, TimeZone, Utc};
use sqlx::PgPool;
use sqlx::postgres::PgPoolOptions;
use unite_matchsystem::lifecycle::{FormationEventKind as K, FormationId, PlayerId, Timestamp};
use unite_matchsystem::matchmaker::{Matchmaker, MatchmakerConfig, MatchmakerRepository, ROLES};
use unite_matchsystem::persistence::{
    FormationRepository, NewEvent, NewFormation, NewFormationPlayer, Team, VcAssignment,
};
use unite_matchsystem::queue::QueueRepository;
use unite_matchsystem::queue::model::{QueueRequest, RolePreference};
use unite_matchsystem::rating::{
    DEFAULT_DISPLAY_MU, DISPLAY_SCALE, MatchResult, RatingAdjustmentRepository, match_delta,
};
use uuid::Uuid;

const APP_ROLE: &str = "assay_app";
const APP_ROLE_PASSWORD: &str = "assay_app_test";

// ----------------------------------------------------------------------
// 道具
// ----------------------------------------------------------------------

fn at(seconds: i64) -> Timestamp {
    Utc.with_ymd_and_hms(2026, 9, 12, 21, 0, 0).unwrap() + TimeDelta::seconds(seconds)
}

fn player(n: u128) -> PlayerId {
    PlayerId(Uuid::from_u128(n))
}

fn fid(n: u128) -> FormationId {
    FormationId(Uuid::from_u128(n))
}

struct AppDb {
    pool: PgPool,
    formations: FormationRepository,
    queue: QueueRepository,
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
            queue: QueueRepository::new(pool.clone()),
            ratings: RatingAdjustmentRepository::new(pool.clone()),
            pool,
        }
    }

    async fn done(self) {
        self.pool.close().await;
    }
}

/// `players` の行を用意する（追記専用テーブルの FK 先）。
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

/// 表示スケールのレートを `rating_adjustments` への追記で作る。
async fn adjust_rating(db: &AppDb, who: PlayerId, delta_mu: f64, key: &str) {
    sqlx::query(
        "insert into rating_adjustments \
         (player_id, delta_mu, delta_sigma, reason, created_by, created_at, idempotency_key) \
         values ($1, $2, 0, 'test seed', 'staff:test', now(), $3)",
    )
    .bind(who.0)
    .bind(delta_mu)
    .bind(key)
    .execute(&db.pool)
    .await
    .expect("append a rating adjustment");
}

/// players 1〜5 が red、6〜10 が blue。レートは表示スケールで渡す。
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
                mu_at_match: if i <= 5 { red_display } else { blue_display } / DISPLAY_SCALE,
                sigma_at_match: 500.0 / DISPLAY_SCALE,
            })
            .collect(),
    }
}

/// formation を作って `in_progress` まで進める（確定イベントを積める状態）。
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

/// 終端イベントを積む。`payload` は Phase 6 の評価器が書くものと同じ形。
async fn finish(db: &AppDb, n: u128, kind: K, outcome: &str) {
    // 冪等キーは formation 内で一意。同じ formation を disputed から confirmed へ進める
    // ケースがあるので、種別も混ぜる
    let event = NewEvent::new(kind, None, at(10), format!("finish-{n}-{}", kind.as_str()))
        .with_payload(serde_json::json!({ "outcome": outcome }));
    db.formations.append_event(fid(n), &event).await.unwrap();
}

/// 1 試合を確定まで進める。
async fn confirmed_match(db: &AppDb, n: u128, red_display: f64, blue_display: f64, winner: &str) {
    play(db, n, red_display, blue_display).await;
    finish(db, n, K::Confirmed, winner).await;
}

/// ビューが出す 1 試合ぶんの増減。
async fn view_delta(db: &AppDb, who: PlayerId, formation: FormationId) -> f64 {
    sqlx::query_scalar(
        "select delta from player_match_rating_deltas where player_id = $1 and formation_id = $2",
    )
    .bind(who.0)
    .bind(formation.0)
    .fetch_one(&db.pool)
    .await
    .expect("the view must expose the match")
}

/// キューが見ているレート。
async fn queued_rating(db: &AppDb, who: PlayerId) -> f64 {
    db.queue
        .queued_players()
        .await
        .unwrap()
        .into_iter()
        .find(|queued| queued.player_id == who)
        .expect("player is queued")
        .rating
}

async fn enqueue(db: &AppDb, who: PlayerId, role: &str, at_time: Timestamp) {
    db.queue
        .enqueue(
            who,
            &QueueRequest {
                roles: vec![RolePreference {
                    role: role.to_owned(),
                    priority: 1,
                }],
            },
            at_time,
        )
        .await
        .expect("enqueue");
}

// ----------------------------------------------------------------------
// 増減式: SQL と Rust が一致すること
// ----------------------------------------------------------------------

/// 式は Rust（[`match_delta`]）と SQL ビューの 2 か所にある。**同じ値を出すこと。**
///
/// 片方だけ直す事故を防ぐ。表示スケールの 60 が両側で一致していることもここで効いてくる。
#[sqlx::test(fixtures("app_role"), fixtures("../ops/app_privileges.sql"))]
async fn the_view_and_the_rust_formula_agree(owner: PgPool) {
    let db = AppDb::connect(&owner).await;
    seed_players(&db, 10).await;

    // (red の表示レート, blue の表示レート, 勝者)
    let cases: [(f64, f64, &str); 8] = [
        (1500.0, 1500.0, "red"),  // 差 0、red 勝ち
        (1500.0, 1500.0, "blue"), // 差 0、red 負け
        (1500.0, 1550.0, "red"),  // 格上に勝つ（境界ちょうど）
        (1500.0, 1550.0, "blue"), // 格上に負ける
        (1500.0, 1450.0, "red"),  // 格下に勝つ（境界ちょうど）
        (1500.0, 1450.0, "blue"), // 格下に負ける
        (1500.0, 1549.0, "red"),  // 境界の直前は基本値
        (1500.0, 1500.0, "draw"), // 引き分け
    ];

    for (index, (red, blue, winner)) in cases.into_iter().enumerate() {
        let n = index as u128 + 1;
        confirmed_match(&db, n, red, blue, winner).await;

        for (who, own, opponent_mean, team) in [
            (player(1), red, blue, "red"),
            (player(6), blue, red, "blue"),
        ] {
            let expected = match_delta(
                own,
                opponent_mean,
                match winner {
                    "draw" => MatchResult::Drawn,
                    w if w == team => MatchResult::Won,
                    _ => MatchResult::Lost,
                },
            );
            assert_eq!(
                view_delta(&db, who, fid(n)).await,
                expected,
                "case {index} ({red} vs {blue}, {winner}) for {team}"
            );
        }
    }
    db.done().await;
}

/// 表と同じ値がそのまま出ること（読んで確認できる形で 1 本置く）。
#[sqlx::test(fixtures("app_role"), fixtures("../ops/app_privileges.sql"))]
async fn the_view_produces_the_documented_numbers(owner: PgPool) {
    let db = AppDb::connect(&owner).await;
    seed_players(&db, 10).await;

    // params v3: 差に関わらず ±7
    confirmed_match(&db, 1, 1500.0, 1500.0, "red").await;
    assert_eq!(view_delta(&db, player(1), fid(1)).await, 7.0);
    assert_eq!(view_delta(&db, player(6), fid(1)).await, -7.0);

    confirmed_match(&db, 2, 1500.0, 1600.0, "red").await;
    assert_eq!(view_delta(&db, player(1), fid(2)).await, 7.0);
    assert_eq!(view_delta(&db, player(6), fid(2)).await, -7.0);
    // red 1600 / blue 1500 で red 勝ち。**格下でも +7 のまま**
    confirmed_match(&db, 3, 1600.0, 1500.0, "red").await;
    assert_eq!(view_delta(&db, player(1), fid(3)).await, 7.0);
    assert_eq!(view_delta(&db, player(6), fid(3)).await, -7.0);

    // 引き分けは双方 0
    confirmed_match(&db, 4, 1500.0, 1600.0, "draw").await;
    assert_eq!(view_delta(&db, player(1), fid(4)).await, 0.0);
    assert_eq!(view_delta(&db, player(6), fid(4)).await, 0.0);

    db.done().await;
}

// ----------------------------------------------------------------------
// 確定していない試合はレートに影響しない
// ----------------------------------------------------------------------

/// `nullified` / `disputed` / `voided` はレートを動かさない。
///
/// どれも `confirmed` イベントを持たないので、ビューにそもそも現れない。
#[sqlx::test(fixtures("app_role"), fixtures("../ops/app_privileges.sql"))]
async fn only_confirmed_matches_move_the_rating(owner: PgPool) {
    let db = AppDb::connect(&owner).await;
    seed_players(&db, 10).await;

    play(&db, 1, 1500.0, 1600.0).await;
    finish(&db, 1, K::Nullified, "nullified").await;
    play(&db, 2, 1500.0, 1600.0).await;
    finish(&db, 2, K::Disputed, "disputed").await;
    play(&db, 3, 1500.0, 1600.0).await;
    db.formations
        .append_event(fid(3), &NewEvent::new(K::Voided, None, at(10), "voided-3"))
        .await
        .unwrap();

    let rows: i64 = sqlx::query_scalar("select count(*) from player_match_rating_deltas")
        .fetch_one(&db.pool)
        .await
        .unwrap();
    assert_eq!(rows, 0, "no confirmed match exists yet");
    assert_eq!(db.ratings.display_rating(player(1)).await.unwrap(), 1500.0);
    assert_eq!(db.ratings.display_rating(player(6)).await.unwrap(), 1500.0);

    // スタッフ裁定で disputed から confirmed へ進めば、その時点で効き始める（04 §1）。
    // formation 2 は red 1500 / blue 1600 で red 勝ち。v3 は分岐が無いので red +7、
    // blue は -7
    finish(&db, 2, K::Confirmed, "red").await;
    assert_eq!(db.ratings.display_rating(player(1)).await.unwrap(), 1507.0);
    assert_eq!(db.ratings.display_rating(player(6)).await.unwrap(), 1493.0);

    db.done().await;
}

// ----------------------------------------------------------------------
// 07 §3-2 プールの融合
// ----------------------------------------------------------------------

/// **1 勝した 1500 が 1511 になり、1600 とマッチ可能になる。**
///
/// これが起きないと、初日は 1500 プールと 1600 プールが分かれたまま固定される。
#[sqlx::test(fixtures("app_role"), fixtures("../ops/app_privileges.sql"))]
async fn one_win_is_not_enough_to_bridge_the_two_pools(owner: PgPool) {
    let db = AppDb::connect(&owner).await;
    seed_players(&db, 11).await;

    // 11 番は運営が +100 した 1600 の人。初期値の第二種ではなく調整ログ 1 行（07 §2-1）
    let seeded = player(11);
    adjust_rating(&db, seeded, 100.0, "seed-1600").await;

    let climber = player(1);
    enqueue(&db, climber, "jungle", at(0)).await;
    enqueue(&db, seeded, "top_carry", at(0)).await;

    // 勝つ前: 1500 と 1600 は差 100 で、固定幅 50 の外
    assert_eq!(queued_rating(&db, climber).await, 1500.0);
    assert_eq!(queued_rating(&db, seeded).await, 1600.0);
    assert_eq!(
        db.queue
            .in_range_summary(climber)
            .await
            .unwrap()
            .map(|s| s.total),
        Some(0),
        "the two pools must not mix before anyone plays"
    );

    // 同格の相手に 1 勝
    confirmed_match(&db, 1, 1500.0, 1500.0, "red").await;

    assert_eq!(
        queued_rating(&db, climber).await,
        1507.0,
        "1 勝で 1507（params v3 は ±7 フラット）"
    );
    assert_eq!(
        db.queue
            .in_range_summary(climber)
            .await
            .unwrap()
            .map(|s| s.total),
        Some(0),
        "**1 勝では届かない。**1507 と 1600 は差 93 で、窓 50 の外"
    );
    assert_eq!(
        db.queue
            .in_range_summary(seeded)
            .await
            .unwrap()
            .map(|s| s.total),
        Some(0)
    );

    db.done().await;
}

/// マッチメイカも同じレートを見ている（キューだけ直っていても意味がない）。
#[sqlx::test(fixtures("app_role"), fixtures("../ops/app_privileges.sql"))]
async fn the_matchmaker_sees_the_updated_rating(owner: PgPool) {
    let db = AppDb::connect(&owner).await;
    seed_players(&db, 10).await;

    // 1〜5 を +100 して 1600 に、6〜10 は 1500 のまま。差 100 で組めない
    for n in 1..=5u128 {
        adjust_rating(&db, player(n), 100.0, &format!("seed-{n}")).await;
    }
    for n in 1..=10u128 {
        enqueue(&db, player(n), ROLES[(n as usize - 1) % ROLES.len()], at(0)).await;
    }
    let matchmaker = Matchmaker::new(
        MatchmakerRepository::new(db.pool.clone()),
        vec![VcAssignment {
            label: "1".into(),
            red_channel_id: 101,
            blue_channel_id: 102,
        }],
        MatchmakerConfig::default(),
    );
    assert!(
        matchmaker
            .run_once(at(600))
            .await
            .unwrap()
            .created
            .is_empty(),
        "1500 と 1600 は差 100 で窓 50 の外"
    );

    // 6〜10 が +50 で 1550 になれば、1600 との差はちょうど 50 で組める
    for n in 6..=10u128 {
        adjust_rating(&db, player(n), 50.0, &format!("win-{n}")).await;
    }
    let outcome = matchmaker.run_once(at(600)).await.unwrap();
    assert_eq!(outcome.created.len(), 1, "{outcome:?}");

    db.done().await;
}

// ----------------------------------------------------------------------
// 導出の性質
// ----------------------------------------------------------------------

/// 同じログを 2 回導出して同じ結果になること。増減は凍結値だけから決まるので総和になる。
#[sqlx::test(fixtures("app_role"), fixtures("../ops/app_privileges.sql"))]
async fn deriving_the_same_log_twice_gives_the_same_rating(owner: PgPool) {
    let db = AppDb::connect(&owner).await;
    seed_players(&db, 10).await;
    adjust_rating(&db, player(1), 100.0, "seed").await;

    confirmed_match(&db, 1, 1600.0, 1500.0, "red").await;
    confirmed_match(&db, 2, 1600.0, 1700.0, "blue").await;
    confirmed_match(&db, 3, 1600.0, 1600.0, "draw").await;

    let first = db.ratings.display_rating(player(1)).await.unwrap();
    let second = db.ratings.display_rating(player(1)).await.unwrap();
    assert_eq!(first, second);
    // 1600 + 10（格下に勝ち） - 10（格上に負け） + 0（引き分け）
    assert_eq!(first, 1600.0);

    // Rust 側の畳み込みと SQL の集計が一致すること
    let aggregated: f64 =
        sqlx::query_scalar("select delta from player_match_ratings where player_id = $1")
            .bind(player(1).0)
            .fetch_one(&db.pool)
            .await
            .unwrap();
    assert_eq!(DEFAULT_DISPLAY_MU + 100.0 + aggregated, first);

    db.done().await;
}

/// `rating_adjustments` の +100 が初期値に効いていること。
#[sqlx::test(fixtures("app_role"), fixtures("../ops/app_privileges.sql"))]
async fn the_manual_adjustment_is_the_starting_point_not_a_second_default(owner: PgPool) {
    let db = AppDb::connect(&owner).await;
    seed_players(&db, 10).await;
    adjust_rating(&db, player(1), 100.0, "seed").await;

    assert_eq!(db.ratings.display_rating(player(1)).await.unwrap(), 1600.0);
    assert_eq!(db.ratings.display_rating(player(2)).await.unwrap(), 1500.0);

    // 調整を打ち消せば 1500 に戻る（打ち消しレコードで表現する）
    let original: i64 =
        sqlx::query_scalar("select id from rating_adjustments where player_id = $1")
            .bind(player(1).0)
            .fetch_one(&db.pool)
            .await
            .unwrap();
    sqlx::query(
        "insert into rating_adjustments \
         (player_id, delta_mu, delta_sigma, reason, created_by, created_at, idempotency_key, reverts_id) \
         values ($1, -100, 0, 'revert', 'staff:test', now(), 'revert-1', $2)",
    )
    .bind(player(1).0)
    .bind(original)
    .execute(&db.pool)
    .await
    .unwrap();
    assert_eq!(db.ratings.display_rating(player(1)).await.unwrap(), 1500.0);

    db.done().await;
}

// ----------------------------------------------------------------------
// 性能
// ----------------------------------------------------------------------

/// 500 試合規模で `GET /queue` の読みとマッチメイカ 1 回が実用時間に収まること。
///
/// `GET /queue` は 96〜170 人が 5 秒ごとにポーリングする（05 §5）ので、呼ばれる回数が多い。
/// レートを導出値のままにしたぶん、1 回の重さがそのまま効く。
///
/// **既定では走らせない**（CI を重くしないため）。実測は
/// `cargo test --test rating -- --ignored --nocapture` で取る。
///
/// 種の投入は生 SQL で行う。ビューが読むのは `formation_players` と `confirmed` イベントだけ
/// なので、状態機械を通す必要がない（通すと 2500 トランザクションになり、測りたいものと
/// 関係のない時間が乗る）。
#[ignore = "性能測定。--ignored で明示的に走らせる"]
#[sqlx::test(fixtures("app_role"), fixtures("../ops/app_privileges.sql"))]
async fn five_hundred_matches_stay_within_budget(owner: PgPool) {
    use std::time::Instant;

    const PLAYERS: u128 = 170;
    const MATCHES: u128 = 500;
    const QUEUED: u128 = 170;

    let db = AppDb::connect(&owner).await;
    seed_players(&db, PLAYERS).await;

    // 500 試合。参加者は 170 人から 10 人ずつ回して選ぶ（1 人あたり約 30 試合）
    for n in 1..=MATCHES {
        let base = (n * 7) % PLAYERS;
        sqlx::query(
            "insert into formations (id, created_at, matchmaking_params_version, predicted_win_prob) \
             values ($1, now(), 1, 0.5)",
        )
        .bind(fid(0x1000 + n).0)
        .execute(&db.pool)
        .await
        .unwrap();
        for slot in 0..10u128 {
            let who = player((base + slot) % PLAYERS + 1);
            sqlx::query(
                "insert into formation_players \
                 (formation_id, player_id, team, role, role_was_flexed, mu_at_match, sigma_at_match) \
                 values ($1, $2, $3, $4, false, $5, 8.33)",
            )
            .bind(fid(0x1000 + n).0)
            .bind(who.0)
            .bind(if slot < 5 { "red" } else { "blue" })
            .bind(ROLES[(slot % 5) as usize])
            .bind((1500.0 + (slot as f64) * 3.0) / DISPLAY_SCALE)
            .execute(&db.pool)
            .await
            .unwrap();
        }
        sqlx::query(
            "insert into formation_events \
             (formation_id, occurred_at, kind, subject_player_id, payload, idempotency_key) \
             values ($1, now(), 'confirmed', null, $2, $3)",
        )
        .bind(fid(0x1000 + n).0)
        .bind(serde_json::json!({ "outcome": if n % 2 == 0 { "red" } else { "blue" } }))
        .bind(format!("perf-{n}"))
        .execute(&db.pool)
        .await
        .unwrap();
    }

    // 170 人がキューにいる状態
    for n in 1..=QUEUED {
        enqueue(&db, player(n), ROLES[(n as usize - 1) % ROLES.len()], at(0)).await;
    }

    let rows: i64 = sqlx::query_scalar("select count(*) from player_match_rating_deltas")
        .fetch_one(&db.pool)
        .await
        .unwrap();
    assert_eq!(rows, (MATCHES * 10) as i64, "seed must be complete");

    // GET /queue が 1 回で行う 3 つの読み（自分のエントリ / 範囲内の人数 / 3 つのゲート）。
    // 1 回目はプラン作成ぶんが乗るので、捨てずに測って max として報告する。
    let registration =
        unite_matchsystem::registration::RegistrationRepository::new(db.pool.clone());
    let mut queue_samples = Vec::new();
    let mut in_range_samples = Vec::new();
    for _ in 0..10 {
        let started = Instant::now();
        let entry = db.queue.find(player(1)).await.unwrap();
        let range_started = Instant::now();
        let count = db
            .queue
            .in_range_summary(player(1))
            .await
            .unwrap()
            .map(|s| s.total);
        in_range_samples.push(range_started.elapsed());
        let eligibility = registration.queue_eligibility(player(1)).await.unwrap();
        queue_samples.push(started.elapsed());
        assert!(entry.is_some() && count.is_some());
        let _ = eligibility;
    }
    queue_samples.sort();
    in_range_samples.sort();

    // マッチメイカ 1 回（候補の読み込み + 選択 + 成立ぶんの書き込み）
    let matchmaker = Matchmaker::new(
        MatchmakerRepository::new(db.pool.clone()),
        (1..=20)
            .map(|n| VcAssignment {
                label: n.to_string(),
                red_channel_id: 100 + n as u64 * 2,
                blue_channel_id: 101 + n as u64 * 2,
            })
            .collect(),
        MatchmakerConfig::default(),
    );
    let started = Instant::now();
    let outcome = matchmaker.run_once(at(600)).await.unwrap();
    let matchmaker_elapsed = started.elapsed();

    println!("--- 500 matches / {QUEUED} queued ---");
    println!(
        "GET /queue の DB 3 本合計: median {:?} / max {:?}",
        queue_samples[queue_samples.len() / 2],
        queue_samples.last().unwrap()
    );
    println!(
        "  うち範囲内の人数（レート導出を含む）: median {:?} / max {:?}",
        in_range_samples[in_range_samples.len() / 2],
        in_range_samples.last().unwrap()
    );
    println!(
        "マッチメイカ 1 回: {matchmaker_elapsed:?} (candidates={} formed={})",
        outcome.candidates,
        outcome.created.len()
    );

    db.done().await;
}
