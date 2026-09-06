//! Discord アダプタの設定。環境変数から読む。コードに ID や文言を埋め込まない。
//!
//! VC は「個数」だけを設定し、名前（`vc<N>-red` / `vc<N>-blue`）からチャンネル ID を起動時に
//! 解決する（[`super::vc_pool::resolve`]）。設定は名前で持ち、ログには ID を記録する。

use std::env;
use std::time::Duration;

use serenity::model::id::{ChannelId, GuildId};

/// 環境変数名。`.env.example` と一致させる。
pub mod keys {
    /// Bot トークン。
    pub const TOKEN: &str = "DISCORD_TOKEN";
    /// 対象ギルド。別ギルドからのインタラクションは無視する。
    pub const GUILD_ID: &str = "DISCORD_GUILD_ID";
    /// 係争内容を運営だけに通知するチャンネル。
    pub const DISPUTE_CHANNEL_ID: &str = "DISCORD_DISPUTE_CHANNEL_ID";
    /// VC の組の個数。`vc1-red` … `vc<N>-blue` を起動時に名前で引く。
    pub const VC_POOL_SIZE: &str = "DISCORD_VC_POOL_SIZE";
    /// 今シーズンのマップ（ランクマのローテで変わる唯一の項目）。
    pub const ROOM_MAP: &str = "ROOM_MAP";
    /// 部屋設定のチェックリスト（マップ以外の固定項目）。`;` 区切り。
    pub const ROOM_CHECKLIST: &str = "ROOM_CHECKLIST";
    /// Discord API 呼び出しのリトライ回数（N）。
    pub const RETRIES: &str = "DISCORD_RETRIES";
    /// Discord API 呼び出しのリトライ間隔の基準（ミリ秒）。試行回数に比例して延びる。
    pub const RETRY_BACKOFF_MS: &str = "DISCORD_RETRY_BACKOFF_MS";
}

/// 既定値。環境変数が無いときだけ使う。
mod defaults {
    pub const RETRIES: u32 = 3;
    pub const RETRY_BACKOFF_MS: u64 = 1000;
}

/// 設定の読み込みエラー。
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ConfigError {
    /// 必須の環境変数が無い。
    #[error("missing environment variable {0}")]
    Missing(&'static str),
    /// 値の形が不正。
    #[error("invalid {name}: {reason}")]
    Invalid {
        /// 環境変数名。
        name: &'static str,
        /// 理由。
        reason: String,
    },
}

/// Discord アダプタの設定。
#[derive(Debug, Clone)]
pub struct DiscordConfig {
    /// Bot トークン。
    pub token: String,
    /// 対象ギルド。
    pub guild_id: GuildId,
    /// `disputed` の詳細を投稿する運営専用チャンネル。
    pub dispute_channel_id: ChannelId,
    /// VC の組の個数（`vc1` … `vc<N>`）。
    pub vc_pool_size: usize,
    /// 今シーズンのマップ。チェックリストの先頭に差し込む。CHECK 制約や enum にはしない。
    pub room_map: String,
    /// 部屋設定のチェックリスト（マップ以外の固定項目）。
    pub room_checklist: Vec<String>,
    /// Discord API 呼び出しのリトライ回数（N）。超えたら `abandoned`（`discord_call_failed`）。
    pub retries: u32,
    /// Discord API 呼び出しのリトライ間隔の基準。
    pub retry_backoff: Duration,
}

impl DiscordConfig {
    /// 環境変数から読む。
    pub fn from_env() -> Result<Self, ConfigError> {
        let vc_pool_size: usize = required(keys::VC_POOL_SIZE)?
            .trim()
            .parse()
            .ok()
            .filter(|size| *size > 0)
            .ok_or_else(|| ConfigError::Invalid {
                name: keys::VC_POOL_SIZE,
                reason: "must be a positive integer".into(),
            })?;
        Ok(Self {
            token: required(keys::TOKEN)?,
            guild_id: GuildId::new(parse_id(keys::GUILD_ID, &required(keys::GUILD_ID)?)?),
            dispute_channel_id: ChannelId::new(parse_id(
                keys::DISPUTE_CHANNEL_ID,
                &required(keys::DISPUTE_CHANNEL_ID)?,
            )?),
            vc_pool_size,
            room_map: required(keys::ROOM_MAP)?.trim().to_owned(),
            room_checklist: parse_checklist(&required(keys::ROOM_CHECKLIST)?),
            retries: optional_number(keys::RETRIES, defaults::RETRIES)?,
            retry_backoff: Duration::from_millis(optional_number(
                keys::RETRY_BACKOFF_MS,
                defaults::RETRY_BACKOFF_MS,
            )?),
        })
    }
}

fn required(name: &'static str) -> Result<String, ConfigError> {
    match env::var(name) {
        Ok(value) if !value.trim().is_empty() => Ok(value),
        _ => Err(ConfigError::Missing(name)),
    }
}

fn optional_number<T: std::str::FromStr>(name: &'static str, default: T) -> Result<T, ConfigError> {
    match env::var(name) {
        Ok(value) if !value.trim().is_empty() => {
            value.trim().parse().map_err(|_| ConfigError::Invalid {
                name,
                reason: format!("{value:?} is not a number"),
            })
        }
        _ => Ok(default),
    }
}

fn parse_id(name: &'static str, value: &str) -> Result<u64, ConfigError> {
    match value.trim().parse::<u64>() {
        Ok(id) if id != 0 => Ok(id),
        _ => Err(ConfigError::Invalid {
            name,
            reason: format!("{value:?} is not a Discord snowflake"),
        }),
    }
}

/// `;` 区切りのチェックリストを読む。空の項目は捨てる。
pub fn parse_checklist(value: &str) -> Vec<String> {
    value
        .split(';')
        .map(str::trim)
        .filter(|item| !item.is_empty())
        .map(str::to_owned)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn checklist_drops_empty_items() {
        assert_eq!(
            parse_checklist(" a ;; b;"),
            vec!["a".to_owned(), "b".to_owned()]
        );
        assert!(parse_checklist("").is_empty());
    }

    #[test]
    fn snowflakes_must_be_positive_integers() {
        assert_eq!(parse_id(keys::GUILD_ID, " 123 "), Ok(123));
        assert!(parse_id(keys::GUILD_ID, "0").is_err());
        assert!(parse_id(keys::GUILD_ID, "abc").is_err());
    }
}
