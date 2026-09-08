//! マッチ画面のエンドポイント。**操作はすべてここ。Discord にはインタラクションを残さない。**
//!
//! - `GET    /match/{id}`            成立から確定までの1枚（HTML / JSON）
//! - `POST   /match/{id}/host`       ホスト打診への応答（`accept` / `decline`）
//! - `POST   /match/{id}/room-code`  ルーム ID の投稿と打ち直し
//! - `POST   /match/{id}/report`     3択の報告
//!
//! 不変条件（このモジュールで守るもの）:
//!
//! - **`player_id` はセッション（[`CurrentSession`]）からしか来ない。**リクエストボディや
//!   クエリから読む経路が無いので、他人の formation も他人の報告も触れない。
//! - **参加者以外には formation の存在を教えない。**参加していなければ 404
//!   （403 を返すと「その試合はある」と分かる。管理パネルと同じ方針）。
//! - **他人の報告状況を返さない。人数も返さない**（00 §3-5）。返すのは
//!   [`MatchView::i_reported`](super::model::MatchView::i_reported) だけ。
//! - **状態遷移そのものを変えない。**積むイベントは既存のものだけで、検証は
//!   [`FormationRepository::append_event`]（ロック → 検証 → INSERT）に任せる。
//!   HTTP は Discord を呼ばない。通知は既存の再開ループ（5 秒スキャン）が DB の状態から出す
//!   ので、**復帰の入口が 2 つにならない**（00 §7 Phase 5 の申し送り）。
//! - **冪等は「同一 idempotency_key のみ」**（00 §5）。内容の違う打ち直しは必ず通る。
//!
//! CSRF: セッション Cookie は `SameSite=Lax` なので、他サイトからの POST には送られない。
//! 登録トラックのフォームと同じ前提に拠る。

use std::sync::Arc;

