//! Integration tests for the initialization sequence.
//!
//! These tests verify that:
//! 1. Single-component chains receive `InitializeRequest` (agent mode)
//! 2. Multi-component chains: proxies receive `InitializeProxyRequest`
//! 3. Last component (agent) receives `InitializeRequest`

use agent_client_protocol_core::schema::{
    AgentCapabilities, InitializeProxyRequest, InitializeRequest, InitializeResponse,
    ProtocolVersion,
};
use agent_client_protocol_core::{Agent, Client, Conductor, ConnectTo, DynConnectTo, Proxy};
use agent_client_protocol_conductor::{ConductorImpl, ProxiesAndAgent};
use agent_client_protocol_test::testy::Testy;
use std::sync::Arc;
use std::sync::Mutex;

use tokio::io::duplex;
use tokio_util::compat::{TokioAsyncReadCompatExt, TokioAsyncWriteCompatExt};

/// Test helper to receive a JSON-RPC response
async fn recv<T: agent_client_protocol_core::JsonRpcResponse + Send>(
    response: agent_client_protocol_core::SentRequest<T>,
) -> Result<T, agent_client_protocol_core::Error> {
    let (tx, rx) = tokio::sync::oneshot::channel();
    response.on_receiving_result(async move |result| {
        tx.send(result).map_err(|_| agent_client_protocol_core::Error::internal_error())
    })?;
    rx.await.map_err(|_| agent_client_protocol_core::Error::internal_error())?
}

/// Tracks what type of initialization request was received
#[derive(Debug, Clone, PartialEq)]
enum InitRequestType {
    Initialize,
    InitializeProxy,
}

struct InitConfig {
    /// What type of init request was received
    received_init_type: Mutex<Option<InitRequestType>>,
}

impl InitConfig {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            received_init_type: Mutex::new(None),
        })
    }

    fn read_init_type(&self) -> Option<InitRequestType> {
        self.received_init_type
            .lock()
            .expect("not poisoned")
            .clone()
    }
}

struct InitComponent {
    config: Arc<InitConfig>,
}

impl InitComponent {
    fn new(config: &Arc<InitConfig>) -> Self {
        Self {
            config: config.clone(),
        }
    }
}

impl ConnectTo<Conductor> for InitComponent {
    async fn connect_to(self, client: impl ConnectTo<Proxy>) -> Result<(), agent_client_protocol_core::Error> {
        let config = self.config;
        let config2 = Arc::clone(&config);

        Proxy
            .builder()
            .name("init-component")
            // Handle InitializeProxyRequest (we're a proxy)
            .on_receive_request_from(
                Client,
                async move |request: InitializeProxyRequest, responder, cx| {
                    *config.received_init_type.lock().expect("unpoisoned") =
                        Some(InitRequestType::InitializeProxy);

                    // Forward InitializeRequest (not InitializeProxyRequest) to successor
                    cx.send_request_to(agent_client_protocol_core::Agent, request.initialize)
                        .on_receiving_result(async move |response| {
                            let response: InitializeResponse = response?;
                            responder.respond(response)
                        })
                },
                agent_client_protocol_core::on_receive_request!(),
            )
            // Handle InitializeRequest (we're the agent)
            .on_receive_request_from(
                Client,
                async move |request: InitializeRequest, responder, _cx| {
                    *config2.received_init_type.lock().expect("unpoisoned") =
                        Some(InitRequestType::Initialize);

                    // We're the final component, just respond
                    let response = InitializeResponse::new(request.protocol_version)
                        .agent_capabilities(AgentCapabilities::new());

                    responder.respond(response)
                },
                agent_client_protocol_core::on_receive_request!(),
            )
            .connect_to(client)
            .await
    }
}

