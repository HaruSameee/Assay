//! 永続化層の統合テスト。実際の PostgreSQL が要る。
//!
//! 実行方法: `DATABASE_URL` を設定して `cargo test --test persistence`。
//! `#[sqlx::test]` がテストごとに専用のデータベースを作って `migrations/` と fixtures を適用する。
//! `DATABASE_URL` のロールには CREATE DATABASE と CREATE ROLE の権限が要る
//! （fixture がアプリ用ロールを作る）。
//!
//! リポジトリは所有者ではなく**アプリ用ロール**（追記専用4テーブルの UPDATE / DELETE を持たない、
//! `tests/fixtures/app_role.sql`）で接続する。所有者権限では権限バグが見えないため。

use chrono::{TimeDelta, TimeZone, Utc};
use serde_json::json;
use sqlx::PgPool;
use sqlx::postgres::PgPoolOptions;
use tokio::task::JoinSet;
use unite_matchsystem::lifecycle::{
    AbandonReason, AnomalyCause, EventPayload, FormationEventKind as K, FormationId,
    FormationState as S, LifecycleConfig, PlayerId, Timestamp, TransitionError, next_deadline,
};
use unite_matchsystem::persistence::{
    FormationRepository, NewEvent, NewFormation, NewFormationPlayer, NewReport, Outcome,
    RepositoryError, Team,
};
use uuid::Uuid;

const APP_ROLE: &str = "assay_app";
const APP_ROLE_PASSWORD: &str = "assay_app_test";

/// マイグレーションが入れる 5 ロール（各チームに 1 人ずつ）。
const ROLES: [&str; 5] = [
    "top_carry",
    "top_support",
    "jungle",
    "bot_carry",
    "bot_tank",
];

/// テスト DB にアプリ用ロールで接続したプールと、それを使うリポジトリ。
struct AppDb {
    pool: PgPool,
    repo: FormationRepository,
}

impl AppDb {
    /// `#[sqlx::test]` がくれる所有者プールの接続先に、アプリ用ロールで繋ぎ直す。
    async fn connect(owner: &PgPool) -> Self {
        let options = (*owner.connect_options())
            .clone()
            .username(APP_ROLE)
            .password(APP_ROLE_PASSWORD);
        let pool = PgPoolOptions::new()
            .max_connections(10)
            .connect_with(options)
            .await
            .expect("connect as the app role");
        let repo = FormationRepository::new(pool.clone());
        Self { pool, repo }
    }

    /// テスト DB を後片付けできるよう接続を閉じる。
    async fn done(self) {
        self.pool.close().await;
    }
}

fn pid(n: u128) -> PlayerId {
    PlayerId(Uuid::from_u128(n))
}

fn fid(n: u128) -> FormationId {
    FormationId(Uuid::from_u128(0xf000 + n))
}

fn at(seq: i64) -> Timestamp {
    Utc.with_ymd_and_hms(2026, 9, 12, 21, 0, 0).unwrap() + TimeDelta::seconds(seq)
}

/// players 1〜10 の 5v5。`n` で作成時刻をずらす。
fn formation(n: u128) -> NewFormation {
    let roles = ROLES;
    NewFormation {
        id: fid(n),
        created_at: at(n as i64 * 60),
        matchmaking_params_version: 1,
        predicted_win_prob: 0.5,
        players: (1..=10)
            .map(|i| NewFormationPlayer {
                player_id: pid(i),
                team: if i <= 5 { Team::Red } else { Team::Blue },
                role: roles[(i as usize - 1) % 5].to_owned(),
                role_was_flexed: false,
                mu_at_match: 30.0 - i as f64,
                sigma_at_match: 8.33,
            })
            .collect(),
    }
}

fn ev(kind: K, subject: Option<PlayerId>, seq: i64) -> NewEvent {
    NewEvent::new(kind, subject, at(seq), format!("k-{seq}"))
}

