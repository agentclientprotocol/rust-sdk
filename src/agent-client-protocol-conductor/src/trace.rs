//! Trace event types for the sequence diagram viewer.
//!
//! Events are serialized as newline-delimited JSON (`.jsons` files).
//! The viewer loads these files to render interactive sequence diagrams.

use std::collections::HashMap;
use std::fs::OpenOptions;
use std::io::{BufWriter, Write};
use std::path::Path;
use std::time::Instant;

use agent_client_protocol_core::schema::{McpOverAcpMessage, SuccessorMessage};
use agent_client_protocol_core::{DynConnectTo, JsonRpcMessage, Role, UntypedMessage, jsonrpcmsg};
use rustc_hash::FxHashMap;
use serde::{Deserialize, Serialize};

use crate::ComponentIndex;
use crate::snoop::SnooperComponent;

/// A trace event representing message flow between components.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
#[non_exhaustive]
pub enum TraceEvent {
    /// A JSON-RPC request from one component to another.
    Request(RequestEvent),

    /// A JSON-RPC response to a prior request.
    Response(ResponseEvent),

    /// A JSON-RPC notification (no response expected).
    Notification(NotificationEvent),
}

/// Protocol type for messages.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum Protocol {
    /// Standard ACP protocol messages.
    Acp,
    /// MCP-over-ACP messages (agent calling proxy's MCP server).
    Mcp,
}

/// A JSON-RPC request from one component to another.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[non_exhaustive]
pub struct RequestEvent {
    /// Monotonic timestamp (seconds since trace start).
    pub ts: f64,

    /// Protocol: ACP or MCP.
    pub protocol: Protocol,

    /// Source component (e.g., "client", "proxy:0", "proxy:1", "agent").
    pub from: String,

    /// Destination component.
    pub to: String,

    /// JSON-RPC request ID (for correlating with response).
    pub id: serde_json::Value,

    /// JSON-RPC method name.
    pub method: String,

    /// ACP session ID, if known (null before session/new completes).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session: Option<String>,

    /// Full request params.
    pub params: serde_json::Value,
}

/// A JSON-RPC response to a prior request.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[non_exhaustive]
pub struct ResponseEvent {
    /// Monotonic timestamp (seconds since trace start).
    pub ts: f64,

    /// Source component (who sent the response).
    pub from: String,

    /// Destination component (who receives the response).
    pub to: String,

    /// JSON-RPC request ID this responds to.
    pub id: serde_json::Value,

    /// True if this is an error response.
    pub is_error: bool,

    /// Response result or error object.
    pub payload: serde_json::Value,
}

/// A JSON-RPC notification (no response expected).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[non_exhaustive]
pub struct NotificationEvent {
    /// Monotonic timestamp (seconds since trace start).
    pub ts: f64,

    /// Protocol: ACP or MCP.
    pub protocol: Protocol,

    /// Source component.
    pub from: String,

    /// Destination component.
    pub to: String,

    /// JSON-RPC method name.
    pub method: String,

    /// ACP session ID, if known.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session: Option<String>,

    /// Full notification params.
    pub params: serde_json::Value,
}

/// Trait for destinations that can receive trace events.
pub trait WriteEvent: Send + 'static {
    /// Write a trace event to the destination.
    fn write_event(&mut self, event: &TraceEvent) -> std::io::Result<()>;
}

/// Writes trace events as newline-delimited JSON to a `Write` impl.
pub(crate) struct EventWriter<W> {
    writer: W,
}

impl<W: Write> EventWriter<W> {
    pub fn new(writer: W) -> Self {
        Self { writer }
    }
}

impl<W: Write + Send + 'static> WriteEvent for EventWriter<W> {
    fn write_event(&mut self, event: &TraceEvent) -> std::io::Result<()> {
        serde_json::to_writer(&mut self.writer, event).map_err(std::io::Error::other)?;
        self.writer.write_all(b"\n")?;
        self.writer.flush()
    }
}

