//! The session and tool call of an HTTP MCP request, as the host supplies them.

use std::{
    future::Future,
    panic::{RefUnwindSafe, UnwindSafe},
    sync::Arc,
};

use agent_client_protocol::Error;
use futures::future::BoxFuture;
use serde_json::{Map, Value};

/// The parts of an HTTP MCP request that an attribution resolver sees.
///
/// See [`McpOverAcpPolyfill::with_attribution`](super::McpOverAcpPolyfill::with_attribution).
#[derive(Clone, Debug)]
#[non_exhaustive]
pub struct McpHttpRequestInfo {
    /// The `serverId` of the `McpServer::Acp` declaration the request is
    /// addressed to.
    pub server_id: String,
    /// The inner MCP method, such as `tools/call`.
    pub method: String,
    /// The inner MCP params, including the `_meta` the MCP client sent.
    pub params: Option<Map<String, Value>>,
}

/// The ACP session, and optionally the tool call in it, that an HTTP MCP
/// request was sent for.
///
/// A tool call is only named together with its session, so the session is
/// required.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct McpHttpRequestAttribution {
    pub(super) session_id: String,
    pub(super) tool_call_id: Option<String>,
}

impl McpHttpRequestAttribution {
    /// Names the session whose work caused the request.
    ///
    /// Use the session ID the agent sends on `session/update` for that work.
    /// For a child session's work, that is the child's ID.
    #[must_use]
    pub fn new(session_id: impl ToString) -> Self {
        Self {
            session_id: session_id.to_string(),
            tool_call_id: None,
        }
    }

    /// Names the tool call in that session that the request is part of.
    ///
    /// Use the tool call ID the agent reports in `session/update`.
    #[must_use]
    pub fn tool_call_id(mut self, tool_call_id: impl ToString) -> Self {
        self.tool_call_id = Some(tool_call_id.to_string());
        self
    }
}

type Resolve = dyn Fn(McpHttpRequestInfo) -> BoxFuture<'static, Result<Option<McpHttpRequestAttribution>, Error>>
    + Send
    + Sync;

/// The host's resolver, shared by the v1 and v2 sides of the polyfill.
#[derive(Clone)]
pub(super) struct AttributionResolver(Arc<Resolve>);

impl AttributionResolver {
    pub(super) fn new<F, Fut>(resolver: F) -> Self
    where
        F: Fn(McpHttpRequestInfo) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<Option<McpHttpRequestAttribution>, Error>> + Send + 'static,
    {
        Self(Arc::new(move |request| Box::pin(resolver(request))))
    }

    pub(super) async fn resolve(
        &self,
        request: McpHttpRequestInfo,
    ) -> Result<Option<McpHttpRequestAttribution>, Error> {
        (self.0)(request).await
    }
}

impl std::fmt::Debug for AttributionResolver {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AttributionResolver")
            .finish_non_exhaustive()
    }
}

// `McpOverAcpPolyfill` had these auto traits before it could hold a resolver,
// and a boxed closure would remove them. The polyfill only calls the resolver.
// It keeps no state of its own that a panic in the resolver could leave
// half-updated.
impl UnwindSafe for AttributionResolver {}
impl RefUnwindSafe for AttributionResolver {}