/// formation を作り、コード開示済み（in_progress）まで進める。
/// VC の組はどの formation も同じでよい（使い回す）。
async fn formation_in_progress(repo: &FormationRepository, n: u128, host: PlayerId) {
    repo.create_formation(&formation(n)).await.unwrap();
    for event in [
        NewEvent::vc_assigned("01", 101, 102, at(0), "k-0"),
        ev(K::HostAssigned, Some(host), 1),
        ev(K::HostAccepted, Some(host), 2),
        NewEvent::code_posted(host, "111111", at(3), "k-3"),
    ] {
        repo.append_event(fid(n), &event).await.unwrap();
    }
}

fn sqlstate(error: &sqlx::Error) -> Option<String> {
    error
        .as_database_error()
        .and_then(|database| database.code())
        .map(|code| code.to_string())
}

#[sqlx::test(
    fixtures("reference", "app_role"),
    fixtures("../ops/app_privileges.sql")
)]
async fn app_role_is_denied_update_and_delete_by_privilege(owner: PgPool) {
    let db = AppDb::connect(&owner).await;

    // アプリ用ロール: 権限で弾かれる（42501）。トリガー（0A000）より前に止まる
    for sql in [
        "update formations set created_at = now()",
        "delete from formations",
        "update formation_players set role_was_flexed = true",
        "delete from formation_players",
        "update formation_events set payload = '{}'",
        "delete from formation_events",
        "update match_reports set outcome = 'draw'",
        "delete from match_reports",
    ] {
        let error = sqlx::query(sql).execute(&db.pool).await.unwrap_err();
        assert_eq!(sqlstate(&error).as_deref(), Some("42501"), "{sql}");
    }

    // 所有者: 権限はあるが、トリガーが弾く（二重の防御）
    let error = sqlx::query("update formations set created_at = now()")
        .execute(&owner)
        .await
        .unwrap_err();
    assert_eq!(sqlstate(&error).as_deref(), Some("0A000"));

    // アプリ用ロールでも追記とリカバリ一覧は通る
    db.repo.create_formation(&formation(1)).await.unwrap();
    db.repo
        .append_event(fid(1), &NewEvent::vc_assigned("01", 101, 102, at(0), "k-0"))
        .await
        .unwrap();
    assert_eq!(db.repo.list_unfinished().await.unwrap().len(), 1);
    db.done().await;
}

#[sqlx::test(
    fixtures("reference", "app_role"),
    fixtures("../ops/app_privileges.sql")
)]
async fn created_formation_starts_matched_and_is_unfinished(owner: PgPool) {
    let db = AppDb::connect(&owner).await;
    let repo = &db.repo;
    repo.create_formation(&formation(1)).await.unwrap();

    let log = repo.load_events(fid(1)).await.unwrap();
    assert!(log.events.is_empty());
    assert!(log.unreadable.is_empty());
    let snapshot = log.snapshot();
    assert_eq!(snapshot.state, S::Matched);
    assert_eq!(log.since(&snapshot), formation(1).created_at);

    let unfinished = repo.list_unfinished().await.unwrap();
    assert_eq!(unfinished.len(), 1);
    assert_eq!(unfinished[0].log.formation_id, fid(1));
    assert_eq!(unfinished[0].snapshot.state, S::Matched);

    assert!(matches!(
        repo.load_events(fid(99)).await,
        Err(RepositoryError::FormationNotFound(_))
    ));
    assert!(matches!(
        repo.append_event(fid(99), &ev(K::VcAssigned, None, 0))
            .await,
        Err(RepositoryError::FormationNotFound(_))
    ));
    db.done().await;
}

#[sqlx::test(
    fixtures("reference", "app_role"),
    fixtures("../ops/app_privileges.sql")
)]
async fn create_formation_rejects_anything_but_five_versus_five(owner: PgPool) {
    let db = AppDb::connect(&owner).await;
    let repo = &db.repo;

    let mut nine = formation(1);
    nine.players.pop();
    assert!(matches!(
        repo.create_formation(&nine).await,
        Err(RepositoryError::InvalidFormation(_))
    ));

    let mut lopsided = formation(2);
    lopsided.players[5].team = Team::Red;
    assert!(matches!(
        repo.create_formation(&lopsided).await,
        Err(RepositoryError::InvalidFormation(_))
    ));

    let mut duplicated = formation(3);
    duplicated.players[9].player_id = pid(1);
    assert!(matches!(
        repo.create_formation(&duplicated).await,
        Err(RepositoryError::InvalidFormation(_))
    ));

    // 何も書かれていない
    assert!(repo.list_unfinished().await.unwrap().is_empty());
    db.done().await;
}

