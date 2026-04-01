use crate::{ConnectionTo, role::Role};

/// Context about the ACP and MCP connection available to an MCP server.
#[derive(Clone)]
pub struct McpConnectionTo<Counterpart: Role> {
    pub(super) acp_url: String,
    pub(super) connection: ConnectionTo<Counterpart>,
}

impl<Counterpart: Role> McpConnectionTo<Counterpart> {
    /// The `acp:UUID` that was given.
    pub fn acp_url(&self) -> String {
        self.acp_url.clone()
    }

    /// The host connection context.
    ///
    /// If this MCP server is hosted inside of an ACP context, this will be the ACP connection context.
    pub fn connection_to(&self) -> ConnectionTo<Counterpart> {
        self.connection.clone()
    }
}
