//! 登録・認証トラックの設定。すべて環境変数から読む。
//!
//! ここに URL・ドメイン・鍵をハードコードしない（03-技術仕様 §8「リソース名に固有名を入れない。
//! サイト名・ロゴ・配色は設定値として外出しする」）。
//!
//! `client_secret` と署名鍵は [`Secret`] に包む。`Debug` は伏字を出し、`Display` は実装しない
//! （`{}` で書式化できないので、ログ・エラー・レスポンスに紛れ込まない）。

use std::env;
use std::fmt;
use std::time::Duration;

use axum_extra::extract::cookie::Key;

/// 環境変数名。`.env.example` と一致させる。
pub mod keys {
    /// Discord アプリケーションの client id（公開値）。
    pub const CLIENT_ID: &str = "DISCORD_CLIENT_ID";
    /// Discord アプリケーションの client secret。ログにもレスポンスにも出さない。
    pub const CLIENT_SECRET: &str = "DISCORD_CLIENT_SECRET";
    /// OAuth のリダイレクト URI。Developer Portal の登録と**完全一致**させる。
    pub const REDIRECT_URI: &str = "OAUTH_REDIRECT_URI";
    /// セッション Cookie の Domain。親ドメイン（先頭ドット）は拒否する。
    pub const COOKIE_DOMAIN: &str = "SESSION_COOKIE_DOMAIN";
    /// セッション Cookie の署名鍵。64 バイト以上。
    pub const SESSION_SECRET: &str = "SESSION_SECRET";
    /// セッションの有効期限（時間）。過ぎたら再ログインに落とす。
    pub const SESSION_TTL_HOURS: &str = "SESSION_TTL_HOURS";
}

mod defaults {
    /// 14 日。9/12 の開催をまたいでも再ログインを求めない長さ。
    pub const SESSION_TTL_HOURS: u64 = 24 * 14;
    /// state Cookie の寿命。Discord の同意画面を挟むので数分あれば足りる。
    pub const STATE_TTL_SECONDS: u64 = 10 * 60;
}

/// 署名鍵の最小長。`axum_extra` の `Key::from` はこれ未満で panic する。
const MIN_SECRET_BYTES: usize = 64;

/// ログに出してはいけない値。`Debug` は伏字、`Display` は無い。
#[derive(Clone)]
pub struct Secret(String);

impl Secret {
    /// 実際の値。呼び出しは Discord への送信と署名鍵の生成に限る。
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Secret(<redacted>)")
    }
}