#[sqlx::test(
    fixtures("reference", "app_role"),
    fixtures("../ops/app_privileges.sql")
)]
async fn events_are_validated_then_persisted_in_order(owner: PgPool) {
    let db = AppDb::connect(&owner).await;
    let repo = &db.repo;
    let host = pid(1);
    formation_in_progress(repo, 1, host).await;

    let log = repo.load_events(fid(1)).await.unwrap();
    let kinds: Vec<K> = log.events.iter().map(|stored| stored.event.kind).collect();
    assert_eq!(
        kinds,
        vec![
            K::VcAssigned,
            K::HostAssigned,
            K::HostAccepted,
            K::CodePosted
        ]
    );
    assert!(log.events.windows(2).all(|pair| pair[0].id < pair[1].id));
    assert_eq!(
        log.events[3].event.payload,
        EventPayload::RoomCode("111111".into())
    );
    let snapshot = log.snapshot();
    assert_eq!(
        snapshot.state,
        S::InProgress {
            host,
            room_code: "111111".into()
        }
    );
    assert_eq!(snapshot.since, Some(at(3)));
    assert!(snapshot.anomalies.is_empty());

    // 生の payload はそのまま保存されている（Discord の ID は文字列）
    let stored_payload = sqlx::query!(
        "select payload from formation_events where formation_id = $1 and kind = 'vc_assigned'",
        fid(1).0
    )
    .fetch_one(&db.pool)
    .await
    .unwrap()
    .payload;
    assert_eq!(
        stored_payload,
        json!({ "vc": { "label": "01", "red_channel_id": "101", "blue_channel_id": "102" } })
    );

    let outcome = repo
        .append_event(fid(1), &ev(K::Confirmed, None, 4))
        .await
        .unwrap();
    assert_eq!(outcome.state, S::Confirmed);
    assert!(!outcome.already_recorded);
    assert!(repo.list_unfinished().await.unwrap().is_empty());
    db.done().await;
}

#[sqlx::test(
    fixtures("reference", "app_role"),
    fixtures("../ops/app_privileges.sql")
)]
async fn invalid_transition_is_rejected_without_writing(owner: PgPool) {
    let db = AppDb::connect(&owner).await;
    let repo = &db.repo;
    repo.create_formation(&formation(1)).await.unwrap();

    let result = repo.append_event(fid(1), &ev(K::Confirmed, None, 0)).await;
    assert!(matches!(
        result,
        Err(RepositoryError::Rejected(
            TransitionError::InvalidTransition { .. }
        ))
    ));
    // 部屋コードの無い code_posted も検証で弾く
    let result = repo
        .append_event(fid(1), &ev(K::CodePosted, Some(pid(1)), 1))
        .await;
    assert!(matches!(
        result,
        Err(RepositoryError::Rejected(
            TransitionError::PayloadMismatch { .. }
        ))
    ));
    assert!(repo.load_events(fid(1)).await.unwrap().events.is_empty());
    db.done().await;
}

#[sqlx::test(
    fixtures("reference", "app_role"),
    fixtures("../ops/app_privileges.sql")
)]
async fn same_idempotency_key_is_not_written_twice(owner: PgPool) {
    let db = AppDb::connect(&owner).await;
    let repo = &db.repo;
    repo.create_formation(&formation(1)).await.unwrap();
    let event = NewEvent::vc_assigned("01", 101, 102, at(0), "retry-me");

    let first = repo.append_event(fid(1), &event).await.unwrap();
    let second = repo.append_event(fid(1), &event).await.unwrap();
    assert!(!first.already_recorded);
    assert!(second.already_recorded);
    assert_eq!(first.event_id, second.event_id);
    assert_eq!(second.state, first.state);
    assert_eq!(repo.load_events(fid(1)).await.unwrap().events.len(), 1);

    // 同じキーで内容が違えばエラー
    let reused = NewEvent {
        idempotency_key: "retry-me".into(),
        ..ev(K::HostAssigned, Some(pid(1)), 0)
    };
    assert!(matches!(
        repo.append_event(fid(1), &reused).await,
        Err(RepositoryError::IdempotencyKeyReused { .. })
    ));
    assert_eq!(repo.load_events(fid(1)).await.unwrap().events.len(), 1);
    db.done().await;
}

