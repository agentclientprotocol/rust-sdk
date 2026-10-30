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
                        connection: V2ConnectionTo<Agent>| {
                responder.respond(reject_permission(&request, &connection)?)
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
            println!("{}", strip_control_characters(&output));
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
    let mut observed_active = false;
    loop {
        let notification = updates.recv().await.ok_or_else(|| {
            Error::internal_error().data("agent disconnected before the prompt ran to completion")
        })?;
        if &notification.session_id != session_id {
            continue;
        }

        match notification.update {
            v2::SessionUpdate::StateUpdate(v2::StateUpdate::Idle(idle)) => {
                if observed_active {
                    return Ok((projection.text(), idle.stop_reason));
                }
            }
            // Running, requires_action, and any state this client does not
            // recognize all mean foreground work is still active. An unknown
            // state must never be treated as idle.
            v2::SessionUpdate::StateUpdate(_) => observed_active = true,
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
    request: &v2::RequestPermissionRequest,
    connection: &V2ConnectionTo<Agent>,
) -> Result<v2::RequestPermissionResponse, Error> {
    if let Some(option) = request
        .options
        .iter()
        .find(|option| option.kind == v2::PermissionOptionKind::RejectOnce)
    {
        eprintln!("Agent requested permission; rejecting it once in this non-interactive example");
        return Ok(v2::RequestPermissionResponse::new(
            v2::RequestPermissionOutcome::Selected(v2::SelectedPermissionOutcome::new(
                option.option_id.clone(),
            )),
        ));
    }

    eprintln!("Agent requested permission without a reject option; cancelling the prompt");
    connection.send_notification(v2::CancelSessionNotification::new(
        request.session_id.clone(),
    ))?;
    Ok(v2::RequestPermissionResponse::new(
        v2::CancelledPermissionOutcome::new().into(),
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

fn content_text(content: &v2::ContentBlock) -> std::borrow::Cow<'_, str> {
    match content {
        v2::ContentBlock::Text(text) => text.text.as_str().into(),
        v2::ContentBlock::Image(image) => format!("[image: {}]", image.mime_type).into(),
        v2::ContentBlock::Audio(audio) => format!("[audio: {}]", audio.mime_type).into(),
        v2::ContentBlock::ResourceLink(link) => format!(
            "[resource: {} <{}>]",
            link.title.as_deref().unwrap_or(&link.name),
            link.uri
        )
        .into(),
        v2::ContentBlock::Resource(_) => "[embedded resource]".into(),
        _ => "[unsupported content]".into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn content_that_cannot_be_rendered_gets_a_placeholder() {
        let message_id = v2::MessageId::new("message-1");
        let mut projection = AgentTextProjection::default();
        projection.apply(v2::SessionUpdate::AgentMessage(
            v2::AgentMessage::new(message_id).content(vec![
                "see ".into(),
                v2::ContentBlock::Image(v2::ImageContent::new("aGk=", "image/png")),
                v2::ContentBlock::ResourceLink(v2::ResourceLink::new(
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
        let message_id = v2::MessageId::new("message-1");
        let mut projection = AgentTextProjection::default();

        projection.apply(v2::SessionUpdate::AgentMessageChunk(v2::ContentChunk::new(
            "hello ".into(),
            message_id.clone(),
        )));
        projection.apply(v2::SessionUpdate::AgentMessageChunk(v2::ContentChunk::new(
            "world".into(),
            message_id.clone(),
        )));
        assert_eq!(projection.text(), "hello world");

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

    #[test]
    fn interleaved_messages_keep_independent_content_and_first_seen_order() {
        // Reverse lexical order so sorting by ID would not preserve arrival order.
        let first_id = v2::MessageId::new("message-z");
        let second_id = v2::MessageId::new("message-a");
        let mut projection = AgentTextProjection::default();

        for (message_id, text) in [
            (&first_id, "first "),
            (&second_id, "second "),
            (&first_id, "message"),
            (&second_id, "message"),
        ] {
            projection.apply(v2::SessionUpdate::AgentMessageChunk(v2::ContentChunk::new(
                text.into(),
                message_id.clone(),
            )));
        }
        assert_eq!(projection.text(), "first messagesecond message");
        assert_eq!(projection.order, [first_id.clone(), second_id.clone()]);

        projection.apply(v2::SessionUpdate::AgentMessage(
            v2::AgentMessage::new(first_id.clone()).content(vec!["replacement".into()]),
        ));
        assert_eq!(projection.text(), "replacementsecond message");

        projection.apply(v2::SessionUpdate::AgentMessage(
            v2::AgentMessage::new(second_id.clone()).content(MaybeUndefined::Null),
        ));
        assert_eq!(projection.text(), "replacement");

        projection.apply(v2::SessionUpdate::AgentMessageChunk(v2::ContentChunk::new(
            "again".into(),
            second_id.clone(),
        )));
        assert_eq!(projection.text(), "replacementagain");
        assert_eq!(projection.order, [first_id, second_id]);
    }

    #[test]
    fn metadata_only_updates_establish_order_without_changing_text() {
        let first_id = v2::MessageId::new("metadata-first");
        let second_id = v2::MessageId::new("content-first");
        let metadata_only = v2::SessionUpdate::AgentMessage(
            v2::AgentMessage::new(first_id.clone()).meta(serde_json::Map::from_iter([(
                "com.example/source".to_owned(),
                serde_json::json!("replay"),
            )])),
        );
        let mut projection = AgentTextProjection::default();

        projection.apply(metadata_only.clone());
        assert_eq!(projection.order.as_slice(), std::slice::from_ref(&first_id));
        assert_eq!(
            projection.messages[&first_id],
            Vec::<v2::ContentBlock>::new()
        );
        assert_eq!(projection.text(), "");

        // The first message's content arrives last, but its metadata already
        // established its place in the text projection.
        for (message_id, text) in [(&second_id, "second"), (&first_id, "first")] {
            projection.apply(v2::SessionUpdate::AgentMessageChunk(v2::ContentChunk::new(
                text.into(),
                message_id.clone(),
            )));
        }
        assert_eq!(projection.text(), "firstsecond");

        projection.apply(metadata_only);
        assert_eq!(projection.text(), "firstsecond");
        assert_eq!(projection.order, [first_id, second_id]);
    }

    #[test]
    fn repeated_chunks_append_before_and_after_content_replacements() {
        let message_id = v2::MessageId::new("message-1");
        let chunk = v2::SessionUpdate::AgentMessageChunk(v2::ContentChunk::new(
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
            projection.apply(v2::SessionUpdate::AgentMessage(
                v2::AgentMessage::new(message_id.clone()).content(content),
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

    #[test]
    fn control_characters_are_stripped_except_newlines_and_tabs() {
        assert_eq!(
            strip_control_characters("\u{1b}[2Jline\tone\r\nline\u{7}\u{9b}31m two"),
            "[2Jline\tone\nline31m two"
        );
    }

    #[tokio::test]
    async fn unrecognized_states_count_as_foreground_work() {
        let session_id = v2::SessionId::new("session-1");
        let (update_tx, mut update_rx) = unbounded_channel();
        let other_state: v2::StateUpdate =
            serde_json::from_value(serde_json::json!({ "state": "_compacting" })).unwrap();
        for update in [
            v2::SessionUpdate::StateUpdate(other_state),
            v2::SessionUpdate::AgentMessageChunk(v2::ContentChunk::new("busy".into(), "message-1")),
            v2::SessionUpdate::StateUpdate(v2::StateUpdate::Idle(
                v2::IdleStateUpdate::new().stop_reason(v2::StopReason::EndTurn),
            )),
        ] {
            update_tx
                .send(v2::UpdateSessionNotification::new(
                    session_id.clone(),
                    update,
                ))
                .unwrap();
        }

        let (text, stop_reason) = wait_until_idle(&mut update_rx, &session_id).await.unwrap();
        assert_eq!(text, "busy");
        assert_eq!(stop_reason, Some(v2::StopReason::EndTurn));
    }

    #[tokio::test]
    async fn foreground_work_can_start_blocked_and_end_without_running() {
        let session_id = v2::SessionId::new("session-1");
        let (update_tx, mut update_rx) = unbounded_channel();
        for state in [
            v2::StateUpdate::Idle(v2::IdleStateUpdate::new()),
            v2::StateUpdate::RequiresAction(v2::RequiresActionStateUpdate::new()),
            v2::StateUpdate::Idle(
                v2::IdleStateUpdate::new().stop_reason(v2::StopReason::Cancelled),
            ),
        ] {
            update_tx
                .send(v2::UpdateSessionNotification::new(
                    session_id.clone(),
                    v2::SessionUpdate::StateUpdate(state),
                ))
                .unwrap();
        }
        drop(update_tx);

        let (text, stop_reason) = wait_until_idle(&mut update_rx, &session_id).await.unwrap();
        assert_eq!(text, "");
        assert_eq!(stop_reason, Some(v2::StopReason::Cancelled));
    }
}
