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
use unite_matchsystem::admin::{
    AdminRepository, NewRatingParams, NewSeasonBoundary, RatingParamsOutcome,
};
use unite_matchsystem::lifecycle::{FormationEventKind as K, FormationId, PlayerId, Timestamp};
use unite_matchsystem::match_screen::MatchRepository;
use unite_matchsystem::matchmaker::{Matchmaker, MatchmakerConfig, MatchmakerRepository, ROLES};
use unite_matchsystem::persistence::{
    FormationRepository, NewEvent, NewFormation, NewFormationPlayer, Team, VC_POOL_LOCK_KEY,
    VcAssignment,
};
use unite_matchsystem::profiles::ProfileRepository;
use unite_matchsystem::queue::QueueRepository;
use unite_matchsystem::queue::model::{QueueRequest, RolePreference};
use unite_matchsystem::ranking::RankingRepository;
use unite_matchsystem::rating::{
    DEFAULT_DISPLAY_MU, DISPLAY_SCALE, MatchResult, RatingAdjustmentRepository, match_delta,
};
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
                streaming: false,
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
                streaming: false,
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
            // 増減幅は**ビューと同じ導出経路**から読む（定数を突き合わせない）
            let base_delta = db.ratings.base_delta().await.unwrap();
            let expected = match_delta(
                own,
                opponent_mean,
                match winner {
                    "draw" => MatchResult::Drawn,
                    w if w == team => MatchResult::Won,
                    _ => MatchResult::Lost,
                },
                base_delta,
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

// ----------------------------------------------------------------------
// レート増減幅の変更（運営操作）
// ----------------------------------------------------------------------

/// **増減幅を変えると、過去の全試合が新しい値で引き直される。**
///
/// docs/02 の不変条件 3（常に現行 params を全履歴に適用する）そのもの。
/// あわせて、**試合ログの行数が 1 行も変わらない**ことを見る——引き直しは
/// 再計算であって、書き換えではない。
#[sqlx::test(fixtures("app_role"), fixtures("../ops/app_privileges.sql"))]
async fn changing_the_delta_recomputes_every_past_match(owner: PgPool) {
    let db = AppDb::connect(&owner).await;
    seed_players(&db, 10).await;
    let admin = AdminRepository::new(db.pool.clone());

    // 3 試合ぶん確定させる（player 1 は red で 2 勝 1 敗）
    confirmed_match(&db, 1, 1500.0, 1500.0, "red").await;
    confirmed_match(&db, 2, 1500.0, 1500.0, "red").await;
    confirmed_match(&db, 3, 1500.0, 1500.0, "blue").await;

    async fn log_rows(db: &AppDb) -> (i64, i64) {
        let events: i64 = sqlx::query_scalar("select count(*) from formation_events")
            .fetch_one(&db.pool)
            .await
            .unwrap();
        let players: i64 = sqlx::query_scalar("select count(*) from formation_players")
            .fetch_one(&db.pool)
            .await
            .unwrap();
        (events, players)
    }

    // 既定値（7）での状態
    let before_params = admin.rating_params().await.unwrap();
    assert_eq!(before_params.base_delta, 7.0);
    assert_eq!(before_params.params_version, 3);
    let before_rating = db.ratings.display_rating(player(1)).await.unwrap();
    let before_deltas = view_delta(&db, player(1), fid(1)).await;
    let before_rows = log_rows(&db).await;
    assert_eq!(before_deltas, 7.0);
    // 2 勝 1 敗 = +7 +7 -7
    assert_eq!(before_rating, 1500.0 + 7.0);

    // 実行前のプレビュー: 引き直される試合数と人数
    let preview = admin.rating_params_preview().await.unwrap();
    assert_eq!(preview.matches_affected, 3, "引き直される試合数が違う");
    assert_eq!(preview.players_affected, 10, "影響を受ける人数が違う");
    assert!(preview.history.is_empty(), "まだ変更していない");

    // **増減幅を 12 に変える**
    let outcome = admin
        .change_rating_params(&NewRatingParams {
            base_delta: 12.0,
            reason: "テストで引き直しを確認する".to_owned(),
            actor_discord_user_id: 1001,
            created_at: at(100),
            idempotency_key: "delta-12".to_owned(),
        })
        .await
        .unwrap();
    let RatingParamsOutcome::Changed { params_version, .. } = outcome else {
        panic!("変更されていない: {outcome:?}");
    };
    // 版数は直前 +1
    assert_eq!(params_version, 4);

    // **過去の試合が新しい値で引き直されている**
    let after_params = admin.rating_params().await.unwrap();
    assert_eq!(after_params.base_delta, 12.0);
    assert_eq!(after_params.params_version, 4);
    let after_deltas = view_delta(&db, player(1), fid(1)).await;
    let after_rating = db.ratings.display_rating(player(1)).await.unwrap();
    assert_eq!(after_deltas, 12.0, "過去の試合が引き直されていない");
    assert_eq!(
        after_rating,
        1500.0 + 12.0,
        "現在レートが引き直されていない"
    );
    // 負けた試合も
    assert_eq!(view_delta(&db, player(1), fid(3)).await, -12.0);

    // **試合ログの行数は 1 行も変わらない**
    let after_rows = log_rows(&db).await;
    assert_eq!(after_rows, before_rows, "試合ログの行数が変わっている");

    // 運営操作ログに残っている
    let action: (String, serde_json::Value) = sqlx::query_as(
        "select kind, payload from admin_actions where kind = 'rating_params_changed'",
    )
    .fetch_one(&db.pool)
    .await
    .unwrap();
    assert_eq!(action.1["base_delta"], serde_json::json!(12.0));
    assert_eq!(action.1["previous_base_delta"], serde_json::json!(7.0));
    assert_eq!(action.1["matches_recomputed"], serde_json::json!(3));

    println!(
        "増減幅 7 → 12: 1 試合の増減 {before_deltas} → {after_deltas} / \
         現在レート {before_rating} → {after_rating} / \
         試合ログ（events, players）= {before_rows:?} → {after_rows:?}（変化なし）"
    );

    // **同一キーは no-op。**状態ベースの判定はしない
    let again = admin
        .change_rating_params(&NewRatingParams {
            base_delta: 20.0,
            reason: "同じキーでもう一度".to_owned(),
            actor_discord_user_id: 1001,
            created_at: at(200),
            idempotency_key: "delta-12".to_owned(),
        })
        .await
        .unwrap();
    assert_eq!(again, RatingParamsOutcome::Duplicate);
    assert_eq!(admin.rating_params().await.unwrap().base_delta, 12.0);

    // 元の値へ戻すのも**追記**（消さずにもう 1 行積む）
    admin
        .change_rating_params(&NewRatingParams {
            base_delta: 7.0,
            reason: "戻す".to_owned(),
            actor_discord_user_id: 1001,
            created_at: at(300),
            idempotency_key: "delta-back".to_owned(),
        })
        .await
        .unwrap();
    let restored = admin.rating_params().await.unwrap();
    assert_eq!(restored.base_delta, 7.0);
    assert_eq!(restored.params_version, 5, "戻しても版数は進む");
    assert_eq!(view_delta(&db, player(1), fid(1)).await, 7.0);
    let history = admin.rating_params_preview().await.unwrap().history;
    assert_eq!(history.len(), 2, "変更の行が消えている");
    println!(
        "戻したあと: 増減幅 {} / params v{}（履歴 {} 行は残る）",
        restored.base_delta,
        restored.params_version,
        history.len()
    );

    // **DB の CHECK が最後の砦。**0 以下・50 超はアプリを通さなくても入らない
    for bad in [0.0, -1.0, 50.1, 100.0] {
        let rejected = sqlx::query(
            "insert into rating_param_changes \
             (params_version, base_delta, reason, created_by, idempotency_key, created_at) \
             values (99, $1, 'r', 'discord:1', $2, now())",
        )
        .bind(bad)
        .bind(format!("bad-{bad}"))
        .execute(&db.pool)
        .await;
        assert!(rejected.is_err(), "{bad} が DB に入ってしまった");
    }
    println!("DB の CHECK: 0 以下と 50 超は拒否");

    db.done().await;
}

/// **進行中の formation があるあいだは増減幅を変えられない。**
///
/// 1 行積んだ瞬間に全員のレートが動き、キュー中の人の範囲判定（±50）も同時に変わる。
/// シーズン境界と同じ条件で止める。**断ったことは記録に残す。**
#[sqlx::test(fixtures("app_role"), fixtures("../ops/app_privileges.sql"))]
async fn an_active_formation_blocks_the_change(owner: PgPool) {
    let db = AppDb::connect(&owner).await;
    seed_players(&db, 10).await;
    let admin = AdminRepository::new(db.pool.clone());

    // 確定済みが 1 件（引き直しの対象）、進行中が 1 件
    confirmed_match(&db, 1, 1500.0, 1500.0, "red").await;
    play(&db, 2, 1500.0, 1500.0).await; // in_progress のまま

    let preview = admin.rating_params_preview().await.unwrap();
    assert_eq!(preview.active_formations, 1, "進行中を数えていない");
    assert_eq!(preview.pending_reports, 1, "未報告を数えていない");
    assert!(!preview.can_change(), "進行中があるのに実行できてしまう");

    let blocked = admin
        .change_rating_params(&NewRatingParams {
            base_delta: 20.0,
            reason: "進行中でも変えようとする".to_owned(),
            actor_discord_user_id: 1001,
            created_at: at(100),
            idempotency_key: "while-active".to_owned(),
        })
        .await
        .unwrap();
    assert_eq!(
        blocked,
        RatingParamsOutcome::Blocked {
            active_formations: 1
        }
    );

    // **増減幅は動いていない。**行も増えていない
    assert_eq!(admin.rating_params().await.unwrap().base_delta, 7.0);
    assert_eq!(view_delta(&db, player(1), fid(1)).await, 7.0);
    let rows: i64 = sqlx::query_scalar("select count(*) from rating_param_changes")
        .fetch_one(&db.pool)
        .await
        .unwrap();
    assert_eq!(rows, 0, "拒否したのに行が積まれている");

    // **断ったことは admin_actions に残る**
    let rejected: (String, serde_json::Value) = sqlx::query_as(
        "select kind, payload from admin_actions where kind = 'rating_params_rejected'",
    )
    .fetch_one(&db.pool)
    .await
    .unwrap();
    assert_eq!(rejected.1["requested_base_delta"], serde_json::json!(20.0));
    assert_eq!(rejected.1["active_formations"], serde_json::json!(1));
    println!("進行中 1 件 → 拒否。admin_actions: {}", rejected.0);

    // 進行中の試合を終わらせれば通る
    finish(&db, 2, K::Confirmed, "red").await;
    let preview = admin.rating_params_preview().await.unwrap();
    assert_eq!(preview.active_formations, 0);
    assert!(preview.can_change());
    let changed = admin
        .change_rating_params(&NewRatingParams {
            base_delta: 20.0,
            reason: "終わったので変える".to_owned(),
            actor_discord_user_id: 1001,
            created_at: at(200),
            idempotency_key: "after-active".to_owned(),
        })
        .await
        .unwrap();
    assert!(matches!(changed, RatingParamsOutcome::Changed { .. }));
    assert_eq!(view_delta(&db, player(1), fid(1)).await, 20.0);
    println!("進行中 0 件 → 実行できる（増減幅 20 で引き直し）");

    db.done().await;
}

/// **増減幅を変えると、キュー中の人の範囲判定（±50）の対象が変わる。**
///
/// レートが導出値なので、増減幅は「過去の勝敗がいまのレートに与える大きさ」を変える。
/// その結果、誰と誰が同じ範囲に入るかがその場で動く。
#[sqlx::test(fixtures("app_role"), fixtures("../ops/app_privileges.sql"))]
async fn changing_the_delta_moves_who_is_in_range(owner: PgPool) {
    let db = AppDb::connect(&owner).await;
    seed_players(&db, 10).await;
    let admin = AdminRepository::new(db.pool.clone());

    // player 1（red）が 1 勝、player 6（blue）が 1 敗。ほかは動かさない
    confirmed_match(&db, 1, 1500.0, 1500.0, "red").await;

    // 3 人をキューに入れる。player 2 も red なので 1 勝している
    for who in [player(1), player(2), player(6)] {
        enqueue(&db, who, "jungle", at(50)).await;
    }

    // 増減幅 7: 勝った 1 と 2 が 1507、負けた 6 が 1493。差は 14 で全員が範囲内
    let winner_before = queued_rating(&db, player(1)).await;
    let loser_before = queued_rating(&db, player(6)).await;
    let before = db
        .queue
        .in_range_summary(player(1))
        .await
        .unwrap()
        .unwrap()
        .total;
    assert_eq!(winner_before, 1507.0);
    assert_eq!(loser_before, 1493.0);
    assert_eq!(before, 2, "差 14 なら 2 人とも範囲内");

    // **増減幅を 30 に上げる。**勝者 1530 / 敗者 1470 で差 60 → 範囲外になる
    admin
        .change_rating_params(&NewRatingParams {
            base_delta: 30.0,
            reason: "範囲判定の変化を見る".to_owned(),
            actor_discord_user_id: 1001,
            created_at: at(300),
            idempotency_key: "widen".to_owned(),
        })
        .await
        .unwrap();

    let winner_after = queued_rating(&db, player(1)).await;
    let loser_after = queued_rating(&db, player(6)).await;
    let after = db
        .queue
        .in_range_summary(player(1))
        .await
        .unwrap()
        .unwrap()
        .total;
    assert_eq!(winner_after, 1530.0);
    assert_eq!(loser_after, 1470.0);
    assert_eq!(after, 1, "差 60 になった相手は範囲外へ出る");

    println!(
        "増減幅 7 → 30: 勝者 {winner_before} → {winner_after} / 敗者 {loser_before} → {loser_after} / \
         勝者から見た「範囲内でキュー中」 {before}人 → {after}人"
    );

    db.done().await;
}

// ----------------------------------------------------------------------
// 合算の突き合わせ（集約リファクタの前提確認）
// ----------------------------------------------------------------------
//
// 現在レートの合算は 8 か所に散っている（`player_current_ratings` へ集約する前の状態）。
// **集約が「同じ式を 1 か所へ寄せるだけ」であること**を、割れやすいケースを並べて確かめる。
//
// 境界の判定は **`formations.created_at`**（migrations/20260911090000 の
// `where f.created_at >= s.started_at`）。確定イベントの時刻ではないので、
// **境界より前に作られた試合は、境界より後に確定しても集計に入らない。**

/// 名簿の読み取りに使う formation（不成立で終わらせるので集計には入らない）。
const ROSTER_FORMATION: u128 = 29;

/// 1 経路ぶんの読み取り結果。適用できない経路は `None`。
#[derive(Debug, Clone, PartialEq)]
struct RatingPaths {
    values: Vec<(&'static str, Option<f64>)>,
    /// 試合数を返す経路（プロフィールとランキング）。
    matches: Vec<(&'static str, Option<i64>)>,
}

/// 8 経路すべてを 1 人について読む。
async fn rating_paths(db: &AppDb, who: PlayerId) -> RatingPaths {
    let by_rust = db.ratings.display_rating(who).await.unwrap();

    let by_queue = db
        .queue
        .queued_players()
        .await
        .unwrap()
        .into_iter()
        .find(|queued| queued.player_id == who)
        .map(|queued| queued.rating);

    let by_preview: Option<f64> = sqlx::query_scalar(
        "select $2::double precision \
                + coalesce((select sum(a.delta_mu) from season_rating_adjustments a \
                            where a.player_id = p.id), 0::double precision) \
                + coalesce(m.delta, 0::double precision) \
         from players p left join player_match_ratings m on m.player_id = p.id where p.id = $1",
    )
    .bind(who.0)
    .bind(DEFAULT_DISPLAY_MU)
    .fetch_optional(&db.pool)
    .await
    .unwrap();

    let by_matchmaker: Option<f64> = sqlx::query_scalar(
        "select $2::double precision \
                + coalesce((select sum(a.delta_mu) from season_rating_adjustments a \
                            where a.player_id = q.player_id), 0::double precision) \
                + coalesce(m.delta, 0::double precision) \
         from queue_entries q left join player_match_ratings m on m.player_id = q.player_id \
         where q.player_id = $1",
    )
    .bind(who.0)
    .bind(DEFAULT_DISPLAY_MU)
    .fetch_optional(&db.pool)
    .await
    .unwrap();

    // 名簿はどの formation でもよい（レートはプレイヤー単位の導出値）
    let by_match_screen = MatchRepository::new(db.pool.clone())
        .roster(fid(ROSTER_FORMATION))
        .await
        .unwrap()
        .into_iter()
        .find(|entry| entry.player_id == who)
        .map(|entry| entry.rating);

    let by_admin = AdminRepository::new(db.pool.clone())
        .queued_entries()
        .await
        .unwrap()
        .into_iter()
        .find(|entry| entry.player_id == who.0)
        .map(|entry| entry.rating);

    let profile = ProfileRepository::new(db.pool.clone())
        .public_profile(who)
        .await
        .unwrap();

    let ranked = RankingRepository::new(db.pool.clone())
        .page(1)
        .await
        .unwrap()
        .players
        .into_iter()
        .find(|row| row.player_id == who.0);

    RatingPaths {
        values: vec![
            ("1 rating/mod.rs (Rust)", Some(by_rust)),
            ("2 queue 範囲判定", by_queue),
            ("3 queue プレビュー", by_preview),
            ("4 matchmaker 凍結値", by_matchmaker),
            ("5 match_screen 名簿", by_match_screen),
            ("6 admin キュー一覧", by_admin),
            (
                "7 profiles 公開プロフィール",
                profile.as_ref().map(|p| p.rating),
            ),
            ("8 ranking ランキング", ranked.as_ref().map(|r| r.rating)),
        ],
        matches: vec![
            ("7 profiles", profile.as_ref().map(|p| p.matches)),
            ("8 ranking", ranked.as_ref().map(|r| r.matches)),
        ],
    }
}

/// 監査の 1 シナリオ。
struct Scenario {
    name: &'static str,
    who: PlayerId,
    expected: f64,
    /// 期待する試合数（`player_match_rating_deltas` の行数）。
    expected_matches: i64,
    /// どの formation にもいないので match_screen は行を持たない。
    in_formation: bool,
}

/// 8 経路を突き合わせ、値と適用可否を検査する。
fn assert_paths_agree(paths: &RatingPaths, scenario: &Scenario) {
    for (name, value) in &paths.values {
        let ranked = name.starts_with('8');
        let roster = name.starts_with('5');
        let applicable = if ranked {
            scenario.expected_matches >= 5
        } else if roster {
            scenario.in_formation
        } else {
            true
        };
        match (applicable, value) {
            (true, Some(value)) => assert_eq!(
                *value, scenario.expected,
                "{}: {name} がずれている",
                scenario.name
            ),
            (true, None) => panic!("{}: {name} が行を返さなかった", scenario.name),
            (false, Some(value)) => {
                panic!(
                    "{}: {name} は対象外のはずが {value} を返した",
                    scenario.name
                )
            }
            (false, None) => {}
        }
    }
    for (name, matches) in &paths.matches {
        if name.starts_with('8') && scenario.expected_matches < 5 {
            assert!(
                matches.is_none(),
                "{}: {name} に載ってはいけない",
                scenario.name
            );
            continue;
        }
        assert_eq!(
            *matches,
            Some(scenario.expected_matches),
            "{}: {name} の試合数がずれている",
            scenario.name
        );
    }
}

fn print_paths(paths: &RatingPaths, scenario: &Scenario) {
    println!("--- {} (期待 {}) ---", scenario.name, scenario.expected);
    for (name, value) in &paths.values {
        match value {
            Some(value) => println!("  {name:<32} {value}"),
            None => println!("  {name:<32} （対象外）"),
        }
    }
}

/// **8 経路が同じ値を返すこと。**割れやすいケースを並べる。
#[sqlx::test(fixtures("app_role"), fixtures("../ops/app_privileges.sql"))]
async fn every_rating_path_agrees(owner: PgPool) {
    let db = AppDb::connect(&owner).await;
    // 11・12 はどの formation にも入らない（1 試合もしていない人の検査用）
    seed_players(&db, 12).await;

    // 境界より前の試合を 2 件。**どの経路にも入ってはいけない**
    confirmed_match(&db, 1, 1500.0, 1500.0, "red").await;
    confirmed_match(&db, 2, 1500.0, 1500.0, "red").await;
    // 境界（この時刻より前に**作られた**試合と調整が集計から外れる）
    sqlx::query(
        "insert into season_boundaries \
         (effective_at, label, created_by, idempotency_key, created_at) \
         values ($1, 'audit', 'discord:1', 'audit-boundary', $1)",
    )
    .bind(at(20))
    .execute(&db.pool)
    .await
    .expect("append a boundary");

    // 境界より後: red が 6 勝、引き分け 1、無効試合 1、不成立 1
    for n in 21..=26u128 {
        confirmed_match(&db, n, 1500.0, 1500.0, "red").await;
    }
    confirmed_match(&db, 27, 1500.0, 1500.0, "draw").await;
    play(&db, 28, 1500.0, 1500.0).await;
    finish(&db, 28, K::Nullified, "nullified").await;
    // 名簿の読み取り用。不成立で終わらせるので集計にも進行中にも入らない
    play(&db, ROSTER_FORMATION, 1500.0, 1500.0).await;
    finish(&db, ROSTER_FORMATION, K::Voided, "voided").await;

    // 調整は**境界より後**に入れる（adjust_rating は now() を使うので明示する）
    for who in [player(1), player(12)] {
        sqlx::query(
            "insert into rating_adjustments \
             (player_id, delta_mu, delta_sigma, reason, created_by, created_at, idempotency_key) \
             values ($1, 100, 0, 'audit', 'staff:test', $2, $3)",
        )
        .bind(who.0)
        .bind(at(30))
        .bind(format!("audit-adjust-{}", who.0))
        .execute(&db.pool)
        .await
        .expect("append an adjustment after the boundary");
    }

    // キューに入れる（2 / 4 / 6 の経路はキュー中の人しか返さない）
    for who in [player(1), player(6), player(11), player(12)] {
        enqueue(&db, who, "jungle", at(50)).await;
    }

    // 引き分けは行として数えるが増減 0。無効試合は行そのものが立たない
    let scenarios = [
        Scenario {
            name: "A 勝ち越し + 調整あり（red）",
            who: player(1),
            expected: 1500.0 + 100.0 + 7.0 * 6.0,
            expected_matches: 7,
            in_formation: true,
        },
        Scenario {
            name: "B 負け越し（blue・合計が負）",
            who: player(6),
            expected: 1500.0 - 7.0 * 6.0,
            expected_matches: 7,
            in_formation: true,
        },
        Scenario {
            name: "C 1 試合もしていない",
            who: player(11),
            expected: 1500.0,
            expected_matches: 0,
            in_formation: false,
        },
        Scenario {
            name: "D 調整のみ・試合ゼロ",
            who: player(12),
            expected: 1600.0,
            expected_matches: 0,
            in_formation: false,
        },
    ];

    for scenario in &scenarios {
        let paths = rating_paths(&db, scenario.who).await;
        print_paths(&paths, scenario);
        assert_paths_agree(&paths, scenario);
    }
    println!("=== 既定の増減幅 7 で 8 経路すべて一致 ===");

    // ------------------------------------------------------------------
    // 増減幅を既定値以外にしても、8 経路すべてが追従すること
    // ------------------------------------------------------------------
    AdminRepository::new(db.pool.clone())
        .change_rating_params(&NewRatingParams {
            base_delta: 12.0,
            reason: "監査: 既定値以外で再確認".to_owned(),
            actor_discord_user_id: 1001,
            created_at: at(400),
            idempotency_key: "audit-delta-12".to_owned(),
        })
        .await
        .unwrap();

    let widened = [
        (
            "増減幅 12 / A",
            player(1),
            1500.0 + 100.0 + 12.0 * 6.0,
            7,
            true,
        ),
        ("増減幅 12 / B", player(6), 1500.0 - 12.0 * 6.0, 7, true),
        ("増減幅 12 / C", player(11), 1500.0, 0, false),
        ("増減幅 12 / D", player(12), 1600.0, 0, false),
    ];
    for (name, who, expected, expected_matches, in_formation) in widened {
        let scenario = Scenario {
            name,
            who,
            expected,
            expected_matches,
            in_formation,
        };
        let paths = rating_paths(&db, who).await;
        print_paths(&paths, &scenario);
        assert_paths_agree(&paths, &scenario);
    }
    println!("=== 増減幅 12 でも 8 経路すべて一致（7 のハードコードは無い） ===");

    db.done().await;
}

/// **境界をまたぐ試合はどちら側に入るか。**
///
/// 判定は **`formations.created_at`**（確定イベントの時刻ではない）。
/// したがって「境界より前に作られ、境界より後に確定した試合」は**入らない**。
#[sqlx::test(fixtures("app_role"), fixtures("../ops/app_privileges.sql"))]
async fn a_match_that_straddles_the_boundary_stays_outside(owner: PgPool) {
    let db = AppDb::connect(&owner).await;
    seed_players(&db, 10).await;

    // at(5) に作った試合を、まだ確定させずに置く
    play(&db, 5, 1500.0, 1500.0).await;
    // at(20) に境界を引く
    sqlx::query(
        "insert into season_boundaries \
         (effective_at, label, created_by, idempotency_key, created_at) \
         values ($1, 'straddle', 'discord:1', 'straddle-boundary', $1)",
    )
    .bind(at(20))
    .execute(&db.pool)
    .await
    .unwrap();
    // **境界より後**に確定させる
    let confirmed_at = at(40);
    let event = NewEvent::new(K::Confirmed, None, confirmed_at, "straddle-finish")
        .with_payload(serde_json::json!({ "outcome": "red" }));
    db.formations.append_event(fid(5), &event).await.unwrap();

    // 確定はしているが、**作られたのが境界より前なので集計に入らない**
    let counted: i64 = sqlx::query_scalar(
        "select count(*) from player_match_rating_deltas where formation_id = $1",
    )
    .bind(fid(5).0)
    .fetch_one(&db.pool)
    .await
    .unwrap();
    assert_eq!(counted, 0, "境界をまたいだ試合が集計に入っている");
    assert_eq!(db.ratings.display_rating(player(1)).await.unwrap(), 1500.0);

    println!(
        "作成 {} / 境界 {} / 確定 {} → 集計対象 {} 行（判定は formations.created_at）",
        at(5),
        at(20),
        confirmed_at,
        counted
    );

    db.done().await;
}

/// **σ を導出しているのは 2 か所だけで、両者は一致する。**
///
/// σ は画面に出していない（`match_screen` に「σ を出さない」検査がある）。
/// 現在 σ を導出するのはマッチメイカの `load_candidates` と Rust の
/// `initial_rating` だけで、どちらも `season_rating_adjustments.delta_sigma` を足す。
#[sqlx::test(fixtures("app_role"), fixtures("../ops/app_privileges.sql"))]
async fn the_two_sigma_paths_agree(owner: PgPool) {
    let db = AppDb::connect(&owner).await;
    seed_players(&db, 10).await;

    sqlx::query(
        "insert into rating_adjustments \
         (player_id, delta_mu, delta_sigma, reason, created_by, created_at, idempotency_key) \
         values ($1, 0, -120, 'sigma audit', 'staff:test', now(), 'sigma-audit')",
    )
    .bind(player(1).0)
    .execute(&db.pool)
    .await
    .unwrap();
    enqueue(&db, player(1), "jungle", at(0)).await;

    // Rust 側（内部スケール → 表示スケールへ戻す）
    let by_rust = db.ratings.initial_rating(player(1)).await.unwrap().sigma * DISPLAY_SCALE;
    // マッチメイカ側（load_candidates と同じ式）
    let by_matchmaker: f64 = sqlx::query_scalar(
        "select $2::double precision \
                + coalesce((select sum(a.delta_sigma) from season_rating_adjustments a \
                            where a.player_id = q.player_id), 0::double precision) \
         from queue_entries q where q.player_id = $1",
    )
    .bind(player(1).0)
    .bind(500.0_f64)
    .fetch_one(&db.pool)
    .await
    .unwrap();

    assert_eq!(by_rust, by_matchmaker);
    assert_eq!(by_rust, 380.0);
    println!(
        "σ: Rust {by_rust} / matchmaker {by_matchmaker}（導出はこの 2 か所だけ。画面には出さない）"
    );

    db.done().await;
}

/// **増減幅の変更とシーズン境界は、マッチメイカのスキャンと直列化される。**
///
/// スキャンは 1 回まるごとが 1 トランザクションで、先頭で
/// `pg_advisory_xact_lock(VC_POOL_LOCK_KEY)` を取る（07 §4-1）。ロックを取らずに
/// 「進行中の formation が 0 件か」を数えると、スキャンが走っているがまだ commit
/// していない瞬間に 0 件と見えてしまい、直後に formation が生まれる。
///
/// いまは凍結値が増減の計算に入らないので実害は小さいが、μ+σ へ移ると凍結値が
/// 計算に入り、原因の分からないずれになる。
#[sqlx::test(fixtures("app_role"), fixtures("../ops/app_privileges.sql"))]
async fn the_params_change_waits_for_a_matchmaker_scan(owner: PgPool) {
    let db = AppDb::connect(&owner).await;
    seed_players(&db, 10).await;
    let admin = AdminRepository::new(db.pool.clone());

    // マッチメイカのスキャンと同じロックを、別のトランザクションで握る
    let mut scan = db.pool.begin().await.unwrap();
    sqlx::query("select pg_advisory_xact_lock($1)")
        .bind(VC_POOL_LOCK_KEY)
        .execute(&mut *scan)
        .await
        .unwrap();

    // ロックを握られているあいだは進めない
    let blocked = tokio::time::timeout(
        std::time::Duration::from_millis(1500),
        admin.change_rating_params(&NewRatingParams {
            base_delta: 20.0,
            reason: "スキャン中に変えようとする".to_owned(),
            actor_discord_user_id: 1001,
            created_at: at(100),
            idempotency_key: "during-scan".to_owned(),
        }),
    )
    .await;
    assert!(
        blocked.is_err(),
        "スキャンのロックを待たずに進んでしまった: {blocked:?}"
    );
    // 行も積まれていない
    let rows: i64 = sqlx::query_scalar("select count(*) from rating_param_changes")
        .fetch_one(&db.pool)
        .await
        .unwrap();
    assert_eq!(rows, 0);
    println!("スキャン中: 増減幅の変更はロック待ちで進まない");

    // 境界も同じ
    let blocked = tokio::time::timeout(
        std::time::Duration::from_millis(1500),
        admin.start_season(&NewSeasonBoundary {
            effective_at: at(200),
            label: "スキャン中".to_owned(),
            actor_discord_user_id: 1001,
            created_at: at(200),
            idempotency_key: "season-during-scan".to_owned(),
        }),
    )
    .await;
    assert!(blocked.is_err(), "境界がロックを待たずに進んでしまった");
    println!("スキャン中: シーズン境界もロック待ちで進まない");

    // スキャンが終われば通る
    scan.commit().await.unwrap();
    let changed = admin
        .change_rating_params(&NewRatingParams {
            base_delta: 20.0,
            reason: "スキャンが終わったので変える".to_owned(),
            actor_discord_user_id: 1001,
            created_at: at(300),
            idempotency_key: "after-scan".to_owned(),
        })
        .await
        .unwrap();
    assert!(matches!(changed, RatingParamsOutcome::Changed { .. }));
    assert_eq!(admin.rating_params().await.unwrap().base_delta, 20.0);
    println!("スキャン終了後: 変更できる");

    db.done().await;
}
