//! The ACP session and tool call of a request, for rmcp handlers.

use std::{fmt, sync::Arc};

use ::rmcp_3 as rmcp;
use acp::v1::schema::{SessionId, ToolCallId};
use agent_client_protocol as acp;
use rmcp::{
    ErrorData, RoleServer,
    handler::server::common::{AsRequestContext, FromContextPart},
    service::RequestContext,
};

/// The `mcp/message` request behind an attribution. It hides the role of the
/// request's host connection, so a handler does not have to name it.
trait AcpRequest: Send + Sync {
    fn session_id(&self) -> Option<&SessionId>;
    fn tool_call_id(&self) -> Option<&ToolCallId>;
    fn require_session_id(&self) -> Result<&SessionId, acp::Error>;
    fn reject_session(&self, reason: &dyn fmt::Display) -> acp::Error;
}

acp::__if_mcp_over_acp! {
    {
        use acp::{Role, mcp_server::McpConnectionTo};

        impl<R: Role> AcpRequest for McpConnectionTo<R> {
            fn session_id(&self) -> Option<&SessionId> {
                McpConnectionTo::session_id(self)
            }

            fn tool_call_id(&self) -> Option<&ToolCallId> {
                McpConnectionTo::tool_call_id(self)
            }

            fn require_session_id(&self) -> Result<&SessionId, acp::Error> {
                McpConnectionTo::require_session_id(self)
            }

            fn reject_session(&self, reason: &dyn fmt::Display) -> acp::Error {
                McpConnectionTo::reject_session(self, reason)
            }
        }

        impl McpRequestAttribution {
            /// The attribution of the `mcp/message` request that `connection`
            /// belongs to. It records rejections in that request's slot.
            pub(super) fn for_request<R: Role>(connection: McpConnectionTo<R>) -> Self {
                Self {
                    request: Some(Arc::new(connection)),
                }
            }
        }
    } else {}
}

/// The ACP session and tool call that an MCP request was sent for, for an
/// rmcp handler.
///
/// It does not name the role of the server's host connection.
///
/// A `#[tool]` method takes it as a parameter:
///
/// ```
/// # extern crate rmcp_3 as rmcp;
/// use agent_client_protocol_rmcp::rmcp_3::McpRequestAttribution;
/// use rmcp::{ErrorData, model::CallToolResult, tool, tool_router};
/// use serde_json::json;
///
/// struct Notes;
///
/// #[tool_router]
/// impl Notes {
///     #[tool(description = "Lists the notes of the session")]
///     async fn list_notes(
///         &self,
///         attribution: McpRequestAttribution,
///     ) -> Result<CallToolResult, ErrorData> {
///         // Rejects a request that has no session.
///         let session_id = attribution.require_session_id()?;
///         Ok(CallToolResult::structured(json!({ "notesOf": session_id })))
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
///         // Rejects a request that has no session.
///         let session_id = attribution.require_session_id()?;
///         // List the tools of `session_id`.
///         # let _ = session_id;
///         Ok(ListToolsResult::default())
///     }
/// }
/// ```
///
/// The SDK does not check the session ID. Before a handler uses it to choose
/// what a request acts on, check that the session is active on the
/// connection, or is a child that the agent announced for an active session,
/// and that the server was declared to it or to one of its ancestors. Refuse
/// a session that fails the check with
/// [`reject_session`](Self::reject_session).
///
/// # Standalone connections
///
/// A standalone MCP connection has no ACP session and no `mcp/message`
/// request. A `#[tool]` parameter and [`of`](Self::of) then give an
/// attribution without a session and a tool call. Its
/// [`require_session_id`](Self::require_session_id) and
/// [`reject_session`](Self::reject_session) return their errors and reject
/// nothing, so the MCP client receives the error the handler returns.
#[derive(Clone)]
pub struct McpRequestAttribution {
    /// `None` on a standalone connection.
    request: Option<Arc<dyn AcpRequest>>,
}

impl fmt::Debug for McpRequestAttribution {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("McpRequestAttribution")
            .field("session_id", &self.session_id())
            .field("tool_call_id", &self.tool_call_id())
            .finish_non_exhaustive()
    }
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
        context
            .extensions
            .get::<Self>()
            .cloned()
            .unwrap_or(Self { request: None })
    }

    /// The session the agent says this request was sent for.
    ///
    /// Returns `None` when the agent supplied no session, and on a standalone
    /// connection. For a child session's work this is the child's ID.
    #[must_use]
    pub fn session_id(&self) -> Option<&SessionId> {
        self.request.as_deref()?.session_id()
    }

    /// The tool call, in the session returned by `session_id`, that the agent
    /// says this request is part of.
    ///
    /// Returns `None` when the agent supplied no tool call, and on a
    /// standalone connection. The agent can report the tool call after it
    /// sends the request, so the ID might not be known yet.
    #[must_use]
    pub fn tool_call_id(&self) -> Option<&ToolCallId> {
        self.request.as_deref()?.tool_call_id()
    }

    /// Returns the session the agent says this request was sent for, or
    /// rejects the request when the agent supplied none.
    ///
    /// Call it before the request does any work, in a handler that needs the
    /// session to run a request.
    ///
    /// # Errors
    ///
    /// When the request has no `sessionId`, this records a rejection of the
    /// `mcp/message` request and returns an Invalid params (`-32602`) error
    /// to return from the handler with `?`. The agent then receives the outer
    /// ACP error Invalid params (`-32602`), in place of the error the handler
    /// returned. As with `McpConnectionTo::require_session_id`, a request that
    /// was stopped before the handler returned keeps its own error.
    ///
    /// On a standalone connection this always returns an Invalid params
    /// (`-32602`) error whose message says that a standalone MCP connection
    /// has no ACP session, and records nothing.
    pub fn require_session_id(&self) -> Result<&SessionId, ErrorData> {
        match &self.request {
            Some(request) => request.require_session_id().map_err(mcp_error),
            None => Err(ErrorData::invalid_params(
                "a standalone MCP connection has no ACP session",
                None,
            )),
        }
    }

    /// Rejects this request because your own check of its session failed.
    ///
    /// Records a rejection of the `mcp/message` request and returns an
    /// Invalid params (`-32602`) error to return from the handler. The agent
    /// then receives the outer ACP error Invalid params (`-32602`) with the
    /// `data` string `sessionId rejected: {reason}`, in place of the error
    /// the handler returned. `reason` is sent to the agent.
    ///
    /// On a standalone connection this returns the same error and records
    /// nothing.
    #[must_use = "return the error from the handler so the request ends"]
    pub fn reject_session(&self, reason: impl fmt::Display) -> ErrorData {
        match &self.request {
            Some(request) => mcp_error(request.reject_session(&reason)),
            None => ErrorData::invalid_params(format!("sessionId rejected: {reason}"), None),
        }
    }
}

/// The rmcp form of a rejection. The reason becomes the message, which is
/// what MCP clients show.
fn mcp_error(error: acp::Error) -> ErrorData {
    let reason = match error.data {
        Some(serde_json::Value::String(reason)) => reason,
        _ => error.message,
    };
    ErrorData::invalid_params(reason, None)
}
