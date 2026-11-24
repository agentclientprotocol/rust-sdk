use crate::{ConnectionTo, role::Role};

#[cfg(feature = "unstable_mcp_over_acp")]
use crate::Error;
#[cfg(feature = "unstable_mcp_over_acp")]
use crate::v1::schema::{McpRequestId, McpServerAcpId, SessionId, ToolCallId};
#[cfg(feature = "unstable_mcp_over_acp")]
use futures::channel::oneshot;
#[cfg(feature = "unstable_mcp_over_acp")]
use std::sync::{Arc, Mutex};

/// Describes how an MCP server connection was established.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum McpConnectionContext {
    /// The MCP server was connected directly, without an ACP transport.
    Standalone,

    /// The MCP server was attached to an ACP session.
    #[cfg(feature = "unstable_mcp_over_acp")]
    #[non_exhaustive]
    Acp {
        /// The identifier advertised in the session's `McpServer::Acp` declaration.
        server_id: McpServerAcpId,

        /// The logical identifier of this independent MCP request.
        request_id: McpRequestId,

        /// The session the Agent says this request was sent for, if it
        /// supplied one. The SDK does not check it.
        session_id: Option<SessionId>,

        /// The tool call in that session that the Agent says this request
        /// is part of, if it supplied one.
        tool_call_id: Option<ToolCallId>,
    },
}

impl McpConnectionContext {
    /// Whether this MCP connection was established without an ACP transport.
    #[must_use]
    pub fn is_standalone(&self) -> bool {
        matches!(self, Self::Standalone)
    }

    /// The identifier advertised in the session's `McpServer::Acp` declaration.
    ///
    /// Returns `None` for a standalone MCP connection.
    #[cfg(feature = "unstable_mcp_over_acp")]
    #[must_use]
    pub fn server_id(&self) -> Option<&McpServerAcpId> {
        match self {
            Self::Standalone => None,
            Self::Acp { server_id, .. } => Some(server_id),
        }
    }

    /// The logical identifier of the active MCP request.
    ///
    /// Returns `None` for a standalone MCP connection.
    #[cfg(feature = "unstable_mcp_over_acp")]
    #[must_use]
    pub fn request_id(&self) -> Option<&McpRequestId> {
        match self {
            Self::Standalone => None,
            Self::Acp { request_id, .. } => Some(request_id),
        }
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
    #[cfg(feature = "unstable_mcp_over_acp")]
    #[must_use]
    pub fn session_id(&self) -> Option<&SessionId> {
        match self {
            Self::Standalone => None,
            Self::Acp { session_id, .. } => session_id.as_ref(),
        }
    }

    /// The tool call, in the session returned by `session_id`, that the Agent
    /// says this request is part of.
    ///
    /// Returns `None` for a standalone MCP connection, and when the Agent
    /// supplied no tool call. The Agent can report the tool call after it
    /// sends the request, so the ID might not be known yet.
    #[cfg(feature = "unstable_mcp_over_acp")]
    #[must_use]
    pub fn tool_call_id(&self) -> Option<&ToolCallId> {
        match self {
            Self::Standalone => None,
            Self::Acp { tool_call_id, .. } => tool_call_id.as_ref(),
        }
    }
}

/// Connection information available to an MCP server.
#[derive(Clone, Debug)]
pub struct McpConnectionTo<Counterpart: Role> {
    pub(super) context: McpConnectionContext,
    pub(super) connection: ConnectionTo<Counterpart>,
    #[cfg(feature = "unstable_mcp_over_acp")]
    pub(super) cleanup: Option<Arc<Mutex<Vec<oneshot::Receiver<()>>>>>,
    /// The rejection recorded for this `mcp/message` request, shared by every
    /// clone of the connection. `None` for a standalone connection.
    #[cfg(feature = "unstable_mcp_over_acp")]
    pub(super) rejection: Option<Arc<Mutex<Option<Error>>>>,
}

impl<Counterpart: Role> McpConnectionTo<Counterpart> {
    /// A connection served directly, without an ACP transport.
    pub(super) fn standalone(connection: ConnectionTo<Counterpart>) -> Self {
        Self {
            context: McpConnectionContext::Standalone,
            connection,
            #[cfg(feature = "unstable_mcp_over_acp")]
            cleanup: None,
            #[cfg(feature = "unstable_mcp_over_acp")]
            rejection: None,
        }
    }

