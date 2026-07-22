//! MCP server builder for creating MCP servers.

use std::{marker::PhantomData, pin::pin, sync::Arc};

use futures::future::{BoxFuture, Either};
use futures_concurrency::future::TryJoin;
use rmcp::{
    ErrorData, ServerHandler,
    model::{CallToolResult, ListToolsResult, Tool},
};
use schemars::JsonSchema;
use serde::{Serialize, de::DeserializeOwned};
use tokio_util::compat::{TokioAsyncReadCompatExt, TokioAsyncWriteCompatExt};

use agent_client_protocol as acp;
use agent_client_protocol::{
    ByteStreams, ChainRun, ConnectTo, DynConnectTo, NullRun, RunWithConnectionTo,
    mcp_server::{
        McpConnectionTo, McpServer, McpServerConnect, McpTool, McpToolMetadata, McpToolRegistry,
    },
    role::{self, Role},
};

/// Builder for creating MCP servers with tools.
///
/// Use [`crate::McpServerExt::builder`] to create a new builder, then chain methods to
/// configure the server and call [`build`](Self::build) to create the server.
///
/// # Example
///
/// ```rust,ignore
/// use agent_client_protocol::mcp_server::McpServer;
/// use agent_client_protocol_rmcp::McpServerExt;
///
/// let server = McpServer::builder("my-server".to_string())
///     .instructions("A helpful assistant")
///     .tool(EchoTool)
///     .tool_fn(
///         "greet",
///         "Greet someone by name",
///         async |input: GreetInput, _cx| Ok(format!("Hello, {}!", input.name)),
///         agent_client_protocol_rmcp::tool_fn!(),
///     )
///     .build();
/// ```
#[derive(Debug)]
pub struct McpServerBuilder<Counterpart: Role, Runner>
where
    Runner: RunWithConnectionTo<Counterpart>,
{
    phantom: PhantomData<Counterpart>,
    name: String,
    data: McpToolRegistry<Counterpart>,
    runner: Runner,
}

impl<Counterpart: Role> McpServerBuilder<Counterpart, NullRun> {
    pub(super) fn new(name: String) -> Self {
        Self {
            name,
            phantom: PhantomData,
            data: McpToolRegistry::default(),
            runner: NullRun,
        }
    }
}

