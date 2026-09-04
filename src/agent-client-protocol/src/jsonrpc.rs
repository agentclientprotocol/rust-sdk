//! Core JSON-RPC server support.

use agent_client_protocol_schema::v1::{
    JsonRpcMessage as VersionedJsonRpcMessage, Notification as RpcNotification,
    Request as RpcRequest, RequestId, Response as RpcResponse, SessionId,
};

// Types re-exported from crate root
use serde::ser::SerializeSeq as _;
use serde::{Deserialize, Serialize};
use std::any::TypeId;
use std::collections::HashMap;
use std::fmt::Debug;
use std::marker::PhantomData;
use std::panic::Location;
use std::pin::pin;
use std::sync::{
    Arc, Mutex, Weak,
    atomic::{AtomicBool, Ordering},
};
use uuid::Uuid;

use futures::FutureExt;
use futures::channel::{mpsc, oneshot};
use futures::future::{self, BoxFuture, Either};
use futures::{AsyncRead, AsyncWrite, StreamExt};

pub(crate) mod close;
mod dynamic_handler;
pub(crate) mod handlers;
mod incoming_actor;
mod outgoing_actor;
mod protocol_compat;
pub(crate) mod run;
mod task_actor;
mod transport_actor;

use crate::jsonrpc::close::{ChainedClose, CloseCallback};
pub use crate::jsonrpc::close::{HandleConnectionClose, NullClose};
use crate::jsonrpc::dynamic_handler::DynamicHandlerMessage;
pub use crate::jsonrpc::handlers::NullHandler;
use crate::jsonrpc::handlers::{ChainedHandler, NamedHandler};
use crate::jsonrpc::handlers::{MessageHandler, NotificationHandler, RequestHandler};
use crate::jsonrpc::outgoing_actor::{OutgoingMessageTx, send_raw_message};
use crate::jsonrpc::protocol_compat::{ProtocolCompat, ProtocolMode};
use crate::jsonrpc::run::SpawnedRun;
use crate::jsonrpc::run::{ChainRun, NullRun, RunWithConnectionTo};
use crate::jsonrpc::task_actor::{Task, TaskTx};
#[cfg(feature = "unstable_mcp_over_acp")]
use crate::mcp_server::McpServer;
use crate::role::HasPeer;
use crate::role::Role;
use crate::{Agent, Client, ConnectTo, Proxy, RoleId};

/// One valid JSON-RPC message carried inside a [`TransportFrame`].
///
/// This uses the JSON-RPC envelope types from `agent-client-protocol-schema`
/// while keeping method params as raw, JSON-RPC-valid params at the transport boundary.
#[derive(Debug, Clone)]
pub enum RawJsonRpcMessage {
    /// A JSON-RPC request with an id and expected response.
    Request(RpcRequest<RawJsonRpcParams>),
    /// A JSON-RPC notification without a response.
    Notification(RpcNotification<RawJsonRpcParams>),
    /// A JSON-RPC response to a prior request.
    Response(RpcResponse<serde_json::Value>),
}

/// A JSON-RPC frame exchanged between protocol components and transports.
///
/// A frame preserves the boundary between a single JSON-RPC value and a batch.
/// Malformed wire input is represented explicitly; transport failures are
/// reported by the future that drives the transport rather than sent through a
/// [`Channel`].
#[derive(Clone, Debug)]
pub enum TransportFrame {
    /// One valid JSON-RPC message.
    Single(RawJsonRpcMessage),
    /// One malformed or invalid wire value retained for relays.
    Malformed {
        /// The original wire representation.
        raw: String,
        /// The JSON-RPC error associated with the malformed value.
        error: crate::Error,
    },
    /// Entries retained from one non-empty JSON-RPC batch, kept in source order.
    Batch(TransportBatch),
}

/// A structurally non-empty JSON-RPC batch retained across framed relays.
#[derive(Clone, Debug)]
pub struct TransportBatch {
    first: TransportBatchEntry,
    rest: Vec<TransportBatchEntry>,
}

/// One entry in a [`TransportBatch`].
#[derive(Clone, Debug)]
pub enum TransportBatchEntry {
    /// A valid JSON-RPC message.
    Message(RawJsonRpcMessage),
    /// A malformed or invalid JSON-RPC value retained for relays.
    Malformed {
        /// The original JSON value.
        raw: serde_json::Value,
        /// The JSON-RPC error associated with the malformed value.
        error: crate::Error,
    },
}

pub(crate) fn is_response_only_shape(value: &serde_json::Value) -> bool {
    value.as_object().is_some_and(|object| {
        !object.contains_key("method")
            && (object.contains_key("result") || object.contains_key("error"))
    })
}

pub(crate) fn raw_is_response_only_shape(raw: &str) -> bool {
    serde_json::from_str(raw).is_ok_and(|value| is_response_only_shape(&value))
}

impl TransportBatchEntry {
    /// Create a valid batch entry.
    #[must_use]
    pub fn message(message: RawJsonRpcMessage) -> Self {
        Self::Message(message)
    }

    /// Create a malformed batch entry.
    #[must_use]
    pub fn malformed(raw: serde_json::Value, error: crate::Error) -> Self {
        Self::Malformed { raw, error }
    }

    #[cfg(test)]
    fn as_result(&self) -> Result<&RawJsonRpcMessage, &crate::Error> {
        match self {
            Self::Message(message) => Ok(message),
            Self::Malformed { error, .. } => Err(error),
        }
    }

    fn message_ref(&self) -> Option<&RawJsonRpcMessage> {
        match self {
            Self::Message(message) => Some(message),
            Self::Malformed { .. } => None,
        }
    }
}

impl Serialize for TransportBatchEntry {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        match self {
            Self::Message(message) => message.serialize(serializer),
            Self::Malformed { raw, .. } => raw.serialize(serializer),
        }
    }
}

impl TransportBatch {
    /// Create a non-empty batch from entries.
    ///
    /// Returns `None` when the iterator is empty.
    pub fn from_entries(entries: impl IntoIterator<Item = TransportBatchEntry>) -> Option<Self> {
        let mut entries = entries.into_iter();
        Some(Self {
            first: entries.next()?,
            rest: entries.collect(),
        })
    }

    /// Create a non-empty batch from valid messages.
    ///
    /// Returns `None` when the iterator is empty.
    pub fn from_messages(messages: impl IntoIterator<Item = RawJsonRpcMessage>) -> Option<Self> {
        Self::from_entries(messages.into_iter().map(TransportBatchEntry::message))
    }

    /// Iterate over entries in source order.
    pub fn entries(&self) -> impl Iterator<Item = &TransportBatchEntry> {
        std::iter::once(&self.first).chain(&self.rest)
    }

    /// Iterate mutably over entries in source order.
    pub fn entries_mut(&mut self) -> impl Iterator<Item = &mut TransportBatchEntry> {
        std::iter::once(&mut self.first).chain(&mut self.rest)
    }

    /// Consume this batch and iterate over its entries in source order.
    pub fn into_entries(self) -> impl Iterator<Item = TransportBatchEntry> {
        std::iter::once(self.first).chain(self.rest)
    }

    /// Return the number of entries in this non-empty batch.
    #[must_use]
    pub fn len(&self) -> usize {
        1 + self.rest.len()
    }

    /// Return whether this batch is empty.
    ///
    /// A `TransportBatch` is structurally non-empty, so this always returns
    /// `false`.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        false
    }

    #[cfg(test)]
    pub(crate) fn iter_results(
        &self,
    ) -> impl Iterator<Item = Result<&RawJsonRpcMessage, &crate::Error>> {
        self.entries().map(TransportBatchEntry::as_result)
    }

    fn messages(&self) -> impl Iterator<Item = &RawJsonRpcMessage> {
        self.entries().filter_map(TransportBatchEntry::message_ref)
    }
}

impl Serialize for TransportBatch {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        let mut sequence = serializer.serialize_seq(Some(1 + self.rest.len()))?;
        sequence.serialize_element(&self.first)?;
        for entry in &self.rest {
            sequence.serialize_element(entry)?;
        }
        sequence.end()
    }
}

impl TransportFrame {
    fn inspect_messages(
        &self,
        observer: &mut impl FnMut(&RawJsonRpcMessage) -> Result<(), crate::Error>,
    ) -> Result<(), crate::Error> {
        match self {
            Self::Single(message) => observer(message),
            Self::Malformed { .. } => Ok(()),
            Self::Batch(batch) => {
                for message in batch.messages() {
                    observer(message)?;
                }
                Ok(())
            }
        }
    }
}

/// Raw JSON-RPC request or notification parameters.
///
/// JSON-RPC params, when present, must be either an array or an object.
#[derive(Debug, Clone, PartialEq)]
pub enum RawJsonRpcParams {
    /// Positional JSON-RPC params.
    Array(Vec<serde_json::Value>),
    /// Named JSON-RPC params.
    Object(serde_json::Map<String, serde_json::Value>),
}

impl RawJsonRpcParams {
    /// Convert a JSON value into JSON-RPC params.
    pub fn from_value(value: serde_json::Value) -> Result<Option<Self>, crate::Error> {
        match value {
            serde_json::Value::Null => Ok(None),
            serde_json::Value::Array(array) => Ok(Some(Self::Array(array))),
            serde_json::Value::Object(object) => Ok(Some(Self::Object(object))),
            _ => {
                Err(crate::Error::invalid_params()
                    .data("JSON-RPC params must be an object or array"))
            }
        }
    }

    /// Convert params back into a JSON value.
    #[must_use]
    pub fn into_value(self) -> serde_json::Value {
        match self {
            Self::Array(array) => serde_json::Value::Array(array),
            Self::Object(object) => serde_json::Value::Object(object),
        }
    }
}

impl Serialize for RawJsonRpcParams {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        match self {
            Self::Array(array) => array.serialize(serializer),
            Self::Object(object) => object.serialize(serializer),
        }
    }
}

impl<'de> Deserialize<'de> for RawJsonRpcParams {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let value = serde_json::Value::deserialize(deserializer)?;
        match value {
            serde_json::Value::Array(array) => Ok(Self::Array(array)),
            serde_json::Value::Object(object) => Ok(Self::Object(object)),
            _ => Err(serde::de::Error::custom(
                "JSON-RPC params must be an object or array",
            )),
        }
    }
}

impl RawJsonRpcMessage {
    /// Build a raw JSON-RPC request message.
    pub fn request(
        method: String,
        params: serde_json::Value,
        id: RequestId,
    ) -> Result<Self, crate::Error> {
        Ok(Self::Request(RpcRequest {
            id,
            method: Arc::from(method),
            params: RawJsonRpcParams::from_value(params)?,
        }))
    }

    /// Build a raw JSON-RPC notification message.
    pub fn notification(method: String, params: serde_json::Value) -> Result<Self, crate::Error> {
        Ok(Self::Notification(RpcNotification {
            method: Arc::from(method),
            params: RawJsonRpcParams::from_value(params)?,
        }))
    }

    /// Build a raw JSON-RPC response message.
    #[must_use]
    pub fn response(id: RequestId, response: Result<serde_json::Value, crate::Error>) -> Self {
        Self::Response(RpcResponse::new(id, response))
    }

    /// The response id, if this is a response.
    #[must_use]
    pub fn response_id(&self) -> Option<&RequestId> {
        match self {
            Self::Response(RpcResponse::Result { id, .. } | RpcResponse::Error { id, .. }) => {
                Some(id)
            }
            Self::Request(_) | Self::Notification(_) => None,
        }
    }
}

impl Serialize for RawJsonRpcMessage {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        match self {
            Self::Request(request) => {
                VersionedJsonRpcMessage::wrap(request.clone()).serialize(serializer)
            }
            Self::Notification(notification) => {
                VersionedJsonRpcMessage::wrap(notification.clone()).serialize(serializer)
            }
            Self::Response(response) => {
                VersionedJsonRpcMessage::wrap(response.clone()).serialize(serializer)
            }
        }
    }
}

impl<'de> Deserialize<'de> for RawJsonRpcMessage {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let value = serde_json::Value::deserialize(deserializer)?;
        let Some(object) = value.as_object() else {
            return Err(serde::de::Error::custom("invalid JSON-RPC message"));
        };

        let has_method = object.contains_key("method");
        let has_id = object.contains_key("id");
        let has_result = object.contains_key("result");
        let has_error = object.contains_key("error");

        if has_method && !has_result && !has_error {
            if has_id {
                let request = serde_json::from_value::<
                    VersionedJsonRpcMessage<RpcRequest<RawJsonRpcParams>>,
                >(value)
                .map_err(serde::de::Error::custom)?
                .into_inner();
                Ok(Self::Request(request))
            } else {
                let notification = serde_json::from_value::<
                    VersionedJsonRpcMessage<RpcNotification<RawJsonRpcParams>>,
                >(value)
                .map_err(serde::de::Error::custom)?
                .into_inner();
                Ok(Self::Notification(notification))
            }
        } else if !has_method && has_id && has_result != has_error {
            let response = serde_json::from_value::<
                VersionedJsonRpcMessage<RpcResponse<serde_json::Value>>,
            >(value)
            .map_err(serde::de::Error::custom)?
            .into_inner();
            Ok(Self::Response(response))
        } else {
            Err(serde::de::Error::custom("invalid JSON-RPC message"))
        }
    }
}

fn params_from_transport(params: Option<RawJsonRpcParams>) -> serde_json::Value {
    params.map_or(serde_json::Value::Null, RawJsonRpcParams::into_value)
}

/// Handlers process incoming JSON-RPC messages on a connection.
///
/// When messages arrive, they flow through a chain of handlers. Each handler can
/// either **claim** the message (handle it) or **decline** it (pass to the next handler).
///
/// # Message Flow
///
/// Messages flow through three layers of handlers in order:
///
/// ```text
/// ┌─────────────────────────────────────────────────────────────────┐
/// │                     Incoming Message                            │
/// └─────────────────────────────────────────────────────────────────┘
///                              │
///                              ▼
/// ┌─────────────────────────────────────────────────────────────────┐
/// │  1. User Handlers (registered via on_receive_request, etc.)     │
/// │     - Tried in registration order                               │
/// │     - First handler to return Handled::Yes claims the message   │
/// └─────────────────────────────────────────────────────────────────┘
///                              │ Handled::No
///                              ▼
/// ┌─────────────────────────────────────────────────────────────────┐
/// │  2. Dynamic Handlers (added at runtime)                         │
/// │     - Used for session-specific message handling                │
/// │     - Added via ConnectionTo::add_dynamic_handler             │
/// └─────────────────────────────────────────────────────────────────┘
///                              │ Handled::No
///                              ▼
/// ┌─────────────────────────────────────────────────────────────────┐
/// │  3. Role Default Handler                                        │
/// │     - Fallback based on the connection's Role                   │
/// │     - Handles protocol-level messages (e.g., proxy forwarding)  │
/// └─────────────────────────────────────────────────────────────────┘
///                              │ Handled::No
///                              ▼
/// ┌─────────────────────────────────────────────────────────────────┐
/// │  Unhandled: requests error, notifications ignored               │
/// └─────────────────────────────────────────────────────────────────┘
/// ```
///
/// # The `Handled` Return Value
///
/// Each handler returns [`Handled`] to indicate whether it processed the message:
///
/// - **`Handled::Yes`** - Message was handled. No further handlers are invoked.
/// - **`Handled::No { message, retry }`** - Message was not handled. The message
///   (possibly modified) is passed to the next handler in the chain.
///
/// For convenience, handlers can return `()` which is equivalent to `Handled::Yes`.
///
/// # The Retry Mechanism
///
/// The `retry` flag in `Handled::No` controls what happens when no handler claims a message:
///
/// - **`retry: false`** (default) - Send a "method not found" error
///   response immediately for requests, or ignore notifications.
/// - **`retry: true`** - Queue the message and retry it when new dynamic handlers are added.
///
/// This mechanism exists because of a timing issue with sessions: when a `session/new`
/// response is being processed, the dynamic handler for that session hasn't been registered
/// yet, but `session/update` notifications for that session may already be arriving.
/// By setting `retry: true`, these early notifications are queued until the session's
/// dynamic handler is added.
///
/// # Handler Registration
///
/// Most users register handlers using the builder methods on [`Builder`]:
///
/// ```
/// # use agent_client_protocol::{Agent, Client, ConnectTo};
/// # use agent_client_protocol::schema::v1::{AgentCapabilities, InitializeRequest, InitializeResponse};
/// # use agent_client_protocol_test::StatusUpdate;
/// # async fn example(transport: impl ConnectTo<Agent>) -> Result<(), agent_client_protocol::Error> {
/// Agent.builder()
///     .on_receive_request(async |req: InitializeRequest, responder, cx| {
///         responder.respond(
///             InitializeResponse::new(req.protocol_version)
///                 .agent_capabilities(AgentCapabilities::new()),
///         )
///     }, agent_client_protocol::on_receive_request!())
///     .on_receive_notification(async |notif: StatusUpdate, cx| {
///         // Process notification
///         Ok(())
///     }, agent_client_protocol::on_receive_notification!())
///     .connect_to(transport)
///     .await?;
/// # Ok(())
/// # }
/// ```
///
/// The type parameter on the closure determines which messages are dispatched to it.
/// Messages that don't match the type are automatically passed to the next handler.
///
/// # Implementing Custom Handlers
///
/// For advanced use cases, you can implement [`HandleDispatchFrom`] directly:
///
/// ```no_run
/// use agent_client_protocol::{
///     Client, ConnectionTo, Dispatch, Error, HandleDispatchFrom, Handled,
/// };
///
/// struct MyHandler;
///
/// impl HandleDispatchFrom<Client> for MyHandler {
///     async fn handle_dispatch_from(
///         &mut self,
///         message: Dispatch,
///         _connection: ConnectionTo<Client>,
///     ) -> Result<Handled<Dispatch>, Error> {
///         if message.method() == "my/custom/method" {
///             // Handle it
///             Ok(Handled::Yes)
///         } else {
///             // Pass to next handler
///             Ok(Handled::No { message, retry: false })
///         }
///     }
///
///     fn describe_chain(&self) -> impl std::fmt::Debug {
///         "MyHandler"
///     }
/// }
/// ```
///
/// # Important: Handlers Must Not Block
///
/// The connection processes messages on a single async task. While a handler is running,
/// no other messages can be processed. For expensive operations, use [`ConnectionTo::spawn`]
/// to run work concurrently:
///
/// ```
/// # use agent_client_protocol::{Client, Agent, ConnectTo};
/// # use agent_client_protocol_test::{expensive_operation, ProcessComplete};
/// # async fn example(transport: impl ConnectTo<Client>) -> Result<(), agent_client_protocol::Error> {
/// # Client.builder().connect_with(transport, async |cx| {
/// cx.spawn({
///     let connection = cx.clone();
///     async move {
///         let result = expensive_operation("data").await?;
///         connection.send_notification(ProcessComplete { result })?;
///         Ok(())
///     }
/// })?;
/// # Ok(())
/// # }).await?;
/// # Ok(())
/// # }
/// ```
#[allow(async_fn_in_trait)]
/// A handler for incoming JSON-RPC messages.
///
/// This trait is implemented by types that can process incoming messages on a connection.
/// Handlers are registered with a [`Builder`] and are called in order until
/// one claims the message.
///
/// The type parameter is the counterpart role that messages arrive from and
/// that the supplied [`ConnectionTo`] addresses. An agent handler therefore
/// implements `HandleDispatchFrom<Client>`, while a client handler implements
/// `HandleDispatchFrom<Agent>`.
pub trait HandleDispatchFrom<Counterpart: Role>: Send {
    /// Attempt to claim an incoming dispatch (request, notification, or response).
    ///
    /// # Important: do not block
    ///
    /// The server will not process new messages until this handler returns.
    /// You should avoid blocking in this callback unless you wish to block the server (e.g., for rate limiting).
    /// The recommended approach to manage expensive operations is to the [`ConnectionTo::spawn`] method available on the message context.
    ///
    /// # Parameters
    ///
    /// * `message` - The incoming message to handle.
    /// * `connection` - The connection, used to send messages and access connection state.
    ///
    /// # Returns
    ///
    /// * `Ok(Handled::Yes)` if the message was claimed. It will not be propagated further.
    /// * `Ok(Handled::No(message))` if not; the (possibly changed) message will be passed to the remaining handlers.
    /// * `Err` if processing fails. Requests receive an Error Response, response
    ///   errors are routed to the local request awaiter, and notification errors
    ///   are logged without a wire reply.
    fn handle_dispatch_from(
        &mut self,
        message: Dispatch,
        connection: ConnectionTo<Counterpart>,
    ) -> impl Future<Output = Result<Handled<Dispatch>, crate::Error>> + Send;

    /// Returns a debug description of the registered handlers for diagnostics.
    fn describe_chain(&self) -> impl std::fmt::Debug;
}

impl<Counterpart: Role, H> HandleDispatchFrom<Counterpart> for &mut H
where
    H: HandleDispatchFrom<Counterpart>,
{
    fn handle_dispatch_from(
        &mut self,
        message: Dispatch,
        cx: ConnectionTo<Counterpart>,
    ) -> impl Future<Output = Result<Handled<Dispatch>, crate::Error>> + Send {
        H::handle_dispatch_from(self, message, cx)
    }

    fn describe_chain(&self) -> impl std::fmt::Debug {
        H::describe_chain(self)
    }
}

/// Selects the connection context exposed by a [`Builder`]'s callbacks.
///
/// This trait is an implementation detail of the typed builder aliases. It is
/// public so the callback connection type remains expressible in public API
/// signatures.
#[doc(hidden)]
#[allow(private_bounds)]
pub trait ConnectionContext: connection_context::Sealed + Send + Sync + 'static {
    /// The connection type exposed to callbacks for `Counterpart`.
    type Connection<Counterpart: Role>: Clone + Send + Sync + 'static;
}

mod connection_context {
    use super::{ConnectionContext, ConnectionTo, Role};

    pub trait Sealed {
        fn from_raw<Counterpart: Role>(
            connection: ConnectionTo<Counterpart>,
        ) -> <Self as ConnectionContext>::Connection<Counterpart>
        where
            Self: ConnectionContext;
    }

    pub(crate) fn from_raw<Context: ConnectionContext, Counterpart: Role>(
        connection: ConnectionTo<Counterpart>,
    ) -> Context::Connection<Counterpart> {
        <Context as Sealed>::from_raw(connection)
    }
}

/// The default callback context used by stable and low-level builders.
#[doc(hidden)]
#[derive(Copy, Clone, Debug, Default)]
pub struct RawConnectionContext;

impl connection_context::Sealed for RawConnectionContext {
    fn from_raw<Counterpart: Role>(
        connection: ConnectionTo<Counterpart>,
    ) -> <Self as ConnectionContext>::Connection<Counterpart> {
        connection
    }
}

impl ConnectionContext for RawConnectionContext {
    type Connection<Counterpart: Role> = ConnectionTo<Counterpart>;
}

/// The callback context used by ACP protocol v2 builders.
#[cfg(feature = "unstable_protocol_v2")]
#[doc(hidden)]
#[derive(Copy, Clone, Debug, Default)]
pub struct V2ConnectionContext;

#[cfg(feature = "unstable_protocol_v2")]
impl connection_context::Sealed for V2ConnectionContext {
    fn from_raw<Counterpart: Role>(
        connection: ConnectionTo<Counterpart>,
    ) -> <Self as ConnectionContext>::Connection<Counterpart> {
        V2ConnectionTo { inner: connection }
    }
}

#[cfg(feature = "unstable_protocol_v2")]
impl ConnectionContext for V2ConnectionContext {
    type Connection<Counterpart: Role> = V2ConnectionTo<Counterpart>;
}

/// A JSON-RPC connection builder whose callbacks receive [`V2ConnectionTo`].
#[cfg(feature = "unstable_protocol_v2")]
pub type V2Builder<Host, Handler = NullHandler, Runner = NullRun, Close = NullClose> =
    Builder<Host, Handler, Runner, Close, V2ConnectionContext>;

/// A JSON-RPC connection that can act as either a server, client, or both.
///
/// [`Builder`] provides a builder-style API for creating JSON-RPC servers and clients.
/// You start by calling `Role.builder()` (e.g., `Client.builder()`), then add message
/// handlers, and finally drive the connection with either [`connect_to`](Builder::connect_to)
/// or [`connect_with`](Builder::connect_with), providing a component implementation
/// (e.g., [`ByteStreams`] for byte streams).
///
/// # JSON-RPC Primer
///
/// JSON-RPC 2.0 has two fundamental message types:
///
/// * **Requests** - Messages that expect a response. They have an `id` field that gets
///   echoed back in the response so the sender can correlate them.
/// * **Notifications** - Fire-and-forget messages with no `id` field. The sender doesn't
///   expect or receive a response.
///
/// # Type-Driven Message Dispatch
///
/// The handler registration methods use Rust's type system to determine which messages
/// to handle. The type parameter you provide controls what gets dispatched to your handler:
///
/// ## Single Message Types
///
/// The simplest case - handle one specific message type:
///
/// ```no_run
/// # use agent_client_protocol_test::*;
/// # use agent_client_protocol::schema::v1::{InitializeRequest, InitializeResponse, SessionNotification};
/// # async fn example() -> Result<(), agent_client_protocol::Error> {
/// # let connection = mock_connection();
/// connection
///     .on_receive_request(async |req: InitializeRequest, responder, cx| {
///         // Handle only InitializeRequest messages
///         responder.respond(InitializeResponse::make())
///     }, agent_client_protocol::on_receive_request!())
///     .on_receive_notification(async |notif: SessionNotification, cx| {
///         // Handle only SessionUpdate notifications
///         Ok(())
///     }, agent_client_protocol::on_receive_notification!())
/// # .connect_to(agent_client_protocol_test::MockTransport).await?;
/// # Ok(())
/// # }
/// ```
///
/// ## Enum Message Types
///
/// You can also handle multiple related messages with a single handler by defining an enum
/// that implements the appropriate trait ([`JsonRpcRequest`] or [`JsonRpcNotification`]):
///
/// ```no_run
/// # use agent_client_protocol_test::*;
/// # use agent_client_protocol::{JsonRpcRequest, JsonRpcMessage, UntypedMessage};
/// # use agent_client_protocol::schema::v1::{InitializeRequest, InitializeResponse, PromptRequest, PromptResponse};
/// # async fn example() -> Result<(), agent_client_protocol::Error> {
/// # let connection = mock_connection();
/// // Define an enum for multiple request types
/// #[derive(Debug, Clone)]
/// enum MyRequests {
///     Initialize(InitializeRequest),
///     Prompt(PromptRequest),
/// }
///
/// // Implement JsonRpcRequest for your enum
/// # impl JsonRpcMessage for MyRequests {
/// #     fn matches_method(_method: &str) -> bool { false }
/// #     fn method(&self) -> &str { "myRequests" }
/// #     fn to_untyped_message(&self) -> Result<UntypedMessage, agent_client_protocol::Error> { todo!() }
/// #     fn parse_message(_method: &str, _params: &impl serde::Serialize) -> Result<Self, agent_client_protocol::Error> { Err(agent_client_protocol::Error::method_not_found()) }
/// # }
/// impl JsonRpcRequest for MyRequests { type Response = serde_json::Value; }
///
/// // Handle all variants in one place
/// connection.on_receive_request(async |req: MyRequests, responder, cx| {
///     match req {
///         MyRequests::Initialize(init) => { responder.respond(serde_json::json!({})) }
///         MyRequests::Prompt(prompt) => { responder.respond(serde_json::json!({})) }
///     }
/// }, agent_client_protocol::on_receive_request!())
/// # .connect_to(agent_client_protocol_test::MockTransport).await?;
/// # Ok(())
/// # }
/// ```
///
/// ## Mixed Message Types
///
/// To handle requests, notifications, and responses in one callback, use
/// [`on_receive_dispatch`](Self::on_receive_dispatch):
///
/// ```no_run
/// # use agent_client_protocol_test::*;
/// # use agent_client_protocol::Dispatch;
/// # use agent_client_protocol::schema::v1::{InitializeRequest, InitializeResponse, SessionNotification};
/// # async fn example() -> Result<(), agent_client_protocol::Error> {
/// # let connection = mock_connection();
/// // on_receive_dispatch receives requests, notifications, and responses
/// connection.on_receive_dispatch(async |msg: Dispatch<InitializeRequest, SessionNotification>, _cx| {
///     match msg {
///         Dispatch::Request(req, responder) => {
///             responder.respond(InitializeResponse::make())
///         }
///         Dispatch::Notification(notif) => {
///             Ok(())
///         }
///         Dispatch::Response(result, router) => {
///             // Forward response to its destination
///             router.route_with_result(result)
///         }
///     }
/// }, agent_client_protocol::on_receive_dispatch!())
/// # .connect_to(agent_client_protocol_test::MockTransport).await?;
/// # Ok(())
/// # }
/// ```
///
/// # Handler Registration
///
/// Register handlers using these methods (listed from most common to most flexible):
///
/// * [`on_receive_request`](Self::on_receive_request) - Handle JSON-RPC requests (messages expecting responses)
/// * [`on_receive_notification`](Self::on_receive_notification) - Handle JSON-RPC notifications (fire-and-forget)
/// * [`on_receive_dispatch`](Self::on_receive_dispatch) - Handle requests, notifications, and responses in one callback
/// * [`with_handler`](Self::with_handler) - Low-level primitive for maximum flexibility
///
/// ## Handler Ordering
///
/// Handlers are tried in the order you register them. The first handler that claims a message
/// (by matching its type) will process it. Subsequent handlers won't see that message:
///
/// ```no_run
/// # use agent_client_protocol_test::*;
/// # use agent_client_protocol::schema::v1::{InitializeRequest, InitializeResponse, PromptRequest, PromptResponse};
/// # async fn example() -> Result<(), agent_client_protocol::Error> {
/// # let connection = mock_connection();
/// connection
///     .on_receive_request(async |req: InitializeRequest, responder, cx| {
///         // This runs first for InitializeRequest
///         responder.respond(InitializeResponse::make())
///     }, agent_client_protocol::on_receive_request!())
///     .on_receive_request(async |req: PromptRequest, responder, cx| {
///         // This runs first for PromptRequest
///         responder.respond(PromptResponse::make())
///     }, agent_client_protocol::on_receive_request!())
///     // Unknown requests receive Method not found automatically; unhandled
///     // notifications are ignored.
/// # .connect_to(agent_client_protocol_test::MockTransport).await?;
/// # Ok(())
/// # }
/// ```
///
/// # Event Loop and Concurrency
///
/// Understanding the event loop is critical for writing correct handlers.
///
/// ## The Event Loop
///
/// [`Builder`] runs all handler callbacks on a single async task - the event loop.
/// While a handler is running, **the server cannot receive new messages**. This means
/// any blocking or expensive work in your handlers will stall the entire connection.
///
/// To avoid blocking the event loop, use [`ConnectionTo::spawn`] to offload serious
/// work to concurrent tasks:
///
/// ```no_run
/// # use agent_client_protocol_test::*;
/// # async fn example() -> Result<(), agent_client_protocol::Error> {
/// # let connection = mock_connection();
/// connection.on_receive_request(async |req: AnalyzeRequest, responder, cx| {
///     // Clone cx for the spawned task
///     cx.spawn({
///         let connection = cx.clone();
///         async move {
///             let result = expensive_analysis(&req.data).await?;
///             connection.send_notification(AnalysisComplete { result })?;
///             Ok(())
///         }
///     })?;
///
///     // Respond immediately without blocking
///     responder.respond(AnalysisStarted { job_id: 42 })
/// }, agent_client_protocol::on_receive_request!())
/// # .connect_to(agent_client_protocol_test::MockTransport).await?;
/// # Ok(())
/// # }
/// ```
///
/// Note that the entire connection runs within one async task, so parallelism must be
/// managed explicitly using [`spawn`](ConnectionTo::spawn).
///
/// ## The Connection Context
///
/// Handler callbacks receive a context object (`cx`) for interacting with the connection:
///
/// * **For request handlers** - [`Responder<R>`] provides [`respond`](Responder::respond)
///   to send the response, plus methods to send other messages
/// * **For notification handlers** - [`ConnectionTo`] provides methods to send messages
///   and spawn tasks
///
/// Both context types support:
/// * [`send_request`](ConnectionTo::send_request) - Send requests to the other side
/// * [`send_notification`](ConnectionTo::send_notification) - Send notifications
/// * [`spawn`](ConnectionTo::spawn) - Run tasks concurrently without blocking the event loop
///
/// The [`SentRequest`] returned by `send_request` provides methods like
/// [`on_receiving_result`](SentRequest::on_receiving_result) that help you
/// avoid accidentally blocking the event loop while waiting for responses.
///
/// # Driving the Connection
///
/// After adding handlers, you must drive the connection using one of two modes:
///
/// ## Server Mode: `connect_to()`
///
/// Use [`connect_to`](Self::connect_to) when you only need to respond to incoming messages:
///
/// ```no_run
/// # use agent_client_protocol_test::*;
/// # async fn example() -> Result<(), agent_client_protocol::Error> {
/// # let connection = mock_connection();
/// connection
///     .on_receive_request(async |req: MyRequest, responder, cx| {
///         responder.respond(MyResponse { status: "ok".into() })
///     }, agent_client_protocol::on_receive_request!())
///     .connect_to(MockTransport)  // Runs until connection closes or error occurs
///     .await?;
/// # Ok(())
/// # }
/// ```
///
/// The connection will process incoming messages and invoke your handlers until the
/// connection is closed or an error occurs.
///
/// ## Client Mode: `connect_with()`
///
/// Use [`connect_with`](Self::connect_with) when you need to both handle incoming messages
/// AND send your own requests/notifications:
///
/// ```no_run
/// # use agent_client_protocol_test::*;
/// # use agent_client_protocol::schema::v1::InitializeRequest;
/// # async fn example() -> Result<(), agent_client_protocol::Error> {
/// # let connection = mock_connection();
/// connection
///     .on_receive_request(async |req: MyRequest, responder, cx| {
///         responder.respond(MyResponse { status: "ok".into() })
///     }, agent_client_protocol::on_receive_request!())
///     .connect_with(MockTransport, async |cx| {
///         // You can send requests to the other side
///         let response = cx.send_request(InitializeRequest::make())
///             .block_task()
///             .await?;
///
///         // And send notifications
///         cx.send_notification(StatusUpdate { message: "ready".into() })?;
///
///         Ok(())
///     })
///     .await?;
/// # Ok(())
/// # }
/// ```
///
/// The connection will serve incoming messages in the background while your client closure
/// runs. When the closure returns, the connection shuts down.
///
/// # Example: Complete Agent
///
/// ```no_run
/// # use agent_client_protocol::UntypedRole;
/// # use agent_client_protocol::{Builder};
/// # use agent_client_protocol::Stdio;
/// # use agent_client_protocol::schema::v1::{InitializeRequest, InitializeResponse, PromptRequest, PromptResponse, SessionNotification};
/// # async fn example() -> Result<(), agent_client_protocol::Error> {
/// let transport = Stdio::new();
///
/// UntypedRole.builder()
///     .name("my-agent")  // Optional: for debugging logs
///     .on_receive_request(async |init: InitializeRequest, responder, cx| {
///         let response: InitializeResponse = todo!();
///         responder.respond(response)
///     }, agent_client_protocol::on_receive_request!())
///     .on_receive_request(async |prompt: PromptRequest, responder, cx| {
///         // You can send notifications while processing a request
///         let notif: SessionNotification = todo!();
///         cx.send_notification(notif)?;
///
///         // Then respond to the request
///         let response: PromptResponse = todo!();
///         responder.respond(response)
///     }, agent_client_protocol::on_receive_request!())
///     .connect_to(transport)
///     .await?;
/// # Ok(())
/// # }
/// ```
#[must_use]
#[derive(Debug)]
pub struct Builder<
    Host: Role,
    Handler = NullHandler,
    Runner = NullRun,
    Close = NullClose,
    Context = RawConnectionContext,
