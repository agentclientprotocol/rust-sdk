#![cfg(feature = "unstable_protocol_v2")]

use std::{path::PathBuf, time::Duration};

use agent_client_protocol::{
    Client, Error, V2ConnectionTo,
    schema::{ProtocolVersion, v1, v2},
};
use agent_client_protocol_test::testy::{Testy, TestyCommand, TestyScenario};
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
                        _connection: V2ConnectionTo<agent_client_protocol::Agent>| {
                update_tx
                    .send(TestEvent::Update(Box::new(update)))
                    .map_err(Error::into_internal_error)
            },
            agent_client_protocol::on_receive_notification!(),
        )
        .connect_with(Testy::new().protocol_router(), async move |connection| {
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
            let (session, new_session_response) = opened.into_parts();
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
                        assert_eq!(result?, v2::PromptResponse::new());
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
                agent_client_protocol::ErrorCode::InvalidParams
            );

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
                        assert_eq!(resumed.session().session_id(), &expected_session_id);
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
            assert!(
                matches!(next_event(&mut event_rx).await, TestEvent::ResumeResponded),
                "resume without replayFrom must respond without replay updates"
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
async fn testy_v2_prompt_acceptance_is_independent_from_cancellation_completion() {
    let (event_tx, mut event_rx) = unbounded_channel();
    let update_tx = event_tx.clone();
    let client = Client
        .v2()
        .on_receive_notification(
            async move |update: v2::UpdateSessionNotification,
                        _connection: V2ConnectionTo<agent_client_protocol::Agent>| {
                update_tx
                    .send(TestEvent::Update(Box::new(update)))
                    .map_err(Error::into_internal_error)
            },
            agent_client_protocol::on_receive_notification!(),
        )
        .connect_with(Testy::new().v2(), async move |connection| {
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
                .into_session();

            session
                .send_prompt(wait_for_cancel_prompt())
                .on_receiving_result({
                    let event_tx = event_tx.clone();
                    async move |result| {
                        assert_eq!(result?, v2::PromptResponse::new());
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
                        _connection: V2ConnectionTo<agent_client_protocol::Agent>| {
                update_tx
                    .send(TestEvent::Update(Box::new(update)))
                    .map_err(Error::into_internal_error)
            },
            agent_client_protocol::on_receive_notification!(),
        )
        .connect_with(Testy::new().v2(), async move |connection| {
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
                        assert_eq!(result?, v2::PromptResponse::new());
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
                agent_client_protocol::ErrorCode::InvalidParams
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
        .builder()
        .connect_with(Testy::new().protocol_router(), async |connection| {
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
