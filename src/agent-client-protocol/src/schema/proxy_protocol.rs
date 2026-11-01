//! JSON-RPC support for the proxy-chain messages.
//!
//! The message types come from the schema crate, where they sit behind its
//! `unstable_proxy_chains` feature, which this SDK always enables:
//! `schema::v1` (and, with `unstable_protocol_v2`, `schema::v2`) has
//! [`InitializeProxyRequest`](super::v1::InitializeProxyRequest),
//! [`InitializeProxyResponse`](super::v1::InitializeProxyResponse),
//! [`ProxySuccessorRequest`](super::v1::ProxySuccessorRequest),
//! [`ProxySuccessorNotification`](super::v1::ProxySuccessorNotification),
//! [`ProxySuccessorResponse`](super::v1::ProxySuccessorResponse), the
//! [`ProxyRequest`](super::v1::ProxyRequest),
//! [`ProxyResponse`](super::v1::ProxyResponse) and
//! [`ProxyNotification`](super::v1::ProxyNotification) unions, and the method
//! names in [`PROXY_METHOD_NAMES`](super::v1::PROXY_METHOD_NAMES).
//!
//! This module implements the SDK's JSON-RPC traits for those types and adds
//! [`SuccessorMessage`], a version-neutral typed view of a `proxy/successor`
//! envelope.
//!
//! See the [Proxy Chains RFD](https://agentclientprotocol.com/rfds/proxy-chains).

use std::sync::Arc;

use serde_json::value::RawValue;

use crate::schema::{IntoOption, v1};
use crate::{JsonRpcMessage, JsonRpcNotification, JsonRpcRequest, JsonRpcResponse, UntypedMessage};

/// `proxy/successor`, the method of every successor envelope.
const PROXY_SUCCESSOR: &str = v1::PROXY_METHOD_NAMES.proxy_successor;
/// `proxy/initialize`, the method a conductor initializes proxies with.
const PROXY_INITIALIZE: &str = v1::PROXY_METHOD_NAMES.proxy_initialize;

// =============================================================================
// Schema proxy types
// =============================================================================

/// Implement the SDK's JSON-RPC traits for one version's schema proxy types.
macro_rules! impl_proxy_jsonrpc {
    ($ns:ident) => {
        const _: () = {
            use $crate::schema::$ns::{
                InitializeProxyRequest, InitializeProxyResponse, ProxyNotification, ProxyRequest,
                ProxyResponse, ProxySuccessorNotification, ProxySuccessorRequest,
                ProxySuccessorResponse,
            };

            impl_jsonrpc_request!(
                InitializeProxyRequest,
                InitializeProxyResponse,
                "proxy/initialize"
            );
            impl_jsonrpc_notification!(ProxySuccessorNotification, "proxy/successor");

            impl JsonRpcMessage for ProxySuccessorRequest {
                fn matches_method(method: &str) -> bool {
                    method == PROXY_SUCCESSOR
                }

                fn method(&self) -> &str {
                    PROXY_SUCCESSOR
                }

                fn to_untyped_message(&self) -> Result<UntypedMessage, crate::Error> {
                    UntypedMessage::new(PROXY_SUCCESSOR, self)
                }

                fn parse_message(
                    method: &str,
                    params: &serde_json::Value,
                ) -> Result<Self, crate::Error> {
                    if method != PROXY_SUCCESSOR {
                        return Err(crate::Error::method_not_found());
                    }
                    crate::util::json_cast_params(params)
                }
            }

            // The response is the wrapped request's result, unchanged.
            impl JsonRpcRequest for ProxySuccessorRequest {
                type Response = ProxySuccessorResponse;
            }

            impl JsonRpcResponse for ProxySuccessorResponse {
                fn into_json(self, _method: &str) -> Result<serde_json::Value, crate::Error> {
                    serde_json::from_str(self.0.get()).map_err(crate::Error::into_internal_error)
                }

                fn from_json(
                    _method: &str,
                    value: serde_json::Value,
                ) -> Result<Self, crate::Error> {
                    Ok(Self::new(to_raw_value(&value)?))
                }
            }

            impl JsonRpcMessage for ProxyRequest {
                fn matches_method(method: &str) -> bool {
                    method == PROXY_INITIALIZE || method == PROXY_SUCCESSOR
                }

                fn method(&self) -> &str {
                    <ProxyRequest>::method(self)
                }

                fn to_untyped_message(&self) -> Result<UntypedMessage, crate::Error> {
                    UntypedMessage::new(<ProxyRequest>::method(self), self)
                }

                fn parse_message(
                    method: &str,
                    params: &serde_json::Value,
                ) -> Result<Self, crate::Error> {
                    match method {
                        PROXY_INITIALIZE => crate::util::json_cast_params(params).map(
                            |value: InitializeProxyRequest| {
                                Self::InitializeProxyRequest(value.into())
                            },
                        ),
                        PROXY_SUCCESSOR => crate::util::json_cast_params(params).map(
                            |value: ProxySuccessorRequest| {
                                Self::ProxySuccessorRequest(value.into())
                            },
                        ),
                        _ => Err(crate::Error::method_not_found()),
                    }
                }
            }

            impl JsonRpcRequest for ProxyRequest {
                type Response = ProxyResponse;
            }

            impl JsonRpcResponse for ProxyResponse {
                fn into_json(self, _method: &str) -> Result<serde_json::Value, crate::Error> {
                    serde_json::to_value(self).map_err(crate::Error::into_internal_error)
                }

                fn from_json(method: &str, value: serde_json::Value) -> Result<Self, crate::Error> {
                    match method {
                        PROXY_INITIALIZE => {
                            crate::util::json_cast(value).map(|value: InitializeProxyResponse| {
                                Self::InitializeProxyResponse(value.into())
                            })
                        }
                        PROXY_SUCCESSOR => ProxySuccessorResponse::from_json(method, value).map(
                            |value: ProxySuccessorResponse| {
                                Self::ProxySuccessorResponse(value.into())
                            },
                        ),
                        _ => Err(crate::Error::method_not_found()),
                    }
                }
            }

            impl JsonRpcMessage for ProxyNotification {
                fn matches_method(method: &str) -> bool {
                    method == PROXY_SUCCESSOR
                }

                fn method(&self) -> &str {
                    <ProxyNotification>::method(self)
                }

                fn to_untyped_message(&self) -> Result<UntypedMessage, crate::Error> {
                    UntypedMessage::new(<ProxyNotification>::method(self), self)
                }

                fn parse_message(
                    method: &str,
                    params: &serde_json::Value,
                ) -> Result<Self, crate::Error> {
                    match method {
                        PROXY_SUCCESSOR => crate::util::json_cast_params(params).map(
                            |value: ProxySuccessorNotification| {
                                Self::ProxySuccessorNotification(value.into())
                            },
                        ),
                        _ => Err(crate::Error::method_not_found()),
                    }
                }
            }

            impl JsonRpcNotification for ProxyNotification {}
        };
    };
}

