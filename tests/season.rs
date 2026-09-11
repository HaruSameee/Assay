//! シーズン境界（導出の起点を追記する）の統合テスト。実際の PostgreSQL を使う。
//!
//! リポジトリは所有者ではなく**アプリ用ロール**（`assay_app`）で接続する。
//!
//! **これは「レートのリセット」ではない。**境界を 1 行追記すると、導出がその時刻から
//! 読み始めるようになるだけで、試合ログは 1 行も消さない。ここではその 2 つを同時に測る。
//!
//! 受け入れ条件（2-4）をそのまま並べてある:
//! 1. 全プレイヤーの表示レートが 1500 になる（調整を投入する前の状態で）
//! 2. 全プレイヤーの試合数が 0、勝率が未定義になる
//! 3. ランキングの掲載条件（確定 5 試合以上）を満たす人が 1 人もいなくなる
//! 4. 未報告の試合を抱えていたプレイヤーがキューに入れる
//! 5. formations / formation_players / formation_events の行数が境界の前後で変わらない
//! 6. 境界の後に +100 の調整を 1 件入れると、その人だけ 1600 になる
//!
//! 加えて、進行中の formation があるときに**拒否される**ことも測る。

mod common;

use common::{APP_ROLE, APP_ROLE_PASSWORD, at, player};
use serde_json::json;
use sqlx::PgPool;
use sqlx::postgres::PgPoolOptions;
use unite_matchsystem::admin::{
    AdminRepository, NewSeasonBoundary, SeasonOutcome, confirmation_matches, normalize_label,
};
use unite_matchsystem::lifecycle::{FormationEventKind as K, FormationId, PlayerId, Timestamp};
use unite_matchsystem::matchmaker::ROLES;
use unite_matchsystem::persistence::{
    FormationRepository, NewEvent, NewFormation, NewFormationPlayer, Team,
};
use unite_matchsystem::queue::QueueRepository;
use unite_matchsystem::queue::model::{QueueRequest, RolePreference};
use unite_matchsystem::rating::RatingAdjustmentRepository;
use unite_matchsystem::registration::{NewDeclaration, Rank, RegistrationRepository};
use unite_matchsystem::status::StatusRepository;
use uuid::Uuid;

/// .env.example に入れている管理者の 1 人目。
const ADMIN_ID: i64 = 900_000_000_000_000_001;

// ----------------------------------------------------------------------
// 道具
// ----------------------------------------------------------------------

/// 境界の時刻。`common::at` の基準（2026-09-12 21:00 UTC）より後に置き、
/// テストデータは `at(..)` 側（＝境界より前）に置く。
fn boundary_at() -> Timestamp {
    at(3_600)
}

/// 境界より後の時刻。
fn after(seconds: i64) -> Timestamp {
    at(7_200 + seconds)
}

fn fid(n: u128) -> FormationId {
    FormationId(Uuid::from_u128(n))
}

struct AppDb {
    pool: PgPool,
    admin: AdminRepository,
    formations: FormationRepository,
    queue: QueueRepository,
    registration: RegistrationRepository,
    ratings: RatingAdjustmentRepository,
    status: StatusRepository,
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
            admin: AdminRepository::new(pool.clone()),
            formations: FormationRepository::new(pool.clone()),
            queue: QueueRepository::new(pool.clone()),
            registration: RegistrationRepository::new(pool.clone()),
            ratings: RatingAdjustmentRepository::new(pool.clone()),
            status: StatusRepository::new(pool.clone()),
            pool,
        }
    }

    async fn done(self) {
        self.pool.close().await;
    }
}

/// `players` の行を用意する（discord_user_id は 1000 + n）。
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

/// 登録を通す（キューのゲートは登録済みを要求する）。
async fn register(db: &AppDb, player_id: PlayerId) {
    db.registration
        .register(&NewDeclaration {
            player_id,
            declared_at: at(0),
            trainer_name: format!("トレーナー{}", player_id.0.as_u128()),
            rank: Rank::Legend,
            rating: Some(1500),
            raw: json!({ "rank": "legend" }),
        })
        .await
        .expect("record the declaration");
}

