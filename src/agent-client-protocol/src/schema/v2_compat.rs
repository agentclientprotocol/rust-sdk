//! Runtime conversion between v1 wire payloads and the SDK's v2 default types.

use std::sync::{
    Arc,
    atomic::{AtomicU8, Ordering},
};

use agent_client_protocol_schema::v2;
use agent_client_protocol_schema::v2::conversion::{IntoV1, IntoV2};
use agent_client_protocol_schema::{self as v1, ProtocolVersion};
use serde::{Serialize, de::DeserializeOwned};
use serde_json::Value;

use crate::{Dispatch, UntypedMessage};

const STATE_UNKNOWN: u8 = 0;
const STATE_V1: u8 = 1;
const STATE_V2: u8 = 2;

const METHOD_INITIALIZE: &str = "initialize";
const METHOD_INITIALIZE_PROXY: &str = "_proxy/initialize";
const METHOD_SUCCESSOR_MESSAGE: &str = "_proxy/successor";

#[derive(Clone, Debug, Default)]
pub(crate) struct ProtocolState {
    active_wire_version: Arc<AtomicU8>,
    negotiated_wire_version: Arc<AtomicU8>,
}

impl ProtocolState {
    pub(crate) fn negotiated_protocol_version(&self) -> Option<ProtocolVersion> {
        match self.negotiated_wire_version.load(Ordering::SeqCst) {
            STATE_V1 => Some(ProtocolVersion::V1),
            STATE_V2 => Some(ProtocolVersion::V2),
            STATE_UNKNOWN => None,
            _ => unreachable!("invalid protocol state"),
        }
    }

    pub(crate) fn convert_incoming_dispatch(
        &self,
        dispatch: Dispatch,
    ) -> Result<Dispatch, IncomingConversionError> {
        match dispatch {
            Dispatch::Request(message, responder) => {
                let original = message.clone();
                match self.convert_incoming_message(MessageKind::Request, message) {
                    Ok(message) => Ok(Dispatch::Request(message, responder)),
                    Err(error) => Err(IncomingConversionError {
                        dispatch: Dispatch::Request(original, responder),
                        error,
                    }),
                }
            }
            Dispatch::Notification(message) => {
                let original = message.clone();
                match self.convert_incoming_message(MessageKind::Notification, message) {
                    Ok(message) => Ok(Dispatch::Notification(message)),
                    Err(error) => Err(IncomingConversionError {
                        dispatch: Dispatch::Notification(original),
                        error,
                    }),
                }
            }
            Dispatch::Response(result, router) => {
                let result = self
                    .convert_incoming_response(router.method(), result)
                    .unwrap_or_else(Err);
                Ok(Dispatch::Response(result, router))
            }
        }
    }

    pub(crate) fn convert_outgoing_request(
        &self,
        message: UntypedMessage,
    ) -> Result<UntypedMessage, crate::Error> {
        self.convert_outgoing_message(MessageKind::Request, message)
    }

    pub(crate) fn convert_outgoing_notification(
        &self,
        message: UntypedMessage,
    ) -> Result<UntypedMessage, crate::Error> {
        self.convert_outgoing_message(MessageKind::Notification, message)
    }

    pub(crate) fn convert_outgoing_response(
        &self,
        method: &str,
        response: Result<Value, crate::Error>,
    ) -> Result<Result<Value, crate::Error>, crate::Error> {
        let Ok(value) = response else {
            return Ok(response);
        };

        let wire_version = self.outgoing_response_wire_version(method, &value);
        let value = match wire_version {
            WireVersion::V1 => response_v2_to_v1(method, value)?,
            WireVersion::V2 => value,
        };
        Ok(Ok(value))
    }

