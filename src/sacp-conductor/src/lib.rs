//! # sacp-conductor
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
//! sacp-conductor agent "python proxy1.py" "python proxy2.py" "python base-agent.py"
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
//! sacp-conductor mcp 8080
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
//! sacp-conductor agent \
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
//! - **[sacp-proxy](https://crates.io/crates/sacp-proxy)** - Framework for building proxy components
//! - **[sacp](https://crates.io/crates/sacp)** - Core ACP SDK
//! - **[sacp-tokio](https://crates.io/crates/sacp-tokio)** - Tokio utilities for process spawning

use std::str::FromStr;

use crate::conductor::Conductor;

/// Core conductor logic for orchestrating proxy chains
pub mod conductor;
/// MCP bridge functionality for TCP-based MCP servers
mod mcp_bridge;

use clap::{Parser, Subcommand};
use sacp_tokio::{AcpAgent, Stdio};

#[derive(Parser, Debug)]
#[command(author, version, about, long_about = None)]
pub struct ConductorArgs {
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
    pub async fn run(self) -> Result<(), sacp::Error> {
        match self.command {
            ConductorCommand::Agent { name, proxies } => {
                let providers = proxies
                    .into_iter()
                    .map(|s| AcpAgent::from_str(&s))
                    .collect::<Result<Vec<_>, sacp::Error>>()?;

                Conductor::new(name, providers, None)
                    .into_handler_chain()
                    .connect_to(Stdio)?
                    .serve()
                    .await
            }
            ConductorCommand::Mcp { port } => mcp_bridge::run_mcp_bridge(port).await,
        }
    }
}
