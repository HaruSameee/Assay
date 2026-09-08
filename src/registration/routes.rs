//! 登録・認証のエンドポイント。
//!
//! - `GET  /`              入口。ログイン済みなら [`landing_path`] の行き先へ
//! - `GET  /auth/login`    state を発行して Discord の認可画面へ
//! - `GET  /auth/callback` state を検証して**即座に破棄**し、コードを交換してセッションを張る
//! - `POST /auth/logout`   セッション Cookie を消す
//! - `GET  /register`      登録フォーム（登録済みなら現在の値が入った状態で編集できる）
//! - `POST /register`      申告を 1 行追記し、3つのゲートの結果を返す
//!
//! **登録済みの人に空のフォームを見せない。**ログイン直後の行き先は
//! [`landing_path`] が `registered_at` だけで決める（未登録はフォーム、登録済みは
//! キュー画面）。登録内容の変更は `/register` を自分で開けばいつでもできる。
//!
//! CSRF: セッション Cookie は `SameSite=Lax` なので、他サイトからの POST には送られない。
//! `POST /register` と `POST /auth/logout` の保護はこれに拠る。

use std::sync::Arc;

use axum::extract::{FromRef, Query, State};
use axum::http::StatusCode;
use axum::response::{Html, IntoResponse, Redirect, Response};
use axum::routing::{get, post};
use axum::{Form, Router};
use axum_extra::extract::SignedCookieJar;
use axum_extra::extract::cookie::Key;
use chrono::Utc;
use serde::Deserialize;
use serde_json::Value;
use tracing::{error, info, warn};

use crate::admin::AdminIds;
use crate::lifecycle::PlayerId;

use super::config::AuthConfig;
use super::html;
use super::model::{NewDeclaration, Rank};
use super::oauth::{OAuthClient, authorize_url, new_state};
use super::repository::{RegistrationError, RegistrationRepository};
use super::session::{
    CurrentSession, STATE_COOKIE, Session, clear_session, clear_state, set_session, set_state,
};
use super::viewer::{Audience, Viewer};

/// パス。リダイレクト URI 側のパスは設定（`OAUTH_REDIRECT_URI`）と一致させること。
const LOGIN_PATH: &str = "/auth/login";
const CALLBACK_PATH: &str = "/auth/callback";
const LOGOUT_PATH: &str = "/auth/logout";
const REGISTER_PATH: &str = "/register";
const ROOT_PATH: &str = "/";
/// キュー画面。ルーターは [`crate::queue`] 側だが、登録済みの行き先として参照する。
const QUEUE_PATH: &str = "/queue";

/// トレーナー名の最大長。DB は text だが、表示と入力欄に合わせて上限を設ける。
const TRAINER_NAME_MAX_CHARS: usize = 64;
/// 申告レートの上限。妥当性の検証はしない（05-登録トラック仕様 §7）が、桁あふれは弾く。
const RATING_MAX: i32 = 100_000;

/// 登録・認証のルーターが持つ状態。
#[derive(Clone)]
pub struct AuthState {
    /// 設定。
    pub config: AuthConfig,
    /// Discord とのやりとり。
    pub oauth: Arc<dyn OAuthClient>,
    /// 永続化。
    pub repo: RegistrationRepository,
    /// Cookie の署名鍵（`config` から導出したものを保持する）。
    key: Key,
    /// ヘッダーに管理パネルの導線を出す相手の判定。既定は「管理者なし」。
    audience: Audience,
    /// Discord 側の見た目を追記する先。**渡さなければ何も積まない**
    /// （既存のログイン経路をそのまま残せるように任意にしてある）。
    profiles: Option<crate::profiles::ProfileRepository>,
}

impl AuthState {
    /// 組み立てる。
    pub fn new(
        config: AuthConfig,
        oauth: Arc<dyn OAuthClient>,
        repo: RegistrationRepository,
    ) -> Self {
        let key = config.cookie_key();
        Self {
            config,
            oauth,
            repo,
            key,
            audience: Audience::default(),
            profiles: None,
        }
    }

    /// 管理者リストを渡す。**渡さなければ誰にも管理パネルの導線を出さない。**
    pub fn with_admins(mut self, admins: Option<AdminIds>) -> Self {
        self.audience = Audience::new(admins);
        self
    }