impl_proxy_jsonrpc!(v1);
#[cfg(feature = "unstable_protocol_v2")]
impl_proxy_jsonrpc!(v2);

fn to_raw_value(value: &serde_json::Value) -> Result<Arc<RawValue>, crate::Error> {
    serde_json::value::to_raw_value(value)
        .map(Arc::from)
        .map_err(crate::Error::into_internal_error)
}

// =============================================================================
// Typed successor envelopes
// =============================================================================

/// A typed view of a `proxy/successor` envelope.
///
/// The schema's [`ProxySuccessorRequest`](super::v1::ProxySuccessorRequest)
/// and [`ProxySuccessorNotification`](super::v1::ProxySuccessorNotification)
/// carry the wrapped message as a raw method name and raw params, with one
/// copy of each type per protocol version. `SuccessorMessage<M>` holds the
/// wrapped message as an SDK message type `M` instead and works for every
/// protocol version. For a wrapped request its response type is the wrapped
/// request's response type, because the result of a wrapped request is
/// returned unchanged as the result of `proxy/successor`.
///
/// Its JSON-RPC form is the schema envelope: the wrapped message's `method`
/// and `params`, next to the envelope's optional `_meta`. The SDK's proxy
/// routing uses it to wrap and unwrap messages for a proxy's successor.
///
/// Build one with [`SuccessorMessage::new`] and [`SuccessorMessage::meta`].
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct SuccessorMessage<M: JsonRpcMessage = UntypedMessage> {
    /// The message to be sent to the successor component.
    pub message: M,

    /// Metadata for the envelope itself, sent as `_meta`. The wrapped message
    /// keeps its own `_meta` in its params. A received `_meta` that is not an
    /// object is ignored, as the schema envelope specifies.
    pub meta: Option<v1::Meta>,
}

impl<M: JsonRpcMessage> SuccessorMessage<M> {
    /// Wrap `message` for the successor, without envelope metadata.
    #[must_use]
    pub fn new(message: M) -> Self {
        Self {
            message,
            meta: None,
        }
    }

    /// Set the envelope's `_meta` object, replacing any existing metadata.
    #[must_use]
    pub fn meta(mut self, meta: impl IntoOption<v1::Meta>) -> Self {
        self.meta = meta.into_option();
        self
    }
}

impl<M: JsonRpcMessage> JsonRpcMessage for SuccessorMessage<M> {
    fn matches_method(method: &str) -> bool {
        method == PROXY_SUCCESSOR
    }

    fn method(&self) -> &str {
        PROXY_SUCCESSOR
    }

    fn to_untyped_message(&self) -> Result<UntypedMessage, crate::Error> {
        let UntypedMessage { method, params } = self.message.to_untyped_message()?;
        // The params are always sent, as `null` when the wrapped message has
        // none. The schema treats `null` and an omitted `params` the same.
        let envelope = v1::ProxySuccessorRequest::new(method)
            .params(to_raw_value(&params)?)
            .meta(self.meta.clone());
        UntypedMessage::new(PROXY_SUCCESSOR, envelope)
    }

    fn parse_message(method: &str, params: &serde_json::Value) -> Result<Self, crate::Error> {
        if method != PROXY_SUCCESSOR {
            return Err(crate::Error::method_not_found());
        }
        // A notification envelope has the same fields as a request envelope.
        let envelope: v1::ProxySuccessorNotification = crate::util::json_cast_params(params)?;
        if !M::matches_method(&envelope.method) {
            return Err(crate::Error::method_not_found());
        }
        // Take the wrapped params from the JSON already in hand rather than
        // re-parsing the envelope's raw copy.
        let inner_params = params
            .get("params")
            .cloned()
            .unwrap_or(serde_json::Value::Null);
        let message = M::parse_message(&envelope.method, &inner_params)?;
        Ok(SuccessorMessage::new(message).meta(envelope.meta))
    }
}

impl<Req: JsonRpcRequest> JsonRpcRequest for SuccessorMessage<Req> {
    type Response = Req::Response;
}

impl<Notif: JsonRpcNotification> JsonRpcNotification for SuccessorMessage<Notif> {}
