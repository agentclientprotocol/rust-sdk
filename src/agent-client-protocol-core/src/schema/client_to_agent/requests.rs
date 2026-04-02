use crate::schema::{
    AuthenticateRequest, AuthenticateResponse, InitializeRequest, InitializeResponse,
    ListSessionsRequest, ListSessionsResponse, LoadSessionRequest, LoadSessionResponse,
    NewSessionRequest, NewSessionResponse, PromptRequest, PromptResponse,
    SetSessionConfigOptionRequest, SetSessionConfigOptionResponse, SetSessionModeRequest,
    SetSessionModeResponse,
};
#[cfg(feature = "unstable_session_close")]
use crate::schema::{CloseSessionRequest, CloseSessionResponse};
#[cfg(feature = "unstable_session_fork")]
use crate::schema::{ForkSessionRequest, ForkSessionResponse};
#[cfg(feature = "unstable_logout")]
use crate::schema::{LogoutRequest, LogoutResponse};
#[cfg(feature = "unstable_session_resume")]
use crate::schema::{ResumeSessionRequest, ResumeSessionResponse};
#[cfg(feature = "unstable_session_model")]
use crate::schema::{SetSessionModelRequest, SetSessionModelResponse};

impl_jsonrpc_request!(InitializeRequest, InitializeResponse, "initialize");
impl_jsonrpc_request!(AuthenticateRequest, AuthenticateResponse, "authenticate");
#[cfg(feature = "unstable_logout")]
impl_jsonrpc_request!(LogoutRequest, LogoutResponse, "logout");
impl_jsonrpc_request!(LoadSessionRequest, LoadSessionResponse, "session/load");
impl_jsonrpc_request!(ListSessionsRequest, ListSessionsResponse, "session/list");
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

#[cfg(feature = "unstable_session_model")]
impl_jsonrpc_request!(
    SetSessionModelRequest,
    SetSessionModelResponse,
    "session/set_model"
);
#[cfg(feature = "unstable_session_fork")]
impl_jsonrpc_request!(ForkSessionRequest, ForkSessionResponse, "session/fork");
#[cfg(feature = "unstable_session_resume")]
impl_jsonrpc_request!(
    ResumeSessionRequest,
    ResumeSessionResponse,
    "session/resume"
);
#[cfg(feature = "unstable_session_close")]
impl_jsonrpc_request!(CloseSessionRequest, CloseSessionResponse, "session/close");