    /// ログインのたびに Discord 側の見た目を積む先を渡す。
    ///
    /// **渡さなければ何も積まない。**積む処理が失敗してもログインは通す
    /// （ここで落とすと、スナップショットの不具合だけで全員が入れなくなる）。
    pub fn with_profiles(mut self, profiles: crate::profiles::ProfileRepository) -> Self {
        self.profiles = Some(profiles);
        self
    }
}

impl FromRef<AuthState> for Key {
    fn from_ref(state: &AuthState) -> Self {
        state.key.clone()
    }
}

impl FromRef<AuthState> for AuthConfig {
    fn from_ref(state: &AuthState) -> Self {
        state.config.clone()
    }
}

/// ルーターを組み立てる。
pub fn router(state: AuthState) -> Router {
    Router::new()
        .route(ROOT_PATH, get(index))
        .route(LOGIN_PATH, get(begin_login))
        .route(CALLBACK_PATH, get(callback))
        .route(LOGOUT_PATH, post(logout))
        .route(REGISTER_PATH, get(show_form).post(submit_form))
        .with_state(state)
}

/// ログイン済みの人を送る先。
///
/// **登録済みなら登録フォームを見せない。**同じ内容を毎回入力させないための分岐で、
/// 判定は `registered_at` が非 NULL かどうかだけ（ゲートの判定は一切見ない。
/// キューに入れるかどうかはキュー画面が3つのゲートで説明する）。
///
/// プロフィールが読めないときは安全側にフォームへ送る（フォームは現在の値を出すので、
/// 登録済みの人が誤って来ても失うものが無い）。
async fn landing_path(app: &AuthState, player_id: PlayerId) -> &'static str {
    match app.repo.find_by_player_id(player_id).await {
        Ok(Some(profile)) if profile.is_registered() => QUEUE_PATH,
        Ok(_) => REGISTER_PATH,
        Err(error) => {
            error!(%error, "could not decide where to send the player after sign-in");
            REGISTER_PATH
        }
    }
}

/// 入口。
async fn index(State(app): State<AuthState>, session: Option<CurrentSession>) -> Response {
    match session {
        Some(CurrentSession(session)) => {
            Redirect::to(landing_path(&app, session.player_id).await).into_response()
        }
        None => Html(html::login_page(LOGIN_PATH)).into_response(),
    }
}

/// state を作って Cookie に入れ、Discord の認可画面へ送る。
async fn begin_login(State(state): State<AuthState>, jar: SignedCookieJar) -> Response {
    let token = new_state();
    let url = authorize_url(&state.config, &token);
    // state は毎回作り直す。使い回さない
    let jar = set_state(jar, &token, &state.config);
    (jar, Redirect::to(&url)).into_response()
}

/// コールバックのクエリ。
#[derive(Debug, Deserialize)]
struct CallbackQuery {
    #[serde(default)]
    code: Option<String>,
    #[serde(default)]
    state: Option<String>,
    /// ユーザーが同意しなかった場合など。
    #[serde(default)]
    error: Option<String>,
}

