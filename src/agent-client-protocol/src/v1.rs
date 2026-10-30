//! The SDK API for ACP protocol v1.
//!
//! Start a v1 connection with [`Client.v1()`](crate::Client::v1),
//! [`Agent.v1()`](crate::Agent::v1) or [`Proxy.v1()`](crate::Proxy::v1). The
//! protocol v1 message types are in [`schema::v1`](crate::schema::v1).
//!
//! With the `unstable_protocol_v2` feature, `agent_client_protocol::v2` exports the same
//! names for protocol v2, so code moves between versions by changing the
//! module it imports from.
//!
//! The request handles here are the crate-root handles with the [`V1`]
//! protocol marker: `id()` returns a [`schema::v1::RequestId`].
//!
//! ```no_run
//! use agent_client_protocol::schema::{ProtocolVersion, v1::InitializeRequest};
//! use agent_client_protocol::{Agent, Client, ConnectTo};
//! use agent_client_protocol::v1::ConnectionTo;
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

/// The connection that v1 builder callbacks receive.
///
/// Protocol v1 uses the version-neutral [`crate::ConnectionTo`], which also
/// carries the v1 session helpers such as
/// [`build_session`](crate::ConnectionTo::build_session). Requests sent
/// through it return the version-neutral [`crate::SentRequest`] and
/// [`crate::PreparedRequest`].
pub use crate::jsonrpc::ConnectionTo;

pub use crate::session::{
    ActiveSession, Blocking, LoadSessionBuilder, NonBlocking, OpenedSession, RestoreSessionBuilder,
    ResumeSessionBuilder, SessionBlockState, SessionBuilder, SessionMessage,
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

/// A message received by a typed handler on a v1 [`Builder`]: the
/// [`Dispatch`](crate::Dispatch) with v1 request handles.
pub type Dispatch<Req = UntypedMessage, Notif = UntypedMessage> = crate::Dispatch<Req, Notif, V1>;
