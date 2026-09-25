//! Request-scoped MCP 2026-07-28 Streamable HTTP adapter for native ACP MCP servers.
//!
//! Native-capable successors receive the original declarations and messages unchanged.
//! HTTP-only successors receive loopback endpoints; no MCP connection or session is created.

pub(crate) mod http;
mod protocol;

use std::{
    collections::HashMap,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
};

use agent_client_protocol::{
    Agent, Client, Conductor, ConnectTo, ConnectionTo, Dispatch, HandleDispatchFrom, Handled,
    Proxy, UntypedMessage, schema::v1::MessageMcpResponse, util::MatchDispatchFrom,
};
use futures::{
    SinkExt, StreamExt,
    channel::{mpsc, oneshot},
};
use serde_json::Value;
use tokio::{net::TcpListener, sync::mpsc as tokio_mpsc};
use tracing::{debug, warn};

use self::protocol::{DownstreamMcpMode, NativeMcpNotification, PolyfillProtocol};

// Conservative per-bridge limits. Notifications are bounded per HTTP POST by
// both message count and serialized bytes; terminal responses bypass the queue.
const MAX_ACTIVE_REQUESTS: usize = 64;
const MAX_QUEUED_NOTIFICATIONS: usize = 16;
const MAX_QUEUED_BYTES: usize = 256 * 1024;
const MAX_TERMINAL_BYTES: usize = 1024 * 1024;
const LOCAL_LIMIT_ERROR: i64 = -33000;

struct QueuedNotification {
    value: Value,
    bytes: usize,
    used: Arc<AtomicUsize>,
}

impl Drop for QueuedNotification {
    fn drop(&mut self) {
        self.used.fetch_sub(self.bytes, Ordering::Relaxed);
    }
}

#[derive(Clone)]
struct StreamSender {
    tx: tokio_mpsc::Sender<QueuedNotification>,
    used: Arc<AtomicUsize>,
}

impl StreamSender {
    fn send(&self, value: Value) -> Result<(), ()> {
        let bytes = serde_json::to_vec(&value).map_err(|_| ())?.len();
        let reserved = self
            .used
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |used| {
                used.checked_add(bytes)
                    .filter(|total| *total <= MAX_QUEUED_BYTES)
            });
        if reserved.is_err() {
            return Err(());
        }
        self.tx
            .try_send(QueuedNotification {
                value,
                bytes,
                used: self.used.clone(),
            })
            .map_err(|_| ())
    }

    async fn closed(&self) {
        self.tx.closed().await;
    }
}

enum BridgeMessage {
    SetProtocol {
        protocol: PolyfillProtocol,
        downstream_mode: DownstreamMcpMode,
    },
    TransformServers {
        servers: Vec<Value>,
        response_tx: oneshot::Sender<Result<Vec<Value>, agent_client_protocol::Error>>,
    },
    Request {
        server_id: String,
        request_id: String,
        http_id: Value,
        method: String,
        params: Option<serde_json::Map<String, Value>>,
        response_tx: StreamSender,
        terminal_tx: tokio::sync::oneshot::Sender<Value>,
    },
    Notification(NativeMcpNotification),
    Finished {
        request_id: String,
        result: Option<Result<Value, agent_client_protocol::Error>>,
    },
}

/// Adapts native MCP-over-ACP servers to loopback Streamable HTTP for HTTP-only agents.
#[derive(Debug, Default)]
pub struct McpOverAcpPolyfill;

impl McpOverAcpPolyfill {
    #[must_use]
    pub fn http() -> Self {
        Self
    }
}

impl ConnectTo<Conductor> for McpOverAcpPolyfill {
    async fn connect_to(
        self,
        client: impl ConnectTo<Proxy>,
    ) -> Result<(), agent_client_protocol::Error> {
        #[cfg(feature = "unstable_protocol_v2")]
        {
            Proxy
                .protocol_router()
                .with_v1(McpOverAcpProxy(PolyfillProtocol::V1))
                .with_v2(McpOverAcpProxy(PolyfillProtocol::V2))
                .connect_to(client)
                .await
        }
        #[cfg(not(feature = "unstable_protocol_v2"))]
        {
            McpOverAcpProxy(PolyfillProtocol::V1)
                .connect_to(client)
                .await
        }
    }
}

#[derive(Debug)]
struct McpOverAcpProxy(PolyfillProtocol);

