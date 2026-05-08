//! Canonical ACP method tables used by JSON-RPC schema integrations.
//!
//! Keep protocol method additions here first. The single-message impls, enum
//! dispatch impls, and experimental v2 compatibility impls all expand from
//! these tables so schema changes have one obvious SDK update point.

macro_rules! client_request_methods {
    ($callback:ident) => {
        $callback!(
            [
                (
                    InitializeRequest,
                    $crate::schema::InitializeRequest,
                    $crate::schema::InitializeResponse,
                    $crate::schema::v2::InitializeRequest,
                    $crate::schema::v2::InitializeResponse,
                    "initialize"
                ),
                (
                    AuthenticateRequest,
                    $crate::schema::AuthenticateRequest,
                    $crate::schema::AuthenticateResponse,
                    $crate::schema::v2::AuthenticateRequest,
                    $crate::schema::v2::AuthenticateResponse,
                    "authenticate"
                ),
                #[cfg(feature = "unstable_logout")]
                (
                    LogoutRequest,
                    $crate::schema::LogoutRequest,
                    $crate::schema::LogoutResponse,
                    $crate::schema::v2::LogoutRequest,
                    $crate::schema::v2::LogoutResponse,
                    "logout"
                ),
                (
                    NewSessionRequest,
                    $crate::schema::NewSessionRequest,
                    $crate::schema::NewSessionResponse,
                    $crate::schema::v2::NewSessionRequest,
                    $crate::schema::v2::NewSessionResponse,
                    "session/new"
                ),
                (
                    LoadSessionRequest,
                    $crate::schema::LoadSessionRequest,
                    $crate::schema::LoadSessionResponse,
                    $crate::schema::v2::LoadSessionRequest,
                    $crate::schema::v2::LoadSessionResponse,
                    "session/load"
                ),
                (
                    ListSessionsRequest,
                    $crate::schema::ListSessionsRequest,
                    $crate::schema::ListSessionsResponse,
                    $crate::schema::v2::ListSessionsRequest,
                    $crate::schema::v2::ListSessionsResponse,
                    "session/list"
                ),
                #[cfg(feature = "unstable_session_fork")]
                (
                    ForkSessionRequest,
                    $crate::schema::ForkSessionRequest,
                    $crate::schema::ForkSessionResponse,
                    $crate::schema::v2::ForkSessionRequest,
                    $crate::schema::v2::ForkSessionResponse,
                    "session/fork"
                ),
                (
                    ResumeSessionRequest,
                    $crate::schema::ResumeSessionRequest,
                    $crate::schema::ResumeSessionResponse,
                    $crate::schema::v2::ResumeSessionRequest,
                    $crate::schema::v2::ResumeSessionResponse,
                    "session/resume"
                ),
                (
                    CloseSessionRequest,
                    $crate::schema::CloseSessionRequest,
                    $crate::schema::CloseSessionResponse,
                    $crate::schema::v2::CloseSessionRequest,
                    $crate::schema::v2::CloseSessionResponse,
                    "session/close"
                ),
                (
                    SetSessionModeRequest,
                    $crate::schema::SetSessionModeRequest,
                    $crate::schema::SetSessionModeResponse,
                    $crate::schema::v2::SetSessionModeRequest,
                    $crate::schema::v2::SetSessionModeResponse,
                    "session/set_mode"
                ),
                (
                    SetSessionConfigOptionRequest,
                    $crate::schema::SetSessionConfigOptionRequest,
                    $crate::schema::SetSessionConfigOptionResponse,
                    $crate::schema::v2::SetSessionConfigOptionRequest,
                    $crate::schema::v2::SetSessionConfigOptionResponse,
                    "session/set_config_option"
                ),
                (
                    PromptRequest,
                    $crate::schema::PromptRequest,
                    $crate::schema::PromptResponse,
                    $crate::schema::v2::PromptRequest,
                    $crate::schema::v2::PromptResponse,
                    "session/prompt"
                ),
                #[cfg(feature = "unstable_session_model")]
                (
                    SetSessionModelRequest,
                    $crate::schema::SetSessionModelRequest,
                    $crate::schema::SetSessionModelResponse,
                    $crate::schema::v2::SetSessionModelRequest,
                    $crate::schema::v2::SetSessionModelResponse,
                    "session/set_model"
                ),
            ],
            ExtMethodRequest
        );
    };
}

macro_rules! client_notification_methods {
    ($callback:ident) => {
        $callback!(
            [(
                CancelNotification,
                $crate::schema::CancelNotification,
                $crate::schema::v2::CancelNotification,
                "session/cancel"
            ),],
            ExtNotification
        );
    };
}

