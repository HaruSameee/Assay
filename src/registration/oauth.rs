//! Discord OAuth2（認可コードフロー）。**scope は `identify` だけ**。
//!
//! `email` も `guilds` も要求しない（05-登録トラック仕様 §1）。要求するスコープが少ないほど
//! 登録画面の同意ハードルが下がる。ギルド所属の確認が要るなら bot 側から問い合わせるほうが安い。
//!
//! `client_secret` はトークン交換のリクエストボディにだけ入れる。URL・ログ・エラー・レスポンスに
//! 出さない（[`OAuthError`] は Discord の応答本文をそのまま載せない）。
//!
//! [`OAuthClient`] を trait にしてあるので、テストは HTTP を出さずに偽物へ差し替えられる。

use std::fmt::Write as _;
use std::time::Duration;

use serde::Deserialize;
use serenity::async_trait;

use super::config::AuthConfig;

/// 認可エンドポイント。
const AUTHORIZE_ENDPOINT: &str = "https://discord.com/oauth2/authorize";
/// トークンエンドポイント。
const TOKEN_ENDPOINT: &str = "https://discord.com/api/oauth2/token";
/// 現在のユーザーを取る。`identify` で足りる。
const CURRENT_USER_ENDPOINT: &str = "https://discord.com/api/users/@me";
/// 要求するスコープ。**増やさない。**
pub const SCOPE: &str = "identify";
/// Discord API 呼び出しのタイムアウト。
const HTTP_TIMEOUT: Duration = Duration::from_secs(10);

/// OAuth の失敗。**秘密も Discord の応答本文も載せない。**
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum OAuthError {
    /// 通信に失敗した。
    #[error("discord oauth: request failed")]
    Transport,
    /// Discord が失敗を返した（ステータスのみ。本文は載せない）。
    #[error("discord oauth: rejected with status {0}")]
    Rejected(u16),
    /// 応答の形が違う。
    #[error("discord oauth: malformed response")]
    Malformed,
}

/// `identify` で取れるユーザー。
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct DiscordUser {
    /// snowflake。**文字列のまま持つ。**
    pub id: String,
    /// ユーザー名。
    #[serde(default)]
    pub username: String,
    /// 表示名（未設定なら `None`）。
    #[serde(default)]
    pub global_name: Option<String>,
}

impl DiscordUser {
    /// 画面に出す名前。表示専用で、保存はしない。
    pub fn display_name(&self) -> &str {
        self.global_name
            .as_deref()
            .filter(|name| !name.is_empty())
            .unwrap_or(&self.username)
    }
}

/// Discord とのやりとり。テストでは偽物に差し替える。
#[async_trait]
pub trait OAuthClient: Send + Sync {
    /// 認可コードをアクセストークンに交換する。
    async fn exchange_code(&self, code: &str) -> Result<String, OAuthError>;

    /// アクセストークンで `identify` のユーザーを取る。
    async fn current_user(&self, access_token: &str) -> Result<DiscordUser, OAuthError>;
}

/// 認可 URL を組み立てる。純粋関数。
///
/// リダイレクト URI は設定から読む（ハードコードしない）。`state` は CSRF 対策で、
/// 呼び出し側が短命な署名付き Cookie に入れてコールバックで突き合わせる。
pub fn authorize_url(config: &AuthConfig, state: &str) -> String {
    let mut url = String::from(AUTHORIZE_ENDPOINT);
    url.push('?');
    for (index, (key, value)) in [
        ("client_id", config.client_id.as_str()),
        ("redirect_uri", config.redirect_uri.as_str()),
        ("response_type", "code"),
        ("scope", SCOPE),
        ("state", state),
    ]
    .into_iter()
    .enumerate()
    {
        if index > 0 {
            url.push('&');
        }
        url.push_str(key);
        url.push('=');
        url.push_str(&percent_encode(value));
    }
    url
}

/// 使い捨ての `state` を作る。v4 UUID（122 ビット）。使い回さない。
pub fn new_state() -> String {
    uuid::Uuid::new_v4().simple().to_string()
}

/// URL のクエリに入れるためのパーセントエンコード（unreserved 以外を全部エスケープ）。
fn percent_encode(value: &str) -> String {
    let mut encoded = String::with_capacity(value.len());
    for byte in value.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                encoded.push(byte as char);
            }
            _ => {
                let _ = write!(encoded, "%{byte:02X}");
            }
        }
    }
    encoded
}

/// `reqwest` で実装した [`OAuthClient`]。
#[derive(Debug, Clone)]
pub struct HttpOAuthClient {
    http: reqwest::Client,
    client_id: String,
    client_secret: super::config::Secret,
    redirect_uri: String,
}

#[derive(Deserialize)]
struct TokenResponse {
    access_token: String,
}

