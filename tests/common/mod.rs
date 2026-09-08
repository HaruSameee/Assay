//! 統合テスト共通の道具（テストバイナリからは `mod common;` で使う）。
//!
//! セッションは**本物の登録フロー**（`/auth/login` → 偽 OAuth → `/auth/callback`）で
//! 張る。署名済み Cookie を手作りで再現すると署名の検証条件と遊んでしまうため、
//! 登録トラックと同じ経路を通るほうが正直（`tests/registration.rs` と同じ偽 OAuth）。

#![allow(dead_code, reason = "テストバイナリごとに使う部分だけ使う")]

use std::collections::HashMap;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use axum::Router;
use axum::body::Body;
use axum::http::{Request, Response, StatusCode, header};
use chrono::{TimeDelta, TimeZone, Utc};
use http_body_util::BodyExt;
use serenity::async_trait;
use tower::ServiceExt;
use unite_matchsystem::lifecycle::{PlayerId, Timestamp};
use unite_matchsystem::registration::{
    AuthConfig, DiscordUser, OAuthClient, OAuthError, RegistrationRepository,
};
use uuid::Uuid;

pub const APP_ROLE: &str = "assay_app";
pub const APP_ROLE_PASSWORD: &str = "assay_app_test";
pub const COOKIE_DOMAIN: &str = "match.example.test";
const STATE_COOKIE: &str = "assay_oauth_state";

pub fn config() -> AuthConfig {
    AuthConfig::new(
        "client-id",
        "client-secret",
        "https://match.example.test/auth/callback",
        COOKIE_DOMAIN,
        "k".repeat(64),
        Duration::from_secs(3600),
    )
    .expect("valid test config")
}

pub fn at(seconds: i64) -> Timestamp {
    Utc.with_ymd_and_hms(2026, 9, 12, 21, 0, 0).unwrap() + TimeDelta::seconds(seconds)
}

pub fn player(n: u128) -> PlayerId {
    PlayerId(Uuid::from_u128(n))
}

/// 偽の Discord。HTTP を出さずに OAuth の応答を差し替える。
pub struct FakeOAuth {
    user_id: Mutex<String>,
    exchanged_codes: Mutex<Vec<String>>,
    fail: AtomicBool,
    /// 返すアバターのハッシュ。`None` なら未設定（既定アバター）。
    avatar: Mutex<Option<String>>,
    /// 返す表示名。
    display_name: Mutex<Option<String>>,
}

impl FakeOAuth {
    pub fn new(user_id: &str) -> Self {
        Self {
            user_id: Mutex::new(user_id.to_owned()),
            exchanged_codes: Mutex::new(Vec::new()),
            fail: AtomicBool::new(false),
            avatar: Mutex::new(None),
            display_name: Mutex::new(Some("表示名".to_owned())),
        }
    }

    /// 同じ偽 Discord で別ユーザーとしてログインし直す。
    pub fn set_user_id(&self, user_id: &str) {
        *self.user_id.lock().unwrap() = user_id.to_owned();
    }

    /// Discord 側のアバターを差し替える（`None` は未設定）。
    pub fn set_avatar(&self, avatar: Option<&str>) {
        *self.avatar.lock().unwrap() = avatar.map(ToOwned::to_owned);
    }

    /// Discord 側の表示名を差し替える。
    pub fn set_display_name(&self, name: Option<&str>) {
        *self.display_name.lock().unwrap() = name.map(ToOwned::to_owned);
    }
}

#[async_trait]
impl OAuthClient for FakeOAuth {
    async fn exchange_code(&self, code: &str) -> Result<String, OAuthError> {
        if self.fail.load(Ordering::SeqCst) {
            return Err(OAuthError::Rejected(401));
        }
        self.exchanged_codes.lock().unwrap().push(code.to_owned());
        Ok(format!("access-token-for-{code}"))
    }

    async fn current_user(&self, _access_token: &str) -> Result<DiscordUser, OAuthError> {
        if self.fail.load(Ordering::SeqCst) {
            return Err(OAuthError::Transport);
        }
        Ok(DiscordUser {
            id: self.user_id.lock().unwrap().clone(),
            username: "handle".into(),
            global_name: self.display_name.lock().unwrap().clone(),
            avatar: self.avatar.lock().unwrap().clone(),
        })
    }
}

