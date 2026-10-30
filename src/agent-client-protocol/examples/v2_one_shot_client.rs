//! A one-shot ACP v2 client that waits for the agent's idle update.
//!
//! Unlike ACP v1, a successful v2 `session/prompt` response only means the
//! prompt was accepted. Output and completion arrive independently through
//! `session/update`, so this example keeps receiving updates until the same
//! session reports that its foreground work is idle.
//!
//! ```text
//! cargo run -p agent-client-protocol --features process,unstable_protocol_v2 \
//!   --example v2_one_shot_client -- \
//!   --command ./target/debug/examples/simple_agent_v2 \
//!   "What should a v2 client wait for?"
//! ```

use std::{collections::HashMap, str::FromStr};

use agent_client_protocol::schema::{MaybeUndefined, ProtocolVersion};
use agent_client_protocol::v2::{ConnectionTo, Responder, schema};
use agent_client_protocol::{AcpAgent, Agent, Client, Error};
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
            async move |notification: schema::UpdateSessionNotification,
                        _connection: ConnectionTo<Agent>| {
                update_tx
                    .send(notification)
                    .map_err(Error::into_internal_error)
            },
            agent_client_protocol::on_receive_notification!(),
        )
        .on_receive_request(
            async move |request: schema::RequestPermissionRequest,
                        responder: Responder<schema::RequestPermissionResponse>,
                        connection: ConnectionTo<Agent>| {
                responder.respond(reject_permission(&request, &connection)?)
            },
            agent_client_protocol::on_receive_request!(),
        )
        .connect_with(agent, async move |connection| {
            let initialize = connection
                .send_request(schema::InitializeRequest::new(
                    ProtocolVersion::V2,
                    schema::Implementation::new("v2-one-shot-client", env!("CARGO_PKG_VERSION")),
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
            let session = opened.session;

            session.send_prompt(&cli.prompt).block_task().await?;
            eprintln!("Prompt accepted; waiting for session output and completion...");

            let (output, stop_reason) =
                wait_until_idle(&mut update_rx, session.session_id()).await?;
            println!("{}", strip_control_characters(&output));
            eprintln!("Session is idle: {stop_reason:?}");

            session.close().block_task().await?;
            Ok(())
        })
        .await?;

    Ok(())
}

async fn wait_until_idle(
    updates: &mut UnboundedReceiver<schema::UpdateSessionNotification>,
    session_id: &schema::SessionId,
) -> Result<(String, Option<schema::StopReason>), Error> {
    let mut projection = AgentTextProjection::default();
    let mut observed_active = false;
    loop {
        let notification = updates.recv().await.ok_or_else(|| {
            Error::internal_error().data("agent disconnected before the prompt ran to completion")
        })?;
        if &notification.session_id != session_id {
            continue;
        }

        match notification.update {
            schema::SessionUpdate::StateUpdate(schema::StateUpdate::Idle(idle)) => {
                if observed_active {
                    return Ok((projection.text(), idle.stop_reason));
                }
            }
            // Running, requires_action, and any state this client does not
            // recognize all mean foreground work is still active. An unknown
            // state must never be treated as idle.
            schema::SessionUpdate::StateUpdate(_) => observed_active = true,
            update if observed_active => projection.apply(update),
            _ => {}
        }
    }
}

/// Declines a permission request in this non-interactive example.
///
/// Selects the agent's `reject_once` option when it offers one. The
/// `cancelled` outcome is only valid once the client has cancelled the active
/// work, so without such an option this sends `session/cancel` first.
fn reject_permission(
    request: &schema::RequestPermissionRequest,
    connection: &ConnectionTo<Agent>,
) -> Result<schema::RequestPermissionResponse, Error> {
    if let Some(option) = request
        .options
        .iter()
        .find(|option| option.kind == schema::PermissionOptionKind::RejectOnce)
    {
        eprintln!("Agent requested permission; rejecting it once in this non-interactive example");
        return Ok(schema::RequestPermissionResponse::new(
            schema::RequestPermissionOutcome::Selected(schema::SelectedPermissionOutcome::new(
                option.option_id.clone(),
            )),
        ));
    }

    eprintln!("Agent requested permission without a reject option; cancelling the prompt");
    connection.send_notification(schema::CancelSessionNotification::new(
        request.session_id.clone(),
    ))?;
    Ok(schema::RequestPermissionResponse::new(
        schema::CancelledPermissionOutcome::new().into(),
    ))
}

/// Removes terminal control characters from agent text before printing it.
///
/// Agent text is untrusted, and escape sequences could rewrite or hide
/// terminal output. Newlines and tabs are kept for readability.
fn strip_control_characters(text: &str) -> String {
    text.chars()
        .filter(|character| !character.is_control() || matches!(character, '\n' | '\t'))
        .collect()
}

/// Minimal projection of v2 agent messages.
///
/// Chunks append to a message, while an `agent_message` snapshot patches the
/// accumulated content for the same `messageId`: a value replaces it, null
/// clears it, and an omitted field preserves it.
#[derive(Default)]
struct AgentTextProjection {
    order: Vec<schema::MessageId>,
    messages: HashMap<schema::MessageId, Vec<schema::ContentBlock>>,
}

impl AgentTextProjection {
    fn apply(&mut self, update: schema::SessionUpdate) {
        match update {
            schema::SessionUpdate::AgentMessageChunk(chunk) => {
                self.message_content(chunk.message_id).push(chunk.content);
            }
            schema::SessionUpdate::AgentMessage(message) => {
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

    fn message_content(&mut self, message_id: schema::MessageId) -> &mut Vec<schema::ContentBlock> {
        if !self.messages.contains_key(&message_id) {
            self.order.push(message_id.clone());
        }
        self.messages.entry(message_id).or_default()
    }

    /// Render the agent's messages as text. Content this terminal client
    /// cannot show is replaced by a placeholder rather than silently dropped.
    fn text(&self) -> String {
        self.order
            .iter()
            .filter_map(|message_id| self.messages.get(message_id))
            .flatten()
            .map(content_text)
            .collect()
    }
}

fn content_text(content: &schema::ContentBlock) -> std::borrow::Cow<'_, str> {
    match content {
        schema::ContentBlock::Text(text) => text.text.as_str().into(),
        schema::ContentBlock::Image(image) => format!("[image: {}]", image.mime_type).into(),
        schema::ContentBlock::Audio(audio) => format!("[audio: {}]", audio.mime_type).into(),
        schema::ContentBlock::ResourceLink(link) => format!(
            "[resource: {} <{}>]",
            link.title.as_deref().unwrap_or(&link.name),
            link.uri
        )
        .into(),
        schema::ContentBlock::Resource(_) => "[embedded resource]".into(),
        _ => "[unsupported content]".into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn content_that_cannot_be_rendered_gets_a_placeholder() {
        let message_id = schema::MessageId::new("message-1");
        let mut projection = AgentTextProjection::default();
        projection.apply(schema::SessionUpdate::AgentMessage(
            schema::AgentMessage::new(message_id).content(vec![
                "see ".into(),
                schema::ContentBlock::Image(schema::ImageContent::new("aGk=", "image/png")),
                schema::ContentBlock::ResourceLink(schema::ResourceLink::new(
                    "notes.md",
                    "file:///notes.md",
                )),
            ]),
        ));

        assert_eq!(
            projection.text(),
            "see [image: image/png][resource: notes.md <file:///notes.md>]"
        );
    }

    #[test]
    fn agent_message_snapshots_patch_accumulated_chunks() {
        let message_id = schema::MessageId::new("message-1");
        let mut projection = AgentTextProjection::default();

        projection.apply(schema::SessionUpdate::AgentMessageChunk(
            schema::ContentChunk::new("hello ".into(), message_id.clone()),
        ));
        projection.apply(schema::SessionUpdate::AgentMessageChunk(
            schema::ContentChunk::new("world".into(), message_id.clone()),
        ));
        assert_eq!(projection.text(), "hello world");

        projection.apply(schema::SessionUpdate::AgentMessage(
            schema::AgentMessage::new(message_id.clone()).content(vec!["replacement".into()]),
        ));
        assert_eq!(projection.text(), "replacement");

        projection.apply(schema::SessionUpdate::AgentMessage(
            schema::AgentMessage::new(message_id.clone()),
        ));
        assert_eq!(projection.text(), "replacement");

        projection.apply(schema::SessionUpdate::AgentMessage(
            schema::AgentMessage::new(message_id.clone()).content(MaybeUndefined::Null),
        ));
        assert_eq!(projection.text(), "");

        projection.apply(schema::SessionUpdate::AgentMessageChunk(
            schema::ContentChunk::new("again".into(), message_id),
        ));
        assert_eq!(projection.text(), "again");
    }

    #[test]
    fn interleaved_messages_keep_independent_content_and_first_seen_order() {
        // Reverse lexical order so sorting by ID would not preserve arrival order.
        let first_id = schema::MessageId::new("message-z");
        let second_id = schema::MessageId::new("message-a");
        let mut projection = AgentTextProjection::default();

        for (message_id, text) in [
            (&first_id, "first "),
            (&second_id, "second "),
            (&first_id, "message"),
            (&second_id, "message"),
        ] {
            projection.apply(schema::SessionUpdate::AgentMessageChunk(
                schema::ContentChunk::new(text.into(), message_id.clone()),
            ));
        }
        assert_eq!(projection.text(), "first messagesecond message");
        assert_eq!(projection.order, [first_id.clone(), second_id.clone()]);

        projection.apply(schema::SessionUpdate::AgentMessage(
            schema::AgentMessage::new(first_id.clone()).content(vec!["replacement".into()]),
        ));
        assert_eq!(projection.text(), "replacementsecond message");

        projection.apply(schema::SessionUpdate::AgentMessage(
            schema::AgentMessage::new(second_id.clone()).content(MaybeUndefined::Null),
        ));
        assert_eq!(projection.text(), "replacement");

        projection.apply(schema::SessionUpdate::AgentMessageChunk(
            schema::ContentChunk::new("again".into(), second_id.clone()),
        ));
        assert_eq!(projection.text(), "replacementagain");
        assert_eq!(projection.order, [first_id, second_id]);
    }

    #[test]
    fn metadata_only_updates_establish_order_without_changing_text() {
        let first_id = schema::MessageId::new("metadata-first");
        let second_id = schema::MessageId::new("content-first");
        let metadata_only = schema::SessionUpdate::AgentMessage(
            schema::AgentMessage::new(first_id.clone()).meta(serde_json::Map::from_iter([(
                "com.example/source".to_owned(),
                serde_json::json!("replay"),
            )])),
        );
        let mut projection = AgentTextProjection::default();

        projection.apply(metadata_only.clone());
        assert_eq!(projection.order.as_slice(), std::slice::from_ref(&first_id));
        assert_eq!(
            projection.messages[&first_id],
            Vec::<schema::ContentBlock>::new()
        );
        assert_eq!(projection.text(), "");

        // The first message's content arrives last, but its metadata already
        // established its place in the text projection.
        for (message_id, text) in [(&second_id, "second"), (&first_id, "first")] {
            projection.apply(schema::SessionUpdate::AgentMessageChunk(
                schema::ContentChunk::new(text.into(), message_id.clone()),
            ));
        }
        assert_eq!(projection.text(), "firstsecond");

        projection.apply(metadata_only);
        assert_eq!(projection.text(), "firstsecond");
        assert_eq!(projection.order, [first_id, second_id]);
    }

    #[test]
    fn repeated_chunks_append_before_and_after_content_replacements() {
        let message_id = schema::MessageId::new("message-1");
        let chunk = schema::SessionUpdate::AgentMessageChunk(schema::ContentChunk::new(
            "repeat ".into(),
            message_id.clone(),
        ));
        let mut projection = AgentTextProjection::default();

        projection.apply(chunk.clone());
        projection.apply(chunk.clone());
        assert_eq!(projection.text(), "repeat repeat ");

        for (content, expected) in [
            (MaybeUndefined::Undefined, "repeat repeat "),
            (
                MaybeUndefined::Value(vec!["replacement ".into()]),
                "replacement ",
            ),
            (MaybeUndefined::Null, ""),
            (MaybeUndefined::Value(Vec::new()), ""),
        ] {
            projection.apply(schema::SessionUpdate::AgentMessage(
                schema::AgentMessage::new(message_id.clone()).content(content),
            ));
            assert_eq!(projection.text(), expected);

            // Repeating the exact same chunk is another append, not an
            // idempotent replay, even after replacement or clearing.
            projection.apply(chunk.clone());
            projection.apply(chunk.clone());
            assert_eq!(projection.text(), format!("{expected}repeat repeat "));
        }
        assert_eq!(projection.order, [message_id]);
    }

    #[tokio::test]
    async fn prompt_completion_ignores_updates_until_running() {
        let session_id = schema::SessionId::new("session-1");
        let (update_tx, mut update_rx) = unbounded_channel();
        let notification =
            |update| schema::UpdateSessionNotification::new(session_id.clone(), update);

        update_tx
            .send(notification(schema::SessionUpdate::AgentMessageChunk(
                schema::ContentChunk::new("stale".into(), "stale-message"),
            )))
            .unwrap();
        update_tx
            .send(notification(schema::SessionUpdate::StateUpdate(
                schema::StateUpdate::Idle(schema::IdleStateUpdate::new()),
            )))
            .unwrap();
        update_tx
            .send(notification(schema::SessionUpdate::StateUpdate(
                schema::StateUpdate::Running(schema::RunningStateUpdate::new()),
            )))
            .unwrap();
        update_tx
            .send(notification(schema::SessionUpdate::AgentMessageChunk(
                schema::ContentChunk::new("current".into(), "current-message"),
            )))
            .unwrap();
        update_tx
            .send(notification(schema::SessionUpdate::AgentMessage(
                schema::AgentMessage::new("current-message").content(vec!["current".into()]),
            )))
            .unwrap();
        update_tx
            .send(notification(schema::SessionUpdate::StateUpdate(
                schema::StateUpdate::Idle(
                    schema::IdleStateUpdate::new().stop_reason(schema::StopReason::EndTurn),
                ),
            )))
            .unwrap();

        let (text, stop_reason) = wait_until_idle(&mut update_rx, &session_id).await.unwrap();
        assert_eq!(text, "current");
        assert_eq!(stop_reason, Some(schema::StopReason::EndTurn));
    }

    #[test]
    fn control_characters_are_stripped_except_newlines_and_tabs() {
        assert_eq!(
            strip_control_characters("\u{1b}[2Jline\tone\r\nline\u{7}\u{9b}31m two"),
            "[2Jline\tone\nline31m two"
        );
    }

    #[tokio::test]
    async fn unrecognized_states_count_as_foreground_work() {
        let session_id = schema::SessionId::new("session-1");
        let (update_tx, mut update_rx) = unbounded_channel();
        let other_state: schema::StateUpdate =
            serde_json::from_value(serde_json::json!({ "state": "_compacting" })).unwrap();
        for update in [
            schema::SessionUpdate::StateUpdate(other_state),
            schema::SessionUpdate::AgentMessageChunk(schema::ContentChunk::new(
                "busy".into(),
                "message-1",
            )),
            schema::SessionUpdate::StateUpdate(schema::StateUpdate::Idle(
                schema::IdleStateUpdate::new().stop_reason(schema::StopReason::EndTurn),
            )),
        ] {
            update_tx
                .send(schema::UpdateSessionNotification::new(
                    session_id.clone(),
                    update,
                ))
                .unwrap();
        }

        let (text, stop_reason) = wait_until_idle(&mut update_rx, &session_id).await.unwrap();
        assert_eq!(text, "busy");
        assert_eq!(stop_reason, Some(schema::StopReason::EndTurn));
    }

    #[tokio::test]
    async fn foreground_work_can_start_blocked_and_end_without_running() {
        let session_id = schema::SessionId::new("session-1");
        let (update_tx, mut update_rx) = unbounded_channel();
        for state in [
            schema::StateUpdate::Idle(schema::IdleStateUpdate::new()),
            schema::StateUpdate::RequiresAction(schema::RequiresActionStateUpdate::new()),
            schema::StateUpdate::Idle(
                schema::IdleStateUpdate::new().stop_reason(schema::StopReason::Cancelled),
            ),
        ] {
            update_tx
                .send(schema::UpdateSessionNotification::new(
                    session_id.clone(),
                    schema::SessionUpdate::StateUpdate(state),
                ))
                .unwrap();
        }
        drop(update_tx);

        let (text, stop_reason) = wait_until_idle(&mut update_rx, &session_id).await.unwrap();
        assert_eq!(text, "");
        assert_eq!(stop_reason, Some(schema::StopReason::Cancelled));
    }
}
