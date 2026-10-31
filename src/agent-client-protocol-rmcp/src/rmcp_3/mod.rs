//! Integration with [rmcp](https://docs.rs/rmcp/3) 3.x.
//!
//! Available with the `rmcp_3` feature. Every rmcp type in this module's API,
//! such as the services accepted by [`McpServerExt::from_rmcp`], comes from
//! rmcp 3.x, so pair it with an `rmcp = "3"` dependency in your own crate.
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
//! use agent_client_protocol_rmcp::rmcp_3::McpServerExt;
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
//! use agent_client_protocol_rmcp::rmcp_3::McpServerExt;
//!
//! let server = McpServer::from_rmcp("my-server", MyRmcpService::new);
//!
//! // With `unstable_mcp_over_acp`, attach it to a proxy and connect the proxy
//! // to its transport.
//! Proxy.v1()
//!     .with_mcp_server(server)
//!     .connect_to(transport)
//!     .await?;
//! ```

use ::rmcp_3 as rmcp;
use agent_client_protocol::mcp_server::{McpConnectionTo, McpServer, McpServerConnect};
use agent_client_protocol::role;
use agent_client_protocol::{ByteStreams, ConnectTo, DynConnectTo, NullRun, Role};
use futures_concurrency::future::TryJoin as _;
use rmcp::ServiceExt;
use tokio_util::compat::{TokioAsyncReadCompatExt, TokioAsyncWriteCompatExt};

mod builder;

// The native MCP-over-ACP path follows the core crate's
// `unstable_mcp_over_acp` feature, not this crate's passthrough feature, so a
// feature-unified build never falls back to bridging a fresh rmcp service for
// every request.
agent_client_protocol::__if_mcp_over_acp! {
    {
        mod native;
        use agent_client_protocol::mcp_server::{McpOutcome, McpRequest, McpRequestContext, McpService};
        use futures::future::BoxFuture;
        use std::sync::{Arc, OnceLock};
    } else {}
}

pub use agent_client_protocol::mcp_server::{McpTool, UnknownToolError};
pub use agent_client_protocol::{tool_fn, tool_fn_mut};
pub use builder::McpServerBuilder;

mod sealed {
    pub trait Sealed {}
}

impl<Counterpart: Role, Run> sealed::Sealed for McpServer<Counterpart, Run> {}

/// Extension constructors for MCP servers backed by `rmcp`.
///
/// This trait is sealed: it is implemented for
/// [`McpServer`] and cannot be
/// implemented outside this crate.
pub trait McpServerExt<Counterpart: Role>: sealed::Sealed {
    /// Create an MCP server builder for defining tools in Rust code.
    fn builder(name: impl ToString) -> McpServerBuilder<Counterpart, NullRun> {
        McpServerBuilder::new(name.to_string())
    }

    /// Create an MCP server from an rmcp service factory.
    ///
    /// Native ACP requests lazily share one application service. Standalone
    /// connections invoke the factory independently and retain their ordinary
    /// MCP lifecycle.
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

        agent_client_protocol::__if_mcp_over_acp! {
            {
                struct SharedRmcp<F, S> {
                    new_fn: Arc<F>,
                    service: OnceLock<Arc<S>>,
                }

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

                // Standalone use under feature unification must not initialize an
                // unused native service. The factory remains per-connection there.
                let new_fn = Arc::new(new_fn);
                McpServer::new_service_with_standalone(
                    SharedRmcp {
                        new_fn: new_fn.clone(),
                        service: OnceLock::new(),
                    },
                    RmcpServer {
                        name: name.to_string(),
                        new_fn: move || new_fn(),
                    },
                    NullRun,
                )
            } else {
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

    use super::McpServerExt as _;

    #[test]
    fn builds_standalone_server_without_acp_transport_feature() {
        let _server: McpServer<role::mcp::Client, _> = McpServer::builder("standalone").build();
    }
}
