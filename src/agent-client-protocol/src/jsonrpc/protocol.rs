//! The protocol markers that type request handles and builder callbacks by ACP
//! version.

use std::fmt::{self, Debug, Display};
use std::hash::Hash;

use serde::Serialize;

use crate::jsonrpc::{ConnectionTo, RawJsonRpcId};
use crate::role::Role;
use crate::schema::v1;
#[cfg(feature = "unstable_protocol_v2")]
use crate::schema::v2;

/// The protocol version of a request handle or builder, as a type.
///
/// The request handles ([`Responder`](crate::Responder),
/// [`ResponseRouter`](crate::ResponseRouter), [`SentRequest`](crate::SentRequest),
/// [`PreparedRequest`](crate::PreparedRequest) and [`Dispatch`](crate::Dispatch))
/// and the connection [`Builder`](crate::Builder) take a protocol marker as their
/// last type parameter. It selects the request ID type their `id()` returns and
/// the connection type builder callbacks receive:
///
/// | Marker | `RequestId` | `Connection<Counterpart>` |
/// | --- | --- | --- |
/// | [`Raw`] (the default) | [`RawJsonRpcId`] | [`ConnectionTo`] |
/// | [`V1`] | [`schema::v1::RequestId`](crate::schema::v1::RequestId) | [`v1::ConnectionTo`](crate::v1::ConnectionTo) |
/// | `V2` (`unstable_protocol_v2`) | `schema::v2::RequestId` | `v2::ConnectionTo` |
///
/// - Typed handlers on `Client.v1()`, `Agent.v1()` and
///   `Proxy.v1()`, and requests sent through `v1::ConnectionTo`, use [`V1`].
/// - Typed handlers on `Client.v2()`, `Agent.v2()` and `Proxy.v2()`, and
///   requests sent through `v2::ConnectionTo`, use `V2`.
/// - Everything that is not tied to one version uses [`Raw`]: the
///   [`HandleDispatchFrom`](crate::HandleDispatchFrom) layer,
///   [`MatchDispatch`](crate::util::MatchDispatch), builders whose version
///   guard is disabled, and requests sent through [`ConnectionTo`].
///
/// The [`v1`](crate::v1) and `v2` modules name the typed handles:
/// `v1::Responder<T>` is `Responder<T, V1>`, `v2::SentRequest<T>` is
/// `SentRequest<T, V2>`, `v1::Builder<Host>` is `Builder<Host, .., V1>`, and so
/// on.
///
/// Every handle also has `raw_id()`, which returns the wire ID as a
/// [`RawJsonRpcId`] whatever the marker. A [`Raw`] handle converts to a typed
/// one with `into_protocol`, and any handle converts back with `into_raw`.
///
/// ```
/// use agent_client_protocol::{RawJsonRpcId, Responder, V1, schema::v1};
///
/// fn log_request(responder: &Responder<v1::PromptResponse, V1>) {
///     let id: &v1::RequestId = responder.id();
///     let wire_id: &RawJsonRpcId = responder.raw_id();
///     assert_eq!(RawJsonRpcId::from(id.clone()), *wire_id);
/// }
///
/// fn log_raw_request(responder: &Responder<v1::PromptResponse>) {
///     // `Responder<T>` is `Responder<T, Raw>`.
///     let id: &RawJsonRpcId = responder.id();
///     # let _ = id;
/// }
/// ```
///
/// Generic code can accept handles of any version:
///
/// ```
/// use agent_client_protocol::{Protocol, Responder, schema::v1};
///
/// fn reject<P: Protocol>(responder: Responder<v1::PromptResponse, P>) -> agent_client_protocol::Result<()> {
///     responder.respond_with_error(agent_client_protocol::Error::invalid_params())
/// }
/// ```
///
/// This trait is sealed: the SDK defines one marker per supported protocol
/// version, plus [`Raw`].
#[allow(private_bounds)]
pub trait Protocol: sealed::Sealed + Copy + Debug + Default + Send + Sync + 'static {
    /// The request ID type of this protocol version.
    type RequestId: Clone
        + Debug
        + Display
        + Eq
        + Hash
        + Serialize
        + Into<RawJsonRpcId>
        + Send
        + Sync
        + 'static;

    /// The connection type that builder callbacks receive for `Counterpart`.
    type Connection<Counterpart: Role>: Clone + Send + Sync + 'static;
}

pub(crate) mod sealed {
    use super::{ConnectionTo, Protocol, RawJsonRpcId, Role};

    pub trait Sealed {
        /// Wraps the version-neutral connection in this version's connection
        /// type.
        fn connection_from_raw<Counterpart: Role>(
            connection: ConnectionTo<Counterpart>,
        ) -> <Self as Protocol>::Connection<Counterpart>
        where
            Self: Protocol;

        /// Converts a wire ID to this version's request ID. Fails only for
        /// IDs the version does not allow.
        fn request_id_from_raw(
            id: &RawJsonRpcId,
        ) -> Result<<Self as Protocol>::RequestId, crate::Error>
        where
            Self: Protocol;

        /// The request ID for a string ID the SDK generated.
        fn request_id_from_string(id: String) -> <Self as Protocol>::RequestId
        where
            Self: Protocol;
    }
}

