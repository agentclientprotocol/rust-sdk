//! A small, complete ACP v2 agent that echoes prompts over stdio.
//!
//! The agent implements the baseline v2 session lifecycle: new, list, resume,
//! close, prompt, cancel, and update. It keeps session history in memory so a
//! client can request replay from the beginning when resuming a session.
//!
//! Run it with the companion `v2_one_shot_client` example. An ACP agent owns
//! stdout for JSON-RPC, so diagnostics belong on stderr.

use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
};

use agent_client_protocol::schema::v2;
use agent_client_protocol::{Agent, Client, Error, Responder, Result, Stdio, V2ConnectionTo};
use tokio::sync::Notify;

const SESSION_TITLE: &str = "Echo agent session";

#[derive(Clone, Debug, Default)]
struct EchoAgent {
    state: Arc<Mutex<AgentState>>,
    state_changed: Arc<Notify>,
}

#[derive(Debug, Default)]
struct AgentState {
    sessions: HashMap<v2::SessionId, Session>,
    next_session_id: u64,
    next_message_id: u64,
}

#[derive(Clone, Debug)]
struct Session {
    cwd: v2::AbsolutePath,
    additional_directories: Vec<v2::AbsolutePath>,
    active: bool,
    foreground_work: bool,
    cancelled: bool,
    history: Vec<v2::SessionUpdate>,
}

impl EchoAgent {
    fn create_session(&self, request: v2::NewSessionRequest) -> Result<v2::SessionId> {
        reject_mcp_servers(&request.mcp_servers)?;
        let mut state = self.state.lock().expect("session state lock poisoned");
        state.next_session_id += 1;
        let session_id = v2::SessionId::new(format!("echo-session-{}", state.next_session_id));
        state.sessions.insert(
            session_id.clone(),
            Session {
                cwd: request.cwd,
                additional_directories: request.additional_directories,
                active: true,
                foreground_work: false,
                cancelled: false,
                history: Vec::new(),
            },
        );
        Ok(session_id)
    }

    fn list_sessions(&self, request: &v2::ListSessionsRequest) -> Vec<v2::SessionInfo> {
        let state = self.state.lock().expect("session state lock poisoned");
        let mut sessions = state
            .sessions
            .iter()
            .filter(|(_, session)| request.cwd.as_ref().is_none_or(|cwd| cwd == &session.cwd))
            .map(|(session_id, session)| {
                v2::SessionInfo::new(session_id.clone(), session.cwd.clone())
                    .additional_directories(session.additional_directories.clone())
                    .title(SESSION_TITLE)
            })
            .collect::<Vec<_>>();
        sessions.sort_by(|left, right| {
            left.session_id
                .to_string()
                .cmp(&right.session_id.to_string())
        });
        sessions
    }

    fn resume_session(&self, request: &v2::ResumeSessionRequest) -> Result<Vec<v2::SessionUpdate>> {
        reject_mcp_servers(&request.mcp_servers)?;
        let mut state = self.state.lock().expect("session state lock poisoned");
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

        let history = match &request.replay_from {
            None => Vec::new(),
            Some(v2::ReplayFrom::Start(_)) => session.history.clone(),
            Some(_) => return Err(invalid_params("unsupported replay cursor")),
        };
        session
            .additional_directories
            .clone_from(&request.additional_directories);
        session.active = true;
        session.cancelled = false;
        Ok(history)
    }

    fn begin_prompt(&self, session_id: &v2::SessionId, prompt: &[v2::ContentBlock]) -> Result<()> {
        let mut state = self.state.lock().expect("session state lock poisoned");
        let session = state
            .sessions
            .get_mut(session_id)
            .filter(|session| session.active)
            .ok_or_else(|| session_not_found(session_id))?;
        if session.foreground_work {
            return Err(invalid_params(format!(
                "session `{session_id}` already has foreground work"
            )));
        }
        // No prompt capabilities are advertised, so only the baseline `text` and
        // `resource_link` blocks are supported. Anything else must be rejected.
        if prompt.iter().any(|block| {
            !matches!(
                block,
                v2::ContentBlock::Text(_) | v2::ContentBlock::ResourceLink(_)
            )
        }) {
            return Err(invalid_params("unsupported prompt content block"));
        }
        session.foreground_work = true;
        session.cancelled = false;
        Ok(())
    }

