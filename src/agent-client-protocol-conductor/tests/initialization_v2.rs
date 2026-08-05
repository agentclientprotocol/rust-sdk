#![cfg(feature = "unstable_protocol_v2")]

use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

use agent_client_protocol::schema::{ProtocolVersion, v1, v2};
use agent_client_protocol::{
    Agent, ByteStreams, Client, Conductor, ConnectTo, ConnectionTo, DynConnectTo, Error,
    JsonRpcRequest, JsonRpcResponse, Proxy, UntypedMessage, V2ConnectionTo,
};
use agent_client_protocol_conductor::{
    ConductorImpl, InstantiateProxies, InstantiateProxiesAndAgent, ProxiesAndAgent,
};
use futures::{StreamExt as _, channel::mpsc};
use serde::{Deserialize, Serialize};
use tokio::io::duplex;
use tokio_util::compat::{TokioAsyncReadCompatExt, TokioAsyncWriteCompatExt};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonRpcRequest)]
#[request(method = "initialize", response = v2::InitializeResponse)]
struct ExtendedInitializeRequest {
    #[serde(flatten)]
    initialize: v2::InitializeRequest,
    #[serde(
        rename = "_futureInitializeField",
        skip_serializing_if = "Option::is_none"
    )]
    future_initialize_field: Option<serde_json::Value>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonRpcRequest)]
#[request(method = "initialize", response = ExtendedInitializeResponse)]
struct ResponseExtensionInitializeRequest {
    #[serde(flatten)]
    initialize: v2::InitializeRequest,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonRpcResponse)]
struct ExtendedInitializeResponse {
    #[serde(flatten)]
    initialize: v2::InitializeResponse,
    #[serde(rename = "_futureInitializeResponseField")]
    future_initialize_response_field: serde_json::Value,
}

fn meta(key: &str, value: &str) -> v2::Meta {
    let mut meta = v2::Meta::new();
    meta.insert(key.to_owned(), serde_json::Value::String(value.to_owned()));
    meta
}

fn initialize_request() -> v2::InitializeRequest {
    let info = v2::Implementation::new("v2-test-client", "1.2.3")
        .title("V2 Test Client")
        .meta(meta("implementation", "client"));
    let capabilities = v2::ClientCapabilities::new().meta(meta("capability", "client-capability"));

    v2::InitializeRequest::new(ProtocolVersion::V2, info)
        .capabilities(capabilities)
        .meta(meta("request", "client-request"))
}

fn initialize_response() -> v2::InitializeResponse {
    let info = v2::Implementation::new("v2-test-agent", "4.5.6")
        .title("V2 Test Agent")
        .meta(meta("implementation", "agent"));
    let session = v2::SessionCapabilities::new().meta(meta("capability", "session-capability"));
    let capabilities = v2::AgentCapabilities::new()
        .session(session)
        .meta(meta("capability", "agent-capability"));
    let auth_method = v2::AuthMethod::Agent(
        v2::AuthMethodAgent::new("agent-auth", "Agent authentication")
            .description("Authenticate through the agent")
            .meta(meta("auth", "agent-auth-method")),
    );

    v2::InitializeResponse::new(ProtocolVersion::V2, info)
        .capabilities(capabilities)
        .auth_methods(vec![auth_method])
        .meta(meta("response", "agent-response"))
}

fn recording_agent(
    expected_request: v2::InitializeRequest,
    response: v2::InitializeResponse,
    sequence: Arc<AtomicUsize>,
    expected_sequence: usize,
) -> impl ConnectTo<Client> {
    Agent.v2().on_receive_request(
        async move |request: v2::InitializeRequest, responder, _cx| {
            assert_eq!(
                sequence.fetch_add(1, Ordering::SeqCst),
                expected_sequence,
                "v2 agent initialized out of order"
            );
            assert_eq!(request, expected_request);
            responder.respond(response.clone())
        },
        agent_client_protocol::on_receive_request!(),
    )
}

enum InitializeMutation {
    ProtocolVersion,
    Metadata,
}

struct MutatingInstantiator {
    agent: DynConnectTo<Client>,
    mutation: InitializeMutation,
}

impl InstantiateProxiesAndAgent for MutatingInstantiator {
    fn instantiate_proxies_and_agent(
        self: Box<Self>,
        request: v1::InitializeRequest,
    ) -> futures::future::BoxFuture<
        'static,
        Result<
            (
                v1::InitializeRequest,
                Vec<DynConnectTo<Conductor>>,
                DynConnectTo<Client>,
            ),
            Error,
        >,
    > {
        drop((self, request));
        Box::pin(async {
            Err(Error::internal_error().data("v1 initialization unexpectedly selected in v2 test"))
        })
    }