use axum::extract::{FromRef, Path, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use axum_extra::extract::cookie::Key;
use chrono::Utc;
use serde::Deserialize;
use serde_json::json;
use tracing::{error, info};

use crate::admin::AdminIds;
use crate::lifecycle::{
    FormationEventKind, FormationId, FormationState, LifecycleConfig, PlayerId,
};
use crate::persistence::{
    FormationRepository, NewEvent, NewReport, Outcome as ReportOutcome, Team,
};
use crate::registration::html::error_page;
use crate::registration::{Audience, AuthConfig, CurrentSession, Viewer};

use super::model::{
    AffordanceView, BLUE, MatchView, OpponentView, RED, StageView, TeammateView, VoiceView,
};
use super::repository::{
    MatchPlayer, MatchRepository, Outcome, parse_formation_id, submission_key,
};

/// 未登録のトレーナー名の代わり。Discord 側の割り当て表と同じ文言。
const UNNAMED: &str = "（名前未登録）";

/// マッチ画面が持つ状態。
#[derive(Clone)]
pub struct MatchState {
    /// セッション Cookie を読むための設定。
    pub config: AuthConfig,
    /// Cookie の署名鍵。
    key: Key,
    /// 共通ヘッダーの導線判定。
    audience: Audience,
    /// 追記の入口（検証つき）。
    pub formations: FormationRepository,
    /// 画面の読み取り。
    pub matches: MatchRepository,
    /// 期限と候補上限（画面の説明文に使う）。
    pub lifecycle: LifecycleConfig,
    /// 部屋設定のチェックリスト（先頭にマップを差し込んだもの）。
    room_checklist: Arc<Vec<String>>,
}

impl MatchState {
    /// 組み立てる。`room_map` はチェックリストの先頭に入る（Discord 側と同じ形）。
    pub fn new(
        config: AuthConfig,
        formations: FormationRepository,
        matches: MatchRepository,
        lifecycle: LifecycleConfig,
        room_map: &str,
        room_checklist: &[String],
    ) -> Self {
        let key = config.cookie_key();
        let mut checklist = vec![format!("マップ: {room_map}")];
        checklist.extend(room_checklist.iter().cloned());
        Self {
            config,
            key,
            audience: Audience::default(),
            formations,
            matches,
            lifecycle,
            room_checklist: Arc::new(checklist),
        }
    }

    /// 管理者リストを渡す。渡さなければ誰にも管理パネルの導線を出さない。
    pub fn with_admins(mut self, admins: Option<AdminIds>) -> Self {
        self.audience = Audience::new(admins);
        self
    }
}

impl FromRef<MatchState> for Key {
    fn from_ref(state: &MatchState) -> Self {
        state.key.clone()
    }
}

impl FromRef<MatchState> for AuthConfig {
    fn from_ref(state: &MatchState) -> Self {
        state.config.clone()
    }
}

/// ルーターを組み立てる。
pub fn router(state: MatchState) -> Router {
    Router::new()
        .route("/match/{formation_id}", get(show_match))
        .route("/match/{formation_id}/host", post(respond_to_host_prompt))
        .route("/match/{formation_id}/room-code", post(submit_room_code))
        .route("/match/{formation_id}/report", post(submit_report))
        .with_state(state)
}

// ----------------------------------------------------------------------
// リクエスト
// ----------------------------------------------------------------------

/// ホスト打診への応答。
#[derive(Debug, Deserialize)]
struct HostResponseRequest {
    /// `accept` または `decline`。
    action: String,
}

/// ルーム ID の投稿。
#[derive(Debug, Deserialize)]
struct RoomCodeRequest {
    /// 入力されたルーム ID。前後の空白は落とす。
    room_code: String,
}

/// 報告。**`draw` は選択肢に入れない**（無効試合に寄せる）。
#[derive(Debug, Deserialize)]
struct ReportRequest {
    /// `red` / `blue` / `nullified`。
    outcome: String,
}

// ----------------------------------------------------------------------
// ハンドラ
// ----------------------------------------------------------------------

/// マッチ画面。参加者以外には 404。
async fn show_match(
    State(app): State<MatchState>,
    session: Option<CurrentSession>,
    Path(formation_id): Path<String>,
    headers: HeaderMap,
) -> Response {
    let html = accepts_html(&headers);
    let Some(player_id) = player(session) else {
        return unauthenticated(html);
    };
    let Some(formation) = parse_formation_id(&formation_id) else {
        return not_found(html);
    };
    match build_view(&app, formation, player_id).await {
        Ok(Some(view)) if html => {
            let viewer = viewer(&app, player_id).await;
            (StatusCode::OK, Html(super::html::match_page(viewer, &view))).into_response()
        }
        Ok(Some(view)) => (StatusCode::OK, Json(view)).into_response(),
        Ok(None) => not_found(html),
        Err(error) => {
            error!(%error, "could not build the match view");
            internal_error(html)
        }
    }
}

/// 「部屋を作る」/「今回はパスする」。**打診中の候補本人だけ。**
async fn respond_to_host_prompt(
    State(app): State<MatchState>,
    session: Option<CurrentSession>,
    Path(formation_id): Path<String>,
    body: Option<Json<HostResponseRequest>>,
) -> Response {
    let Some(player_id) = player(session) else {
        return unauthenticated(false);
    };
    let Some(formation) = parse_formation_id(&formation_id) else {
        return not_found(false);
    };
    if !is_participant(&app, formation, player_id).await {
        return not_found(false);
    }
    let Some(Json(request)) = body else {
        return bad_request("invalid_request", "リクエストの形式が正しくありません。");
    };
    let kind = match request.action.trim() {
        "accept" => FormationEventKind::HostAccepted,
        "decline" => FormationEventKind::HostDeclined,
        _ => return bad_request("invalid_action", "accept か decline を指定してください。"),
    };

    // 打診中の候補本人か。状態機械も同じ照合をするが、こちらは「あなた宛てではない」を
    // 400 ではなく 409 で返すために先に見る（押した本人に理由が分かる形にする）
    let state = match current_state(&app, formation).await {
        Ok(state) => state,
        Err(error) => {
            error!(%error, "could not read the formation state");
            return internal_error(false);
        }
    };
    let FormationState::AwaitingHost {
        pending: Some(pending),
        ..
    } = state
    else {
        return conflict("not_awaiting_host", "いまは打診を受け付けていません。");
    };
    if pending != player_id {
        return conflict("not_the_candidate", "この打診はあなた宛てではありません。");
    }

    let now = Utc::now();
    let event = NewEvent::new(
        kind,
        Some(player_id),
        now,
        submission_key("host", player_id, kind.as_str(), now),
    )
    .with_payload(json!({ "source": "web" }));
    match app.formations.append_event(formation, &event).await {
        Ok(_) => {
            info!(formation = %formation, player = %player_id, action = %kind, "host response accepted from the site");
            (StatusCode::OK, Json(json!({ "ok": true }))).into_response()
        }
        Err(error) => {
            error!(%error, "could not record the host response");
            conflict("rejected", "いまはその操作を受け付けていません。")
        }
    }
}

/// ルーム ID の投稿と打ち直し。**ホスト本人だけ。**
async fn submit_room_code(
    State(app): State<MatchState>,
    session: Option<CurrentSession>,
    Path(formation_id): Path<String>,
    body: Option<Json<RoomCodeRequest>>,
) -> Response {
    let Some(player_id) = player(session) else {
        return unauthenticated(false);
    };
    let Some(formation) = parse_formation_id(&formation_id) else {
        return not_found(false);
    };
    if !is_participant(&app, formation, player_id).await {
        return not_found(false);
    }
    let Some(Json(request)) = body else {
        return bad_request("invalid_request", "リクエストの形式が正しくありません。");
    };
    let room_code = request.room_code.trim();
    if room_code.is_empty() || room_code.chars().count() > ROOM_CODE_MAX_CHARS {
        return bad_request(
            "invalid_room_code",
            "ルーム ID を1文字以上32文字以内で入力してください。",
        );
    }

    let state = match current_state(&app, formation).await {
        Ok(state) => state,
        Err(error) => {
            error!(%error, "could not read the formation state");
            return internal_error(false);
        }
    };
    // ホストは状態から読む。リクエストからは読まない
    let host = match &state {
        FormationState::AwaitingCode { host, .. } => *host,
        FormationState::InProgress { host, .. } => *host,
        _ => {
            return conflict(
                "not_awaiting_code",
                "いまはルーム ID を受け付けていません。",
            );
        }
    };
    if host != player_id {
        return conflict("not_the_host", "ルーム ID を入力できるのはホストだけです。");
    }

    let now = Utc::now();
    // 内容が違えばキーが変わる = 打ち直しは新しいイベントとして通る（00 §5）
    let event = NewEvent::new(
        FormationEventKind::CodePosted,
        Some(player_id),
        now,
        submission_key("code", player_id, room_code, now),
    )
    .with_payload(json!({ "room_code": room_code, "source": "web" }));
    match app.formations.append_event(formation, &event).await {
        Ok(_) => {
            info!(formation = %formation, player = %player_id, "room code posted from the site");
            (StatusCode::OK, Json(json!({ "ok": true }))).into_response()
        }
        Err(error) => {
            error!(%error, "could not record the room code");
            conflict("rejected", "いまはルーム ID を受け付けていません。")
        }
    }
}

/// 3択の報告。**参加者だけ。**判定結果も件数も返さない。
async fn submit_report(
    State(app): State<MatchState>,
    session: Option<CurrentSession>,
    Path(formation_id): Path<String>,
    body: Option<Json<ReportRequest>>,
) -> Response {
    let Some(player_id) = player(session) else {
        return unauthenticated(false);
    };
    let Some(formation) = parse_formation_id(&formation_id) else {
        return not_found(false);
    };
    // 参加者以外は 404。他人の代わりに報告する経路も、存在を知る経路も無い
    if !is_participant(&app, formation, player_id).await {
        return not_found(false);
    }
    let Some(Json(request)) = body else {
        return bad_request("invalid_request", "リクエストの形式が正しくありません。");
    };
    // `draw` は受け付けない（無効試合に寄せる）
    let outcome = match request.outcome.trim() {
        "red" => ReportOutcome::Win(Team::Red),
        "blue" => ReportOutcome::Win(Team::Blue),
        "nullified" => ReportOutcome::Nullified,
        _ => {
            return bad_request(
                "invalid_outcome",
                "red / blue / nullified のいずれかを指定してください。",
            );
        }
    };

    let now = Utc::now();
    let report = NewReport {
        formation_id: formation,
        player_id,
        reported_at: now,
        outcome,
        raw_result: json!({ "source": "web", "choice": outcome.as_str() }),
        idempotency_key: submission_key("report", player_id, outcome.as_str(), now),
        supersedes_id: None,
    };
    if let Err(error) = app.formations.append_latest_report(report).await {
        error!(%error, "could not record the report");
        return conflict("rejected", "いまは報告を受け付けていません。");
    }
    // 判定はサーバ内部。**結果を応答に載せない**（他人の報告が読めてしまう）
    match app
        .formations
        .evaluate_reports(
            formation,
            false,
            now,
            format!("report_evaluation:all:web:{}", now.timestamp_micros()),
        )
        .await
    {
        Ok(evaluation) => {
            if evaluation.event.is_some() {
                info!(formation = %formation, "all-player report evaluation advanced");
            }
        }
        Err(error) => error!(%error, "report evaluation failed after a web report"),
    }
    info!(formation = %formation, player = %player_id, "report accepted from the site");
    // **本人の報告が追記された時点で**キュー画面へ戻す。試合の確定は待たない
    // （報告ゲートはプレイヤー単位なので、確定を待つと10人目が報告するまで拘束される）。
    // 行き先はサーバが決める。クライアントに遷移条件もタイマーも持たせない。
    // `reported` を付けて、戻った先から結果を追える導線を残す（/status には
    // 確定した試合しか出ないので、disputed と未確定はここからしか辿れない）
    (
        StatusCode::OK,
        Json(json!({
            "ok": true,
            "next": format!("/queue?reported={formation}"),
        })),
    )
        .into_response()
}

/// ルーム ID の最大文字数（Discord のモーダルと同じ上限に揃える）。
const ROOM_CODE_MAX_CHARS: usize = 32;

// ----------------------------------------------------------------------
// 画面の組み立て
// ----------------------------------------------------------------------

/// 参加者かどうかだけを見る（読み取り失敗も「見せない」に倒す）。
async fn is_participant(app: &MatchState, formation: FormationId, player_id: PlayerId) -> bool {
    match app.matches.my_team(formation, player_id).await {
        Ok(team) => team.is_some(),
        Err(error) => {
            error!(%error, "could not check match participation");
            false
        }
    }
}

async fn current_state(
    app: &MatchState,
    formation: FormationId,
) -> Result<FormationState, crate::persistence::RepositoryError> {
    Ok(app
        .formations
        .load_events(formation)
        .await?
        .snapshot()
        .state)
}

/// 画面1枚ぶんを組み立てる。参加者でなければ `Ok(None)`。
async fn build_view(
    app: &MatchState,
    formation: FormationId,
    me: PlayerId,
) -> Result<Option<MatchView>, crate::persistence::RepositoryError> {
    let Some(my_team) = app.matches.my_team(formation, me).await? else {
        return Ok(None);
    };
    let log = app.formations.load_events(formation).await?;
    let state = log.snapshot().state;
    let roster = app.matches.roster(formation).await?;
    let vc = app.matches.voice_pair(formation).await?;
    let i_reported = app.matches.has_reported(formation, me).await?;

    let host = host_of(&state);
    let name_of = |player: PlayerId| -> String {
        roster
            .iter()
            .find(|entry| entry.player_id == player)
            .map(display_name)
            .unwrap_or_else(|| UNNAMED.to_owned())
    };

    let mut teammates = Vec::new();
    let mut opponents = Vec::new();
    for entry in &roster {
        let is_host = host == Some(entry.player_id);
        if entry.team == my_team {
            teammates.push(TeammateView {
                player_id: entry.player_id.0,
                trainer_name: display_name(entry),
                role: entry.role_display_name.clone(),
                rating: entry.rating,
                is_me: entry.player_id == me,
                is_host,
            });
        } else {
            // ロールを写す場所が無い。相手の構成は部屋に入るまで分からない
            opponents.push(OpponentView {
                player_id: entry.player_id.0,
                trainer_name: display_name(entry),
                rating: entry.rating,
                is_host,
            });
        }
    }

    let opponent_team = if my_team == RED { BLUE } else { RED };
    let voice_of = |team: &str| -> Option<VoiceView> {
        vc.as_ref().map(|vc| VoiceView {
            channel_name: vc.channel_name(team),
            channel_id: vc.channel_id(team).to_owned(),
        })
    };

    let stage = stage_of(app, formation, me, &state, &name_of).await?;
    let actions = AffordanceView {
        host_response: matches!(
            &state,
            FormationState::AwaitingHost { pending: Some(p), .. } if *p == me
        ),
        room_code: host == Some(me)
            && matches!(
                state,
                FormationState::AwaitingCode { .. } | FormationState::InProgress { .. }
            ),
        // 報告義務がある状態で、まだ自分が出していないときだけ。他人の状況は見ない
        report: state.requires_reports() && !i_reported,
    };

    Ok(Some(MatchView {
        formation_id: formation.0,
        stage,
        my_team: my_team.clone(),
        teammates,
        opponents,
        my_voice: voice_of(&my_team),
        opponent_voice: voice_of(opponent_team),
        room_checklist: app.room_checklist.as_ref().clone(),
        i_reported,
        actions,
    }))
}

/// 状態機械の状態を、そのまま画面の段階へ写す。判定を足さない。
async fn stage_of(
    app: &MatchState,
    formation: FormationId,
    me: PlayerId,
    state: &FormationState,
    name_of: &impl Fn(PlayerId) -> String,
) -> Result<StageView, crate::persistence::RepositoryError> {
    Ok(match state {
        FormationState::Matched => StageView::Matched,
        FormationState::AwaitingHost { pending, .. } => StageView::AwaitingHost {
            pending: pending.map(name_of),
            i_am_candidate: *pending == Some(me),
        },
        FormationState::AwaitingCode { host, .. } => StageView::AwaitingCode {
            host: name_of(*host),
            i_am_host: *host == me,
        },
        FormationState::InProgress { host, room_code } => StageView::InProgress {
            room_code: room_code.clone(),
            host: name_of(*host),
            i_am_host: *host == me,
        },
        FormationState::Confirmed => {
            // 自分の増減だけを読む。他人の増減も報告内訳も引かない
            let mine = app.matches.my_outcome(formation, me).await?;
            let (outcome, result, my_delta) = match mine {
                Some(mine) => {
                    let result = match (mine.outcome, mine.delta) {
                        (Outcome::Draw, _) => "drawn",
                        (_, delta) if delta > 0.0 => "won",
                        _ => "lost",
                    };
                    (
                        mine.outcome.as_str().to_owned(),
                        result.to_owned(),
                        Some(mine.delta),
                    )
                }
                // 確定直後にビューがまだ読めない、という並びは起きうる。数字を捏造しない
                None => ("unknown".to_owned(), "unknown".to_owned(), None),
            };
            StageView::Confirmed {
                outcome,
                result,
                my_delta,
            }
        }
        FormationState::Disputed => StageView::Disputed,
        FormationState::Nullified => StageView::Nullified,
        FormationState::Voided => StageView::Voided,
        FormationState::Abandoned => StageView::Abandoned,
    })
}

fn host_of(state: &FormationState) -> Option<PlayerId> {
    match state {
        FormationState::AwaitingCode { host, .. } | FormationState::InProgress { host, .. } => {
            Some(*host)
        }
        _ => None,
    }
}

fn display_name(entry: &MatchPlayer) -> String {
    entry
        .trainer_name
        .as_deref()
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .unwrap_or(UNNAMED)
        .to_owned()
}

async fn viewer(app: &MatchState, player_id: PlayerId) -> Viewer {
    match app.matches.discord_user_id(player_id).await {
        Ok(Some(discord_user_id)) => app.audience.viewer_for(&discord_user_id),
        _ => Viewer::MEMBER,
    }
}

// ----------------------------------------------------------------------
// 応答
// ----------------------------------------------------------------------

fn player(session: Option<CurrentSession>) -> Option<PlayerId> {
    session.map(|CurrentSession(session)| session.player_id)
}

fn accepts_html(headers: &HeaderMap) -> bool {
    headers
        .get(header::ACCEPT)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|accept| accept.contains("text/html"))
}

