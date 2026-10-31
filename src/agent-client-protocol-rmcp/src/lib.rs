//! # agent-client-protocol-rmcp
//!
//! This crate provides integration between [rmcp](https://docs.rs/rmcp) MCP servers
//! and the Agent Client Protocol MCP server framework.
//!
//! ## Choosing an rmcp version
//!
//! Each supported rmcp major version has its own module behind a matching
//! Cargo feature. No rmcp feature is enabled by default; without one, this
//! crate is empty. Enable the feature for the rmcp major version your
//! application uses:
//!
//! | Feature  | Module       | rmcp |
//! | -------- | ------------ | ---- |
//! | `rmcp_3` | `rmcp_3`     | 3.x  |
//!
//! ```toml
//! [dependencies]
//! agent-client-protocol-rmcp = { version = "5", features = ["rmcp_3"] }
//! rmcp = "3"
//! ```
//!
//! The features are additive: enabling one adds its module and changes
//! nothing else, so an application can enable several while it migrates
//! between rmcp major versions. Support for a new rmcp major version arrives
//! as a new feature in a minor release; dropping one is a breaking change.
//!
//! The `unstable_mcp_over_acp` feature passes through to the core crate and
//! applies to every rmcp module. The native MCP-over-ACP path follows the core
//! crate's feature, however it is enabled. For draft ACP v2, enable
//! `unstable_protocol_v2` on `agent-client-protocol` directly.
#![cfg_attr(docsrs, feature(doc_cfg))]

#[cfg(feature = "rmcp_3")]
#[cfg_attr(docsrs, doc(cfg(feature = "rmcp_3")))]
pub mod rmcp_3;