    fn convert_incoming_message(
        &self,
        kind: MessageKind,
        message: UntypedMessage,
    ) -> Result<UntypedMessage, crate::Error> {
        let wire_version = self.incoming_message_wire_version(&message);
        let params = match wire_version {
            WireVersion::V1 => match kind {
                MessageKind::Request => request_v1_to_v2(&message.method, message.params)?,
                MessageKind::Notification => {
                    notification_v1_to_v2(&message.method, message.params)?
                }
            },
            WireVersion::V2 => message.params,
        };
        Ok(UntypedMessage {
            method: message.method,
            params,
        })
    }

    fn convert_outgoing_message(
        &self,
        kind: MessageKind,
        message: UntypedMessage,
    ) -> Result<UntypedMessage, crate::Error> {
        let wire_version = self.outgoing_message_wire_version(&message);
        let params = match wire_version {
            WireVersion::V1 => match kind {
                MessageKind::Request => request_v2_to_v1(&message.method, message.params)?,
                MessageKind::Notification => {
                    notification_v2_to_v1(&message.method, message.params)?
                }
            },
            WireVersion::V2 => message.params,
        };
        Ok(UntypedMessage {
            method: message.method,
            params,
        })
    }

    fn convert_incoming_response(
        &self,
        method: &str,
        response: Result<Value, crate::Error>,
    ) -> Result<Result<Value, crate::Error>, crate::Error> {
        let Ok(value) = response else {
            return Ok(response);
        };

        let wire_version = self.incoming_response_wire_version(method, &value);
        let value = match wire_version {
            WireVersion::V1 => response_v1_to_v2(method, value)?,
            WireVersion::V2 => value,
        };
        Ok(Ok(value))
    }

    fn incoming_message_wire_version(&self, message: &UntypedMessage) -> WireVersion {
        if is_initialize_method(&message.method) {
            let wire_version = wire_version_from_params(&message.params);
            self.set_provisional_wire_version(wire_version);
            return wire_version;
        }

        self.current_wire_version().unwrap_or(WireVersion::V2)
    }

    fn outgoing_message_wire_version(&self, message: &UntypedMessage) -> WireVersion {
        if is_initialize_method(&message.method) {
            let wire_version = wire_version_from_params(&message.params);
            self.set_provisional_wire_version(wire_version);
            return wire_version;
        }

        self.current_wire_version().unwrap_or(WireVersion::V2)
    }

    fn incoming_response_wire_version(&self, method: &str, value: &Value) -> WireVersion {
        if is_initialize_method(method) {
            let wire_version = wire_version_from_params(value);
            self.set_wire_version(wire_version);
            return wire_version;
        }

        self.current_wire_version().unwrap_or(WireVersion::V2)
    }

    fn outgoing_response_wire_version(&self, method: &str, value: &Value) -> WireVersion {
        if is_initialize_method(method) {
            let wire_version = wire_version_from_params(value);
            self.set_wire_version(wire_version);
            return wire_version;
        }

        self.current_wire_version().unwrap_or(WireVersion::V2)
    }

    fn current_wire_version(&self) -> Option<WireVersion> {
        match self.active_wire_version.load(Ordering::SeqCst) {
            STATE_UNKNOWN => None,
            STATE_V1 => Some(WireVersion::V1),
            STATE_V2 => Some(WireVersion::V2),
            _ => unreachable!("invalid protocol state"),
        }
    }

    fn set_provisional_wire_version(&self, version: WireVersion) {
        self.active_wire_version
            .store(version as u8, Ordering::SeqCst);
    }

    fn set_wire_version(&self, version: WireVersion) {
        self.set_provisional_wire_version(version);
        self.negotiated_wire_version
            .store(version as u8, Ordering::SeqCst);
    }
}

#[derive(Debug)]
pub(crate) struct IncomingConversionError {
    pub(crate) dispatch: Dispatch,
    pub(crate) error: crate::Error,
}

#[derive(Clone, Copy, Debug)]
enum MessageKind {
    Request,
    Notification,
}

#[derive(Clone, Copy, Debug)]
enum WireVersion {
    V1 = STATE_V1 as isize,
    V2 = STATE_V2 as isize,
}