> where
    Handler: HandleDispatchFrom<Host::Counterpart>,
    Runner: RunWithConnectionTo<Host::Counterpart>,
    Close: HandleConnectionClose<Host::Counterpart>,
    Context: ConnectionContext,
{
    /// My role.
    host: Host,

    /// Name of the connection, used in tracing logs.
    name: Option<String>,

    /// Handler for incoming messages.
    handler: Handler,

    /// Runner for background connection tasks.
    runner: Runner,

    /// Protocol version mode for the public API and wire compatibility layer.
    protocol_mode: ProtocolMode,

    /// Handler run when the incoming transport reaches clean EOF.
    on_close: Close,

    /// Selects the connection type exposed to user callbacks.
    context: PhantomData<fn() -> Context>,
}

fn default_protocol_mode<Host: Role>() -> ProtocolMode {
    let role = TypeId::of::<Host>();

    if role == TypeId::of::<Agent>() {
        ProtocolMode::v1_agent()
    } else if role == TypeId::of::<Client>() {
        ProtocolMode::v1_client()
    } else if role == TypeId::of::<Proxy>() {
        ProtocolMode::v1_proxy()
    } else {
        ProtocolMode::disabled()
    }
}

impl<Host: Role> Builder<Host, NullHandler, NullRun, NullClose> {
    /// Create a new connection builder for the given role.
    /// This type follows a builder pattern; use other methods to configure and then invoke
    /// [`Self::connect_to`] (to use as a server) or [`Self::connect_with`] to use as a client.
    pub fn new(role: Host) -> Self {
        Self {
            host: role,
            name: None,
            handler: NullHandler,
            runner: NullRun,
            protocol_mode: default_protocol_mode::<Host>(),
            on_close: NullClose,
            context: PhantomData,
        }
    }
}

impl<Host: Role, Handler> Builder<Host, Handler, NullRun, NullClose>
where
    Handler: HandleDispatchFrom<Host::Counterpart>,
{
    /// Create a new connection builder with the given handler.
    pub fn new_with(role: Host, handler: Handler) -> Self {
        Self {
            host: role,
            name: None,
            handler,
            runner: NullRun,
            protocol_mode: default_protocol_mode::<Host>(),
            on_close: NullClose,
            context: PhantomData,
        }
    }
}

#[cfg(feature = "unstable_protocol_v2")]
impl<
    Host: Role,
    Handler: HandleDispatchFrom<Host::Counterpart>,
    Runner: RunWithConnectionTo<Host::Counterpart>,
    Close: HandleConnectionClose<Host::Counterpart>,
> Builder<Host, Handler, Runner, Close>
{
    pub(crate) fn v2_agent(self) -> V2Builder<Host, Handler, Runner, Close> {
        Builder {
            host: self.host,
            name: self.name,
            handler: self.handler,
            runner: self.runner,
            protocol_mode: ProtocolMode::v2_agent(),
            on_close: self.on_close,
            context: PhantomData,
        }
    }

    pub(crate) fn v2_client(self) -> V2Builder<Host, Handler, Runner, Close> {
        Builder {
            host: self.host,
            name: self.name,
            handler: self.handler,
            runner: self.runner,
            protocol_mode: ProtocolMode::v2_client(),
            on_close: self.on_close,
            context: PhantomData,
        }
    }

    pub(crate) fn v2_proxy(self) -> V2Builder<Host, Handler, Runner, Close> {
        Builder {
            host: self.host,
            name: self.name,
            handler: self.handler,
            runner: self.runner,
            protocol_mode: ProtocolMode::v2_proxy(),
            on_close: self.on_close,
            context: PhantomData,
        }
    }

    /// Disable all automatic ACP protocol-version tracking and validation.
    ///
    /// This is a low-level escape hatch for protocol-routing infrastructure
    /// that inspects and validates raw initialize requests and responses
    /// itself before selecting a version-specific implementation. It also
    /// disables the version checks applied to messages after initialization.
    ///
    /// This method is deliberately available only on builders whose callbacks
    /// receive raw [`ConnectionTo`] values. Applications should normally use
    /// [`Client::builder`](crate::Client::builder),
    /// [`Agent::builder`](crate::Agent::builder),
    /// [`Proxy::builder`](crate::Proxy::builder), [`Client::v2`](crate::Client::v2),
    /// [`Agent::v2`](crate::Agent::v2), or [`Proxy::v2`](crate::Proxy::v2)
    /// instead.
    ///
    /// ```compile_fail
    /// # use agent_client_protocol::Client;
    /// let _ = Client.v2().without_acp_version_guard();
    /// ```
    pub fn without_acp_version_guard(mut self) -> Self {
        self.protocol_mode = ProtocolMode::disabled();
        self
    }
}

#[cfg(feature = "unstable_protocol_v2")]
impl<
    Handler: HandleDispatchFrom<Agent>,
    Runner: RunWithConnectionTo<Agent>,
    Close: HandleConnectionClose<Agent>,
> Builder<Client, Handler, Runner, Close>
{
    /// Apply protocol-v2 wire validation while retaining raw callback contexts.
    ///
    /// This is intended for protocol-routing infrastructure that has already
    /// selected v2 but still needs protocol-neutral [`ConnectionTo`] values in
    /// its callbacks. The guarded child must still send and receive the
    /// `initialize` round trip; a router that consumes initialization itself
    /// must use [`Builder::without_acp_version_guard`] for the selected child.
    /// Most clients should use [`Client::v2`](crate::Client::v2), which also
    /// exposes the version-typed [`V2ConnectionTo`] API.
    pub fn with_v2_protocol_guard(mut self) -> Self {
        self.protocol_mode = ProtocolMode::v2_client();
        self
    }
}

#[cfg(feature = "unstable_protocol_v2")]
impl<
    Handler: HandleDispatchFrom<Client>,
    Runner: RunWithConnectionTo<Client>,
    Close: HandleConnectionClose<Client>,
> Builder<Agent, Handler, Runner, Close>
{
    /// Apply protocol-v2 wire validation while retaining raw callback contexts.
    ///
    /// This is intended for protocol-routing infrastructure that has already
    /// selected v2 but still needs protocol-neutral [`ConnectionTo`] values in
    /// its callbacks. The guarded child must still receive and answer the
    /// `initialize` request; a router that consumes initialization itself must
    /// use [`Builder::without_acp_version_guard`] for the selected child. Most
    /// agents should use [`Agent::v2`](crate::Agent::v2), which also exposes the
    /// version-typed [`V2ConnectionTo`] API.
    pub fn with_v2_protocol_guard(mut self) -> Self {
        self.protocol_mode = ProtocolMode::v2_agent();
        self
    }
}

impl<
    Host: Role,
    Handler: HandleDispatchFrom<Host::Counterpart>,
    Runner: RunWithConnectionTo<Host::Counterpart>,
    Close: HandleConnectionClose<Host::Counterpart>,
    Context: ConnectionContext,
> Builder<Host, Handler, Runner, Close, Context>
{
    /// Set the "name" of this connection -- used only for debugging logs.
    pub fn name(mut self, name: impl ToString) -> Self {
        self.name = Some(name.to_string());
        self
    }

    pub(crate) fn v1_agent(mut self) -> Self {
        self.protocol_mode = ProtocolMode::v1_agent();
        self
    }

    pub(crate) fn v1_client(mut self) -> Self {
        self.protocol_mode = ProtocolMode::v1_client();
        self
    }

    /// Merge another [`Builder`] into this one.
    ///
    /// Prefer [`Self::on_receive_request`] or [`Self::on_receive_notification`].
    /// This is a low-level method that is not intended for general use.
    pub fn with_connection_builder(
        self,
        other: Builder<
            Host,
            impl HandleDispatchFrom<Host::Counterpart>,
            impl RunWithConnectionTo<Host::Counterpart>,
            impl HandleConnectionClose<Host::Counterpart>,
            Context,
        >,
    ) -> Builder<
        Host,
        impl HandleDispatchFrom<Host::Counterpart>,
        impl RunWithConnectionTo<Host::Counterpart>,
        impl HandleConnectionClose<Host::Counterpart>,
        Context,
    > {
        let Builder {
            name: other_name,
            handler: other_handler,
            runner: other_runner,
            protocol_mode: other_protocol_mode,
            on_close: other_on_close,
            context: _,
            host: _,
        } = other;
        Builder {
            host: self.host,
            name: self.name,
            handler: ChainedHandler::new(
                self.handler,
                NamedHandler::new(other_name, other_handler),
            ),
            runner: ChainRun::new(self.runner, other_runner),
            protocol_mode: self.protocol_mode.merge(other_protocol_mode),
            on_close: ChainedClose::new(self.on_close, other_on_close),
            context: PhantomData,
        }
    }

    /// Add a new [`HandleDispatchFrom`] to the chain.
    ///
    /// Prefer [`Self::on_receive_request`] or [`Self::on_receive_notification`].
    /// This is a low-level method that is not intended for general use.
    pub fn with_handler(
        self,
        handler: impl HandleDispatchFrom<Host::Counterpart>,
    ) -> Builder<Host, impl HandleDispatchFrom<Host::Counterpart>, Runner, Close, Context> {
        Builder {
            host: self.host,
            name: self.name,
            handler: ChainedHandler::new(self.handler, handler),
            runner: self.runner,
            protocol_mode: self.protocol_mode,
            on_close: self.on_close,
            context: PhantomData,
        }
    }

    /// Add a new [`RunWithConnectionTo`] to the chain.
    pub fn with_runner<Run1>(
        self,
        runner: Run1,
    ) -> Builder<Host, Handler, impl RunWithConnectionTo<Host::Counterpart>, Close, Context>
    where
        Run1: RunWithConnectionTo<Host::Counterpart>,
    {
        Builder {
            host: self.host,
            name: self.name,
            handler: self.handler,
            runner: ChainRun::new(self.runner, runner),
            protocol_mode: self.protocol_mode,
            on_close: self.on_close,
            context: PhantomData,
        }
    }

    /// Enqueue a task to run once the connection is actively serving traffic.
    #[track_caller]
    pub fn with_spawned<F, Fut>(
        self,
        task: F,
    ) -> Builder<Host, Handler, impl RunWithConnectionTo<Host::Counterpart>, Close, Context>
    where
        F: FnOnce(Context::Connection<Host::Counterpart>) -> Fut + Send,
        Fut: Future<Output = Result<(), crate::Error>> + Send,
    {
        let location = Location::caller();
        self.with_runner(SpawnedRun::<_, Context>::new(location, task))
    }

    /// Run a callback when the incoming transport reaches clean EOF.
    ///
    /// Each callback runs at most once and receives the connection context. A
    /// successful callback observes the close without otherwise changing the
    /// lifetime of [`connect_with`](Self::connect_with). Returning an error
    /// shuts down the connection and cancels a still-running `connect_with`
    /// future.
    ///
    /// Multiple callbacks run sequentially in registration order. All of them
    /// run even if an earlier callback fails, after which the first error is
    /// returned. Pending requests are failed before callbacks begin, while the
    /// selected connection context's `incoming_closed` future completes only
    /// after they finish. A callback must therefore not await that close
    /// future itself.
    ///
    /// This separation lets applications choose their cancellation policy. A
    /// callback can notify application-owned tasks and return `Ok(())` for
    /// graceful cleanup, or return an error to stop them immediately.
    ///
    /// ```
    /// # use agent_client_protocol::{Client, ConnectTo, Error};
    /// # async fn example(transport: impl ConnectTo<Client>) -> Result<(), Error> {
    /// Client.builder()
    ///     .on_close(async |_cx| {
    ///         Err(Error::internal_error().data("agent transport closed"))
    ///     })
    ///     .connect_with(transport, async |_cx| {
    ///         std::future::pending().await
    ///     })
    ///     .await?;
    /// # Ok(())
    /// # }
    /// ```
    pub fn on_close<F, Fut>(
        self,
        callback: F,
    ) -> Builder<Host, Handler, Runner, impl HandleConnectionClose<Host::Counterpart>, Context>
    where
        F: FnOnce(Context::Connection<Host::Counterpart>) -> Fut + Send,
        Fut: Future<Output = Result<(), crate::Error>> + Send,
    {
        Builder {
            host: self.host,
            name: self.name,
            handler: self.handler,
            runner: self.runner,
            protocol_mode: self.protocol_mode,
            on_close: ChainedClose::new(self.on_close, CloseCallback::<_, Context>::new(callback)),
            context: PhantomData,
        }
    }

    /// Register a handler for requests, notifications, and responses.
    ///
    /// Use this when you want to handle all JSON-RPC message kinds in one callback.
    /// Your handler receives a [`Dispatch<Req, Notif>`] with three variants:
    ///
    /// - `Dispatch::Request(request, responder)` - A request with its response context
    /// - `Dispatch::Notification(notification)` - A notification
    /// - `Dispatch::Response(result, router)` - A response to a request we sent
    ///
    /// # Example
    ///
    /// ```no_run
    /// # use agent_client_protocol_test::*;
    /// # use agent_client_protocol::Dispatch;
    /// # async fn example() -> Result<(), agent_client_protocol::Error> {
    /// # let connection = mock_connection();
    /// connection.on_receive_dispatch(async |message: Dispatch<MyRequest, StatusUpdate>, _cx| {
    ///     match message {
    ///         Dispatch::Request(req, responder) => {
    ///             // Handle request and send response
    ///             responder.respond(MyResponse { status: "ok".into() })
    ///         }
    ///         Dispatch::Notification(notif) => {
    ///             // Handle notification (no response needed)
    ///             Ok(())
    ///         }
    ///         Dispatch::Response(result, router) => {
    ///             // Forward response to its destination
    ///             router.route_with_result(result)
    ///         }
    ///     }
    /// }, agent_client_protocol::on_receive_dispatch!())
    /// # .connect_to(agent_client_protocol_test::MockTransport).await?;
    /// # Ok(())
    /// # }
    /// ```
    ///
    /// For most use cases, prefer [`on_receive_request`](Self::on_receive_request) or
    /// [`on_receive_notification`](Self::on_receive_notification) which provide cleaner APIs
    /// for handling requests or notifications separately.
    ///
    /// # Ordering
    ///
    /// This callback runs inside the dispatch loop and blocks further message processing
    /// until it completes. See the [`ordering`](crate::concepts::ordering) module for details on
    /// ordering guarantees and how to avoid deadlocks.
    pub fn on_receive_dispatch<Req, Notif, F, T, ToFut>(
        self,
        op: F,
        to_future_hack: ToFut,
    ) -> Builder<Host, impl HandleDispatchFrom<Host::Counterpart>, Runner, Close, Context>
    where
        Host::Counterpart: HasPeer<Host::Counterpart>,
        Req: JsonRpcRequest,
        Notif: JsonRpcNotification,
        F: AsyncFnMut(
                Dispatch<Req, Notif>,
                Context::Connection<Host::Counterpart>,
            ) -> Result<T, crate::Error>
            + Send,
        T: IntoHandled<Dispatch<Req, Notif>>,
        ToFut: Fn(
                &mut F,
                Dispatch<Req, Notif>,
                Context::Connection<Host::Counterpart>,
            ) -> crate::BoxFuture<'_, Result<T, crate::Error>>
            + Send
            + Sync,
    {
        let handler = MessageHandler::<_, _, _, _, _, _, Context>::new(
            self.host.counterpart(),
            self.host.counterpart(),
            op,
            to_future_hack,
        );
        self.with_handler(handler)
    }

    /// Register a handler for JSON-RPC requests of type `Req`.
    ///
    /// Your handler receives three arguments:
    /// 1. The request (type `Req`)
    /// 2. A [`Responder<Req::Response>`] for sending the response
    /// 3. The builder-selected connection context for the peer that sent the
    ///    request (`ConnectionTo` by default, or `V2ConnectionTo` for a
    ///    `V2Builder`)
    ///
    /// The request context allows you to:
    /// - Send the response with [`Responder::respond`]
    /// - Send notifications to the client with the context's
    ///   `send_notification` method
    /// - Send requests to the client with the context's `send_request` method
    ///
    /// # Example
    ///
    /// ```no_run
    /// # use agent_client_protocol::{Agent, ConnectTo};
    /// # use agent_client_protocol::schema::v1::{PromptRequest, PromptResponse, SessionNotification};
    /// # async fn example(transport: impl ConnectTo<Agent>) -> Result<(), agent_client_protocol::Error> {
    /// Agent.builder().on_receive_request(async |request: PromptRequest, responder, cx| {
    ///     // Send a notification while processing
    ///     let notif: SessionNotification = todo!();
    ///     cx.send_notification(notif)?;
    ///
    ///     // Send the response
    ///     let response: PromptResponse = todo!();
    ///     responder.respond(response)
    /// }, agent_client_protocol::on_receive_request!())
    /// .connect_to(transport)
    /// .await
    /// # }
    /// ```
    ///
    /// # Type Parameter
    ///
    /// `Req` can be either a single request type or an enum of multiple request types.
    /// See the [type-driven dispatch](Self#type-driven-message-dispatch) section for details.
    ///
    /// # Ordering
    ///
    /// This callback runs inside the dispatch loop and blocks further message processing
    /// until it completes. See the [`ordering`](crate::concepts::ordering) module for details on
    /// ordering guarantees and how to avoid deadlocks.
    pub fn on_receive_request<Req: JsonRpcRequest, F, T, ToFut>(
        self,
        op: F,
        to_future_hack: ToFut,
    ) -> Builder<Host, impl HandleDispatchFrom<Host::Counterpart>, Runner, Close, Context>
    where
        Host::Counterpart: HasPeer<Host::Counterpart>,
        F: AsyncFnMut(
                Req,
                Responder<Req::Response>,
                Context::Connection<Host::Counterpart>,
            ) -> Result<T, crate::Error>
            + Send,
        T: IntoHandled<(Req, Responder<Req::Response>)>,
        ToFut: Fn(
                &mut F,
                Req,
                Responder<Req::Response>,
                Context::Connection<Host::Counterpart>,
            ) -> crate::BoxFuture<'_, Result<T, crate::Error>>
            + Send
            + Sync,
    {
        let handler = RequestHandler::<_, _, _, _, _, Context>::new(
            self.host.counterpart(),
            self.host.counterpart(),
            op,
            to_future_hack,
        );
        self.with_handler(handler)
    }

    /// Register a handler for JSON-RPC notifications of type `Notif`.
    ///
    /// Notifications are fire-and-forget messages that don't expect a response.
    /// Your handler receives:
    /// 1. The notification (type `Notif`)
    /// 2. The builder-selected connection context for sending messages to the
    ///    other side
    ///
    /// Unlike request handlers, you cannot send a response (notifications don't have IDs),
    /// but you can still send your own requests and notifications using the context.
    ///
    /// # Example
    ///
    /// ```no_run
    /// # use agent_client_protocol_test::*;
    /// # async fn example() -> Result<(), agent_client_protocol::Error> {
    /// # let connection = mock_connection();
    /// connection.on_receive_notification(async |notif: SessionUpdate, cx| {
    ///     // Process the notification
    ///     update_session_state(&notif)?;
    ///
    ///     // Optionally send a notification back
    ///     cx.send_notification(StatusUpdate {
    ///         message: "Acknowledged".into(),
    ///     })?;
    ///
    ///     Ok(())
    /// }, agent_client_protocol::on_receive_notification!())
    /// # .connect_to(agent_client_protocol_test::MockTransport).await?;
    /// # Ok(())
    /// # }
    /// ```
    ///
    /// # Type Parameter
    ///
    /// `Notif` can be either a single notification type or an enum of multiple notification types.
    /// See the [type-driven dispatch](Self#type-driven-message-dispatch) section for details.
    ///
    /// # Ordering
    ///
    /// This callback runs inside the dispatch loop and blocks further message processing
    /// until it completes. See the [`ordering`](crate::concepts::ordering) module for details on
    /// ordering guarantees and how to avoid deadlocks.
    pub fn on_receive_notification<Notif, F, T, ToFut>(
        self,
        op: F,
        to_future_hack: ToFut,
    ) -> Builder<Host, impl HandleDispatchFrom<Host::Counterpart>, Runner, Close, Context>
    where
        Host::Counterpart: HasPeer<Host::Counterpart>,
        Notif: JsonRpcNotification,
        F: AsyncFnMut(Notif, Context::Connection<Host::Counterpart>) -> Result<T, crate::Error>
            + Send,
        T: IntoHandled<(Notif, Context::Connection<Host::Counterpart>)>,
        ToFut: Fn(
                &mut F,
                Notif,
                Context::Connection<Host::Counterpart>,
            ) -> crate::BoxFuture<'_, Result<T, crate::Error>>
            + Send
            + Sync,
    {
        let handler = NotificationHandler::<_, _, _, _, _, Context>::new(
            self.host.counterpart(),
            self.host.counterpart(),
            op,
            to_future_hack,
        );
        self.with_handler(handler)
    }

    /// Register a handler for messages from a specific peer.
    ///
    /// This is similar to [`on_receive_dispatch`](Self::on_receive_dispatch), but allows
    /// specifying the source peer explicitly. This is useful when receiving messages
    /// from a peer that requires message transformation (e.g., unwrapping `SuccessorMessage`
    /// envelopes when receiving from an agent via a proxy).
    ///
    /// For the common case of receiving from the default counterpart, use
    /// [`on_receive_dispatch`](Self::on_receive_dispatch) instead.
    ///
    /// # Ordering
    ///
    /// This callback runs inside the dispatch loop and blocks further message processing
    /// until it completes. See the [`ordering`](crate::concepts::ordering) module for details on
    /// ordering guarantees and how to avoid deadlocks.
    pub fn on_receive_dispatch_from<
        Req: JsonRpcRequest,
        Notif: JsonRpcNotification,
        Peer: Role,
        F,
        T,
        ToFut,
    >(
        self,
        peer: Peer,
        op: F,
        to_future_hack: ToFut,
    ) -> Builder<Host, impl HandleDispatchFrom<Host::Counterpart>, Runner, Close, Context>
    where
        Host::Counterpart: HasPeer<Peer>,
        F: AsyncFnMut(
                Dispatch<Req, Notif>,
                Context::Connection<Host::Counterpart>,
            ) -> Result<T, crate::Error>
            + Send,
        T: IntoHandled<Dispatch<Req, Notif>>,
        ToFut: Fn(
                &mut F,
                Dispatch<Req, Notif>,
                Context::Connection<Host::Counterpart>,
            ) -> crate::BoxFuture<'_, Result<T, crate::Error>>
            + Send
            + Sync,
    {
        let handler = MessageHandler::<_, _, _, _, _, _, Context>::new(
            self.host.counterpart(),
            peer,
            op,
            to_future_hack,
        );
        self.with_handler(handler)
    }

    /// Register a handler for JSON-RPC requests from a specific peer.
    ///
    /// This is similar to [`on_receive_request`](Self::on_receive_request), but allows
    /// specifying the source peer explicitly. This is useful when receiving messages
    /// from a peer that requires message transformation (e.g., unwrapping `SuccessorRequest`
    /// envelopes when receiving from an agent via a proxy).
    ///
    /// For the common case of receiving from the default counterpart, use
    /// [`on_receive_request`](Self::on_receive_request) instead.
    ///
    /// # Example
    ///
    /// ```ignore
    /// use agent_client_protocol::Agent;
    /// use agent_client_protocol::schema::v1::InitializeRequest;
    ///
    /// // Conductor receiving from agent direction - messages will be unwrapped from SuccessorMessage
    /// connection.on_receive_request_from(Agent, async |req: InitializeRequest, responder, cx| {
    ///     // Handle the request
    ///     responder.respond(InitializeResponse::make())
    /// })
    /// ```
    ///
    /// # Ordering
    ///
    /// This callback runs inside the dispatch loop and blocks further message processing
    /// until it completes. See the [`ordering`](crate::concepts::ordering) module for details on
    /// ordering guarantees and how to avoid deadlocks.
    pub fn on_receive_request_from<Req: JsonRpcRequest, Peer: Role, F, T, ToFut>(
        self,
        peer: Peer,
        op: F,
        to_future_hack: ToFut,
    ) -> Builder<Host, impl HandleDispatchFrom<Host::Counterpart>, Runner, Close, Context>
    where
        Host::Counterpart: HasPeer<Peer>,
        F: AsyncFnMut(
                Req,
                Responder<Req::Response>,
                Context::Connection<Host::Counterpart>,
            ) -> Result<T, crate::Error>
            + Send,
        T: IntoHandled<(Req, Responder<Req::Response>)>,
        ToFut: Fn(
                &mut F,
                Req,
                Responder<Req::Response>,
                Context::Connection<Host::Counterpart>,
            ) -> crate::BoxFuture<'_, Result<T, crate::Error>>
            + Send
            + Sync,
    {
        let handler = RequestHandler::<_, _, _, _, _, Context>::new(
            self.host.counterpart(),
            peer,
            op,
            to_future_hack,
        );
        self.with_handler(handler)
    }

    /// Register a handler for JSON-RPC notifications from a specific peer.
    ///
    /// This is similar to [`on_receive_notification`](Self::on_receive_notification), but allows
    /// specifying the source peer explicitly. This is useful when receiving messages
    /// from a peer that requires message transformation (e.g., unwrapping `SuccessorNotification`
    /// envelopes when receiving from an agent via a proxy).
    ///
    /// For the common case of receiving from the default counterpart, use
    /// [`on_receive_notification`](Self::on_receive_notification) instead.
    ///
    /// # Ordering
    ///
    /// This callback runs inside the dispatch loop and blocks further message processing
    /// until it completes. See the [`ordering`](crate::concepts::ordering) module for details on
    /// ordering guarantees and how to avoid deadlocks.
    pub fn on_receive_notification_from<Notif: JsonRpcNotification, Peer: Role, F, T, ToFut>(
        self,
        peer: Peer,
        op: F,
        to_future_hack: ToFut,
    ) -> Builder<Host, impl HandleDispatchFrom<Host::Counterpart>, Runner, Close, Context>
    where
        Host::Counterpart: HasPeer<Peer>,
        F: AsyncFnMut(Notif, Context::Connection<Host::Counterpart>) -> Result<T, crate::Error>
            + Send,
        T: IntoHandled<(Notif, Context::Connection<Host::Counterpart>)>,
        ToFut: Fn(
                &mut F,
                Notif,
                Context::Connection<Host::Counterpart>,
            ) -> crate::BoxFuture<'_, Result<T, crate::Error>>
            + Send
            + Sync,
    {
        let handler = NotificationHandler::<_, _, _, _, _, Context>::new(
            self.host.counterpart(),
            peer,
            op,
            to_future_hack,
        );
        self.with_handler(handler)
    }

    /// Run in server mode with the provided transport.
    ///
    /// This drives the connection by continuously processing messages from the transport
    /// and dispatching them to your registered handlers. The connection will run until:
    /// - The transport closes (e.g., EOF on byte streams)
    /// - An error occurs
    ///
    /// Handler errors are normally contained: requests receive an Error Response,
    /// response-handler errors are routed to the pending local request, and
    /// notification errors are logged without a wire reply.
    ///
    /// On clean EOF, messages already accepted by the outgoing queue—including
    /// handler responses and close-callback notifications—are drained through
    /// the transport sink before this returns `Ok(())`.
    ///
    /// The transport boundary carries [`TransportFrame`] values. Physical stream adapters
    /// serialize and deserialize frames, while channel-based components relay them directly.
    ///
    /// Use this mode when you only need to respond to incoming messages and don't need
    /// to initiate your own requests. If you need to send requests to the other side,
    /// use [`connect_with`](Self::connect_with) instead.
    ///
    /// # Example: Byte Stream Transport
    ///
    /// ```no_run
    /// # use agent_client_protocol::UntypedRole;
    /// # use agent_client_protocol::{Builder};
    /// # use agent_client_protocol::Stdio;
    /// # use agent_client_protocol_test::*;
    /// # async fn example() -> Result<(), agent_client_protocol::Error> {
    /// let transport = Stdio::new();
    ///
    /// UntypedRole.builder()
    ///     .on_receive_request(async |req: MyRequest, responder, cx| {
    ///         responder.respond(MyResponse { status: "ok".into() })
    ///     }, agent_client_protocol::on_receive_request!())
    ///     .connect_to(transport)
    ///     .await?;
    /// # Ok(())
    /// # }
    /// ```
    pub async fn connect_to(
        self,
        transport: impl ConnectTo<Host> + 'static,
    ) -> Result<(), crate::Error> {
        let (_, future) = self.into_connection_and_future(transport, async move |cx| {
            cx.incoming_closed().await;
            cx.drain_outgoing().await
        });
        future.await
    }

    /// Run the connection until the provided closure completes.
    ///
    /// This drives the connection by:
    /// 1. Running your registered handlers in the background to process incoming messages
    /// 2. Executing your `main_fn` closure with the builder-selected connection
    ///    context for sending requests and notifications
    ///
    /// The connection stays active until your `main_fn` returns, then shuts down.
    /// Clean incoming EOF fails every pending request and makes future
    /// requests fail immediately. It does not cancel unrelated work in
    /// `main_fn`: that future may observe the context's `incoming_closed`
    /// future, or the builder can use [`on_close`](Self::on_close) to notify it
    /// or return an error and stop it.
    ///
    /// Use this mode when you need to initiate communication (send requests/notifications)
    /// in addition to responding to incoming messages. For server-only mode where you just
    /// respond to messages, use [`connect_to`](Self::connect_to) instead.
    ///
    /// # Example
    ///
    /// ```no_run
    /// # use agent_client_protocol::UntypedRole;
    /// # use agent_client_protocol::{Builder};
    /// # use agent_client_protocol::ByteStreams;
    /// # use agent_client_protocol::schema::v1::InitializeRequest;
    /// # use agent_client_protocol::Stdio;
    /// # use agent_client_protocol_test::*;
    /// # async fn example() -> Result<(), agent_client_protocol::Error> {
    /// let transport = Stdio::new();
    ///
    /// UntypedRole.builder()
    ///     .on_receive_request(async |req: MyRequest, responder, cx| {
    ///         // Handle incoming requests in the background
    ///         responder.respond(MyResponse { status: "ok".into() })
    ///     }, agent_client_protocol::on_receive_request!())
    ///     .connect_with(transport, async |cx| {
    ///         // Initialize the protocol
    ///         let init_response = cx.send_request(InitializeRequest::make())
    ///             .block_task()
    ///             .await?;
    ///
    ///         // Send more requests...
    ///         let result = cx.send_request(MyRequest {})
    ///             .block_task()
    ///             .await?;
    ///
    ///         // When this closure returns, the connection shuts down
    ///         Ok(())
    ///     })
    ///     .await?;
    /// # Ok(())
    /// # }
    /// ```
    ///
    /// # Parameters
    ///
    /// - `main_fn`: Your client logic. Receives the builder-selected connection
    ///   context for sending messages.
    ///
    /// # Errors
    ///
    /// Returns an error if a handler, background task, transport, or close
    /// callback fails, or if `main_fn` returns an error. Clean incoming EOF is
    /// observable through the context's `incoming_closed` future and is not
    /// itself an error in this mode.
    pub async fn connect_with<R>(
        self,
        transport: impl ConnectTo<Host> + 'static,
        main_fn: impl AsyncFnOnce(Context::Connection<Host::Counterpart>) -> Result<R, crate::Error>,
    ) -> Result<R, crate::Error> {
        let (_, future) = self.into_connection_and_future(transport, async move |connection| {
            main_fn(connection_context::from_raw::<Context, _>(connection)).await
        });
        future.await
    }

    /// Helper that returns a [`ConnectionTo<R>`] and a future that runs this connection until `main_fn` returns.
    fn into_connection_and_future<R>(
        self,
        transport: impl ConnectTo<Host> + 'static,
        main_fn: impl AsyncFnOnce(ConnectionTo<Host::Counterpart>) -> Result<R, crate::Error>,
    ) -> (
        ConnectionTo<Host::Counterpart>,
        impl Future<Output = Result<R, crate::Error>>,
    ) {
        let Self {
            name,
            handler,
            runner,
            host: me,
            protocol_mode,
            on_close,
            context: _,
        } = self;

        let (outgoing_tx, outgoing_rx) = mpsc::unbounded();
        let (new_task_tx, new_task_rx) = mpsc::unbounded();
        let (dynamic_handler_tx, dynamic_handler_rx) = mpsc::unbounded();
        let pending_replies = PendingReplies::default();

        // Convert transport into server - this returns a channel for us to use
        // and a future that runs the transport.
        let transport_component = crate::DynConnectTo::new(transport);
        let (transport_channel, transport_future) = transport_component.into_channel_and_future();
        let (transport_completion_tx, transport_completion_rx) = oneshot::channel();
        let transport_completion = transport_completion_rx
            .map(|result| {
                result.unwrap_or_else(|error| {
                    Err(crate::util::internal_error(format!(
                        "transport task dropped before reporting completion: {error}"
                    )))
                })
            })
            .boxed()
            .shared();

        let connection = ConnectionTo::new(
            me.counterpart(),
            outgoing_tx,
            new_task_tx,
            dynamic_handler_tx,
            transport_completion,
            pending_replies.registrar(),
            protocol_mode,
        );
        let spawn_result = connection.spawn(async move {
            let result = transport_future.await;
            drop(transport_completion_tx.send(result.clone()));
            result
        });

        // Destructure the channel endpoints
        let Channel {
            rx: transport_incoming_rx,
            tx: transport_outgoing_tx,
        } = transport_channel;

        let protocol_compat = ProtocolCompat::new(protocol_mode);

        let future = crate::util::instrument_with_connection_name(name, {
            let connection = connection.clone();
            async move {
                let () = spawn_result?;

                let background = async {
                    let incoming = incoming_actor::incoming_protocol_actor(
                        me.counterpart(),
                        &connection,
                        transport_incoming_rx,
                        dynamic_handler_rx,
                        pending_replies.clone(),
                        incoming_actor::IncomingHandlers::new(handler, on_close),
                        protocol_compat.clone(),
                    );
                    let other_actors = async {
                        futures::try_join!(
                            // Protocol layer: OutgoingMessage -> RawJsonRpcMessage
                            outgoing_actor::outgoing_protocol_actor(
                                outgoing_rx,
                                pending_replies,
                                transport_outgoing_tx,
                                protocol_compat,
                            ),
                            task_actor::task_actor(new_task_rx, &connection),
                            runner.run_with_connection_to(connection.clone()),
                        )?;
                        Ok(())
                    };

                    // EOF can wake a pending request consumer, which may make
                    // the task actor fail while close callbacks are running.
                    // Keep the incoming actor alive until those callbacks have
                    // all finished, just as we do when the foreground wakes.
                    run_until_connection_close(
                        incoming,
                        other_actors,
                        connection.incoming_closed.clone(),
                    )
                    .await
                };

                run_until_connection_close(
                    background,
                    main_fn(connection.clone()),
                    connection.incoming_closed.clone(),
                )
                .await
            }
        });

        (connection, future)
    }
}