impl ConnectTo<Conductor> for McpOverAcpProxy {
    async fn connect_to(
        self,
        client: impl ConnectTo<Proxy>,
    ) -> Result<(), agent_client_protocol::Error> {
        let (bridge_tx, bridge_rx) = mpsc::channel(128);
        let runner = BridgeRunner {
            bridge_tx: bridge_tx.clone(),
            bridge_rx,
            protocol: None,
            downstream_mode: DownstreamMcpMode::Unknown,
            listener: None,
            active: HashMap::new(),
        };
        let handler = PolyfillHandler {
            protocol: None,
            bridge_tx,
        };
        match self.0 {
            PolyfillProtocol::V1 => {
                Proxy
                    .builder()
                    .name("mcp-over-acp-polyfill")
                    .with_runner(runner)
                    .with_handler(handler)
                    .connect_to(client)
                    .await
            }
            #[cfg(feature = "unstable_protocol_v2")]
            PolyfillProtocol::V2 => {
                Proxy
                    .v2()
                    .name("mcp-over-acp-polyfill")
                    .with_runner(runner)
                    .with_handler(handler)
                    .connect_to(client)
                    .await
            }
        }
    }
}

#[derive(Debug)]
struct PolyfillHandler {
    protocol: Option<PolyfillProtocol>,
    bridge_tx: mpsc::Sender<BridgeMessage>,
}

impl HandleDispatchFrom<Conductor> for PolyfillHandler {
    async fn handle_dispatch_from(
        &mut self,
        message: Dispatch,
        cx: ConnectionTo<Conductor>,
    ) -> Result<Handled<Dispatch>, agent_client_protocol::Error> {
        MatchDispatchFrom::new(message, &cx)
            .if_dispatch_from(Client, async |message: Dispatch| {
                self.handle_client_dispatch(message, &cx).await
            })
            .await
            .done()
    }

    fn describe_chain(&self) -> impl std::fmt::Debug {
        self
    }
}

impl PolyfillHandler {
    async fn handle_client_dispatch(
        &mut self,
        message: Dispatch,
        cx: &ConnectionTo<Conductor>,
    ) -> Result<Handled<Dispatch>, agent_client_protocol::Error> {
        match message {
            Dispatch::Request(mut request, responder) => {
                if request.method() == agent_client_protocol::schema::METHOD_INITIALIZE_PROXY {
                    if self.protocol.is_some() {
                        return Err(agent_client_protocol::Error::invalid_request()
                            .data("MCP-over-ACP polyfill was already initialized"));
                    }
                    let protocol = PolyfillProtocol::from_initialize_request(&request)?;
                    self.protocol = Some(protocol);
                    request.method = "initialize".into();
                    let sent = cx
                        .send_request_to(Agent, request)
                        .forward_cancellation_from(responder.cancellation());
                    let mut bridge_tx = self.bridge_tx.clone();
                    sent.on_receiving_result(async move |result| {
                        let result = match result {
                            Ok(mut response) => {
                                let mode = protocol.transform_initialize_response(&mut response)?;
                                bridge_tx
                                    .send(BridgeMessage::SetProtocol {
                                        protocol,
                                        downstream_mode: mode,
                                    })
                                    .await
                                    .map_err(agent_client_protocol::Error::into_internal_error)?;
                                Ok(response)
                            }
                            Err(error) => Err(error),
                        };
                        responder.respond_with_result(result)
                    })?;
                    return Ok(Handled::Yes);
                }
                let Some(protocol) = self.protocol else {
                    return Ok(Handled::No {
                        message: Dispatch::Request(request, responder),
                        retry: false,
                    });
                };
                if protocol.is_session_setup_method(request.method()) {
                    protocol.validate_session_setup_request(&request)?;
                    transform_session_servers(&mut request, &mut self.bridge_tx).await?;
                    cx.send_request_to(Agent, request)
                        .forward_response_to(responder)?;
                    return Ok(Handled::Yes);
                }
                // Only agent-to-provider requests are valid; reverse RPC is never forwarded.
                if request.method() == "mcp/message" {
                    responder
                        .respond_with_error(agent_client_protocol::Error::method_not_found())?;
                    return Ok(Handled::Yes);
                }
                Ok(Handled::No {
                    message: Dispatch::Request(request, responder),
                    retry: false,
                })
            }
            Dispatch::Notification(notification) => {
                if notification.method() == "mcp/message" {
                    let Some(protocol) = self.protocol else {
                        return Ok(Handled::No {
                            message: Dispatch::Notification(notification),
                            retry: false,
                        });
                    };
                    let notification = protocol.parse_notification(notification)?;
                    self.bridge_tx
                        .send(BridgeMessage::Notification(notification))
                        .await
                        .map_err(agent_client_protocol::Error::into_internal_error)?;
                    return Ok(Handled::Yes);
                }
                Ok(Handled::No {
                    message: Dispatch::Notification(notification),
                    retry: false,
                })
            }
            message @ Dispatch::Response(_, _) => Ok(Handled::No {
                message,
                retry: false,
            }),
        }
    }
}

