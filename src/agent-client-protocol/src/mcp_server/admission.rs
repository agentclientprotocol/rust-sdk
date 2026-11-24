//! The check that decides whether an `mcp/message` request runs.

use std::{fmt, sync::Arc};

use futures::future::BoxFuture;
use serde_json::{Map, Value};

use crate::v1::schema::{McpRequestId, McpServerAcpId, SessionId, ToolCallId};

/// One `mcp/message` request, before it runs.
///
/// An admission check receives it and decides whether the request is
/// admitted. Register a check for a server with
/// [`McpServer::with_admission`](super::McpServer::with_admission).
///
/// The SDK builds the value for each request. Build one yourself to test a
/// check without a connection:
///
/// ```
/// use agent_client_protocol::mcp_server::{McpAdmission, McpRejection};
///
/// async fn needs_session(request: McpAdmission) -> Result<(), McpRejection> {
///     request.require_session_id()?;
///     Ok(())
/// }
///
/// # futures::executor::block_on(async {
/// let request = McpAdmission::new("server-1", "request-1", "tools/call");
/// assert_eq!(
///     needs_session(request.clone()).await,
///     Err(McpRejection::session_required())
/// );
/// assert_eq!(needs_session(request.with_session_id("sess_1")).await, Ok(()));
/// # });
/// ```
#[derive(Clone)]
pub struct McpAdmission {
    // Shared, so that handing the request to each check copies no params.
    parts: Arc<Parts>,
}

/// The parts of a request that a check reads.
#[derive(Clone)]
struct Parts {
    server_id: McpServerAcpId,
    request_id: McpRequestId,
    method: String,
    params: Option<Map<String, Value>>,
    session_id: Option<SessionId>,
    tool_call_id: Option<ToolCallId>,
}

// The inner params can hold tool arguments, so `Debug` leaves them out.
impl fmt::Debug for McpAdmission {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("McpAdmission")
            .field("server_id", &self.parts.server_id)
            .field("request_id", &self.parts.request_id)
            .field("method", &self.parts.method)
            .field("session_id", &self.parts.session_id)
            .field("tool_call_id", &self.parts.tool_call_id)
            .finish_non_exhaustive()
    }
}

impl McpAdmission {
    /// Builds a request to `method` of the server `server_id`, with no inner
    /// params, no session, and no tool call.
    #[must_use]
    pub fn new(
        server_id: impl Into<McpServerAcpId>,
        request_id: impl Into<McpRequestId>,
        method: impl Into<String>,
    ) -> Self {
        Self {
            parts: Arc::new(Parts {
                server_id: server_id.into(),
                request_id: request_id.into(),
                method: method.into(),
                params: None,
                session_id: None,
                tool_call_id: None,
            }),
        }
    }

    /// Sets the inner MCP params.
    #[must_use]
    pub fn with_params(mut self, params: Map<String, Value>) -> Self {
        Arc::make_mut(&mut self.parts).params = Some(params);
        self
    }

    /// Sets the session the request was sent for.
    #[must_use]
    pub fn with_session_id(mut self, session_id: impl Into<SessionId>) -> Self {
        Arc::make_mut(&mut self.parts).session_id = Some(session_id.into());
        self
    }

    /// Sets the tool call the request is part of.
    #[must_use]
    pub fn with_tool_call_id(mut self, tool_call_id: impl Into<ToolCallId>) -> Self {
        Arc::make_mut(&mut self.parts).tool_call_id = Some(tool_call_id.into());
        self
    }

    /// The request a server runs once it is admitted.
    pub(super) fn from_request(
        server_id: McpServerAcpId,
        request_id: McpRequestId,
        method: String,
        params: Option<Map<String, Value>>,
        session_id: Option<SessionId>,
        tool_call_id: Option<ToolCallId>,
    ) -> Self {
        Self {
            parts: Arc::new(Parts {
                server_id,
                request_id,
                method,
                params,
                session_id,
                tool_call_id,
            }),
        }
    }

    /// Gives the inner method and params back to the server that runs them.
    /// Copies them only if a check kept the request.
    pub(super) fn into_method_and_params(self) -> (String, Option<Map<String, Value>>) {
        match Arc::try_unwrap(self.parts) {
            Ok(parts) => (parts.method, parts.params),
            Err(shared) => (shared.method.clone(), shared.params.clone()),
        }
    }

    /// The declared server the request is addressed to.
    #[must_use]
    pub fn server_id(&self) -> &McpServerAcpId {
        &self.parts.server_id
    }

    /// The logical identifier of the request.
    #[must_use]
    pub fn request_id(&self) -> &McpRequestId {
        &self.parts.request_id
    }

    /// The inner MCP method, such as `tools/call`.
    #[must_use]
    pub fn method(&self) -> &str {
        &self.parts.method
    }

    /// The inner MCP params, including the request's `_meta`.
    #[must_use]
    pub fn params(&self) -> Option<&Map<String, Value>> {
        self.parts.params.as_ref()
    }