    fn instantiate_v2_proxies_and_agent(
        self: Box<Self>,
        mut request: v2::InitializeRequest,
    ) -> futures::future::BoxFuture<
        'static,
        Result<
            (
                v2::InitializeRequest,
                Vec<DynConnectTo<Conductor>>,
                DynConnectTo<Client>,
            ),
            Error,
        >,
    > {
        match self.mutation {
            InitializeMutation::ProtocolVersion => {
                request.protocol_version = ProtocolVersion::from(3_u16);
            }
            InitializeMutation::Metadata => {
                request = request.meta(meta("instantiator", "modified"));
            }
        }
        let agent = self.agent;
        Box::pin(async move { Ok((request, Vec::new(), agent)) })
    }
}

struct VersionMutatingProxyInstantiator;

impl InstantiateProxies for VersionMutatingProxyInstantiator {
    fn instantiate_proxies(
        self: Box<Self>,
        request: v1::InitializeRequest,
    ) -> futures::future::BoxFuture<
        'static,
        Result<(v1::InitializeRequest, Vec<DynConnectTo<Conductor>>), Error>,
    > {
        drop((self, request));
        Box::pin(async {
            Err(Error::internal_error()
                .data("v1 proxy initialization unexpectedly selected in v2 test"))
        })
    }

    fn instantiate_v2_proxies(
        self: Box<Self>,
        mut request: v2::InitializeRequest,
    ) -> futures::future::BoxFuture<
        'static,
        Result<(v2::InitializeRequest, Vec<DynConnectTo<Conductor>>), Error>,
    > {
        request.protocol_version = ProtocolVersion::from(3_u16);
        Box::pin(async move { Ok((request, Vec::new())) })
    }
}

struct RecordingPassthroughProxy {
    expected_request: v2::InitializeRequest,
    sequence: Arc<AtomicUsize>,
}

impl ConnectTo<Conductor> for RecordingPassthroughProxy {
    async fn connect_to(
        self,
        client: impl ConnectTo<Proxy>,
    ) -> Result<(), agent_client_protocol::Error> {
        let expected_request = self.expected_request;
        let sequence = self.sequence;

        Proxy
            .v2()
            .name("v2-passthrough-proxy")
            .on_receive_request_from(
                Client,
                async move |request: v2::InitializeProxyRequest,
                            responder,
                            cx: V2ConnectionTo<Conductor>| {
                    assert_eq!(
                        sequence.fetch_add(1, Ordering::SeqCst),
                        0,
                        "v2 proxy must initialize before the agent"
                    );
                    assert_eq!(request.initialize, expected_request);

                    cx.send_request_to(Agent, request.initialize)
                        .forward_response_to(responder)
                },
                agent_client_protocol::on_receive_request!(),
            )
            .on_receive_request_from(
                Client,
                async move |request: v2::NewSessionRequest,
                            responder,
                            cx: V2ConnectionTo<Conductor>| {
                    cx.send_request_to(Agent, request)
                        .forward_response_to(responder)
                },
                agent_client_protocol::on_receive_request!(),
            )
            .connect_to(client)
            .await
    }
}

async fn run_with_conductor(
    components: impl InstantiateProxiesAndAgent + 'static,
    editor_task: impl AsyncFnOnce(V2ConnectionTo<Agent>) -> Result<(), Error>,
) -> Result<(), Error> {
    let (editor_out, conductor_in) = duplex(4096);
    let (conductor_out, editor_in) = duplex(4096);
    let transport = ByteStreams::new(editor_out.compat_write(), editor_in.compat());

    Client
        .v2()
        .name("v2-editor")
        .with_spawned(|_cx| async move {
            ConductorImpl::new_agent("v2-conductor", components)
                .run(ByteStreams::new(
                    conductor_out.compat_write(),
                    conductor_in.compat(),
                ))
                .await
        })
        .connect_with(transport, editor_task)
        .await
}

async fn run_raw_client_with_conductor(
    components: ProxiesAndAgent,
    editor_task: impl AsyncFnOnce(ConnectionTo<Agent>) -> Result<(), Error>,
) -> Result<(), Error> {
    let (editor_out, conductor_in) = duplex(4096);
    let (conductor_out, editor_in) = duplex(4096);
    let transport = ByteStreams::new(editor_out.compat_write(), editor_in.compat());

    Client
        .builder()
        .without_acp_version_guard()
        .name("raw-v2-editor")
        .with_spawned(|_cx| async move {
            ConductorImpl::new_agent("v2-conductor", components)
                .run(ByteStreams::new(
                    conductor_out.compat_write(),
                    conductor_in.compat(),
                ))
                .await
        })
        .connect_with(transport, editor_task)
        .await
}

