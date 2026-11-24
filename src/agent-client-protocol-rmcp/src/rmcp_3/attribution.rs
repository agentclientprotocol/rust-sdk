//! The ACP session and tool call of a request, for rmcp handlers.

use ::rmcp_3 as rmcp;
use acp::v1::schema::{SessionId, ToolCallId};
use agent_client_protocol as acp;
use rmcp::{
    ErrorData, RoleServer,
    handler::server::common::{AsRequestContext, FromContextPart},
    service::RequestContext,
};

/// The ACP session and tool call that an MCP request was sent for, for an
/// rmcp handler.
///
/// A `#[tool]` method takes it as a parameter:
///
/// ```
/// # extern crate rmcp_3 as rmcp;
/// use agent_client_protocol_rmcp::rmcp_3::McpRequestAttribution;
/// use rmcp::{model::CallToolResult, tool, tool_router};
/// use serde_json::json;
///
/// struct Notes;
///
/// #[tool_router]
/// impl Notes {
///     #[tool(description = "Lists the notes of the session")]
///     async fn list_notes(&self, attribution: McpRequestAttribution) -> CallToolResult {
///         match attribution.session_id() {
///             Some(session_id) => CallToolResult::structured(json!({ "notesOf": session_id })),
///             None => CallToolResult::structured(json!({ "notes": [] })),
///         }
///     }
/// }
/// ```
///
/// A `ServerHandler` method you write yourself gets it with
/// [`McpRequestAttribution::of`]. That works in any method that receives a
/// `RequestContext`, such as `call_tool`, `list_tools`, or `discover`:
///
/// ```
/// # use rmcp_3 as rmcp;
/// use agent_client_protocol_rmcp::rmcp_3::McpRequestAttribution;
/// use rmcp::{
///     ErrorData, RoleServer, ServerHandler,
///     model::{ListToolsResult, PaginatedRequestParams, ServerCapabilities, ServerConfig},
///     service::RequestContext,
/// };
///
/// struct Notes;
///
/// impl ServerHandler for Notes {
///     fn get_info(&self) -> ServerConfig {
///         ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
///     }
///
///     async fn list_tools(
///         &self,
///         _request: Option<PaginatedRequestParams>,
///         context: RequestContext<RoleServer>,
///     ) -> Result<ListToolsResult, ErrorData> {
///         let attribution = McpRequestAttribution::of(&context);
///         // List the tools of `attribution.session_id()`.
///         # let _ = attribution.session_id();
///         Ok(ListToolsResult::default())
///     }
/// }
/// ```
///
/// The attribution only reads the request. A server that needs the session,
/// or refuses one, rejects the request before the handler runs, with an
/// admission check: `McpServer::with_admission` in `agent-client-protocol`.
///
/// The SDK does not check the session ID. Before a handler uses it to choose
/// what a request acts on, the server has to check that the session is active
/// on the connection, or is a child that the agent announced for an active
/// session, and that the server was declared to it or to one of its
/// ancestors.
///
/// # Standalone connections
///
/// A standalone MCP connection has no ACP session and no `mcp/message`
/// request, and no admission check runs for it. A `#[tool]` parameter and
/// [`of`](Self::of) then give an attribution without a session and a tool
/// call, so a handler that is also served standalone has to handle `None`.
#[derive(Clone, Debug)]
pub struct McpRequestAttribution {
    pub(super) session_id: Option<SessionId>,
    pub(super) tool_call_id: Option<ToolCallId>,
}

/// Lets a method that rmcp's `#[tool]` macro handles take the attribution as
/// a parameter. It is the value [`McpRequestAttribution::of`] returns for the
/// request, so it has no session on a standalone connection.
impl<C: AsRequestContext> FromContextPart<C> for McpRequestAttribution {
    fn from_context_part(context: &mut C) -> Result<Self, ErrorData> {
        Ok(Self::of(context.as_request_context()))
    }
}

impl McpRequestAttribution {
    /// The attribution of the request that `context` belongs to.
    ///
    /// On a standalone connection it has no session and no tool call.
    #[must_use]
    pub fn of(context: &RequestContext<RoleServer>) -> Self {
        context.extensions.get::<Self>().cloned().unwrap_or(Self {
            session_id: None,
            tool_call_id: None,
        })
    }

    /// The session the agent says this request was sent for.
    ///
    /// Returns `None` when the agent supplied no session, and on a standalone
    /// connection. For a child session's work this is the child's ID.
    #[must_use]
    pub fn session_id(&self) -> Option<&SessionId> {
        self.session_id.as_ref()
    }

    /// The tool call, in the session returned by `session_id`, that the agent
    /// says this request is part of.
    ///
    /// Returns `None` when the agent supplied no tool call, and on a
    /// standalone connection. The agent can report the tool call after it
    /// sends the request, so the ID might not be known yet.
    #[must_use]
    pub fn tool_call_id(&self) -> Option<&ToolCallId> {
        self.tool_call_id.as_ref()
    }
}
