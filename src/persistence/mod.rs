//! 永続化層（Phase 3）。Phase 2 の状態機械を sqlx / PostgreSQL に載せる。
//!
//! 不変条件:
//! - 書き込みは INSERT だけ。UPDATE / DELETE は書かない。DB 側では二重に守る:
//!   アプリ用ロールから追記専用4テーブルの UPDATE / DELETE を REVOKE し（`ops/app_role.sql`）、
//!   さらに `reject_mutation` トリガーが所有者からの UPDATE / DELETE も拒否する。
//! - イベントの追記は、formation 単位の**アドバイザリロック**（`pg_advisory_xact_lock`）で直列化し、
//!   同一トランザクション内で現在状態を導出して [`crate::lifecycle::apply`] で検証してから INSERT する。
//!   `SELECT ... FOR UPDATE` は使わない。テーブルの UPDATE 権限を要求するので、REVOKE 済みの
//!   アプリ用ロールでは落ちる（テストはアプリ用ロールで走らせて、これを検出する）。
//! - 重複排除は DB の `unique (formation_id, idempotency_key)` の責務。同一キーの再送は
//!   「既に記録済み」として成功を返す（内容が違えばエラー）。
//! - 読み出しは失敗しない。読めない行は [`UnreadableRow`] として返し、状態は
//!   [`crate::lifecycle::derive_snapshot`] で導出する（適用できない行は `Anomaly`）。
//! - 報告はブラインド。報告内容は、それを書いたプレイヤー以外のクライアントに到達してはならない。
//!   この層に他人の報告を返す関数は無く、`append_report` の戻り値は受理した報告の id だけ
//!   （確定状態・係争の有無・フラグは返さない）。
//! - 報告の訂正は `supersedes_id` を持つ新しい行。1つの報告を打ち消せるのは1件だけ。
//!   勝敗判定は最新の報告を使うが、報告一致率は各プレイヤーの最初の報告で測る（Phase 6）。
//! - 報告の生データ（`raw_result`）は受け取った JSON をそのまま保存する。
//! - SQL はすべて `sqlx::query!` / `query_as!` でコンパイル時に検証する（`.sqlx/` にオフラインデータ。
//!   クエリを変えたら `cargo sqlx prepare -- --all-targets` で更新し、CI の `--check` で陳腐化を検出する）。
//! - TLS は使わない（PostgreSQL は同一ホスト前提）。判断の詳細は `Cargo.toml` の sqlx の項。

mod codec;
mod error;
mod model;
mod reporting;
pub(crate) mod repository;

pub use codec::{
    ABANDON_REASON_KEY, ROOM_CODE_KEY, VC_BLUE_CHANNEL_ID_KEY, VC_KEY, VC_LABEL_KEY,
    VC_RED_CHANNEL_ID_KEY, decode_payload,
};
pub use error::RepositoryError;
pub use model::{
    AppendOutcome, AppendReportOutcome, EventId, FormationLog, NewDiscordFailure, NewEvent,
    NewFormation, NewFormationPlayer, NewReport, Outcome, ReportId, RosterEntry, StoredEvent, Team,
    UnfinishedFormation, UnreadableRow, VcAssignment,
};
pub(crate) use model::{ReportDecision, ReportVote};
pub use repository::{FormationRepository, VcAssignmentOutcome};
