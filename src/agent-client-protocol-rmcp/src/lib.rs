//! # agent-client-protocol-rmcp
//!
//! This crate provides integration between [rmcp](https://docs.rs/rmcp) MCP servers
//! and the Agent Client Protocol MCP server framework.
//!
//! Building or directly serving a standalone MCP server requires no unstable
//! ACP feature. Enable `unstable_mcp_over_acp` when attaching the server to an
//! ACP connection with `with_mcp_server`.
//!
//! ## Usage
//!
//! Build an MCP server with tools using the extension trait:
//!
//! ```no_run
//! use agent_client_protocol::{ConnectTo, mcp_server::McpServer, role::mcp};
//! use agent_client_protocol_rmcp::McpServerExt;
//!
//! # async fn serve(
//! #     client_transport: impl ConnectTo<mcp::Server>,
//! # ) -> agent_client_protocol::Result<()> {
//! let server = McpServer::<mcp::Client>::builder("my-tools").build();
//! server.connect_to(client_transport).await
//! # }
//! ```
//!
//! Or create an MCP server from an rmcp service:
//!
//! ```ignore
//! use agent_client_protocol::mcp_server::McpServer;
//! use agent_client_protocol_rmcp::McpServerExt;
//!
//! let server = McpServer::from_rmcp("my-server", MyRmcpService::new);
//!
//! // With `unstable_mcp_over_acp`, attach it to a proxy and connect the proxy
//! // to its transport.
//! Proxy.builder()
//!     .with_mcp_server(server)
//!     .connect_to(transport)
//!     .await?;
//! ```

use agent_client_protocol::mcp_server::{McpConnectionTo, McpServer, McpServerConnect};
#[cfg(feature = "unstable_mcp_over_acp")]
use agent_client_protocol::mcp_server::{McpOutcome, McpRequest, McpRequestContext, McpService};
use agent_client_protocol::role;
use agent_client_protocol::{ByteStreams, ConnectTo, DynConnectTo, NullRun, Role};
#[cfg(feature = "unstable_mcp_over_acp")]
use futures::future::BoxFuture;
use futures_concurrency::future::TryJoin as _;
use rmcp::ServiceExt;
#[cfg(feature = "unstable_mcp_over_acp")]
use std::sync::Arc;
use tokio_util::compat::{TokioAsyncReadCompatExt, TokioAsyncWriteCompatExt};

mod builder;
#[cfg(feature = "unstable_mcp_over_acp")]
mod native;

pub use agent_client_protocol::mcp_server::{EnabledTools, McpTool};
pub use agent_client_protocol::{tool_fn, tool_fn_mut};
pub use builder::McpServerBuilder;

/// Extension constructors for MCP servers backed by `rmcp`.
pub trait McpServerExt<Counterpart: Role> {
    /// Create an MCP server builder for defining tools in Rust code.
    fn builder(name: impl ToString) -> McpServerBuilder<Counterpart, NullRun> {
        McpServerBuilder::new(name.to_string())
    }

    /// Create an MCP server from something that implements the [`McpServerConnect`] trait.
    ///
    /// # See also
    ///
    /// See [`Self::builder`] to construct MCP servers from Rust code.
    fn from_rmcp<S>(
        name: impl ToString,
        new_fn: impl Fn() -> S + Send + Sync + 'static,
    ) -> McpServer<Counterpart, NullRun>
    where
        S: rmcp::Service<rmcp::RoleServer>,
    {
        struct RmcpServer<F> {
            name: String,
            new_fn: F,
        }

        #[cfg(feature = "unstable_mcp_over_acp")]
        struct SharedRmcp<F, S> {
            new_fn: Arc<F>,
            service: std::sync::OnceLock<Arc<S>>,
        }

        #[cfg(feature = "unstable_mcp_over_acp")]
        impl<Counterpart, F, S> McpService<Counterpart> for SharedRmcp<F, S>
        where
            Counterpart: Role,
            F: Fn() -> S + Send + Sync + 'static,
            S: rmcp::Service<rmcp::RoleServer>,
        {
            fn execute(
                &self,
                request: McpRequest,
                context: McpRequestContext<Counterpart>,
            ) -> BoxFuture<'static, Result<McpOutcome, agent_client_protocol::Error>> {
                let service = self.service.get_or_init(|| Arc::new((self.new_fn)()));
                native::execute(service.clone(), request, context)
            }
        }

        impl<Counterpart, F, S> McpServerConnect<Counterpart> for RmcpServer<F>
        where
            Counterpart: Role,
            F: Fn() -> S + Send + Sync + 'static,
            S: rmcp::Service<rmcp::RoleServer>,
        {
            fn name(&self) -> String {
                self.name.clone()
            }

            fn connect(
                &self,
                _cx: McpConnectionTo<Counterpart>,
            ) -> DynConnectTo<role::mcp::Client> {
                let service = (self.new_fn)();
                DynConnectTo::new(RmcpServerComponent { service })
            }
        }

        #[cfg(feature = "unstable_mcp_over_acp")]
        {
            // Feature unification must not construct an unused ACP service
            // when this server is only used through its standalone adapter.
            let new_fn = Arc::new(new_fn);
            let shared = SharedRmcp {
                new_fn: new_fn.clone(),
                service: std::sync::OnceLock::new(),
            };
            McpServer::new_service_with_standalone(
                shared,
                RmcpServer {
                    name: name.to_string(),
                    new_fn: move || new_fn(),
                },
                NullRun,
            )
        }
        #[cfg(not(feature = "unstable_mcp_over_acp"))]
        {
            McpServer::new(
                RmcpServer {
                    name: name.to_string(),
                    new_fn,
                },
                NullRun,
            )
        }
    }
}

impl<Counterpart: Role> McpServerExt<Counterpart> for McpServer<Counterpart> {}

/// Component wrapper for rmcp services.
struct RmcpServerComponent<S> {
    service: S,
}

impl<S> ConnectTo<role::mcp::Client> for RmcpServerComponent<S>
where
    S: rmcp::Service<rmcp::RoleServer>,
{
    async fn connect_to(
        self,
        client: impl ConnectTo<role::mcp::Server>,
    ) -> Result<(), agent_client_protocol::Error> {
        // Create tokio byte streams that rmcp expects
        let (mcp_server_stream, mcp_client_stream) = tokio::io::duplex(8192);
        let (mcp_server_read, mcp_server_write) = tokio::io::split(mcp_server_stream);
        let (mcp_client_read, mcp_client_write) = tokio::io::split(mcp_client_stream);

        let bytes_to_acp = async {
            // Create ByteStreams component for the client side
            let byte_streams =
                ByteStreams::new(mcp_client_write.compat_write(), mcp_client_read.compat());

            ConnectTo::<role::mcp::Client>::connect_to(byte_streams, client).await
        };

        let bytes_to_rmcp = async {
            // Run the rmcp server with the server side of the duplex stream
            let running_server = self
                .service
                .serve((mcp_server_read, mcp_server_write))
                .await
                .map_err(agent_client_protocol::Error::into_internal_error)?;

            // Wait for the server to finish
            running_server
                .waiting()
                .await
                .map(|_quit_reason| ())
                .map_err(agent_client_protocol::Error::into_internal_error)
        };

        (bytes_to_acp, bytes_to_rmcp).try_join().await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use agent_client_protocol::{mcp_server::McpServer, role};

    use crate::McpServerExt as _;

    #[test]
    fn builds_standalone_server_without_acp_transport_feature() {
        let _server: McpServer<role::mcp::Client, _> = McpServer::builder("standalone").build();
    }
}