async fn transform_session_servers(
    request: &mut UntypedMessage,
    bridge_tx: &mut mpsc::Sender<BridgeMessage>,
) -> Result<(), agent_client_protocol::Error> {
    let Some(servers) = request
        .params
        .as_object_mut()
        .and_then(|params| params.get_mut("mcpServers"))
        .and_then(Value::as_array_mut)
    else {
        return Ok(());
    };
    let (response_tx, response_rx) = oneshot::channel();
    bridge_tx
        .send(BridgeMessage::TransformServers {
            servers: std::mem::take(servers),
            response_tx,
        })
        .await
        .map_err(agent_client_protocol::Error::into_internal_error)?;
    *servers = response_rx
        .await
        .map_err(agent_client_protocol::Error::into_internal_error)??;
    Ok(())
}

struct ActiveRequest {
    server_id: String,
    http_id: Value,
    method: String,
    response_tx: StreamSender,
    terminal_tx: tokio::sync::oneshot::Sender<Value>,
    cancel_tx: tokio::sync::oneshot::Sender<()>,
}

struct BridgeRunner {
    bridge_tx: mpsc::Sender<BridgeMessage>,
    bridge_rx: mpsc::Receiver<BridgeMessage>,
    protocol: Option<PolyfillProtocol>,
    downstream_mode: DownstreamMcpMode,
    listener: Option<(u16, Arc<http::BridgeState>)>,
    active: HashMap<String, ActiveRequest>,
}

impl std::fmt::Debug for BridgeRunner {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BridgeRunner")
            .field("protocol", &self.protocol)
            .field("downstream_mode", &self.downstream_mode)
            .field("listener", &self.listener.is_some())
            .field("active", &self.active.len())
            .finish_non_exhaustive()
    }
}

