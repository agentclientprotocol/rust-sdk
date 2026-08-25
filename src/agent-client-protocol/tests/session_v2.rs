#![cfg(feature = "unstable_protocol_v2")]

use std::{
    path::PathBuf,
    sync::{Arc, Mutex},
    time::Duration,
};

use agent_client_protocol::{
    Agent, Client, ConnectionTo, Error, ErrorCode, Responder, RunWithConnectionTo, V2ConnectionTo,
    schema::{ProtocolVersion, v2},
};
use futures::{
    StreamExt as _,
    channel::{
        mpsc::{self, UnboundedReceiver},
        oneshot,
    },
};

const TIMEOUT: Duration = Duration::from_secs(10);

fn cwd() -> Result<PathBuf, Error> {
    std::env::current_dir().map_err(Error::into_internal_error)
}

fn implementation() -> v2::Implementation {
    v2::Implementation::new("session-v2-test", env!("CARGO_PKG_VERSION"))
}

fn initialize_response(protocol_version: ProtocolVersion) -> v2::InitializeResponse {
    v2::InitializeResponse::new(protocol_version, implementation())
        .capabilities(v2::AgentCapabilities::new().session(v2::SessionCapabilities::new()))
}

struct V1SessionGuardRunner {
    cwd: PathBuf,
    completed: oneshot::Sender<()>,
}

impl RunWithConnectionTo<Agent> for V1SessionGuardRunner {
    async fn run_with_connection_to(self, connection: ConnectionTo<Agent>) -> Result<(), Error> {
        let error = connection
            .build_session(self.cwd)
            .block_task()
            .start_session()
            .await
            .expect_err("the v1 helper must reject the raw protocol v2 connection");
        assert_eq!(error.code, ErrorCode::InvalidRequest);

        let data = error
            .data
            .as_ref()
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default();
        assert!(data.contains("V2ConnectionTo"), "{error:?}");

        self.completed.send(()).map_err(|()| {
            Error::internal_error().data("v1 session guard test receiver was dropped")
        })
    }
}