fn is_initialize_method(method: &str) -> bool {
    matches!(method, METHOD_INITIALIZE | METHOD_INITIALIZE_PROXY)
}

fn wire_version_for_protocol_version(version: ProtocolVersion) -> WireVersion {
    if version >= ProtocolVersion::V2 {
        WireVersion::V2
    } else {
        WireVersion::V1
    }
}

fn wire_version_from_params(params: &Value) -> WireVersion {
    protocol_version_from_params(params).map_or(WireVersion::V1, wire_version_for_protocol_version)
}

fn protocol_version_from_params(params: &Value) -> Option<ProtocolVersion> {
    params
        .get("protocolVersion")
        .cloned()
        .and_then(|value| serde_json::from_value(value).ok())
}

fn conversion_error(error: v2::conversion::ProtocolConversionError) -> crate::Error {
    crate::Error::internal_error().data(error.to_string())
}

fn into_v1_value<T>(value: Value) -> Result<Value, crate::Error>
where
    T: DeserializeOwned + IntoV1,
    T::Output: Serialize,
{
    let typed = serde_json::from_value::<T>(value)?;
    let converted = typed.into_v1().map_err(conversion_error)?;
    serde_json::to_value(converted).map_err(crate::Error::into_internal_error)
}

fn into_v2_value<T>(value: Value) -> Result<Value, crate::Error>
where
    T: DeserializeOwned + IntoV2,
    T::Output: Serialize,
{
    let typed = serde_json::from_value::<T>(value)?;
    let converted = typed.into_v2().map_err(conversion_error)?;
    serde_json::to_value(converted).map_err(crate::Error::into_internal_error)
}