impl agent_client_protocol::RunWithConnectionTo<Conductor> for BridgeRunner {
    async fn run_with_connection_to(
        mut self,
        connection: ConnectionTo<Conductor>,
    ) -> Result<(), agent_client_protocol::Error> {
        while let Some(message) = self.bridge_rx.next().await {
            match message {
                BridgeMessage::SetProtocol {
                    protocol,
                    downstream_mode,
                } => {
                    self.protocol = Some(protocol);
                    self.downstream_mode = downstream_mode;
                }
                BridgeMessage::TransformServers {
                    servers,
                    response_tx,
                } => {
                    let result = self.transform_servers(&connection, servers).await;
                    drop(response_tx.send(result));
                }
                BridgeMessage::Request {
                    server_id,
                    request_id,
                    http_id,
                    method,
                    params,
                    response_tx,
                    terminal_tx,
                } => {
                    let Some(protocol) = self
                        .protocol
                        .filter(|_| self.downstream_mode == DownstreamMcpMode::HttpAdapter)
                    else {
                        drop(terminal_tx.send(http::rpc_error(
                            http_id,
                            -33002,
                            "MCP adapter unavailable",
                        )));
                        continue;
                    };
                    if !self.can_admit_request() {
                        drop(terminal_tx.send(http::rpc_error(
                            http_id,
                            LOCAL_LIMIT_ERROR,
                            "Too many active MCP requests",
                        )));
                        continue;
                    }
                    let (cancel_tx, cancel_rx) = tokio::sync::oneshot::channel();
                    self.active.insert(
                        request_id.clone(),
                        ActiveRequest {
                            server_id: server_id.clone(),
                            http_id: http_id.clone(),
                            method: method.clone(),
                            response_tx: response_tx.clone(),
                            terminal_tx,
                            cancel_tx,
                        },
                    );
                    let mut tx = self.bridge_tx.clone();
                    let cx = connection.clone();
                    let request_id_for_task = request_id.clone();
                    connection.spawn(async move {
                        // Dropping the HTTP response stream cancels precisely this ACP request.
                        let result = tokio::select! {
                            result = forward_http_request(cx, protocol, server_id,
                                request_id_for_task, method, params) => Some(result),
                            () = response_tx.closed() => None,
                            _ = cancel_rx => None,
                        };
                        tx.send(BridgeMessage::Finished { request_id, result })
                            .await
                            .map_err(agent_client_protocol::Error::into_internal_error)?;
                        Ok(())
                    })?;
                }
                BridgeMessage::Notification(notification) => {
                    if self.downstream_mode == DownstreamMcpMode::Native {
                        connection.send_notification_to(Agent, notification.raw)?;
                    } else if self.downstream_mode == DownstreamMcpMode::HttpAdapter {
                        let Some(active) = self.active.get(&notification.request_id) else {
                            debug!("dropping notification for stale MCP request");
                            continue;
                        };
                        if active.server_id != notification.server_id {
                            warn!("dropping notification with mismatched MCP server");
                            continue;
                        }
                        let mut params = Value::Object(notification.params.unwrap_or_default());
                        http::rewrite_subscription_id(
                            &mut params,
                            &notification.request_id,
                            &active.http_id,
                        );
                        let message = serde_json::json!({
                            "jsonrpc": "2.0",
                            "method": notification.method,
                            "params": params,
                        });
                        if active.response_tx.send(message).is_err() {
                            // Stop only this request. Its final error goes through a
                            // separate control path that cannot be blocked by a full queue.
                            let active = self
                                .active
                                .remove(&notification.request_id)
                                .expect("active request checked above");
                            let _ = active.cancel_tx.send(());
                            drop(active.terminal_tx.send(http::rpc_error(
                                active.http_id,
                                LOCAL_LIMIT_ERROR,
                                "MCP notification queue overflow",
                            )));
                        }
                    }
                }
                BridgeMessage::Finished { request_id, result } => {
                    let Some(active) = self.active.remove(&request_id) else {
                        continue;
                    };
                    if let Some(result) = result {
                        let http_id = active.http_id.clone();
                        let value = match result {
                            Ok(carrier) => project_mcp_carrier(
                                active.http_id,
                                &request_id,
                                &active.method,
                                carrier,
                            ),
                            Err(error) => http::rpc_binding_error(active.http_id, error),
                        };
                        let value = if serde_json::to_vec(&value)
                            .is_ok_and(|bytes| bytes.len() <= MAX_TERMINAL_BYTES)
                        {
                            value
                        } else {
                            http::rpc_error(
                                http_id,
                                LOCAL_LIMIT_ERROR,
                                "MCP terminal response too large",
                            )
                        };
                        drop(active.terminal_tx.send(value));
                    }
                }
            }
        }
        Ok(())
    }
}

/// ACP success carries exactly one MCP outcome. An outer ACP failure is a
/// binding/runtime failure, not an MCP error carried in a successful response.
fn project_mcp_carrier(http_id: Value, request_id: &str, method: &str, carrier: Value) -> Value {
    // Both ACP revisions share this type. Keep envelope validation in the schema,
    // rather than maintaining a second parser that can drift from its null rules.
    match serde_json::from_value::<MessageMcpResponse>(carrier) {
        Ok(MessageMcpResponse::Result { mut result, .. }) => {
            if method == "tools/list" {
                strip_header_annotations(&mut result);
            }
            http::rpc_result(http_id, request_id, result)
        }
        Ok(MessageMcpResponse::Error { error, .. }) => http::rpc_peer_error(
            http_id,
            serde_json::to_value(error).expect("MCP errors contain only JSON values"),
        ),
        _ => http::rpc_error(http_id, -33002, "Invalid MCP-over-ACP response carrier"),
    }
}

impl BridgeRunner {
    fn can_admit_request(&self) -> bool {
        self.active.len() < MAX_ACTIVE_REQUESTS
    }

