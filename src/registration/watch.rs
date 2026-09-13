//! 特定のプレイヤーの接続元（IP・User-Agent）の記録。**全員は記録しない。**
//!
//! 迷惑行為のサブアカウントを特定するために、運営が環境変数
//! [`IP_WATCH_DISCORD_USER_IDS`] に並べた Discord ユーザー ID の人だけを記録する。
//! 対象でなければ何も積まない（表に 1 行も入らない）。誰を対象にするかは環境変数だけで
//! 決まり、DB には持たない: 変えたいときは環境変数を書き換えて再起動する。
//!
//! 記録する契機は 2 つ: ログイン（OAuth コールバック）とキュー投入。セッションは 14 日
//! 持つのでログインだけでは疎になるため、開催日ごとに必ず通るキュー投入でも取る。
//!
//! **記録に失敗してもログインもキュー投入も通す。**記録はおまけで、本体の動作を止める
//! 理由にはならない（失敗は warn で残す）。
//!
//! サイトは Cloudflare 経由なので、本当の接続元は `CF-Connecting-IP` にあり、
//! `X-Forwarded-For` は「本人, Cloudflare」の並びになる。どちらも**生のまま**残す
//! （片方に潰すと後から検証できない）。

use std::collections::HashSet;
use std::env;
use std::sync::Arc;

use axum::http::HeaderMap;
use axum::http::header::USER_AGENT;

/// 記録対象の Discord ユーザー ID（カンマ / 空白区切り）。未設定なら誰も記録しない。
pub const IP_WATCH_DISCORD_USER_IDS: &str = "IP_WATCH_DISCORD_USER_IDS";

/// 記録対象の集合。**空なら誰も記録しない。**
#[derive(Debug, Clone, Default)]
pub struct AddressWatch {
    ids: Arc<HashSet<i64>>,
}

impl AddressWatch {
    /// 環境変数から読む。未設定・空は「誰も記録しない」。数値でないトークンは
    /// **起動時に落とす**（黙って無視すると、記録しているつもりで取れていない）。
    pub fn from_env() -> Result<Self, AddressWatchError> {
        match env::var(IP_WATCH_DISCORD_USER_IDS) {
            Err(env::VarError::NotPresent) => Ok(Self::default()),
            Ok(value) => Self::parse(&value),
            Err(env::VarError::NotUnicode(_)) => Err(AddressWatchError::InvalidToken {
                token: "(not unicode)".to_owned(),
            }),
        }
    }

    /// 文字列から作る。**純粋関数。**
    pub fn parse(raw: &str) -> Result<Self, AddressWatchError> {
        let mut ids = HashSet::new();
        for token in raw.split(|c: char| c == ',' || c.is_whitespace()) {
            if token.is_empty() {
                continue;
            }
            let id = token
                .parse::<i64>()
                .map_err(|_| AddressWatchError::InvalidToken {
                    token: token.to_owned(),
                })?;
            ids.insert(id);
        }
        Ok(Self { ids: Arc::new(ids) })
    }

    /// 明示的に作る（テスト用）。
    pub fn of(ids: impl IntoIterator<Item = i64>) -> Self {
        Self {
            ids: Arc::new(ids.into_iter().collect()),
        }
    }

    /// この Discord ユーザーを記録するか。
    pub fn is_watched(&self, discord_user_id: i64) -> bool {
        self.ids.contains(&discord_user_id)
    }

    /// 文字列の snowflake でも引ける（OAuth のユーザーは文字列で持っている）。
    pub fn is_watched_str(&self, discord_user_id: &str) -> bool {
        discord_user_id
            .trim()
            .parse::<i64>()
            .is_ok_and(|id| self.is_watched(id))
    }

    /// 対象の人数（起動ログ用）。
    pub fn len(&self) -> usize {
        self.ids.len()
    }

    /// 誰も記録しないか。
    pub fn is_empty(&self) -> bool {
        self.ids.is_empty()
    }
}

/// 環境変数の不備。
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum AddressWatchError {
    #[error("{IP_WATCH_DISCORD_USER_IDS} contains a token that is not a discord user id: {token}")]
    InvalidToken { token: String },
}

/// 1 回の接続元。ヘッダを**生のまま**持つ。
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ClientAddress {
    /// `CF-Connecting-IP`。Cloudflare を経由していなければ `None`。
    pub cf_connecting_ip: Option<String>,
    /// `X-Forwarded-For` をそのまま。
    pub x_forwarded_for: Option<String>,
    /// `User-Agent`。
    pub user_agent: Option<String>,
}

impl ClientAddress {
    /// リクエストヘッダから読む。**純粋関数。**値は 512 文字で切る（ヘッダは利用者が
    /// 自由に書けるので、巨大な値をそのまま積まない）。
    pub fn from_headers(headers: &HeaderMap) -> Self {
        let read = |name: &str| {
            headers
                .get(name)
                .and_then(|value| value.to_str().ok())
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .map(|value| value.chars().take(512).collect::<String>())
        };
        Self {
            cf_connecting_ip: read("cf-connecting-ip"),
            x_forwarded_for: read("x-forwarded-for"),
            user_agent: read(USER_AGENT.as_str()),
        }
    }

