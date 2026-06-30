#[cfg(feature = "unstable")]
use std::collections::BTreeMap;
use std::{
    collections::{HashMap, HashSet},
    path::PathBuf,
    sync::{Arc, Mutex},
    time::Duration,
};

#[cfg(feature = "unstable")]
use agent_client_protocol::schema::v1::{
    CompleteElicitationNotification, CreateElicitationRequest, CreateElicitationResponse,
    ElicitationAcceptAction, ElicitationAction, ElicitationCapabilities, ElicitationContentValue,
    ElicitationFormCapabilities, ElicitationMode, ElicitationScope, ElicitationUrlCapabilities,
    ErrorCode, UrlElicitationRequiredData,
};
use agent_client_protocol::{
    Client, Responder,
    schema::{
        ProtocolVersion,
        v1::{
            AuthenticateRequest, CancelNotification, ClientCapabilities, CloseSessionRequest,
            ContentBlock, CreateTerminalRequest, CreateTerminalResponse, DeleteSessionRequest,
            FileSystemCapabilities, InitializeRequest, KillTerminalRequest, KillTerminalResponse,
            ListSessionsRequest, LoadSessionRequest, LogoutRequest, McpServer, McpServerStdio,
            NewSessionRequest, PermissionOptionId, PromptRequest, ReadTextFileRequest,
            ReadTextFileResponse, ReleaseTerminalRequest, ReleaseTerminalResponse,
            RequestPermissionOutcome, RequestPermissionRequest, RequestPermissionResponse,
            ResumeSessionRequest, SelectedPermissionOutcome, SessionId, SessionNotification,
            SessionUpdate, SetSessionConfigOptionRequest, SetSessionModeRequest, StopReason,
            TerminalExitStatus, TerminalOutputRequest, TerminalOutputResponse, ToolCallContent,
            ToolCallStatus, WaitForTerminalExitRequest, WaitForTerminalExitResponse,
            WriteTextFileRequest, WriteTextFileResponse,
        },
    },
};
use agent_client_protocol_test::testy::{Testy, TestyCommand, TestyScenario};

#[tokio::test]
async fn testy_handles_stable_agent_requests() -> Result<(), agent_client_protocol::Error> {
    let agent_messages = Arc::new(Mutex::new(Vec::<String>::new()));

    Client
        .builder()
        .on_receive_notification(
            {
                let agent_messages = Arc::clone(&agent_messages);
                async move |notification: SessionNotification, _cx| {
                    let SessionUpdate::AgentMessageChunk(chunk) = notification.update else {
                        return Ok(());
                    };
                    let ContentBlock::Text(text) = chunk.content else {
                        return Ok(());
                    };
                    agent_messages.lock().unwrap().push(text.text);
                    Ok(())
                }
            },
            agent_client_protocol::on_receive_notification!(),
        )
        .connect_with(Testy::new(), async |cx| {
            let initialize = InitializeRequest::new(ProtocolVersion::V1).client_capabilities(
                ClientCapabilities::new()
                    .fs(FileSystemCapabilities::new()
                        .read_text_file(true)
                        .write_text_file(true))
                    .terminal(true),
            );
            let init = cx.send_request(initialize).block_task().await?;
            assert!(init.agent_capabilities.load_session);
            assert!(init.agent_capabilities.prompt_capabilities.image);
            assert!(init.agent_capabilities.prompt_capabilities.audio);
            assert!(init.agent_capabilities.prompt_capabilities.embedded_context);
            assert_eq!(init.auth_methods.len(), 1);

            assert!(
                cx.send_request(AuthenticateRequest::new("unknown-auth-method"))
                    .block_task()
                    .await
                    .is_err()
            );
            cx.send_request(AuthenticateRequest::new("testy-agent-auth"))
                .block_task()
                .await?;
            cx.send_request(LogoutRequest::new()).block_task().await?;

            let session = cx
                .send_request(
                    NewSessionRequest::new(PathBuf::from("/tmp"))
                        .additional_directories(vec![PathBuf::from("/var/tmp")]),
                )
                .block_task()
                .await?;
            assert_eq!(session.session_id.to_string(), "testy-session-1");
            assert!(session.modes.is_some());
            assert!(session.config_options.is_some());

            assert!(
                cx.send_request(SetSessionModeRequest::new(
                    SessionId::new("missing-session"),
                    "chat",
                ))
                .block_task()
                .await
                .is_err()
            );
            assert!(
                cx.send_request(SetSessionModeRequest::new(
                    session.session_id.clone(),
                    "invalid-mode",
                ))
                .block_task()
                .await
                .is_err()
            );

            let list = cx
                .send_request(ListSessionsRequest::new().cwd(PathBuf::from("/tmp")))
                .block_task()
                .await?;
            assert_eq!(list.sessions.len(), 1);
            assert_eq!(list.sessions[0].session_id, session.session_id);
            assert_eq!(
                list.sessions[0].additional_directories,
                vec![PathBuf::from("/var/tmp")]
            );

            cx.send_request(SetSessionModeRequest::new(
                session.session_id.clone(),
                "plan",
            ))
            .block_task()
            .await?;

            let config = cx
                .send_request(SetSessionConfigOptionRequest::new(
                    session.session_id.clone(),
                    "verbosity",
                    "verbose",
                ))
                .block_task()
                .await?;
            assert_eq!(config.config_options.len(), 1);

            assert!(
                cx.send_request(SetSessionConfigOptionRequest::new(
                    SessionId::new("missing-session"),
                    "verbosity",
                    "verbose",
                ))
                .block_task()
                .await
                .is_err()
            );
            assert!(
                cx.send_request(SetSessionConfigOptionRequest::new(
                    session.session_id.clone(),
                    "unknown",
                    "verbose",
                ))
                .block_task()
                .await
                .is_err()
            );
            assert!(
                cx.send_request(SetSessionConfigOptionRequest::new(
                    session.session_id.clone(),
                    "verbosity",
                    "loud",
                ))
                .block_task()
                .await
                .is_err()
            );

            cx.send_notification(CancelNotification::new(session.session_id.clone()))?;
            let prompt = cx
                .send_request(PromptRequest::new(
                    session.session_id.clone(),
                    vec![TestyCommand::Greet.to_prompt().into()],
                ))
                .block_task()
                .await?;
            assert_eq!(prompt.stop_reason, StopReason::EndTurn);

            let prompt = cx
                .send_request(PromptRequest::new(
                    session.session_id,
                    vec![
                        TestyCommand::RunScenario {
                            scenario: TestyScenario::CancelStatus,
                        }
                        .to_prompt()
                        .into(),
                    ],
                ))
                .block_task()
                .await?;
            assert_eq!(prompt.stop_reason, StopReason::Cancelled);

            Ok(())
        })
        .await?;

    assert!(
        agent_messages
            .lock()
            .unwrap()
            .iter()
            .any(|message| message == "Hello, world!")
    );

    Ok(())
}