    async fn transform_servers(
        &mut self,
        connection: &ConnectionTo<Conductor>,
        servers: Vec<Value>,
    ) -> Result<Vec<Value>, agent_client_protocol::Error> {
        let protocol = self
            .protocol
            .ok_or_else(agent_client_protocol::Error::invalid_request)?;
        let mut transformed = Vec::with_capacity(servers.len());
        for server in servers {
            let Some(native) = protocol.native_server(server.clone()) else {
                transformed.push(server);
                continue;
            };
            match self.downstream_mode {
                DownstreamMcpMode::Native => transformed.push(server),
                DownstreamMcpMode::HttpAdapter => {
                    if self.listener.is_none() {
                        let listener = TcpListener::bind("127.0.0.1:0")
                            .await
                            .map_err(agent_client_protocol::Error::into_internal_error)?;
                        let port = listener
                            .local_addr()
                            .map_err(agent_client_protocol::Error::into_internal_error)?
                            .port();
                        let state = http::BridgeState::new(self.bridge_tx.clone());
                        connection.spawn(http::run_http_listener(listener, state.clone()))?;
                        self.listener = Some((port, state));
                    }
                    let (port, state) = self.listener.as_ref().expect("listener created");
                    let (url, token) = state.declaration_url(*port, &native.server_id);
                    transformed.push(native.http_declaration(protocol, url, &token)?);
                }
                DownstreamMcpMode::Unknown | DownstreamMcpMode::Unavailable => {
                    return Err(agent_client_protocol::Error::invalid_params().data(
                        "the downstream agent supports neither native nor HTTP MCP transport",
                    ));
                }
            }
        }
        Ok(transformed)
    }
}

async fn forward_http_request(
    connection: ConnectionTo<Conductor>,
    protocol: PolyfillProtocol,
    server_id: String,
    request_id: String,
    method: String,
    params: Option<serde_json::Map<String, Value>>,
) -> Result<Value, agent_client_protocol::Error> {
    let request = protocol.message_request(server_id, request_id, method, params, None)?;
    connection
        .send_request_to(Client, request)
        .block_task()
        .await
}

fn strip_header_annotations(result: &mut Value) {
    let Some(tools) = result.get_mut("tools").and_then(Value::as_array_mut) else {
        return;
    };
    for tool in tools {
        if let Some(schema) = tool.get_mut("inputSchema") {
            strip_schema_annotation(schema);
        }
    }
}

fn strip_schema_annotation(schema: &mut Value) {
    let Some(object) = schema.as_object_mut() else {
        return;
    };
    object.remove("x-mcp-header");
    for key in [
        "properties",
        "patternProperties",
        "$defs",
        "definitions",
        "dependentSchemas",
    ] {
        if let Some(children) = object.get_mut(key).and_then(Value::as_object_mut) {
            for child in children.values_mut() {
                strip_schema_annotation(child);
            }
        }
    }
    for key in [
        "items",
        "additionalItems",
        "additionalProperties",
        "unevaluatedItems",
        "unevaluatedProperties",
        "contains",
        "contentSchema",
        "not",
        "if",
        "then",
        "else",
        "propertyNames",
    ] {
        if let Some(child) = object.get_mut(key) {
            strip_schema_annotation(child);
        }
    }
    for key in ["allOf", "anyOf", "oneOf", "prefixItems"] {
        if let Some(children) = object.get_mut(key).and_then(Value::as_array_mut) {
            for child in children {
                strip_schema_annotation(child);
            }
        }
    }
}

#[cfg(test)]
mod http_limits_tests {
    use super::*;

    #[test]
    fn annotation_removal_only_traverses_schema_locations() {
        let mut listing = serde_json::json!({"tools":[{
            "name":"with-header",
            "inputSchema":{
                "type":"object",
                "properties":{
                    "x-mcp-header":{"type":"string","default":"retain"},
                    "nested":{"type":"object","x-mcp-header":"Nested","properties":{
                        "value":{"type":"string","x-mcp-header":"Value",
                            "examples":[{"x-mcp-header":"user data"}]}
                    }}
                },
                "$defs":{"inner":{"type":"string","x-mcp-header":"Inner"}},
                "default":{"x-mcp-header":"not a schema"}
            }
        }]});
        strip_header_annotations(&mut listing);
        let schema = &listing["tools"][0]["inputSchema"];
        assert_eq!(schema["properties"]["x-mcp-header"]["default"], "retain");
        assert_eq!(
            schema["properties"]["nested"]["properties"]["value"]["examples"][0]["x-mcp-header"],
            "user data"
        );
        assert_eq!(schema["default"]["x-mcp-header"], "not a schema");
        assert!(schema["properties"]["nested"].get("x-mcp-header").is_none());
        assert!(schema["$defs"]["inner"].get("x-mcp-header").is_none());
    }

