use serde::Serialize;

use crate::jsonrpc::{JrMessage, JrRequest, JrResponsePayload};
use crate::schema::{
    CreateTerminalRequest, CreateTerminalResponse, KillTerminalCommandRequest,
    KillTerminalCommandResponse, ReadTextFileRequest, ReadTextFileResponse, ReleaseTerminalRequest,
    ReleaseTerminalResponse, RequestPermissionRequest, RequestPermissionResponse,
    TerminalOutputRequest, TerminalOutputResponse, WaitForTerminalExitRequest,
    WaitForTerminalExitResponse, WriteTextFileRequest, WriteTextFileResponse,
};
use crate::util::json_cast;

// Agent -> Client requests
// These are messages that agents send to clients/editors

// ============================================================================
// RequestPermissionRequest
// ============================================================================

impl JrMessage for RequestPermissionRequest {
    fn into_untyped_message(self) -> Result<crate::UntypedMessage, crate::Error> {
        let method = self.method().to_string();
        crate::UntypedMessage::new(&method, self)
    }

    fn method(&self) -> &str {
        "session/request_permission"
    }

    fn parse_request(method: &str, params: &impl Serialize) -> Option<Result<Self, crate::Error>> {
        if method != "session/request_permission" {
            return None;
        }
        Some(json_cast(params))
    }

    fn parse_notification(
        _method: &str,
        _params: &impl Serialize,
    ) -> Option<Result<Self, crate::Error>> {
        // This is a request, not a notification
        None
    }
}

impl JrRequest for RequestPermissionRequest {
    type Response = RequestPermissionResponse;
}

impl JrResponsePayload for RequestPermissionResponse {
    fn into_json(self, _method: &str) -> Result<serde_json::Value, crate::Error> {
        serde_json::to_value(self).map_err(crate::Error::into_internal_error)
    }

    fn from_value(_method: &str, value: serde_json::Value) -> Result<Self, crate::Error> {
        json_cast(&value)
    }
}

// ============================================================================
// WriteTextFileRequest
// ============================================================================

impl JrMessage for WriteTextFileRequest {
    fn into_untyped_message(self) -> Result<crate::UntypedMessage, crate::Error> {
        let method = self.method().to_string();
        crate::UntypedMessage::new(&method, self)
    }

    fn method(&self) -> &str {
        "fs/write_text_file"
    }

    fn parse_request(method: &str, params: &impl Serialize) -> Option<Result<Self, crate::Error>> {
        if method != "fs/write_text_file" {
            return None;
        }
        Some(json_cast(params))
    }

    fn parse_notification(
        _method: &str,
        _params: &impl Serialize,
    ) -> Option<Result<Self, crate::Error>> {
        // This is a request, not a notification
        None
    }
}

impl JrRequest for WriteTextFileRequest {
    type Response = WriteTextFileResponse;
}

impl JrResponsePayload for WriteTextFileResponse {
    fn into_json(self, _method: &str) -> Result<serde_json::Value, crate::Error> {
        serde_json::to_value(self).map_err(crate::Error::into_internal_error)
    }

    fn from_value(_method: &str, value: serde_json::Value) -> Result<Self, crate::Error> {
        json_cast(&value)
    }
}

// ============================================================================
// ReadTextFileRequest
// ============================================================================

impl JrMessage for ReadTextFileRequest {
    fn into_untyped_message(self) -> Result<crate::UntypedMessage, crate::Error> {
        let method = self.method().to_string();
        crate::UntypedMessage::new(&method, self)
    }

    fn method(&self) -> &str {
        "fs/read_text_file"
    }

    fn parse_request(method: &str, params: &impl Serialize) -> Option<Result<Self, crate::Error>> {
        if method != "fs/read_text_file" {
            return None;
        }
        Some(json_cast(params))
    }

    fn parse_notification(
        _method: &str,
        _params: &impl Serialize,
    ) -> Option<Result<Self, crate::Error>> {
        // This is a request, not a notification
        None
    }
}

impl JrRequest for ReadTextFileRequest {
    type Response = ReadTextFileResponse;
}

impl JrResponsePayload for ReadTextFileResponse {
    fn into_json(self, _method: &str) -> Result<serde_json::Value, crate::Error> {
        serde_json::to_value(self).map_err(crate::Error::into_internal_error)
    }

    fn from_value(_method: &str, value: serde_json::Value) -> Result<Self, crate::Error> {
        json_cast(&value)
    }
}

// ============================================================================
// CreateTerminalRequest
// ============================================================================

impl JrMessage for CreateTerminalRequest {
    fn into_untyped_message(self) -> Result<crate::UntypedMessage, crate::Error> {
        let method = self.method().to_string();
        crate::UntypedMessage::new(&method, self)
    }

    fn method(&self) -> &str {
        "terminal/create"
    }

    fn parse_request(method: &str, params: &impl Serialize) -> Option<Result<Self, crate::Error>> {
        if method != "terminal/create" {
            return None;
        }
        Some(json_cast(params))
    }

    fn parse_notification(
        _method: &str,
        _params: &impl Serialize,
    ) -> Option<Result<Self, crate::Error>> {
        // This is a request, not a notification
        None
    }
}

impl JrRequest for CreateTerminalRequest {
    type Response = CreateTerminalResponse;
}

