//! The SDK API for ACP protocol v1.
//!
//! Start a v1 connection with [`Client.v1()`](crate::Client::v1),
//! [`Agent.v1()`](crate::Agent::v1) or [`Proxy.v1()`](crate::Proxy::v1). The
//! protocol v1 message types are in [`v1::schema`](schema), the same module as
//! [`agent_client_protocol::schema::v1`](crate::schema::v1).
//!
//! With the `unstable_protocol_v2` feature, `agent_client_protocol::v2` exports the same
//! names for protocol v2, so code moves between versions by changing the
//! module it imports from.
//!
//! The request handles here are the crate-root handles with the [`V1`]
//! protocol marker: `id()` returns a [`schema::v1::RequestId`].
//!
//! ```no_run
//! use agent_client_protocol::schema::ProtocolVersion;
//! use agent_client_protocol::v1::{ConnectionTo, schema::InitializeRequest};
//! use agent_client_protocol::{Agent, Client, ConnectTo};
//!
//! # async fn run(transport: impl ConnectTo<Client>) -> agent_client_protocol::Result<()> {
//! Client
//!     .v1()
//!     .connect_with(transport, async |cx: ConnectionTo<Agent>| {
//!         cx.send_request(InitializeRequest::new(ProtocolVersion::V1))
//!             .block_task()
//!             .await?;
//!         Ok(())
//!     })
//!     .await
//! # }
//! ```
//!
//! [`schema::v1::RequestId`]: crate::schema::v1::RequestId

use crate::{NullClose, NullHandler, UntypedMessage, V1, jsonrpc::run::NullRun};

/// The protocol v1 message types: the same module as
/// [`agent_client_protocol::schema::v1`](crate::schema::v1).
///
/// `use agent_client_protocol::v1::{self, schema};` brings both the SDK
/// types and the messages into scope under names that do not clash with the
/// SDK's `v1` module.
pub use crate::schema::v1 as schema;

pub use crate::jsonrpc::v1_connection::ConnectionTo;

pub use crate::session::{
    ActiveSession, Blocking, LoadSessionBuilder, NonBlocking, RestoreSessionBuilder,
    ResumeSessionBuilder, SessionBlockState, SessionBuilder, SessionMessage, SessionSetup,
};

/// A protocol v1 connection builder, returned by
/// [`Client.v1()`](crate::Client::v1), [`Agent.v1()`](crate::Agent::v1) and
/// [`Proxy.v1()`](crate::Proxy::v1).
///
/// Typed handlers receive v1 request handles and callbacks receive a
/// [`ConnectionTo`].
pub type Builder<Host, Handler = NullHandler, Runner = NullRun, Close = NullClose> =
    crate::Builder<Host, Handler, Runner, Close, V1>;

/// A v1 request's [`Responder`](crate::Responder): `id()` returns a
/// [`schema::v1::RequestId`](crate::schema::v1::RequestId).
pub type Responder<T = serde_json::Value> = crate::Responder<T, V1>;

/// A v1 [`ResponseRouter`](crate::ResponseRouter): `id()` returns a
/// [`schema::v1::RequestId`](crate::schema::v1::RequestId).
pub type ResponseRouter<T = serde_json::Value> = crate::ResponseRouter<T, V1>;

/// A request sent through a v1 [`ConnectionTo`]: the
/// [`SentRequest`](crate::SentRequest) with a v1 request ID.
pub type SentRequest<T> = crate::SentRequest<T, V1>;

/// A request prepared through a v1 [`ConnectionTo`]: the
/// [`PreparedRequest`](crate::PreparedRequest) with a v1 request ID.
pub type PreparedRequest<T> = crate::PreparedRequest<T, V1>;

/// A message received by a typed handler on a v1 [`Builder`]: the
/// [`Dispatch`](crate::Dispatch) with v1 request handles.
pub type Dispatch<Req = UntypedMessage, Notif = UntypedMessage> = crate::Dispatch<Req, Notif, V1>;
