//! # agent-client-protocol-conductor
//!
//! Binary for orchestrating ACP proxy chains.
//!
//! ## What is the conductor?
//!
//! The conductor is a tool that manages proxy chains - it spawns proxy components and the base agent,
//! then routes messages between them. From the editor's perspective, the conductor appears as a single ACP agent.
//!
//! ```text
//! Editor ← stdio → Conductor → Proxy 1 → Proxy 2 → Agent
//! ```
//!
//! ## Usage
//!
//! ### Agent Mode
//!
//! Orchestrate a chain of proxies in front of an agent:
//!
//! ```bash
//! # Chain format: proxy1 proxy2 ... agent
//! agent-client-protocol-conductor agent "python proxy1.py" "python proxy2.py" "python base-agent.py"
//! ```
//!
//! The conductor:
//! 1. Spawns each component as a subprocess
//! 2. Connects them in a chain
//! 3. Presents as a single agent on stdin/stdout
//! 4. Manages the lifecycle of all processes
//!
//! ### MCP Bridge Mode
//!
//! Connect stdio to a TCP-based MCP server:
//!
//! ```bash
//! # Bridge stdio to MCP server on localhost:8080
//! agent-client-protocol-conductor mcp 8080
//! ```
//!
//! This allows stdio-based tools to communicate with TCP MCP servers.
//!
//! ## How It Works
//!
//! **Component Communication:**
//! - Editor talks to conductor via stdio
//! - Conductor uses `_proxy/successor/*` protocol extensions to route messages
//! - Each proxy can intercept, transform, or forward messages
//! - Final agent receives standard ACP messages
//!
//! **Process Management:**
//! - All components are spawned as child processes
//! - When conductor exits, all children are terminated
//! - Errors in any component bring down the entire chain
//!
//! ## Example Use Case
//!
//! Add Sparkle embodiment + custom tools to any agent:
//!
//! ```bash
//! agent-client-protocol-conductor agent \
//!   "sparkle-acp-proxy" \
//!   "my-custom-tools-proxy" \
//!   "claude-agent"
//! ```
//!
//! This creates a stack where:
//! 1. Sparkle proxy injects MCP servers and prepends embodiment
//! 2. Custom tools proxy adds domain-specific functionality
//! 3. Base agent handles the actual AI responses
//!
//! ## Related Crates
//!
//! - **[agent-client-protocol-proxy](https://crates.io/crates/agent-client-protocol-proxy)** - Framework for building proxy components
//! - **[agent-client-protocol-core](https://crates.io/crates/agent-client-protocol-core)** - Core ACP SDK
//! - **[agent-client-protocol-tokio](https://crates.io/crates/agent-client-protocol-tokio)** - Tokio utilities for process spawning

use std::path::PathBuf;
use std::str::FromStr;

/// Core conductor logic for orchestrating proxy chains
mod conductor;
/// Debug logging for conductor
mod debug_logger;
/// MCP bridge functionality for TCP-based MCP servers
mod mcp_bridge;
mod snoop;
/// Trace event types for sequence diagram viewer
pub mod trace;

pub use self::conductor::*;

use clap::{Parser, Subcommand};

use agent_client_protocol_core::{Client, Conductor, DynConnectTo, schema::InitializeRequest};
use agent_client_protocol_tokio::{AcpAgent, Stdio};
use tracing::Instrument;
use tracing_subscriber::{EnvFilter, layer::SubscriberExt, util::SubscriberInitExt};

/// Wrapper for command-line component lists that can serve as either
/// proxies-only (for proxy mode) or proxies+agent (for agent mode).
///
/// This exists because `AcpAgent` implements `Component<L>` for all `L`,
/// so a `Vec<AcpAgent>` can be used as either a list of proxies or as
/// proxies + final agent depending on the conductor mode.
#[derive(Debug)]
pub struct CommandLineComponents(pub Vec<AcpAgent>);

impl InstantiateProxies for CommandLineComponents {
    fn instantiate_proxies(
        self: Box<Self>,
        req: InitializeRequest,
    ) -> futures::future::BoxFuture<
        'static,
        Result<
            (InitializeRequest, Vec<DynConnectTo<Conductor>>),
            agent_client_protocol_core::Error,
        >,
    > {
        Box::pin(async move {
            let proxies = self.0.into_iter().map(DynConnectTo::new).collect();
            Ok((req, proxies))
        })
    }
}

