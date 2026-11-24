//! Reusable application services with request-scoped execution authority.

use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
};

use futures::{
    channel::oneshot,
    future::{BoxFuture, FutureExt, Shared},
};
use serde_json::{Map, Value};

use super::McpConnectionTo;
use crate::{
    Error, RequestCancellation, Role,
    v1::schema::{McpError, McpRequestId, McpServerAcpId, SessionId, ToolCallId},
};

/// One MCP invocation against a reusable service.
#[derive(Debug)]
pub struct McpRequest {
    /// MCP method, without an implicit initialize or discovery exchange.
    pub method: String,
    /// Named parameters, including validated request metadata.
    pub params: Option<Map<String, Value>>,
}

/// An MCP outcome, distinct from failure of the ACP binding.
#[derive(Debug)]
pub enum McpOutcome {
    /// Opaque successful MCP result.
    Result(Value),
    /// Unmodified MCP error, including absent/null data and extension fields.
    Error(McpError),
}

type Notify = dyn Fn(String, Option<Map<String, Value>>) -> BoxFuture<'static, Result<(), Error>>
    + Send
    + Sync;

/// Cancellation from the caller, provider removal, or connection shutdown.
#[derive(Clone)]
pub struct McpOperationCancellation {
    state: Arc<CancellationState>,
}

struct CancellationState {
    cancelled: AtomicBool,
    sender: Mutex<Option<oneshot::Sender<()>>>,
    signal: Shared<BoxFuture<'static, ()>>,
}

impl std::fmt::Debug for McpOperationCancellation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("McpOperationCancellation")
            .field("cancelled", &self.is_cancelled())
            .finish()
    }
}

impl McpOperationCancellation {
    pub(crate) fn new() -> Self {
        let (sender, receiver) = oneshot::channel();
        Self {
            state: Arc::new(CancellationState {
                cancelled: AtomicBool::new(false),
                sender: Mutex::new(Some(sender)),
                signal: receiver.map(|_| ()).boxed().shared(),
            }),
        }
    }

    pub(crate) fn cancel(&self) {
        self.state.cancelled.store(true, Ordering::Release);
        drop(
            self.state
                .sender
                .lock()
                .expect("MCP cancellation poisoned")
                .take(),
        );
    }

    /// Wait until the operation loses its output authority.
    pub async fn cancelled(&self) {
        self.state.signal.clone().await;
    }

    /// Whether cancellation has been requested.
    #[must_use]
    pub fn is_cancelled(&self) -> bool {
        self.state.cancelled.load(Ordering::Acquire)
    }
}

/// Authority for a single operation, not for the lifetime of its service.
#[derive(Clone)]
pub struct McpRequestContext<Counterpart: Role> {
    server_id: McpServerAcpId,
    request_id: McpRequestId,
    connection: McpConnectionTo<Counterpart>,
    metadata: Map<String, Value>,
    cancellation: RequestCancellation,
    operation_cancellation: McpOperationCancellation,
    notify: Arc<Notify>,
}

impl<Counterpart: Role> std::fmt::Debug for McpRequestContext<Counterpart> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("McpRequestContext")
            .field("server_id", &self.server_id)
            .field("request_id", &self.request_id)
            .field("metadata", &self.metadata)
            .field("operation_cancellation", &self.operation_cancellation)
            .finish_non_exhaustive()
    }
}

impl<Counterpart: Role> McpRequestContext<Counterpart> {
    pub(crate) fn new(
        server_id: McpServerAcpId,
        request_id: McpRequestId,
        connection: McpConnectionTo<Counterpart>,
        metadata: Map<String, Value>,
        cancellation: RequestCancellation,
        operation_cancellation: McpOperationCancellation,
        notify: Arc<Notify>,
    ) -> Self {
        Self {
            server_id,
            request_id,
            connection,
            metadata,
            cancellation,
            operation_cancellation,
            notify,
        }
    }

