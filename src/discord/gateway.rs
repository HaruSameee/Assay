//! Discord API の薄い抽象。アダプタが使う操作だけを持つ。
//!
//! ここにある操作がアダプタから Discord へ出る**すべて**の呼び出し。
//! チャンネルの作成・削除・権限変更・メッセージの削除は無い（VC は読んで投稿するだけ）。
//! テストでは [`Gateway`] をメモリ上の偽物に差し替える。

use std::sync::Arc;

use serenity::async_trait;
use serenity::builder::{
    CreateActionRow, CreateAllowedMentions, CreateButton, CreateInputText,
    CreateInteractionResponse, CreateInteractionResponseMessage, CreateMessage, CreateModal,
    EditMessage,
};
use serenity::http::Http;
use serenity::model::application::InputTextStyle;
use serenity::model::channel::ChannelType;
use serenity::model::id::{ChannelId, GuildId, InteractionId, MessageId, UserId};

/// Discord API のエラー。リトライするかどうかは呼び出し側が決める。
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("discord api: {0}")]
pub struct GatewayError(pub String);

impl From<serenity::Error> for GatewayError {
    fn from(error: serenity::Error) -> Self {
        Self(error.to_string())
    }
}

/// ボタンの見た目。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ButtonStyle {
    /// 主要な操作（青）。
    Primary,
    /// 副次的な操作（灰）。
    Secondary,
}

/// メッセージに付けるボタン。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Button {
    /// [`super::custom_id::CustomId::encode`] の結果。
    pub custom_id: String,
    /// 表示文字列。
    pub label: String,
    /// 見た目。
    pub style: ButtonStyle,
}

/// 投稿・編集するメッセージ。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct OutgoingMessage {
    /// 本文。
    pub content: String,
    /// ボタン（0〜5 個、1 行）。編集時に空なら既存のボタンを外す。
    pub buttons: Vec<Button>,
    /// 通知を飛ばすユーザー。本文中の他のメンションは通知しない。
    pub mention_users: Vec<UserId>,
}

/// ルーム ID を受け取るモーダル。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Modal {
    /// モーダル自体の `custom_id`（送信時のインタラクションに付いてくる）。
    pub custom_id: String,
    /// タイトル。
    pub title: String,
    /// 入力欄の `custom_id`。
    pub input_custom_id: String,
    /// 入力欄のラベル。
    pub input_label: String,
    /// プレースホルダ。
    pub placeholder: Option<String>,
    /// 入力の最大長。
    pub max_length: u16,
    /// 初期値（修正時は現在のルーム ID）。
    pub prefill: Option<String>,
}

/// インタラクションへの応答。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InteractionResponse {
    /// ボタンが付いていたメッセージを書き換える。チャンネルの全員に見える。
    UpdateMessage(OutgoingMessage),
    /// 押した本人にだけ見えるメッセージ。エラーの通知に使う。
    Ephemeral(String),
    /// モーダルを開く。
    Modal(Modal),
    /// 何も表示せずに受領する（メッセージは別途編集する）。
    Acknowledge,
}

/// 応答先のインタラクション。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InteractionRef {
    /// インタラクション ID（冪等キーに使う）。
    pub id: InteractionId,
    /// 応答用トークン。
    pub token: String,
}

/// Discord への操作。
#[async_trait]
pub trait Gateway: Send + Sync {
    /// ギルドのボイスチャンネル一覧（名前, ID）。起動時の名前解決に使う。
    async fn voice_channels(
        &self,
        guild: GuildId,
    ) -> Result<Vec<(String, ChannelId)>, GatewayError>;

    /// チャンネル（VC のテキストチャット）にメッセージを投稿する。
    async fn send_message(
        &self,
        channel: ChannelId,
        message: &OutgoingMessage,
    ) -> Result<MessageId, GatewayError>;

    /// メッセージを編集する。
    async fn edit_message(
        &self,
        channel: ChannelId,
        message_id: MessageId,
        message: &OutgoingMessage,
    ) -> Result<(), GatewayError>;

    /// インタラクションに応答する（受信から 3 秒以内）。
    async fn respond(
        &self,
        interaction: &InteractionRef,
        response: InteractionResponse,
    ) -> Result<(), GatewayError>;
}