impl HttpOAuthClient {
    /// 設定から作る。
    pub fn new(config: &AuthConfig) -> Result<Self, OAuthError> {
        let http = reqwest::Client::builder()
            .timeout(HTTP_TIMEOUT)
            .build()
            .map_err(|_| OAuthError::Transport)?;
        Ok(Self {
            http,
            client_id: config.client_id.clone(),
            client_secret: config.client_secret.clone(),
            redirect_uri: config.redirect_uri.clone(),
        })
    }
}

#[async_trait]
impl OAuthClient for HttpOAuthClient {
    async fn exchange_code(&self, code: &str) -> Result<String, OAuthError> {
        // client_secret はボディにだけ入れる。URL にもログにも出さない
        let form = [
            ("client_id", self.client_id.as_str()),
            ("client_secret", self.client_secret.expose()),
            ("grant_type", "authorization_code"),
            ("code", code),
            ("redirect_uri", self.redirect_uri.as_str()),
        ];
        let response = self
            .http
            .post(TOKEN_ENDPOINT)
            .form(&form)
            .send()
            .await
            .map_err(|_| OAuthError::Transport)?;
        if !response.status().is_success() {
            // 応答本文は載せない（リクエストの反射で秘密が混ざる可能性を避ける）
            return Err(OAuthError::Rejected(response.status().as_u16()));
        }
        let token: TokenResponse = response.json().await.map_err(|_| OAuthError::Malformed)?;
        if token.access_token.is_empty() {
            return Err(OAuthError::Malformed);
        }
        Ok(token.access_token)
    }

    async fn current_user(&self, access_token: &str) -> Result<DiscordUser, OAuthError> {
        let response = self
            .http
            .get(CURRENT_USER_ENDPOINT)
            .bearer_auth(access_token)
            .send()
            .await
            .map_err(|_| OAuthError::Transport)?;
        if !response.status().is_success() {
            return Err(OAuthError::Rejected(response.status().as_u16()));
        }
        let user: DiscordUser = response.json().await.map_err(|_| OAuthError::Malformed)?;
        if user.id.is_empty() {
            return Err(OAuthError::Malformed);
        }
        Ok(user)
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    fn config() -> AuthConfig {
        AuthConfig::new(
            "123456789",
            "top-secret-value",
            "https://match.example.test/auth/callback",
            "match.example.test",
            "k".repeat(64),
            Duration::from_secs(3600),
        )
        .expect("valid test config")
    }

    #[test]
    fn the_authorize_url_requests_identify_only() {
        let url = authorize_url(&config(), "state-token");
        assert!(
            url.starts_with("https://discord.com/oauth2/authorize?"),
            "{url}"
        );
        assert!(url.contains("client_id=123456789"), "{url}");
        assert!(url.contains("response_type=code"), "{url}");
        assert!(url.contains("scope=identify"), "{url}");
        assert!(url.contains("state=state-token"), "{url}");
        // リダイレクト URI は設定から。エンコードして載せる
        assert!(
            url.contains("redirect_uri=https%3A%2F%2Fmatch.example.test%2Fauth%2Fcallback"),
            "{url}"
        );
        // email も guilds も要求しない
        assert!(!url.contains("email"), "{url}");
        assert!(!url.contains("guilds"), "{url}");
        // client_secret を URL に載せない
        assert!(!url.contains("top-secret-value"), "{url}");
        assert!(!url.contains("secret"), "{url}");
    }

    #[test]
    fn percent_encoding_escapes_everything_outside_the_unreserved_set() {
        assert_eq!(percent_encode("abcXYZ019-_.~"), "abcXYZ019-_.~");
        assert_eq!(percent_encode("a b&c=d?e"), "a%20b%26c%3Dd%3Fe");
        assert_eq!(percent_encode("https://x/y"), "https%3A%2F%2Fx%2Fy");
        // 非 ASCII は UTF-8 のバイト列として
        assert_eq!(percent_encode("あ"), "%E3%81%82");
    }

    #[test]
    fn state_tokens_are_unique_per_call() {
        let first = new_state();
        let second = new_state();
        assert_ne!(first, second);
        assert_eq!(first.len(), 32);
        assert!(first.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn oauth_errors_never_carry_a_body_or_a_secret() {
        for error in [
            OAuthError::Transport,
            OAuthError::Rejected(401),
            OAuthError::Malformed,
        ] {
            let rendered = error.to_string();
            assert!(!rendered.contains("secret"), "{rendered}");
            assert!(rendered.starts_with("discord oauth: "), "{rendered}");
        }
    }

    #[test]
    fn the_display_name_falls_back_to_the_username() {
        let with_global = DiscordUser {
            id: "1".into(),
            username: "handle".into(),
            global_name: Some("表示名".into()),
        };
        assert_eq!(with_global.display_name(), "表示名");
        let without = DiscordUser {
            id: "1".into(),
            username: "handle".into(),
            global_name: None,
        };
        assert_eq!(without.display_name(), "handle");
        let empty = DiscordUser {
            id: "1".into(),
            username: "handle".into(),
            global_name: Some(String::new()),
        };
        assert_eq!(empty.display_name(), "handle");
    }
}
