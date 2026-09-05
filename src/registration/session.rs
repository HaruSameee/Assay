//! セッションと state の Cookie。セッションストアは立てない（05-登録トラック仕様 §1）。
//!
//! Cookie に入れるのは **player の識別子と発行時刻だけ**。権限・ロール・トレーナー名は入れない。
//! 権限は毎回 DB から引く（Cookie は「誰か」だけを言う）。
//!
//! 属性は `HttpOnly` / `Secure` / `SameSite=Lax`。**`Strict` にしない**: OAuth のコールバックは
//! discord.com からのトップレベル遷移なので、`Strict` だと state Cookie が送られず、
//! ローカルの http では通って本番の https で落ちる。
//!
//! 署名は `axum-extra` の [`SignedCookieJar`]（鍵は環境変数、[`super::AuthConfig::cookie_key`]）。
//! 有効期限を過ぎたセッションは復号できても拒否し、再ログインに落とす。

use axum::extract::{FromRef, FromRequestParts, OptionalFromRequestParts};
use axum::http::request::Parts;
use axum::response::{IntoResponse, Redirect, Response};
use axum_extra::extract::SignedCookieJar;
use axum_extra::extract::cookie::{Cookie, Key, SameSite};
use chrono::{DateTime, TimeDelta, Utc};
use uuid::Uuid;

use crate::lifecycle::{PlayerId, Timestamp};

use super::config::AuthConfig;

/// セッション Cookie の名前。
pub const SESSION_COOKIE: &str = "assay_session";
/// OAuth の state Cookie の名前。コールバックで検証したら即座に破棄する。
pub const STATE_COOKIE: &str = "assay_oauth_state";

/// 発行時刻が未来にずれていても許す幅（複数プロセス間のクロックスキュー）。
const FUTURE_SKEW: TimeDelta = TimeDelta::minutes(5);

/// ログイン済みのセッション。**識別子と発行時刻だけ。**
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Session {
    /// `players.id`。
    pub player_id: PlayerId,
    /// 発行時刻。有効期限の判定に使う。
    pub issued_at: Timestamp,
}

/// セッションが使えない理由。呼び出し側は再ログインに落とす。
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum SessionError {
    /// Cookie が無い。
    #[error("no session cookie")]
    Missing,
    /// 署名は通ったが中身の形が違う（鍵の入れ替え後の古い Cookie など）。
    #[error("malformed session cookie")]
    Malformed,
    /// 有効期限切れ。
    #[error("session expired")]
    Expired,
}

impl Session {
    /// 今この瞬間に発行する。
    pub fn issue(player_id: PlayerId) -> Self {
        Self {
            player_id,
            issued_at: Utc::now(),
        }
    }

    /// Cookie に入れる文字列。`<player uuid>.<unix 秒>`。
    pub fn encode(&self) -> String {
        format!(
            "{}.{}",
            self.player_id.0.simple(),
            self.issued_at.timestamp()
        )
    }

    /// Cookie の文字列を読む。形が違えば [`SessionError::Malformed`]。
    pub fn decode(value: &str) -> Result<Self, SessionError> {
        let (player, issued) = value.split_once('.').ok_or(SessionError::Malformed)?;
        let player_id = Uuid::try_parse(player).map_err(|_| SessionError::Malformed)?;
        let issued_at = issued
            .parse::<i64>()
            .ok()
            .and_then(|seconds| DateTime::from_timestamp(seconds, 0))
            .ok_or(SessionError::Malformed)?;
        Ok(Self {
            player_id: PlayerId(player_id),
            issued_at,
        })
    }

    /// `now` 時点で有効か。期限切れと、未来に発行されたものを拒否する。
    pub fn validate(&self, now: Timestamp, ttl: std::time::Duration) -> Result<(), SessionError> {
        if self.issued_at > now + FUTURE_SKEW {
            return Err(SessionError::Malformed);
        }
        let ttl = TimeDelta::from_std(ttl).map_err(|_| SessionError::Expired)?;
        if now - self.issued_at > ttl {
            return Err(SessionError::Expired);
        }
        Ok(())
    }
}

/// 期限切れのセッションは再ログインに落とす（入口へ戻す）。
impl IntoResponse for SessionError {
    fn into_response(self) -> Response {
        Redirect::to("/").into_response()
    }
}