/// 表示スケールのレートを `rating_adjustments` への追記で作る。
async fn adjust(db: &AppDb, who: PlayerId, delta_mu: f64, created_at: Timestamp, key: &str) {
    sqlx::query(
        "insert into rating_adjustments \
         (player_id, delta_mu, delta_sigma, reason, created_by, created_at, idempotency_key) \
         values ($1, $2, 0, 'test seed', 'staff:test', $3, $4)",
    )
    .bind(who.0)
    .bind(delta_mu)
    .bind(created_at)
    .bind(format!("{key}-{}", who.0))
    .execute(&db.pool)
    .await
    .expect("append a rating adjustment");
}

/// players 1〜5 が red、6〜10 が blue。全員 1500。
fn formation_at(n: u128, created_at: Timestamp) -> NewFormation {
    NewFormation {
        id: fid(n),
        created_at,
        matchmaking_params_version: 3,
        predicted_win_prob: 0.5,
        players: (1..=10u128)
            .map(|i| NewFormationPlayer {
                player_id: player(i),
                team: if i <= 5 { Team::Red } else { Team::Blue },
                role: ROLES[(i as usize - 1) % ROLES.len()].to_owned(),
                role_was_flexed: false,
                streaming: false,
                mu_at_match: 25.0,
                sigma_at_match: 500.0 / 60.0,
            })
            .collect(),
    }
}

/// formation を作って `in_progress` まで進める（まだ確定していない）。
async fn play(db: &AppDb, n: u128, created_at: Timestamp) {
    db.formations
        .create_formation(&formation_at(n, created_at))
        .await
        .expect("create formation");
    let host = player(1);
    for event in [
        NewEvent::vc_assigned("1", 101, 102, created_at, format!("vc-{n}")),
        NewEvent::new(K::HostAssigned, Some(host), created_at, format!("as-{n}")),
        NewEvent::new(K::HostAccepted, Some(host), created_at, format!("ac-{n}")),
        NewEvent::code_posted(host, "1234", created_at, format!("code-{n}")),
    ] {
        db.formations.append_event(fid(n), &event).await.unwrap();
    }
}

/// 終端イベントを追記する。
async fn finish(db: &AppDb, n: u128, kind: K, outcome: &str, occurred_at: Timestamp) {
    let event = NewEvent::new(
        kind,
        None,
        occurred_at,
        format!("fin-{n}-{}", kind.as_str()),
    )
    .with_payload(json!({ "outcome": outcome }));
    db.formations.append_event(fid(n), &event).await.unwrap();
}

async fn confirmed_match(db: &AppDb, n: u128, created_at: Timestamp) {
    play(db, n, created_at).await;
    finish(db, n, K::Confirmed, "red", created_at).await;
}

/// 行数を数える。`table` はこのファイル内のリテラルだけなので組み立ててよい。
async fn count(db: &AppDb, table: &'static str) -> i64 {
    let sql = format!("select count(*)::bigint from {table}");
    sqlx::query_scalar(sqlx::AssertSqlSafe(sql))
        .fetch_one(&db.pool)
        .await
        .expect("count rows")
}

async fn display_rating(db: &AppDb, who: PlayerId) -> f64 {
    db.ratings
        .display_rating(who)
        .await
        .expect("derive the display rating")
}

/// ランキングの掲載条件（確定 5 試合以上）を満たす人数。
/// `/ranking` はこの数え方をそのまま使う（別の集計表やキャッシュは作らない）。
async fn qualified_for_ranking(db: &AppDb) -> i64 {
    sqlx::query_scalar(
        "select count(*)::bigint from (select player_id from player_match_rating_deltas \
         group by player_id having count(*) >= 5) q",
    )
    .fetch_one(&db.pool)
    .await
    .expect("count the players who qualify")
}

/// 境界を 1 本追記する（管理パネルと同じ経路）。
async fn start_season(db: &AppDb, at_time: Timestamp, label: &str, key: &str) -> SeasonOutcome {
    db.admin
        .start_season(&NewSeasonBoundary {
            effective_at: at_time,
            label: label.to_owned(),
            actor_discord_user_id: ADMIN_ID,
            created_at: at_time,
            idempotency_key: key.to_owned(),
        })
        .await
        .expect("append the boundary")
}

