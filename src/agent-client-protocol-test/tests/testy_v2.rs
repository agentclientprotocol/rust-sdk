#![cfg(feature = "unstable_protocol_v2")]

use std::{path::PathBuf, time::Duration};

use agent_client_protocol::v2::ConnectionTo;
use agent_client_protocol::{
    Client, Error, schema::ProtocolVersion, v1::schema as v1, v2::schema as v2,
};
use agent_client_protocol_test::testy::{self, TestyCommand, TestyScenario};
use tokio::sync::mpsc::{UnboundedReceiver, unbounded_channel};

const TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Debug)]
enum TestEvent {
    Update(Box<v2::UpdateSessionNotification>),
    PromptAccepted,
    ResumeResponded,
    CloseResponded,
}

fn implementation() -> v2::Implementation {
    v2::Implementation::new("testy-v2-test", env!("CARGO_PKG_VERSION"))
}

fn wait_for_cancel_prompt() -> String {
    TestyCommand::RunScenario {
        scenario: TestyScenario::WaitForCancel,
    }
    .to_prompt()
}

async fn next_event(events: &mut UnboundedReceiver<TestEvent>) -> TestEvent {
    events
        .recv()
        .await
        .expect("the Testy v2 event handler should remain active")
}

async fn next_update(events: &mut UnboundedReceiver<TestEvent>) -> v2::UpdateSessionNotification {
    match next_event(events).await {
        TestEvent::Update(update) => *update,
        other => panic!("expected a session update, got {other:?}"),
    }
}

fn assert_session_info_snapshot(notification: v2::UpdateSessionNotification) {
    match notification.update {
        v2::SessionUpdate::SessionInfoUpdate(info) => {
            assert_eq!(
                info.title.value().map(String::as_str),
                Some("Testy v2 session")
            );
        }
        other => panic!("expected a session info snapshot, got {other:?}"),
    }
}

