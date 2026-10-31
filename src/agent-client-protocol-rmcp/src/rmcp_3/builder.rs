//! MCP server builder for creating MCP servers.

use std::{future::Future, marker::PhantomData, pin::pin, sync::Arc};

use ::rmcp_3 as rmcp;
use futures::future::{BoxFuture, Either};
use futures_concurrency::future::TryJoin;
use rmcp::{
    ErrorData, ServerHandler,
    model::{
        CacheScope, CallToolResponse, CallToolResult, ListToolsResult, ProtocolVersion,
        ServerConfig, Tool,
    },
};
use schemars::JsonSchema;
use serde::{Serialize, de::DeserializeOwned};
use tokio_util::compat::{TokioAsyncReadCompatExt, TokioAsyncWriteCompatExt};

use agent_client_protocol as acp;
use agent_client_protocol::{
    ByteStreams, ChainRun, ConnectTo, DynConnectTo, NullRun, RunWithConnectionTo,
    mcp_server::{
        McpConnectionTo, McpServer, McpServerConnect, McpTool, McpToolMetadata, McpToolRegistry,
        UnknownToolError,
    },
    role::{self, Role},
};

acp::__if_mcp_over_acp! {
    {
        use acp::mcp_server::{McpOutcome, McpRequest, McpRequestContext, McpService};
    } else {}
}

/// The `serverInfo.version` reported until [`McpServerBuilder::version`] is called.
const DEFAULT_SERVER_VERSION: &str = "0.0.0";

/// Builder for creating MCP servers with tools.
///
/// Use [`McpServerExt::builder`](super::McpServerExt::builder) to create a new builder, then chain methods to
/// configure the server and call [`build`](Self::build) to create the server.
///
/// The server reports the builder's name and [`version`](Self::version) as its
/// MCP `serverInfo`.
///
/// # Tool errors
///
/// - A call to a tool that is not registered or is disabled gets an invalid
///   params JSON-RPC error (`-32602`).
/// - Arguments that do not deserialize into the tool's input type, and errors
///   returned by the tool itself, become a `tools/call` result with
///   `isError: true` and the error text as its content, so the model can see
///   and correct them.
/// - A cancelled call gets an internal JSON-RPC error (`-32603`).
///
/// # Example
///
/// ```
/// use agent_client_protocol::{mcp_server::McpServer, role::mcp};
/// use agent_client_protocol_rmcp::rmcp_3::McpServerExt;
/// use schemars::JsonSchema;
/// use serde::Deserialize;
///
/// #[derive(Deserialize, JsonSchema)]
/// struct GreetInput {
///     name: String,
/// }
///
/// let server = McpServer::<mcp::Client>::builder("my-server")
///     .version("1.0.0")
///     .instructions("A helpful assistant")
///     .tool_fn(
///         "greet",
///         "Greet someone by name",
///         async |input: GreetInput, _cx| Ok(format!("Hello, {}!", input.name)),
///         agent_client_protocol_rmcp::rmcp_3::tool_fn!(),
///     )
///     .build();
/// # drop(server);
/// ```
#[derive(Debug)]
pub struct McpServerBuilder<Counterpart: Role, Runner>
where
    Runner: RunWithConnectionTo<Counterpart>,
{
    phantom: PhantomData<Counterpart>,
    name: String,
    version: String,
    data: McpToolRegistry<Counterpart>,
    runner: Runner,
}

