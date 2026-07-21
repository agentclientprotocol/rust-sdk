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

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use agent_client_protocol::DynConnectTo;
use agent_client_protocol::schema::v1::{
    CancelRequestNotification, ContentBlock, ContentChunk, InitializeRequest, InitializeResponse,
    McpServer as SchemaMcpServer, NewSessionRequest, NewSessionResponse, PermissionOption,
    PermissionOptionKind, PromptRequest, PromptResponse, RequestPermissionOutcome,
    RequestPermissionRequest, RequestPermissionResponse, SelectedPermissionOutcome, SessionId,
    SessionNotification, SessionUpdate, StopReason, ToolCallUpdate, ToolCallUpdateFields,
};
use agent_client_protocol::schema::{McpConnectRequest, ProtocolVersion};
use agent_client_protocol::{
    Agent, ByteStreams, Client, Conductor, ConnectTo, ConnectionTo, Error, JsonRpcRequest,
    JsonRpcResponse, NullRun, Proxy, Responder, Role, SentRequest,
    mcp_server::{McpConnectionTo, McpServer, McpServerConnect},
    role,
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

#[derive(Clone)]
struct TrackingMcpServer {
    connect_tx: mpsc::UnboundedSender<String>,
}

impl<Counterpart: Role> McpServerConnect<Counterpart> for TrackingMcpServer {
    fn name(&self) -> String {
        "tracking-mcp".to_string()
    }

    fn connect(&self, cx: McpConnectionTo<Counterpart>) -> DynConnectTo<role::mcp::Client> {
        self.connect_tx
            .unbounded_send(cx.acp_id().to_owned())
            .unwrap();
        DynConnectTo::new(EmptyMcpServerComponent)
    }
}

struct EmptyMcpServerComponent;

impl ConnectTo<role::mcp::Client> for EmptyMcpServerComponent {
    async fn connect_to(self, client: impl ConnectTo<role::mcp::Server>) -> Result<(), Error> {
        role::mcp::Server
            .builder()
            .connect_with(client, async |_cx| {
                std::future::pending::<Result<(), Error>>().await
            })
            .await
    }
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

fn advertised_mcp_acp_id(request: &NewSessionRequest) -> String {
    match request.mcp_servers.as_slice() {
        [SchemaMcpServer::Http(http)] => http.url.clone(),
        servers => panic!("expected exactly one HTTP MCP server, got {servers:?}"),
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
                    parked_id_tx.unbounded_send(responder.id().clone()).unwrap();
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
                    let client_request_id = request.id().clone();
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
    assert_eq!(observed, parked_id);
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
                    parked_id_tx.unbounded_send(responder.id().clone()).unwrap();
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
    assert_eq!(observed, parked_id);
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

                prompt_id_tx.unbounded_send(responder.id().clone()).unwrap();
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
                    permission_id_tx
                        .unbounded_send(responder.id().clone())
                        .unwrap();
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
                    let client_prompt_id = prompt.id().clone();
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
    assert_eq!(observed, prompt_id);
    assert_no_event(&mut agent_cancel_rx);

    // The client saw exactly one `$/cancel_request` (for the permission
    // request), with the ID of that request on the client's own connection.
    let permission_id = next_with_timeout(&mut permission_id_rx).await;
    let observed = next_with_timeout(&mut client_cancel_rx).await;
    assert_eq!(observed, permission_id);
    assert_no_event(&mut client_cancel_rx);

    // The barrier prompt's session update was transformed by the arrow proxy.
    let update = next_with_timeout(&mut session_update_rx).await;
    assert_eq!(update, ">barrier");

    conductor_handle.abort();
    Ok(())
}

/// `session/new` is forwarded by proxies with a result hook (to register the
/// session's dynamic handler), not with `forward_response_to` — cancellation
/// must still propagate hop by hop, exactly like every other request.
#[tokio::test]
async fn session_new_cancellation_propagates_through_proxy() -> Result<(), Error> {
    let (agent_cancel_tx, mut agent_cancel_rx) = mpsc::unbounded();
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
            async move |request: NewSessionRequest,
                        responder: Responder<NewSessionResponse>,
                        cx: ConnectionTo<Client>| {
                if request.cwd.ends_with("park-session") {
                    parked_id_tx.unbounded_send(responder.id().clone()).unwrap();
                    let cancellation = responder.cancellation();
                    cx.spawn(async move {
                        let response = cancellation
                            .run_until_cancelled(std::future::pending::<
                                Result<NewSessionResponse, Error>,
                            >())
                            .await;
                        responder.respond_with_result(response)
                    })?;
                    return Ok(());
                }

                responder.respond(NewSessionResponse::new(SessionId::new("normal-session")))
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

    // The passthrough proxy is what exercises the proxy-side `session/new`
    // forwarding hook.
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

                    let request: SentRequest<NewSessionResponse> =
                        cx.send_request(NewSessionRequest::new("/park-session"));
                    let client_request_id = request.id().clone();
                    request.cancel()?;

                    let error = request
                        .block_task()
                        .await
                        .expect_err("session/new should be cancelled");
                    assert_eq!(i32::from(error.code), -32800);

                    // Barrier through the whole chain: a fresh session still
                    // works after the cancelled one.
                    let session = cx
                        .send_request(NewSessionRequest::new(
                            std::env::current_dir().map_err(Error::into_internal_error)?,
                        ))
                        .block_task()
                        .await?;
                    assert_eq!(session.session_id, SessionId::new("normal-session"));

                    Ok(client_request_id)
                },
            )
            .await
    })
    .await
    .expect("test timed out")
    .expect("client failed");

    // The agent saw exactly one `$/cancel_request`, for the `session/new` ID
    // on its own connection.
    let parked_id = next_with_timeout(&mut parked_id_rx).await;
    assert_ne!(
        parked_id, client_request_id,
        "each hop must re-issue the request under its own ID"
    );
    let observed = next_with_timeout(&mut agent_cancel_rx).await;
    assert_eq!(observed, parked_id);
    assert_no_event(&mut agent_cancel_rx);

    conductor_handle.abort();
    Ok(())
}

