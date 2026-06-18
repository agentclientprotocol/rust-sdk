#![cfg_attr(docsrs, feature(doc_cfg))]

#[cfg(feature = "client")]
mod client;
#[cfg(feature = "server")]
mod connection;
#[cfg(feature = "server")]
mod http_server;
#[cfg(any(feature = "client", feature = "server"))]
mod protocol;
#[cfg(feature = "server")]
mod server;
#[cfg(feature = "server")]
mod websocket_server;

#[cfg(feature = "client")]
pub use client::{HttpClient, HttpClientError};
#[cfg(feature = "server")]
pub use server::{AcpHttpServer, CorsOptions, ServerOptions};