impl JrResponsePayload for CreateTerminalResponse {
    fn into_json(self, _method: &str) -> Result<serde_json::Value, crate::Error> {
        serde_json::to_value(self).map_err(crate::Error::into_internal_error)
    }

    fn from_value(_method: &str, value: serde_json::Value) -> Result<Self, crate::Error> {
        json_cast(&value)
    }
}

// ============================================================================
// TerminalOutputRequest
// ============================================================================

impl JrMessage for TerminalOutputRequest {
    fn into_untyped_message(self) -> Result<crate::UntypedMessage, crate::Error> {
        let method = self.method().to_string();
        crate::UntypedMessage::new(&method, self)
    }

    fn method(&self) -> &str {
        "terminal/output"
    }

    fn parse_request(method: &str, params: &impl Serialize) -> Option<Result<Self, crate::Error>> {
        if method != "terminal/output" {
            return None;
        }
        Some(json_cast(params))
    }

    fn parse_notification(
        _method: &str,
        _params: &impl Serialize,
    ) -> Option<Result<Self, crate::Error>> {
        // This is a request, not a notification
        None
    }
}

impl JrRequest for TerminalOutputRequest {
    type Response = TerminalOutputResponse;
}

impl JrResponsePayload for TerminalOutputResponse {
    fn into_json(self, _method: &str) -> Result<serde_json::Value, crate::Error> {
        serde_json::to_value(self).map_err(crate::Error::into_internal_error)
    }

    fn from_value(_method: &str, value: serde_json::Value) -> Result<Self, crate::Error> {
        json_cast(&value)
    }
}

// ============================================================================
// ReleaseTerminalRequest
// ============================================================================

impl JrMessage for ReleaseTerminalRequest {
    fn into_untyped_message(self) -> Result<crate::UntypedMessage, crate::Error> {
        let method = self.method().to_string();
        crate::UntypedMessage::new(&method, self)
    }

    fn method(&self) -> &str {
        "terminal/release"
    }

    fn parse_request(method: &str, params: &impl Serialize) -> Option<Result<Self, crate::Error>> {
        if method != "terminal/release" {
            return None;
        }
        Some(json_cast(params))
    }

    fn parse_notification(
        _method: &str,
        _params: &impl Serialize,
    ) -> Option<Result<Self, crate::Error>> {
        // This is a request, not a notification
        None
    }
}

impl JrRequest for ReleaseTerminalRequest {
    type Response = ReleaseTerminalResponse;
}

impl JrResponsePayload for ReleaseTerminalResponse {
    fn into_json(self, _method: &str) -> Result<serde_json::Value, crate::Error> {
        serde_json::to_value(self).map_err(crate::Error::into_internal_error)
    }

    fn from_value(_method: &str, value: serde_json::Value) -> Result<Self, crate::Error> {
        json_cast(&value)
    }
}

// ============================================================================
// WaitForTerminalExitRequest
// ============================================================================

impl JrMessage for WaitForTerminalExitRequest {
    fn into_untyped_message(self) -> Result<crate::UntypedMessage, crate::Error> {
        let method = self.method().to_string();
        crate::UntypedMessage::new(&method, self)
    }

    fn method(&self) -> &str {
        "terminal/wait_for_exit"
    }

    fn parse_request(method: &str, params: &impl Serialize) -> Option<Result<Self, crate::Error>> {
        if method != "terminal/wait_for_exit" {
            return None;
        }
        Some(json_cast(params))
    }

    fn parse_notification(
        _method: &str,
        _params: &impl Serialize,
    ) -> Option<Result<Self, crate::Error>> {
        // This is a request, not a notification
        None
    }
}

impl JrRequest for WaitForTerminalExitRequest {
    type Response = WaitForTerminalExitResponse;
}

impl JrResponsePayload for WaitForTerminalExitResponse {
    fn into_json(self, _method: &str) -> Result<serde_json::Value, crate::Error> {
        serde_json::to_value(self).map_err(crate::Error::into_internal_error)
    }

    fn from_value(_method: &str, value: serde_json::Value) -> Result<Self, crate::Error> {
        json_cast(&value)
    }
}

// ============================================================================
// KillTerminalCommandRequest
// ============================================================================

impl JrMessage for KillTerminalCommandRequest {
    fn into_untyped_message(self) -> Result<crate::UntypedMessage, crate::Error> {
        let method = self.method().to_string();
        crate::UntypedMessage::new(&method, self)
    }

    fn method(&self) -> &str {
        "terminal/kill"
    }

    fn parse_request(method: &str, params: &impl Serialize) -> Option<Result<Self, crate::Error>> {
        if method != "terminal/kill" {
            return None;
        }
        Some(json_cast(params))
    }

    fn parse_notification(
        _method: &str,
        _params: &impl Serialize,
    ) -> Option<Result<Self, crate::Error>> {
        // This is a request, not a notification
        None
    }
}

impl JrRequest for KillTerminalCommandRequest {
    type Response = KillTerminalCommandResponse;
}

impl JrResponsePayload for KillTerminalCommandResponse {
    fn into_json(self, _method: &str) -> Result<serde_json::Value, crate::Error> {
        serde_json::to_value(self).map_err(crate::Error::into_internal_error)
    }

    fn from_value(_method: &str, value: serde_json::Value) -> Result<Self, crate::Error> {
        json_cast(&value)
    }
}
