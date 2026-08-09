use std::{
    io,
    sync::{Arc, Mutex},
    time::Duration,
};

use agent_client_protocol::{
    ActiveSession, Agent, Channel, Client, Conductor, ConnectionTo, RawJsonRpcMessage, Responder,
    SessionMessage, TransportBatch, TransportFrame,
    schema::v1::{
        ContentBlock, ContentChunk, NewSessionRequest, NewSessionResponse, PromptRequest,
        PromptResponse, SessionId, SessionNotification, SessionUpdate, StopReason, TextContent,
    },
};
use futures::{
    StreamExt as _,
    channel::{mpsc, oneshot},
};

#[cfg(feature = "unstable_protocol_v2")]
use agent_client_protocol::{
    JsonRpcMessage, JsonRpcResponse, Proxy, V2ConnectionTo,
    schema::{ProtocolVersion, SuccessorMessage, v2},
};

const TIMEOUT: Duration = Duration::from_secs(10);
const SENSITIVE_SESSION_UPDATE_CONTENT: &str = "TOP_SECRET_SESSION_UPDATE_SENTINEL";

#[derive(Clone)]
struct SharedLogWriter(Arc<Mutex<Vec<u8>>>);

impl io::Write for SharedLogWriter {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        io::Write::write(&mut *self.0.lock().unwrap(), buffer)
    }

    fn flush(&mut self) -> io::Result<()> {
        io::Write::flush(&mut *self.0.lock().unwrap())
    }
}

// Compile-time regressions for the callback future bounds on the two non-blocking
// session helpers. The callbacks themselves are `'static`, but their future
// types deliberately carry an arbitrary shorter lifetime.
#[allow(dead_code)]
mod callback_future_lifetimes {
    use std::{
        future::Future,
        marker::PhantomData,
        pin::Pin,
        task::{Context, Poll},
    };

    use super::*;

    struct LifetimeTaggedFuture<'a>(PhantomData<&'a ()>);

    impl Future for LifetimeTaggedFuture<'_> {
        type Output = Result<(), agent_client_protocol::Error>;

        fn poll(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Self::Output> {
            Poll::Ready(Ok(()))
        }
    }

    fn session_callback<'a>()
    -> impl FnOnce(ActiveSession<'static, Agent>) -> LifetimeTaggedFuture<'a> + Send + 'static {
        |_session| LifetimeTaggedFuture(PhantomData)
    }

    fn proxy_session_callback<'a>()
    -> impl FnOnce(SessionId) -> LifetimeTaggedFuture<'a> + Send + 'static {
        |_session_id| LifetimeTaggedFuture(PhantomData)
    }

    #[cfg(feature = "unstable_protocol_v2")]
    fn v2_proxy_session_callback<'a>() -> impl FnOnce(
        agent_client_protocol::OpenedV2Session<Conductor, v2::NewSessionResponse>,
    ) -> LifetimeTaggedFuture<'a>
    + Send
    + 'static {
        |_opened| LifetimeTaggedFuture(PhantomData)
    }

    fn on_session_start_accepts_non_static_callback_future<'a>(
        connection: &ConnectionTo<Agent>,
        _scope: &'a str,
    ) -> Result<(), agent_client_protocol::Error> {
        connection
            .build_session_cwd()?
            .on_session_start(session_callback::<'a>())
    }

    fn on_proxy_session_start_accepts_non_static_callback_future<'a>(
        connection: &ConnectionTo<Conductor>,
        request: NewSessionRequest,
        responder: Responder<NewSessionResponse>,
        _scope: &'a str,
    ) -> Result<(), agent_client_protocol::Error> {
        connection
            .build_session_from(request)
            .on_proxy_session_start(responder, proxy_session_callback::<'a>())
    }

    #[cfg(feature = "unstable_protocol_v2")]
    fn v2_on_proxy_session_start_accepts_non_static_callback_future<'a>(
        connection: &V2ConnectionTo<Conductor>,
        request: v2::NewSessionRequest,
        responder: Responder<v2::NewSessionResponse>,
        _scope: &'a str,
    ) -> Result<(), agent_client_protocol::Error> {
        connection
            .build_session_from(request)
            .on_proxy_session_start(responder, v2_proxy_session_callback::<'a>())
    }
}