/// 認可コードを受け取る。**state Cookie は成否によらず必ず破棄する。**
async fn callback(
    State(app): State<AuthState>,
    jar: SignedCookieJar,
    Query(query): Query<CallbackQuery>,
) -> Response {
    // 何があってもここで state を捨てる（使い回さない）
    let stored = jar
        .get(STATE_COOKIE)
        .map(|cookie| cookie.value().to_owned());
    let jar = clear_state(jar, &app.config);

    let fail = |jar: SignedCookieJar, status: StatusCode, message: &str| {
        // ステータスを先に置く（axum の IntoResponse は (StatusCode, parts.., body) の順を要求する）
        // ログインが成立していないので、ヘッダーは未ログインのまま
        (
            status,
            jar,
            Html(html::error_page(
                Viewer::ANONYMOUS,
                "ログインできませんでした",
                message,
            )),
        )
            .into_response()
    };

    if let Some(reason) = query.error {
        info!(reason, "discord returned an oauth error");
        return fail(
            jar,
            StatusCode::BAD_REQUEST,
            "Discord 側でログインが中断されました。もう一度お試しください。",
        );
    }
    let (Some(code), Some(returned_state)) = (query.code, query.state) else {
        return fail(
            jar,
            StatusCode::BAD_REQUEST,
            "リクエストが不完全です。もう一度ログインしてください。",
        );
    };
    let Some(expected) = stored else {
        return fail(
            jar,
            StatusCode::BAD_REQUEST,
            "ログインの有効期限が切れました。もう一度ログインしてください。",
        );
    };
    if !constant_time_eq(expected.as_bytes(), returned_state.as_bytes()) {
        warn!("oauth state mismatch");
        return fail(
            jar,
            StatusCode::BAD_REQUEST,
            "ログインを検証できませんでした。もう一度ログインしてください。",
        );
    }

    // client_secret はここ（リクエストボディ）にだけ入る。エラーにも載せない
    let access_token = match app.oauth.exchange_code(&code).await {
        Ok(token) => token,
        Err(error) => {
            warn!(%error, "token exchange failed");
            return fail(
                jar,
                StatusCode::BAD_GATEWAY,
                "Discord と通信できませんでした。少し待ってからお試しください。",
            );
        }
    };
    let user = match app.oauth.current_user(&access_token).await {
        Ok(user) => user,
        Err(error) => {
            warn!(%error, "fetching the discord user failed");
            return fail(
                jar,
                StatusCode::BAD_GATEWAY,
                "Discord と通信できませんでした。少し待ってからお試しください。",
            );
        }
    };

    let player_id = match app.repo.ensure_player(&user.id, Utc::now()).await {
        Ok(player_id) => player_id,
        Err(error) => {
            error!(%error, "could not create or find the player row");
            return fail(
                jar,
                StatusCode::INTERNAL_SERVER_ERROR,
                "登録の準備に失敗しました。時間をおいてお試しください。",
            );
        }
    };
    // Discord 側の見た目を追記する。**失敗してもログインは通す。**
    // ここで落とすと、スナップショットの不具合だけで全員が入れなくなる。
    // 表示名の履歴を残すのは、係争の裁定で discord_user_id しか手掛かりが
    // 無いという問題への対処（当時の名前が分からないと後から追えない）
    if let Some(profiles) = app.profiles.as_ref()
        && let Err(error) = profiles
            .record_appearance(
                player_id,
                user.display_name(),
                user.avatar.as_deref(),
                Utc::now(),
            )
            .await
    {
        warn!(%error, %player_id, "could not record the discord appearance; sign-in continues");
    }
    info!(%player_id, "signed in");
    let jar = set_session(jar, &Session::issue(player_id), &app.config);
    // 登録済みならフォームを飛ばしてキューへ。ここで初めてセッションが張られる
    let destination = landing_path(&app, player_id).await;
    (jar, Redirect::to(destination)).into_response()
}

/// ログアウト。セッション Cookie を消すだけ（ストアが無いので失効管理も無い）。
async fn logout(State(app): State<AuthState>, jar: SignedCookieJar) -> Response {
    (clear_session(jar, &app.config), Redirect::to(ROOT_PATH)).into_response()
}

/// 登録フォーム。
async fn show_form(State(app): State<AuthState>, session: Option<CurrentSession>) -> Response {
    let Some(CurrentSession(session)) = session else {
        return session_expired();
    };
    match app.repo.find_by_player_id(session.player_id).await {
        Ok(Some(profile)) => {
            // 登録済みなら現在の値がそのまま入る（空のフォームを出さない）
            let values = html::RegistrationFormValues::from_profile(&profile);
            Html(html::registration_form(
                app.audience.viewer_of(Some(&profile)),
                REGISTER_PATH,
                &profile,
                &values,
                &html::RegistrationFormErrors::default(),
            ))
            .into_response()
        }
        // セッションは有効だが行が無い（DB を入れ替えた後など）。ログインし直させる
        Ok(None) => Redirect::to(ROOT_PATH).into_response(),
        Err(error) => internal_error(error),
    }
}

/// 登録フォームの入力。**受け取った値はそのまま `raw` に残す。**
#[derive(Debug, Deserialize)]
struct RegistrationForm {
    #[serde(default)]
    trainer_name: String,
    #[serde(default)]
    rank: String,
    #[serde(default)]
    rating: String,
}

