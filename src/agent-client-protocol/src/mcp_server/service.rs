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
    schema::v1::{McpError, McpRequestId, McpServerAcpId},
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
    fn execute(
        &self,
        request: McpRequest,
        context: McpRequestContext<Counterpart>,
    ) -> BoxFuture<'static, Result<McpOutcome, Error>>;
}