macro_rules! agent_request_methods {
    ($callback:ident) => {
        $callback!(
            [
                (
                    WriteTextFileRequest,
                    $crate::schema::WriteTextFileRequest,
                    $crate::schema::WriteTextFileResponse,
                    $crate::schema::v2::WriteTextFileRequest,
                    $crate::schema::v2::WriteTextFileResponse,
                    "fs/write_text_file"
                ),
                (
                    ReadTextFileRequest,
                    $crate::schema::ReadTextFileRequest,
                    $crate::schema::ReadTextFileResponse,
                    $crate::schema::v2::ReadTextFileRequest,
                    $crate::schema::v2::ReadTextFileResponse,
                    "fs/read_text_file"
                ),
                (
                    RequestPermissionRequest,
                    $crate::schema::RequestPermissionRequest,
                    $crate::schema::RequestPermissionResponse,
                    $crate::schema::v2::RequestPermissionRequest,
                    $crate::schema::v2::RequestPermissionResponse,
                    "session/request_permission"
                ),
                (
                    CreateTerminalRequest,
                    $crate::schema::CreateTerminalRequest,
                    $crate::schema::CreateTerminalResponse,
                    $crate::schema::v2::CreateTerminalRequest,
                    $crate::schema::v2::CreateTerminalResponse,
                    "terminal/create"
                ),
                (
                    TerminalOutputRequest,
                    $crate::schema::TerminalOutputRequest,
                    $crate::schema::TerminalOutputResponse,
                    $crate::schema::v2::TerminalOutputRequest,
                    $crate::schema::v2::TerminalOutputResponse,
                    "terminal/output"
                ),
                (
                    ReleaseTerminalRequest,
                    $crate::schema::ReleaseTerminalRequest,
                    $crate::schema::ReleaseTerminalResponse,
                    $crate::schema::v2::ReleaseTerminalRequest,
                    $crate::schema::v2::ReleaseTerminalResponse,
                    "terminal/release"
                ),
                (
                    WaitForTerminalExitRequest,
                    $crate::schema::WaitForTerminalExitRequest,
                    $crate::schema::WaitForTerminalExitResponse,
                    $crate::schema::v2::WaitForTerminalExitRequest,
                    $crate::schema::v2::WaitForTerminalExitResponse,
                    "terminal/wait_for_exit"
                ),
                (
                    KillTerminalRequest,
                    $crate::schema::KillTerminalRequest,
                    $crate::schema::KillTerminalResponse,
                    $crate::schema::v2::KillTerminalRequest,
                    $crate::schema::v2::KillTerminalResponse,
                    "terminal/kill"
                ),
            ],
            ExtMethodRequest
        );
    };
}

macro_rules! agent_notification_methods {
    ($callback:ident) => {
        $callback!(
            [(
                SessionNotification,
                $crate::schema::SessionNotification,
                $crate::schema::v2::SessionNotification,
                "session/update"
            ),],
            ExtNotification
        );
    };
}

macro_rules! impl_v1_request_singletons {
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
            impl_jsonrpc_request!($v1_req, $v1_resp, $method);
        )*
    };
}

macro_rules! impl_v1_notification_singletons {
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
            impl_jsonrpc_notification!($v1_notif, $method);
        )*
    };
}