async fn assert_invalid_final_agent_initialize_response(
    response: serde_json::Value,
) -> Result<(), Error> {
    let agent = Agent
        .builder()
        .without_acp_version_guard()
        .on_receive_request(
            async move |request: UntypedMessage, responder, _cx| {
                assert_eq!(request.method(), "initialize");
                assert_eq!(
                    request.params().get("protocolVersion"),
                    Some(&serde_json::json!(2))
                );
                responder.respond(response.clone())
            },
            agent_client_protocol::on_receive_request!(),
        );

    run_with_conductor(ProxiesAndAgent::new(agent), async move |cx| {
        let error = cx
            .send_request(initialize_request())
            .block_task()
            .await
            .expect_err("the conductor must reject an invalid final-agent response");
        assert!(
            error.to_string().contains("protocol version")
                || error.to_string().contains("protocolVersion"),
            "unexpected initialize response validation error: {error:?}"
        );
        Ok(())
    })
    .await
}

async fn assert_raw_initialize_rejected(
    params: serde_json::Value,
    expected_error: &str,
) -> Result<(), Error> {
    run_raw_client_with_conductor(ProxiesAndAgent::new(Agent.v2()), async move |cx| {
        let request = UntypedMessage {
            method: "initialize".to_string(),
            params,
        };
        let error = tokio::time::timeout(
            std::time::Duration::from_secs(2),
            cx.send_request(request).block_task(),
        )
        .await
        .expect("invalid initialize request should not hang")
        .expect_err("invalid initialize request must be rejected");
        assert!(
            error.to_string().contains(expected_error),
            "unexpected initialize rejection: {error:?}"
        );
        Ok(())
    })
    .await
}

#[tokio::test]
async fn v2_initialize_preserves_request_and_response_through_conductor() -> Result<(), Error> {
    let request = initialize_request();
    let response = initialize_response();
    let sequence = Arc::new(AtomicUsize::new(0));
    let agent = recording_agent(request.clone(), response.clone(), Arc::clone(&sequence), 0);

    run_with_conductor(ProxiesAndAgent::new(agent), async move |cx| {
        let received = cx.send_request(request).block_task().await?;
        assert_eq!(received, response);
        Ok(())
    })
    .await?;

    assert_eq!(sequence.load(Ordering::SeqCst), 1);
    Ok(())
}

#[tokio::test]
async fn v2_initialize_preserves_unknown_response_fields() -> Result<(), Error> {
    let request = ResponseExtensionInitializeRequest {
        initialize: initialize_request(),
    };
    let response = ExtendedInitializeResponse {
        initialize: initialize_response(),
        future_initialize_response_field: serde_json::json!({
            "preserved": true,
        }),
    };
    let expected_request = request.clone();
    let expected_response = response.clone();
    let agent = Agent.v2().on_receive_request(
        async move |request: ResponseExtensionInitializeRequest, responder, _cx| {
            assert_eq!(request, expected_request);
            responder.respond(expected_response.clone())
        },
        agent_client_protocol::on_receive_request!(),
    );

    run_with_conductor(ProxiesAndAgent::new(agent), async move |cx| {
        let received = cx.send_request(request).block_task().await?;
        assert_eq!(received, response);
        Ok(())
    })
    .await
}

#[tokio::test]
async fn v2_instantiator_cannot_change_selected_protocol_version() -> Result<(), Error> {
    let request = initialize_request();
    let response = initialize_response();
    let sequence = Arc::new(AtomicUsize::new(0));
    let agent = recording_agent(request.clone(), response.clone(), Arc::clone(&sequence), 0);
    let instantiator = MutatingInstantiator {
        agent: DynConnectTo::new(agent),
        mutation: InitializeMutation::ProtocolVersion,
    };

    run_with_conductor(instantiator, async move |cx| {
        let received = cx.send_request(request).block_task().await?;
        assert_eq!(received, response);
        Ok(())
    })
    .await?;

    assert_eq!(sequence.load(Ordering::SeqCst), 1);
    Ok(())
}

#[tokio::test]
async fn v2_modified_typed_request_becomes_authoritative() -> Result<(), Error> {
    let initialize = initialize_request();
    let request = ExtendedInitializeRequest {
        initialize: initialize.clone(),
        future_initialize_field: Some(serde_json::json!({
            "preserved-only-while-unchanged": true,
        })),
    };
    let mut expected_initialize = initialize.meta(meta("instantiator", "modified"));
    expected_initialize.protocol_version = ProtocolVersion::V2;
    let expected_request = ExtendedInitializeRequest {
        initialize: expected_initialize,
        future_initialize_field: None,
    };
    let response = initialize_response();
    let expected_response = response.clone();
    let agent = Agent.v2().on_receive_request(
        async move |request: ExtendedInitializeRequest, responder, _cx| {
            assert_eq!(request, expected_request);
            responder.respond(expected_response.clone())
        },
        agent_client_protocol::on_receive_request!(),
    );
    let instantiator = MutatingInstantiator {
        agent: DynConnectTo::new(agent),
        mutation: InitializeMutation::Metadata,
    };

    run_with_conductor(instantiator, async move |cx| {
        let received = cx.send_request(request).block_task().await?;
        assert_eq!(received, response);
        Ok(())
    })
    .await
}