/// The SDK's documented proxy session helper also forwards `session/new` with
/// a result hook, so it must opt into cancellation propagation explicitly.
#[tokio::test]
async fn proxy_session_helper_cancellation_propagates_to_agent() -> Result<(), Error> {
    let (agent_cancel_tx, mut agent_cancel_rx) = mpsc::unbounded();
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
            async move |request: NewSessionRequest,
                        responder: Responder<NewSessionResponse>,
                        cx: ConnectionTo<Client>| {
                if request.cwd.ends_with("park-session") {
                    parked_id_tx.unbounded_send(responder.id().clone()).unwrap();
                    let cancellation = responder.cancellation();
                    cx.spawn(async move {
                        let response = cancellation
                            .run_until_cancelled(std::future::pending::<
                                Result<NewSessionResponse, Error>,
                            >())
                            .await;
                        responder.respond_with_result(response)
                    })?;
                    return Ok(());
                }

                responder.respond(NewSessionResponse::new(SessionId::new("normal-session")))
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

    let proxy = Proxy.builder().on_receive_request_from(
        Client,
        async |request: NewSessionRequest,
               responder: Responder<NewSessionResponse>,
               cx: ConnectionTo<Conductor>| {
            cx.build_session_from(request)
                .on_proxy_session_start(responder, async |_session_id| Ok::<(), Error>(()))
        },
        agent_client_protocol::on_receive_request!(),
    );

    let (editor_write, conductor_read) = duplex(8192);
    let (conductor_write, editor_read) = duplex(8192);

    let conductor_handle = tokio::spawn(async move {
        ConductorImpl::new_agent(
            "helper-cancellation-conductor".to_string(),
            ProxiesAndAgent::new(agent).proxy(proxy),
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

                    let request: SentRequest<NewSessionResponse> =
                        cx.send_request(NewSessionRequest::new("/park-session"));
                    let client_request_id = request.id().clone();
                    request.cancel()?;

                    let error = request
                        .block_task()
                        .await
                        .expect_err("session/new should be cancelled");
                    assert_eq!(i32::from(error.code), -32800);

                    let session = cx
                        .send_request(NewSessionRequest::new(
                            std::env::current_dir().map_err(Error::into_internal_error)?,
                        ))
                        .block_task()
                        .await?;
                    assert_eq!(session.session_id, SessionId::new("normal-session"));

                    Ok(client_request_id)
                },
            )
            .await
    })
    .await
    .expect("test timed out")
    .expect("client failed");

    let parked_id = next_with_timeout(&mut parked_id_rx).await;
    assert_ne!(
        parked_id, client_request_id,
        "each hop must re-issue the request under its own ID"
    );
    let observed = next_with_timeout(&mut agent_cancel_rx).await;
    assert_eq!(observed, parked_id);
    assert_no_event(&mut agent_cancel_rx);

    conductor_handle.abort();
    Ok(())
}