#[cfg(feature = "unstable_mcp_over_acp")]
impl<
    Host: Role,
    Handler: HandleDispatchFrom<Host::Counterpart>,
    Runner: RunWithConnectionTo<Host::Counterpart>,
    Close: HandleConnectionClose<Host::Counterpart>,
> Builder<Host, Handler, Runner, Close, RawConnectionContext>
{
    /// Add an MCP server to protocol v1 session setup requests proxied through
    /// this connection.
    ///
    /// The same native MCP server declaration is added to new, load, and resume
    /// requests, plus fork requests when `unstable_session_fork` is enabled.
    ///
    /// Only applicable to proxies. Use the same method on `V2Builder` to
    /// attach the server to protocol v2 setup requests.
    pub fn with_mcp_server(
        self,
        mcp_server: McpServer<Host::Counterpart, impl RunWithConnectionTo<Host::Counterpart>>,
    ) -> Builder<
        Host,
        impl HandleDispatchFrom<Host::Counterpart>,
        impl RunWithConnectionTo<Host::Counterpart>,
        Close,
        RawConnectionContext,
    >
    where
        Host::Counterpart: HasPeer<Agent> + HasPeer<Client>,
    {
        let (handler, runner) = mcp_server.into_handler_and_runner();
        self.with_handler(handler).with_runner(runner)
    }
}

#[cfg(all(feature = "unstable_mcp_over_acp", feature = "unstable_protocol_v2"))]
impl<
    Host: Role,
    Handler: HandleDispatchFrom<Host::Counterpart>,
    Runner: RunWithConnectionTo<Host::Counterpart>,
    Close: HandleConnectionClose<Host::Counterpart>,
> Builder<Host, Handler, Runner, Close, V2ConnectionContext>
{
    /// Add an MCP server to protocol v2 session setup requests proxied through
    /// this connection.
    ///
    /// The same native MCP server declaration is added to new and resume
    /// requests, plus fork requests when `unstable_session_fork` is enabled.
    /// Unrelated request fields are preserved exactly.
    ///
    /// Only applicable to proxies.
    pub fn with_mcp_server(
        self,
        mcp_server: McpServer<Host::Counterpart, impl RunWithConnectionTo<Host::Counterpart>>,
    ) -> Builder<
        Host,
        impl HandleDispatchFrom<Host::Counterpart>,
        impl RunWithConnectionTo<Host::Counterpart>,
        Close,
        V2ConnectionContext,
    >
    where
        Host::Counterpart: HasPeer<Agent> + HasPeer<Client>,
    {
        let (handler, runner) = mcp_server.into_v2_handler_and_runner();
        self.with_handler(handler).with_runner(runner)
    }
}

impl<R, H, Run, Close, Context> ConnectTo<R::Counterpart> for Builder<R, H, Run, Close, Context>
where
    R: Role,
    H: HandleDispatchFrom<R::Counterpart> + 'static,
    Run: RunWithConnectionTo<R::Counterpart> + 'static,
    Close: HandleConnectionClose<R::Counterpart> + 'static,
    Context: ConnectionContext,
{
    async fn connect_to(self, client: impl ConnectTo<R>) -> Result<(), crate::Error> {
        Builder::connect_to(self, client).await
    }
}

/// The payload sent through the response oneshot channel.
///
/// Includes the response value and an optional ack channel for dispatch loop
/// synchronization.
pub(crate) struct ResponsePayload {
    /// The response result - either the JSON value or an error.
    pub(crate) result: Result<serde_json::Value, crate::Error>,

    /// Optional acknowledgment channel for dispatch loop synchronization.
    ///
    /// When present, the receiver must send on this channel to signal that
    /// response processing is complete, allowing the dispatch loop to continue
    /// to the next message.
    ///
    /// This is present when ordered response consumption was selected before
    /// the response was routed during its original dispatch. Public callback
    /// consumption and framework-owned ordered blocking transforms can hold
    /// the dispatch loop; ordinary blocking consumers, local error paths, and
    /// responses routed later do not.
    pub(crate) ack_tx: Option<oneshot::Sender<()>>,
}

type ResponseRouteHook =
    Box<dyn FnOnce(&str, &serde_json::Value) -> Result<(), crate::Error> + Send>;

/// A prerequisite that must complete before an outgoing request is published
/// to the transport.
struct RequestReadiness {
    future: BoxFuture<'static, Result<(), crate::Error>>,
}

impl RequestReadiness {
    fn new(future: impl Future<Output = Result<(), crate::Error>> + Send + 'static) -> Self {
        Self {
            future: future.boxed(),
        }
    }
}

impl Future for RequestReadiness {
    type Output = Result<(), crate::Error>;

    fn poll(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Self::Output> {
        self.future.as_mut().poll(cx)
    }
}

impl Debug for RequestReadiness {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("RequestReadiness")
            .finish_non_exhaustive()
    }
}

impl std::fmt::Debug for ResponsePayload {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ResponsePayload")
            .field("result", &self.result)
            .field("ack_tx", &self.ack_tx.as_ref().map(|_| "..."))
            .finish()
    }
}

#[derive(Clone, Debug, Default)]
struct ResponseOrdering {
    ordered: Arc<AtomicBool>,
}

impl ResponseOrdering {
    fn mark_ordered(&self) {
        self.ordered.store(true, Ordering::Release);
    }

    fn is_ordered(&self) -> bool {
        self.ordered.load(Ordering::Acquire)
    }
}

struct PendingReply {
    method: String,
    role_id: RoleId,
    sender: oneshot::Sender<ResponsePayload>,
    cancellation_disarm: SentRequestCancellationDisarm,
    ordering: ResponseOrdering,
    response_route_hook: Option<ResponseRouteHook>,
}

impl PendingReply {
    fn fail(self, error: crate::Error) {
        self.cancellation_disarm.disarm();
        if self
            .sender
            .send(ResponsePayload {
                result: Err(error),
                ack_tx: None,
            })
            .is_err()
        {
            tracing::trace!(method = %self.method, "Pending request was already dropped");
        }
    }

    fn fail_incoming_closed(self) {
        let error = incoming_transport_closed_error(&self.method);
        self.fail(error);
    }
}

#[derive(Default)]
struct PendingRepliesInner {
    incoming_closed: bool,
    replies: HashMap<RequestId, PendingReply>,
}

#[derive(Clone, Default)]
struct PendingReplies {
    inner: Arc<Mutex<PendingRepliesInner>>,
}

impl PendingReplies {
    fn registrar(&self) -> PendingRepliesRegistrar {
        PendingRepliesRegistrar {
            inner: Arc::downgrade(&self.inner),
        }
    }

    fn contains(&self, id: &RequestId) -> bool {
        self.inner
            .lock()
            .expect("pending replies mutex poisoned")
            .replies
            .contains_key(id)
    }

    fn remove(&self, id: &RequestId) -> Option<PendingReply> {
        self.inner
            .lock()
            .expect("pending replies mutex poisoned")
            .replies
            .remove(id)
    }

    /// Atomically reject new subscriptions and fail every existing one.
    fn close_incoming(&self) -> usize {
        let replies = {
            let mut inner = self.inner.lock().expect("pending replies mutex poisoned");
            inner.incoming_closed = true;
            std::mem::take(&mut inner.replies)
        };
        let count = replies.len();
        for (_, reply) in replies {
            reply.fail_incoming_closed();
        }
        count
    }
}

/// A non-owning handle used to register a request before it enters the
/// outgoing queue. Keeping this weak prevents escaped [`ConnectionTo`] clones
/// from extending the lifetime of response senders after the driver stops.
#[derive(Clone)]
struct PendingRepliesRegistrar {
    inner: Weak<Mutex<PendingRepliesInner>>,
}

impl PendingRepliesRegistrar {
    /// Register a response destination before the request becomes observable.
    ///
    /// Returns `false` after failing `reply` when EOF has already made a
    /// response impossible or the connection driver is no longer running.
    fn subscribe(
        &self,
        id: RequestId,
        reply: PendingReply,
        incoming_closed: &IncomingClosed,
    ) -> bool {
        let Some(inner) = self.inner.upgrade() else {
            if incoming_closed.is_closing() {
                reply.fail_incoming_closed();
            } else {
                let method = reply.method.clone();
                reply.fail(crate::util::internal_error(format!(
                    "failed to send outgoing request `{method}`: connection is no longer running"
                )));
            }
            return false;
        };

        let result = {
            let mut inner = inner.lock().expect("pending replies mutex poisoned");
            if inner.incoming_closed {
                Err(reply)
            } else {
                Ok(inner.replies.insert(id, reply))
            }
        };

        match result {
            Err(rejected) => {
                rejected.fail_incoming_closed();
                false
            }
            Ok(replaced) => {
                if let Some(replaced) = replaced {
                    replaced.fail(
                        crate::Error::internal_error()
                            .data("outgoing request ID was reused before its response arrived"),
                    );
                }
                true
            }
        }
    }

    fn remove(&self, id: &RequestId) -> Option<PendingReply> {
        self.inner
            .upgrade()?
            .lock()
            .expect("pending replies mutex poisoned")
            .replies
            .remove(id)
    }
}

impl Debug for PendingRepliesRegistrar {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("PendingRepliesRegistrar")
            .field("is_connected", &(self.inner.strong_count() > 0))
            .finish()
    }
}

/// A request-local marker that is set when the peer asks to cancel the request.
///
/// Request handlers can get this handle from [`Responder::cancellation`] and
/// use it from spawned work to stop long-running request processing
/// cooperatively.
#[derive(Clone)]
pub struct RequestCancellation {
    state: Arc<RequestCancellationState>,
}

struct RequestCancellationState {
    cancelled: AtomicBool,
    signal_tx: Mutex<Option<oneshot::Sender<()>>>,
    signal_rx: future::Shared<BoxFuture<'static, ()>>,
}

impl RequestCancellation {
    fn new() -> Self {
        let (signal_tx, signal_rx) = oneshot::channel();
        let signal_rx = signal_rx.map(|_| ()).boxed().shared();
        Self {
            state: Arc::new(RequestCancellationState {
                cancelled: AtomicBool::new(false),
                signal_tx: Mutex::new(Some(signal_tx)),
                signal_rx,
            }),
        }
    }

    /// Wait until the peer sends `$/cancel_request` for this request.
    ///
    /// If cancellation was already requested, this returns immediately.
    pub async fn cancelled(&self) {
        self.state.signal_rx.clone().await;
    }

    /// Run request work until it completes or the peer asks to cancel it.
    ///
    /// If cancellation is requested first, this returns
    /// [`Error::request_cancelled`]. This is a convenience for request handlers
    /// that want to respond with the normal result or the standard
    /// cancellation error.
    ///
    /// When cancellation wins, `future` is dropped: work stops at its next
    /// await point, partial results are lost, and any cleanup must happen in
    /// `Drop` implementations. Handlers that need to flush partial results or
    /// run async cleanup should instead watch [`cancelled`](Self::cancelled)
    /// or poll [`is_cancelled`](Self::is_cancelled) from inside the work.
    ///
    /// [`Error::request_cancelled`]: crate::Error::request_cancelled
    pub async fn run_until_cancelled<T>(
        &self,
        future: impl std::future::Future<Output = Result<T, crate::Error>>,
    ) -> Result<T, crate::Error> {
        if self.is_cancelled() {
            return Err(crate::Error::request_cancelled());
        }

        match future::select(pin!(future), pin!(self.cancelled())).await {
            Either::Left((result, _)) => result,
            Either::Right(((), _)) => Err(crate::Error::request_cancelled()),
        }
    }

    /// Returns whether the peer has already requested cancellation.
    #[must_use]
    pub fn is_cancelled(&self) -> bool {
        self.state.cancelled.load(Ordering::Acquire)
    }

    fn cancel(&self) {
        if self.state.cancelled.swap(true, Ordering::AcqRel) {
            return;
        }

        let signal_tx = self
            .state
            .signal_tx
            .lock()
            .expect("request cancellation signal mutex poisoned")
            .take();

        // Complete the oneshot outside the lock: it wakes waiters, and
        // arbitrary waker code must not observe the lock held.
        if let Some(signal_tx) = signal_tx {
            let _ = signal_tx.send(());
        }
    }
}

impl Debug for RequestCancellation {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("RequestCancellation")
            .field("is_cancelled", &self.is_cancelled())
            .finish_non_exhaustive()
    }
}

/// Per-request cancellation state tracked by [`RequestCancellationRegistry`].
///
/// The full [`RequestCancellation`] marker (with its wakeup machinery) is only
/// allocated once a handler asks for it via [`Responder::cancellation`]; until
/// then an incoming `$/cancel_request` just flips the entry to `Cancelled`.
/// This keeps the per-request cost of the registry to a single map entry.
#[derive(Debug)]
enum RequestCancellationEntry {
    /// The request is in flight; no marker handed out, no cancellation yet.
    Armed,
    /// `$/cancel_request` arrived before a marker was handed out.
    Cancelled,
    /// A marker was handed out via [`Responder::cancellation`].
    Marker(RequestCancellation),
}

/// A registered request's cancellation state, tagged with the generation of
/// its registration.
///
/// The generation distinguishes a registration from earlier ones that used
/// the same request ID, so that when a (protocol-violating) peer reuses the
/// ID of a request that is still in flight, the stale request's responder can
/// neither remove nor observe the cancellation state of the newer request.
#[derive(Debug)]
struct RequestCancellationSlot {
    generation: u64,
    entry: RequestCancellationEntry,
}

#[derive(Debug, Default)]
struct RequestCancellationRegistryInner {
    slots: HashMap<RequestId, RequestCancellationSlot>,
    next_generation: u64,
}

#[derive(Clone, Debug, Default)]
struct RequestCancellationRegistry {
    inner: Arc<Mutex<RequestCancellationRegistryInner>>,
}

#[derive(Debug)]
struct ResponderCancellation {
    id: RequestId,
    generation: u64,
    registry: RequestCancellationRegistry,
}

impl RequestCancellationRegistry {
    fn new() -> Self {
        Self::default()
    }

    fn register(&self, id: &RequestId) -> ResponderCancellation {
        let generation = {
            let mut inner = self
                .inner
                .lock()
                .expect("request cancellation registry mutex poisoned");
            let generation = inner.next_generation;
            inner.next_generation += 1;
            if inner
                .slots
                .insert(
                    id.clone(),
                    RequestCancellationSlot {
                        generation,
                        entry: RequestCancellationEntry::Armed,
                    },
                )
                .is_some()
            {
                tracing::debug!("peer reused the ID of a request that is still in flight");
            }
            generation
        };
        ResponderCancellation {
            id: id.clone(),
            generation,
            registry: self.clone(),
        }
    }

    /// Get the cancellation marker for a registered request, creating it on
    /// first use. Repeated calls return markers that share the same state.
    ///
    /// Exception: when the registration is stale (a protocol-violating peer
    /// reused this request ID and the slot now belongs to a newer request, or
    /// was already removed by it), every call returns a fresh *detached*
    /// marker. Detached markers can never fire, and detached markers from
    /// repeated calls do not share state with each other.
    fn marker(&self, id: &RequestId, generation: u64) -> RequestCancellation {
        let mut inner = self
            .inner
            .lock()
            .expect("request cancellation registry mutex poisoned");
        let Some(slot) = inner.slots.get_mut(id) else {
            // The slot lives as long as the responder that owns it, so this
            // is only reachable if the peer reused this request ID and the
            // newer request's responder already removed the replacement slot.
            // Hand out a detached marker rather than panicking.
            return RequestCancellation::new();
        };
        if slot.generation != generation {
            // The peer reused this request ID while the request was still in
            // flight, and the slot now belongs to the newer request. Hand the
            // stale responder a detached marker instead of cross-wiring the
            // two requests' cancellation states.
            return RequestCancellation::new();
        }
        let entry = &mut slot.entry;
        match entry {
            RequestCancellationEntry::Marker(marker) => marker.clone(),
            RequestCancellationEntry::Armed => {
                let marker = RequestCancellation::new();
                *entry = RequestCancellationEntry::Marker(marker.clone());
                marker
            }
            RequestCancellationEntry::Cancelled => {
                // No one can be waiting on a marker that did not exist yet,
                // so firing it while holding the registry lock is fine.
                let marker = RequestCancellation::new();
                marker.cancel();
                *entry = RequestCancellationEntry::Marker(marker.clone());
                marker
            }
        }
    }

    fn cancel_if_requested(&self, dispatch: &Dispatch) -> Result<bool, crate::Error> {
        let Some(request_id) = cancellation_request_id(dispatch)? else {
            return Ok(false);
        };
        Ok(self.cancel(&request_id))
    }

    /// Mark whichever request currently owns `request_id` as cancelled.
    fn cancel(&self, request_id: &RequestId) -> bool {
        let marker = {
            let mut inner = self
                .inner
                .lock()
                .expect("request cancellation registry mutex poisoned");
            let Some(slot) = inner.slots.get_mut(request_id) else {
                return false;
            };
            let entry = &mut slot.entry;
            match entry {
                RequestCancellationEntry::Marker(marker) => marker.clone(),
                RequestCancellationEntry::Cancelled => return true,
                RequestCancellationEntry::Armed => {
                    *entry = RequestCancellationEntry::Cancelled;
                    return true;
                }
            }
        };

        // Fire the marker outside the registry lock: waking waiters runs
        // arbitrary waker code that must not observe the lock held.
        marker.cancel();
        true
    }

    /// Remove the slot for `request_id`, but only if it still belongs to the
    /// registration identified by `generation`.
    fn remove(&self, request_id: &RequestId, generation: u64) {
        let mut inner = self
            .inner
            .lock()
            .expect("request cancellation registry mutex poisoned");
        if inner
            .slots
            .get(request_id)
            .is_some_and(|slot| slot.generation == generation)
        {
            inner.slots.remove(request_id);
        }
    }
}

impl ResponderCancellation {
    fn cancellation(&self) -> RequestCancellation {
        self.registry.marker(&self.id, self.generation)
    }
}

impl Drop for ResponderCancellation {
    fn drop(&mut self) {
        self.registry.remove(&self.id, self.generation);
    }
}

fn cancellation_request_id(dispatch: &Dispatch) -> Result<Option<RequestId>, crate::Error> {
    let Dispatch::Notification(message) = dispatch else {
        return Ok(None);
    };
    cancellation_request_id_from_message(message)
}

fn cancellation_request_id_from_message(
    message: &UntypedMessage,
) -> Result<Option<RequestId>, crate::Error> {
    let (method, params) = peel_successor_envelopes(&message.method, &message.params);
    if !crate::schema::v1::CancelRequestNotification::matches_method(method) {
        return Ok(None);
    }

    let notification = crate::schema::v1::CancelRequestNotification::parse_message(method, params)?;
    Ok(Some(notification.request_id))
}

/// Peel any [`SuccessorMessage`] envelopes off a notification by reference,
/// returning the innermost method and params.
///
/// This only peeks at the envelope's `method`/`params` fields instead of
/// deserializing the envelope, for two reasons:
///
/// - It avoids deep-cloning the params of every wrapped notification on the
///   hot dispatch path just to inspect the inner method name.
/// - It is deliberately lenient: a malformed envelope is left as-is here and
///   flows on to the handler chain, which is responsible for reporting it.
///
/// [`SuccessorMessage`]: crate::schema::SuccessorMessage
fn peel_successor_envelopes<'message>(
    mut method: &'message str,
    mut params: &'message serde_json::Value,
) -> (&'message str, &'message serde_json::Value) {
    while crate::schema::SuccessorMessage::<UntypedMessage>::matches_method(method) {
        let Some(inner_method) = params.get("method").and_then(serde_json::Value::as_str) else {
            break;
        };
        method = inner_method;
        params = params.get("params").unwrap_or(&serde_json::Value::Null);
    }
    (method, params)
}

/// Whether a notification is a `$/cancel_request`, even when it is still
/// wrapped in `_proxy/successor` envelopes.
///
/// `$/cancel_request` is connection-scoped: its `requestId` was allocated on
/// the connection the notification arrived over and means nothing on any
/// other connection. Generic forwarding code (such as
/// [`ConnectionTo::send_proxied_message_to`]) uses this check to drop the raw
/// notification instead of tunneling it across a hop; the cancellation still
/// propagates because [`forward_response_to`](SentRequest::forward_response_to)
/// re-issues it with the forwarded request's own ID.
///
/// Checking a notification whose method is not the successor envelope is a
/// plain method-name comparison. Only successor-wrapped notifications pay for
/// a serialization to peel the envelope.
#[must_use]
pub fn is_cancel_request_notification<N: JsonRpcNotification>(notification: &N) -> bool {
    let method = notification.method();
    if crate::schema::v1::CancelRequestNotification::matches_method(method) {
        return true;
    }
    if !crate::schema::SuccessorMessage::<UntypedMessage>::matches_method(method) {
        return false;
    }

    match notification.to_untyped_message() {
        Ok(untyped) => {
            let (method, _params) = peel_successor_envelopes(&untyped.method, &untyped.params);
            crate::schema::v1::CancelRequestNotification::matches_method(method)
        }
        Err(error) => {
            tracing::debug!(
                ?error,
                "failed to inspect successor-wrapped notification for cancellation"
            );
            false
        }
    }
}

/// Messages send to be serialized over the transport.
#[derive(Clone)]
enum ResponseDestination {
    Individual(IndividualResponseSlot),
    Batch(BatchResponseSlot),
}

impl std::fmt::Debug for ResponseDestination {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Individual(slot) => formatter.debug_tuple("Individual").field(slot).finish(),
            Self::Batch(slot) => formatter.debug_tuple("Batch").field(slot).finish(),
        }
    }
}

impl ResponseDestination {
    fn individual() -> Self {
        Self::Individual(IndividualResponseSlot::default())
    }

    fn batch(slot_count: usize) -> (impl Iterator<Item = Self>, BatchDispatchCompletion) {
        let state = Arc::new(Mutex::new(BatchResponseState {
            remaining: slot_count,
            responses: (0..slot_count).map(|_| None).collect(),
            abandoned: (0..slot_count).map(|_| None).collect(),
            active_handler_attempts: (0..slot_count).map(|_| 0).collect(),
            dispatch_complete: false,
            emitted: false,
        }));

        (
            (0..slot_count).map({
                let state = state.clone();
                move |index| {
                    Self::Batch(BatchResponseSlot {
                        state: state.clone(),
                        index,
                    })
                }
            }),
            BatchDispatchCompletion { state },
        )
    }

    fn complete(self, response: RawJsonRpcMessage) -> Option<TransportFrame> {
        match self {
            Self::Individual(slot) => slot.complete(response),
            Self::Batch(slot) => slot.complete(response).map(batch_response_frame),
        }
    }

    fn abandon(self, fallback: RawJsonRpcMessage) -> Option<TransportFrame> {
        match self {
            Self::Individual(_) => None,
            Self::Batch(slot) => slot.abandon(fallback).map(batch_response_frame),
        }
    }

    fn is_batch(&self) -> bool {
        matches!(self, Self::Batch(_))
    }

    fn begin_handler_attempt(
        &self,
        message_tx: OutgoingMessageTx,
    ) -> Option<ResponderHandlerAttempt> {
        let Self::Batch(slot) = self else {
            return None;
        };
        slot.begin_handler_attempt();
        Some(ResponderHandlerAttempt {
            message_tx,
            destination: self.clone(),
        })
    }

    fn finish_handler_attempt(self) -> Option<TransportFrame> {
        match self {
            Self::Individual(_) => None,
            Self::Batch(slot) => slot.finish_handler_attempt().map(batch_response_frame),
        }
    }
}

#[derive(Clone, Debug, Default)]
struct IndividualResponseSlot {
    completed: Arc<AtomicBool>,
}

impl IndividualResponseSlot {
    fn complete(self, response: RawJsonRpcMessage) -> Option<TransportFrame> {
        if self.completed.swap(true, Ordering::AcqRel) {
            tracing::warn!("Ignoring duplicate completion of JSON-RPC request");
            return None;
        }

        Some(TransportFrame::Single(response))
    }
}

