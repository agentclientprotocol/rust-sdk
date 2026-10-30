use agent_client_protocol::RawJsonRpcId;
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

use agent_client_protocol::{
    Agent, Channel, Client, ConnectTo, Error, RawJsonRpcMessage, RawJsonRpcResponse,
    TransportFrame,
    schema::{ProtocolVersion, v1},
};
use futures::StreamExt as _;
use serde_json::{Value, json};

async fn request(
    channel: &mut Channel,
    id: i64,
    method: &str,
    params: Value,
) -> Result<Value, Error> {
    channel
        .tx
        .unbounded_send(TransportFrame::Single(RawJsonRpcMessage::request(
            method.into(),
            params,
            RawJsonRpcId::Number(id),
        )?))
        .map_err(Error::into_internal_error)?;
    let frame = tokio::time::timeout(std::time::Duration::from_secs(5), channel.rx.next())
        .await
        .map_err(Error::into_internal_error)?
        .expect("agent must respond before closing");
    match frame {
        TransportFrame::Single(RawJsonRpcMessage::Response(RawJsonRpcResponse::Result {
            id: response_id,
            result,
            ..
        })) => {
            assert_eq!(response_id, RawJsonRpcId::Number(id));
            Ok(json!({ "result": result }))
        }
        TransportFrame::Single(RawJsonRpcMessage::Response(RawJsonRpcResponse::Error {
            id: response_id,
            error,
            ..
        })) => {
            assert_eq!(response_id, RawJsonRpcId::Number(id));
            Ok(json!({
                "error": {
                    "code": error.code,
                    "message": error.message,
                    "data": error.data.value(),
                }
            }))
        }
        other => panic!("expected a response for {method}, got {other:?}"),
    }
}

// This file deliberately runs both with and without unstable_protocol_v2.
#[tokio::test(flavor = "current_thread")]
async fn standalone_v1_keeps_legacy_selection_and_serves_v1_sessions() -> Result<(), Error> {
    for requested in [0_u16, 1, 2, 3, u16::MAX] {
        let initialized = Arc::new(AtomicUsize::new(0));
        let calls = initialized.clone();
        let agent = Agent
            .v1()
            .on_receive_request(
                async move |initialize: v1::InitializeRequest, responder, _cx| {
                    calls.fetch_add(1, Ordering::SeqCst);
                    assert_eq!(initialize.protocol_version, ProtocolVersion::V1);
                    if requested >= 2 {
                        assert_eq!(
                            initialize
                                .client_info
                                .as_ref()
                                .map(|info| info.name.as_str()),
                            Some("newer-client")
                        );
                        assert!(initialize.client_capabilities.auth.terminal);
                        assert!(!initialize.client_capabilities.fs.read_text_file);
                        assert!(!initialize.client_capabilities.fs.write_text_file);
                        assert!(!initialize.client_capabilities.terminal);
                        let wire = serde_json::to_value(&initialize)?;
                        assert_eq!(wire["_meta"], json!({ "request": true }));
                        assert_eq!(
                            wire["clientCapabilities"]["_meta"],
                            json!({ "capability": true })
                        );
                        assert_eq!(
                            wire["clientCapabilities"]["session"]["configOptions"]["boolean"],
                            json!({})
                        );
                    }
                    responder.respond(v1::InitializeResponse::new(initialize.protocol_version))
                },
                agent_client_protocol::on_receive_request!(),
            )
            .on_receive_request(
                async |_request: v1::NewSessionRequest, responder, _cx| {
                    responder.respond(v1::NewSessionResponse::new(v1::SessionId::new("native-v1")))
                },
                agent_client_protocol::on_receive_request!(),
            );
        let (mut channel, driver) = ConnectTo::<Client>::into_channel_and_future(agent);
        let task = tokio::spawn(driver.expect("agent owns a driver"));
        let params = if requested < 2 {
            json!({ "protocolVersion": requested, "clientCapabilities": {} })
        } else {
            json!({
                "protocolVersion": requested,
                "info": { "name": "newer-client", "version": "1" },
                "capabilities": {
                    "auth": { "terminal": { "_meta": { "marker": true } } },
                    "_meta": { "capability": true },
                    "fs": { "readTextFile": true, "writeTextFile": true },
                    "terminal": true,
                },
                "_meta": { "request": true },
            })
        };
        let initialize = request(&mut channel, 1, "initialize", params).await?;
        assert_eq!(initialize["result"]["protocolVersion"], 1);
        assert_eq!(initialized.load(Ordering::SeqCst), 1);
        if requested >= 1 {
            let params = serde_json::to_value(v1::NewSessionRequest::new(
                std::env::current_dir().map_err(Error::into_internal_error)?,
            ))?;
            let session = request(&mut channel, 2, "session/new", params).await?;
            assert_eq!(session["result"]["sessionId"], "native-v1");
        }
        task.abort();
    }
    Ok(())
}

