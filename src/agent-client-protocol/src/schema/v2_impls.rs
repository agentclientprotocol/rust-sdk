//! JSON-RPC trait implementations for the experimental schema v2 namespace.

use crate::schema::v2;
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

macro_rules! impl_v2_jsonrpc_request_enum {
    ($enum:ty {
        $( $(#[$meta:meta])* $variant:ident => $method:literal, )*
        [ext] $ext_variant:ident,
    }) => {
        impl JsonRpcMessage for $enum {
            fn matches_method(_method: &str) -> bool {
                true
            }

            fn method(&self) -> &str {
                // The schema enum's inherent `method()` (path syntax prefers
                // inherent items) covers every variant, including ones this
                // macro does not list.
                <$enum>::method(self)
            }

            fn to_untyped_message(&self) -> Result<UntypedMessage, crate::Error> {
                UntypedMessage::new(self.method(), self)
            }

            fn parse_message(
                method: &str,
                params: &serde_json::Value,
            ) -> Result<Self, crate::Error> {
                match method {
                    $( $(#[$meta])* $method => crate::util::json_cast_params(params).map(Self::$variant), )*
                    _ => {
                        if method.starts_with('_') {
                            crate::util::json_cast_params(params).map(
                                |ext_req: v2::ExtRequest| {
                                    Self::$ext_variant(Box::new(v2::ExtRequest::new(
                                        method.to_string(),
                                        ext_req.params,
                                    )))
                                },
                            )
                        } else {
                            Err(crate::Error::method_not_found())
                        }
                    }
                }
            }
        }

        impl JsonRpcRequest for $enum {
            type Response = serde_json::Value;
        }
    };
}

macro_rules! impl_v2_jsonrpc_notification_enum {
    ($enum:ty {
        $( $(#[$meta:meta])* $variant:ident => $method:literal, )*
        [ext] $ext_variant:ident,
    }) => {
        impl JsonRpcMessage for $enum {
            fn matches_method(_method: &str) -> bool {
                true
            }

            fn method(&self) -> &str {
                // The schema enum's inherent `method()` (path syntax prefers
                // inherent items) covers every variant, including ones this
                // macro does not list.
                <$enum>::method(self)
            }

            fn to_untyped_message(&self) -> Result<UntypedMessage, crate::Error> {
                UntypedMessage::new(self.method(), self)
            }

            fn parse_message(
                method: &str,
                params: &serde_json::Value,
            ) -> Result<Self, crate::Error> {
                match method {
                    $( $(#[$meta])* $method => crate::util::json_cast_params(params).map(Self::$variant), )*
                    _ => {
                        if method.starts_with('_') {
                            crate::util::json_cast_params(params).map(
                                |ext_notif: v2::ExtNotification| {
                                    Self::$ext_variant(Box::new(v2::ExtNotification::new(
                                        method.to_string(),
                                        ext_notif.params,
                                    )))
                                },
                            )
                        } else {
                            Err(crate::Error::method_not_found())
                        }
                    }
                }
            }
        }

        impl JsonRpcNotification for $enum {}
    };
}

macro_rules! impl_v2_jsonrpc_response_enum {
    ($enum:ty {
        $( $(#[$meta:meta])* $variant:ident => $method:literal, )*
        [ext] $ext_variant:ident,
    }) => {
        impl JsonRpcResponse for $enum {
            fn into_json(
                self,
                _method: &str,
            ) -> Result<serde_json::Value, crate::Error> {
                serde_json::to_value(self).map_err(crate::Error::into_internal_error)
            }

            fn from_json(
                method: &str,
                value: serde_json::Value,
            ) -> Result<Self, crate::Error> {
                match method {
                    $( $(#[$meta])* $method => crate::util::json_cast(value).map(Self::$variant), )*
                    _ => {
                        if method.starts_with('_') {
                            crate::util::json_cast(value).map(Self::$ext_variant)
                        } else {
                            Err(crate::Error::method_not_found())
                        }
                    }
                }
            }
        }
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

impl_jsonrpc_protocol_level_notification_enum!(v2::ProtocolLevelNotification {
    CancelRequestNotification => "$/cancel_request",
});

impl_v2_jsonrpc_request_enum!(v2::ClientRequest {
    InitializeRequest => "initialize",
    LoginAuthRequest => "auth/login",
    LogoutAuthRequest => "auth/logout",
    #[cfg(feature = "unstable_auth_status")]
    AuthStatusRequest => "auth/status",
    #[cfg(feature = "unstable_llm_providers")]
    ListProvidersRequest => "providers/list",
    #[cfg(feature = "unstable_llm_providers")]
    SetProviderRequest => "providers/set",
    #[cfg(feature = "unstable_llm_providers")]
    DisableProviderRequest => "providers/disable",
    NewSessionRequest => "session/new",
    ListSessionsRequest => "session/list",
    DeleteSessionRequest => "session/delete",
    #[cfg(feature = "unstable_session_fork")]
    ForkSessionRequest => "session/fork",
    #[cfg(feature = "unstable_session_archive")]
    ArchiveSessionRequest => "session/archive",
    #[cfg(feature = "unstable_session_archive")]
    UnarchiveSessionRequest => "session/unarchive",
    #[cfg(feature = "unstable_session_status")]
    SessionStatusRequest => "session/status",
    #[cfg(feature = "unstable_session_set_title")]
    SetSessionTitleRequest => "session/set_title",
    ResumeSessionRequest => "session/resume",
    CloseSessionRequest => "session/close",
    CancelToolCallRequest => "tool_call/cancel",
    CancelTerminalRequest => "terminal/cancel",
    SetSessionConfigOptionRequest => "session/set_config_option",
    PromptRequest => "session/prompt",
    [ext] ExtMethodRequest,
});

impl_v2_jsonrpc_response_enum!(v2::AgentResponse {
    InitializeResponse => "initialize",
    LoginAuthResponse => "auth/login",
    LogoutAuthResponse => "auth/logout",
    #[cfg(feature = "unstable_auth_status")]
    AuthStatusResponse => "auth/status",
    #[cfg(feature = "unstable_llm_providers")]
    ListProvidersResponse => "providers/list",
    #[cfg(feature = "unstable_llm_providers")]
    SetProviderResponse => "providers/set",
    #[cfg(feature = "unstable_llm_providers")]
    DisableProviderResponse => "providers/disable",
    NewSessionResponse => "session/new",
    ListSessionsResponse => "session/list",
    DeleteSessionResponse => "session/delete",
    #[cfg(feature = "unstable_session_fork")]
    ForkSessionResponse => "session/fork",
    #[cfg(feature = "unstable_session_archive")]
    ArchiveSessionResponse => "session/archive",
    #[cfg(feature = "unstable_session_archive")]
    UnarchiveSessionResponse => "session/unarchive",
    #[cfg(feature = "unstable_session_status")]
    SessionStatusResponse => "session/status",
    #[cfg(feature = "unstable_session_set_title")]
    SetSessionTitleResponse => "session/set_title",
    ResumeSessionResponse => "session/resume",
    CloseSessionResponse => "session/close",
    CancelToolCallResponse => "tool_call/cancel",
    CancelTerminalResponse => "terminal/cancel",
    SetSessionConfigOptionResponse => "session/set_config_option",
    PromptResponse => "session/prompt",
    [ext] ExtMethodResponse,
});

impl_v2_jsonrpc_notification_enum!(v2::ClientNotification {
    CancelSessionNotification => "session/cancel",
    #[cfg(feature = "unstable_mcp_over_acp")]
    MessageMcpNotification => "mcp/message",
    [ext] ExtNotification,
});

impl_v2_jsonrpc_request_enum!(v2::AgentRequest {
    RequestPermissionRequest => "session/request_permission",
    CreateElicitationRequest => "elicitation/create",
    #[cfg(feature = "unstable_mcp_over_acp")]
    MessageMcpRequest => "mcp/message",
    [ext] ExtMethodRequest,
});

impl_v2_jsonrpc_response_enum!(v2::ClientResponse {
    RequestPermissionResponse => "session/request_permission",
    CreateElicitationResponse => "elicitation/create",
    #[cfg(feature = "unstable_mcp_over_acp")]
    MessageMcpResponse => "mcp/message",
    [ext] ExtMethodResponse,
});

impl_v2_jsonrpc_notification_enum!(v2::AgentNotification {
    UpdateSessionNotification => "session/update",
    CompleteElicitationNotification => "elicitation/complete",
    #[cfg(feature = "unstable_auth_status")]
    AuthStatusUpdateNotification => "auth/status_update",
    [ext] ExtNotification,
});