fn unauthenticated(html: bool) -> Response {
    if html {
        return (
            StatusCode::UNAUTHORIZED,
            Html(error_page(
                Viewer::ANONYMOUS,
                "ログインが必要です",
                "セッションが切れているか、まだログインしていません。入口からDiscordでログインしてください。",
            )),
        )
            .into_response();
    }
    (
        StatusCode::UNAUTHORIZED,
        Json(json!({ "error": "unauthenticated" })),
    )
        .into_response()
}

/// 存在しない formation と、参加していない formation を同じ応答に寄せる。
fn not_found(html: bool) -> Response {
    if html {
        return (
            StatusCode::NOT_FOUND,
            Html(error_page(
                Viewer::MEMBER,
                "見つかりません",
                "お探しの試合は見つかりませんでした。",
            )),
        )
            .into_response();
    }
    (StatusCode::NOT_FOUND, Json(json!({ "error": "not_found" }))).into_response()
}

fn bad_request(code: &'static str, message: &str) -> Response {
    (
        StatusCode::BAD_REQUEST,
        Json(json!({ "error": code, "message": message })),
    )
        .into_response()
}

fn conflict(code: &'static str, message: &str) -> Response {
    (
        StatusCode::CONFLICT,
        Json(json!({ "error": code, "message": message })),
    )
        .into_response()
}

fn internal_error(html: bool) -> Response {
    if html {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Html(error_page(
                Viewer::MEMBER,
                "エラー",
                "試合の状態を取得できませんでした。時間をおいてお試しください。",
            )),
        )
            .into_response();
    }
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        Json(json!({ "error": "internal_error" })),
    )
        .into_response()
}
