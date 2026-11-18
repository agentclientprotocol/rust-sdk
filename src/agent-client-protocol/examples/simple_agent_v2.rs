//! A small, complete ACP v2 agent that echoes prompts over stdio.
//!
//! The agent implements the baseline v2 session lifecycle: new, list, resume,
//! close, prompt, cancel, and update. It keeps session history in memory so a
//! client can request replay from the beginning when resuming a session.
//! A prompt sent while earlier work is still running is held and inserted
//! once that work ends, rather than rejected. A prompt sent again with the same
//! `idempotencyKey` is answered with the first one's message ID and inserts no
//! second user message.
//!
//! Run it with the companion `v2_one_shot_client` example. An ACP agent owns
//! stdout for JSON-RPC, so diagnostics belong on stderr.

use std::{
    collections::{HashMap, VecDeque},
    sync::{Arc, Mutex},
};

use agent_client_protocol::v2::{ConnectionTo, Responder, schema};
use agent_client_protocol::{Agent, Client, Error, RequestCancellation, Result, Stdio};
use tokio::sync::Notify;

const SESSION_TITLE: &str = "Echo agent session";

#[derive(Clone, Debug, Default)]
struct EchoAgent {
    state: Arc<Mutex<AgentState>>,
    state_changed: Arc<Notify>,
}

#[derive(Debug, Default)]
struct AgentState {
    sessions: HashMap<schema::SessionId, Session>,
    next_session_id: u64,
    next_message_id: u64,
    next_prompt_ticket: u64,
}

#[derive(Clone, Debug)]
struct Session {
    cwd: schema::AbsolutePath,
    active: bool,
    foreground_work: bool,
    cancelled: bool,
    /// Incremented by each `session/cancel` and `session/close`, which cancel
    /// every prompt held before them.
    cancel_epoch: u64,
    /// Prompts received during foreground work, held in received order.
    held_prompts: VecDeque<u64>,
    /// What the session has recorded for each `idempotencyKey`. A key lasts as
    /// long as the history that holds its message.
    idempotency_keys: HashMap<schema::IdempotencyKey, KeyedPrompt>,
    history: Vec<schema::SessionUpdate>,
}

/// What a session has recorded for an `idempotencyKey`.
#[derive(Clone, Debug)]
enum KeyedPrompt {
    /// A prompt with this key was accepted and its user message is not
    /// inserted yet.
    Pending,
    /// The user message that was inserted with this key.
    Inserted(schema::MessageId),
}

/// How an accepted `session/prompt` request proceeds.
enum PromptAdmission {
    /// The prompt starts foreground work now.
    Inserted,
    /// The prompt waits for the current foreground work to end.
    Held { ticket: u64, cancel_epoch: u64 },
    /// A retry of a prompt whose user message is already inserted. It gets
    /// that message's ID and starts no work.
    Retry(schema::MessageId),
    /// A retry of a prompt that is not inserted yet. It is not held a second
    /// time. It gets the outcome of the first request with this key.
    RetryOfPending(schema::IdempotencyKey),
}

impl EchoAgent {
    fn create_session(&self, request: schema::NewSessionRequest) -> Result<schema::SessionId> {
        require_absolute(&request.cwd)?;
        reject_mcp_servers(&request.mcp_servers)?;
        let mut state = self.state.lock().expect("session state lock poisoned");
        state.next_session_id += 1;
        let session_id = schema::SessionId::new(format!("echo-session-{}", state.next_session_id));
        state.sessions.insert(
            session_id.clone(),
            // `additionalDirectories` is not advertised, so it is ignored.
            Session {
                cwd: request.cwd,
                active: true,
                foreground_work: false,
                cancelled: false,
                cancel_epoch: 0,
                held_prompts: VecDeque::new(),
                idempotency_keys: HashMap::new(),
                history: Vec::new(),
            },
        );
        Ok(session_id)
    }

