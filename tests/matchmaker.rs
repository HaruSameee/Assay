//! マッチメイカの統合テスト。実際の PostgreSQL を使う。
//!
//! リポジトリは所有者ではなく**アプリ用ロール**（`assay_app`）で接続する。所有者権限で
//! 走らせると、権限の配線ミスが本番でしか出ない。
//!
//! 検証すること:
//! - `ROLES` の定数が `roles` テーブルと一致する（片方だけ変わる事故を防ぐ）
//! - 1 回の実行で formation 確定・`vc_assigned`・キューからの除去が揃うこと
//! - **VC が枯渇しているとき formation を作らない**（00 §6）。キューはそのまま残る
//! - **formation 確定とキュー除去が同一トランザクション**であること。途中で落ちれば
//!   どちらも起きない
//! - 1 回の実行で複数の formation が成立すること（07 §4-2）
//! - フレックス補償が `formation_players` から導出されていること（07 §5）

use chrono::{TimeDelta, TimeZone, Utc};
use sqlx::PgPool;
use sqlx::postgres::PgPoolOptions;
use unite_matchsystem::lifecycle::{
    FormationEventKind as K, FormationId, FormationState, PlayerId, Timestamp,
};
use unite_matchsystem::matchmaker::{
    Matchmaker, MatchmakerConfig, MatchmakerRepository, ROLES, RunOutcome,
};
use unite_matchsystem::persistence::{
    FormationRepository, NewEvent, NewFormation, NewFormationPlayer, Team, VcAssignment,
};
use unite_matchsystem::queue::QueueRepository;
use unite_matchsystem::queue::model::{QueueRequest, RolePreference};
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
    sqlx::query(
        "insert into rating_adjustments \
         (player_id, delta_mu, delta_sigma, reason, created_by, created_at, idempotency_key) \
         values ($1, $2, 0, 'test seed', 'staff:test', now(), $3)",
    )
    .bind(who.0)
    .bind(delta_mu)
    .bind(format!("seed-{}", who.0))
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
                    down_range: 95,
                    roles: vec![RolePreference {
                        role: role.to_owned(),
                        priority: 1,
                    }],
                },
                enqueued_at,
            )
            .await
            .expect("enqueue");
    }
}

async fn count(db: &AppDb, table: &str) -> i64 {
    let sql = format!("select count(*) from {table}");
    sqlx::query_scalar(sqlx::AssertSqlSafe(sql))
        .fetch_one(&db.pool)
        .await
        .expect("count rows")
}

/// 未終了の formation を 1 つ作り、`vc1` を占有させる。
async fn occupy_vc(db: &AppDb, formation: u128, label: &str) {
    let players: Vec<NewFormationPlayer> = (1..=10u128)
        .map(|n| NewFormationPlayer {
            player_id: player(n),
            team: if n <= 5 { Team::Red } else { Team::Blue },
            role: ROLES[(n as usize - 1) % ROLES.len()].to_owned(),
            role_was_flexed: false,
            mu_at_match: 25.0,
            sigma_at_match: 8.33,
        })
        .collect();
    db.formations
        .create_formation(&NewFormation {
            id: fid(formation),
            created_at: at(-600),
            matchmaking_params_version: 1,
            predicted_win_prob: 0.5,
            players,
        })
        .await
        .expect("create the blocking formation");
    db.formations
        .append_event(
            fid(formation),
            &NewEvent::vc_assigned(label, 1, 2, at(-600), format!("vc_assigned:{label}")),
        )
        .await
        .expect("occupy the pair");
}

// ----------------------------------------------------------------------
// 定数とスキーマの整合
// ----------------------------------------------------------------------

/// `ROLES` は `roles` テーブルと同じ 5 つでなければならない。
///
/// マッチメイカは「各チームに 5 ロールが 1 人ずつ」を前提に枠を作るので、片方だけ変わると
/// 編成がまったく作れなくなる。
#[sqlx::test(fixtures("app_role"), fixtures("../ops/app_privileges.sql"))]
async fn the_role_constants_match_the_roles_table(owner: PgPool) {
    let db = AppDb::connect(&owner).await;
    let mut stored: Vec<String> = sqlx::query_scalar("select id from roles order by sort_order")
        .fetch_all(&db.pool)
        .await
        .unwrap();
    let mut expected: Vec<String> = ROLES.iter().map(|role| (*role).to_owned()).collect();
    stored.sort();
    expected.sort();
    assert_eq!(stored, expected);
    db.done().await;
}

