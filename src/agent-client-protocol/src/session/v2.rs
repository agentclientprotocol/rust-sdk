use std::path::Path;

#[cfg(feature = "unstable_mcp_over_acp")]
use futures::{
    channel::oneshot,
    future::{self, Either},
};

use crate::{
    Agent, DynamicHandlerGuard, SentRequest, V2ConnectionTo,
    jsonrpc::run::{NullRun, RunWithConnectionTo},
    role::HasPeer,
    schema::v2,
};

#[cfg(feature = "unstable_mcp_over_acp")]
use crate::{ConnectionTo, jsonrpc::run::ChainRun, mcp_server::McpServer};

#[cfg(feature = "unstable_mcp_over_acp")]
async fn run_pending_mcp_attachment<Counterpart, Run>(
    connection: ConnectionTo<Counterpart>,
    run: Run,
    started_tx: oneshot::Sender<Result<(), crate::Error>>,
    promotion_rx: oneshot::Receiver<()>,
) -> Result<(), crate::Error>
where
    Counterpart: HasPeer<Agent>,
    Run: RunWithConnectionTo<Counterpart>,
{
    let mut run = Box::pin(run.run_with_connection_to(connection));
    let first_poll =
        future::poll_fn(|cx| std::task::Poll::Ready(std::future::Future::poll(run.as_mut(), cx)))
            .await;
    let readiness = match &first_poll {
        std::task::Poll::Ready(result) => result.clone(),
        std::task::Poll::Pending => Ok(()),
    };
    drop(started_tx.send(readiness));

    match first_poll {
        std::task::Poll::Ready(Ok(())) => {
            let _ = promotion_rx.await;
            Ok(())
        }
        // The request has not been published yet, so report an immediate
        // startup failure through its readiness result without failing the
        // whole connection.
        std::task::Poll::Ready(Err(_)) => Ok(()),
        std::task::Poll::Pending => match future::select(run, promotion_rx).await {
            Either::Left((result, promotion_rx)) => {
                // A Pending first poll releases session/new for publication.
                // From that point onward the agent may already be using this
                // attachment, so runner failures are connection-fatal just as
                // they are for other connection runners.
                result?;
                let _ = promotion_rx.await;
                Ok(())
            }
            Either::Right((Ok(()), run)) => run.await,
            Either::Right((Err(_), _run)) => Ok(()),
        },
    }
}

impl<Counterpart> V2ConnectionTo<Counterpart>
where
    Counterpart: HasPeer<Agent>,
{
    /// Build a draft protocol v2 `session/new` request.
    pub fn build_session(&self, cwd: impl AsRef<Path>) -> V2SessionBuilder<Counterpart> {
        V2SessionBuilder::new(self, v2::NewSessionRequest::new(cwd.as_ref()))
    }

    /// Build a draft protocol v2 session using the current working directory.
    ///
    /// Returns an error if the current directory cannot be determined.
    pub fn build_session_cwd(&self) -> Result<V2SessionBuilder<Counterpart>, crate::Error> {
        let cwd = std::env::current_dir().map_err(|error| {
            crate::Error::internal_error().data(format!("cannot get current directory: {error}"))
        })?;
        Ok(self.build_session(cwd))
    }

    /// Build a draft protocol v2 session from an existing `session/new` request.
    pub fn build_session_from(
        &self,
        request: v2::NewSessionRequest,
    ) -> V2SessionBuilder<Counterpart> {
        V2SessionBuilder::new(self, request)
    }

    /// Resume a draft protocol v2 session.
    ///
    /// Use [`Self::resume_session_from`] to request history replay or set
    /// other optional resume parameters.
    pub fn resume_session(
        &self,
        session_id: impl Into<v2::SessionId>,
        cwd: impl AsRef<Path>,
    ) -> SentRequest<OpenedV2Session<Counterpart, v2::ResumeSessionResponse>> {
        self.resume_session_from(v2::ResumeSessionRequest::new(session_id, cwd.as_ref()))
    }

    /// Resume a draft protocol v2 session from an existing request.
    ///
    /// Register typed session update handlers before connecting. When the
    /// request asks for replay, the agent sends those updates before the
    /// [`v2::ResumeSessionResponse`].
    pub fn resume_session_from(
        &self,
        request: v2::ResumeSessionRequest,
    ) -> SentRequest<OpenedV2Session<Counterpart, v2::ResumeSessionResponse>> {
        let session_id = request.session_id.clone();
        let session_connection = self.clone();

        self.send_request_to(Agent, request).map(move |response| {
            let session = V2Session {
                session_id,
                connection: session_connection,
            };
            Ok(OpenedV2Session { session, response })
        })
    }
}

/// Builder for a draft protocol v2 `session/new` request.
///
/// Protocol v2 acknowledges `session/prompt` independently from inbound
/// session updates. Register typed [`v2::UpdateSessionNotification`] and
/// session request handlers on [`crate::Builder`] before connecting, then use
/// [`Self::start_session`] to create the command-only [`V2Session`] handle.
///
/// With both the `unstable_protocol_v2` and `unstable_mcp_over_acp` features,
/// [`Self::with_mcp_server`] attaches an MCP server to the new session.
/// Proxy-session helpers remain available only through the stable protocol v1
/// [`crate::SessionBuilder`].
#[must_use = "call `start_session` to send the `session/new` request"]
#[derive(Debug)]
pub struct V2SessionBuilder<Counterpart, Run = NullRun>
where
    Counterpart: HasPeer<Agent>,
    Run: RunWithConnectionTo<Counterpart>,
{
    connection: V2ConnectionTo<Counterpart>,
    request: v2::NewSessionRequest,
    dynamic_handler_registrations: Vec<DynamicHandlerGuard<Counterpart>>,
    run: Run,
}

