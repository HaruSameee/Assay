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
use crate::lifecycle::{FormationId, PlayerId};
use crate::persistence::{Outcome, Team};
use crate::quiet::QuietReportRepository;
use crate::registration::html::error_page;
use crate::registration::{AuthConfig, CurrentSession, Viewer};

use super::actions::{
    AdminActionKind, MAX_BLOCK_ALLOWANCE, normalize_reason, parse_block_allowance,
};
use super::config::AdminIds;
use super::disputes::{
    DisputeOutcome, NewDisputeResolution, NewReadjudication, ReadjudicationOutcome, parse_decision,
};
use crate::nullified::NullifiedReportRepository;

use super::block_cooldown::{BlockCooldownOutcome, parse_cooldown_days};
use super::html::{
    BlockCooldownView, MatchingRangeView, MatchmakerView, Notice, NoticeKind, PlayerView,
    QueueView, QuietReportsView, RatingParamsView, ReportsView, RosterView, ScheduleView,
    SearchView, SeasonView, ShadowBansView, block_cooldown_page, matching_range_page, player_page,
    queue_page, quiet_reports_page, rating_params_page, reports_page, roster_page, search_page,
    season_page, shadow_bans_page,
};
use super::matching_range::{MatchingRangeOutcome, parse_ranges};
use super::rating_params::{MAX_BASE_DELTA, RatingParamsOutcome, normalize_base_delta};
use super::repository::{
    ActionOutcome, AdminRepository, AdminStoreError, InsertOutcome, NewAdjustment, NewAdminAction,
    NewBlockCooldown, NewMatchingRange, NewMatchmakerSchedule, NewRatingParams, NewSeasonBoundary,
    NewShadowBan, NewStaffReport, RemovalOutcome, StaffReportOutcome,
};
use super::schedule::{MatchmakerSchedule, ScheduleOutcome, parse_days, parse_time};
use super::season::{
    MAX_SEASON_LABEL_LENGTH, SeasonOutcome, confirmation_matches, normalize_label,
};
use super::shadow_bans::{BanSubject, ShadowBanKind, ShadowBanOutcome};

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
    /// 聞き専報告。**運営パネルだけがこれを読む**（票数と閾値を見られる唯一の経路）。
    pub quiet: QuietReportRepository,
    /// 無効試合の通報。**運営パネルだけがこれを読む**（票数と閾値を見られる唯一の経路）。
    pub nullified: NullifiedReportRepository,
}

