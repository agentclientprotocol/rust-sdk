//! HTTP/SSE and WebSocket transports for the Agent Client Protocol (ACP).
//!
//! Both sides serve ACP over a single endpoint, `/acp` by default:
//!
//! - `POST` carries client-to-agent JSON-RPC messages. An `initialize` request
//!   creates a connection and returns its `Acp-Connection-Id`.
//! - `GET` with `Accept: text/event-stream` streams agent-to-client messages
//!   over SSE, one stream per connection plus one per session.
//! - `GET` with a WebSocket upgrade carries both directions as text frames.
//! - `DELETE` closes the connection.
//!
//! Neither side is enabled by default. Enable the one you need:
//!
//! - `client`: [`HttpClient`] connects to a server over HTTP/SSE or WebSocket,
//!   chosen by the URL scheme. It implements
//!   [`ConnectTo<Client>`](agent_client_protocol::ConnectTo), so it takes the
//!   place of an agent component. [`HttpClientBuilder::configure_http`] and
//!   [`HttpClient::from_http_client`] take reqwest types, so add a matching
//!   `reqwest` dependency to customize the HTTP client.
//! - `server`: [`AcpHttpServer`] turns an agent factory into an
//!   `axum::Router` to mount in your axum application. [`ServerOptions`] and
//!   [`CorsOptions`] configure the endpoint path, the `/health` route, and
//!   browser origins.
//!
//! The transport carries JSON-RPC messages unchanged, so it serves ACP v1, v2,
//! or both through `AgentProtocolRouter`.
//! JSON-RPC batches are rejected. See the
//! [HTTP transport guide](https://agentclientprotocol.github.io/rust-sdk/http-transport.html)
//! for routing, cancellation, and browser policy details.
#![cfg_attr(docsrs, feature(doc_cfg))]
// The crate docs link to both the client and the server types. Builds with
// only one of those features can't resolve the other side's links.
#![cfg_attr(
    not(all(feature = "client", feature = "server")),
    allow(rustdoc::broken_intra_doc_links)
)]

#[cfg(feature = "client")]
mod client;
#[cfg(feature = "server")]
mod connection;
#[cfg(feature = "server")]
mod diagnostics;
#[cfg(feature = "server")]
mod http_server;
#[cfg(any(feature = "client", feature = "server"))]
mod protocol;
#[cfg(feature = "server")]
mod server;
#[cfg(feature = "server")]
mod websocket_server;

#[cfg(feature = "client")]
pub use client::{HttpClient, HttpClientBuilder, HttpClientError, InvalidUrlError};
#[cfg(feature = "server")]
pub use server::{AcpHttpServer, CorsOptions, InvalidOriginError, ServerOptions};
