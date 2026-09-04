//! serenity のイベントハンドラ。ゲートウェイのイベントを [`Adapter`] に渡すだけ。
//!
//! 受けるのは `ready` と `interaction_create` だけ。メッセージ本文は読まない
//! （MESSAGE_CONTENT の特権インテントは不要）。

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock};

use serenity::async_trait;
use serenity::client::{Context, EventHandler};
use serenity::model::application::{ActionRowComponent, Interaction};
use serenity::model::gateway::Ready;
use tracing::{error, info, warn};

use super::adapter::{Adapter, IncomingInteraction, IncomingKind};

/// イベントハンドラ。
///
/// アダプタはゲートウェイクライアントの `Http` を必要とし、クライアントはハンドラを
/// 必要とするので、アダプタは後から [`Handler::attach`] で差し込む。
#[derive(Default)]
pub struct Handler {
    adapter: OnceLock<Arc<Adapter>>,
    resumed: AtomicBool,
}

impl Handler {
    /// アダプタを差し込む。2 回目以降は無視する。
    pub fn attach(&self, adapter: Arc<Adapter>) {
        if self.adapter.set(adapter).is_err() {
            warn!("adapter was already attached");
        }
    }
}

/// serenity の `Interaction` をアダプタの入力に正規化する。対象外の種類は `None`。
pub fn normalize(interaction: Interaction) -> Option<IncomingInteraction> {
    match interaction {
        Interaction::Component(component) => Some(IncomingInteraction {
            id: component.id,
            token: component.token,
            user: component.user.id,
            guild: component.guild_id,
            kind: IncomingKind::Button {
                custom_id: component.data.custom_id,
            },
        }),
        Interaction::Modal(modal) => {
            let inputs: HashMap<String, String> = modal
                .data
                .components
                .iter()
                .flat_map(|row| row.components.iter())
                .filter_map(|component| match component {
                    ActionRowComponent::InputText(input) => Some((
                        input.custom_id.clone(),
                        input.value.clone().unwrap_or_default(),
                    )),
                    _ => None,
                })
                .collect();
            Some(IncomingInteraction {
                id: modal.id,
                token: modal.token,
                user: modal.user.id,
                guild: modal.guild_id,
                kind: IncomingKind::ModalSubmit {
                    custom_id: modal.data.custom_id,
                    inputs,
                },
            })
        }
        _ => None,
    }
}

#[async_trait]
impl EventHandler for Handler {
    async fn ready(&self, _ctx: Context, ready: Ready) {
        info!(user = %ready.user.name, "discord gateway ready");
        let Some(adapter) = self.adapter.get().cloned() else {
            warn!("adapter is not attached; nothing to resume");
            return;
        };
        // 再接続のたびに ready が来る。再開は最初の 1 回だけ
        if self.resumed.swap(true, Ordering::SeqCst) {
            return;
        }
        tokio::spawn(async move {
            if let Err(error) = adapter.resume_unfinished().await {
                error!(%error, "resume failed");
            }
        });
    }

    async fn interaction_create(&self, _ctx: Context, interaction: Interaction) {
        let Some(adapter) = self.adapter.get().cloned() else {
            warn!("adapter is not attached; dropping interaction");
            return;
        };
        let Some(incoming) = normalize(interaction) else {
            return;
        };
        let id = incoming.id;
        if let Err(error) = adapter.handle_interaction(incoming).await {
            error!(interaction = %id, %error, "interaction handling failed");
        }
    }
}