/// 設定の読み込みエラー。値そのものはメッセージに含めない。
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum AuthConfigError {
    /// 必須の環境変数が無い。
    #[error("missing environment variable {0}")]
    Missing(&'static str),
    /// 値の形が不正。**値自体は載せない**（秘密が混ざりうるため）。
    #[error("invalid {name}: {reason}")]
    Invalid {
        /// 環境変数名。
        name: &'static str,
        /// 理由。
        reason: String,
    },
}

/// 登録・認証トラックの設定。
#[derive(Debug, Clone)]
pub struct AuthConfig {
    /// Discord の client id。
    pub client_id: String,
    /// Discord の client secret。
    pub client_secret: Secret,
    /// リダイレクト URI（Developer Portal の登録と完全一致）。
    pub redirect_uri: String,
    /// セッション Cookie の Domain。
    pub cookie_domain: String,
    /// Cookie の署名鍵の材料。
    session_secret: Secret,
    /// セッションの有効期限。
    pub session_ttl: Duration,
    /// state Cookie の寿命。
    pub state_ttl: Duration,
}

impl AuthConfig {
    /// 値を直接与えて組み立てる。検証は [`AuthConfig::from_env`] と同じ。
    pub fn new(
        client_id: impl Into<String>,
        client_secret: impl Into<String>,
        redirect_uri: impl Into<String>,
        cookie_domain: impl Into<String>,
        session_secret: impl Into<String>,
        session_ttl: Duration,
    ) -> Result<Self, AuthConfigError> {
        let redirect_uri = redirect_uri.into();
        validate_redirect_uri(&redirect_uri)?;
        let cookie_domain = cookie_domain.into();
        validate_cookie_domain(&cookie_domain)?;
        let session_secret = session_secret.into();
        if session_secret.len() < MIN_SECRET_BYTES {
            return Err(AuthConfigError::Invalid {
                name: keys::SESSION_SECRET,
                reason: format!("must be at least {MIN_SECRET_BYTES} bytes"),
            });
        }
        Ok(Self {
            client_id: client_id.into(),
            client_secret: Secret(client_secret.into()),
            redirect_uri,
            cookie_domain,
            session_secret: Secret(session_secret),
            session_ttl,
            state_ttl: Duration::from_secs(defaults::STATE_TTL_SECONDS),
        })
    }

    /// 環境変数から読む。
    pub fn from_env() -> Result<Self, AuthConfigError> {
        let redirect_uri = required(keys::REDIRECT_URI)?;
        validate_redirect_uri(&redirect_uri)?;
        let cookie_domain = required(keys::COOKIE_DOMAIN)?;
        validate_cookie_domain(&cookie_domain)?;
        let session_secret = required(keys::SESSION_SECRET)?;
        if session_secret.len() < MIN_SECRET_BYTES {
            return Err(AuthConfigError::Invalid {
                name: keys::SESSION_SECRET,
                reason: format!("must be at least {MIN_SECRET_BYTES} bytes"),
            });
        }
        let ttl_hours = match env::var(keys::SESSION_TTL_HOURS) {
            Ok(value) if !value.trim().is_empty() => value
                .trim()
                .parse::<u64>()
                .ok()
                .filter(|hours| *hours > 0)
                .ok_or(AuthConfigError::Invalid {
                    name: keys::SESSION_TTL_HOURS,
                    reason: "must be a positive integer".into(),
                })?,
            _ => defaults::SESSION_TTL_HOURS,
        };
        Ok(Self {
            client_id: required(keys::CLIENT_ID)?,
            client_secret: Secret(required(keys::CLIENT_SECRET)?),
            redirect_uri,
            cookie_domain,
            session_secret: Secret(session_secret),
            session_ttl: Duration::from_secs(ttl_hours * 3600),
            state_ttl: Duration::from_secs(defaults::STATE_TTL_SECONDS),
        })
    }

    /// Cookie の署名鍵。
    pub fn cookie_key(&self) -> Key {
        Key::from(self.session_secret.expose().as_bytes())
    }
}

fn required(name: &'static str) -> Result<String, AuthConfigError> {
    match env::var(name) {
        Ok(value) if !value.trim().is_empty() => Ok(value.trim().to_owned()),
        _ => Err(AuthConfigError::Missing(name)),
    }
}

/// Cookie の Domain を検証する。
///
/// **先頭ドット（`.aynm.dev`）を拒否する。**親ドメインを指定すると、同じ親を共有する
/// 個人サイトにもセッション Cookie が送られる（05-登録トラック仕様 §1）。
/// 値そのものは環境変数から読む（本番は `match.aynm.dev`）。
fn validate_cookie_domain(domain: &str) -> Result<(), AuthConfigError> {
    let invalid = |reason: &str| AuthConfigError::Invalid {
        name: keys::COOKIE_DOMAIN,
        reason: reason.to_owned(),
    };
    if domain.starts_with('.') {
        return Err(invalid(
            "must be an exact host; a leading dot shares the cookie with every sibling subdomain",
        ));
    }
    if domain.contains('/') || domain.contains(':') || domain.contains(' ') {
        return Err(invalid(
            "must be a bare host name, without scheme, port or path",
        ));
    }
    Ok(())
}

/// リダイレクト URI を検証する。https を要求する（loopback の開発用だけ http を許す）。
fn validate_redirect_uri(uri: &str) -> Result<(), AuthConfigError> {
    let invalid = |reason: &str| AuthConfigError::Invalid {
        name: keys::REDIRECT_URI,
        reason: reason.to_owned(),
    };
    if let Some(rest) = uri.strip_prefix("https://") {
        if rest.is_empty() {
            return Err(invalid("must have a host"));
        }
        return Ok(());
    }
    if let Some(rest) = uri.strip_prefix("http://")
        && (rest.starts_with("localhost") || rest.starts_with("127.0.0.1"))
    {
        // 開発用。Discord は loopback に限り http のリダイレクトを許可する
        return Ok(());
    }
    Err(invalid("must be https (http is allowed only for loopback)"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(domain: &str, redirect: &str) -> AuthConfig {
        AuthConfig::new(
            "1",
            "shhh",
            redirect,
            domain,
            "x".repeat(MIN_SECRET_BYTES),
            Duration::from_secs(3600),
        )
        .expect("valid test config")
    }

    #[test]
    fn secrets_are_redacted_in_debug_output() {
        let config = config(
            "match.example.test",
            "https://match.example.test/auth/callback",
        );
        let rendered = format!("{config:?}");
        assert!(!rendered.contains("shhh"), "{rendered}");
        assert!(rendered.contains("<redacted>"), "{rendered}");
        assert_eq!(config.client_secret.expose(), "shhh");
    }

    #[test]
    fn cookie_domain_must_be_an_exact_host() {
        assert!(validate_cookie_domain("match.aynm.dev").is_ok());
        // 親ドメインは個人サイトと Cookie を共有してしまう
        assert!(validate_cookie_domain(".aynm.dev").is_err());
        assert!(validate_cookie_domain("https://match.aynm.dev").is_err());
        assert!(validate_cookie_domain("match.aynm.dev:8080").is_err());
        assert!(validate_cookie_domain("match.aynm.dev/x").is_err());
    }

    #[test]
    fn redirect_uri_requires_https_except_on_loopback() {
        assert!(validate_redirect_uri("https://match.aynm.dev/auth/callback").is_ok());
        assert!(validate_redirect_uri("http://localhost:8080/auth/callback").is_ok());
        assert!(validate_redirect_uri("http://127.0.0.1:8080/auth/callback").is_ok());
        assert!(validate_redirect_uri("http://match.aynm.dev/auth/callback").is_err());
        assert!(validate_redirect_uri("/auth/callback").is_err());
        assert!(validate_redirect_uri("https://").is_err());
    }

    #[test]
    fn the_signing_key_comes_from_the_environment() {
        let config = config(
            "match.example.test",
            "https://match.example.test/auth/callback",
        );
        // 同じ材料からは同じ鍵（プロセスを再起動してもセッションが切れない）
        assert_eq!(config.cookie_key().signing(), config.cookie_key().signing());
    }
}