fn batch_response_frame(responses: Vec<RawJsonRpcMessage>) -> TransportFrame {
    TransportFrame::Batch(
        TransportBatch::from_messages(responses)
            .expect("a completed JSON-RPC response batch is non-empty"),
    )
}

#[derive(Clone)]
struct BatchDispatchCompletion {
    state: Arc<Mutex<BatchResponseState>>,
}

impl std::fmt::Debug for BatchDispatchCompletion {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("BatchDispatchCompletion")
            .finish_non_exhaustive()
    }
}

impl BatchDispatchCompletion {
    fn complete(self) -> Option<TransportFrame> {
        let mut state = self
            .state
            .lock()
            .expect("batch response accumulator mutex poisoned");
        if state.dispatch_complete {
            tracing::warn!("Ignoring duplicate JSON-RPC batch dispatch completion");
            return None;
        }
        state.dispatch_complete = true;
        for index in 0..state.responses.len() {
            promote_abandoned_response(&mut state, index);
        }
        take_completed_batch(&mut state).map(batch_response_frame)
    }
}

fn promote_abandoned_response(state: &mut BatchResponseState, index: usize) {
    if state.active_handler_attempts[index] == 0
        && state.responses[index].is_none()
        && let Some(fallback) = state.abandoned[index].take()
    {
        state.responses[index] = Some(fallback);
        state.remaining -= 1;
    }
}

fn take_completed_batch(state: &mut BatchResponseState) -> Option<Vec<RawJsonRpcMessage>> {
    if !state.dispatch_complete || state.remaining != 0 || state.emitted {
        return None;
    }

    state.emitted = true;
    Some(
        state
            .responses
            .iter_mut()
            .map(|response| {
                response
                    .take()
                    .expect("completed JSON-RPC batch has every response slot")
            })
            .collect(),
    )
}

#[derive(Clone)]
struct BatchResponseSlot {
    state: Arc<Mutex<BatchResponseState>>,
    index: usize,
}

impl std::fmt::Debug for BatchResponseSlot {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("BatchResponseSlot")
            .field("index", &self.index)
            .finish_non_exhaustive()
    }
}

impl BatchResponseSlot {
    fn begin_handler_attempt(&self) {
        let mut state = self
            .state
            .lock()
            .expect("batch response accumulator mutex poisoned");
        state.active_handler_attempts[self.index] += 1;
    }

    fn finish_handler_attempt(self) -> Option<Vec<RawJsonRpcMessage>> {
        let mut state = self
            .state
            .lock()
            .expect("batch response accumulator mutex poisoned");
        state.active_handler_attempts[self.index] = state.active_handler_attempts[self.index]
            .checked_sub(1)
            .expect("handler attempt completion without a matching start");
        if state.dispatch_complete {
            promote_abandoned_response(&mut state, self.index);
        }
        take_completed_batch(&mut state)
    }

    fn complete(self, response: RawJsonRpcMessage) -> Option<Vec<RawJsonRpcMessage>> {
        let mut state = self
            .state
            .lock()
            .expect("batch response accumulator mutex poisoned");
        if state.emitted {
            tracing::warn!(
                index = self.index,
                "Ignoring response after JSON-RPC batch was already completed"
            );
            return None;
        }
        if self.index >= state.responses.len() {
            tracing::error!(index = self.index, "Invalid JSON-RPC batch response slot");
            return None;
        }
        if state.responses[self.index].is_some() {
            tracing::warn!(
                index = self.index,
                "Ignoring duplicate completion of JSON-RPC batch response slot"
            );
            return None;
        }

        state.abandoned[self.index] = None;
        state.responses[self.index] = Some(response);
        state.remaining -= 1;
        take_completed_batch(&mut state)
    }

    fn abandon(self, fallback: RawJsonRpcMessage) -> Option<Vec<RawJsonRpcMessage>> {
        let mut state = self
            .state
            .lock()
            .expect("batch response accumulator mutex poisoned");
        if state.emitted || state.responses[self.index].is_some() {
            return None;
        }
        if state.abandoned[self.index].is_some() {
            tracing::warn!(
                index = self.index,
                "Ignoring duplicate abandonment of JSON-RPC batch response slot"
            );
            return None;
        }

        if state.dispatch_complete && state.active_handler_attempts[self.index] == 0 {
            state.responses[self.index] = Some(fallback);
            state.remaining -= 1;
        } else {
            state.abandoned[self.index] = Some(fallback);
        }
        take_completed_batch(&mut state)
    }
}

struct BatchResponseState {
    remaining: usize,
    responses: Vec<Option<RawJsonRpcMessage>>,
    abandoned: Vec<Option<RawJsonRpcMessage>>,
    active_handler_attempts: Vec<usize>,
    dispatch_complete: bool,
    emitted: bool,
}

#[derive(Clone, Debug)]
struct RequestReplyTarget {
    id: RequestId,
    method: String,
    destination: ResponseDestination,
}

struct ResponderHandlerAttempt {
    message_tx: OutgoingMessageTx,
    destination: ResponseDestination,
}

impl Drop for ResponderHandlerAttempt {
    fn drop(&mut self) {
        if let Err(error) = send_raw_message(
            &self.message_tx,
            OutgoingMessage::BatchHandlerAttemptComplete {
                destination: self.destination.clone(),
            },
        ) {
            tracing::debug!(?error, "could not complete JSON-RPC batch handler attempt");
        }
    }
}

#[derive(Clone)]
struct ResponseReplyTarget {
    id: RequestId,
    method: String,
    sender: Arc<Mutex<Option<oneshot::Sender<ResponsePayload>>>>,
    ordering: ResponseOrdering,
    dispatch: ResponseDispatch,
}

impl ResponseReplyTarget {
    fn route(self, result: Result<serde_json::Value, crate::Error>) {
        let sender = self
            .sender
            .lock()
            .expect("response reply mutex poisoned")
            .take();
        let Some(sender) = sender else {
            tracing::debug!(
                method = %self.method,
                id = ?self.id,
                "response was already routed to its local awaiter"
            );
            return;
        };

        let ack_tx = self.dispatch.acknowledgment(&self.ordering);
        if sender.send(ResponsePayload { result, ack_tx }).is_err() {
            tracing::debug!(
                method = %self.method,
                id = ?self.id,
                "dropped response because local receiver was gone"
            );
        }
    }
}

#[derive(Clone, Default)]
struct ResponseDispatch {
    state: Arc<Mutex<ResponseDispatchState>>,
}

#[derive(Default)]
struct ResponseDispatchState {
    complete: bool,
    ack_rx: Option<oneshot::Receiver<()>>,
}

impl ResponseDispatch {
    fn acknowledgment(&self, ordering: &ResponseOrdering) -> Option<oneshot::Sender<()>> {
        if !ordering.is_ordered() {
            return None;
        }

        let mut state = self.state.lock().expect("response dispatch mutex poisoned");
        if state.complete {
            return None;
        }

        let (ack_tx, ack_rx) = oneshot::channel();
        let previous_ack = state.ack_rx.replace(ack_rx);
        debug_assert!(
            previous_ack.is_none(),
            "a response dispatch can only be routed once"
        );
        Some(ack_tx)
    }

    fn complete(&self) -> Option<oneshot::Receiver<()>> {
        let mut state = self.state.lock().expect("response dispatch mutex poisoned");
        state.complete = true;
        state.ack_rx.take()
    }
}

enum HandlerErrorTarget {
    Request(RequestReplyTarget),
    Response(ResponseReplyTarget),
}

impl HandlerErrorTarget {
    fn begin_handler_attempt(
        &self,
        message_tx: &OutgoingMessageTx,
    ) -> Option<ResponderHandlerAttempt> {
        match self {
            Self::Request(target) => target.destination.begin_handler_attempt(message_tx.clone()),
            Self::Response(_) => None,
        }
    }
}

#[derive(Debug)]
enum OutgoingMessage {
    /// Close the outgoing application queue and acknowledge after every
    /// already-accepted message has entered the raw transport queue.
    CloseAfterDraining { done: oneshot::Sender<()> },

    /// Mark every entry in an incoming batch as dispatched. A completed
    /// response array may only be emitted after this barrier.
    BatchDispatchComplete { completion: BatchDispatchCompletion },

    /// Finish arbitration for a handler attempt that may have dropped a batch
    /// responder immediately before returning an error.
    BatchHandlerAttemptComplete { destination: ResponseDestination },

    /// Record that a claimed batch request dropped its responder without
    /// replying. The fallback remains provisional while its handler attempt is
    /// active so a handler error can supply the authoritative response.
    AbandonedBatchResponse {
        id: RequestId,
        method: String,
        destination: ResponseDestination,
    },

    /// Send a request to the server.
    Request {
        /// id assigned to this request (generated by sender)
        id: RequestId,

        /// the original method
        method: String,

        /// The logical message before peer-direction wrapping.
        untyped: UntypedMessage,

        /// How to transform the logical message for its target peer.
        remote_style: crate::role::RemoteStyle,

        /// Optional prerequisite that must finish before the request becomes
        /// visible on the transport.
        readiness: Option<RequestReadiness>,
    },

    /// Send a notification to the server.
    Notification {
        /// the message to send; this may have a distinct method
        /// depending on the peer
        untyped: UntypedMessage,
    },

    /// Send a response to a message from the server
    Response {
        id: RequestId,

        /// Method of the incoming request this response completes.
        method: String,

        response: Result<serde_json::Value, crate::Error>,

        destination: ResponseDestination,
    },

    /// Send an Error Response that cannot be correlated to a request ID.
    UncorrelatedErrorResponse {
        error: crate::Error,
        destination: ResponseDestination,
    },
}

/// Return type from JrHandler; indicates whether the request was handled or not.
#[must_use]
#[derive(Debug)]
pub enum Handled<T> {
    /// The message was handled
    Yes,

    /// The message was not handled; returns the original value.
    ///
    /// If `retry` is true,
    No {
        /// The message to be passed to subsequent handlers
        /// (typically the original message, but it may have been
        /// mutated.)
        message: T,

        /// If true, request the message to be queued and retried with
        /// dynamic handlers as they are added.
        ///
        /// This is used for managing session updates since the dynamic
        /// handler for a session cannot be added until the response to the
        /// new session request has been processed and there may be updates
        /// that get processed at the same time.
        retry: bool,
    },
}

/// Trait for converting handler return values into [`Handled`].
///
/// This trait allows handlers to return either `()` (which becomes `Handled::Yes`)
/// or an explicit `Handled<T>` value for more control over handler propagation.
pub trait IntoHandled<T> {
    /// Convert this value into a `Handled<T>`.
    fn into_handled(self) -> Handled<T>;
}

impl<T> IntoHandled<T> for () {
    fn into_handled(self) -> Handled<T> {
        Handled::Yes
    }
}

impl<T> IntoHandled<T> for Handled<T> {
    fn into_handled(self) -> Handled<T> {
        self
    }
}

/// A protocol-v2 connection context.
///
/// Values of this type are supplied to callbacks registered on a
/// [`V2Builder`]. It exposes the general connection operations that are valid
/// for protocol v2 while keeping version-specific high-level helpers for other
/// protocol versions out of the typed context. The generic JSON-RPC send
/// methods remain intentionally schema-agnostic.
///
/// This is a thin, cheaply cloneable handle to the underlying JSON-RPC
/// connection. It intentionally does not implement [`Deref`](std::ops::Deref)
/// to [`ConnectionTo`].
#[cfg(feature = "unstable_protocol_v2")]
#[derive(Clone, Debug)]
pub struct V2ConnectionTo<Counterpart: Role> {
    inner: ConnectionTo<Counterpart>,
}

#[cfg(feature = "unstable_protocol_v2")]
impl<Counterpart: Role> V2ConnectionTo<Counterpart> {
    /// Access the underlying version-neutral connection inside the SDK.
    pub(crate) fn raw_connection(&self) -> &ConnectionTo<Counterpart> {
        &self.inner
    }

    /// Return the counterpart role this connection is talking to.
    pub fn counterpart(&self) -> Counterpart {
        self.inner.counterpart()
    }

    /// Wait until the incoming transport reaches clean EOF.
    pub async fn incoming_closed(&self) {
        self.inner.incoming_closed().await;
    }

    /// Return whether clean incoming-EOF processing has completed.
    #[must_use]
    pub fn is_incoming_closed(&self) -> bool {
        self.inner.is_incoming_closed()
    }

    /// Spawn a task that runs for as long as the JSON-RPC connection is served.
    #[track_caller]
    pub fn spawn(
        &self,
        task: impl IntoFuture<Output = Result<(), crate::Error>, IntoFuture: Send + 'static>,
    ) -> Result<(), crate::Error> {
        self.inner.spawn(task)
    }

    /// Spawn a JSON-RPC connection in the background.
    ///
    /// The returned connection context is selected by `builder`; spawning a
    /// [`V2Builder`] therefore returns another [`V2ConnectionTo`].
    ///
    /// ```no_run
    /// # use agent_client_protocol::{
    /// #     Agent, Client, ConnectTo, Error, V2ConnectionTo,
    /// # };
    /// # fn example(
    /// #     connection: V2ConnectionTo<Agent>,
    /// #     transport: impl ConnectTo<Client> + 'static,
    /// # ) -> Result<(), Error> {
    /// let child: V2ConnectionTo<Agent> =
    ///     connection.spawn_connection(Client.v2(), transport)?;
    /// # drop(child);
    /// # Ok(())
    /// # }
    /// ```
    #[track_caller]
    pub fn spawn_connection<R: Role, Context: ConnectionContext>(
        &self,
        builder: Builder<
            R,
            impl HandleDispatchFrom<R::Counterpart> + 'static,
            impl RunWithConnectionTo<R::Counterpart> + 'static,
            impl HandleConnectionClose<R::Counterpart> + 'static,
            Context,
        >,
        transport: impl ConnectTo<R> + 'static,
    ) -> Result<Context::Connection<R::Counterpart>, crate::Error> {
        self.inner.spawn_connection_with_context(builder, transport)
    }

    /// Send a request or notification and forward its response appropriately.
    pub fn send_proxied_message<Req: JsonRpcRequest<Response: Send>, Notif: JsonRpcNotification>(
        &self,
        message: Dispatch<Req, Notif>,
    ) -> Result<(), crate::Error>
    where
        Counterpart: HasPeer<Counterpart>,
    {
        self.inner.send_proxied_message(message)
    }

    /// Send a request or notification to a specific peer and forward its
    /// response appropriately.
    pub fn send_proxied_message_to<
        Peer: Role,
        Req: JsonRpcRequest<Response: Send>,
        Notif: JsonRpcNotification,
    >(
        &self,
        peer: Peer,
        message: Dispatch<Req, Notif>,
    ) -> Result<(), crate::Error>
    where
        Counterpart: HasPeer<Peer>,
    {
        self.inner.send_proxied_message_to(peer, message)
    }

    /// Send an outgoing request to the default counterpart peer.
    pub fn send_request<Req: JsonRpcRequest>(&self, request: Req) -> SentRequest<Req::Response>
    where
        Counterpart: HasPeer<Counterpart>,
    {
        self.inner.send_request(request)
    }

    /// Send an outgoing request to a specific peer.
    pub fn send_request_to<Peer: Role, Req: JsonRpcRequest>(
        &self,
        peer: Peer,
        request: Req,
    ) -> SentRequest<Req::Response>
    where
        Counterpart: HasPeer<Peer>,
    {
        self.inner.send_request_to(peer, request)
    }

    /// Send an outgoing notification to the default counterpart peer.
    pub fn send_notification<N: JsonRpcNotification>(
        &self,
        notification: N,
    ) -> Result<(), crate::Error>
    where
        Counterpart: HasPeer<Counterpart>,
    {
        self.inner.send_notification(notification)
    }

    /// Send an outgoing notification to a specific peer.
    pub fn send_notification_to<Peer: Role, N: JsonRpcNotification>(
        &self,
        peer: Peer,
        notification: N,
    ) -> Result<(), crate::Error>
    where
        Counterpart: HasPeer<Peer>,
    {
        self.inner.send_notification_to(peer, notification)
    }

    /// Send a `$/cancel_request` notification to the default counterpart peer.
    pub fn send_cancel_request(
        &self,
        request_id: impl Into<crate::schema::v1::RequestId>,
    ) -> Result<(), crate::Error>
    where
        Counterpart: HasPeer<Counterpart>,
    {
        self.inner.send_cancel_request(request_id)
    }

    /// Send a `$/cancel_request` notification to a specific peer.
    pub fn send_cancel_request_to<Peer: Role>(
        &self,
        peer: Peer,
        request_id: impl Into<crate::schema::v1::RequestId>,
    ) -> Result<(), crate::Error>
    where
        Counterpart: HasPeer<Peer>,
    {
        self.inner.send_cancel_request_to(peer, request_id)
    }

    /// Register a low-level dynamic message handler.
    ///
    /// Dynamic handlers use the version-neutral [`HandleDispatchFrom`] trait,
    /// so their callback receives the underlying [`ConnectionTo`]. Prefer
    /// typed handlers on [`V2Builder`] when registration can happen before the
    /// connection starts.
    ///
    /// ```no_run
    /// # use agent_client_protocol::{
    /// #     Agent, Client, ConnectTo, DynamicHandlerGuard, Error, NullHandler,
    /// # };
    /// # async fn example(
    /// #     transport: impl ConnectTo<Client> + 'static,
    /// # ) -> Result<(), Error> {
    /// Client.v2().connect_with(transport, async |connection| {
    ///     let guard: DynamicHandlerGuard<Agent> =
    ///         connection.add_dynamic_handler(NullHandler)?;
    ///
    ///     // Keep `guard` alive for as long as the modal handler is needed.
    ///     drop(guard);
    ///     Ok(())
    /// }).await
    /// # }
    /// ```
    pub fn add_dynamic_handler(
        &self,
        handler: impl HandleDispatchFrom<Counterpart> + 'static,
    ) -> Result<DynamicHandlerGuard<Counterpart>, crate::Error> {
        self.inner.add_dynamic_handler(handler)
    }
}

/// Connection context for sending messages and spawning tasks.
///
/// This is the primary handle for interacting with the JSON-RPC connection from
/// within handler callbacks. You can use it to:
///
/// * Send requests and notifications to the other side
/// * Spawn concurrent tasks that run alongside the connection
/// * Respond to requests (via [`Responder`] which wraps this)
///
/// # Cloning
///
/// `ConnectionTo` is cheaply cloneable - all clones refer to the same underlying connection.
/// This makes it easy to share across async tasks.
///
/// # Event Loop and Concurrency
///
/// Handler callbacks run on the event loop, which means the connection cannot process new
/// messages while your handler is running. Use [`spawn`](Self::spawn) to offload any
/// expensive or blocking work to concurrent tasks.
///
/// See the [Event Loop and Concurrency](Builder#event-loop-and-concurrency) section
/// for more details.
#[derive(Clone, Debug)]
pub struct ConnectionTo<Counterpart: Role> {
    counterpart: Counterpart,
    message_tx: OutgoingMessageTx,
    task_tx: TaskTx,
    dynamic_handler_tx: mpsc::UnboundedSender<DynamicHandlerMessage<Counterpart>>,
    transport_completion: SharedTransportCompletion,
    pending_replies: PendingRepliesRegistrar,
    #[cfg_attr(
        not(feature = "unstable_protocol_v2"),
        allow(
            dead_code,
            reason = "retained so ConnectionTo has one constructor shape"
        )
    )]
    protocol_mode: ProtocolMode,
    incoming_closed: IncomingClosed,
}

type SharedTransportCompletion = future::Shared<BoxFuture<'static, Result<(), crate::Error>>>;

#[derive(Clone)]
struct IncomingClosed {
    state: Arc<IncomingClosedState>,
}

struct IncomingClosedState {
    closing: AtomicBool,
    closed: AtomicBool,
    signal_tx: Mutex<Option<oneshot::Sender<()>>>,
    signal_rx: future::Shared<BoxFuture<'static, ()>>,
}

impl IncomingClosed {
    fn new() -> Self {
        let (signal_tx, signal_rx) = oneshot::channel();
        Self {
            state: Arc::new(IncomingClosedState {
                closing: AtomicBool::new(false),
                closed: AtomicBool::new(false),
                signal_tx: Mutex::new(Some(signal_tx)),
                signal_rx: signal_rx.map(|_| ()).boxed().shared(),
            }),
        }
    }

    fn begin_close(&self) {
        self.state.closing.store(true, Ordering::Release);
    }

    fn finish_close(&self) {
        self.state.closed.store(true, Ordering::Release);
        let signal_tx = self
            .state
            .signal_tx
            .lock()
            .expect("incoming-close signal mutex poisoned")
            .take();

        if let Some(signal_tx) = signal_tx {
            let _ = signal_tx.send(());
        }
    }

    async fn closed(&self) {
        self.state.signal_rx.clone().await;
    }

    fn is_closed(&self) -> bool {
        self.state.closed.load(Ordering::Acquire)
    }

    fn is_closing(&self) -> bool {
        self.state.closing.load(Ordering::Acquire)
    }
}

impl Debug for IncomingClosed {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("IncomingClosed")
            .field("is_closing", &self.is_closing())
            .field("is_closed", &self.is_closed())
            .finish_non_exhaustive()
    }
}

/// Stable discriminator stored in the `data.reason` field of errors produced
/// when the incoming transport reaches clean EOF before a request receives its
/// response.
pub const INCOMING_TRANSPORT_CLOSED_REASON: &str = "incoming_transport_closed";

/// Return whether `error` reports that the incoming transport reached clean
/// EOF before a request received its response.
#[must_use]
pub fn is_incoming_transport_closed(error: &crate::Error) -> bool {
    error
        .data
        .as_ref()
        .and_then(|data| data.get("reason"))
        .and_then(serde_json::Value::as_str)
        == Some(INCOMING_TRANSPORT_CLOSED_REASON)
}

fn incoming_transport_closed_error(method: &str) -> crate::Error {
    let mut error = crate::Error::internal_error();
    error.message = "Incoming transport closed".to_string();
    error.data(serde_json::json!({
        "reason": INCOMING_TRANSPORT_CLOSED_REASON,
        "method": method,
    }))
}

/// Run the connection background alongside its foreground while ensuring that
/// a foreground woken by incoming EOF cannot cancel close callbacks midway.
fn run_until_connection_close<R>(
    background: impl Future<Output = Result<(), crate::Error>>,
    foreground: impl Future<Output = Result<R, crate::Error>>,
    incoming_closed: IncomingClosed,
) -> impl Future<Output = Result<R, crate::Error>> {
    // Box these before constructing the returned future. Keeping the generic
    // connection actors directly in this async state would substantially grow
    // every `connect_*` future.
    let background = Box::pin(background);
    let foreground = Box::pin(foreground);

    async move {
        match future::select(background, foreground).await {
            Either::Left((background_result, foreground)) => {
                background_result?;
                foreground.await
            }
            Either::Right((foreground_result, background)) => {
                if !incoming_closed.is_closing() {
                    return foreground_result;
                }

                match future::select(background, Box::pin(incoming_closed.closed())).await {
                    Either::Left((background_result, _)) => {
                        background_result?;
                        foreground_result
                    }
                    Either::Right(((), background)) => {
                        // Poll the background first once more so an error returned
                        // by the just-finished close callback wins over the ready
                        // foreground result.
                        crate::util::run_until(background, future::ready(foreground_result)).await
                    }
                }
            }
        }
    }
}

impl<Counterpart: Role> ConnectionTo<Counterpart> {
    fn new(
        counterpart: Counterpart,
        message_tx: mpsc::UnboundedSender<OutgoingMessage>,
        task_tx: mpsc::UnboundedSender<Task>,
        dynamic_handler_tx: mpsc::UnboundedSender<DynamicHandlerMessage<Counterpart>>,
        transport_completion: SharedTransportCompletion,
        pending_replies: PendingRepliesRegistrar,
        protocol_mode: ProtocolMode,
    ) -> Self {
        Self {
            counterpart,
            message_tx,
            task_tx,
            dynamic_handler_tx,
            transport_completion,
            pending_replies,
            protocol_mode,
            incoming_closed: IncomingClosed::new(),
        }
    }

    #[cfg(feature = "unstable_protocol_v2")]
    pub(crate) fn acp_protocol_version(&self) -> Option<crate::schema::ProtocolVersion> {
        self.protocol_mode.api_protocol_version()
    }

    /// Return the counterpart role this connection is talking to.
    pub fn counterpart(&self) -> Counterpart {
        self.counterpart.clone()
    }

    /// Wait until the incoming transport reaches clean EOF.
    ///
    /// Transport closure means that no more messages or responses can arrive.
    /// Pending requests are failed first; this completes after registered
    /// [`Builder::on_close`] callbacks finish.
    /// It does not automatically cancel the future passed to
    /// [`Builder::connect_with`]; use [`Builder::on_close`] when the connection
    /// should run application-specific cleanup or terminate that future.
    pub async fn incoming_closed(&self) {
        self.incoming_closed.closed().await;
    }

    /// Return whether clean incoming-EOF processing has completed.
    ///
    /// This remains `false` while [`Builder::on_close`] callbacks are running.
    #[must_use]
    pub fn is_incoming_closed(&self) -> bool {
        self.incoming_closed.is_closed()
    }

    /// Stop accepting outgoing messages, drain those already accepted through
    /// the protocol actor, and wait for the transport sink to finish them.
    async fn drain_outgoing(&self) -> Result<(), crate::Error> {
        let (done_tx, done_rx) = oneshot::channel();
        let marker_result = send_raw_message(
            &self.message_tx,
            OutgoingMessage::CloseAfterDraining { done: done_tx },
        );
        let marker_result = match marker_result {
            Ok(()) => done_rx.await.map_err(|error| {
                crate::util::internal_error(format!(
                    "outgoing drain marker was dropped before completion: {error}"
                ))
            }),
            Err(error) => Err(error),
        };

        // The marker only proves that all accepted protocol messages entered
        // the raw transport queue. Transport completion is the sink-level
        // barrier that proves a backpressured writer finished them.
        self.transport_completion.clone().await?;
        marker_result
    }

    fn is_incoming_closing(&self) -> bool {
        self.incoming_closed.is_closing()
    }

    pub(super) fn begin_incoming_close(&self) {
        self.incoming_closed.begin_close();
    }

    pub(super) fn finish_incoming_close(&self) {
        self.incoming_closed.finish_close();
    }

    /// Spawns a task that will run so long as the JSON-RPC connection is being served.
    ///
    /// This is the primary mechanism for offloading expensive work from handler callbacks
    /// to avoid blocking the event loop. Spawned tasks run concurrently with the connection,
    /// allowing the server to continue processing messages.
    ///
    /// # Event Loop
    ///
    /// Handler callbacks run on the event loop, which cannot process new messages while
    /// your handler is running. Use `spawn` for any expensive operations:
    ///
    /// ```no_run
    /// # use agent_client_protocol_test::*;
    /// # async fn example() -> Result<(), agent_client_protocol::Error> {
    /// # let connection = mock_connection();
    /// connection.on_receive_request(async |req: ProcessRequest, responder, cx| {
    ///     // Clone cx for the spawned task
    ///     cx.spawn({
    ///         let connection = cx.clone();
    ///         async move {
    ///             let result = expensive_operation(&req.data).await?;
    ///             connection.send_notification(ProcessComplete { result })?;
    ///             Ok(())
    ///         }
    ///     })?;
    ///
    ///     // Respond immediately
    ///     responder.respond(ProcessResponse { result: "started".into() })
    /// }, agent_client_protocol::on_receive_request!())
    /// # .connect_to(agent_client_protocol_test::MockTransport).await?;
    /// # Ok(())
    /// # }
    /// ```
    ///
    /// # Errors
    ///
    /// If the spawned task returns an error, the entire server will shut down.
    #[track_caller]
    pub fn spawn(
        &self,
        task: impl IntoFuture<Output = Result<(), crate::Error>, IntoFuture: Send + 'static>,
    ) -> Result<(), crate::Error> {
        let location = std::panic::Location::caller();
        let task = task.into_future();
        Task::new(location, task).spawn(&self.task_tx)
    }

    /// Spawn a JSON-RPC connection in the background and return a raw
    /// [`ConnectionTo`] for it.
    ///
    /// This is useful for creating multiple connections that communicate with each other,
    /// such as implementing proxy patterns or connecting to multiple backend services.
    ///
    /// # Arguments
    ///
    /// - `builder`: The connection builder with handlers configured
    /// - `transport`: The transport component to connect to
    ///
    /// # Returns
    ///
    /// The child builder may select any callback context. For example, this
    /// method can spawn a `V2Builder`, whose callbacks receive
    /// `V2ConnectionTo`, while preserving this method's existing raw return
    /// type and single explicit role parameter.
    ///
    /// When a raw parent also needs the builder-selected child handle, use the
    /// protocol-v2 `spawn_connection_with_context` method.
    ///
    /// # Example: Proxying to a backend connection
    ///
    /// ```
    /// # use agent_client_protocol::UntypedRole;
    /// # use agent_client_protocol::{Builder, ConnectionTo};
    /// # use agent_client_protocol_test::*;
    /// # async fn example(cx: ConnectionTo<UntypedRole>) -> Result<(), agent_client_protocol::Error> {
    /// // Set up a backend connection builder
    /// let backend = UntypedRole.builder()
    ///     .on_receive_request(async |req: MyRequest, responder, _cx| {
    ///         responder.respond(MyResponse { status: "ok".into() })
    ///     }, agent_client_protocol::on_receive_request!());
    ///
    /// // Spawn it and get a context to send requests to it
    /// let backend_connection = cx.spawn_connection::<UntypedRole>(backend, MockTransport)?;
    ///
    /// // Now you can forward requests to the backend
    /// let response = backend_connection.send_request(MyRequest {}).block_task().await?;
    /// # Ok(())
    /// # }
    /// ```
    #[track_caller]
    pub fn spawn_connection<R: Role>(
        &self,
        builder: Builder<
            R,
            impl HandleDispatchFrom<R::Counterpart> + 'static,
            impl RunWithConnectionTo<R::Counterpart> + 'static,
            impl HandleConnectionClose<R::Counterpart> + 'static,
            impl ConnectionContext,
        >,
        transport: impl ConnectTo<R> + 'static,
    ) -> Result<ConnectionTo<R::Counterpart>, crate::Error> {
        self.spawn_connection_raw(builder, transport)
    }

    /// Spawn a JSON-RPC connection and return the connection context selected
    /// by its builder.
    ///
    /// This is the low-level counterpart to
    /// [`V2ConnectionTo::spawn_connection`] for code that intentionally works
    /// with a raw [`ConnectionTo`], such as custom [`HandleDispatchFrom`] or
    /// [`RunWithConnectionTo`] implementations. Prefer [`Self::spawn_connection`]
    /// when a raw child handle is sufficient.
    ///
    /// ```no_run
    /// # use agent_client_protocol::{
    /// #     Agent, Client, ConnectTo, ConnectionTo, Error, UntypedRole,
    /// #     V2ConnectionTo,
    /// # };
    /// # fn example(
    /// #     connection: ConnectionTo<UntypedRole>,
    /// #     transport: impl ConnectTo<Client> + 'static,
    /// # ) -> Result<(), Error> {
    /// let child: V2ConnectionTo<Agent> =
    ///     connection.spawn_connection_with_context(Client.v2(), transport)?;
    /// # drop(child);
    /// # Ok(())
    /// # }
    /// ```
    #[cfg(feature = "unstable_protocol_v2")]
    #[track_caller]
    pub fn spawn_connection_with_context<R: Role, Context: ConnectionContext>(
        &self,
        builder: Builder<
            R,
            impl HandleDispatchFrom<R::Counterpart> + 'static,
            impl RunWithConnectionTo<R::Counterpart> + 'static,
            impl HandleConnectionClose<R::Counterpart> + 'static,
            Context,
        >,
        transport: impl ConnectTo<R> + 'static,
    ) -> Result<Context::Connection<R::Counterpart>, crate::Error> {
        let connection = self.spawn_connection_raw(builder, transport)?;
        Ok(connection_context::from_raw::<Context, _>(connection))
    }

