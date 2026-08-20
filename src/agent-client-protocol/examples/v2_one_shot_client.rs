//! A one-shot ACP v2 client that waits for the agent's idle update.
//!
//! Unlike ACP v1, a successful v2 `session/prompt` response only means the
//! prompt was accepted. Output and completion arrive independently through
//! `session/update`, so this example keeps receiving updates until the same
//! session reports that its foreground work is idle.
//!
//! ```text
//! cargo run -p agent-client-protocol --features unstable_protocol_v2 \
//!   --example v2_one_shot_client -- \
//!   --command ./target/debug/examples/simple_agent_v2 \
//!   "What should a v2 client wait for?"
//! ```

use std::{collections::HashMap, str::FromStr};

use agent_client_protocol::schema::{MaybeUndefined, ProtocolVersion, v2};
use agent_client_protocol::{AcpAgent, Agent, Client, Error, Responder, V2ConnectionTo};
use clap::Parser;
use tokio::sync::mpsc::{UnboundedReceiver, unbounded_channel};

#[derive(Parser)]
#[command(name = "v2-one-shot-client")]
#[command(about = "Send one prompt to an ACP v2 agent and wait for idle")]
struct Cli {
    /// Command used to start the agent.
    #[arg(short, long)]
    command: String,

    /// Text to send to the agent.
    prompt: String,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let cli = Cli::parse();
    let agent = AcpAgent::from_str(&cli.command)?;
    let (update_tx, mut update_rx) = unbounded_channel();

    Client
        .v2()
        .name("v2-one-shot-client")
        .on_receive_notification(
            async move |notification: v2::UpdateSessionNotification,
                        _connection: V2ConnectionTo<Agent>| {
                update_tx
                    .send(notification)
                    .map_err(Error::into_internal_error)
            },
            agent_client_protocol::on_receive_notification!(),
        )
        .on_receive_request(
            async move |request: v2::RequestPermissionRequest,
                        responder: Responder<v2::RequestPermissionResponse>,
                        _connection: V2ConnectionTo<Agent>| {
                eprintln!(
                    "Agent requested permission for session {}; cancelling in this non-interactive example",
                    request.session_id
                );
                responder.respond(v2::RequestPermissionResponse::new(
                    v2::RequestPermissionOutcome::Cancelled,
                ))
            },
            agent_client_protocol::on_receive_request!(),
        )
        .connect_with(agent, async move |connection| {
            let initialize = connection
                .send_request(v2::InitializeRequest::new(
                    ProtocolVersion::V2,
                    v2::Implementation::new("v2-one-shot-client", env!("CARGO_PKG_VERSION")),
                ))
                .block_task()
                .await?;
            if initialize.capabilities.session.is_none() {
                return Err(Error::invalid_params()
                    .data("agent did not advertise the v2 session capability"));
            }

            let opened = connection
                .build_session_cwd()?
                .start_session()
                .block_task()
                .await?;
            let session = opened.into_session();

            session.send_prompt(&cli.prompt).block_task().await?;
            eprintln!("Prompt accepted; waiting for session output and completion...");

            let (output, stop_reason) =
                wait_until_idle(&mut update_rx, session.session_id()).await?;
            println!("{output}");
            eprintln!("Session is idle: {stop_reason:?}");

            session.close().block_task().await?;
            Ok(())
        })
        .await?;

    Ok(())
}

async fn wait_until_idle(
    updates: &mut UnboundedReceiver<v2::UpdateSessionNotification>,
    session_id: &v2::SessionId,
) -> Result<(String, Option<v2::StopReason>), Error> {
    let mut projection = AgentTextProjection::default();
    let mut observed_running = false;
    loop {
        let notification = updates.recv().await.ok_or_else(|| {
            Error::internal_error().data("agent disconnected before the prompt ran to completion")
        })?;
        if &notification.session_id != session_id {
            continue;
        }

        match notification.update {
            v2::SessionUpdate::StateUpdate(v2::StateUpdate::Running(_)) => {
                observed_running = true;
            }
            v2::SessionUpdate::StateUpdate(v2::StateUpdate::Idle(idle)) if observed_running => {
                return Ok((projection.text(), idle.stop_reason));
            }
            update if observed_running => projection.apply(update),
            _ => {}
        }
    }
}

