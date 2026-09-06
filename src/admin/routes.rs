//! 管理パネルのエンドポイント。
//!
//! - `GET /admin` 検索と直近の調整履歴
//! - `GET /admin/player?player=<uuid>` 1 プレイヤーの調整履歴と追記フォーム
//! - `POST /admin/adjustments` 調整の追記
//! - `POST /admin/revert` 既存調整の打ち消し（打ち消し行の追記）
//!
//! **管理者の判定はセッションの Discord ID と環境変数リストの照合だけ。**
//! クエリ・ヘッダ・フォームで管理者を名乗る経路は無い。セッションが無い、
//! プレイヤーが見つからない、ID がリストに無い — いずれも **404** を返す
//! （403 を返さない。この画面の存在自体を管理者以外に教えない）。
//! ルート自体の未マウント（環境変数未設定）は `main.rs` の側で行う。

use axum::extract::{FromRef, Query, State};
use axum::http::StatusCode;
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Form, Router};
use axum_extra::extract::cookie::Key;
use chrono::Utc;
use serde::Deserialize;
use tracing::error;
use uuid::Uuid;

use crate::registration::html::error_page;
use crate::registration::{AuthConfig, CurrentSession, Viewer};

use super::config::AdminIds;
use super::html::{Notice, NoticeKind, PlayerView, SearchView, player_page, search_page};
use super::repository::{AdminRepository, AdminStoreError, InsertOutcome, NewAdjustment};

/// 検索のヒット上限と、一覧に出す直近ログの件数。
const SEARCH_LIMIT: i64 = 25;
const RECENT_LIMIT: i64 = 50;

/// 管理パネルが持つ状態。`ids` は起動時に確定した管理者リスト。
#[derive(Clone)]
pub struct AdminState {
    /// セッション Cookie を読むための設定。
    pub config: AuthConfig,
    key: Key,
    ids: AdminIds,
    pub store: AdminRepository,
}

impl AdminState {
    pub fn new(config: AuthConfig, ids: AdminIds, store: AdminRepository) -> Self {
        let key = config.cookie_key();
        Self {
            config,
            key,
            ids,
            store,
        }
    }
}

impl FromRef<AdminState> for Key {
    fn from_ref(state: &AdminState) -> Self {
        state.key.clone()
    }
}

impl FromRef<AdminState> for AuthConfig {
    fn from_ref(state: &AdminState) -> Self {
        state.config.clone()
    }
}

/// ルーターを組み立てる。**管理者リストが設定されたときだけ**呼び出すこと
/// （未設定で呼び出すと、管理者 0 人のパネルがマウントされてしまう）。
pub fn router(state: AdminState) -> Router {
    Router::new()
        .route("/admin", get(show_search))
        .route("/admin/player", get(show_player))
        .route("/admin/adjustments", post(create_adjustment))
        .route("/admin/revert", post(create_revert))
        .with_state(state)
}

/// 照合を通過した管理者。判定に使う Discord ID はセッション → DB 経由の値だけ。
struct Actor {
    discord_user_id: i64,
}

/// 照合の不都合（`api.rs` の `ApiError` と同じ形。エラー型を小さく保つ）。
enum ActorError {
    NotFound,
    Internal,
}

impl From<ActorError> for Response {
    fn from(error: ActorError) -> Self {
        match error {
            ActorError::NotFound => not_found(),
            ActorError::Internal => internal_error(),
        }
    }
}

/// セッション → Discord ID → 管理者リスト。通らなければ 404 を返す。
/// DB が読めないときだけ 500（認証の合否ではなく障害なので、形を変えて出す）。
async fn actor(app: &AdminState, session: Option<CurrentSession>) -> Result<Actor, ActorError> {
    let Some(CurrentSession(session)) = session else {
        return Err(ActorError::NotFound);
    };
    let discord_user_id = match app.store.discord_user_id(session.player_id.0).await {
        Ok(Some(discord_user_id)) => discord_user_id,
        Ok(None) => return Err(ActorError::NotFound),
        Err(err) => {
            error!(%err, "could not read the discord id for a session");
            return Err(ActorError::Internal);
        }
    };
    if !app.ids.contains(discord_user_id) {
        return Err(ActorError::NotFound);
    }
    Ok(Actor { discord_user_id })
}

/// 「ここには何も無い」。非管理者・不明セッション・不明 id を同じ応答に寄せる。
fn not_found() -> Response {
    (
        StatusCode::NOT_FOUND,
        Html(error_page(
            // 存在を教えない。ヘッダーの導線も出さない
            Viewer::ANONYMOUS,
            "見つかりません",
            "お探しのページは見つかりませんでした。",
        )),
    )
        .into_response()
}