    /// The name of the tool a `tools/call` request calls.
    ///
    /// Returns `None` for every other method, and for a `tools/call` request
    /// whose params have no `name` string.
    #[must_use]
    pub fn tool_name(&self) -> Option<&str> {
        if self.method() != "tools/call" {
            return None;
        }
        self.params()?.get("name")?.as_str()
    }

    /// The session the Agent says this request was sent for.
    ///
    /// Returns `None` when the Agent supplied no session. For a child
    /// session's work this is the child's ID. The SDK does not check the
    /// value.
    #[must_use]
    pub fn session_id(&self) -> Option<&SessionId> {
        self.parts.session_id.as_ref()
    }

    /// The tool call, in the session returned by `session_id`, that the Agent
    /// says this request is part of.
    ///
    /// Returns `None` when the Agent supplied no tool call. The Agent can
    /// report the tool call after it sends the request, so the ID might not be
    /// known yet.
    #[must_use]
    pub fn tool_call_id(&self) -> Option<&ToolCallId> {
        self.parts.tool_call_id.as_ref()
    }

    /// Returns the session the Agent says this request was sent for, for a
    /// check that needs one.
    ///
    /// This only reads the request. The request is rejected when the check
    /// returns the error.
    ///
    /// # Errors
    ///
    /// Returns [`McpRejection::session_required`] when the Agent supplied no
    /// session.
    pub fn require_session_id(&self) -> Result<&SessionId, McpRejection> {
        self.session_id().ok_or_else(McpRejection::session_required)
    }
}

/// Why an `mcp/message` request is not admitted.
///
/// An admission check returns it. The SDK then answers the request with the
/// outer ACP error Invalid params (`-32602`), with this reason as the `data`
/// string, and runs nothing else for the request.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct McpRejection {
    reason: String,
}

impl McpRejection {
    /// The server needs the session of a request, and the Agent supplied none.
    ///
    /// The `data` string is
    /// `this MCP server requires sessionId on mcp/message`.
    #[must_use]
    pub fn session_required() -> Self {
        Self {
            reason: "this MCP server requires sessionId on mcp/message".to_owned(),
        }
    }

    /// The server's own check of the request's session failed.
    ///
    /// Use it for a session that is not active on the connection and is not a
    /// child the Agent announced for an active session, or that this server
    /// was not declared to. The `data` string is
    /// `sessionId rejected: {reason}`. `reason` is sent to the Agent.
    #[must_use]
    pub fn session_rejected(reason: impl fmt::Display) -> Self {
        Self {
            reason: format!("sessionId rejected: {reason}"),
        }
    }

    /// The outer ACP error that answers the rejected request.
    pub(super) fn into_error(self) -> crate::Error {
        crate::Error::invalid_params().data(self.reason)
    }
}

impl fmt::Display for McpRejection {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.reason)
    }
}

impl std::error::Error for McpRejection {}

/// One admission check, stored without the type of its closure.
trait Check: Send + Sync {
    fn admit(&self, request: McpAdmission) -> BoxFuture<'_, Result<(), McpRejection>>;
}

/// A check with the function that boxes its future. The future can borrow
/// what the check captured.
struct AsyncCheck<F, ToFut> {
    admit: F,
    to_future_hack: ToFut,
}

impl<F, ToFut> Check for AsyncCheck<F, ToFut>
where
    F: Send + Sync,
    ToFut: for<'a> Fn(&'a F, McpAdmission) -> BoxFuture<'a, Result<(), McpRejection>> + Send + Sync,
{
    fn admit(&self, request: McpAdmission) -> BoxFuture<'_, Result<(), McpRejection>> {
        (self.to_future_hack)(&self.admit, request)
    }
}

/// Admission checks, in the order they were added.
#[derive(Clone, Default)]
pub(super) struct AdmissionChecks(Vec<Arc<dyn Check>>);

impl AdmissionChecks {
    pub(super) fn push<F>(
        &mut self,
        admit: F,
        to_future_hack: impl for<'a> Fn(&'a F, McpAdmission) -> BoxFuture<'a, Result<(), McpRejection>>
        + Send
        + Sync
        + 'static,
    ) where
        F: AsyncFn(McpAdmission) -> Result<(), McpRejection> + Send + Sync + 'static,
    {
        self.0.push(Arc::new(AsyncCheck {
            admit,
            to_future_hack,
        }));
    }

    /// Runs the checks one after the other and returns the first rejection.
    /// The checks after it do not run.
    pub(super) async fn admit(&self, request: &McpAdmission) -> Result<(), McpRejection> {
        for check in &self.0 {
            check.admit(request.clone()).await?;
        }
        Ok(())
    }
}

impl fmt::Debug for AdmissionChecks {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AdmissionChecks")
            .field("checks", &self.0.len())
            .finish()
    }
}
