//! Discord アダプタ（Phase 4）。axum と同じバイナリの中で動き、DB の接続プールを共有する。
//!
//! 試合ごとのスレッドは作らない。事前に用意された VC（`vc<N>-red` / `vc<N>-blue`）の
//! テキストチャットに投稿する。全員に届ける必要があるもの（割り当て表、VC 案内、部屋設定、
//! ルーム ID）は両方の VC に、ホスト打診はホスト候補の側の VC にだけ出す。
//!
//! 扱うのはイベントで駆動されるものだけ（VC 割当、ホスト打診、ボタン、モーダル）。
//! 時間で駆動されるもの（タイムアウト、候補の繰り上げ）は Phase 5 で、[`Adapter::resume_unfinished`]
//! を拡張する形で実装する（復帰の入口を 2 つ作らない）。報告の受付は Phase 6。
//!
//! 不変条件（このモジュールで守るもの）:
//! - Discord に投稿したもの（メッセージ ID）と VC 割当（チャンネル ID）、そして Discord 呼び出しの失敗は
//!   必ず `formation_events.payload` に記録する。再起動後は Discord 側を検索し直さず、ログから続きを
//!   操作する（[`payload::DiscordRecord`]）。設定は VC の名前で持ち、ログには ID を記録する。
//! - 先に進めない失敗（VC 無し、投稿失敗）は `abandoned` で解散し、理由と詳細を payload に残す。
//!   10 人を黙って拘束しない。
//! - インタラクション由来のイベントの冪等キーは Discord のインタラクション ID。
//! - VC チャンネルは読んで投稿するだけ。作らない、消さない、権限を変更しない、メッセージを消さない
//!   （[`gateway::Gateway`] にそのような操作は無い）。
//! - VC チャットに勝敗に関する情報を出さない（[`render`] のテストで検査）。
//! - ルーム ID はモーダルで受ける。チャットの自由記述は読まない（MESSAGE_CONTENT 不要）。

pub mod adapter;
pub mod config;
pub mod custom_id;
pub mod gateway;
pub mod handler;
pub mod payload;
pub mod render;
pub mod vc_pool;

use std::sync::Arc;

use serenity::client::Client;
use serenity::model::gateway::GatewayIntents;
use serenity::model::id::GuildId;
use tracing::info;

pub use adapter::{Adapter, AdapterError, IncomingInteraction, IncomingKind};
pub use config::{ConfigError, DiscordConfig};
pub use gateway::{Gateway, GatewayError, SerenityGateway};
pub use handler::Handler;
pub use vc_pool::{ResolveError, VcPair, VcPool};

/// 起動時の VC プールの解決に失敗した。起動を失敗させる。
#[derive(Debug, thiserror::Error)]
pub enum VcPoolSetupError {
    /// ギルドのチャンネル一覧を取れなかった。
    #[error("could not list the guild's voice channels: {0}")]
    Gateway(#[from] GatewayError),
    /// 名前が欠けている、または重複している。
    #[error(transparent)]
    Resolve(#[from] ResolveError),
}

/// `vc1-red` … `vc<size>-blue` をギルドのチャンネル一覧から解決し、結果を起動ログに出す。
///
/// 1 本でも見つからない、または同名が 2 本以上あれば `Err`（試合中に発覚するより起動時に落ちるほうがよい）。
pub async fn resolve_vc_pool(
    gateway: &dyn Gateway,
    guild: GuildId,
    size: usize,
) -> Result<Vec<VcPair>, VcPoolSetupError> {
    let channels = gateway.voice_channels(guild).await?;
    let pairs = vc_pool::resolve(size, &channels)?;
    for pair in &pairs {
        info!(
            label = %pair.label,
            red = %format!("{} = {}", pair.channel_name(crate::persistence::Team::Red), pair.red),
            blue = %format!("{} = {}", pair.channel_name(crate::persistence::Team::Blue), pair.blue),
            "voice channel pair resolved"
        );
    }
    Ok(pairs)
}

/// ゲートウェイクライアントを組み立てる。
///
/// インテントは `GUILDS` だけ。インタラクションはインテントに関係なく届く。
/// 返ったクライアントの `http` を [`SerenityGateway`] に渡してアダプタを作り、
/// [`Handler::attach`] で差し込んでから `start` する。
pub async fn build_client(
    token: &str,
    handler: Arc<Handler>,
) -> Result<Client, Box<serenity::Error>> {
    Client::builder(token, GatewayIntents::GUILDS)
        .event_handler_arc(handler)
        .await
        .map_err(Box::new)
}