// ----------------------------------------------------------------------
// 1 回の実行
// ----------------------------------------------------------------------

/// formation 確定・`vc_assigned`・キューからの除去が 1 回の実行で揃う。
#[sqlx::test(fixtures("app_role"), fixtures("../ops/app_privileges.sql"))]
async fn a_run_forms_a_match_assigns_a_vc_and_clears_the_queue(owner: PgPool) {
    let db = AppDb::connect(&owner).await;
    seed_players(&db, 10).await;
    enqueue_full_rosters(&db, 10, at(0)).await;

    let outcome = db
        .matchmaker(2)
        .run_once(at(600))
        .await
        .expect("the run succeeds");
    assert_eq!(outcome.candidates, 10);
    assert_eq!(outcome.created.len(), 1);
    assert!(!outcome.vc_exhausted);

    let formation = outcome.created[0];
    assert_eq!(count(&db, "formations").await, 1);
    assert_eq!(count(&db, "formation_players").await, 10);
    assert_eq!(
        count(&db, "queue_entries").await,
        0,
        "the ten left the queue"
    );
    assert_eq!(count(&db, "queue_entry_roles").await, 0, "roles cascade");

    // VC は確定と同じトランザクションで記録されている
    let log = db.formations.load_events(formation).await.unwrap();
    let kinds: Vec<K> = log.events.iter().map(|stored| stored.event.kind).collect();
    assert_eq!(kinds, vec![K::VcAssigned]);
    // 状態は awaiting_host。既存の resume_unfinished が割り当て表とホスト打診を出す
    assert!(
        matches!(
            log.snapshot().state,
            FormationState::AwaitingHost { pending: None, .. }
        ),
        "{}",
        log.snapshot().state
    );

    // 凍結値は内部スケール（表示 1500 / 60 = 25）
    let mu: f64 = sqlx::query_scalar("select mu_at_match from formation_players limit 1")
        .fetch_one(&db.pool)
        .await
        .unwrap();
    assert!((mu - 25.0).abs() < 1e-9, "mu = {mu}");

    db.done().await;
}

/// 10 人揃わなければ何も起きない。
#[sqlx::test(fixtures("app_role"), fixtures("../ops/app_privileges.sql"))]
async fn a_run_with_too_few_players_changes_nothing(owner: PgPool) {
    let db = AppDb::connect(&owner).await;
    seed_players(&db, 9).await;
    enqueue_full_rosters(&db, 9, at(0)).await;

    let outcome = db.matchmaker(2).run_once(at(600)).await.unwrap();
    assert_eq!(
        outcome,
        RunOutcome {
            candidates: 9,
            created: Vec::new(),
            assignments: Vec::new(),
            vc_exhausted: false,
            unmatched: 9,
            longest_unmatched_wait_seconds: Some(600),
        }
    );
    assert_eq!(count(&db, "formations").await, 0);
    assert_eq!(
        count(&db, "queue_entries").await,
        9,
        "nobody left the queue"
    );
    db.done().await;
}

#[sqlx::test(
    fixtures("reference", "app_role"),
    fixtures("../ops/app_privileges.sql")
)]
async fn summary_uses_the_persisted_queue_and_last_fifteen_minutes(owner: PgPool) {
    let db = AppDb::connect(&owner).await;
    seed_players(&db, 12).await;
    enqueue_full_rosters(&db, 10, at(0)).await;
    let matchmaker = db.matchmaker(2);
    matchmaker.run_once(at(600)).await.unwrap();

    for (number, role, enqueued_at) in [(11, ROLES[0], at(300)), (12, ROLES[1], at(540))] {
        db.queue
            .enqueue(
                player(number),
                &QueueRequest {
                    down_range: 95,
                    roles: vec![RolePreference {
                        role: role.to_owned(),
                        priority: 1,
                    }],
                },
                enqueued_at,
            )
            .await
            .unwrap();
    }
    let summary = matchmaker.summary(at(600)).await.unwrap();
    assert_eq!(summary.queued, 2);
    assert_eq!(summary.formed_last_15_minutes, 1);
    assert_eq!(summary.median_wait_seconds, Some(180));
    assert_eq!(summary.longest_wait_seconds, Some(300));
    db.done().await;
}

