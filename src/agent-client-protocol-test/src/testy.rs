//! testy: a deterministic ACP test agent with typed JSON prompt commands.
//!
//! The agent accepts JSON-serialized [`TestyCommand`] values as prompt text. It
//! can also recognize short plain-text scenario names such as `full`,
//! `callbacks`, and `session_updates` for manual testing in clients.

use agent_client_protocol::schema::v1::{
    AgentAuthCapabilities, AgentCapabilities, AudioContent, AuthMethod, AuthMethodAgent,
    AuthenticateRequest, AuthenticateResponse, AvailableCommand, AvailableCommandInput,
    AvailableCommandsUpdate, CancelNotification, CloseSessionRequest, CloseSessionResponse,
    ConfigOptionUpdate, ContentBlock, ContentChunk, Cost, CreateTerminalRequest, CurrentModeUpdate,
    DeleteSessionRequest, DeleteSessionResponse, Diff, EmbeddedResource, EmbeddedResourceResource,
    HttpHeader, ImageContent, InitializeRequest, InitializeResponse, ListSessionsRequest,
    ListSessionsResponse, LoadSessionRequest, LoadSessionResponse, LogoutCapabilities,
    LogoutRequest, LogoutResponse, McpCapabilities, McpServer, MessageId, NewSessionRequest,
    NewSessionResponse, PermissionOption, PermissionOptionKind, Plan, PlanEntry, PlanEntryPriority,
    PlanEntryStatus, PromptCapabilities, PromptRequest, PromptResponse, ReadTextFileRequest,
    RequestPermissionRequest, ResourceLink, ResumeSessionRequest, ResumeSessionResponse,
    SessionAdditionalDirectoriesCapabilities, SessionCapabilities, SessionCloseCapabilities,
    SessionConfigId, SessionConfigOption, SessionConfigSelectOption, SessionConfigValueId,
    SessionDeleteCapabilities, SessionId, SessionInfo, SessionInfoUpdate, SessionListCapabilities,
    SessionMode, SessionModeId, SessionModeState, SessionNotification, SessionResumeCapabilities,
    SessionUpdate, SetSessionConfigOptionRequest, SetSessionConfigOptionResponse,
    SetSessionModeRequest, SetSessionModeResponse, StopReason, Terminal, TerminalId,
    TerminalOutputRequest, TextContent, TextResourceContents, ToolCall, ToolCallContent,
    ToolCallLocation, ToolCallStatus, ToolCallUpdate, ToolCallUpdateFields, ToolKind,
    UnstructuredCommandInput, UsageUpdate, WriteTextFileRequest,
};
#[cfg(feature = "unstable")]
use agent_client_protocol::schema::v1::{
    CompleteElicitationNotification, CreateElicitationRequest, ElicitationAction,
    ElicitationCapabilities, ElicitationFormMode, ElicitationRequestScope, ElicitationSchema,
    ElicitationSessionScope, ElicitationUrlMode, ErrorCode, MultiSelectPropertySchema, RequestId,
    StringPropertySchema,
};
use agent_client_protocol::{
    Agent, Client, ConnectTo, ConnectionTo, JsonRpcRequest, Responder, SentRequest,
};
use anyhow::Result;
use serde::{Deserialize, Serialize};
#[cfg(feature = "unstable")]
use std::collections::BTreeMap;
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use tokio::sync::Notify;

const TESTY_AGENT_AUTH_METHOD_ID: &str = "testy-agent-auth";

/// Commands that can be sent as prompt text (serialized as JSON) to the [`Testy`].
///
/// Tests construct these as typed values and serialize to JSON via [`TestyCommand::to_prompt`].
/// The agent deserializes the prompt text and dispatches accordingly.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "command", rename_all = "snake_case")]
pub enum TestyCommand {
    /// Returns a short list of supported commands and scenarios.
    Help,

    /// Responds with `"Hello, world!"`.
    Greet,

    /// Echoes the given message back as the response.
    Echo { message: String },

    /// Runs a deterministic scenario that exercises part of the ACP surface.
    RunScenario { scenario: TestyScenario },

    /// Invokes an MCP tool and returns the result.
    /// The agent must have been given MCP servers in the `NewSessionRequest`.
    CallTool {
        server: String,
        tool: String,
        #[serde(default)]
        params: serde_json::Value,
    },

    /// Lists tools from the named MCP server.
    ListTools { server: String },
}

impl TestyCommand {
    /// Serialize this command to a JSON string suitable for use as prompt text.
    #[must_use]
    pub fn to_prompt(&self) -> String {
        serde_json::to_string(self).expect("TestyCommand serialization should not fail")
    }
}

/// Prompt-driven scenarios supported by [`TestyCommand::RunScenario`].
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum TestyScenario {
    /// Emits every stable `session/update` variant.
    SessionUpdates,
    /// Exercises content blocks and prompt-content parsing.
    Content,
    /// Emits tool-call create/update notifications with content, diff, and locations.
    ToolCalls,
    /// Sends every stable agent-to-client request and any enabled unstable callback coverage.
    Callbacks,
    /// Runs unstable elicitation coverage without the stable callback requests.
    #[cfg(feature = "unstable")]
    Elicitations,
    /// Reports whether the session has received a `session/cancel` notification.
    CancelStatus,
    /// Runs all stable scenarios and any enabled unstable coverage in a deterministic order.
    Full,
}

impl TestyScenario {
    fn all() -> Vec<Self> {
        vec![
            Self::SessionUpdates,
            Self::Content,
            Self::ToolCalls,
            Self::Callbacks,
            #[cfg(feature = "unstable")]
            Self::Elicitations,
            Self::CancelStatus,
            Self::Full,
        ]
    }

    fn from_prompt(input: &str) -> Option<Self> {
        match input.trim().to_ascii_lowercase().as_str() {
            "session_updates" | "session updates" | "updates" => Some(Self::SessionUpdates),
            "content" => Some(Self::Content),
            "tool_calls" | "tool calls" | "tools" => Some(Self::ToolCalls),
            "callbacks" | "client_callbacks" | "client callbacks" => Some(Self::Callbacks),
            #[cfg(feature = "unstable")]
            "elicitations" | "elicitation" | "elicit" => Some(Self::Elicitations),
            "cancel_status" | "cancel status" | "cancel" => Some(Self::CancelStatus),
            "full" | "all" => Some(Self::Full),
            _ => None,
        }
    }

    const fn name(self) -> &'static str {
        match self {
            Self::SessionUpdates => "session_updates",
            Self::Content => "content",
            Self::ToolCalls => "tool_calls",
            Self::Callbacks => "callbacks",
            #[cfg(feature = "unstable")]
            Self::Elicitations => "elicitations",
            Self::CancelStatus => "cancel_status",
            Self::Full => "full",
        }
    }
}

/// Session data for each active session.
#[derive(Clone, Debug)]
struct SessionData {
    cwd: PathBuf,
    additional_directories: Vec<PathBuf>,
    mcp_servers: Vec<McpServer>,
    current_mode_id: SessionModeId,
    current_config_value: SessionConfigValueId,
    title: String,
    updated_at: String,
    cancelled: bool,
    pending_cancel_status: bool,
    closed: bool,
    active_prompts: u64,
}

#[derive(Debug)]
struct TestyState {
    sessions: HashMap<SessionId, SessionData>,
    next_session_id: u64,
    next_message_id: u64,
    next_tool_call_id: u64,
    authenticated_methods: HashSet<String>,
    #[cfg(feature = "unstable")]
    client_elicitation_capabilities: Option<ElicitationCapabilities>,
}

impl Default for TestyState {
    fn default() -> Self {
        Self {
            sessions: HashMap::new(),
            next_session_id: 1,
            next_message_id: 1,
            next_tool_call_id: 1,
            authenticated_methods: HashSet::new(),
            #[cfg(feature = "unstable")]
            client_elicitation_capabilities: None,
        }
    }
}

/// A deterministic ACP test agent.
///
/// Implements `ConnectTo<Client>` and handles every stable client-to-agent ACP
/// request and notification. Prompt text is parsed as a JSON [`TestyCommand`];
/// if parsing fails, a few short plain-text commands are recognized and all
/// other prompts behave like [`TestyCommand::Greet`].
#[derive(Clone, Debug)]
pub struct Testy {
    state: Arc<Mutex<TestyState>>,
    cancel_notify: Arc<Notify>,
}

