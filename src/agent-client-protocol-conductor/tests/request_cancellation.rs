#![cfg(feature = "unstable_cancel_request")]

//! Integration tests for `$/cancel_request` propagation through the conductor.
//!
//! Cancellation is hop-by-hop: every hop re-issues `$/cancel_request` with
//! its own connection's request ID, and the raw notification (whose
//! `requestId` is only meaningful on the connection it arrived over) must
//! *not* be tunneled verbatim through the chain.
//!
//! These tests avoid sleeps:
//!
//! - Channels report what each endpoint observed, awaited with a timeout.
//! - "Exactly one cancellation" assertions rely on a barrier round trip
//!   through the whole chain: the conductor's routing loop and each
//!   connection deliver messages in order, so by the time the barrier
//!   response arrives, any erroneously tunneled notification would already
//!   have been observed.

use std::time::Duration;

use agent_client_protocol::schema::{
    CancelRequestNotification, ContentBlock, ContentChunk, InitializeRequest, InitializeResponse,
    NewSessionRequest, NewSessionResponse, PermissionOption, PermissionOptionKind, PromptRequest,
    PromptResponse, ProtocolVersion, RequestPermissionRequest, RequestPermissionResponse,
    SessionId, SessionNotification, SessionUpdate, StopReason, ToolCallUpdate,
    ToolCallUpdateFields,
};
use agent_client_protocol::{
    Agent, ByteStreams, Client, Conductor, ConnectTo, ConnectionTo, Error, JsonRpcRequest,
    JsonRpcResponse, Proxy, Responder, SentRequest,
};
use agent_client_protocol_conductor::{ConductorImpl, ProxiesAndAgent};
use futures::StreamExt as _;
use futures::channel::mpsc;
use serde::{Deserialize, Serialize};
use tokio::io::duplex;
use tokio_util::compat::{TokioAsyncReadCompatExt, TokioAsyncWriteCompatExt};

#[derive(Debug, Clone, Serialize, Deserialize, JsonRpcRequest)]
#[request(method = "test/simple", response = SimpleResponse)]
struct SimpleRequest {
    message: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonRpcResponse)]
struct SimpleResponse {
    result: String,
}

/// Await the next item on `rx`, panicking instead of hanging if it never
/// arrives.
async fn next_with_timeout<T>(rx: &mut mpsc::UnboundedReceiver<T>) -> T {
    tokio::time::timeout(Duration::from_secs(10), rx.next())
        .await
        .expect("timed out waiting for channel event")
        .expect("channel closed before expected event")
}

/// Assert that no item is currently buffered on `rx`.
///
/// Callers must first establish an ordering barrier (such as a round trip
/// through the whole chain) that guarantees any erroneously sent
/// notification would already have been observed.
fn assert_no_event<T: std::fmt::Debug>(rx: &mut mpsc::UnboundedReceiver<T>) {
    if let Ok(event) = rx.try_recv() {
        panic!("unexpected event: {event:?}");
    }
}

/// The real intercepting proxy from the test fixtures, run in-process.
struct InProcessArrowProxy;

impl ConnectTo<Conductor> for InProcessArrowProxy {
    async fn connect_to(self, client: impl ConnectTo<Proxy>) -> Result<(), Error> {
        agent_client_protocol_test::arrow_proxy::run_arrow_proxy(client).await
    }
}

fn prompt_text(request: &PromptRequest) -> String {
    request
        .prompt
        .iter()
        .filter_map(|block| match block {
            ContentBlock::Text(text) => Some(text.text.as_str()),
            _ => None,
        })
        .collect()
}