#[tokio::test]
async fn v2_final_agent_initialize_response_requires_protocol_version() -> Result<(), Error> {
    let mut response =
        serde_json::to_value(initialize_response()).map_err(Error::into_internal_error)?;
    response
        .as_object_mut()
        .expect("initialize response should serialize as an object")
        .remove("protocolVersion");
    assert_invalid_final_agent_initialize_response(response).await
}

#[tokio::test]
async fn v2_final_agent_initialize_response_must_match_selected_version() -> Result<(), Error> {
    let mut response =
        serde_json::to_value(initialize_response()).map_err(Error::into_internal_error)?;
    response["protocolVersion"] = serde_json::json!(1);
    assert_invalid_final_agent_initialize_response(response).await
}

#[tokio::test]
async fn v2_conductor_rejects_invalid_protocol_versions_with_responses() -> Result<(), Error> {
    assert_raw_initialize_rejected(
        serde_json::json!({}),
        "protocolVersion must be a valid ACP protocol version",
    )
    .await?;
    assert_raw_initialize_rejected(
        serde_json::json!({ "protocolVersion": "2" }),
        "protocolVersion must be a valid ACP protocol version",
    )
    .await?;
    assert_raw_initialize_rejected(
        serde_json::json!({ "protocolVersion": 0 }),
        "unsupported ACP protocol version 0",
    )
    .await
}

#[tokio::test]
async fn v2_proxy_initialize_precedes_agent_initialize() -> Result<(), Error> {
    let request = initialize_request();
    let response = initialize_response();
    let sequence = Arc::new(AtomicUsize::new(0));
    let agent = recording_agent(request.clone(), response.clone(), Arc::clone(&sequence), 1);
    let proxy = RecordingPassthroughProxy {
        expected_request: request.clone(),
        sequence: Arc::clone(&sequence),
    };

    run_with_conductor(ProxiesAndAgent::new(agent).proxy(proxy), async move |cx| {
        let received = cx.send_request(request).block_task().await?;
        assert_eq!(received, response);
        Ok(())
    })
    .await?;

    assert_eq!(sequence.load(Ordering::SeqCst), 2);
    Ok(())
}

#[tokio::test]
async fn v2_tracing_without_user_proxies_uses_version_neutral_bridge() -> Result<(), Error> {
    let request = initialize_request();
    let response = initialize_response();
    let sequence = Arc::new(AtomicUsize::new(0));
    let agent = recording_agent(request.clone(), response.clone(), Arc::clone(&sequence), 0);
    let components = ProxiesAndAgent::new(agent);
    let (trace_tx, _trace_rx) = mpsc::unbounded();
    let (editor_out, conductor_in) = duplex(4096);
    let (conductor_out, editor_in) = duplex(4096);
    let transport = ByteStreams::new(editor_out.compat_write(), editor_in.compat());

    Client
        .v2()
        .name("v2-editor")
        .with_spawned(|_cx| async move {
            ConductorImpl::new_agent("v2-conductor", components)
                .trace_to(trace_tx)
                .run(ByteStreams::new(
                    conductor_out.compat_write(),
                    conductor_in.compat(),
                ))
                .await
        })
        .connect_with(transport, async move |cx| {
            let received = cx.send_request(request).block_task().await?;
            assert_eq!(received, response);
            Ok(())
        })
        .await?;

    assert_eq!(sequence.load(Ordering::SeqCst), 1);
    Ok(())
}

#[tokio::test]
async fn v2_nested_proxy_instantiator_cannot_change_selected_version() -> Result<(), Error> {
    let request = initialize_request();
    let response = initialize_response();
    let sequence = Arc::new(AtomicUsize::new(0));
    let agent = recording_agent(request.clone(), response.clone(), Arc::clone(&sequence), 0);
    let nested_conductor =
        ConductorImpl::new_proxy("version-mutating-proxy", VersionMutatingProxyInstantiator);

    run_with_conductor(
        ProxiesAndAgent::new(agent).proxy(nested_conductor),
        async move |cx| {
            let received = cx.send_request(request).block_task().await?;
            assert_eq!(received, response);
            Ok(())
        },
    )
    .await?;

    assert_eq!(sequence.load(Ordering::SeqCst), 1);
    Ok(())
}