macro_rules! convert_request_to_v1 {
    ($method:expr, $params:expr, {$($(#[$meta:meta])* $name:pat => $ty:ty,)*}) => {
        match $method {
            $($(#[$meta])* $name => into_v1_value::<$ty>($params),)*
            _ if $method == METHOD_SUCCESSOR_MESSAGE => {
                convert_successor_message($params, request_v2_to_v1)
            }
            _ => Ok($params),
        }
    };
}

macro_rules! convert_request_to_v2 {
    ($method:expr, $params:expr, {$($(#[$meta:meta])* $name:pat => $ty:ty,)*}) => {
        match $method {
            $($(#[$meta])* $name => into_v2_value::<$ty>($params),)*
            _ if $method == METHOD_SUCCESSOR_MESSAGE => {
                convert_successor_message($params, request_v1_to_v2)
            }
            _ => Ok($params),
        }
    };
}

macro_rules! convert_notification_to_v1 {
    ($method:expr, $params:expr, {$($(#[$meta:meta])* $name:pat => $ty:ty,)*}) => {
        match $method {
            $($(#[$meta])* $name => into_v1_value::<$ty>($params),)*
            _ if $method == METHOD_SUCCESSOR_MESSAGE => {
                convert_successor_message($params, notification_v2_to_v1)
            }
            _ => Ok($params),
        }
    };
}

macro_rules! convert_notification_to_v2 {
    ($method:expr, $params:expr, {$($(#[$meta:meta])* $name:pat => $ty:ty,)*}) => {
        match $method {
            $($(#[$meta])* $name => into_v2_value::<$ty>($params),)*
            _ if $method == METHOD_SUCCESSOR_MESSAGE => {
                convert_successor_message($params, notification_v1_to_v2)
            }
            _ => Ok($params),
        }
    };
}

macro_rules! convert_response_to_v1 {
    ($method:expr, $params:expr, {$($(#[$meta:meta])* $name:pat => $ty:ty,)*}) => {
        match $method {
            $($(#[$meta])* $name => into_v1_value::<$ty>($params),)*
            _ => Ok($params),
        }
    };
}

macro_rules! convert_response_to_v2 {
    ($method:expr, $params:expr, {$($(#[$meta:meta])* $name:pat => $ty:ty,)*}) => {
        match $method {
            $($(#[$meta])* $name => into_v2_value::<$ty>($params),)*
            _ => Ok($params),
        }
    };
}

fn request_v2_to_v1(method: &str, params: Value) -> Result<Value, crate::Error> {
    convert_request_to_v1!(method, params, {
        METHOD_INITIALIZE => v2::InitializeRequest,
        METHOD_INITIALIZE_PROXY => v2::InitializeRequest,
        "authenticate" => v2::AuthenticateRequest,
        #[cfg(feature = "unstable_logout")]
        "logout" => v2::LogoutRequest,
        "session/new" => v2::NewSessionRequest,
        "session/load" => v2::LoadSessionRequest,
        "session/list" => v2::ListSessionsRequest,
        #[cfg(feature = "unstable_session_fork")]
        "session/fork" => v2::ForkSessionRequest,
        "session/resume" => v2::ResumeSessionRequest,
        "session/close" => v2::CloseSessionRequest,
        "session/set_mode" => v2::SetSessionModeRequest,
        "session/set_config_option" => v2::SetSessionConfigOptionRequest,
        "session/prompt" => v2::PromptRequest,
        #[cfg(feature = "unstable_session_model")]
        "session/set_model" => v2::SetSessionModelRequest,
        "fs/write_text_file" => v2::WriteTextFileRequest,
        "fs/read_text_file" => v2::ReadTextFileRequest,
        "session/request_permission" => v2::RequestPermissionRequest,
        "terminal/create" => v2::CreateTerminalRequest,
        "terminal/output" => v2::TerminalOutputRequest,
        "terminal/release" => v2::ReleaseTerminalRequest,
        "terminal/wait_for_exit" => v2::WaitForTerminalExitRequest,
        "terminal/kill" => v2::KillTerminalRequest,
    })
}

fn request_v1_to_v2(method: &str, params: Value) -> Result<Value, crate::Error> {
    convert_request_to_v2!(method, params, {
        METHOD_INITIALIZE => v1::InitializeRequest,
        METHOD_INITIALIZE_PROXY => v1::InitializeRequest,
        "authenticate" => v1::AuthenticateRequest,
        #[cfg(feature = "unstable_logout")]
        "logout" => v1::LogoutRequest,
        "session/new" => v1::NewSessionRequest,
        "session/load" => v1::LoadSessionRequest,
        "session/list" => v1::ListSessionsRequest,
        #[cfg(feature = "unstable_session_fork")]
        "session/fork" => v1::ForkSessionRequest,
        "session/resume" => v1::ResumeSessionRequest,
        "session/close" => v1::CloseSessionRequest,
        "session/set_mode" => v1::SetSessionModeRequest,
        "session/set_config_option" => v1::SetSessionConfigOptionRequest,
        "session/prompt" => v1::PromptRequest,
        #[cfg(feature = "unstable_session_model")]
        "session/set_model" => v1::SetSessionModelRequest,
        "fs/write_text_file" => v1::WriteTextFileRequest,
        "fs/read_text_file" => v1::ReadTextFileRequest,
        "session/request_permission" => v1::RequestPermissionRequest,
        "terminal/create" => v1::CreateTerminalRequest,
        "terminal/output" => v1::TerminalOutputRequest,
        "terminal/release" => v1::ReleaseTerminalRequest,
        "terminal/wait_for_exit" => v1::WaitForTerminalExitRequest,
        "terminal/kill" => v1::KillTerminalRequest,
    })
}

fn notification_v2_to_v1(method: &str, params: Value) -> Result<Value, crate::Error> {
    convert_notification_to_v1!(method, params, {
        "session/cancel" => v2::CancelNotification,
        "session/update" => v2::SessionNotification,
    })
}

fn notification_v1_to_v2(method: &str, params: Value) -> Result<Value, crate::Error> {
    convert_notification_to_v2!(method, params, {
        "session/cancel" => v1::CancelNotification,
        "session/update" => v1::SessionNotification,
    })
}

fn response_v2_to_v1(method: &str, params: Value) -> Result<Value, crate::Error> {
    convert_response_to_v1!(method, params, {
        METHOD_INITIALIZE => v2::InitializeResponse,
        METHOD_INITIALIZE_PROXY => v2::InitializeResponse,
        "authenticate" => v2::AuthenticateResponse,
        #[cfg(feature = "unstable_logout")]
        "logout" => v2::LogoutResponse,
        "session/new" => v2::NewSessionResponse,
        "session/load" => v2::LoadSessionResponse,
        "session/list" => v2::ListSessionsResponse,
        #[cfg(feature = "unstable_session_fork")]
        "session/fork" => v2::ForkSessionResponse,
        "session/resume" => v2::ResumeSessionResponse,
        "session/close" => v2::CloseSessionResponse,
        "session/set_mode" => v2::SetSessionModeResponse,
        "session/set_config_option" => v2::SetSessionConfigOptionResponse,
        "session/prompt" => v2::PromptResponse,
        #[cfg(feature = "unstable_session_model")]
        "session/set_model" => v2::SetSessionModelResponse,
        "fs/write_text_file" => v2::WriteTextFileResponse,
        "fs/read_text_file" => v2::ReadTextFileResponse,
        "session/request_permission" => v2::RequestPermissionResponse,
        "terminal/create" => v2::CreateTerminalResponse,
        "terminal/output" => v2::TerminalOutputResponse,
        "terminal/release" => v2::ReleaseTerminalResponse,
        "terminal/wait_for_exit" => v2::WaitForTerminalExitResponse,
        "terminal/kill" => v2::KillTerminalResponse,
    })
}

fn response_v1_to_v2(method: &str, params: Value) -> Result<Value, crate::Error> {
    convert_response_to_v2!(method, params, {
        METHOD_INITIALIZE => v1::InitializeResponse,
        METHOD_INITIALIZE_PROXY => v1::InitializeResponse,
        "authenticate" => v1::AuthenticateResponse,
        #[cfg(feature = "unstable_logout")]
        "logout" => v1::LogoutResponse,
        "session/new" => v1::NewSessionResponse,
        "session/load" => v1::LoadSessionResponse,
        "session/list" => v1::ListSessionsResponse,
        #[cfg(feature = "unstable_session_fork")]
        "session/fork" => v1::ForkSessionResponse,
        "session/resume" => v1::ResumeSessionResponse,
        "session/close" => v1::CloseSessionResponse,
        "session/set_mode" => v1::SetSessionModeResponse,
        "session/set_config_option" => v1::SetSessionConfigOptionResponse,
        "session/prompt" => v1::PromptResponse,
        #[cfg(feature = "unstable_session_model")]
        "session/set_model" => v1::SetSessionModelResponse,
        "fs/write_text_file" => v1::WriteTextFileResponse,
        "fs/read_text_file" => v1::ReadTextFileResponse,
        "session/request_permission" => v1::RequestPermissionResponse,
        "terminal/create" => v1::CreateTerminalResponse,
        "terminal/output" => v1::TerminalOutputResponse,
        "terminal/release" => v1::ReleaseTerminalResponse,
        "terminal/wait_for_exit" => v1::WaitForTerminalExitResponse,
        "terminal/kill" => v1::KillTerminalResponse,
    })
}

fn convert_successor_message(
    params: Value,
    convert_inner: fn(&str, Value) -> Result<Value, crate::Error>,
) -> Result<Value, crate::Error> {
    let Value::Object(mut message) = params else {
        return Ok(params);
    };
    let Some(Value::String(method)) = message.get("method").cloned() else {
        return Ok(Value::Object(message));
    };
    let Some(params) = message.remove("params") else {
        return Ok(Value::Object(message));
    };

    let params = convert_inner(&method, params)?;
    message.insert("params".to_string(), params);
    Ok(Value::Object(message))
}
