//! # agent-client-protocol-polyfill
//!
//! Polyfill proxies for adapting newer ACP features to older agents.
//!
//! ## MCP-over-ACP Polyfill
//!
//! The [`mcp_over_acp`] module consumes schema-native `McpServer::Acp` declarations and
//! `mcp/*` messages, exposing each server to an agent through a loopback HTTP bridge.

pub mod mcp_over_acp;