#[tokio::test]
async fn v2_nested_proxy_rejection_is_flushed_and_remains_failed() -> Result<(), Error> {
    let v1_only_proxy = |request: v1::InitializeRequest| async move {
        Ok::<_, Error>((request, Vec::<DynConnectTo<Conductor>>::new()))
    };
    let nested_conductor = ConductorImpl::new_proxy("v1-only-proxy", v1_only_proxy);
    let request = initialize_request();

    run_with_conductor(
        ProxiesAndAgent::new(Agent.v2()).proxy(nested_conductor),
        async move |cx| {
            for attempt in 1..=2 {
                let error = tokio::time::timeout(
                    std::time::Duration::from_secs(2),
                    cx.send_request(request.clone()).block_task(),
                )
                .await
                .expect("nested proxy rejection should not hang")
                .expect_err("v1-only nested proxy must reject v2 initialize");
                assert!(
                    error
                        .to_string()
                        .contains("proxy instantiator does not support ACP protocol v2"),
                    "unexpected nested proxy rejection on attempt {attempt}: {error:?}"
                );
            }
            Ok(())
        },
    )
    .await
}

#[tokio::test]
async fn v2_session_new_preserves_request_and_response_through_proxy() -> Result<(), Error> {
    let initialize_request = initialize_request();
    let initialize_response = initialize_response();
    let session_request = v2::NewSessionRequest::new("/v2-session")
        .additional_directories(["/v2-session/workspace"])
        .meta(meta("request", "session-request"));
    let session_response = v2::NewSessionResponse::new(v2::SessionId::new("v2-session"))
        .meta(meta("response", "session-response"));
    let expected_initialize = initialize_request.clone();
    let expected_session = session_request.clone();
    let agent_initialize_response = initialize_response.clone();
    let agent_session_response = session_response.clone();
    let session_update = UntypedMessage {
        method: "session/update".to_string(),
        params: serde_json::json!({
            "sessionId": "v2-session",
            "update": {
                "sessionUpdate": "future_update",
                "payload": {
                    "preserved": true,
                },
            },
        }),
    };
    let agent_session_update = session_update.clone();
    let sequence = Arc::new(AtomicUsize::new(0));
    let proxy = RecordingPassthroughProxy {
        expected_request: initialize_request.clone(),
        sequence: Arc::clone(&sequence),
    };
    let agent = Agent
        .v2()
        .on_receive_request(
            async move |request: v2::InitializeRequest, responder, _cx| {
                assert_eq!(request, expected_initialize);
                responder.respond(agent_initialize_response.clone())
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            async move |request: v2::NewSessionRequest, responder, cx| {
                assert_eq!(request, expected_session);
                responder.respond(agent_session_response.clone())?;
                cx.send_notification(agent_session_update.clone())
            },
            agent_client_protocol::on_receive_request!(),
        );

    let components = ProxiesAndAgent::new(agent).proxy(proxy);
    let (update_tx, mut update_rx) = mpsc::unbounded();
    let (editor_out, conductor_in) = duplex(4096);
    let (conductor_out, editor_in) = duplex(4096);
    let transport = ByteStreams::new(editor_out.compat_write(), editor_in.compat());
    Client
        .v2()
        .on_receive_notification(
            async move |notification: UntypedMessage, _cx| {
                update_tx
                    .unbounded_send(notification)
                    .map_err(Error::into_internal_error)
            },
            agent_client_protocol::on_receive_notification!(),
        )
        .name("v2-editor")
        .with_spawned(|_cx| async move {
            ConductorImpl::new_agent("v2-conductor", components)
                .run(ByteStreams::new(
                    conductor_out.compat_write(),
                    conductor_in.compat(),
                ))
                .await
        })
        .connect_with(transport, async move |cx| {
            let received_initialize = cx.send_request(initialize_request).block_task().await?;
            assert_eq!(received_initialize, initialize_response);

            let received_session = cx.send_request(session_request).block_task().await?;
            assert_eq!(received_session, session_response);

            let received_update =
                tokio::time::timeout(std::time::Duration::from_secs(2), update_rx.next())
                    .await
                    .expect("post-session update should not hang")
                    .ok_or_else(|| Error::internal_error().data("session update channel closed"))?;
            assert_eq!(received_update, session_update);
            Ok(())
        })
        .await?;

    Ok(())
}

#[tokio::test]
async fn v2_proxy_session_helper_preserves_response_and_routes_later_updates() -> Result<(), Error>
{
    let initialize_request = initialize_request();
    let initialize_response = initialize_response();
    let session_id = v2::SessionId::new("v2-helper-session");
    let session_response = v2::NewSessionResponse::new(session_id.clone())
        .config_options(vec![v2::SessionConfigOption::boolean(
            "thinking", "Thinking", true,
        )])
        .meta(meta("response", "proxy-helper-response"));
    let expected_callback_response = session_response.clone();
    let agent_initialize_response = initialize_response.clone();
    let agent_session_response = session_response.clone();
    let agent_session_id = session_id.clone();
    let agent = Agent
        .v2()
        .on_receive_request(
            async move |_request: v2::InitializeRequest, responder, _cx| {
                responder.respond(agent_initialize_response.clone())
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            async move |_request: v2::NewSessionRequest, responder, cx| {
                responder.respond(agent_session_response.clone())?;
                cx.send_notification(v2::UpdateSessionNotification::new(
                    agent_session_id.clone(),
                    v2::SessionUpdate::StateUpdate(v2::StateUpdate::Running(
                        v2::RunningStateUpdate::new(),
                    )),
                ))
            },
            agent_client_protocol::on_receive_request!(),
        );

    let (callback_tx, mut callback_rx) = mpsc::unbounded();
    let proxy = Proxy.v2().on_receive_request_from(
        Client,
        async move |request: v2::NewSessionRequest, responder, cx: V2ConnectionTo<Conductor>| {
            let callback_tx = callback_tx.clone();
            cx.build_session_from(request).on_proxy_session_start(
                responder,
                move |opened| async move {
                    callback_tx
                        .unbounded_send((
                            opened.session().session_id().clone(),
                            opened.response().clone(),
                        ))
                        .map_err(Error::into_internal_error)
                },
            )
        },
        agent_client_protocol::on_receive_request!(),
    );

    let (update_tx, mut update_rx) = mpsc::unbounded();
    let (editor_out, conductor_in) = duplex(4096);
    let (conductor_out, editor_in) = duplex(4096);
    let transport = ByteStreams::new(editor_out.compat_write(), editor_in.compat());
    Client
        .v2()
        .on_receive_notification(
            async move |update: v2::UpdateSessionNotification, _cx| {
                update_tx
                    .unbounded_send(update)
                    .map_err(Error::into_internal_error)
            },
            agent_client_protocol::on_receive_notification!(),
        )
        .with_spawned(|_cx| async move {
            ConductorImpl::new_agent("v2-conductor", ProxiesAndAgent::new(agent).proxy(proxy))
                .run(ByteStreams::new(
                    conductor_out.compat_write(),
                    conductor_in.compat(),
                ))
                .await
        })
        .connect_with(transport, async move |cx| {
            cx.send_request(initialize_request).block_task().await?;

            let received = cx
                .send_request(v2::NewSessionRequest::new("/v2-helper-session"))
                .block_task()
                .await?;
            assert_eq!(received, session_response);

            let (callback_session_id, callback_response) =
                tokio::time::timeout(std::time::Duration::from_secs(2), callback_rx.next())
                    .await
                    .expect("proxy session callback should not hang")
                    .ok_or_else(|| Error::internal_error().data("proxy callback channel closed"))?;
            assert_eq!(callback_session_id, session_id);
            assert_eq!(callback_response, expected_callback_response);

            let update = tokio::time::timeout(std::time::Duration::from_secs(2), update_rx.next())
                .await
                .expect("post-response session update should not hang")
                .ok_or_else(|| Error::internal_error().data("session update channel closed"))?;
            assert_eq!(update.session_id, session_id);
            assert!(matches!(
                update.update,
                v2::SessionUpdate::StateUpdate(v2::StateUpdate::Running(_))
            ));
            Ok(())
        })
        .await
}

#[tokio::test]
async fn v2_proxy_session_helper_reissues_cancellation_for_the_downstream_hop() -> Result<(), Error>
{
    let (parked_id_tx, mut parked_id_rx) = mpsc::unbounded();
    let (cancel_tx, mut cancel_rx) = mpsc::unbounded();
    let agent = Agent
        .v2()
        .on_receive_request(
            async |_request: v2::InitializeRequest, responder, _cx| {
                responder.respond(initialize_response())
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            async move |request: v2::NewSessionRequest, responder, cx| {
                if AsRef::<std::path::Path>::as_ref(&request.cwd).ends_with("park-session") {
                    parked_id_tx
                        .unbounded_send(responder.id().clone())
                        .map_err(Error::into_internal_error)?;
                    let cancellation = responder.cancellation();
                    cx.spawn(async move {
                        let result = cancellation
                            .run_until_cancelled(std::future::pending::<
                                Result<v2::NewSessionResponse, Error>,
                            >())
                            .await;
                        responder.respond_with_result(result)
                    })?;
                    return Ok(());
                }

                responder.respond(v2::NewSessionResponse::new(v2::SessionId::new(
                    "normal-session",
                )))
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_notification(
            async move |cancel: v1::CancelRequestNotification, _cx| {
                cancel_tx
                    .unbounded_send(cancel.request_id)
                    .map_err(Error::into_internal_error)
            },
            agent_client_protocol::on_receive_notification!(),
        );
    let proxy = Proxy.v2().on_receive_request_from(
        Client,
        async |request: v2::NewSessionRequest, responder, cx: V2ConnectionTo<Conductor>| {
            cx.build_session_from(request)
                .on_proxy_session_start(responder, |_opened| async { Ok(()) })
        },
        agent_client_protocol::on_receive_request!(),
    );

    let (editor_out, conductor_in) = duplex(4096);
    let (conductor_out, editor_in) = duplex(4096);
    let transport = ByteStreams::new(editor_out.compat_write(), editor_in.compat());
    let client_request_id = tokio::time::timeout(
        std::time::Duration::from_secs(10),
        Client
            .v2()
            .with_spawned(|_cx| async move {
                ConductorImpl::new_agent(
                    "v2-cancellation-conductor",
                    ProxiesAndAgent::new(agent).proxy(proxy),
                )
                .run(ByteStreams::new(
                    conductor_out.compat_write(),
                    conductor_in.compat(),
                ))
                .await
            })
            .connect_with(transport, async move |cx| {
                cx.send_request(initialize_request()).block_task().await?;

                let pending = cx.send_request(v2::NewSessionRequest::new("/park-session"));
                let client_request_id = pending.id().clone();
                pending.cancel()?;
                let error = pending
                    .block_task()
                    .await
                    .expect_err("cancelled v2 session/new should fail");
                assert_eq!(i32::from(error.code), -32800);

                let response = cx
                    .send_request(v2::NewSessionRequest::new("/normal-session"))
                    .block_task()
                    .await?;
                assert_eq!(response.session_id, v2::SessionId::new("normal-session"));
                Ok(client_request_id)
            }),
    )
    .await
    .expect("v2 proxy cancellation test timed out")?;

    let parked_id = tokio::time::timeout(std::time::Duration::from_secs(2), parked_id_rx.next())
        .await
        .expect("agent should observe the forwarded request")
        .ok_or_else(|| Error::internal_error().data("parked request channel closed"))?;
    assert_ne!(
        parked_id, client_request_id,
        "each proxy hop must allocate its own request ID"
    );
    let cancelled_id = tokio::time::timeout(std::time::Duration::from_secs(2), cancel_rx.next())
        .await
        .expect("agent should observe the reissued cancellation")
        .ok_or_else(|| Error::internal_error().data("cancellation channel closed"))?;
    assert_eq!(cancelled_id, parked_id);
    assert!(
        cancel_rx.try_recv().is_err(),
        "the downstream hop must receive exactly one cancellation"
    );
    Ok(())
}

#[tokio::test]
async fn v2_proxy_session_helper_forwards_invalid_success_without_closing_connection()
-> Result<(), Error> {
    let attempts = Arc::new(AtomicUsize::new(0));
    let agent_attempts = Arc::clone(&attempts);
    let agent = Agent
        .builder()
        .without_acp_version_guard()
        .on_receive_request(
            async move |request: UntypedMessage,
                        responder: agent_client_protocol::Responder<serde_json::Value>,
                        _cx| {
                match request.method() {
                    "initialize" => responder.respond(
                        serde_json::to_value(initialize_response())
                            .map_err(Error::into_internal_error)?,
                    ),
                    "session/new" if agent_attempts.fetch_add(1, Ordering::SeqCst) == 0 => {
                        responder.respond(serde_json::json!({
                            "_futureResponseField": {
                                "preserved": false,
                            },
                        }))
                    }
                    "session/new" => responder.respond(serde_json::json!({
                        "sessionId": "recovered-session",
                        "_futureResponseField": {
                            "preserved": true,
                        },
                    })),
                    method => responder.respond_with_error(
                        Error::method_not_found().data(format!("unexpected method `{method}`")),
                    ),
                }
            },
            agent_client_protocol::on_receive_request!(),
        );

    let callback_count = Arc::new(AtomicUsize::new(0));
    let proxy_callback_count = Arc::clone(&callback_count);
    let proxy = Proxy.v2().on_receive_request_from(
        Client,
        async move |request: v2::NewSessionRequest, responder, cx: V2ConnectionTo<Conductor>| {
            let callback_count = Arc::clone(&proxy_callback_count);
            cx.build_session_from(request).on_proxy_session_start(
                responder,
                move |_opened| async move {
                    callback_count.fetch_add(1, Ordering::SeqCst);
                    Ok(())
                },
            )
        },
        agent_client_protocol::on_receive_request!(),
    );

    run_with_conductor(ProxiesAndAgent::new(agent).proxy(proxy), async move |cx| {
        cx.send_request(initialize_request()).block_task().await?;

        let error = cx
            .send_request(v2::NewSessionRequest::new("/malformed-session"))
            .block_task()
            .await
            .expect_err("a malformed downstream success must be rejected");
        assert!(
            error.to_string().contains("sessionId"),
            "unexpected malformed response error: {error:?}"
        );

        let response = cx
            .send_request(v2::NewSessionRequest::new("/recovered-session"))
            .block_task()
            .await?;
        assert_eq!(response.session_id, v2::SessionId::new("recovered-session"));
        Ok(())
    })
    .await?;

    assert_eq!(attempts.load(Ordering::SeqCst), 2);
    assert_eq!(
        callback_count.load(Ordering::SeqCst),
        1,
        "the callback must run only for the valid setup"
    );
    Ok(())
}

#[tokio::test]
async fn v2_nested_conductor_preserves_exact_version_unknown_fields() -> Result<(), Error> {
    let initialize = initialize_request();
    let request = ExtendedInitializeRequest {
        initialize,
        future_initialize_field: Some(serde_json::json!({
            "nested": true,
            "preserved": true,
        })),
    };
    let response = initialize_response();
    let expected_request = request.clone();
    let expected_response = response.clone();
    let sequence = Arc::new(AtomicUsize::new(0));
    let agent_sequence = Arc::clone(&sequence);
    let agent = Agent.v2().on_receive_request(
        async move |request: ExtendedInitializeRequest, responder, _cx| {
            assert_eq!(
                agent_sequence.fetch_add(1, Ordering::SeqCst),
                0,
                "v2 agent initialized more than once"
            );
            assert_eq!(request, expected_request);
            responder.respond(expected_response.clone())
        },
        agent_client_protocol::on_receive_request!(),
    );
    let nested_conductor = ConductorImpl::new_proxy(
        "v2-nested-conductor",
        Vec::<RecordingPassthroughProxy>::new(),
    );

    run_raw_client_with_conductor(
        ProxiesAndAgent::new(agent).proxy(nested_conductor),
        async move |cx| {
            let received = cx.send_request(request).block_task().await?;
            assert_eq!(received, response);
            Ok(())
        },
    )
    .await?;

    assert_eq!(sequence.load(Ordering::SeqCst), 1);
    Ok(())
}

#[tokio::test]
async fn v2_nested_conductor_canonicalizes_future_version_fields() -> Result<(), Error> {
    let future_version = ProtocolVersion::from(3_u16);
    let mut initialize = initialize_request();
    initialize.protocol_version = future_version;
    let request = ExtendedInitializeRequest {
        initialize,
        future_initialize_field: Some(serde_json::json!({
            "nested": true,
            "future-only": true,
        })),
    };
    let response = initialize_response();
    let mut expected_request = request.clone();
    expected_request.initialize.protocol_version = ProtocolVersion::V2;
    expected_request.future_initialize_field = None;
    let expected_response = response.clone();
    let sequence = Arc::new(AtomicUsize::new(0));
    let agent_sequence = Arc::clone(&sequence);
    let agent = Agent.v2().on_receive_request(
        async move |request: ExtendedInitializeRequest, responder, _cx| {
            assert_eq!(
                agent_sequence.fetch_add(1, Ordering::SeqCst),
                0,
                "v2 agent initialized more than once"
            );
            assert_eq!(request, expected_request);
            responder.respond(expected_response.clone())
        },
        agent_client_protocol::on_receive_request!(),
    );
    let nested_conductor = ConductorImpl::new_proxy(
        "v2-nested-conductor",
        Vec::<RecordingPassthroughProxy>::new(),
    );

    run_raw_client_with_conductor(
        ProxiesAndAgent::new(agent).proxy(nested_conductor),
        async move |cx| {
            let received = cx.send_request(request).block_task().await?;
            assert_eq!(received, response);
            Ok(())
        },
    )
    .await?;

    assert_eq!(sequence.load(Ordering::SeqCst), 1);
    Ok(())
}

#[tokio::test]
async fn v2_initialize_rejects_v1_only_instantiator_with_response() -> Result<(), Error> {
    let request = initialize_request();
    let v1_only_instantiator = |_request: v1::InitializeRequest| async move {
        Err::<
            (
                v1::InitializeRequest,
                Vec<DynConnectTo<Conductor>>,
                DynConnectTo<Client>,
            ),
            Error,
        >(Error::internal_error().data("v1 instantiator unexpectedly called for v2"))
    };
    run_with_conductor(v1_only_instantiator, async move |cx| {
        for attempt in 1..=2 {
            let error = tokio::time::timeout(
                std::time::Duration::from_secs(2),
                cx.send_request(request.clone()).block_task(),
            )
            .await
            .expect("v1-only instantiator rejection should not hang")
            .expect_err("v1-only instantiator must reject v2 initialize");
            assert!(
                error
                    .to_string()
                    .contains("does not support ACP protocol v2"),
                "unexpected v2 rejection on attempt {attempt}: {error:?}"
            );
        }
        Ok(())
    })
    .await
}
