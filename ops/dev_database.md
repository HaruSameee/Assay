# 開発用 PostgreSQL（作業ホスト）

sqlx のオフラインデータ（`.sqlx/`）を生成し、`#[sqlx::test]` の DB テストを
ローカルで走らせるための恒久的な環境。**本番にも CI にも関係しない。**

## なぜ要るか

`sqlx::query!` はコンパイル時にクエリを DB へ問い合わせて型を検証する。新しい
クエリを足すと `.sqlx/` にオフラインデータが要り、これは**実際の DB が無いと
生成できない**（`cargo sqlx prepare`）。手書きは `nullable` の推論を当てられず、
CI の `cargo sqlx prepare --check` を何往復もすることになる。

## 構成

| 項目 | 値 |
|---|---|
| バージョン | PostgreSQL **17.6**（本番は 17.11。型推論がずれないようメジャーを揃えてある） |
| ポート | **5433** |
| 待ち受け | `127.0.0.1` のみ |
| バイナリ | `F:\assay-devdb\pgsql\bin` |
| データディレクトリ | `F:\assay-devdb\data` |
| ログ | `F:\assay-devdb\pg.log` |
| データベース | `assay_dev` |
| エンコーディング | UTF8 / locale C |
| 認証 | `scram-sha-256`（local / host とも。`trust` は使わない） |

**ポート 5432 で別の PostgreSQL が動いていることがある。それには触れない。**
このクラスタは 5433 で完全に独立している。

## ロール構成（本番・CI と同じ形）

`ops/ci_roles.sql` をそのまま流してあるので、**本番と同じ所有関係**になっている。

| ロール | 役割 | 権限 |
|---|---|---|
| `ci_admin` | スーパーユーザー。**隔離 DB の作成にだけ使う** | superuser |
| `assay_migrator` | `assay_dev` と `public` スキーマの所有者。マイグレーションと権限設定 | nocreatedb / nocreaterole |
| `assay_app` | アプリケーション。既定は `select` / `insert` のみ | nocreatedb / nocreaterole |

**所有者権限でテストを走らせないこと。**`assay_app` の権限不足は本番でしか
出ないバグになるため、テストは必ずアプリ用ロールで DB を触る
（`tests/*.rs` の `AppDb::connect` が所有者プールから `assay_app` へ接続し直す）。

`assay_app` のパスワードは `tests/fixtures/app_role.sql` と `tests/*.rs` の
`APP_ROLE_PASSWORD` に合わせてある（リポジトリに書かれている固定値）。
`ci_admin` と `assay_migrator` のパスワードはランダムに生成した。

## 資格情報

作業ツリー直下の **`.env`**（`.gitignore` 済み）に接続文字列がある。

- **値を端末に出さないこと。**`cat .env` / `grep -r ... .` などで表示しない
  （`grep` は `--exclude-dir` だけでなく `.env` 自体も除外すること）
- 生成した値の実体は `F:\assay-devdb\secrets\`（`umask 077`）
- コミットしない。共有しない

## 起動と停止

```sh
BIN=/f/assay-devdb/pgsql/bin
"$BIN/pg_ctl.exe" -D /f/assay-devdb/data -l /f/assay-devdb/pg.log -w start
"$BIN/pg_ctl.exe" -D /f/assay-devdb/data -w stop
"$BIN/pg_isready.exe" -h 127.0.0.1 -p 5433
```

## 使い方

```sh
cd <作業ツリー>
export PATH="/f/assay-devdb/pgsql/bin:$PATH"
set -a; . ./.env; set +a

# マイグレーションと権限（マイグレーションを足したら毎回）
DATABASE_URL="$MIGRATOR_DATABASE_URL" cargo sqlx migrate run
psql "$MIGRATOR_DATABASE_URL" -v ON_ERROR_STOP=1 -f ops/app_role.sql

# 権限の確認（CI と同じ 2 本）
psql "$DATABASE_URL" -v ON_ERROR_STOP=1 -f ops/verify_app_role.sql
psql "$DATABASE_URL" -XAt -v ON_ERROR_STOP=1 -f ops/check_app_role.sql   # 出力が空なら合格

# オフラインデータの生成（新しいクエリを足したら毎回）
cargo sqlx prepare --workspace -- --all-targets

# DB を使うテスト。**管理用接続で起動する**（隔離 DB の作成に CREATE DATABASE が要る）。
# テスト本体は assay_app に接続し直す
DATABASE_URL="$TEST_ADMIN_DATABASE_URL" cargo test --test schema --test rating   # 等
```

`--lib` / `--bins` / `--test lifecycle` は DB を使わないので `SQLX_OFFLINE=true`
だけで走る。

## 作り直す場合

`F:\assay-devdb` を消して、PostgreSQL 17 の Windows 用 zip を展開し直し、
上記の構成で `initdb`（`--encoding=UTF8 --locale=C --auth-local=scram-sha-256
--auth-host=scram-sha-256`）→ `postgresql.conf` に `port = 5433` と
`listen_addresses = '127.0.0.1'` を追記 → `ops/ci_roles.sql` を
`-v migrator_password=... -v app_password=...` 付きで流す。
