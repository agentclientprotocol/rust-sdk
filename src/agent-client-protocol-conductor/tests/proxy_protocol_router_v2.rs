#![cfg(feature = "unstable_protocol_v2")]

use std::sync::{
    Arc, Mutex,
    atomic::{AtomicUsize, Ordering},
};

use agent_client_protocol::schema::{InitializeProxyRequest, ProtocolVersion, v1, v2};
use agent_client_protocol::{
    Agent, ByteStreams, Client, Conductor, ConnectionTo, Error, Proxy, V2ConnectionTo,
};
use agent_client_protocol_conductor::{ConductorImpl, ProxiesAndAgent};
use tokio::io::duplex;
use tokio_util::compat::{TokioAsyncReadCompatExt, TokioAsyncWriteCompatExt};

#[derive(Default)]
struct ProtocolObservations {
    proxy_initializations: AtomicUsize,
    proxy_list_sessions: AtomicUsize,
    agent_initializations: AtomicUsize,
    agent_list_sessions: AtomicUsize,
    proxy_versions: Mutex<Vec<ProtocolVersion>>,
    agent_versions: Mutex<Vec<ProtocolVersion>>,
}

#[derive(Default)]
struct Observations {
    v1: ProtocolObservations,
    v2: ProtocolObservations,
}

impl ProtocolObservations {
    fn record_proxy_initialize(&self, version: ProtocolVersion) {
        self.proxy_initializations.fetch_add(1, Ordering::SeqCst);
        self.proxy_versions
            .lock()
            .expect("proxy versions lock should not be poisoned")
            .push(version);
    }

    fn record_agent_initialize(&self, version: ProtocolVersion) {
        self.agent_initializations.fetch_add(1, Ordering::SeqCst);
        self.agent_versions
            .lock()
            .expect("agent versions lock should not be poisoned")
            .push(version);
    }

    fn assert_selected(&self, version: ProtocolVersion) {
        assert_eq!(self.proxy_initializations.load(Ordering::SeqCst), 1);
        assert_eq!(self.proxy_list_sessions.load(Ordering::SeqCst), 1);
        assert_eq!(self.agent_initializations.load(Ordering::SeqCst), 1);
        assert_eq!(self.agent_list_sessions.load(Ordering::SeqCst), 1);
        assert_eq!(
            *self
                .proxy_versions
                .lock()
                .expect("proxy versions lock should not be poisoned"),
            [version]
        );
        assert_eq!(
            *self
                .agent_versions
                .lock()
                .expect("agent versions lock should not be poisoned"),
            [version]
        );
    }

    fn assert_not_selected(&self) {
        assert_eq!(self.proxy_initializations.load(Ordering::SeqCst), 0);
        assert_eq!(self.proxy_list_sessions.load(Ordering::SeqCst), 0);
        assert_eq!(self.agent_initializations.load(Ordering::SeqCst), 0);
        assert_eq!(self.agent_list_sessions.load(Ordering::SeqCst), 0);
        assert!(
            self.proxy_versions
                .lock()
                .expect("proxy versions lock should not be poisoned")
                .is_empty()
        );
        assert!(
            self.agent_versions
                .lock()
                .expect("agent versions lock should not be poisoned")
                .is_empty()
        );
    }
}

