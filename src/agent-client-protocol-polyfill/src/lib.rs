//! # agent-client-protocol-polyfill
//!
//! Polyfill proxies for backward compatibility with agents that don't support
//! newer ACP features natively.
//!
//! ## MCP-over-ACP Polyfill
//!
//! The [`mcp_over_acp`] module implements the legacy v1 MCP-over-ACP extension for
//! agents that don't support `mcpCapabilities.acp`. It transforms `McpServer::Http`
//! entries with `acp:` URLs into localhost TCP bridges and routes the legacy
//! `_mcp/connect`, `_mcp/message`, and `_mcp/disconnect` methods through those bridges.
//! This is distinct from the draft native `McpServer::Acp` transport and its `mcp/*` methods.

pub mod mcp_over_acp;