/// フォームの検証結果。
struct ValidatedForm {
    trainer_name: String,
    rank: Rank,
    rating: Option<i32>,
}

/// フォームを検証する。純粋関数（テストしやすいように分けてある）。
fn validate(form: &RegistrationForm) -> Result<ValidatedForm, html::RegistrationFormErrors> {
    let mut errors = html::RegistrationFormErrors::default();
    let trainer_name = form.trainer_name.trim();
    if trainer_name.is_empty() {
        errors.trainer_name = Some("トレーナー名を入力してください。");
    } else if trainer_name.chars().count() > TRAINER_NAME_MAX_CHARS {
        errors.trainer_name = Some("トレーナー名は64文字以内で入力してください。");
    }

    let rank = match form.rank.trim().parse::<Rank>() {
        Ok(rank) => Some(rank),
        Err(_) => {
            errors.rank = Some("本家ランクを選択してください。");
            None
        }
    };
    let rating_text = form.rating.trim();
    let rating = if rank.is_some_and(Rank::requires_rating) {
        match rating_text.parse::<i32>() {
            Ok(value) if (0..=RATING_MAX).contains(&value) => Some(value),
            Ok(_) => {
                errors.rating = Some("本家レートは0から100000の範囲で入力してください。");
                None
            }
            Err(_) => {
                errors.rating = Some("本家レートを数字で入力してください。");
                None
            }
        }
    } else {
        // マスター・レジェンド以外はレートを持たない。入力があっても raw にだけ残す
        None
    };

    if !errors.is_empty() {
        return Err(errors);
    }
    let Some(rank) = rank else {
        return Err(errors);
    };
    Ok(ValidatedForm {
        trainer_name: trainer_name.to_owned(),
        rank,
        rating,
    })
}

fn form_values(form: &RegistrationForm) -> html::RegistrationFormValues {
    html::RegistrationFormValues {
        trainer_name: form.trainer_name.clone(),
        rank: form.rank.clone(),
        rating: form.rating.clone(),
    }
}

/// 送信されたフォームを丸ごと JSON にする（パースできなかった項目も残す）。
fn raw_form(form: &RegistrationForm) -> Value {
    RegistrationRepository::raw_declaration(&[
        ("trainer_name", form.trainer_name.as_str()),
        ("rank", form.rank.as_str()),
        ("rating", form.rating.as_str()),
    ])
}

/// 申告を追記する。再送は毎回新しい行になる（同じ内容でも捨てない）。
async fn submit_form(
    State(app): State<AuthState>,
    session: Option<CurrentSession>,
    Form(form): Form<RegistrationForm>,
) -> Response {
    let Some(CurrentSession(session)) = session else {
        return session_expired();
    };
    let profile = match app.repo.find_by_player_id(session.player_id).await {
        Ok(Some(profile)) => profile,
        Ok(None) => return Redirect::to(ROOT_PATH).into_response(),
        Err(error) => return internal_error(error),
    };

    let validated = match validate(&form) {
        Ok(validated) => validated,
        Err(errors) => {
            let values = form_values(&form);
            return (
                StatusCode::BAD_REQUEST,
                Html(html::registration_form(
                    app.audience.viewer_of(Some(&profile)),
                    REGISTER_PATH,
                    &profile,
                    &values,
                    &errors,
                )),
            )
                .into_response();
        }
    };

    let declaration = NewDeclaration {
        player_id: session.player_id,
        declared_at: Utc::now(),
        trainer_name: validated.trainer_name,
        rank: validated.rank,
        rating: validated.rating,
        raw: raw_form(&form),
    };
    if let Err(error) = app.repo.register(&declaration).await {
        error!(%error, "could not record the declaration");
        return internal_error(error);
    }
    info!(player = %session.player_id, rank = %validated.rank, "declaration recorded");

    // 保存後の状態でゲートを評価して返す（2つの理由は畳まない）
    let (profile, eligibility) = match (
        app.repo.find_by_player_id(session.player_id).await,
        app.repo.queue_eligibility(session.player_id).await,
    ) {
        (Ok(Some(profile)), Ok(eligibility)) => (profile, eligibility),
        (Err(error), _) | (_, Err(error)) => return internal_error(error),
        (Ok(None), _) => return Redirect::to(ROOT_PATH).into_response(),
    };
    Html(html::registration_result(
        app.audience.viewer_of(Some(&profile)),
        &profile,
        &eligibility,
    ))
    .into_response()
}

