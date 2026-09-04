use std::{future::Future, path::Path};

use futures::{
    channel::oneshot,
    future::{self, Either},
};

use crate::{
    Agent, Client, ConnectionTo, DynamicHandlerGuard, JsonRpcRequest, Responder, SentRequest,
    V2ConnectionTo,
    jsonrpc::run::{NullRun, RunWithConnectionTo},
    role::{HasPeer, acp::ProxySessionMessages},
    schema::v2,
};

#[cfg(feature = "unstable_mcp_over_acp")]
use crate::{jsonrpc::run::ChainRun, mcp_server::McpServer};

async fn run_pending_session_setup<Counterpart, Run>(
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
                // A Pending first poll releases session setup for publication.
                // From that point onward the agent may already be using an
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

fn send_session_setup<Counterpart, Request, Run>(
    connection: V2ConnectionTo<Counterpart>,
    request: Request,
    dynamic_handler_registrations: Vec<DynamicHandlerGuard<Counterpart>>,
    run: Run,
    ordered: bool,
) -> SentRequest<Request::Response>
where
    Counterpart: HasPeer<Agent>,
    Request: JsonRpcRequest + 'static,
    Request::Response: 'static,
    Run: RunWithConnectionTo<Counterpart> + 'static,
{
    let raw_connection = connection.raw_connection().clone();
    if dynamic_handler_registrations.is_empty() {
        drop(run);
        if ordered {
            raw_connection.send_ordered_request_to(Agent, request)
        } else {
            raw_connection.send_request_to(Agent, request)
        }
    } else {
        let handlers_ready = raw_connection.dynamic_handler_barrier();
        let (runner_started_tx, runner_started_rx) = oneshot::channel();
        let (promotion_tx, promotion_rx) = oneshot::channel();
        let runner_started = match raw_connection.spawn(run_pending_session_setup(
            raw_connection.clone(),
            run,
            runner_started_tx,
            promotion_rx,
        )) {
            Ok(()) => Either::Left(async move {
                runner_started_rx.await.map_err(|error| {
                    crate::util::internal_error(format!(
                        "session setup runner stopped before its initial poll: {error}"
                    ))
                })?
            }),
            Err(error) => Either::Right(future::ready(Err(error))),
        };
        let readiness = async move {
            future::try_join(handlers_ready, runner_started).await?;
            Ok(())
        };
        let response_hook = move |_response: &Request::Response| {
            promotion_tx.send(()).map_err(|()| {
                crate::util::internal_error("session setup runner stopped before setup completed")
            })?;
            dynamic_handler_registrations
                .into_iter()
                .for_each(DynamicHandlerGuard::detach);
            Ok(())
        };

        if ordered {
            raw_connection.send_ordered_request_to_with_response_hook_after(
                Agent,
                request,
                readiness,
                response_hook,
            )
        } else {
            raw_connection.send_request_to_with_response_hook_after(
                Agent,
                request,
                readiness,
                response_hook,
            )
        }
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

    /// Build an unstable draft protocol v2 `session/fork` request.
    ///
    /// This helper is available with the `unstable_session_fork` feature. Call
    /// [`V2ForkSessionBuilder::start_session`] to publish the request and
    /// obtain a command handle for the newly created fork.
    #[cfg(feature = "unstable_session_fork")]
    pub fn fork_session(
        &self,
        session_id: impl Into<v2::SessionId>,
        cwd: impl AsRef<Path>,
    ) -> V2ForkSessionBuilder<Counterpart> {
        self.fork_session_from(v2::ForkSessionRequest::new(session_id, cwd.as_ref()))
    }

    /// Build an unstable draft protocol v2 `session/fork` request from an
    /// existing request.
    ///
    /// This helper is available with the `unstable_session_fork` feature. Call
    /// [`V2ForkSessionBuilder::start_session`] to publish the request.
    #[cfg(feature = "unstable_session_fork")]
    pub fn fork_session_from(
        &self,
        request: v2::ForkSessionRequest,
    ) -> V2ForkSessionBuilder<Counterpart> {
        V2ForkSessionBuilder::new(self, request)
    }

    /// Build a draft protocol v2 `session/resume` request.
    ///
    /// Use [`Self::resume_session_from`] to request history replay or set
    /// other optional resume parameters. Call
    /// [`V2ResumeSessionBuilder::start_session`] to publish the request.
    pub fn resume_session(
        &self,
        session_id: impl Into<v2::SessionId>,
        cwd: impl AsRef<Path>,
    ) -> V2ResumeSessionBuilder<Counterpart> {
        self.resume_session_from(v2::ResumeSessionRequest::new(session_id, cwd.as_ref()))
    }

    /// Build a draft protocol v2 `session/resume` request from an existing request.
    ///
    /// Register typed session update handlers before connecting. When the
    /// request asks for replay, the agent sends those updates before the
    /// [`v2::ResumeSessionResponse`]. Call
    /// [`V2ResumeSessionBuilder::start_session`] to publish the request.
    pub fn resume_session_from(
        &self,
        request: v2::ResumeSessionRequest,
    ) -> V2ResumeSessionBuilder<Counterpart> {
        V2ResumeSessionBuilder::new(self, request)
    }
}

/// Builder for a draft protocol v2 `session/new` request.
///
/// Protocol v2 acknowledges `session/prompt` independently from inbound
/// session updates. Register typed [`v2::UpdateSessionNotification`] and
/// session request handlers on [`crate::Builder`] before connecting, then use
/// [`Self::start_session`] to create the command-only [`V2Session`] handle or
/// `on_proxy_session_start` to forward setup through a proxy.
///
/// With both the `unstable_protocol_v2` and `unstable_mcp_over_acp` features,
/// `with_mcp_server` attaches an MCP server to the new session.
#[must_use = "call `start_session` or `on_proxy_session_start` to send the `session/new` request"]
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
            .push(handler.into_dynamic_handler(&mut self.request.mcp_servers, &self.connection)?);
        Ok(V2SessionBuilder {
            connection: self.connection,
            request: self.request,
            dynamic_handler_registrations: self.dynamic_handler_registrations,
            run: ChainRun::new(self.run, mcp_run),
        })
    }

    fn send_new_session(self, ordered: bool) -> SentRequest<v2::NewSessionResponse>
    where
        Run: 'static,
    {
        let Self {
            connection,
            request,
            dynamic_handler_registrations,
            run,
        } = self;
        send_session_setup(
            connection,
            request,
            dynamic_handler_registrations,
            run,
            ordered,
        )
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
        let session_connection = self.connection.clone();
        self.send_new_session(false).map(move |response| {
            let session = V2Session {
                session_id: response.session_id.clone(),
                connection: session_connection,
            };
            Ok(OpenedV2Session { session, response })
        })
    }

    /// Start a protocol v2 session through a proxy and forward its response.
    ///
    /// The downstream request is ordered and inherits cancellation from the
    /// upstream request. On success, this helper installs session routing before
    /// later inbound traffic is processed, forwards the complete response, and
    /// spawns `op` with an [`OpenedV2Session`] containing the command-only
    /// session handle plus the complete setup response. Inbound updates and
    /// interactive requests remain independent connection traffic.
    ///
    /// The callback runs outside the ordered response barrier, so it may wait
    /// for later connection traffic without deadlocking the dispatch loop.
    pub fn on_proxy_session_start<F, Fut>(
        self,
        responder: Responder<v2::NewSessionResponse>,
        op: F,
    ) -> Result<(), crate::Error>
    where
        Counterpart: HasPeer<Client>,
        Run: 'static,
        F: FnOnce(OpenedV2Session<Counterpart, v2::NewSessionResponse>) -> Fut + Send + 'static,
        Fut: Future<Output = Result<(), crate::Error>> + Send,
    {
        let session_connection = self.connection.clone();
        self.send_new_session(true)
            .forward_cancellation_from(responder.cancellation())
            .on_receiving_ok_result(responder, async move |response, responder| {
                let session_id = response.session_id.clone();
                let raw_connection = session_connection.raw_connection();
                let route = match raw_connection.add_dynamic_handler(ProxySessionMessages::new(
                    crate::schema::v1::SessionId::from(session_id.0.clone()),
                )) {
                    Ok(route) => route,
                    Err(error) => return responder.respond_with_error(error),
                };

                let opened = OpenedV2Session {
                    session: V2Session {
                        session_id,
                        connection: session_connection.clone(),
                    },
                    response: response.clone(),
                };
                responder.respond(response)?;
                route.detach();
                raw_connection.spawn(async move { op(opened).await })
            })
    }
}