#[sqlx::test(
    fixtures("reference", "app_role"),
    fixtures("../ops/app_privileges.sql")
)]
async fn concurrent_host_assignments_are_serialized(owner: PgPool) {
    let db = AppDb::connect(&owner).await;
    let repo = &db.repo;
    repo.create_formation(&formation(1)).await.unwrap();
    repo.append_event(fid(1), &NewEvent::vc_assigned("01", 101, 102, at(0), "k-0"))
        .await
        .unwrap();

    // 8 人を同時に指名しようとする。最初にロックを取った 1 件だけが通り、残りは検証で弾かれる
    let mut tasks = JoinSet::new();
    for i in 1..=8u128 {
        let repo = repo.clone();
        tasks.spawn(async move {
            repo.append_event(fid(1), &ev(K::HostAssigned, Some(pid(i)), i as i64))
                .await
        });
    }
    let mut appended = 0;
    let mut rejected = 0;
    while let Some(joined) = tasks.join_next().await {
        match joined.unwrap() {
            Ok(outcome) => {
                appended += 1;
                assert!(!outcome.already_recorded);
                assert!(matches!(
                    outcome.state,
                    S::AwaitingHost {
                        pending: Some(_),
                        tried: 1
                    }
                ));
            }
            Err(RepositoryError::Rejected(TransitionError::SubjectMismatch { .. })) => {
                rejected += 1;
            }
            Err(other) => panic!("unexpected error: {other}"),
        }
    }
    assert_eq!((appended, rejected), (1, 7));

    let log = repo.load_events(fid(1)).await.unwrap();
    assert_eq!(log.events.len(), 2);
    let snapshot = log.snapshot();
    assert!(snapshot.anomalies.is_empty());
    assert!(matches!(
        snapshot.state,
        S::AwaitingHost {
            pending: Some(_),
            tried: 1
        }
    ));
    db.done().await;
}

#[sqlx::test(
    fixtures("reference", "app_role"),
    fixtures("../ops/app_privileges.sql")
)]
async fn concurrent_decline_and_timeout_let_only_one_through(owner: PgPool) {
    let db = AppDb::connect(&owner).await;
    let repo = &db.repo;
    let host = pid(1);
    repo.create_formation(&formation(1)).await.unwrap();
    repo.append_event(fid(1), &NewEvent::vc_assigned("01", 101, 102, at(0), "k-0"))
        .await
        .unwrap();
    repo.append_event(fid(1), &ev(K::HostAssigned, Some(host), 1))
        .await
        .unwrap();

    let mut tasks = JoinSet::new();
    for (kind, seq) in [(K::HostDeclined, 2), (K::HostTimeout, 3)] {
        let repo = repo.clone();
        tasks.spawn(async move { repo.append_event(fid(1), &ev(kind, Some(host), seq)).await });
    }
    let mut results = Vec::new();
    while let Some(joined) = tasks.join_next().await {
        results.push(joined.unwrap());
    }
    assert_eq!(results.iter().filter(|result| result.is_ok()).count(), 1);
    assert_eq!(
        results
            .iter()
            .filter(|result| matches!(
                result,
                Err(RepositoryError::Rejected(
                    TransitionError::InvalidTransition { .. }
                ))
            ))
            .count(),
        1
    );
    let log = repo.load_events(fid(1)).await.unwrap();
    assert_eq!(log.events.len(), 3);
    assert_eq!(
        log.snapshot().state,
        S::AwaitingHost {
            pending: None,
            tried: 1
        }
    );
    db.done().await;
}