    /// Server identifier bound to this invocation.
    pub fn server_id(&self) -> &McpServerAcpId {
        &self.server_id
    }
    /// Logical request identifier, held until cleanup and terminal output finish.
    pub fn request_id(&self) -> &McpRequestId {
        &self.request_id
    }
    /// The session the Agent says this request was sent for.
    ///
    /// Returns `None` for a standalone MCP connection, and when the Agent
    /// supplied no session. For a child session's work this is the child's ID.
    ///
    /// The SDK does not check the value. Before you use it to choose what a
    /// request acts on, check that the session is active on the connection, or
    /// is a child that the Agent announced for an active session, and that this
    /// server was declared to it or to one of its ancestors.
    pub fn session_id(&self) -> Option<&SessionId> {
        self.connection.session_id()
    }
    /// The tool call, in the session returned by `session_id`, that the Agent
    /// says this request is part of.
    ///
    /// Returns `None` for a standalone MCP connection, and when the Agent
    /// supplied no tool call. The Agent can report the tool call after it
    /// sends the request, so the ID might not be known yet.
    pub fn tool_call_id(&self) -> Option<&ToolCallId> {
        self.connection.tool_call_id()
    }
    /// Returns the session the Agent says this request was sent for, or
    /// rejects the request when the Agent supplied none.
    ///
    /// Call it before the request does any work, in a service that needs the
    /// session to run a request. Like [`session_id`](Self::session_id), it
    /// does not check the value.
    ///
    /// # Errors
    ///
    /// When the request has no `sessionId`, this records a rejection of the
    /// `mcp/message` request and returns it: Invalid params (`-32602`) with a
    /// `data` string that says the server requires `sessionId`. Return the
    /// error from [`McpService::execute`].
    ///
    /// The response to the `mcp/message` request is then the recorded error,
    /// as the outer ACP error. It replaces whatever the service returned: a
    /// result, an MCP error, or another `Err`. The first rejection recorded
    /// for a request is the one sent.
    ///
    /// A rejection only replaces the outcome of an operation that ran to its
    /// end. A request that was stopped before the service returned is
    /// answered as if no rejection was recorded, for example with Request
    /// cancelled (`-32800`) when the Agent cancelled it, or with `-33001`
    /// when its server registration was removed.
    ///
    /// The rejection is shared with [`connection`](Self::connection), so
    /// [`McpConnectionTo::require_session_id`] has the same effect.
    pub fn require_session_id(&self) -> Result<&SessionId, Error> {
        self.connection.require_session_id()
    }
    /// Rejects this request because your own check of its session failed.
    ///
    /// Use it when the request names a session that is not active on the
    /// connection and is not a child the Agent announced for an active
    /// session, or a session that this server was not declared to. Call it
    /// before the request does any work, and do not run the request for
    /// another session instead.
    ///
    /// Records a rejection of the `mcp/message` request and returns it:
    /// Invalid params (`-32602`) with the `data` string
    /// `sessionId rejected: {reason}`. `reason` is sent to the Agent. Return
    /// the error from [`McpService::execute`]. The response to the request is
    /// then that error, as described for
    /// [`require_session_id`](Self::require_session_id).
    #[must_use = "return the error from `McpService::execute` so the operation ends"]
    pub fn reject_session(&self, reason: impl std::fmt::Display) -> Error {
        self.connection.reject_session(reason)
    }
    /// Host connection for application tools.
    pub fn connection(&self) -> &McpConnectionTo<Counterpart> {
        &self.connection
    }
    /// Validated protocol version and client capability metadata.
    pub fn metadata(&self) -> &Map<String, Value> {
        &self.metadata
    }
    /// Cancellation of the outer ACP request.
    pub fn cancellation(&self) -> &RequestCancellation {
        &self.cancellation
    }
    /// Cancellation including provider removal and transport EOF.
    pub fn operation_cancellation(&self) -> &McpOperationCancellation {
        &self.operation_cancellation
    }

    /// Send an operation-scoped notification while this invocation is live.
    ///
    /// # Errors
    ///
    /// Returns Request cancelled (`-32800`) if the operation has finished
    /// or was cancelled. Otherwise propagates any failure to send the notification.
    pub async fn send_notification(
        &self,
        method: impl Into<String>,
        params: Option<Map<String, Value>>,
    ) -> Result<(), Error> {
        if self.cancellation.is_cancelled() || self.operation_cancellation.is_cancelled() {
            return Err(Error::request_cancelled());
        }
        (self.notify)(method.into(), params).await
    }
}

/// Reusable MCP application state with owned invocation futures.
pub trait McpService<Counterpart: Role>: Send + Sync + 'static {
    /// Execute one invocation, including backend teardown.
    ///
    /// On operation cancellation, stop user work and complete owned cleanup
    /// before returning. The binding drives this future through cancellation;
    /// dropping it is not used as a substitute for joining cleanup.
    ///
    /// # Errors
    ///
    /// An `Err` from an operation that ran to its end is reported to the
    /// Agent as an MCP backend failure (`-33002`) with the error's text as
    /// `data`, whatever the error's own code is. To answer Invalid params
    /// (`-32602`) for a missing or refused session, use
    /// [`McpRequestContext::require_session_id`] or
    /// [`McpRequestContext::reject_session`].
    fn execute(
        &self,
        request: McpRequest,
        context: McpRequestContext<Counterpart>,
    ) -> BoxFuture<'static, Result<McpOutcome, Error>>;
}