/// Cookie の `Max-Age`。`cookie` クレートの時間型に合わせる。
fn max_age(ttl: std::time::Duration) -> Option<time::Duration> {
    i64::try_from(ttl.as_secs())
        .ok()
        .map(time::Duration::seconds)
}

/// Cookie の共通属性。`Secure` / `HttpOnly` / `SameSite=Lax` / Domain 限定。
fn base_cookie(name: &'static str, value: String, config: &AuthConfig) -> Cookie<'static> {
    Cookie::build((name, value))
        .http_only(true)
        .secure(true)
        // Strict にしない。OAuth コールバックは discord.com からの遷移なので state が落ちる
        .same_site(SameSite::Lax)
        // 親ドメイン（.aynm.dev）にしない。設定値の検証は AuthConfig 側で行う
        .domain(config.cookie_domain.clone())
        .path("/")
        .build()
}

/// セッション Cookie を載せる。
pub fn set_session(
    jar: SignedCookieJar,
    session: &Session,
    config: &AuthConfig,
) -> SignedCookieJar {
    let mut cookie = base_cookie(SESSION_COOKIE, session.encode(), config);
    cookie.set_max_age(max_age(config.session_ttl));
    jar.add(cookie)
}

/// セッション Cookie を消す（ログアウト）。属性を揃えないとブラウザが消してくれない。
pub fn clear_session(jar: SignedCookieJar, config: &AuthConfig) -> SignedCookieJar {
    jar.remove(base_cookie(SESSION_COOKIE, String::new(), config))
}

/// state Cookie を載せる。短命。
pub fn set_state(jar: SignedCookieJar, state: &str, config: &AuthConfig) -> SignedCookieJar {
    let mut cookie = base_cookie(STATE_COOKIE, state.to_owned(), config);
    cookie.set_max_age(max_age(config.state_ttl));
    jar.add(cookie)
}

/// state Cookie を消す。**検証の成否にかかわらず、コールバックで必ず呼ぶ**（使い回さない）。
pub fn clear_state(jar: SignedCookieJar, config: &AuthConfig) -> SignedCookieJar {
    jar.remove(base_cookie(STATE_COOKIE, String::new(), config))
}

/// 署名付き Cookie からセッションを取り出す。期限切れは拒否する。
pub fn read_session(
    jar: &SignedCookieJar,
    config: &AuthConfig,
    now: Timestamp,
) -> Result<Session, SessionError> {
    let cookie = jar.get(SESSION_COOKIE).ok_or(SessionError::Missing)?;
    let session = Session::decode(cookie.value())?;
    session.validate(now, config.session_ttl)?;
    Ok(session)
}

/// ログイン必須のハンドラで使う抽出子。無い / 壊れている / 期限切れは `Err`。
///
/// `Option<CurrentSession>` として受け取ると、未ログインを `None` で扱える。
#[derive(Debug, Clone, Copy)]
pub struct CurrentSession(pub Session);

impl<S> FromRequestParts<S> for CurrentSession
where
    S: Send + Sync,
    Key: FromRef<S>,
    AuthConfig: FromRef<S>,
{
    type Rejection = SessionError;

    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self, Self::Rejection> {
        let jar = SignedCookieJar::from_request_parts(parts, state)
            .await
            .map_err(|_| SessionError::Missing)?;
        let config = AuthConfig::from_ref(state);
        read_session(&jar, &config, Utc::now()).map(CurrentSession)
    }
}

impl<S> OptionalFromRequestParts<S> for CurrentSession
where
    S: Send + Sync,
    Key: FromRef<S>,
    AuthConfig: FromRef<S>,
{
    type Rejection = std::convert::Infallible;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &S,
    ) -> Result<Option<Self>, Self::Rejection> {
        Ok(
            <Self as FromRequestParts<S>>::from_request_parts(parts, state)
                .await
                .ok(),
        )
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;
    use crate::registration::config::AuthConfig;

    fn player(n: u128) -> PlayerId {
        PlayerId(Uuid::from_u128(n))
    }

    fn at(seconds: i64) -> Timestamp {
        DateTime::from_timestamp(1_800_000_000 + seconds, 0).unwrap()
    }

    fn config() -> AuthConfig {
        AuthConfig::new(
            "client-id",
            "client-secret",
            "https://match.example.test/auth/callback",
            "match.example.test",
            "k".repeat(64),
            Duration::from_secs(3600),
        )
        .expect("valid test config")
    }

    #[test]
    fn a_session_round_trips_through_the_cookie_value() {
        let session = Session {
            player_id: player(0xabc),
            issued_at: at(0),
        };
        let encoded = session.encode();
        assert_eq!(Session::decode(&encoded), Ok(session));
        // 識別子と発行時刻しか入っていない（ドット区切りの 2 要素）
        assert_eq!(encoded.split('.').count(), 2);
    }

    #[test]
    fn malformed_cookie_values_are_rejected() {
        for value in [
            "",
            "nodot",
            "not-a-uuid.100",
            "00000000000000000000000000000001.later",
        ] {
            assert_eq!(
                Session::decode(value),
                Err(SessionError::Malformed),
                "{value}"
            );
        }
    }

    #[test]
    fn an_expired_session_is_rejected_but_a_fresh_one_passes() {
        let ttl = Duration::from_secs(3600);
        let session = Session {
            player_id: player(1),
            issued_at: at(0),
        };
        assert_eq!(session.validate(at(0), ttl), Ok(()));
        assert_eq!(session.validate(at(3600), ttl), Ok(()));
        assert_eq!(session.validate(at(3601), ttl), Err(SessionError::Expired));
        // 未来に発行された Cookie は許容幅を超えたら拒否する
        assert_eq!(session.validate(at(-299), ttl), Ok(()));
        assert_eq!(
            session.validate(at(-301), ttl),
            Err(SessionError::Malformed)
        );
    }

    #[test]
    fn cookies_carry_the_required_attributes_and_an_exact_domain() {
        let config = config();
        let key = config.cookie_key();
        let session = Session {
            player_id: player(1),
            issued_at: at(0),
        };
        let jar = set_state(
            set_session(SignedCookieJar::new(key.clone()), &session, &config),
            "state-token",
            &config,
        );
        for name in [SESSION_COOKIE, STATE_COOKIE] {
            let cookie = jar.get(name).expect(name);
            assert!(cookie.http_only().unwrap_or(false), "{name}: HttpOnly");
            assert!(cookie.secure().unwrap_or(false), "{name}: Secure");
            assert_eq!(cookie.same_site(), Some(SameSite::Lax), "{name}: SameSite");
            assert_eq!(cookie.domain(), Some("match.example.test"), "{name}");
            assert_eq!(cookie.path(), Some("/"), "{name}");
            assert!(cookie.max_age().is_some(), "{name}: Max-Age");
        }
        // state は署名されているだけで、セッションとは別の Cookie
        assert_eq!(jar.get(STATE_COOKIE).unwrap().value(), "state-token");
        assert_eq!(
            read_session(&jar, &config, at(0)),
            Ok(session),
            "signed session reads back"
        );
    }

    #[test]
    fn reading_a_session_reports_why_it_is_unusable() {
        let config = config();
        let key = config.cookie_key();
        let empty = SignedCookieJar::new(key.clone());
        assert_eq!(
            read_session(&empty, &config, at(0)),
            Err(SessionError::Missing)
        );

        let stale = Session {
            player_id: player(1),
            issued_at: at(0),
        };
        let jar = set_session(SignedCookieJar::new(key), &stale, &config);
        let past_ttl = at(config.session_ttl.as_secs() as i64 + 1);
        assert_eq!(
            read_session(&jar, &config, past_ttl),
            Err(SessionError::Expired)
        );
    }

    #[test]
    fn clearing_removes_both_cookies() {
        let config = config();
        let jar = SignedCookieJar::new(config.cookie_key());
        let session = Session {
            player_id: player(1),
            issued_at: at(0),
        };
        let jar = set_state(set_session(jar, &session, &config), "s", &config);
        let jar = clear_state(clear_session(jar, &config), &config);
        assert!(jar.get(SESSION_COOKIE).is_none());
        assert!(jar.get(STATE_COOKIE).is_none());
    }
}