impl Testy {
    #[must_use]
    pub fn new() -> Self {
        Self {
            state: Arc::new(Mutex::new(TestyState::default())),
            cancel_notify: Arc::new(Notify::new()),
        }
    }

    fn lock_state(&self) -> std::sync::MutexGuard<'_, TestyState> {
        self.state.lock().expect("testy state lock poisoned")
    }

    fn create_session(
        &self,
        cwd: PathBuf,
        additional_directories: Vec<PathBuf>,
        mcp_servers: Vec<McpServer>,
    ) -> (SessionId, SessionData) {
        let mut state = self.lock_state();
        let session = SessionData::new(cwd, additional_directories, mcp_servers);
        loop {
            let session_id = SessionId::new(format!("testy-session-{}", state.next_session_id));
            state.next_session_id += 1;
            if !state.sessions.contains_key(&session_id) {
                state.sessions.insert(session_id.clone(), session.clone());
                return (session_id, session);
            }
        }
    }

    fn next_message_id(&self, prefix: &str) -> MessageId {
        let mut state = self.lock_state();
        let message_id = MessageId::new(format!("{prefix}-{}", state.next_message_id));
        state.next_message_id += 1;
        message_id
    }

    fn next_tool_call_id(&self, prefix: &str) -> String {
        let mut state = self.lock_state();
        let tool_call_id = format!("{prefix}-{}", state.next_tool_call_id);
        state.next_tool_call_id += 1;
        tool_call_id
    }

    fn upsert_session(
        &self,
        session_id: SessionId,
        cwd: PathBuf,
        additional_directories: Vec<PathBuf>,
        mcp_servers: Vec<McpServer>,
    ) -> SessionData {
        let mut state = self.lock_state();
        let session = state
            .sessions
            .entry(session_id)
            .or_insert_with(|| SessionData::new(cwd.clone(), vec![], vec![]));
        session.cwd = cwd;
        session.additional_directories = additional_directories;
        session.mcp_servers = mcp_servers;
        session.clone()
    }

    fn get_session(
        &self,
        session_id: &SessionId,
    ) -> Result<SessionData, agent_client_protocol::Error> {
        self.lock_state()
            .sessions
            .get(session_id)
            .cloned()
            .ok_or_else(|| invalid_params(format!("unknown session `{session_id}`")))
    }

    fn begin_prompt(&self, session_id: &SessionId) -> Result<(), agent_client_protocol::Error> {
        let mut state = self.lock_state();
        let session = state
            .sessions
            .get_mut(session_id)
            .ok_or_else(|| invalid_params(format!("unknown session `{session_id}`")))?;
        if session.closed {
            return Err(invalid_params(format!("closed session `{session_id}`")));
        }
        session.active_prompts += 1;
        Ok(())
    }

    fn finish_prompt(&self, session_id: &SessionId, stop_reason: StopReason) -> StopReason {
        let mut state = self.lock_state();
        let Some(session) = state.sessions.get_mut(session_id) else {
            return StopReason::Cancelled;
        };
        let cancelled = session.cancelled || session.closed;
        session.active_prompts = session.active_prompts.saturating_sub(1);
        if cancelled && !session.closed && session.active_prompts == 0 {
            session.cancelled = false;
        }
        let remove_session = session.closed && session.active_prompts == 0;
        if remove_session {
            state.sessions.remove(session_id);
        }
        if cancelled {
            StopReason::Cancelled
        } else {
            stop_reason
        }
    }

    fn get_mcp_servers(&self, session_id: &SessionId) -> Option<Vec<McpServer>> {
        self.lock_state()
            .sessions
            .get(session_id)
            .map(|session| session.mcp_servers.clone())
    }

    fn mark_cancelled(&self, session_id: &SessionId) {
        let did_cancel = if let Some(session) = self.lock_state().sessions.get_mut(session_id) {
            if session.active_prompts == 0 {
                session.pending_cancel_status = true;
            } else {
                session.cancelled = true;
            }
            true
        } else {
            false
        };
        if did_cancel {
            self.cancel_notify.notify_waiters();
        }
    }

    fn is_cancelled(&self, session_id: &SessionId) -> bool {
        self.lock_state()
            .sessions
            .get(session_id)
            .is_some_and(|session| session.cancelled || session.closed)
    }

    fn take_cancelled(&self, session_id: &SessionId) -> bool {
        let mut state = self.lock_state();
        let Some(session) = state.sessions.get_mut(session_id) else {
            return false;
        };
        let cancelled = session.cancelled || session.pending_cancel_status;
        session.pending_cancel_status = false;
        if session.active_prompts <= 1 {
            session.cancelled = false;
        }
        cancelled
    }

    async fn wait_for_cancelled(&self, session_id: &SessionId) {
        let notified = self.cancel_notify.notified();
        tokio::pin!(notified);
        loop {
            notified.as_mut().enable();
            if self.is_cancelled(session_id) {
                return;
            }
            notified.as_mut().await;
            notified.set(self.cancel_notify.notified());
        }
    }

    fn agent_capabilities() -> AgentCapabilities {
        AgentCapabilities::new()
            .load_session(true)
            .prompt_capabilities(
                PromptCapabilities::new()
                    .image(true)
                    .audio(true)
                    .embedded_context(true),
            )
            .mcp_capabilities(McpCapabilities::new().http(true))
            .session_capabilities(
                SessionCapabilities::new()
                    .list(SessionListCapabilities::new())
                    .delete(SessionDeleteCapabilities::new())
                    .additional_directories(SessionAdditionalDirectoriesCapabilities::new())
                    .resume(SessionResumeCapabilities::new())
                    .close(SessionCloseCapabilities::new()),
            )
            .auth(AgentAuthCapabilities::new().logout(LogoutCapabilities::new()))
    }

    fn auth_methods() -> Vec<AuthMethod> {
        vec![AuthMethod::Agent(
            AuthMethodAgent::new(TESTY_AGENT_AUTH_METHOD_ID, "Testy agent auth")
                .description("Deterministic no-op authentication for ACP client testing"),
        )]
    }

    fn handle_initialize(
        &self,
        request: InitializeRequest,
        responder: Responder<InitializeResponse>,
    ) -> Result<(), agent_client_protocol::Error> {
        #[cfg(feature = "unstable")]
        {
            self.lock_state()
                .client_elicitation_capabilities
                .clone_from(&request.client_capabilities.elicitation);
        }

        responder.respond(
            InitializeResponse::new(request.protocol_version)
                .agent_capabilities(Testy::agent_capabilities())
                .auth_methods(Testy::auth_methods()),
        )
    }

    #[cfg(feature = "unstable")]
    fn client_supports_url_elicitation(&self) -> bool {
        self.lock_state()
            .client_elicitation_capabilities
            .as_ref()
            .is_some_and(|capabilities| capabilities.url.is_some())
    }

    fn handle_authenticate(
        &self,
        request: AuthenticateRequest,
        responder: Responder<AuthenticateResponse>,
    ) -> Result<(), agent_client_protocol::Error> {
        let method_id = request.method_id.to_string();
        if method_id != TESTY_AGENT_AUTH_METHOD_ID {
            return responder.respond_with_error(invalid_params(format!(
                "unsupported auth method `{method_id}`; supported methods: {TESTY_AGENT_AUTH_METHOD_ID}",
            )));
        }
        self.lock_state().authenticated_methods.insert(method_id);
        responder.respond(AuthenticateResponse::new())
    }

    fn handle_logout(
        &self,
        responder: Responder<LogoutResponse>,
    ) -> Result<(), agent_client_protocol::Error> {
        self.lock_state().authenticated_methods.clear();
        responder.respond(LogoutResponse::new())
    }

    fn handle_list_sessions(
        &self,
        request: ListSessionsRequest,
        responder: Responder<ListSessionsResponse>,
    ) -> Result<(), agent_client_protocol::Error> {
        let mut sessions = self
            .lock_state()
            .sessions
            .iter()
            .filter(|(_, session)| {
                !session.closed && request.cwd.as_ref().is_none_or(|cwd| cwd == &session.cwd)
            })
            .map(|(session_id, session)| {
                SessionInfo::new(session_id.clone(), session.cwd.clone())
                    .additional_directories(session.additional_directories.clone())
                    .title(session.title.clone())
                    .updated_at(session.updated_at.clone())
            })
            .collect::<Vec<_>>();
        sessions.sort_by(|left, right| left.session_id.0.cmp(&right.session_id.0));
        responder.respond(ListSessionsResponse::new(sessions))
    }

    fn handle_delete_session(
        &self,
        request: DeleteSessionRequest,
        responder: Responder<DeleteSessionResponse>,
    ) -> Result<(), agent_client_protocol::Error> {
        let (remove_session, did_cancel) = {
            let mut state = self.lock_state();
            let Some(session) = state.sessions.get_mut(&request.session_id) else {
                return responder.respond(DeleteSessionResponse::new());
            };
            session.cancelled = true;
            session.closed = true;
            (session.active_prompts == 0, true)
        };
        if did_cancel {
            self.cancel_notify.notify_waiters();
        }
        if remove_session {
            self.lock_state().sessions.remove(&request.session_id);
        }
        responder.respond(DeleteSessionResponse::new())
    }

    fn handle_close_session(
        &self,
        request: CloseSessionRequest,
        responder: Responder<CloseSessionResponse>,
    ) -> Result<(), agent_client_protocol::Error> {
        let (remove_session, did_cancel) = {
            let mut state = self.lock_state();
            let Some(session) = state.sessions.get_mut(&request.session_id) else {
                return responder.respond(CloseSessionResponse::new());
            };
            session.cancelled = true;
            session.closed = true;
            (session.active_prompts == 0, true)
        };
        if did_cancel {
            self.cancel_notify.notify_waiters();
        }
        if remove_session {
            self.lock_state().sessions.remove(&request.session_id);
        }
        responder.respond(CloseSessionResponse::new())
    }

    fn handle_set_mode(
        &self,
        request: SetSessionModeRequest,
        responder: Responder<SetSessionModeResponse>,
    ) -> Result<(), agent_client_protocol::Error> {
        if !is_supported_mode_id(&request.mode_id) {
            return responder.respond_with_error(invalid_params(format!(
                "unsupported mode `{}`; supported modes: chat, plan",
                request.mode_id
            )));
        }

        let mut state = self.lock_state();
        let Some(session) = state.sessions.get_mut(&request.session_id) else {
            return responder.respond_with_error(invalid_params(format!(
                "unknown session `{}`",
                request.session_id
            )));
        };
        if session.closed {
            return responder.respond_with_error(invalid_params(format!(
                "closed session `{}`",
                request.session_id
            )));
        }
        session.current_mode_id = request.mode_id;
        responder.respond(SetSessionModeResponse::new())
    }

    fn handle_set_config_option(
        &self,
        request: SetSessionConfigOptionRequest,
        responder: Responder<SetSessionConfigOptionResponse>,
    ) -> Result<(), agent_client_protocol::Error> {
        if !is_supported_config_id(&request.config_id) {
            return responder.respond_with_error(invalid_params(format!(
                "unsupported config option `{}`; supported config options: verbosity",
                request.config_id
            )));
        }

        let selected_value = match config_value_id_from_request(&request) {
            Ok(value) => value,
            Err(error) => return responder.respond_with_error(error),
        };
        if !is_supported_config_value_id(&selected_value) {
            return responder.respond_with_error(invalid_params(format!(
                "unsupported verbosity value `{selected_value}`; supported values: brief, normal, verbose",
            )));
        }

        let config_options = {
            let mut state = self.lock_state();
            let Some(session) = state.sessions.get_mut(&request.session_id) else {
                return responder.respond_with_error(invalid_params(format!(
                    "unknown session `{}`",
                    request.session_id
                )));
            };
            if session.closed {
                return responder.respond_with_error(invalid_params(format!(
                    "closed session `{}`",
                    request.session_id
                )));
            }
            session.current_config_value = selected_value;
            session.config_options()
        };
        responder.respond(SetSessionConfigOptionResponse::new(config_options))
    }

    async fn process_prompt(
        &self,
        request: PromptRequest,
        responder: Responder<PromptResponse>,
        connection: ConnectionTo<Client>,
    ) -> Result<(), agent_client_protocol::Error> {
        let session_id = request.session_id.clone();
        let input_text = extract_text_from_prompt(&request.prompt);

        let command = parse_command(&input_text);

        let prompt_result: Result<(String, StopReason), agent_client_protocol::Error> =
            match command {
                TestyCommand::Help => Ok((help_text(), StopReason::EndTurn)),

                TestyCommand::Greet => Ok(("Hello, world!".to_string(), StopReason::EndTurn)),

                TestyCommand::Echo { message } => Ok((message, StopReason::EndTurn)),

                TestyCommand::RunScenario { scenario } => {
                    self.run_scenario(&session_id, scenario, &request.prompt, &connection)
                        .await
                }

                TestyCommand::CallTool {
                    server,
                    tool,
                    params,
                } => Ok(
                    match self
                        .execute_tool_call(&session_id, &server, &tool, params)
                        .await
                    {
                        Ok(result) => (format!("OK: {result}"), StopReason::EndTurn),
                        Err(e) => (format!("ERROR: {e}"), StopReason::EndTurn),
                    },
                ),

                TestyCommand::ListTools { server } => {
                    Ok(match self.list_tools(&session_id, &server).await {
                        Ok(tools) => (format!("Available tools:\n{tools}"), StopReason::EndTurn),
                        Err(e) => (format!("ERROR: {e}"), StopReason::EndTurn),
                    })
                }
            };

        let (response_text, stop_reason) = match prompt_result {
            Ok(response) => response,
            Err(error) => {
                self.finish_prompt(&session_id, StopReason::EndTurn);
                #[cfg(feature = "unstable")]
                if is_url_elicitation_required_error(&error) {
                    return responder.respond_with_error(error);
                }
                return Err(error);
            }
        };
        let stop_reason = self.finish_prompt(&session_id, stop_reason);
        if !matches!(stop_reason, StopReason::Cancelled) {
            let message_id = self.next_message_id(stop_reason_message_id_prefix(stop_reason));
            send_session_update(
                &connection,
                &session_id,
                SessionUpdate::AgentMessageChunk(
                    ContentChunk::new(response_text.clone().into()).message_id(message_id),
                ),
            )?;
        }

        responder.respond(PromptResponse::new(stop_reason))
    }

    async fn run_scenario(
        &self,
        session_id: &SessionId,
        scenario: TestyScenario,
        prompt: &[ContentBlock],
        connection: &ConnectionTo<Client>,
    ) -> Result<(String, StopReason), agent_client_protocol::Error> {
        let mut report = Vec::new();
        self.run_scenario_inner(session_id, scenario, prompt, connection, &mut report)
            .await?;
        let stop_reason = if report.iter().any(|line| line == "cancel_status: cancelled") {
            StopReason::Cancelled
        } else {
            StopReason::EndTurn
        };
        Ok((report.join("\n"), stop_reason))
    }

    async fn run_scenario_inner(
        &self,
        session_id: &SessionId,
        scenario: TestyScenario,
        prompt: &[ContentBlock],
        connection: &ConnectionTo<Client>,
        report: &mut Vec<String>,
    ) -> Result<(), agent_client_protocol::Error> {
        report.push(format!("scenario: {}", scenario.name()));
        match scenario {
            TestyScenario::SessionUpdates => {
                self.emit_session_updates(session_id, connection)?;
                self.emit_content_updates(session_id, prompt, connection)?;
                self.emit_tool_call_updates(session_id, connection, None)?;
                report.push("session_updates: sent".to_string());
            }
            TestyScenario::Content => {
                self.emit_content_updates(session_id, prompt, connection)?;
                report.push(format!("content_blocks: {}", prompt.len()));
            }
            TestyScenario::ToolCalls => {
                self.emit_tool_call_updates(session_id, connection, None)?;
                report.push("tool_calls: sent".to_string());
            }
            TestyScenario::Callbacks => {
                self.exercise_client_callbacks(session_id, connection, report)
                    .await?;
                #[cfg(feature = "unstable")]
                if !self.is_cancelled(session_id) {
                    self.exercise_elicitations(session_id, connection, report)
                        .await?;
                }
            }
            #[cfg(feature = "unstable")]
            TestyScenario::Elicitations => {
                if self.is_cancelled(session_id) {
                    report.push("elicitations: cancelled".to_string());
                } else {
                    self.exercise_elicitations(session_id, connection, report)
                        .await?;
                }
            }
            TestyScenario::CancelStatus => {
                if self.take_cancelled(session_id) {
                    report.push("cancel_status: cancelled".to_string());
                } else {
                    report.push("cancel_status: not_cancelled".to_string());
                }
            }
            TestyScenario::Full => {
                self.emit_session_updates(session_id, connection)?;
                report.push("session_updates: sent".to_string());
                self.emit_content_updates(session_id, prompt, connection)?;
                report.push(format!("content_blocks: {}", prompt.len()));
                let terminal_id = self
                    .create_terminal_for_tool_content(session_id, connection, report)
                    .await?;
                if self.is_cancelled(session_id) {
                    if let Some(terminal_id) = terminal_id {
                        self.release_terminal_for_tool_content(
                            session_id,
                            connection,
                            terminal_id,
                            report,
                        )
                        .await?;
                    }
                    report.push("full: cancelled".to_string());
                    return Ok(());
                }
                let terminal_id_text = terminal_id.as_ref().map(ToString::to_string);
                self.emit_tool_call_updates(session_id, connection, terminal_id_text.as_deref())?;
                report.push("tool_calls: sent".to_string());
                if let Some(terminal_id) = terminal_id {
                    self.release_terminal_for_tool_content(
                        session_id,
                        connection,
                        terminal_id,
                        report,
                    )
                    .await?;
                }
                if self.is_cancelled(session_id) {
                    report.push("full: cancelled".to_string());
                    return Ok(());
                }
                self.exercise_client_callbacks(session_id, connection, report)
                    .await?;
                #[cfg(feature = "unstable")]
                if !self.is_cancelled(session_id) {
                    self.exercise_elicitations(session_id, connection, report)
                        .await?;
                }
                if self.take_cancelled(session_id) {
                    report.push("cancel_status: cancelled".to_string());
                } else {
                    report.push("cancel_status: not_cancelled".to_string());
                }
            }
        }
        Ok(())
    }

    fn emit_session_updates(
        &self,
        session_id: &SessionId,
        connection: &ConnectionTo<Client>,
    ) -> Result<(), agent_client_protocol::Error> {
        let session = self.get_session(session_id)?;

        send_session_update(
            connection,
            session_id,
            SessionUpdate::SessionInfoUpdate(
                SessionInfoUpdate::new()
                    .title("Testy deterministic session")
                    .updated_at("2026-01-01T00:00:00Z"),
            ),
        )?;
        send_session_update(
            connection,
            session_id,
            SessionUpdate::CurrentModeUpdate(CurrentModeUpdate::new(
                session.current_mode_id.clone(),
            )),
        )?;
        send_session_update(
            connection,
            session_id,
            SessionUpdate::ConfigOptionUpdate(ConfigOptionUpdate::new(session.config_options())),
        )?;
        send_session_update(
            connection,
            session_id,
            SessionUpdate::AvailableCommandsUpdate(AvailableCommandsUpdate::new(
                available_commands(),
            )),
        )?;
        send_session_update(
            connection,
            session_id,
            SessionUpdate::UsageUpdate(UsageUpdate::new(128, 4096).cost(Cost::new(0.0, "USD"))),
        )
    }

    fn emit_content_updates(
        &self,
        session_id: &SessionId,
        prompt: &[ContentBlock],
        connection: &ConnectionTo<Client>,
    ) -> Result<(), agent_client_protocol::Error> {
        let prompt_summary = format!("received {} prompt content block(s)", prompt.len());
        send_session_update(
            connection,
            session_id,
            SessionUpdate::UserMessageChunk(
                ContentChunk::new(prompt_summary.into())
                    .message_id(self.next_message_id("testy-user-message")),
            ),
        )?;
        send_session_update(
            connection,
            session_id,
            SessionUpdate::AgentThoughtChunk(
                ContentChunk::new("thinking deterministically".into())
                    .message_id(self.next_message_id("testy-thought-message")),
            ),
        )?;
        send_session_update(
            connection,
            session_id,
            SessionUpdate::AgentMessageChunk(
                ContentChunk::new("text content block".into())
                    .message_id(self.next_message_id("testy-content-text")),
            ),
        )?;
        send_session_update(
            connection,
            session_id,
            SessionUpdate::AgentMessageChunk(
                ContentChunk::new(ContentBlock::Image(
                    ImageContent::new(
                        "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mP8/x8AAwMCAO+/p9sAAAAASUVORK5CYII=",
                        "image/png",
                    )
                    .uri("file:///tmp/testy-pixel.png"),
                ))
                .message_id(self.next_message_id("testy-content-image")),
            ),
        )?;
        send_session_update(
            connection,
            session_id,
            SessionUpdate::AgentMessageChunk(
                ContentChunk::new(ContentBlock::Audio(AudioContent::new(
                    "UklGRiQAAABXQVZFZm10IBAAAAABAAEAESsAACJWAAACABAAZGF0YQAAAAA=",
                    "audio/wav",
                )))
                .message_id(self.next_message_id("testy-content-audio")),
            ),
        )?;
        send_session_update(
            connection,
            session_id,
            SessionUpdate::AgentMessageChunk(
                ContentChunk::new(ContentBlock::ResourceLink(
                    ResourceLink::new("Testy reference", "file:///tmp/testy-reference.txt")
                        .description("A deterministic resource link emitted by Testy")
                        .mime_type("text/plain"),
                ))
                .message_id(self.next_message_id("testy-resource-message")),
            ),
        )?;
        send_session_update(
            connection,
            session_id,
            SessionUpdate::AgentMessageChunk(
                ContentChunk::new(ContentBlock::Resource(EmbeddedResource::new(
                    EmbeddedResourceResource::TextResourceContents(
                        TextResourceContents::new(
                            "embedded resource text from testy",
                            "file:///tmp/testy-embedded.txt",
                        )
                        .mime_type("text/plain"),
                    ),
                )))
                .message_id(self.next_message_id("testy-content-resource")),
            ),
        )
    }

    fn emit_tool_call_updates(
        &self,
        session_id: &SessionId,
        connection: &ConnectionTo<Client>,
        terminal_id: Option<&str>,
    ) -> Result<(), agent_client_protocol::Error> {
        let read_tool_call_id = self.next_tool_call_id("testy-tool-read");
        let edit_tool_call_id = self.next_tool_call_id("testy-tool-edit");
        let initial = ToolCall::new(read_tool_call_id.clone(), "Read deterministic fixture")
            .kind(ToolKind::Read)
            .status(ToolCallStatus::Pending)
            .locations(vec![ToolCallLocation::new("/tmp/testy-input.txt").line(1)])
            .raw_input(serde_json::json!({ "path": "/tmp/testy-input.txt" }));
        send_session_update(connection, session_id, SessionUpdate::ToolCall(initial))?;

        let in_progress = ToolCallUpdate::new(
            read_tool_call_id.clone(),
            ToolCallUpdateFields::new()
                .status(ToolCallStatus::InProgress)
                .content(vec![ToolCallContent::from("reading /tmp/testy-input.txt")]),
        );
        send_session_update(
            connection,
            session_id,
            SessionUpdate::ToolCallUpdate(in_progress),
        )?;

        let read_completed = ToolCallUpdate::new(
            read_tool_call_id,
            ToolCallUpdateFields::new()
                .status(ToolCallStatus::Completed)
                .content(vec![ToolCallContent::from("read /tmp/testy-input.txt")])
                .raw_output(serde_json::json!({ "bytes": 12 })),
        );
        send_session_update(
            connection,
            session_id,
            SessionUpdate::ToolCallUpdate(read_completed),
        )?;

        let edit_initial = ToolCall::new(edit_tool_call_id.clone(), "Apply deterministic edit")
            .kind(ToolKind::Edit)
            .status(ToolCallStatus::Pending)
            .raw_input(serde_json::json!({ "path": "/tmp/testy-output.txt" }));
        send_session_update(
            connection,
            session_id,
            SessionUpdate::ToolCall(edit_initial),
        )?;

        let edit = ToolCallUpdate::new(
            edit_tool_call_id,
            ToolCallUpdateFields::new()
                .title("Apply deterministic edit")
                .kind(ToolKind::Edit)
                .status(ToolCallStatus::Completed)
                .content(vec![ToolCallContent::Diff(
                    Diff::new("/tmp/testy-output.txt", "after\n").old_text("before\n"),
                )])
                .raw_output(serde_json::json!({ "changed": true })),
        );
        send_session_update(connection, session_id, SessionUpdate::ToolCallUpdate(edit))?;

        if let Some(terminal_id) = terminal_id {
            let terminal_tool_call_id = self.next_tool_call_id("testy-tool-terminal");
            let terminal_initial = ToolCall::new(
                terminal_tool_call_id.clone(),
                "Embed deterministic terminal",
            )
            .kind(ToolKind::Execute)
            .status(ToolCallStatus::Pending)
            .raw_input(serde_json::json!({ "command": "printf testy" }));
            send_session_update(
                connection,
                session_id,
                SessionUpdate::ToolCall(terminal_initial),
            )?;

            let terminal = ToolCallUpdate::new(
                terminal_tool_call_id,
                ToolCallUpdateFields::new()
                    .title("Embed deterministic terminal")
                    .kind(ToolKind::Execute)
                    .status(ToolCallStatus::Completed)
                    .content(vec![ToolCallContent::Terminal(Terminal::new(
                        terminal_id.to_string(),
                    ))]),
            );
            send_session_update(
                connection,
                session_id,
                SessionUpdate::ToolCallUpdate(terminal),
            )?;
        }

        send_session_update(
            connection,
            session_id,
            SessionUpdate::Plan(Plan::new(vec![
                PlanEntry::new(
                    "Emit session updates",
                    PlanEntryPriority::High,
                    PlanEntryStatus::Completed,
                ),
                PlanEntry::new(
                    "Exercise callbacks",
                    PlanEntryPriority::Medium,
                    PlanEntryStatus::InProgress,
                ),
                PlanEntry::new(
                    "Summarize results",
                    PlanEntryPriority::Low,
                    PlanEntryStatus::Pending,
                ),
            ])),
        )
    }

    async fn create_terminal_for_tool_content(
        &self,
        session_id: &SessionId,
        connection: &ConnectionTo<Client>,
        report: &mut Vec<String>,
    ) -> Result<Option<TerminalId>, agent_client_protocol::Error> {
        let request = CreateTerminalRequest::new(session_id.clone(), "printf")
            .args(vec!["testy terminal content\n".to_string()])
            .cwd(PathBuf::from("/tmp"))
            .output_byte_limit(1024);
        match self
            .request_result_until_cancelled(session_id, connection, request)
            .await
        {
            Ok(response) => {
                let terminal_id = response.terminal_id.clone();
                report.push(format!("terminal/create_for_tool_call: ok {response:?}"));
                Ok(Some(terminal_id))
            }
            Err(error) => {
                report.push(format!("terminal/create_for_tool_call: error {error:?}"));
                Ok(None)
            }
        }
    }

    async fn release_terminal_for_tool_content(
        &self,
        session_id: &SessionId,
        connection: &ConnectionTo<Client>,
        terminal_id: TerminalId,
        report: &mut Vec<String>,
    ) -> Result<(), agent_client_protocol::Error> {
        let request = agent_client_protocol::schema::v1::ReleaseTerminalRequest::new(
            session_id.clone(),
            terminal_id,
        );
        if self.is_cancelled(session_id) {
            connection.send_request(request).detach();
            report.push("terminal/release_for_tool_call: sent".to_string());
        } else {
            report.push(
                self.request_report_until_cancelled(
                    session_id,
                    connection,
                    "terminal/release_for_tool_call",
                    request,
                )
                .await,
            );
        }
        Ok(())
    }

    async fn request_report_until_cancelled<Req>(
        &self,
        session_id: &SessionId,
        connection: &ConnectionTo<Client>,
        label: &str,
        request: Req,
    ) -> String
    where
        Req: JsonRpcRequest,
        Req::Response: std::fmt::Debug + Send,
    {
        match self
            .request_result_until_cancelled(session_id, connection, request)
            .await
        {
            Ok(response) => format!("{label}: ok {response:?}"),
            Err(error) => format!("{label}: error {error:?}"),
        }
    }

    async fn request_result_until_cancelled<Req>(
        &self,
        session_id: &SessionId,
        connection: &ConnectionTo<Client>,
        request: Req,
    ) -> Result<Req::Response, agent_client_protocol::Error>
    where
        Req: JsonRpcRequest,
        Req::Response: Send,
    {
        let request: SentRequest<Req::Response> = connection.send_request(request);
        tokio::select! {
            result = request.block_task() => result,
            () = self.wait_for_cancelled(session_id) => Err(agent_client_protocol::Error::new(-32800, "Request was cancelled")),
        }
    }

    #[cfg(feature = "unstable")]
    async fn exercise_elicitations(
        &self,
        session_id: &SessionId,
        connection: &ConnectionTo<Client>,
        report: &mut Vec<String>,
    ) -> Result<(), agent_client_protocol::Error> {
        let tool_call_id = self.next_tool_call_id("testy-elicit-tool");
        let form_accept = CreateElicitationRequest::new(
            ElicitationFormMode::new(
                ElicitationSessionScope::new(session_id.clone())
                    .tool_call_id(tool_call_id.as_str()),
                testy_elicitation_schema(),
            ),
            "Accept the Testy session-scoped form elicitation",
        );
        report.push(
            self.request_elicitation_report_until_cancelled(
                session_id,
                connection,
                "elicitation/form_session_accept",
                form_accept,
            )
            .await,
        );
        if elicitation_cancelled(self, session_id, report) {
            return Ok(());
        }

        let form_decline = CreateElicitationRequest::new(
            ElicitationFormMode::new(
                ElicitationSessionScope::new(session_id.clone()),
                ElicitationSchema::new().string("reason", false),
            ),
            "Decline the Testy session-scoped form elicitation",
        );
        report.push(
            self.request_elicitation_report_until_cancelled(
                session_id,
                connection,
                "elicitation/form_session_decline",
                form_decline,
            )
            .await,
        );
        if elicitation_cancelled(self, session_id, report) {
            return Ok(());
        }

        let form_cancel = CreateElicitationRequest::new(
            ElicitationFormMode::new(
                ElicitationRequestScope::new(RequestId::Str(
                    "testy-request-form-cancel".to_string(),
                )),
                ElicitationSchema::new().boolean("confirmed", true),
            ),
            "Cancel the Testy request-scoped form elicitation",
        );
        report.push(
            self.request_elicitation_report_until_cancelled(
                session_id,
                connection,
                "elicitation/form_request_cancel",
                form_cancel,
            )
            .await,
        );
        if elicitation_cancelled(self, session_id, report) {
            return Ok(());
        }

        if !self.client_supports_url_elicitation() {
            return Err(url_elicitation_required_error());
        }

        let session_url_id = "testy-url-session";
        let url_accept = CreateElicitationRequest::new(
            ElicitationUrlMode::new(
                ElicitationSessionScope::new(session_id.clone()),
                session_url_id,
                "https://example.com/testy/session",
            ),
            "Accept the Testy session-scoped URL elicitation",
        );
        report.push(
            self.request_elicitation_report_until_cancelled(
                session_id,
                connection,
                "elicitation/url_session_accept",
                url_accept,
            )
            .await,
        );
        if elicitation_cancelled(self, session_id, report) {
            return Ok(());
        }
        connection.send_notification(CompleteElicitationNotification::new(session_url_id))?;
        report.push("elicitation/complete_session_url: sent".to_string());

        let url_decline = CreateElicitationRequest::new(
            ElicitationUrlMode::new(
                ElicitationRequestScope::new(RequestId::Str(
                    "testy-request-url-decline".to_string(),
                )),
                "testy-url-request",
                "https://example.com/testy/request",
            ),
            "Decline the Testy request-scoped URL elicitation",
        );
        report.push(
            self.request_elicitation_report_until_cancelled(
                session_id,
                connection,
                "elicitation/url_request_decline",
                url_decline,
            )
            .await,
        );
        if elicitation_cancelled(self, session_id, report) {
            return Ok(());
        }

        report.push("elicitations: completed".to_string());
        Ok(())
    }

    #[cfg(feature = "unstable")]
    async fn request_elicitation_report_until_cancelled(
        &self,
        session_id: &SessionId,
        connection: &ConnectionTo<Client>,
        label: &str,
        request: CreateElicitationRequest,
    ) -> String {
        match self
            .request_result_until_cancelled(session_id, connection, request)
            .await
        {
            Ok(response) => format!(
                "{label}: ok {}",
                elicitation_action_summary(&response.action)
            ),
            Err(error) => format!("{label}: error {error:?}"),
        }
    }

    async fn exercise_client_callbacks(
        &self,
        session_id: &SessionId,
        connection: &ConnectionTo<Client>,
        report: &mut Vec<String>,
    ) -> Result<(), agent_client_protocol::Error> {
        let callback_cancelled = |agent: &Self, report: &mut Vec<String>| {
            if agent.is_cancelled(session_id) {
                report.push("callbacks: cancelled".to_string());
                true
            } else {
                false
            }
        };

        let permission = RequestPermissionRequest::new(
            session_id.clone(),
            ToolCallUpdate::new(
                self.next_tool_call_id("testy-permission"),
                ToolCallUpdateFields::new()
                    .title("Permission request")
                    .kind(ToolKind::Execute)
                    .status(ToolCallStatus::Pending),
            ),
            vec![
                PermissionOption::new("allow_once", "Allow once", PermissionOptionKind::AllowOnce),
                PermissionOption::new(
                    "reject_once",
                    "Reject once",
                    PermissionOptionKind::RejectOnce,
                ),
            ],
        );
        report.push(
            self.request_report_until_cancelled(
                session_id,
                connection,
                "session/request_permission",
                permission,
            )
            .await,
        );
        if callback_cancelled(self, report) {
            return Ok(());
        }

        let write_path = PathBuf::from("/tmp/testy-write.txt");
        report.push(
            self.request_report_until_cancelled(
                session_id,
                connection,
                "fs/write_text_file",
                WriteTextFileRequest::new(
                    session_id.clone(),
                    write_path.clone(),
                    "written by testy\n",
                ),
            )
            .await,
        );
        if callback_cancelled(self, report) {
            return Ok(());
        }
        report.push(
            self.request_report_until_cancelled(
                session_id,
                connection,
                "fs/read_text_file",
                ReadTextFileRequest::new(session_id.clone(), write_path)
                    .line(1)
                    .limit(20),
            )
            .await,
        );
        if callback_cancelled(self, report) {
            return Ok(());
        }

        let terminal = self
            .request_result_until_cancelled(
                session_id,
                connection,
                CreateTerminalRequest::new(session_id.clone(), "printf")
                    .args(vec!["testy terminal\n".to_string()])
                    .cwd(PathBuf::from("/tmp"))
                    .output_byte_limit(1024),
            )
            .await;
        match terminal {
            Ok(response) => {
                let terminal_id = response.terminal_id.clone();
                report.push(format!("terminal/create: ok {response:?}"));
                if callback_cancelled(self, report) {
                    return Ok(());
                }
                report.push(
                    self.request_report_until_cancelled(
                        session_id,
                        connection,
                        "terminal/output",
                        TerminalOutputRequest::new(session_id.clone(), terminal_id.clone()),
                    )
                    .await,
                );
                if callback_cancelled(self, report) {
                    return Ok(());
                }
                report.push(
                    self.request_report_until_cancelled(
                        session_id,
                        connection,
                        "terminal/wait_for_exit",
                        agent_client_protocol::schema::v1::WaitForTerminalExitRequest::new(
                            session_id.clone(),
                            terminal_id.clone(),
                        ),
                    )
                    .await,
                );
                if callback_cancelled(self, report) {
                    return Ok(());
                }
                report.push(
                    self.request_report_until_cancelled(
                        session_id,
                        connection,
                        "terminal/kill",
                        agent_client_protocol::schema::v1::KillTerminalRequest::new(
                            session_id.clone(),
                            terminal_id.clone(),
                        ),
                    )
                    .await,
                );
                if callback_cancelled(self, report) {
                    return Ok(());
                }
                report.push(
                    self.request_report_until_cancelled(
                        session_id,
                        connection,
                        "terminal/release",
                        agent_client_protocol::schema::v1::ReleaseTerminalRequest::new(
                            session_id.clone(),
                            terminal_id,
                        ),
                    )
                    .await,
                );
            }
            Err(error) => {
                report.push(format!("terminal/create: error {error:?}"));
                report.push("terminal/output: skipped".to_string());
                report.push("terminal/wait_for_exit: skipped".to_string());
                report.push("terminal/kill: skipped".to_string());
                report.push("terminal/release: skipped".to_string());
            }
        }
        if callback_cancelled(self, report) {
            return Ok(());
        }

        Ok(())
    }

    /// Helper to execute an operation with a spawned MCP client.
    async fn with_mcp_client<F, Fut, T>(
        &self,
        session_id: &SessionId,
        server_name: &str,
        operation: F,
    ) -> Result<T>
    where
        F: FnOnce(rmcp::service::RunningService<rmcp::RoleClient, ()>) -> Fut,
        Fut: std::future::Future<Output = Result<T>>,
    {
        use rmcp::{
            ServiceExt,
            transport::{ConfigureCommandExt, TokioChildProcess},
        };
        use tokio::process::Command;

        let mcp_servers = self
            .get_mcp_servers(session_id)
            .ok_or_else(|| anyhow::anyhow!("Session not found"))?;

        let mcp_server = mcp_servers
            .iter()
            .find(|server| match server {
                McpServer::Stdio(stdio) => stdio.name == server_name,
                McpServer::Http(http) => http.name == server_name,
                McpServer::Sse(sse) => sse.name == server_name,
                _ => false,
            })
            .ok_or_else(|| anyhow::anyhow!("MCP server '{server_name}' not found"))?;

        match mcp_server {
            McpServer::Stdio(stdio) => {
                self.run_until_session_cancelled(session_id, async move {
                    let mcp_client = ()
                        .serve(TokioChildProcess::new(
                            Command::new(&stdio.command).configure(|cmd| {
                                cmd.args(&stdio.args);
                                for env_var in &stdio.env {
                                    cmd.env(&env_var.name, &env_var.value);
                                }
                            }),
                        )?)
                        .await?;

                    operation(mcp_client).await
                })
                .await
            }
            McpServer::Http(http) => {
                use rmcp::transport::{
                    StreamableHttpClientTransport,
                    streamable_http_client::StreamableHttpClientTransportConfig,
                };

                let transport_config =
                    StreamableHttpClientTransportConfig::with_uri(http.url.as_str())
                        .custom_headers(http_headers(&http.headers)?);

                self.run_until_session_cancelled(session_id, async move {
                    let mcp_client =
                        ().serve(StreamableHttpClientTransport::from_config(transport_config))
                            .await?;

                    operation(mcp_client).await
                })
                .await
            }
            McpServer::Sse(_) => Err(anyhow::anyhow!("SSE MCP servers not yet supported")),
            _ => Err(anyhow::anyhow!("Unknown MCP server type")),
        }
    }

    async fn run_until_session_cancelled<T>(
        &self,
        session_id: &SessionId,
        future: impl std::future::Future<Output = Result<T>>,
    ) -> Result<T> {
        tokio::select! {
            result = future => result,
            () = self.wait_for_cancelled(session_id) => Err(anyhow::anyhow!("session cancelled")),
        }
    }

    async fn list_tools(&self, session_id: &SessionId, server_name: &str) -> Result<String> {
        self.with_mcp_client(session_id, server_name, async move |mcp_client| {
            let tools_result = mcp_client.list_tools(None).await?;
            mcp_client.cancel().await?;

            let tools_list = tools_result
                .tools
                .iter()
                .map(|tool| {
                    format!(
                        "  - {}: {}",
                        tool.name,
                        tool.description.as_deref().unwrap_or("No description")
                    )
                })
                .collect::<Vec<_>>()
                .join("\n");

            Ok(tools_list)
        })
        .await
    }

    async fn execute_tool_call(
        &self,
        session_id: &SessionId,
        server_name: &str,
        tool_name: &str,
        params: serde_json::Value,
    ) -> Result<String> {
        use rmcp::model::CallToolRequestParams;

        let params_obj = params.as_object().cloned().unwrap_or_default();
        let tool_name = tool_name.to_string();

        self.with_mcp_client(session_id, server_name, async move |mcp_client| {
            let tool_result = mcp_client
                .call_tool(CallToolRequestParams::new(tool_name).with_arguments(params_obj))
                .await?;

            mcp_client.cancel().await?;

            Ok(format!("{tool_result:?}"))
        })
        .await
    }
}

