//! Native draft protocol v2 support for Testy.

use super::{TestyCommand, TestyScenario, parse_command};
use agent_client_protocol::{
    Agent, Client, ConnectTo, RequestCancellation, Responder, V2ConnectionTo,
    schema::{ProtocolVersion, v2 as acp},
};
use std::{
    collections::{HashMap, VecDeque},
    sync::{Arc, Mutex},
};
use tokio::sync::Notify;

const SESSION_TITLE: &str = "Testy v2 session";
const SESSION_UPDATED_AT: &str = "2026-01-01T00:00:00Z";

/// The native draft protocol v2 Testy agent.
///
/// Construct this with [`super::Testy::v2`]. Unlike the v1 fixture, prompt
/// responses only acknowledge acceptance; output and completion are reported
/// independently through typed `session/update` notifications.
#[derive(Clone, Debug)]
pub struct V2Testy {
    state: Arc<Mutex<V2TestyState>>,
    state_changed: Arc<Notify>,
}

#[derive(Debug, Default)]
struct V2TestyState {
    sessions: HashMap<acp::SessionId, V2SessionData>,
    next_session_id: u64,
    next_message_id: u64,
    next_prompt_ticket: u64,
}

#[derive(Clone, Debug)]
struct V2SessionData {
    cwd: acp::AbsolutePath,
    active: bool,
    foreground_work: bool,
    cancelled: bool,
    /// Incremented by each `session/cancel` and `session/close`. A held
    /// prompt is cancelled when this changes after it was received.
    cancel_epoch: u64,
    /// Prompts received during foreground work, held in received order.
    held_prompts: VecDeque<u64>,
    history: Vec<acp::SessionUpdate>,
}

/// How an accepted `session/prompt` request proceeds.
enum PromptAdmission {
    /// The prompt starts foreground work now.
    Inserted,
    /// The prompt waits for the current foreground work to end.
    Held { ticket: u64, cancel_epoch: u64 },
}

impl V2Testy {
    pub(super) fn new() -> Self {
        Self {
            state: Arc::new(Mutex::new(V2TestyState {
                next_session_id: 1,
                ..V2TestyState::default()
            })),
            state_changed: Arc::new(Notify::new()),
        }
    }