#[tokio::test(flavor = "current_thread")]
async fn on_session_start_callback_can_consume_later_session_messages() {
    let logs = Arc::new(Mutex::new(Vec::new()));
    let writer = SharedLogWriter(logs.clone());
    let subscriber = tracing_subscriber::fmt()
        .with_env_filter("agent_client_protocol::session=trace")
        .without_time()
        .with_ansi(false)
        .with_writer(move || writer.clone())
        .finish();
    let dispatch = tracing::Dispatch::new(subscriber);
    let _guard = tracing::dispatcher::set_default(&dispatch);

    let session_id = SessionId::new("ordered-session");
    let new_session_id = session_id.clone();
    let prompt_session_id = session_id.clone();

    let agent = Agent
        .builder()
        .on_receive_request(
            async move |_request: NewSessionRequest,
                        responder: Responder<NewSessionResponse>,
                        _connection: ConnectionTo<Client>| {
                responder.respond(NewSessionResponse::new(new_session_id.clone()))
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            async move |request: PromptRequest,
                        responder: Responder<PromptResponse>,
                        connection: ConnectionTo<Client>| {
                assert_eq!(request.session_id, prompt_session_id);
                connection.send_notification(SessionNotification::new(
                    request.session_id,
                    SessionUpdate::AgentMessageChunk(ContentChunk::new(ContentBlock::Text(
                        TextContent::new(SENSITIVE_SESSION_UPDATE_CONTENT),
                    ))),
                ))?;
                responder.respond(PromptResponse::new(StopReason::EndTurn))
            },
            agent_client_protocol::on_receive_request!(),
        );

    let (result_tx, result_rx) = oneshot::channel();
    let client = Client
        .builder()
        .connect_with(agent, async move |connection| {
            connection
                .build_session_cwd()?
                .on_session_start(async move |mut session| {
                    session.send_prompt("test ordering")?;
                    let text = session.read_to_string().await?;
                    result_tx
                        .send(text)
                        .map_err(|_| agent_client_protocol::Error::internal_error())
                })?;

            let text = result_rx
                .await
                .map_err(|_| agent_client_protocol::Error::internal_error())?;
            assert_eq!(text, SENSITIVE_SESSION_UPDATE_CONTENT);
            Ok(())
        });

    tokio::time::timeout(TIMEOUT, client)
        .await
        .expect("session callback deadlocked the incoming dispatch loop")
        .expect("session connection failed");

    let logs = String::from_utf8(logs.lock().unwrap().clone()).unwrap();
    assert!(
        logs.contains("read_to_string update"),
        "session update trace was not captured:\n{logs}"
    );
    assert!(
        !logs.contains(SENSITIVE_SESSION_UPDATE_CONTENT),
        "session update content leaked into logs:\n{logs}"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn on_session_start_installs_routing_before_later_batch_entry() {
    let session_id = SessionId::new("same-batch-session");
    let response_session_id = session_id.clone();
    let notification_session_id = session_id.clone();
    let (transport, mut peer) = Channel::duplex();
    let (result_tx, result_rx) = oneshot::channel();

    let client = Client
        .builder()
        .connect_with(transport, async move |connection| {
            connection
                .build_session_cwd()?
                .on_session_start(async move |mut session| {
                    let update = session.read_update().await?;
                    assert!(matches!(update, SessionMessage::SessionMessage(_)));
                    result_tx
                        .send(())
                        .map_err(|()| agent_client_protocol::Error::internal_error())
                })?;

            result_rx
                .await
                .map_err(|_| agent_client_protocol::Error::internal_error())?;
            Ok(())
        });

    let peer = async move {
        let Some(TransportFrame::Single(RawJsonRpcMessage::Request(request))) =
            peer.rx.next().await
        else {
            panic!("expected a session/new request");
        };
        assert_eq!(request.method.as_ref(), "session/new");

        let response = RawJsonRpcMessage::response(
            request.id,
            Ok(
                serde_json::to_value(NewSessionResponse::new(response_session_id))
                    .expect("session response should serialize"),
            ),
        );
        let notification = RawJsonRpcMessage::notification(
            "session/update".into(),
            serde_json::to_value(SessionNotification::new(
                notification_session_id,
                SessionUpdate::AgentMessageChunk(ContentChunk::new(ContentBlock::Text(
                    TextContent::new("same batch"),
                ))),
            ))
            .expect("session notification should serialize"),
        )
        .expect("session notification should form valid JSON-RPC parameters");
        let batch = TransportBatch::from_messages([response, notification])
            .expect("test batch should be non-empty");
        peer.tx
            .unbounded_send(TransportFrame::Batch(batch))
            .expect("client should accept the response batch");

        while peer.rx.next().await.is_some() {}
        Ok::<(), agent_client_protocol::Error>(())
    };

    tokio::time::timeout(TIMEOUT, async { futures::try_join!(client, peer) })
        .await
        .expect("same-batch session update was not routed")
        .expect("session connection failed");
}

#[cfg(feature = "unstable_protocol_v2")]
#[tokio::test(flavor = "current_thread")]
async fn v2_proxy_session_start_installs_routing_before_later_batch_entry() {
    let session_id = v2::SessionId::new("same-batch-v2-session");
    let setup_response = v2::NewSessionResponse::new(session_id.clone()).config_options(vec![
        v2::SessionConfigOption::boolean("thinking", "Thinking", true),
    ]);
    let callback_response = setup_response.clone();
    let response_session_id = session_id.clone();
    let notification_session_id = session_id.clone();
    let (transport, mut peer) = Channel::duplex();
    let (callback_tx, mut callback_rx) = mpsc::unbounded();
    let (peer_done_tx, peer_done_rx) = oneshot::channel();

    let proxy = Proxy
        .v2()
        .on_receive_request_from(
            Client,
            async |request: v2::InitializeProxyRequest, responder, _connection| {
                responder.respond(v2::InitializeResponse::new(
                    request.initialize.protocol_version,
                    v2::Implementation::new("same-batch-proxy", env!("CARGO_PKG_VERSION")),
                ))
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request_from(
            Client,
            async move |request: v2::NewSessionRequest,
                        responder,
                        connection: V2ConnectionTo<Conductor>| {
                let callback_response = callback_response.clone();
                let response_session_id = response_session_id.clone();
                let callback_tx = callback_tx.clone();
                connection
                    .build_session_from(request)
                    .on_proxy_session_start(responder, move |opened| async move {
                        assert_eq!(opened.session().session_id(), &response_session_id);
                        assert_eq!(opened.response(), &callback_response);
                        callback_tx.unbounded_send(()).map_err(|_| {
                            agent_client_protocol::Error::internal_error()
                                .data("v2 proxy callback receiver was dropped")
                        })
                    })
            },
            agent_client_protocol::on_receive_request!(),
        )
        .connect_with(transport, async move |_connection| {
            callback_rx.next().await.ok_or_else(|| {
                agent_client_protocol::Error::internal_error().data("v2 proxy callback did not run")
            })?;
            peer_done_rx.await.map_err(|_| {
                agent_client_protocol::Error::internal_error().data("raw peer stopped early")
            })
        });

    let peer = async move {
        let initialize_id = agent_client_protocol::schema::v1::RequestId::Number(1);
        let initialize = v2::InitializeProxyRequest::new(v2::InitializeRequest::new(
            ProtocolVersion::V2,
            v2::Implementation::new("same-batch-client", env!("CARGO_PKG_VERSION")),
        ));
        peer.tx
            .unbounded_send(TransportFrame::Single(RawJsonRpcMessage::request(
                "_proxy/initialize".to_owned(),
                serde_json::to_value(initialize).expect("initialize request should serialize"),
                initialize_id.clone(),
            )?))
            .expect("proxy should accept initialization");

        let Some(TransportFrame::Single(RawJsonRpcMessage::Response(
            agent_client_protocol::schema::v1::Response::Result { id, result },
        ))) = peer.rx.next().await
        else {
            panic!("expected the proxy initialize response");
        };
        assert_eq!(id, initialize_id);
        let initialize_response = v2::InitializeResponse::from_value("_proxy/initialize", result)?;
        assert_eq!(initialize_response.protocol_version, ProtocolVersion::V2);

        let upstream_id = agent_client_protocol::schema::v1::RequestId::Number(2);
        peer.tx
            .unbounded_send(TransportFrame::Single(RawJsonRpcMessage::request(
                "session/new".to_owned(),
                serde_json::to_value(v2::NewSessionRequest::new("/same-batch-v2-session"))
                    .expect("session request should serialize"),
                upstream_id.clone(),
            )?))
            .expect("proxy should accept session/new");

        let Some(TransportFrame::Single(RawJsonRpcMessage::Request(forwarded))) =
            peer.rx.next().await
        else {
            panic!("expected a forwarded session/new request");
        };
        let successor = SuccessorMessage::<v2::NewSessionRequest>::parse_message(
            forwarded.method.as_ref(),
            &forwarded.params,
        )?;
        assert_eq!(
            successor.message.cwd,
            v2::AbsolutePath::new("/same-batch-v2-session")
        );

        let response = RawJsonRpcMessage::response(
            forwarded.id,
            Ok(serde_json::to_value(setup_response).expect("session response should serialize")),
        );
        let update = SuccessorMessage {
            message: v2::UpdateSessionNotification::new(
                notification_session_id,
                v2::SessionUpdate::StateUpdate(v2::StateUpdate::Running(
                    v2::RunningStateUpdate::new(),
                )),
            ),
            meta: None,
        }
        .to_untyped_message()?;
        let (method, params) = update.into_parts();
        let notification = RawJsonRpcMessage::notification(method, params)?;
        let batch = TransportBatch::from_messages([response, notification])
            .expect("test response batch should be non-empty");
        peer.tx
            .unbounded_send(TransportFrame::Batch(batch))
            .expect("proxy should accept the response batch");

        let mut saw_response = false;
        let mut saw_update = false;
        for _ in 0..2 {
            let Some(TransportFrame::Single(message)) = peer.rx.next().await else {
                panic!("expected a forwarded session response and update");
            };
            match message {
                RawJsonRpcMessage::Response(
                    agent_client_protocol::schema::v1::Response::Result { id, result },
                ) => {
                    assert_eq!(id, upstream_id);
                    let response = v2::NewSessionResponse::from_value("session/new", result)?;
                    assert_eq!(response.session_id, session_id);
                    saw_response = true;
                }
                RawJsonRpcMessage::Notification(notification) => {
                    let update = v2::UpdateSessionNotification::parse_message(
                        notification.method.as_ref(),
                        &notification.params,
                    )?;
                    assert_eq!(update.session_id, session_id);
                    assert!(matches!(
                        update.update,
                        v2::SessionUpdate::StateUpdate(v2::StateUpdate::Running(_))
                    ));
                    saw_update = true;
                }
                message => panic!("unexpected proxy output: {message:?}"),
            }
        }
        assert!(saw_response);
        assert!(saw_update);
        peer_done_tx
            .send(())
            .map_err(|()| agent_client_protocol::Error::internal_error())
    };

    tokio::time::timeout(TIMEOUT, async { futures::try_join!(proxy, peer) })
        .await
        .expect("same-batch v2 session update was not routed")
        .expect("v2 proxy session connection failed");
}
