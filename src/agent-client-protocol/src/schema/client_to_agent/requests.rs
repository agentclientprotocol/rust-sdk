#[cfg(feature = "unstable_session_archive")]
use crate::v1::schema::{
    ArchiveSessionRequest, ArchiveSessionResponse, UnarchiveSessionRequest,
    UnarchiveSessionResponse,
};
#[cfg(feature = "unstable_auth_status")]
use crate::v1::schema::{AuthStatusRequest, AuthStatusResponse};
use crate::v1::schema::{
    AuthenticateRequest, AuthenticateResponse, CloseSessionRequest, CloseSessionResponse,
    DeleteSessionRequest, DeleteSessionResponse, InitializeRequest, InitializeResponse,
    ListSessionsRequest, ListSessionsResponse, LoadSessionRequest, LoadSessionResponse,
    LogoutRequest, LogoutResponse, NewSessionRequest, NewSessionResponse, PromptRequest,
    PromptResponse, ResumeSessionRequest, ResumeSessionResponse, SetSessionConfigOptionRequest,
    SetSessionConfigOptionResponse, SetSessionModeRequest, SetSessionModeResponse,
};
#[cfg(feature = "unstable_llm_providers")]
use crate::v1::schema::{
    DisableProviderRequest, DisableProviderResponse, ListProvidersRequest, ListProvidersResponse,
    SetProviderRequest, SetProviderResponse,
};
#[cfg(feature = "unstable_session_fork")]
use crate::v1::schema::{ForkSessionRequest, ForkSessionResponse};
#[cfg(feature = "unstable_session_status")]
use crate::v1::schema::{SessionStatusRequest, SessionStatusResponse};
#[cfg(feature = "unstable_session_set_title")]
use crate::v1::schema::{SetSessionTitleRequest, SetSessionTitleResponse};

impl_jsonrpc_request!(InitializeRequest, InitializeResponse, "initialize");
impl_jsonrpc_request!(AuthenticateRequest, AuthenticateResponse, "authenticate");
#[cfg(feature = "unstable_llm_providers")]
impl_jsonrpc_request!(
    ListProvidersRequest,
    ListProvidersResponse,
    "providers/list"
);
#[cfg(feature = "unstable_llm_providers")]
impl_jsonrpc_request!(SetProviderRequest, SetProviderResponse, "providers/set");
#[cfg(feature = "unstable_llm_providers")]
impl_jsonrpc_request!(
    DisableProviderRequest,
    DisableProviderResponse,
    "providers/disable"
);
impl_jsonrpc_request!(LogoutRequest, LogoutResponse, "logout");
#[cfg(feature = "unstable_auth_status")]
impl_jsonrpc_request!(AuthStatusRequest, AuthStatusResponse, "auth/status");
impl_jsonrpc_request!(LoadSessionRequest, LoadSessionResponse, "session/load");
impl_jsonrpc_request!(ListSessionsRequest, ListSessionsResponse, "session/list");
impl_jsonrpc_request!(
    DeleteSessionRequest,
    DeleteSessionResponse,
    "session/delete"
);
impl_jsonrpc_request!(NewSessionRequest, NewSessionResponse, "session/new");
impl_jsonrpc_request!(PromptRequest, PromptResponse, "session/prompt");
impl_jsonrpc_request!(
    SetSessionModeRequest,
    SetSessionModeResponse,
    "session/set_mode"
);
impl_jsonrpc_request!(
    SetSessionConfigOptionRequest,
    SetSessionConfigOptionResponse,
    "session/set_config_option"
);

#[cfg(feature = "unstable_session_fork")]
impl_jsonrpc_request!(ForkSessionRequest, ForkSessionResponse, "session/fork");
#[cfg(feature = "unstable_session_archive")]
impl_jsonrpc_request!(
    ArchiveSessionRequest,
    ArchiveSessionResponse,
    "session/archive"
);
#[cfg(feature = "unstable_session_archive")]
impl_jsonrpc_request!(
    UnarchiveSessionRequest,
    UnarchiveSessionResponse,
    "session/unarchive"
);
#[cfg(feature = "unstable_session_status")]
impl_jsonrpc_request!(
    SessionStatusRequest,
    SessionStatusResponse,
    "session/status"
);
#[cfg(feature = "unstable_session_set_title")]
impl_jsonrpc_request!(
    SetSessionTitleRequest,
    SetSessionTitleResponse,
    "session/set_title"
);
impl_jsonrpc_request!(
    ResumeSessionRequest,
    ResumeSessionResponse,
    "session/resume"
);
impl_jsonrpc_request!(CloseSessionRequest, CloseSessionResponse, "session/close");