macro_rules! impl_client_request_enum_from_table {
    (
        [
            $( $(#[$meta:meta])* (
                $variant:ident,
                $v1_req:ty,
                $v1_resp:ty,
                $v2_req:ty,
                $v2_resp:ty,
                $method:literal
            ), )*
        ],
        $ext_variant:ident
    ) => {
        impl_jsonrpc_request_enum!($crate::schema::ClientRequest {
            $( $(#[$meta])* $variant => $method, )*
            [ext] $ext_variant,
        });
    };
}

macro_rules! impl_agent_request_enum_from_table {
    (
        [
            $( $(#[$meta:meta])* (
                $variant:ident,
                $v1_req:ty,
                $v1_resp:ty,
                $v2_req:ty,
                $v2_resp:ty,
                $method:literal
            ), )*
        ],
        $ext_variant:ident
    ) => {
        impl_jsonrpc_request_enum!($crate::schema::AgentRequest {
            $( $(#[$meta])* $variant => $method, )*
            [ext] $ext_variant,
        });
    };
}

macro_rules! impl_client_notification_enum_from_table {
    (
        [
            $( $(#[$meta:meta])* (
                $variant:ident,
                $v1_notif:ty,
                $v2_notif:ty,
                $method:literal
            ), )*
        ],
        $ext_variant:ident
    ) => {
        impl_jsonrpc_notification_enum!($crate::schema::ClientNotification {
            $( $(#[$meta])* $variant => $method, )*
            [ext] $ext_variant,
        });
    };
}

macro_rules! impl_agent_notification_enum_from_table {
    (
        [
            $( $(#[$meta:meta])* (
                $variant:ident,
                $v1_notif:ty,
                $v2_notif:ty,
                $method:literal
            ), )*
        ],
        $ext_variant:ident
    ) => {
        impl_jsonrpc_notification_enum!($crate::schema::AgentNotification {
            $( $(#[$meta])* $variant => $method, )*
            [ext] $ext_variant,
        });
    };
}

macro_rules! impl_client_request_enum {
    () => {
        client_request_methods!(impl_client_request_enum_from_table);
    };
}

macro_rules! impl_client_notification_enum {
    () => {
        client_notification_methods!(impl_client_notification_enum_from_table);
    };
}

macro_rules! impl_agent_request_enum {
    () => {
        agent_request_methods!(impl_agent_request_enum_from_table);
    };
}

macro_rules! impl_agent_notification_enum {
    () => {
        agent_notification_methods!(impl_agent_notification_enum_from_table);
    };
}

#[cfg(feature = "unstable_protocol_v2")]
macro_rules! impl_v2_client_request_enum_from_table {
    (
        [
            $( $(#[$meta:meta])* (
                $variant:ident,
                $v1_req:ty,
                $v1_resp:ty,
                $v2_req:ty,
                $v2_resp:ty,
                $method:literal
            ), )*
        ],
        $ext_variant:ident
    ) => {
        impl_v2_jsonrpc_request_enum!(
            $crate::schema::v2::ClientRequest,
            $crate::schema::ClientRequest {
                $( $(#[$meta])* $variant => $method, )*
                [ext] $ext_variant,
            }
        );
    };
}

#[cfg(feature = "unstable_protocol_v2")]
macro_rules! impl_v2_agent_request_enum_from_table {
    (
        [
            $( $(#[$meta:meta])* (
                $variant:ident,
                $v1_req:ty,
                $v1_resp:ty,
                $v2_req:ty,
                $v2_resp:ty,
                $method:literal
            ), )*
        ],
        $ext_variant:ident
    ) => {
        impl_v2_jsonrpc_request_enum!(
            $crate::schema::v2::AgentRequest,
            $crate::schema::AgentRequest {
                $( $(#[$meta])* $variant => $method, )*
                [ext] $ext_variant,
            }
        );
    };
}

#[cfg(feature = "unstable_protocol_v2")]
macro_rules! impl_v2_client_notification_enum_from_table {
    (
        [
            $( $(#[$meta:meta])* (
                $variant:ident,
                $v1_notif:ty,
                $v2_notif:ty,
                $method:literal
            ), )*
        ],
        $ext_variant:ident
    ) => {
        impl_v2_jsonrpc_notification_enum!(
            $crate::schema::v2::ClientNotification,
            $crate::schema::ClientNotification {
                $( $(#[$meta])* $variant => $method, )*
                [ext] $ext_variant,
            }
        );
    };
}

#[cfg(feature = "unstable_protocol_v2")]
macro_rules! impl_v2_agent_notification_enum_from_table {
    (
        [
            $( $(#[$meta:meta])* (
                $variant:ident,
                $v1_notif:ty,
                $v2_notif:ty,
                $method:literal
            ), )*
        ],
        $ext_variant:ident
    ) => {
        impl_v2_jsonrpc_notification_enum!(
            $crate::schema::v2::AgentNotification,
            $crate::schema::AgentNotification {
                $( $(#[$meta])* $variant => $method, )*
                [ext] $ext_variant,
            }
        );
    };
}

#[cfg(feature = "unstable_protocol_v2")]
macro_rules! impl_v2_client_request_enum {
    () => {
        client_request_methods!(impl_v2_client_request_enum_from_table);
    };
}

#[cfg(feature = "unstable_protocol_v2")]
macro_rules! impl_v2_client_notification_enum {
    () => {
        client_notification_methods!(impl_v2_client_notification_enum_from_table);
    };
}

#[cfg(feature = "unstable_protocol_v2")]
macro_rules! impl_v2_agent_request_enum {
    () => {
        agent_request_methods!(impl_v2_agent_request_enum_from_table);
    };
}

#[cfg(feature = "unstable_protocol_v2")]
macro_rules! impl_v2_agent_notification_enum {
    () => {
        agent_notification_methods!(impl_v2_agent_notification_enum_from_table);
    };
}