async fn run_test_with_components(
    proxies: Vec<InitComponent>,
    editor_task: impl AsyncFnOnce(agent_client_protocol_core::ConnectionTo<Agent>) -> Result<(), agent_client_protocol_core::Error>,
) -> Result<(), agent_client_protocol_core::Error> {
    // Set up editor <-> conductor communication
    let (editor_out, conductor_in) = duplex(1024);
    let (conductor_out, editor_in) = duplex(1024);

    let transport = agent_client_protocol_core::ByteStreams::new(editor_out.compat_write(), editor_in.compat());

    agent_client_protocol_core::Client
        .builder()
        .name("editor-to-connector")
        .with_spawned(|_cx| async move {
            ConductorImpl::new_agent(
                "conductor".to_string(),
                ProxiesAndAgent::new(Testy::new()).proxies(proxies),
                Default::default(),
            )
            .run(agent_client_protocol_core::ByteStreams::new(
                conductor_out.compat_write(),
                conductor_in.compat(),
            ))
            .await
        })
        .connect_with(transport, editor_task)
        .await
}

#[tokio::test]
async fn test_single_component_gets_initialize_request() -> Result<(), agent_client_protocol_core::Error> {
    // Single component (agent) should receive InitializeRequest - we use ElizaAgent
    // which properly handles InitializeRequest
    run_test_with_components(vec![], async |connection_to_editor| {
        let init_response = recv(
            connection_to_editor.send_request(InitializeRequest::new(ProtocolVersion::LATEST)),
        )
        .await;

        assert!(
            init_response.is_ok(),
            "Initialize should succeed: {:?}",
            init_response
        );

        Ok::<(), agent_client_protocol_core::Error>(())
    })
    .await?;

    Ok(())
}

#[tokio::test]
async fn test_two_components_proxy_gets_initialize_proxy() -> Result<(), agent_client_protocol_core::Error> {
    // First component (proxy) gets InitializeProxyRequest
    // Second component (agent, ElizaAgent) gets InitializeRequest
    let component1 = InitConfig::new();

    run_test_with_components(
        vec![InitComponent::new(&component1)],
        async |connection_to_editor| {
            let init_response = recv(
                connection_to_editor.send_request(InitializeRequest::new(ProtocolVersion::LATEST)),
            )
            .await;

            assert!(
                init_response.is_ok(),
                "Initialize should succeed: {:?}",
                init_response
            );

            Ok::<(), agent_client_protocol_core::Error>(())
        },
    )
    .await?;

    // First component (proxy) should receive InitializeProxyRequest
    assert_eq!(
        component1.read_init_type(),
        Some(InitRequestType::InitializeProxy),
        "Proxy component should receive InitializeProxyRequest"
    );

    // Second component (ElizaAgent) receives InitializeRequest implicitly

    Ok(())
}

#[tokio::test]
async fn test_three_components_all_proxies_get_initialize_proxy() -> Result<(), agent_client_protocol_core::Error> {
    // First two components (proxies) get InitializeProxyRequest
    // Third component (agent, ElizaAgent) gets InitializeRequest
    let component1 = InitConfig::new();
    let component2 = InitConfig::new();

    run_test_with_components(
        vec![
            InitComponent::new(&component1),
            InitComponent::new(&component2),
        ],
        async |connection_to_editor| {
            let init_response = recv(
                connection_to_editor.send_request(InitializeRequest::new(ProtocolVersion::LATEST)),
            )
            .await;

            assert!(
                init_response.is_ok(),
                "Initialize should succeed: {:?}",
                init_response
            );

            Ok::<(), agent_client_protocol_core::Error>(())
        },
    )
    .await?;

    // First two components (proxies) should receive InitializeProxyRequest
    assert_eq!(
        component1.read_init_type(),
        Some(InitRequestType::InitializeProxy),
        "First proxy should receive InitializeProxyRequest"
    );
    assert_eq!(
        component2.read_init_type(),
        Some(InitRequestType::InitializeProxy),
        "Second proxy should receive InitializeProxyRequest"
    );

    // Third component (ElizaAgent) receives InitializeRequest implicitly

    Ok(())
}

/// A proxy that incorrectly forwards InitializeProxyRequest instead of InitializeRequest.
/// This tests that the conductor rejects such malformed forwarding.
struct BadProxy;