/// Builder for an unstable draft protocol v2 `session/fork` request.
///
/// A successful fork creates a new independent session whose ID comes from the
/// [`v2::ForkSessionResponse`]. Register typed
/// [`v2::UpdateSessionNotification`] and interactive request handlers on
/// [`crate::Builder`] before connecting, then use [`Self::start_session`] to
/// obtain a command handle for the fork or `on_proxy_session_start` to forward
/// setup through a proxy.
///
/// This type is available with the `unstable_session_fork` feature. With
/// `unstable_mcp_over_acp` as well, `with_mcp_server` attaches an MCP server to
/// the forked session.
#[cfg(feature = "unstable_session_fork")]
#[must_use = "call `start_session` or `on_proxy_session_start` to send the `session/fork` request"]
#[derive(Debug)]
pub struct V2ForkSessionBuilder<Counterpart, Run = NullRun>
where
    Counterpart: HasPeer<Agent>,
    Run: RunWithConnectionTo<Counterpart>,
{
    connection: V2ConnectionTo<Counterpart>,
    request: v2::ForkSessionRequest,
    dynamic_handler_registrations: Vec<DynamicHandlerGuard<Counterpart>>,
    run: Run,
}

#[cfg(feature = "unstable_session_fork")]
impl<Counterpart> V2ForkSessionBuilder<Counterpart, NullRun>
where
    Counterpart: HasPeer<Agent>,
{
    fn new(connection: &V2ConnectionTo<Counterpart>, request: v2::ForkSessionRequest) -> Self {
        Self {
            connection: connection.clone(),
            request,
            dynamic_handler_registrations: Vec::new(),
            run: NullRun,
        }
    }
}