    #[cfg(all(feature = "unstable_mcp_over_acp", feature = "schemars"))]
    pub(crate) fn register_cleanup(&self, done: oneshot::Receiver<()>) {
        if let Some(cleanup) = &self.cleanup {
            cleanup.lock().expect("MCP cleanup poisoned").push(done);
        }
    }

    #[cfg(feature = "unstable_mcp_over_acp")]
    pub(crate) async fn wait_cleanup(&self) {
        if let Some(cleanup) = &self.cleanup {
            let pending = std::mem::take(&mut *cleanup.lock().expect("MCP cleanup poisoned"));
            for done in pending {
                let _finished = done.await;
            }
        }
    }

    /// Records `error` as the answer to this `mcp/message` request, unless a
    /// rejection is already recorded, and returns it. A standalone connection
    /// has no request to answer, so nothing is recorded.
    #[cfg(feature = "unstable_mcp_over_acp")]
    fn reject(&self, error: Error) -> Error {
        if let Some(rejection) = &self.rejection {
            rejection
                .lock()
                .expect("MCP rejection poisoned")
                .get_or_insert_with(|| error.clone());
        }
        error
    }

    /// Takes the rejection recorded for this request, if there is one.
    #[cfg(feature = "unstable_mcp_over_acp")]
    pub(crate) fn take_rejection(&self) -> Option<Error> {
        self.rejection
            .as_ref()?
            .lock()
            .expect("MCP rejection poisoned")
            .take()
    }

    /// Describes whether this is a standalone or ACP-attached MCP connection.
    #[must_use]
    pub fn context(&self) -> &McpConnectionContext {
        &self.context
    }

    /// The identifier advertised in the session's `McpServer::Acp` declaration.
    ///
    /// Returns `None` for a standalone MCP connection.
    #[cfg(feature = "unstable_mcp_over_acp")]
    #[must_use]
    pub fn server_id(&self) -> Option<&McpServerAcpId> {
        self.context.server_id()
    }