fn internal_error() -> Response {
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        Html(error_page(
            Viewer::ADMIN,
            "エラー",
            "管理パネルを利用できません。時間をおいてお試しください。",
        )),
    )
        .into_response()
}

fn info(message: String) -> Notice {
    Notice {
        kind: NoticeKind::Info,
        message,
    }
}

fn fail(message: String) -> Notice {
    Notice {
        kind: NoticeKind::Error,
        message,
    }
}

/// 追記結果を運用の言葉に翻訳する。打ち消しの制約違反は DB が正（回避しない）。
fn store_error_notice(error: &AdminStoreError) -> Option<Notice> {
    match error {
        AdminStoreError::AlreadyReverted => {
            Some(fail("この調整は既に打ち消されています。".to_owned()))
        }
        AdminStoreError::RevertOfRevert => {
            Some(fail("打ち消し行そのものは打ち消せません。".to_owned()))
        }
        AdminStoreError::OriginalMissing => {
            Some(fail("打ち消し対象の調整が見つかりません。".to_owned()))
        }
        // 逆符号の不一致はアプリが計算を間違えたときだけ。正常系では来ない
        AdminStoreError::NegationMismatch => Some(fail(
            "打ち消しの増減が元の調整と相殺できません。".to_owned(),
        )),
        AdminStoreError::Database(_) => None,
    }
}

#[derive(Debug, Deserialize)]
struct SearchQuery {
    #[serde(default)]
    q: String,
}

async fn show_search(
    State(app): State<AdminState>,
    session: Option<CurrentSession>,
    Query(params): Query<SearchQuery>,
) -> Response {
    if let Err(error) = actor(&app, session).await {
        return error.into();
    }
    let query = params.q.trim();
    let players = if query.is_empty() {
        Vec::new()
    } else {
        match app.store.search_players(query, SEARCH_LIMIT).await {
            Ok(players) => players,
            Err(err) => {
                error!(%err, "player search failed");
                return internal_error();
            }
        }
    };
    let recent = match app.store.recent_adjustments(RECENT_LIMIT).await {
        Ok(recent) => recent,
        Err(err) => {
            error!(%err, "could not list recent adjustments");
            return internal_error();
        }
    };
    Html(search_page(&SearchView {
        query,
        players: &players,
        recent: &recent,
        notice: None,
    }))
    .into_response()
}

#[derive(Debug, Deserialize)]
struct PlayerQuery {
    #[serde(default)]
    player: String,
}

async fn show_player(
    State(app): State<AdminState>,
    session: Option<CurrentSession>,
    Query(params): Query<PlayerQuery>,
) -> Response {
    if let Err(error) = actor(&app, session).await {
        return error.into();
    }
    let Ok(player_id) = Uuid::parse_str(params.player.trim()) else {
        return not_found();
    };
    render_player(&app, player_id, None).await
}

/// プレイヤー画面を描画する。見つからない場合は 404 を返す。
async fn render_player(app: &AdminState, player_id: Uuid, notice: Option<Notice>) -> Response {
    let Ok(player) = app.store.find_player(player_id).await else {
        error!("could not read the player");
        return internal_error();
    };
    let Some(player) = player else {
        return not_found();
    };
    let Ok(adjustments) = app.store.adjustments_for(player_id).await else {
        error!("could not read the adjustment history");
        return internal_error();
    };
    // 冪等キーはフォーム描画のたびに発行。同じフォームの再送信は同じキーを持つ
    let form_key = Uuid::new_v4().to_string();
    Html(player_page(&PlayerView {
        player: &player,
        adjustments: &adjustments,
        form_key: &form_key,
        notice,
    }))
    .into_response()
}

#[derive(Debug, Deserialize)]
struct AdjustForm {
    #[serde(default)]
    player: String,
    #[serde(default)]
    key: String,
    #[serde(default)]
    delta_mu: String,
    #[serde(default)]
    delta_sigma: String,
    #[serde(default)]
    reason: String,
}