fn components(observations: Arc<Observations>) -> ProxiesAndAgent {
    let v1_proxy_initialize = Arc::clone(&observations);
    let v1_proxy_list = Arc::clone(&observations);
    let v1_proxy = Proxy
        .builder()
        .name("v1-proxy")
        .on_receive_request_from(
            Client,
            async move |request: InitializeProxyRequest, responder, cx| {
                v1_proxy_initialize
                    .v1
                    .record_proxy_initialize(request.initialize.protocol_version);
                cx.send_request_to(Agent, request.initialize)
                    .forward_response_to(responder)
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request_from(
            Client,
            async move |request: v1::ListSessionsRequest, responder, cx| {
                v1_proxy_list
                    .v1
                    .proxy_list_sessions
                    .fetch_add(1, Ordering::SeqCst);
                cx.send_request_to(Agent, request)
                    .forward_response_to(responder)
            },
            agent_client_protocol::on_receive_request!(),
        );

    let v2_proxy_initialize = Arc::clone(&observations);
    let v2_proxy_list = Arc::clone(&observations);
    let v2_proxy = Proxy
        .v2()
        .name("v2-proxy")
        .on_receive_request_from(
            Client,
            async move |request: v2::InitializeProxyRequest,
                        responder,
                        cx: V2ConnectionTo<Conductor>| {
                v2_proxy_initialize
                    .v2
                    .record_proxy_initialize(request.initialize.protocol_version);
                cx.send_request_to(Agent, request.initialize)
                    .forward_response_to(responder)
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request_from(
            Client,
            async move |request: v2::ListSessionsRequest,
                        responder,
                        cx: V2ConnectionTo<Conductor>| {
                v2_proxy_list
                    .v2
                    .proxy_list_sessions
                    .fetch_add(1, Ordering::SeqCst);
                cx.send_request_to(Agent, request)
                    .forward_response_to(responder)
            },
            agent_client_protocol::on_receive_request!(),
        );

    let v1_agent_initialize = Arc::clone(&observations);
    let v1_agent_list = Arc::clone(&observations);
    let v1_agent = Agent
        .builder()
        .name("v1-agent")
        .on_receive_request(
            async move |request: v1::InitializeRequest, responder, _cx| {
                v1_agent_initialize
                    .v1
                    .record_agent_initialize(request.protocol_version);
                responder.respond(v1::InitializeResponse::new(request.protocol_version))
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            async move |_request: v1::ListSessionsRequest, responder, _cx| {
                v1_agent_list
                    .v1
                    .agent_list_sessions
                    .fetch_add(1, Ordering::SeqCst);
                responder.respond(v1::ListSessionsResponse::new(Vec::new()))
            },
            agent_client_protocol::on_receive_request!(),
        );

    let v2_agent_initialize = Arc::clone(&observations);
    let v2_agent_list = observations;
    let v2_agent = Agent
        .v2()
        .name("v2-agent")
        .on_receive_request(
            async move |request: v2::InitializeRequest, responder, _cx| {
                v2_agent_initialize
                    .v2
                    .record_agent_initialize(request.protocol_version);
                responder.respond(v2::InitializeResponse::new(
                    request.protocol_version,
                    v2::Implementation::new("v2-agent", "1.0.0"),
                ))
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            async move |_request: v2::ListSessionsRequest, responder, _cx| {
                v2_agent_list
                    .v2
                    .agent_list_sessions
                    .fetch_add(1, Ordering::SeqCst);
                responder.respond(v2::ListSessionsResponse::new(Vec::new()))
            },
            agent_client_protocol::on_receive_request!(),
        );

    let proxy = Proxy.protocol_router().with_v1(v1_proxy).with_v2(v2_proxy);
    let agent = Agent.protocol_router().with_v1(v1_agent).with_v2(v2_agent);

    ProxiesAndAgent::new(agent).proxy(proxy)
}

async fn run_v1_client(
    components: ProxiesAndAgent,
    client_task: impl AsyncFnOnce(ConnectionTo<Agent>) -> Result<(), Error>,
) -> Result<(), Error> {
    let (client_out, conductor_in) = duplex(4096);
    let (conductor_out, client_in) = duplex(4096);

    Client
        .builder()
        .name("v1-client")
        .with_spawned(|_cx| async move {
            ConductorImpl::new_agent("conductor", components)
                .run(ByteStreams::new(
                    conductor_out.compat_write(),
                    conductor_in.compat(),
                ))
                .await
        })
        .connect_with(
            ByteStreams::new(client_out.compat_write(), client_in.compat()),
            client_task,
        )
        .await
}

async fn run_v2_client(
    components: ProxiesAndAgent,
    client_task: impl AsyncFnOnce(V2ConnectionTo<Agent>) -> Result<(), Error>,
) -> Result<(), Error> {
    let (client_out, conductor_in) = duplex(4096);
    let (conductor_out, client_in) = duplex(4096);

    Client
        .v2()
        .name("v2-client")
        .with_spawned(|_cx| async move {
            ConductorImpl::new_agent("conductor", components)
                .run(ByteStreams::new(
                    conductor_out.compat_write(),
                    conductor_in.compat(),
                ))
                .await
        })
        .connect_with(
            ByteStreams::new(client_out.compat_write(), client_in.compat()),
            client_task,
        )
        .await
}

async fn run_raw_client(
    components: ProxiesAndAgent,
    client_task: impl AsyncFnOnce(ConnectionTo<Agent>) -> Result<(), Error>,
) -> Result<(), Error> {
    let (client_out, conductor_in) = duplex(4096);
    let (conductor_out, client_in) = duplex(4096);

    Client
        .builder()
        .without_acp_version_guard()
        .name("future-client")
        .with_spawned(|_cx| async move {
            ConductorImpl::new_agent("conductor", components)
                .run(ByteStreams::new(
                    conductor_out.compat_write(),
                    conductor_in.compat(),
                ))
                .await
        })
        .connect_with(
            ByteStreams::new(client_out.compat_write(), client_in.compat()),
            client_task,
        )
        .await
}

#[tokio::test]
async fn v1_conductor_routes_initialize_and_later_requests_to_v1() -> Result<(), Error> {
    let observations = Arc::new(Observations::default());

    run_v1_client(components(Arc::clone(&observations)), async |cx| {
        let response = cx
            .send_request(v1::InitializeRequest::new(ProtocolVersion::V1))
            .block_task()
            .await?;
        assert_eq!(response.protocol_version, ProtocolVersion::V1);

        let response = cx
            .send_request(v1::ListSessionsRequest::new())
            .block_task()
            .await?;
        assert!(response.sessions.is_empty());
        Ok(())
    })
    .await?;

    observations.v1.assert_selected(ProtocolVersion::V1);
    observations.v2.assert_not_selected();
    Ok(())
}

#[tokio::test]
async fn v2_conductor_routes_initialize_and_later_requests_to_v2() -> Result<(), Error> {
    let observations = Arc::new(Observations::default());

    run_v2_client(components(Arc::clone(&observations)), async |cx| {
        let response = cx
            .send_request(v2::InitializeRequest::new(
                ProtocolVersion::V2,
                v2::Implementation::new("v2-client", "1.0.0"),
            ))
            .block_task()
            .await?;
        assert_eq!(response.protocol_version, ProtocolVersion::V2);

        let response = cx
            .send_request(v2::ListSessionsRequest::new())
            .block_task()
            .await?;
        assert!(response.sessions.is_empty());
        Ok(())
    })
    .await?;

    observations.v1.assert_not_selected();
    observations.v2.assert_selected(ProtocolVersion::V2);
    Ok(())
}

#[tokio::test]
async fn conductor_canonicalizes_future_version_before_proxy_routing() -> Result<(), Error> {
    let observations = Arc::new(Observations::default());

    run_raw_client(components(Arc::clone(&observations)), async |cx| {
        let response = cx
            .send_request(v2::InitializeRequest::new(
                ProtocolVersion::from(3_u16),
                v2::Implementation::new("future-client", "1.0.0"),
            ))
            .block_task()
            .await?;
        assert_eq!(response.protocol_version, ProtocolVersion::V2);

        let response = cx
            .send_request(v2::ListSessionsRequest::new())
            .block_task()
            .await?;
        assert!(response.sessions.is_empty());
        Ok(())
    })
    .await?;

    observations.v1.assert_not_selected();
    observations.v2.assert_selected(ProtocolVersion::V2);
    Ok(())
}