/// Impl for UnboundedSender - sends events to a channel (useful for testing).
impl WriteEvent for futures::channel::mpsc::UnboundedSender<TraceEvent> {
    fn write_event(&mut self, event: &TraceEvent) -> std::io::Result<()> {
        self.unbounded_send(event.clone())
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::BrokenPipe, e))
    }
}

/// Writer for trace events.
pub struct TraceWriter {
    dest: Box<dyn WriteEvent>,
    start_time: Instant,

    /// When we trace a request, we store its id along with the
    /// details here. When we see responses, we try to match them up.
    request_details: FxHashMap<serde_json::Value, RequestDetails>,
}

impl std::fmt::Debug for TraceWriter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TraceWriter")
            .field("start_time", &self.start_time)
            .finish_non_exhaustive()
    }
}

struct RequestDetails {
    #[expect(dead_code)]
    protocol: Protocol,

    #[expect(dead_code)]
    method: String,

    request_from: ComponentIndex,
    request_to: ComponentIndex,
}

impl TraceWriter {
    /// Create a new trace writer from any WriteEvent destination.
    pub fn new<D: WriteEvent>(dest: D) -> Self {
        Self {
            dest: Box::new(dest),
            start_time: Instant::now(),
            request_details: HashMap::default(),
        }
    }

    /// Create a new trace writer that writes to a file path.
    pub fn from_path(path: impl AsRef<Path>) -> std::io::Result<Self> {
        let file = OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(path.as_ref())?;
        Ok(Self::new(EventWriter::new(BufWriter::new(file))))
    }

    /// Get the elapsed time since trace start, in seconds.
    fn elapsed(&self) -> f64 {
        self.start_time.elapsed().as_secs_f64()
    }

    /// Write a trace event.
    fn write_event(&mut self, event: &TraceEvent) {
        // Ignore errors - tracing should not break the conductor
        drop(self.dest.write_event(event));
    }

    /// Write a request event.
    #[expect(clippy::too_many_arguments)]
    fn request(
        &mut self,
        protocol: Protocol,
        from: ComponentIndex,
        to: ComponentIndex,
        id: serde_json::Value,
        method: String,
        session: Option<String>,
        params: serde_json::Value,
    ) {
        self.request_details.insert(
            id.clone(),
            RequestDetails {
                protocol,
                method: method.clone(),
                request_from: from,
                request_to: to,
            },
        );
        self.write_event(&TraceEvent::Request(RequestEvent {
            ts: self.elapsed(),
            protocol,
            from: format!("{from:?}"),
            to: format!("{to:?}"),
            id,
            method,
            session,
            params,
        }));
    }

    /// Write a response event.
    fn response(
        &mut self,
        from: ComponentIndex,
        to: ComponentIndex,
        id: serde_json::Value,
        is_error: bool,
        payload: serde_json::Value,
    ) {
        self.write_event(&TraceEvent::Response(ResponseEvent {
            ts: self.elapsed(),
            from: format!("{from:?}"),
            to: format!("{to:?}"),
            id,
            is_error,
            payload,
        }));
    }

    /// Write a notification event.
    fn notification(
        &mut self,
        protocol: Protocol,
        from: ComponentIndex,
        to: ComponentIndex,
        method: impl Into<String>,
        session: Option<String>,
        params: serde_json::Value,
    ) {
        self.write_event(&TraceEvent::Notification(NotificationEvent {
            ts: self.elapsed(),
            protocol,
            from: format!("{from:?}"),
            to: format!("{to:?}"),
            method: method.into(),
            session,
            params,
        }));
    }