    /// 画面に出す「本当の接続元」。Cloudflare の値を優先し、無ければ
    /// `X-Forwarded-For` の先頭、それも無ければ「不明」。
    pub fn best_guess(&self) -> String {
        if let Some(ip) = &self.cf_connecting_ip {
            return ip.clone();
        }
        self.x_forwarded_for
            .as_deref()
            .and_then(|list| list.split(',').next())
            .map(str::trim)
            .filter(|first| !first.is_empty())
            .map_or_else(|| "不明".to_owned(), str::to_owned)
    }
}

/// 記録対象の人のアクセス 1 回分。運営への通知に使う。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WatchedAccess {
    /// Discord ユーザー ID（文字列のまま）。
    pub discord_user_id: String,
    /// `players.id`。運営パネルのプレイヤー画面への導線に使う。
    pub player_id: crate::lifecycle::PlayerId,
    /// 契機。
    pub source: AddressSource,
    /// 接続元（生のヘッダ）。
    pub address: ClientAddress,
    /// 時刻。
    pub at: crate::lifecycle::Timestamp,
}

/// 記録対象の人のアクセスを運営に知らせる先（Discord の係争チャンネルなど）。
///
/// **失敗しても呼ぶ側は止まらない**（ログイン・キュー投入は通す）。
#[serenity::async_trait]
pub trait WatchAlert: Send + Sync {
    /// 1 回のアクセスを通知する。
    async fn watched_access(&self, access: &WatchedAccess) -> Result<(), String>;
}

/// 記録の契機。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AddressSource {
    /// OAuth コールバック（ログイン）。
    Login,
    /// キュー投入。
    Queue,
}

impl AddressSource {
    /// DB に入る文字列。
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Login => "login",
            Self::Queue => "queue",
        }
    }

    /// 画面の言葉。
    pub const fn label(self) -> &'static str {
        match self {
            Self::Login => "ログイン",
            Self::Queue => "キュー投入",
        }
    }

    /// 読み取り側。知らない値は `None`。
    pub fn parse(raw: &str) -> Option<Self> {
        match raw {
            "login" => Some(Self::Login),
            "queue" => Some(Self::Queue),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderValue;

    /// **未設定・空は誰も記録しない。**数値でないトークンは落とす。
    #[test]
    fn the_watch_list_is_empty_unless_ids_are_given() {
        assert!(AddressWatch::default().is_empty());
        assert!(AddressWatch::parse("").unwrap().is_empty());
        assert!(AddressWatch::parse(" , ").unwrap().is_empty());
        let watch = AddressWatch::parse("900000000000000004, 42\n7").unwrap();
        assert_eq!(watch.len(), 3);
        assert!(watch.is_watched(900000000000000004));
        assert!(watch.is_watched_str("900000000000000004"));
        assert!(watch.is_watched(7));
        assert!(!watch.is_watched(8));
        assert!(!watch.is_watched_str("abc"));
        assert_eq!(
            AddressWatch::parse("1, x").unwrap_err(),
            AddressWatchError::InvalidToken {
                token: "x".to_owned()
            }
        );
    }

    /// **Cloudflare の値を優先し、無ければ X-Forwarded-For の先頭。**生の値も残す。
    #[test]
    fn the_client_address_prefers_cloudflare_then_the_first_forwarded_hop() {
        let mut headers = HeaderMap::new();
        headers.insert(
            "x-forwarded-for",
            HeaderValue::from_static("203.0.113.9, 172.64.0.1"),
        );
        headers.insert(USER_AGENT, HeaderValue::from_static("Mozilla/5.0"));
        let address = ClientAddress::from_headers(&headers);
        assert_eq!(address.cf_connecting_ip, None);
        assert_eq!(address.best_guess(), "203.0.113.9");
        assert_eq!(
            address.x_forwarded_for.as_deref(),
            Some("203.0.113.9, 172.64.0.1")
        );
        assert_eq!(address.user_agent.as_deref(), Some("Mozilla/5.0"));

        headers.insert("cf-connecting-ip", HeaderValue::from_static("198.51.100.7"));
        let address = ClientAddress::from_headers(&headers);
        assert_eq!(address.best_guess(), "198.51.100.7");

        assert_eq!(ClientAddress::default().best_guess(), "不明");
    }

    /// 巨大なヘッダは 512 文字で切る。
    #[test]
    fn header_values_are_truncated() {
        let mut headers = HeaderMap::new();
        headers.insert(
            USER_AGENT,
            HeaderValue::from_str(&"a".repeat(2000)).unwrap(),
        );
        let address = ClientAddress::from_headers(&headers);
        assert_eq!(address.user_agent.map(|ua| ua.len()), Some(512));
    }

    #[test]
    fn sources_round_trip() {
        for source in [AddressSource::Login, AddressSource::Queue] {
            assert_eq!(AddressSource::parse(source.as_str()), Some(source));
        }
        assert_eq!(AddressSource::parse("nope"), None);
    }
}
