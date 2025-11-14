//! Handler types for building custom JSON-RPC message handlers.
//!
//! This module contains the handler types used by [`JrConnection`](crate::JrConnection)
//! to process incoming messages. Most users won't need to use these types directly,
//! as the builder methods on `JrConnection` handle the construction automatically.
//!
//! However, these types can be useful for:
//! - Building reusable handler components
//! - Composing handlers programmatically
//! - Understanding the handler infrastructure

pub use crate::jsonrpc::handlers::*;
