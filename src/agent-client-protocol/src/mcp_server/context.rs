use crate::{ConnectionTo, role::Role};

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
}

impl<Counterpart: Role> McpConnectionTo<Counterpart> {
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
}
