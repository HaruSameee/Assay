//! 監視対象のアクセスを係争チャンネルに知らせる（[`crate::registration::watch`]）。
//!
//! 通知先は `DISCORD_DISPUTE_CHANNEL_ID`（運営だけが見る係争チャンネル）。公開の
//! 招集チャンネルには出さない。本人にはメンションしない（ID を素の文字列で書く）。
//!
//! **失敗しても呼ぶ側（ログイン・キュー投入）は止まらない。**再送もしない:
//! 記録そのものは `player_address_records` にあり、通知は「いま来た」を知らせるだけ。

use std::sync::Arc;

use serenity::model::id::ChannelId;
use tracing::{info, warn};

use crate::registration::{WatchAlert, WatchedAccess};

use super::gateway::{Gateway, OutgoingMessage};

/// 係争チャンネルへ流す通知。
pub struct DisputeChannelAlert {
    gateway: Arc<dyn Gateway>,
    channel: ChannelId,
    /// 運営パネルのプレイヤー画面への導線の起点（`https://match.aynm.dev`）。
    site: String,
}

impl DisputeChannelAlert {
    pub fn new(gateway: Arc<dyn Gateway>, channel: ChannelId, site: impl Into<String>) -> Self {
        Self {
            gateway,
            channel,
            site: site.into(),
        }
    }
}

/// 通知の本文。**純粋関数。**メンションはしない。
pub fn watched_access_message(access: &WatchedAccess, site: &str) -> OutgoingMessage {
    let address = &access.address;
    let content = format!(
        "🔎 監視対象のアクセス（{source}）\n\
         Discord ID: `{discord}`\n\
         接続元: `{ip}`\n\
         CF-Connecting-IP: `{cf}` / X-Forwarded-For: `{xff}`\n\
         User-Agent: `{ua}`\n\
         {site}/admin/player?player={player}",
        source = access.source.label(),
        discord = access.discord_user_id,
        ip = address.best_guess(),
        cf = address.cf_connecting_ip.as_deref().unwrap_or("-"),
        xff = address.x_forwarded_for.as_deref().unwrap_or("-"),
        ua = address.user_agent.as_deref().unwrap_or("-"),
        player = access.player_id.0,
    );
    OutgoingMessage {
        content,
        buttons: Vec::new(),
        mention_users: Vec::new(),
        embed: None,
    }
}

#[serenity::async_trait]
impl WatchAlert for DisputeChannelAlert {
    async fn watched_access(&self, access: &WatchedAccess) -> Result<(), String> {
        let message = watched_access_message(access, &self.site);
        match self.gateway.send_message(self.channel, &message).await {
            Ok(message_id) => {
                info!(
                    channel = %self.channel,
                    %message_id,
                    discord_user_id = %access.discord_user_id,
                    "watched access posted"
                );
                Ok(())
            }
            Err(error) => {
                warn!(channel = %self.channel, %error, "watched access notification failed");
                Err(error.to_string())
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use chrono::{TimeZone, Utc};
    use uuid::Uuid;

    use super::*;
    use crate::lifecycle::PlayerId;
    use crate::registration::{AddressSource, ClientAddress};

    /// 係争チャンネル向けの文面。**メンションはしない**（本人にも運営にも通知を飛ばさない）。
    #[test]
    fn the_message_names_the_account_and_the_address_without_mentioning() {
        let access = WatchedAccess {
            discord_user_id: "900000000000000004".to_owned(),
            player_id: PlayerId(Uuid::from_u128(7)),
            source: AddressSource::Queue,
            address: ClientAddress {
                cf_connecting_ip: Some("198.51.100.7".to_owned()),
                x_forwarded_for: Some("198.51.100.7, 172.64.0.1".to_owned()),
                user_agent: Some("Mozilla/5.0".to_owned()),
            },
            at: Utc.with_ymd_and_hms(2026, 9, 13, 12, 0, 0).unwrap(),
        };
        let message = watched_access_message(&access, "https://match.example.test");
        assert!(message.content.contains("監視対象のアクセス（キュー投入）"));
        assert!(message.content.contains("`900000000000000004`"));
        assert!(message.content.contains("接続元: `198.51.100.7`"));
        assert!(
            message
                .content
                .contains("X-Forwarded-For: `198.51.100.7, 172.64.0.1`")
        );
        assert!(message.content.contains(
            "https://match.example.test/admin/player?player=00000000-0000-0000-0000-000000000007"
        ));
        assert!(!message.content.contains("<@"), "メンションしない");
        assert!(message.mention_users.is_empty());
        assert!(message.embed.is_none());
    }
}
