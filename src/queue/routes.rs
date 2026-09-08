//! キューの画面と JSON API。
//!
//! - `GET    /queue` 自分のエントリと「範囲内でキュー中の人数」
//! - `POST   /queue` 投入 / 設定の更新
//! - `DELETE /queue` 離脱
//!
//! **他人のエントリは触れない。**対象の `player_id` はすべてセッション
//! （[`CurrentSession`]）から取り、リクエストボディからは読まない。
//!
//! **3つのゲートは畳まない。**拒否のレスポンスは `registration` / `active_formation` /
//! `pending_reports` を別々のフィールドで返す（05 §4 / 07 §4-5）。1つの真偽値にすると、キュー画面で
//! 「なぜ入れないのか」を説明できなくなる。
//!
//! CSRF: セッション Cookie は `SameSite=Lax` なので、他サイトからの POST / DELETE には
//! 送られない。登録トラックのフォームと同じ前提に拠る。

use axum::extract::{FromRef, Query, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use axum_extra::extract::cookie::Key;
use chrono::Utc;
use serde::{Deserialize, Serialize};
use serde_json::json;
use tracing::error;

use uuid::Uuid;

use crate::admin::AdminIds;
use crate::lifecycle::{FormationId, PlayerId};
use crate::registration::{
    Audience, AuthConfig, CurrentSession, QueueEligibility, RegistrationBlock,
    RegistrationRepository, Viewer,
};

use super::model::{
    QueueEntry, QueueRequestError, RolePreference, StaffRemoval, UP_RANGE, VoiceDestination,
    validate_request,
};
use super::repository::{QueueError, QueueRepository};

/// パス。
const QUEUE_PATH: &str = "/queue";

/// キューのルーターが持つ状態。
#[derive(Clone)]
pub struct QueueState {
    /// セッション Cookie を読むための設定。
    pub config: AuthConfig,
    /// 3つのゲートの評価に使う。
    pub registration: RegistrationRepository,
    /// キューの永続化。
    pub queue: QueueRepository,
    /// Cookie の署名鍵（`config` から導出したものを保持する）。
    key: Key,
    /// 共通ヘッダーに管理パネルの導線を出す相手の判定。既定は「管理者なし」。
    audience: Audience,
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
            audience: Audience::default(),
        }
    }

    /// 管理者リストを渡す。**渡さなければ誰にも管理パネルの導線を出さない。**
    pub fn with_admins(mut self, admins: Option<AdminIds>) -> Self {
        self.audience = Audience::new(admins);
        self
    }

    /// ランキングの公開状態を渡す。共通ヘッダーに導線を出すかどうかだけに使う。
    pub fn with_ranking(mut self, public: bool) -> Self {
        self.audience.set_ranking(public);
        self
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

/// 投入リクエスト。**マッチング範囲は受け取らない**（上下 50 固定）。
///
/// 古い画面が `down_range` を送ってきても、未知のフィールドとして無視される。
#[derive(Debug, Deserialize)]
struct JoinRequest {
    /// 希望ロール。1〜3、優先度付き。
    #[serde(default)]
    roles: Vec<RolePreference>,
}

/// 登録ゲートの理由。**報告ゲートとは別のフィールドで返す。**
#[derive(Debug, Serialize)]
pub(super) struct RegistrationBlockView {
    /// 機械可読な理由。
    pub(super) code: &'static str,
    /// 画面に出す説明。
    pub(super) message: String,
    /// 直近の申告ランク（`rank_below_entry` のときだけ）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) declared: Option<&'static str>,
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

/// 3つのゲートの状態。畳まない。
#[derive(Debug, Serialize)]
pub(super) struct EligibilityView {
    /// 登録ゲート。通っていれば `null`。
    pub(super) registration: Option<RegistrationBlockView>,
    /// 進行中ゲート。成立済みでまだ始まっていない formation。無ければ `null`。
    pub(super) active_formation: Option<Uuid>,
    /// 報告ゲート。未報告の formation。空なら通過。
    pub(super) pending_reports: Vec<Uuid>,
}

impl From<&QueueEligibility> for EligibilityView {
    fn from(eligibility: &QueueEligibility) -> Self {
        Self {
            registration: eligibility.registration.as_ref().map(Into::into),
            active_formation: eligibility.active_formation.map(|FormationId(id)| id),
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
pub(super) struct EntryView {
    /// `queue_entries.id`。
    pub(super) id: Uuid,
    /// 所有者。常にセッションのプレイヤー。
    pub(super) player_id: Uuid,
    /// 最初にキューへ入った時刻。設定変更では動かない。RFC 3339。
    pub(super) enqueued_at: String,
    /// 設定を最後に変えた時刻。RFC 3339。
    pub(super) updated_at: String,
    /// 下方向のマッチング範囲。
    pub(super) down_range: i32,
    /// 上方向のマッチング範囲。**固定値**を毎回返す（UI が定数を持たないで済むように）。
    pub(super) up_range: i32,
    /// 希望ロール。`priority` の昇順。
    pub(super) roles: Vec<RolePreference>,
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
pub(super) struct StatusView {
    /// キュー中か。
    pub(super) queued: bool,
    /// 自分のエントリ。キュー中でなければ `null`。
    pub(super) entry: Option<EntryView>,
    /// 自分の範囲内でキュー中の人数（相互条件、07 §3-1）。
    ///
    /// 範囲は上下 50 固定なので、キューに入る前でも決まる（読み取りに失敗したときだけ `null`）。
    pub(super) in_range: Option<usize>,
    /// 3つのゲートの現在の状態。キュー中でも進行中・報告ゲートは後から立ちうる。
    pub(super) eligibility: EligibilityView,
    /// 成立済みなら、`vc_assigned` のログから読んだ本人側の集合先。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) destination: Option<DestinationView>,
    /// 運営にキューから外された直後なら、その通知。**黙って消さない。**
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) removed_by_staff: Option<RemovalView>,
}

/// 「運営によってキューから外されました」。本人にだけ返す。
#[derive(Debug, Serialize)]
pub(super) struct RemovalView {
    /// 外された時刻。RFC 3339。
    pub(super) removed_at: String,
    /// 運営が書いた理由（書かれていなければ `null`）。
    pub(super) reason: Option<String>,
}

impl From<StaffRemoval> for RemovalView {
    fn from(removal: StaffRemoval) -> Self {
        Self {
            removed_at: removal.removed_at.to_rfc3339(),
            reason: removal.reason,
        }
    }
}

/// キュー画面に出す本人側の集合先。
#[derive(Debug, Serialize)]
pub(super) struct DestinationView {
    pub(super) formation_id: Uuid,
    pub(super) team: String,
    pub(super) channel_name: String,
    pub(super) channel_id: String,
}

impl From<VoiceDestination> for DestinationView {
    fn from(destination: VoiceDestination) -> Self {
        let channel_name = destination.channel_name();
        Self {
            formation_id: destination.formation_id.0,
            team: destination.team,
            channel_name,
            channel_id: destination.channel_id,
        }
    }
}

// ----------------------------------------------------------------------
// ハンドラ
// ----------------------------------------------------------------------

/// 状態取得。
/// `/queue` のクエリ。**表示にしか使わない。**
#[derive(Debug, Default, Deserialize)]
struct ShowQueueParams {
    /// 直前に報告した試合。マッチ画面へ戻る導線をこの id で描く。
    ///
    /// 値は本人が持ち込むが、リンクを描くだけで権限判断には使わない
    /// （`/match/{id}` 側が、参加者でなければ 404 を返す）。
    #[serde(default)]
    reported: Option<Uuid>,
}

async fn show_queue(
    State(app): State<QueueState>,
    session: Option<CurrentSession>,
    Query(params): Query<ShowQueueParams>,
    headers: HeaderMap,
) -> Response {
    let html = accepts_html(&headers);
    let Some(player_id) = player(session) else {
        return if html {
            (
                StatusCode::UNAUTHORIZED,
                Html(crate::registration::html::error_page(
                    Viewer::ANONYMOUS,
                    "ログインが必要です",
                    "セッションが切れているか、まだログインしていません。入口からDiscordでログインしてください。",
                )),
            )
                .into_response()
        } else {
            unauthenticated()
        };
    };
    // 範囲は固定なので、未投入時の人数プレビューに指定するものが無い。画面（HTML）を
    // 開いたときだけ数える（JSON のポーリングは投入前の人数を要求しない）
    match status(&app, player_id).await {
        Some(status) if html => {
            // 前回のロール希望でフォームを埋める。**画面にしか使わない**ので
            // JSON の `StatusView` には載せない。読めなければ空のまま出す
            let prefill = if status.entry.is_none() {
                app.queue
                    .last_role_preferences(player_id)
                    .await
                    .inspect_err(|error| {
                        error!(%error, "could not read the previous role preferences");
                    })
                    .unwrap_or_default()
            } else {
                Vec::new()
            };
            (
                StatusCode::OK,
                Html(super::html::queue_page(
                    // ヘッダーの描画にだけ必要。ポーリングされる JSON 側では引かない
                    viewer(&app, player_id).await,
                    &status,
                    Utc::now(),
                    params.reported.map(FormationId),
                    &prefill,
                )),
            )
                .into_response()
        }
        Some(status) => (StatusCode::OK, Json(status)).into_response(),
        None => internal_error(),
    }
}

/// 共通ヘッダー用の見ている人。プロフィールが読めなければ導線を出さない側へ倒す。
async fn viewer(app: &QueueState, player_id: PlayerId) -> Viewer {
    match app.registration.find_by_player_id(player_id).await {
        Ok(profile) => app.audience.viewer_of(profile.as_ref()),
        Err(error) => {
            error!(%error, "could not read the profile for the header");
            Viewer::MEMBER
        }
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
    let validated = match validate_request(&request.roles) {
        Ok(validated) => validated,
        Err(error) => return invalid_request(&error),
    };

    // 3つのゲート。どれかに引っかかっていれば投入しない
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
                // 3つのゲートは別々のフィールド。畳んで1つの理由にしない
                "registration": view.registration,
                "active_formation": view.active_formation,
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
    // 範囲は固定なので、キューに入る前でも人数が決まる（旧仕様の「プレビュー」は不要）
    let in_range = if entry.is_some() {
        app.queue.in_range_count(player_id).await
    } else {
        app.queue.preview_in_range_count(player_id).await.map(Some)
    }
    .inspect_err(|error| error!(%error, "could not count the players in range"))
    .ok()?;
    let eligibility = app
        .registration
        .queue_eligibility(player_id)
        .await
        .inspect_err(|error| error!(%error, "could not evaluate the queue gates"))
        .ok()?;
    let destination = if let Some(FormationId(formation_id)) = eligibility.active_formation {
        app.queue
            .voice_destination(FormationId(formation_id), player_id)
            .await
            .inspect_err(|error| error!(%error, "could not read the assigned voice channel"))
            .ok()?
            .map(Into::into)
    } else {
        None
    };
    // キュー中の人には出さない（外された後に自分で入り直した人に、古い通知を見せない）
    let removed_by_staff = if entry.is_none() {
        app.queue
            .staff_removal(player_id, Utc::now())
            .await
            .inspect_err(|error| error!(%error, "could not read the staff removal notice"))
            .ok()?
            .map(Into::into)
    } else {
        None
    };
    Some(StatusView {
        queued: entry.is_some(),
        entry: entry.map(EntryView::from),
        in_range,
        eligibility: EligibilityView::from(&eligibility),
        destination,
        removed_by_staff,
    })
}

/// Browser navigation asks for HTML; fetch/tests without that Accept value keep the JSON API.
fn accepts_html(headers: &HeaderMap) -> bool {
    headers
        .get(header::ACCEPT)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| {
            value
                .split(',')
                .any(|item| item.trim().starts_with("text/html"))
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