pub(crate) fn connection_from_raw<P: Protocol, Counterpart: Role>(
    connection: ConnectionTo<Counterpart>,
) -> P::Connection<Counterpart> {
    <P as sealed::Sealed>::connection_from_raw(connection)
}

/// The version-neutral protocol marker of the raw dispatch layer, and the
/// default.
///
/// `id()` on a `Raw` handle returns the [`RawJsonRpcId`] exactly as it
/// appears on the wire, which may be `null`. See [`Protocol`].
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq, Hash)]
pub struct Raw;

impl sealed::Sealed for Raw {
    fn connection_from_raw<Counterpart: Role>(
        connection: ConnectionTo<Counterpart>,
    ) -> ConnectionTo<Counterpart> {
        connection
    }

    fn request_id_from_raw(id: &RawJsonRpcId) -> Result<RawJsonRpcId, crate::Error> {
        Ok(id.clone())
    }

    fn request_id_from_string(id: String) -> RawJsonRpcId {
        RawJsonRpcId::Str(id)
    }
}

impl Protocol for Raw {
    type RequestId = RawJsonRpcId;
    type Connection<Counterpart: Role> = ConnectionTo<Counterpart>;
}

/// ACP protocol version 1.
///
/// See [`Protocol`].
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq, Hash)]
pub struct V1;

impl sealed::Sealed for V1 {
    fn connection_from_raw<Counterpart: Role>(
        connection: ConnectionTo<Counterpart>,
    ) -> super::v1_connection::ConnectionTo<Counterpart> {
        super::v1_connection::ConnectionTo { inner: connection }
    }

    fn request_id_from_raw(id: &RawJsonRpcId) -> Result<v1::RequestId, crate::Error> {
        Ok(id.clone().into())
    }

    fn request_id_from_string(id: String) -> v1::RequestId {
        v1::RequestId::Str(id)
    }
}

impl Protocol for V1 {
    type RequestId = v1::RequestId;
    type Connection<Counterpart: Role> = super::v1_connection::ConnectionTo<Counterpart>;
}

/// ACP protocol version 2.
///
/// See [`Protocol`]. Request IDs on v2 connections are never `null`: the SDK
/// rejects such requests before dispatch, so `id()` on a `V2` handle is
/// infallible.
#[cfg(feature = "unstable_protocol_v2")]
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq, Hash)]
pub struct V2;

#[cfg(feature = "unstable_protocol_v2")]
impl sealed::Sealed for V2 {
    fn connection_from_raw<Counterpart: Role>(
        connection: ConnectionTo<Counterpart>,
    ) -> super::v2_connection::ConnectionTo<Counterpart> {
        super::v2_connection::ConnectionTo { inner: connection }
    }

    fn request_id_from_raw(id: &RawJsonRpcId) -> Result<v2::RequestId, crate::Error> {
        id.clone().try_into()
    }

    fn request_id_from_string(id: String) -> v2::RequestId {
        v2::RequestId::Str(id)
    }
}

#[cfg(feature = "unstable_protocol_v2")]
impl Protocol for V2 {
    type RequestId = v2::RequestId;
    type Connection<Counterpart: Role> = super::v2_connection::ConnectionTo<Counterpart>;
}

/// The ID of a request handle: the wire ID and the same ID as the handle's
/// protocol version types it.
pub(crate) struct HandleId<P: Protocol> {
    raw: RawJsonRpcId,
    typed: P::RequestId,
}

impl<P: Protocol> HandleId<P> {
    /// The ID of a request the SDK sends.
    pub(crate) fn generated(id: String) -> Self {
        Self {
            typed: P::request_id_from_string(id.clone()),
            raw: RawJsonRpcId::Str(id),
        }
    }

    /// The ID of a received request or response. Fails only for IDs `P` does
    /// not allow.
    pub(crate) fn from_raw(raw: RawJsonRpcId) -> Result<Self, crate::Error> {
        Ok(Self {
            typed: P::request_id_from_raw(&raw)?,
            raw,
        })
    }

    pub(crate) fn raw(&self) -> &RawJsonRpcId {
        &self.raw
    }

    pub(crate) fn typed(&self) -> &P::RequestId {
        &self.typed
    }

    /// The same ID for protocol `Q`. Fails only for IDs `Q` does not allow.
    pub(crate) fn into_protocol<Q: Protocol>(self) -> Result<HandleId<Q>, crate::Error> {
        HandleId::from_raw(self.raw)
    }

    /// The same ID in the raw dispatch layer.
    pub(crate) fn into_raw(self) -> HandleId<Raw> {
        HandleId::raw_layer(self.raw)
    }
}

impl HandleId<Raw> {
    /// The ID of a message in the raw dispatch layer, which allows every
    /// JSON-RPC ID.
    pub(crate) fn raw_layer(raw: RawJsonRpcId) -> Self {
        Self {
            typed: raw.clone(),
            raw,
        }
    }
}

impl<P: Protocol> Clone for HandleId<P> {
    fn clone(&self) -> Self {
        Self {
            raw: self.raw.clone(),
            typed: self.typed.clone(),
        }
    }
}

impl<P: Protocol> Debug for HandleId<P> {
    /// Formats as the wire ID, as the handles did before they were typed.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        Debug::fmt(&self.raw, f)
    }
}