impl Default for Testy {
    fn default() -> Self {
        Self::new()
    }
}

impl SessionData {
    fn new(
        cwd: PathBuf,
        additional_directories: Vec<PathBuf>,
        mcp_servers: Vec<McpServer>,
    ) -> Self {
        Self {
            cwd,
            additional_directories,
            mcp_servers,
            current_mode_id: SessionModeId::new("chat"),
            current_config_value: SessionConfigValueId::new("normal"),
            title: "Testy session".to_string(),
            updated_at: "2026-01-01T00:00:00Z".to_string(),
            cancelled: false,
            pending_cancel_status: false,
            closed: false,
            active_prompts: 0,
        }
    }

    fn modes(&self) -> SessionModeState {
        SessionModeState::new(
            self.current_mode_id.clone(),
            vec![
                SessionMode::new("chat", "Chat").description("Default deterministic chat mode"),
                SessionMode::new("plan", "Plan").description("Planning-focused test mode"),
            ],
        )
    }

    fn config_options(&self) -> Vec<SessionConfigOption> {
        vec![
            SessionConfigOption::select(
                "verbosity",
                "Verbosity",
                self.current_config_value.clone(),
                vec![
                    SessionConfigSelectOption::new("brief", "Brief"),
                    SessionConfigSelectOption::new("normal", "Normal"),
                    SessionConfigSelectOption::new("verbose", "Verbose"),
                ],
            )
            .description("Controls how much text Testy includes in summaries"),
        ]
    }
}

