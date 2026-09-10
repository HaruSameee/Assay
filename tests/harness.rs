//! テストの書き方そのものを検査する。**DB は使わない。**
//!
//! docs/02 の禁止事項:
//!
//! > テストはアプリ用ロールで走らせる。所有者権限で走らせると権限バグが本番でしか出ない。
//!
//! `#[sqlx::test]` がテストに渡す `owner: PgPool` は `DATABASE_URL` から作られ、
//! CI ではそれが**管理用接続**（`ci_admin`）である。隔離 DB の作成に
//! `CREATE DATABASE` が要るためで、これは避けられない。
//!
//! したがって「テストがアプリ用ロールで走ること」は**環境変数では守れない**。
//! 各テストが `owner` からアプリ用ロールへ接続し直すことで守っている。
//! それが慣習のままだと、新しく書かれたテストが所有者接続のまま
//! リポジトリを叩いても誰も気づかない——**このファイルがそこを見る。**
//!
//! `owner` を直接使うこと自体は禁止しない。次の 2 つは正当である:
//!
//! - **シード**（`players` の行を作る等）。アプリの経路ではない
//! - **所有者でもトリガーに弾かれること**の検査（`0A000` を期待する側）
//!
//! 禁じるのは「テスト対象のコード経路を所有者接続で走らせること」だけ。

use std::fs;
use std::path::{Path, PathBuf};

/// このファイル自身は検査しない（禁止パターンを文字列として持っているため）。
const SELF: &str = "harness.rs";

/// `owner` プールをそのまま渡してテスト対象を組み立てている形。
///
/// `Foo::new(owner)` / `Foo::new(&owner)` / `Foo::new(owner.clone())` をまとめて拾う。
const FORBIDDEN: [&str; 2] = ["::new(owner", "::new(&owner"];

/// アプリ用ロールへ接続し直していると認める印。
///
/// どのスイートも `owner` の接続情報からユーザー名とパスワードだけ差し替える形なので、
/// そのどれかが出てくる。
const APP_ROLE_MARKERS: [&str; 3] = ["APP_ROLE", "app_pool", "username("];

fn tests_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests")
}

/// `tests/*.rs` を読む（サブディレクトリの共通モジュールは対象外）。
fn test_sources() -> Vec<(String, String)> {
    let mut sources = Vec::new();
    for entry in fs::read_dir(tests_dir()).expect("tests/ を読めること") {
        let path = entry.expect("ディレクトリの読み取り").path();
        if path.extension().and_then(|ext| ext.to_str()) != Some("rs") {
            continue;
        }
        let name = path
            .file_name()
            .and_then(|name| name.to_str())
            .expect("ファイル名")
            .to_owned();
        if name == SELF {
            continue;
        }
        let body = fs::read_to_string(&path).expect("テストソースを読めること");
        sources.push((name, body));
    }
    sources.sort();
    assert!(!sources.is_empty(), "tests/*.rs が 1 つも見つからない");
    sources
}

/// **テスト対象を所有者接続で組み立てていないこと。**
///
/// 所有者接続でリポジトリを叩くと、アプリ用ロールに権限が無い操作もテストでは通り、
/// 本番でだけ 42501 で落ちる。
#[test]
fn no_test_builds_a_repository_from_the_owner_pool() {
    let mut violations = Vec::new();
    for (name, body) in test_sources() {
        for (number, line) in body.lines().enumerate() {
            for pattern in FORBIDDEN {
                if line.contains(pattern) {
                    violations.push(format!("{name}:{} {}", number + 1, line.trim()));
                }
            }
        }
    }
    assert!(
        violations.is_empty(),
        "所有者接続でテスト対象を組み立てている。アプリ用ロールへ接続し直すこと\n\
         （シードや 0A000 の検査で owner を使うのは問題ない。禁じているのは\n\
         リポジトリ・State を owner から作ることだけ）:\n{}",
        violations.join("\n")
    );
}

/// **DB を使うスイートは、必ずアプリ用ロールへ接続し直すこと。**
///
/// 新しいスイートを足した人が接続し直しを忘れると、そのファイルのテストは
/// まるごと管理用接続で走る。ここで落ちる。
#[test]
fn every_database_suite_reconnects_as_the_application_role() {
    let mut missing = Vec::new();
    let mut checked = Vec::new();
    for (name, body) in test_sources() {
        if !body.contains("sqlx::test") {
            continue;
        }
        checked.push(name.clone());
        if !APP_ROLE_MARKERS.iter().any(|marker| body.contains(marker)) {
            missing.push(name);
        }
    }
    assert!(
        !checked.is_empty(),
        "#[sqlx::test] を使うスイートが 1 つも見つからない（検査が空振りしている）"
    );
    assert!(
        missing.is_empty(),
        "所有者接続のままの DB スイートがある。owner からアプリ用ロールへ\n\
         接続し直すこと（tests/*.rs の AppDb::connect / app_pool を参照）:\n{}",
        missing.join("\n")
    );
    println!(
        "アプリ用ロールへ接続し直していることを確認したスイート: {} 件\n  {}",
        checked.len(),
        checked.join(" ")
    );
}