/// Minimal projection of v2 agent messages.
///
/// Chunks append to a message, while an `agent_message` snapshot patches the
/// accumulated content for the same `messageId`: a value replaces it, null
/// clears it, and an omitted field preserves it.
#[derive(Default)]
struct AgentTextProjection {
    order: Vec<v2::MessageId>,
    messages: HashMap<v2::MessageId, Vec<v2::ContentBlock>>,
}

impl AgentTextProjection {
    fn apply(&mut self, update: v2::SessionUpdate) {
        match update {
            v2::SessionUpdate::AgentMessageChunk(chunk) => {
                self.message_content(chunk.message_id).push(chunk.content);
            }
            v2::SessionUpdate::AgentMessage(message) => {
                let content = self.message_content(message.message_id);
                match message.content {
                    MaybeUndefined::Undefined => {}
                    MaybeUndefined::Null => content.clear(),
                    MaybeUndefined::Value(replacement) => *content = replacement,
                }
            }
            _ => {}
        }
    }

    fn message_content(&mut self, message_id: v2::MessageId) -> &mut Vec<v2::ContentBlock> {
        if !self.messages.contains_key(&message_id) {
            self.order.push(message_id.clone());
        }
        self.messages.entry(message_id).or_default()
    }

    fn text(&self) -> String {
        self.order
            .iter()
            .filter_map(|message_id| self.messages.get(message_id))
            .flatten()
            .filter_map(|content| match content {
                v2::ContentBlock::Text(text) => Some(text.text.as_str()),
                _ => None,
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn agent_message_snapshots_patch_accumulated_chunks() {
        let message_id = v2::MessageId::new("message-1");
        let mut projection = AgentTextProjection::default();

        projection.apply(v2::SessionUpdate::AgentMessageChunk(v2::ContentChunk::new(
            "hel".into(),
            message_id.clone(),
        )));
        projection.apply(v2::SessionUpdate::AgentMessageChunk(v2::ContentChunk::new(
            "lo".into(),
            message_id.clone(),
        )));
        assert_eq!(projection.text(), "hello");

        projection.apply(v2::SessionUpdate::AgentMessage(
            v2::AgentMessage::new(message_id.clone()).content(vec!["replacement".into()]),
        ));
        assert_eq!(projection.text(), "replacement");

        projection.apply(v2::SessionUpdate::AgentMessage(v2::AgentMessage::new(
            message_id.clone(),
        )));
        assert_eq!(projection.text(), "replacement");

        projection.apply(v2::SessionUpdate::AgentMessage(
            v2::AgentMessage::new(message_id.clone()).content(MaybeUndefined::Null),
        ));
        assert_eq!(projection.text(), "");

        projection.apply(v2::SessionUpdate::AgentMessageChunk(v2::ContentChunk::new(
            "again".into(),
            message_id,
        )));
        assert_eq!(projection.text(), "again");
    }

    #[tokio::test]
    async fn prompt_completion_ignores_updates_until_running() {
        let session_id = v2::SessionId::new("session-1");
        let (update_tx, mut update_rx) = unbounded_channel();
        let notification = |update| v2::UpdateSessionNotification::new(session_id.clone(), update);

        update_tx
            .send(notification(v2::SessionUpdate::AgentMessageChunk(
                v2::ContentChunk::new("stale".into(), "stale-message"),
            )))
            .unwrap();
        update_tx
            .send(notification(v2::SessionUpdate::StateUpdate(
                v2::StateUpdate::Idle(v2::IdleStateUpdate::new()),
            )))
            .unwrap();
        update_tx
            .send(notification(v2::SessionUpdate::StateUpdate(
                v2::StateUpdate::Running(v2::RunningStateUpdate::new()),
            )))
            .unwrap();
        update_tx
            .send(notification(v2::SessionUpdate::AgentMessageChunk(
                v2::ContentChunk::new("current".into(), "current-message"),
            )))
            .unwrap();
        update_tx
            .send(notification(v2::SessionUpdate::AgentMessage(
                v2::AgentMessage::new("current-message").content(vec!["current".into()]),
            )))
            .unwrap();
        update_tx
            .send(notification(v2::SessionUpdate::StateUpdate(
                v2::StateUpdate::Idle(
                    v2::IdleStateUpdate::new().stop_reason(v2::StopReason::EndTurn),
                ),
            )))
            .unwrap();

        let (text, stop_reason) = wait_until_idle(&mut update_rx, &session_id).await.unwrap();
        assert_eq!(text, "current");
        assert_eq!(stop_reason, Some(v2::StopReason::EndTurn));
    }
}