    fn lock_state(&self) -> std::sync::MutexGuard<'_, V2TestyState> {
        self.state.lock().expect("v2 Testy state lock poisoned")
    }

    fn create_session(
        &self,
        request: acp::NewSessionRequest,
    ) -> Result<acp::SessionId, agent_client_protocol::Error> {
        require_absolute(&request.cwd, "cwd")?;
        reject_mcp_servers(&request.mcp_servers)?;
        let mut state = self.lock_state();
        loop {
            let session_id =
                acp::SessionId::new(format!("testy-v2-session-{}", state.next_session_id));
            state.next_session_id += 1;
            if !state.sessions.contains_key(&session_id) {
                state.sessions.insert(
                    session_id.clone(),
                    // Testy does not advertise `additionalDirectories`, so it
                    // ignores that field.
                    V2SessionData {
                        cwd: request.cwd,
                        active: true,
                        foreground_work: false,
                        cancelled: false,
                        cancel_epoch: 0,
                        held_prompts: VecDeque::new(),
                        history: Vec::new(),
                    },
                );
                return Ok(session_id);
            }
        }
    }

    fn next_message_id(&self, kind: &str) -> acp::MessageId {
        let mut state = self.lock_state();
        let message_id = acp::MessageId::new(format!("testy-v2-{kind}-{}", state.next_message_id));
        state.next_message_id += 1;
        message_id
    }

    fn list_sessions(&self, request: &acp::ListSessionsRequest) -> Vec<acp::SessionInfo> {
        let state = self.lock_state();
        let mut sessions = state
            .sessions
            .iter()
            .filter(|(_, session)| request.cwd.as_ref().is_none_or(|cwd| cwd == &session.cwd))
            .map(|(session_id, session)| {
                acp::SessionInfo::new(session_id.clone(), session.cwd.clone())
                    .title(SESSION_TITLE)
                    .updated_at(SESSION_UPDATED_AT)
            })
            .collect::<Vec<_>>();
        sessions.sort_by(|left, right| {
            left.session_id
                .to_string()
                .cmp(&right.session_id.to_string())
        });
        sessions
    }

    fn resume_session(
        &self,
        request: &acp::ResumeSessionRequest,
    ) -> Result<Vec<acp::SessionUpdate>, agent_client_protocol::Error> {
        require_absolute(&request.cwd, "cwd")?;
        reject_mcp_servers(&request.mcp_servers)?;
        let mut state = self.lock_state();
        let session = state
            .sessions
            .get_mut(&request.session_id)
            .ok_or_else(|| session_not_found(&request.session_id))?;
        if session.foreground_work {
            return Err(invalid_params(format!(
                "session `{}` still has foreground work",
                request.session_id
            )));
        }
        if session.cwd != request.cwd {
            return Err(invalid_params(format!(
                "session `{}` has a different working directory",
                request.session_id
            )));
        }

        let replay = match &request.replay_from {
            None => Vec::new(),
            Some(acp::ReplayFrom::Start(_)) => session.history.clone(),
            Some(_) => {
                return Err(invalid_params("unsupported session replay cursor"));
            }
        };
        session.active = true;
        session.cancelled = false;
        Ok(replay)
    }

    /// Accept a prompt, inserting it now or holding it behind active work.
    ///
    /// A v2 Agent must not reject a prompt only because foreground work is
    /// active, so Testy holds it and inserts held prompts in received order.
    fn admit_prompt(
        &self,
        session_id: &acp::SessionId,
        prompt: &[acp::ContentBlock],
    ) -> Result<PromptAdmission, agent_client_protocol::Error> {
        let mut state = self.lock_state();
        let ticket = state.next_prompt_ticket;
        let session = state
            .sessions
            .get_mut(session_id)
            .filter(|session| session.active)
            .ok_or_else(|| session_not_found(session_id))?;
        validate_prompt_content(prompt)?;
        if session.foreground_work || !session.held_prompts.is_empty() {
            session.held_prompts.push_back(ticket);
            let cancel_epoch = session.cancel_epoch;
            state.next_prompt_ticket += 1;
            return Ok(PromptAdmission::Held {
                ticket,
                cancel_epoch,
            });
        }
        session.foreground_work = true;
        session.cancelled = false;
        Ok(PromptAdmission::Inserted)
    }

    /// Wait until a held prompt can start foreground work.
    ///
    /// Fails with `-32800` when the prompt is withdrawn with
    /// `$/cancel_request`, or when `session/cancel` or `session/close` arrives
    /// before it is inserted.
    async fn insert_held_prompt(
        &self,
        session_id: &acp::SessionId,
        ticket: u64,
        cancel_epoch: u64,
        withdrawn: RequestCancellation,
    ) -> Result<(), agent_client_protocol::Error> {
        loop {
            let notified = self.state_changed.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            {
                let mut state = self.lock_state();
                let Some(session) = state.sessions.get_mut(session_id) else {
                    return Err(agent_client_protocol::Error::request_cancelled());
                };
                if !session.active
                    || session.cancel_epoch != cancel_epoch
                    || withdrawn.is_cancelled()
                {
                    session.held_prompts.retain(|held| *held != ticket);
                    drop(state);
                    self.state_changed.notify_waiters();
                    return Err(agent_client_protocol::Error::request_cancelled());
                }
                if !session.foreground_work && session.held_prompts.front() == Some(&ticket) {
                    session.held_prompts.pop_front();
                    session.foreground_work = true;
                    session.cancelled = false;
                    return Ok(());
                }
            }
            tokio::select! {
                () = &mut notified => {}
                () = withdrawn.cancelled() => {}
            }
        }
    }

    fn abandon_prompt(&self, session_id: &acp::SessionId) {
        if let Some(session) = self.lock_state().sessions.get_mut(session_id) {
            session.foreground_work = false;
            session.cancelled = false;
        }
        self.state_changed.notify_waiters();
    }

    fn finish_prompt(
        &self,
        session_id: &acp::SessionId,
        connection: &V2ConnectionTo<Client>,
        requested_stop_reason: acp::StopReason,
    ) -> Result<(), agent_client_protocol::Error> {
        let mut state = self.lock_state();
        let session = state
            .sessions
            .get_mut(session_id)
            .ok_or_else(|| session_not_found(session_id))?;
        let stop_reason = if session.cancelled || !session.active {
            acp::StopReason::Cancelled
        } else {
            requested_stop_reason
        };
        let send_result = send_update(
            connection,
            session_id,
            acp::SessionUpdate::StateUpdate(acp::StateUpdate::Idle(
                acp::IdleStateUpdate::new().stop_reason(stop_reason),
            )),
        );
        session.foreground_work = false;
        session.cancelled = false;
        drop(state);
        self.state_changed.notify_waiters();
        send_result
    }

    /// Handle `session/cancel`: cancel foreground work and every prompt held
    /// before it. Agents ignore `session/cancel` for a session that is not
    /// active.
    fn mark_cancelled(&self, session_id: &acp::SessionId) {
        let did_cancel = self
            .lock_state()
            .sessions
            .get_mut(session_id)
            .filter(|session| session.active)
            .is_some_and(|session| {
                session.cancel_epoch += 1;
                if session.foreground_work {
                    session.cancelled = true;
                }
                true
            });
        if did_cancel {
            self.state_changed.notify_waiters();
        }
    }

    /// Close a session and report whether foreground work must finish before
    /// the close response.
    ///
    /// Closing is idempotent: an unknown or already-closed session closes
    /// successfully with nothing to wait for.
    fn close_session(&self, session_id: &acp::SessionId) -> bool {
        let mut state = self.lock_state();
        let Some(session) = state
            .sessions
            .get_mut(session_id)
            .filter(|session| session.active)
        else {
            return false;
        };
        session.active = false;
        session.cancel_epoch += 1;
        if session.foreground_work {
            session.cancelled = true;
        }
        let foreground_work = session.foreground_work;
        drop(state);
        self.state_changed.notify_waiters();
        foreground_work
    }

    fn is_cancelled(&self, session_id: &acp::SessionId) -> bool {
        self.lock_state()
            .sessions
            .get(session_id)
            .is_none_or(|session| session.cancelled || !session.active)
    }

    fn has_foreground_work(&self, session_id: &acp::SessionId) -> bool {
        self.lock_state()
            .sessions
            .get(session_id)
            .is_some_and(|session| session.foreground_work)
    }

    async fn wait_for_cancelled(&self, session_id: &acp::SessionId) {
        let notified = self.state_changed.notified();
        tokio::pin!(notified);
        loop {
            notified.as_mut().enable();
            if self.is_cancelled(session_id) {
                return;
            }
            notified.as_mut().await;
            notified.set(self.state_changed.notified());
        }
    }

    async fn wait_for_foreground_work(&self, session_id: &acp::SessionId) {
        let notified = self.state_changed.notified();
        tokio::pin!(notified);
        loop {
            notified.as_mut().enable();
            if !self.has_foreground_work(session_id) {
                return;
            }
            notified.as_mut().await;
            notified.set(self.state_changed.notified());
        }
    }

    fn record_history(&self, session_id: &acp::SessionId, update: acp::SessionUpdate) {
        if let Some(session) = self.lock_state().sessions.get_mut(session_id) {
            session.history.push(update);
        }
    }

    async fn process_prompt(
        &self,
        request: acp::PromptRequest,
        user_update: acp::SessionUpdate,
        connection: V2ConnectionTo<Client>,
    ) -> Result<(), agent_client_protocol::Error> {
        let session_id = request.session_id.clone();
        match self
            .process_prompt_inner(request, user_update, &connection)
            .await
        {
            Ok(stop_reason) => self.finish_prompt(&session_id, &connection, stop_reason),
            Err(error) => {
                self.abandon_prompt(&session_id);
                Err(error)
            }
        }
    }

    async fn process_prompt_inner(
        &self,
        request: acp::PromptRequest,
        user_update: acp::SessionUpdate,
        connection: &V2ConnectionTo<Client>,
    ) -> Result<acp::StopReason, agent_client_protocol::Error> {
        let session_id = request.session_id;
        send_update(connection, &session_id, user_update)?;

        send_update(
            connection,
            &session_id,
            acp::SessionUpdate::StateUpdate(acp::StateUpdate::Running(
                acp::RunningStateUpdate::new(),
            )),
        )?;

        let command = parse_command(&extract_text_from_prompt(&request.prompt));
        let (response_text, requested_stop_reason) = match command {
            TestyCommand::Help => (v2_help_text(), acp::StopReason::EndTurn),
            TestyCommand::Greet => ("Hello, world!".to_string(), acp::StopReason::EndTurn),
            TestyCommand::Echo { message } => (message, acp::StopReason::EndTurn),
            TestyCommand::RunScenario {
                scenario: TestyScenario::WaitForCancel,
            } => {
                self.wait_for_cancelled(&session_id).await;
                (String::new(), acp::StopReason::Cancelled)
            }
            TestyCommand::RunScenario {
                scenario: TestyScenario::CancelStatus,
            } => {
                let cancelled = self.is_cancelled(&session_id);
                (
                    format!(
                        "cancel_status: {}",
                        if cancelled {
                            "cancelled"
                        } else {
                            "not_cancelled"
                        }
                    ),
                    if cancelled {
                        acp::StopReason::Cancelled
                    } else {
                        acp::StopReason::EndTurn
                    },
                )
            }
            TestyCommand::RunScenario { scenario } => unsupported_command(format!(
                "Testy v2 scenario `{}` is not implemented yet",
                scenario.name()
            )),
            TestyCommand::CallTool { .. } | TestyCommand::ListTools { .. } => {
                unsupported_command("Testy v2 does not advertise MCP support yet".to_string())
            }
        };

        if requested_stop_reason != acp::StopReason::Cancelled && !self.is_cancelled(&session_id) {
            let agent_message_id = self.next_message_id("agent-message");
            send_update(
                connection,
                &session_id,
                acp::SessionUpdate::AgentMessageChunk(acp::ContentChunk::new(
                    response_text.clone().into(),
                    agent_message_id.clone(),
                )),
            )?;
            self.record_history(
                &session_id,
                acp::SessionUpdate::AgentMessage(
                    acp::AgentMessage::new(agent_message_id).content(vec![response_text.into()]),
                ),
            );
        }

        Ok(requested_stop_reason)
    }

    /// Insert the user message for a prompt that holds the session's
    /// foreground work, acknowledge it, and run it.
    fn run_inserted_prompt(
        &self,
        request: acp::PromptRequest,
        responder: Responder<acp::PromptResponse>,
        connection: &V2ConnectionTo<Client>,
    ) -> Result<(), agent_client_protocol::Error> {
        let session_id = request.session_id.clone();
        let user_message_id = self.next_message_id("user-message");
        let user_update = acp::SessionUpdate::UserMessage(
            acp::UserMessage::new(user_message_id.clone()).content(request.prompt.clone()),
        );
        self.record_history(&session_id, user_update.clone());
        if let Err(error) = responder.respond(acp::PromptResponse::new(user_message_id)) {
            self.abandon_prompt(&session_id);
            return Err(error);
        }
        let prompt_connection = connection.clone();
        let spawn_result = connection.spawn({
            let agent = self.clone();
            async move {
                agent
                    .process_prompt(request, user_update, prompt_connection)
                    .await
            }
        });
        if spawn_result.is_err() {
            self.abandon_prompt(&session_id);
        }
        spawn_result
    }

    fn initialize_response(protocol_version: ProtocolVersion) -> acp::InitializeResponse {
        acp::InitializeResponse::new(
            protocol_version,
            acp::Implementation::new("test-agent", env!("CARGO_PKG_VERSION")),
        )
        .capabilities(acp::AgentCapabilities::new().session(acp::SessionCapabilities::new()))
    }
}