/// Cookie を持ち回るブラウザもどき。
pub struct Browser {
    app: Router,
    cookies: HashMap<String, String>,
}

impl Browser {
    pub fn new(app: Router) -> Self {
        Self {
            app,
            cookies: HashMap::new(),
        }
    }

    fn cookie_header(&self) -> Option<String> {
        if self.cookies.is_empty() {
            return None;
        }
        let mut pairs: Vec<String> = self
            .cookies
            .iter()
            .map(|(name, value)| format!("{name}={value}"))
            .collect();
        pairs.sort();
        Some(pairs.join("; "))
    }

    /// レスポンスの `Set-Cookie` を取り込む。空値 / `Max-Age=0` は削除として扱う。
    fn absorb(&mut self, response: &Response<Body>) {
        for header in response.headers().get_all(header::SET_COOKIE) {
            let Ok(raw) = header.to_str() else { continue };
            let mut parts = raw.split(';');
            let Some((name, value)) = parts.next().and_then(|pair| pair.split_once('=')) else {
                continue;
            };
            let removed = value.is_empty()
                || raw
                    .split(';')
                    .any(|attribute| attribute.trim().eq_ignore_ascii_case("Max-Age=0"));
            if removed {
                self.cookies.remove(name);
            } else {
                self.cookies.insert(name.to_owned(), value.to_owned());
            }
        }
    }

    pub async fn send(&mut self, request: Request<Body>) -> Response<Body> {
        let response = self.app.clone().oneshot(request).await.unwrap();
        self.absorb(&response);
        response
    }

    pub async fn get(&mut self, uri: &str) -> Response<Body> {
        let mut builder = Request::get(uri);
        if let Some(cookies) = self.cookie_header() {
            builder = builder.header(header::COOKIE, cookies);
        }
        self.send(builder.body(Body::empty()).unwrap()).await
    }

    pub async fn post_form(&mut self, uri: &str, body: &str) -> Response<Body> {
        let mut builder =
            Request::post(uri).header(header::CONTENT_TYPE, "application/x-www-form-urlencoded");
        if let Some(cookies) = self.cookie_header() {
            builder = builder.header(header::COOKIE, cookies);
        }
        self.send(builder.body(Body::from(body.to_owned())).unwrap())
            .await
    }
}

pub async fn body_text(response: Response<Body>) -> String {
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    String::from_utf8(bytes.to_vec()).unwrap()
}

fn location(response: &Response<Body>) -> String {
    response
        .headers()
        .get(header::LOCATION)
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default()
        .to_owned()
}

/// 認可 URL のクエリから `state` を取る。
fn state_from_redirect(response: &Response<Body>) -> String {
    location(response)
        .split("state=")
        .nth(1)
        .expect("state in the authorize url")
        .split('&')
        .next()
        .expect("state value")
        .to_owned()
}

/// ログインを通してセッションを張り、`players.id` を返す。
///
/// `players` に先に同じ `discord_user_id` の行を置いておくと、その行が再利用される
/// （`signing_in_twice_reuses_the_same_player_row` と同じ性質に依存する）。
pub async fn sign_in(
    browser: &mut Browser,
    repo: &RegistrationRepository,
    discord_user_id: &str,
) -> PlayerId {
    let login = browser.get("/auth/login").await;
    assert_eq!(login.status(), StatusCode::SEE_OTHER);
    assert!(
        browser.cookies.contains_key(STATE_COOKIE),
        "state cookie is set"
    );
    let state = state_from_redirect(&login);
    let callback = browser
        .get(&format!("/auth/callback?code=abc&state={state}"))
        .await;
    assert_eq!(callback.status(), StatusCode::SEE_OTHER);
    assert!(
        browser.cookies.contains_key("assay_session"),
        "session cookie is set"
    );
    repo.find_by_discord_user_id(discord_user_id)
        .await
        .unwrap()
        .expect("player row exists after sign-in")
        .player_id
}