// ----------------------------------------------------------------------
// 受け入れ条件 2-4
// ----------------------------------------------------------------------

/// テストデータが入った状態に境界を 1 本入れ、6 項目すべてを測る。
#[sqlx::test(fixtures("app_role"), fixtures("../ops/app_privileges.sql"))]
async fn a_boundary_moves_the_origin_without_deleting_anything(owner: PgPool) {
    let db = AppDb::connect(&owner).await;
    seed_players(&db, 10).await;
    for n in 1..=10u128 {
        register(&db, player(n)).await;
    }

    // 「今夜のテストデータ」相当: 確定した試合 6 件（掲載条件の 5 を超える）と、
    // 報告しないまま残った試合 1 件、それに運営の調整 2 件
    for n in 1..=6u128 {
        confirmed_match(&db, n, at(n as i64)).await;
    }
    play(&db, 7, at(100)).await;
    adjust(&db, player(1), 25.0, at(0), "seed-a").await;
    adjust(&db, player(6), 1.0, at(0), "seed-b").await;

    // 境界の前の状態を控える
    let before = (
        count(&db, "formations").await,
        count(&db, "formation_players").await,
        count(&db, "formation_events").await,
        count(&db, "match_reports").await,
        count(&db, "rating_adjustments").await,
    );
    assert_eq!(before.0, 7, "formation は 7 件");
    assert_eq!(display_rating(&db, player(1)).await, 1567.0, "1500+25+7*6");
    assert_eq!(display_rating(&db, player(6)).await, 1459.0, "1500+1-7*6");
    let summary = db.status.summary(player(1)).await.unwrap();
    assert_eq!((summary.matches, summary.wins, summary.losses), (6, 6, 0));
    assert_eq!(qualified_for_ranking(&db).await, 10, "全員が 6 試合");

    // 未報告の試合（formation 7）がキューを塞いでいる
    let gate = db.registration.queue_eligibility(player(1)).await.unwrap();
    assert!(!gate.is_allowed(), "報告ゲートで止まっている: {gate:?}");
    assert!(gate.is_report_blocked());

    // ------------------------------------------------------------------
    // 進行中があるうちは実行できない（件数を出して止める）
    // ------------------------------------------------------------------
    let blocked = start_season(&db, boundary_at(), "2026 開幕", "season-1").await;
    assert_eq!(
        blocked,
        SeasonOutcome::Blocked {
            active_formations: 1
        },
        "in_progress の formation が 1 件あるので拒否する"
    );
    assert_eq!(count(&db, "season_boundaries").await, 0, "何も書かない");
    assert_eq!(count(&db, "admin_actions").await, 0, "操作ログも書かない");

    // 進行中を畳む（当日の運用でも、試合が終わってから境界を入れる）
    finish(&db, 7, K::Voided, "voided", at(200)).await;

    // ------------------------------------------------------------------
    // 境界を 1 本入れる
    // ------------------------------------------------------------------
    let started = start_season(&db, boundary_at(), "2026 開幕", "season-1").await;
    assert!(
        matches!(started, SeasonOutcome::Started { .. }),
        "{started:?}"
    );
    // 同一キーの再送は行を増やさない（冪等は状態ベースにしない）
    assert_eq!(
        start_season(&db, boundary_at(), "2026 開幕", "season-1").await,
        SeasonOutcome::Duplicate
    );
    assert_eq!(count(&db, "season_boundaries").await, 1);
    assert_eq!(count(&db, "admin_actions").await, 1, "誰がいつやったか残る");

    // 1. 全プレイヤーの表示レートが 1500
    for n in 1..=10u128 {
        assert_eq!(
            display_rating(&db, player(n)).await,
            1500.0,
            "player {n} の表示レート"
        );
    }

    // 2. 試合数 0、勝率は未定義（分母 0）
    for n in 1..=10u128 {
        let summary = db.status.summary(player(n)).await.unwrap();
        assert_eq!(
            (summary.matches, summary.wins, summary.losses),
            (0, 0, 0),
            "player {n} の総合成績"
        );
    }
    assert!(
        db.status.history(player(1), 50).await.unwrap().is_empty(),
        "履歴も空になる"
    );

    // 3. ランキングの掲載条件を満たす人が 1 人もいない
    assert_eq!(qualified_for_ranking(&db).await, 0);

    // 4. 未報告の試合を抱えていた人がキューに入れる
    let gate = db.registration.queue_eligibility(player(1)).await.unwrap();
    assert!(
        gate.is_allowed(),
        "境界より前の未報告では止まらない: {gate:?}"
    );
    assert!(gate.pending_reports.is_empty());
    assert!(gate.active_formation.is_none());
    db.queue
        .enqueue(
            player(1),
            &QueueRequest {
                roles: vec![RolePreference {
                    role: "jungle".to_owned(),
                    priority: 1,
                }],
                streaming: false,
            },
            after(0),
        )
        .await
        .expect("実際にキューへ入れる");

    // 5. 試合ログの行数は 1 つも変わっていない
    //    （voided を 1 件追記したぶんだけ formation_events が増えるが、
    //     境界の追記そのものでは 1 行も増減しない）
    let after_counts = (
        count(&db, "formations").await,
        count(&db, "formation_players").await,
        count(&db, "formation_events").await - 1,
        count(&db, "match_reports").await,
        count(&db, "rating_adjustments").await,
    );
    assert_eq!(after_counts, before, "ログは 1 行も消えていない");

    // 6. 境界の後に +100 を 1 件入れると、その人だけ 1600
    adjust(&db, player(2), 100.0, after(60), "seeded-1600").await;
    assert_eq!(display_rating(&db, player(2)).await, 1600.0);
    for n in [1u128, 3, 6, 10] {
        assert_eq!(display_rating(&db, player(n)).await, 1500.0, "player {n}");
    }

    db.done().await;
}