#[tokio::test]
async fn testy_reserves_loaded_session_ids_and_rejects_after_close()
-> Result<(), agent_client_protocol::Error> {
    Client
        .builder()
        .connect_with(Testy::new(), async |cx| {
            cx.send_request(InitializeRequest::new(ProtocolVersion::V1))
                .block_task()
                .await?;

            let loaded_id = SessionId::new("testy-session-1");
            cx.send_request(LoadSessionRequest::new(
                loaded_id.clone(),
                PathBuf::from("/loaded"),
            ))
            .block_task()
            .await?;

            let resumed_id = SessionId::new("testy-session-2");
            cx.send_request(ResumeSessionRequest::new(
                resumed_id.clone(),
                PathBuf::from("/resumed"),
            ))
            .block_task()
            .await?;

            let created = cx
                .send_request(NewSessionRequest::new(PathBuf::from("/created")))
                .block_task()
                .await?;
            assert_eq!(created.session_id.to_string(), "testy-session-3");

            let listed = cx
                .send_request(ListSessionsRequest::new())
                .block_task()
                .await?;
            assert_eq!(listed.sessions.len(), 3);

            cx.send_request(CloseSessionRequest::new(loaded_id.clone()))
                .block_task()
                .await?;
            assert!(
                cx.send_request(PromptRequest::new(
                    loaded_id.clone(),
                    vec![TestyCommand::Greet.to_prompt().into()],
                ))
                .block_task()
                .await
                .is_err()
            );
            assert!(
                cx.send_request(SetSessionModeRequest::new(loaded_id.clone(), "chat"))
                    .block_task()
                    .await
                    .is_err()
            );
            assert!(
                cx.send_request(SetSessionConfigOptionRequest::new(
                    loaded_id.clone(),
                    "verbosity",
                    "brief",
                ))
                .block_task()
                .await
                .is_err()
            );

            let listed = cx
                .send_request(ListSessionsRequest::new())
                .block_task()
                .await?;
            let session_ids = listed
                .sessions
                .iter()
                .map(|session| session.session_id.to_string())
                .collect::<HashSet<_>>();
            assert!(!session_ids.contains("testy-session-1"));
            assert!(session_ids.contains("testy-session-2"));
            assert!(session_ids.contains("testy-session-3"));

            Ok(())
        })
        .await
}

