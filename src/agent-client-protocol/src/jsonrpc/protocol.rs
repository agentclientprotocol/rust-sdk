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

/// An ACP protocol version, as a type.
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
/// | [`V1`] (the default) | [`schema::v1::RequestId`](crate::schema::v1::RequestId) | [`ConnectionTo`] |
/// | `V2` (`unstable_protocol_v2`) | `schema::v2::RequestId` | `V2ConnectionTo` |
///
/// Handles created by `Client.builder()`, `Agent.builder()` and
/// `Proxy.builder()` use [`V1`], so v1 code never names the marker. Handles
/// created by the v2 builders (`Client.v2()`, `Agent.v2()`, `Proxy.v2()`) and
/// by `V2ConnectionTo` use `V2`.
///
/// Every handle also has a version-neutral `raw_id()` that returns the
/// [`RawJsonRpcId`] exactly as it appears on the wire. Low-level handlers that
/// are not tied to one version ([`HandleDispatchFrom`](crate::HandleDispatchFrom),
/// [`MatchDispatch`](crate::util::MatchDispatch)) receive the default [`V1`]
/// handles and should use `raw_id()`.
///
/// ```
/// use agent_client_protocol::{RawJsonRpcId, Responder, schema::v1};
///
/// fn log_request(responder: &Responder<v1::PromptResponse>) {
///     // `Responder<T>` is `Responder<T, V1>`.
///     let id: &v1::RequestId = responder.id();
///     let wire_id: &RawJsonRpcId = responder.raw_id();
///     assert_eq!(RawJsonRpcId::from(id.clone()), *wire_id);
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
/// version.
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

/// ACP protocol version 1, the default protocol marker.
///
/// See [`Protocol`].
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq, Hash)]
pub struct V1;

impl sealed::Sealed for V1 {
    fn connection_from_raw<Counterpart: Role>(
        connection: ConnectionTo<Counterpart>,
    ) -> ConnectionTo<Counterpart> {
        connection
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
    type Connection<Counterpart: Role> = ConnectionTo<Counterpart>;
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
    ) -> super::V2ConnectionTo<Counterpart> {
        super::V2ConnectionTo { inner: connection }
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
    type Connection<Counterpart: Role> = super::V2ConnectionTo<Counterpart>;
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
}

impl HandleId<V1> {
    /// The v1 ID of any wire ID; v1 allows every JSON-RPC ID.
    pub(crate) fn v1(raw: RawJsonRpcId) -> Self {
        Self {
            typed: raw.clone().into(),
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