/// 境界の後に**作られた**試合だけが数え直しの対象になる。
/// 境界より前に作られた formation は、確定が境界より後でも入らない
/// （`formations.created_at` で切っているので、後から確定させて滑り込ませられない）。
#[sqlx::test(fixtures("app_role"), fixtures("../ops/app_privileges.sql"))]
async fn only_matches_created_after_the_boundary_are_counted(owner: PgPool) {
    let db = AppDb::connect(&owner).await;
    seed_players(&db, 10).await;

    // 境界より前に作り、まだ確定していない試合。境界は進行中を拒否するので先に畳む
    play(&db, 1, at(0)).await;
    finish(&db, 1, K::Disputed, "disputed", at(10)).await;
    start_season(&db, boundary_at(), "2026 開幕", "season-1").await;

    // 境界の後にスタッフ裁定で確定させる
    finish(&db, 1, K::Confirmed, "red", after(0)).await;
    assert_eq!(
        display_rating(&db, player(1)).await,
        1500.0,
        "境界より前に作られた試合は、確定が後でも入らない"
    );
    assert_eq!(db.status.summary(player(1)).await.unwrap().matches, 0);

    // 境界の後に作った試合はそのまま入る
    confirmed_match(&db, 2, after(10)).await;
    assert_eq!(display_rating(&db, player(1)).await, 1507.0);
    assert_eq!(db.status.summary(player(1)).await.unwrap().matches, 1);

    db.done().await;
}

/// 境界より前の調整を境界より後で打ち消しても、逆符号だけが効いたりしない。
#[sqlx::test(fixtures("app_role"), fixtures("../ops/app_privileges.sql"))]
async fn a_reversal_of_an_out_of_season_adjustment_does_not_leak_in(owner: PgPool) {
    let db = AppDb::connect(&owner).await;
    seed_players(&db, 1).await;
    adjust(&db, player(1), 100.0, at(0), "old").await;
    assert_eq!(display_rating(&db, player(1)).await, 1600.0);

    start_season(&db, boundary_at(), "2026 開幕", "season-1").await;
    assert_eq!(display_rating(&db, player(1)).await, 1500.0, "境界で外れる");

    // 境界の後に、境界より前の調整を打ち消す行を追記する
    let original_id: i64 = sqlx::query_scalar("select id from rating_adjustments limit 1")
        .fetch_one(&db.pool)
        .await
        .unwrap();
    sqlx::query(
        "insert into rating_adjustments (player_id, delta_mu, delta_sigma, reason, created_by, \
         created_at, idempotency_key, reverts_id) \
         values ($1, -100, 0, 'revert', 'staff:test', $2, 'revert-1', $3)",
    )
    .bind(player(1).0)
    .bind(after(0))
    .bind(original_id)
    .execute(&db.pool)
    .await
    .expect("append the reversal");

    assert_eq!(
        display_rating(&db, player(1)).await,
        1500.0,
        "打ち消しだけが効いて 1400 にはならない"
    );
    db.done().await;
}

