//! JsonRpc trait implementations for the experimental ACP v2 schema.
//!
//! These impls expose the v2 Rust API while letting the connection choose the
//! active wire schema. When a connection negotiates v1, v2 values are converted
//! through the schema crate's compatibility layer. When it negotiates v2, they
//! serialize and parse as native v2 JSON.

use crate::schema::v2::conversion::{IntoV1, IntoV2};
use crate::schema::{self, ProtocolVersion, v2};
use crate::{JsonRpcMessage, JsonRpcNotification, JsonRpcRequest, JsonRpcResponse, UntypedMessage};

fn uses_v2_wire(protocol_version: ProtocolVersion) -> bool {
    protocol_version >= ProtocolVersion::V2
}

macro_rules! impl_v2_jsonrpc_request {
    ($v2_req:ty, $v1_req:ty, $v2_resp:ty, $v1_resp:ty, $method:literal) => {
        impl JsonRpcMessage for $v2_req {
            fn matches_method(method: &str) -> bool {
                method == $method
            }

            fn method(&self) -> &str {
                $method
            }

            fn to_untyped_message(&self) -> Result<UntypedMessage, crate::Error> {
                self.to_untyped_message_for_protocol(ProtocolVersion::LATEST)
            }

            fn to_untyped_message_for_protocol(
                &self,
                protocol_version: ProtocolVersion,
            ) -> Result<UntypedMessage, crate::Error> {
                if uses_v2_wire(protocol_version) {
                    return UntypedMessage::new($method, self);
                }

                let v1: $v1_req = self.clone().into_v1()?;
                UntypedMessage::new($method, &v1)
            }

            fn parse_message(
                method: &str,
                params: &impl serde::Serialize,
            ) -> Result<Self, crate::Error> {
                if method != $method {
                    return Err(crate::Error::method_not_found());
                }

                let v1: $v1_req = crate::util::json_cast_params(params)?;
                Ok(v1.into_v2()?)
            }

            fn parse_message_for_protocol(
                method: &str,
                params: &impl serde::Serialize,
                protocol_version: ProtocolVersion,
            ) -> Result<Self, crate::Error> {
                if method != $method {
                    return Err(crate::Error::method_not_found());
                }

                if uses_v2_wire(protocol_version) {
                    return crate::util::json_cast_params(params);
                }

                let v1: $v1_req = crate::util::json_cast_params(params)?;
                Ok(v1.into_v2()?)
            }
        }

        impl JsonRpcRequest for $v2_req {
            type Response = $v2_resp;
        }

        impl JsonRpcResponse for $v2_resp {
            fn into_json(self, _method: &str) -> Result<serde_json::Value, crate::Error> {
                self.into_json_for_protocol($method, ProtocolVersion::LATEST)
            }

            fn into_json_for_protocol(
                self,
                _method: &str,
                protocol_version: ProtocolVersion,
            ) -> Result<serde_json::Value, crate::Error> {
                if uses_v2_wire(protocol_version) {
                    return serde_json::to_value(self).map_err(crate::Error::into_internal_error);
                }

                let v1: $v1_resp = self.into_v1()?;
                serde_json::to_value(v1).map_err(crate::Error::into_internal_error)
            }

            fn from_value(_method: &str, value: serde_json::Value) -> Result<Self, crate::Error> {
                Self::from_value_for_protocol($method, value, ProtocolVersion::LATEST)
            }

            fn from_value_for_protocol(
                _method: &str,
                value: serde_json::Value,
                protocol_version: ProtocolVersion,
            ) -> Result<Self, crate::Error> {
                if uses_v2_wire(protocol_version) {
                    return crate::util::json_cast(&value);
                }

                let v1: $v1_resp = crate::util::json_cast(&value)?;
                Ok(v1.into_v2()?)
            }
        }
    };
}

