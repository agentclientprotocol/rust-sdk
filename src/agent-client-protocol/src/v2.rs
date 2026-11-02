//! The SDK API for draft ACP protocol v2.
//!
//! Start a v2 connection with [`Client.v2()`](crate::Client::v2),
//! [`Agent.v2()`](crate::Agent::v2) or [`Proxy.v2()`](crate::Proxy::v2). The
//! protocol v2 message types are in [`v2::schema`](schema).
//!
//! This module uses the same names as [`crate::v1`] for protocol v2's
//! connection, builder, request handles and `schema`. The session APIs differ:
//! [`ActiveSession`] here only sends commands, and the session's updates and
//! the agent's requests arrive through typed handlers on the connection,
//! while `v1::ActiveSession` reads them from its own stream. [`InitializeOutcome`]
//! and, with `unstable_session_fork`, `ForkSessionBuilder` exist only here.
//! v1's `LoadSessionBuilder`, `SessionMessage` and blocking states have no v2
//! counterpart.
//!
//! Message types are not tied to a protocol version in the type system: a v2
//! connection accepts any [`JsonRpcRequest`](crate::JsonRpcRequest), so
//! sending a protocol v1 message through it compiles. Send the types from
//! [`schema`].
//!
//! The request handles here are the crate-root handles with the [`V2`]
//! protocol marker: `id()` returns a [`v2::schema::RequestId`].
//!
//! Requires the `unstable_protocol_v2` crate feature.
//!
//! ```no_run
//! use agent_client_protocol::schema::ProtocolVersion;
//! use agent_client_protocol::v2::schema::{Implementation, InitializeRequest};
//! use agent_client_protocol::v2::{ConnectionTo, InitializeOutcome};
//! use agent_client_protocol::{Agent, Client, ConnectTo};
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
//! # Stability
//!
//! Everything in this module is unstable and may change in a minor release.
//! Stabilizing protocol v2 only removes the `unstable_protocol_v2` gate. See
//! [Stability](crate#stability) in the crate docs.
//!
//! [`v2::schema::RequestId`]: crate::v2::schema::RequestId

use crate::{NullClose, NullHandler, UntypedMessage, V2, jsonrpc::run::NullRun};

/// The protocol v2 message types: requests, responses, notifications and
/// their supporting types, re-exported from the `agent-client-protocol-schema`
/// crate's `v2` module.
///
/// This is the only path to them.
/// `use agent_client_protocol::v2::{self, schema};` brings both the SDK
/// types and the messages into scope under names that do not clash with the
/// SDK's `v2` module. The version-neutral items, such as
/// [`ProtocolVersion`](crate::schema::ProtocolVersion), are in
/// [`crate::schema`].
pub use agent_client_protocol_schema::v2 as schema;

pub use crate::jsonrpc::InitializeOutcome;
pub use crate::jsonrpc::v2_connection::ConnectionTo;
#[cfg(feature = "unstable_session_fork")]
pub use crate::session::v2::ForkSessionBuilder;
pub use crate::session::v2::{ActiveSession, ResumeSessionBuilder, SessionBuilder, SessionSetup};

/// A protocol v2 connection builder, returned by
/// [`Client.v2()`](crate::Client::v2), [`Agent.v2()`](crate::Agent::v2) and
/// [`Proxy.v2()`](crate::Proxy::v2).
///
/// Typed handlers receive v2 request handles and callbacks receive a
/// [`ConnectionTo`].
pub type Builder<Host, Handler = NullHandler, Runner = NullRun, Close = NullClose> =
    crate::Builder<Host, Handler, Runner, Close, V2>;

/// A v2 request's [`Responder`](crate::Responder): `id()` returns a
/// [`v2::schema::RequestId`](crate::v2::schema::RequestId).
pub type Responder<T = serde_json::Value> = crate::Responder<T, V2>;

/// A v2 [`ResponseRouter`](crate::ResponseRouter): `id()` returns a
/// [`v2::schema::RequestId`](crate::v2::schema::RequestId).
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