impl<Counterpart> V2SessionBuilder<Counterpart, NullRun>
where
    Counterpart: HasPeer<Agent>,
{
    fn new(connection: &V2ConnectionTo<Counterpart>, request: v2::NewSessionRequest) -> Self {
        Self {
            connection: connection.clone(),
            request,
            dynamic_handler_registrations: Vec::new(),
            run: NullRun,
        }
    }
}

impl<Counterpart, Run> V2SessionBuilder<Counterpart, Run>
where
    Counterpart: HasPeer<Agent>,
    Run: RunWithConnectionTo<Counterpart>,
{
    /// Attach an MCP server to this new protocol v2 session.
    ///
    /// This method is available when both `unstable_protocol_v2` and
    /// `unstable_mcp_over_acp` are enabled. MCP routes are installed and their
    /// runner tasks receive an initial poll before `session/new` is published,
    /// allowing the agent to connect while handling session setup. A
    /// successful attachment remains active for the lifetime of the connection.
    #[cfg(feature = "unstable_mcp_over_acp")]
    pub fn with_mcp_server<McpRun>(
        mut self,
        mcp_server: McpServer<Counterpart, McpRun>,
    ) -> Result<V2SessionBuilder<Counterpart, ChainRun<Run, McpRun>>, crate::Error>
    where
        McpRun: RunWithConnectionTo<Counterpart>,
    {
        let (handler, mcp_run) = mcp_server.into_v2_handler_and_runner();
        self.dynamic_handler_registrations
            .push(handler.into_dynamic_handler(&mut self.request, &self.connection)?);
        Ok(V2SessionBuilder {
            connection: self.connection,
            request: self.request,
            dynamic_handler_registrations: self.dynamic_handler_registrations,
            run: ChainRun::new(self.run, mcp_run),
        })
    }

    /// Send `session/new` and return its independently consumable request.
    ///
    /// The successful result contains both a cloneable command handle and the
    /// complete [`v2::NewSessionResponse`]. Consume the returned request with
    /// [`SentRequest::block_task`], [`SentRequest::on_receiving_result`], or
    /// another explicit [`SentRequest`] completion mode.
    ///
    /// Attached MCP routes are installed and their runner tasks begin
    /// executing before the request is published. A valid success response
    /// promotes them to the connection lifetime, independently from how this
    /// request handle is consumed. Setup errors clean up the pending
    /// attachment.
    pub fn start_session(self) -> SentRequest<OpenedV2Session<Counterpart, v2::NewSessionResponse>>
    where
        Run: 'static,
    {
        let Self {
            connection,
            request,
            dynamic_handler_registrations,
            run,
        } = self;
        let session_connection = connection.clone();
        let raw_connection = connection.raw_connection().clone();

        #[cfg(feature = "unstable_mcp_over_acp")]
        let sent_request = if dynamic_handler_registrations.is_empty() {
            drop(run);
            raw_connection.send_request_to(Agent, request)
        } else {
            let handlers_ready = raw_connection.dynamic_handler_barrier();
            let (runner_started_tx, runner_started_rx) = oneshot::channel();
            let (promotion_tx, promotion_rx) = oneshot::channel();
            let runner_started = match raw_connection.spawn(run_pending_mcp_attachment(
                raw_connection.clone(),
                run,
                runner_started_tx,
                promotion_rx,
            )) {
                Ok(()) => Either::Left(async move {
                    runner_started_rx.await.map_err(|error| {
                        crate::util::internal_error(format!(
                            "MCP runner stopped before its initial poll: {error}"
                        ))
                    })?
                }),
                Err(error) => Either::Right(future::ready(Err(error))),
            };
            let readiness = async move {
                future::try_join(handlers_ready, runner_started).await?;
                Ok(())
            };

            raw_connection.send_request_to_with_response_hook_after(
                Agent,
                request,
                readiness,
                move |_response| {
                    promotion_tx.send(()).map_err(|()| {
                        crate::util::internal_error(
                            "MCP runner stopped before session setup completed",
                        )
                    })?;
                    dynamic_handler_registrations
                        .into_iter()
                        .for_each(DynamicHandlerGuard::detach);
                    Ok(())
                },
            )
        };

        #[cfg(not(feature = "unstable_mcp_over_acp"))]
        let sent_request = {
            drop(dynamic_handler_registrations);
            drop(run);
            raw_connection.send_request_to(Agent, request)
        };

        sent_request.map(move |response| {
            let session = V2Session {
                session_id: response.session_id.clone(),
                connection: session_connection,
            };
            Ok(OpenedV2Session { session, response })
        })
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
    connection: V2ConnectionTo<Link>,
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
    pub fn connection(&self) -> &V2ConnectionTo<Link> {
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
