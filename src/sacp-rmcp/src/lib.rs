//! # sacp-rmcp - rmcp integration for SACP
//!
//! This crate provides integration between [rmcp](https://docs.rs/rmcp) MCP servers
//! and the SACP proxy framework.
//!
//! ## Usage
//!
//! Add rmcp-based MCP servers to your proxy using the extension trait:
//!
//! ```ignore
//! use sacp_proxy::McpServiceRegistry;
//! use sacp_rmcp::McpServiceRegistryRmcpExt;
//!
//! let registry = McpServiceRegistry::new();
//! registry.add_rmcp_server("my-server", || MyRmcpService::new())?;
//! ```

use rmcp::ServiceExt;
use sacp::{ByteStreams, Component};
use sacp_proxy::McpServiceRegistry;
use tokio_util::compat::{TokioAsyncReadCompatExt, TokioAsyncWriteCompatExt};

/// Extension trait for adding rmcp-based MCP servers to a registry.
pub trait McpServiceRegistryRmcpExt {
    /// Add an MCP server implemented using the rmcp crate.
    ///
    /// # Parameters
    ///
    /// - `name`: The name of the server.
    /// - `make_service`: A function that creates the service (e.g., `YourService::new`).
    ///
    /// # Example
    ///
    /// ```ignore
    /// registry.add_rmcp_server("my-server", || MyRmcpService::new())?;
    /// ```
    fn add_rmcp_server<S>(
        &self,
        name: impl ToString,
        make_service: impl Fn() -> S + 'static + Send + Sync,
    ) -> Result<(), sacp::Error>
    where
        S: rmcp::Service<rmcp::RoleServer>;

    /// Add the MCP server to the registry and return `self`. Useful for chaining.
    /// Equivalent to [`Self::add_rmcp_server`].
    fn with_rmcp_server<S>(
        self,
        name: impl ToString,
        make_service: impl Fn() -> S + 'static + Send + Sync,
    ) -> Result<Self, sacp::Error>
    where
        S: rmcp::Service<rmcp::RoleServer>,
        Self: Sized;
}

impl McpServiceRegistryRmcpExt for McpServiceRegistry {
    fn add_rmcp_server<S>(
        &self,
        name: impl ToString,
        make_service: impl Fn() -> S + 'static + Send + Sync,
    ) -> Result<(), sacp::Error>
    where
        S: rmcp::Service<rmcp::RoleServer>,
    {
        self.add_mcp_server(name, move || {
            let service = make_service();
            RmcpServerComponent { service }
        })
    }

    fn with_rmcp_server<S>(
        self,
        name: impl ToString,
        make_service: impl Fn() -> S + 'static + Send + Sync,
    ) -> Result<Self, sacp::Error>
    where
        S: rmcp::Service<rmcp::RoleServer>,
    {
        self.add_rmcp_server(name, make_service)?;
        Ok(self)
    }
}

/// Component wrapper for rmcp services
struct RmcpServerComponent<S> {
    service: S,
}

impl<S> Component for RmcpServerComponent<S>
where
    S: rmcp::Service<rmcp::RoleServer>,
{
    async fn serve(self, client: impl Component) -> Result<(), sacp::Error> {
        // Create tokio byte streams that rmcp expects
        let (mcp_server_stream, mcp_client_stream) = tokio::io::duplex(8192);
        let (mcp_server_read, mcp_server_write) = tokio::io::split(mcp_server_stream);
        let (mcp_client_read, mcp_client_write) = tokio::io::split(mcp_client_stream);

        // Create ByteStreams component for the client side
        let byte_streams =
            ByteStreams::new(mcp_client_write.compat_write(), mcp_client_read.compat());

        // Spawn task to connect byte_streams to the provided client
        tokio::spawn(async move {
            let _ = byte_streams.serve(client).await;
        });

        // Run the rmcp server with the server side of the duplex stream
        let running_server = self
            .service
            .serve((mcp_server_read, mcp_server_write))
            .await
            .map_err(sacp::Error::into_internal_error)?;

        // Wait for the server to finish
        running_server
            .waiting()
            .await
            .map(|_quit_reason| ())
            .map_err(sacp::Error::into_internal_error)
    }
}
