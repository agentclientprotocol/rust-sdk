use std::path::Path;

use crate::{
    Agent, ConnectionTo, SentRequest,
    role::HasPeer,
    schema::{ProtocolVersion, v2},
};

use super::ensure_session_protocol;

impl<Counterpart> ConnectionTo<Counterpart>
where
    Counterpart: HasPeer<Agent>,
{
    /// Build a draft protocol v2 `session/new` request.
    ///
    /// This is the v2 counterpart to [`ConnectionTo::build_session`]. The
    /// unversioned helper remains the stable protocol v1 API.
    pub fn build_v2_session(&self, cwd: impl AsRef<Path>) -> V2SessionBuilder<Counterpart> {
        V2SessionBuilder::new(self, v2::NewSessionRequest::new(cwd.as_ref()))
    }

    /// Build a draft protocol v2 session using the current working directory.
    ///
    /// Returns an error if the current directory cannot be determined.
    pub fn build_v2_session_cwd(&self) -> Result<V2SessionBuilder<Counterpart>, crate::Error> {
        let cwd = std::env::current_dir().map_err(|error| {
            crate::Error::internal_error().data(format!("cannot get current directory: {error}"))
        })?;
        Ok(self.build_v2_session(cwd))
    }

    /// Build a draft protocol v2 session from an existing `session/new` request.
    pub fn build_v2_session_from(
        &self,
        request: v2::NewSessionRequest,
    ) -> V2SessionBuilder<Counterpart> {
        V2SessionBuilder::new(self, request)
    }

    /// Resume a draft protocol v2 session.
    ///
    /// Use [`Self::resume_v2_session_from`] to request history replay or set
    /// other optional resume parameters.
    pub fn resume_v2_session(
        &self,
        session_id: impl Into<v2::SessionId>,
        cwd: impl AsRef<Path>,
    ) -> Result<SentRequest<OpenedV2Session<Counterpart, v2::ResumeSessionResponse>>, crate::Error>
    {
        self.resume_v2_session_from(v2::ResumeSessionRequest::new(session_id, cwd.as_ref()))
    }

    /// Resume a draft protocol v2 session from an existing request.
    ///
    /// Register typed session update handlers before connecting. When the
    /// request asks for replay, the agent sends those updates before the
    /// [`v2::ResumeSessionResponse`].
    pub fn resume_v2_session_from(
        &self,
        request: v2::ResumeSessionRequest,
    ) -> Result<SentRequest<OpenedV2Session<Counterpart, v2::ResumeSessionResponse>>, crate::Error>
    {
        ensure_session_protocol(
            self,
            ProtocolVersion::V2,
            "resume_v2_session",
            "send a v1 `ResumeSessionRequest` with `ConnectionTo::send_request` instead",
        )?;

        let session_id = request.session_id.clone();
        let session_connection = self.clone();

        Ok(self.send_request_to(Agent, request).map(move |response| {
            let session = V2Session {
                session_id,
                connection: session_connection,
            };
            Ok(OpenedV2Session { session, response })
        }))
    }
}

/// Builder for a draft protocol v2 `session/new` request.
///
/// Protocol v2 acknowledges `session/prompt` independently from inbound
/// session updates. Register typed [`v2::UpdateSessionNotification`] and
/// session request handlers on [`crate::Builder`] before connecting, then use
/// [`Self::start_session`] to create the command-only [`V2Session`] handle.
///
/// Per-session MCP attachment and proxy-session helpers are currently available
/// only through the stable protocol v1 [`crate::SessionBuilder`].
#[must_use = "call `start_session` to send the `session/new` request"]
#[derive(Debug)]
pub struct V2SessionBuilder<Counterpart>
where
    Counterpart: HasPeer<Agent>,
{
    connection: ConnectionTo<Counterpart>,
    request: v2::NewSessionRequest,
}

impl<Counterpart> V2SessionBuilder<Counterpart>
where
    Counterpart: HasPeer<Agent>,
{
    fn new(connection: &ConnectionTo<Counterpart>, request: v2::NewSessionRequest) -> Self {
        Self {
            connection: connection.clone(),
            request,
        }
    }

    /// Send `session/new` and return its independently consumable request.
    ///
    /// The successful result contains both a cloneable command handle and the
    /// complete [`v2::NewSessionResponse`]. Consume the returned request with
    /// [`SentRequest::block_task`], [`SentRequest::on_receiving_result`], or
    /// another explicit [`SentRequest`] completion mode.
    pub fn start_session(
        self,
    ) -> Result<SentRequest<OpenedV2Session<Counterpart, v2::NewSessionResponse>>, crate::Error>
    {
        ensure_session_protocol(
            &self.connection,
            ProtocolVersion::V2,
            "build_v2_session",
            "use `build_session` instead",
        )?;

        let Self {
            connection,
            request,
        } = self;
        let session_connection = connection.clone();

        Ok(connection
            .send_request_to(Agent, request)
            .map(move |response| {
                let session = V2Session {
                    session_id: response.session_id.clone(),
                    connection: session_connection,
                };
                Ok(OpenedV2Session { session, response })
            }))
    }
}

