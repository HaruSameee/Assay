//! Rust 側の列挙と Phase 1 マイグレーションの整合性。片方だけ変わる事故を防ぐ。

use std::fs;
use std::path::{Path, PathBuf};

use unite_matchsystem::lifecycle::{AbandonReason, FormationEventKind};

const PRIVILEGES: &str = include_str!("../ops/app_privileges.sql");
const AUDIT: &str = include_str!("../ops/check_app_role.sql");

async fn app_pool(owner: &sqlx::PgPool) -> sqlx::PgPool {
    sqlx::postgres::PgPoolOptions::new()
        .max_connections(2)
        .connect_with(
            (*owner.connect_options())
                .clone()
                .username("assay_app")
                .password("assay_app_test"),
        )
        .await
        .expect("connect as assay_app")
}

async fn violations(pool: &sqlx::PgPool) -> Vec<String> {
    sqlx::query_scalar(AUDIT).fetch_all(pool).await.unwrap()
}

#[sqlx::test(fixtures("app_role"), fixtures("../ops/app_privileges.sql"))]
async fn append_only_privileges_and_triggers_are_wired(owner: sqlx::PgPool) {
    let app = app_pool(&owner).await;
    let who: String = sqlx::query_scalar("select current_user::text")
        .fetch_one(&app)
        .await
        .unwrap();
    assert_eq!(who, "assay_app");
    assert_eq!(violations(&app).await, Vec::<String>::new());

    for sql in [
        "update formations set id = id",
        "delete from formations",
        "update formation_players set formation_id = formation_id",
        "delete from formation_players",
        "update formation_events set id = id",
        "delete from formation_events",
        "update match_reports set id = id",
        "delete from match_reports",
    ] {
        for (pool, expected) in [(&app, "42501"), (&owner, "0A000")] {
            let error = sqlx::query(sql).execute(pool).await.unwrap_err();
            assert_eq!(
                error.as_database_error().unwrap().code().as_deref(),
                Some(expected),
                "{sql}"
            );
        }
    }
    let mut tx = app.begin().await.unwrap();
    sqlx::query("insert into players values ($1, 123456789, now())")
        .bind(uuid::Uuid::new_v4())
        .execute(&mut *tx)
        .await
        .unwrap();
    assert_eq!(
        sqlx::query("update players set created_at = now()")
            .execute(&mut *tx)
            .await
            .unwrap()
            .rows_affected(),
        1
    );
    assert_eq!(
        sqlx::query("update roles set display_name = display_name")
            .execute(&mut *tx)
            .await
            .unwrap()
            .rows_affected(),
        5
    );
    tx.rollback().await.unwrap();
    app.close().await;
}

#[sqlx::test(fixtures("app_role"), fixtures("../ops/app_privileges.sql"))]
async fn new_tables_fail_closed_and_audit_detects_both_missing_defenses(owner: sqlx::PgPool) {
    let app = app_pool(&owner).await;
    sqlx::raw_sql("create table future_log (id bigserial primary key)")
        .execute(&owner)
        .await
        .unwrap();
    sqlx::query("insert into future_log default values")
        .execute(&app)
        .await
        .unwrap();
    let count: i64 = sqlx::query_scalar("select count(*) from future_log")
        .fetch_one(&app)
        .await
        .unwrap();
    assert_eq!(count, 1);
    let sequence_value: i64 = sqlx::query_scalar("select last_value from future_log_id_seq")
        .fetch_one(&app)
        .await
        .unwrap();
    assert_eq!(sequence_value, 1);
    for sql in ["update future_log set id = id", "delete from future_log"] {
        let error = sqlx::query(sql).execute(&app).await.unwrap_err();
        assert_eq!(
            error.as_database_error().unwrap().code().as_deref(),
            Some("42501")
        );
    }
    assert_eq!(violations(&app).await, ["future_log"]);
    sqlx::raw_sql("create trigger future_log_guard before update or delete on future_log for each statement execute function forbid_update_delete()")
        .execute(&owner).await.unwrap();
    assert!(violations(&app).await.is_empty());

    // Check each direction independently, including column-level and PUBLIC grants.
    for grant in [
        "grant update on future_log to assay_app",
        "grant delete on future_log to assay_app",
        "grant update (id) on future_log to assay_app",
        "grant delete on future_log to public",
    ] {
        let mut tx = owner.begin().await.unwrap();
        sqlx::raw_sql(grant).execute(&mut *tx).await.unwrap();
        let failures: Vec<String> = sqlx::query_scalar(AUDIT).fetch_all(&mut *tx).await.unwrap();
        assert_eq!(failures, ["future_log"], "{grant}");
        tx.rollback().await.unwrap();
    }
    for change in [
        "alter table future_log disable trigger future_log_guard",
        "drop trigger future_log_guard on future_log; create trigger future_log_guard before update on future_log for each statement execute function forbid_update_delete()",
    ] {
        let mut tx = owner.begin().await.unwrap();
        sqlx::raw_sql(change).execute(&mut *tx).await.unwrap();
        let failures: Vec<String> = sqlx::query_scalar(AUDIT).fetch_all(&mut *tx).await.unwrap();
        assert_eq!(failures, ["future_log"]);
        tx.rollback().await.unwrap();
    }
    let mut tx = owner.begin().await.unwrap();
    sqlx::raw_sql("create trigger roles_accidental_guard before update or delete on roles for each statement execute function forbid_update_delete()")
        .execute(&mut *tx).await.unwrap();
    let failures: Vec<String> = sqlx::query_scalar(AUDIT).fetch_all(&mut *tx).await.unwrap();
    assert_eq!(failures, ["roles"]);
    tx.rollback().await.unwrap();
    app.close().await;
}