impl ConnectTo<Conductor> for BadProxy {
    async fn connect_to(self, client: impl ConnectTo<Proxy>) -> Result<(), agent_client_protocol_core::Error> {
        Proxy
            .builder()
            .name("bad-proxy")
            .on_receive_request_from(
                Client,
                async move |request: InitializeProxyRequest, responder, cx| {
                    // BUG: forwards InitializeProxyRequest instead of request.initialize
                    cx.send_request_to(Agent, request)
                        .on_receiving_result(async move |response| {
                            let response: InitializeResponse = response?;
                            responder.respond(response)
                        })
                },
                agent_client_protocol_core::on_receive_request!(),
            )
            .connect_to(client)
            .await
    }
}

/// Run test with explicit proxy and agent DynComponents (for mixing different types)
async fn run_bad_proxy_test(
    proxies: Vec<DynConnectTo<Conductor>>,
    agent: DynConnectTo<Client>,
    editor_task: impl AsyncFnOnce(agent_client_protocol_core::ConnectionTo<Agent>) -> Result<(), agent_client_protocol_core::Error>,
) -> Result<(), agent_client_protocol_core::Error> {
    let (editor_out, conductor_in) = duplex(1024);
    let (conductor_out, editor_in) = duplex(1024);

    let transport = agent_client_protocol_core::ByteStreams::new(editor_out.compat_write(), editor_in.compat());

    agent_client_protocol_core::Client
        .builder()
        .name("editor-to-connector")
        .with_spawned(|_cx| async move {
            ConductorImpl::new_agent(
                "conductor".to_string(),
                ProxiesAndAgent::new(agent).proxies(proxies),
                Default::default(),
            )
            .run(agent_client_protocol_core::ByteStreams::new(
                conductor_out.compat_write(),
                conductor_in.compat(),
            ))
            .await
        })
        .connect_with(transport, editor_task)
        .await
}

#[tokio::test]
async fn test_conductor_rejects_initialize_proxy_forwarded_to_agent() -> Result<(), agent_client_protocol_core::Error> {
    // BadProxy incorrectly forwards InitializeProxyRequest to the agent.
    // The conductor should reject this with an error.
    let result = run_bad_proxy_test(
        vec![DynConnectTo::new(BadProxy)],
        DynConnectTo::new(Testy::new()),
        async |connection_to_editor| {
            let init_response = recv(
                connection_to_editor.send_request(InitializeRequest::new(ProtocolVersion::LATEST)),
            )
            .await;

            if let Err(err) = init_response {
                assert!(
                    err.to_string().contains("initialize/proxy"),
                    "Error should mention initialize/proxy: {:?}",
                    err
                );
            }

            Ok::<(), agent_client_protocol_core::Error>(())
        },
    )
    .await;

    match result {
        Ok(()) => panic!("Expected error when proxy forwards InitializeProxyRequest to agent"),
        Err(err) => {
            assert!(
                err.to_string().contains("initialize/proxy"),
                "Error should mention initialize/proxy: {:?}",
                err
            );
        }
    }

    Ok(())
}

#[tokio::test]
async fn test_conductor_rejects_initialize_proxy_forwarded_to_proxy() -> Result<(), agent_client_protocol_core::Error> {
    // BadProxy incorrectly forwards InitializeProxyRequest to another proxy.
    // The conductor should reject this with an error.
    let result = run_bad_proxy_test(
        vec![
            DynConnectTo::new(BadProxy),
            DynConnectTo::new(InitComponent::new(&InitConfig::new())), // This proxy will receive the bad request
        ],
        DynConnectTo::new(Testy::new()), // Agent
        async |connection_to_editor| {
            let init_response = recv(
                connection_to_editor.send_request(InitializeRequest::new(ProtocolVersion::LATEST)),
            )
            .await;

            // The error may come through recv() or bubble up through the test harness
            if let Err(err) = init_response {
                assert!(
                    err.to_string().contains("initialize/proxy"),
                    "Error should mention initialize/proxy: {:?}",
                    err
                );
            }

            Ok::<(), agent_client_protocol_core::Error>(())
        },
    )
    .await;

    // The error might bubble up through run_test_with_components instead
    match result {
        Ok(()) => panic!("Expected error when proxy forwards InitializeProxyRequest to proxy"),
        Err(err) => {
            assert!(
                err.to_string().contains("initialize/proxy"),
                "Error should mention initialize/proxy: {:?}",
                err
            );
        }
    }

    Ok(())
}