#[sqlx::test(
    fixtures("reference", "app_role"),
    fixtures("../ops/app_privileges.sql")
)]
async fn concurrent_identical_events_write_one_row(owner: PgPool) {
    let db = AppDb::connect(&owner).await;
    let repo = &db.repo;
    repo.create_formation(&formation(1)).await.unwrap();

    let mut tasks = JoinSet::new();
    for _ in 0..5 {
        let repo = repo.clone();
        tasks.spawn(async move {
            repo.append_event(
                fid(1),
                &NewEvent::vc_assigned("01", 101, 102, at(0), "same-key"),
            )
            .await
        });
    }
    let mut written = 0;
    let mut already = 0;
    while let Some(joined) = tasks.join_next().await {
        let outcome = joined.unwrap().unwrap();
        if outcome.already_recorded {
            already += 1;
        } else {
            written += 1;
        }
    }
    assert_eq!((written, already), (1, 4));
    assert_eq!(repo.load_events(fid(1)).await.unwrap().events.len(), 1);
    db.done().await;
}

#[sqlx::test(
    fixtures("reference", "app_role"),
    fixtures("../ops/app_privileges.sql")
)]
async fn corrupted_log_is_still_readable_and_appendable(owner: PgPool) {
    let db = AppDb::connect(&owner).await;
    let repo = &db.repo;
    repo.create_formation(&formation(1)).await.unwrap();

    // 検証を通さずに不正なイベントを直接 INSERT する（手動 INSERT や競合を模す）
    sqlx::query!(
        "insert into formation_events \
         (formation_id, occurred_at, kind, subject_player_id, payload, idempotency_key) \
         values ($1, $2, 'host_accepted', $3, '{}', 'bogus')",
        fid(1).0,
        at(0),
        pid(1).0,
    )
    .execute(&db.pool)
    .await
    .unwrap();

    let log = repo.load_events(fid(1)).await.unwrap();
    assert_eq!(log.events.len(), 1);
    let snapshot = log.snapshot();
    assert_eq!(snapshot.state, S::Matched, "不正な行はスキップされる");
    assert_eq!(snapshot.anomalies.len(), 1);
    assert!(matches!(
        snapshot.anomalies[0].cause,
        AnomalyCause::Rejected(TransitionError::InvalidTransition { .. })
    ));

    // 追記は止まらない。異常は結果に載って返る
    let outcome = repo
        .append_event(fid(1), &NewEvent::vc_assigned("01", 101, 102, at(1), "k-1"))
        .await
        .unwrap();
    assert_eq!(
        outcome.state,
        S::AwaitingHost {
            pending: None,
            tried: 0
        }
    );
    assert_eq!(outcome.anomalies.len(), 1);

    // リカバリ一覧にも載る（終端種別のイベントは無い）
    let unfinished = repo.list_unfinished().await.unwrap();
    assert_eq!(unfinished.len(), 1);
    assert_eq!(unfinished[0].snapshot.anomalies.len(), 1);
    db.done().await;
}

#[sqlx::test(
    fixtures("reference", "app_role"),
    fixtures("../ops/app_privileges.sql")
)]
async fn list_unfinished_returns_only_non_terminal_formations_in_creation_order(owner: PgPool) {
    let db = AppDb::connect(&owner).await;
    let repo = &db.repo;
    let host = pid(1);

    // 1: matched（イベント無し）
    repo.create_formation(&formation(1)).await.unwrap();
    // 2: in_progress
    formation_in_progress(repo, 2, host).await;
    // 3: confirmed
    formation_in_progress(repo, 3, host).await;
    repo.append_event(fid(3), &ev(K::Confirmed, None, 4))
        .await
        .unwrap();
    // 4: matched → abandoned（空いている VC が無い）
    repo.create_formation(&formation(4)).await.unwrap();
    repo.append_event(
        fid(4),
        &NewEvent::abandoned(AbandonReason::VcPoolExhausted, at(0), "k-0"),
    )
    .await
    .unwrap();

    let unfinished = repo.list_unfinished().await.unwrap();
    let ids: Vec<FormationId> = unfinished.iter().map(|f| f.log.formation_id).collect();
    assert_eq!(ids, vec![fid(1), fid(2)]);
    assert_eq!(unfinished[0].snapshot.state, S::Matched);
    assert_eq!(
        unfinished[1].snapshot.state,
        S::InProgress {
            host,
            room_code: "111111".into()
        }
    );
    assert_eq!(unfinished[1].log.events.len(), 4);
    db.done().await;
}