#[sqlx::test(fixtures("app_role"), fixtures("../ops/app_privileges.sql"))]
async fn policy_is_idempotent_and_clears_legacy_defaults(owner: sqlx::PgPool) {
    sqlx::raw_sql("grant update, delete on formations to assay_app; alter default privileges grant all on tables to assay_app; alter default privileges in schema public grant all on tables to assay_app;")
        .execute(&owner).await.unwrap();
    for _ in 0..2 {
        sqlx::raw_sql(PRIVILEGES).execute(&owner).await.unwrap();
    }
    let app = app_pool(&owner).await;
    assert!(violations(&app).await.is_empty());
    sqlx::raw_sql("create table future_log (id integer); create trigger future_log_guard before update or delete on future_log for each statement execute function forbid_update_delete();")
        .execute(&owner).await.unwrap();
    assert!(violations(&app).await.is_empty());
    app.close().await;
}

/// `migrations/*.up.sql` を（パス, 内容）で返す。
fn up_migrations() -> Vec<(PathBuf, String)> {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("migrations");
    let mut files: Vec<(PathBuf, String)> = fs::read_dir(&dir)
        .expect("migrations directory")
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.path())
        .filter(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.ends_with(".up.sql"))
        })
        .map(|path| {
            let sql = fs::read_to_string(&path).expect("read migration");
            (path, sql)
        })
        .collect();
    files.sort();
    assert!(
        !files.is_empty(),
        "no up migrations under {}",
        dir.display()
    );
    files
}

/// `check (kind in ('a', 'b', ...))` の値を取り出す。無ければ `None`。
fn kind_check_values(sql: &str) -> Option<Vec<String>> {
    let normalized = sql.split_whitespace().collect::<Vec<_>>().join(" ");
    let marker = "check (kind in (";
    let start = normalized.find(marker)? + marker.len();
    let end = start + normalized[start..].find(')')?;
    let values = normalized[start..end]
        .split(',')
        .map(|value| value.trim().trim_matches('\'').to_owned())
        .collect();
    Some(values)
}

#[test]
fn event_kind_check_matches_rust_enum() {
    let checks: Vec<(PathBuf, Vec<String>)> = up_migrations()
        .into_iter()
        .filter_map(|(path, sql)| kind_check_values(&sql).map(|values| (path, values)))
        .collect();
    assert_eq!(
        checks.len(),
        1,
        "exactly one migration must define the kind CHECK: {checks:?}"
    );
    let (path, sql_values) = &checks[0];

    let mut sql_sorted = sql_values.clone();
    sql_sorted.sort();
    let mut rust_sorted: Vec<String> = FormationEventKind::ALL
        .iter()
        .map(|kind| kind.as_str().to_owned())
        .collect();
    rust_sorted.sort();
    assert_eq!(
        sql_sorted,
        rust_sorted,
        "kind CHECK in {} must match FormationEventKind::ALL",
        path.display()
    );
}

#[test]
fn formation_events_uses_subject_player_id() {
    // `--` コメント行は除いて、定義本体だけを見る（ヘッダーには改名の経緯として旧名が残る）
    let sql: String = up_migrations()
        .into_iter()
        .flat_map(|(_, sql)| {
            sql.lines()
                .filter(|line| !line.trim_start().starts_with("--"))
                .map(|line| format!("{line}\n"))
                .collect::<Vec<_>>()
        })
        .collect();
    assert!(sql.contains("subject_player_id"));
    assert!(
        !sql.contains("actor_player_id"),
        "actor_player_id was renamed to subject_player_id"
    );
}

#[test]
fn abandon_reasons_are_documented_in_the_migration() {
    let sql: String = up_migrations().into_iter().map(|(_, sql)| sql).collect();
    for reason in AbandonReason::ALL {
        assert!(
            sql.contains(reason.as_str()),
            "payload comment must mention {reason}"
        );
    }
}