    /// Trace a raw JSON-RPC message being sent from one component to another.
    fn trace_message(&mut self, traced_message: TracedMessage) {
        let TracedMessage {
            component_index,
            successor_index,
            incoming,
            message,
        } = traced_message;

        // We get every message going into or out of a proxy. This includes
        // a fair number of duplicates: for example, if proxy P0 sends to P1,
        // we'll get it as an *outgoing* message from P0 and an *incoming* message to P1.
        // So we want to keep just one copy.
        //
        // We retain:
        //
        // * Incoming requests/notifications targeting a PROXY.
        // * Incoming requests/notifications targeting the AGENT.

        match message {
            jsonrpcmsg::Message::Request(req) => {
                let MessageInfo {
                    successor,
                    id,
                    protocol,
                    method,
                    params,
                } = MessageInfo::from_req(req);

                let (from, to) = match (successor, incoming, component_index, successor_index) {
                    // An incoming request/notification to a proxy from its predecessor.
                    (Successor(false), Incoming(true), ComponentIndex::Proxy(proxy_index), _) => (
                        ComponentIndex::predecessor_of(proxy_index),
                        ComponentIndex::Proxy(proxy_index),
                    ),

                    // An incoming request/notification to any component from its successor.
                    //
                    // This includes incoming messages to the client in the case where we have no proxies.
                    (Successor(true), Incoming(true), component_index, successor_index) => {
                        (successor_index, component_index)
                    }

                    // An outgoing request/notification from a component to its successor
                    // *and* its successor is not a proxy.
                    //
                    // (If its successor is a proxy, we ignore it, because we'll also see the
                    // message in "incoming" form).
                    (Successor(true), Incoming(false), component_index, ComponentIndex::Agent) => {
                        (component_index, ComponentIndex::Agent)
                    }

                    _ => return,
                };

                match id {
                    Some(id) => {
                        self.request(protocol, from, to, id_to_json(&id), method, None, params);
                    }
                    None => {
                        self.notification(protocol, from, to, method, None, params);
                    }
                }
            }
            jsonrpcmsg::Message::Response(resp) => {
                // Lookup the response by its id.
                // All of the messages we are intercepting go to our proxies,
                // and we always assign them globally unique
                if let Some(id) = resp.id {
                    let id = id_to_json(&id);
                    if let Some(RequestDetails {
                        protocol: _,
                        method: _,
                        request_from,
                        request_to,
                    }) = self.request_details.remove(&id)
                    {
                        let (is_error, payload) = match (&resp.result, &resp.error) {
                            (Some(result), _) => (false, result.clone()),
                            (_, Some(error)) => {
                                (true, serde_json::to_value(error).unwrap_or_default())
                            }
                            (None, None) => (false, serde_json::Value::Null),
                        };
                        self.response(request_to, request_from, id, is_error, payload);
                    }
                }
            }
        }
    }

    /// Spawn a trace writer task.
    ///
    /// Returns a `TraceHandle` that can be cloned and used from multiple tasks,
    /// and a future that should be spawned (e.g., via `with_spawned`).
    pub(crate) fn spawn(
        mut self: TraceWriter,
    ) -> (
        TraceHandle,
        impl std::future::Future<Output = Result<(), agent_client_protocol_core::Error>>,
    ) {
        use futures::StreamExt;

        let (tx, mut rx) = futures::channel::mpsc::unbounded();

        let future = async move {
            while let Some(event) = rx.next().await {
                self.trace_message(event);
            }
            Ok(())
        };

        (TraceHandle { tx }, future)
    }
}

/// A cloneable handle for sending trace events to the trace writer task.
///
/// Create with [`spawn_trace_writer`], then clone and pass to bridges.
#[derive(Clone, Debug)]
pub(crate) struct TraceHandle {
    tx: futures::channel::mpsc::UnboundedSender<TracedMessage>,
}

impl TraceHandle {
    /// Trace a raw JSON-RPC message being sent from one component to another.
    fn trace_message(
        &self,
        component_index: ComponentIndex,
        successor_index: ComponentIndex,
        incoming: Incoming,
        message: &jsonrpcmsg::Message,
    ) -> Result<(), agent_client_protocol_core::Error> {
        self.tx
            .unbounded_send(TracedMessage {
                component_index,
                successor_index,
                incoming,
                message: message.clone(),
            })
            .map_err(agent_client_protocol_core::util::internal_error)
    }

