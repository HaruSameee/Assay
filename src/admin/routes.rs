//! 管理パネルのエンドポイント。
//!
//! - `GET  /admin` 検索・直近の調整履歴・マッチメイカの停止/再開
//! - `GET  /admin/queue` キュー中の一覧、強制離脱、運営操作の履歴
//! - `POST /admin/matchmaker` マッチメイカの停止/再開（`admin_actions` への追記）
//! - `POST /admin/queue/remove` キューからの強制離脱
//! - `POST /admin/blocks` ブロック枠の変更
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

use crate::blocks::{AdminBlockOverrideError, BlockRepository};
use crate::lifecycle::PlayerId;
use crate::registration::html::error_page;
use crate::registration::{AuthConfig, CurrentSession, Viewer};

use super::actions::{
    AdminActionKind, MAX_BLOCK_ALLOWANCE, normalize_reason, parse_block_allowance,
};
use super::config::AdminIds;
use super::html::{
    MatchmakerView, Notice, NoticeKind, PlayerView, QueueView, SearchView, SeasonView, player_page,
    queue_page, search_page, season_page,
};
use super::repository::{
    ActionOutcome, AdminRepository, AdminStoreError, InsertOutcome, NewAdjustment, NewAdminAction,
    NewSeasonBoundary, RemovalOutcome,
};
use super::season::{
    MAX_SEASON_LABEL_LENGTH, SeasonOutcome, confirmation_matches, normalize_label,
};

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
    pub blocks: BlockRepository,
}