/// 内部エラー。詳細はログに残し、画面には出さない。
///
/// ここは DB が読めていない経路なので、管理者かどうかも確かめられない。
/// ヘッダーは未ログイン扱いで出す（導線を出さない方に倒す）。
fn internal_error(error: RegistrationError) -> Response {
    error!(%error, "registration request failed");
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        Html(html::error_page(
            Viewer::ANONYMOUS,
            "エラー",
            "処理に失敗しました。時間をおいてお試しください。",
        )),
    )
        .into_response()
}

fn session_expired() -> Response {
    (
        StatusCode::UNAUTHORIZED,
        Html(html::error_page(
            Viewer::ANONYMOUS,
            "セッションの有効期限が切れました",
            "ログイン状態を確認できませんでした。入口に戻って、Discord で再度ログインしてください。",
        )),
    )
        .into_response()
}

/// 長さに依存しない比較（state の突き合わせ）。
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    let mut diff = (a.len() ^ b.len()) as u8;
    for (x, y) in a.iter().zip(b.iter().chain(std::iter::repeat(&0))) {
        diff |= x ^ y;
    }
    diff == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    fn form(trainer: &str, rank: &str, rating: &str) -> RegistrationForm {
        RegistrationForm {
            trainer_name: trainer.to_owned(),
            rank: rank.to_owned(),
            rating: rating.to_owned(),
        }
    }

    #[test]
    fn a_legend_declaration_needs_a_numeric_rating() {
        let valid = validate(&form(" トレーナー ", "legend", " 1500 ")).unwrap();
        assert_eq!(valid.trainer_name, "トレーナー", "trimmed");
        assert_eq!(valid.rank, Rank::Legend);
        assert_eq!(valid.rating, Some(1500));

        assert!(validate(&form("t", "legend", "")).is_err());
        assert!(validate(&form("t", "legend", "つよい")).is_err());
        assert!(validate(&form("t", "master", "-1")).is_err());
        assert!(validate(&form("t", "master", "100001")).is_err());
    }

    #[test]
    fn ranks_below_master_do_not_carry_a_rating() {
        let valid = validate(&form("t", "elite", "9999")).unwrap();
        assert_eq!(valid.rank, Rank::Elite);
        // 入力があっても列には入れない。raw には残る
        assert_eq!(valid.rating, None);
    }

    #[test]
    fn the_trainer_name_is_required_and_bounded() {
        assert!(validate(&form("", "legend", "1500")).is_err());
        assert!(validate(&form("   ", "legend", "1500")).is_err());
        let long = "あ".repeat(TRAINER_NAME_MAX_CHARS);
        assert!(validate(&form(&long, "legend", "1500")).is_ok());
        let too_long = "あ".repeat(TRAINER_NAME_MAX_CHARS + 1);
        assert!(validate(&form(&too_long, "legend", "1500")).is_err());
    }

    #[test]
    fn an_unknown_rank_is_rejected() {
        assert!(validate(&form("t", "", "1500")).is_err());
        assert!(validate(&form("t", "diamond", "1500")).is_err());
    }

    #[test]
    fn the_raw_json_keeps_what_was_submitted_even_when_invalid() {
        let submitted = form("  トレーナー  ", "legend", "つよい");
        assert!(validate(&submitted).is_err());
        let raw = raw_form(&submitted);
        // トリムも正規化もせず、送られてきたまま
        assert_eq!(raw["trainer_name"], "  トレーナー  ");
        assert_eq!(raw["rating"], "つよい");
        assert_eq!(raw["rank"], "legend");
    }

    #[test]
    fn state_comparison_handles_length_differences() {
        assert!(constant_time_eq(b"abc", b"abc"));
        assert!(!constant_time_eq(b"abc", b"abd"));
        assert!(!constant_time_eq(b"abc", b"ab"));
        assert!(!constant_time_eq(b"", b"a"));
        assert!(constant_time_eq(b"", b""));
    }
}