fn only_text(
    content: &agent_client_protocol::schema::MaybeUndefined<Vec<v2::ContentBlock>>,
) -> Option<&str> {
    match content.value()?.as_slice() {
        [v2::ContentBlock::Text(text)] => Some(&text.text),
        _ => None,
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn testy_router_runs_v2_prompt_lifecycle_and_resume_replay() {
    let (event_tx, mut event_rx) = unbounded_channel();
    let update_tx = event_tx.clone();
    let client = Client
        .v2()
        .on_receive_notification(
            async move |update: v2::UpdateSessionNotification,
                        _connection: ConnectionTo<agent_client_protocol::Agent>| {
                update_tx
                    .send(TestEvent::Update(Box::new(update)))
                    .map_err(Error::into_internal_error)
            },
            agent_client_protocol::on_receive_notification!(),
        )
        .connect_with(testy::protocol_router(), async move |connection| {
            let initialize = connection
                .send_request(v2::InitializeRequest::new(
                    ProtocolVersion::V2,
                    implementation(),
                ))
                .block_task()
                .await?;
            assert_eq!(initialize.protocol_version, ProtocolVersion::V2);
            assert!(initialize.capabilities.session.is_some());

            let opened = connection
                .build_session(PathBuf::from("/tmp"))
                .start_session()
                .block_task()
                .await?;
            let (session, new_session_response) = (opened.session, opened.response);
            assert_eq!(&new_session_response.session_id, session.session_id());
            assert!(
                session
                    .session_id()
                    .to_string()
                    .starts_with("testy-v2-session-")
            );

            session
                .send_prompt(
                    TestyCommand::Echo {
                        message: "hello from v2".to_string(),
                    }
                    .to_prompt(),
                )
                .on_receiving_result({
                    let event_tx = event_tx.clone();
                    async move |result| {
                        assert_eq!(result?, v2::PromptResponse::new("testy-v2-user-message-0"));
                        event_tx
                            .send(TestEvent::PromptAccepted)
                            .map_err(Error::into_internal_error)
                    }
                })?;
            assert!(matches!(
                next_event(&mut event_rx).await,
                TestEvent::PromptAccepted
            ));

            let accepted_user = next_update(&mut event_rx).await;
            assert_eq!(&accepted_user.session_id, session.session_id());
            let user_message_id = match accepted_user.update {
                v2::SessionUpdate::UserMessage(message) => message.message_id,
                other => panic!("expected accepted user message, got {other:?}"),
            };

            assert!(matches!(
                next_update(&mut event_rx).await.update,
                v2::SessionUpdate::StateUpdate(v2::StateUpdate::Running(_))
            ));

            let agent_message_id = match next_update(&mut event_rx).await.update {
                v2::SessionUpdate::AgentMessageChunk(chunk) => {
                    assert!(matches!(
                        chunk.content,
                        v2::ContentBlock::Text(text) if text.text == "hello from v2"
                    ));
                    chunk.message_id
                }
                other => panic!("expected agent message chunk, got {other:?}"),
            };

            assert!(matches!(
                next_update(&mut event_rx).await.update,
                v2::SessionUpdate::StateUpdate(v2::StateUpdate::Idle(idle))
                    if idle.stop_reason == Some(v2::StopReason::EndTurn)
            ));

            let sessions = connection
                .send_request(v2::ListSessionsRequest::new())
                .block_task()
                .await?;
            assert_eq!(sessions.sessions.len(), 1);
            assert_eq!(&sessions.sessions[0].session_id, session.session_id());

            session.close().block_task().await?;
            let closed_prompt = session
                .send_prompt(TestyCommand::Greet.to_prompt())
                .block_task()
                .await
                .expect_err("closed v2 sessions should reject prompts");
            assert_eq!(
                closed_prompt.code,
                agent_client_protocol::ErrorCode::RESOURCE_NOT_FOUND
            );
            assert_eq!(
                closed_prompt.data,
                Some(serde_json::json!({ "sessionId": session.session_id() }))
            );
            // Closing is idempotent: closing an inactive session succeeds.
            connection
                .send_request(v2::CloseSessionRequest::new(session.session_id().clone()))
                .block_task()
                .await?;

            let session_id = session.session_id().clone();
            let resume = v2::ResumeSessionRequest::new(session_id.clone(), PathBuf::from("/tmp"))
                .replay_from(v2::ReplayFrom::from(v2::ReplayFromStart::new()));
            connection
                .resume_session_from(resume)
                .start_session()
                .on_receiving_result({
                    let event_tx = event_tx.clone();
                    let expected_session_id = session_id.clone();
                    async move |result| {
                        let resumed = result?;
                        assert_eq!(resumed.session.session_id(), &expected_session_id);
                        event_tx
                            .send(TestEvent::ResumeResponded)
                            .map_err(Error::into_internal_error)
                    }
                })?;

            let replayed_user = next_update(&mut event_rx).await;
            assert_eq!(replayed_user.session_id, session_id);
            assert!(matches!(
                replayed_user.update,
                v2::SessionUpdate::UserMessage(message)
                    if message.message_id == user_message_id
                        && only_text(&message.content)
                            .is_some_and(|text| text.contains("\"echo\""))
            ));

            let replayed_agent = next_update(&mut event_rx).await;
            assert_eq!(replayed_agent.session_id, session_id);
            assert!(matches!(
                replayed_agent.update,
                v2::SessionUpdate::AgentMessage(message)
                    if message.message_id == agent_message_id
                        && only_text(&message.content) == Some("hello from v2")
            ));
            // `replay_complete` follows the last replayed update and precedes
            // current-state synchronization and the response.
            let replay_complete = next_update(&mut event_rx).await;
            assert_eq!(replay_complete.session_id, session_id);
            assert!(matches!(
                replay_complete.update,
                v2::SessionUpdate::ReplayComplete(_)
            ));
            assert_session_info_snapshot(next_update(&mut event_rx).await);
            assert!(matches!(
                next_update(&mut event_rx).await.update,
                v2::SessionUpdate::StateUpdate(v2::StateUpdate::Idle(idle))
                    if idle.stop_reason.is_none()
            ));
            assert!(matches!(
                next_event(&mut event_rx).await,
                TestEvent::ResumeResponded
            ));

            connection
                .send_request(v2::CloseSessionRequest::new(session_id.clone()))
                .block_task()
                .await?;

            connection
                .resume_session(session_id.clone(), PathBuf::from("/tmp"))
                .start_session()
                .on_receiving_result({
                    let event_tx = event_tx.clone();
                    async move |result| {
                        result?;
                        event_tx
                            .send(TestEvent::ResumeResponded)
                            .map_err(Error::into_internal_error)
                    }
                })?;
            // Without `replayFrom` there is no replay, so no `replay_complete`:
            // the first update is the session info snapshot.
            assert_session_info_snapshot(next_update(&mut event_rx).await);
            assert!(matches!(
                next_update(&mut event_rx).await.update,
                v2::SessionUpdate::StateUpdate(v2::StateUpdate::Idle(idle))
                    if idle.stop_reason.is_none()
            ));
            assert!(
                matches!(next_event(&mut event_rx).await, TestEvent::ResumeResponded),
                "resume without replayFrom must synchronize session info and state before responding"
            );

            connection
                .send_request(v2::CloseSessionRequest::new(session_id))
                .block_task()
                .await?;
            Ok(())
        });

    tokio::time::timeout(TIMEOUT, client)
        .await
        .expect("Testy v2 lifecycle timed out")
        .expect("Testy v2 lifecycle failed");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn testy_v2_marks_the_end_of_an_empty_replay() {
    let (event_tx, mut event_rx) = unbounded_channel();
    let update_tx = event_tx.clone();
    let client = Client
        .v2()
        .on_receive_notification(
            async move |update: v2::UpdateSessionNotification,
                        _connection: ConnectionTo<agent_client_protocol::Agent>| {
                update_tx
                    .send(TestEvent::Update(Box::new(update)))
                    .map_err(Error::into_internal_error)
            },
            agent_client_protocol::on_receive_notification!(),
        )
        .connect_with(testy::v2::Testy::new(), async move |connection| {
            connection
                .send_request(v2::InitializeRequest::new(
                    ProtocolVersion::V2,
                    implementation(),
                ))
                .block_task()
                .await?;

            let session_id = connection
                .build_session(PathBuf::from("/tmp"))
                .start_session()
                .block_task()
                .await?
                .session
                .session_id()
                .clone();
            connection
                .send_request(v2::CloseSessionRequest::new(session_id.clone()))
                .block_task()
                .await?;

            let resume = v2::ResumeSessionRequest::new(session_id.clone(), PathBuf::from("/tmp"))
                .replay_from(v2::ReplayFrom::from(v2::ReplayFromStart::new()));
            connection
                .resume_session_from(resume)
                .start_session()
                .on_receiving_result({
                    let event_tx = event_tx.clone();
                    async move |result| {
                        result?;
                        event_tx
                            .send(TestEvent::ResumeResponded)
                            .map_err(Error::into_internal_error)
                    }
                })?;

            // The session has no history. `replay_complete` is still sent, as
            // the first update for the session after the request.
            let replay_complete = next_update(&mut event_rx).await;
            assert_eq!(replay_complete.session_id, session_id);
            assert!(matches!(
                replay_complete.update,
                v2::SessionUpdate::ReplayComplete(_)
            ));
            assert_session_info_snapshot(next_update(&mut event_rx).await);
            assert!(matches!(
                next_update(&mut event_rx).await.update,
                v2::SessionUpdate::StateUpdate(v2::StateUpdate::Idle(idle))
                    if idle.stop_reason.is_none()
            ));
            assert!(matches!(
                next_event(&mut event_rx).await,
                TestEvent::ResumeResponded
            ));

            connection
                .send_request(v2::CloseSessionRequest::new(session_id))
                .block_task()
                .await?;
            Ok(())
        });

    tokio::time::timeout(TIMEOUT, client)
        .await
        .expect("Testy v2 empty replay timed out")
        .expect("Testy v2 empty replay failed");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn testy_v2_prompt_acceptance_is_independent_from_cancellation_completion() {
    let (event_tx, mut event_rx) = unbounded_channel();
    let update_tx = event_tx.clone();
    let client = Client
        .v2()
        .on_receive_notification(
            async move |update: v2::UpdateSessionNotification,
                        _connection: ConnectionTo<agent_client_protocol::Agent>| {
                update_tx
                    .send(TestEvent::Update(Box::new(update)))
                    .map_err(Error::into_internal_error)
            },
            agent_client_protocol::on_receive_notification!(),
        )
        .connect_with(testy::v2::Testy::new(), async move |connection| {
            connection
                .send_request(v2::InitializeRequest::new(
                    ProtocolVersion::V2,
                    implementation(),
                ))
                .block_task()
                .await?;

            let session = connection
                .build_session(PathBuf::from("/tmp"))
                .start_session()
                .block_task()
                .await?
                .session;

            session
                .send_prompt(wait_for_cancel_prompt())
                .on_receiving_result({
                    let event_tx = event_tx.clone();
                    async move |result| {
                        assert_eq!(result?, v2::PromptResponse::new("testy-v2-user-message-0"));
                        event_tx
                            .send(TestEvent::PromptAccepted)
                            .map_err(Error::into_internal_error)
                    }
                })?;
            assert!(
                matches!(next_event(&mut event_rx).await, TestEvent::PromptAccepted),
                "prompt acceptance must precede foreground updates and completion"
            );

            assert!(matches!(
                next_update(&mut event_rx).await.update,
                v2::SessionUpdate::UserMessage(_)
            ));
            assert!(matches!(
                next_update(&mut event_rx).await.update,
                v2::SessionUpdate::StateUpdate(v2::StateUpdate::Running(_))
            ));

            session.cancel_active_work()?;
            assert!(matches!(
                next_update(&mut event_rx).await.update,
                v2::SessionUpdate::StateUpdate(v2::StateUpdate::Idle(idle))
                    if idle.stop_reason == Some(v2::StopReason::Cancelled)
            ));

            session.close().block_task().await?;
            Ok(())
        });

    tokio::time::timeout(TIMEOUT, client)
        .await
        .expect("Testy v2 cancellation timed out")
        .expect("Testy v2 cancellation failed");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn testy_v2_close_cancels_active_work_before_responding() {
    let (event_tx, mut event_rx) = unbounded_channel();
    let update_tx = event_tx.clone();
    let client = Client
        .v2()
        .on_receive_notification(
            async move |update: v2::UpdateSessionNotification,
                        _connection: ConnectionTo<agent_client_protocol::Agent>| {
                update_tx
                    .send(TestEvent::Update(Box::new(update)))
                    .map_err(Error::into_internal_error)
            },
            agent_client_protocol::on_receive_notification!(),
        )
        .connect_with(testy::v2::Testy::new(), async move |connection| {
            connection
                .send_request(v2::InitializeRequest::new(
                    ProtocolVersion::V2,
                    implementation(),
                ))
                .block_task()
                .await?;

            let session_id = connection
                .send_request(v2::NewSessionRequest::new(PathBuf::from("/tmp")))
                .block_task()
                .await?
                .session_id;
            connection
                .send_request(v2::PromptRequest::new(
                    session_id.clone(),
                    vec![wait_for_cancel_prompt().into()],
                ))
                .on_receiving_result({
                    let event_tx = event_tx.clone();
                    async move |result| {
                        assert_eq!(result?, v2::PromptResponse::new("testy-v2-user-message-0"));
                        event_tx
                            .send(TestEvent::PromptAccepted)
                            .map_err(Error::into_internal_error)
                    }
                })?;

            assert!(matches!(
                next_event(&mut event_rx).await,
                TestEvent::PromptAccepted
            ));
            assert!(matches!(
                next_update(&mut event_rx).await.update,
                v2::SessionUpdate::UserMessage(_)
            ));
            assert!(matches!(
                next_update(&mut event_rx).await.update,
                v2::SessionUpdate::StateUpdate(v2::StateUpdate::Running(_))
            ));

            connection
                .send_request(v2::CloseSessionRequest::new(session_id.clone()))
                .on_receiving_result({
                    let event_tx = event_tx.clone();
                    async move |result| {
                        assert_eq!(result?, v2::CloseSessionResponse::new());
                        event_tx
                            .send(TestEvent::CloseResponded)
                            .map_err(Error::into_internal_error)
                    }
                })?;

            assert!(matches!(
                next_update(&mut event_rx).await.update,
                v2::SessionUpdate::StateUpdate(v2::StateUpdate::Idle(idle))
                    if idle.stop_reason == Some(v2::StopReason::Cancelled)
            ));
            assert!(matches!(
                next_event(&mut event_rx).await,
                TestEvent::CloseResponded
            ));

            let prompt_error = connection
                .send_request(v2::PromptRequest::new(
                    session_id,
                    vec![TestyCommand::Greet.to_prompt().into()],
                ))
                .block_task()
                .await
                .expect_err("close should leave the session inactive");
            assert_eq!(
                prompt_error.code,
                agent_client_protocol::ErrorCode::RESOURCE_NOT_FOUND
            );
            Ok(())
        });

    tokio::time::timeout(TIMEOUT, client)
        .await
        .expect("Testy v2 active close timed out")
        .expect("Testy v2 active close failed");
}

#[tokio::test(flavor = "current_thread")]
async fn testy_router_keeps_the_v1_implementation_available() {
    Client
        .v1()
        .connect_with(testy::protocol_router(), async |connection| {
            let initialize = connection
                .send_request(v1::InitializeRequest::new(ProtocolVersion::V1))
                .block_task()
                .await?;
            assert_eq!(initialize.protocol_version, ProtocolVersion::V1);

            let session = connection
                .send_request(v1::NewSessionRequest::new(PathBuf::from("/tmp")))
                .block_task()
                .await?;
            let prompt = connection
                .send_request(v1::PromptRequest::new(
                    session.session_id,
                    vec![TestyCommand::Greet.to_prompt().into()],
                ))
                .block_task()
                .await?;
            assert_eq!(prompt.stop_reason, v1::StopReason::EndTurn);
            Ok(())
        })
        .await
        .expect("Testy protocol router should retain v1 support");
}

#[tokio::test(flavor = "current_thread")]
async fn testy_v2_rejects_unknown_sessions_unadvertised_mcp_servers_and_list_cursors() {
    Client
        .v2()
        .connect_with(testy::v2::Testy::new(), async |connection| {
            let initialize = connection
                .send_request(v2::InitializeRequest::new(
                    ProtocolVersion::V2,
                    implementation(),
                ))
                .block_task()
                .await?;
            assert!(
                initialize
                    .capabilities
                    .session
                    .as_ref()
                    .is_some_and(|session| session.mcp.is_none()),
                "Testy v2 does not advertise MCP transports"
            );

            let unknown = v2::SessionId::new("testy-v2-session-unknown");
            let prompt_error = connection
                .send_request(v2::PromptRequest::new(
                    unknown.clone(),
                    vec![TestyCommand::Greet.to_prompt().into()],
                ))
                .block_task()
                .await
                .expect_err("prompting an unknown v2 session should fail");
            assert_eq!(
                prompt_error.code,
                agent_client_protocol::ErrorCode::RESOURCE_NOT_FOUND
            );
            assert_eq!(
                prompt_error.data,
                Some(serde_json::json!({ "sessionId": "testy-v2-session-unknown" }))
            );

            // Closing is idempotent: closing an unknown session succeeds.
            connection
                .send_request(v2::CloseSessionRequest::new(unknown.clone()))
                .block_task()
                .await?;

            for (method, error) in [
                (
                    "session/new",
                    connection
                        .send_request(v2::NewSessionRequest::new(PathBuf::from("relative")))
                        .block_task()
                        .await,
                ),
                (
                    "session/list",
                    connection
                        .send_request(v2::ListSessionsRequest::new().cwd(PathBuf::from("relative")))
                        .block_task()
                        .await
                        .map(|_| v2::NewSessionResponse::new("unused")),
                ),
            ] {
                let error = error.expect_err("a relative cwd must be rejected");
                assert_eq!(
                    error.code,
                    agent_client_protocol::ErrorCode::INVALID_PARAMS,
                    "{method}"
                );
            }

            let resume_error = connection
                .send_request(v2::ResumeSessionRequest::new(
                    unknown,
                    PathBuf::from("/tmp"),
                ))
                .block_task()
                .await
                .expect_err("resuming an unknown v2 session should fail");
            assert_eq!(
                resume_error.code,
                agent_client_protocol::ErrorCode::RESOURCE_NOT_FOUND
            );

            let mcp_server = v2::McpServer::Http(v2::McpServerHttp::new(
                "unadvertised",
                "http://localhost:1/mcp",
            ));
            let new_session_error = connection
                .send_request(
                    v2::NewSessionRequest::new(PathBuf::from("/tmp")).mcp_servers(vec![mcp_server]),
                )
                .block_task()
                .await
                .expect_err("unadvertised MCP transports should be rejected");
            assert_eq!(
                new_session_error.code,
                agent_client_protocol::ErrorCode::INVALID_PARAMS
            );
            assert!(
                connection
                    .send_request(v2::ListSessionsRequest::new())
                    .block_task()
                    .await?
                    .sessions
                    .is_empty(),
                "a rejected session/new must not create a session"
            );

            let cursor_error = connection
                .send_request(v2::ListSessionsRequest::new().cursor("never-issued"))
                .block_task()
                .await
                .expect_err("Testy v2 never issues a nextCursor, so every cursor is invalid");
            assert_eq!(
                cursor_error.code,
                agent_client_protocol::ErrorCode::INVALID_PARAMS
            );
            Ok(())
        })
        .await
        .expect("Testy v2 error handling failed");
}

#[tokio::test(flavor = "current_thread")]
async fn testy_v2_rejects_unadvertised_prompt_content() {
    Client
        .v2()
        .connect_with(testy::v2::Testy::new(), async |connection| {
            let initialize = connection
                .send_request(v2::InitializeRequest::new(
                    ProtocolVersion::V2,
                    implementation(),
                ))
                .block_task()
                .await?;
            assert!(
                initialize
                    .capabilities
                    .session
                    .as_ref()
                    .is_some_and(|session| session.prompt.is_none()),
                "Testy v2 advertises no prompt capabilities"
            );
            let session_id = connection
                .send_request(v2::NewSessionRequest::new(PathBuf::from("/tmp")))
                .block_task()
                .await?
                .session_id;

            let error = connection
                .send_request(v2::PromptRequest::new(
                    session_id.clone(),
                    vec![
                        TestyCommand::Greet.to_prompt().into(),
                        v2::ContentBlock::Image(v2::ImageContent::new("", "image/png")),
                    ],
                ))
                .block_task()
                .await
                .expect_err("an unadvertised image block should be rejected");
            assert_eq!(error.code, agent_client_protocol::ErrorCode::INVALID_PARAMS);

            connection
                .send_request(v2::PromptRequest::new(
                    session_id,
                    vec![
                        TestyCommand::Greet.to_prompt().into(),
                        v2::ContentBlock::ResourceLink(v2::ResourceLink::new(
                            "notes",
                            "file:///tmp/notes.md",
                        )),
                    ],
                ))
                .block_task()
                .await
                .expect("baseline content is accepted after a rejected prompt");
            Ok(())
        })
        .await
        .expect("Testy v2 prompt content validation failed");
}

#[tokio::test(flavor = "current_thread")]
async fn testy_v2_rejects_empty_prompt_without_inserting_it() {
    let (event_tx, mut event_rx) = unbounded_channel();
    let update_tx = event_tx.clone();
    Client
        .v2()
        .on_receive_notification(
            async move |update: v2::UpdateSessionNotification,
                        _connection: ConnectionTo<agent_client_protocol::Agent>| {
                update_tx
                    .send(TestEvent::Update(Box::new(update)))
                    .map_err(Error::into_internal_error)
            },
            agent_client_protocol::on_receive_notification!(),
        )
        .connect_with(testy::v2::Testy::new(), async move |connection| {
            connection
                .send_request(v2::InitializeRequest::new(
                    ProtocolVersion::V2,
                    implementation(),
                ))
                .block_task()
                .await?;
            let session_id = connection
                .send_request(v2::NewSessionRequest::new(PathBuf::from("/tmp")))
                .block_task()
                .await?
                .session_id;

            let error = connection
                .send_request(v2::PromptRequest::new(session_id.clone(), vec![]))
                .block_task()
                .await
                .expect_err("an empty prompt should be rejected");
            assert_eq!(error.code, agent_client_protocol::ErrorCode::INVALID_PARAMS);

            let accepted = connection
                .send_request(v2::PromptRequest::new(
                    session_id,
                    vec![TestyCommand::Greet.to_prompt().into()],
                ))
                .block_task()
                .await
                .expect("the session accepts a prompt after an empty one is rejected");

            // The first user message belongs to the accepted prompt, so the
            // empty prompt inserted nothing.
            loop {
                match next_update(&mut event_rx).await.update {
                    v2::SessionUpdate::UserMessage(message) => {
                        assert_eq!(message.message_id, accepted.message_id);
                        break;
                    }
                    v2::SessionUpdate::SessionInfoUpdate(_) => {}
                    other => panic!("expected the accepted user message, got {other:?}"),
                }
            }
            Ok(())
        })
        .await
        .expect("Testy v2 empty prompt validation failed");
}

#[tokio::test(flavor = "current_thread")]
async fn testy_v2_unimplemented_scenarios_end_with_error_stop_reason() {
    let (event_tx, mut event_rx) = unbounded_channel();
    Client
        .v2()
        .on_receive_notification(
            async move |update: v2::UpdateSessionNotification,
                        _connection: ConnectionTo<agent_client_protocol::Agent>| {
                event_tx
                    .send(TestEvent::Update(Box::new(update)))
                    .map_err(Error::into_internal_error)
            },
            agent_client_protocol::on_receive_notification!(),
        )
        .connect_with(testy::v2::Testy::new(), async move |connection| {
            connection
                .send_request(v2::InitializeRequest::new(
                    ProtocolVersion::V2,
                    implementation(),
                ))
                .block_task()
                .await?;
            let session_id = connection
                .send_request(v2::NewSessionRequest::new(PathBuf::from("/tmp")))
                .block_task()
                .await?
                .session_id;
            connection
                .send_request(v2::PromptRequest::new(
                    session_id,
                    vec![
                        TestyCommand::RunScenario {
                            scenario: TestyScenario::Full,
                        }
                        .to_prompt()
                        .into(),
                    ],
                ))
                .block_task()
                .await?;

            loop {
                if let v2::SessionUpdate::StateUpdate(v2::StateUpdate::Idle(idle)) =
                    next_update(&mut event_rx).await.update
                {
                    assert!(
                        matches!(idle.stop_reason, Some(v2::StopReason::Error(_))),
                        "unimplemented scenarios should not be reported as refusals: {:?}",
                        idle.stop_reason
                    );
                    break;
                }
            }
            Ok(())
        })
        .await
        .expect("Testy v2 unimplemented scenario failed");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn testy_v2_holds_prompts_during_active_work() {
    let client = Client
        .v2()
        .connect_with(testy::v2::Testy::new(), async move |connection| {
            connection
                .send_request(v2::InitializeRequest::new(
                    ProtocolVersion::V2,
                    implementation(),
                ))
                .block_task()
                .await?;
            let session = connection
                .build_session(PathBuf::from("/tmp"))
                .start_session()
                .block_task()
                .await?
                .session;

            let running = session
                .send_prompt(wait_for_cancel_prompt())
                .block_task()
                .await?;

            // Prompts received during foreground work are held, not rejected.
            let withdrawn = session.send_prompt(TestyCommand::Greet.to_prompt());
            let held = session.send_prompt(TestyCommand::Greet.to_prompt());

            // Testy honors `$/cancel_request` for a held prompt. That is best
            // effort: an agent may insert the prompt instead.
            withdrawn.cancel()?;
            let error = withdrawn
                .block_task()
                .await
                .expect_err("a withdrawn held prompt is never inserted");
            assert_eq!(
                error.code,
                agent_client_protocol::ErrorCode::REQUEST_CANCELLED
            );

            // `session/cancel` answers held prompts received before it with
            // -32800.
            session.cancel_active_work()?;
            let error = held
                .block_task()
                .await
                .expect_err("session/cancel cancels held prompts");
            assert_eq!(
                error.code,
                agent_client_protocol::ErrorCode::REQUEST_CANCELLED
            );

            // A prompt received after `session/cancel` is not cancelled by it.
            // It is held until the cancelled work ends, then inserted.
            let after_cancel = session
                .send_prompt(TestyCommand::Greet.to_prompt())
                .block_task()
                .await?;
            assert_ne!(after_cancel.message_id, running.message_id);

            // Back-to-back prompts are each inserted, in received order.
            let first = session.send_prompt(TestyCommand::Greet.to_prompt());
            let second = session.send_prompt(TestyCommand::Greet.to_prompt());
            let first = first.block_task().await?;
            let second = second.block_task().await?;
            assert_ne!(first.message_id, second.message_id);
            Ok(())
        });

    tokio::time::timeout(TIMEOUT, client)
        .await
        .expect("Testy v2 held prompts timed out")
        .expect("Testy v2 held prompts failed");
}

/// Collect the IDs of the user messages reported for `session_id` until the
/// session reports idle after foreground work.
async fn user_messages_until_idle(
    events: &mut UnboundedReceiver<TestEvent>,
    session_id: &v2::SessionId,
) -> Vec<v2::MessageId> {
    let mut user_messages = Vec::new();
    let mut running = false;
    loop {
        let notification = next_update(events).await;
        if &notification.session_id != session_id {
            continue;
        }
        match notification.update {
            v2::SessionUpdate::UserMessage(message) => user_messages.push(message.message_id),
            v2::SessionUpdate::StateUpdate(v2::StateUpdate::Running(_)) => running = true,
            v2::SessionUpdate::StateUpdate(v2::StateUpdate::Idle(_)) if running => {
                return user_messages;
            }
            _ => {}
        }
    }
}

/// An agent that advertises `capabilities.session` supports `idempotencyKey`
/// on `session/prompt`: it inserts at most one user message for a key in a
/// session and answers a retry with the original `messageId`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn testy_v2_answers_a_prompt_retry_with_the_original_message_id() {
    let (event_tx, mut event_rx) = unbounded_channel();
    let client = Client
        .v2()
        .on_receive_notification(
            async move |update: v2::UpdateSessionNotification,
                        _connection: ConnectionTo<agent_client_protocol::Agent>| {
                event_tx
                    .send(TestEvent::Update(Box::new(update)))
                    .map_err(Error::into_internal_error)
            },
            agent_client_protocol::on_receive_notification!(),
        )
        .connect_with(testy::v2::Testy::new(), async move |connection| {
            connection
                .send_request(v2::InitializeRequest::new(
                    ProtocolVersion::V2,
                    implementation(),
                ))
                .block_task()
                .await?;
            let session = connection
                .build_session(PathBuf::from("/tmp"))
                .start_session()
                .block_task()
                .await?
                .session;
            let session_id = session.session_id().clone();
            let greet = || vec![v2::ContentBlock::from(TestyCommand::Greet.to_prompt())];

            let key = v2::IdempotencyKey::generate();
            let first = session
                .send_prompt_blocks_with_key(greet(), key.clone())
                .block_task()
                .await?;
            assert_eq!(
                user_messages_until_idle(&mut event_rx, &session_id).await,
                std::slice::from_ref(&first.message_id)
            );

            // A retry gets the original `messageId` and nothing else.
            for _ in 0..2 {
                let retry = session
                    .send_prompt_blocks_with_key(greet(), key.clone())
                    .block_task()
                    .await?;
                assert_eq!(retry.message_id, first.message_id);
            }

            // A reused key never inserts a second message, also when the
            // prompt differs. Testy does not compare prompts.
            let reused = session
                .send_prompt_blocks_with_key(
                    vec![v2::ContentBlock::from(
                        TestyCommand::Echo {
                            message: "another prompt".to_string(),
                        }
                        .to_prompt(),
                    )],
                    key.clone(),
                )
                .block_task()
                .await?;
            assert_eq!(reused.message_id, first.message_id);

            // The same text with a new key is a new submission. The retries
            // inserted no message and started no work, so its user message is
            // the next one reported.
            let second = session
                .send_prompt_blocks_with_key(greet(), v2::IdempotencyKey::generate())
                .block_task()
                .await?;
            assert_ne!(second.message_id, first.message_id);
            assert_eq!(
                user_messages_until_idle(&mut event_rx, &session_id).await,
                std::slice::from_ref(&second.message_id)
            );

            // A prompt without a key is never deduplicated.
            let unkeyed = session
                .send_prompt_blocks(greet())
                .block_task()
                .await?
                .message_id;
            assert_ne!(unkeyed, first.message_id);
            assert_ne!(unkeyed, second.message_id);
            assert_eq!(
                user_messages_until_idle(&mut event_rx, &session_id).await,
                [unkeyed]
            );

            // The key lasts as long as the message. After the session is
            // closed and resumed, a retry still gets the original ID.
            session.close().block_task().await?;
            let error = session
                .send_prompt_blocks_with_key(greet(), key.clone())
                .block_task()
                .await
                .expect_err("a retry needs an active session");
            assert_eq!(
                error.code,
                agent_client_protocol::ErrorCode::RESOURCE_NOT_FOUND
            );
            let resumed = connection
                .resume_session(session_id.clone(), PathBuf::from("/tmp"))
                .start_session()
                .block_task()
                .await?
                .session;
            let retry = resumed
                .send_prompt_blocks_with_key(greet(), key)
                .block_task()
                .await?;
            assert_eq!(retry.message_id, first.message_id);
            Ok(())
        });

    tokio::time::timeout(TIMEOUT, client)
        .await
        .expect("Testy v2 prompt retry timed out")
        .expect("Testy v2 prompt retry failed");
}

/// A retry of a prompt that is not inserted yet is not held a second time. It
/// is answered when the first request is, with the same outcome. A dropped
/// prompt leaves its key unused.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn testy_v2_answers_a_retry_of_a_held_prompt_with_its_outcome() {
    let client = Client
        .v2()
        .connect_with(testy::v2::Testy::new(), async move |connection| {
            connection
                .send_request(v2::InitializeRequest::new(
                    ProtocolVersion::V2,
                    implementation(),
                ))
                .block_task()
                .await?;
            let session = connection
                .build_session(PathBuf::from("/tmp"))
                .start_session()
                .block_task()
                .await?
                .session;
            let greet = || vec![v2::ContentBlock::from(TestyCommand::Greet.to_prompt())];

            let running = session
                .send_prompt(wait_for_cancel_prompt())
                .block_task()
                .await?;

            // `$/cancel_request` for the held prompt withdraws it, and every
            // pending request with its key gets `-32800`, even if the
            // cancellation names a retry rather than the original request.
            let key = v2::IdempotencyKey::generate();
            for cancel_retry in [false, true] {
                let held = session.send_prompt_blocks_with_key(greet(), key.clone());
                let retry = session.send_prompt_blocks_with_key(greet(), key.clone());
                if cancel_retry {
                    retry.cancel()?;
                } else {
                    held.cancel()?;
                }
                for request in [held, retry] {
                    let error = request
                        .block_task()
                        .await
                        .expect_err("a withdrawn prompt is never inserted");
                    assert_eq!(
                        error.code,
                        agent_client_protocol::ErrorCode::REQUEST_CANCELLED
                    );
                }
            }

            // `session/cancel` drops a held prompt and its pending retry.
            let held = session.send_prompt_blocks_with_key(greet(), key.clone());
            let retry = session.send_prompt_blocks_with_key(greet(), key.clone());
            session.cancel_active_work()?;
            for request in [held, retry] {
                let error = request
                    .block_task()
                    .await
                    .expect_err("session/cancel drops prompts that are not inserted");
                assert_eq!(
                    error.code,
                    agent_client_protocol::ErrorCode::REQUEST_CANCELLED
                );
            }

            // An error leaves the key unused, so the same key now inserts the
            // prompt. A retry sent right behind it gets the same message,
            // whether or not the first request is still held.
            let inserted = session.send_prompt_blocks_with_key(greet(), key.clone());
            let retry = session.send_prompt_blocks_with_key(greet(), key);
            let inserted = inserted.block_task().await?;
            let retry = retry.block_task().await?;
            assert_ne!(inserted.message_id, running.message_id);
            assert_eq!(retry.message_id, inserted.message_id);
            Ok(())
        });

    tokio::time::timeout(TIMEOUT, client)
        .await
        .expect("Testy v2 held prompt retry timed out")
        .expect("Testy v2 held prompt retry failed");
}

/// A malformed `idempotencyKey` fails the request with `-32602`. It is never
/// treated as an omitted key.
#[tokio::test(flavor = "current_thread")]
async fn testy_v2_rejects_a_malformed_idempotency_key() {
    tokio::time::timeout(
        TIMEOUT,
        Client
            .v2()
            .connect_with(testy::v2::Testy::new(), async |connection| {
                connection
                    .send_request(v2::InitializeRequest::new(
                        ProtocolVersion::V2,
                        implementation(),
                    ))
                    .block_task()
                    .await?;
                let session = connection
                    .build_session(PathBuf::from("/tmp"))
                    .start_session()
                    .block_task()
                    .await?
                    .session;

                let too_long = "k".repeat(v2::IdempotencyKey::MAX_LENGTH + 1);
                for key in [
                    serde_json::json!(""),
                    serde_json::json!(too_long),
                    serde_json::json!(7),
                ] {
                    let error = connection
                        .send_request(agent_client_protocol::UntypedMessage::new(
                            "session/prompt",
                            serde_json::json!({
                                "sessionId": session.session_id(),
                                "idempotencyKey": key,
                                "prompt": [{ "type": "text", "text": "greet" }]
                            }),
                        )?)
                        .block_task()
                        .await
                        .expect_err("a malformed key must fail the request");
                    assert_eq!(i32::from(error.code), -32602, "{key}: {error:?}");
                }

                // Nothing was inserted: the next prompt gets the session's
                // first user message ID.
                let accepted = session
                    .send_prompt(TestyCommand::Greet.to_prompt())
                    .block_task()
                    .await?;
                assert_eq!(accepted, v2::PromptResponse::new("testy-v2-user-message-0"));
                Ok(())
            }),
    )
    .await
    .expect("Testy v2 malformed idempotency key timed out")
    .expect("Testy v2 malformed idempotency key failed");
}

#[tokio::test(flavor = "current_thread")]
async fn testy_v2_rejects_an_unsupported_system_prompt_without_opening_a_session() {
    let client = Client
        .v2()
        .connect_with(testy::v2::Testy::new(), async |connection| {
            connection
                .send_request(v2::InitializeRequest::new(
                    ProtocolVersion::V2,
                    implementation(),
                ))
                .block_task()
                .await?;
            let cwd = PathBuf::from("/tmp");
            let system_prompt = v2::SystemPrompt::new(vec!["Use short replies.".into()]);
            let new_error = connection
                .send_request(
                    v2::NewSessionRequest::new(cwd.clone()).system_prompt(system_prompt.clone()),
                )
                .block_task()
                .await
                .expect_err("an unsupported system prompt must not be ignored");
            assert_eq!(
                new_error.code,
                agent_client_protocol::ErrorCode::INVALID_PARAMS
            );
            let sessions = connection
                .send_request(v2::ListSessionsRequest::new())
                .block_task()
                .await?;
            assert!(sessions.sessions.is_empty());

            let session = connection
                .build_session(cwd.clone())
                .start_session()
                .block_task()
                .await?
                .session;
            let session_id = session.session_id().clone();
            session.close().block_task().await?;
            let resume_error = connection
                .send_request(
                    v2::ResumeSessionRequest::new(session_id.clone(), cwd)
                        .system_prompt(system_prompt),
                )
                .block_task()
                .await
                .expect_err("an unsupported system prompt must not resume a session");
            assert_eq!(
                resume_error.code,
                agent_client_protocol::ErrorCode::INVALID_PARAMS
            );
            let prompt_error = connection
                .send_request(v2::PromptRequest::new(session_id, vec!["hello".into()]))
                .block_task()
                .await
                .expect_err("the session stays closed");
            assert_eq!(
                prompt_error.code,
                agent_client_protocol::ErrorCode::RESOURCE_NOT_FOUND
            );
            Ok(())
        });
    tokio::time::timeout(TIMEOUT, client)
        .await
        .expect("Testy v2 system prompt validation timed out")
        .expect("Testy v2 system prompt validation failed");
}

#[tokio::test(flavor = "current_thread")]
async fn testy_v2_does_not_implement_entity_cancel() {
    tokio::time::timeout(
        TIMEOUT,
        Client
            .v2()
            .connect_with(testy::v2::Testy::new(), async |connection| {
                connection
                    .send_request(v2::InitializeRequest::new(
                        ProtocolVersion::V2,
                        implementation(),
                    ))
                    .block_task()
                    .await?;
                let error = connection
                    .send_request(v2::CancelToolCallRequest::new("s", "c"))
                    .block_task()
                    .await
                    .expect_err("Testy has no cancellable tool call");
                assert_eq!(i32::from(error.code), -32601, "{error:?}");
                let error = connection
                    .send_request(v2::CancelTerminalRequest::new("s", "t"))
                    .block_task()
                    .await
                    .expect_err("Testy has no cancellable terminal");
                assert_eq!(i32::from(error.code), -32601, "{error:?}");
                Ok(())
            }),
    )
    .await
    .expect("Testy v2 entity cancel timed out")
    .expect("Testy v2 entity cancel failed");
}