#[cfg(feature = "unstable_protocol_v2")]
#[tokio::test(flavor = "current_thread")]
async fn v1_only_router_preserves_legacy_fallback() -> Result<(), Error> {
    let agent = Agent
        .protocol_router()
        .with_v1(Agent.v1().on_receive_request(
            async |initialize: v1::InitializeRequest, responder, _cx| {
                assert_eq!(initialize.protocol_version, ProtocolVersion::V1);
                responder.respond(v1::InitializeResponse::new(initialize.protocol_version))
            },
            agent_client_protocol::on_receive_request!(),
        ));
    let (mut channel, driver) = ConnectTo::<Client>::into_channel_and_future(agent);
    let task = tokio::spawn(driver.expect("router owns a driver"));
    let response = request(
        &mut channel,
        1,
        "initialize",
        json!({ "protocolVersion": 0, "clientCapabilities": {} }),
    )
    .await?;
    assert_eq!(response["result"]["protocolVersion"], 1);
    task.abort();
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn standalone_v1_rejects_malformed_initialization_before_dispatch() -> Result<(), Error> {
    for params in [
        json!({}),
        json!({ "protocolVersion": "1" }),
        json!({ "protocolVersion": -1 }),
        json!({ "protocolVersion": 1.5 }),
        json!({ "protocolVersion": 65536 }),
    ] {
        let agent = Agent.v1().on_receive_request(
            async |_initialize: v1::InitializeRequest, responder, _cx| {
                responder
                    .respond_with_internal_error("malformed initialization reached the handler")
            },
            agent_client_protocol::on_receive_request!(),
        );
        let (mut channel, driver) = ConnectTo::<Client>::into_channel_and_future(agent);
        let task = tokio::spawn(driver.expect("agent owns a driver"));
        let response = request(&mut channel, 1, "initialize", params).await?;
        assert_eq!(response["error"]["code"], -32602, "{response}");
        task.abort();
    }
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn standalone_v1_ignores_unparsable_newer_initialization_params() -> Result<(), Error> {
    for params in [
        json!({ "protocolVersion": 2 }),
        json!({ "protocolVersion": 2, "info": { "name": 123, "version": "1" } }),
        json!({ "protocolVersion": 2, "info": "malformed", "_meta": 1, "capabilities": [] }),
    ] {
        let agent = Agent.v1().on_receive_request(
            async |initialize: v1::InitializeRequest, responder, _cx| {
                assert_eq!(initialize.protocol_version, ProtocolVersion::V1);
                assert_eq!(initialize.client_info, None);
                assert_eq!(initialize.meta, None);
                responder.respond(v1::InitializeResponse::new(initialize.protocol_version))
            },
            agent_client_protocol::on_receive_request!(),
        );
        let (mut channel, driver) = ConnectTo::<Client>::into_channel_and_future(agent);
        let task = tokio::spawn(driver.expect("agent owns a driver"));
        let response = request(&mut channel, 1, "initialize", params).await?;
        assert_eq!(response["result"]["protocolVersion"], 1, "{response}");
        task.abort();
    }
    Ok(())
}

/// A v1 proxy negotiates `proxy/initialize` exactly as a v1 agent negotiates
/// `initialize`: every request selects v1, and a newer request keeps what
/// parses under v1.
#[tokio::test(flavor = "current_thread")]
async fn standalone_v1_proxy_negotiates_like_a_v1_agent() -> Result<(), Error> {
    for requested in [0_u16, 1, 2, 3, u16::MAX] {
        let proxy = agent_client_protocol::Proxy.v1().on_receive_request_from(
            Client,
            async move |request: agent_client_protocol::schema::InitializeProxyRequest,
                        responder,
                        _cx| {
                assert_eq!(request.initialize.protocol_version, ProtocolVersion::V1);
                if requested >= 2 {
                    assert_eq!(
                        request
                            .initialize
                            .client_info
                            .as_ref()
                            .map(|info| info.name.as_str()),
                        Some("newer-client")
                    );
                }
                responder.respond(v1::InitializeResponse::new(
                    request.initialize.protocol_version,
                ))
            },
            agent_client_protocol::on_receive_request!(),
        );
        let (mut channel, driver) =
            ConnectTo::<agent_client_protocol::Conductor>::into_channel_and_future(proxy);
        let task = tokio::spawn(driver.expect("proxy owns a driver"));
        let params = if requested < 2 {
            json!({ "protocolVersion": requested, "clientCapabilities": {} })
        } else {
            json!({
                "protocolVersion": requested,
                "info": { "name": "newer-client", "version": "1" },
                "capabilities": { "_future": true },
            })
        };
        let response = request(&mut channel, 1, "proxy/initialize", params).await?;
        assert_eq!(response["result"]["protocolVersion"], 1, "{response}");
        task.abort();
    }
    Ok(())
}

fn v1_agent_with_initialize_and_new_session() -> impl ConnectTo<Client> {
    Agent
        .v1()
        .on_receive_request(
            async |initialize: v1::InitializeRequest, responder, _cx| {
                responder.respond(v1::InitializeResponse::new(initialize.protocol_version))
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            async |_request: v1::NewSessionRequest, responder, _cx| {
                responder.respond(v1::NewSessionResponse::new("v1-session"))
            },
            agent_client_protocol::on_receive_request!(),
        )
}

#[tokio::test(flavor = "current_thread")]
async fn v1_agent_with_initialize_handler_rejects_requests_before_initialization()
-> Result<(), Error> {
    let (mut channel, driver) =
        ConnectTo::<Client>::into_channel_and_future(v1_agent_with_initialize_and_new_session());
    let task = tokio::spawn(driver.expect("agent owns a driver"));
    let new_session = json!({ "cwd": "/tmp", "mcpServers": [] });

    let response = request(&mut channel, 1, "session/new", new_session.clone()).await?;
    assert_eq!(response["error"]["code"], -32600, "{response}");

    let initialize = json!({ "protocolVersion": 1, "clientCapabilities": {} });
    let response = request(&mut channel, 2, "initialize", initialize.clone()).await?;
    assert_eq!(response["result"]["protocolVersion"], 1, "{response}");
    let response = request(&mut channel, 3, "session/new", new_session).await?;
    assert_eq!(response["result"]["sessionId"], "v1-session", "{response}");

    // V1 keeps accepting a repeated initialize.
    let response = request(&mut channel, 4, "initialize", initialize).await?;
    assert_eq!(response["result"]["protocolVersion"], 1, "{response}");
    task.abort();
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn v1_agent_without_initialize_handler_serves_requests_without_initialization()
-> Result<(), Error> {
    let agent = Agent.v1().on_receive_request(
        async |_request: v1::NewSessionRequest, responder, _cx| {
            responder.respond(v1::NewSessionResponse::new("v1-session"))
        },
        agent_client_protocol::on_receive_request!(),
    );
    let (mut channel, driver) = ConnectTo::<Client>::into_channel_and_future(agent);
    let task = tokio::spawn(driver.expect("agent owns a driver"));
    let response = request(
        &mut channel,
        1,
        "session/new",
        json!({ "cwd": "/tmp", "mcpServers": [] }),
    )
    .await?;
    assert_eq!(response["result"]["sessionId"], "v1-session", "{response}");
    task.abort();
    Ok(())
}
