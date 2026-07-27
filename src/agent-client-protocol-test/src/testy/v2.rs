//! Native draft protocol v2 support for Testy.

use super::{TestyCommand, TestyScenario, parse_command};
use agent_client_protocol::{
    Agent, Client, ConnectTo, Responder, V2ConnectionTo,
    schema::{ProtocolVersion, v2 as acp},
};
use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
};
use tokio::sync::Notify;

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
}

#[derive(Clone, Debug)]
struct V2SessionData {
    cwd: acp::AbsolutePath,
    additional_directories: Vec<acp::AbsolutePath>,
    active: bool,
    foreground_work: bool,
    cancelled: bool,
    history: Vec<acp::SessionUpdate>,
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

    fn create_session(&self, request: acp::NewSessionRequest) -> acp::SessionId {
        let mut state = self.lock_state();
        loop {
            let session_id =
                acp::SessionId::new(format!("testy-v2-session-{}", state.next_session_id));
            state.next_session_id += 1;
            if !state.sessions.contains_key(&session_id) {
                state.sessions.insert(
                    session_id.clone(),
                    V2SessionData {
                        cwd: request.cwd,
                        additional_directories: request.additional_directories,
                        active: true,
                        foreground_work: false,
                        cancelled: false,
                        history: Vec::new(),
                    },
                );
                return session_id;
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
                    .additional_directories(session.additional_directories.clone())
                    .title("Testy v2 session")
                    .updated_at("2026-01-01T00:00:00Z")
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
        let mut state = self.lock_state();
        let session = state
            .sessions
            .get_mut(&request.session_id)
            .ok_or_else(|| invalid_params(format!("unknown session `{}`", request.session_id)))?;
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
        session
            .additional_directories
            .clone_from(&request.additional_directories);
        session.active = true;
        session.cancelled = false;
        Ok(replay)
    }

    fn begin_prompt(
        &self,
        session_id: &acp::SessionId,
    ) -> Result<(), agent_client_protocol::Error> {
        let mut state = self.lock_state();
        let session = state
            .sessions
            .get_mut(session_id)
            .ok_or_else(|| invalid_params(format!("unknown session `{session_id}`")))?;
        if !session.active {
            return Err(invalid_params(format!("closed session `{session_id}`")));
        }
        if session.foreground_work {
            return Err(invalid_params(format!(
                "session `{session_id}` already has foreground work"
            )));
        }
        session.foreground_work = true;
        session.cancelled = false;
        Ok(())
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
            .ok_or_else(|| invalid_params(format!("unknown session `{session_id}`")))?;
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

    fn mark_cancelled(&self, session_id: &acp::SessionId) {
        let did_cancel = self
            .lock_state()
            .sessions
            .get_mut(session_id)
            .is_some_and(|session| {
                if session.foreground_work {
                    session.cancelled = true;
                    true
                } else {
                    false
                }
            });
        if did_cancel {
            self.state_changed.notify_waiters();
        }
    }

    fn close_session(
        &self,
        session_id: &acp::SessionId,
    ) -> Result<bool, agent_client_protocol::Error> {
        let mut state = self.lock_state();
        let session = state
            .sessions
            .get_mut(session_id)
            .ok_or_else(|| invalid_params(format!("unknown session `{session_id}`")))?;
        session.active = false;
        if session.foreground_work {
            session.cancelled = true;
        }
        let foreground_work = session.foreground_work;
        drop(state);
        self.state_changed.notify_waiters();
        Ok(foreground_work)
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
        connection: V2ConnectionTo<Client>,
    ) -> Result<(), agent_client_protocol::Error> {
        let session_id = request.session_id.clone();
        match self.process_prompt_inner(request, &connection).await {
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
        connection: &V2ConnectionTo<Client>,
    ) -> Result<acp::StopReason, agent_client_protocol::Error> {
        let session_id = request.session_id;
        let user_message_id = self.next_message_id("user-message");
        let user_message = acp::UserMessage::new(user_message_id).content(request.prompt.clone());
        let user_update = acp::SessionUpdate::UserMessage(user_message);
        send_update(connection, &session_id, user_update.clone())?;
        self.record_history(&session_id, user_update);

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
            TestyCommand::RunScenario { scenario } => (
                format!(
                    "Testy v2 scenario `{}` is not implemented yet",
                    scenario.name()
                ),
                acp::StopReason::Refusal,
            ),
            TestyCommand::CallTool { .. } | TestyCommand::ListTools { .. } => (
                "Testy v2 does not advertise MCP support yet".to_string(),
                acp::StopReason::Refusal,
            ),
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
                        let session_id = agent.create_session(request);
                        responder.respond(acp::NewSessionResponse::new(session_id))
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
                        let has_foreground_work = match agent.close_session(&request.session_id) {
                            Ok(has_foreground_work) => has_foreground_work,
                            Err(error) => return responder.respond_with_error(error),
                        };
                        if !has_foreground_work {
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
                        let session_id = request.session_id.clone();
                        if let Err(error) = agent.begin_prompt(&session_id) {
                            return responder.respond_with_error(error);
                        }

                        responder.respond(acp::PromptResponse::new())?;
                        let prompt_connection = connection.clone();
                        let spawn_result = connection.spawn({
                            let agent = agent.clone();
                            async move { agent.process_prompt(request, prompt_connection).await }
                        });
                        if spawn_result.is_err() {
                            agent.abandon_prompt(&session_id);
                        }
                        spawn_result
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