impl InstantiateProxiesAndAgent for CommandLineComponents {
    fn instantiate_proxies_and_agent(
        self: Box<Self>,
        req: InitializeRequest,
    ) -> futures::future::BoxFuture<
        'static,
        Result<
            (
                InitializeRequest,
                Vec<DynConnectTo<Conductor>>,
                DynConnectTo<Client>,
            ),
            agent_client_protocol_core::Error,
        >,
    > {
        Box::pin(async move {
            let mut iter = self.0.into_iter().peekable();
            let mut proxies: Vec<DynConnectTo<Conductor>> = Vec::new();

            // All but the last element are proxies
            while let Some(component) = iter.next() {
                if iter.peek().is_some() {
                    proxies.push(DynConnectTo::new(component));
                } else {
                    // Last element is the agent
                    let agent = DynConnectTo::new(component);
                    return Ok((req, proxies, agent));
                }
            }

            Err(agent_client_protocol_core::util::internal_error(
                "no agent component in list",
            ))
        })
    }
}

/// Wrapper to implement WriteEvent for TraceHandle.
struct TraceHandleWriter(agent_client_protocol_trace_viewer::TraceHandle);

impl trace::WriteEvent for TraceHandleWriter {
    fn write_event(&mut self, event: &trace::TraceEvent) -> std::io::Result<()> {
        let value = serde_json::to_value(event).map_err(std::io::Error::other)?;
        self.0.push(value);
        Ok(())
    }
}

/// Mode for the MCP bridge.
#[derive(Debug, Clone, Default)]
pub enum McpBridgeMode {
    /// Use stdio-based MCP bridge with a conductor subprocess.
    Stdio {
        /// Command and args to spawn conductor MCP bridge processes.
        /// E.g., vec!["conductor"] or vec!["cargo", "run", "-p", "conductor", "--"]
        conductor_command: Vec<String>,
    },

    /// Use HTTP-based MCP bridge
    #[default]
    Http,
}

#[derive(Parser, Debug)]
#[command(author, version, about, long_about = None)]
pub struct ConductorArgs {
    /// Enable debug logging of all stdin/stdout/stderr from components
    #[arg(long)]
    pub debug: bool,

    /// Directory for debug log files (defaults to current directory)
    #[arg(long)]
    pub debug_dir: Option<PathBuf>,

    /// Set log level (e.g., "trace", "debug", "info", "warn", "error", or module-specific like "conductor=debug")
    /// Only applies when --debug is enabled
    #[arg(long)]
    pub log: Option<String>,

    /// Path to write trace events for sequence diagram visualization.
    /// Events are written as newline-delimited JSON (.jsons format).
    #[arg(long)]
    pub trace: Option<PathBuf>,

    /// Serve trace viewer in browser with live updates.
    /// Can be used alone (in-memory) or with --trace (file-backed).
    #[arg(long)]
    pub serve: bool,

    #[command(subcommand)]
    pub command: ConductorCommand,
}

#[derive(Subcommand, Debug)]
pub enum ConductorCommand {
    /// Run as agent orchestrator managing a proxy chain
    Agent {
        /// Name of the agent
        #[arg(short, long, default_value = "conductor")]
        name: String,

        /// List of commands to chain together; the final command must be the agent.
        components: Vec<String>,
    },

    /// Run as a proxy orchestrating a proxy chain
    Proxy {
        /// Name of the proxy
        #[arg(short, long, default_value = "conductor")]
        name: String,

        /// List of proxy commands to chain together
        proxies: Vec<String>,
    },

    /// Run as MCP bridge connecting stdio to TCP
    Mcp {
        /// TCP port to connect to on localhost
        port: u16,
    },
}