    fn finish_prompt(
        &self,
        session_id: &v2::SessionId,
        connection: &V2ConnectionTo<Client>,
    ) -> Result<()> {
        let mut state = self.state.lock().expect("session state lock poisoned");
        let session = state
            .sessions
            .get_mut(session_id)
            .ok_or_else(|| session_not_found(session_id))?;
        let stop_reason = if session.cancelled || !session.active {
            v2::StopReason::Cancelled
        } else {
            v2::StopReason::EndTurn
        };
        send_update(
            connection,
            session_id,
            v2::SessionUpdate::StateUpdate(v2::StateUpdate::Idle(
                v2::IdleStateUpdate::new().stop_reason(stop_reason),
            )),
        )?;
        session.foreground_work = false;
        session.cancelled = false;
        drop(state);
        self.state_changed.notify_waiters();
        Ok(())
    }

    fn abandon_prompt(&self, session_id: &v2::SessionId) {
        if let Some(session) = self
            .state
            .lock()
            .expect("session state lock poisoned")
            .sessions
            .get_mut(session_id)
        {
            session.foreground_work = false;
            session.cancelled = false;
        }
        self.state_changed.notify_waiters();
    }

    fn next_message_id(&self, kind: &str) -> v2::MessageId {
        let mut state = self.state.lock().expect("session state lock poisoned");
        state.next_message_id += 1;
        v2::MessageId::new(format!("{kind}-{}", state.next_message_id))
    }

    fn record_history(&self, session_id: &v2::SessionId, update: v2::SessionUpdate) {
        if let Some(session) = self
            .state
            .lock()
            .expect("session state lock poisoned")
            .sessions
            .get_mut(session_id)
        {
            session.history.push(update);
        }
    }

    fn is_cancelled(&self, session_id: &v2::SessionId) -> bool {
        self.state
            .lock()
            .expect("session state lock poisoned")
            .sessions
            .get(session_id)
            .is_none_or(|session| session.cancelled || !session.active)
    }

    fn cancel(&self, session_id: &v2::SessionId) {
        if let Some(session) = self
            .state
            .lock()
            .expect("session state lock poisoned")
            .sessions
            .get_mut(session_id)
            && session.foreground_work
        {
            session.cancelled = true;
        }
        self.state_changed.notify_waiters();
    }

    fn close(&self, session_id: &v2::SessionId) -> Result<bool> {
        let mut state = self.state.lock().expect("session state lock poisoned");
        let session = state
            .sessions
            .get_mut(session_id)
            .filter(|session| session.active)
            .ok_or_else(|| session_not_found(session_id))?;
        session.active = false;
        if session.foreground_work {
            session.cancelled = true;
        }
        let foreground_work = session.foreground_work;
        drop(state);
        self.state_changed.notify_waiters();
        Ok(foreground_work)
    }

    fn has_foreground_work(&self, session_id: &v2::SessionId) -> bool {
        self.state
            .lock()
            .expect("session state lock poisoned")
            .sessions
            .get(session_id)
            .is_some_and(|session| session.foreground_work)
    }