    #[track_caller]
    fn spawn_connection_raw<R: Role, Context: ConnectionContext>(
        &self,
        builder: Builder<
            R,
            impl HandleDispatchFrom<R::Counterpart> + 'static,
            impl RunWithConnectionTo<R::Counterpart> + 'static,
            impl HandleConnectionClose<R::Counterpart> + 'static,
            Context,
        >,
        transport: impl ConnectTo<R> + 'static,
    ) -> Result<ConnectionTo<R::Counterpart>, crate::Error> {
        let (connection, future) =
            builder.into_connection_and_future(transport, |_| std::future::pending());
        Task::new(std::panic::Location::caller(), future).spawn(&self.task_tx)?;
        Ok(connection)
    }

    /// Send a request/notification and forward the response appropriately.
    ///
    /// The request context's response type matches the request's response type,
    /// enabling type-safe message forwarding.
    pub fn send_proxied_message<Req: JsonRpcRequest<Response: Send>, Notif: JsonRpcNotification>(
        &self,
        message: Dispatch<Req, Notif>,
    ) -> Result<(), crate::Error>
    where
        Counterpart: HasPeer<Counterpart>,
    {
        self.send_proxied_message_to(self.counterpart(), message)
    }

    /// Send a request/notification and forward the response appropriately.
    ///
    /// The request context's response type matches the request's response type,
    /// enabling type-safe message forwarding.
    ///
    /// `$/cancel_request` notifications are *not* forwarded: their `requestId`
    /// refers to a request on the connection they arrived over and would be
    /// meaningless to `peer`. Cancellation instead propagates hop by hop,
    /// because the responders passed to
    /// [`forward_response_to`](SentRequest::forward_response_to) observe it
    /// and re-issue the cancellation with the forwarded request's own ID.
    pub fn send_proxied_message_to<
        Peer: Role,
        Req: JsonRpcRequest<Response: Send>,
        Notif: JsonRpcNotification,
    >(
        &self,
        peer: Peer,
        message: Dispatch<Req, Notif>,
    ) -> Result<(), crate::Error>
    where
        Counterpart: HasPeer<Peer>,
    {
        match message {
            Dispatch::Request(request, responder) => self
                .send_ordered_request_to(peer, request)
                .forward_response_to(responder),
            Dispatch::Notification(notification) => {
                // `$/cancel_request` is connection-scoped: its `requestId` was
                // allocated on the connection the notification arrived over
                // and means nothing to `peer`. The cancellation has already
                // been recorded on this connection's responder markers, and
                // `forward_response_to` re-issues it for the forwarded request
                // with the correct per-hop ID, so drop the raw notification
                // instead of tunneling a meaningless ID across the hop.
                if is_cancel_request_notification(&notification) {
                    tracing::debug!(
                        "not forwarding hop-scoped `$/cancel_request` notification across proxy hop"
                    );
                    return Ok(());
                }
                self.send_notification_to(peer, notification)
            }
            Dispatch::Response(result, router) => {
                // Responses are forwarded directly to their destination
                router.route_with_result(result)
            }
        }
    }

    /// Send an outgoing request and return a [`SentRequest`] for handling the reply.
    ///
    /// The returned [`SentRequest`] makes the response-consumption mode explicit:
    ///
    /// * [`on_receiving_result`](SentRequest::on_receiving_result) - Register a callback and
    ///   return immediately. If registered before the response is routed during its original
    ///   dispatch, the loop waits for the callback to complete.
    /// * [`block_task`](SentRequest::block_task) - Wait on the current task until the response
    ///   arrives. This is only safe when that task already runs outside the dispatch loop.
    ///
    /// # Anti-Footgun Design
    ///
    /// The API intentionally makes it difficult to block on the result directly to prevent
    /// the common mistake of blocking the event loop while waiting for a response:
    ///
    /// ```compile_fail
    /// # use agent_client_protocol_test::*;
    /// # async fn example(cx: agent_client_protocol::ConnectionTo<agent_client_protocol::UntypedRole>) -> Result<(), agent_client_protocol::Error> {
    /// // ❌ This doesn't compile - prevents blocking the event loop
    /// let response = cx.send_request(MyRequest {}).await?;
    /// # Ok(())
    /// # }
    /// ```
    ///
    /// ```no_run
    /// # use agent_client_protocol_test::*;
    /// # async fn example(cx: agent_client_protocol::ConnectionTo<agent_client_protocol::UntypedRole>) -> Result<(), agent_client_protocol::Error> {
    /// // ✅ Option 1: Register an ordered callback (safe in handlers)
    /// cx.send_request(MyRequest {})
    ///     .on_receiving_result(async |result| {
    ///         // Handle the response
    ///         Ok(())
    ///     })?;
    ///
    /// // ✅ Option 2: Block in spawned task (safe because task is concurrent)
    /// cx.spawn({
    ///     let cx = cx.clone();
    ///     async move {
    ///         let response = cx.send_request(MyRequest {})
    ///             .block_task()
    ///             .await?;
    ///         // Process response...
    ///         Ok(())
    ///     }
    /// })?;
    /// # Ok(())
    /// # }
    /// ```
    /// Send an outgoing request to the default counterpart peer.
    ///
    /// This is a convenience method that sends to the counterpart role `R`.
    /// For explicit control over the target peer, use [`send_request_to`](Self::send_request_to).
    pub fn send_request<Req: JsonRpcRequest>(&self, request: Req) -> SentRequest<Req::Response>
    where
        Counterpart: HasPeer<Counterpart>,
    {
        self.send_request_to(self.counterpart.clone(), request)
    }

    /// Send an outgoing request to a specific peer.
    ///
    /// The message will be transformed according to the [`HasPeer`](crate::role::HasPeer)
    /// implementation before being sent.
    pub fn send_request_to<Peer: Role, Req: JsonRpcRequest>(
        &self,
        peer: Peer,
        request: Req,
    ) -> SentRequest<Req::Response>
    where
        Counterpart: HasPeer<Peer>,
    {
        self.send_request_to_with_options(peer, request, false, None, None)
    }

    /// Send a request and run a synchronous side effect when its valid success
    /// response is routed, after `before_send` completes and independently
    /// from how the returned request is eventually consumed.
    #[cfg(feature = "unstable_protocol_v2")]
    pub(crate) fn send_request_to_with_response_hook_after<
        Peer: Role,
        Req: JsonRpcRequest,
        BeforeSend: Future<Output = Result<(), crate::Error>> + Send + 'static,
    >(
        &self,
        peer: Peer,
        request: Req,
        before_send: BeforeSend,
        response_hook: impl FnOnce(&Req::Response) -> Result<(), crate::Error> + Send + 'static,
    ) -> SentRequest<Req::Response>
    where
        Counterpart: HasPeer<Peer>,
    {
        let hook: ResponseRouteHook = Box::new(move |method, value| {
            let response = Req::Response::from_value(method, value.clone())?;
            response_hook(&response)
        });
        self.send_request_to_with_options(
            peer,
            request,
            false,
            Some(RequestReadiness::new(before_send)),
            Some(hook),
        )
    }

    /// Send an ordered request with readiness and valid-success hooks.
    #[cfg(feature = "unstable_protocol_v2")]
    pub(crate) fn send_ordered_request_to_with_response_hook_after<
        Peer: Role,
        Req: JsonRpcRequest,
        BeforeSend: Future<Output = Result<(), crate::Error>> + Send + 'static,
    >(
        &self,
        peer: Peer,
        request: Req,
        before_send: BeforeSend,
        response_hook: impl FnOnce(&Req::Response) -> Result<(), crate::Error> + Send + 'static,
    ) -> SentRequest<Req::Response>
    where
        Counterpart: HasPeer<Peer>,
    {
        let hook: ResponseRouteHook = Box::new(move |method, value| {
            let response = Req::Response::from_value(method, value.clone())?;
            response_hook(&response)
        });
        self.send_request_to_with_options(
            peer,
            request,
            true,
            Some(RequestReadiness::new(before_send)),
            Some(hook),
        )
    }

    /// Send a request whose callback must run before later inbound messages.
    ///
    /// The ordering marker is installed before the request enters the outgoing
    /// queue, closing the race between a fast peer response and the immediate
    /// [`SentRequest::on_receiving_result`] call. Callers must consume the
    /// returned request with a callback-style method without yielding.
    pub(crate) fn send_ordered_request_to<Peer: Role, Req: JsonRpcRequest>(
        &self,
        peer: Peer,
        request: Req,
    ) -> SentRequest<Req::Response>
    where
        Counterpart: HasPeer<Peer>,
    {
        self.send_request_to_with_options(peer, request, true, None, None)
    }

    /// Send an ordered request after `before_send` completes successfully.
    ///
    /// The ordering marker and readiness prerequisite are both registered
    /// before the request enters the outgoing queue. This is used by framework
    /// setup paths that must acknowledge local routing before the peer can
    /// observe the request.
    pub(crate) fn send_ordered_request_to_after<
        Peer: Role,
        Req: JsonRpcRequest,
        BeforeSend: Future<Output = Result<(), crate::Error>> + Send + 'static,
    >(
        &self,
        peer: Peer,
        request: Req,
        before_send: BeforeSend,
    ) -> SentRequest<Req::Response>
    where
        Counterpart: HasPeer<Peer>,
    {
        self.send_request_to_with_options(
            peer,
            request,
            true,
            Some(RequestReadiness::new(before_send)),
            None,
        )
    }

    fn send_request_to_with_options<Peer: Role, Req: JsonRpcRequest>(
        &self,
        peer: Peer,
        request: Req,
        ordered: bool,
        readiness: Option<RequestReadiness>,
        response_route_hook: Option<ResponseRouteHook>,
    ) -> SentRequest<Req::Response>
    where
        Counterpart: HasPeer<Peer>,
    {
        let method = request.method().to_string();
        let id = RequestId::Str(uuid::Uuid::new_v4().to_string());
        let (response_tx, response_rx) = oneshot::channel();
        let response_ordering = ResponseOrdering::default();
        if ordered {
            response_ordering.mark_ordered();
        }
        let role_id = peer.role_id();
        let remote_style = self.counterpart.remote_style(peer);
        let cancellation =
            SentRequestCancellation::new(self.message_tx.clone(), remote_style, id.clone());
        if self.is_incoming_closing() {
            cancellation.disarm();
            drop(response_tx.send(ResponsePayload {
                result: Err(incoming_transport_closed_error(&method)),
                ack_tx: None,
            }));
            return SentRequest::new(
                id,
                method.clone(),
                self.task_tx.clone(),
                response_rx,
                cancellation,
                response_ordering,
            )
            .map(move |json| <Req::Response>::from_value(&method, json));
        }

        match request.to_untyped_message() {
            Ok(untyped) => {
                // Register before enqueueing so incoming EOF can fail every
                // observable request before close callbacks begin. The
                // outgoing actor checks that the registration still exists
                // before sending the request.
                let pending_reply = PendingReply {
                    method: method.clone(),
                    role_id,
                    sender: response_tx,
                    cancellation_disarm: cancellation.disarm_handle(),
                    ordering: response_ordering.clone(),
                    response_route_hook,
                };

                if self
                    .pending_replies
                    .subscribe(id.clone(), pending_reply, &self.incoming_closed)
                {
                    let message = OutgoingMessage::Request {
                        id: id.clone(),
                        method: method.clone(),
                        untyped,
                        remote_style,
                        readiness,
                    };

                    if let Err(error) = self.message_tx.unbounded_send(message) {
                        cancellation.disarm();

                        let OutgoingMessage::Request { id, method, .. } = error.into_inner() else {
                            unreachable!();
                        };

                        if let Some(pending_reply) = self.pending_replies.remove(&id) {
                            if self.is_incoming_closing() {
                                pending_reply.fail_incoming_closed();
                            } else {
                                pending_reply.fail(crate::util::internal_error(format!(
                                    "failed to send outgoing request `{method}`"
                                )));
                            }
                        }
                    }
                }
            }

            Err(err) => {
                cancellation.disarm();

                response_tx
                    .send(ResponsePayload {
                        result: Err(crate::util::internal_error(format!(
                            "failed to create untyped request for `{method}`: {err}"
                        ))),
                        ack_tx: None,
                    })
                    .unwrap();
            }
        }

        SentRequest::new(
            id,
            method.clone(),
            self.task_tx.clone(),
            response_rx,
            cancellation,
            response_ordering,
        )
        .map(move |json| <Req::Response>::from_value(&method, json))
    }

    /// Send an outgoing notification to the default counterpart peer (no reply expected).
    ///
    /// Notifications are fire-and-forget messages that don't have IDs and don't expect responses.
    /// This method sends the notification immediately and returns.
    ///
    /// This is a convenience method that sends to the counterpart role `R`.
    /// For explicit control over the target peer, use [`send_notification_to`](Self::send_notification_to).
    ///
    /// ```no_run
    /// # use agent_client_protocol_test::*;
    /// # async fn example(cx: agent_client_protocol::ConnectionTo<agent_client_protocol::Agent>) -> Result<(), agent_client_protocol::Error> {
    /// cx.send_notification(StatusUpdate {
    ///     message: "Processing...".into(),
    /// })?;
    /// # Ok(())
    /// # }
    /// ```
    pub fn send_notification<N: JsonRpcNotification>(
        &self,
        notification: N,
    ) -> Result<(), crate::Error>
    where
        Counterpart: HasPeer<Counterpart>,
    {
        self.send_notification_to(self.counterpart.clone(), notification)
    }

    /// Send an outgoing notification to a specific peer (no reply expected).
    ///
    /// The message will be transformed according to the [`HasPeer`](crate::role::HasPeer)
    /// implementation before being sent.
    pub fn send_notification_to<Peer: Role, N: JsonRpcNotification>(
        &self,
        peer: Peer,
        notification: N,
    ) -> Result<(), crate::Error>
    where
        Counterpart: HasPeer<Peer>,
    {
        let remote_style = self.counterpart.remote_style(peer);
        tracing::debug!(
            role = std::any::type_name::<Counterpart>(),
            peer = std::any::type_name::<Peer>(),
            notification_type = std::any::type_name::<N>(),
            ?remote_style,
            original_method = notification.method(),
            "send_notification_to"
        );
        let transformed = remote_style.transform_outgoing_message(notification)?;
        tracing::debug!(
            transformed_method = %transformed.method,
            "send_notification_to transformed"
        );
        send_raw_message(
            &self.message_tx,
            OutgoingMessage::Notification {
                untyped: transformed,
            },
        )
    }

    /// Send a `$/cancel_request` notification for an arbitrary request ID to
    /// the default counterpart peer.
    ///
    /// Prefer [`SentRequest::cancel`] when you have the request handle: it
    /// already knows the correct peer, request ID, and proxy wrapping. Use this
    /// low-level method only when implementing custom routing with a request ID
    /// that is valid on this connection.
    pub fn send_cancel_request(
        &self,
        request_id: impl Into<crate::schema::v1::RequestId>,
    ) -> Result<(), crate::Error>
    where
        Counterpart: HasPeer<Counterpart>,
    {
        self.send_cancel_request_to(self.counterpart.clone(), request_id)
    }

    /// Send a `$/cancel_request` notification for an arbitrary request ID to a
    /// specific peer.
    ///
    /// Prefer [`SentRequest::cancel`] when you have the request handle: it
    /// already knows the correct peer, request ID, and proxy wrapping. Use this
    /// low-level method only when implementing custom routing with a request ID
    /// that is valid on the target peer's connection.
    pub fn send_cancel_request_to<Peer: Role>(
        &self,
        peer: Peer,
        request_id: impl Into<crate::schema::v1::RequestId>,
    ) -> Result<(), crate::Error>
    where
        Counterpart: HasPeer<Peer>,
    {
        self.send_notification_to(
            peer,
            crate::schema::v1::CancelRequestNotification::new(request_id),
        )
    }

    /// Register a dynamic message handler, used to intercept messages specific to a particular session
    /// or some similar modal thing.
    ///
    /// Dynamic message handlers run after the handlers registered on [`Builder`] and before the
    /// role's default handler. They receive messages that the builder handlers decline.
    ///
    /// The handler will stay registered until the returned registration guard is dropped.
    pub fn add_dynamic_handler(
        &self,
        handler: impl HandleDispatchFrom<Counterpart> + 'static,
    ) -> Result<DynamicHandlerGuard<Counterpart>, crate::Error> {
        let uuid = Uuid::new_v4();
        let active = Arc::new(AtomicBool::new(true));
        self.dynamic_handler_tx
            .unbounded_send(DynamicHandlerMessage::AddDynamicHandler(
                uuid,
                Box::new(GuardedDynamicHandler {
                    active: active.clone(),
                    handler,
                }),
            ))
            .map_err(crate::util::internal_error)?;

        Ok(DynamicHandlerGuard::new(uuid, active, self.clone()))
    }

    /// Wait until every dynamic-handler update queued before this call has
    /// been applied by the incoming protocol actor.
    pub(crate) fn dynamic_handler_barrier(&self) -> BoxFuture<'static, Result<(), crate::Error>> {
        let (acknowledgment_tx, acknowledgment_rx) = oneshot::channel();
        if let Err(error) =
            self.dynamic_handler_tx
                .unbounded_send(DynamicHandlerMessage::AcknowledgedBarrier(
                    acknowledgment_tx,
                ))
        {
            return future::ready(Err(crate::Error::into_internal_error(error))).boxed();
        }

        async move {
            acknowledgment_rx.await.map_err(|error| {
                crate::util::internal_error(format!(
                    "dynamic-handler barrier was dropped before acknowledgment: {error}"
                ))
            })
        }
        .boxed()
    }

    fn remove_dynamic_handler(&self, uuid: Uuid) {
        // Ignore errors
        drop(
            self.dynamic_handler_tx
                .unbounded_send(DynamicHandlerMessage::RemoveDynamicHandler(uuid)),
        );
    }
}

struct GuardedDynamicHandler<Handler> {
    active: Arc<AtomicBool>,
    handler: Handler,
}

impl<Counterpart, Handler> HandleDispatchFrom<Counterpart> for GuardedDynamicHandler<Handler>
where
    Counterpart: Role,
    Handler: HandleDispatchFrom<Counterpart>,
{
    async fn handle_dispatch_from(
        &mut self,
        message: Dispatch,
        connection: ConnectionTo<Counterpart>,
    ) -> Result<Handled<Dispatch>, crate::Error> {
        if !self.active.load(Ordering::Acquire) {
            return Ok(Handled::No {
                message,
                retry: false,
            });
        }
        self.handler.handle_dispatch_from(message, connection).await
    }

    fn describe_chain(&self) -> impl Debug {
        self.handler.describe_chain()
    }
}

/// A guard that keeps a dynamic message handler registered.
///
/// Dropping the guard immediately deactivates the handler and queues its
/// removal from the connection. Use [`detach`](Self::detach) to keep the
/// handler registered for the remaining lifetime of the connection.
#[must_use = "dropping this guard unregisters the dynamic handler"]
#[derive(Debug)]
pub struct DynamicHandlerGuard<R: Role> {
    uuid: Option<Uuid>,
    active: Arc<AtomicBool>,
    cx: ConnectionTo<R>,
}

impl<R: Role> DynamicHandlerGuard<R> {
    fn new(uuid: Uuid, active: Arc<AtomicBool>, cx: ConnectionTo<R>) -> Self {
        Self {
            uuid: Some(uuid),
            active,
            cx,
        }
    }

    /// Keep the dynamic handler registered after this guard is dropped.
    ///
    /// The handler remains registered until the connection itself shuts down.
    /// Unlike leaking the guard, detaching does not retain an extra
    /// [`ConnectionTo`] handle.
    pub fn detach(mut self) {
        self.uuid = None;
    }
}

impl<R: Role> Drop for DynamicHandlerGuard<R> {
    fn drop(&mut self) {
        if let Some(uuid) = self.uuid {
            self.active.store(false, Ordering::Release);
            self.cx.remove_dynamic_handler(uuid);
        }
    }
}

/// The context to respond to an incoming request.
///
/// This context is provided to request handlers and serves a dual role:
///
/// 1. **Respond to the request** - Use [`respond`](Self::respond) or
///    [`respond_with_result`](Self::respond_with_result) to send the response
/// 2. **Send other messages** - Use the [`ConnectionTo`] parameter passed to your
///    handler, which provides [`send_request`](`ConnectionTo::send_request`),
///    [`send_notification`](`ConnectionTo::send_notification`), and
///    [`spawn`](`ConnectionTo::spawn`)
///
/// # Example
///
/// ```no_run
/// # use agent_client_protocol_test::*;
/// # async fn example() -> Result<(), agent_client_protocol::Error> {
/// # let connection = mock_connection();
/// connection.on_receive_request(async |req: ProcessRequest, responder, cx| {
///     // Send a notification while processing
///     cx.send_notification(StatusUpdate {
///         message: "processing".into(),
///     })?;
///
///     // Do some work...
///     let result = process(&req.data)?;
///
///     // Respond to the request
///     responder.respond(ProcessResponse { result })
/// }, agent_client_protocol::on_receive_request!())
/// # .connect_to(agent_client_protocol_test::MockTransport).await?;
/// # Ok(())
/// # }
/// ```
///
/// # Event Loop Considerations
///
/// Like all handlers, request handlers run on the event loop. Use
/// [`spawn`](ConnectionTo::spawn) for expensive operations to avoid blocking
/// the connection.
///
/// See the [Event Loop and Concurrency](Builder#event-loop-and-concurrency)
/// section for more details.
///
/// # Drop behavior
///
/// Dropping a responder for a request that arrived in a batch completes that
/// slot with an Internal Error, so one abandoned request cannot withhold valid
/// sibling responses forever. A responder for an individual request retains
/// the historical behavior: dropping it does not automatically send a reply.
#[must_use]
pub struct Responder<T: JsonRpcResponse = serde_json::Value> {
    /// The method of the request.
    method: String,

    /// The `id` of the message we are replying to.
    id: RequestId,

    /// Request-local cancellation state.
    cancellation: ResponderCancellation,

    /// Whether this response is emitted on its own or collected into a batch.
    destination: ResponseDestination,

    /// Function to send the response to its destination.
    ///
    /// For incoming requests: serializes to JSON and sends over the wire.
    /// For incoming responses: sends to the waiting oneshot channel.
    send_fn: Box<dyn FnOnce(Result<T, crate::Error>) -> Result<(), crate::Error> + Send>,

    /// Completes an abandoned batch slot unless an explicit response disarms it.
    drop_guard: ResponderDropGuard,
}

struct ResponderDropGuard {
    message_tx: OutgoingMessageTx,
    id: RequestId,
    method: String,
    destination: ResponseDestination,
    armed: bool,
}

impl ResponderDropGuard {
    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for ResponderDropGuard {
    fn drop(&mut self) {
        if !self.armed || !self.destination.is_batch() {
            return;
        }

        if let Err(error) = send_raw_message(
            &self.message_tx,
            OutgoingMessage::AbandonedBatchResponse {
                id: self.id.clone(),
                method: self.method.clone(),
                destination: self.destination.clone(),
            },
        ) {
            tracing::debug!(
                method = %self.method,
                error_code = ?&error.code,
                "could not complete abandoned JSON-RPC batch response slot"
            );
        }
    }
}

impl<T: JsonRpcResponse> std::fmt::Debug for Responder<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Responder")
            .field("method", &self.method)
            .field("id", &self.id)
            .field("response_type", &std::any::type_name::<T>())
            .finish_non_exhaustive()
    }
}

impl Responder<serde_json::Value> {
    /// Create a new request context for an incoming request.
    ///
    /// The response will be serialized to JSON and sent over the wire.
    fn new(
        message_tx: OutgoingMessageTx,
        method: String,
        id: RequestId,
        cancellation_registry: &RequestCancellationRegistry,
        destination: ResponseDestination,
    ) -> Self {
        let id_clone = id.clone();
        let method_clone = method.clone();
        let cancellation = cancellation_registry.register(&id);
        let send_destination = destination.clone();
        let drop_guard = ResponderDropGuard {
            message_tx: message_tx.clone(),
            id: id.clone(),
            method: method.clone(),
            destination: destination.clone(),
            armed: true,
        };
        Self {
            method,
            id,
            cancellation,
            destination,
            send_fn: Box::new(move |response: Result<serde_json::Value, crate::Error>| {
                send_raw_message(
                    &message_tx,
                    OutgoingMessage::Response {
                        id: id_clone,
                        method: method_clone,
                        response,
                        destination: send_destination,
                    },
                )
            }),
            drop_guard,
        }
    }

    /// Cast this request context to a different response type.
    ///
    /// The provided type `T` will be serialized to JSON before sending.
    pub fn cast<T: JsonRpcResponse>(self) -> Responder<T> {
        self.wrap_params(move |method, value| match value {
            Ok(value) => T::into_json(value, method),
            Err(e) => Err(e),
        })
    }
}

impl<T: JsonRpcResponse> Responder<T> {
    /// Method of the incoming request
    #[must_use]
    pub fn method(&self) -> &str {
        &self.method
    }

    /// ID of the incoming request.
    #[must_use]
    pub fn id(&self) -> &RequestId {
        &self.id
    }

    /// Returns the cancellation marker for this request.
    ///
    /// The marker is set when the peer sends `$/cancel_request` for this
    /// request's JSON-RPC ID. Cancellation is cooperative: handlers should use
    /// the marker to stop long-running work and then decide whether to respond
    /// with [`Error::request_cancelled`] or partial data.
    ///
    /// [`Error::request_cancelled`]: crate::Error::request_cancelled
    #[must_use]
    pub fn cancellation(&self) -> RequestCancellation {
        self.cancellation.cancellation()
    }

    /// Convert to a `Responder` that expects a JSON value
    /// and which checks (dynamically) that the JSON value it receives
    /// can be converted to `T`.
    pub fn erase_to_json(self) -> Responder<serde_json::Value> {
        self.wrap_params(|method, value| T::from_value(method, value?))
    }

    /// Return a new Responder with a different method name.
    pub fn wrap_method(mut self, method: String) -> Responder<T> {
        self.drop_guard.method.clone_from(&method);
        Responder {
            method,
            id: self.id,
            cancellation: self.cancellation,
            destination: self.destination,
            send_fn: self.send_fn,
            drop_guard: self.drop_guard,
        }
    }

    /// Return a new Responder that expects a response of type U.
    ///
    /// `wrap_fn` will be invoked with the method name and the result to transform
    /// type `U` into type `T` before sending.
    pub fn wrap_params<U: JsonRpcResponse>(
        self,
        wrap_fn: impl FnOnce(&str, Result<U, crate::Error>) -> Result<T, crate::Error> + Send + 'static,
    ) -> Responder<U> {
        let method = self.method.clone();
        Responder {
            method: self.method,
            id: self.id,
            cancellation: self.cancellation,
            destination: self.destination,
            send_fn: Box::new(move |input: Result<U, crate::Error>| {
                let t_value = wrap_fn(&method, input);
                (self.send_fn)(t_value)
            }),
            drop_guard: self.drop_guard,
        }
    }

    /// Respond to the JSON-RPC request with either a value (`Ok`) or an error (`Err`).
    pub fn respond_with_result(
        mut self,
        response: Result<T, crate::Error>,
    ) -> Result<(), crate::Error> {
        tracing::debug!(method = %self.method, "respond called");
        self.drop_guard.disarm();
        (self.send_fn)(response)
    }

    /// Respond to the JSON-RPC request with a value.
    pub fn respond(self, response: T) -> Result<(), crate::Error> {
        self.respond_with_result(Ok(response))
    }

    /// Respond to the JSON-RPC request with an internal error containing a message.
    pub fn respond_with_internal_error(self, message: impl ToString) -> Result<(), crate::Error> {
        self.respond_with_error(crate::util::internal_error(message))
    }

    /// Respond to the JSON-RPC request with an error.
    pub fn respond_with_error(self, error: crate::Error) -> Result<(), crate::Error> {
        tracing::debug!(method = %self.method, error_code = ?&error.code, "respond_with_error called");
        self.respond_with_result(Err(error))
    }

    fn reply_target(&self) -> RequestReplyTarget {
        RequestReplyTarget {
            id: self.id.clone(),
            method: self.method.clone(),
            destination: self.destination.clone(),
        }
    }
}

/// Context for handling an incoming JSON-RPC response.
///
/// This is the response-side counterpart to [`Responder`]. While `Responder` handles
/// incoming requests (where you send a response over the wire), `ResponseRouter` handles
/// incoming responses (where you route the response to a local task waiting for it).
///
/// Both are fundamentally "sinks" that push the message through a `send_fn`, but they
/// represent different points in the message lifecycle and carry different metadata.
///
/// # Drop Behavior
///
/// Dropping a `ResponseRouter` without routing the response (for example, from a
/// dispatch handler that claims a [`Dispatch::Response`]) discards the
/// response: the local awaiter observes the response as never received. The
/// request still counts as settled: routing a response this far disarms the
/// originating [`SentRequest`]'s drop-time auto-cancellation even if the router
/// is never invoked, since the peer has already answered.
#[must_use]
pub struct ResponseRouter<T: JsonRpcResponse = serde_json::Value> {
    /// The method of the original request.
    method: String,

    /// The `id` of the original request.
    id: RequestId,

    /// The RoleId to which the original request was sent
    /// (and hence from which the reply is expected).
    role_id: RoleId,

    /// Function to send the response to the waiting task.
    send_fn: Box<dyn FnOnce(Result<T, crate::Error>) -> Result<(), crate::Error> + Send>,

    /// Shared route used to deliver a dispatch-handler error to the same waiter.
    reply_target: ResponseReplyTarget,
}

impl<T: JsonRpcResponse> std::fmt::Debug for ResponseRouter<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ResponseRouter")
            .field("method", &self.method)
            .field("id", &self.id)
            .field("response_type", &std::any::type_name::<T>())
            .finish_non_exhaustive()
    }
}

impl ResponseRouter<serde_json::Value> {
    /// Create a new response context for routing a response to a local awaiter.
    ///
    /// When [`route_with_result`](Self::route_with_result) is called, the response is sent through the oneshot
    /// channel to the code that originally sent the request. If that receiver was
    /// dropped, the response is discarded because there is no local awaiter left.
    fn new(id: RequestId, pending_reply: PendingReply, dispatch: ResponseDispatch) -> Self {
        let PendingReply {
            method,
            role_id,
            sender,
            cancellation_disarm,
            ordering,
            response_route_hook,
        } = pending_reply;
        let reply_target = ResponseReplyTarget {
            id: id.clone(),
            method: method.clone(),
            sender: Arc::new(Mutex::new(Some(sender))),
            ordering,
            dispatch,
        };
        let send_target = reply_target.clone();
        // A response for the request reached this router, so the request is
        // settled from the peer's perspective and a `$/cancel_request` could
        // only ever be redundant. Disarm immediately so handlers may retain
        // the router without leaving auto-cancellation armed.
        cancellation_disarm.disarm();
        let hook_method = method.clone();
        Self {
            method,
            id,
            role_id,
            send_fn: Box::new(move |response: Result<serde_json::Value, crate::Error>| {
                let response = match response {
                    Ok(value) => match response_route_hook {
                        Some(hook) => hook(&hook_method, &value).map(|()| value),
                        None => Ok(value),
                    },
                    Err(error) => Err(error),
                };
                send_target.route(response);
                Ok(())
            }),
            reply_target,
        }
    }