/// If the proxy helper attaches MCP servers for a `session/new` that later
/// fails, the MCP dynamic handler must be removed before the next retry.
#[tokio::test]
async fn proxy_session_helper_cleans_up_mcp_handlers_after_cancelled_session() -> Result<(), Error>
{
    let (agent_cancel_tx, mut agent_cancel_rx) = mpsc::unbounded();
    let (parked_id_tx, mut parked_id_rx) = mpsc::unbounded();
    let (mcp_connect_tx, mut mcp_connect_rx) = mpsc::unbounded();
    let (probe_barrier_tx, mut probe_barrier_rx) = mpsc::unbounded();
    let cancelled_mcp_acp_id = Arc::new(Mutex::new(None::<String>));

    let agent = Agent
        .builder()
        .on_receive_request(
            async |initialize: InitializeRequest, responder, _cx: ConnectionTo<Client>| {
                responder.respond(InitializeResponse::new(initialize.protocol_version))
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            {
                let cancelled_mcp_acp_id = cancelled_mcp_acp_id.clone();
                let parked_id_tx = parked_id_tx.clone();
                let probe_barrier_tx = probe_barrier_tx.clone();
                async move |request: NewSessionRequest,
                            responder: Responder<NewSessionResponse>,
                            cx: ConnectionTo<Client>| {
                    let cancelled_mcp_acp_id = cancelled_mcp_acp_id.clone();
                    let parked_id_tx = parked_id_tx.clone();
                    let probe_barrier_tx = probe_barrier_tx.clone();
                    let advertised_mcp_acp_id = advertised_mcp_acp_id(&request);

                    if request.cwd.ends_with("park-session") {
                        *cancelled_mcp_acp_id
                            .lock()
                            .expect("cancelled MCP ID mutex poisoned") =
                            Some(advertised_mcp_acp_id);
                        parked_id_tx.unbounded_send(responder.id().clone()).unwrap();
                        let cancellation = responder.cancellation();
                        cx.spawn(async move {
                            let response = cancellation
                                .run_until_cancelled(std::future::pending::<
                                    Result<NewSessionResponse, Error>,
                                >())
                                .await;
                            responder.respond_with_result(response)
                        })?;
                        return Ok(());
                    }

                    responder.respond(NewSessionResponse::new(SessionId::new("normal-session")))?;

                    let stale_acp_id = cancelled_mcp_acp_id
                        .lock()
                        .expect("cancelled MCP ID mutex poisoned")
                        .clone()
                        .expect("cancelled session should have advertised an MCP server");
                    let connection = cx.clone();
                    cx.spawn(async move {
                        connection
                            .send_request(McpConnectRequest {
                                acp_id: stale_acp_id,
                                meta: None,
                            })
                            .on_receiving_result(async |_| Ok(()))?;

                        let barrier = connection
                            .send_request(RequestPermissionRequest::new(
                                SessionId::new("normal-session"),
                                ToolCallUpdate::new(
                                    "stale-mcp-probe-barrier",
                                    ToolCallUpdateFields::default(),
                                ),
                                vec![PermissionOption::new(
                                    "allow",
                                    "Allow",
                                    PermissionOptionKind::AllowOnce,
                                )],
                            ))
                            .block_task()
                            .await
                            .map(|_| ())
                            .map_err(|error| i32::from(error.code));

                        probe_barrier_tx.unbounded_send(barrier).unwrap();
                        Ok(())
                    })
                }
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

    let proxy = Proxy.builder().on_receive_request_from(
        Client,
        async move |request: NewSessionRequest,
                    responder: Responder<NewSessionResponse>,
                    cx: ConnectionTo<Conductor>| {
            let mcp_server = McpServer::new(
                TrackingMcpServer {
                    connect_tx: mcp_connect_tx.clone(),
                },
                NullRun,
            );
            cx.build_session_from(request)
                .with_mcp_server(mcp_server)?
                .on_proxy_session_start(responder, async |_session_id| Ok::<(), Error>(()))
        },
        agent_client_protocol::on_receive_request!(),
    );

    let (editor_write, conductor_read) = duplex(8192);
    let (conductor_write, editor_read) = duplex(8192);

    let conductor_handle = tokio::spawn(async move {
        ConductorImpl::new_agent(
            "helper-cleanup-conductor".to_string(),
            ProxiesAndAgent::new(agent).proxy(proxy),
        )
        .run(ByteStreams::new(
            conductor_write.compat_write(),
            conductor_read.compat(),
        ))
        .await
    });

    let client_result = tokio::time::timeout(Duration::from_secs(30), async move {
        Client
            .builder()
            .on_receive_request(
                async |request: RequestPermissionRequest,
                       responder: Responder<RequestPermissionResponse>,
                       _cx: ConnectionTo<Agent>| {
                    assert_eq!(request.session_id, SessionId::new("normal-session"));
                    responder.respond(RequestPermissionResponse::new(
                        RequestPermissionOutcome::Selected(SelectedPermissionOutcome::new("allow")),
                    ))
                },
                agent_client_protocol::on_receive_request!(),
            )
            .connect_with(
                ByteStreams::new(editor_write.compat_write(), editor_read.compat()),
                async move |cx| {
                    let initialize = cx
                        .send_request(InitializeRequest::new(ProtocolVersion::V1))
                        .block_task()
                        .await?;
                    assert_eq!(initialize.protocol_version, ProtocolVersion::V1);

                    let request: SentRequest<NewSessionResponse> =
                        cx.send_request(NewSessionRequest::new("/park-session"));
                    let client_request_id = request.id().clone();
                    request.cancel()?;

                    let error = request
                        .block_task()
                        .await
                        .expect_err("session/new should be cancelled");
                    assert_eq!(i32::from(error.code), -32800);

                    let session = cx
                        .send_request(NewSessionRequest::new(
                            std::env::current_dir().map_err(Error::into_internal_error)?,
                        ))
                        .block_task()
                        .await?;
                    assert_eq!(session.session_id, SessionId::new("normal-session"));

                    let probe_barrier = next_with_timeout(&mut probe_barrier_rx).await;
                    Ok((client_request_id, probe_barrier))
                },
            )
            .await
    })
    .await
    .expect("test timed out")
    .expect("client failed");
    let (client_request_id, probe_barrier) = client_result;

    let parked_id = next_with_timeout(&mut parked_id_rx).await;
    assert_ne!(
        parked_id, client_request_id,
        "each hop must re-issue the request under its own ID"
    );
    let observed = next_with_timeout(&mut agent_cancel_rx).await;
    assert_eq!(observed, parked_id);
    assert_no_event(&mut agent_cancel_rx);

    assert_eq!(
        probe_barrier,
        Ok(()),
        "agent-to-client barrier should succeed after stale MCP probe"
    );
    assert_no_event(&mut mcp_connect_rx);

    conductor_handle.abort();
    Ok(())
}

/// `initialize` is rewritten to `_proxy/initialize` at the conductor-to-proxy
/// hop and forwarded with a result hook — cancellation must still propagate
/// hop by hop, exactly like every other request.
#[tokio::test]
async fn initialize_cancellation_propagates_through_proxy() -> Result<(), Error> {
    let (agent_cancel_tx, mut agent_cancel_rx) = mpsc::unbounded();
    let (parked_id_tx, mut parked_id_rx) = mpsc::unbounded();
    let parked_first = Arc::new(AtomicBool::new(false));

    let agent = Agent.builder().on_receive_request(
        {
            let parked_first = parked_first.clone();
            async move |initialize: InitializeRequest,
                        responder: Responder<InitializeResponse>,
                        cx: ConnectionTo<Client>| {
                if !parked_first.swap(true, Ordering::SeqCst) {
                    parked_id_tx.unbounded_send(responder.id().clone()).unwrap();
                    let cancellation = responder.cancellation();
                    cx.spawn(async move {
                        let response = cancellation
                            .run_until_cancelled(std::future::pending::<
                                Result<InitializeResponse, Error>,
                            >())
                            .await;
                        responder.respond_with_result(response)
                    })?;
                    return Ok(());
                }

                responder.respond(InitializeResponse::new(initialize.protocol_version))
            }
        },
        agent_client_protocol::on_receive_request!(),
    );

    // The cancel observer must be registered on the same builder; do it
    // separately so the request handler above can keep its captures.
    let agent = agent.on_receive_notification(
        async move |cancel: CancelRequestNotification, _cx: ConnectionTo<Client>| {
            agent_cancel_tx.unbounded_send(cancel.request_id).unwrap();
            Ok(())
        },
        agent_client_protocol::on_receive_notification!(),
    );

    let (editor_write, conductor_read) = duplex(8192);
    let (conductor_write, editor_read) = duplex(8192);

    // The passthrough proxy is what exercises the conductor's
    // `initialize` -> `_proxy/initialize` rewriting hop.
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
                    let request: SentRequest<InitializeResponse> =
                        cx.send_request(InitializeRequest::new(ProtocolVersion::V1));
                    let client_request_id = request.id().clone();
                    request.cancel()?;

                    let error = request
                        .block_task()
                        .await
                        .expect_err("initialize should be cancelled");
                    assert_eq!(i32::from(error.code), -32800);

                    // Barrier through the whole chain: initializing again
                    // still works after the cancelled attempt.
                    let initialize = cx
                        .send_request(InitializeRequest::new(ProtocolVersion::V1))
                        .block_task()
                        .await?;
                    assert_eq!(initialize.protocol_version, ProtocolVersion::V1);

                    Ok(client_request_id)
                },
            )
            .await
    })
    .await
    .expect("test timed out")
    .expect("client failed");

    // The agent saw exactly one `$/cancel_request`, for the `initialize` ID
    // on its own connection.
    let parked_id = next_with_timeout(&mut parked_id_rx).await;
    assert_ne!(
        parked_id, client_request_id,
        "each hop must re-issue the request under its own ID"
    );
    let observed = next_with_timeout(&mut agent_cancel_rx).await;
    assert_eq!(observed, parked_id);
    assert_no_event(&mut agent_cancel_rx);

    conductor_handle.abort();
    Ok(())
}