    async fn wait_for_foreground_work(&self, session_id: &v2::SessionId) {
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

    async fn process_prompt(
        &self,
        request: v2::PromptRequest,
        user_message: v2::SessionUpdate,
        connection: V2ConnectionTo<Client>,
    ) -> Result<()> {
        let session_id = request.session_id.clone();
        let result = self
            .process_prompt_inner(request, user_message, &connection)
            .await;
        match result {
            Ok(()) => self.finish_prompt(&session_id, &connection),
            Err(error) => {
                self.abandon_prompt(&session_id);
                Err(error)
            }
        }
    }

    async fn process_prompt_inner(
        &self,
        request: v2::PromptRequest,
        user_message: v2::SessionUpdate,
        connection: &V2ConnectionTo<Client>,
    ) -> Result<()> {
        let session_id = request.session_id;
        send_update(connection, &session_id, user_message)?;

        send_update(
            connection,
            &session_id,
            v2::SessionUpdate::StateUpdate(v2::StateUpdate::Running(v2::RunningStateUpdate::new())),
        )?;

        // Prompt acceptance is independent from this work. Yield once so the
        // client can observe the response before output starts arriving.
        tokio::task::yield_now().await;
        if self.is_cancelled(&session_id) {
            return Ok(());
        }

        let prompt = request
            .prompt
            .iter()
            .filter_map(|block| match block {
                v2::ContentBlock::Text(text) => Some(text.text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join(" ");
        let response = if prompt.is_empty() {
            "I received a prompt without text content.".to_string()
        } else {
            format!("Echo: {prompt}")
        };
        let message_id = self.next_message_id("agent-message");
        send_update(
            connection,
            &session_id,
            v2::SessionUpdate::AgentMessageChunk(v2::ContentChunk::new(
                response.clone().into(),
                message_id.clone(),
            )),
        )?;
        // A complete snapshot replaces the content accumulated from chunks
        // with the same message ID; clients must not render it a second time.
        let agent_message = v2::SessionUpdate::AgentMessage(
            v2::AgentMessage::new(message_id).content(vec![response.into()]),
        );
        send_update(connection, &session_id, agent_message.clone())?;
        self.record_history(&session_id, agent_message);
        Ok(())
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    let agent = EchoAgent::default();

    Agent
        .v2()
        .name("simple-agent-v2")
        .on_receive_request(
            async |request: v2::InitializeRequest,
                   responder: Responder<v2::InitializeResponse>,
                   _connection: V2ConnectionTo<Client>| {
                responder.respond(
                    v2::InitializeResponse::new(
                        request.protocol_version,
                        v2::Implementation::new("simple-agent-v2", env!("CARGO_PKG_VERSION")),
                    )
                    .capabilities(
                        v2::AgentCapabilities::new().session(v2::SessionCapabilities::new()),
                    ),
                )
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            {
                let agent = agent.clone();
                async move |request: v2::NewSessionRequest,
                            responder: Responder<v2::NewSessionResponse>,
                            connection: V2ConnectionTo<Client>| {
                    let session_id = match agent.create_session(request) {
                        Ok(session_id) => session_id,
                        Err(error) => return responder.respond_with_error(error),
                    };
                    responder.respond(v2::NewSessionResponse::new(session_id.clone()))?;
                    // A client can already have this ready-state update queued
                    // when a later prompt response arrives. Prompt completion
                    // must wait for running and the subsequent idle instead.
                    send_update(
                        &connection,
                        &session_id,
                        v2::SessionUpdate::StateUpdate(v2::StateUpdate::Idle(
                            v2::IdleStateUpdate::new(),
                        )),
                    )
                }
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            {
                let agent = agent.clone();
                async move |request: v2::ListSessionsRequest,
                            responder: Responder<v2::ListSessionsResponse>,
                            _connection: V2ConnectionTo<Client>| {
                    responder.respond(v2::ListSessionsResponse::new(agent.list_sessions(&request)))
                }
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            {
                let agent = agent.clone();
                async move |request: v2::ResumeSessionRequest,
                            responder: Responder<v2::ResumeSessionResponse>,
                            connection: V2ConnectionTo<Client>| {
                    let history = match agent.resume_session(&request) {
                        Ok(history) => history,
                        Err(error) => return responder.respond_with_error(error),
                    };
                    for update in history {
                        send_update(&connection, &request.session_id, update)?;
                    }
                    // This example only resumes sessions without foreground work.
                    // Every successful resume synchronizes current state, with or
                    // without history replay, before its response.
                    send_update(
                        &connection,
                        &request.session_id,
                        v2::SessionUpdate::SessionInfoUpdate(
                            v2::SessionInfoUpdate::new().title(SESSION_TITLE),
                        ),
                    )?;
                    send_update(
                        &connection,
                        &request.session_id,
                        v2::SessionUpdate::StateUpdate(v2::StateUpdate::Idle(
                            v2::IdleStateUpdate::new(),
                        )),
                    )?;
                    responder.respond(v2::ResumeSessionResponse::new())
                }
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            {
                let agent = agent.clone();
                async move |request: v2::CloseSessionRequest,
                            responder: Responder<v2::CloseSessionResponse>,
                            connection: V2ConnectionTo<Client>| {
                    let has_foreground_work = match agent.close(&request.session_id) {
                        Ok(has_foreground_work) => has_foreground_work,
                        Err(error) => return responder.respond_with_error(error),
                    };
                    if !has_foreground_work {
                        return responder.respond(v2::CloseSessionResponse::new());
                    }
                    let waiting_agent = agent.clone();
                    connection.spawn(async move {
                        waiting_agent
                            .wait_for_foreground_work(&request.session_id)
                            .await;
                        responder.respond(v2::CloseSessionResponse::new())
                    })
                }
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            {
                let agent = agent.clone();
                async move |request: v2::PromptRequest,
                            responder: Responder<v2::PromptResponse>,
                            connection: V2ConnectionTo<Client>| {
                    if let Err(error) = agent.begin_prompt(&request.session_id, &request.prompt) {
                        return responder.respond_with_error(error);
                    }
                    let user_message_id = agent.next_message_id("user-message");
                    let user_message = v2::SessionUpdate::UserMessage(
                        v2::UserMessage::new(user_message_id.clone())
                            .content(request.prompt.clone()),
                    );
                    agent.record_history(&request.session_id, user_message.clone());
                    responder.respond(v2::PromptResponse::new(user_message_id))?;

                    let prompt_connection = connection.clone();
                    let session_id = request.session_id.clone();
                    if let Err(error) = connection.spawn({
                        let agent = agent.clone();
                        async move {
                            agent
                                .process_prompt(request, user_message, prompt_connection)
                                .await
                        }
                    }) {
                        agent.abandon_prompt(&session_id);
                        return Err(error);
                    }
                    Ok(())
                }
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_notification(
            async move |notification: v2::CancelSessionNotification,
                        _connection: V2ConnectionTo<Client>| {
                agent.cancel(&notification.session_id);
                Ok(())
            },
            agent_client_protocol::on_receive_notification!(),
        )
        .connect_to(Stdio::new())
        .await
}

fn send_update(
    connection: &V2ConnectionTo<Client>,
    session_id: &v2::SessionId,
    update: v2::SessionUpdate,
) -> Result<()> {
    connection.send_notification(v2::UpdateSessionNotification::new(
        session_id.clone(),
        update,
    ))
}

fn invalid_params(message: impl ToString) -> Error {
    Error::invalid_params().data(message.to_string())
}

/// Requests naming a session that does not exist or is not active on this
/// connection fail with `-32002` and `data.sessionId`.
fn session_not_found(session_id: &v2::SessionId) -> Error {
    Error::session_not_found(session_id.0.clone())
}

/// This agent advertises no `session.mcp` transports, so it must reject every
/// MCP server with `-32602` rather than silently ignoring it.
fn reject_mcp_servers(mcp_servers: &[v2::McpServer]) -> Result<()> {
    if mcp_servers.is_empty() {
        Ok(())
    } else {
        Err(invalid_params(
            "this agent does not advertise any MCP server transports",
        ))
    }
}