impl AdminState {
    pub fn new(
        config: AuthConfig,
        ids: AdminIds,
        store: AdminRepository,
        blocks: BlockRepository,
    ) -> Self {
        let key = config.cookie_key();
        Self {
            config,
            key,
            ids,
            store,
            blocks,
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
        // 当日の運用。どれも `admin_actions` への追記で表す
        .route("/admin/queue", get(show_queue))
        .route("/admin/matchmaker", post(control_matchmaker))
        .route("/admin/queue/remove", post(remove_from_queue))
        .route("/admin/blocks", post(set_block_allowance))
        .route("/admin/blocks/override", post(override_block))
        // シーズン境界（導出の起点）。2 段階の確認。実行は POST /admin/season
        .route("/admin/season", get(show_season).post(start_season))
        .route("/admin/season/confirm", post(confirm_season))
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
    // 当日の入口の画面。マッチメイカの状態はここでも読めるようにする
    let paused = match app.store.matchmaker_pause().await {
        Ok(paused) => paused,
        Err(err) => {
            error!(%err, "could not read the matchmaker pause state");
            return internal_error();
        }
    };
    let key = form_key();
    Html(search_page(&SearchView {
        query,
        players: &players,
        recent: &recent,
        matchmaker: MatchmakerView {
            paused: paused.as_ref(),
            form_key: &key,
        },
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
    // 枠数も追記ログからの導出値。行が無ければ既定の 2 枠
    let block_allowance = match app.store.block_allowance(player_id).await {
        Ok(allowance) => allowance,
        Err(err) => {
            error!(%err, "could not read the block allowance");
            return internal_error();
        }
    };
    let outgoing_blocks = match app.blocks.current_for(PlayerId(player_id)).await {
        Ok(blocks) => blocks,
        Err(err) => {
            error!(%err, "could not read outgoing blocks");
            return internal_error();
        }
    };
    let incoming_blocks = match app.blocks.blocked_by(PlayerId(player_id)).await {
        Ok(blocks) => blocks,
        Err(err) => {
            error!(%err, "could not read incoming blocks");
            return internal_error();
        }
    };
    // 冪等キーはフォーム描画のたびに発行。同じフォームの再送信は同じキーを持つ
    let form_key = form_key();
    Html(player_page(&PlayerView {
        player: &player,
        adjustments: &adjustments,
        form_key: &form_key,
        block_allowance,
        outgoing_blocks: &outgoing_blocks,
        incoming_blocks: &incoming_blocks,
        notice,
    }))
    .into_response()
}

#[derive(Debug, Deserialize)]
struct BlockOverrideForm {
    #[serde(default)]
    player: String,
    #[serde(default)]
    target: String,
    #[serde(default)]
    key: String,
    #[serde(default)]
    reason: String,
}

async fn override_block(
    State(app): State<AdminState>,
    session: Option<CurrentSession>,
    Form(form): Form<BlockOverrideForm>,
) -> Response {
    let Ok(player_id) = Uuid::parse_str(form.player.trim()) else {
        return not_found();
    };
    let Ok(target_id) = Uuid::parse_str(form.target.trim()) else {
        return not_found();
    };
    let actor = match actor(&app, session).await {
        Ok(actor) => actor,
        Err(error) => return error.into(),
    };
    if form.key.trim().is_empty() {
        return render_player(
            &app,
            player_id,
            Some(fail("操作をやり直してください。".to_owned())),
        )
        .await;
    }
    let reason = normalize_reason(&form.reason);
    match app
        .blocks
        .override_unblock(
            actor.discord_user_id,
            PlayerId(player_id),
            PlayerId(target_id),
            reason,
            &form.key,
            Utc::now(),
        )
        .await
    {
        Ok(()) => {
            render_player(
                &app,
                player_id,
                Some(info(
                    "ブロックを管理解除しました。操作は管理ログに記録されています。".to_owned(),
                )),
            )
            .await
        }
        Err(AdminBlockOverrideError::NotBlocked) => {
            render_player(
                &app,
                player_id,
                Some(fail("このブロックはすでに解除されています。".to_owned())),
            )
            .await
        }
        Err(AdminBlockOverrideError::Database(error)) => {
            error!(%error, "could not override a block");
            internal_error()
        }
        Err(AdminBlockOverrideError::Admin(error)) => {
            error!(%error, "could not record a block override");
            internal_error()
        }
    }
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

// ----------------------------------------------------------------------
// 当日の運用（マッチメイカの停止・再開 / 強制離脱 / ブロック枠）
// ----------------------------------------------------------------------
//
// どれも `admin_actions` への**追記だけ**で表現する（強制離脱の `queue_entries` からの
// DELETE を除く。キューは可変テーブル）。現在の状態は最新行から導出するので、
// 「誰がいつ止めたか」が上書きで消えない。

/// フォームごとに発行する冪等キー。同じフォームの二重送信は同じキーを持つので行が増えない。
fn form_key() -> String {
    Uuid::new_v4().to_string()
}

/// キューと運用の画面を描画する。運用操作の後は必ずここへ戻す（結果と履歴が同じ画面で読める）。
async fn render_queue(app: &AdminState, notice: Option<Notice>) -> Response {
    let paused = match app.store.matchmaker_pause().await {
        Ok(paused) => paused,
        Err(err) => {
            error!(%err, "could not read the matchmaker pause state");
            return internal_error();
        }
    };
    let entries = match app.store.queued_entries().await {
        Ok(entries) => entries,
        Err(err) => {
            error!(%err, "could not list the queue");
            return internal_error();
        }
    };
    let actions = match app.store.recent_actions(RECENT_LIMIT).await {
        Ok(actions) => actions,
        Err(err) => {
            error!(%err, "could not list the recent admin actions");
            return internal_error();
        }
    };
    let key = form_key();
    Html(queue_page(&QueueView {
        matchmaker: MatchmakerView {
            paused: paused.as_ref(),
            form_key: &key,
        },
        entries: &entries,
        actions: &actions,
        form_key: &key,
        now: Utc::now(),
        notice,
    }))
    .into_response()
}

async fn show_queue(State(app): State<AdminState>, session: Option<CurrentSession>) -> Response {
    if let Err(error) = actor(&app, session).await {
        return error.into();
    }
    render_queue(&app, None).await
}

#[derive(Debug, Deserialize)]
struct MatchmakerForm {
    #[serde(default)]
    action: String,
    #[serde(default)]
    key: String,
    #[serde(default)]
    reason: String,
}

/// マッチメイカの停止・再開。
///
/// **状態は DB に追記して表す。**プロセス内のフラグにすると、止めたまま再起動したときに
/// 勝手に動き出す。停止しても `systemctl stop` とは違い、進行中の試合・キュー・報告・
/// 通知はそのまま動き続ける（止まるのは新しい formation を作ることだけ）。
async fn control_matchmaker(
    State(app): State<AdminState>,
    session: Option<CurrentSession>,
    Form(form): Form<MatchmakerForm>,
) -> Response {
    let actor = match actor(&app, session).await {
        Ok(actor) => actor,
        Err(error) => return error.into(),
    };
    // 受け付けるのはこの 2 つだけ。知らない値は 404（画面に無い操作を作らせない）
    let kind = match form.action.trim() {
        "pause" => AdminActionKind::MatchmakerPaused,
        "resume" => AdminActionKind::MatchmakerResumed,
        _ => return not_found(),
    };
    let input = NewAdminAction {
        kind,
        actor_discord_user_id: actor.discord_user_id,
        target_player_id: None,
        reason: normalize_reason(&form.reason),
        payload: serde_json::json!({}),
        occurred_at: Utc::now(),
        idempotency_key: form.key.clone(),
    };
    let notice = match app.store.append_action(&input).await {
        Ok(ActionOutcome::Recorded { .. }) => info(
            match kind {
                AdminActionKind::MatchmakerPaused => {
                    "マッチメイカを停止しました。新しい試合は成立しません。\
                     進行中の試合・キュー・報告はそのまま動きます。"
                }
                _ => "マッチメイカを再開しました。次の実行から試合が成立します。",
            }
            .to_owned(),
        ),
        Ok(ActionOutcome::Duplicate) => {
            info("この操作は既に記録されています。状態は変わっていません。".to_owned())
        }
        Err(err) => {
            error!(%err, "could not record the matchmaker control");
            return internal_error();
        }
    };
    render_queue(&app, Some(notice)).await
}

#[derive(Debug, Deserialize)]
struct RemoveForm {
    #[serde(default)]
    player: String,
    #[serde(default)]
    key: String,
    #[serde(default)]
    reason: String,
}

/// キューからの強制離脱。
///
/// 進行中の formation には触らない（キューと formation は別の系）。理由は任意だが、
/// 書けば**本人のキュー画面にそのまま表示される** — 黙って消すと「バグでキューから外れた」と
/// 受け取られるため。
async fn remove_from_queue(
    State(app): State<AdminState>,
    session: Option<CurrentSession>,
    Form(form): Form<RemoveForm>,
) -> Response {
    let Ok(player_id) = Uuid::parse_str(form.player.trim()) else {
        return not_found();
    };
    let actor = match actor(&app, session).await {
        Ok(actor) => actor,
        Err(error) => return error.into(),
    };
    let input = NewAdminAction {
        kind: AdminActionKind::QueueRemoved,
        actor_discord_user_id: actor.discord_user_id,
        target_player_id: Some(player_id),
        reason: normalize_reason(&form.reason),
        payload: serde_json::json!({}),
        occurred_at: Utc::now(),
        idempotency_key: form.key.clone(),
    };
    let notice = match app.store.remove_from_queue(&input).await {
        Ok(RemovalOutcome::Removed) => info(
            "キューから外しました。本人のキュー画面に「運営によってキューから外されました」と\
             表示されます。"
                .to_owned(),
        ),
        // 記録も残していない（身に覚えのない通知を本人に出さない）
        Ok(RemovalOutcome::NotQueued) => {
            fail("そのプレイヤーは既にキューにいません。記録は残していません。".to_owned())
        }
        Err(err) => {
            error!(%err, "could not remove a player from the queue");
            return internal_error();
        }
    };
    render_queue(&app, Some(notice)).await
}

#[derive(Debug, Deserialize)]
struct BlockForm {
    #[serde(default)]
    player: String,
    #[serde(default)]
    key: String,
    #[serde(default)]
    allowance: String,
    #[serde(default)]
    reason: String,
}

/// ブロック枠の変更。現在の有効ブロック数は、`player_blocks` の最新行から導出する。
async fn set_block_allowance(
    State(app): State<AdminState>,
    session: Option<CurrentSession>,
    Form(form): Form<BlockForm>,
) -> Response {
    let Ok(player_id) = Uuid::parse_str(form.player.trim()) else {
        return not_found();
    };
    let actor = match actor(&app, session).await {
        Ok(actor) => actor,
        Err(error) => return error.into(),
    };
    // 存在しないプレイヤーは 404 に寄せる（追記してから FK 違反を 500 にしない）
    match app.store.find_player(player_id).await {
        Ok(Some(_)) => {}
        Ok(None) => return not_found(),
        Err(err) => {
            error!(%err, "could not read the player before setting the block allowance");
            return internal_error();
        }
    }
    let Some(allowance) = parse_block_allowance(&form.allowance) else {
        let notice = fail(format!(
            "ブロック枠は 0〜{MAX_BLOCK_ALLOWANCE} の整数で入力してください。"
        ));
        return render_player(&app, player_id, Some(notice)).await;
    };
    let input = NewAdminAction {
        kind: AdminActionKind::BlockAllowanceSet,
        actor_discord_user_id: actor.discord_user_id,
        target_player_id: Some(player_id),
        reason: normalize_reason(&form.reason),
        payload: serde_json::json!({ "allowance": allowance }),
        occurred_at: Utc::now(),
        idempotency_key: form.key.clone(),
    };
    let notice = match app.store.append_action(&input).await {
        Ok(ActionOutcome::Recorded { .. }) => {
            info(format!("ブロック枠を {allowance} 枠に変更しました。"))
        }
        Ok(ActionOutcome::Duplicate) => {
            info("この操作は既に記録されています。枠数は変わっていません。".to_owned())
        }
        Err(err) => {
            error!(%err, "could not record the block allowance");
            return internal_error();
        }
    };
    render_player(&app, player_id, Some(notice)).await
}

// ----------------------------------------------------------------------
// シーズン境界（導出の起点を追記する）
// ----------------------------------------------------------------------
//
// **レートのリセットではない。**画面の文言も「新しいシーズンを開始する
// （ここより前の試合を集計から外す）」で統一する。
//
// 2 段階の確認: 1 段目は警告と件数、2 段目は**同じラベルの打ち直し**。
// クリック 2 回だと誤操作がそのまま通る。

/// シーズン画面を描画する。件数は毎回数え直す（保存しない）。
async fn render_season(
    app: &AdminState,
    confirming: Option<&str>,
    notice: Option<Notice>,
) -> Response {
    let effective_at = Utc::now();
    let preview = match app.store.season_preview(effective_at).await {
        Ok(preview) => preview,
        Err(err) => {
            error!(%err, "could not count the season preview");
            return internal_error();
        }
    };
    let key = form_key();
    Html(season_page(&SeasonView {
        preview: &preview,
        effective_at,
        form_key: &key,
        confirming,
        notice,
    }))
    .into_response()
}

async fn show_season(State(app): State<AdminState>, session: Option<CurrentSession>) -> Response {
    if let Err(error) = actor(&app, session).await {
        return error.into();
    }
    render_season(&app, None, None).await
}

#[derive(Debug, Deserialize)]
struct SeasonConfirmForm {
    #[serde(default)]
    label: String,
    #[serde(default)]
    key: String,
}

/// 1 段目 → 2 段目。ラベルを検証して確認画面を出すだけで、まだ何も書かない。
async fn confirm_season(
    State(app): State<AdminState>,
    session: Option<CurrentSession>,
    Form(form): Form<SeasonConfirmForm>,
) -> Response {
    if let Err(error) = actor(&app, session).await {
        return error.into();
    }
    let Some(label) = normalize_label(&form.label) else {
        let notice = fail(format!(
            "シーズン名を 1〜{MAX_SEASON_LABEL_LENGTH} 文字で入力してください（改行は使えません）。"
        ));
        return render_season(&app, None, Some(notice)).await;
    };
    // 冪等キーは 1 段目で発行したものを 2 段目へ持ち越す（同じ操作は 1 行にしかならない）
    render_season_with_key(&app, &label, &form.key, None).await
}

/// 2 段目の描画。冪等キーを引き継ぐので [`render_season`] とは別にしてある。
async fn render_season_with_key(
    app: &AdminState,
    label: &str,
    key: &str,
    notice: Option<Notice>,
) -> Response {
    let effective_at = Utc::now();
    let preview = match app.store.season_preview(effective_at).await {
        Ok(preview) => preview,
        Err(err) => {
            error!(%err, "could not count the season preview");
            return internal_error();
        }
    };
    Html(season_page(&SeasonView {
        preview: &preview,
        effective_at,
        form_key: key,
        confirming: Some(label),
        notice,
    }))
    .into_response()
}

#[derive(Debug, Deserialize)]
struct SeasonStartForm {
    #[serde(default)]
    label: String,
    #[serde(default)]
    confirm_label: String,
    #[serde(default)]
    key: String,
}

/// 2 段目の送信。ラベルの打ち直しが一致したときだけ追記する。
async fn start_season(
    State(app): State<AdminState>,
    session: Option<CurrentSession>,
    Form(form): Form<SeasonStartForm>,
) -> Response {
    let actor = match actor(&app, session).await {
        Ok(actor) => actor,
        Err(error) => return error.into(),
    };
    let Some(label) = normalize_label(&form.label) else {
        return render_season(&app, None, Some(fail("シーズン名が不正です。".to_owned()))).await;
    };
    if !confirmation_matches(&label, &form.confirm_label) {
        let notice = fail("シーズン名が一致しません。同じ文字列を入力してください。".to_owned());
        return render_season_with_key(&app, &label, &form.key, Some(notice)).await;
    }

    let now = Utc::now();
    let input = NewSeasonBoundary {
        effective_at: now,
        label: label.clone(),
        actor_discord_user_id: actor.discord_user_id,
        created_at: now,
        idempotency_key: form.key.clone(),
    };
    let notice = match app.store.start_season(&input).await {
        Ok(SeasonOutcome::Started { id }) => info(format!(
            "シーズン「{label}」を開始しました（境界 #{id}）。ここより前の試合は集計から外れます。\
             ログは 1 行も消えていません。"
        )),
        Ok(SeasonOutcome::Duplicate) => {
            info("この操作は既に記録されています。境界は増やしていません。".to_owned())
        }
        Ok(SeasonOutcome::Blocked { active_formations }) => fail(format!(
            "進行中の formation が {active_formations} 件あるため実行しませんでした。\
             終わってからやり直してください。"
        )),
        Err(err) => {
            error!(%err, "could not append the season boundary");
            return internal_error();
        }
    };
    render_season(&app, None, Some(notice)).await
}
