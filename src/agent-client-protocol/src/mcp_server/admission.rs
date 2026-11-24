//! The check that decides whether an `mcp/message` request runs.

use std::{fmt, sync::Arc};

use serde_json::{Map, Value};

use crate::v1::schema::{McpRequestId, McpServerAcpId, SessionId, ToolCallId};

/// One `mcp/message` request, before it runs.
///
/// An admission check receives it and decides whether the request is
/// admitted. Register a check for a server with
/// [`McpServer::with_admission`](super::McpServer::with_admission).
pub struct McpAdmission<'a> {
    server_id: &'a McpServerAcpId,
    request_id: &'a McpRequestId,
    method: &'a str,
    params: Option<&'a Map<String, Value>>,
    session_id: Option<&'a SessionId>,
    tool_call_id: Option<&'a ToolCallId>,
}

// The inner params can hold tool arguments, so `Debug` leaves them out.
impl fmt::Debug for McpAdmission<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("McpAdmission")
            .field("server_id", &self.server_id)
            .field("request_id", &self.request_id)
            .field("method", &self.method)
            .field("session_id", &self.session_id)
            .field("tool_call_id", &self.tool_call_id)
            .finish_non_exhaustive()
    }
}

impl<'a> McpAdmission<'a> {
    pub(super) fn new(
        server_id: &'a McpServerAcpId,
        request_id: &'a McpRequestId,
        method: &'a str,
        params: Option<&'a Map<String, Value>>,
        session_id: Option<&'a SessionId>,
        tool_call_id: Option<&'a ToolCallId>,
    ) -> Self {
        Self {
            server_id,
            request_id,
            method,
            params,
            session_id,
            tool_call_id,
        }
    }

    /// The declared server the request is addressed to.
    #[must_use]
    pub fn server_id(&self) -> &McpServerAcpId {
        self.server_id
    }

    /// The logical identifier of the request.
    #[must_use]
    pub fn request_id(&self) -> &McpRequestId {
        self.request_id
    }

    /// The inner MCP method, such as `tools/call`.
    #[must_use]
    pub fn method(&self) -> &str {
        self.method
    }

    /// The inner MCP params, including the request's `_meta`.
    #[must_use]
    pub fn params(&self) -> Option<&Map<String, Value>> {
        self.params
    }

    /// The name of the tool a `tools/call` request calls.
    ///
    /// Returns `None` for every other method, and for a `tools/call` request
    /// whose params have no `name` string.
    #[must_use]
    pub fn tool_name(&self) -> Option<&str> {
        if self.method != "tools/call" {
            return None;
        }
        self.params?.get("name")?.as_str()
    }

    /// The session the Agent says this request was sent for.
    ///
    /// Returns `None` when the Agent supplied no session. For a child
    /// session's work this is the child's ID. The SDK does not check the
    /// value.
    #[must_use]
    pub fn session_id(&self) -> Option<&SessionId> {
        self.session_id
    }

    /// The tool call, in the session returned by `session_id`, that the Agent
    /// says this request is part of.
    ///
    /// Returns `None` when the Agent supplied no tool call. The Agent can
    /// report the tool call after it sends the request, so the ID might not be
    /// known yet.
    #[must_use]
    pub fn tool_call_id(&self) -> Option<&ToolCallId> {
        self.tool_call_id
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
        self.session_id.ok_or_else(McpRejection::session_required)
    }
}

/// Why an `mcp/message` request is not admitted.
///
/// An admission check returns it. The SDK then answers the request with the
/// outer ACP error Invalid params (`-32602`), with this reason as the `data`
/// string, and runs nothing for the request.
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

type Admit = dyn Fn(&McpAdmission<'_>) -> Result<(), McpRejection> + Send + Sync;

/// Admission checks, in the order they were added.
#[derive(Clone, Default)]
pub(super) struct AdmissionChecks(Vec<Arc<Admit>>);

impl AdmissionChecks {
    pub(super) fn push(
        &mut self,
        admit: impl Fn(&McpAdmission<'_>) -> Result<(), McpRejection> + Send + Sync + 'static,
    ) {
        self.0.push(Arc::new(admit));
    }

    /// Runs every check in order and returns the first rejection.
    pub(super) fn admit(&self, request: &McpAdmission<'_>) -> Result<(), McpRejection> {
        self.0.iter().try_for_each(|admit| admit(request))
    }
}

impl fmt::Debug for AdmissionChecks {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AdmissionChecks")
            .field("checks", &self.0.len())
            .finish()
    }
}