/// 境界そのものも追記ログ。**アプリ用ロールからは書き換えも削除もできない。**
/// 取り消しは「より前の境界をもう 1 本追記する」であって、行を消すことではない。
#[sqlx::test(fixtures("app_role"), fixtures("../ops/app_privileges.sql"))]
async fn the_boundary_is_itself_append_only(owner: PgPool) {
    let db = AppDb::connect(&owner).await;
    seed_players(&db, 1).await;
    start_season(&db, boundary_at(), "2026 開幕", "season-1").await;

    for sql in [
        "update season_boundaries set label = 'x'",
        "delete from season_boundaries",
    ] {
        let error = sqlx::query(sql).execute(&db.pool).await.unwrap_err();
        assert_eq!(
            error.as_database_error().and_then(|e| e.code()).as_deref(),
            Some("42501"),
            "{sql}"
        );
    }

    // より前の境界を追記すると、最新行が正なので起点が戻る
    let earlier = at(-86_400);
    start_season(&db, earlier, "取り消し", "season-2").await;
    let current = db.admin.current_season().await.unwrap().expect("boundary");
    assert_eq!(current.effective_at, earlier, "最新行が正");
    assert_eq!(count(&db, "season_boundaries").await, 2, "2 行とも残る");
    db.done().await;
}

/// 2 段目はラベルの打ち直しを要求する（クリック 2 回では通らない）。
#[test]
fn the_confirmation_requires_the_label_to_be_typed_again() {
    let label = normalize_label(" 2026 開幕 ").expect("valid");
    assert_eq!(label, "2026 開幕");
    assert!(confirmation_matches(&label, "2026 開幕"));
    assert!(!confirmation_matches(&label, "2026開幕"));
    assert!(!confirmation_matches(&label, ""));
}

/// 本番と同じ形に組み立てたローカル DB へ、**アプリの経路で**境界を 1 本入れる。
///
/// 受け入れ条件の実測に使う。`SEASON_PROBE_URL` を指定したときだけ動く:
/// `SEASON_PROBE_URL=postgres://... cargo test --test season -- --ignored probe`
#[tokio::test]
#[ignore = "実測用。SEASON_PROBE_URL を指定したときだけ回す"]
async fn probe_start_season_against_a_production_shaped_database() {
    let Ok(url) = std::env::var("SEASON_PROBE_URL") else {
        panic!("SEASON_PROBE_URL を指定すること");
    };
    let pool = PgPoolOptions::new()
        .max_connections(4)
        .connect(&url)
        .await
        .expect("connect to the probe database");
    let admin = AdminRepository::new(pool.clone());

    let effective_at = chrono::Utc::now();
    let preview = admin
        .season_preview(effective_at)
        .await
        .expect("count the preview");
    println!("season_preview = {preview:#?}");

    let outcome = admin
        .start_season(&NewSeasonBoundary {
            effective_at,
            label: "2026 開幕".to_owned(),
            actor_discord_user_id: ADMIN_ID,
            created_at: effective_at,
            idempotency_key: "probe-season-1".to_owned(),
        })
        .await
        .expect("append the boundary");
    println!("start_season = {outcome:?}");

    // 同じキーの再送は行を増やさない
    let again = admin
        .start_season(&NewSeasonBoundary {
            effective_at,
            label: "2026 開幕".to_owned(),
            actor_discord_user_id: ADMIN_ID,
            created_at: effective_at,
            idempotency_key: "probe-season-1".to_owned(),
        })
        .await
        .expect("append the boundary again");
    println!("start_season (retry) = {again:?}");
    println!(
        "current_season = {:?}",
        admin.current_season().await.unwrap()
    );
    pool.close().await;
}
