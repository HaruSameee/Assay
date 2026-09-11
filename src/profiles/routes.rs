//! プロフィールの画面と JSON。
//!
//! - `GET  /players/{player_id}` 公開プロフィール（HTML / JSON）
//! - `GET  /profile`             自分の編集画面
//! - `POST /profile`             保存（追記）
//!
//! 不変条件:
//! - **編集の対象はセッションからしか来ない。**`player_id` をボディやクエリで受け取らない
//! - **公開ビューは [`PublicProfile`] しか返さない。**非公開の担当ロールは型に入らない
//! - URL に `discord_user_id` を出さない（`players.id` の UUID を使う）

use axum::extract::{FromRef, Multipart, Path, Query, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{Html, IntoResponse, Redirect, Response};
use axum::routing::get;
use axum::{Json, Router};
use axum_extra::extract::cookie::Key;
use chrono::Utc;
use serde::Deserialize;
use tracing::{error, warn};
use uuid::Uuid;

use crate::admin::AdminIds;
use crate::blocks::{BlockRepository, UserBlockError};
use crate::lifecycle::PlayerId;
use crate::registration::html::error_page;
use crate::registration::{Audience, AuthConfig, CurrentSession, RegistrationRepository, Viewer};

use super::model::validate_edit;
use super::repository::ProfileRepository;

/// プロフィールのルーターが持つ状態。
#[derive(Clone)]
pub struct ProfileState {
    pub config: AuthConfig,
    pub profiles: ProfileRepository,
    pub blocks: BlockRepository,
    /// 共通ヘッダーの導線判定に使う。
    pub registration: RegistrationRepository,
    key: Key,
    audience: Audience,
}

impl ProfileState {
    /// 組み立てる。
    pub fn new(
        config: AuthConfig,
        profiles: ProfileRepository,
        registration: RegistrationRepository,
        blocks: BlockRepository,
    ) -> Self {
        let key = config.cookie_key();
        Self {
            config,
            profiles,
            registration,
            blocks,
            key,
            audience: Audience::default(),
        }
    }

    /// 管理者リストを渡す。渡さなければ誰にも管理パネルの導線を出さない。
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

impl FromRef<ProfileState> for Key {
    fn from_ref(state: &ProfileState) -> Self {
        state.key.clone()
    }
}

impl FromRef<ProfileState> for AuthConfig {
    fn from_ref(state: &ProfileState) -> Self {
        state.config.clone()
    }
}

/// ルーターを組み立てる。
pub fn router(state: ProfileState) -> Router {
    Router::new()
        .route("/players/{player_id}", get(show_public))
        .route("/profile", get(show_edit).post(save))
        .route("/profile/blocks", axum::routing::post(change_block))
        // キャラプールの画像。**このルートだけ本体の上限を広げる**（既定 2MB < 3MB）
        .route(
            "/profile/pool-image",
            axum::routing::post(save_pool_image)
                .layer(axum::extract::DefaultBodyLimit::max(POOL_UPLOAD_LIMIT)),
        )
        .route("/players/{player_id}/pool.png", get(show_pool_image))
        .with_state(state)
}

// ----------------------------------------------------------------------
// キャラプールの画像
// ----------------------------------------------------------------------

/// 受け付けるリクエスト全体の上限。**画像 3MB + multipart の枠**。
///
/// axum の既定は 2MB なので、このルートだけ広げる。広げるのはここだけで、
/// ほかの経路は既定のままにする。
const POOL_UPLOAD_LIMIT: usize = crate::profiles::pool::MAX_IMAGE_BYTES + 64 * 1024;

/// 画像を保存する。**1 人 1 枚で、貼り直しは置き換え。**
///
/// アップロード（PNG）か URL のどちらか一方。どちらも空なら設定を消す。
async fn save_pool_image(
    State(app): State<ProfileState>,
    session: Option<CurrentSession>,
    mut multipart: Multipart,
) -> Response {
    let Some(CurrentSession(current)) = session.as_ref() else {
        return session_expired();
    };
    let player_id = current.player_id;
    let mut upload: Option<Vec<u8>> = None;
    let mut url = String::new();
    loop {
        let field = match multipart.next_field().await {
            Ok(Some(field)) => field,
            Ok(None) => break,
            Err(error) => {
                // 上限を超えた本体もここに来る。理由は画面の文言で分ける
                warn!(%error, "could not read the pool image upload");
                return pool_failure(
                    &app,
                    session.as_ref(),
                    player_id,
                    "画像を読み取れませんでした。3MB 以内の PNG を選んでください。",
                )
                .await;
            }
        };
        match field.name() {
            Some("image") => match field.bytes().await {
                Ok(bytes) => upload = Some(bytes.to_vec()),
                Err(error) => {
                    warn!(%error, "could not read the pool image bytes");
                    return pool_failure(
                        &app,
                        session.as_ref(),
                        player_id,
                        "画像を読み取れませんでした。3MB 以内の PNG を選んでください。",
                    )
                    .await;
                }
            },
            Some("image_url") => url = field.text().await.unwrap_or_default(),
            _ => {}
        }
    }

    let image = match crate::profiles::pool::parse(upload.as_deref(), &url) {
        Ok(image) => image,
        Err(error) => {
            return pool_failure(&app, session.as_ref(), player_id, &error.to_string()).await;
        }
    };
    if let Err(error) = app
        .profiles
        .save_pool_image(player_id, &image, Utc::now())
        .await
    {
        error!(%error, "could not save the pool image");
        return internal_error(true);
    }
    Redirect::to("/profile?saved=pool").into_response()
}

/// 受け付けられなかったときに、編集画面へ理由を書いて戻る。
async fn pool_failure(
    app: &ProfileState,
    session: Option<&CurrentSession>,
    player_id: PlayerId,
    message: &str,
) -> Response {
    render_edit(app, session, player_id, Some(message), false, "").await
}

/// アップロードされた PNG を配信する。
///
/// **content-type を固定し、`nosniff` を付ける。**中身が PNG であることは
/// 受け取るときに検査しているが、配信側でも型を推測させない。
async fn show_pool_image(
    State(app): State<ProfileState>,
    Path(player_id): Path<String>,
) -> Response {
    let Ok(player_id) = Uuid::parse_str(&player_id) else {
        return not_found(false);
    };
    match app.profiles.pool_image_bytes(PlayerId(player_id)).await {
        Ok(Some(bytes)) => (
            StatusCode::OK,
            [
                (header::CONTENT_TYPE, "image/png"),
                (header::X_CONTENT_TYPE_OPTIONS, "nosniff"),
                (header::CONTENT_DISPOSITION, "inline"),
                // 貼り直しがすぐ反映されてほしいので長くしない
                (header::CACHE_CONTROL, "private, max-age=60"),
            ],
            bytes,
        )
            .into_response(),
        Ok(None) => not_found(false),
        Err(error) => {
            error!(%error, "could not read the pool image");
            internal_error(false)
        }
    }
}

/// 共通ヘッダーの見ている人。読めなければ導線を出さない側へ倒す。
async fn viewer(app: &ProfileState, session: Option<&CurrentSession>) -> Viewer {
    let Some(CurrentSession(session)) = session else {
        return Viewer::ANONYMOUS;
    };
    match app.registration.find_by_player_id(session.player_id).await {
        Ok(profile) => app.audience.viewer_of(profile.as_ref()),
        Err(error) => {
            error!(%error, "could not read the profile for the header");
            Viewer::MEMBER
        }
    }
}

/// 公開プロフィール。**誰でも見られる。**存在しなければ 404。
async fn show_public(
    State(app): State<ProfileState>,
    session: Option<CurrentSession>,
    Path(player_id): Path<String>,
    headers: HeaderMap,
) -> Response {
    let html = accepts_html(&headers);
    let Ok(player_id) = Uuid::parse_str(&player_id) else {
        return not_found(html);
    };
    let found = match app.profiles.find_player(player_id).await {
        Ok(found) => found,
        Err(error) => {
            error!(%error, "could not look up the player");
            return internal_error(html);
        }
    };
    let Some(player_id) = found else {
        return not_found(html);
    };
    match app.profiles.public_profile(player_id).await {
        Ok(Some(profile)) if html => {
            let viewer = viewer(&app, session.as_ref()).await;
            (
                StatusCode::OK,
                Html(super::html::public_page(viewer, &profile)),
            )
                .into_response()
        }
        // **返すのは PublicProfile だけ。**非公開の担当ロールは型に入っていない
        Ok(Some(profile)) => (StatusCode::OK, Json(profile)).into_response(),
        Ok(None) => not_found(html),
        Err(error) => {
            error!(%error, "could not build the public profile");
            internal_error(html)
        }
    }
}

/// 自分の編集画面。
#[derive(Debug, Default, Deserialize)]
struct EditQuery {
    #[serde(default)]
    block_query: String,
}

async fn show_edit(
    State(app): State<ProfileState>,
    session: Option<CurrentSession>,
    Query(query): Query<EditQuery>,
) -> Response {
    let Some(CurrentSession(current)) = session else {
        return session_expired();
    };
    render_edit(
        &app,
        session.as_ref(),
        current.player_id,
        None,
        false,
        &query.block_query,
    )
    .await
}

/// 編集画面を描く（保存後の再描画にも使う）。
async fn render_edit(
    app: &ProfileState,
    session: Option<&CurrentSession>,
    player_id: PlayerId,
    error: Option<&str>,
    saved: bool,
    block_query: &str,
) -> Response {
    let own = match app.profiles.own(player_id).await {
        Ok(own) => own,
        Err(error) => {
            error!(%error, "could not read the profile");
            return internal_error(true);
        }
    };
    let known_roles = match app.profiles.known_roles().await {
        Ok(roles) => roles,
        Err(error) => {
            error!(%error, "could not read the roles");
            return internal_error(true);
        }
    };
    let current_blocks = match app.blocks.current_for(player_id).await {
        Ok(blocks) => blocks,
        Err(error) => {
            error!(%error, "could not read current blocks");
            return internal_error(true);
        }
    };
    let search_results = match app
        .blocks
        .search_display_names(player_id, block_query)
        .await
    {
        Ok(results) => results,
        Err(error) => {
            error!(%error, "could not search block targets");
            return internal_error(true);
        }
    };
    let viewer = viewer(app, session).await;
    let status = if error.is_some() {
        StatusCode::BAD_REQUEST
    } else {
        StatusCode::OK
    };
    (
        status,
        Html(super::html::edit_page(super::html::EditPage {
            viewer,
            player_id: player_id.0,
            own: &own,
            known_roles: &known_roles,
            current_blocks: &current_blocks,
            block_query,
            search_results: &search_results,
            error,
            saved,
        })),
    )
        .into_response()
}

/// `application/x-www-form-urlencoded` を、**同じ名前の繰り返しを保ったまま**読む。
///
/// チェックボックスは同じ `name` で複数送られる。`serde_urlencoded`（axum の `Form`）は
/// 繰り返しを `Vec` に入れられないので、ここだけ自前で読む（依存は増やさない）。
fn parse_form(body: &str) -> Vec<(String, String)> {
    body.split('&')
        .filter(|pair| !pair.is_empty())
        .map(|pair| match pair.split_once('=') {
            Some((name, value)) => (percent_decode(name), percent_decode(value)),
            None => (percent_decode(pair), String::new()),
        })
        .collect()
}

/// `+` を空白に、`%XX` を 1 バイトに戻す。**読めないバイト列はそのまま落とす**
/// （壊れた入力で 500 にしない。検証はこの後で掛かる）。
fn percent_decode(value: &str) -> String {
    let bytes = value.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        match bytes[index] {
            b'+' => {
                out.push(b' ');
                index += 1;
            }
            b'%' if index + 2 < bytes.len() => {
                let hex = std::str::from_utf8(&bytes[index + 1..index + 3]).unwrap_or("");
                match u8::from_str_radix(hex, 16) {
                    Ok(byte) => {
                        out.push(byte);
                        index += 3;
                    }
                    Err(_) => {
                        out.push(bytes[index]);
                        index += 1;
                    }
                }
            }
            byte => {
                out.push(byte);
                index += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// 編集フォームの中身。**`player_id` は受け取らない。**
#[derive(Debug, Default)]
struct EditForm {
    bio: String,
    x_id: String,
    /// チェックされたロール。1 つも無ければ空。
    main_roles: Vec<String>,
    /// チェックされていれば true。
    main_roles_public: bool,
}

#[derive(Debug, Default)]
struct BlockForm {
    action: String,
    target: String,
    key: String,
}

impl BlockForm {
    fn from_body(body: &str) -> Self {
        let mut form = Self::default();
        for (name, value) in parse_form(body) {
            match name.as_str() {
                "action" => form.action = value,
                "target" => form.target = value,
                "key" => form.key = value,
                _ => {}
            }
        }
        form
    }
}

impl EditForm {
    fn from_body(body: &str) -> Self {
        let mut form = Self::default();
        for (name, value) in parse_form(body) {
            match name.as_str() {
                "bio" => form.bio = value,
                "x_id" => form.x_id = value,
                "main_roles" => form.main_roles.push(value),
                "main_roles_public" => form.main_roles_public = true,
                _ => {}
            }
        }
        form
    }
}

/// 保存する。**追記であって上書きではない。**
async fn save(
    State(app): State<ProfileState>,
    session: Option<CurrentSession>,
    body: String,
) -> Response {
    let Some(CurrentSession(current)) = session else {
        return session_expired();
    };
    let form = EditForm::from_body(&body);
    let known_roles = match app.profiles.known_roles().await {
        Ok(roles) => roles,
        Err(error) => {
            error!(%error, "could not read the roles");
            return internal_error(true);
        }
    };
    // **検証はすべてサーバ側。**画面の maxlength も pattern も当てにしない
    let edit = match validate_edit(
        &form.bio,
        Some(form.x_id.as_str()),
        &form.main_roles,
        form.main_roles_public,
        &known_roles,
    ) {
        Ok(edit) => edit,
        Err(error) => {
            let message = error.message();
            return render_edit(
                &app,
                session.as_ref(),
                current.player_id,
                Some(&message),
                false,
                "",
            )
            .await;
        }
    };
    if let Err(error) = app
        .profiles
        .save(current.player_id, &edit, Utc::now())
        .await
    {
        error!(%error, "could not save the profile");
        return internal_error(true);
    }
    render_edit(&app, session.as_ref(), current.player_id, None, true, "").await
}

async fn change_block(
    State(app): State<ProfileState>,
    session: Option<CurrentSession>,
    body: String,
) -> Response {
    let Some(CurrentSession(current)) = session else {
        return session_expired();
    };
    let form = BlockForm::from_body(&body);
    let Ok(target) = Uuid::parse_str(form.target.trim()) else {
        return render_edit(
            &app,
            session.as_ref(),
            current.player_id,
            Some("ブロックする相手を選択してください。"),
            false,
            "",
        )
        .await;
    };
    let Some(target) = (match app.profiles.find_player(target).await {
        Ok(player) => player,
        Err(error) => {
            error!(%error, "could not validate a block target");
            return internal_error(true);
        }
    }) else {
        return render_edit(
            &app,
            session.as_ref(),
            current.player_id,
            Some("ブロックする相手を選択してください。"),
            false,
            "",
        )
        .await;
    };
    if form.key.trim().is_empty() {
        return render_edit(
            &app,
            session.as_ref(),
            current.player_id,
            Some("操作をやり直してください。"),
            false,
            "",
        )
        .await;
    }
    let result = match form.action.as_str() {
        "block" => {
            app.blocks
                .block(current.player_id, target, &form.key, Utc::now())
                .await
        }
        "unblock" => {
            app.blocks
                .unblock(current.player_id, target, &form.key, Utc::now())
                .await
        }
        _ => {
            return render_edit(
                &app,
                session.as_ref(),
                current.player_id,
                Some("操作をやり直してください。"),
                false,
                "",
            )
            .await;
        }
    };
    match result {
        Ok(()) => render_edit(&app, session.as_ref(), current.player_id, None, true, "").await,
        Err(error) => {
            render_edit(
                &app,
                session.as_ref(),
                current.player_id,
                Some(block_error_message(&error)),
                false,
                "",
            )
            .await
        }
    }
}

fn block_error_message(error: &UserBlockError) -> &'static str {
    match error {
        UserBlockError::SelfTarget => "自分自身はブロックできません。",
        UserBlockError::AlreadyBlocked => "このプレイヤーはすでにブロックしています。",
        UserBlockError::AllowanceFull { .. } => "ブロック枠が埋まっています。",
        UserBlockError::NotBlocked => "このプレイヤーは現在ブロックしていません。",
        UserBlockError::Cooldown { .. } => "このブロックはまだ解除できません。",
        UserBlockError::Database(_) => {
            "操作を完了できませんでした。時間をおいて再試行してください。"
        }
    }
}

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

fn not_found(html: bool) -> Response {
    if html {
        return (
            StatusCode::NOT_FOUND,
            Html(error_page(
                Viewer::ANONYMOUS,
                "見つかりません",
                "そのプレイヤーは存在しません。",
            )),
        )
            .into_response();
    }
    (
        StatusCode::NOT_FOUND,
        Json(serde_json::json!({ "error": "not_found" })),
    )
        .into_response()
}

fn internal_error(html: bool) -> Response {
    if html {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Html(error_page(
                Viewer::ANONYMOUS,
                "エラーが発生しました",
                "時間をおいてもう一度お試しください。",
            )),
        )
            .into_response();
    }
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        Json(serde_json::json!({ "error": "internal_error" })),
    )
        .into_response()
}

fn session_expired() -> Response {
    (StatusCode::UNAUTHORIZED, Redirect::to("/")).into_response()
}
