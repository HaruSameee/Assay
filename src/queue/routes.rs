//! キューのエンドポイント。JSON。
//!
//! - `GET    /queue` 自分のエントリと「範囲内でキュー中の人数」
//! - `POST   /queue` 投入 / 設定の更新
//! - `DELETE /queue` 離脱
//!
//! **他人のエントリは触れない。**対象の `player_id` はすべてセッション
//! （[`CurrentSession`]）から取り、リクエストボディからは読まない。
//!
//! **2つのゲートは畳まない。**拒否のレスポンスは `registration` と `pending_reports` を
//! 別々のフィールドで返す（05 §4 / 07 §4-5）。1つの真偽値にすると、キュー画面で
//! 「なぜ入れないのか」を説明できなくなる。
//!
//! CSRF: セッション Cookie は `SameSite=Lax` なので、他サイトからの POST / DELETE には
//! 送られない。登録トラックのフォームと同じ前提に拠る。

use axum::extract::{FromRef, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use axum_extra::extract::cookie::Key;
use chrono::Utc;
use serde::{Deserialize, Serialize};
use serde_json::json;
use tracing::error;

use uuid::Uuid;

use crate::lifecycle::{FormationId, PlayerId};
use crate::registration::{
    AuthConfig, CurrentSession, QueueEligibility, RegistrationBlock, RegistrationRepository,
};

use super::model::{
    MIN_DOWN_RANGE, QueueEntry, QueueRequestError, RolePreference, UP_RANGE, validate_request,
};
use super::repository::{QueueError, QueueRepository};

/// パス。
const QUEUE_PATH: &str = "/queue";

/// キューのルーターが持つ状態。
#[derive(Clone)]
pub struct QueueState {
    /// セッション Cookie を読むための設定。
    pub config: AuthConfig,
    /// 2つのゲートの評価に使う。
    pub registration: RegistrationRepository,
    /// キューの永続化。
    pub queue: QueueRepository,
    /// Cookie の署名鍵（`config` から導出したものを保持する）。
    key: Key,
}

impl QueueState {
    /// 組み立てる。
    pub fn new(
        config: AuthConfig,
        registration: RegistrationRepository,
        queue: QueueRepository,
    ) -> Self {
        let key = config.cookie_key();
        Self {
            config,
            registration,
            queue,
            key,
        }
    }
}

impl FromRef<QueueState> for Key {
    fn from_ref(state: &QueueState) -> Self {
        state.key.clone()
    }
}

impl FromRef<QueueState> for AuthConfig {
    fn from_ref(state: &QueueState) -> Self {
        state.config.clone()
    }
}

/// ルーターを組み立てる。
pub fn router(state: QueueState) -> Router {
    Router::new()
        .route(
            QUEUE_PATH,
            get(show_queue).post(join_queue).delete(leave_queue),
        )
        .with_state(state)
}

// ----------------------------------------------------------------------
// 表現（DTO）
// ----------------------------------------------------------------------

/// 投入リクエスト。**`player_id` は受け取らない**（セッションから取る）。
#[derive(Debug, Deserialize)]
struct JoinRequest {
    /// 下方向のマッチング範囲。省略時は既定（95）。上方向は固定なので受け取らない。
    #[serde(default = "default_down_range")]
    down_range: i32,
    /// 希望ロール。1〜3、優先度付き。
    #[serde(default)]
    roles: Vec<RolePreference>,
}

fn default_down_range() -> i32 {
    MIN_DOWN_RANGE
}

/// 登録ゲートの理由。**報告ゲートとは別のフィールドで返す。**
#[derive(Debug, Serialize)]
struct RegistrationBlockView {
    /// 機械可読な理由。
    code: &'static str,
    /// 画面に出す説明。
    message: String,
    /// 直近の申告ランク（`rank_below_entry` のときだけ）。
    #[serde(skip_serializing_if = "Option::is_none")]
    declared: Option<&'static str>,
}

impl From<&RegistrationBlock> for RegistrationBlockView {
    fn from(block: &RegistrationBlock) -> Self {
        let (code, declared) = match block {
            RegistrationBlock::NotRegistered => ("not_registered", None),
            RegistrationBlock::Suspended => ("suspended", None),
            RegistrationBlock::UnknownStatus => ("unknown_status", None),
            RegistrationBlock::RankBelowEntry { declared } => {
                ("rank_below_entry", declared.map(|rank| rank.as_str()))
            }
        };
        Self {
            code,
            message: block.message(),
            declared,
        }
    }
}

/// 2つのゲートの状態。畳まない。
#[derive(Debug, Serialize)]
struct EligibilityView {
    /// 登録ゲート。通っていれば `null`。
    registration: Option<RegistrationBlockView>,
    /// 報告ゲート。未報告の formation。空なら通過。
    pending_reports: Vec<Uuid>,
}

impl From<&QueueEligibility> for EligibilityView {
    fn from(eligibility: &QueueEligibility) -> Self {
        Self {
            registration: eligibility.registration.as_ref().map(Into::into),
            pending_reports: eligibility
                .pending_reports
                .iter()
                .map(|FormationId(id)| *id)
                .collect(),
        }
    }
}

/// 自分のエントリ。識別子は素の UUID にする（ドメインの newtype に serde を足さない）。
#[derive(Debug, Serialize)]
struct EntryView {
    /// `queue_entries.id`。
    id: Uuid,
    /// 所有者。常にセッションのプレイヤー。
    player_id: Uuid,
    /// 最初にキューへ入った時刻。設定変更では動かない。RFC 3339。
    enqueued_at: String,
    /// 設定を最後に変えた時刻。RFC 3339。
    updated_at: String,
    /// 下方向のマッチング範囲。
    down_range: i32,
    /// 上方向のマッチング範囲。**固定値**を毎回返す（UI が定数を持たないで済むように）。
    up_range: i32,
    /// 希望ロール。`priority` の昇順。
    roles: Vec<RolePreference>,
}

impl From<QueueEntry> for EntryView {
    fn from(entry: QueueEntry) -> Self {
        Self {
            id: entry.id.0,
            player_id: entry.player_id.0,
            enqueued_at: entry.enqueued_at.to_rfc3339(),
            updated_at: entry.updated_at.to_rfc3339(),
            down_range: entry.down_range,
            up_range: UP_RANGE,
            roles: entry.roles,
        }
    }
}

/// キューの状態。
#[derive(Debug, Serialize)]
struct StatusView {
    /// キュー中か。
    queued: bool,
    /// 自分のエントリ。キュー中でなければ `null`。
    entry: Option<EntryView>,
    /// 自分の範囲内でキュー中の人数（相互条件、07 §3-1）。
    ///
    /// 範囲は本人の設定なので、キューに入る前は決まらない。そのときは `null`。
    in_range: Option<usize>,
    /// 2つのゲートの現在の状態。キュー中でも報告ゲートは後から立ちうる。
    eligibility: EligibilityView,
}

// ----------------------------------------------------------------------
// ハンドラ
// ----------------------------------------------------------------------

/// 状態取得。
async fn show_queue(State(app): State<QueueState>, session: Option<CurrentSession>) -> Response {
    let Some(player_id) = player(session) else {
        return unauthenticated();
    };
    match status(&app, player_id).await {
        Some(status) => (StatusCode::OK, Json(status)).into_response(),
        None => internal_error(),
    }
}

/// 投入 / 設定の更新。
async fn join_queue(
    State(app): State<QueueState>,
    session: Option<CurrentSession>,
    body: Option<Json<JoinRequest>>,
) -> Response {
    let Some(player_id) = player(session) else {
        return unauthenticated();
    };
    let Some(Json(request)) = body else {
        return bad_request("invalid_request", "リクエストの形式が正しくありません。");
    };

    // 検証は DB を触る前に済ませる（純粋関数）
    let validated = match validate_request(request.down_range, &request.roles) {
        Ok(validated) => validated,
        Err(error) => return invalid_request(&error),
    };

    // 2つのゲート。どちらかに引っかかっていれば投入しない
    let eligibility = match app.registration.queue_eligibility(player_id).await {
        Ok(eligibility) => eligibility,
        Err(error) => {
            error!(%error, "could not evaluate the queue gates");
            return internal_error();
        }
    };
    if !eligibility.is_allowed() {
        let view = EligibilityView::from(&eligibility);
        return (
            StatusCode::FORBIDDEN,
            Json(json!({
                "error": "queue_blocked",
                // 2つのゲートは別々のフィールド。畳んで1つの理由にしない
                "registration": view.registration,
                "pending_reports": view.pending_reports,
                "messages": eligibility.messages(),
            })),
        )
            .into_response();
    }

    match app.queue.enqueue(player_id, &validated, Utc::now()).await {
        Ok(_) => {}
        Err(QueueError::UnknownRole) => {
            return bad_request("unknown_role", "選べないロールが含まれています。");
        }
        Err(QueueError::PlayerNotFound(_)) => return unauthenticated(),
        Err(error) => {
            error!(%error, "could not enqueue");
            return internal_error();
        }
    }

    match status(&app, player_id).await {
        Some(status) => (StatusCode::OK, Json(status)).into_response(),
        None => internal_error(),
    }
}

/// 離脱。自分のエントリだけを消す。
async fn leave_queue(State(app): State<QueueState>, session: Option<CurrentSession>) -> Response {
    let Some(player_id) = player(session) else {
        return unauthenticated();
    };
    match app.queue.leave(player_id).await {
        // キュー中でなくても成功として扱う（離脱の再送は「既に抜けている」で正しい）
        Ok(was_queued) => (
            StatusCode::OK,
            Json(json!({ "queued": false, "was_queued": was_queued })),
        )
            .into_response(),
        Err(error) => {
            error!(%error, "could not leave the queue");
            internal_error()
        }
    }
}

// ----------------------------------------------------------------------
// 補助
// ----------------------------------------------------------------------

/// セッションが指すプレイヤー。**ここ以外から `player_id` を取らない。**
fn player(session: Option<CurrentSession>) -> Option<PlayerId> {
    session.map(|CurrentSession(session)| session.player_id)
}

/// 状態を組み立てる。
/// 失敗の詳細はログにだけ残し、呼び出し側は 500 を返す。
async fn status(app: &QueueState, player_id: PlayerId) -> Option<StatusView> {
    let entry = app
        .queue
        .find(player_id)
        .await
        .inspect_err(|error| error!(%error, "could not read the queue entry"))
        .ok()?;
    let in_range = app
        .queue
        .in_range_count(player_id)
        .await
        .inspect_err(|error| error!(%error, "could not count the players in range"))
        .ok()?;
    let eligibility = app
        .registration
        .queue_eligibility(player_id)
        .await
        .inspect_err(|error| error!(%error, "could not evaluate the queue gates"))
        .ok()?;
    Some(StatusView {
        queued: entry.is_some(),
        entry: entry.map(EntryView::from),
        in_range,
        eligibility: EligibilityView::from(&eligibility),
    })
}

fn unauthenticated() -> Response {
    (
        StatusCode::UNAUTHORIZED,
        Json(json!({ "error": "unauthenticated" })),
    )
        .into_response()
}

fn bad_request(code: &'static str, message: &str) -> Response {
    (
        StatusCode::BAD_REQUEST,
        Json(json!({ "error": code, "message": message })),
    )
        .into_response()
}

/// 検証エラー。**送られてきた設定値は返してよい**（本人が送った値で、秘密ではない）。
fn invalid_request(error: &QueueRequestError) -> Response {
    let code = match error {
        QueueRequestError::DownRangeOutOfBounds(_) => "down_range_out_of_bounds",
        QueueRequestError::NoRoles => "no_roles",
        QueueRequestError::TooManyRoles(_) => "too_many_roles",
        QueueRequestError::DuplicateRole(_) => "duplicate_role",
        QueueRequestError::MalformedPriorities(_) => "malformed_priorities",
        QueueRequestError::EmptyRole => "empty_role",
    };
    (
        StatusCode::BAD_REQUEST,
        Json(json!({ "error": code, "message": error.to_string() })),
    )
        .into_response()
}

fn internal_error() -> Response {
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        Json(json!({ "error": "internal" })),
    )
        .into_response()
}
