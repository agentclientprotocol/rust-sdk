//! # agent-client-protocol-polyfill
//!
//! Polyfill proxies for backward compatibility with agents that don't support
//! newer ACP features natively.
//!
//! ## MCP-over-ACP Polyfill
//!
//! The [`mcp_over_acp`] module provides a proxy that bridges MCP-over-ACP transport
//! for agents that don't support `mcpCapabilities.acp`. It intercepts `NewSessionRequest`
//! to transform `McpServer::Http` entries with `acp:` URLs into localhost TCP bridges,
//! and handles `_mcp/*` messages by routing them through those bridges.

pub mod mcp_over_acp;