impl ConductorArgs {
    /// Main entry point that sets up tracing and runs the conductor
    pub async fn main(self) -> anyhow::Result<()> {
        let pid = std::process::id();
        let cwd = std::env::current_dir()
            .map_or_else(|_| "<unknown>".to_string(), |p| p.display().to_string());

        // Only set up tracing if --debug is enabled
        let debug_logger = if self.debug {
            // Extract proxy list to create the debug logger
            let components = match &self.command {
                ConductorCommand::Agent { components, .. } => components.clone(),
                ConductorCommand::Proxy { proxies, .. } => proxies.clone(),
                ConductorCommand::Mcp { .. } => Vec::new(),
            };

            // Create debug logger
            Some(
                debug_logger::DebugLogger::new(self.debug_dir.clone(), &components)
                    .await
                    .map_err(|e| anyhow::anyhow!("Failed to create debug logger: {e}"))?,
            )
        } else {
            None
        };

        if let Some(debug_logger) = &debug_logger {
            // Set up log level from --log flag, defaulting to "info"
            let log_level = self.log.as_deref().unwrap_or("info");

            // Set up tracing to write to the debug file with "C !" prefix
            let tracing_writer = debug_logger.create_tracing_writer();
            tracing_subscriber::registry()
                .with(EnvFilter::new(log_level))
                .with(
                    tracing_subscriber::fmt::layer()
                        .with_target(true)
                        .with_writer(move || tracing_writer.clone()),
                )
                .init();

            tracing::info!(pid = %pid, cwd = %cwd, level = %log_level, "Conductor starting with debug logging");
        }

        // Set up tracing based on --trace and --serve flags
        let (trace_writer, _viewer_server) = match (&self.trace, self.serve) {
            // --trace only: write to file
            (Some(trace_path), false) => {
                let writer = trace::TraceWriter::from_path(trace_path)
                    .map_err(|e| anyhow::anyhow!("Failed to create trace writer: {e}"))?;
                (Some(writer), None)
            }
            // --serve only: in-memory with viewer
            (None, true) => {
                let (handle, server) = agent_client_protocol_trace_viewer::serve_memory(
                    agent_client_protocol_trace_viewer::TraceViewerConfig::default(),
                )?;
                let writer = trace::TraceWriter::new(TraceHandleWriter(handle));
                (Some(writer), Some(tokio::spawn(server)))
            }
            // --trace --serve: write to file and serve it
            (Some(trace_path), true) => {
                let writer = trace::TraceWriter::from_path(trace_path)
                    .map_err(|e| anyhow::anyhow!("Failed to create trace writer: {e}"))?;
                let server = agent_client_protocol_trace_viewer::serve_file(
                    trace_path.clone(),
                    agent_client_protocol_trace_viewer::TraceViewerConfig::default(),
                );
                (Some(writer), Some(tokio::spawn(server)))
            }
            // Neither: no tracing
            (None, false) => (None, None),
        };

        Box::pin(
            self.run(debug_logger.as_ref(), trace_writer)
                .instrument(tracing::info_span!("conductor", pid = %pid, cwd = %cwd)),
        )
        .await
        .map_err(|err| anyhow::anyhow!("{err}"))
    }

    async fn run(
        self,
        debug_logger: Option<&debug_logger::DebugLogger>,
        trace_writer: Option<trace::TraceWriter>,
    ) -> Result<(), agent_client_protocol_core::Error> {
        match self.command {
            ConductorCommand::Agent { name, components } => {
                Box::pin(initialize_conductor(
                    debug_logger,
                    trace_writer,
                    name,
                    components,
                    ConductorImpl::new_agent,
                ))
                .await
            }
            ConductorCommand::Proxy { name, proxies } => {
                Box::pin(initialize_conductor(
                    debug_logger,
                    trace_writer,
                    name,
                    proxies,
                    ConductorImpl::new_proxy,
                ))
                .await
            }
            ConductorCommand::Mcp { port } => mcp_bridge::run_mcp_bridge(port).await,
        }
    }
}

async fn initialize_conductor<Host: ConductorHostRole>(
    debug_logger: Option<&debug_logger::DebugLogger>,
    trace_writer: Option<trace::TraceWriter>,
    name: String,
    components: Vec<String>,
    new_conductor: impl FnOnce(
        String,
        CommandLineComponents,
        crate::McpBridgeMode,
    ) -> ConductorImpl<Host>,
) -> Result<(), agent_client_protocol_core::Error> {
    // Parse agents and optionally wrap with debug callbacks
    let providers: Vec<AcpAgent> = components
        .into_iter()
        .enumerate()
        .map(|(i, s)| {
            let mut agent = AcpAgent::from_str(&s)?;
            if let Some(logger) = debug_logger {
                agent = agent.with_debug(logger.create_callback(i.to_string()));
            }
            Ok(agent)
        })
        .collect::<Result<Vec<_>, agent_client_protocol_core::Error>>()?;

    // Create Stdio component with optional debug logging
    let stdio = if let Some(logger) = debug_logger {
        Stdio::new().with_debug(logger.create_callback("C".to_string()))
    } else {
        Stdio::new()
    };

    // Create conductor with optional trace writer
    let mut conductor = new_conductor(
        name,
        CommandLineComponents(providers),
        McpBridgeMode::default(),
    );
    if let Some(writer) = trace_writer {
        conductor = conductor.with_trace_writer(writer);
    }

    conductor.run(stdio).await
}