#[cfg(feature = "unstable_session_fork")]
impl<Counterpart, Run> V2ForkSessionBuilder<Counterpart, Run>
where
    Counterpart: HasPeer<Agent>,
    Run: RunWithConnectionTo<Counterpart>,
{
    /// Attach an MCP server to this forked protocol v2 session.
    ///
    /// This method is available when `unstable_mcp_over_acp` is enabled in
    /// addition to `unstable_protocol_v2` and `unstable_session_fork`. MCP
    /// routes are installed and their runner tasks receive an initial poll
    /// before `session/fork` is published, allowing the agent to connect while
    /// handling session setup. A successful attachment remains active for the
    /// lifetime of the connection.
    #[cfg(feature = "unstable_mcp_over_acp")]
    pub fn with_mcp_server<McpRun>(
        mut self,
        mcp_server: McpServer<Counterpart, McpRun>,
    ) -> Result<V2ForkSessionBuilder<Counterpart, ChainRun<Run, McpRun>>, crate::Error>
    where
        McpRun: RunWithConnectionTo<Counterpart>,
    {
        let (handler, mcp_run) = mcp_server.into_v2_handler_and_runner();
        self.dynamic_handler_registrations
            .push(handler.into_dynamic_handler(&mut self.request.mcp_servers, &self.connection)?);
        Ok(V2ForkSessionBuilder {
            connection: self.connection,
            request: self.request,
            dynamic_handler_registrations: self.dynamic_handler_registrations,
            run: ChainRun::new(self.run, mcp_run),
        })
    }

    fn send_fork_session(self, ordered: bool) -> SentRequest<v2::ForkSessionResponse>
    where
        Run: 'static,
    {
        let Self {
            connection,
            request,
            dynamic_handler_registrations,
            run,
        } = self;
        send_session_setup(
            connection,
            request,
            dynamic_handler_registrations,
            run,
            ordered,
        )
    }

    /// Send `session/fork` and return its independently consumable request.
    ///
    /// The successful result contains both a cloneable command handle for the
    /// newly created fork and the complete [`v2::ForkSessionResponse`]. Consume
    /// the returned request with [`SentRequest::block_task`],
    /// [`SentRequest::on_receiving_result`], or another explicit [`SentRequest`]
    /// completion mode.
    ///
    /// Attached MCP routes are installed and their runner tasks begin
    /// executing before the request is published. A valid success response
    /// promotes them to the connection lifetime independently from how this
    /// request handle is consumed; setup errors clean up the pending
    /// attachment.
    pub fn start_session(self) -> SentRequest<OpenedV2Session<Counterpart, v2::ForkSessionResponse>>
    where
        Run: 'static,
    {
        let session_connection = self.connection.clone();
        self.send_fork_session(false).map(move |response| {
            let session = V2Session {
                session_id: response.session_id.clone(),
                connection: session_connection,
            };
            Ok(OpenedV2Session { session, response })
        })
    }

    /// Fork a protocol v2 session through a proxy and forward its response.
    ///
    /// The downstream request is ordered and inherits cancellation from the
    /// upstream request. On success, this helper obtains the new session ID
    /// from the response, installs session routing before later inbound traffic
    /// is processed, forwards the complete response, and spawns `op` with an
    /// [`OpenedV2Session`] containing the fork's command handle and response.
    /// Inbound updates and interactive requests remain independent connection
    /// traffic.
    ///
    /// The callback runs outside the ordered response barrier, so it may wait
    /// for later connection traffic without deadlocking the dispatch loop.
    pub fn on_proxy_session_start<F, Fut>(
        self,
        responder: Responder<v2::ForkSessionResponse>,
        op: F,
    ) -> Result<(), crate::Error>
    where
        Counterpart: HasPeer<Client>,
        Run: 'static,
        F: FnOnce(OpenedV2Session<Counterpart, v2::ForkSessionResponse>) -> Fut + Send + 'static,
        Fut: Future<Output = Result<(), crate::Error>> + Send,
    {
        let session_connection = self.connection.clone();
        self.send_fork_session(true)
            .forward_cancellation_from(responder.cancellation())
            .on_receiving_ok_result(responder, async move |response, responder| {
                let session_id = response.session_id.clone();
                let raw_connection = session_connection.raw_connection();
                let route = match raw_connection.add_dynamic_handler(ProxySessionMessages::new(
                    crate::schema::v1::SessionId::from(session_id.0.clone()),
                )) {
                    Ok(route) => route,
                    Err(error) => return responder.respond_with_error(error),
                };

                let opened = OpenedV2Session {
                    session: V2Session {
                        session_id,
                        connection: session_connection.clone(),
                    },
                    response: response.clone(),
                };
                responder.respond(response)?;
                route.detach();
                raw_connection.spawn(async move { op(opened).await })
            })
    }
}