    /// Create a tracing bridge that wraps a proxy component.
    ///
    /// Spawns a bridge task that forwards messages between the channel and the component
    /// while tracing them. Returns the wrapped component.
    ///
    /// Tracing strategy:
    /// - **Left→Right (incoming)**: Trace requests/notifications, skip responses
    /// - **Right→Left (outgoing)**: Trace responses, and if `trace_outgoing_requests` is true,
    ///   also trace requests/notifications (needed for edge bridges at conductor boundaries)
    ///
    /// - `cx`: Connection context for spawning the bridge task
    /// - `left_name`: Logical name of the component on the "left" side (e.g., "client", "proxy:0")
    /// - `right_name`: Logical name of the component on the "right" side (e.g., "proxy:0", "agent")
    /// - `component`: The component to wrap
    pub fn bridge_component<R: Role>(
        &self,
        proxy_index: ComponentIndex,
        successor_index: ComponentIndex,
        proxy: impl agent_client_protocol_core::ConnectTo<R>,
    ) -> DynConnectTo<R> {
        DynConnectTo::new(SnooperComponent::new(
            proxy,
            {
                let trace_handle = self.clone();
                move |msg| {
                    trace_handle.trace_message(proxy_index, successor_index, Incoming(true), msg)
                }
            },
            {
                let trace_handle = self.clone();
                move |msg| {
                    trace_handle.trace_message(proxy_index, successor_index, Incoming(false), msg)
                }
            },
        ))
    }
}

/// Convert a jsonrpcmsg::Id to serde_json::Value.
fn id_to_json(id: &jsonrpcmsg::Id) -> serde_json::Value {
    match id {
        jsonrpcmsg::Id::String(s) => serde_json::Value::String(s.clone()),
        jsonrpcmsg::Id::Number(n) => serde_json::Value::Number((*n).into()),
        jsonrpcmsg::Id::Null => serde_json::Value::Null,
    }
}

/// A message observed going over a channel connected to `left` and `right`.
/// This could be a successor message, a mcp-over-acp message, etc.
#[derive(Debug)]
struct TracedMessage {
    component_index: ComponentIndex,
    successor_index: ComponentIndex,
    incoming: Incoming,
    message: jsonrpcmsg::Message,
}

/// Fully interpreted message info.
#[derive(Debug)]
struct MessageInfo {
    successor: Successor,
    id: Option<jsonrpcmsg::Id>,
    protocol: Protocol,
    method: String,
    params: serde_json::Value,
}

#[derive(Copy, Clone, Debug)]
struct Successor(bool);

#[derive(Copy, Clone, Debug)]
struct Incoming(bool);

impl MessageInfo {
    /// Extract logical message info from method and params.
    ///
    /// This unwraps protocol wrappers to get the "real" message:
    /// - `_proxy/successor` messages are unwrapped to get the inner message
    /// - `_proxy/initialize` messages are unwrapped to get `initialize`
    /// - `_mcp/message` messages are detected and marked as MCP protocol
    ///
    /// Returns (protocol, method, params).
    fn from_req(req: jsonrpcmsg::Request) -> Self {
        let untyped = UntypedMessage::parse_message(&req.method, &req.params)
            .expect("untyped message is infallible");
        Self::from_untyped(Successor(false), req.id, Protocol::Acp, untyped)
    }

    fn from_untyped(
        successor: Successor,
        id: Option<jsonrpcmsg::Id>,
        protocol: Protocol,
        untyped: UntypedMessage,
    ) -> Self {
        if let Ok(m) = SuccessorMessage::parse_message(&untyped.method, &untyped.params) {
            return Self::from_untyped(Successor(true), id, protocol, m.message);
        }

        if let Ok(m) = McpOverAcpMessage::parse_message(&untyped.method, &untyped.params) {
            return Self::from_untyped(successor, id, Protocol::Mcp, m.message);
        }

        Self {
            successor,
            id,
            protocol,
            method: untyped.method,
            params: untyped.params,
        }
    }
}