impl AdminState {
    pub fn new(
        config: AuthConfig,
        ids: AdminIds,
        store: AdminRepository,
        blocks: BlockRepository,
        quiet: QuietReportRepository,
        nullified: NullifiedReportRepository,
    ) -> Self {
        let key = config.cookie_key();
        Self {
            config,
            key,
            ids,
            store,
            blocks,
            quiet,
            nullified,
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
        .route("/admin/players", get(show_roster))
        .route("/admin/player", get(show_player))
        .route("/admin/adjustments", post(create_adjustment))
        .route("/admin/revert", post(create_revert))
        // 当日の運用。どれも `admin_actions` への追記で表す
        .route("/admin/queue", get(show_queue))
        .route("/admin/matchmaker", post(control_matchmaker))
        // 稼働スケジュール。**手動の停止には触らない**（枠を書くだけ）
        .route("/admin/matchmaker/schedule", post(set_matchmaker_schedule))
        .route("/admin/queue/remove", post(remove_from_queue))
        .route("/admin/blocks", post(set_block_allowance))
        .route("/admin/blocks/override", post(override_block))
        // シーズン境界（導出の起点）。2 段階の確認。実行は POST /admin/season
        .route("/admin/reports", get(show_reports).post(enter_report))
        // 係争の裁定。**終端イベントを 1 行積むだけ**（報告は書き換えない）
        .route("/admin/reports/dispute", post(resolve_dispute))
        // 終わった試合の裁定のやり直し。同じく終端イベントを 1 行積むだけ
        .route("/admin/reports/readjudicate", post(readjudicate))
        .route("/admin/season", get(show_season).post(start_season))
        // レート増減幅。2 段階の確認。実行は POST /admin/rating-params
        .route(
            "/admin/rating-params",
            get(show_rating_params).post(change_rating_params),
        )
        .route("/admin/rating-params/confirm", post(confirm_rating_params))
        // マッチング範囲。**1 段階**（過去を変えない・追記で戻せる操作なので）
        .route(
            "/admin/matching-range",
            get(show_matching_range).post(change_matching_range),
        )
        // ブロック解除の待機日数。1 段階（試合に影響しない。追記で戻せる）
        .route(
            "/admin/block-cooldown",
            get(show_block_cooldown).post(change_block_cooldown),
        )
        // シャドウバン（IP / Discord ユーザー ID）。1 段階（試合に影響しない。解除で戻せる）
        .route(
            "/admin/shadow-bans",
            get(show_shadow_bans).post(change_shadow_ban),
        )
        // 聞き専報告。**閲覧だけ**（運営が代理で入れる経路は作らない）
        .route("/admin/quiet-reports", get(show_quiet_reports))
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

#[derive(Debug, Deserialize)]
struct RosterQuery {
    #[serde(default)]
    q: String,
    #[serde(default)]
    page: Option<i64>,
}

/// 登録者の一覧。**検索と違って、何も入力しなくても全員が出る。**
async fn show_roster(
    State(app): State<AdminState>,
    session: Option<CurrentSession>,
    Query(params): Query<RosterQuery>,
) -> Response {
    if let Err(error) = actor(&app, session).await {
        return error.into();
    }
    let page = match app.store.roster(&params.q, params.page.unwrap_or(1)).await {
        Ok(page) => page,
        Err(err) => {
            error!(%err, "could not list the registered players");
            return internal_error();
        }
    };
    Html(roster_page(&RosterView { page: &page })).into_response()
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
    let received_blocks = match app.blocks.received_block_counts().await {
        Ok(rows) => rows,
        Err(err) => {
            error!(%err, "could not count received blocks");
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
        received_blocks: &received_blocks,
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
    let addresses = match app.store.addresses_for(player_id).await {
        Ok(addresses) => addresses,
        Err(err) => {
            error!(%err, "could not read the address records");
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
        addresses: &addresses,
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
    let now = Utc::now();
    // **スケジュールも同じ画面に出す。**手動の停止と取り違えさせないため、
    // 「いま枠の内側か」を別に持って画面に書く
    let schedule = match app.store.matchmaker_schedule().await {
        Ok(schedule) => schedule,
        Err(err) => {
            error!(%err, "could not read the matchmaker schedule");
            return internal_error();
        }
    };
    let key = form_key();
    Html(queue_page(&QueueView {
        matchmaker: MatchmakerView {
            paused: paused.as_ref(),
            form_key: &key,
        },
        schedule: ScheduleView {
            schedule: &schedule,
            open: schedule.is_open(now),
            form_key: &key,
        },
        entries: &entries,
        actions: &actions,
        form_key: &key,
        now,
        notice,
    }))
    .into_response()
}

#[derive(Debug, Deserialize)]
struct ScheduleForm {
    /// チェックボックスなので、外れていればキーごと来ない。
    #[serde(default)]
    enabled: Option<String>,
    // **曜日は 7 つの独立したチェックボックス。**同名キーを 1 つの Vec にまとめる形は
    // `serde_urlencoded`（axum の `Form`）が扱えず 422 になる。依存を増やすより、
    // 名前を分けて `Option` を 7 つ受ける方が素直。
    #[serde(default)]
    day_mon: Option<String>,
    #[serde(default)]
    day_tue: Option<String>,
    #[serde(default)]
    day_wed: Option<String>,
    #[serde(default)]
    day_thu: Option<String>,
    #[serde(default)]
    day_fri: Option<String>,
    #[serde(default)]
    day_sat: Option<String>,
    #[serde(default)]
    day_sun: Option<String>,
    #[serde(default)]
    start: String,
    #[serde(default)]
    end: String,
    /// キュー受付開始（`HH:MM`）。空なら「開始と同時」。
    #[serde(default)]
    queue_open: String,
    #[serde(default)]
    reason: String,
    #[serde(default)]
    key: String,
}

impl ScheduleForm {
    /// チェックの付いた曜日のキーを並べる（`parse_days` に渡す形）。
    fn days(&self) -> Vec<String> {
        [
            ("mon", &self.day_mon),
            ("tue", &self.day_tue),
            ("wed", &self.day_wed),
            ("thu", &self.day_thu),
            ("fri", &self.day_fri),
            ("sat", &self.day_sat),
            ("sun", &self.day_sun),
        ]
        .into_iter()
        .filter(|(_, checked)| checked.is_some())
        .map(|(key, _)| key.to_owned())
        .collect()
    }
}
/// 稼働スケジュールを保存する。**手動の停止には触らない。**
///
/// ここで書くのは枠だけで、いま止まっているかどうかは変えない。
/// 手動で止めているあいだは、枠を変えても止まったままである。
async fn set_matchmaker_schedule(
    State(app): State<AdminState>,
    session: Option<CurrentSession>,
    Form(form): Form<ScheduleForm>,
) -> Response {
    let actor = match actor(&app, session).await {
        Ok(actor) => actor,
        Err(error) => return error.into(),
    };
    let Some(start_minute) = parse_time(&form.start) else {
        return render_queue(&app, Some(fail("開始時刻が不正です。".to_owned()))).await;
    };
    let Some(end_minute) = parse_time(&form.end) else {
        return render_queue(&app, Some(fail("終了時刻が不正です。".to_owned()))).await;
    };
    if start_minute == end_minute {
        let notice = fail(
            "開始と終了が同じ時刻です。24 時間動かしたい場合はスケジュールを外してください。"
                .to_owned(),
        );
        return render_queue(&app, Some(notice)).await;
    }
    // 空欄は「稼働開始と同時に受付」
    let queue_open_minute = if form.queue_open.trim().is_empty() {
        start_minute
    } else {
        let Some(minute) = parse_time(&form.queue_open) else {
            return render_queue(
                &app,
                Some(fail("キュー受付開始の時刻が不正です。".to_owned())),
            )
            .await;
        };
        minute
    };
    let enabled = form.enabled.is_some();
    let days_mask = parse_days(&form.days());
    if enabled && days_mask == 0 {
        let notice = fail(
            "曜日が 1 つも選ばれていません。この設定だとマッチメイカは動きません。".to_owned(),
        );
        return render_queue(&app, Some(notice)).await;
    }
    let Some(reason) = normalize_reason(&form.reason) else {
        return render_queue(&app, Some(fail("理由を入力してください。".to_owned()))).await;
    };

    let schedule = MatchmakerSchedule {
        enabled,
        days_mask,
        start_minute,
        end_minute,
        queue_open_minute,
    };
    if !schedule.queue_open_is_valid() {
        let notice = fail(format!(
            "キュー受付開始は稼働開始（{start}）と同じか、その前 12 時間以内にしてください。",
            start = schedule.start_text(),
        ));
        return render_queue(&app, Some(notice)).await;
    }
    let input = NewMatchmakerSchedule {
        schedule,
        reason,
        actor_discord_user_id: actor.discord_user_id,
        created_at: Utc::now(),
        idempotency_key: form.key.clone(),
    };
    let notice = match app.store.change_matchmaker_schedule(&input).await {
        Ok(ScheduleOutcome::Changed { id }) if enabled => info(format!(
            "スケジュールを保存しました（#{id}）。{days} の {start} 〜 {end}（JST）、\
             キュー受付は {queue_open} から。\
             手動で停止しているあいだは、稼働時間になっても再開しません。",
            days = schedule.day_labels().join(""),
            start = schedule.start_text(),
            end = schedule.end_text(),
            queue_open = schedule.queue_open_text(),
        )),
        Ok(ScheduleOutcome::Changed { id }) => info(format!(
            "スケジュールを外しました（#{id}）。手動の停止・再開だけが効きます。"
        )),
        Ok(ScheduleOutcome::Duplicate) => {
            info("この操作は既に記録されています。行は増やしていません。".to_owned())
        }
        Err(err) => {
            error!(%err, "could not change the matchmaker schedule");
            return internal_error();
        }
    };
    render_queue(&app, Some(notice)).await
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

// ----------------------------------------------------------------------
// 運営による報告の入力
// ----------------------------------------------------------------------
//
// **プレイヤーの報告を UPDATE しない。**運営の報告も 1 行の追記で、確定判定は
// 従来どおり各プレイヤーの最新報告を読む。対象は未報告の人だけ。

#[derive(Debug, Deserialize)]
struct ResolveDisputeForm {
    #[serde(default)]
    formation: String,
    #[serde(default)]
    decision: String,
    #[serde(default)]
    reason: String,
    #[serde(default)]
    key: String,
}

/// 係争を裁定する。**終端イベントを 1 行積むだけ。**
///
/// 報告は書き換えない。割れた報告はそのまま残り、裁定はその上に乗る別の 1 行になる。
/// 追記は通常の経路を通るので、状態機械の検証も冪等キーもそのまま効く。
async fn resolve_dispute(
    State(app): State<AdminState>,
    session: Option<CurrentSession>,
    Form(form): Form<ResolveDisputeForm>,
) -> Response {
    let actor = match actor(&app, session).await {
        Ok(actor) => actor,
        Err(error) => return error.into(),
    };
    let Ok(formation_id) = Uuid::parse_str(form.formation.trim()) else {
        return render_reports(&app, Some(fail("試合の指定が不正です。".to_owned()))).await;
    };
    let Some(decision) = parse_decision(&form.decision) else {
        return render_reports(&app, Some(fail("裁定の指定が不正です。".to_owned()))).await;
    };
    let Some(reason) = normalize_reason(&form.reason) else {
        return render_reports(&app, Some(fail("理由を入力してください。".to_owned()))).await;
    };

    let input = NewDisputeResolution {
        formation_id: FormationId(formation_id),
        decision,
        reason,
        actor_discord_user_id: actor.discord_user_id,
        created_at: Utc::now(),
        idempotency_key: form.key.clone(),
    };
    let notice = match app.store.resolve_dispute(&input).await {
        Ok(DisputeOutcome::Resolved { state }) => {
            let ratings = if decision.changes_ratings() {
                "この試合の増減がレートに入りました。"
            } else {
                "レートは動いていません。"
            };
            info(format!(
                "裁定を記録しました（{label} / {state}）。{ratings}\
                 プレイヤーの報告は 1 行も書き換えていません。",
                label = decision.label(),
            ))
        }
        Ok(DisputeOutcome::Duplicate) => {
            info("この裁定は既に記録されています。行は増やしていません。".to_owned())
        }
        Ok(DisputeOutcome::NotDisputed { state }) => fail(format!(
            "その試合は係争中ではありません（現在: {state}）。\
             ほかの運営が先に裁定した可能性があります。画面を読み直してください。"
        )),
        Err(err) => {
            error!(%err, "could not resolve the dispute");
            return internal_error();
        }
    };
    render_reports(&app, Some(notice)).await
}

/// 終わった試合の裁定をやり直す。**終端イベントをもう 1 行積むだけ。**
///
/// 元の裁定もプレイヤーの報告も書き換えない。レートは導出値なので、最後の
/// 終端イベントが変われば 10 人ぶんの増減が同時に引き直される。
async fn readjudicate(
    State(app): State<AdminState>,
    session: Option<CurrentSession>,
    Form(form): Form<ResolveDisputeForm>,
) -> Response {
    let actor = match actor(&app, session).await {
        Ok(actor) => actor,
        Err(error) => return error.into(),
    };
    let Ok(formation_id) = Uuid::parse_str(form.formation.trim()) else {
        return render_reports(&app, Some(fail("試合の指定が不正です。".to_owned()))).await;
    };
    let Some(decision) = parse_decision(&form.decision) else {
        return render_reports(&app, Some(fail("裁定の指定が不正です。".to_owned()))).await;
    };
    let Some(reason) = normalize_reason(&form.reason) else {
        return render_reports(&app, Some(fail("理由を入力してください。".to_owned()))).await;
    };

    let input = NewReadjudication {
        formation_id: FormationId(formation_id),
        decision,
        reason,
        actor_discord_user_id: actor.discord_user_id,
        created_at: Utc::now(),
        idempotency_key: form.key.clone(),
    };
    let notice = match app.store.readjudicate(&input).await {
        Ok(ReadjudicationOutcome::Changed { before, state }) => {
            let ratings = match (
                before.kind == crate::lifecycle::FormationEventKind::Confirmed,
                decision.changes_ratings(),
            ) {
                (true, true) => "10 人ぶんの増減が新しい結果で引き直されました。",
                (true, false) => "この試合の増減が 10 人ぶんまとめてレートから消えました。",
                (false, true) => "この試合の増減が 10 人ぶんレートに入りました。",
                (false, false) => "レートは動いていません。",
            };
            info(format!(
                "裁定をやり直しました（{before} → {label} / {state}）。{ratings}\
                 元の裁定もプレイヤーの報告も 1 行も書き換えていません。",
                before = before.label(),
                label = decision.label(),
            ))
        }
        Ok(ReadjudicationOutcome::Duplicate) => {
            info("このやり直しは既に記録されています。行は増やしていません。".to_owned())
        }
        Ok(ReadjudicationOutcome::Unchanged { current }) => fail(format!(
            "その試合は既に「{}」です。行は増やしていません。",
            current.label()
        )),
        Ok(ReadjudicationOutcome::NotFinished { state }) => fail(format!(
            "その試合はまだ終わっていません（現在: {state}）。\
             係争中なら上の係争の一覧から、進行中なら報告の到着を待ってください。"
        )),
        Err(err) => {
            error!(%err, "could not readjudicate the match");
            return internal_error();
        }
    };
    render_reports(&app, Some(notice)).await
}

/// 裁定のやり直しの一覧に出す件数。**直近の分だけ。**
///
/// 「後からやっぱり」は当日か翌日に起きる。1 晩で最大 50 組なので、
/// 前の晩まで遡れる程度にしてある。
const FINISHED_LIMIT: i64 = 80;

/// 報告の入力画面を描く。一覧は毎回引き直す（保存しない）。
async fn render_reports(app: &AdminState, notice: Option<Notice>) -> Response {
    let awaiting = match app.store.formations_awaiting_reports().await {
        Ok(awaiting) => awaiting,
        Err(err) => {
            error!(%err, "could not list the formations awaiting reports");
            return internal_error();
        }
    };
    let disputed = match app.store.disputed_formations().await {
        Ok(disputed) => disputed,
        Err(err) => {
            error!(%err, "could not list the disputed formations");
            return internal_error();
        }
    };
    let finished = match app.store.finished_formations(FINISHED_LIMIT).await {
        Ok(finished) => finished,
        Err(err) => {
            error!(%err, "could not list the finished formations");
            return internal_error();
        }
    };
    Html(reports_page(&ReportsView {
        awaiting: &awaiting,
        form_key: &form_key(),
        disputed: &disputed,
        finished: &finished,
        notice,
    }))
    .into_response()
}

async fn show_reports(State(app): State<AdminState>, session: Option<CurrentSession>) -> Response {
    if let Err(error) = actor(&app, session).await {
        return error.into();
    }
    render_reports(&app, None).await
}

#[derive(Debug, Deserialize)]
struct EnterReportForm {
    #[serde(default)]
    formation: String,
    #[serde(default)]
    player: String,
    #[serde(default)]
    outcome: String,
    #[serde(default)]
    reason: String,
    #[serde(default)]
    key: String,
}

async fn enter_report(
    State(app): State<AdminState>,
    session: Option<CurrentSession>,
    Form(form): Form<EnterReportForm>,
) -> Response {
    let actor = match actor(&app, session).await {
        Ok(actor) => actor,
        Err(error) => return error.into(),
    };
    let (Ok(formation), Ok(player)) = (
        Uuid::parse_str(form.formation.trim()),
        Uuid::parse_str(form.player.trim()),
    ) else {
        return render_reports(&app, Some(fail("対象が不正です。".to_owned()))).await;
    };
    // **`draw` は受け付けない**（マッチ画面の3択と揃える。無効試合に寄せる）
    let outcome = match form.outcome.trim() {
        "red" => Outcome::Win(Team::Red),
        "blue" => Outcome::Win(Team::Blue),
        "nullified" => Outcome::Nullified,
        _ => {
            let notice =
                fail("赤の勝ち / 青の勝ち / 無効試合 のいずれかを選んでください。".to_owned());
            return render_reports(&app, Some(notice)).await;
        }
    };

    let now = Utc::now();
    let input = NewStaffReport {
        formation_id: FormationId(formation),
        player_id: PlayerId(player),
        outcome,
        actor_discord_user_id: actor.discord_user_id,
        reason: normalize_reason(&form.reason),
        occurred_at: now,
        idempotency_key: form.key.clone(),
    };
    // **断ったときは 409 を返す。**マッチ画面の訂正を 409 にしたのと同じ方針で、
    // 「受理したのに何も起きない」応答を返さない
    let (status, notice) = match app.store.enter_report(&input).await {
        Ok(StaffReportOutcome::Entered { settled: true }) => (
            StatusCode::OK,
            info("報告を追記しました。この報告で試合の結果が確定しました。".to_owned()),
        ),
        Ok(StaffReportOutcome::Entered { settled: false }) => (
            StatusCode::OK,
            info(
                "報告を追記しました。まだ確定していません（全員の報告か、受付時間の経過を待ちます）。"
                    .to_owned(),
            ),
        ),
        Ok(StaffReportOutcome::AlreadyReported) => (
            StatusCode::CONFLICT,
            fail(
                "その人は既に報告しています。運営が上書きすることはできません（係争のフローで裁定してください）。"
                    .to_owned(),
            ),
        ),
        Ok(StaffReportOutcome::Rejected { state }) => {
            (StatusCode::CONFLICT, fail(rejection_message(&state)))
        }
        Ok(StaffReportOutcome::NotAParticipant) => (
            StatusCode::CONFLICT,
            fail("その人はこの試合の参加者ではありません。".to_owned()),
        ),
        Err(err) => {
            error!(%err, "could not enter the staff report");
            return internal_error();
        }
    };
    let page = render_reports(&app, Some(notice)).await;
    if status == StatusCode::OK {
        return page;
    }
    (status, page).into_response()
}

/// 断った理由の説明。**状態ごとに、次に何をすればよいかまで書く。**
fn rejection_message(state: &str) -> String {
    match state {
        "disputed" => "この試合は係争中です。Discord の係争チャンネルで対応してください\
                       （ここから報告を入れても状態は動きません）。"
            .to_owned(),
        "confirmed" | "nullified" => {
            "この試合はもう結果が出ています。運営が報告を足しても結果は変わりません。".to_owned()
        }
        "voided" | "abandoned" => {
            "この試合は不成立で終わっています。報告は受け付けません。".to_owned()
        }
        _ => "この試合はまだ報告を受け付けられる段階にありません。".to_owned(),
    }
}

// ----------------------------------------------------------------------
// レート増減幅
// ----------------------------------------------------------------------
//
// **この操作は過去の全試合を引き直す。**docs/02 の不変条件 3 そのものなので、
// 隠さずに画面で言い切り、2 段階の確認を挟む。
// 1 段目は警告と件数、2 段目は**同じ数値の打ち直し**。クリック 2 回だと誤操作が通る。

/// 増減幅の画面を描画する。件数は毎回数え直す（保存しない）。
async fn render_rating_params(
    app: &AdminState,
    confirming: Option<f64>,
    notice: Option<Notice>,
    key: Option<&str>,
) -> Response {
    let preview = match app.store.rating_params_preview().await {
        Ok(preview) => preview,
        Err(err) => {
            error!(%err, "could not read the rating params preview");
            return internal_error();
        }
    };
    let generated = form_key();
    Html(rating_params_page(&RatingParamsView {
        preview: &preview,
        form_key: key.unwrap_or(&generated),
        confirming,
        notice,
    }))
    .into_response()
}

async fn show_rating_params(
    State(app): State<AdminState>,
    session: Option<CurrentSession>,
) -> Response {
    if let Err(error) = actor(&app, session).await {
        return error.into();
    }
    render_rating_params(&app, None, None, None).await
}

#[derive(Debug, Deserialize)]
struct RatingParamsConfirmForm {
    #[serde(default)]
    base_delta: String,
    #[serde(default)]
    key: String,
}

/// 1 段目 → 2 段目。値を検証して確認画面を出すだけで、まだ何も書かない。
async fn confirm_rating_params(
    State(app): State<AdminState>,
    session: Option<CurrentSession>,
    Form(form): Form<RatingParamsConfirmForm>,
) -> Response {
    if let Err(error) = actor(&app, session).await {
        return error.into();
    }
    let Some(base_delta) = normalize_base_delta(&form.base_delta) else {
        let notice = fail(format!(
            "増減幅は 0 より大きく {MAX_BASE_DELTA} 以下の数値で入力してください。"
        ));
        return render_rating_params(&app, None, Some(notice), None).await;
    };
    // 冪等キーは 1 段目で発行したものを 2 段目へ持ち越す（同じ操作は 1 行にしかならない）
    render_rating_params(&app, Some(base_delta), None, Some(&form.key)).await
}

#[derive(Debug, Deserialize)]
struct RatingParamsChangeForm {
    #[serde(default)]
    base_delta: String,
    #[serde(default)]
    confirm_delta: String,
    #[serde(default)]
    reason: String,
    #[serde(default)]
    key: String,
}

/// 2 段目の送信。数値の打ち直しが一致したときだけ追記する。
async fn change_rating_params(
    State(app): State<AdminState>,
    session: Option<CurrentSession>,
    Form(form): Form<RatingParamsChangeForm>,
) -> Response {
    let actor = match actor(&app, session).await {
        Ok(actor) => actor,
        Err(error) => return error.into(),
    };
    let Some(base_delta) = normalize_base_delta(&form.base_delta) else {
        let notice = fail("増減幅が不正です。".to_owned());
        return render_rating_params(&app, None, Some(notice), None).await;
    };
    // 打ち直しは**数値として**比較する（"7" と "7.0" を別物にしない）
    if normalize_base_delta(&form.confirm_delta) != Some(base_delta) {
        let notice = fail("数値が一致しません。同じ値を入力してください。".to_owned());
        return render_rating_params(&app, Some(base_delta), Some(notice), Some(&form.key)).await;
    }
    let Some(reason) = normalize_reason(&form.reason) else {
        let notice = fail("理由を入力してください。".to_owned());
        return render_rating_params(&app, Some(base_delta), Some(notice), Some(&form.key)).await;
    };

    let now = Utc::now();
    let input = NewRatingParams {
        base_delta,
        reason,
        actor_discord_user_id: actor.discord_user_id,
        created_at: now,
        idempotency_key: form.key.clone(),
    };
    let notice = match app.store.change_rating_params(&input).await {
        Ok(RatingParamsOutcome::Changed { id, params_version }) => info(format!(
            "増減幅を ±{base_delta} に変更しました（#{id} / params v{params_version}）。\
             過去の全試合が新しい値で引き直されています。試合ログは 1 行も変わっていません。"
        )),
        Ok(RatingParamsOutcome::Duplicate) => {
            info("この操作は既に記録されています。行は増やしていません。".to_owned())
        }
        Ok(RatingParamsOutcome::Blocked { active_formations }) => fail(format!(
            "進行中の formation が {active_formations} 件あるため実行しませんでした。\
             増減幅は 1 行積んだ瞬間に全員のレートとキューの範囲判定を変えます。\
             終わってからやり直してください。"
        )),
        Err(err) => {
            error!(%err, "could not change the rating params");
            return internal_error();
        }
    };
    render_rating_params(&app, None, Some(notice), None).await
}

// ----------------------------------------------------------------------
// マッチング範囲
// ----------------------------------------------------------------------
//
// **1 段階で書く。**増減幅（`/admin/rating-params`）が 2 段階なのは、1 行積むと
// 過去の全試合が引き直されて取り返しの説明が難しくなるためだった。幅は過去を
// 1 件も変えず、元の値を追記すればそのまま戻る。当日の校正で何度も触る操作に
// 確認を挟むと、かえって「とりあえず広げる」を誘発する。
//
// そのかわり、**理由は必須**で、現在値と履歴を同じ画面に出す。

async fn render_matching_range(
    app: &AdminState,
    notice: Option<Notice>,
    key: Option<&str>,
) -> Response {
    let preview = match app.store.matching_range_preview().await {
        Ok(preview) => preview,
        Err(err) => {
            error!(%err, "could not read the matching range preview");
            return internal_error();
        }
    };
    let generated = form_key();
    Html(matching_range_page(&MatchingRangeView {
        preview: &preview,
        form_key: key.unwrap_or(&generated),
        notice,
    }))
    .into_response()
}

async fn show_matching_range(
    State(app): State<AdminState>,
    session: Option<CurrentSession>,
) -> Response {
    if let Err(error) = actor(&app, session).await {
        return error.into();
    }
    render_matching_range(&app, None, None).await
}

#[derive(Debug, Deserialize)]
struct MatchingRangeForm {
    #[serde(default)]
    up_range: String,
    #[serde(default)]
    down_range: String,
    #[serde(default)]
    reason: String,
    #[serde(default)]
    key: String,
}

async fn change_matching_range(
    State(app): State<AdminState>,
    session: Option<CurrentSession>,
    Form(form): Form<MatchingRangeForm>,
) -> Response {
    let actor = match actor(&app, session).await {
        Ok(actor) => actor,
        Err(error) => return error.into(),
    };
    let ranges = match parse_ranges(&form.up_range, &form.down_range) {
        Ok(ranges) => ranges,
        Err(error) => {
            let notice = fail(error.to_string());
            return render_matching_range(&app, Some(notice), Some(&form.key)).await;
        }
    };
    let Some(reason) = normalize_reason(&form.reason) else {
        let notice = fail("理由を入力してください。".to_owned());
        return render_matching_range(&app, Some(notice), Some(&form.key)).await;
    };

    let input = NewMatchingRange {
        ranges,
        reason,
        actor_discord_user_id: actor.discord_user_id,
        created_at: Utc::now(),
        idempotency_key: form.key.clone(),
    };
    let notice = match app.store.change_matching_range(&input).await {
        Ok(MatchingRangeOutcome::Changed { id }) => info(format!(
            "マッチング範囲を 上 +{up} / 下 -{down} に変更しました（#{id}）。\
             次のスキャンから効きます。確定済みの試合も進行中の formation も変わっていません。",
            up = ranges.up,
            down = ranges.down,
        )),
        Ok(MatchingRangeOutcome::Duplicate) => {
            info("この操作は既に記録されています。行は増やしていません。".to_owned())
        }
        Err(err) => {
            error!(%err, "could not change the matching range");
            return internal_error();
        }
    };
    render_matching_range(&app, Some(notice), None).await
}

// ----------------------------------------------------------------------
// ブロック解除の待機日数
// ----------------------------------------------------------------------
//
// マッチング範囲と同じ 1 段階。試合には何も影響せず、追記で戻せる。
// **既にあるブロックにも効く**ので、画面にはいま有効なブロックの件数を出す。

async fn render_block_cooldown(
    app: &AdminState,
    notice: Option<Notice>,
    key: Option<&str>,
) -> Response {
    let preview = match app.store.block_cooldown_preview().await {
        Ok(preview) => preview,
        Err(err) => {
            error!(%err, "could not read the block cooldown preview");
            return internal_error();
        }
    };
    let generated = form_key();
    Html(block_cooldown_page(&BlockCooldownView {
        preview: &preview,
        form_key: key.unwrap_or(&generated),
        notice,
    }))
    .into_response()
}

async fn show_block_cooldown(
    State(app): State<AdminState>,
    session: Option<CurrentSession>,
) -> Response {
    if let Err(error) = actor(&app, session).await {
        return error.into();
    }
    render_block_cooldown(&app, None, None).await
}

#[derive(Debug, Deserialize)]
struct BlockCooldownForm {
    #[serde(default)]
    cooldown_days: String,
    #[serde(default)]
    reason: String,
    #[serde(default)]
    key: String,
}

async fn change_block_cooldown(
    State(app): State<AdminState>,
    session: Option<CurrentSession>,
    Form(form): Form<BlockCooldownForm>,
) -> Response {
    let actor = match actor(&app, session).await {
        Ok(actor) => actor,
        Err(error) => return error.into(),
    };
    let cooldown_days = match parse_cooldown_days(&form.cooldown_days) {
        Ok(days) => days,
        Err(error) => {
            let notice = fail(error.to_string());
            return render_block_cooldown(&app, Some(notice), Some(&form.key)).await;
        }
    };
    let Some(reason) = normalize_reason(&form.reason) else {
        let notice = fail("理由を入力してください。".to_owned());
        return render_block_cooldown(&app, Some(notice), Some(&form.key)).await;
    };

    let input = NewBlockCooldown {
        cooldown_days,
        reason,
        actor_discord_user_id: actor.discord_user_id,
        created_at: Utc::now(),
        idempotency_key: form.key.clone(),
    };
    let notice = match app.store.change_block_cooldown(&input).await {
        Ok(BlockCooldownOutcome::Changed { id }) => info(format!(
            "ブロック解除の待機日数を {cooldown_days} 日に変更しました（#{id}）。\
             既にあるブロックにもこの日数で効きます。試合は何も変わりません。"
        )),
        Ok(BlockCooldownOutcome::Duplicate) => {
            info("この操作は既に記録されています。行は増やしていません。".to_owned())
        }
        Err(err) => {
            error!(%err, "could not change the block cooldown");
            return internal_error();
        }
    };
    render_block_cooldown(&app, Some(notice), None).await
}

// ----------------------------------------------------------------------
// シャドウバン（IP / Discord ユーザー ID）
// ----------------------------------------------------------------------
//
// 断らずに組まれないようにする。効くのは次の投入から。解除で戻せる。

async fn render_shadow_bans(
    app: &AdminState,
    notice: Option<Notice>,
    key: Option<&str>,
) -> Response {
    let preview = match app.store.shadow_ban_preview().await {
        Ok(preview) => preview,
        Err(err) => {
            error!(%err, "could not read the shadow ban preview");
            return internal_error();
        }
    };
    let generated = form_key();
    Html(shadow_bans_page(&ShadowBansView {
        preview: &preview,
        form_key: key.unwrap_or(&generated),
        notice,
    }))
    .into_response()
}

async fn show_shadow_bans(
    State(app): State<AdminState>,
    session: Option<CurrentSession>,
) -> Response {
    if let Err(error) = actor(&app, session).await {
        return error.into();
    }
    render_shadow_bans(&app, None, None).await
}

#[derive(Debug, Deserialize)]
struct ShadowBanForm {
    /// `ban` / `unban`。
    #[serde(default)]
    action: String,
    /// IP か Discord ユーザー ID（見分けは `BanSubject::parse`）。
    #[serde(default)]
    subject: String,
    #[serde(default)]
    reason: String,
    #[serde(default)]
    key: String,
}

async fn change_shadow_ban(
    State(app): State<AdminState>,
    session: Option<CurrentSession>,
    Form(form): Form<ShadowBanForm>,
) -> Response {
    let actor = match actor(&app, session).await {
        Ok(actor) => actor,
        Err(error) => return error.into(),
    };
    let kind = match form.action.as_str() {
        "ban" => ShadowBanKind::Banned,
        "unban" => ShadowBanKind::Unbanned,
        _ => {
            let notice = fail("操作の種類が不正です。".to_owned());
            return render_shadow_bans(&app, Some(notice), Some(&form.key)).await;
        }
    };
    let Some(subject) = BanSubject::parse(&form.subject) else {
        let notice = fail(
            "IP か Discord ユーザー ID を入力してください（空白を含まない 64 文字以内）。"
                .to_owned(),
        );
        return render_shadow_bans(&app, Some(notice), Some(&form.key)).await;
    };
    let Some(reason) = normalize_reason(&form.reason) else {
        let notice = fail("理由を入力してください。".to_owned());
        return render_shadow_bans(&app, Some(notice), Some(&form.key)).await;
    };

    let label = format!("{} {}", subject.kind_label(), subject.value());
    let input = NewShadowBan {
        idempotency_key: format!("{}:{}:{}", form.key, subject.kind_str(), subject.value()),
        subject,
        kind,
        reason,
        actor_discord_user_id: actor.discord_user_id,
        created_at: Utc::now(),
    };
    let notice = match app.store.change_shadow_ban(&input).await {
        Ok(ShadowBanOutcome::Changed { id }) if kind == ShadowBanKind::Banned => info(format!(
            "{label} をシャドウバンしました（#{id}）。次の投入以降は組まれません。\
             いまキューにいる行は変わりません。"
        )),
        Ok(ShadowBanOutcome::Changed { id }) => info(format!(
            "{label} のシャドウバンを解除しました（#{id}）。次の投入から組まれます。"
        )),
        Ok(ShadowBanOutcome::Unchanged) if kind == ShadowBanKind::Banned => {
            info(format!("{label} は既に BAN 中です。行は増やしていません。"))
        }
        Ok(ShadowBanOutcome::Unchanged) => info(format!(
            "{label} は BAN していません。行は増やしていません。"
        )),
        Ok(ShadowBanOutcome::Duplicate) => {
            info("この操作は既に記録されています。行は増やしていません。".to_owned())
        }
        Err(err) => {
            error!(%err, "could not change the shadow ban");
            return internal_error();
        }
    };
    render_shadow_bans(&app, Some(notice), None).await
}

// ----------------------------------------------------------------------
// 聞き専報告（閲覧のみ）
// ----------------------------------------------------------------------
//
// **ここが票数と閾値を見られる唯一の画面。**入力の経路は作らない
// （報告はプレイヤーがマッチ画面から出すもので、運営が代理で入れるものではない）。

/// 一覧に出す件数の上限。
const QUIET_HISTORY_LIMIT: i64 = 200;
const QUIET_TALLY_LIMIT: i64 = 200;

async fn show_quiet_reports(
    State(app): State<AdminState>,
    session: Option<CurrentSession>,
) -> Response {
    if let Err(error) = actor(&app, session).await {
        return error.into();
    }
    let tallies = match app.quiet.admin_tallies(QUIET_TALLY_LIMIT).await {
        Ok(tallies) => tallies,
        Err(err) => {
            error!(%err, "could not read the quiet report tallies");
            return internal_error();
        }
    };
    let history = match app.quiet.admin_history(QUIET_HISTORY_LIMIT).await {
        Ok(history) => history,
        Err(err) => {
            error!(%err, "could not read the quiet report history");
            return internal_error();
        }
    };
    let thresholds = match app.quiet.thresholds().await {
        Ok(thresholds) => thresholds,
        Err(err) => {
            error!(%err, "could not read the quiet report thresholds");
            return internal_error();
        }
    };
    let nullified_tallies = match app.nullified.admin_tallies(QUIET_TALLY_LIMIT).await {
        Ok(tallies) => tallies,
        Err(err) => {
            error!(%err, "could not read the nullified report tallies");
            return internal_error();
        }
    };
    let nullified_history = match app.nullified.admin_history(QUIET_HISTORY_LIMIT).await {
        Ok(history) => history,
        Err(err) => {
            error!(%err, "could not read the nullified report history");
            return internal_error();
        }
    };
    let (nullified_threshold, nullified_penalty) = match app.nullified.nullified_params().await {
        Ok(params) => params,
        Err(err) => {
            error!(%err, "could not read the nullified report params");
            return internal_error();
        }
    };
    Html(quiet_reports_page(&QuietReportsView {
        tallies: &tallies,
        history: &history,
        nullified_tallies: &nullified_tallies,
        nullified_history: &nullified_history,
        nullified_threshold,
        nullified_penalty,
        winner_threshold: thresholds.winner_threshold,
        loser_threshold: thresholds.loser_threshold,
        notice: None,
    }))
    .into_response()
}