/// Builder for a draft protocol v2 `session/resume` request.
///
/// Replay updates arrive before the resume response. Direct clients must
/// register typed [`v2::UpdateSessionNotification`] and interactive request
/// handlers on [`crate::Builder`] before connecting. Proxies should use
/// [`Self::on_proxy_session_start`], which makes downstream session routing
/// ready before publishing the resume request.
///
/// With both the `unstable_protocol_v2` and `unstable_mcp_over_acp` features,
/// `with_mcp_server` attaches an MCP server to the resumed session and makes it
/// ready before the agent can send replay or its response.
#[must_use = "call `start_session` or `on_proxy_session_start` to send the `session/resume` request"]
#[derive(Debug)]
pub struct V2ResumeSessionBuilder<Counterpart, Run = NullRun>
where
    Counterpart: HasPeer<Agent>,
    Run: RunWithConnectionTo<Counterpart>,
{
    connection: V2ConnectionTo<Counterpart>,
    request: v2::ResumeSessionRequest,
    dynamic_handler_registrations: Vec<DynamicHandlerGuard<Counterpart>>,
    run: Run,
}

impl<Counterpart> V2ResumeSessionBuilder<Counterpart, NullRun>
where
    Counterpart: HasPeer<Agent>,
{
    fn new(connection: &V2ConnectionTo<Counterpart>, request: v2::ResumeSessionRequest) -> Self {
        Self {
            connection: connection.clone(),
            request,
            dynamic_handler_registrations: Vec::new(),
            run: NullRun,
        }
    }
}

