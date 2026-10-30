#![cfg(feature = "unstable_protocol_v2")]

use agent_client_protocol::RawJsonRpcId;
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

use agent_client_protocol::schema::{InitializeProxyRequest, ProtocolVersion, v1, v2};
use agent_client_protocol::{
    ByteStreams, Channel, Client, Conductor, ConnectTo, Error, Proxy, ProxyProtocolRouter,
    RawJsonRpcMessage, RawJsonRpcResponse, TransportFrame,
};
use futures::StreamExt as _;
use serde_json::Value;
use tokio_util::compat::{TokioAsyncReadCompatExt, TokioAsyncWriteCompatExt};

fn v1_proxy_initialize(protocol_version: ProtocolVersion) -> InitializeProxyRequest {
    InitializeProxyRequest::from(
        v1::InitializeRequest::new(protocol_version)
            .client_info(v1::Implementation::new("router-test-client", "1.0.0")),
    )
}

fn v2_proxy_initialize(protocol_version: ProtocolVersion) -> v2::InitializeProxyRequest {
    v2::InitializeProxyRequest::new(v2::InitializeRequest::new(
        protocol_version,
        v2::Implementation::new("router-test-client", "1.0.0"),
    ))
}

fn router(v1_hits: Arc<AtomicUsize>, v2_hits: Arc<AtomicUsize>) -> ProxyProtocolRouter {
    Proxy
        .protocol_router()
        .with_v1(v1_proxy(v1_hits))
        .with_v2(v2_proxy(v2_hits))
}

fn v1_proxy(v1_hits: Arc<AtomicUsize>) -> impl ConnectTo<Conductor> {
    Proxy.builder().on_receive_request_from(
        Client,
        async move |request: InitializeProxyRequest, responder, _cx| {
            v1_hits.fetch_add(1, Ordering::SeqCst);
            responder.respond(v1::InitializeResponse::new(
                request.initialize.protocol_version,
            ))
        },
        agent_client_protocol::on_receive_request!(),
    )
}

fn v2_proxy(v2_hits: Arc<AtomicUsize>) -> impl ConnectTo<Conductor> {
    Proxy.v2().on_receive_request_from(
        Client,
        async move |request: v2::InitializeProxyRequest, responder, _cx| {
            v2_hits.fetch_add(1, Ordering::SeqCst);
            responder.respond(v2::InitializeResponse::new(
                request.initialize.protocol_version,
                v2::Implementation::new("router-test-proxy", "1.0.0"),
            ))
        },
        agent_client_protocol::on_receive_request!(),
    )
}

async fn initialize(
    router: ProxyProtocolRouter,
    params: Value,
) -> Result<Result<Value, Error>, Error> {
    request(router, "proxy/initialize", params).await
}

async fn request(
    router: impl ConnectTo<Conductor>,
    method: &str,
    params: Value,
) -> Result<Result<Value, Error>, Error> {
    let (Channel { mut rx, tx }, future) = ConnectTo::<Conductor>::into_channel_and_future(router);
    let task = tokio::spawn(future.expect("proxy router owns a connection driver"));
    let request_id = RawJsonRpcId::Number(1);

    tx.unbounded_send(TransportFrame::Single(RawJsonRpcMessage::request(
        method.into(),
        params,
        request_id.clone(),
    )?))
    .map_err(Error::into_internal_error)?;

    let result = loop {
        let frame = rx.next().await.ok_or_else(|| {
            Error::internal_error().data("proxy router closed before initialize response")
        })?;
        let TransportFrame::Single(RawJsonRpcMessage::Response(response)) = frame else {
            continue;
        };
        match response {
            RawJsonRpcResponse::Result { id, result } if id == request_id => break Ok(result),
            RawJsonRpcResponse::Error { id, error } if id == request_id => {
                break Err(error.into_acp_error());
            }
            _ => {}
        }
    };

    drop(tx);
    drop(rx);
    task.await.map_err(Error::into_internal_error)??;
    Ok(result)
}