    fn list_sessions(&self, request: &schema::ListSessionsRequest) -> Vec<schema::SessionInfo> {
        let state = self.state.lock().expect("session state lock poisoned");
        let mut sessions = state
            .sessions
            .iter()
            .filter(|(_, session)| request.cwd.as_ref().is_none_or(|cwd| cwd == &session.cwd))
            .map(|(session_id, session)| {
                schema::SessionInfo::new(session_id.clone(), session.cwd.clone())
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

    /// Reattach to a session. Returns the history to replay when the client
    /// sent a `replayFrom` cursor, and `None` when it asked for no replay.
    fn resume_session(
        &self,
        request: &schema::ResumeSessionRequest,
    ) -> Result<Option<Vec<schema::SessionUpdate>>> {
        require_absolute(&request.cwd)?;
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
            None => None,
            Some(schema::ReplayFrom::Start(_)) => Some(session.history.clone()),
            Some(_) => return Err(invalid_params("unsupported replay cursor")),
        };
        session.active = true;
        session.cancelled = false;
        Ok(history)
    }

    /// Accept a prompt, starting it now or holding it behind active work.
    ///
    /// A v2 agent must not reject a prompt only because foreground work is
    /// active. It may hold the prompt and insert it later, in received order.
    ///
    /// An agent that advertises `capabilities.session` must support
    /// `idempotencyKey`. A request whose key the session has recorded is a
    /// retry: the agent never inserts a second user message for it. This
    /// agent does not compare the retry's prompt with the first one, which
    /// the protocol allows. A malformed key never reaches this handler,
    /// because the request fails to parse and the SDK answers `-32602`.
    fn admit_prompt(
        &self,
        session_id: &schema::SessionId,
        prompt: &[schema::ContentBlock],
        idempotency_key: Option<&schema::IdempotencyKey>,
    ) -> Result<PromptAdmission> {
        let mut state = self.state.lock().expect("session state lock poisoned");
        let ticket = state.next_prompt_ticket;
        let session = state
            .sessions
            .get_mut(session_id)
            .filter(|session| session.active)
            .ok_or_else(|| session_not_found(session_id))?;
        if let Some(key) = idempotency_key {
            match session.idempotency_keys.get(key) {
                Some(KeyedPrompt::Inserted(message_id)) => {
                    return Ok(PromptAdmission::Retry(message_id.clone()));
                }
                Some(KeyedPrompt::Pending) => {
                    return Ok(PromptAdmission::RetryOfPending(key.clone()));
                }
                None => {}
            }
        }
        // Clients must not send an empty prompt; reject one without inserting it.
        if prompt.is_empty() {
            return Err(invalid_params("prompt must not be empty"));
        }
        // No prompt capabilities are advertised, so only the baseline `text` and
        // `resource_link` blocks are supported. Anything else must be rejected.
        if prompt.iter().any(|block| {
            !matches!(
                block,
                schema::ContentBlock::Text(_) | schema::ContentBlock::ResourceLink(_)
            )
        }) {
            return Err(invalid_params("unsupported prompt content block"));
        }
        // A rejected prompt records nothing, so its key stays unused. From
        // here on the prompt is accepted.
        if let Some(key) = idempotency_key {
            session
                .idempotency_keys
                .insert(key.clone(), KeyedPrompt::Pending);
        }
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

    /// Wait until a held prompt can start. `session/cancel` and
    /// `session/close` drop a held prompt with `-32800`. This agent also
    /// honors `$/cancel_request` for it, which is optional. A dropped prompt
    /// leaves its `idempotencyKey` unused, and pending retries with that key
    /// get `-32800` too.
    async fn insert_held_prompt(
        &self,
        session_id: &schema::SessionId,
        ticket: u64,
        cancel_epoch: u64,
        idempotency_key: Option<&schema::IdempotencyKey>,
        withdrawn: RequestCancellation,
    ) -> Result<()> {
        loop {
            let notified = self.state_changed.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            {
                let mut state = self.state.lock().expect("session state lock poisoned");
                let Some(session) = state.sessions.get_mut(session_id) else {
                    return Err(Error::request_cancelled());
                };
                if !session.active
                    || session.cancel_epoch != cancel_epoch
                    || withdrawn.is_cancelled()
                {
                    session.held_prompts.retain(|held| *held != ticket);
                    if let Some(key) = idempotency_key {
                        session.idempotency_keys.remove(key);
                    }
                    drop(state);
                    self.state_changed.notify_waiters();
                    return Err(Error::request_cancelled());
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

    /// Wait for the outcome of the pending prompt that first carried
    /// `idempotency_key`, and return it for a retry of that prompt: the first
    /// prompt's message ID once its user message is inserted, or `-32800`
    /// when that prompt is dropped before insertion.
    async fn wait_for_pending_prompt(
        &self,
        session_id: &schema::SessionId,
        idempotency_key: &schema::IdempotencyKey,
    ) -> Result<schema::MessageId> {
        let notified = self.state_changed.notified();
        tokio::pin!(notified);
        loop {
            notified.as_mut().enable();
            let recorded = self
                .state
                .lock()
                .expect("session state lock poisoned")
                .sessions
                .get(session_id)
                .and_then(|session| session.idempotency_keys.get(idempotency_key))
                .cloned();
            match recorded {
                Some(KeyedPrompt::Inserted(message_id)) => return Ok(message_id),
                Some(KeyedPrompt::Pending) => {}
                None => return Err(Error::request_cancelled()),
            }
            notified.as_mut().await;
            notified.set(self.state_changed.notified());
        }
    }

    /// Insert the user message for a prompt that holds the foreground work,
    /// acknowledge it with its message ID, and run it.
    fn run_inserted_prompt(
        &self,
        request: schema::PromptRequest,
        responder: Responder<schema::PromptResponse>,
        connection: &ConnectionTo<Client>,
    ) -> Result<()> {
        let user_message_id = self.next_message_id("user-message");
        let user_message = schema::SessionUpdate::UserMessage(
            schema::UserMessage::new(user_message_id.clone()).content(request.prompt.clone()),
        );
        self.record_user_message(
            &request.session_id,
            &user_message_id,
            request.idempotency_key.clone(),
            user_message.clone(),
        );
        let session_id = request.session_id.clone();
        if let Err(error) = responder.respond(schema::PromptResponse::new(user_message_id)) {
            self.abandon_prompt(&session_id);
            return Err(error);
        }

        let prompt_connection = connection.clone();
        if let Err(error) = connection.spawn({
            let agent = self.clone();
            async move {
                agent
                    .process_prompt(request, user_message, prompt_connection)
                    .await
            }
        }) {
            self.abandon_prompt(&session_id);
            return Err(error);
        }
        Ok(())
    }

    fn finish_prompt(
        &self,
        session_id: &schema::SessionId,
        connection: &ConnectionTo<Client>,
    ) -> Result<()> {
        let mut state = self.state.lock().expect("session state lock poisoned");
        let session = state
            .sessions
            .get_mut(session_id)
            .ok_or_else(|| session_not_found(session_id))?;
        let stop_reason = if session.cancelled || !session.active {
            schema::StopReason::Cancelled
        } else {
            schema::StopReason::EndTurn
        };
        send_update(
            connection,
            session_id,
            schema::SessionUpdate::StateUpdate(schema::StateUpdate::Idle(
                schema::IdleStateUpdate::new().stop_reason(stop_reason),
            )),
        )?;
        session.foreground_work = false;
        session.cancelled = false;
        drop(state);
        self.state_changed.notify_waiters();
        Ok(())
    }

    fn abandon_prompt(&self, session_id: &schema::SessionId) {
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

    fn next_message_id(&self, kind: &str) -> schema::MessageId {
        let mut state = self.state.lock().expect("session state lock poisoned");
        state.next_message_id += 1;
        schema::MessageId::new(format!("{kind}-{}", state.next_message_id))
    }

    fn record_history(&self, session_id: &schema::SessionId, update: schema::SessionUpdate) {
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

    /// Record an inserted user message, and its `idempotencyKey` in the same
    /// step, so a retry with that key finds the message. The key is not
    /// echoed in any update, response, or replayed history.
    fn record_user_message(
        &self,
        session_id: &schema::SessionId,
        message_id: &schema::MessageId,
        idempotency_key: Option<schema::IdempotencyKey>,
        update: schema::SessionUpdate,
    ) {
        if let Some(session) = self
            .state
            .lock()
            .expect("session state lock poisoned")
            .sessions
            .get_mut(session_id)
        {
            session.history.push(update);
            if let Some(key) = idempotency_key {
                session
                    .idempotency_keys
                    .insert(key, KeyedPrompt::Inserted(message_id.clone()));
            }
        }
        // Wake retries that wait for this prompt's outcome.
        self.state_changed.notify_waiters();
    }

    fn is_cancelled(&self, session_id: &schema::SessionId) -> bool {
        self.state
            .lock()
            .expect("session state lock poisoned")
            .sessions
            .get(session_id)
            .is_none_or(|session| session.cancelled || !session.active)
    }

    /// `session/cancel` cancels foreground work and every prompt held before
    /// it. It is ignored for a session that is not active.
    fn cancel(&self, session_id: &schema::SessionId) {
        if let Some(session) = self
            .state
            .lock()
            .expect("session state lock poisoned")
            .sessions
            .get_mut(session_id)
            .filter(|session| session.active)
        {
            session.cancel_epoch += 1;
            if session.foreground_work {
                session.cancelled = true;
            }
        }
        self.state_changed.notify_waiters();
    }

    /// Close a session and report whether foreground work must finish before
    /// the close response. Closing is idempotent: an unknown or already-closed
    /// session closes successfully.
    fn close(&self, session_id: &schema::SessionId) -> bool {
        let mut state = self.state.lock().expect("session state lock poisoned");
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

    fn has_foreground_work(&self, session_id: &schema::SessionId) -> bool {
        self.state
            .lock()
            .expect("session state lock poisoned")
            .sessions
            .get(session_id)
            .is_some_and(|session| session.foreground_work)
    }

    async fn wait_for_foreground_work(&self, session_id: &schema::SessionId) {
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
        request: schema::PromptRequest,
        user_message: schema::SessionUpdate,
        connection: ConnectionTo<Client>,
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
        request: schema::PromptRequest,
        user_message: schema::SessionUpdate,
        connection: &ConnectionTo<Client>,
    ) -> Result<()> {
        let session_id = request.session_id;
        send_update(connection, &session_id, user_message)?;

        send_update(
            connection,
            &session_id,
            schema::SessionUpdate::StateUpdate(schema::StateUpdate::Running(
                schema::RunningStateUpdate::new(),
            )),
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
                schema::ContentBlock::Text(text) => Some(text.text.as_str()),
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
            schema::SessionUpdate::AgentMessageChunk(schema::ContentChunk::new(
                response.clone().into(),
                message_id.clone(),
            )),
        )?;
        // A complete snapshot replaces the content accumulated from chunks
        // with the same message ID; clients must not render it a second time.
        let agent_message = schema::SessionUpdate::AgentMessage(
            schema::AgentMessage::new(message_id).content(vec![response.into()]),
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
            async |request: schema::InitializeRequest,
                   responder: Responder<schema::InitializeResponse>,
                   _connection: ConnectionTo<Client>| {
                responder.respond(
                    schema::InitializeResponse::new(
                        request.protocol_version,
                        schema::Implementation::new("simple-agent-v2", env!("CARGO_PKG_VERSION")),
                    )
                    .capabilities(
                        schema::AgentCapabilities::new()
                            .session(schema::SessionCapabilities::new()),
                    ),
                )
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            {
                let agent = agent.clone();
                async move |request: schema::NewSessionRequest,
                            responder: Responder<schema::NewSessionResponse>,
                            connection: ConnectionTo<Client>| {
                    let session_id = match agent.create_session(request) {
                        Ok(session_id) => session_id,
                        Err(error) => return responder.respond_with_error(error),
                    };
                    responder.respond(schema::NewSessionResponse::new(session_id.clone()))?;
                    // A client can already have this ready-state update queued
                    // when a later prompt response arrives. Prompt completion
                    // must wait for running and the subsequent idle instead.
                    send_update(
                        &connection,
                        &session_id,
                        schema::SessionUpdate::StateUpdate(schema::StateUpdate::Idle(
                            schema::IdleStateUpdate::new(),
                        )),
                    )
                }
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            {
                let agent = agent.clone();
                async move |request: schema::ListSessionsRequest,
                            responder: Responder<schema::ListSessionsResponse>,
                            _connection: ConnectionTo<Client>| {
                    // Every session fits in one page and no `nextCursor` is ever
                    // issued, so any cursor a client sends is invalid.
                    if let Some(cursor) = &request.cursor {
                        return responder.respond_with_error(invalid_params(format!(
                            "invalid session/list cursor `{cursor}`"
                        )));
                    }
                    if let Some(cwd) = &request.cwd
                        && let Err(error) = require_absolute(cwd)
                    {
                        return responder.respond_with_error(error);
                    }
                    responder.respond(schema::ListSessionsResponse::new(
                        agent.list_sessions(&request),
                    ))
                }
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            {
                let agent = agent.clone();
                async move |request: schema::ResumeSessionRequest,
                            responder: Responder<schema::ResumeSessionResponse>,
                            connection: ConnectionTo<Client>| {
                    let history = match agent.resume_session(&request) {
                        Ok(history) => history,
                        Err(error) => return responder.respond_with_error(error),
                    };
                    // A resume with a `replayFrom` cursor ends its replay with
                    // `replay_complete`, also when the history is empty. A
                    // resume without a cursor must not send it.
                    if let Some(history) = history {
                        for update in history {
                            send_update(&connection, &request.session_id, update)?;
                        }
                        send_update(
                            &connection,
                            &request.session_id,
                            schema::SessionUpdate::ReplayComplete(schema::ReplayComplete::new()),
                        )?;
                    }
                    // This example only resumes sessions without foreground work.
                    // Every successful resume synchronizes current state, with or
                    // without history replay, before its response.
                    send_update(
                        &connection,
                        &request.session_id,
                        schema::SessionUpdate::SessionInfoUpdate(
                            schema::SessionInfoUpdate::new().title(SESSION_TITLE),
                        ),
                    )?;
                    send_update(
                        &connection,
                        &request.session_id,
                        schema::SessionUpdate::StateUpdate(schema::StateUpdate::Idle(
                            schema::IdleStateUpdate::new(),
                        )),
                    )?;
                    responder.respond(schema::ResumeSessionResponse::new())
                }
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            {
                let agent = agent.clone();
                async move |request: schema::CloseSessionRequest,
                            responder: Responder<schema::CloseSessionResponse>,
                            connection: ConnectionTo<Client>| {
                    if !agent.close(&request.session_id) {
                        return responder.respond(schema::CloseSessionResponse::new());
                    }
                    let waiting_agent = agent.clone();
                    connection.spawn(async move {
                        waiting_agent
                            .wait_for_foreground_work(&request.session_id)
                            .await;
                        responder.respond(schema::CloseSessionResponse::new())
                    })
                }
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            {
                let agent = agent.clone();
                async move |request: schema::PromptRequest,
                            responder: Responder<schema::PromptResponse>,
                            connection: ConnectionTo<Client>| {
                    match agent.admit_prompt(
                        &request.session_id,
                        &request.prompt,
                        request.idempotency_key.as_ref(),
                    ) {
                        Err(error) => responder.respond_with_error(error),
                        Ok(PromptAdmission::Inserted) => {
                            agent.run_inserted_prompt(request, responder, &connection)
                        }
                        // A retry starts no second round of work.
                        Ok(PromptAdmission::Retry(message_id)) => {
                            responder.respond(schema::PromptResponse::new(message_id))
                        }
                        Ok(PromptAdmission::RetryOfPending(idempotency_key)) => {
                            let agent = agent.clone();
                            connection.spawn(async move {
                                let outcome = agent
                                    .wait_for_pending_prompt(&request.session_id, &idempotency_key)
                                    .await;
                                responder
                                    .respond_with_result(outcome.map(schema::PromptResponse::new))
                            })
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
                                        request.idempotency_key.as_ref(),
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
            async move |notification: schema::CancelSessionNotification,
                        _connection: ConnectionTo<Client>| {
                agent.cancel(&notification.session_id);
                Ok(())
            },
            agent_client_protocol::on_receive_notification!(),
        )
        .connect_to(Stdio::new())
        .await
}

fn send_update(
    connection: &ConnectionTo<Client>,
    session_id: &schema::SessionId,
    update: schema::SessionUpdate,
) -> Result<()> {
    connection.send_notification(schema::UpdateSessionNotification::new(
        session_id.clone(),
        update,
    ))
}

fn invalid_params(message: impl ToString) -> Error {
    Error::invalid_params().data(message.to_string())
}

/// Requests naming a session that does not exist or is not active on this
/// connection fail with `-32002` and `data.sessionId`.
fn session_not_found(session_id: &schema::SessionId) -> Error {
    schema::Error::session_not_found(session_id.clone()).into()
}

/// `cwd` must be an absolute path in the native syntax of the agent's machine;
/// a relative one is rejected with `-32602`.
fn require_absolute(cwd: &schema::AbsolutePath) -> Result<()> {
    if AsRef::<std::path::Path>::as_ref(cwd).is_absolute() {
        Ok(())
    } else {
        Err(invalid_params("`cwd` must be an absolute path"))
    }
}

/// This agent advertises no `session.mcp` transports, so it must reject every
/// MCP server with `-32602` rather than silently ignoring it.
fn reject_mcp_servers(mcp_servers: &[schema::McpServer]) -> Result<()> {
    if mcp_servers.is_empty() {
        Ok(())
    } else {
        Err(invalid_params(
            "this agent does not advertise any MCP server transports",
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_empty_prompt_without_inserting_it() {
        let agent = EchoAgent::default();
        let session_id = agent
            .create_session(schema::NewSessionRequest::new("/tmp"))
            .expect("session should be created");

        let Err(error) = agent.admit_prompt(&session_id, &[], None) else {
            panic!("an empty prompt should be rejected");
        };
        assert_eq!(error.code, agent_client_protocol::ErrorCode::INVALID_PARAMS);
        {
            let state = agent.state.lock().expect("session state lock poisoned");
            let session = &state.sessions[&session_id];
            assert!(!session.foreground_work, "no work should start");
            assert!(session.held_prompts.is_empty(), "nothing should be held");
            assert!(session.history.is_empty(), "nothing should be inserted");
        }

        assert!(matches!(
            agent.admit_prompt(&session_id, &[schema::ContentBlock::from("hello")], None),
            Ok(PromptAdmission::Inserted)
        ));
    }

    #[test]
    fn rejected_prompt_leaves_its_idempotency_key_unused() {
        let agent = EchoAgent::default();
        let session_id = agent
            .create_session(schema::NewSessionRequest::new("/tmp"))
            .expect("session should be created");
        let key = schema::IdempotencyKey::generate();

        assert!(agent.admit_prompt(&session_id, &[], Some(&key)).is_err());
        // The same key then starts the prompt as a new submission.
        assert!(matches!(
            agent.admit_prompt(
                &session_id,
                &[schema::ContentBlock::from("hello")],
                Some(&key)
            ),
            Ok(PromptAdmission::Inserted)
        ));
        // A second request with that key is a retry and is not held.
        assert!(matches!(
            agent.admit_prompt(
                &session_id,
                &[schema::ContentBlock::from("hello")],
                Some(&key)
            ),
            Ok(PromptAdmission::RetryOfPending(pending)) if pending == key
        ));
    }

    #[test]
    fn retry_gets_the_inserted_message_id_and_inserts_nothing() {
        let agent = EchoAgent::default();
        let session_id = agent
            .create_session(schema::NewSessionRequest::new("/tmp"))
            .expect("session should be created");
        let key = schema::IdempotencyKey::generate();
        let message_id = schema::MessageId::new("user-message-1");
        let user_message = schema::SessionUpdate::UserMessage(
            schema::UserMessage::new(message_id.clone())
                .content(vec![schema::ContentBlock::from("hello")]),
        );
        agent.record_user_message(&session_id, &message_id, Some(key.clone()), user_message);

        for _ in 0..2 {
            assert!(matches!(
                agent.admit_prompt(
                    &session_id,
                    &[schema::ContentBlock::from("hello")],
                    Some(&key)
                ),
                Ok(PromptAdmission::Retry(retried)) if retried == message_id
            ));
        }
        {
            let state = agent.state.lock().expect("session state lock poisoned");
            let session = &state.sessions[&session_id];
            assert_eq!(session.history.len(), 1, "a retry inserts no message");
            assert!(!session.foreground_work, "a retry starts no work");
        }
        // Another key with the same text is a new submission.
        let other = schema::IdempotencyKey::generate();
        assert!(matches!(
            agent.admit_prompt(
                &session_id,
                &[schema::ContentBlock::from("hello")],
                Some(&other)
            ),
            Ok(PromptAdmission::Inserted)
        ));
    }
}