impl<Counterpart: Role> McpServerBuilder<Counterpart, NullRun> {
    pub(super) fn new(name: String) -> Self {
        Self {
            name,
            version: DEFAULT_SERVER_VERSION.to_string(),
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
    /// Set the version reported in the MCP `serverInfo`.
    ///
    /// Defaults to `0.0.0`.
    #[must_use]
    pub fn version(mut self, version: impl ToString) -> Self {
        self.version = version.to_string();
        self
    }

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
    /// # Errors
    ///
    /// Returns [`UnknownToolError`] if no tool with this name has been added.
    pub fn disable_tool(mut self, name: &str) -> Result<Self, UnknownToolError> {
        self.data.disable_tool(name)?;
        Ok(self)
    }

    /// Enable a specific tool by name.
    ///
    /// # Errors
    ///
    /// Returns [`UnknownToolError`] if no tool with this name has been added.
    pub fn enable_tool(mut self, name: &str) -> Result<Self, UnknownToolError> {
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
            version: this.version,
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
    /// ```
    /// use agent_client_protocol::{mcp_server::McpServer, role::mcp};
    /// use agent_client_protocol_rmcp::rmcp_3::McpServerExt;
    /// use schemars::JsonSchema;
    /// use serde::Deserialize;
    ///
    /// #[derive(Deserialize, JsonSchema)]
    /// struct CountInput {
    ///     amount: u32,
    /// }
    ///
    /// let mut total = 0;
    /// let server = McpServer::<mcp::Client>::builder("my-server")
    ///     .tool_fn_mut(
    ///         "count",
    ///         "Add to a running total",
    ///         async move |input: CountInput, _cx| {
    ///             total += input.amount;
    ///             Ok(total)
    ///         },
    ///         agent_client_protocol_rmcp::rmcp_3::tool_fn_mut!(),
    ///     )
    ///     .build();
    /// # drop(server);
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
    /// ```
    /// use agent_client_protocol::{mcp_server::McpServer, role::mcp};
    /// use agent_client_protocol_rmcp::rmcp_3::McpServerExt;
    /// use schemars::JsonSchema;
    /// use serde::Deserialize;
    ///
    /// #[derive(Deserialize, JsonSchema)]
    /// struct GreetInput {
    ///     name: String,
    /// }
    ///
    /// let server = McpServer::<mcp::Client>::builder("my-server")
    ///     .tool_fn(
    ///         "greet",
    ///         "Greet someone by name",
    ///         async |input: GreetInput, _cx| Ok(format!("Hello, {}!", input.name)),
    ///         agent_client_protocol_rmcp::rmcp_3::tool_fn!(),
    ///     )
    ///     .build();
    /// # drop(server);
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
        let built = McpServerBuilt {
            name: self.name,
            version: self.version,
            data: Arc::new(self.data),
        };
        acp::__if_mcp_over_acp! {
            {
                let standalone = McpServerBuilt {
                    name: built.name.clone(),
                    version: built.version.clone(),
                    data: built.data.clone(),
                };
                McpServer::new_service_with_standalone(built, standalone, self.runner)
            } else {
                McpServer::new(built, self.runner)
            }
        }
    }
}

struct McpServerBuilt<Counterpart: Role> {
    name: String,
    version: String,
    data: Arc<McpToolRegistry<Counterpart>>,
}

impl<Counterpart: Role> McpServerBuilt<Counterpart> {
    fn connection(
        &self,
        mcp_connection: McpConnectionTo<Counterpart>,
    ) -> McpServerConnection<Counterpart> {
        McpServerConnection {
            name: self.name.clone(),
            version: self.version.clone(),
            data: self.data.clone(),
            mcp_connection,
        }
    }
}

acp::__if_mcp_over_acp! {
    {
        impl<Counterpart: Role> McpService<Counterpart> for McpServerBuilt<Counterpart> {
            fn execute(
                &self,
                request: McpRequest,
                context: McpRequestContext<Counterpart>,
            ) -> BoxFuture<'static, Result<McpOutcome, acp::Error>> {
                super::native::execute(
                    Arc::new(self.connection(context.connection().clone())),
                    request,
                    context,
                )
            }
        }
    } else {}
}

impl<Counterpart: Role> McpServerConnect<Counterpart> for McpServerBuilt<Counterpart> {
    fn name(&self) -> String {
        self.name.clone()
    }

    fn connect(
        &self,
        mcp_connection: McpConnectionTo<Counterpart>,
    ) -> DynConnectTo<role::mcp::Client> {
        DynConnectTo::new(self.connection(mcp_connection))
    }
}

/// A connected MCP server instance.
pub(crate) struct McpServerConnection<Counterpart: Role> {
    name: String,
    version: String,
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
    ) -> Result<CallToolResponse, ErrorData> {
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
                        Ok(CallToolResult::structured(result).into())
                    } else {
                        let content = rmcp::model::ContentBlock::text(result.to_string());
                        Ok(CallToolResult::success(vec![content]).into())
                    }
                }
                // Tool failures, including arguments that do not match the
                // input schema, are tool results the model can act on, not
                // protocol errors.
                Err(error) => Ok(CallToolResult::error(vec![rmcp::model::ContentBlock::text(
                    error.to_string(),
                )])
                .into()),
            },

            // If cancelled
            Either::Right(((), _)) => {
                Err(rmcp::ErrorData::internal_error("operation cancelled", None))
            }
        }
    }

    fn list_tools(
        &self,
        _request: Option<rmcp::model::PaginatedRequestParams>,
        context: rmcp::service::RequestContext<rmcp::RoleServer>,
    ) -> impl Future<Output = Result<rmcp::model::ListToolsResult, ErrorData>> + Send {
        // Return only enabled tools
        let tools: Vec<_> = self
            .data
            .enabled_tools()
            .map(|tool| make_tool_model(tool.metadata()))
            .collect();
        let mut result = ListToolsResult::with_all_items(tools);
        if context
            .protocol_version()
            .is_some_and(|version| version >= ProtocolVersion::V_2026_07_28)
        {
            // Modern MCP requires cache metadata. Avoid sharing tool catalogs
            // across authorization contexts or promising a stale cache lifetime.
            result = result.with_ttl_ms(0).with_cache_scope(CacheScope::Private);
        }
        std::future::ready(Ok(result))
    }

    fn get_info(&self) -> ServerConfig {
        // Basic server info
        let base = ServerConfig::new(
            rmcp::model::ServerCapabilities::builder()
                .enable_tools()
                .build(),
        )
        .with_server_info(rmcp::model::Implementation::new(
            self.name.clone(),
            self.version.clone(),
        ))
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
    );

    if let Some(title) = metadata.title() {
        tool = tool.with_title(title.to_string());
    }

    if let Some(schema) = metadata.output_schema() {
        tool = tool.with_raw_output_schema(schema.clone());
    }

    tool
}
