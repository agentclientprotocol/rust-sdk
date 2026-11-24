//! MCP tool trait for defining tools.

use schemars::JsonSchema;
use serde::{Serialize, de::DeserializeOwned};

use crate::role::Role;

use super::McpConnectionTo;
#[cfg(feature = "unstable_mcp_over_acp")]
use super::{McpAdmission, McpRejection};

/// Trait for defining MCP tools.
///
/// Implement this trait to create a tool that can be registered with an MCP server.
/// The tool's input and output types must implement JSON Schema for automatic
/// documentation.
///
/// # Example
///
/// ```rust,ignore
/// use agent_client_protocol::mcp_server::{McpConnectionTo, McpTool};
/// use schemars::JsonSchema;
/// use serde::{Deserialize, Serialize};
///
/// #[derive(JsonSchema, Deserialize)]
/// struct EchoInput {
///     message: String,
/// }
///
/// #[derive(JsonSchema, Serialize)]
/// struct EchoOutput {
///     echoed: String,
/// }
///
/// struct EchoTool;
///
/// impl<R: agent_client_protocol::role::Role> McpTool<R> for EchoTool {
///     type Input = EchoInput;
///     type Output = EchoOutput;
///
///     fn name(&self) -> String {
///         "echo".to_string()
///     }
///
///     fn description(&self) -> String {
///         "Echoes back the input message".to_string()
///     }
///
///     async fn call_tool(
///         &self,
///         input: EchoInput,
///         _context: McpConnectionTo<R>,
///     ) -> Result<EchoOutput, agent_client_protocol::Error> {
///         Ok(EchoOutput {
///             echoed: format!("Echo: {}", input.message),
///         })
///     }
/// }
/// ```
pub trait McpTool<R: Role>: Send + Sync {
    /// The type of input the tool accepts.
    type Input: JsonSchema + DeserializeOwned + Send + 'static;

    /// The type of output the tool produces.
    type Output: JsonSchema + Serialize + Send + 'static;

    /// The name of the tool
    fn name(&self) -> String;

    /// A description of what the tool does
    fn description(&self) -> String;

    /// A human-readable title for the tool
    fn title(&self) -> Option<String> {
        None
    }

    /// Decides, before the tool runs, whether a call of it over ACP is
    /// admitted. The default admits every call.
    ///
    /// A server built from a tool registry, such as one from the rmcp crate's
    /// `McpServerBuilder`, awaits this for a `tools/call` request that names
    /// this tool, after the server's own checks
    /// ([`McpServer::with_admission`](super::McpServer::with_admission)). A
    /// rejected call is answered with the outer ACP error Invalid params
    /// (`-32602`), and [`call_tool`](Self::call_tool) is not called.
    ///
    /// The returned future is dropped when the request is cancelled, so it
    /// must not own work that needs cleanup.
    ///
    /// A standalone MCP connection has no `mcp/message` request, so this does
    /// not run there. A tool that is also served standalone still has to
    /// handle a call without a session.
    ///
    /// # Errors
    ///
    /// The future returns the [`McpRejection`] that answers a call the tool
    /// does not admit.
    #[cfg(feature = "unstable_mcp_over_acp")]
    fn admit(
        &self,
        request: McpAdmission,
    ) -> impl Future<Output = Result<(), McpRejection>> + Send {
        drop(request);
        std::future::ready(Ok(()))
    }

    /// Define the tool's behavior. You can implement this with an `async fn`.
    fn call_tool(
        &self,
        input: Self::Input,
        context: McpConnectionTo<R>,
    ) -> impl Future<Output = Result<Self::Output, crate::Error>> + Send;
}