/// A client cancels a request it sent through the conductor (and a
/// passthrough proxy) to the agent.
///
/// The agent must observe exactly one `$/cancel_request`, carrying the ID of
/// the request on the conductor-to-agent connection — not the client's raw
/// notification with its hop-local request ID.
#[tokio::test]
async fn client_cancellation_propagates_hop_by_hop_to_agent() -> Result<(), Error> {
    let (agent_cancel_tx, mut agent_cancel_rx) = mpsc::unbounded();
    // The JSON-RPC id of the parked request, as seen by the agent.
    let (parked_id_tx, mut parked_id_rx) = mpsc::unbounded();

    let agent = Agent
        .builder()
        .on_receive_request(
            async |initialize: InitializeRequest, responder, _cx: ConnectionTo<Client>| {
                responder.respond(InitializeResponse::new(initialize.protocol_version))
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            async move |request: SimpleRequest,
                        responder: Responder<SimpleResponse>,
                        cx: ConnectionTo<Client>| {
                if request.message == "park" {
                    parked_id_tx.unbounded_send(responder.id()).unwrap();
                    let cancellation = responder.cancellation();
                    cx.spawn(async move {
                        let response = cancellation
                            .run_until_cancelled(std::future::pending::<
                                Result<SimpleResponse, Error>,
                            >())
                            .await;
                        responder.respond_with_result(response)
                    })?;
                    return Ok(());
                }

                responder.respond(SimpleResponse {
                    result: format!("echo: {}", request.message),
                })
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_notification(
            async move |cancel: CancelRequestNotification, _cx: ConnectionTo<Client>| {
                agent_cancel_tx.unbounded_send(cancel.request_id).unwrap();
                Ok(())
            },
            agent_client_protocol::on_receive_notification!(),
        );

    let (editor_write, conductor_read) = duplex(8192);
    let (conductor_write, editor_read) = duplex(8192);

    // One passthrough proxy in the chain, so the cancellation crosses every
    // kind of hop: client→conductor, conductor→proxy, proxy→conductor
    // (successor-wrapped), and conductor→agent.
    let conductor_handle = tokio::spawn(async move {
        ConductorImpl::new_agent(
            "cancellation-conductor".to_string(),
            ProxiesAndAgent::new(agent).proxy(Proxy.builder()),
        )
        .run(ByteStreams::new(
            conductor_write.compat_write(),
            conductor_read.compat(),
        ))
        .await
    });

    let client_request_id = tokio::time::timeout(Duration::from_secs(30), async move {
        Client
            .builder()
            .connect_with(
                ByteStreams::new(editor_write.compat_write(), editor_read.compat()),
                async |cx| {
                    let initialize = cx
                        .send_request(InitializeRequest::new(ProtocolVersion::V1))
                        .block_task()
                        .await?;
                    assert_eq!(initialize.protocol_version, ProtocolVersion::V1);

                    let request: SentRequest<SimpleResponse> = cx.send_request(SimpleRequest {
                        message: "park".into(),
                    });
                    let client_request_id = request.id();
                    request.cancel()?;

                    // The cancellation reaches the agent hop by hop, and the
                    // agent's cancellation error flows back the same way.
                    let error = request
                        .block_task()
                        .await
                        .expect_err("request should be cancelled");
                    assert_eq!(i32::from(error.code), -32800);

                    // Barrier: this round trip traverses the whole chain
                    // after the cancellation, so a tunneled raw
                    // `$/cancel_request` would already have been recorded by
                    // the agent.
                    let barrier = cx
                        .send_request(SimpleRequest {
                            message: "barrier".into(),
                        })
                        .block_task()
                        .await?;
                    assert_eq!(barrier.result, "echo: barrier");

                    Ok(client_request_id)
                },
            )
            .await
    })
    .await
    .expect("test timed out")
    .expect("client failed");

    // The agent saw exactly one `$/cancel_request`, for the request ID on
    // its own connection.
    let parked_id = next_with_timeout(&mut parked_id_rx).await;
    assert_ne!(
        parked_id, client_request_id,
        "each hop must re-issue the request under its own ID"
    );
    let observed = next_with_timeout(&mut agent_cancel_rx).await;
    assert_eq!(serde_json::to_value(observed).unwrap(), parked_id);
    assert_no_event(&mut agent_cancel_rx);

    conductor_handle.abort();
    Ok(())
}

/// The agent cancels a request it sent through the conductor to the client
/// (the right-to-left direction).
///
/// The client must observe exactly one `$/cancel_request`, carrying the ID
/// of the request on the client-to-conductor connection — not the agent's
/// raw notification with its hop-local request ID.
#[tokio::test]
async fn agent_cancellation_propagates_hop_by_hop_to_client() -> Result<(), Error> {
    let (client_cancel_tx, mut client_cancel_rx) = mpsc::unbounded();
    // The JSON-RPC id of the parked request, as seen by the client.
    let (parked_id_tx, mut parked_id_rx) = mpsc::unbounded();

    let agent = Agent
        .builder()
        .on_receive_request(
            async |initialize: InitializeRequest, responder, _cx: ConnectionTo<Client>| {
                responder.respond(InitializeResponse::new(initialize.protocol_version))
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            async |request: SimpleRequest,
                   responder: Responder<SimpleResponse>,
                   cx: ConnectionTo<Client>| {
                if request.message == "trigger reverse cancel" {
                    let connection = cx.clone();
                    cx.spawn(async move {
                        // Send a request to the client, cancel it, and report
                        // how it concluded as the response to the trigger.
                        let upstream: SentRequest<SimpleResponse> =
                            connection.send_request(SimpleRequest {
                                message: "park".into(),
                            });
                        upstream.cancel()?;
                        let error = upstream
                            .block_task()
                            .await
                            .expect_err("request to the client should be cancelled");
                        responder.respond(SimpleResponse {
                            result: format!("client request error: {}", i32::from(error.code)),
                        })
                    })?;
                    return Ok(());
                }

                responder.respond(SimpleResponse {
                    result: format!("echo: {}", request.message),
                })
            },
            agent_client_protocol::on_receive_request!(),
        );

    let (editor_write, conductor_read) = duplex(8192);
    let (conductor_write, editor_read) = duplex(8192);

    let conductor_handle = tokio::spawn(async move {
        ConductorImpl::new_agent(
            "cancellation-conductor".to_string(),
            ProxiesAndAgent::new(agent),
        )
        .run(ByteStreams::new(
            conductor_write.compat_write(),
            conductor_read.compat(),
        ))
        .await
    });

    tokio::time::timeout(Duration::from_secs(30), async move {
        Client
            .builder()
            .on_receive_request(
                async move |request: SimpleRequest,
                            responder: Responder<SimpleResponse>,
                            cx: ConnectionTo<Agent>| {
                    assert_eq!(request.message, "park");
                    parked_id_tx.unbounded_send(responder.id()).unwrap();
                    let cancellation = responder.cancellation();
                    cx.spawn(async move {
                        let response = cancellation
                            .run_until_cancelled(std::future::pending::<
                                Result<SimpleResponse, Error>,
                            >())
                            .await;
                        responder.respond_with_result(response)
                    })?;
                    Ok(())
                },
                agent_client_protocol::on_receive_request!(),
            )
            .on_receive_notification(
                async move |cancel: CancelRequestNotification, _cx: ConnectionTo<Agent>| {
                    client_cancel_tx.unbounded_send(cancel.request_id).unwrap();
                    Ok(())
                },
                agent_client_protocol::on_receive_notification!(),
            )
            .connect_with(
                ByteStreams::new(editor_write.compat_write(), editor_read.compat()),
                async |cx| {
                    let initialize = cx
                        .send_request(InitializeRequest::new(ProtocolVersion::V1))
                        .block_task()
                        .await?;
                    assert_eq!(initialize.protocol_version, ProtocolVersion::V1);

                    // The agent answers the trigger only after its request to
                    // the client was cancelled and answered with the standard
                    // cancellation error.
                    let response = cx
                        .send_request(SimpleRequest {
                            message: "trigger reverse cancel".into(),
                        })
                        .block_task()
                        .await?;
                    assert_eq!(response.result, "client request error: -32800");

                    // Barrier: a tunneled raw `$/cancel_request` was queued
                    // in the conductor's routing loop before this request, so
                    // it would already have been recorded by the client.
                    let barrier = cx
                        .send_request(SimpleRequest {
                            message: "barrier".into(),
                        })
                        .block_task()
                        .await?;
                    assert_eq!(barrier.result, "echo: barrier");

                    Ok(())
                },
            )
            .await
    })
    .await
    .expect("test timed out")
    .expect("client failed");

    // The client saw exactly one `$/cancel_request`, for the request ID on
    // its own connection.
    let parked_id = next_with_timeout(&mut parked_id_rx).await;
    let observed = next_with_timeout(&mut client_cancel_rx).await;
    assert_eq!(serde_json::to_value(observed).unwrap(), parked_id);
    assert_no_event(&mut client_cancel_rx);

    conductor_handle.abort();
    Ok(())
}

/// The canonical real-world cancellation cascade, through a real intercepting
/// proxy (`arrow_proxy`) and a full ACP session:
///
/// 1. The client initializes, creates a session, and sends `session/prompt`.
/// 2. The agent asks the client for permission (`session/request_permission`)
///    and waits.
/// 3. The client cancels the *prompt*; the agent reacts by cancelling its
///    outstanding *permission request*, then answers the prompt with the
///    cancellation error.
///
/// Both cancellations must arrive re-issued with the receiving connection's
/// own request ID — exactly once each — and the chain (including the
/// transforming proxy) must keep working afterwards.
#[tokio::test]
async fn prompt_cancellation_cascades_through_real_proxy_chain() -> Result<(), Error> {
    // What the agent observed: incoming `$/cancel_request`s and the id of the
    // parked prompt.
    let (agent_cancel_tx, mut agent_cancel_rx) = mpsc::unbounded();
    let (prompt_id_tx, mut prompt_id_rx) = mpsc::unbounded();
    // What the client observed: incoming `$/cancel_request`s, the id of the
    // parked permission request, and session updates.
    let (client_cancel_tx, mut client_cancel_rx) = mpsc::unbounded();
    let (permission_id_tx, mut permission_id_rx) = mpsc::unbounded();
    let (session_update_tx, mut session_update_rx) = mpsc::unbounded();

    let agent = Agent
        .builder()
        .on_receive_request(
            async |initialize: InitializeRequest, responder, _cx: ConnectionTo<Client>| {
                responder.respond(InitializeResponse::new(initialize.protocol_version))
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            async |request: NewSessionRequest, responder, _cx: ConnectionTo<Client>| {
                assert!(request.mcp_servers.is_empty());
                responder.respond(NewSessionResponse::new(SessionId::new("test-session")))
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            async move |request: PromptRequest,
                        responder: Responder<PromptResponse>,
                        cx: ConnectionTo<Client>| {
                let text = prompt_text(&request);
                if text != "park" {
                    // Echo prompts complete normally, with a session update
                    // the arrow proxy will transform on its way back.
                    cx.send_notification(SessionNotification::new(
                        request.session_id,
                        SessionUpdate::AgentMessageChunk(ContentChunk::new(text.into())),
                    ))?;
                    return responder.respond(PromptResponse::new(StopReason::EndTurn));
                }

                prompt_id_tx.unbounded_send(responder.id()).unwrap();
                let cancellation = responder.cancellation();
                let connection = cx.clone();
                cx.spawn(async move {
                    // Ask the client for permission through the chain.
                    let permission: SentRequest<RequestPermissionResponse> = connection
                        .send_request(RequestPermissionRequest::new(
                            request.session_id,
                            ToolCallUpdate::new("tool-1", ToolCallUpdateFields::default()),
                            vec![PermissionOption::new(
                                "allow",
                                "Allow",
                                PermissionOptionKind::AllowOnce,
                            )],
                        ));

                    // The client cancels the prompt rather than answering the
                    // permission request.
                    cancellation.cancelled().await;

                    // React like a real agent: withdraw the outstanding
                    // permission request, then report the prompt as
                    // cancelled.
                    permission.cancel()?;
                    let permission_error = permission
                        .block_task()
                        .await
                        .expect_err("permission request should be cancelled");

                    if i32::from(permission_error.code) == -32800 {
                        responder.respond_with_result(Err(Error::request_cancelled()))
                    } else {
                        responder.respond_with_result(Err(
                            agent_client_protocol::util::internal_error(format!(
                                "unexpected permission error: {permission_error:?}"
                            )),
                        ))
                    }
                })?;
                Ok(())
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_notification(
            async move |cancel: CancelRequestNotification, _cx: ConnectionTo<Client>| {
                agent_cancel_tx.unbounded_send(cancel.request_id).unwrap();
                Ok(())
            },
            agent_client_protocol::on_receive_notification!(),
        );

    let (editor_write, conductor_read) = duplex(8192);
    let (conductor_write, editor_read) = duplex(8192);

    let conductor_handle = tokio::spawn(async move {
        ConductorImpl::new_agent(
            "cancellation-conductor".to_string(),
            ProxiesAndAgent::new(agent).proxy(InProcessArrowProxy),
        )
        .run(ByteStreams::new(
            conductor_write.compat_write(),
            conductor_read.compat(),
        ))
        .await
    });

    let client_prompt_id = tokio::time::timeout(Duration::from_secs(30), async move {
        Client
            .builder()
            .on_receive_request(
                async move |_request: RequestPermissionRequest,
                            responder: Responder<RequestPermissionResponse>,
                            cx: ConnectionTo<Agent>| {
                    permission_id_tx.unbounded_send(responder.id()).unwrap();
                    let cancellation = responder.cancellation();
                    cx.spawn(async move {
                        let response = cancellation
                            .run_until_cancelled(std::future::pending::<
                                Result<RequestPermissionResponse, Error>,
                            >())
                            .await;
                        responder.respond_with_result(response)
                    })?;
                    Ok(())
                },
                agent_client_protocol::on_receive_request!(),
            )
            .on_receive_notification(
                async move |notification: SessionNotification, _cx: ConnectionTo<Agent>| {
                    if let SessionUpdate::AgentMessageChunk(ContentChunk {
                        content: ContentBlock::Text(text),
                        ..
                    }) = notification.update
                    {
                        session_update_tx.unbounded_send(text.text).unwrap();
                    }
                    Ok(())
                },
                agent_client_protocol::on_receive_notification!(),
            )
            .on_receive_notification(
                async move |cancel: CancelRequestNotification, _cx: ConnectionTo<Agent>| {
                    client_cancel_tx.unbounded_send(cancel.request_id).unwrap();
                    Ok(())
                },
                agent_client_protocol::on_receive_notification!(),
            )
            .connect_with(
                ByteStreams::new(editor_write.compat_write(), editor_read.compat()),
                async |cx| {
                    let initialize = cx
                        .send_request(InitializeRequest::new(ProtocolVersion::V1))
                        .block_task()
                        .await?;
                    assert_eq!(initialize.protocol_version, ProtocolVersion::V1);

                    let session = cx
                        .send_request(NewSessionRequest::new(
                            std::env::current_dir().map_err(Error::into_internal_error)?,
                        ))
                        .block_task()
                        .await?;

                    let prompt: SentRequest<PromptResponse> = cx.send_request(PromptRequest::new(
                        session.session_id.clone(),
                        vec!["park".into()],
                    ));
                    let client_prompt_id = prompt.id();
                    prompt.cancel()?;

                    let error = prompt
                        .block_task()
                        .await
                        .expect_err("prompt should be cancelled");
                    assert_eq!(i32::from(error.code), -32800);

                    // The chain still works end to end: a normal prompt
                    // completes, and its session update comes back through
                    // the arrow proxy, which prefixes `>` — proving the real
                    // proxy sits in the message path.
                    let barrier: PromptResponse = cx
                        .send_request(PromptRequest::new(
                            session.session_id.clone(),
                            vec!["barrier".into()],
                        ))
                        .block_task()
                        .await?;
                    assert_eq!(barrier.stop_reason, StopReason::EndTurn);

                    Ok(client_prompt_id)
                },
            )
            .await
    })
    .await
    .expect("test timed out")
    .expect("client failed");

    // The agent saw exactly one `$/cancel_request` (for the prompt), with the
    // ID of the prompt on the conductor-to-agent connection.
    let prompt_id = next_with_timeout(&mut prompt_id_rx).await;
    assert_ne!(
        prompt_id, client_prompt_id,
        "each hop must re-issue the request under its own ID"
    );
    let observed = next_with_timeout(&mut agent_cancel_rx).await;
    assert_eq!(serde_json::to_value(observed).unwrap(), prompt_id);
    assert_no_event(&mut agent_cancel_rx);

    // The client saw exactly one `$/cancel_request` (for the permission
    // request), with the ID of that request on the client's own connection.
    let permission_id = next_with_timeout(&mut permission_id_rx).await;
    let observed = next_with_timeout(&mut client_cancel_rx).await;
    assert_eq!(serde_json::to_value(observed).unwrap(), permission_id);
    assert_no_event(&mut client_cancel_rx);

    // The barrier prompt's session update was transformed by the arrow proxy.
    let update = next_with_timeout(&mut session_update_rx).await;
    assert_eq!(update, ">barrier");

    conductor_handle.abort();
    Ok(())
}