impl ConnectTo<Client> for V2Testy {
    async fn connect_to(
        self,
        client: impl ConnectTo<Agent>,
    ) -> Result<(), agent_client_protocol::Error> {
        Agent
            .v2()
            .name("test-agent-v2")
            .on_receive_request(
                async |request: acp::InitializeRequest,
                       responder: Responder<acp::InitializeResponse>,
                       _connection: V2ConnectionTo<Client>| {
                    responder.respond(V2Testy::initialize_response(request.protocol_version))
                },
                agent_client_protocol::on_receive_request!(),
            )
            .on_receive_request(
                {
                    let agent = self.clone();
                    async move |request: acp::NewSessionRequest,
                                responder: Responder<acp::NewSessionResponse>,
                                _connection: V2ConnectionTo<Client>| {
                        match agent.create_session(request) {
                            Ok(session_id) => {
                                responder.respond(acp::NewSessionResponse::new(session_id))
                            }
                            Err(error) => responder.respond_with_error(error),
                        }
                    }
                },
                agent_client_protocol::on_receive_request!(),
            )
            .on_receive_request(
                {
                    let agent = self.clone();
                    async move |request: acp::ListSessionsRequest,
                                responder: Responder<acp::ListSessionsResponse>,
                                _connection: V2ConnectionTo<Client>| {
                        // Testy returns every session in one page and never issues a
                        // `nextCursor`, so any cursor a client sends is invalid.
                        if let Some(cursor) = &request.cursor {
                            return responder.respond_with_error(invalid_params(format!(
                                "invalid session/list cursor `{cursor}`"
                            )));
                        }
                        if let Some(cwd) = &request.cwd
                            && let Err(error) = require_absolute(cwd, "cwd")
                        {
                            return responder.respond_with_error(error);
                        }
                        responder.respond(acp::ListSessionsResponse::new(
                            agent.list_sessions(&request),
                        ))
                    }
                },
                agent_client_protocol::on_receive_request!(),
            )
            .on_receive_request(
                {
                    let agent = self.clone();
                    async move |request: acp::ResumeSessionRequest,
                                responder: Responder<acp::ResumeSessionResponse>,
                                connection: V2ConnectionTo<Client>| {
                        let replay = match agent.resume_session(&request) {
                            Ok(replay) => replay,
                            Err(error) => return responder.respond_with_error(error),
                        };
                        for update in replay {
                            send_update(&connection, &request.session_id, update)?;
                        }
                        // Synchronize current metadata rather than replaying
                        // historical changes.
                        send_update(
                            &connection,
                            &request.session_id,
                            acp::SessionUpdate::SessionInfoUpdate(
                                acp::SessionInfoUpdate::new()
                                    .title(SESSION_TITLE)
                                    .updated_at(SESSION_UPDATED_AT),
                            ),
                        )?;
                        // Testy only resumes sessions without foreground work.
                        // This is a live snapshot, not replayed turn completion.
                        send_update(
                            &connection,
                            &request.session_id,
                            acp::SessionUpdate::StateUpdate(acp::StateUpdate::Idle(
                                acp::IdleStateUpdate::new(),
                            )),
                        )?;
                        responder.respond(acp::ResumeSessionResponse::new())
                    }
                },
                agent_client_protocol::on_receive_request!(),
            )
            .on_receive_request(
                {
                    let agent = self.clone();
                    async move |request: acp::CloseSessionRequest,
                                responder: Responder<acp::CloseSessionResponse>,
                                connection: V2ConnectionTo<Client>| {
                        if !agent.close_session(&request.session_id) {
                            return responder.respond(acp::CloseSessionResponse::new());
                        }
                        let waiting_agent = agent.clone();
                        connection.spawn(async move {
                            waiting_agent
                                .wait_for_foreground_work(&request.session_id)
                                .await;
                            responder.respond(acp::CloseSessionResponse::new())
                        })
                    }
                },
                agent_client_protocol::on_receive_request!(),
            )
            .on_receive_request(
                {
                    let agent = self.clone();
                    async move |request: acp::PromptRequest,
                                responder: Responder<acp::PromptResponse>,
                                connection: V2ConnectionTo<Client>| {
                        match agent.admit_prompt(&request.session_id, &request.prompt) {
                            Err(error) => responder.respond_with_error(error),
                            Ok(PromptAdmission::Inserted) => {
                                agent.run_inserted_prompt(request, responder, &connection)
                            }
                            Ok(PromptAdmission::Held {
                                ticket,
                                cancel_epoch,
                            }) => {
                                let withdrawn = responder.cancellation();
                                let held_connection = connection.clone();
                                let agent = agent.clone();
                                connection.spawn(async move {
                                    match agent
                                        .insert_held_prompt(
                                            &request.session_id,
                                            ticket,
                                            cancel_epoch,
                                            withdrawn,
                                        )
                                        .await
                                    {
                                        Ok(()) => agent.run_inserted_prompt(
                                            request,
                                            responder,
                                            &held_connection,
                                        ),
                                        Err(error) => responder.respond_with_error(error),
                                    }
                                })
                            }
                        }
                    }
                },
                agent_client_protocol::on_receive_request!(),
            )
            .on_receive_notification(
                {
                    let agent = self;
                    async move |notification: acp::CancelSessionNotification,
                                _connection: V2ConnectionTo<Client>| {
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

/// Testy advertises no prompt capabilities, so only the baseline `text` and `resource_link`
/// blocks are supported. v2 Agents must reject any other block with `-32602`.
fn validate_prompt_content(
    blocks: &[acp::ContentBlock],
) -> Result<(), agent_client_protocol::Error> {
    for block in blocks {
        match block {
            acp::ContentBlock::Text(_) | acp::ContentBlock::ResourceLink(_) => {}
            _ => return Err(invalid_params("unsupported prompt content block")),
        }
    }
    Ok(())
}

fn extract_text_from_prompt(blocks: &[acp::ContentBlock]) -> String {
    blocks
        .iter()
        .filter_map(|block| match block {
            acp::ContentBlock::Text(text) => Some(text.text.clone()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join(" ")
}

fn v2_help_text() -> String {
    format!(
        "Testy v2 commands: help, greet, echo <message>, wait_for_cancel, cancel_status. JSON command form: {}",
        TestyCommand::RunScenario {
            scenario: TestyScenario::WaitForCancel,
        }
        .to_prompt()
    )
}

fn send_update(
    connection: &V2ConnectionTo<Client>,
    session_id: &acp::SessionId,
    update: acp::SessionUpdate,
) -> Result<(), agent_client_protocol::Error> {
    connection.send_notification(acp::UpdateSessionNotification::new(
        session_id.clone(),
        update,
    ))
}

fn invalid_params(message: impl ToString) -> agent_client_protocol::Error {
    agent_client_protocol::Error::invalid_params().data(message.to_string())
}

/// `-32002` for a session that does not exist or is not active on this connection.
fn session_not_found(session_id: &acp::SessionId) -> agent_client_protocol::Error {
    agent_client_protocol::Error::session_not_found(session_id.0.clone())
}

/// `cwd` must be an absolute path in the native syntax of the Agent's machine;
/// a relative one is rejected with `-32602`.
fn require_absolute(
    path: &acp::AbsolutePath,
    field: &str,
) -> Result<(), agent_client_protocol::Error> {
    if AsRef::<std::path::Path>::as_ref(path).is_absolute() {
        Ok(())
    } else {
        Err(invalid_params(format!(
            "`{field}` must be an absolute path"
        )))
    }
}

/// Testy v2 advertises no `session.mcp` transports, so every MCP server is
/// unsupported and the request must fail with `-32602`.
fn reject_mcp_servers(mcp_servers: &[acp::McpServer]) -> Result<(), agent_client_protocol::Error> {
    if mcp_servers.is_empty() {
        Ok(())
    } else {
        Err(invalid_params(
            "Testy v2 does not advertise any MCP server transports",
        ))
    }
}

/// Ends a command Testy v2 cannot run with an `error` stop reason; `refusal`
/// is reserved for the agent refusing to continue.
fn unsupported_command(message: String) -> (String, acp::StopReason) {
    let error = acp::Error::invalid_params().data(message.clone());
    (
        message,
        acp::StopReason::Error(acp::ErrorStopReason::new().error(error)),
    )
}
