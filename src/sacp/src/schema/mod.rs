//! ACP protocol schema types and message implementations.
//!
//! This module contains all the types from the Agent-Client Protocol schema,
//! including requests, responses, notifications, and supporting types.
//! All types are re-exported flatly from this module.

// Internal organization
mod agent_to_client;
mod client_to_agent;
mod enum_impls;

// Re-export everything from agent_client_protocol_schema
pub use agent_client_protocol_schema::*;
