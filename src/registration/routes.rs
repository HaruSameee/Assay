//! 登録・認証のエンドポイント。
//!
//! - `GET  /`              入口。ログイン済みなら登録フォームへ
//! - `GET  /auth/login`    state を発行して Discord の認可画面へ
//! - `GET  /auth/callback` state を検証して**即座に破棄**し、コードを交換してセッションを張る
//! - `POST /auth/logout`   セッション Cookie を消す
//! - `GET  /register`      登録フォーム
//! - `POST /register`      申告を 1 行追記し、2つのゲートの結果を返す
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

use super::config::AuthConfig;
use super::html;
use super::model::{NewDeclaration, Rank};
use super::oauth::{OAuthClient, authorize_url, new_state};
use super::repository::{RegistrationError, RegistrationRepository};
use super::session::{
    CurrentSession, STATE_COOKIE, Session, clear_session, clear_state, set_session, set_state,
};

/// パス。リダイレクト URI 側のパスは設定（`OAUTH_REDIRECT_URI`）と一致させること。
const LOGIN_PATH: &str = "/auth/login";
const CALLBACK_PATH: &str = "/auth/callback";
const LOGOUT_PATH: &str = "/auth/logout";
const REGISTER_PATH: &str = "/register";
const ROOT_PATH: &str = "/";

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
        }
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

/// 入口。
async fn index(session: Option<CurrentSession>) -> Response {
    match session {
        Some(_) => Redirect::to(REGISTER_PATH).into_response(),
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
        (
            status,
            jar,
            Html(html::error_page("ログインできませんでした", message)),
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
    info!(%player_id, "signed in");
    let jar = set_session(jar, &Session::issue(player_id), &app.config);
    (jar, Redirect::to(REGISTER_PATH)).into_response()
}

/// ログアウト。セッション Cookie を消すだけ（ストアが無いので失効管理も無い）。
async fn logout(State(app): State<AuthState>, jar: SignedCookieJar) -> Response {
    (clear_session(jar, &app.config), Redirect::to(ROOT_PATH)).into_response()
}

/// 登録フォーム。
async fn show_form(State(app): State<AuthState>, session: Option<CurrentSession>) -> Response {
    let Some(CurrentSession(session)) = session else {
        // 期限切れ・未ログインはどちらも入口に戻す（再ログインに落とす）
        return Redirect::to(ROOT_PATH).into_response();
    };
    match app.repo.find_by_player_id(session.player_id).await {
        Ok(Some(profile)) => {
            let display = profile.trainer_name.clone().unwrap_or_default();
            Html(html::registration_form(
                REGISTER_PATH,
                &display,
                Some(&profile),
                None,
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
fn validate(form: &RegistrationForm) -> Result<ValidatedForm, &'static str> {
    let trainer_name = form.trainer_name.trim();
    if trainer_name.is_empty() {
        return Err("トレーナー名を入力してください。");
    }
    if trainer_name.chars().count() > TRAINER_NAME_MAX_CHARS {
        return Err("トレーナー名が長すぎます。");
    }
    let rank: Rank = form
        .rank
        .trim()
        .parse()
        .map_err(|_| "ランクを選択してください。")?;
    let rating_text = form.rating.trim();
    let rating = if rank.requires_rating() {
        let value: i32 = rating_text
            .parse()
            .map_err(|_| "本家レートを数字で入力してください。")?;
        if !(0..=RATING_MAX).contains(&value) {
            return Err("本家レートの値を確認してください。");
        }
        Some(value)
    } else {
        // マスター・レジェンド以外はレートを持たない。入力があっても raw にだけ残す
        None
    };
    Ok(ValidatedForm {
        trainer_name: trainer_name.to_owned(),
        rank,
        rating,
    })
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
        return Redirect::to(ROOT_PATH).into_response();
    };
    let profile = match app.repo.find_by_player_id(session.player_id).await {
        Ok(Some(profile)) => profile,
        Ok(None) => return Redirect::to(ROOT_PATH).into_response(),
        Err(error) => return internal_error(error),
    };

    let validated = match validate(&form) {
        Ok(validated) => validated,
        Err(message) => {
            let display = profile.trainer_name.clone().unwrap_or_default();
            return (
                StatusCode::BAD_REQUEST,
                Html(html::registration_form(
                    REGISTER_PATH,
                    &display,
                    Some(&profile),
                    Some(message),
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
    Html(html::registration_result(&profile, &eligibility)).into_response()
}

/// 内部エラー。詳細はログに残し、画面には出さない。
fn internal_error(error: RegistrationError) -> Response {
    error!(%error, "registration request failed");
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        Html(html::error_page(
            "エラー",
            "処理に失敗しました。時間をおいてお試しください。",
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