#[sqlx::test(
    fixtures("reference", "app_role"),
    fixtures("../ops/app_privileges.sql")
)]
async fn disputed_formation_stays_unfinished_without_a_deadline(owner: PgPool) {
    let db = AppDb::connect(&owner).await;
    let repo = &db.repo;
    formation_in_progress(repo, 1, pid(1)).await;
    repo.append_event(fid(1), &ev(K::Disputed, None, 4))
        .await
        .unwrap();

    // スタッフが裁定するまで一覧に残り続ける。期限は無いので、リカバリ側はタイマーを張らない
    let unfinished = repo.list_unfinished().await.unwrap();
    assert_eq!(unfinished.len(), 1);
    let disputed = &unfinished[0];
    assert_eq!(disputed.snapshot.state, S::Disputed);
    assert_eq!(
        next_deadline(
            &disputed.snapshot.state,
            disputed.log.since(&disputed.snapshot),
            None,
            &LifecycleConfig::default()
        ),
        None
    );
    db.done().await;
}

#[sqlx::test(
    fixtures("reference", "app_role"),
    fixtures("../ops/app_privileges.sql")
)]
async fn reports_are_accepted_only_while_the_formation_requires_them(owner: PgPool) {
    let db = AppDb::connect(&owner).await;
    let repo = &db.repo;
    let host = pid(1);
    repo.create_formation(&formation(1)).await.unwrap();
    let report = |player: PlayerId, seq: i64, supersedes_id| NewReport {
        formation_id: fid(1),
        player_id: player,
        reported_at: at(seq),
        outcome: Outcome::Draw,
        raw_result: json!({ "screenshot": "s3://x", "seq": seq }),
        supersedes_id,
        idempotency_key: format!("report-{seq}"),
    };

    // コード開示前は受け付けない。理由（状態）はエラーに載らない
    assert!(matches!(
        repo.append_report(&report(pid(1), 0, None)).await,
        Err(RepositoryError::ReportsNotAccepted { formation_id }) if formation_id == fid(1)
    ));

    for event in [
        NewEvent::vc_assigned("01", 101, 102, at(0), "k-0"),
        ev(K::HostAssigned, Some(host), 1),
        ev(K::HostAccepted, Some(host), 2),
        NewEvent::code_posted(host, "111111", at(3), "k-3"),
    ] {
        repo.append_event(fid(1), &event).await.unwrap();
    }

    let first = repo.append_report(&report(pid(1), 10, None)).await.unwrap();
    // 生データはそのまま保存される
    let stored = sqlx::query!(
        "select outcome, raw_result from match_reports where id = $1",
        first.0
    )
    .fetch_one(&db.pool)
    .await
    .unwrap();
    assert_eq!(stored.outcome, "draw");
    assert_eq!(
        stored.raw_result,
        json!({ "screenshot": "s3://x", "seq": 10 })
    );

    // 本人による打ち消しは通る
    let second = repo
        .append_report(&report(pid(1), 11, Some(first)))
        .await
        .unwrap();
    assert!(second > first);
    // 同じ報告をもう一度打ち消すことはできない（訂正チェーンは分岐しない）
    assert!(matches!(
        repo.append_report(&report(pid(1), 12, Some(first))).await,
        Err(RepositoryError::AlreadySuperseded(id)) if id == first
    ));
    // チェーンの先頭（最新）は打ち消せる
    let third = repo
        .append_report(&report(pid(1), 13, Some(second)))
        .await
        .unwrap();
    assert!(third > second);
    // 他人の報告は打ち消せない
    assert!(matches!(
        repo.append_report(&report(pid(2), 14, Some(first))).await,
        Err(RepositoryError::SupersedeMismatch(id)) if id == first
    ));
    // 参加者でない人の報告は複合 FK で弾かれる
    assert!(matches!(
        repo.append_report(&report(pid(11), 15, None)).await,
        Err(RepositoryError::Database(_))
    ));
    // 確定後も未報告者の義務は残るので受け付ける
    repo.append_event(fid(1), &ev(K::Confirmed, None, 20))
        .await
        .unwrap();
    repo.append_report(&report(pid(2), 21, None)).await.unwrap();
    // 不成立になった試合は受け付けない
    formation_in_progress(repo, 2, host).await;
    repo.append_event(fid(2), &ev(K::Voided, None, 4))
        .await
        .unwrap();
    assert!(matches!(
        repo.append_report(&NewReport {
            formation_id: fid(2),
            ..report(pid(1), 30, None)
        })
        .await,
        Err(RepositoryError::ReportsNotAccepted { .. })
    ));
    db.done().await;
}