impl<Counterpart: Role, Runner> McpServerBuilder<Counterpart, Runner>
where
    Runner: RunWithConnectionTo<Counterpart>,
{
    /// Set the server instructions that are provided to the client.
    #[must_use]
    pub fn instructions(mut self, instructions: impl ToString) -> Self {
        self.data.set_instructions(instructions);
        self
    }

    /// Add a tool to the server.
    #[must_use]
    pub fn tool(mut self, tool: impl McpTool<Counterpart> + 'static) -> Self {
        self.data.register_tool(tool);
        self
    }

    /// Disable all tools. After calling this, only tools explicitly enabled
    /// with [`enable_tool`](Self::enable_tool) will be available.
    #[must_use]
    pub fn disable_all_tools(mut self) -> Self {
        self.data.disable_all_tools();
        self
    }

    /// Enable all tools. After calling this, all tools will be available
    /// except those explicitly disabled with [`disable_tool`](Self::disable_tool).
    #[must_use]
    pub fn enable_all_tools(mut self) -> Self {
        self.data.enable_all_tools();
        self
    }

    /// Disable a specific tool by name.
    ///
    /// Returns an error if the tool is not registered.
    pub fn disable_tool(mut self, name: &str) -> Result<Self, acp::Error> {
        self.data.disable_tool(name)?;
        Ok(self)
    }

    /// Enable a specific tool by name.
    ///
    /// Returns an error if the tool is not registered.
    pub fn enable_tool(mut self, name: &str) -> Result<Self, acp::Error> {
        self.data.enable_tool(name)?;
        Ok(self)
    }

    /// Private fn: adds the tool but also adds a runner that will be
    /// run while the MCP server is active.
    fn tool_with_runner(
        self,
        tool: impl McpTool<Counterpart> + 'static,
        tool_runner: impl RunWithConnectionTo<Counterpart>,
    ) -> McpServerBuilder<Counterpart, impl RunWithConnectionTo<Counterpart>> {
        let this = self.tool(tool);
        McpServerBuilder {
            phantom: PhantomData,
            name: this.name,
            data: this.data,
            runner: ChainRun::new(this.runner, tool_runner),
        }
    }

    /// Convenience wrapper for defining a "single-threaded" tool without having to create a struct.
    /// By "single-threaded", we mean that only one invocation of the tool can be running at a time.
    /// Typically agents invoke a tool once per session and then block waiting for the result,
    /// so this is fine, but they could attempt to run multiple invocations concurrently, in which
    /// case those invocations would be serialized.
    ///
    /// # Parameters
    ///
    /// * `name`: The name of the tool.
    /// * `description`: The description of the tool.
    /// * `func`: The function that implements the tool. Use an async closure like `async |args, cx| { .. }`.
    ///
    /// # Examples
    ///
    /// ```rust,ignore
    /// McpServer::builder("my-server")
    ///     .tool_fn_mut(
    ///         "greet",
    ///         "Greet someone by name",
    ///         async |input: GreetInput, _cx| Ok(format!("Hello, {}!", input.name)),
    ///     )
    /// ```
    pub fn tool_fn_mut<P, Ret, F>(
        self,
        name: impl ToString,
        description: impl ToString,
        func: F,
        tool_future_hack: impl for<'a> Fn(
            &'a mut F,
            P,
            McpConnectionTo<Counterpart>,
        ) -> BoxFuture<'a, Result<Ret, acp::Error>>
        + Send
        + 'static,
    ) -> McpServerBuilder<Counterpart, impl RunWithConnectionTo<Counterpart>>
    where
        P: JsonSchema + DeserializeOwned + 'static + Send,
        Ret: JsonSchema + Serialize + 'static + Send,
        F: AsyncFnMut(P, McpConnectionTo<Counterpart>) -> Result<Ret, acp::Error> + Send,
    {
        let (tool, runner) =
            acp::mcp_server::tool_fn_mut(name, description, func, tool_future_hack);
        self.tool_with_runner(tool, runner)
    }

    /// Convenience wrapper for defining a stateless tool that can run concurrently.
    /// Unlike [`tool_fn_mut`](Self::tool_fn_mut), multiple invocations of this tool can run
    /// at the same time since the function is `Fn` rather than `FnMut`.
    ///
    /// # Parameters
    ///
    /// * `name`: The name of the tool.
    /// * `description`: The description of the tool.
    /// * `func`: The function that implements the tool. Use an async closure like `async |args, cx| { .. }`.
    ///
    /// # Examples
    ///
    /// ```rust,ignore
    /// McpServer::builder("my-server")
    ///     .tool_fn(
    ///         "greet",
    ///         "Greet someone by name",
    ///         async |input: GreetInput, _cx| Ok(format!("Hello, {}!", input.name)),
    ///     )
    /// ```
    pub fn tool_fn<P, Ret, F>(
        self,
        name: impl ToString,
        description: impl ToString,
        func: F,
        tool_future_hack: impl for<'a> Fn(
            &'a F,
            P,
            McpConnectionTo<Counterpart>,
        ) -> BoxFuture<'a, Result<Ret, acp::Error>>
        + Send
        + Sync
        + 'static,
    ) -> McpServerBuilder<Counterpart, impl RunWithConnectionTo<Counterpart>>
    where
        P: JsonSchema + DeserializeOwned + 'static + Send,
        Ret: JsonSchema + Serialize + 'static + Send,
        F: AsyncFn(P, McpConnectionTo<Counterpart>) -> Result<Ret, acp::Error>
            + Send
            + Sync
            + 'static,
    {
        let (tool, runner) = acp::mcp_server::tool_fn(name, description, func, tool_future_hack);
        self.tool_with_runner(tool, runner)
    }

    /// Create an MCP server from this builder.
    ///
    /// This builder can be served directly. With the `unstable_mcp_over_acp`
    /// feature, it can also be attached through
    /// `SessionBuilder::with_mcp_server` or `Builder::with_mcp_server`.
    pub fn build(self) -> McpServer<Counterpart, Runner> {
        McpServer::new(
            McpServerBuilt {
                name: self.name,
                data: Arc::new(self.data),
            },
            self.runner,
        )
    }
}

struct McpServerBuilt<Counterpart: Role> {
    name: String,
    data: Arc<McpToolRegistry<Counterpart>>,
}

impl<Counterpart: Role> McpServerConnect<Counterpart> for McpServerBuilt<Counterpart> {
    fn name(&self) -> String {
        self.name.clone()
    }

    fn connect(
        &self,
        mcp_connection: McpConnectionTo<Counterpart>,
    ) -> DynConnectTo<role::mcp::Client> {
        DynConnectTo::new(McpServerConnection {
            data: self.data.clone(),
            mcp_connection,
        })
    }
}

/// A connected MCP server instance.
pub(crate) struct McpServerConnection<Counterpart: Role> {
    data: Arc<McpToolRegistry<Counterpart>>,
    mcp_connection: McpConnectionTo<Counterpart>,
}