/// 1 回の実行で複数の formation が成立する（07 §4-2）。
#[sqlx::test(fixtures("app_role"), fixtures("../ops/app_privileges.sql"))]
async fn twenty_queued_players_form_two_matches_in_one_run(owner: PgPool) {
    let db = AppDb::connect(&owner).await;
    seed_players(&db, 20).await;
    enqueue_full_rosters(&db, 20, at(0)).await;

    let outcome = db.matchmaker(3).run_once(at(600)).await.unwrap();
    assert_eq!(outcome.created.len(), 2, "{outcome:?}");
    assert!(!outcome.vc_exhausted);
    assert_eq!(count(&db, "formations").await, 2);
    assert_eq!(count(&db, "formation_players").await, 20);
    assert_eq!(count(&db, "queue_entries").await, 0);

    // 別々の VC の組が割り当てられている
    let labels: Vec<String> = sqlx::query_scalar(
        "select payload->'vc'->>'label' from formation_events where kind = 'vc_assigned' order by 1",
    )
    .fetch_all(&db.pool)
    .await
    .unwrap();
    assert_eq!(labels.len(), 2);
    assert_ne!(labels[0], labels[1], "two matches never share a pair");
    db.done().await;
}

// ----------------------------------------------------------------------
// VC の空き（00 §6）
// ----------------------------------------------------------------------

/// **座席が無いのに formation を作らない。**キューはそのまま残る。
#[sqlx::test(fixtures("app_role"), fixtures("../ops/app_privileges.sql"))]
async fn an_exhausted_vc_pool_leaves_the_queue_untouched(owner: PgPool) {
    let db = AppDb::connect(&owner).await;
    seed_players(&db, 10).await;
    // プールは 1 組だけ。それを未終了の formation が押さえている
    occupy_vc(&db, 0xf1, "1").await;
    enqueue_full_rosters(&db, 10, at(0)).await;

    let outcome = db.matchmaker(1).run_once(at(600)).await.unwrap();
    assert!(outcome.vc_exhausted, "{outcome:?}");
    assert!(outcome.created.is_empty());
    assert_eq!(
        count(&db, "formations").await,
        1,
        "only the pre-existing formation"
    );
    assert_eq!(
        count(&db, "queue_entries").await,
        10,
        "the ten keep their place in the queue"
    );
    db.done().await;
}

/// プールが 1 組でキューが 20 人なら、1 組だけ作って残りは次の実行に回す。
#[sqlx::test(fixtures("app_role"), fixtures("../ops/app_privileges.sql"))]
async fn a_partially_exhausted_pool_forms_what_it_can(owner: PgPool) {
    let db = AppDb::connect(&owner).await;
    seed_players(&db, 20).await;
    enqueue_full_rosters(&db, 20, at(0)).await;

    let outcome = db.matchmaker(1).run_once(at(600)).await.unwrap();
    assert_eq!(outcome.created.len(), 1);
    assert!(outcome.vc_exhausted, "the second match had no seat");
    assert_eq!(count(&db, "formations").await, 1);
    assert_eq!(
        count(&db, "queue_entries").await,
        10,
        "the players of the second match stay queued"
    );
    db.done().await;
}

// ----------------------------------------------------------------------
// 同一トランザクション
// ----------------------------------------------------------------------

/// 確定の途中で落ちたら、**formation もキューの除去もどちらも起きない**。
///
/// 2 つ目の formation の採番をわざと 1 つ目と同じにして、主キー違反を起こす。
/// これが別々のトランザクションなら「1 つ目だけ成立してキューから 10 人消えた」状態が残る。
#[sqlx::test(fixtures("app_role"), fixtures("../ops/app_privileges.sql"))]
async fn a_failure_midway_rolls_back_the_whole_run(owner: PgPool) {
    let db = AppDb::connect(&owner).await;
    seed_players(&db, 20).await;
    enqueue_full_rosters(&db, 20, at(0)).await;

    let duplicate = fid(0xdead);
    let error = db
        .matchmaker(3)
        .run_once_with(at(600), &mut || duplicate)
        .await
        .expect_err("the second insert must violate the primary key");
    assert!(format!("{error}").contains("database"), "{error}");

    assert_eq!(
        count(&db, "formations").await,
        0,
        "the first formation must not survive the failed run"
    );
    assert_eq!(count(&db, "formation_players").await, 0);
    assert_eq!(count(&db, "formation_events").await, 0);
    assert_eq!(
        count(&db, "queue_entries").await,
        20,
        "nobody may vanish from the queue without a formation"
    );
    db.done().await;
}

// ----------------------------------------------------------------------
// フレックス補償（07 §5）
// ----------------------------------------------------------------------