#[sqlx::test(
    fixtures("reference", "app_role"),
    fixtures("../ops/app_privileges.sql")
)]
async fn the_same_role_twice_in_one_team_is_rejected_by_the_database(owner: PgPool) {
    let db = AppDb::connect(&owner).await;

    // red の 2 人目に、1 人目と同じロールを付ける（5v5 の人数チェックは通る）
    let mut duplicated = formation(1);
    duplicated.players[1].role = duplicated.players[0].role.clone();
    assert_eq!(duplicated.players[0].team, duplicated.players[1].team);
    let error = db.repo.create_formation(&duplicated).await.unwrap_err();
    let RepositoryError::Database(error) = error else {
        panic!("expected a database error, got {error}");
    };
    assert_eq!(
        sqlstate(&error).as_deref(),
        Some("23505"),
        "unique violation"
    );
    assert_eq!(
        error
            .as_database_error()
            .and_then(|database| database.constraint()),
        Some("formation_players_formation_id_team_role_key")
    );
    // トランザクションごと巻き戻っているので、formations にも残っていない
    assert!(db.repo.list_unfinished().await.unwrap().is_empty());

    // 同じロールでもチームが違えば良い（red の top_carry と blue の top_carry）
    db.repo.create_formation(&formation(2)).await.unwrap();
    let roster = db.repo.load_roster(fid(2)).await.unwrap();
    assert_eq!(roster.len(), 10);
    assert_eq!(
        roster
            .iter()
            .filter(|entry| entry.role == "top_carry")
            .count(),
        2
    );
    assert_eq!(roster[0].team, Team::Red);
    assert_eq!(roster[0].role_display_name, "上キャリー");
    db.done().await;
}

#[sqlx::test(
    fixtures("reference", "app_role"),
    fixtures("../ops/app_privileges.sql")
)]
async fn winners_can_be_joined_with_team_equals_outcome(owner: PgPool) {
    let db = AppDb::connect(&owner).await;
    let repo = &db.repo;
    let host = pid(1);
    formation_in_progress(repo, 1, host).await;

    // red の 1 人と blue の 1 人が「red の勝ち」、blue のもう 1 人が引き分けと報告
    for (player, outcome) in [
        (pid(1), Outcome::Win(Team::Red)),
        (pid(6), Outcome::Win(Team::Red)),
        (pid(7), Outcome::Draw),
    ] {
        repo.append_report(&NewReport {
            formation_id: fid(1),
            player_id: player,
            reported_at: at(10),
            outcome,
            raw_result: json!({}),
            supersedes_id: None,
            idempotency_key: format!("outcome-{}", player.0),
        })
        .await
        .unwrap();
    }

    // outcome の値域が formation_players.team と同じなので、勝者側は CASE 無しで結合できる
    let winners = sqlx::query!(
        "select fp.player_id \
         from match_reports mr \
         join formation_players fp on fp.formation_id = mr.formation_id and fp.team = mr.outcome \
         where mr.formation_id = $1 and mr.player_id = $2 \
         order by fp.player_id",
        fid(1).0,
        pid(1).0,
    )
    .fetch_all(&db.pool)
    .await
    .unwrap();
    let winners: Vec<PlayerId> = winners
        .into_iter()
        .map(|row| PlayerId(row.player_id))
        .collect();
    assert_eq!(winners, (1..=5).map(pid).collect::<Vec<_>>());

    // 保存されている値は red / blue / draw そのもの
    let stored: Vec<String> = sqlx::query_scalar!(
        "select outcome from match_reports where formation_id = $1 order by id",
        fid(1).0
    )
    .fetch_all(&db.pool)
    .await
    .unwrap();
    assert_eq!(stored, vec!["red", "red", "draw"]);
    db.done().await;
}