async fn next_update(
    updates: &mut UnboundedReceiver<v2::UpdateSessionNotification>,
) -> v2::UpdateSessionNotification {
    updates
        .next()
        .await
        .expect("the typed session update handler should remain active")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn v2_prompt_acceptance_is_independent_from_session_updates() {
    let session_id = v2::SessionId::new("v2-session");
    let config_option = v2::SessionConfigOption::boolean("thinking", "Thinking", true);
    let meta = serde_json::Map::from_iter([(
        "extension".to_owned(),
        serde_json::json!({"preserved": true}),
    )]);
    let expected_new_response = v2::NewSessionResponse::new(session_id.clone())
        .config_options(vec![config_option])
        .meta(meta);
    let agent_new_response = expected_new_response.clone();

    let agent = Agent
        .v2()
        .on_receive_request(
            async |request: v2::InitializeRequest,
                   responder: Responder<v2::InitializeResponse>,
                   _connection: V2ConnectionTo<Client>| {
                assert_eq!(request.protocol_version, ProtocolVersion::V2);
                responder.respond(initialize_response(request.protocol_version))
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            async move |request: v2::NewSessionRequest,
                        responder: Responder<v2::NewSessionResponse>,
                        _connection: V2ConnectionTo<Client>| {
                assert!(AsRef::<std::path::Path>::as_ref(&request.cwd).is_absolute());
                responder.respond(agent_new_response.clone())
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            async move |request: v2::PromptRequest,
                        responder: Responder<v2::PromptResponse>,
                        connection: V2ConnectionTo<Client>| {
                assert_eq!(request.session_id, v2::SessionId::new("v2-session"));
                assert!(matches!(
                    request.prompt.as_slice(),
                    [v2::ContentBlock::Text(text)] if text.text == "hello"
                ));

                // The update lane is independent and can be observed before
                // this prompt's acceptance response.
                connection.send_notification(v2::UpdateSessionNotification::new(
                    request.session_id.clone(),
                    v2::SessionUpdate::AgentMessage(
                        v2::AgentMessage::new(v2::MessageId::new("background"))
                            .content(vec!["unrelated".into()]),
                    ),
                ))?;
                responder.respond(v2::PromptResponse::new())?;
                connection.send_notification(v2::UpdateSessionNotification::new(
                    request.session_id.clone(),
                    v2::SessionUpdate::UserMessage(
                        v2::UserMessage::new(v2::MessageId::new("accepted-user"))
                            .content(request.prompt),
                    ),
                ))?;
                connection.send_notification(v2::UpdateSessionNotification::new(
                    request.session_id.clone(),
                    v2::SessionUpdate::StateUpdate(v2::StateUpdate::Running(
                        v2::RunningStateUpdate::new(),
                    )),
                ))?;
                connection.send_notification(v2::UpdateSessionNotification::new(
                    request.session_id,
                    v2::SessionUpdate::StateUpdate(v2::StateUpdate::Idle(
                        v2::IdleStateUpdate::new().stop_reason(v2::StopReason::MaxTokens),
                    )),
                ))
            },
            agent_client_protocol::on_receive_request!(),
        );

    let (update_tx, mut update_rx) = mpsc::unbounded();
    let client = Client
        .v2()
        .on_receive_notification(
            async move |update: v2::UpdateSessionNotification,
                        _connection: V2ConnectionTo<Agent>| {
                update_tx
                    .unbounded_send(update)
                    .map_err(Error::into_internal_error)
            },
            agent_client_protocol::on_receive_notification!(),
        )
        .connect_with(agent, async move |connection| {
            let initialize = connection
                .send_request(v2::InitializeRequest::new(
                    ProtocolVersion::V2,
                    implementation(),
                ))
                .block_task()
                .await?;
            assert_eq!(initialize.protocol_version, ProtocolVersion::V2);

            let opened = connection
                .build_session(cwd()?)
                .start_session()
                .block_task()
                .await?;
            assert_eq!(opened.response(), &expected_new_response);

            let (session, response) = opened.into_parts();
            assert_eq!(response, expected_new_response);
            assert_eq!(session.session_id(), &session_id);

            let acceptance = session.send_prompt("hello");

            let background = next_update(&mut update_rx).await;
            assert_eq!(background.session_id, session_id);
            assert!(matches!(
                background.update,
                v2::SessionUpdate::AgentMessage(message)
                    if message.message_id == v2::MessageId::new("background")
            ));

            assert_eq!(acceptance.block_task().await?, v2::PromptResponse::new());

            assert!(matches!(
                next_update(&mut update_rx).await.update,
                v2::SessionUpdate::UserMessage(message)
                    if message.message_id == v2::MessageId::new("accepted-user")
            ));
            assert!(matches!(
                next_update(&mut update_rx).await.update,
                v2::SessionUpdate::StateUpdate(v2::StateUpdate::Running(_))
            ));
            assert!(matches!(
                next_update(&mut update_rx).await.update,
                v2::SessionUpdate::StateUpdate(v2::StateUpdate::Idle(idle))
                    if idle.stop_reason == Some(v2::StopReason::MaxTokens)
            ));
            Ok(())
        });

    tokio::time::timeout(TIMEOUT, client)
        .await
        .expect("v2 session lifecycle timed out")
        .expect("v2 session connection failed");
}

#[cfg(feature = "unstable_session_fork")]
#[tokio::test(flavor = "current_thread")]
async fn v2_fork_builder_uses_the_forked_response_id_and_preserves_the_response() {
    let source_session_id = v2::SessionId::new("source-session");
    let forked_session_id = v2::SessionId::new("forked-session");
    let fork_cwd = cwd().expect("test cwd should be available");
    let expected_cwd = v2::AbsolutePath::new(fork_cwd.clone());
    let config_option = v2::SessionConfigOption::boolean("thinking", "Thinking", true);
    let response_meta = serde_json::Map::from_iter([(
        "extension".to_owned(),
        serde_json::json!({"preserved": true}),
    )]);
    let expected_response = v2::ForkSessionResponse::new(forked_session_id.clone())
        .config_options(vec![config_option])
        .meta(response_meta);
    let agent_response = expected_response.clone();
    let agent_source_session_id = source_session_id.clone();

    let agent = Agent
        .v2()
        .on_receive_request(
            async |request: v2::InitializeRequest,
                   responder: Responder<v2::InitializeResponse>,
                   _connection: V2ConnectionTo<Client>| {
                responder.respond(
                    v2::InitializeResponse::new(request.protocol_version, implementation())
                        .capabilities(v2::AgentCapabilities::new().session(
                            v2::SessionCapabilities::new().fork(v2::SessionForkCapabilities::new()),
                        )),
                )
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            async move |request: v2::ForkSessionRequest,
                        responder: Responder<v2::ForkSessionResponse>,
                        _connection: V2ConnectionTo<Client>| {
                assert_eq!(request.session_id, agent_source_session_id);
                assert_eq!(request.cwd, expected_cwd);
                responder.respond(agent_response.clone())
            },
            agent_client_protocol::on_receive_request!(),
        );

    let client = Client.v2().connect_with(agent, async move |connection| {
        connection
            .send_request(v2::InitializeRequest::new(
                ProtocolVersion::V2,
                implementation(),
            ))
            .block_task()
            .await?;

        let opened = connection
            .fork_session(source_session_id.clone(), fork_cwd)
            .start_session()
            .block_task()
            .await?;
        assert_eq!(opened.session().session_id(), &forked_session_id);
        assert_ne!(opened.session().session_id(), &source_session_id);
        assert_eq!(opened.response(), &expected_response);

        let (session, response) = opened.into_parts();
        assert_eq!(session.session_id(), &forked_session_id);
        assert_eq!(response, expected_response);
        Ok(())
    });

    tokio::time::timeout(TIMEOUT, client)
        .await
        .expect("v2 fork builder test timed out")
        .expect("v2 fork builder test failed");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn v2_session_cancellation_completes_at_cancelled_idle() {
    let agent = Agent
        .v2()
        .on_receive_request(
            async |request: v2::InitializeRequest,
                   responder: Responder<v2::InitializeResponse>,
                   _connection: V2ConnectionTo<Client>| {
                responder.respond(initialize_response(request.protocol_version))
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            async |_request: v2::NewSessionRequest,
                   responder: Responder<v2::NewSessionResponse>,
                   _connection: V2ConnectionTo<Client>| {
                responder.respond(v2::NewSessionResponse::new(v2::SessionId::new(
                    "cancel-session",
                )))
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            async |request: v2::PromptRequest,
                   responder: Responder<v2::PromptResponse>,
                   connection: V2ConnectionTo<Client>| {
                responder.respond(v2::PromptResponse::new())?;
                connection.send_notification(v2::UpdateSessionNotification::new(
                    request.session_id.clone(),
                    v2::SessionUpdate::StateUpdate(v2::StateUpdate::Running(
                        v2::RunningStateUpdate::new(),
                    )),
                ))?;
                connection.send_notification(v2::UpdateSessionNotification::new(
                    request.session_id,
                    v2::SessionUpdate::AgentMessageChunk(v2::ContentChunk::new(
                        "before".into(),
                        v2::MessageId::new("cancel-answer"),
                    )),
                ))
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_notification(
            async |cancel: v2::CancelSessionNotification, connection: V2ConnectionTo<Client>| {
                connection.send_notification(v2::UpdateSessionNotification::new(
                    cancel.session_id.clone(),
                    v2::SessionUpdate::AgentMessageChunk(v2::ContentChunk::new(
                        " after".into(),
                        v2::MessageId::new("cancel-answer"),
                    )),
                ))?;
                connection.send_notification(v2::UpdateSessionNotification::new(
                    cancel.session_id,
                    v2::SessionUpdate::StateUpdate(v2::StateUpdate::Idle(
                        v2::IdleStateUpdate::new().stop_reason(v2::StopReason::Cancelled),
                    )),
                ))
            },
            agent_client_protocol::on_receive_notification!(),
        );

    let (update_tx, mut update_rx) = mpsc::unbounded();
    let client = Client
        .v2()
        .on_receive_notification(
            async move |update: v2::UpdateSessionNotification,
                        _connection: V2ConnectionTo<Agent>| {
                update_tx
                    .unbounded_send(update)
                    .map_err(Error::into_internal_error)
            },
            agent_client_protocol::on_receive_notification!(),
        )
        .connect_with(agent, async move |connection| {
            connection
                .send_request(v2::InitializeRequest::new(
                    ProtocolVersion::V2,
                    implementation(),
                ))
                .block_task()
                .await?;

            let opened = connection
                .build_session(cwd()?)
                .start_session()
                .block_task()
                .await?;
            let (session, _) = opened.into_parts();
            session.send_prompt("cancel me").block_task().await?;

            assert!(matches!(
                next_update(&mut update_rx).await.update,
                v2::SessionUpdate::StateUpdate(v2::StateUpdate::Running(_))
            ));
            assert!(matches!(
                next_update(&mut update_rx).await.update,
                v2::SessionUpdate::AgentMessageChunk(_)
            ));

            session.cancel_active_work()?;
            assert!(matches!(
                next_update(&mut update_rx).await.update,
                v2::SessionUpdate::AgentMessageChunk(_)
            ));
            assert!(matches!(
                next_update(&mut update_rx).await.update,
                v2::SessionUpdate::StateUpdate(v2::StateUpdate::Idle(idle))
                    if idle.stop_reason == Some(v2::StopReason::Cancelled)
            ));
            Ok(())
        });

    tokio::time::timeout(TIMEOUT, client)
        .await
        .expect("v2 cancellation lifecycle timed out")
        .expect("v2 cancellation connection failed");
}

#[tokio::test(flavor = "current_thread")]
async fn cloned_v2_session_handles_do_not_own_prompt_state() {
    let agent = Agent
        .v2()
        .on_receive_request(
            async |request: v2::InitializeRequest,
                   responder: Responder<v2::InitializeResponse>,
                   _connection: V2ConnectionTo<Client>| {
                responder.respond(initialize_response(request.protocol_version))
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            async |_request: v2::NewSessionRequest,
                   responder: Responder<v2::NewSessionResponse>,
                   _connection: V2ConnectionTo<Client>| {
                responder.respond(v2::NewSessionResponse::new(v2::SessionId::new(
                    "parallel-prompts",
                )))
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            async |request: v2::PromptRequest,
                   responder: Responder<v2::PromptResponse>,
                   connection: V2ConnectionTo<Client>| {
                assert!(matches!(
                    request.prompt.as_slice(),
                    [v2::ContentBlock::Text(text)]
                        if text.text == "first" || text.text == "second"
                ));
                responder.respond(v2::PromptResponse::new())?;
                connection.send_notification(v2::UpdateSessionNotification::new(
                    request.session_id,
                    v2::SessionUpdate::StateUpdate(v2::StateUpdate::Idle(
                        v2::IdleStateUpdate::new().stop_reason(v2::StopReason::EndTurn),
                    )),
                ))
            },
            agent_client_protocol::on_receive_request!(),
        );

    let (update_tx, mut update_rx) = mpsc::unbounded();
    let client = Client
        .v2()
        .on_receive_notification(
            async move |update: v2::UpdateSessionNotification,
                        _connection: V2ConnectionTo<Agent>| {
                update_tx
                    .unbounded_send(update)
                    .map_err(Error::into_internal_error)
            },
            agent_client_protocol::on_receive_notification!(),
        )
        .connect_with(agent, async move |connection| {
            connection
                .send_request(v2::InitializeRequest::new(
                    ProtocolVersion::V2,
                    implementation(),
                ))
                .block_task()
                .await?;

            let session = connection
                .build_session(cwd()?)
                .start_session()
                .block_task()
                .await?
                .into_session();
            let other_task = session.clone();

            assert_eq!(
                session.send_prompt("first").block_task().await?,
                v2::PromptResponse::new()
            );
            assert!(matches!(
                next_update(&mut update_rx).await.update,
                v2::SessionUpdate::StateUpdate(v2::StateUpdate::Idle(_))
            ));

            assert_eq!(
                other_task.send_prompt("second").block_task().await?,
                v2::PromptResponse::new()
            );
            assert!(matches!(
                next_update(&mut update_rx).await.update,
                v2::SessionUpdate::StateUpdate(v2::StateUpdate::Idle(_))
            ));
            Ok(())
        });

    tokio::time::timeout(TIMEOUT, client)
        .await
        .expect("cloned v2 session handle test timed out")
        .expect("cloned v2 session handle test failed");
}

#[tokio::test(flavor = "current_thread")]
async fn v2_prompt_error_does_not_stop_session_updates() {
    let agent = Agent
        .v2()
        .on_receive_request(
            async |request: v2::InitializeRequest,
                   responder: Responder<v2::InitializeResponse>,
                   _connection: V2ConnectionTo<Client>| {
                responder.respond(initialize_response(request.protocol_version))
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            async |_request: v2::NewSessionRequest,
                   responder: Responder<v2::NewSessionResponse>,
                   _connection: V2ConnectionTo<Client>| {
                responder.respond(v2::NewSessionResponse::new(v2::SessionId::new(
                    "rejected-prompt",
                )))
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            async |request: v2::PromptRequest,
                   responder: Responder<v2::PromptResponse>,
                   connection: V2ConnectionTo<Client>| {
                responder.respond_with_error(Error::invalid_params().data("prompt rejected"))?;
                connection.send_notification(v2::UpdateSessionNotification::new(
                    request.session_id,
                    v2::SessionUpdate::AgentMessage(
                        v2::AgentMessage::new(v2::MessageId::new("unrelated-after-error"))
                            .content(vec!["background".into()]),
                    ),
                ))
            },
            agent_client_protocol::on_receive_request!(),
        );

    let (update_tx, mut update_rx) = mpsc::unbounded();
    let client = Client
        .v2()
        .on_receive_notification(
            async move |update: v2::UpdateSessionNotification,
                        _connection: V2ConnectionTo<Agent>| {
                update_tx
                    .unbounded_send(update)
                    .map_err(Error::into_internal_error)
            },
            agent_client_protocol::on_receive_notification!(),
        )
        .connect_with(agent, async move |connection| {
            connection
                .send_request(v2::InitializeRequest::new(
                    ProtocolVersion::V2,
                    implementation(),
                ))
                .block_task()
                .await?;

            let opened = connection
                .build_session(cwd()?)
                .start_session()
                .block_task()
                .await?;
            let (session, _) = opened.into_parts();

            let error = session
                .send_prompt("reject this")
                .block_task()
                .await
                .expect_err("the prompt error must reach its acceptance request");
            assert_eq!(
                error,
                Error::invalid_params().data("prompt rejected"),
                "the helper must preserve the peer's prompt error"
            );

            assert!(matches!(
                next_update(&mut update_rx).await.update,
                v2::SessionUpdate::AgentMessage(message)
                    if message.message_id == v2::MessageId::new("unrelated-after-error")
            ));
            Ok(())
        });

    tokio::time::timeout(TIMEOUT, client)
        .await
        .expect("v2 prompt rejection test timed out")
        .expect("v2 prompt rejection stopped typed session updates");
}

#[tokio::test(flavor = "current_thread")]
async fn v2_permission_requests_use_a_separate_typed_handler() {
    let agent = Agent
        .v2()
        .on_receive_request(
            async |request: v2::InitializeRequest,
                   responder: Responder<v2::InitializeResponse>,
                   _connection: V2ConnectionTo<Client>| {
                responder.respond(initialize_response(request.protocol_version))
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            async |_request: v2::NewSessionRequest,
                   responder: Responder<v2::NewSessionResponse>,
                   _connection: V2ConnectionTo<Client>| {
                responder.respond(v2::NewSessionResponse::new(v2::SessionId::new(
                    "permission-session",
                )))
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            async |request: v2::PromptRequest,
                   responder: Responder<v2::PromptResponse>,
                   connection: V2ConnectionTo<Client>| {
                responder.respond(v2::PromptResponse::new())?;

                let session_id = request.session_id;
                connection
                    .send_request(v2::RequestPermissionRequest::new(
                        session_id.clone(),
                        "Continue?",
                        vec![v2::PermissionOption::new(
                            "reject",
                            "Reject",
                            v2::PermissionOptionKind::RejectOnce,
                        )],
                    ))
                    .on_receiving_result({
                        let connection = connection.clone();
                        async move |response| {
                            assert_eq!(
                                response?.outcome,
                                v2::RequestPermissionOutcome::Selected(
                                    v2::SelectedPermissionOutcome::new("reject")
                                )
                            );
                            connection.send_notification(v2::UpdateSessionNotification::new(
                                session_id,
                                v2::SessionUpdate::StateUpdate(v2::StateUpdate::Idle(
                                    v2::IdleStateUpdate::new().stop_reason(v2::StopReason::Refusal),
                                )),
                            ))
                        }
                    })
            },
            agent_client_protocol::on_receive_request!(),
        );

    let (update_tx, mut update_rx) = mpsc::unbounded();
    let (permission_tx, mut permission_rx) = mpsc::unbounded::<(
        v2::RequestPermissionRequest,
        Responder<v2::RequestPermissionResponse>,
    )>();
    let client = Client
        .v2()
        .on_receive_notification(
            async move |update: v2::UpdateSessionNotification,
                        _connection: V2ConnectionTo<Agent>| {
                update_tx
                    .unbounded_send(update)
                    .map_err(Error::into_internal_error)
            },
            agent_client_protocol::on_receive_notification!(),
        )
        .on_receive_request(
            async move |request: v2::RequestPermissionRequest,
                        responder: Responder<v2::RequestPermissionResponse>,
                        _connection: V2ConnectionTo<Agent>| {
                permission_tx
                    .unbounded_send((request, responder))
                    .map_err(Error::into_internal_error)
            },
            agent_client_protocol::on_receive_request!(),
        )
        .connect_with(agent, async move |connection| {
            connection
                .send_request(v2::InitializeRequest::new(
                    ProtocolVersion::V2,
                    implementation(),
                ))
                .block_task()
                .await?;

            let opened = connection
                .build_session(cwd()?)
                .start_session()
                .block_task()
                .await?;
            let (session, _) = opened.into_parts();
            session.send_prompt("ask").block_task().await?;

            let (request, responder) = permission_rx
                .next()
                .await
                .expect("permission handler should forward the request");
            assert_eq!(request.session_id, session.session_id().clone());
            assert_eq!(request.title, "Continue?");
            responder.respond(v2::RequestPermissionResponse::new(
                v2::RequestPermissionOutcome::Selected(v2::SelectedPermissionOutcome::new(
                    "reject",
                )),
            ))?;

            assert!(matches!(
                next_update(&mut update_rx).await.update,
                v2::SessionUpdate::StateUpdate(v2::StateUpdate::Idle(idle))
                    if idle.stop_reason == Some(v2::StopReason::Refusal)
            ));
            Ok(())
        });

    tokio::time::timeout(TIMEOUT, client)
        .await
        .expect("v2 typed permission test timed out")
        .expect("v2 typed permission test failed");
}

#[tokio::test(flavor = "current_thread")]
async fn unhandled_v2_session_messages_are_not_deferred() {
    let (permission_result_tx, mut permission_result_rx) =
        mpsc::unbounded::<Result<v2::RequestPermissionResponse, Error>>();
    let agent = Agent
        .v2()
        .on_receive_request(
            async |request: v2::InitializeRequest,
                   responder: Responder<v2::InitializeResponse>,
                   _connection: V2ConnectionTo<Client>| {
                responder.respond(initialize_response(request.protocol_version))
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            async |_request: v2::NewSessionRequest,
                   responder: Responder<v2::NewSessionResponse>,
                   _connection: V2ConnectionTo<Client>| {
                responder.respond(v2::NewSessionResponse::new(v2::SessionId::new(
                    "unhandled-session",
                )))
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            async move |request: v2::PromptRequest,
                        responder: Responder<v2::PromptResponse>,
                        connection: V2ConnectionTo<Client>| {
                responder.respond(v2::PromptResponse::new())?;

                // A v2 client without typed handlers should ignore an
                // unhandled notification and reject an unhandled request,
                // rather than retaining either for a dynamic v1 session route.
                connection.send_notification(v2::UpdateSessionNotification::new(
                    request.session_id.clone(),
                    v2::SessionUpdate::StateUpdate(v2::StateUpdate::Running(
                        v2::RunningStateUpdate::new(),
                    )),
                ))?;

                let permission_result_tx = permission_result_tx.clone();
                connection
                    .send_request(v2::RequestPermissionRequest::new(
                        request.session_id,
                        "Continue?",
                        vec![v2::PermissionOption::new(
                            "reject",
                            "Reject",
                            v2::PermissionOptionKind::RejectOnce,
                        )],
                    ))
                    .on_receiving_result(async move |result| {
                        permission_result_tx
                            .unbounded_send(result)
                            .map_err(Error::into_internal_error)
                    })
            },
            agent_client_protocol::on_receive_request!(),
        );

    let client = Client.v2().connect_with(agent, async move |connection| {
        connection
            .send_request(v2::InitializeRequest::new(
                ProtocolVersion::V2,
                implementation(),
            ))
            .block_task()
            .await?;

        let session = connection
            .build_session(cwd()?)
            .start_session()
            .block_task()
            .await?
            .into_session();
        session.send_prompt("ask").block_task().await?;

        let error = permission_result_rx
            .next()
            .await
            .expect("the unhandled request should receive a response")
            .expect_err("the client should reject an unhandled permission request");
        assert_eq!(error.code, ErrorCode::MethodNotFound);
        Ok(())
    });

    tokio::time::timeout(TIMEOUT, client)
        .await
        .expect("unhandled v2 session message test timed out")
        .expect("unhandled v2 session message test failed");
}

#[tokio::test(flavor = "current_thread")]
async fn v2_resume_replay_is_handled_before_the_response() {
    let session_id = v2::SessionId::new("resumed-session");
    let config_option = v2::SessionConfigOption::boolean("thinking", "Thinking", false);
    let expected_response = v2::ResumeSessionResponse::new().config_options(vec![config_option]);
    let agent_response = expected_response.clone();
    let agent_session_id = session_id.clone();

    let agent = Agent
        .v2()
        .on_receive_request(
            async |request: v2::InitializeRequest,
                   responder: Responder<v2::InitializeResponse>,
                   _connection: V2ConnectionTo<Client>| {
                responder.respond(initialize_response(request.protocol_version))
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            async move |request: v2::ResumeSessionRequest,
                        responder: Responder<v2::ResumeSessionResponse>,
                        connection: V2ConnectionTo<Client>| {
                assert_eq!(request.session_id, agent_session_id);
                assert!(matches!(
                    request.replay_from,
                    Some(v2::ReplayFrom::Start(_))
                ));

                connection.send_notification(v2::UpdateSessionNotification::new(
                    request.session_id.clone(),
                    v2::SessionUpdate::UserMessage(v2::UserMessage::new(v2::MessageId::new(
                        "replayed-user",
                    ))),
                ))?;
                connection.send_notification(v2::UpdateSessionNotification::new(
                    request.session_id,
                    v2::SessionUpdate::AgentMessage(v2::AgentMessage::new(v2::MessageId::new(
                        "replayed-agent",
                    ))),
                ))?;
                responder.respond(agent_response.clone())
            },
            agent_client_protocol::on_receive_request!(),
        );

    let applied_replay = Arc::new(Mutex::new(Vec::new()));
    let applied_replay_handler = applied_replay.clone();
    let client = Client
        .v2()
        .on_receive_notification(
            async move |update: v2::UpdateSessionNotification,
                        _connection: V2ConnectionTo<Agent>| {
                let message_id = match update.update {
                    v2::SessionUpdate::UserMessage(message) => message.message_id,
                    v2::SessionUpdate::AgentMessage(message) => message.message_id,
                    other => panic!("unexpected replay update: {other:?}"),
                };
                applied_replay_handler
                    .lock()
                    .expect("replay projection lock should not be poisoned")
                    .push(message_id);
                Ok(())
            },
            agent_client_protocol::on_receive_notification!(),
        )
        .connect_with(agent, async move |connection| {
            connection
                .send_request(v2::InitializeRequest::new(
                    ProtocolVersion::V2,
                    implementation(),
                ))
                .block_task()
                .await?;

            let request = v2::ResumeSessionRequest::new(session_id.clone(), cwd()?)
                .replay_from(v2::ReplayFrom::from(v2::ReplayFromStart::new()));
            let opened = connection
                .resume_session_from(request)
                .start_session()
                .block_task()
                .await?;

            assert_eq!(opened.session().session_id(), &session_id);
            assert_eq!(opened.response(), &expected_response);
            assert_eq!(
                *applied_replay
                    .lock()
                    .expect("replay projection lock should not be poisoned"),
                vec![
                    v2::MessageId::new("replayed-user"),
                    v2::MessageId::new("replayed-agent"),
                ],
                "typed replay handlers must finish before the resume response is observed"
            );
            Ok(())
        });

    tokio::time::timeout(TIMEOUT, client)
        .await
        .expect("v2 resume replay test timed out")
        .expect("v2 resume replay test failed");
}

#[tokio::test(flavor = "current_thread")]
async fn v2_session_commands_cover_configuration_and_close() {
    let new_session_config_option = v2::SessionConfigOption::boolean("thinking", "Thinking", false);
    let updated_config_option = v2::SessionConfigOption::boolean("thinking", "Thinking", true);
    let set_response = v2::SetSessionConfigOptionResponse::new(vec![updated_config_option]);
    let agent_set_response = set_response.clone();

    let agent = Agent
        .v2()
        .on_receive_request(
            async |request: v2::InitializeRequest,
                   responder: Responder<v2::InitializeResponse>,
                   _connection: V2ConnectionTo<Client>| {
                responder.respond(initialize_response(request.protocol_version))
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            async move |_request: v2::NewSessionRequest,
                        responder: Responder<v2::NewSessionResponse>,
                        _connection: V2ConnectionTo<Client>| {
                responder.respond(
                    v2::NewSessionResponse::new(v2::SessionId::new("command-session"))
                        .config_options(vec![new_session_config_option.clone()]),
                )
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            async move |request: v2::SetSessionConfigOptionRequest,
                        responder: Responder<v2::SetSessionConfigOptionResponse>,
                        _connection: V2ConnectionTo<Client>| {
                assert_eq!(request.session_id, v2::SessionId::new("command-session"));
                assert_eq!(request.config_id, v2::SessionConfigId::new("thinking"));
                assert_eq!(request.value, v2::SessionConfigOptionValue::from(true));
                responder.respond(agent_set_response.clone())
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            async |request: v2::CloseSessionRequest,
                   responder: Responder<v2::CloseSessionResponse>,
                   _connection: V2ConnectionTo<Client>| {
                assert_eq!(request.session_id, v2::SessionId::new("command-session"));
                responder.respond(v2::CloseSessionResponse::new())
            },
            agent_client_protocol::on_receive_request!(),
        );

    let client = Client.v2().connect_with(agent, async move |connection| {
        connection
            .send_request(v2::InitializeRequest::new(
                ProtocolVersion::V2,
                implementation(),
            ))
            .block_task()
            .await?;

        let opened = connection
            .build_session(cwd()?)
            .start_session()
            .block_task()
            .await?;
        let (session, _) = opened.into_parts();

        assert_eq!(
            session
                .set_config_option("thinking", true)
                .block_task()
                .await?,
            set_response
        );
        assert_eq!(
            session.close().block_task().await?,
            v2::CloseSessionResponse::new()
        );
        Ok(())
    });

    tokio::time::timeout(TIMEOUT, client)
        .await
        .expect("v2 session command test timed out")
        .expect("v2 session command test failed");
}

#[cfg(feature = "unstable_session_inject")]
#[tokio::test(flavor = "current_thread")]
async fn v2_session_inject_helpers_preserve_typed_content_and_message_ids() {
    let session_id = v2::SessionId::new("inject-session");
    let agent_session_id = session_id.clone();

    let agent = Agent
        .v2()
        .on_receive_request(
            async |request: v2::InitializeRequest,
                   responder: Responder<v2::InitializeResponse>,
                   _connection: V2ConnectionTo<Client>| {
                responder.respond(initialize_response(request.protocol_version))
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            async move |_request: v2::NewSessionRequest,
                        responder: Responder<v2::NewSessionResponse>,
                        _connection: V2ConnectionTo<Client>| {
                responder.respond(v2::NewSessionResponse::new(agent_session_id.clone()))
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            async |request: v2::InjectSessionRequest,
                   responder: Responder<v2::InjectSessionResponse>,
                   _connection: V2ConnectionTo<Client>| {
                assert_eq!(request.session_id, v2::SessionId::new("inject-session"));
                assert_eq!(request.mode, v2::SessionInjectMode::Steer);
                assert_eq!(
                    request.content,
                    vec![v2::ContentBlock::Text(v2::TextContent::new("steer"))]
                );
                responder.respond(v2::InjectSessionResponse::new(v2::MessageId::new(
                    "message-1",
                )))
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            async |request: v2::RevokeInjectSessionRequest,
                   responder: Responder<v2::RevokeInjectSessionResponse>,
                   _connection: V2ConnectionTo<Client>| {
                assert_eq!(request.session_id, v2::SessionId::new("inject-session"));
                assert_eq!(request.message_id, v2::MessageId::new("message-1"));
                responder.respond(v2::RevokeInjectSessionResponse::new())
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            async |request: v2::ReplaceInjectSessionRequest,
                   responder: Responder<v2::ReplaceInjectSessionResponse>,
                   _connection: V2ConnectionTo<Client>| {
                assert_eq!(request.session_id, v2::SessionId::new("inject-session"));
                assert_eq!(request.message_id, v2::MessageId::new("message-1"));
                assert_eq!(
                    request.content,
                    vec![v2::ContentBlock::Text(v2::TextContent::new("replacement"))]
                );
                responder.respond(v2::ReplaceInjectSessionResponse::new(request.message_id))
            },
            agent_client_protocol::on_receive_request!(),
        );

    let client = Client.v2().connect_with(agent, async move |connection| {
        connection
            .send_request(v2::InitializeRequest::new(
                ProtocolVersion::V2,
                implementation(),
            ))
            .block_task()
            .await?;

        let opened = connection
            .build_session(cwd()?)
            .start_session()
            .block_task()
            .await?;
        let (session, _) = opened.into_parts();

        let injected = session
            .inject(
                v2::SessionInjectMode::Steer,
                vec![v2::ContentBlock::Text(v2::TextContent::new("steer"))],
            )
            .block_task()
            .await?;
        assert_eq!(injected.message_id, v2::MessageId::new("message-1"));

        let replaced = session
            .replace_inject(
                injected.message_id,
                vec![v2::ContentBlock::Text(v2::TextContent::new("replacement"))],
            )
            .block_task()
            .await?;
        assert_eq!(replaced.message_id, v2::MessageId::new("message-1"));

        session
            .revoke_inject(replaced.message_id)
            .block_task()
            .await?;
        Ok(())
    });

    tokio::time::timeout(TIMEOUT, client)
        .await
        .expect("v2 session inject helper test timed out")
        .expect("v2 session inject helper test failed");
}

#[tokio::test(flavor = "current_thread")]
async fn dropping_v2_session_does_not_unregister_update_handling() {
    let session_id = v2::SessionId::new("dropped-handle");
    let agent_session_id = session_id.clone();

    let agent = Agent
        .v2()
        .on_receive_request(
            async |request: v2::InitializeRequest,
                   responder: Responder<v2::InitializeResponse>,
                   _connection: V2ConnectionTo<Client>| {
                responder.respond(initialize_response(request.protocol_version))
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            async move |_request: v2::NewSessionRequest,
                        responder: Responder<v2::NewSessionResponse>,
                        _connection: V2ConnectionTo<Client>| {
                responder.respond(v2::NewSessionResponse::new(agent_session_id.clone()))
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            async move |_request: v2::ListSessionsRequest,
                        responder: Responder<v2::ListSessionsResponse>,
                        connection: V2ConnectionTo<Client>| {
                connection.send_notification(v2::UpdateSessionNotification::new(
                    v2::SessionId::new("dropped-handle"),
                    v2::SessionUpdate::AgentMessage(
                        v2::AgentMessage::new(v2::MessageId::new("background"))
                            .content(vec!["still routed".into()]),
                    ),
                ))?;
                responder.respond(v2::ListSessionsResponse::new(Vec::new()))
            },
            agent_client_protocol::on_receive_request!(),
        );

    let (update_tx, mut update_rx) = mpsc::unbounded();
    let client = Client
        .v2()
        .on_receive_notification(
            async move |update: v2::UpdateSessionNotification,
                        _connection: V2ConnectionTo<Agent>| {
                update_tx
                    .unbounded_send(update)
                    .map_err(Error::into_internal_error)
            },
            agent_client_protocol::on_receive_notification!(),
        )
        .connect_with(agent, async move |connection| {
            connection
                .send_request(v2::InitializeRequest::new(
                    ProtocolVersion::V2,
                    implementation(),
                ))
                .block_task()
                .await?;

            let opened = connection
                .build_session(cwd()?)
                .start_session()
                .block_task()
                .await?;
            let (session, _) = opened.into_parts();
            assert_eq!(session.session_id(), &session_id);
            drop(session);

            let sessions = connection
                .send_request(v2::ListSessionsRequest::new())
                .block_task()
                .await?;
            assert!(sessions.sessions.is_empty());

            let update = next_update(&mut update_rx).await;
            assert_eq!(update.session_id, session_id);
            assert!(matches!(
                update.update,
                v2::SessionUpdate::AgentMessage(message)
                    if message.message_id == v2::MessageId::new("background")
            ));
            Ok(())
        });

    tokio::time::timeout(TIMEOUT, client)
        .await
        .expect("v2 dropped-session update handling test timed out")
        .expect("v2 dropped-session update handling test failed");
}

#[tokio::test(flavor = "current_thread")]
async fn v2_session_new_error_is_preserved_without_closing_connection() {
    let agent = Agent
        .v2()
        .on_receive_request(
            async |request: v2::InitializeRequest,
                   responder: Responder<v2::InitializeResponse>,
                   _connection: V2ConnectionTo<Client>| {
                responder.respond(initialize_response(request.protocol_version))
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            async |_request: v2::NewSessionRequest,
                   responder: Responder<v2::NewSessionResponse>,
                   _connection: V2ConnectionTo<Client>| {
                responder.respond_with_error(Error::invalid_params().data("session rejected"))
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            async |_request: v2::ListSessionsRequest,
                   responder: Responder<v2::ListSessionsResponse>,
                   _connection: V2ConnectionTo<Client>| {
                responder.respond(v2::ListSessionsResponse::new(Vec::new()))
            },
            agent_client_protocol::on_receive_request!(),
        );

    let client = Client.v2().connect_with(agent, async move |connection| {
        connection
            .send_request(v2::InitializeRequest::new(
                ProtocolVersion::V2,
                implementation(),
            ))
            .block_task()
            .await?;

        let error = connection
            .build_session(cwd()?)
            .start_session()
            .block_task()
            .await
            .expect_err("the session/new error must reach the caller");
        assert_eq!(
            error,
            Error::invalid_params().data("session rejected"),
            "the helper must preserve the peer's JSON-RPC error"
        );

        let sessions = connection
            .send_request(v2::ListSessionsRequest::new())
            .block_task()
            .await?;
        assert!(sessions.sessions.is_empty());
        Ok(())
    });

    tokio::time::timeout(TIMEOUT, client)
        .await
        .expect("v2 session/new error test timed out")
        .expect("v2 session/new error closed the connection");
}

#[tokio::test(flavor = "current_thread")]
async fn low_level_v2_runner_rejects_the_v1_session_helper() {
    let (completed_tx, completed_rx) = oneshot::channel();
    let client = Client
        .v2()
        .with_runner(V1SessionGuardRunner {
            cwd: cwd().expect("current directory should be available"),
            completed: completed_tx,
        })
        .connect_with(Agent.v2(), async move |_connection| {
            completed_rx.await.map_err(Error::into_internal_error)
        });

    tokio::time::timeout(TIMEOUT, client)
        .await
        .expect("low-level v1 session guard test timed out")
        .expect("low-level v1 session guard connection failed");
}