    /// Cast this response context to a different response type.
    ///
    /// The provided type `T` will be serialized to JSON before sending.
    pub fn cast<T: JsonRpcResponse>(self) -> ResponseRouter<T> {
        self.wrap_params(move |method, value| match value {
            Ok(value) => T::into_json(value, method),
            Err(e) => Err(e),
        })
    }
}

impl<T: JsonRpcResponse> ResponseRouter<T> {
    /// Method of the original request
    #[must_use]
    pub fn method(&self) -> &str {
        &self.method
    }

    /// ID of the original request.
    #[must_use]
    pub fn id(&self) -> &RequestId {
        &self.id
    }

    /// The peer to which the original request was sent.
    ///
    /// This is the peer from which we expect to receive the response.
    #[must_use]
    pub fn role_id(&self) -> RoleId {
        self.role_id.clone()
    }

    /// Convert to a `ResponseRouter` that expects a JSON value
    /// and which checks (dynamically) that the JSON value it receives
    /// can be converted to `T`.
    pub fn erase_to_json(self) -> ResponseRouter<serde_json::Value> {
        self.wrap_params(|method, value| T::from_value(method, value?))
    }

    /// Return a new ResponseRouter that expects a response of type U.
    ///
    /// `wrap_fn` will be invoked with the method name and the result to transform
    /// type `U` into type `T` before sending.
    fn wrap_params<U: JsonRpcResponse>(
        self,
        wrap_fn: impl FnOnce(&str, Result<U, crate::Error>) -> Result<T, crate::Error> + Send + 'static,
    ) -> ResponseRouter<U> {
        let method = self.method.clone();
        ResponseRouter {
            method: self.method,
            id: self.id,
            role_id: self.role_id,
            send_fn: Box::new(move |input: Result<U, crate::Error>| {
                let t_value = wrap_fn(&method, input);
                (self.send_fn)(t_value)
            }),
            reply_target: self.reply_target,
        }
    }

    /// Route the response result to the waiting task.
    pub fn route_with_result(self, response: Result<T, crate::Error>) -> Result<(), crate::Error> {
        tracing::debug!(id = ?self.id, "response routed to awaiter");
        (self.send_fn)(response)
    }

    /// Route a successful response value to the waiting task.
    pub fn route(self, response: T) -> Result<(), crate::Error> {
        self.route_with_result(Ok(response))
    }

    /// Route an internal error to the waiting task.
    pub fn route_with_internal_error(self, message: impl ToString) -> Result<(), crate::Error> {
        self.route_with_error(crate::util::internal_error(message))
    }

    /// Route an error response to the waiting task.
    pub fn route_with_error(self, error: crate::Error) -> Result<(), crate::Error> {
        tracing::debug!(id = ?self.id, error_code = ?&error.code, "error routed to awaiter");
        self.route_with_result(Err(error))
    }
}

/// Common bounds for any JSON-RPC message.
///
/// # Derive Macro
///
/// For simple message types, you can use the `JsonRpcRequest` or `JsonRpcNotification` derive macros
/// which will implement both `JsonRpcMessage` and the respective trait. See [`JsonRpcRequest`] and
/// [`JsonRpcNotification`] for examples.
pub trait JsonRpcMessage: 'static + Debug + Sized + Send + Clone {
    /// Check if this message type matches the given method name.
    fn matches_method(method: &str) -> bool;

    /// The method name for the message.
    fn method(&self) -> &str;

    /// Convert this message into an untyped message.
    fn to_untyped_message(&self) -> Result<UntypedMessage, crate::Error>;

    /// Parse this type from a method name and parameters.
    ///
    /// Returns an error if the method doesn't match or deserialization fails.
    /// Callers should use `matches_method` first to check if this type handles the method.
    fn parse_message(method: &str, params: &impl Serialize) -> Result<Self, crate::Error>;
}

/// Defines the "payload" of a successful response to a JSON-RPC request.
///
/// # Derive Macro
///
/// Use `#[derive(JsonRpcResponse)]` to automatically implement this trait:
///
/// ```ignore
/// use agent_client_protocol::JsonRpcResponse;
/// use serde::{Serialize, Deserialize};
///
/// #[derive(Debug, Clone, Serialize, Deserialize, JsonRpcResponse)]
/// struct HelloResponse {
///     greeting: String,
/// }
/// ```
pub trait JsonRpcResponse: 'static + Debug + Sized + Send + Clone {
    /// Convert this message into a JSON value.
    fn into_json(self, method: &str) -> Result<serde_json::Value, crate::Error>;

    /// Parse a JSON value into the response type.
    fn from_value(method: &str, value: serde_json::Value) -> Result<Self, crate::Error>;
}

impl JsonRpcResponse for serde_json::Value {
    fn from_value(_method: &str, value: serde_json::Value) -> Result<Self, crate::Error> {
        Ok(value)
    }

    fn into_json(self, _method: &str) -> Result<serde_json::Value, crate::Error> {
        Ok(self)
    }
}

/// A struct that represents a notification (JSON-RPC message that does not expect a response).
///
/// # Derive Macro
///
/// Use `#[derive(JsonRpcNotification)]` to automatically implement both `JsonRpcMessage` and `JsonRpcNotification`:
///
/// ```ignore
/// use agent_client_protocol::JsonRpcNotification;
/// use serde::{Serialize, Deserialize};
///
/// #[derive(Debug, Clone, Serialize, Deserialize, JsonRpcNotification)]
/// #[notification(method = "_ping")]
/// struct PingNotification {
///     timestamp: u64,
/// }
/// ```
pub trait JsonRpcNotification: JsonRpcMessage {}

/// A struct that represents a request (JSON-RPC message expecting a response).
///
/// # Derive Macro
///
/// Use `#[derive(JsonRpcRequest)]` to automatically implement both `JsonRpcMessage` and `JsonRpcRequest`:
///
/// ```ignore
/// use agent_client_protocol::{JsonRpcRequest, JsonRpcResponse};
/// use serde::{Serialize, Deserialize};
///
/// #[derive(Debug, Clone, Serialize, Deserialize, JsonRpcRequest)]
/// #[request(method = "_hello", response = HelloResponse)]
/// struct HelloRequest {
///     name: String,
/// }
///
/// #[derive(Debug, Clone, Serialize, Deserialize, JsonRpcResponse)]
/// struct HelloResponse {
///     greeting: String,
/// }
/// ```
pub trait JsonRpcRequest: JsonRpcMessage {
    /// The type of data expected in response.
    type Response: JsonRpcResponse;
}

/// An incoming request, notification, or response being dispatched through handlers.
/// Requests include the context used to answer them; responses include the context
/// used to route them to the local requester.
///
/// Type parameters allow specifying the concrete request and notification types.
/// By default, both are `UntypedMessage` for dynamic dispatch.
/// The request context's response type matches the request's response type.
#[derive(Debug)]
pub enum Dispatch<Req: JsonRpcRequest = UntypedMessage, Notif: JsonRpcNotification = UntypedMessage>
{
    /// Incoming request and the context where the response should be sent.
    Request(Req, Responder<Req::Response>),

    /// Incoming notification.
    Notification(Notif),

    /// Incoming response to a request we sent.
    ///
    /// The first field is the response result (success or error from the remote).
    /// The second field is the context for forwarding the response to its destination
    /// (typically a waiting oneshot channel).
    Response(
        Result<Req::Response, crate::Error>,
        ResponseRouter<Req::Response>,
    ),
}

impl<Req: JsonRpcRequest, Notif: JsonRpcNotification> Dispatch<Req, Notif> {
    /// Map the request and notification types to new types.
    ///
    /// Note: Response variants are passed through unchanged since they don't
    /// contain a parseable message payload.
    pub fn map<Req1, Notif1>(
        self,
        map_request: impl FnOnce(Req, Responder<Req::Response>) -> (Req1, Responder<Req1::Response>),
        map_notification: impl FnOnce(Notif) -> Notif1,
    ) -> Dispatch<Req1, Notif1>
    where
        Req1: JsonRpcRequest<Response = Req::Response>,
        Notif1: JsonRpcNotification,
    {
        match self {
            Dispatch::Request(request, responder) => {
                let (new_request, new_responder) = map_request(request, responder);
                Dispatch::Request(new_request, new_responder)
            }
            Dispatch::Notification(notification) => {
                let new_notification = map_notification(notification);
                Dispatch::Notification(new_notification)
            }
            Dispatch::Response(result, router) => Dispatch::Response(result, router),
        }
    }

    /// Convert the message in self to an untyped message.
    ///
    /// Note: Response variants don't have an untyped message representation.
    /// This returns an error for Response variants.
    pub fn to_untyped_message(&self) -> Result<UntypedMessage, crate::Error> {
        match self {
            Dispatch::Request(request, _) => request.to_untyped_message(),
            Dispatch::Notification(notification) => notification.to_untyped_message(),
            Dispatch::Response(_, _) => Err(crate::util::internal_error(
                "Response variant has no untyped message representation",
            )),
        }
    }

    /// Convert self to an untyped message context.
    ///
    /// Note: Response variants cannot be converted. This returns an error for Response variants.
    pub fn into_untyped_dispatch(self) -> Result<Dispatch, crate::Error> {
        match self {
            Dispatch::Request(request, responder) => Ok(Dispatch::Request(
                request.to_untyped_message()?,
                responder.erase_to_json(),
            )),
            Dispatch::Notification(notification) => {
                Ok(Dispatch::Notification(notification.to_untyped_message()?))
            }
            Dispatch::Response(_, _) => Err(crate::util::internal_error(
                "cannot convert Response variant to untyped message context",
            )),
        }
    }

    /// Returns the request ID if this is a request or response, None if notification.
    pub fn id(&self) -> Option<&RequestId> {
        match self {
            Dispatch::Request(_, cx) => Some(cx.id()),
            Dispatch::Notification(_) => None,
            Dispatch::Response(_, cx) => Some(cx.id()),
        }
    }

    fn handler_error_target(&self) -> Option<HandlerErrorTarget> {
        match self {
            Dispatch::Request(_, responder) => {
                Some(HandlerErrorTarget::Request(responder.reply_target()))
            }
            Dispatch::Notification(_) => None,
            Dispatch::Response(_, router) => {
                Some(HandlerErrorTarget::Response(router.reply_target.clone()))
            }
        }
    }

    /// Returns the method of the message.
    ///
    /// For requests and notifications, this is the method from the message payload.
    /// For responses, this is the method of the original request.
    pub fn method(&self) -> &str {
        match self {
            Dispatch::Request(msg, _) => msg.method(),
            Dispatch::Notification(msg) => msg.method(),
            Dispatch::Response(_, cx) => cx.method(),
        }
    }
}

impl Dispatch {
    /// Attempts to parse `self` into a typed message context.
    ///
    /// # Returns
    ///
    /// * `Ok(Ok(typed))` if this dispatch matches the requested type for its variant
    /// * `Ok(Err(self))` if it does not match the requested type for its variant
    /// * `Err` if its method matches the requested type but parsing fails
    #[tracing::instrument(skip(self), fields(Request = ?std::any::type_name::<Req>(), Notif = ?std::any::type_name::<Notif>()), level = "trace")]
    pub(crate) fn into_typed_dispatch<Req: JsonRpcRequest, Notif: JsonRpcNotification>(
        self,
    ) -> Result<Result<Dispatch<Req, Notif>, Dispatch>, crate::Error> {
        tracing::debug!(method = %self.method(), "into_typed_dispatch");
        match self {
            Dispatch::Request(message, responder) => {
                if Req::matches_method(&message.method) {
                    match Req::parse_message(&message.method, &message.params) {
                        Ok(req) => {
                            tracing::trace!("request parsed");
                            Ok(Ok(Dispatch::Request(req, responder.cast())))
                        }
                        Err(err) => {
                            tracing::trace!(error_code = ?&err.code, "parse error");
                            Err(err)
                        }
                    }
                } else {
                    tracing::trace!("method doesn't match");
                    Ok(Err(Dispatch::Request(message, responder)))
                }
            }

            Dispatch::Notification(message) => {
                if Notif::matches_method(&message.method) {
                    match Notif::parse_message(&message.method, &message.params) {
                        Ok(notif) => {
                            tracing::trace!("notification parsed");
                            Ok(Ok(Dispatch::Notification(notif)))
                        }
                        Err(err) => {
                            tracing::trace!(error_code = ?&err.code, "parse error");
                            Err(err)
                        }
                    }
                } else {
                    tracing::trace!("method doesn't match");
                    Ok(Err(Dispatch::Notification(message)))
                }
            }

            Dispatch::Response(result, cx) => {
                let method = cx.method();
                if Req::matches_method(method) {
                    // Parse the response result
                    let typed_result = match result {
                        Ok(value) => {
                            match <Req::Response as JsonRpcResponse>::from_value(method, value) {
                                Ok(parsed) => {
                                    tracing::trace!(
                                        response_type = std::any::type_name::<Req::Response>(),
                                        "parse ok"
                                    );
                                    Ok(parsed)
                                }
                                Err(err) => {
                                    tracing::trace!(error_code = ?&err.code, "parse error");
                                    return Err(err);
                                }
                            }
                        }
                        Err(err) => {
                            tracing::trace!("error, passthrough");
                            Err(err)
                        }
                    };
                    Ok(Ok(Dispatch::Response(typed_result, cx.cast())))
                } else {
                    tracing::trace!("method doesn't match");
                    Ok(Err(Dispatch::Response(result, cx)))
                }
            }
        }
    }

    /// True if this message has a field with the given name.
    ///
    /// Returns `false` for Response variants.
    #[must_use]
    pub fn has_field(&self, field_name: &str) -> bool {
        self.message()
            .and_then(|m| m.params().get(field_name))
            .is_some()
    }

    /// Returns true if this message has a session-id field.
    ///
    /// Returns `false` for Response variants.
    pub(crate) fn has_session_id(&self) -> bool {
        self.has_field("sessionId")
    }

    /// Extract the ACP session-id from this message (if any).
    ///
    /// Returns `Ok(None)` for Response variants.
    pub(crate) fn get_session_id(&self) -> Result<Option<SessionId>, crate::Error> {
        let Some(message) = self.message() else {
            return Ok(None);
        };
        let Some(value) = message.params().get("sessionId") else {
            return Ok(None);
        };
        let session_id = serde_json::from_value(value.clone())?;
        Ok(Some(session_id))
    }

    /// Try to parse this as a notification of the given type.
    ///
    /// # Returns
    ///
    /// * `Ok(Ok(typed))` if this is a notification of the requested type
    /// * `Ok(Err(self))` if this is not a matching notification
    /// * `Err` if its method matches the requested type but parsing fails
    pub fn into_notification<N: JsonRpcNotification>(
        self,
    ) -> Result<Result<N, Dispatch>, crate::Error> {
        match self {
            Dispatch::Notification(msg) => {
                if !N::matches_method(&msg.method) {
                    return Ok(Err(Dispatch::Notification(msg)));
                }
                match N::parse_message(&msg.method, &msg.params) {
                    Ok(n) => Ok(Ok(n)),
                    Err(err) => Err(err),
                }
            }
            Dispatch::Request(..) | Dispatch::Response(..) => Ok(Err(self)),
        }
    }

    /// Try to parse this as a request of the given type.
    ///
    /// # Returns
    ///
    /// * `Ok(Ok(typed))` if this is a request of the requested type
    /// * `Ok(Err(self))` if this is not a matching request
    /// * `Err` if its method matches the requested type but parsing fails
    pub fn into_request<Req: JsonRpcRequest>(
        self,
    ) -> Result<Result<(Req, Responder<Req::Response>), Dispatch>, crate::Error> {
        match self {
            Dispatch::Request(msg, responder) => {
                if !Req::matches_method(&msg.method) {
                    return Ok(Err(Dispatch::Request(msg, responder)));
                }
                match Req::parse_message(&msg.method, &msg.params) {
                    Ok(req) => Ok(Ok((req, responder.cast()))),
                    Err(err) => Err(err),
                }
            }
            Dispatch::Notification(..) | Dispatch::Response(..) => Ok(Err(self)),
        }
    }
}

impl<M: JsonRpcRequest + JsonRpcNotification> Dispatch<M, M> {
    /// Returns the message payload for requests and notifications.
    ///
    /// Returns `None` for Response variants since they don't contain a message payload.
    pub fn message(&self) -> Option<&M> {
        match self {
            Dispatch::Request(msg, _) | Dispatch::Notification(msg) => Some(msg),
            Dispatch::Response(_, _) => None,
        }
    }

    /// Map the request/notification message.
    ///
    /// Response variants pass through unchanged.
    pub(crate) fn try_map_message(
        self,
        map_message: impl FnOnce(M) -> Result<M, crate::Error>,
    ) -> Result<Dispatch<M, M>, crate::Error> {
        match self {
            Dispatch::Request(request, cx) => Ok(Dispatch::Request(map_message(request)?, cx)),
            Dispatch::Notification(notification) => {
                Ok(Dispatch::<M, M>::Notification(map_message(notification)?))
            }
            Dispatch::Response(result, cx) => Ok(Dispatch::Response(result, cx)),
        }
    }
}

/// An incoming JSON message without any typing. Can be a request or a notification.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct UntypedMessage {
    /// The JSON-RPC method name
    pub method: String,
    /// The JSON-RPC parameters as a raw JSON value
    pub params: serde_json::Value,
}

impl UntypedMessage {
    /// Returns an untyped message with the given method and parameters.
    pub fn new(method: &str, params: impl Serialize) -> Result<Self, crate::Error> {
        let params = serde_json::to_value(params)?;
        Ok(Self {
            method: method.to_string(),
            params,
        })
    }

    /// Returns the method name
    #[must_use]
    pub fn method(&self) -> &str {
        &self.method
    }

    /// Returns the parameters as a JSON value
    #[must_use]
    pub fn params(&self) -> &serde_json::Value {
        &self.params
    }

    /// Consumes this message and returns the method and params
    #[must_use]
    pub fn into_parts(self) -> (String, serde_json::Value) {
        (self.method, self.params)
    }

    /// Convert `self` to a raw JSON-RPC message.
    pub(crate) fn into_raw_jsonrpc_message(
        self,
        id: Option<RequestId>,
    ) -> Result<RawJsonRpcMessage, crate::Error> {
        let Self { method, params } = self;
        match id {
            Some(id) => RawJsonRpcMessage::request(method, params, id),
            None => RawJsonRpcMessage::notification(method, params),
        }
    }
}

impl JsonRpcMessage for UntypedMessage {
    fn matches_method(_method: &str) -> bool {
        // UntypedMessage matches any method - it's the untyped fallback
        true
    }

    fn method(&self) -> &str {
        &self.method
    }

    fn to_untyped_message(&self) -> Result<UntypedMessage, crate::Error> {
        Ok(self.clone())
    }

    fn parse_message(method: &str, params: &impl Serialize) -> Result<Self, crate::Error> {
        UntypedMessage::new(method, params)
    }
}

impl JsonRpcRequest for UntypedMessage {
    type Response = serde_json::Value;
}

impl JsonRpcNotification for UntypedMessage {}

/// Represents a pending response of type `R` from an outgoing request.
///
/// Returned by [`ConnectionTo::send_request`], this type provides explicit response-consumption
/// modes. The API is intentionally designed to make it difficult to accidentally wait for a
/// response inside the dispatch loop.
///
/// # Anti-Footgun Design
///
/// You cannot directly `.await` a `SentRequest`. Instead, you must choose how to handle
/// the response:
///
/// ## Option 1: Register an Ordered Callback (Safe in Handlers)
///
/// Calling [`on_receiving_result`](Self::on_receiving_result) registers the callback and returns
/// immediately. When ordered consumption is selected before the response is routed during its
/// original dispatch, the loop waits for the callback to complete before processing the next
/// message:
///
/// ```no_run
/// # use agent_client_protocol_test::*;
/// # async fn example(cx: agent_client_protocol::ConnectionTo<agent_client_protocol::UntypedRole>) -> Result<(), agent_client_protocol::Error> {
/// cx.send_request(MyRequest {})
///     .on_receiving_result(async |result| {
///         match result {
///             Ok(response) => {
///                 // Handle successful response
///                 Ok(())
///             }
///             Err(error) => {
///                 // Handle error
///                 Err(error)
///             }
///         }
///     })?;
/// # Ok(())
/// # }
/// ```
///
/// ## Option 2: Wait Outside the Dispatch Loop
///
/// Use [`block_task`](Self::block_task) only when the current task already runs outside the
/// dispatch loop—for example, in the foreground future passed to `connect_with` or in a task
/// created with [`ConnectionTo::spawn`]. Never await it in a handler:
///
/// ```no_run
/// # use agent_client_protocol_test::*;
/// # async fn example(cx: agent_client_protocol::ConnectionTo<agent_client_protocol::UntypedRole>) -> Result<(), agent_client_protocol::Error> {
/// // ✅ Safe: Spawned task runs concurrently
/// cx.spawn({
///     let cx = cx.clone();
///     async move {
///         let response = cx.send_request(MyRequest {})
///             .block_task()
///             .await?;
///         // Process response...
///         Ok(())
///     }
/// })?;
/// # Ok(())
/// # }
/// ```
///
/// ```no_run
/// # use agent_client_protocol_test::*;
/// # async fn example() -> Result<(), agent_client_protocol::Error> {
/// # let connection = mock_connection();
/// // ❌ NEVER do this in a handler - blocks the event loop!
/// connection.on_receive_request(async |req: MyRequest, responder, cx| {
///     let response = cx.send_request(MyRequest {})
///         .block_task()  // This will deadlock!
///         .await?;
///     responder.respond(response)
/// }, agent_client_protocol::on_receive_request!())
/// # .connect_to(agent_client_protocol_test::MockTransport).await?;
/// # Ok(())
/// # }
/// ```
///
/// # Why This Design?
///
/// If you block the event loop while waiting for a response, the connection cannot process
/// the incoming response message, creating a deadlock. This API design prevents that footgun
/// by making blocking explicit and encouraging non-blocking patterns.
///
/// # Drop Behavior
///
/// By default, dropping a `SentRequest` before the SDK has received the
/// response sends a `$/cancel_request` notification asking the peer to cancel
/// the request, then discards the response when it arrives. Requests whose
/// eventual response should be ignored, but which should keep running on the
/// peer, should use [`detach`](Self::detach) instead.
///
/// # Incoming Transport EOF
///
/// If the incoming transport reaches clean EOF before the response arrives, every
/// consumption mode receives an error with the message `Incoming transport
/// closed` and data containing
/// `{"reason":"incoming_transport_closed","method":"..."}`. Requests made
/// after incoming EOF fail immediately with the same error. Use
/// [`is_incoming_transport_closed`] to identify it.
#[must_use = "dropping a SentRequest asks the peer to cancel the request and \
              discards the response; consume it with `block_task`, \
              `on_receiving_result`, `forward_response_to`, or `detach`"]
pub struct SentRequest<T> {
    id: RequestId,
    method: String,
    task_tx: TaskTx,
    response_rx: oneshot::Receiver<ResponsePayload>,
    to_result: Box<dyn FnOnce(serde_json::Value) -> Result<T, crate::Error> + Send>,
    cancellation: SentRequestCancellation,
    response_ordering: ResponseOrdering,
    /// Cancellation markers of other (incoming) requests whose cancellation
    /// should be forwarded to this request. See
    /// [`forward_cancellation_from`](Self::forward_cancellation_from).
    cancellation_sources: Vec<RequestCancellation>,
}

#[derive(Clone, Debug)]
pub(crate) struct SentRequestCancellationDisarm {
    armed: Arc<AtomicBool>,
}

impl SentRequestCancellationDisarm {
    fn new() -> Self {
        Self {
            armed: Arc::new(AtomicBool::new(true)),
        }
    }

    fn disarm(&self) {
        self.armed.store(false, Ordering::Release);
    }
}

struct SentRequestCancellation {
    message_tx: OutgoingMessageTx,
    remote_style: crate::role::RemoteStyle,
    request_id: RequestId,
    disarm: SentRequestCancellationDisarm,
}

impl SentRequestCancellation {
    fn new(
        message_tx: OutgoingMessageTx,
        remote_style: crate::role::RemoteStyle,
        request_id: RequestId,
    ) -> Self {
        Self {
            message_tx,
            remote_style,
            request_id,
            disarm: SentRequestCancellationDisarm::new(),
        }
    }

    fn disarm(&self) {
        self.disarm.disarm();
    }

    fn disarm_handle(&self) -> SentRequestCancellationDisarm {
        self.disarm.clone()
    }

    fn send(&self) -> Result<(), crate::Error> {
        if !self.disarm.armed.swap(false, Ordering::AcqRel) {
            return Ok(());
        }

        // Build the notification lazily: most requests are never cancelled,
        // so this avoids serializing a notification per outgoing request.
        let untyped = self.remote_style.transform_outgoing_message(
            crate::schema::v1::CancelRequestNotification::new(self.request_id.clone()),
        )?;

        send_raw_message(&self.message_tx, OutgoingMessage::Notification { untyped })
    }
}

impl Drop for SentRequestCancellation {
    fn drop(&mut self) {
        if let Err(error) = self.send() {
            tracing::debug!(?error, "failed to auto-cancel dropped request");
        }
    }
}

impl Debug for SentRequestCancellation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SentRequestCancellation")
            .field("request_id", &self.request_id)
            .field("remote_style", &self.remote_style)
            .field("armed", &self.disarm.armed.load(Ordering::Acquire))
            .finish_non_exhaustive()
    }
}

/// Await the response payload for an outgoing request, watching `sources` for
/// cancellation of the upstream requests it was registered with.
///
/// When any source reports cancellation, a `$/cancel_request` is forwarded to
/// the outgoing request (at most once, shared with [`SentRequest::cancel`] and
/// drop-time auto-cancellation), and the response is *still* awaited: the peer
/// always answers, with normal data or a cancellation error.
///
/// Watching is deliberately bounded by response arrival so that completed
/// requests do not leak waiters on markers that will never fire.
async fn await_response_forwarding_cancellation(
    response_rx: oneshot::Receiver<ResponsePayload>,
    cancellation: &SentRequestCancellation,
    sources: &[RequestCancellation],
) -> Result<ResponsePayload, oneshot::Canceled> {
    // Failing to forward the cancellation must not abort the wait: the
    // response (normal data or a cancellation error) may still arrive and
    // must still be processed.
    let forward_cancellation = || {
        if let Err(error) = cancellation.send() {
            tracing::debug!(
                ?error,
                "failed to forward cancellation to downstream request"
            );
        }
    };

    let response = if sources.is_empty() {
        response_rx.await
    } else if sources.iter().any(RequestCancellation::is_cancelled) {
        forward_cancellation();
        response_rx.await
    } else {
        let cancelled = sources.iter().map(|source| source.state.signal_rx.clone());
        match future::select(future::select_all(cancelled), response_rx).await {
            Either::Left((_, response_rx)) => {
                forward_cancellation();
                response_rx.await
            }
            Either::Right((response, _)) => response,
        }
    };

    cancellation.disarm();
    response
}

impl<T: Debug> Debug for SentRequest<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mut debug = f.debug_struct("SentRequest");
        debug
            .field("id", &self.id)
            .field("method", &self.method)
            .field("task_tx", &self.task_tx)
            .field("response_rx", &self.response_rx);
        debug
            .field("cancellation", &self.cancellation)
            .field("cancellation_sources", &self.cancellation_sources);
        debug.finish_non_exhaustive()
    }
}

impl SentRequest<serde_json::Value> {
    fn new(
        id: RequestId,
        method: String,
        task_tx: mpsc::UnboundedSender<Task>,
        response_rx: oneshot::Receiver<ResponsePayload>,
        cancellation: SentRequestCancellation,
        response_ordering: ResponseOrdering,
    ) -> Self {
        Self {
            id,
            method,
            response_rx,
            task_tx,
            to_result: Box::new(Ok),
            cancellation,
            response_ordering,
            cancellation_sources: Vec::new(),
        }
    }
}

impl<T> SentRequest<T> {
    /// Detach this request handle without waiting for its response.
    ///
    /// The response will be discarded when it arrives. This also disarms the
    /// drop-time automatic cancellation described in
    /// [Drop Behavior](Self#drop-behavior), so use it for requests whose
    /// eventual response should be ignored, but which should keep running on
    /// the peer. The peer is still expected to answer the JSON-RPC request
    /// eventually; use a notification instead when no response is expected at
    /// all.
    ///
    /// To ask the peer to stop the request, call `cancel` instead, or drop the
    /// handle while automatic cancellation is armed.
    pub fn detach(self) {
        self.cancellation.disarm();
    }

    /// Send a `$/cancel_request` notification for this outgoing request.
    ///
    /// This uses the same peer and message wrapping that were used to send the
    /// original request, so it is the preferred way to cancel a [`SentRequest`]
    /// when the request handle is still available.
    ///
    /// At most one `$/cancel_request` is ever sent per request: the first
    /// `cancel` call sends it (and also prevents the drop-time automatic
    /// cancellation described in [Drop Behavior](Self#drop-behavior)), while
    /// later calls return `Ok(())` without sending anything. Likewise, once
    /// the SDK has routed the response to this handle, `cancel` becomes a
    /// no-op: there is nothing left to cancel.
    ///
    /// Errors are only reported by the call that attempts to send the
    /// notification.
    pub fn cancel(&self) -> Result<(), crate::Error> {
        self.cancellation.send()
    }

    /// Forward cancellation of another request to this one.
    ///
    /// When the request that `source` belongs to is cancelled by its peer,
    /// a `$/cancel_request` for *this* request is sent to its peer, using the
    /// same wrapping as the original request. The response is still awaited
    /// and delivered as usual (normal data or a cancellation error), so this
    /// composes with [`block_task`](Self::block_task) and
    /// [`on_receiving_result`](Self::on_receiving_result).
    ///
    /// This is the building block for proxies that forward a request with
    /// custom logic instead of [`forward_response_to`](Self::forward_response_to)
    /// (which wires this up automatically from its responder). Without it,
    /// custom forwarding *absorbs* cancellation: the upstream marker is still
    /// set, but nothing is sent downstream.
    ///
    /// ```
    /// # use agent_client_protocol::{ConnectionTo, Error, Responder, UntypedRole};
    /// # use agent_client_protocol_test::{MyRequest, MyResponse};
    /// # async fn example(request: MyRequest, responder: Responder<MyResponse>, backend: ConnectionTo<UntypedRole>) -> Result<(), Error> {
    /// backend
    ///     .send_request(request)
    ///     .forward_cancellation_from(responder.cancellation())
    ///     .on_receiving_result(async move |result| {
    ///         // Custom result handling, e.g. bookkeeping or rewriting.
    ///         responder.respond_with_result(result)
    ///     })?;
    /// # Ok(())
    /// # }
    /// ```
    ///
    /// May be called multiple times; cancellation of any registered source
    /// triggers the forwarding (at most one `$/cancel_request` is ever sent
    /// per request). Sources are observed while the response is being
    /// awaited — that is, once the handle is consumed with
    /// [`block_task`](Self::block_task),
    /// [`on_receiving_result`](Self::on_receiving_result), or
    /// [`forward_response_to`](Self::forward_response_to); a source that was
    /// already cancelled by then is honored immediately.
    pub fn forward_cancellation_from(mut self, source: RequestCancellation) -> Self {
        self.cancellation_sources.push(source);
        self
    }
}

impl<T> SentRequest<T> {
    /// The id of the outgoing request.
    #[must_use]
    pub fn id(&self) -> &RequestId {
        &self.id
    }

