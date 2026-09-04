//! Rust 側の列挙と Phase 1 マイグレーションの整合性。片方だけ変わる事故を防ぐ。

use std::fs;
use std::path::{Path, PathBuf};

use unite_matchsystem::lifecycle::{AbandonReason, FormationEventKind};

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