/// Extract text content from prompt blocks.
fn extract_text_from_prompt(blocks: &[ContentBlock]) -> String {
    blocks
        .iter()
        .filter_map(|block| match block {
            ContentBlock::Text(TextContent { text, .. }) => Some(text.clone()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join(" ")
}

fn parse_command(input_text: &str) -> TestyCommand {
    if input_text.trim().is_empty() {
        return TestyCommand::Greet;
    }

    if let Ok(command) = serde_json::from_str(input_text) {
        return command;
    }

    let trimmed = input_text.trim();
    if trimmed.eq_ignore_ascii_case("help") {
        return TestyCommand::Help;
    }

    if let Some(scenario) = TestyScenario::from_prompt(trimmed) {
        return TestyCommand::RunScenario { scenario };
    }

    if let Some(message) = trimmed.strip_prefix("echo ") {
        return TestyCommand::Echo {
            message: message.to_string(),
        };
    }

    TestyCommand::Greet
}

fn help_text() -> String {
    let unstable = cfg!(feature = "unstable");
    let scenarios = TestyScenario::all()
        .into_iter()
        .map(TestyScenario::name)
        .collect::<Vec<_>>()
        .join(", ");
    let mut text = format!(
        "Testy commands: help, greet, echo <message>, {scenarios}. JSON command form: {}",
        TestyCommand::RunScenario {
            scenario: TestyScenario::Full
        }
        .to_prompt()
    );
    if unstable {
        text.push_str(
            ". Unstable mode is enabled: callbacks and full include unstable elicitation coverage",
        );
    }
    text
}

fn available_commands() -> Vec<AvailableCommand> {
    let unstable = cfg!(feature = "unstable");
    let full_description = if unstable {
        "Run every stable and enabled unstable Testy scenario"
    } else {
        "Run every stable Testy scenario"
    };
    let callbacks_description = if unstable {
        "Exercise stable and enabled unstable agent-to-client requests"
    } else {
        "Exercise agent-to-client requests"
    };

    let full_command =
        AvailableCommand::new("full", full_description).input(AvailableCommandInput::Unstructured(
            UnstructuredCommandInput::new("optional scenario arguments"),
        ));
    let callbacks_command = AvailableCommand::new("callbacks", callbacks_description);

    #[cfg(feature = "unstable")]
    {
        vec![
            full_command,
            callbacks_command,
            AvailableCommand::new("elicitations", "Exercise unstable elicitation requests"),
        ]
    }
    #[cfg(not(feature = "unstable"))]
    {
        vec![full_command, callbacks_command]
    }
}

#[cfg(feature = "unstable")]
fn testy_elicitation_schema() -> ElicitationSchema {
    ElicitationSchema::new()
        .title("Testy elicitation form")
        .description("Deterministic form covering ACP elicitation field and value shapes")
        .string("name", true)
        .email("email", false)
        .uri("homepage", false)
        .date("birthday", false)
        .date_time("available_at", false)
        .number("confidence", 0.0, 1.0, true)
        .integer("age", 0, 120, true)
        .boolean("confirmed", true)
        .property(
            "priority",
            StringPropertySchema::new().enum_values(vec![
                "low".to_string(),
                "normal".to_string(),
                "high".to_string(),
            ]),
            false,
        )
        .property(
            "tags",
            MultiSelectPropertySchema::new(vec![
                "rust".to_string(),
                "acp".to_string(),
                "testy".to_string(),
            ]),
            false,
        )
}

#[cfg(feature = "unstable")]
fn elicitation_action_summary(action: &ElicitationAction) -> String {
    match action {
        ElicitationAction::Accept(action) => {
            let field_count = action.content.as_ref().map_or(0, BTreeMap::len);
            format!("accept content_fields={field_count}")
        }
        ElicitationAction::Decline => "decline".to_string(),
        ElicitationAction::Cancel => "cancel".to_string(),
        _ => "unknown".to_string(),
    }
}

#[cfg(feature = "unstable")]
fn elicitation_cancelled(agent: &Testy, session_id: &SessionId, report: &mut Vec<String>) -> bool {
    if agent.is_cancelled(session_id) {
        report.push("elicitations: cancelled".to_string());
        true
    } else {
        false
    }
}

#[cfg(feature = "unstable")]
fn url_elicitation_required_error() -> agent_client_protocol::Error {
    agent_client_protocol::Error::invalid_params().data(url_elicitation_required_data())
}

#[cfg(feature = "unstable")]
fn is_url_elicitation_required_error(error: &agent_client_protocol::Error) -> bool {
    error.code == ErrorCode::InvalidParams
        && error
            .data
            .as_ref()
            .is_some_and(|data| data.get("elicitations").is_some())
}

#[cfg(feature = "unstable")]
fn url_elicitation_required_data() -> serde_json::Value {
    serde_json::json!({
        "elicitations": [{
            "mode": "url",
            "elicitationId": "testy-url-required",
            "url": "https://example.com/testy/required",
            "message": "Complete the Testy URL elicitation before continuing"
        }]
    })
}

fn send_session_update(
    connection: &ConnectionTo<Client>,
    session_id: &SessionId,
    update: SessionUpdate,
) -> Result<(), agent_client_protocol::Error> {
    connection.send_notification(SessionNotification::new(session_id.clone(), update))
}

fn invalid_params(message: impl ToString) -> agent_client_protocol::Error {
    agent_client_protocol::Error::invalid_params().data(message.to_string())
}

fn is_supported_mode_id(mode_id: &SessionModeId) -> bool {
    matches!(mode_id.0.as_ref(), "chat" | "plan")
}

fn is_supported_config_id(config_id: &SessionConfigId) -> bool {
    config_id.0.as_ref() == "verbosity"
}

fn is_supported_config_value_id(config_value_id: &SessionConfigValueId) -> bool {
    matches!(config_value_id.0.as_ref(), "brief" | "normal" | "verbose")
}

fn config_value_id_from_request(
    request: &SetSessionConfigOptionRequest,
) -> Result<SessionConfigValueId, agent_client_protocol::Error> {
    let value = serde_json::to_value(&request.value)
        .map_err(agent_client_protocol::Error::into_internal_error)?;
    match value {
        serde_json::Value::String(value) => Ok(SessionConfigValueId::new(value)),
        serde_json::Value::Object(object) => match object.get("value") {
            Some(serde_json::Value::String(value)) => Ok(SessionConfigValueId::new(value.clone())),
            Some(serde_json::Value::Bool(value)) => {
                Ok(SessionConfigValueId::new(value.to_string()))
            }
            _ => Err(invalid_params(
                "session config value must include a string value",
            )),
        },
        _ => Err(invalid_params(
            "session config value must be a string or object",
        )),
    }
}

fn stop_reason_message_id_prefix(stop_reason: StopReason) -> &'static str {
    match stop_reason {
        StopReason::EndTurn => "testy-message-end-turn",
        StopReason::MaxTokens => "testy-message-max-tokens",
        StopReason::MaxTurnRequests => "testy-message-max-turn-requests",
        StopReason::Refusal => "testy-message-refusal",
        StopReason::Cancelled => "testy-message-cancelled",
        _ => "testy-message-other",
    }
}

impl ConnectTo<Client> for Testy {
    async fn connect_to(
        self,
        client: impl ConnectTo<Agent>,
    ) -> Result<(), agent_client_protocol::Error> {
        Agent
            .builder()
            .name("test-agent")
            .on_receive_request(
                {
                    let agent = self.clone();
                    async move |initialize: InitializeRequest, responder, _cx| {
                        agent.handle_initialize(initialize, responder)
                    }
                },
                agent_client_protocol::on_receive_request!(),
            )
            .on_receive_request(
                {
                    let agent = self.clone();
                    async move |request: AuthenticateRequest, responder, _cx| {
                        agent.handle_authenticate(request, responder)
                    }
                },
                agent_client_protocol::on_receive_request!(),
            )
            .on_receive_request(
                {
                    let agent = self.clone();
                    async move |_request: LogoutRequest, responder, _cx| {
                        agent.handle_logout(responder)
                    }
                },
                agent_client_protocol::on_receive_request!(),
            )
            .on_receive_request(
                {
                    let agent = self.clone();
                    async move |request: NewSessionRequest, responder, _cx| {
                        let (session_id, session) = agent.create_session(
                            request.cwd,
                            request.additional_directories,
                            request.mcp_servers,
                        );
                        responder.respond(
                            NewSessionResponse::new(session_id)
                                .modes(session.modes())
                                .config_options(session.config_options()),
                        )
                    }
                },
                agent_client_protocol::on_receive_request!(),
            )
            .on_receive_request(
                {
                    let agent = self.clone();
                    async move |request: LoadSessionRequest, responder, _cx| {
                        let session = agent.upsert_session(
                            request.session_id,
                            request.cwd,
                            request.additional_directories,
                            request.mcp_servers,
                        );
                        responder.respond(
                            LoadSessionResponse::new()
                                .modes(session.modes())
                                .config_options(session.config_options()),
                        )
                    }
                },
                agent_client_protocol::on_receive_request!(),
            )
            .on_receive_request(
                {
                    let agent = self.clone();
                    async move |request: ResumeSessionRequest, responder, _cx| {
                        let session = agent.upsert_session(
                            request.session_id,
                            request.cwd,
                            request.additional_directories,
                            request.mcp_servers,
                        );
                        responder.respond(
                            ResumeSessionResponse::new()
                                .modes(session.modes())
                                .config_options(session.config_options()),
                        )
                    }
                },
                agent_client_protocol::on_receive_request!(),
            )
            .on_receive_request(
                {
                    let agent = self.clone();
                    async move |request: ListSessionsRequest, responder, _cx| {
                        agent.handle_list_sessions(request, responder)
                    }
                },
                agent_client_protocol::on_receive_request!(),
            )
            .on_receive_request(
                {
                    let agent = self.clone();
                    async move |request: DeleteSessionRequest, responder, _cx| {
                        agent.handle_delete_session(request, responder)
                    }
                },
                agent_client_protocol::on_receive_request!(),
            )
            .on_receive_request(
                {
                    let agent = self.clone();
                    async move |request: CloseSessionRequest, responder, _cx| {
                        agent.handle_close_session(request, responder)
                    }
                },
                agent_client_protocol::on_receive_request!(),
            )
            .on_receive_request(
                {
                    let agent = self.clone();
                    async move |request: SetSessionModeRequest, responder, _cx| {
                        agent.handle_set_mode(request, responder)
                    }
                },
                agent_client_protocol::on_receive_request!(),
            )
            .on_receive_request(
                {
                    let agent = self.clone();
                    async move |request: SetSessionConfigOptionRequest, responder, _cx| {
                        agent.handle_set_config_option(request, responder)
                    }
                },
                agent_client_protocol::on_receive_request!(),
            )
            .on_receive_request(
                {
                    let agent = self.clone();
                    async move |request: PromptRequest, responder, cx| {
                        let session_id = request.session_id.clone();
                        if let Err(error) = agent.begin_prompt(&session_id) {
                            return responder.respond_with_error(error);
                        }
                        let cx_clone = cx.clone();
                        let spawn_result = cx.spawn({
                            let agent = agent.clone();
                            async move { agent.process_prompt(request, responder, cx_clone).await }
                        });
                        if spawn_result.is_err() {
                            agent.finish_prompt(&session_id, StopReason::EndTurn);
                        }
                        spawn_result
                    }
                },
                agent_client_protocol::on_receive_request!(),
            )
            .on_receive_notification(
                {
                    let agent = self;
                    async move |notification: CancelNotification, _cx| {
                        agent.mark_cancelled(&notification.session_id);
                        Ok(())
                    }
                },
                agent_client_protocol::on_receive_notification!(),
            )
            .connect_to(client)
            .await
    }
}

fn http_headers(headers: &[HttpHeader]) -> Result<HashMap<http::HeaderName, http::HeaderValue>> {
    headers
        .iter()
        .map(|header| {
            let name = http::HeaderName::from_bytes(header.name.as_bytes()).map_err(|error| {
                anyhow::anyhow!("invalid HTTP header name `{}`: {error}", header.name)
            })?;
            let value = http::HeaderValue::from_str(&header.value).map_err(|error| {
                anyhow::anyhow!("invalid HTTP header value for `{}`: {error}", header.name)
            })?;
            Ok((name, value))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn http_headers_preserve_configured_values() {
        let headers = http_headers(&[
            HttpHeader::new("Authorization", "Bearer test-token"),
            HttpHeader::new("X-Testy", "enabled"),
        ])
        .unwrap();

        assert_eq!(
            headers.get(&http::header::AUTHORIZATION).unwrap(),
            "Bearer test-token"
        );
        assert_eq!(
            headers
                .get(&http::HeaderName::from_static("x-testy"))
                .unwrap(),
            "enabled"
        );
    }

    #[test]
    fn http_headers_reject_invalid_header_values() {
        let error = http_headers(&[HttpHeader::new("X-Testy", "line\nbreak")]).unwrap_err();

        assert!(
            error
                .to_string()
                .contains("invalid HTTP header value for `X-Testy`")
        );
    }

    #[test]
    fn cancellation_stays_set_until_all_active_prompts_finish() {
        let testy = Testy::new();
        let (session_id, _) = testy.create_session(PathBuf::from("/tmp"), vec![], vec![]);

        testy.begin_prompt(&session_id).unwrap();
        testy.begin_prompt(&session_id).unwrap();
        testy.mark_cancelled(&session_id);

        assert_eq!(
            testy.finish_prompt(&session_id, StopReason::EndTurn),
            StopReason::Cancelled
        );
        assert!(testy.is_cancelled(&session_id));

        assert_eq!(
            testy.finish_prompt(&session_id, StopReason::EndTurn),
            StopReason::Cancelled
        );
        assert!(!testy.is_cancelled(&session_id));
    }

    #[cfg(feature = "unstable")]
    #[test]
    fn parse_command_accepts_elicitation_prompt_aliases() {
        for input in ["elicitations", "elicitation", "elicit"] {
            let command = parse_command(input);
            assert!(
                matches!(
                    command,
                    TestyCommand::RunScenario {
                        scenario: TestyScenario::Elicitations
                    }
                ),
                "unexpected command for {input:?}: {command:?}"
            );
        }
    }
}