    #[test]
    fn slow_reader_overflows_by_count_without_blocking_other_requests() {
        let (tx, mut rx) = tokio_mpsc::channel(MAX_QUEUED_NOTIFICATIONS);
        let sender = StreamSender {
            tx,
            used: Arc::new(AtomicUsize::new(0)),
        };
        let (other_tx, mut other_rx) = tokio_mpsc::channel(MAX_QUEUED_NOTIFICATIONS);
        let other = StreamSender {
            tx: other_tx,
            used: Arc::new(AtomicUsize::new(0)),
        };
        for i in 0..MAX_QUEUED_NOTIFICATIONS {
            assert!(sender.send(serde_json::json!({"sequence":i})).is_ok());
        }
        assert!(
            sender
                .send(serde_json::json!({"sequence":"overflow"}))
                .is_err()
        );
        assert!(
            other
                .send(serde_json::json!({"sequence":"unaffected"}))
                .is_ok()
        );
        assert_eq!(other_rx.try_recv().unwrap().value["sequence"], "unaffected");
        while rx.try_recv().is_ok() {}
        assert_eq!(sender.used.load(Ordering::Relaxed), 0);
        assert!(
            sender
                .send(serde_json::json!({"sequence":"recovered"}))
                .is_ok()
        );
    }

    #[test]
    fn large_notification_exceeds_byte_budget_without_reserving_memory() {
        let (tx, _rx) = tokio_mpsc::channel(MAX_QUEUED_NOTIFICATIONS);
        let sender = StreamSender {
            tx,
            used: Arc::new(AtomicUsize::new(0)),
        };
        assert!(
            sender
                .send(serde_json::json!({"data":"x".repeat(MAX_QUEUED_BYTES)}))
                .is_err()
        );
        assert_eq!(sender.used.load(Ordering::Relaxed), 0);
        assert!(sender.send(serde_json::json!({"data":"ok"})).is_ok());
    }

    #[test]
    fn backend_capacity_reopens_when_an_active_request_finishes() {
        let (bridge_tx, bridge_rx) = mpsc::channel(1);
        let mut runner = BridgeRunner {
            bridge_tx,
            bridge_rx,
            protocol: None,
            downstream_mode: DownstreamMcpMode::Unknown,
            listener: None,
            active: HashMap::new(),
        };
        let (tx, _rx) = tokio_mpsc::channel(MAX_QUEUED_NOTIFICATIONS);
        let sender = StreamSender {
            tx,
            used: Arc::new(AtomicUsize::new(0)),
        };
        for index in 0..MAX_ACTIVE_REQUESTS {
            let (terminal_tx, _terminal_rx) = tokio::sync::oneshot::channel();
            let (cancel_tx, _cancel_rx) = tokio::sync::oneshot::channel();
            runner.active.insert(
                index.to_string(),
                ActiveRequest {
                    server_id: String::new(),
                    http_id: Value::Null,
                    method: String::new(),
                    response_tx: sender.clone(),
                    terminal_tx,
                    cancel_tx,
                },
            );
        }
        assert!(!runner.can_admit_request());
        runner.active.remove("0");
        assert!(runner.can_admit_request());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mcp_carrier_preserves_peer_error_and_rejects_ambiguous_outcomes() {
        let error = serde_json::json!({
            "code":-32000,"message":"peer-defined error",
            "data":{"nested":[1,2]},"extension":"preserved"
        });
        let project = |carrier| {
            project_mcp_carrier(
                serde_json::json!("external"),
                "internal",
                "tools/call",
                carrier,
            )
        };
        assert_eq!(project(serde_json::json!({"error":error}))["error"], error);
        assert_eq!(
            project(serde_json::json!({"result":null})),
            serde_json::json!({"jsonrpc":"2.0","id":"external","result":null})
        );
        for invalid in [
            serde_json::json!({"result":null,"error":error}),
            serde_json::json!({"tools":[]}),
            serde_json::json!({"error":null}),
        ] {
            let response = project(invalid);
            assert_eq!(response["id"], "external");
            assert_eq!(response["error"]["code"], -33002);
        }
    }

    #[test]
    fn annotated_tools_are_reexported_without_transport_annotations() {
        let mut result = serde_json::json!({"tools":[
            {"name":"plain","inputSchema":{"type":"object","properties":{}}},
            {"name":"annotated","inputSchema":{"properties":{"nested":{"properties":{
                "region":{"type":"string","x-mcp-header":"Region"}
            }}}}}
        ]});
        strip_header_annotations(&mut result);
        assert_eq!(result["tools"].as_array().unwrap().len(), 2);
        assert_eq!(result["tools"][0]["name"], "plain");
        assert_eq!(result["tools"][1]["name"], "annotated");
        assert_eq!(
            result["tools"][1]["inputSchema"]["properties"]["nested"]["properties"]["region"],
            serde_json::json!({"type":"string"})
        );
    }
}