#[tokio::test(flavor = "current_thread")]
async fn routes_exact_versions_to_their_implementation() -> Result<(), Error> {
    let v1_hits = Arc::new(AtomicUsize::new(0));
    let v2_hits = Arc::new(AtomicUsize::new(0));

    let response = initialize(
        router(Arc::clone(&v1_hits), Arc::clone(&v2_hits)),
        serde_json::to_value(v1_proxy_initialize(ProtocolVersion::V1))
            .map_err(Error::into_internal_error)?,
    )
    .await??;
    assert_eq!(response["protocolVersion"], 1);

    let response = initialize(
        router(Arc::clone(&v1_hits), Arc::clone(&v2_hits)),
        serde_json::to_value(v2_proxy_initialize(ProtocolVersion::V2))
            .map_err(Error::into_internal_error)?,
    )
    .await??;
    assert_eq!(response["protocolVersion"], 2);

    assert_eq!(v1_hits.load(Ordering::SeqCst), 1);
    assert_eq!(v2_hits.load(Ordering::SeqCst), 1);
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn v1_only_router_answers_newer_versions_as_v1() -> Result<(), Error> {
    for requested in [2_u16, 3] {
        let v1_hits = Arc::new(AtomicUsize::new(0));
        let response = initialize(
            Proxy
                .protocol_router()
                .with_v1(v1_proxy(Arc::clone(&v1_hits))),
            serde_json::to_value(v2_proxy_initialize(ProtocolVersion::from(requested)))
                .map_err(Error::into_internal_error)?,
        )
        .await??;
        assert_eq!(response["protocolVersion"], 1);
        assert_eq!(v1_hits.load(Ordering::SeqCst), 1);
    }
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn v2_only_router_rejects_v1_like_a_v2_agent() -> Result<(), Error> {
    let v2_hits = Arc::new(AtomicUsize::new(0));
    let error = initialize(
        Proxy
            .protocol_router()
            .with_v2(v2_proxy(Arc::clone(&v2_hits))),
        serde_json::to_value(v1_proxy_initialize(ProtocolVersion::V1))
            .map_err(Error::into_internal_error)?,
    )
    .await?
    .expect_err("a v2-only router must reject v1");
    assert_eq!(i32::from(error.code), -32022);
    assert_eq!(
        error.data,
        Some(serde_json::json!({ "requested": 1, "supported": [2] }))
    );
    assert_eq!(v2_hits.load(Ordering::SeqCst), 0);
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn requires_proxy_initialize_as_the_first_call() -> Result<(), Error> {
    let error = request(
        Proxy
            .protocol_router()
            .with_v1(v1_proxy(Arc::new(AtomicUsize::new(0)))),
        "initialize",
        serde_json::to_value(v1_proxy_initialize(ProtocolVersion::V1))
            .map_err(Error::into_internal_error)?,
    )
    .await?
    .expect_err("an ordinary initialize request must not select a proxy");

    // The request does not select v2, so the v1 proxy answers it exactly as it
    // would without the router.
    let standalone = request(
        v1_proxy(Arc::new(AtomicUsize::new(0))),
        "initialize",
        serde_json::to_value(v1_proxy_initialize(ProtocolVersion::V1))
            .map_err(Error::into_internal_error)?,
    )
    .await?
    .expect_err("a v1 proxy rejects an ordinary initialize request");
    assert_eq!(error, standalone);
    assert_eq!(i32::from(error.code), -32600);
    assert!(
        error
            .data
            .as_ref()
            .and_then(Value::as_str)
            .is_some_and(|data| data.contains("`proxy/initialize`")),
        "{error:?}"
    );
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn selects_v2_for_newer_versions_like_an_agent_router() -> Result<(), Error> {
    let v1_hits = Arc::new(AtomicUsize::new(0));
    let v2_hits = Arc::new(AtomicUsize::new(0));

    let mut params = serde_json::to_value(v2_proxy_initialize(ProtocolVersion::from(3_u16)))
        .map_err(Error::into_internal_error)?;
    let object = params
        .as_object_mut()
        .expect("serialized initialize params should be an object");
    // Written for a newer version: keep what parses under v2.
    object.insert("info".into(), serde_json::json!("not-a-v2-implementation"));
    object.insert("_futureInitializeField".into(), serde_json::json!(true));

    let response = initialize(router(Arc::clone(&v1_hits), Arc::clone(&v2_hits)), params).await??;

    assert_eq!(response["protocolVersion"], 2);
    assert_eq!(v1_hits.load(Ordering::SeqCst), 0);
    assert_eq!(v2_hits.load(Ordering::SeqCst), 1);
    Ok(())
}

fn initialize_params_with_extensions(protocol_version: ProtocolVersion) -> Result<Value, Error> {
    let mut params = if protocol_version == ProtocolVersion::V1 {
        serde_json::to_value(v1_proxy_initialize(protocol_version))
    } else {
        serde_json::to_value(v2_proxy_initialize(protocol_version))
    }
    .map_err(Error::into_internal_error)?;

    let params = params
        .as_object_mut()
        .expect("serialized initialize params should be an object");
    params.insert(
        "_futureInitializeField".into(),
        serde_json::json!({ "preserved": true }),
    );
    Ok(Value::Object(params.clone()))
}

async fn write_wire_json(
    writer: &mut (impl tokio::io::AsyncWrite + Unpin),
    value: &Value,
) -> Result<(), Error> {
    use tokio::io::AsyncWriteExt as _;

    let mut bytes = serde_json::to_vec(value).map_err(Error::into_internal_error)?;
    bytes.push(b'\n');
    writer
        .write_all(&bytes)
        .await
        .map_err(Error::into_internal_error)?;
    writer.flush().await.map_err(Error::into_internal_error)
}

async fn read_wire_json(
    reader: &mut (impl tokio::io::AsyncBufRead + Unpin),
) -> Result<Value, Error> {
    use tokio::io::AsyncBufReadExt as _;

    let mut line = String::new();
    reader
        .read_line(&mut line)
        .await
        .map_err(Error::into_internal_error)?;
    serde_json::from_str(line.trim()).map_err(Error::into_internal_error)
}

#[tokio::test(flavor = "current_thread")]
async fn preserves_the_complete_initial_transport_frame() -> Result<(), Error> {
    use tokio::io::{AsyncWriteExt as _, BufReader};

    for protocol_version in [ProtocolVersion::V1, ProtocolVersion::V2] {
        let (mut conductor_writer, router_reader) = tokio::io::duplex(4096);
        let (router_writer, _conductor_reader) = tokio::io::duplex(4096);
        let conductor_transport =
            ByteStreams::new(router_writer.compat_write(), router_reader.compat());

        let (router_to_proxy_writer, proxy_reader) = tokio::io::duplex(4096);
        let (mut proxy_writer, proxy_to_router_reader) = tokio::io::duplex(4096);
        let external_proxy_transport = ByteStreams::new(
            router_to_proxy_writer.compat_write(),
            proxy_to_router_reader.compat(),
        );
        let router = if protocol_version == ProtocolVersion::V1 {
            Proxy.protocol_router().with_v1(external_proxy_transport)
        } else {
            Proxy.protocol_router().with_v2(external_proxy_transport)
        };
        let router_task = tokio::spawn(router.connect_to(conductor_transport));
        let mut proxy_reader = BufReader::new(proxy_reader);

        let initialize = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "proxy/initialize",
            "params": initialize_params_with_extensions(protocol_version)?,
        });
        write_wire_json(&mut conductor_writer, &initialize).await?;
        assert_eq!(read_wire_json(&mut proxy_reader).await?, initialize);

        conductor_writer
            .shutdown()
            .await
            .map_err(Error::into_internal_error)?;
        proxy_writer
            .shutdown()
            .await
            .map_err(Error::into_internal_error)?;
        router_task.await.map_err(Error::into_internal_error)??;
    }

    Ok(())
}