#[tokio::test]
async fn testy_session_updates_scenario_emits_every_update_variant()
-> Result<(), agent_client_protocol::Error> {
    let updates = Arc::new(Mutex::new(Vec::<&'static str>::new()));

    Client
        .builder()
        .on_receive_notification(
            {
                let updates = Arc::clone(&updates);
                async move |notification: SessionNotification, _cx| {
                    let label = match &notification.update {
                        SessionUpdate::UserMessageChunk(_) => "user_message_chunk",
                        SessionUpdate::AgentMessageChunk(_) => "agent_message_chunk",
                        SessionUpdate::AgentThoughtChunk(_) => "agent_thought_chunk",
                        SessionUpdate::ToolCall(_) => "tool_call",
                        SessionUpdate::ToolCallUpdate(_) => "tool_call_update",
                        SessionUpdate::Plan(_) => "plan",
                        SessionUpdate::AvailableCommandsUpdate(_) => "available_commands_update",
                        SessionUpdate::CurrentModeUpdate(_) => "current_mode_update",
                        SessionUpdate::ConfigOptionUpdate(_) => "config_option_update",
                        SessionUpdate::SessionInfoUpdate(_) => "session_info_update",
                        SessionUpdate::UsageUpdate(_) => "usage_update",
                        _ => "other",
                    };
                    updates.lock().unwrap().push(label);
                    Ok(())
                }
            },
            agent_client_protocol::on_receive_notification!(),
        )
        .connect_with(Testy::new(), async |cx| {
            cx.send_request(InitializeRequest::new(ProtocolVersion::V1))
                .block_task()
                .await?;
            let session = cx
                .send_request(NewSessionRequest::new(PathBuf::from("/tmp")))
                .block_task()
                .await?;
            let response = cx
                .send_request(PromptRequest::new(
                    session.session_id,
                    vec![
                        TestyCommand::RunScenario {
                            scenario: TestyScenario::SessionUpdates,
                        }
                        .to_prompt()
                        .into(),
                    ],
                ))
                .block_task()
                .await?;
            assert_eq!(response.stop_reason, StopReason::EndTurn);
            Ok(())
        })
        .await?;

    let updates = updates.lock().unwrap();
    for expected in [
        "session_info_update",
        "current_mode_update",
        "config_option_update",
        "available_commands_update",
        "usage_update",
        "user_message_chunk",
        "agent_thought_chunk",
        "agent_message_chunk",
        "tool_call",
        "tool_call_update",
        "plan",
    ] {
        assert!(updates.contains(&expected), "missing update: {expected}");
    }

    Ok(())
}

#[tokio::test]
async fn testy_full_scenario_returns_cancelled_after_session_cancel()
-> Result<(), agent_client_protocol::Error> {
    Client
        .builder()
        .on_receive_request(
            async move |request: RequestPermissionRequest, responder, cx| {
                cx.send_notification(CancelNotification::new(request.session_id.clone()))?;
                let option_id = request.options.first().map_or_else(
                    || PermissionOptionId::new("allow_once"),
                    |option| option.option_id.clone(),
                );
                responder.respond(RequestPermissionResponse::new(
                    RequestPermissionOutcome::Selected(SelectedPermissionOutcome::new(option_id)),
                ))
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            async move |_request: WriteTextFileRequest, responder, _cx| {
                responder.respond(WriteTextFileResponse::new())
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            async move |_request: ReadTextFileRequest, responder, _cx| {
                responder.respond(ReadTextFileResponse::new("read by testy client"))
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            async move |_request: CreateTerminalRequest, responder, _cx| {
                responder.respond(CreateTerminalResponse::new("testy-terminal-client"))
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            async move |_request: TerminalOutputRequest, responder, _cx| {
                responder.respond(
                    TerminalOutputResponse::new("terminal output", false)
                        .exit_status(TerminalExitStatus::new().exit_code(0)),
                )
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            async move |_request: WaitForTerminalExitRequest, responder, _cx| {
                responder.respond(WaitForTerminalExitResponse::new(
                    TerminalExitStatus::new().exit_code(0),
                ))
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            async move |_request: KillTerminalRequest, responder, _cx| {
                responder.respond(KillTerminalResponse::new())
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            async move |_request: ReleaseTerminalRequest, responder, _cx| {
                responder.respond(ReleaseTerminalResponse::new())
            },
            agent_client_protocol::on_receive_request!(),
        )
        .connect_with(Testy::new(), async |cx| {
            cx.send_request(InitializeRequest::new(ProtocolVersion::V1))
                .block_task()
                .await?;
            let session = cx
                .send_request(NewSessionRequest::new(PathBuf::from("/tmp")))
                .block_task()
                .await?;
            let response = cx
                .send_request(PromptRequest::new(
                    session.session_id,
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
            assert_eq!(response.stop_reason, StopReason::Cancelled);

            Ok(())
        })
        .await
}

#[tokio::test]
async fn testy_full_scenario_stops_after_terminal_create_cancellation()
-> Result<(), agent_client_protocol::Error> {
    let forbidden_updates = Arc::new(Mutex::new(Vec::<&'static str>::new()));
    let callback_requests = Arc::new(Mutex::new(Vec::<&'static str>::new()));

    Client
        .builder()
        .on_receive_notification(
            {
                let forbidden_updates = Arc::clone(&forbidden_updates);
                async move |notification: SessionNotification, _cx| {
                    let update = match notification.update {
                        SessionUpdate::ToolCall(_) => "tool_call",
                        SessionUpdate::ToolCallUpdate(_) => "tool_call_update",
                        SessionUpdate::Plan(_) => "plan",
                        _ => return Ok(()),
                    };
                    forbidden_updates.lock().unwrap().push(update);
                    Ok(())
                }
            },
            agent_client_protocol::on_receive_notification!(),
        )
        .on_receive_request(
            {
                let callback_requests = Arc::clone(&callback_requests);
                async move |request: CreateTerminalRequest, responder, cx| {
                    callback_requests.lock().unwrap().push("terminal/create");
                    cx.send_notification(CancelNotification::new(request.session_id.clone()))?;
                    responder.respond(CreateTerminalResponse::new("testy-terminal-client"))
                }
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            {
                let callback_requests = Arc::clone(&callback_requests);
                async move |request: RequestPermissionRequest, responder, _cx| {
                    callback_requests
                        .lock()
                        .unwrap()
                        .push("session/request_permission");
                    let option_id = request.options.first().map_or_else(
                        || PermissionOptionId::new("allow_once"),
                        |option| option.option_id.clone(),
                    );
                    responder.respond(RequestPermissionResponse::new(
                        RequestPermissionOutcome::Selected(SelectedPermissionOutcome::new(
                            option_id,
                        )),
                    ))
                }
            },
            agent_client_protocol::on_receive_request!(),
        )
        .connect_with(Testy::new(), async |cx| {
            cx.send_request(InitializeRequest::new(ProtocolVersion::V1))
                .block_task()
                .await?;
            let session = cx
                .send_request(NewSessionRequest::new(PathBuf::from("/tmp")))
                .block_task()
                .await?;
            let response = cx
                .send_request(PromptRequest::new(
                    session.session_id,
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
            assert_eq!(response.stop_reason, StopReason::Cancelled);

            Ok(())
        })
        .await?;

    let forbidden_updates = forbidden_updates.lock().unwrap();
    assert!(
        forbidden_updates.is_empty(),
        "full continued with updates after cancellation: {forbidden_updates:?}",
    );
    let callback_requests = callback_requests.lock().unwrap();
    assert_eq!(&*callback_requests, &["terminal/create"]);

    Ok(())
}

#[tokio::test]
async fn testy_callbacks_scenario_returns_cancelled_after_session_cancel()
-> Result<(), agent_client_protocol::Error> {
    Client
        .builder()
        .on_receive_request(
            async move |request: RequestPermissionRequest, responder, cx| {
                cx.send_notification(CancelNotification::new(request.session_id.clone()))?;
                let option_id = request.options.first().map_or_else(
                    || PermissionOptionId::new("allow_once"),
                    |option| option.option_id.clone(),
                );
                responder.respond(RequestPermissionResponse::new(
                    RequestPermissionOutcome::Selected(SelectedPermissionOutcome::new(option_id)),
                ))
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            async move |_request: WriteTextFileRequest, responder, _cx| {
                responder.respond(WriteTextFileResponse::new())
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            async move |_request: ReadTextFileRequest, responder, _cx| {
                responder.respond(ReadTextFileResponse::new("read by testy client"))
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            async move |_request: CreateTerminalRequest, responder, _cx| {
                responder.respond(CreateTerminalResponse::new("testy-terminal-client"))
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            async move |_request: TerminalOutputRequest, responder, _cx| {
                responder.respond(
                    TerminalOutputResponse::new("terminal output", false)
                        .exit_status(TerminalExitStatus::new().exit_code(0)),
                )
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            async move |_request: WaitForTerminalExitRequest, responder, _cx| {
                responder.respond(WaitForTerminalExitResponse::new(
                    TerminalExitStatus::new().exit_code(0),
                ))
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            async move |_request: KillTerminalRequest, responder, _cx| {
                responder.respond(KillTerminalResponse::new())
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            async move |_request: ReleaseTerminalRequest, responder, _cx| {
                responder.respond(ReleaseTerminalResponse::new())
            },
            agent_client_protocol::on_receive_request!(),
        )
        .connect_with(Testy::new(), async |cx| {
            cx.send_request(InitializeRequest::new(ProtocolVersion::V1))
                .block_task()
                .await?;
            let session = cx
                .send_request(NewSessionRequest::new(PathBuf::from("/tmp")))
                .block_task()
                .await?;
            let response = cx
                .send_request(PromptRequest::new(
                    session.session_id.clone(),
                    vec![
                        TestyCommand::RunScenario {
                            scenario: TestyScenario::Callbacks,
                        }
                        .to_prompt()
                        .into(),
                    ],
                ))
                .block_task()
                .await?;
            assert_eq!(response.stop_reason, StopReason::Cancelled);

            let response = cx
                .send_request(PromptRequest::new(
                    session.session_id,
                    vec![
                        TestyCommand::RunScenario {
                            scenario: TestyScenario::CancelStatus,
                        }
                        .to_prompt()
                        .into(),
                    ],
                ))
                .block_task()
                .await?;
            assert_eq!(response.stop_reason, StopReason::EndTurn);

            Ok(())
        })
        .await
}

#[tokio::test]
async fn testy_callbacks_scenario_cancels_pending_callback_request()
-> Result<(), agent_client_protocol::Error> {
    let callbacks = Arc::new(Mutex::new(Vec::<&'static str>::new()));
    let pending_permission = Arc::new(Mutex::new(None::<Responder<RequestPermissionResponse>>));

    Client
        .builder()
        .on_receive_request(
            {
                let callbacks = Arc::clone(&callbacks);
                let pending_permission = Arc::clone(&pending_permission);
                async move |request: RequestPermissionRequest, responder, cx| {
                    callbacks.lock().unwrap().push("session/request_permission");
                    cx.send_notification(CancelNotification::new(request.session_id.clone()))?;
                    *pending_permission.lock().unwrap() = Some(responder);
                    Ok(())
                }
            },
            agent_client_protocol::on_receive_request!(),
        )
        .connect_with(Testy::new(), async |cx| {
            cx.send_request(InitializeRequest::new(ProtocolVersion::V1))
                .block_task()
                .await?;
            let session = cx
                .send_request(NewSessionRequest::new(PathBuf::from("/tmp")))
                .block_task()
                .await?;
            let response = tokio::time::timeout(
                Duration::from_secs(1),
                cx.send_request(PromptRequest::new(
                    session.session_id,
                    vec![
                        TestyCommand::RunScenario {
                            scenario: TestyScenario::Callbacks,
                        }
                        .to_prompt()
                        .into(),
                    ],
                ))
                .block_task(),
            )
            .await
            .expect("prompt did not observe cancellation while callback was pending")?;
            assert_eq!(response.stop_reason, StopReason::Cancelled);

            if let Some(responder) = pending_permission.lock().unwrap().take() {
                drop(responder.respond(RequestPermissionResponse::new(
                    RequestPermissionOutcome::Selected(SelectedPermissionOutcome::new(
                        PermissionOptionId::new("allow_once"),
                    )),
                )));
            }

            Ok(())
        })
        .await?;

    assert_eq!(&*callbacks.lock().unwrap(), &["session/request_permission"]);

    Ok(())
}

#[tokio::test]
async fn testy_call_tool_prompt_observes_session_cancel() -> Result<(), agent_client_protocol::Error>
{
    Client
        .builder()
        .connect_with(Testy::new(), async |cx| {
            cx.send_request(InitializeRequest::new(ProtocolVersion::V1))
                .block_task()
                .await?;
            let session = cx
                .send_request(
                    NewSessionRequest::new(PathBuf::from("/tmp"))
                        .mcp_servers(vec![hanging_mcp_server()]),
                )
                .block_task()
                .await?;

            let prompt = cx.send_request(PromptRequest::new(
                session.session_id.clone(),
                vec![
                    TestyCommand::CallTool {
                        server: "hung".to_string(),
                        tool: "echo".to_string(),
                        params: serde_json::json!({ "message": "hello" }),
                    }
                    .to_prompt()
                    .into(),
                ],
            ));
            cx.send_notification(CancelNotification::new(session.session_id))?;

            let response = tokio::time::timeout(Duration::from_secs(1), prompt.block_task())
                .await
                .expect("MCP tool call prompt did not observe session/cancel")?;
            assert_eq!(response.stop_reason, StopReason::Cancelled);

            Ok(())
        })
        .await
}

#[tokio::test]
async fn testy_list_tools_prompt_observes_session_close() -> Result<(), agent_client_protocol::Error>
{
    Client
        .builder()
        .connect_with(Testy::new(), async |cx| {
            cx.send_request(InitializeRequest::new(ProtocolVersion::V1))
                .block_task()
                .await?;
            let session = cx
                .send_request(
                    NewSessionRequest::new(PathBuf::from("/tmp"))
                        .mcp_servers(vec![hanging_mcp_server()]),
                )
                .block_task()
                .await?;

            let prompt = cx.send_request(PromptRequest::new(
                session.session_id.clone(),
                vec![
                    TestyCommand::ListTools {
                        server: "hung".to_string(),
                    }
                    .to_prompt()
                    .into(),
                ],
            ));
            cx.send_request(CloseSessionRequest::new(session.session_id))
                .block_task()
                .await?;

            let response = tokio::time::timeout(Duration::from_secs(1), prompt.block_task())
                .await
                .expect("MCP list tools prompt did not observe session/close")?;
            assert_eq!(response.stop_reason, StopReason::Cancelled);

            Ok(())
        })
        .await
}

#[tokio::test]
async fn testy_delete_cancels_in_flight_prompt_before_cleanup()
-> Result<(), agent_client_protocol::Error> {
    Client
        .builder()
        .connect_with(Testy::new(), async |cx| {
            cx.send_request(InitializeRequest::new(ProtocolVersion::V1))
                .block_task()
                .await?;
            let session = cx
                .send_request(
                    NewSessionRequest::new(PathBuf::from("/tmp"))
                        .mcp_servers(vec![hanging_mcp_server()]),
                )
                .block_task()
                .await?;

            let prompt = cx.send_request(PromptRequest::new(
                session.session_id.clone(),
                vec![
                    TestyCommand::CallTool {
                        server: "hung".to_string(),
                        tool: "never".to_string(),
                        params: serde_json::json!({}),
                    }
                    .to_prompt()
                    .into(),
                ],
            ));
            cx.send_request(DeleteSessionRequest::new(session.session_id.clone()))
                .block_task()
                .await?;

            let response = tokio::time::timeout(Duration::from_secs(1), prompt.block_task())
                .await
                .expect("MCP tool call prompt did not observe session/delete")?;
            assert_eq!(response.stop_reason, StopReason::Cancelled);

            let list = cx
                .send_request(ListSessionsRequest::new())
                .block_task()
                .await?;
            assert!(list.sessions.is_empty());
            assert!(
                cx.send_request(PromptRequest::new(
                    session.session_id,
                    vec![TestyCommand::Greet.to_prompt().into()],
                ))
                .block_task()
                .await
                .is_err()
            );

            Ok(())
        })
        .await
}

#[tokio::test]
async fn testy_close_cancels_in_flight_prompt_before_cleanup()
-> Result<(), agent_client_protocol::Error> {
    Client
        .builder()
        .on_receive_request(
            async move |request: RequestPermissionRequest, responder, cx| {
                cx.send_request(CloseSessionRequest::new(request.session_id.clone()))
                    .detach();
                let option_id = request.options.first().map_or_else(
                    || PermissionOptionId::new("allow_once"),
                    |option| option.option_id.clone(),
                );
                responder.respond(RequestPermissionResponse::new(
                    RequestPermissionOutcome::Selected(SelectedPermissionOutcome::new(option_id)),
                ))
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            async move |_request: WriteTextFileRequest, responder, _cx| {
                responder.respond(WriteTextFileResponse::new())
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            async move |_request: ReadTextFileRequest, responder, _cx| {
                responder.respond(ReadTextFileResponse::new("read by testy client"))
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            async move |_request: CreateTerminalRequest, responder, _cx| {
                responder.respond(CreateTerminalResponse::new("testy-terminal-client"))
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            async move |_request: TerminalOutputRequest, responder, _cx| {
                responder.respond(
                    TerminalOutputResponse::new("terminal output", false)
                        .exit_status(TerminalExitStatus::new().exit_code(0)),
                )
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            async move |_request: WaitForTerminalExitRequest, responder, _cx| {
                responder.respond(WaitForTerminalExitResponse::new(
                    TerminalExitStatus::new().exit_code(0),
                ))
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            async move |_request: KillTerminalRequest, responder, _cx| {
                responder.respond(KillTerminalResponse::new())
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            async move |_request: ReleaseTerminalRequest, responder, _cx| {
                responder.respond(ReleaseTerminalResponse::new())
            },
            agent_client_protocol::on_receive_request!(),
        )
        .connect_with(Testy::new(), async |cx| {
            cx.send_request(InitializeRequest::new(ProtocolVersion::V1))
                .block_task()
                .await?;
            let session = cx
                .send_request(NewSessionRequest::new(PathBuf::from("/tmp")))
                .block_task()
                .await?;
            let response = cx
                .send_request(PromptRequest::new(
                    session.session_id.clone(),
                    vec![
                        TestyCommand::RunScenario {
                            scenario: TestyScenario::Callbacks,
                        }
                        .to_prompt()
                        .into(),
                    ],
                ))
                .block_task()
                .await?;
            assert_eq!(response.stop_reason, StopReason::Cancelled);

            let list = cx
                .send_request(ListSessionsRequest::new())
                .block_task()
                .await?;
            assert!(list.sessions.is_empty());
            assert!(
                cx.send_request(PromptRequest::new(
                    session.session_id,
                    vec![TestyCommand::Greet.to_prompt().into()],
                ))
                .block_task()
                .await
                .is_err()
            );

            Ok(())
        })
        .await
}

#[tokio::test]
async fn testy_callbacks_stop_after_terminal_output_cancellation()
-> Result<(), agent_client_protocol::Error> {
    let callbacks = Arc::new(Mutex::new(Vec::<&'static str>::new()));

    Client
        .builder()
        .on_receive_request(
            {
                let callbacks = Arc::clone(&callbacks);
                async move |request: RequestPermissionRequest, responder, _cx| {
                    callbacks.lock().unwrap().push("session/request_permission");
                    let option_id = request.options.first().map_or_else(
                        || PermissionOptionId::new("allow_once"),
                        |option| option.option_id.clone(),
                    );
                    responder.respond(RequestPermissionResponse::new(
                        RequestPermissionOutcome::Selected(SelectedPermissionOutcome::new(
                            option_id,
                        )),
                    ))
                }
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            {
                let callbacks = Arc::clone(&callbacks);
                async move |_request: WriteTextFileRequest, responder, _cx| {
                    callbacks.lock().unwrap().push("fs/write_text_file");
                    responder.respond(WriteTextFileResponse::new())
                }
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            {
                let callbacks = Arc::clone(&callbacks);
                async move |_request: ReadTextFileRequest, responder, _cx| {
                    callbacks.lock().unwrap().push("fs/read_text_file");
                    responder.respond(ReadTextFileResponse::new("read by testy client"))
                }
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            {
                let callbacks = Arc::clone(&callbacks);
                async move |_request: CreateTerminalRequest, responder, _cx| {
                    callbacks.lock().unwrap().push("terminal/create");
                    responder.respond(CreateTerminalResponse::new("testy-terminal-client"))
                }
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            {
                let callbacks = Arc::clone(&callbacks);
                async move |request: TerminalOutputRequest, responder, cx| {
                    callbacks.lock().unwrap().push("terminal/output");
                    cx.send_notification(CancelNotification::new(request.session_id.clone()))?;
                    responder.respond(
                        TerminalOutputResponse::new("terminal output", false)
                            .exit_status(TerminalExitStatus::new().exit_code(0)),
                    )
                }
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            {
                let callbacks = Arc::clone(&callbacks);
                async move |_request: WaitForTerminalExitRequest, responder, _cx| {
                    callbacks.lock().unwrap().push("terminal/wait_for_exit");
                    responder.respond(WaitForTerminalExitResponse::new(
                        TerminalExitStatus::new().exit_code(0),
                    ))
                }
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            {
                let callbacks = Arc::clone(&callbacks);
                async move |_request: KillTerminalRequest, responder, _cx| {
                    callbacks.lock().unwrap().push("terminal/kill");
                    responder.respond(KillTerminalResponse::new())
                }
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            {
                let callbacks = Arc::clone(&callbacks);
                async move |_request: ReleaseTerminalRequest, responder, _cx| {
                    callbacks.lock().unwrap().push("terminal/release");
                    responder.respond(ReleaseTerminalResponse::new())
                }
            },
            agent_client_protocol::on_receive_request!(),
        )
        .connect_with(Testy::new(), async |cx| {
            cx.send_request(InitializeRequest::new(ProtocolVersion::V1))
                .block_task()
                .await?;
            let session = cx
                .send_request(NewSessionRequest::new(PathBuf::from("/tmp")))
                .block_task()
                .await?;
            let response = cx
                .send_request(PromptRequest::new(
                    session.session_id,
                    vec![
                        TestyCommand::RunScenario {
                            scenario: TestyScenario::Callbacks,
                        }
                        .to_prompt()
                        .into(),
                    ],
                ))
                .block_task()
                .await?;
            assert_eq!(response.stop_reason, StopReason::Cancelled);

            Ok(())
        })
        .await?;

    let callbacks = callbacks.lock().unwrap();
    assert_eq!(
        &*callbacks,
        &[
            "session/request_permission",
            "fs/write_text_file",
            "fs/read_text_file",
            "terminal/create",
            "terminal/output",
        ]
    );

    Ok(())
}

#[tokio::test]
async fn testy_uses_unique_message_ids_per_prompt_response()
-> Result<(), agent_client_protocol::Error> {
    let message_ids = Arc::new(Mutex::new(Vec::<String>::new()));

    Client
        .builder()
        .on_receive_notification(
            {
                let message_ids = Arc::clone(&message_ids);
                async move |notification: SessionNotification, _cx| {
                    let SessionUpdate::AgentMessageChunk(chunk) = notification.update else {
                        return Ok(());
                    };
                    let ContentBlock::Text(text) = chunk.content else {
                        return Ok(());
                    };
                    if text.text == "Hello, world!"
                        && let Some(message_id) = chunk.message_id
                    {
                        message_ids.lock().unwrap().push(message_id.to_string());
                    }
                    Ok(())
                }
            },
            agent_client_protocol::on_receive_notification!(),
        )
        .connect_with(Testy::new(), async |cx| {
            cx.send_request(InitializeRequest::new(ProtocolVersion::V1))
                .block_task()
                .await?;
            let session = cx
                .send_request(NewSessionRequest::new(PathBuf::from("/tmp")))
                .block_task()
                .await?;

            for _ in 0..2 {
                let response = cx
                    .send_request(PromptRequest::new(
                        session.session_id.clone(),
                        vec![TestyCommand::Greet.to_prompt().into()],
                    ))
                    .block_task()
                    .await?;
                assert_eq!(response.stop_reason, StopReason::EndTurn);
            }

            Ok(())
        })
        .await?;

    let message_ids = message_ids.lock().unwrap();
    assert_eq!(message_ids.len(), 2);
    assert_ne!(message_ids[0], message_ids[1]);

    Ok(())
}

#[tokio::test]
async fn testy_repeated_scenarios_use_fresh_protocol_ids()
-> Result<(), agent_client_protocol::Error> {
    let message_ids = Arc::new(Mutex::new(Vec::<String>::new()));
    let tool_call_ids = Arc::new(Mutex::new(Vec::<String>::new()));
    let tool_call_update_ids = Arc::new(Mutex::new(Vec::<String>::new()));
    let unknown_tool_call_update_ids = Arc::new(Mutex::new(Vec::<String>::new()));
    let tool_call_statuses = Arc::new(Mutex::new(HashMap::<String, ToolCallStatus>::new()));

    Client
        .builder()
        .on_receive_notification(
            {
                let message_ids = Arc::clone(&message_ids);
                let tool_call_ids = Arc::clone(&tool_call_ids);
                let tool_call_update_ids = Arc::clone(&tool_call_update_ids);
                let unknown_tool_call_update_ids = Arc::clone(&unknown_tool_call_update_ids);
                let tool_call_statuses = Arc::clone(&tool_call_statuses);
                async move |notification: SessionNotification, _cx| {
                    match notification.update {
                        SessionUpdate::UserMessageChunk(chunk)
                        | SessionUpdate::AgentThoughtChunk(chunk)
                        | SessionUpdate::AgentMessageChunk(chunk) => {
                            if let Some(message_id) = chunk.message_id {
                                message_ids.lock().unwrap().push(message_id.to_string());
                            }
                        }
                        SessionUpdate::ToolCall(tool_call) => {
                            let tool_call_id = tool_call.tool_call_id.to_string();
                            tool_call_ids.lock().unwrap().push(tool_call_id.clone());
                            tool_call_statuses
                                .lock()
                                .unwrap()
                                .insert(tool_call_id, tool_call.status);
                        }
                        SessionUpdate::ToolCallUpdate(update) => {
                            let tool_call_id = update.tool_call_id.to_string();
                            if !tool_call_ids.lock().unwrap().contains(&tool_call_id) {
                                unknown_tool_call_update_ids
                                    .lock()
                                    .unwrap()
                                    .push(tool_call_id.clone());
                            }
                            tool_call_update_ids
                                .lock()
                                .unwrap()
                                .push(tool_call_id.clone());
                            if let Some(status) = update.fields.status {
                                tool_call_statuses
                                    .lock()
                                    .unwrap()
                                    .insert(tool_call_id, status);
                            }
                        }
                        _ => {}
                    }
                    Ok(())
                }
            },
            agent_client_protocol::on_receive_notification!(),
        )
        .connect_with(Testy::new(), async |cx| {
            cx.send_request(InitializeRequest::new(ProtocolVersion::V1))
                .block_task()
                .await?;
            let session = cx
                .send_request(NewSessionRequest::new(PathBuf::from("/tmp")))
                .block_task()
                .await?;

            for scenario in [
                TestyScenario::Content,
                TestyScenario::Content,
                TestyScenario::ToolCalls,
                TestyScenario::ToolCalls,
            ] {
                let response = cx
                    .send_request(PromptRequest::new(
                        session.session_id.clone(),
                        vec![TestyCommand::RunScenario { scenario }.to_prompt().into()],
                    ))
                    .block_task()
                    .await?;
                assert_eq!(response.stop_reason, StopReason::EndTurn);
            }

            Ok(())
        })
        .await?;

    let message_ids = message_ids.lock().unwrap();
    assert_all_unique(&message_ids);
    let tool_call_ids = tool_call_ids.lock().unwrap();
    assert_all_unique(&tool_call_ids);
    let tool_call_update_ids = tool_call_update_ids.lock().unwrap();
    assert_eq!(tool_call_update_ids.len(), 6);
    let unknown_tool_call_update_ids = unknown_tool_call_update_ids.lock().unwrap();
    assert!(
        unknown_tool_call_update_ids.is_empty(),
        "updates before tool call announcements: {unknown_tool_call_update_ids:?}"
    );
    let tool_call_statuses = tool_call_statuses.lock().unwrap();
    assert_eq!(tool_call_statuses.len(), tool_call_ids.len());
    assert!(
        tool_call_statuses
            .values()
            .all(|status| *status == ToolCallStatus::Completed),
        "non-completed tool calls: {tool_call_statuses:?}"
    );

    Ok(())
}

#[tokio::test]
async fn testy_full_scenario_exercises_updates_and_callbacks()
-> Result<(), agent_client_protocol::Error> {
    let updates = Arc::new(Mutex::new(Vec::<&'static str>::new()));
    let agent_messages = Arc::new(Mutex::new(Vec::<String>::new()));
    let callbacks = Arc::new(Mutex::new(Vec::<&'static str>::new()));
    let created_terminal_ids = Arc::new(Mutex::new(Vec::<String>::new()));
    let released_terminal_ids = Arc::new(Mutex::new(Vec::<String>::new()));
    let terminal_content_ids = Arc::new(Mutex::new(Vec::<String>::new()));
    #[cfg(feature = "unstable")]
    let elicitation_requests = Arc::new(Mutex::new(Vec::<&'static str>::new()));
    #[cfg(feature = "unstable")]
    let completed_elicitations = Arc::new(Mutex::new(Vec::<String>::new()));

    let builder = Client
        .builder()
        .on_receive_notification(
            {
                let updates = Arc::clone(&updates);
                let agent_messages = Arc::clone(&agent_messages);
                let terminal_content_ids = Arc::clone(&terminal_content_ids);
                async move |notification: SessionNotification, _cx| {
                    let label = match &notification.update {
                        SessionUpdate::UserMessageChunk(_) => "user_message_chunk",
                        SessionUpdate::AgentMessageChunk(chunk) => {
                            if let ContentBlock::Text(text) = &chunk.content {
                                agent_messages.lock().unwrap().push(text.text.clone());
                            }
                            "agent_message_chunk"
                        }
                        SessionUpdate::AgentThoughtChunk(_) => "agent_thought_chunk",
                        SessionUpdate::ToolCall(_) => "tool_call",
                        SessionUpdate::ToolCallUpdate(update) => {
                            if let Some(content) = &update.fields.content {
                                for content in content {
                                    if let ToolCallContent::Terminal(terminal) = content {
                                        terminal_content_ids
                                            .lock()
                                            .unwrap()
                                            .push(terminal.terminal_id.to_string());
                                    }
                                }
                            }
                            "tool_call_update"
                        }
                        SessionUpdate::Plan(_) => "plan",
                        SessionUpdate::AvailableCommandsUpdate(_) => "available_commands_update",
                        SessionUpdate::CurrentModeUpdate(_) => "current_mode_update",
                        SessionUpdate::ConfigOptionUpdate(_) => "config_option_update",
                        SessionUpdate::SessionInfoUpdate(_) => "session_info_update",
                        SessionUpdate::UsageUpdate(_) => "usage_update",
                        _ => "other",
                    };
                    updates.lock().unwrap().push(label);
                    Ok(())
                }
            },
            agent_client_protocol::on_receive_notification!(),
        )
        .on_receive_request(
            {
                let callbacks = Arc::clone(&callbacks);
                async move |request: RequestPermissionRequest, responder, _cx| {
                    callbacks.lock().unwrap().push("session/request_permission");
                    let option_id = request.options.first().map_or_else(
                        || PermissionOptionId::new("allow_once"),
                        |option| option.option_id.clone(),
                    );
                    responder.respond(RequestPermissionResponse::new(
                        RequestPermissionOutcome::Selected(SelectedPermissionOutcome::new(
                            option_id,
                        )),
                    ))
                }
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            {
                let callbacks = Arc::clone(&callbacks);
                async move |_request: WriteTextFileRequest, responder, _cx| {
                    callbacks.lock().unwrap().push("fs/write_text_file");
                    responder.respond(WriteTextFileResponse::new())
                }
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            {
                let callbacks = Arc::clone(&callbacks);
                async move |_request: ReadTextFileRequest, responder, _cx| {
                    callbacks.lock().unwrap().push("fs/read_text_file");
                    responder.respond(ReadTextFileResponse::new("read by testy client"))
                }
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            {
                let callbacks = Arc::clone(&callbacks);
                let created_terminal_ids = Arc::clone(&created_terminal_ids);
                async move |_request: CreateTerminalRequest, responder, _cx| {
                    callbacks.lock().unwrap().push("terminal/create");
                    let terminal_id = {
                        let mut created_terminal_ids = created_terminal_ids.lock().unwrap();
                        let terminal_id =
                            format!("testy-terminal-client-{}", created_terminal_ids.len() + 1);
                        created_terminal_ids.push(terminal_id.clone());
                        terminal_id
                    };
                    responder.respond(CreateTerminalResponse::new(terminal_id))
                }
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            {
                let callbacks = Arc::clone(&callbacks);
                async move |_request: TerminalOutputRequest, responder, _cx| {
                    callbacks.lock().unwrap().push("terminal/output");
                    responder.respond(
                        TerminalOutputResponse::new("terminal output", false)
                            .exit_status(TerminalExitStatus::new().exit_code(0)),
                    )
                }
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            {
                let callbacks = Arc::clone(&callbacks);
                async move |_request: WaitForTerminalExitRequest, responder, _cx| {
                    callbacks.lock().unwrap().push("terminal/wait_for_exit");
                    responder.respond(WaitForTerminalExitResponse::new(
                        TerminalExitStatus::new().exit_code(0),
                    ))
                }
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            {
                let callbacks = Arc::clone(&callbacks);
                async move |_request: KillTerminalRequest, responder, _cx| {
                    callbacks.lock().unwrap().push("terminal/kill");
                    responder.respond(KillTerminalResponse::new())
                }
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            {
                let callbacks = Arc::clone(&callbacks);
                let released_terminal_ids = Arc::clone(&released_terminal_ids);
                async move |request: ReleaseTerminalRequest, responder, _cx| {
                    callbacks.lock().unwrap().push("terminal/release");
                    released_terminal_ids
                        .lock()
                        .unwrap()
                        .push(request.terminal_id.to_string());
                    responder.respond(ReleaseTerminalResponse::new())
                }
            },
            agent_client_protocol::on_receive_request!(),
        );

    #[cfg(feature = "unstable")]
    let builder = builder
        .on_receive_notification(
            {
                let completed_elicitations = Arc::clone(&completed_elicitations);
                async move |notification: CompleteElicitationNotification, _cx| {
                    completed_elicitations
                        .lock()
                        .unwrap()
                        .push(notification.elicitation_id.to_string());
                    Ok(())
                }
            },
            agent_client_protocol::on_receive_notification!(),
        )
        .on_receive_request(
            {
                let elicitation_requests = Arc::clone(&elicitation_requests);
                async move |request: CreateElicitationRequest, responder, _cx| {
                    let label = testy_elicitation_request_label(&request);
                    elicitation_requests.lock().unwrap().push(label);

                    let action = match label {
                        "form_session_accept" => ElicitationAction::Accept(
                            ElicitationAcceptAction::new().content(BTreeMap::from([
                                ("name".to_string(), ElicitationContentValue::from("Ada")),
                                ("age".to_string(), ElicitationContentValue::from(42_i32)),
                                (
                                    "confidence".to_string(),
                                    ElicitationContentValue::from(0.95_f64),
                                ),
                                ("confirmed".to_string(), ElicitationContentValue::from(true)),
                                (
                                    "tags".to_string(),
                                    ElicitationContentValue::from(vec!["rust", "acp"]),
                                ),
                            ])),
                        ),
                        "form_session_decline" | "url_request_decline" => {
                            ElicitationAction::Decline
                        }
                        "form_request_cancel" => ElicitationAction::Cancel,
                        "url_session_accept" => {
                            ElicitationAction::Accept(ElicitationAcceptAction::new())
                        }
                        other => panic!("unexpected elicitation request label: {other}"),
                    };

                    responder.respond(CreateElicitationResponse::new(action))
                }
            },
            agent_client_protocol::on_receive_request!(),
        );

    builder
        .connect_with(Testy::new(), async |cx| {
            let initialize = InitializeRequest::new(ProtocolVersion::V1);
            #[cfg(feature = "unstable")]
            let initialize = initialize.client_capabilities(
                ClientCapabilities::new().elicitation(
                    ElicitationCapabilities::new()
                        .form(ElicitationFormCapabilities::new())
                        .url(ElicitationUrlCapabilities::new()),
                ),
            );
            cx.send_request(initialize).block_task().await?;
            let session = cx
                .send_request(NewSessionRequest::new(PathBuf::from("/tmp")))
                .block_task()
                .await?;
            let response = cx
                .send_request(PromptRequest::new(
                    session.session_id,
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
            assert_eq!(response.stop_reason, StopReason::EndTurn);
            Ok(())
        })
        .await?;

    let updates = updates.lock().unwrap();
    for expected in [
        "session_info_update",
        "current_mode_update",
        "config_option_update",
        "available_commands_update",
        "usage_update",
        "user_message_chunk",
        "agent_thought_chunk",
        "agent_message_chunk",
        "tool_call",
        "tool_call_update",
        "plan",
    ] {
        assert!(updates.contains(&expected), "missing update: {expected}");
    }

    let callbacks = callbacks.lock().unwrap();
    assert_eq!(
        callbacks.as_slice(),
        [
            "terminal/create",
            "terminal/release",
            "session/request_permission",
            "fs/write_text_file",
            "fs/read_text_file",
            "terminal/create",
            "terminal/output",
            "terminal/wait_for_exit",
            "terminal/kill",
            "terminal/release",
        ]
    );

    let messages = agent_messages.lock().unwrap().join("\n");
    assert!(messages.contains("scenario: full"));
    assert!(messages.contains("terminal/release_for_tool_call: ok"));
    assert!(messages.contains("terminal/release: ok"));
    #[cfg(feature = "unstable")]
    for expected in [
        "elicitation/form_session_accept: ok accept content_fields=5",
        "elicitation/form_session_decline: ok decline",
        "elicitation/form_request_cancel: ok cancel",
        "elicitation/url_session_accept: ok accept content_fields=0",
        "elicitation/complete_session_url: sent",
        "elicitation/url_request_decline: ok decline",
        "elicitations: completed",
    ] {
        assert!(
            messages.contains(expected),
            "missing report line: {expected}"
        );
    }

    #[cfg(feature = "unstable")]
    {
        assert_eq!(
            elicitation_requests.lock().unwrap().as_slice(),
            [
                "form_session_accept",
                "form_session_decline",
                "form_request_cancel",
                "url_session_accept",
                "url_request_decline",
            ]
        );
        assert_eq!(
            completed_elicitations.lock().unwrap().as_slice(),
            ["testy-url-session"]
        );
    }

    let created_terminal_ids = created_terminal_ids.lock().unwrap();
    let released_terminal_ids = released_terminal_ids.lock().unwrap();
    let terminal_content_ids = terminal_content_ids.lock().unwrap();
    assert!(!terminal_content_ids.is_empty());
    for terminal_id in terminal_content_ids.iter() {
        assert!(
            created_terminal_ids.contains(terminal_id),
            "terminal content referenced uncreated terminal: {terminal_id}"
        );
        assert!(
            released_terminal_ids.contains(terminal_id),
            "terminal content terminal was not released: {terminal_id}"
        );
    }

    Ok(())
}

#[cfg(feature = "unstable")]
#[tokio::test]
async fn testy_elicitations_prompt_exercises_all_elicitation_create_and_complete_paths()
-> Result<(), agent_client_protocol::Error> {
    let agent_messages = Arc::new(Mutex::new(Vec::<String>::new()));
    let requests = Arc::new(Mutex::new(Vec::<&'static str>::new()));
    let completions = Arc::new(Mutex::new(Vec::<String>::new()));

    Client
        .builder()
        .on_receive_notification(
            {
                let agent_messages = Arc::clone(&agent_messages);
                async move |notification: SessionNotification, _cx| {
                    let SessionUpdate::AgentMessageChunk(chunk) = notification.update else {
                        return Ok(());
                    };
                    let ContentBlock::Text(text) = chunk.content else {
                        return Ok(());
                    };
                    agent_messages.lock().unwrap().push(text.text);
                    Ok(())
                }
            },
            agent_client_protocol::on_receive_notification!(),
        )
        .on_receive_notification(
            {
                let completions = Arc::clone(&completions);
                async move |notification: CompleteElicitationNotification, _cx| {
                    completions
                        .lock()
                        .unwrap()
                        .push(notification.elicitation_id.to_string());
                    Ok(())
                }
            },
            agent_client_protocol::on_receive_notification!(),
        )
        .on_receive_request(
            async move |request: RequestPermissionRequest, responder, _cx| {
                let option_id = request.options.first().map_or_else(
                    || PermissionOptionId::new("allow_once"),
                    |option| option.option_id.clone(),
                );
                responder.respond(RequestPermissionResponse::new(
                    RequestPermissionOutcome::Selected(SelectedPermissionOutcome::new(option_id)),
                ))
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            async move |_request: WriteTextFileRequest, responder, _cx| {
                responder.respond(WriteTextFileResponse::new())
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            async move |_request: ReadTextFileRequest, responder, _cx| {
                responder.respond(ReadTextFileResponse::new("read by testy client"))
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            async move |_request: CreateTerminalRequest, responder, _cx| {
                responder.respond(CreateTerminalResponse::new("testy-terminal-client"))
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            async move |_request: TerminalOutputRequest, responder, _cx| {
                responder.respond(
                    TerminalOutputResponse::new("terminal output", false)
                        .exit_status(TerminalExitStatus::new().exit_code(0)),
                )
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            async move |_request: WaitForTerminalExitRequest, responder, _cx| {
                responder.respond(WaitForTerminalExitResponse::new(
                    TerminalExitStatus::new().exit_code(0),
                ))
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            async move |_request: KillTerminalRequest, responder, _cx| {
                responder.respond(KillTerminalResponse::new())
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            async move |_request: ReleaseTerminalRequest, responder, _cx| {
                responder.respond(ReleaseTerminalResponse::new())
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            {
                let requests = Arc::clone(&requests);
                async move |request: CreateElicitationRequest, responder, _cx| {
                    let label = testy_elicitation_request_label(&request);
                    requests.lock().unwrap().push(label);

                    let action = match label {
                        "form_session_accept" => ElicitationAction::Accept(
                            ElicitationAcceptAction::new().content(BTreeMap::from([
                                ("name".to_string(), ElicitationContentValue::from("Ada")),
                                ("age".to_string(), ElicitationContentValue::from(42_i32)),
                                (
                                    "confidence".to_string(),
                                    ElicitationContentValue::from(0.95_f64),
                                ),
                                ("confirmed".to_string(), ElicitationContentValue::from(true)),
                                (
                                    "tags".to_string(),
                                    ElicitationContentValue::from(vec!["rust", "acp"]),
                                ),
                            ])),
                        ),
                        "form_session_decline" | "url_request_decline" => {
                            ElicitationAction::Decline
                        }
                        "form_request_cancel" => ElicitationAction::Cancel,
                        "url_session_accept" => {
                            ElicitationAction::Accept(ElicitationAcceptAction::new())
                        }
                        other => panic!("unexpected elicitation request label: {other}"),
                    };

                    responder.respond(CreateElicitationResponse::new(action))
                }
            },
            agent_client_protocol::on_receive_request!(),
        )
        .connect_with(Testy::new(), async |cx| {
            let initialize = InitializeRequest::new(ProtocolVersion::V1).client_capabilities(
                ClientCapabilities::new().elicitation(
                    ElicitationCapabilities::new()
                        .form(ElicitationFormCapabilities::new())
                        .url(ElicitationUrlCapabilities::new()),
                ),
            );
            cx.send_request(initialize).block_task().await?;
            let session = cx
                .send_request(NewSessionRequest::new(PathBuf::from("/tmp")))
                .block_task()
                .await?;

            let response = cx
                .send_request(PromptRequest::new(
                    session.session_id,
                    vec!["elicitations".to_string().into()],
                ))
                .block_task()
                .await?;
            assert_eq!(response.stop_reason, StopReason::EndTurn);
            Ok(())
        })
        .await?;

    assert_eq!(
        requests.lock().unwrap().as_slice(),
        [
            "form_session_accept",
            "form_session_decline",
            "form_request_cancel",
            "url_session_accept",
            "url_request_decline",
        ]
    );
    assert_eq!(
        completions.lock().unwrap().as_slice(),
        ["testy-url-session"]
    );

    let messages = agent_messages.lock().unwrap().join("\n");
    for expected in [
        "scenario: elicitations",
        "elicitation/form_session_accept: ok accept content_fields=5",
        "elicitation/form_session_decline: ok decline",
        "elicitation/form_request_cancel: ok cancel",
        "elicitation/url_session_accept: ok accept content_fields=0",
        "elicitation/complete_session_url: sent",
        "elicitation/url_request_decline: ok decline",
        "elicitations: completed",
    ] {
        assert!(
            messages.contains(expected),
            "missing report line: {expected}"
        );
    }

    Ok(())
}

#[cfg(feature = "unstable")]
#[tokio::test]
async fn testy_callbacks_with_unstable_feature_returns_url_required_when_url_elicitation_is_unsupported()
-> Result<(), agent_client_protocol::Error> {
    let requests = Arc::new(Mutex::new(Vec::<&'static str>::new()));

    Client
        .builder()
        .on_receive_request(
            async move |request: RequestPermissionRequest, responder, _cx| {
                let option_id = request.options.first().map_or_else(
                    || PermissionOptionId::new("allow_once"),
                    |option| option.option_id.clone(),
                );
                responder.respond(RequestPermissionResponse::new(
                    RequestPermissionOutcome::Selected(SelectedPermissionOutcome::new(option_id)),
                ))
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            async move |_request: WriteTextFileRequest, responder, _cx| {
                responder.respond(WriteTextFileResponse::new())
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            async move |_request: ReadTextFileRequest, responder, _cx| {
                responder.respond(ReadTextFileResponse::new("read by testy client"))
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            async move |_request: CreateTerminalRequest, responder, _cx| {
                responder.respond(CreateTerminalResponse::new("testy-terminal-client"))
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            async move |_request: TerminalOutputRequest, responder, _cx| {
                responder.respond(
                    TerminalOutputResponse::new("terminal output", false)
                        .exit_status(TerminalExitStatus::new().exit_code(0)),
                )
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            async move |_request: WaitForTerminalExitRequest, responder, _cx| {
                responder.respond(WaitForTerminalExitResponse::new(
                    TerminalExitStatus::new().exit_code(0),
                ))
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            async move |_request: KillTerminalRequest, responder, _cx| {
                responder.respond(KillTerminalResponse::new())
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            async move |_request: ReleaseTerminalRequest, responder, _cx| {
                responder.respond(ReleaseTerminalResponse::new())
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            {
                let requests = Arc::clone(&requests);
                async move |request: CreateElicitationRequest, responder, _cx| {
                    let label = testy_elicitation_request_label(&request);
                    requests.lock().unwrap().push(label);
                    let action = match label {
                        "form_session_accept" => ElicitationAction::Accept(
                            ElicitationAcceptAction::new().content(BTreeMap::from([
                                ("name".to_string(), ElicitationContentValue::from("Ada")),
                                ("age".to_string(), ElicitationContentValue::from(42_i32)),
                                (
                                    "confidence".to_string(),
                                    ElicitationContentValue::from(0.95_f64),
                                ),
                                ("confirmed".to_string(), ElicitationContentValue::from(true)),
                            ])),
                        ),
                        "form_session_decline" => ElicitationAction::Decline,
                        "form_request_cancel" => ElicitationAction::Cancel,
                        other => panic!("unexpected elicitation request before URL error: {other}"),
                    };
                    responder.respond(CreateElicitationResponse::new(action))
                }
            },
            agent_client_protocol::on_receive_request!(),
        )
        .connect_with(Testy::new(), async |cx| {
            let initialize = InitializeRequest::new(ProtocolVersion::V1).client_capabilities(
                ClientCapabilities::new().elicitation(
                    ElicitationCapabilities::new().form(ElicitationFormCapabilities::new()),
                ),
            );
            cx.send_request(initialize).block_task().await?;
            let session = cx
                .send_request(NewSessionRequest::new(PathBuf::from("/tmp")))
                .block_task()
                .await?;

            let error = cx
                .send_request(PromptRequest::new(
                    session.session_id,
                    vec![
                        TestyCommand::RunScenario {
                            scenario: TestyScenario::Callbacks,
                        }
                        .to_prompt()
                        .into(),
                    ],
                ))
                .block_task()
                .await
                .expect_err("url-required elicitation scenario should fail the prompt request");
            assert_eq!(error.code, ErrorCode::UrlElicitationRequired);

            let data: UrlElicitationRequiredData =
                serde_json::from_value(error.data.expect("url-required error should include data"))
                    .expect("url-required error data should deserialize");
            assert_eq!(data.elicitations.len(), 1);
            let elicitation = &data.elicitations[0];
            assert_eq!(elicitation.elicitation_id.to_string(), "testy-url-required");
            assert_eq!(elicitation.url, "https://example.com/testy/required");
            assert_eq!(
                elicitation.message,
                "Complete the Testy URL elicitation before continuing"
            );
            Ok(())
        })
        .await?;

    assert_eq!(
        requests.lock().unwrap().as_slice(),
        [
            "form_session_accept",
            "form_session_decline",
            "form_request_cancel",
        ]
    );

    Ok(())
}

#[cfg(feature = "unstable")]
fn testy_elicitation_request_label(request: &CreateElicitationRequest) -> &'static str {
    match request.message.as_str() {
        "Accept the Testy session-scoped form elicitation" => {
            let ElicitationMode::Form(form) = &request.mode else {
                panic!("expected form mode for session accept");
            };
            let ElicitationScope::Session(scope) = &form.scope else {
                panic!("expected session scope for form accept");
            };
            assert!(scope.tool_call_id.is_some());
            for property in [
                "name",
                "email",
                "homepage",
                "birthday",
                "available_at",
                "confidence",
                "age",
                "confirmed",
                "priority",
                "tags",
            ] {
                assert!(
                    form.requested_schema.properties.contains_key(property),
                    "missing schema property: {property}"
                );
            }
            "form_session_accept"
        }
        "Decline the Testy session-scoped form elicitation" => {
            assert!(matches!(
                &request.mode,
                ElicitationMode::Form(form)
                    if matches!(&form.scope, ElicitationScope::Session(scope) if scope.tool_call_id.is_none())
            ));
            "form_session_decline"
        }
        "Cancel the Testy request-scoped form elicitation" => {
            assert!(matches!(
                &request.mode,
                ElicitationMode::Form(form)
                    if matches!(&form.scope, ElicitationScope::Request(_))
            ));
            "form_request_cancel"
        }
        "Accept the Testy session-scoped URL elicitation" => {
            let ElicitationMode::Url(url) = &request.mode else {
                panic!("expected URL mode for session URL accept");
            };
            assert!(matches!(&url.scope, ElicitationScope::Session(_)));
            assert_eq!(url.elicitation_id.to_string(), "testy-url-session");
            assert_eq!(url.url, "https://example.com/testy/session");
            "url_session_accept"
        }
        "Decline the Testy request-scoped URL elicitation" => {
            let ElicitationMode::Url(url) = &request.mode else {
                panic!("expected URL mode for request URL decline");
            };
            assert!(matches!(&url.scope, ElicitationScope::Request(_)));
            assert_eq!(url.elicitation_id.to_string(), "testy-url-request");
            assert_eq!(url.url, "https://example.com/testy/request");
            "url_request_decline"
        }
        other => panic!("unexpected elicitation request message: {other}"),
    }
}

fn assert_all_unique(values: &[String]) {
    let unique = values.iter().collect::<HashSet<_>>();
    assert_eq!(unique.len(), values.len(), "duplicate values: {values:?}");
}

fn hanging_mcp_server() -> McpServer {
    if cfg!(windows) {
        McpServer::Stdio(
            McpServerStdio::new("hung", "cmd")
                .args(vec!["/C".to_string(), "more >NUL".to_string()]),
        )
    } else {
        McpServer::Stdio(
            McpServerStdio::new("hung", "sh")
                .args(vec!["-c".to_string(), "cat >/dev/null".to_string()]),
        )
    }
}