impl<Counterpart: Role> ConnectTo<role::mcp::Client> for McpServerConnection<Counterpart> {
    async fn connect_to(self, client: impl ConnectTo<role::mcp::Server>) -> Result<(), acp::Error> {
        // Create tokio byte streams that rmcp expects
        let (mcp_server_stream, mcp_client_stream) = tokio::io::duplex(8192);
        let (mcp_server_read, mcp_server_write) = tokio::io::split(mcp_server_stream);
        let (mcp_client_read, mcp_client_write) = tokio::io::split(mcp_client_stream);

        let run_client = async {
            let byte_streams =
                ByteStreams::new(mcp_client_write.compat_write(), mcp_client_read.compat());
            <ByteStreams<_, _> as ConnectTo<role::mcp::Client>>::connect_to(byte_streams, client)
                .await
        };

        let run_server = async {
            // Run the rmcp server with the server side of the duplex stream
            let running_server = rmcp::ServiceExt::serve(self, (mcp_server_read, mcp_server_write))
                .await
                .map_err(acp::Error::into_internal_error)?;

            // Wait for the server to finish
            running_server
                .waiting()
                .await
                .map(|_quit_reason| ())
                .map_err(acp::Error::into_internal_error)
        };

        (run_client, run_server).try_join().await?;
        Ok(())
    }
}

impl<R: Role> ServerHandler for McpServerConnection<R> {
    async fn call_tool(
        &self,
        request: rmcp::model::CallToolRequestParams,
        context: rmcp::service::RequestContext<rmcp::RoleServer>,
    ) -> Result<CallToolResult, ErrorData> {
        // Lookup the tool definition, erroring if not found or disabled
        let Some(registered) = self.data.enabled_tool(&request.name) else {
            return Err(rmcp::model::ErrorData::invalid_params(
                format!("tool `{}` not found", request.name),
                None,
            ));
        };

        // Convert input into JSON
        let serde_value = serde_json::to_value(request.arguments).expect("valid json");

        // Execute the user's tool, unless cancellation occurs
        let has_structured_output = registered.has_structured_output();
        match futures::future::select(
            registered.call_tool(serde_value, self.mcp_connection.clone()),
            pin!(context.ct.cancelled()),
        )
        .await
        {
            // If completed successfully
            Either::Left((m, _)) => match m {
                Ok(result) => {
                    // Use structured output only if the tool declared an output_schema
                    if has_structured_output {
                        Ok(CallToolResult::structured(result))
                    } else {
                        Ok(CallToolResult::success(vec![
                            rmcp::model::ContentBlock::text(result.to_string()),
                        ]))
                    }
                }
                Err(error) => Err(to_rmcp_error(error)),
            },

            // If cancelled
            Either::Right(((), _)) => {
                Err(rmcp::ErrorData::internal_error("operation cancelled", None))
            }
        }
    }

    async fn list_tools(
        &self,
        _request: Option<rmcp::model::PaginatedRequestParams>,
        _context: rmcp::service::RequestContext<rmcp::RoleServer>,
    ) -> Result<rmcp::model::ListToolsResult, ErrorData> {
        // Return only enabled tools
        let tools: Vec<_> = self
            .data
            .enabled_tools()
            .map(|tool| make_tool_model(tool.metadata()))
            .collect();
        Ok(ListToolsResult::with_all_items(tools))
    }

    fn get_info(&self) -> rmcp::model::ServerInfo {
        // Basic server info
        let base = rmcp::model::ServerInfo::new(
            rmcp::model::ServerCapabilities::builder()
                .enable_tools()
                .build(),
        )
        .with_server_info(rmcp::model::Implementation::default())
        .with_protocol_version(rmcp::model::ProtocolVersion::default());

        if let Some(instructions) = self.data.instructions() {
            base.with_instructions(instructions.to_string())
        } else {
            base
        }
    }
}

/// Create an `rmcp` tool model from runtime-neutral MCP tool metadata.
fn make_tool_model(metadata: &McpToolMetadata) -> Tool {
    let mut tool = rmcp::model::Tool::new(
        metadata.name().to_string(),
        metadata.description().to_string(),
        metadata.input_schema().clone(),
    )
    .with_execution(rmcp::model::ToolExecution::new());

    if let Some(title) = metadata.title() {
        tool = tool.with_title(title.to_string());
    }

    if let Some(schema) = metadata.output_schema() {
        tool = tool.with_raw_output_schema(schema.clone());
    }

    tool
}

/// Convert an [`agent_client_protocol::Error`] into an [`rmcp::ErrorData`].
fn to_rmcp_error(error: acp::Error) -> rmcp::ErrorData {
    rmcp::ErrorData {
        code: rmcp::model::ErrorCode(error.code.into()),
        message: error.message.into(),
        data: error.data,
    }
}