    /// The method of the request this is in response to.
    #[must_use]
    pub fn method(&self) -> &str {
        &self.method
    }

    /// Map a successful JSON-RPC response into an application type.
    ///
    /// The mapped type does not need to implement [`JsonRpcResponse`]. The
    /// mapper runs at most once and may consume captured state. JSON-RPC error
    /// responses bypass the mapper. The mapped type may carry a non-`'static`
    /// lifetime when it is consumed with [`block_task`](Self::block_task);
    /// callback-style consumption still requires a `'static` mapped type
    /// because its work is spawned onto the connection.
    pub fn map<U>(
        self,
        map_fn: impl FnOnce(T) -> Result<U, crate::Error> + 'static + Send,
    ) -> SentRequest<U>
    where
        T: 'static,
    {
        SentRequest {
            id: self.id,
            method: self.method,
            response_rx: self.response_rx,
            task_tx: self.task_tx,
            to_result: Box::new(move |value| map_fn((self.to_result)(value)?)),
            cancellation: self.cancellation,
            response_ordering: self.response_ordering,
            cancellation_sources: self.cancellation_sources,
        }
    }

    /// Forward the response (success or error) to a request context when it arrives.
    ///
    /// This is a convenience method for proxying messages between connections. When the
    /// response arrives, it will be automatically sent to the provided request context,
    /// whether it's a successful response or an error.
    ///
    /// # Example: Proxying requests
    ///
    /// ```
    /// # use agent_client_protocol::UntypedRole;
    /// # use agent_client_protocol::{Builder, ConnectionTo};
    /// # use agent_client_protocol_test::*;
    /// # async fn example(cx: ConnectionTo<UntypedRole>) -> Result<(), agent_client_protocol::Error> {
    /// // Set up backend connection builder
    /// let backend = UntypedRole.builder()
    ///     .on_receive_request(async |req: MyRequest, responder, cx| {
    ///         responder.respond(MyResponse { status: "ok".into() })
    ///     }, agent_client_protocol::on_receive_request!());
    ///
    /// // Spawn backend and get a context to send to it
    /// let backend_connection = cx.spawn_connection(backend, MockTransport)?;
    ///
    /// // Set up proxy that forwards requests to backend
    /// UntypedRole.builder()
    ///     .on_receive_request({
    ///         let backend_connection = backend_connection.clone();
    ///         async move |req: MyRequest, responder, cx| {
    ///             // Forward the request to backend and proxy the response back
    ///             backend_connection.send_request(req)
    ///                 .forward_response_to(responder)?;
    ///             Ok(())
    ///         }
    ///     }, agent_client_protocol::on_receive_request!());
    /// # Ok(())
    /// # }
    /// ```
    ///
    /// # Type Safety
    ///
    /// The request context's response type must match the request's response type,
    /// ensuring type-safe message forwarding.
    ///
    /// # When to Use
    ///
    /// Use this when:
    /// - You're implementing a proxy or gateway pattern
    /// - You want to forward responses without processing them
    /// - The response types match between the outgoing request and incoming request
    ///
    /// This is equivalent to calling `on_receiving_result` and manually forwarding
    /// the result, with two proxy-specific additions:
    ///
    /// - If the pending response cannot be delivered, the incoming request is
    ///   answered with an internal error instead of being left unanswered.
    ///   Known clean incoming EOF is delivered like any other response
    ///   error; an unexpected response-channel loss is forwarded as an outer
    ///   consumption error.
    /// - When the peer cancels the incoming request, the cancellation is
    ///   forwarded to the outgoing request, and the downstream response
    ///   (normal data or a cancellation error) is still forwarded back. This is
    ///   equivalent to registering the responder's marker with
    ///   `forward_cancellation_from`.
    #[track_caller]
    pub fn forward_response_to(self, responder: Responder<T>) -> Result<(), crate::Error>
    where
        T: JsonRpcResponse,
    {
        let this = self.forward_cancellation_from(responder.cancellation());

        this.consume_with(async move |response| {
            // An unexpected response-channel loss (outer `Err`) is forwarded
            // as an error: the incoming request must not be left unanswered.
            responder.respond_with_result(response.unwrap_or_else(Err))
        })
    }

    /// Spawn the response-consumption task shared by
    /// [`on_receiving_result`](Self::on_receiving_result) and
    /// [`forward_response_to`](Self::forward_response_to).
    ///
    /// The task awaits the response (forwarding cancellation from registered
    /// sources while waiting, converts the payload, and invokes `handle` with
    /// the typed result (`Ok(Result<T, _>)`). The dispatch loop's ack, if any,
    /// is sent after `handle` completes.
    ///
    /// Clean incoming EOF is delivered as `Ok(Err(error))`, just like
    /// a peer response error, so callback-style consumers still run. If the
    /// response channel disappears for another reason, `handle` receives an
    /// outer `Err` describing that unexpected loss; there is no ack then.
    #[track_caller]
    fn consume_with<F>(
        self,
        handle: impl FnOnce(Result<Result<T, crate::Error>, crate::Error>) -> F + 'static + Send,
    ) -> Result<(), crate::Error>
    where
        T: 'static,
        F: Future<Output = Result<(), crate::Error>> + 'static + Send,
    {
        self.response_ordering.mark_ordered();
        let task_tx = self.task_tx.clone();
        let method = self.method;
        let response_rx = self.response_rx;
        let to_result = self.to_result;
        let cancellation = self.cancellation;
        let cancellation_sources = self.cancellation_sources;
        let location = Location::caller();

        Task::new(location, async move {
            let response = await_response_forwarding_cancellation(
                response_rx,
                &cancellation,
                &cancellation_sources,
            )
            .await;

            match response {
                Ok(ResponsePayload { result, ack_tx }) => {
                    // Convert the result using to_result for Ok values
                    let typed_result = match result {
                        Ok(json_value) => to_result(json_value),
                        Err(err) => Err(err),
                    };

                    let outcome = handle(Ok(typed_result)).await;

                    // Ack AFTER the handler completes - this is the key
                    // difference from block_task. The dispatch loop waits for
                    // this ack.
                    if let Some(tx) = ack_tx {
                        let _ = tx.send(());
                    }

                    outcome
                }
                Err(err) => {
                    handle(Err(crate::util::internal_error(format!(
                        "response to `{method}` never received: {err}"
                    ))))
                    .await
                }
            }
        })
        .spawn(&task_tx)
    }

    /// Block the current task until the response is received.
    ///
    /// **Warning:** This method blocks the current async task. It is safe only when that task
    /// already runs outside the dispatch loop, such as the foreground future passed to
    /// `connect_with` or a task created with [`ConnectionTo::spawn`]. Using it directly in a
    /// handler callback will deadlock the connection.
    ///
    /// # Safe Usage (outside the dispatch loop)
    ///
    /// ```no_run
    /// # use agent_client_protocol_test::*;
    /// # async fn example() -> Result<(), agent_client_protocol::Error> {
    /// # let connection = mock_connection();
    /// connection.on_receive_request(async |req: MyRequest, responder, cx| {
    ///     // Spawn a task to handle the request
    ///     cx.spawn({
    ///         let connection = cx.clone();
    ///         async move {
    ///             // Safe: We're in a spawned task, not blocking the event loop
    ///             let response = connection.send_request(OtherRequest {})
    ///                 .block_task()
    ///                 .await?;
    ///
    ///             // Process the response...
    ///             Ok(())
    ///         }
    ///     })?;
    ///
    ///     // Respond immediately
    ///     responder.respond(MyResponse { status: "ok".into() })
    /// }, agent_client_protocol::on_receive_request!())
    /// # .connect_to(agent_client_protocol_test::MockTransport).await?;
    /// # Ok(())
    /// # }
    /// ```
    ///
    /// # Unsafe Usage (in handlers - will deadlock!)
    ///
    /// ```no_run
    /// # use agent_client_protocol_test::*;
    /// # async fn example() -> Result<(), agent_client_protocol::Error> {
    /// # let connection = mock_connection();
    /// connection.on_receive_request(async |req: MyRequest, responder, cx| {
    ///     // ❌ DEADLOCK: Handler blocks event loop, which can't process the response
    ///     let response = cx.send_request(OtherRequest {})
    ///         .block_task()
    ///         .await?;
    ///
    ///     responder.respond(MyResponse { status: response.value })
    /// }, agent_client_protocol::on_receive_request!())
    /// # .connect_to(agent_client_protocol_test::MockTransport).await?;
    /// # Ok(())
    /// # }
    /// ```
    ///
    /// # When to Use
    ///
    /// Use this method when:
    /// - Your current task already runs outside the dispatch loop
    /// - You need the response value to proceed with your logic
    /// - Linear control flow is more natural than callbacks
    ///
    /// For handler callbacks, use [`on_receiving_result`](Self::on_receiving_result) instead.
    pub async fn block_task(self) -> Result<T, crate::Error> {
        let response = await_response_forwarding_cancellation(
            self.response_rx,
            &self.cancellation,
            &self.cancellation_sources,
        )
        .await;

        match response {
            Ok(ResponsePayload {
                result: Ok(json_value),
                ack_tx,
            }) => {
                // Blocking consumers ack before converting or returning the
                // value, so dispatch can continue while the caller processes it.
                if let Some(tx) = ack_tx {
                    let _ = tx.send(());
                }
                match (self.to_result)(json_value) {
                    Ok(value) => Ok(value),
                    Err(err) => Err(err),
                }
            }
            Ok(ResponsePayload {
                result: Err(err),
                ack_tx,
            }) => {
                if let Some(tx) = ack_tx {
                    let _ = tx.send(());
                }
                Err(err)
            }
            Err(err) => Err(crate::util::internal_error(format!(
                "response to `{}` never received: {}",
                self.method, err
            ))),
        }
    }

    /// Block the current task and transform the typed result before releasing
    /// the ordered-response barrier.
    ///
    /// Framework lifecycle code uses this when success transfers local state
    /// to the returned value while an error must drop that state before later
    /// messages from the same transport frame are dispatched. The synchronous
    /// transform must not wait for additional connection traffic.
    pub(crate) async fn block_task_with_ordered_result<U>(
        self,
        transform: impl FnOnce(Result<T, crate::Error>) -> Result<U, crate::Error>,
    ) -> Result<U, crate::Error> {
        let response = await_response_forwarding_cancellation(
            self.response_rx,
            &self.cancellation,
            &self.cancellation_sources,
        )
        .await;

        let (result, ack_tx) = match response {
            Ok(ResponsePayload { result, ack_tx }) => {
                let typed_result = match result {
                    Ok(json_value) => (self.to_result)(json_value),
                    Err(error) => Err(error),
                };
                (typed_result, ack_tx)
            }
            Err(error) => (
                Err(crate::util::internal_error(format!(
                    "response to `{}` never received: {error}",
                    self.method
                ))),
                None,
            ),
        };

        let outcome = transform(result);
        if let Some(acknowledgment) = ack_tx {
            let _ = acknowledgment.send(());
        }
        outcome
    }

    /// Schedule an async task to run when a successful response is received.
    ///
    /// This is a convenience wrapper around [`on_receiving_result`](Self::on_receiving_result)
    /// for the common pattern of forwarding errors to a request context while only processing
    /// successful responses.
    ///
    /// # Behavior
    ///
    /// - If the response is `Ok(value)`, your task receives the value and the request context
    /// - If the response is `Err(error)`, the error is automatically sent to `responder`
    ///   and your task is not called
    ///
    /// # Example: Chaining requests
    ///
    /// ```no_run
    /// # use agent_client_protocol_test::*;
    /// # async fn example() -> Result<(), agent_client_protocol::Error> {
    /// # let connection = mock_connection();
    /// connection.on_receive_request(async |req: ValidateRequest, responder, cx| {
    ///     // Send initial request
    ///     cx.send_request(ValidateRequest { data: req.data.clone() })
    ///         .on_receiving_ok_result(responder, async |validation, responder| {
    ///             // Only runs if validation succeeded
    ///             if validation.is_valid {
    ///                 // Respond to original request
    ///                 responder.respond(ValidateResponse { is_valid: true, error: None })
    ///             } else {
    ///                 responder.respond_with_error(agent_client_protocol::util::internal_error("validation failed"))
    ///             }
    ///         })?;
    ///
    ///     Ok(())
    /// }, agent_client_protocol::on_receive_request!())
    /// # .connect_to(agent_client_protocol_test::MockTransport).await?;
    /// # Ok(())
    /// # }
    /// ```
    ///
    /// # Ordering
    ///
    /// Like [`on_receiving_result`](Self::on_receiving_result), response handling holds the
    /// dispatch loop through callback completion when ordered consumption is selected before a
    /// peer response is routed during its original dispatch. Pending-request failures delivered
    /// without an incoming response and delayed routes do not carry that barrier. The callback
    /// must not await later inbound traffic on the same connection. See the
    /// [`ordering`](crate::concepts::ordering) module for details.
    ///
    /// # When to Use
    ///
    /// Use this when:
    /// - You need to respond to a request based on another request's result
    /// - You want errors to automatically propagate to the request context
    /// - You only care about the success case
    ///
    /// For more control over error handling, use [`on_receiving_result`](Self::on_receiving_result).
    #[track_caller]
    pub fn on_receiving_ok_result<F>(
        self,
        responder: Responder<T>,
        task: impl FnOnce(T, Responder<T>) -> F + 'static + Send,
    ) -> Result<(), crate::Error>
    where
        F: Future<Output = Result<(), crate::Error>> + 'static + Send,
        T: JsonRpcResponse,
    {
        self.on_receiving_result(async move |result| match result {
            Ok(value) => task(value, responder).await,
            Err(err) => responder.respond_with_error(err),
        })
    }

    /// Register an async callback to run when the response is received.
    ///
    /// This is the recommended way to select response handling from inside a handler because
    /// registration returns immediately. The response-consumption task waits concurrently for
    /// the response; once the response is dispatched, the ordered callback may hold the dispatch
    /// loop until it completes.
    ///
    /// # Example: Handle response in callback
    ///
    /// ```no_run
    /// # use agent_client_protocol_test::*;
    /// # async fn example() -> Result<(), agent_client_protocol::Error> {
    /// # let connection = mock_connection();
    /// connection.on_receive_request(async |req: MyRequest, responder, cx| {
    ///     // Send a request and schedule a callback for the response
    ///     cx.send_request(QueryRequest { id: 22 })
    ///         .on_receiving_result({
    ///             let connection = cx.clone();
    ///             async move |result| {
    ///                 match result {
    ///                     Ok(response) => {
    ///                         println!("Got response: {:?}", response);
    ///                         // Can send more messages here
    ///                         connection.send_notification(QueryComplete {})?;
    ///                         Ok(())
    ///                 }
    ///                     Err(error) => {
    ///                         eprintln!("Request failed: {}", error);
    ///                         Err(error)
    ///                     }
    ///                 }
    ///             }
    ///         })?;
    ///
    ///     // Handler continues immediately after registering the callback
    ///     responder.respond(MyResponse { status: "processing".into() })
    /// }, agent_client_protocol::on_receive_request!())
    /// # .connect_to(agent_client_protocol_test::MockTransport).await?;
    /// # Ok(())
    /// # }
    /// ```
    ///
    /// # Ordering
    ///
    /// When ordered consumption is selected before a peer response is routed during its original
    /// dispatch, the callback runs in a connection-managed task and the dispatch loop waits for
    /// it to complete before processing the next message.
    ///
    /// The barrier does not apply when the pending request is failed without an incoming response,
    /// such as on EOF. If the response was already routed, or an interceptor routes a retained
    /// [`ResponseRouter`] after its original dispatch, the callback still runs but cannot
    /// retroactively block messages that were already released.
    ///
    /// While the barrier is held, the callback must not await a later response, notification, or
    /// other inbound traffic on the same connection: that traffic cannot be dispatched until the
    /// callback completes. Spawn follow-up work with [`ConnectionTo::spawn`] and return, or use
    /// [`block_task`](Self::block_task) from a task already outside the dispatch loop.
    ///
    /// This differs from [`block_task`](Self::block_task), which does not select ordered
    /// consumption: dispatch remains free while the caller processes the delivered response.
    ///
    /// See the [`ordering`](crate::concepts::ordering) module for details on ordering guarantees
    /// and how to avoid deadlocks.
    ///
    /// # Error Handling
    ///
    /// If the scheduled task returns `Err`, the entire server will shut down. Make sure to handle
    /// errors appropriately within your task.
    ///
    /// # When to Use
    ///
    /// Use this method when:
    /// - You need to register response handling from a handler callback
    /// - You want a peer response callback to complete before later messages are dispatched
    /// - The callback performs bounded work that does not depend on later inbound traffic
    ///
    /// When already outside the dispatch loop and you do not need ordering guarantees, consider
    /// [`block_task`](Self::block_task).
    #[track_caller]
    pub fn on_receiving_result<F>(
        self,
        task: impl FnOnce(Result<T, crate::Error>) -> F + 'static + Send,
    ) -> Result<(), crate::Error>
    where
        T: 'static,
        F: Future<Output = Result<(), crate::Error>> + 'static + Send,
    {
        self.consume_with(move |response| match response {
            // Invoke the callback before constructing its future so the
            // response value does not need to be `Send` across an await.
            Ok(result) => Either::Left(task(result)),
            // A response that was never delivered fails the consuming
            // task instead of invoking the callback.
            Err(err) => Either::Right(future::ready(Err(err))),
        })
    }
}

// ============================================================================
// IntoJrConnectionTransport Implementations
// ============================================================================

/// A component that communicates over line streams.
///
/// `Lines` implements the [`ConnectTo`] trait for any pair of line-based streams
/// (a `Stream<Item = io::Result<String>>` for incoming and a `Sink<String>` for outgoing),
/// handling serialization of JSON-RPC messages to/from newline-delimited JSON.
/// An incoming line may contain one JSON-RPC message or a non-empty batch array. Batch
/// entries are dispatched individually in source order, and responses to the batch are
/// collected into one response-array line. SDK-initiated requests and notifications remain
/// individual messages.
///
/// This is a lower-level primitive than [`ByteStreams`] that enables interception and
/// transformation of individual lines before they are parsed or after they are serialized.
/// This is particularly useful for debugging, logging, or implementing custom line-based
/// protocols.
///
/// # Use Cases
///
/// - **Line-by-line logging**: Intercept and log each line before parsing
/// - **Custom protocols**: Transform lines before/after JSON-RPC processing
/// - **Debugging**: Inspect raw message strings
/// - **Line filtering**: Skip or modify specific messages
///
/// Most users should use [`ByteStreams`] instead, which provides a simpler interface
/// for byte-based I/O.
///
/// [`ConnectTo`]: crate::ConnectTo
#[derive(Debug)]
pub struct Lines<OutgoingSink, IncomingStream> {
    outgoing: OutgoingSink,
    incoming: IncomingStream,
}

impl<OutgoingSink, IncomingStream> Lines<OutgoingSink, IncomingStream>
where
    OutgoingSink: futures::Sink<String, Error = std::io::Error> + Send + 'static,
    IncomingStream: futures::Stream<Item = std::io::Result<String>> + Send + 'static,
{
    /// Create a new line stream transport.
    pub fn new(outgoing: OutgoingSink, incoming: IncomingStream) -> Self {
        Self { outgoing, incoming }
    }

    fn into_channel_transport(self) -> (Channel, BoxFuture<'static, Result<(), crate::Error>>) {
        let Self { outgoing, incoming } = self;
        let (channel_for_caller, channel_for_lines) = Channel::duplex();

        let server_future = Box::pin(async move {
            let Channel { rx, tx } = channel_for_lines;
            let outgoing_future = transport_actor::transport_outgoing_lines_actor(rx, outgoing);
            let incoming_future = transport_actor::transport_incoming_lines_actor(incoming, tx);
            futures::try_join!(outgoing_future, incoming_future)?;
            Ok(())
        });

        (channel_for_caller, server_future)
    }
}

impl<OutgoingSink, IncomingStream, R: Role> ConnectTo<R> for Lines<OutgoingSink, IncomingStream>
where
    OutgoingSink: futures::Sink<String, Error = std::io::Error> + Send + 'static,
    IncomingStream: futures::Stream<Item = std::io::Result<String>> + Send + 'static,
{
    async fn connect_to(self, client: impl ConnectTo<R::Counterpart>) -> Result<(), crate::Error> {
        let Self { outgoing, incoming } = self;
        let (Channel { rx, tx }, client_channel) = Channel::duplex();
        let close_client_output = client_channel.tx.clone();
        let client_future = Box::pin(async move {
            let result = client.connect_to(client_channel).await;
            close_client_output.close_channel();
            result
        });

        // Once the client completes successfully, its incoming channel is
        // gone. Keep consuming successful messages from the physical read
        // half without forwarding them so a full-duplex peer cannot block our
        // outgoing sink while it is being drained. Transport errors must still
        // fail the connection.
        let discard_incoming = Arc::new(AtomicBool::new(false));
        let incoming = incoming.filter_map({
            let discard_incoming = discard_incoming.clone();
            move |item| {
                let discard_incoming = discard_incoming.load(Ordering::Acquire);
                future::ready((!discard_incoming || item.is_err()).then_some(item))
            }
        });

        let outgoing = transport_actor::transport_outgoing_lines_actor(rx, outgoing)
            .boxed()
            .shared();
        let serve_self = Box::pin({
            let outgoing = outgoing.clone();
            async move {
                futures::try_join!(
                    outgoing,
                    transport_actor::transport_incoming_lines_actor(incoming, tx),
                )?;
                Ok(())
            }
        });

        match futures::future::select(client_future, serve_self).await {
            Either::Left((result, serve_self)) => {
                result?;
                discard_incoming.store(true, Ordering::Release);

                // Drive the read half while waiting for the write half, but do
                // not require the peer's independent incoming stream to reach
                // EOF. If incoming processing finishes successfully first,
                // the shared outgoing future still owns and drains the sink.
                // A successful `serve_self` result includes its shared
                // outgoing clone, while any error must remain authoritative
                // instead of being hidden behind the other handle. Poll it
                // first so a ready read error wins over clean outgoing
                // completion.
                match future::select(serve_self, outgoing).await {
                    Either::Left((result, _)) | Either::Right((result, _)) => result,
                }
            }
            Either::Right((result, _)) => result,
        }
    }

    fn into_channel_and_future(self) -> (Channel, BoxFuture<'static, Result<(), crate::Error>>) {
        self.into_channel_transport()
    }
}

/// A component that communicates over byte streams (stdin/stdout, sockets, pipes, etc.).
///
/// `ByteStreams` implements the [`ConnectTo`] trait for any pair of `AsyncRead` and `AsyncWrite`
/// streams, handling serialization of JSON-RPC messages to/from newline-delimited JSON.
/// This is the standard way to communicate with external processes or network connections.
///
/// # Use Cases
///
/// - **Stdio communication**: Connect to agents or proxies via stdin/stdout
/// - **Network sockets**: TCP, Unix domain sockets, or other stream-based protocols
/// - **Named pipes**: Cross-process communication on the same machine
/// - **File I/O**: Reading from and writing to file descriptors
///
/// # Example
///
/// Connecting to an agent via stdio:
///
/// ```no_run
/// use agent_client_protocol::UntypedRole;
/// # use agent_client_protocol::{ByteStreams};
/// use tokio_util::compat::{TokioAsyncReadCompatExt, TokioAsyncWriteCompatExt};
///
/// # async fn example() -> Result<(), agent_client_protocol::Error> {
/// let component = ByteStreams::new(
///     tokio::io::stdout().compat_write(),
///     tokio::io::stdin().compat(),
/// );
///
/// // Use as a component in a connection
/// agent_client_protocol::UntypedRole.builder()
///     .name("my-client")
///     .connect_to(component)
///     .await?;
/// # Ok(())
/// # }
/// ```
///
/// [`ConnectTo`]: crate::ConnectTo
#[derive(Debug)]
pub struct ByteStreams<OB, IB> {
    outgoing: OB,
    incoming: IB,
}

impl<OB, IB> ByteStreams<OB, IB>
where
    OB: AsyncWrite + Send + 'static,
    IB: AsyncRead + Send + 'static,
{
    /// Create a new byte stream transport.
    pub fn new(outgoing: OB, incoming: IB) -> Self {
        Self { outgoing, incoming }
    }

    fn into_lines(
        self,
    ) -> Lines<
        impl futures::Sink<String, Error = std::io::Error> + Send + 'static,
        impl futures::Stream<Item = std::io::Result<String>> + Send + 'static,
    > {
        use futures::AsyncBufReadExt;
        use futures::io::BufReader;
        let Self { outgoing, incoming } = self;

        let incoming_lines = Box::pin(BufReader::new(incoming).lines());
        let outgoing_lines =
            futures::sink::unfold(Box::pin(outgoing), async move |mut writer, line: String| {
                write_line(&mut writer, line).await?;
                Ok::<_, std::io::Error>(writer)
            });

        Lines::new(outgoing_lines, incoming_lines)
    }
}

pub(crate) async fn write_line<W>(writer: &mut W, line: String) -> std::io::Result<()>
where
    W: AsyncWrite + Unpin + ?Sized,
{
    use futures::AsyncWriteExt as _;

    let mut bytes = line.into_bytes();
    bytes.push(b'\n');
    writer.write_all(&bytes).await?;
    writer.flush().await
}

impl<OB, IB, R: Role> ConnectTo<R> for ByteStreams<OB, IB>
where
    OB: AsyncWrite + Send + 'static,
    IB: AsyncRead + Send + 'static,
{
    async fn connect_to(self, client: impl ConnectTo<R::Counterpart>) -> Result<(), crate::Error> {
        ConnectTo::<R>::connect_to(self.into_lines(), client).await
    }

    fn into_channel_and_future(self) -> (Channel, BoxFuture<'static, Result<(), crate::Error>>) {
        ConnectTo::<R>::into_channel_and_future(self.into_lines())
    }
}

/// A channel endpoint representing one side of a bidirectional JSON-RPC transport.
///
/// A channel carries complete TransportFrame values, preserving batch boundaries
/// across in-process components and transport adapters. Malformed wire input is an
/// explicit frame; failures while driving a physical transport are returned by that
/// transport's future.
///
/// # Example
///
/// ```no_run
/// # use agent_client_protocol::UntypedRole;
/// # use agent_client_protocol::Channel;
/// # async fn example() -> Result<(), agent_client_protocol::Error> {
/// let (channel_a, _channel_b) = Channel::duplex();
///
/// UntypedRole.builder()
///     .name("connection-a")
///     .connect_to(channel_a)
///     .await?;
/// # Ok(())
/// # }
/// ```
#[derive(Debug)]
pub struct Channel {
    /// Receives frames from the counterpart.
    pub rx: mpsc::UnboundedReceiver<TransportFrame>,
    /// Sends frames to the counterpart.
    pub tx: mpsc::UnboundedSender<TransportFrame>,
}

impl Channel {
    /// Create a pair of connected channel endpoints.
    ///
    /// Frames sent through either endpoint are received by the other endpoint.
    #[must_use]
    pub fn duplex() -> (Self, Self) {
        let (a_tx, b_rx) = mpsc::unbounded();
        let (b_tx, a_rx) = mpsc::unbounded();

        (Self { rx: a_rx, tx: a_tx }, Self { rx: b_rx, tx: b_tx })
    }

    /// Copy frames from `rx` to `tx` until the input closes.
    ///
    /// # Errors
    ///
    /// Returns an error if the receiving endpoint closes before the input.
    pub(crate) async fn copy(mut self) -> Result<(), crate::Error> {
        while let Some(frame) = self.rx.next().await {
            self.tx
                .unbounded_send(frame)
                .map_err(crate::util::internal_error)?;
        }
        Ok(())
    }

    /// Bridge two endpoints while inspecting every valid message.
    ///
    /// Observers are invoked in source order, including for each valid member of
    /// a batch. The original frame is forwarded unchanged after inspection.
    ///
    /// # Errors
    ///
    /// Returns an observer error or an error if a destination closes before its
    /// source.
    pub async fn bridge_with_inspection(
        left: Self,
        right: Self,
        mut left_to_right: impl FnMut(&RawJsonRpcMessage) -> Result<(), crate::Error> + Send,
        mut right_to_left: impl FnMut(&RawJsonRpcMessage) -> Result<(), crate::Error> + Send,
    ) -> Result<(), crate::Error> {
        let Self {
            rx: mut left_rx,
            tx: left_tx,
        } = left;
        let Self {
            rx: mut right_rx,
            tx: right_tx,
        } = right;

        let left_to_right = async move {
            while let Some(frame) = left_rx.next().await {
                frame.inspect_messages(&mut left_to_right)?;
                right_tx
                    .unbounded_send(frame)
                    .map_err(crate::util::internal_error)?;
            }
            Ok::<(), crate::Error>(())
        };
        let right_to_left = async move {
            while let Some(frame) = right_rx.next().await {
                frame.inspect_messages(&mut right_to_left)?;
                left_tx
                    .unbounded_send(frame)
                    .map_err(crate::util::internal_error)?;
            }
            Ok::<(), crate::Error>(())
        };

        futures::try_join!(left_to_right, right_to_left)?;
        Ok(())
    }
}

impl<R: Role> ConnectTo<R> for Channel {
    async fn connect_to(self, client: impl ConnectTo<R::Counterpart>) -> Result<(), crate::Error> {
        let (client_channel, client_future) = client.into_channel_and_future();

        let ((), (), ()) = futures::try_join!(
            Channel {
                rx: client_channel.rx,
                tx: self.tx,
            }
            .copy(),
            Channel {
                rx: self.rx,
                tx: client_channel.tx,
            }
            .copy(),
            client_future,
        )?;
        Ok(())
    }

    fn into_channel_and_future(self) -> (Channel, BoxFuture<'static, Result<(), crate::Error>>) {
        (self, Box::pin(future::ready(Ok(()))))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(feature = "unstable_protocol_v2")]
    fn connection_with_task_receiver() -> (
        ConnectionTo<crate::role::UntypedRole>,
        mpsc::UnboundedReceiver<Task>,
    ) {
        let (message_tx, _message_rx) = mpsc::unbounded();
        let (task_tx, task_rx) = mpsc::unbounded();
        let (dynamic_handler_tx, _dynamic_handler_rx) = mpsc::unbounded();
        let transport_completion: SharedTransportCompletion =
            future::ready(Ok::<(), crate::Error>(())).boxed().shared();
        let pending_replies = PendingReplies::default();

        (
            ConnectionTo::new(
                crate::role::UntypedRole,
                message_tx,
                task_tx,
                dynamic_handler_tx,
                transport_completion,
                pending_replies.registrar(),
                ProtocolMode::disabled(),
            ),
            task_rx,
        )
    }

    #[cfg(feature = "unstable_protocol_v2")]
    #[test]
    fn v2_builder_exposes_typed_context_to_user_callbacks() {
        fn assert_v2_context(_connection: &V2ConnectionTo<Agent>) {}

        let _builder = Client
            .v2()
            .on_receive_request(
                async |_request: UntypedMessage, _responder, connection| {
                    assert_v2_context(&connection);
                    Ok(())
                },
                crate::on_receive_request!(),
            )
            .on_receive_notification(
                async |_notification: UntypedMessage, connection| {
                    assert_v2_context(&connection);
                    Ok(())
                },
                crate::on_receive_notification!(),
            )
            .on_receive_dispatch(
                async |_dispatch: Dispatch<UntypedMessage, UntypedMessage>, connection| {
                    assert_v2_context(&connection);
                    Ok(())
                },
                crate::on_receive_dispatch!(),
            )
            .on_receive_request_from(
                Agent,
                async |_request: UntypedMessage, _responder, connection| {
                    assert_v2_context(&connection);
                    Ok(())
                },
                crate::on_receive_request!(),
            )
            .on_receive_notification_from(
                Agent,
                async |_notification: UntypedMessage, connection| {
                    assert_v2_context(&connection);
                    Ok(())
                },
                crate::on_receive_notification!(),
            )
            .on_receive_dispatch_from(
                Agent,
                async |_dispatch: Dispatch<UntypedMessage, UntypedMessage>, connection| {
                    assert_v2_context(&connection);
                    Ok(())
                },
                crate::on_receive_dispatch!(),
            )
            .with_spawned(async |connection| {
                assert_v2_context(&connection);
                Ok(())
            })
            .on_close(async |connection| {
                assert_v2_context(&connection);
                Ok(())
            });
    }