macro_rules! impl_v2_jsonrpc_notification {
    ($v2_notif:ty, $v1_notif:ty, $method:literal) => {
        impl JsonRpcMessage for $v2_notif {
            fn matches_method(method: &str) -> bool {
                method == $method
            }

            fn method(&self) -> &str {
                $method
            }

            fn to_untyped_message(&self) -> Result<UntypedMessage, crate::Error> {
                self.to_untyped_message_for_protocol(ProtocolVersion::LATEST)
            }

            fn to_untyped_message_for_protocol(
                &self,
                protocol_version: ProtocolVersion,
            ) -> Result<UntypedMessage, crate::Error> {
                if uses_v2_wire(protocol_version) {
                    return UntypedMessage::new($method, self);
                }

                let v1: $v1_notif = self.clone().into_v1()?;
                UntypedMessage::new($method, &v1)
            }

            fn parse_message(
                method: &str,
                params: &impl serde::Serialize,
            ) -> Result<Self, crate::Error> {
                if method != $method {
                    return Err(crate::Error::method_not_found());
                }

                let v1: $v1_notif = crate::util::json_cast_params(params)?;
                Ok(v1.into_v2()?)
            }

            fn parse_message_for_protocol(
                method: &str,
                params: &impl serde::Serialize,
                protocol_version: ProtocolVersion,
            ) -> Result<Self, crate::Error> {
                if method != $method {
                    return Err(crate::Error::method_not_found());
                }

                if uses_v2_wire(protocol_version) {
                    return crate::util::json_cast_params(params);
                }

                let v1: $v1_notif = crate::util::json_cast_params(params)?;
                Ok(v1.into_v2()?)
            }
        }

        impl JsonRpcNotification for $v2_notif {}
    };
}

