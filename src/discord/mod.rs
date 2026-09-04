//! Discord アダプタ（Phase 4）。axum と同じバイナリの中で動き、DB の接続プールを共有する。
//!
//! 扱うのはイベントで駆動されるものだけ（スレッド作成、ホスト打診、ボタン、モーダル）。
//! 時間で駆動されるもの（タイムアウト、候補の繰り上げ）は Phase 5。報告の受付は Phase 6。
//!
//! 不変条件（このモジュールで守るもの）:
//! - Discord に作ったもの（スレッド・メッセージ・VC 割当）の ID は必ず `formation_events.payload` に
//!   記録する。再起動後は Discord 側を検索し直さず、ログから続きを操作する（[`payload::DiscordRecord`]）。
//! - インタラクション由来のイベントの冪等キーは Discord のインタラクション ID。
//! - VC チャンネルは読むだけ。作らない、消さない、権限を変更しない（[`gateway::Gateway`] に
//!   そのような操作は無い）。
//! - スレッドに勝敗に関する情報を出さない（[`render`] のテストで検査）。
//! - ルーム ID はモーダルで受ける。スレッドの自由記述は読まない（MESSAGE_CONTENT 不要）。

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

pub use adapter::{Adapter, AdapterError, IncomingInteraction, IncomingKind};
pub use config::{ConfigError, DiscordConfig};
pub use gateway::{Gateway, GatewayError, SerenityGateway};
pub use handler::Handler;
pub use render::{team_from_name, team_name};
pub use vc_pool::{VcPair, VcPool};

/// ゲートウェイクライアントを組み立てる。
///
/// インテントは `GUILDS` だけ。インタラクションはインテントに関係なく届く。
/// 返ったクライアントの `http` を [`SerenityGateway`] に渡してアダプタを作り、
/// [`Handler::attach`] で差し込んでから `start` する。
pub async fn build_client(token: &str, handler: Arc<Handler>) -> Result<Client, serenity::Error> {
    Client::builder(token, GatewayIntents::GUILDS)
        .event_handler_arc(handler)
        .await
}
