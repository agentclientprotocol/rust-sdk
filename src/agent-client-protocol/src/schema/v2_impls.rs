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
                params: &impl serde::Serialize,
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

            fn from_value(_method: &str, value: serde_json::Value) -> Result<Self, crate::Error> {
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
                params: &impl serde::Serialize,
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
                match self {
                    $( $(#[$meta])* Self::$variant(_) => $method, )*
                    Self::$ext_variant(ext) => &ext.method,
                    _ => "_unknown",
                }
            }

            fn to_untyped_message(&self) -> Result<UntypedMessage, crate::Error> {
                UntypedMessage::new(self.method(), self)
            }

            fn parse_message(
                method: &str,
                params: &impl serde::Serialize,
            ) -> Result<Self, crate::Error> {
                match method {
                    $( $(#[$meta])* $method => crate::util::json_cast_params(params).map(Self::$variant), )*
                    _ => {
                        if method.starts_with('_') {
                            crate::util::json_cast_params(params).map(
                                |ext_req: v2::ExtRequest| {
                                    Self::$ext_variant(v2::ExtRequest::new(
                                        method.to_string(),
                                        ext_req.params,
                                    ))
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
                match self {
                    $( $(#[$meta])* Self::$variant(_) => $method, )*
                    Self::$ext_variant(ext) => &ext.method,
                    _ => "_unknown",
                }
            }

            fn to_untyped_message(&self) -> Result<UntypedMessage, crate::Error> {
                UntypedMessage::new(self.method(), self)
            }

            fn parse_message(
                method: &str,
                params: &impl serde::Serialize,
            ) -> Result<Self, crate::Error> {
                match method {
                    $( $(#[$meta])* $method => crate::util::json_cast_params(params).map(Self::$variant), )*
                    _ => {
                        if method.starts_with('_') {
                            crate::util::json_cast_params(params).map(
                                |ext_notif: v2::ExtNotification| {
                                    Self::$ext_variant(v2::ExtNotification::new(
                                        method.to_string(),
                                        ext_notif.params,
                                    ))
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

            fn from_value(
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
impl_v2_jsonrpc_request!(
    v2::AuthenticateRequest,
    v2::AuthenticateResponse,
    "authenticate"
);
impl_v2_jsonrpc_request!(v2::LogoutRequest, v2::LogoutResponse, "logout");
impl_v2_jsonrpc_request!(v2::NewSessionRequest, v2::NewSessionResponse, "session/new");
impl_v2_jsonrpc_request!(
    v2::LoadSessionRequest,
    v2::LoadSessionResponse,
    "session/load"
);
impl_v2_jsonrpc_request!(
    v2::ListSessionsRequest,
    v2::ListSessionsResponse,
    "session/list"
);
#[cfg(feature = "unstable_session_delete")]
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
    v2::SetSessionModeRequest,
    v2::SetSessionModeResponse,
    "session/set_mode"
);
impl_v2_jsonrpc_request!(
    v2::SetSessionConfigOptionRequest,
    v2::SetSessionConfigOptionResponse,
    "session/set_config_option"
);
impl_v2_jsonrpc_request!(v2::PromptRequest, v2::PromptResponse, "session/prompt");
#[cfg(feature = "unstable_session_model")]
impl_v2_jsonrpc_request!(
    v2::SetSessionModelRequest,
    v2::SetSessionModelResponse,
    "session/set_model"
);
#[cfg(feature = "unstable_mcp_over_acp")]
impl_v2_jsonrpc_request!(v2::MessageMcpRequest, v2::MessageMcpResponse, "mcp/message");

impl_v2_jsonrpc_notification!(v2::CancelNotification, "session/cancel");
#[cfg(feature = "unstable_mcp_over_acp")]
impl_v2_jsonrpc_notification!(v2::MessageMcpNotification, "mcp/message");

impl_v2_jsonrpc_request!(
    v2::WriteTextFileRequest,
    v2::WriteTextFileResponse,
    "fs/write_text_file"
);
impl_v2_jsonrpc_request!(
    v2::ReadTextFileRequest,
    v2::ReadTextFileResponse,
    "fs/read_text_file"
);
impl_v2_jsonrpc_request!(
    v2::RequestPermissionRequest,
    v2::RequestPermissionResponse,
    "session/request_permission"
);
impl_v2_jsonrpc_request!(
    v2::CreateTerminalRequest,
    v2::CreateTerminalResponse,
    "terminal/create"
);
impl_v2_jsonrpc_request!(
    v2::TerminalOutputRequest,
    v2::TerminalOutputResponse,
    "terminal/output"
);
impl_v2_jsonrpc_request!(
    v2::ReleaseTerminalRequest,
    v2::ReleaseTerminalResponse,
    "terminal/release"
);
impl_v2_jsonrpc_request!(
    v2::WaitForTerminalExitRequest,
    v2::WaitForTerminalExitResponse,
    "terminal/wait_for_exit"
);
impl_v2_jsonrpc_request!(
    v2::KillTerminalRequest,
    v2::KillTerminalResponse,
    "terminal/kill"
);
#[cfg(feature = "unstable_mcp_over_acp")]
impl_v2_jsonrpc_request!(v2::ConnectMcpRequest, v2::ConnectMcpResponse, "mcp/connect");
#[cfg(feature = "unstable_mcp_over_acp")]
impl_v2_jsonrpc_request!(
    v2::DisconnectMcpRequest,
    v2::DisconnectMcpResponse,
    "mcp/disconnect"
);

impl_v2_jsonrpc_notification!(v2::SessionNotification, "session/update");

impl_v2_jsonrpc_request_enum!(v2::ClientRequest {
    InitializeRequest => "initialize",
    AuthenticateRequest => "authenticate",
    LogoutRequest => "logout",
    NewSessionRequest => "session/new",
    LoadSessionRequest => "session/load",
    ListSessionsRequest => "session/list",
    #[cfg(feature = "unstable_session_delete")]
    DeleteSessionRequest => "session/delete",
    #[cfg(feature = "unstable_session_fork")]
    ForkSessionRequest => "session/fork",
    ResumeSessionRequest => "session/resume",
    CloseSessionRequest => "session/close",
    SetSessionModeRequest => "session/set_mode",
    SetSessionConfigOptionRequest => "session/set_config_option",
    PromptRequest => "session/prompt",
    #[cfg(feature = "unstable_session_model")]
    SetSessionModelRequest => "session/set_model",
    #[cfg(feature = "unstable_mcp_over_acp")]
    MessageMcpRequest => "mcp/message",
    [ext] ExtMethodRequest,
});

impl_v2_jsonrpc_response_enum!(v2::AgentResponse {
    InitializeResponse => "initialize",
    AuthenticateResponse => "authenticate",
    LogoutResponse => "logout",
    NewSessionResponse => "session/new",
    LoadSessionResponse => "session/load",
    ListSessionsResponse => "session/list",
    #[cfg(feature = "unstable_session_delete")]
    DeleteSessionResponse => "session/delete",
    #[cfg(feature = "unstable_session_fork")]
    ForkSessionResponse => "session/fork",
    ResumeSessionResponse => "session/resume",
    CloseSessionResponse => "session/close",
    SetSessionModeResponse => "session/set_mode",
    SetSessionConfigOptionResponse => "session/set_config_option",
    PromptResponse => "session/prompt",
    #[cfg(feature = "unstable_session_model")]
    SetSessionModelResponse => "session/set_model",
    #[cfg(feature = "unstable_mcp_over_acp")]
    MessageMcpResponse => "mcp/message",
    [ext] ExtMethodResponse,
});

impl_v2_jsonrpc_notification_enum!(v2::ClientNotification {
    CancelNotification => "session/cancel",
    #[cfg(feature = "unstable_mcp_over_acp")]
    MessageMcpNotification => "mcp/message",
    [ext] ExtNotification,
});

impl_v2_jsonrpc_request_enum!(v2::AgentRequest {
    WriteTextFileRequest => "fs/write_text_file",
    ReadTextFileRequest => "fs/read_text_file",
    RequestPermissionRequest => "session/request_permission",
    CreateTerminalRequest => "terminal/create",
    TerminalOutputRequest => "terminal/output",
    ReleaseTerminalRequest => "terminal/release",
    WaitForTerminalExitRequest => "terminal/wait_for_exit",
    KillTerminalRequest => "terminal/kill",
    #[cfg(feature = "unstable_mcp_over_acp")]
    ConnectMcpRequest => "mcp/connect",
    #[cfg(feature = "unstable_mcp_over_acp")]
    MessageMcpRequest => "mcp/message",
    #[cfg(feature = "unstable_mcp_over_acp")]
    DisconnectMcpRequest => "mcp/disconnect",
    [ext] ExtMethodRequest,
});

impl_v2_jsonrpc_response_enum!(v2::ClientResponse {
    WriteTextFileResponse => "fs/write_text_file",
    ReadTextFileResponse => "fs/read_text_file",
    RequestPermissionResponse => "session/request_permission",
    CreateTerminalResponse => "terminal/create",
    TerminalOutputResponse => "terminal/output",
    ReleaseTerminalResponse => "terminal/release",
    WaitForTerminalExitResponse => "terminal/wait_for_exit",
    KillTerminalResponse => "terminal/kill",
    #[cfg(feature = "unstable_mcp_over_acp")]
    ConnectMcpResponse => "mcp/connect",
    #[cfg(feature = "unstable_mcp_over_acp")]
    MessageMcpResponse => "mcp/message",
    #[cfg(feature = "unstable_mcp_over_acp")]
    DisconnectMcpResponse => "mcp/disconnect",
    [ext] ExtMethodResponse,
});

impl_v2_jsonrpc_notification_enum!(v2::AgentNotification {
    SessionNotification => "session/update",
    #[cfg(feature = "unstable_mcp_over_acp")]
    MessageMcpNotification => "mcp/message",
    [ext] ExtNotification,
});
