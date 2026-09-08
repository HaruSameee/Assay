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

use axum::extract::{FromRef, Path, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{Html, IntoResponse, Redirect, Response};
use axum::routing::get;
use axum::{Json, Router};
use axum_extra::extract::cookie::Key;
use chrono::Utc;
use tracing::error;
use uuid::Uuid;

use crate::admin::AdminIds;
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
    ) -> Self {
        let key = config.cookie_key();
        Self {
            config,
            profiles,
            registration,
            key,
            audience: Audience::default(),
        }
    }

    /// 管理者リストを渡す。渡さなければ誰にも管理パネルの導線を出さない。
    pub fn with_admins(mut self, admins: Option<AdminIds>) -> Self {
        self.audience = Audience::new(admins);
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
        .with_state(state)
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
async fn show_edit(State(app): State<ProfileState>, session: Option<CurrentSession>) -> Response {
    let Some(CurrentSession(current)) = session else {
        return session_expired();
    };
    render_edit(&app, session.as_ref(), current.player_id, None, false).await
}

/// 編集画面を描く（保存後の再描画にも使う）。
async fn render_edit(
    app: &ProfileState,
    session: Option<&CurrentSession>,
    player_id: PlayerId,
    error: Option<&str>,
    saved: bool,
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
    let viewer = viewer(app, session).await;
    let status = if error.is_some() {
        StatusCode::BAD_REQUEST
    } else {
        StatusCode::OK
    };
    (
        status,
        Html(super::html::edit_page(
            viewer,
            player_id.0,
            &own,
            &known_roles,
            error,
            saved,
        )),
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
    render_edit(&app, session.as_ref(), current.player_id, None, true).await
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
