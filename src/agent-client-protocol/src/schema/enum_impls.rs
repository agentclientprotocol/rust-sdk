//! JsonRpcMessage and JsonRpcNotification/JsonRpcRequest implementations for
//! the ACP enum types from agent-client-protocol-schema.

use crate::schema::v1::{
    AgentNotification, AgentRequest, AgentResponse, ClientNotification, ClientRequest,
    ClientResponse,
};

// ============================================================================
// Agent side (messages that agents receive)
// ============================================================================

impl_jsonrpc_request_enum!(ClientRequest {
    InitializeRequest => "initialize",
    AuthenticateRequest => "authenticate",
    #[cfg(feature = "unstable_llm_providers")]
    ListProvidersRequest => "providers/list",
    #[cfg(feature = "unstable_llm_providers")]
    SetProviderRequest => "providers/set",
    #[cfg(feature = "unstable_llm_providers")]
    DisableProviderRequest => "providers/disable",
    LogoutRequest => "logout",
    #[cfg(feature = "unstable_auth_status")]
    AuthStatusRequest => "auth/status",
    NewSessionRequest => "session/new",
    LoadSessionRequest => "session/load",
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
    SetSessionModeRequest => "session/set_mode",
    SetSessionConfigOptionRequest => "session/set_config_option",
    PromptRequest => "session/prompt",
    [ext] ExtMethodRequest,
});

impl_jsonrpc_response_enum!(AgentResponse {
    InitializeResponse => "initialize",
    AuthenticateResponse => "authenticate",
    #[cfg(feature = "unstable_llm_providers")]
    ListProvidersResponse => "providers/list",
    #[cfg(feature = "unstable_llm_providers")]
    SetProviderResponse => "providers/set",
    #[cfg(feature = "unstable_llm_providers")]
    DisableProviderResponse => "providers/disable",
    LogoutResponse => "logout",
    #[cfg(feature = "unstable_auth_status")]
    AuthStatusResponse => "auth/status",
    NewSessionResponse => "session/new",
    LoadSessionResponse => "session/load",
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
    SetSessionModeResponse => "session/set_mode",
    SetSessionConfigOptionResponse => "session/set_config_option",
    PromptResponse => "session/prompt",
    [ext] ExtMethodResponse,
});

impl_jsonrpc_notification_enum!(ClientNotification {
    CancelNotification => "session/cancel",
    #[cfg(feature = "unstable_mcp_over_acp")]
    MessageMcpNotification => "mcp/message",
    [ext] ExtNotification,
});

// ============================================================================
// Client side (messages that clients/editors receive)
// ============================================================================

impl_jsonrpc_request_enum!(AgentRequest {
    WriteTextFileRequest => "fs/write_text_file",
    ReadTextFileRequest => "fs/read_text_file",
    RequestPermissionRequest => "session/request_permission",
    CreateTerminalRequest => "terminal/create",
    TerminalOutputRequest => "terminal/output",
    ReleaseTerminalRequest => "terminal/release",
    WaitForTerminalExitRequest => "terminal/wait_for_exit",
    KillTerminalRequest => "terminal/kill",
    CreateElicitationRequest => "elicitation/create",
    #[cfg(feature = "unstable_mcp_over_acp")]
    MessageMcpRequest => "mcp/message",
    [ext] ExtMethodRequest,
});

impl_jsonrpc_response_enum!(ClientResponse {
    WriteTextFileResponse => "fs/write_text_file",
    ReadTextFileResponse => "fs/read_text_file",
    RequestPermissionResponse => "session/request_permission",
    CreateTerminalResponse => "terminal/create",
    TerminalOutputResponse => "terminal/output",
    ReleaseTerminalResponse => "terminal/release",
    WaitForTerminalExitResponse => "terminal/wait_for_exit",
    KillTerminalResponse => "terminal/kill",
    CreateElicitationResponse => "elicitation/create",
    #[cfg(feature = "unstable_mcp_over_acp")]
    MessageMcpResponse => "mcp/message",
    [ext] ExtMethodResponse,
});

impl_jsonrpc_notification_enum!(AgentNotification {
    SessionNotification => "session/update",
    CompleteElicitationNotification => "elicitation/complete",
    #[cfg(feature = "unstable_auth_status")]
    AuthStatusUpdateNotification => "auth/status_update",
    [ext] ExtNotification,
});