async fn create_adjustment(
    State(app): State<AdminState>,
    session: Option<CurrentSession>,
    Form(form): Form<AdjustForm>,
) -> Response {
    let Ok(player_id) = Uuid::parse_str(form.player.trim()) else {
        return not_found();
    };
    let actor = match actor(&app, session).await {
        Ok(actor) => actor,
        Err(error) => return error.into(),
    };

    // 理由は必須。空（空白のみ含む）は行にしない
    let reason = form.reason.trim();
    if reason.is_empty() {
        let notice = fail("理由は必須です。理由を入力してから再送信してください。".to_owned());
        return render_player(&app, player_id, Some(notice)).await;
    }
    // 表示スケールのまま受ける（μ / σ への変換は DB 側の既存経路）
    let delta_mu = match parse_delta(&form.delta_mu) {
        Some(value) => value,
        None => {
            let notice = fail("レート増減は数値で入力してください（例: +10、-25）。".to_owned());
            return render_player(&app, player_id, Some(notice)).await;
        }
    };
    let delta_sigma = match parse_delta(&form.delta_sigma) {
        Some(value) => value,
        None => {
            let notice = fail("σ 増減は数値で入力してください（通常は 0）。".to_owned());
            return render_player(&app, player_id, Some(notice)).await;
        }
    };
    if delta_mu == 0.0 && delta_sigma == 0.0 {
        let notice = fail("増減が 0 です。調整が必要な値を入力してください。".to_owned());
        return render_player(&app, player_id, Some(notice)).await;
    }

    // created_by は行動中の管理者から自動で埋める（フォーム値は一切使わない）
    let input = NewAdjustment {
        player_id,
        delta_mu,
        delta_sigma,
        reason: reason.to_owned(),
        created_by: format!("discord:{}", actor.discord_user_id),
        created_at: Utc::now(),
        idempotency_key: form.key.clone(),
        reverts_id: None,
    };
    let notice = match app.store.insert(&input).await {
        Ok(InsertOutcome::Inserted { id }) => info(format!(
            "調整 #{id} を追記しました。レートとマッチング範囲に反映されます。"
        )),
        Ok(InsertOutcome::Duplicate { existing }) => info(format!(
            "この送信は既に調整 #{} として記録されています。行は増やしていません。",
            existing.id
        )),
        Err(AdminStoreError::Database(err)) => {
            error!(%err, "adjustment insert failed");
            return internal_error();
        }
        Err(err) => {
            // 通常行の追記で打ち消し系の制約には到達しない。万が一は表示に落とす
            match store_error_notice(&err) {
                Some(notice) => notice,
                None => return internal_error(),
            }
        }
    };
    render_player(&app, player_id, Some(notice)).await
}

#[derive(Debug, Deserialize)]
struct RevertForm {
    #[serde(default)]
    adjustment_id: String,
    #[serde(default)]
    key: String,
}

async fn create_revert(
    State(app): State<AdminState>,
    session: Option<CurrentSession>,
    Form(form): Form<RevertForm>,
) -> Response {
    let Ok(adjustment_id) = form.adjustment_id.trim().parse::<i64>() else {
        return not_found();
    };
    let actor = match actor(&app, session).await {
        Ok(actor) => actor,
        Err(error) => return error.into(),
    };
    let original = match app.store.find_adjustment(adjustment_id).await {
        Ok(Some(original)) => original,
        Ok(None) => return not_found(),
        Err(err) => {
            error!(%err, "could not read the adjustment to revert");
            return internal_error();
        }
    };

    // 逆符号の組を作って追記する。整合（相殺・再打ち消し禁止・二重打ち消し禁止）の
    // 判定は DB のトリガーと unique index が持つ — ここでは先に弾かない
    let input = NewAdjustment {
        player_id: original.player_id,
        delta_mu: -original.delta_mu,
        delta_sigma: -original.delta_sigma,
        reason: format!("revert of #{}", original.id),
        created_by: format!("discord:{}", actor.discord_user_id),
        created_at: Utc::now(),
        idempotency_key: form.key.clone(),
        reverts_id: Some(original.id),
    };
    let notice = match app.store.insert(&input).await {
        Ok(InsertOutcome::Inserted { id }) => info(format!(
            "調整 #{} を打ち消しました（#{id} を追記）。レートに反映されます。",
            original.id
        )),
        Ok(InsertOutcome::Duplicate { existing }) => info(format!(
            "この打ち消しは既に #{id} として記録されています。行は増やしていません。",
            id = existing.id
        )),
        Err(AdminStoreError::Database(err)) => {
            error!(%err, "revert insert failed");
            return internal_error();
        }
        Err(err) => match store_error_notice(&err) {
            Some(notice) => notice,
            None => return internal_error(),
        },
    };
    render_player(&app, original.player_id, Some(notice)).await
}

/// 入力された増減を読む。空・非数・NaN / inf は拒否する。
fn parse_delta(raw: &str) -> Option<f64> {
    let value: f64 = raw.trim().parse().ok()?;
    if value.is_finite() { Some(value) } else { None }
}