/// A newly available protocol v2 session and its operation-specific response.
///
/// Keeping the response separate from [`V2Session`] avoids treating
/// `session/new` setup data as mutable session state and lets each setup
/// operation, including `session/resume`, retain its own complete response
/// type.
#[derive(Debug)]
pub struct OpenedV2Session<Link, Response>
where
    Link: HasPeer<Agent>,
{
    session: V2Session<Link>,
    response: Response,
}

impl<Link, Response> OpenedV2Session<Link, Response>
where
    Link: HasPeer<Agent>,
{
    /// Access the command handle for the opened session.
    pub fn session(&self) -> &V2Session<Link> {
        &self.session
    }

    /// Access the complete response from the operation that opened the session.
    pub fn response(&self) -> &Response {
        &self.response
    }

    /// Split this result into the command handle and complete setup response.
    pub fn into_parts(self) -> (V2Session<Link>, Response) {
        (self.session, self.response)
    }

    /// Consume this result and return only the command handle.
    pub fn into_session(self) -> V2Session<Link> {
        self.session
    }
}

/// Cloneable command handle for a draft protocol v2 session.
///
/// Inbound protocol traffic is intentionally not owned by this value. Receive
/// authoritative [`v2::UpdateSessionNotification`] values and interactive
/// requests such as [`v2::RequestPermissionRequest`] through typed handlers
/// installed on [`crate::Builder`].
#[derive(Debug, Clone)]
pub struct V2Session<Link>
where
    Link: HasPeer<Agent>,
{
    session_id: v2::SessionId,
    connection: ConnectionTo<Link>,
}

impl<Link> V2Session<Link>
where
    Link: HasPeer<Agent>,
{
    /// Access the session ID.
    pub fn session_id(&self) -> &v2::SessionId {
        &self.session_id
    }

    /// Access the underlying connection.
    pub fn connection(&self) -> &ConnectionTo<Link> {
        &self.connection
    }

    /// Submit a text prompt and return its independent acceptance request.
    ///
    /// A successful response only acknowledges that the agent accepted the
    /// prompt. The accepted user message, output, state changes, and completion
    /// arrive independently through [`v2::UpdateSessionNotification`].
    pub fn send_prompt(&self, prompt: impl ToString) -> SentRequest<v2::PromptResponse> {
        self.send_prompt_blocks(vec![prompt.to_string().into()])
    }

    /// Submit arbitrary prompt content and return its acceptance request.
    ///
    /// The SDK does not track foreground state or gate prompt submission
    /// locally. Wait for an `idle` state update before another ordinary prompt
    /// unless using a separately defined admission mechanism.
    pub fn send_prompt_blocks(
        &self,
        prompt: Vec<v2::ContentBlock>,
    ) -> SentRequest<v2::PromptResponse> {
        self.connection.send_request_to(
            Agent,
            v2::PromptRequest::new(self.session_id.clone(), prompt),
        )
    }

    /// Ask the agent to cancel the session's current foreground work.
    ///
    /// This is independent from cancelling a prompt's [`SentRequest`].
    /// Cancellation completes when the agent reports an `idle` state update
    /// with [`v2::StopReason::Cancelled`]. The client should immediately mark
    /// unfinished tool calls for the active work as cancelled and remains
    /// responsible for resolving every pending [`v2::RequestPermissionRequest`]
    /// with the cancelled outcome.
    pub fn cancel_active_work(&self) -> Result<(), crate::Error> {
        self.connection.send_notification_to(
            Agent,
            v2::CancelSessionNotification::new(self.session_id.clone()),
        )
    }

    /// Set a session configuration option.
    ///
    /// The response contains the full current option set. It is not cached on
    /// this command handle.
    pub fn set_config_option(
        &self,
        config_id: impl Into<v2::SessionConfigId>,
        value: impl Into<v2::SessionConfigOptionValue>,
    ) -> SentRequest<v2::SetSessionConfigOptionResponse> {
        self.connection.send_request_to(
            Agent,
            v2::SetSessionConfigOptionRequest::new(self.session_id.clone(), config_id, value),
        )
    }

    /// Close the remote session and release its resources.
    ///
    /// Existing clones of this local command handle are not invalidated, but
    /// the agent should reject subsequent commands for the closed session.
    pub fn close(&self) -> SentRequest<v2::CloseSessionResponse> {
        self.connection
            .send_request_to(Agent, v2::CloseSessionRequest::new(self.session_id.clone()))
    }
}
