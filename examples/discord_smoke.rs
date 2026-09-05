//! Run with: SQLX_OFFLINE=true cargo run --example discord_smoke

use std::{env, error::Error, io, sync::Arc, time::Duration};

use serenity::{
    async_trait,
    client::{Client, Context, EventHandler},
    model::{
        application::Interaction,
        gateway::{GatewayIntents, Ready},
        id::GuildId,
    },
};
use tokio::sync::{Notify, mpsc};
use unite_matchsystem::discord::{
    Gateway, GatewayError, SerenityGateway,
    config::keys,
    gateway::{Button, ButtonStyle, InteractionRef, InteractionResponse, OutgoingMessage},
};

type SmokeResult = Result<(), Box<dyn Error + Send + Sync>>;

struct Handler {
    guild: GuildId,
    initial_id: String,
    edited_id: String,
    ready: Arc<Notify>,
    finished: mpsc::UnboundedSender<Result<(), GatewayError>>,
}

#[async_trait]
impl EventHandler for Handler {
    async fn ready(&self, _ctx: Context, ready: Ready) {
        println!("Connected as {}", ready.user.name);
        self.ready.notify_one();
    }

    async fn interaction_create(&self, ctx: Context, interaction: Interaction) {
        let Interaction::Component(component) = interaction else {
            return;
        };
        if component.guild_id != Some(self.guild) {
            return;
        }
        let edited = component.data.custom_id == self.edited_id;
        if !edited && component.data.custom_id != self.initial_id {
            return;
        }

        // A click during the three-second preview must not finish the test early.
        let response = if edited {
            InteractionResponse::UpdateMessage(OutgoingMessage {
                content: "Discord smoke test: UpdateMessage received. Complete.".into(),
                ..Default::default()
            })
        } else {
            InteractionResponse::Acknowledge
        };
        let result = SerenityGateway::new(ctx.http)
            .respond(
                &InteractionRef {
                    id: component.id,
                    token: component.token,
                },
                response,
            )
            .await;
        if edited || result.is_err() {
            let _ = self.finished.send(result);
        }
    }
}

fn message(content: &str, custom_id: String, label: &str, style: ButtonStyle) -> OutgoingMessage {
    OutgoingMessage {
        content: content.into(),
        buttons: vec![Button {
            custom_id,
            label: label.into(),
            style,
        }],
        ..Default::default()
    }
}

#[tokio::main]
async fn main() -> SmokeResult {
    if let Err(error) = dotenvy::dotenv()
        && !error.not_found()
    {
        // Do not include dotenv parse errors, which can contain the token's source line.
        return Err(io::Error::other("could not load .env").into());
    }
    let token = env::var(keys::TOKEN)?;
    if token.trim().is_empty() {
        return Err(io::Error::other("DISCORD_TOKEN must not be empty").into());
    }
    let guild = env::var(keys::GUILD_ID)?.trim().parse::<u64>()?;
    if guild == 0 {
        return Err(io::Error::other("DISCORD_GUILD_ID must be nonzero").into());
    }
    let guild = GuildId::new(guild);
    let run_id = uuid::Uuid::new_v4();
    let initial_id = format!("discord_smoke:{run_id}:initial");
    let edited_id = format!("discord_smoke:{run_id}:edited");
    let ready = Arc::new(Notify::new());
    let (finished, mut completion) = mpsc::unbounded_channel();
    let mut client = Client::builder(token, GatewayIntents::GUILDS)
        .event_handler(Handler {
            guild,
            initial_id: initial_id.clone(),
            edited_id: edited_id.clone(),
            ready: ready.clone(),
            finished,
        })
        .await?;
    let gateway = SerenityGateway::new(client.http.clone());
    let shutdown = client.shard_manager.clone();
    let smoke = async {
        ready.notified().await;
        let mut channels = gateway.voice_channels(guild).await?;
        channels.sort();
        for (name, id) in &channels {
            println!("{name}\t{id}");
        }
        let targets: Vec<_> = channels
            .iter()
            .filter(|(name, _)| name == "vc1-red")
            .collect();
        let channel = match targets.as_slice() {
            [(_, id)] => *id,
            _ => {
                return Err(
                    io::Error::other("expected exactly one voice channel named vc1-red").into(),
                );
            }
        };
        let id = gateway
            .send_message(
                channel,
                &message(
                    "Discord smoke test: editing in 3 seconds.",
                    initial_id,
                    "Preview",
                    ButtonStyle::Secondary,
                ),
            )
            .await?;
        println!("Posted message {id} in vc1-red ({channel})");
        tokio::time::sleep(Duration::from_secs(3)).await;
        gateway
            .edit_message(
                channel,
                id,
                &message(
                    "Discord smoke test: message and button replaced.",
                    edited_id,
                    "Complete smoke test",
                    ButtonStyle::Primary,
                ),
            )
            .await?;
        println!("Edited message {id}; waiting for the button (Ctrl+C to stop).");
        completion
            .recv()
            .await
            .ok_or_else(|| io::Error::other("interaction handler closed"))??;
        println!("UpdateMessage succeeded.");
        Ok::<(), Box<dyn Error + Send + Sync>>(())
    };
    let connection = client.start();
    tokio::pin!(connection);
    let result = tokio::select! {
        result = &mut connection => return result.map_err(Into::into),
        result = smoke => result,
        result = tokio::signal::ctrl_c() => result.map_err(Into::into),
    };
    shutdown.shutdown_all().await;
    connection.await?;
    result
}
