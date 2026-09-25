//! Request-native application services for MCP-over-ACP.

use std::sync::Arc;

use futures::{
    channel::oneshot,
    future::{BoxFuture, FutureExt, Shared},
};
use serde_json::{Map, Value};

use super::McpConnectionTo;
use crate::{
    Error, RequestCancellation, Role,
    schema::v1::{McpError, McpRequestId, McpServerAcpId},
};

/// One MCP invocation. Its application service may be reused across invocations.
#[derive(Debug)]
pub struct McpRequest {
    /// The MCP method.
    pub method: String,
    /// Its MCP parameters; metadata is validated before dispatch.
    pub params: Option<Map<String, Value>>,
}

/// The MCP outcome is distinct from a failure in the ACP binding itself.
#[derive(Debug)]
pub enum McpOutcome {
    /// Successful, opaque MCP result.
    Result(Value),
    /// An unmodified MCP error object (including optional or explicitly null data).
    Error(McpError),
}

type Notify = dyn Fn(String, Option<Map<String, Value>>) -> BoxFuture<'static, Result<(), Error>>
    + Send
    + Sync;

/// Explicit cancellation of an operation, including provider removal and
/// connection shutdown (which need not cancel the original ACP request).
#[derive(Clone)]
pub struct McpOperationCancellation {
    state: Arc<CancellationState>,
}

impl std::fmt::Debug for McpOperationCancellation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("McpOperationCancellation")
            .field("cancelled", &self.is_cancelled())
            .finish()
    }
}

struct CancellationState {
    cancelled: std::sync::atomic::AtomicBool,
    sender: std::sync::Mutex<Option<oneshot::Sender<()>>>,
    signal: Shared<BoxFuture<'static, ()>>,
}

impl McpOperationCancellation {
    pub(crate) fn new() -> Self {
        let (tx, rx) = oneshot::channel();
        Self {
            state: Arc::new(CancellationState {
                cancelled: std::sync::atomic::AtomicBool::new(false),
                sender: std::sync::Mutex::new(Some(tx)),
                signal: rx.map(|_| ()).boxed().shared(),
            }),
        }
    }

    pub(crate) fn cancel(&self) {
        self.state
            .cancelled
            .store(true, std::sync::atomic::Ordering::Release);
        drop(
            self.state
                .sender
                .lock()
                .expect("MCP cancellation poisoned")
                .take(),
        );
    }

    /// Await cancellation from the caller, provider, or transport.
    pub async fn cancelled(&self) {
        self.state.signal.clone().await;
    }
    /// Whether the operation may still produce output.
    #[must_use]
    pub fn is_cancelled(&self) -> bool {
        self.state
            .cancelled
            .load(std::sync::atomic::Ordering::Acquire)
    }
}

/// Per-operation authority. Notifications are admitted only while this request
/// is live; retaining the service does not retain an operation's output rights.
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

    /// Server identifier bound to this operation.
    pub fn server_id(&self) -> &McpServerAcpId {
        &self.server_id
    }
    /// Logical operation identifier.
    pub fn request_id(&self) -> &McpRequestId {
        &self.request_id
    }
    /// Host connection, available to application tools.
    pub fn connection(&self) -> &McpConnectionTo<Counterpart> {
        &self.connection
    }
    /// Validated MCP metadata, including the negotiated protocol version and
    /// the client's capability declaration.
    pub fn metadata(&self) -> &Map<String, Value> {
        &self.metadata
    }
    /// Request cancellation handle.
    pub fn cancellation(&self) -> &RequestCancellation {
        &self.cancellation
    }
    /// Cancellation for this operation, including provider removal and EOF.
    pub fn operation_cancellation(&self) -> &McpOperationCancellation {
        &self.operation_cancellation
    }

    /// Send a bounded, operation-scoped MCP notification.
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

/// Reusable application service. An invocation owns its returned future; an
/// implementation may deliberately share application state between requests.
pub trait McpService<Counterpart: Role>: Send + Sync + 'static {
    /// Execute one MCP request, returning an owned operation future.
    ///
    /// The future includes backend teardown: on
    /// [`McpRequestContext::operation_cancellation`], stop user work and finish
    /// owned cleanup before returning. The binding keeps admission until this
    /// future completes rather than abandoning cleanup by dropping it. The rmcp
    /// adapter implements this supervision for its handler futures.
    fn execute(
        &self,
        request: McpRequest,
        context: McpRequestContext<Counterpart>,
    ) -> BoxFuture<'static, Result<McpOutcome, Error>>;
}