/// 補償の対象かどうかは `formation_players.role_was_flexed` から導出する。
///
/// jungle を 3 人で争わせ、直近のマッチでフレックスされた 1 人が第 1 希望を取ることを見る。
#[sqlx::test(fixtures("app_role"), fixtures("../ops/app_privileges.sql"))]
async fn flex_compensation_is_derived_from_the_last_formation(owner: PgPool) {
    let db = AppDb::connect(&owner).await;
    seed_players(&db, 10).await;

    // 3 番が「前回フレックスされた」履歴を持つ。formation は終端まで進めて VC を解放しておく
    let compensated = player(3);
    let history_players: Vec<NewFormationPlayer> = (1..=10u128)
        .map(|n| NewFormationPlayer {
            player_id: player(n),
            team: if n <= 5 { Team::Red } else { Team::Blue },
            role: ROLES[(n as usize - 1) % ROLES.len()].to_owned(),
            role_was_flexed: player(n) == compensated,
            mu_at_match: 25.0,
            sigma_at_match: 8.33,
        })
        .collect();
    db.formations
        .create_formation(&NewFormation {
            id: fid(0xf0),
            created_at: at(-3600),
            matchmaking_params_version: 1,
            predicted_win_prob: 0.5,
            players: history_players,
        })
        .await
        .unwrap();
    db.formations
        .append_event(
            fid(0xf0),
            &NewEvent::abandoned(
                unite_matchsystem::lifecycle::AbandonReason::NotEnoughPlayers,
                at(-3500),
                "abandoned:history",
            ),
        )
        .await
        .unwrap();

    // 1 / 2 / 3 番が jungle を争う。あぶれた 1 人は bot_tank へ
    let wishes: [(u128, &[&str]); 10] = [
        (1, &["jungle", "bot_tank"]),
        (2, &["jungle", "bot_tank"]),
        (3, &["jungle", "bot_tank"]),
        (4, &["bot_tank"]),
        (5, &["top_carry"]),
        (6, &["top_carry"]),
        (7, &["top_support"]),
        (8, &["top_support"]),
        (9, &["bot_carry"]),
        (10, &["bot_carry"]),
    ];
    for (n, roles) in wishes {
        db.queue
            .enqueue(
                player(n),
                &QueueRequest {
                    down_range: 95,
                    roles: roles
                        .iter()
                        .enumerate()
                        .map(|(index, role)| RolePreference {
                            role: (*role).to_owned(),
                            priority: index as i16 + 1,
                        })
                        .collect(),
                },
                at(0),
            )
            .await
            .expect("enqueue");
    }

    let outcome = db.matchmaker(2).run_once(at(600)).await.unwrap();
    assert_eq!(outcome.created.len(), 1);

    let role: String = sqlx::query_scalar(
        "select role from formation_players where formation_id = $1 and player_id = $2",
    )
    .bind(outcome.created[0].0)
    .bind(compensated.0)
    .fetch_one(&db.pool)
    .await
    .unwrap();
    assert_eq!(
        role, "jungle",
        "the player flexed in the previous formation gets the first choice"
    );
    db.done().await;
}

// ----------------------------------------------------------------------
// レート範囲（07 §3-1）を DB 経由で
// ----------------------------------------------------------------------

/// レートは `rating_adjustments` から導出される。差 100 のプールは混ざらない。
#[sqlx::test(fixtures("app_role"), fixtures("../ops/app_privileges.sql"))]
async fn the_two_starting_pools_do_not_mix_through_the_database(owner: PgPool) {
    let db = AppDb::connect(&owner).await;
    seed_players(&db, 10).await;
    for n in 1..=5u128 {
        adjust_rating(&db, player(n), 100.0).await;
    }
    enqueue_full_rosters(&db, 10, at(0)).await;

    let outcome = db.matchmaker(2).run_once(at(600)).await.unwrap();
    assert!(
        outcome.created.is_empty(),
        "1500 and 1600 are 100 apart: {outcome:?}"
    );
    assert_eq!(count(&db, "queue_entries").await, 10);

    // 1 勝ぶん（+11）で 1511 になれば 1600 と組める
    for n in 6..=10u128 {
        adjust_rating(&db, player(n), 11.0).await;
    }
    let outcome = db.matchmaker(2).run_once(at(600)).await.unwrap();
    assert_eq!(outcome.created.len(), 1, "1511 and 1600 are 89 apart");
    assert_eq!(count(&db, "queue_entries").await, 0);
    db.done().await;
}