macro_rules! impl_v2_jsonrpc_request_enum {
    ($v2_enum:ty, $v1_enum:ty {
        $( $(#[$meta:meta])* $variant:ident => $method:literal, )*
        [ext] $ext_variant:ident,
    }) => {
        impl JsonRpcMessage for $v2_enum {
            fn matches_method(method: &str) -> bool {
                <$v1_enum as JsonRpcMessage>::matches_method(method)
            }

            fn method(&self) -> &str {
                self.method()
            }

            fn to_untyped_message(&self) -> Result<UntypedMessage, crate::Error> {
                self.to_untyped_message_for_protocol(ProtocolVersion::LATEST)
            }

            fn to_untyped_message_for_protocol(
                &self,
                protocol_version: ProtocolVersion,
            ) -> Result<UntypedMessage, crate::Error> {
                if uses_v2_wire(protocol_version) {
                    return UntypedMessage::new(self.method(), self);
                }

                let v1: $v1_enum = self.clone().into_v1()?;
                v1.to_untyped_message()
            }

            fn parse_message(
                method: &str,
                params: &impl serde::Serialize,
            ) -> Result<Self, crate::Error> {
                let v1 = <$v1_enum as JsonRpcMessage>::parse_message(method, params)?;
                Ok(v1.into_v2()?)
            }

            fn parse_message_for_protocol(
                method: &str,
                params: &impl serde::Serialize,
                protocol_version: ProtocolVersion,
            ) -> Result<Self, crate::Error> {
                if uses_v2_wire(protocol_version) {
                    return match method {
                        $( $(#[$meta])* $method => crate::util::json_cast_params(params).map(Self::$variant), )*
                        _ => {
                            if let Some(custom_method) = method.strip_prefix('_') {
                                crate::util::json_cast_params(params).map(
                                    |ext_req: v2::ExtRequest| {
                                        Self::$ext_variant(v2::ExtRequest::new(
                                            custom_method.to_string(),
                                            ext_req.params,
                                        ))
                                    },
                                )
                            } else {
                                Err(crate::Error::method_not_found())
                            }
                        }
                    };
                }

                let v1 = <$v1_enum as JsonRpcMessage>::parse_message(method, params)?;
                Ok(v1.into_v2()?)
            }
        }

        impl JsonRpcRequest for $v2_enum {
            type Response = serde_json::Value;
        }
    };
}

macro_rules! impl_v2_jsonrpc_notification_enum {
    ($v2_enum:ty, $v1_enum:ty {
        $( $(#[$meta:meta])* $variant:ident => $method:literal, )*
        [ext] $ext_variant:ident,
    }) => {
        impl JsonRpcMessage for $v2_enum {
            fn matches_method(method: &str) -> bool {
                <$v1_enum as JsonRpcMessage>::matches_method(method)
            }

            fn method(&self) -> &str {
                self.method()
            }

            fn to_untyped_message(&self) -> Result<UntypedMessage, crate::Error> {
                self.to_untyped_message_for_protocol(ProtocolVersion::LATEST)
            }

            fn to_untyped_message_for_protocol(
                &self,
                protocol_version: ProtocolVersion,
            ) -> Result<UntypedMessage, crate::Error> {
                if uses_v2_wire(protocol_version) {
                    return UntypedMessage::new(self.method(), self);
                }

                let v1: $v1_enum = self.clone().into_v1()?;
                v1.to_untyped_message()
            }

            fn parse_message(
                method: &str,
                params: &impl serde::Serialize,
            ) -> Result<Self, crate::Error> {
                let v1 = <$v1_enum as JsonRpcMessage>::parse_message(method, params)?;
                Ok(v1.into_v2()?)
            }

            fn parse_message_for_protocol(
                method: &str,
                params: &impl serde::Serialize,
                protocol_version: ProtocolVersion,
            ) -> Result<Self, crate::Error> {
                if uses_v2_wire(protocol_version) {
                    return match method {
                        $( $(#[$meta])* $method => crate::util::json_cast_params(params).map(Self::$variant), )*
                        _ => {
                            if let Some(custom_method) = method.strip_prefix('_') {
                                crate::util::json_cast_params(params).map(
                                    |ext_notif: v2::ExtNotification| {
                                        Self::$ext_variant(v2::ExtNotification::new(
                                            custom_method.to_string(),
                                            ext_notif.params,
                                        ))
                                    },
                                )
                            } else {
                                Err(crate::Error::method_not_found())
                            }
                        }
                    };
                }

                let v1 = <$v1_enum as JsonRpcMessage>::parse_message(method, params)?;
                Ok(v1.into_v2()?)
            }
        }

        impl JsonRpcNotification for $v2_enum {}
    };
}

// Client -> Agent requests.
impl JsonRpcMessage for v2::InitializeRequest {
    fn matches_method(method: &str) -> bool {
        method == "initialize"
    }

    fn method(&self) -> &str {
        "initialize"
    }

    fn to_untyped_message(&self) -> Result<UntypedMessage, crate::Error> {
        self.to_untyped_message_for_protocol(self.protocol_version)
    }

    fn protocol_version_hint(&self) -> Option<ProtocolVersion> {
        Some(self.protocol_version)
    }

    fn to_untyped_message_for_protocol(
        &self,
        protocol_version: ProtocolVersion,
    ) -> Result<UntypedMessage, crate::Error> {
        if uses_v2_wire(protocol_version) {
            return UntypedMessage::new("initialize", self);
        }

        let v1: schema::InitializeRequest = self.clone().into_v1()?;
        UntypedMessage::new("initialize", &v1)
    }

    fn parse_message(method: &str, params: &impl serde::Serialize) -> Result<Self, crate::Error> {
        let protocol_version = crate::util::json_cast_params::<_, serde_json::Value>(params)
            .ok()
            .and_then(|params| {
                params
                    .get("protocolVersion")
                    .and_then(|value| serde_json::from_value(value.clone()).ok())
            })
            .unwrap_or(ProtocolVersion::LATEST);
        Self::parse_message_for_protocol(method, params, protocol_version)
    }

    fn parse_message_for_protocol(
        method: &str,
        params: &impl serde::Serialize,
        protocol_version: ProtocolVersion,
    ) -> Result<Self, crate::Error> {
        if method != "initialize" {
            return Err(crate::Error::method_not_found());
        }

        if uses_v2_wire(protocol_version) {
            return crate::util::json_cast_params(params);
        }

        let v1: schema::InitializeRequest = crate::util::json_cast_params(params)?;
        Ok(v1.into_v2()?)
    }
}

impl JsonRpcRequest for v2::InitializeRequest {
    type Response = v2::InitializeResponse;
}

impl JsonRpcResponse for v2::InitializeResponse {
    fn into_json(self, method: &str) -> Result<serde_json::Value, crate::Error> {
        let protocol_version = self.protocol_version;
        self.into_json_for_protocol(method, protocol_version)
    }

    fn protocol_version_hint(&self, method: &str) -> Option<ProtocolVersion> {
        if method == "initialize" {
            Some(self.protocol_version)
        } else {
            None
        }
    }

    fn into_json_for_protocol(
        self,
        _method: &str,
        protocol_version: ProtocolVersion,
    ) -> Result<serde_json::Value, crate::Error> {
        if uses_v2_wire(protocol_version) {
            return serde_json::to_value(self).map_err(crate::Error::into_internal_error);
        }

        let v1: schema::InitializeResponse = self.into_v1()?;
        serde_json::to_value(v1).map_err(crate::Error::into_internal_error)
    }

    fn from_value(_method: &str, value: serde_json::Value) -> Result<Self, crate::Error> {
        let protocol_version = value
            .get("protocolVersion")
            .and_then(|value| serde_json::from_value(value.clone()).ok())
            .unwrap_or(ProtocolVersion::LATEST);
        Self::from_value_for_protocol("initialize", value, protocol_version)
    }

    fn from_value_for_protocol(
        _method: &str,
        value: serde_json::Value,
        protocol_version: ProtocolVersion,
    ) -> Result<Self, crate::Error> {
        if uses_v2_wire(protocol_version) {
            return crate::util::json_cast(&value);
        }

        let v1: schema::InitializeResponse = crate::util::json_cast(&value)?;
        Ok(v1.into_v2()?)
    }
}

macro_rules! impl_v2_request_singletons {
    ([
        $( $(#[$meta:meta])* (
            $variant:ident,
            $v1_req:ty,
            $v1_resp:ty,
            $v2_req:ty,
            $v2_resp:ty,
            $method:literal
        ), )*
    ], $ext_variant:ident) => {
        $(
            $(#[$meta])*
            impl_v2_request_singleton!($variant, $v1_req, $v1_resp, $v2_req, $v2_resp, $method);
        )*
    };
}

macro_rules! impl_v2_request_singleton {
    (
        InitializeRequest,
        $v1_req:ty,
        $v1_resp:ty,
        $v2_req:ty,
        $v2_resp:ty,
        $method:literal
    ) => {};
    ($variant:ident, $v1_req:ty, $v1_resp:ty, $v2_req:ty, $v2_resp:ty, $method:literal) => {
        impl_v2_jsonrpc_request!($v2_req, $v1_req, $v2_resp, $v1_resp, $method);
    };
}

macro_rules! impl_v2_notification_singletons {
    ([
        $( $(#[$meta:meta])* (
            $variant:ident,
            $v1_notif:ty,
            $v2_notif:ty,
            $method:literal
        ), )*
    ], $ext_variant:ident) => {
        $(
            $(#[$meta])*
            impl_v2_jsonrpc_notification!($v2_notif, $v1_notif, $method);
        )*
    };
}

client_request_methods!(impl_v2_request_singletons);
agent_request_methods!(impl_v2_request_singletons);
client_notification_methods!(impl_v2_notification_singletons);
agent_notification_methods!(impl_v2_notification_singletons);

impl_v2_client_request_enum!();
impl_v2_client_notification_enum!();
impl_v2_agent_request_enum!();
impl_v2_agent_notification_enum!();