    #[cfg(feature = "unstable_protocol_v2")]
    #[test]
    fn proxy_builders_select_exact_proxy_protocol_guards() -> Result<(), crate::Error> {
        use crate::schema::ProtocolVersion;

        for (mode, selected, unsupported) in [
            (
                Proxy.builder().protocol_mode,
                ProtocolVersion::V1,
                ProtocolVersion::V2,
            ),
            (
                Proxy.v2().protocol_mode,
                ProtocolVersion::V2,
                ProtocolVersion::V1,
            ),
        ] {
            assert_eq!(mode.api_protocol_version(), Some(selected));

            let error = ProtocolCompat::new(mode)
                .incoming_message(UntypedMessage::new(
                    "_proxy/initialize",
                    serde_json::json!({ "protocolVersion": unsupported }),
                )?)
                .expect_err("a proxy builder must reject the other protocol version");
            let data = error
                .data
                .as_ref()
                .and_then(|data| data.as_str())
                .unwrap_or_default();
            assert!(
                data.contains(&format!("only supports ACP protocol version {selected}")),
                "{error:?}"
            );
        }

        Ok(())
    }

    #[cfg(feature = "unstable_protocol_v2")]
    #[test]
    fn v2_proxy_rejects_explicitly_prewrapped_initialize_request() {
        let (message_tx, message_rx) = mpsc::unbounded();
        let (task_tx, _task_rx) = mpsc::unbounded();
        let (dynamic_handler_tx, _dynamic_handler_rx) = mpsc::unbounded();
        let transport_completion: SharedTransportCompletion =
            future::ready(Ok::<(), crate::Error>(())).boxed().shared();
        let pending_replies = PendingReplies::default();
        let connection = ConnectionTo::new(
            crate::Conductor,
            message_tx,
            task_tx,
            dynamic_handler_tx,
            transport_completion,
            pending_replies.registrar(),
            ProtocolMode::v2_proxy(),
        );

        let request = crate::schema::SuccessorMessage {
            message: UntypedMessage::new(
                "initialize",
                serde_json::json!({ "protocolVersion": crate::schema::ProtocolVersion::V1 }),
            )
            .expect("test initialize request should serialize"),
            meta: None,
        };
        let sent = connection.send_request_to(Agent, request);

        let (transport_tx, mut transport_rx) = mpsc::unbounded();
        let mut actor = Box::pin(outgoing_actor::outgoing_protocol_actor(
            message_rx,
            pending_replies,
            transport_tx,
            ProtocolCompat::new(ProtocolMode::v2_proxy()),
        ));
        assert!(
            actor.as_mut().now_or_never().is_none(),
            "the outgoing actor should continue after rejecting the request"
        );
        assert!(
            transport_rx.next().now_or_never().is_none(),
            "an explicitly prewrapped initialize must not reach the transport"
        );

        let error = futures::executor::block_on(sent.block_task())
            .expect_err("connection routing must own successor wrapping");
        let data = error
            .data
            .as_ref()
            .and_then(|data| data.as_str())
            .unwrap_or_default();
        assert!(data.contains("logical `initialize`"), "{error:?}");
        assert!(data.contains("_proxy/successor"), "{error:?}");
    }

    #[cfg(feature = "unstable_protocol_v2")]
    #[test]
    fn v2_proxy_builder_exposes_typed_context_to_user_callbacks() {
        fn assert_v2_context(_connection: &V2ConnectionTo<crate::Conductor>) {}

        let _builder = Proxy
            .v2()
            .on_receive_request_from(
                Client,
                async |_request: UntypedMessage, _responder, connection| {
                    assert_v2_context(&connection);
                    Ok(())
                },
                crate::on_receive_request!(),
            )
            .on_receive_notification_from(
                Agent,
                async |_notification: UntypedMessage, connection| {
                    assert_v2_context(&connection);
                    Ok(())
                },
                crate::on_receive_notification!(),
            )
            .on_receive_dispatch_from(
                Client,
                async |_dispatch: Dispatch<UntypedMessage, UntypedMessage>, connection| {
                    assert_v2_context(&connection);
                    Ok(())
                },
                crate::on_receive_dispatch!(),
            )
            .with_spawned(async |connection| {
                assert_v2_context(&connection);
                Ok(())
            })
            .on_close(async |connection| {
                assert_v2_context(&connection);
                Ok(())
            });
    }

    #[cfg(feature = "unstable_protocol_v2")]
    #[test]
    fn raw_connection_spawns_v2_builder_with_typed_child_callback() {
        let (parent, mut task_rx) = connection_with_task_receiver();
        let (transport, _peer) = Channel::duplex();
        let (callback_tx, callback_rx) = oneshot::channel();

        let child: ConnectionTo<Agent> = parent
            .spawn_connection::<Client>(
                Client
                    .v2()
                    .with_spawned(async move |_connection: V2ConnectionTo<Agent>| {
                        callback_tx.send(()).map_err(|()| {
                            crate::util::internal_error("typed child callback receiver was dropped")
                        })
                    }),
                transport,
            )
            .expect("v2 child connection should be spawned");

        let task = futures::FutureExt::now_or_never(futures::StreamExt::next(&mut task_rx))
            .expect("child connection task should already be queued")
            .expect("parent task queue should remain open");
        futures::executor::block_on(async {
            match future::select(Box::pin(task.run_for_test()), Box::pin(callback_rx)).await {
                Either::Right((Ok(()), child_task)) => drop(child_task),
                Either::Right((Err(error), _)) => {
                    panic!("typed child callback sender was dropped: {error}")
                }
                Either::Left((result, _)) => {
                    panic!("child connection stopped before its typed callback ran: {result:?}")
                }
            }
        });

        drop(child);
    }

    #[cfg(feature = "unstable_protocol_v2")]
    #[test]
    fn raw_connection_can_return_v2_context_for_spawned_builder() {
        let (parent, mut task_rx) = connection_with_task_receiver();
        let (transport, _peer) = Channel::duplex();

        let child: V2ConnectionTo<Agent> = parent
            .spawn_connection_with_context(Client.v2(), transport)
            .expect("v2 child connection should be spawned");

        let child_task = futures::FutureExt::now_or_never(futures::StreamExt::next(&mut task_rx))
            .expect("child connection task should already be queued")
            .expect("parent task queue should remain open");

        drop((child, child_task));
    }

    fn connection_with_dynamic_handler_receiver() -> (
        ConnectionTo<crate::role::UntypedRole>,
        mpsc::UnboundedReceiver<DynamicHandlerMessage<crate::role::UntypedRole>>,
    ) {
        let (message_tx, _message_rx) = mpsc::unbounded();
        let (task_tx, _task_rx) = mpsc::unbounded();
        let (dynamic_handler_tx, dynamic_handler_rx) = mpsc::unbounded();
        let transport_completion: SharedTransportCompletion =
            future::ready(Ok::<(), crate::Error>(())).boxed().shared();
        let pending_replies = PendingReplies::default();

        (
            ConnectionTo::new(
                crate::role::UntypedRole,
                message_tx,
                task_tx,
                dynamic_handler_tx,
                transport_completion,
                pending_replies.registrar(),
                ProtocolMode::disabled(),
            ),
            dynamic_handler_rx,
        )
    }

    struct ClaimingDynamicHandler;

    impl HandleDispatchFrom<crate::role::UntypedRole> for ClaimingDynamicHandler {
        fn handle_dispatch_from(
            &mut self,
            _message: Dispatch,
            _connection: ConnectionTo<crate::role::UntypedRole>,
        ) -> impl Future<Output = Result<Handled<Dispatch>, crate::Error>> + Send {
            future::ready(Ok(Handled::Yes))
        }

        fn describe_chain(&self) -> impl Debug {
            "ClaimingDynamicHandler"
        }
    }

    fn connection_for_response_hook_tests() -> (
        ConnectionTo<crate::role::UntypedRole>,
        mpsc::UnboundedReceiver<OutgoingMessage>,
        PendingReplies,
    ) {
        let (message_tx, message_rx) = mpsc::unbounded();
        let (task_tx, _task_rx) = mpsc::unbounded();
        let (dynamic_handler_tx, _dynamic_handler_rx) = mpsc::unbounded();
        let transport_completion: SharedTransportCompletion =
            future::ready(Ok::<(), crate::Error>(())).boxed().shared();
        let pending_replies = PendingReplies::default();

        (
            ConnectionTo::new(
                crate::role::UntypedRole,
                message_tx,
                task_tx,
                dynamic_handler_tx,
                transport_completion,
                pending_replies.registrar(),
                ProtocolMode::disabled(),
            ),
            message_rx,
            pending_replies,
        )
    }

    #[cfg(feature = "unstable_protocol_v2")]
    fn route_test_response(
        request_id: RequestId,
        pending_replies: &PendingReplies,
        result: Result<serde_json::Value, crate::Error>,
    ) {
        let pending_reply = pending_replies
            .remove(&request_id)
            .expect("the request should have a pending reply");
        let (dispatch, _) =
            incoming_actor::dispatch_from_response(request_id, pending_reply, result);
        let Dispatch::Response(result, router) = dispatch else {
            panic!("expected a response dispatch");
        };
        router
            .route_with_result(result)
            .expect("response should route to the pending request");
    }

    #[cfg(feature = "unstable_protocol_v2")]
    #[test]
    fn response_hook_runs_when_success_is_routed_before_consumption() {
        let (connection, _message_rx, pending_replies) = connection_for_response_hook_tests();
        let hook_ran = Arc::new(AtomicBool::new(false));
        let sent = connection.send_request_to_with_response_hook_after(
            crate::role::UntypedRole,
            UntypedMessage::new("hooked", serde_json::json!({}))
                .expect("test request should serialize"),
            future::ready(Ok(())),
            {
                let hook_ran = hook_ran.clone();
                move |response| {
                    assert_eq!(response, &serde_json::json!({"ok": true}));
                    hook_ran.store(true, Ordering::Release);
                    Ok(())
                }
            },
        );
        let request_id = sent.id().clone();

        route_test_response(
            request_id,
            &pending_replies,
            Ok(serde_json::json!({"ok": true})),
        );

        assert!(hook_ran.load(Ordering::Acquire));
        assert_eq!(
            futures::executor::block_on(sent.block_task())
                .expect("routed response should remain consumable"),
            serde_json::json!({"ok": true})
        );
    }

    #[cfg(feature = "unstable_protocol_v2")]
    #[test]
    fn response_hook_skips_errors_but_outlives_a_dropped_consumer() {
        let (connection, _message_rx, pending_replies) = connection_for_response_hook_tests();
        let peer_error_hook_ran = Arc::new(AtomicBool::new(false));
        let peer_error = connection.send_request_to_with_response_hook_after(
            crate::role::UntypedRole,
            UntypedMessage::new("peer-error", serde_json::json!({}))
                .expect("test request should serialize"),
            future::ready(Ok(())),
            {
                let hook_ran = peer_error_hook_ran.clone();
                move |_| {
                    hook_ran.store(true, Ordering::Release);
                    Ok(())
                }
            },
        );
        let peer_error_id = peer_error.id().clone();
        route_test_response(
            peer_error_id,
            &pending_replies,
            Err(crate::Error::invalid_request()),
        );
        assert!(
            futures::executor::block_on(peer_error.block_task()).is_err(),
            "the peer error should reach the consumer"
        );
        assert!(!peer_error_hook_ran.load(Ordering::Acquire));

        let dropped_hook_ran = Arc::new(AtomicBool::new(false));
        let dropped = connection.send_request_to_with_response_hook_after(
            crate::role::UntypedRole,
            UntypedMessage::new("dropped", serde_json::json!({}))
                .expect("test request should serialize"),
            future::ready(Ok(())),
            {
                let hook_ran = dropped_hook_ran.clone();
                move |_| {
                    hook_ran.store(true, Ordering::Release);
                    Ok(())
                }
            },
        );
        let dropped_id = dropped.id().clone();
        drop(dropped);
        route_test_response(
            dropped_id,
            &pending_replies,
            Ok(serde_json::json!({"ok": true})),
        );
        assert!(dropped_hook_ran.load(Ordering::Acquire));
    }

    #[cfg(feature = "unstable_protocol_v2")]
    #[test]
    fn response_hook_failure_replaces_the_success_result() {
        let (connection, _message_rx, pending_replies) = connection_for_response_hook_tests();
        let sent = connection.send_request_to_with_response_hook_after(
            crate::role::UntypedRole,
            UntypedMessage::new("hook-failure", serde_json::json!({}))
                .expect("test request should serialize"),
            future::ready(Ok(())),
            |_| Err(crate::Error::internal_error().data("response hook failed")),
        );
        let request_id = sent.id().clone();
        route_test_response(
            request_id,
            &pending_replies,
            Ok(serde_json::json!({"ok": true})),
        );

        let error = futures::executor::block_on(sent.block_task())
            .expect_err("the hook failure should replace the successful response");
        assert_eq!(error.code, crate::ErrorCode::InternalError);
        assert_eq!(error.data, Some(serde_json::json!("response hook failed")));
    }

    #[test]
    fn ordered_request_waits_for_readiness_before_publication() {
        let (connection, message_rx, pending_replies) = connection_for_response_hook_tests();
        let (ready_tx, ready_rx) = oneshot::channel();
        let sent = connection.send_ordered_request_to_after(
            crate::role::UntypedRole,
            UntypedMessage::new("after-ready", serde_json::json!({}))
                .expect("test request should serialize"),
            async move { ready_rx.await.map_err(crate::Error::into_internal_error) },
        );

        let (transport_tx, mut transport_rx) = mpsc::unbounded();
        let mut actor = Box::pin(outgoing_actor::outgoing_protocol_actor(
            message_rx,
            pending_replies,
            transport_tx,
            ProtocolCompat::new(ProtocolMode::disabled()),
        ));

        assert!(
            actor.as_mut().now_or_never().is_none(),
            "the outgoing actor should wait for readiness"
        );
        assert!(
            transport_rx.next().now_or_never().is_none(),
            "the request must not be published before readiness"
        );

        ready_tx
            .send(())
            .expect("the readiness receiver should remain active");
        assert!(
            actor.as_mut().now_or_never().is_none(),
            "the outgoing actor should continue serving after publication"
        );
        let frame = transport_rx
            .next()
            .now_or_never()
            .expect("the ready request should be published")
            .expect("the transport queue should remain open");
        assert!(matches!(
            frame,
            TransportFrame::Single(RawJsonRpcMessage::Request(_))
        ));

        drop(sent);
    }

    #[test]
    fn ordered_blocking_transform_precedes_response_acknowledgment() {
        let (connection, _message_rx, pending_replies) = connection_for_response_hook_tests();
        let sent = connection.send_ordered_request_to(
            crate::role::UntypedRole,
            UntypedMessage::new("ordered-transform", serde_json::json!({}))
                .expect("test request should serialize"),
        );
        let request_id = sent.id().clone();
        let pending_reply = pending_replies
            .remove(&request_id)
            .expect("the request should have a pending reply");
        let (dispatch, response_dispatch) = incoming_actor::dispatch_from_response(
            request_id,
            pending_reply,
            Err(crate::Error::invalid_params()),
        );
        let Dispatch::Response(result, router) = dispatch else {
            panic!("expected a response dispatch");
        };
        router
            .route_with_result(result)
            .expect("response should route to the pending request");
        let acknowledgment = response_dispatch
            .complete()
            .expect("an ordered response should wait for acknowledgment");
        let acknowledgment = Arc::new(Mutex::new(Some(acknowledgment)));
        let acknowledgment_probe = acknowledgment.clone();

        let error =
            futures::executor::block_on(sent.block_task_with_ordered_result(move |result| {
                assert_eq!(
                    acknowledgment_probe
                        .lock()
                        .expect("acknowledgment mutex poisoned")
                        .as_mut()
                        .expect("acknowledgment receiver should remain available")
                        .try_recv()
                        .expect("acknowledgment sender should remain open"),
                    None,
                    "the ordered response was acknowledged before its transform"
                );
                result
            }))
            .expect_err("the peer error should survive the ordered transform");
        assert_eq!(error.code, crate::ErrorCode::InvalidParams);

        let acknowledgment = acknowledgment
            .lock()
            .expect("acknowledgment mutex poisoned")
            .take()
            .expect("acknowledgment receiver should remain available");
        futures::executor::block_on(acknowledgment)
            .expect("the transform should release the ordered response");
    }

    #[cfg(feature = "unstable_protocol_v2")]
    #[test]
    fn outgoing_request_readiness_failure_rejects_without_publication() {
        let (connection, message_rx, pending_replies) = connection_for_response_hook_tests();
        let hook_ran = Arc::new(AtomicBool::new(false));
        let sent = connection.send_request_to_with_response_hook_after(
            crate::role::UntypedRole,
            UntypedMessage::new("never-published", serde_json::json!({}))
                .expect("test request should serialize"),
            future::ready(Err(crate::Error::internal_error().data("readiness failed"))),
            {
                let hook_ran = hook_ran.clone();
                move |_| {
                    hook_ran.store(true, Ordering::Release);
                    Ok(())
                }
            },
        );

        let (transport_tx, mut transport_rx) = mpsc::unbounded();
        let mut actor = Box::pin(outgoing_actor::outgoing_protocol_actor(
            message_rx,
            pending_replies,
            transport_tx,
            ProtocolCompat::new(ProtocolMode::disabled()),
        ));

        assert!(
            actor.as_mut().now_or_never().is_none(),
            "the outgoing actor should continue serving after rejecting the request"
        );
        assert!(
            transport_rx.next().now_or_never().is_none(),
            "a request whose readiness failed must not be published"
        );
        let error = futures::executor::block_on(sent.block_task())
            .expect_err("the readiness error should reach the request consumer");
        assert_eq!(error.code, crate::ErrorCode::InternalError);
        assert_eq!(error.data, Some(serde_json::json!("readiness failed")));
        assert!(!hook_ran.load(Ordering::Acquire));
    }

    #[test]
    fn ordered_request_is_marked_before_entering_outgoing_queue() {
        let (message_tx, mut message_rx) = mpsc::unbounded();
        let (task_tx, mut task_rx) = mpsc::unbounded();
        let (dynamic_handler_tx, _dynamic_handler_rx) = mpsc::unbounded();
        let transport_completion: SharedTransportCompletion =
            future::ready(Ok::<(), crate::Error>(())).boxed().shared();
        let pending_replies = PendingReplies::default();
        let connection = ConnectionTo::new(
            crate::role::UntypedRole,
            message_tx,
            task_tx,
            dynamic_handler_tx,
            transport_completion,
            pending_replies.registrar(),
            ProtocolMode::disabled(),
        );

        let sent = connection.send_ordered_request_to(
            crate::role::UntypedRole,
            UntypedMessage::new("ordered", serde_json::json!({}))
                .expect("test request should serialize"),
        );
        let request_id = sent.id().clone();
        let message = futures::FutureExt::now_or_never(futures::StreamExt::next(&mut message_rx))
            .expect("outgoing request should already be queued")
            .expect("outgoing request queue should remain open");
        let OutgoingMessage::Request { id, .. } = message else {
            panic!("expected an outgoing request");
        };
        assert_eq!(id, request_id);

        let pending_reply = pending_replies
            .remove(&request_id)
            .expect("the request should have a pending reply");
        assert!(
            pending_reply.ordering.is_ordered(),
            "the response ordering barrier must be installed before publication"
        );

        // Route the response before the callback is registered. The pre-set
        // ordering marker must hold dispatch until the callback task is
        // subsequently installed and completes.
        let (dispatch, response_dispatch) = incoming_actor::dispatch_from_response(
            request_id,
            pending_reply,
            Ok(serde_json::json!({"ok": true})),
        );
        let Dispatch::Response(result, router) = dispatch else {
            panic!("expected a response dispatch");
        };
        router
            .route_with_result(result)
            .expect("response should route to the pending request");
        let acknowledgment = response_dispatch
            .complete()
            .expect("an ordered response should require acknowledgment");

        let callback_ran = Arc::new(AtomicBool::new(false));
        sent.on_receiving_result({
            let callback_ran = callback_ran.clone();
            async move |result| {
                assert_eq!(result?, serde_json::json!({"ok": true}));
                callback_ran.store(true, Ordering::Release);
                Ok(())
            }
        })
        .expect("ordered callback should be scheduled");

        let task = futures::FutureExt::now_or_never(futures::StreamExt::next(&mut task_rx))
            .expect("callback task should already be queued")
            .expect("callback task queue should remain open");
        futures::executor::block_on(task.run_for_test()).expect("callback task should succeed");
        futures::executor::block_on(acknowledgment)
            .expect("callback completion should acknowledge dispatch");
        assert!(callback_ran.load(Ordering::Acquire));
    }

    fn next_dynamic_handler_message<Counterpart: Role>(
        receiver: &mut mpsc::UnboundedReceiver<DynamicHandlerMessage<Counterpart>>,
    ) -> Option<DynamicHandlerMessage<Counterpart>> {
        futures::FutureExt::now_or_never(futures::StreamExt::next(receiver))
            .expect("dynamic-handler receiver should be ready")
    }

    #[cfg(feature = "unstable_protocol_v2")]
    #[test]
    fn v2_dynamic_handler_guard_registers_and_removes_handler() {
        let (message_tx, _message_rx) = mpsc::unbounded();
        let (task_tx, _task_rx) = mpsc::unbounded();
        let (dynamic_handler_tx, mut dynamic_handler_rx) = mpsc::unbounded();
        let transport_completion: SharedTransportCompletion =
            future::ready(Ok::<(), crate::Error>(())).boxed().shared();
        let pending_replies = PendingReplies::default();
        let connection = V2ConnectionTo {
            inner: ConnectionTo::new(
                Agent,
                message_tx,
                task_tx,
                dynamic_handler_tx,
                transport_completion,
                pending_replies.registrar(),
                ProtocolMode::v2_client(),
            ),
        };

        let guard = connection
            .add_dynamic_handler(NullHandler)
            .expect("v2 dynamic handler should register");
        let added_uuid = match next_dynamic_handler_message(&mut dynamic_handler_rx) {
            Some(DynamicHandlerMessage::AddDynamicHandler(uuid, _)) => uuid,
            other => panic!("expected v2 handler registration, got {other:?}"),
        };

        drop(guard);

        match next_dynamic_handler_message(&mut dynamic_handler_rx) {
            Some(DynamicHandlerMessage::RemoveDynamicHandler(uuid)) => {
                assert_eq!(uuid, added_uuid);
            }
            other => panic!("expected v2 handler removal, got {other:?}"),
        }
    }

    #[test]
    fn dropping_dynamic_handler_guard_unregisters_handler() {
        let (connection, mut receiver) = connection_with_dynamic_handler_receiver();
        let guard = connection.add_dynamic_handler(NullHandler).unwrap();

        let added_uuid = match next_dynamic_handler_message(&mut receiver) {
            Some(DynamicHandlerMessage::AddDynamicHandler(uuid, _)) => uuid,
            other => panic!("expected handler registration, got {other:?}"),
        };

        drop(guard);

        match next_dynamic_handler_message(&mut receiver) {
            Some(DynamicHandlerMessage::RemoveDynamicHandler(uuid)) => {
                assert_eq!(uuid, added_uuid);
            }
            other => panic!("expected handler removal, got {other:?}"),
        }
    }

    #[test]
    fn dropping_dynamic_handler_guard_deactivates_queued_handler_immediately() {
        let (connection, mut receiver) = connection_with_dynamic_handler_receiver();
        let guard = connection
            .add_dynamic_handler(ClaimingDynamicHandler)
            .expect("dynamic handler should register");
        let mut handler = match next_dynamic_handler_message(&mut receiver) {
            Some(DynamicHandlerMessage::AddDynamicHandler(_, handler)) => handler,
            other => panic!("expected handler registration, got {other:?}"),
        };

        drop(guard);

        let message = Dispatch::Notification(
            UntypedMessage::new("stale", serde_json::json!({}))
                .expect("test notification should serialize"),
        );
        let handled =
            futures::executor::block_on(handler.dyn_handle_dispatch_from(message, connection))
                .expect("inactive handler should decline cleanly");
        assert!(matches!(handled, Handled::No { retry: false, .. }));
    }

    #[test]
    fn dynamic_handler_barrier_acknowledges_prior_messages() {
        let (connection, mut receiver) = connection_with_dynamic_handler_receiver();
        let _guard = connection.add_dynamic_handler(NullHandler).unwrap();
        let mut barrier = Box::pin(connection.dynamic_handler_barrier());

        assert!(matches!(
            next_dynamic_handler_message(&mut receiver),
            Some(DynamicHandlerMessage::AddDynamicHandler(_, _))
        ));
        assert!(
            barrier.as_mut().now_or_never().is_none(),
            "the barrier must wait for the incoming actor"
        );

        let acknowledgment = match next_dynamic_handler_message(&mut receiver) {
            Some(DynamicHandlerMessage::AcknowledgedBarrier(acknowledgment)) => acknowledgment,
            other => panic!("expected acknowledged barrier, got {other:?}"),
        };
        acknowledgment
            .send(())
            .expect("the barrier receiver should remain active");
        futures::executor::block_on(barrier)
            .expect("the acknowledged dynamic-handler barrier should complete");
    }

    #[test]
    fn detaching_dynamic_handler_guard_does_not_leak_connection() {
        let (connection, mut receiver) = connection_with_dynamic_handler_receiver();
        let guard = connection.add_dynamic_handler(NullHandler).unwrap();

        assert!(matches!(
            next_dynamic_handler_message(&mut receiver),
            Some(DynamicHandlerMessage::AddDynamicHandler(_, _))
        ));

        drop(connection);
        guard.detach();

        assert!(
            next_dynamic_handler_message(&mut receiver).is_none(),
            "detach should retain the handler without retaining a connection sender"
        );
    }

    #[tokio::test]
    async fn write_line_flushes_buffered_writers() {
        let mut writer =
            futures::io::BufWriter::with_capacity(4096, futures::io::Cursor::new(Vec::new()));

        write_line(&mut writer, "message".into()).await.unwrap();

        assert_eq!(writer.into_inner().into_inner(), b"message\n");
    }

    #[test]
    fn peel_successor_envelopes_returns_plain_messages_unchanged() {
        let params = serde_json::json!({ "key": "value" });
        let (method, peeled) = peel_successor_envelopes("session/update", &params);
        assert_eq!(method, "session/update");
        assert_eq!(peeled, &params);
    }

    #[test]
    fn peel_successor_envelopes_unwraps_nested_envelopes() {
        let params = serde_json::json!({
            "method": "_proxy/successor",
            "params": {
                "method": "$/cancel_request",
                "params": { "requestId": "req-1" }
            }
        });
        let (method, peeled) = peel_successor_envelopes("_proxy/successor", &params);
        assert_eq!(method, "$/cancel_request");
        assert_eq!(peeled, &serde_json::json!({ "requestId": "req-1" }));
    }

    #[test]
    fn peel_successor_envelopes_leaves_malformed_envelopes_intact() {
        // No string `method` field: the envelope cannot be peeled, so the
        // message is returned as-is for the handler chain to deal with.
        let params = serde_json::json!({ "unexpected": true });
        let (method, peeled) = peel_successor_envelopes("_proxy/successor", &params);
        assert_eq!(method, "_proxy/successor");
        assert_eq!(peeled, &params);
    }

    mod cancel_request {
        use super::super::*;

        fn notification(method: &str, params: serde_json::Value) -> UntypedMessage {
            UntypedMessage::new(method, params).expect("well-formed JSON")
        }

        #[test]
        fn cancellation_request_id_is_extracted_from_wrapped_notifications() {
            let message = notification(
                "_proxy/successor",
                serde_json::json!({
                    "method": "$/cancel_request",
                    "params": { "requestId": "req-1" }
                }),
            );
            let request_id = cancellation_request_id_from_message(&message)
                .expect("wrapped cancel should parse");
            assert_eq!(request_id, Some(RequestId::Str("req-1".into())));
        }

        #[test]
        fn malformed_successor_envelope_is_not_treated_as_cancellation() {
            // The envelope cannot be peeled; the message must flow on to the
            // handler chain instead of erroring the dispatch.
            let message = notification("_proxy/successor", serde_json::json!({ "bogus": true }));
            let request_id = cancellation_request_id_from_message(&message)
                .expect("malformed envelope should be left to the handler chain");
            assert_eq!(request_id, None);
        }

        #[test]
        fn cancel_request_notifications_are_detected_even_when_wrapped() {
            let plain = notification("$/cancel_request", serde_json::json!({ "requestId": 1 }));
            assert!(is_cancel_request_notification(&plain));

            let wrapped = notification(
                "_proxy/successor",
                serde_json::json!({
                    "method": "$/cancel_request",
                    "params": { "requestId": 1 }
                }),
            );
            assert!(is_cancel_request_notification(&wrapped));

            let other_wrapped = notification(
                "_proxy/successor",
                serde_json::json!({
                    "method": "session/update",
                    "params": {}
                }),
            );
            assert!(!is_cancel_request_notification(&other_wrapped));

            let malformed_envelope =
                notification("_proxy/successor", serde_json::json!({ "bogus": true }));
            assert!(!is_cancel_request_notification(&malformed_envelope));
        }

        #[test]
        fn malformed_cancel_request_params_error() {
            let message = notification(
                "$/cancel_request",
                serde_json::json!({ "requestId": { "not": "an id" } }),
            );
            cancellation_request_id_from_message(&message)
                .expect_err("malformed cancel params should error");
        }

        #[test]
        fn registry_marks_and_removes_requests() {
            let registry = RequestCancellationRegistry::new();
            let id = RequestId::Str("req-1".into());

            let responder_cancellation = registry.register(&id);
            let marker = responder_cancellation.cancellation();
            assert!(!marker.is_cancelled());

            assert!(registry.cancel(&id));
            assert!(marker.is_cancelled());
            assert!(responder_cancellation.cancellation().is_cancelled());

            drop(responder_cancellation);
            assert!(!registry.cancel(&id), "slot should be removed on drop");
        }

        #[test]
        fn reused_request_id_does_not_cross_wire_cancellation_state() {
            let registry = RequestCancellationRegistry::new();
            let id = RequestId::Str("dup".into());

            // A protocol-violating peer reuses an in-flight request ID.
            let first = registry.register(&id);
            let first_marker = first.cancellation();
            let second = registry.register(&id);
            let second_marker = second.cancellation();

            // A cancellation targets whichever request currently owns the ID.
            assert!(registry.cancel(&id));
            assert!(second_marker.is_cancelled());
            assert!(
                !first_marker.is_cancelled(),
                "the stale request must not observe the newer request's cancellation"
            );

            // The stale responder must hand out detached markers, not the
            // newer request's marker.
            assert!(!first.cancellation().is_cancelled());

            // Dropping the stale responder must not remove the newer
            // request's slot.
            drop(first);
            assert!(registry.cancel(&id), "newer slot should still be present");

            drop(second);
            assert!(!registry.cancel(&id), "slot should be removed on drop");
        }
    }
}
