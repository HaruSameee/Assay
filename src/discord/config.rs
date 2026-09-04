//! Discord アダプタの設定。環境変数から読む。コードに ID や文言を埋め込まない。

use std::env;
use std::time::Duration;

use serenity::model::id::{ChannelId, GuildId};

use super::vc_pool::VcPair;

/// 環境変数名。`.env.example` と一致させる。
pub mod keys {
    /// Bot トークン。
    pub const TOKEN: &str = "DISCORD_TOKEN";
    /// 対象ギルド。別ギルドからのインタラクションは無視する。
    pub const GUILD_ID: &str = "DISCORD_GUILD_ID";
    /// プライベートスレッドを作る親テキストチャンネル。
    pub const PARENT_CHANNEL_ID: &str = "DISCORD_MATCH_PARENT_CHANNEL_ID";
    /// VC プール。`<label>:<red_channel_id>/<blue_channel_id>` をカンマ区切り。
    pub const VC_POOL: &str = "DISCORD_VC_POOL";
    /// 部屋設定のチェックリスト。`;` 区切り。
    pub const ROOM_CHECKLIST: &str = "ROOM_CHECKLIST";
    /// スレッド作成のリトライ回数（N）。
    pub const THREAD_CREATE_RETRIES: &str = "DISCORD_THREAD_CREATE_RETRIES";
    /// 複数スレッドを続けて作るときの間隔（ミリ秒）。
    pub const THREAD_CREATE_STAGGER_MS: &str = "DISCORD_THREAD_CREATE_STAGGER_MS";
    /// Discord API 呼び出しのリトライ間隔の基準（ミリ秒）。試行回数に比例して延びる。
    pub const RETRY_BACKOFF_MS: &str = "DISCORD_RETRY_BACKOFF_MS";
}

/// 既定値。環境変数が無いときだけ使う。
mod defaults {
    pub const THREAD_CREATE_RETRIES: u32 = 3;
    pub const THREAD_CREATE_STAGGER_MS: u64 = 500;
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
    /// プライベートスレッドを作る親テキストチャンネル。
    pub parent_channel_id: ChannelId,
    /// 事前に作られた VC の組。作らない・消さない・権限を変えない。読むだけ。
    pub vc_pool: Vec<VcPair>,
    /// 部屋設定のチェックリスト（割り当て表に載せる）。
    pub room_checklist: Vec<String>,
    /// スレッド作成のリトライ回数（N）。これを超えて失敗したら `abandoned`。
    pub thread_create_retries: u32,
    /// 複数スレッドを続けて作るときの最小間隔。
    pub thread_create_stagger: Duration,
    /// Discord API 呼び出しのリトライ間隔の基準。
    pub retry_backoff: Duration,
}

impl DiscordConfig {
    /// 環境変数から読む。
    pub fn from_env() -> Result<Self, ConfigError> {
        Ok(Self {
            token: required(keys::TOKEN)?,
            guild_id: GuildId::new(parse_id(keys::GUILD_ID, &required(keys::GUILD_ID)?)?),
            parent_channel_id: ChannelId::new(parse_id(
                keys::PARENT_CHANNEL_ID,
                &required(keys::PARENT_CHANNEL_ID)?,
            )?),
            vc_pool: parse_vc_pool(&required(keys::VC_POOL)?)?,
            room_checklist: parse_checklist(&required(keys::ROOM_CHECKLIST)?),
            thread_create_retries: optional_number(
                keys::THREAD_CREATE_RETRIES,
                defaults::THREAD_CREATE_RETRIES,
            )?,
            thread_create_stagger: Duration::from_millis(optional_number(
                keys::THREAD_CREATE_STAGGER_MS,
                defaults::THREAD_CREATE_STAGGER_MS,
            )?),
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

/// `<label>:<red_channel_id>/<blue_channel_id>` をカンマ区切りで並べたものを読む。
///
/// 例: `01:111111111111111111/222222222222222222,02:333333333333333333/444444444444444444`
pub fn parse_vc_pool(value: &str) -> Result<Vec<VcPair>, ConfigError> {
    let name = keys::VC_POOL;
    let invalid = |reason: String| ConfigError::Invalid { name, reason };
    let mut pairs = Vec::new();
    for entry in value
        .split(',')
        .map(str::trim)
        .filter(|entry| !entry.is_empty())
    {
        let (label, channels) = entry
            .split_once(':')
            .ok_or_else(|| invalid(format!("{entry:?} must be <label>:<red>/<blue>")))?;
        let (red, blue) = channels
            .split_once('/')
            .ok_or_else(|| invalid(format!("{entry:?} must be <label>:<red>/<blue>")))?;
        let label = label.trim();
        if label.is_empty() {
            return Err(invalid(format!("{entry:?} has an empty label")));
        }
        if pairs.iter().any(|pair: &VcPair| pair.label == label) {
            return Err(invalid(format!("label {label:?} appears twice")));
        }
        let red = ChannelId::new(parse_id(name, red)?);
        let blue = ChannelId::new(parse_id(name, blue)?);
        if red == blue {
            return Err(invalid(format!(
                "{entry:?} uses the same channel for both teams"
            )));
        }
        pairs.push(VcPair {
            label: label.to_owned(),
            red,
            blue,
        });
    }
    if pairs.is_empty() {
        return Err(invalid("no voice channel pairs".into()));
    }
    Ok(pairs)
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
    fn vc_pool_is_parsed_from_label_and_two_channels() {
        let pairs = parse_vc_pool("01:111/222, 02:333/444").unwrap();
        assert_eq!(pairs.len(), 2);
        assert_eq!(pairs[0].label, "01");
        assert_eq!(pairs[0].red, ChannelId::new(111));
        assert_eq!(pairs[0].blue, ChannelId::new(222));
        assert_eq!(pairs[1].label, "02");
    }

    #[test]
    fn vc_pool_rejects_malformed_entries() {
        assert!(parse_vc_pool("").is_err());
        assert!(parse_vc_pool("01:111").is_err());
        assert!(parse_vc_pool("01:111/111").is_err());
        assert!(parse_vc_pool("01:111/222,01:333/444").is_err());
        assert!(parse_vc_pool(":111/222").is_err());
        assert!(parse_vc_pool("01:abc/222").is_err());
    }

    #[test]
    fn checklist_drops_empty_items() {
        assert_eq!(
            parse_checklist(" a ;; b;"),
            vec!["a".to_owned(), "b".to_owned()]
        );
        assert!(parse_checklist("").is_empty());
    }
}
