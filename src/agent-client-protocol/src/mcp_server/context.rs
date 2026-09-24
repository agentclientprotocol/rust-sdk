use crate::{ConnectionTo, role::Role};

#[cfg(feature = "unstable_mcp_over_acp")]
use crate::schema::v1::{McpRequestId, McpServerAcpId};

/// Describes how an MCP server connection was established.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum McpConnectionContext {
    /// The MCP server was connected directly, without an ACP transport.
    Standalone,

    /// The MCP server was attached to an ACP session.
    #[cfg(feature = "unstable_mcp_over_acp")]
    Acp {
        /// The identifier advertised in the session's `McpServer::Acp` declaration.
        server_id: McpServerAcpId,

        /// The logical identifier of this independent MCP request.
        request_id: McpRequestId,
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
}

/// Connection information available to an MCP server.
#[derive(Clone, Debug)]
pub struct McpConnectionTo<Counterpart: Role> {
    pub(super) context: McpConnectionContext,
    pub(super) connection: ConnectionTo<Counterpart>,
}

impl<Counterpart: Role> McpConnectionTo<Counterpart> {
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
        }
    }

    #[cfg(feature = "unstable_mcp_over_acp")]
    #[test]
    fn acp_context_exposes_server_and_request_ids() {
        use crate::schema::v1::{McpRequestId, McpServerAcpId};

        let server_id = McpServerAcpId::new("server-id");
        let request_id = McpRequestId::new("request-id");
        let context = McpConnectionContext::Acp {
            server_id: server_id.clone(),
            request_id: request_id.clone(),
        };

        assert!(!context.is_standalone());
        assert_eq!(context.server_id(), Some(&server_id));
        assert_eq!(context.request_id(), Some(&request_id));
    }
}