impl<Counterpart, Run> V2ResumeSessionBuilder<Counterpart, Run>
where
    Counterpart: HasPeer<Agent>,
    Run: RunWithConnectionTo<Counterpart>,
{
    /// Attach an MCP server to this resumed protocol v2 session.
    ///
    /// This method is available when both `unstable_protocol_v2` and
    /// `unstable_mcp_over_acp` are enabled. MCP routes are installed and their
    /// runner tasks receive an initial poll before `session/resume` is
    /// published, allowing the agent to use the server during replay and
    /// session setup. A successful attachment remains active for the lifetime
    /// of the connection.
    #[cfg(feature = "unstable_mcp_over_acp")]
    pub fn with_mcp_server<McpRun>(
        mut self,
        mcp_server: McpServer<Counterpart, McpRun>,
    ) -> Result<V2ResumeSessionBuilder<Counterpart, ChainRun<Run, McpRun>>, crate::Error>
    where
        McpRun: RunWithConnectionTo<Counterpart>,
    {
        let (handler, mcp_run) = mcp_server.into_v2_handler_and_runner();
        self.dynamic_handler_registrations
            .push(handler.into_dynamic_handler(&mut self.request.mcp_servers, &self.connection)?);
        Ok(V2ResumeSessionBuilder {
            connection: self.connection,
            request: self.request,
            dynamic_handler_registrations: self.dynamic_handler_registrations,
            run: ChainRun::new(self.run, mcp_run),
        })
    }

    fn send_resume_session(self, ordered: bool) -> SentRequest<v2::ResumeSessionResponse>
    where
        Run: 'static,
    {
        let Self {
            connection,
            request,
            dynamic_handler_registrations,
            run,
        } = self;
        send_session_setup(
            connection,
            request,
            dynamic_handler_registrations,
            run,
            ordered,
        )
    }

    /// Send `session/resume` and return its independently consumable request.
    ///
    /// The successful result contains both a cloneable command handle and the
    /// complete [`v2::ResumeSessionResponse`]. Consume the returned request
    /// with [`SentRequest::block_task`], [`SentRequest::on_receiving_result`],
    /// or another explicit [`SentRequest`] completion mode.
    ///
    /// Replay is delivered through the typed connection handlers before the
    /// response. Attached MCP routes and runner tasks are ready before the
    /// request is published. A valid success response promotes them to the
    /// connection lifetime independently from how this request handle is
    /// consumed; setup errors clean up the pending attachment.
    pub fn start_session(
        self,
    ) -> SentRequest<OpenedV2Session<Counterpart, v2::ResumeSessionResponse>>
    where
        Run: 'static,
    {
        let session_id = self.request.session_id.clone();
        let session_connection = self.connection.clone();
        self.send_resume_session(false).map(move |response| {
            let session = V2Session {
                session_id,
                connection: session_connection,
            };
            Ok(OpenedV2Session { session, response })
        })
    }

    /// Resume a protocol v2 session through a proxy and forward its response.
    ///
    /// The session ID is known before the request, so this helper installs and
    /// acknowledges downstream session routing before publishing the ordered
    /// `session/resume` request. Replay can therefore be forwarded before the
    /// response as required by the protocol. The downstream request inherits
    /// cancellation from the upstream request.
    ///
    /// On success, the helper forwards the complete response and spawns `op`
    /// with an [`OpenedV2Session`] containing the command-only session handle
    /// plus that response. The callback runs outside the ordered response
    /// barrier, so it may wait for later connection traffic without
    /// deadlocking the dispatch loop.
    pub fn on_proxy_session_start<F, Fut>(
        mut self,
        responder: Responder<v2::ResumeSessionResponse>,
        op: F,
    ) -> Result<(), crate::Error>
    where
        Counterpart: HasPeer<Client>,
        Run: 'static,
        F: FnOnce(OpenedV2Session<Counterpart, v2::ResumeSessionResponse>) -> Fut + Send + 'static,
        Fut: Future<Output = Result<(), crate::Error>> + Send,
    {
        let session_id = self.request.session_id.clone();
        let session_connection = self.connection.clone();
        self.dynamic_handler_registrations.push(
            session_connection
                .raw_connection()
                .add_dynamic_handler(ProxySessionMessages::new(
                    crate::schema::v1::SessionId::from(session_id.0.clone()),
                ))?,
        );

        self.send_resume_session(true)
            .forward_cancellation_from(responder.cancellation())
            .on_receiving_ok_result(responder, async move |response, responder| {
                let opened = OpenedV2Session {
                    session: V2Session {
                        session_id,
                        connection: session_connection.clone(),
                    },
                    response: response.clone(),
                };
                responder.respond(response)?;
                session_connection
                    .raw_connection()
                    .spawn(async move { op(opened).await })
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

    /// Inject content for pending delivery to this session.
    #[cfg(feature = "unstable_session_inject")]
    pub fn inject(
        &self,
        mode: v2::SessionInjectMode,
        content: Vec<v2::ContentBlock>,
    ) -> SentRequest<v2::InjectSessionResponse> {
        self.connection.send_request_to(
            Agent,
            v2::InjectSessionRequest::new(self.session_id.clone(), mode, content),
        )
    }

    /// Revoke a pending injected message.
    #[cfg(feature = "unstable_session_inject")]
    pub fn revoke_inject(
        &self,
        message_id: impl Into<v2::MessageId>,
    ) -> SentRequest<v2::RevokeInjectSessionResponse> {
        self.connection.send_request_to(
            Agent,
            v2::RevokeInjectSessionRequest::new(self.session_id.clone(), message_id),
        )
    }

    /// Replace the content of a pending injected message.
    #[cfg(feature = "unstable_session_inject")]
    pub fn replace_inject(
        &self,
        message_id: impl Into<v2::MessageId>,
        content: Vec<v2::ContentBlock>,
    ) -> SentRequest<v2::ReplaceInjectSessionResponse> {
        self.connection.send_request_to(
            Agent,
            v2::ReplaceInjectSessionRequest::new(self.session_id.clone(), message_id, content),
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
