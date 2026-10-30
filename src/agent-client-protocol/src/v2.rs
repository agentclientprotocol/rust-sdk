//! The SDK API for draft ACP protocol v2.
//!
//! Start a v2 connection with [`Client.v2()`](crate::Client::v2),
//! [`Agent.v2()`](crate::Agent::v2) or [`Proxy.v2()`](crate::Proxy::v2). The
//! protocol v2 message types are in [`schema::v2`](crate::schema::v2).
//!
//! This module exports the same names as [`crate::v1`] for protocol v2, so
//! code moves between versions by changing the module it imports from.
//!
//! The request handles here are the crate-root handles with the [`V2`]
//! protocol marker: `id()` returns a [`schema::v2::RequestId`].
//!
//! Requires the `unstable_protocol_v2` crate feature.
//!
//! ```no_run
//! use agent_client_protocol::schema::{ProtocolVersion, v2::{Implementation, InitializeRequest}};
//! use agent_client_protocol::{Agent, Client, ConnectTo};
//! use agent_client_protocol::v2::{ConnectionTo, InitializeOutcome};
//!
//! # async fn run(transport: impl ConnectTo<Client>) -> agent_client_protocol::Result<()> {
//! Client
//!     .v2()
//!     .connect_with(transport, async |cx: ConnectionTo<Agent>| {
//!         let request = InitializeRequest::new(
//!             ProtocolVersion::V2,
//!             Implementation::new("my-client", "0.1.0"),
//!         );
//!         let InitializeOutcome::Initialized(_response) =
//!             cx.send_initialize(request).block_task().await?
//!         else {
//!             return Err(agent_client_protocol::Error::internal_error());
//!         };
//!         Ok(())
//!     })
//!     .await
//! # }
//! ```
//!
//! [`schema::v2::RequestId`]: crate::schema::v2::RequestId

use crate::{NullClose, NullHandler, UntypedMessage, V2, jsonrpc::run::NullRun};

pub use crate::jsonrpc::v2_connection::ConnectionTo;
pub use crate::jsonrpc::{
    InitializationVersionMismatch, InitializeOutcome, SelectedInitializeResponse,
};
#[cfg(feature = "unstable_session_fork")]
pub use crate::session::v2::ForkSessionBuilder;
pub use crate::session::v2::{ActiveSession, OpenedSession, ResumeSessionBuilder, SessionBuilder};

/// A protocol v2 connection builder, returned by
/// [`Client.v2()`](crate::Client::v2), [`Agent.v2()`](crate::Agent::v2) and
/// [`Proxy.v2()`](crate::Proxy::v2).
///
/// Typed handlers receive v2 request handles and callbacks receive a
/// [`ConnectionTo`].
pub type Builder<Host, Handler = NullHandler, Runner = NullRun, Close = NullClose> =
    crate::Builder<Host, Handler, Runner, Close, V2>;

/// A v2 request's [`Responder`](crate::Responder): `id()` returns a
/// [`schema::v2::RequestId`](crate::schema::v2::RequestId).
pub type Responder<T = serde_json::Value> = crate::Responder<T, V2>;

/// A v2 [`ResponseRouter`](crate::ResponseRouter): `id()` returns a
/// [`schema::v2::RequestId`](crate::schema::v2::RequestId).
pub type ResponseRouter<T = serde_json::Value> = crate::ResponseRouter<T, V2>;

/// A request sent through a v2 [`ConnectionTo`]: the
/// [`SentRequest`](crate::SentRequest) with a v2 request ID.
pub type SentRequest<T> = crate::SentRequest<T, V2>;

/// A request prepared through a v2 [`ConnectionTo`]: the
/// [`PreparedRequest`](crate::PreparedRequest) with a v2 request ID.
pub type PreparedRequest<T> = crate::PreparedRequest<T, V2>;

/// A message received by a typed handler on a v2 [`Builder`]: the
/// [`Dispatch`](crate::Dispatch) with v2 request handles.
pub type Dispatch<Req = UntypedMessage, Notif = UntypedMessage> = crate::Dispatch<Req, Notif, V2>;