/// serenity の REST クライアントで [`Gateway`] を実装したもの。
#[derive(Debug, Clone)]
pub struct SerenityGateway {
    http: Arc<Http>,
}

impl SerenityGateway {
    /// ゲートウェイクライアントと同じ `Http`（レート制限器を共有）を包む。
    pub fn new(http: Arc<Http>) -> Self {
        Self { http }
    }
}

fn components(buttons: &[Button]) -> Vec<CreateActionRow> {
    if buttons.is_empty() {
        return Vec::new();
    }
    let buttons = buttons
        .iter()
        .map(|button| {
            CreateButton::new(&button.custom_id)
                .label(&button.label)
                .style(match button.style {
                    ButtonStyle::Primary => serenity::model::application::ButtonStyle::Primary,
                    ButtonStyle::Secondary => serenity::model::application::ButtonStyle::Secondary,
                })
        })
        .collect();
    vec![CreateActionRow::Buttons(buttons)]
}

fn allowed_mentions(users: &[UserId]) -> CreateAllowedMentions {
    // 既定は「誰にも通知しない」。列挙したユーザーにだけ通知する。
    CreateAllowedMentions::new()
        .everyone(false)
        .empty_roles()
        .users(users.iter().copied())
}

fn response_builder(response: InteractionResponse) -> CreateInteractionResponse {
    match response {
        InteractionResponse::UpdateMessage(message) => CreateInteractionResponse::UpdateMessage(
            CreateInteractionResponseMessage::new()
                .content(message.content)
                .components(components(&message.buttons))
                .allowed_mentions(allowed_mentions(&message.mention_users)),
        ),
        InteractionResponse::Ephemeral(content) => CreateInteractionResponse::Message(
            CreateInteractionResponseMessage::new()
                .content(content)
                .ephemeral(true)
                .allowed_mentions(allowed_mentions(&[])),
        ),
        InteractionResponse::Modal(modal) => {
            let mut input = CreateInputText::new(
                InputTextStyle::Short,
                modal.input_label,
                modal.input_custom_id,
            )
            .min_length(1)
            .max_length(modal.max_length)
            .required(true);
            if let Some(placeholder) = modal.placeholder {
                input = input.placeholder(placeholder);
            }
            if let Some(prefill) = modal.prefill {
                input = input.value(prefill);
            }
            CreateInteractionResponse::Modal(
                CreateModal::new(modal.custom_id, modal.title)
                    .components(vec![CreateActionRow::InputText(input)]),
            )
        }
        InteractionResponse::Acknowledge => CreateInteractionResponse::Acknowledge,
    }
}

#[async_trait]
impl Gateway for SerenityGateway {
    async fn voice_channels(
        &self,
        guild: GuildId,
    ) -> Result<Vec<(String, ChannelId)>, GatewayError> {
        let channels = guild.channels(&self.http).await?;
        Ok(channels
            .into_values()
            .filter(|channel| channel.kind == ChannelType::Voice)
            .map(|channel| (channel.name, channel.id))
            .collect())
    }

    async fn send_message(
        &self,
        channel: ChannelId,
        message: &OutgoingMessage,
    ) -> Result<MessageId, GatewayError> {
        let sent = channel
            .send_message(
                &self.http,
                CreateMessage::new()
                    .content(&message.content)
                    .components(components(&message.buttons))
                    .allowed_mentions(allowed_mentions(&message.mention_users)),
            )
            .await?;
        Ok(sent.id)
    }

    async fn edit_message(
        &self,
        channel: ChannelId,
        message_id: MessageId,
        message: &OutgoingMessage,
    ) -> Result<(), GatewayError> {
        channel
            .edit_message(
                &self.http,
                message_id,
                EditMessage::new()
                    .content(&message.content)
                    .components(components(&message.buttons))
                    .allowed_mentions(allowed_mentions(&message.mention_users)),
            )
            .await?;
        Ok(())
    }

    async fn respond(
        &self,
        interaction: &InteractionRef,
        response: InteractionResponse,
    ) -> Result<(), GatewayError> {
        self.http
            .create_interaction_response(
                interaction.id,
                &interaction.token,
                &response_builder(response),
                Vec::new(),
            )
            .await?;
        Ok(())
    }
}
