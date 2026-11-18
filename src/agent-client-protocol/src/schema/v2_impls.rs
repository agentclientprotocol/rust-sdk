//! JSON-RPC trait implementations for the experimental schema v2 namespace.

use crate::v2::schema as v2;
use crate::{JsonRpcMessage, JsonRpcNotification, JsonRpcRequest, JsonRpcResponse, UntypedMessage};

macro_rules! impl_v2_jsonrpc_request {
    ($req:ty, $resp:ty, $method:literal) => {
        impl JsonRpcMessage for $req {
            fn matches_method(method: &str) -> bool {
                method == $method
            }

            fn method(&self) -> &str {
                $method
            }

            fn to_untyped_message(&self) -> Result<UntypedMessage, crate::Error> {
                UntypedMessage::new($method, self)
            }

            fn parse_message(
                method: &str,
                params: &serde_json::Value,
            ) -> Result<Self, crate::Error> {
                if method != $method {
                    return Err(crate::Error::method_not_found());
                }
                crate::util::json_cast_params(params)
            }
        }

        impl JsonRpcRequest for $req {
            type Response = $resp;
        }

        impl JsonRpcResponse for $resp {
            fn into_json(self, _method: &str) -> Result<serde_json::Value, crate::Error> {
                serde_json::to_value(self).map_err(crate::Error::into_internal_error)
            }

            fn from_json(_method: &str, value: serde_json::Value) -> Result<Self, crate::Error> {
                crate::util::json_cast(value)
            }
        }
    };
}

macro_rules! impl_v2_jsonrpc_notification {
    ($notif:ty, $method:literal) => {
        impl JsonRpcMessage for $notif {
            fn matches_method(method: &str) -> bool {
                method == $method
            }

            fn method(&self) -> &str {
                $method
            }

            fn to_untyped_message(&self) -> Result<UntypedMessage, crate::Error> {
                UntypedMessage::new($method, self)
            }

            fn parse_message(
                method: &str,
                params: &serde_json::Value,
            ) -> Result<Self, crate::Error> {
                if method != $method {
                    return Err(crate::Error::method_not_found());
                }
                crate::util::json_cast_params(params)
            }
        }

        impl JsonRpcNotification for $notif {}
    };
}

impl_v2_jsonrpc_request!(v2::InitializeRequest, v2::InitializeResponse, "initialize");
impl_v2_jsonrpc_request!(v2::LoginAuthRequest, v2::LoginAuthResponse, "auth/login");
impl_v2_jsonrpc_request!(v2::LogoutAuthRequest, v2::LogoutAuthResponse, "auth/logout");
#[cfg(feature = "unstable_auth_status")]
impl_v2_jsonrpc_request!(v2::AuthStatusRequest, v2::AuthStatusResponse, "auth/status");
#[cfg(feature = "unstable_llm_providers")]
impl_v2_jsonrpc_request!(
    v2::ListProvidersRequest,
    v2::ListProvidersResponse,
    "providers/list"
);
#[cfg(feature = "unstable_llm_providers")]
impl_v2_jsonrpc_request!(
    v2::SetProviderRequest,
    v2::SetProviderResponse,
    "providers/set"
);
#[cfg(feature = "unstable_llm_providers")]
impl_v2_jsonrpc_request!(
    v2::DisableProviderRequest,
    v2::DisableProviderResponse,
    "providers/disable"
);
impl_v2_jsonrpc_request!(v2::NewSessionRequest, v2::NewSessionResponse, "session/new");
impl_v2_jsonrpc_request!(
    v2::ListSessionsRequest,
    v2::ListSessionsResponse,
    "session/list"
);
impl_v2_jsonrpc_request!(
    v2::DeleteSessionRequest,
    v2::DeleteSessionResponse,
    "session/delete"
);
#[cfg(feature = "unstable_session_fork")]
impl_v2_jsonrpc_request!(
    v2::ForkSessionRequest,
    v2::ForkSessionResponse,
    "session/fork"
);
#[cfg(feature = "unstable_session_archive")]
impl_v2_jsonrpc_request!(
    v2::ArchiveSessionRequest,
    v2::ArchiveSessionResponse,
    "session/archive"
);
#[cfg(feature = "unstable_session_archive")]
impl_v2_jsonrpc_request!(
    v2::UnarchiveSessionRequest,
    v2::UnarchiveSessionResponse,
    "session/unarchive"
);
#[cfg(feature = "unstable_session_status")]
impl_v2_jsonrpc_request!(
    v2::SessionStatusRequest,
    v2::SessionStatusResponse,
    "session/status"
);
#[cfg(feature = "unstable_session_options")]
impl_v2_jsonrpc_request!(
    v2::SessionOptionsRequest,
    v2::SessionOptionsResponse,
    "session/options"
);
#[cfg(feature = "unstable_session_set_title")]
impl_v2_jsonrpc_request!(
    v2::SetSessionTitleRequest,
    v2::SetSessionTitleResponse,
    "session/set_title"
);
impl_v2_jsonrpc_request!(
    v2::ResumeSessionRequest,
    v2::ResumeSessionResponse,
    "session/resume"
);
impl_v2_jsonrpc_request!(
    v2::CloseSessionRequest,
    v2::CloseSessionResponse,
    "session/close"
);
impl_v2_jsonrpc_request!(
    v2::SetSessionConfigOptionRequest,
    v2::SetSessionConfigOptionResponse,
    "session/set_config_option"
);
impl_v2_jsonrpc_request!(
    v2::CancelToolCallRequest,
    v2::CancelToolCallResponse,
    "tool_call/cancel"
);
impl_v2_jsonrpc_request!(
    v2::CancelTerminalRequest,
    v2::CancelTerminalResponse,
    "terminal/cancel"
);
impl_v2_jsonrpc_request!(v2::PromptRequest, v2::PromptResponse, "session/prompt");
#[cfg(feature = "unstable_mcp_over_acp")]
impl_v2_jsonrpc_request!(v2::MessageMcpRequest, v2::MessageMcpResponse, "mcp/message");

impl_v2_jsonrpc_notification!(v2::CancelRequestNotification, "$/cancel_request");
impl_v2_jsonrpc_notification!(v2::CancelSessionNotification, "session/cancel");
#[cfg(feature = "unstable_mcp_over_acp")]
impl_v2_jsonrpc_notification!(v2::MessageMcpNotification, "mcp/message");

impl_v2_jsonrpc_request!(
    v2::RequestPermissionRequest,
    v2::RequestPermissionResponse,
    "session/request_permission"
);
impl_v2_jsonrpc_request!(
    v2::CreateElicitationRequest,
    v2::CreateElicitationResponse,
    "elicitation/create"
);

impl_v2_jsonrpc_notification!(v2::UpdateSessionNotification, "session/update");
impl_v2_jsonrpc_notification!(v2::CompleteElicitationNotification, "elicitation/complete");
#[cfg(feature = "unstable_auth_status")]
impl_v2_jsonrpc_notification!(v2::AuthStatusUpdateNotification, "auth/status_update");

impl_jsonrpc_protocol_level_notification_enum!(v2::ProtocolLevelNotification);
impl_jsonrpc_request_enum!(v2::ClientRequest);
impl_jsonrpc_response_enum!(v2::AgentResponse);
impl_jsonrpc_notification_enum!(v2::ClientNotification);
impl_jsonrpc_request_enum!(v2::AgentRequest);
impl_jsonrpc_response_enum!(v2::ClientResponse);
impl_jsonrpc_notification_enum!(v2::AgentNotification);