    /// The logical identifier of the active MCP request.
    ///
    /// Returns `None` for a standalone MCP connection.
    #[cfg(feature = "unstable_mcp_over_acp")]
    #[must_use]
    pub fn request_id(&self) -> Option<&McpRequestId> {
        self.context.request_id()
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
    #[cfg(feature = "unstable_mcp_over_acp")]
    #[must_use]
    pub fn session_id(&self) -> Option<&SessionId> {
        self.context.session_id()
    }

    /// The tool call, in the session returned by `session_id`, that the Agent
    /// says this request is part of.
    ///
    /// Returns `None` for a standalone MCP connection, and when the Agent
    /// supplied no tool call. The Agent can report the tool call after it
    /// sends the request, so the ID might not be known yet.
    #[cfg(feature = "unstable_mcp_over_acp")]
    #[must_use]
    pub fn tool_call_id(&self) -> Option<&ToolCallId> {
        self.context.tool_call_id()
    }

    /// Returns the session the Agent says this request was sent for, or
    /// rejects the request when the Agent supplied none.
    ///
    /// Call it before the request does any work, in a server that needs the
    /// session to run a request. Like [`session_id`](Self::session_id), it
    /// does not check the value.
    ///
    /// # Errors
    ///
    /// When the request has no `sessionId`, this records a rejection of the
    /// `mcp/message` request and returns it: Invalid params (`-32602`) with a
    /// `data` string that says the server requires `sessionId`. Return the
    /// error from the tool or service.
    ///
    /// The response to the `mcp/message` request is then the recorded error,
    /// as the outer ACP error. It replaces whatever the tool or service
    /// returned, including the `isError` result that the `Err` of an
    /// `McpServerBuilder` tool becomes. The first rejection recorded for a
    /// request is the one sent.
    ///
    /// A rejection only replaces the outcome of an operation that ran to its
    /// end. A request that was stopped before the tool or service returned is
    /// answered as if no rejection was recorded, for example with Request
    /// cancelled (`-32800`) when the Agent cancelled it, or with `-33001`
    /// when its server registration was removed.
    ///
    /// A standalone MCP connection has no ACP session and no `mcp/message`
    /// request to reject. There, this always returns Invalid params
    /// (`-32602`) with a `data` string that says so, and records nothing.
    /// What the MCP client sees depends on what the server does with the
    /// returned error. An `McpServerBuilder` tool reports it as a tool result
    /// with `isError: true`.
    #[cfg(feature = "unstable_mcp_over_acp")]
    pub fn require_session_id(&self) -> Result<&SessionId, Error> {
        if let Some(session_id) = self.session_id() {
            return Ok(session_id);
        }
        let reason = if self.context.is_standalone() {
            "a standalone MCP connection has no ACP session"
        } else {
            "this MCP server requires sessionId on mcp/message"
        };
        Err(self.reject(Error::invalid_params().data(reason)))
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
    /// the error from the tool or service. The response to the request is
    /// then that error, as described for
    /// [`require_session_id`](Self::require_session_id).
    ///
    /// On a standalone MCP connection this returns the same error and records
    /// nothing.
    #[cfg(feature = "unstable_mcp_over_acp")]
    #[must_use = "return the error from the tool or service so the operation ends"]
    pub fn reject_session(&self, reason: impl std::fmt::Display) -> Error {
        self.reject(Error::invalid_params().data(format!("sessionId rejected: {reason}")))
    }

    /// Borrow the host protocol connection.
    ///
    /// For an ACP-attached server, this is its host ACP connection. For a
    /// standalone server, this is the direct MCP client connection.
    #[must_use]
    pub fn connection(&self) -> &ConnectionTo<Counterpart> {
        &self.connection
    }
}

#[cfg(test)]
mod tests {
    use super::McpConnectionContext;

    #[test]
    fn standalone_context_is_explicit() {
        let context = McpConnectionContext::Standalone;

        assert!(context.is_standalone());

        #[cfg(feature = "unstable_mcp_over_acp")]
        {
            assert_eq!(context.server_id(), None);
            assert_eq!(context.request_id(), None);
            assert_eq!(context.session_id(), None);
            assert_eq!(context.tool_call_id(), None);
        }
    }

    #[cfg(feature = "unstable_mcp_over_acp")]
    #[test]
    fn acp_context_exposes_server_and_request_ids() {
        use crate::v1::schema::{McpRequestId, McpServerAcpId, SessionId, ToolCallId};

        let server_id = McpServerAcpId::new("server-id");
        let request_id = McpRequestId::new("request-id");
        let session_id = SessionId::new("session-id");
        let tool_call_id = ToolCallId::new("tool-call-id");
        let context = McpConnectionContext::Acp {
            server_id: server_id.clone(),
            request_id: request_id.clone(),
            session_id: Some(session_id.clone()),
            tool_call_id: Some(tool_call_id.clone()),
        };

        assert!(!context.is_standalone());
        assert_eq!(context.server_id(), Some(&server_id));
        assert_eq!(context.request_id(), Some(&request_id));
        assert_eq!(context.session_id(), Some(&session_id));
        assert_eq!(context.tool_call_id(), Some(&tool_call_id));
    }

    #[cfg(feature = "unstable_mcp_over_acp")]
    #[test]
    fn standalone_connection_returns_a_rejection_without_recording_it() {
        use super::McpConnectionTo;
        use crate::{Channel, Error, role::mcp};

        let (channel, _peer) = Channel::duplex();
        futures::executor::block_on(mcp::Server.builder().connect_with(
            channel,
            async |connection| {
                let standalone = McpConnectionTo::standalone(connection);
                assert_eq!(
                    standalone.require_session_id(),
                    Err(Error::invalid_params()
                        .data("a standalone MCP connection has no ACP session"))
                );
                assert_eq!(
                    standalone.reject_session("not active"),
                    Error::invalid_params().data("sessionId rejected: not active")
                );
                assert_eq!(standalone.take_rejection(), None);
                Ok(())
            },
        ))
        .unwrap();
    }
}
