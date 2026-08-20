//! MCP server construction, direct serving, and optional ACP attachment.

use std::{marker::PhantomData, sync::Arc};

use futures::{StreamExt, channel::mpsc};

use crate::{
    ConnectTo, Dispatch, DynConnectTo, Role,
    jsonrpc::run::{NullRun, RunWithConnectionTo},
    mcp_server::{McpConnectionContext, McpConnectionTo, McpServerConnect},
    role,
};

#[cfg(feature = "unstable_mcp_over_acp")]
use uuid::Uuid;

#[cfg(feature = "unstable_mcp_over_acp")]
use crate::{
    Agent, Client, ConnectionTo, HandleDispatchFrom, Handled,
    jsonrpc::DynamicHandlerGuard,
    mcp_server::active_session::{McpActiveSession, V1McpProtocol},
    schema::v1::{
        LoadSessionRequest, McpServer as SchemaMcpServer, McpServerAcp, McpServerAcpId,
        NewSessionRequest, ResumeSessionRequest,
    },
    util::MatchDispatchFrom,
};

#[cfg(all(feature = "unstable_mcp_over_acp", feature = "unstable_protocol_v2"))]
use crate::{JsonRpcMessage, UntypedMessage};

#[cfg(feature = "unstable_mcp_over_acp")]
use crate::role::HasPeer;

#[cfg(all(feature = "unstable_mcp_over_acp", feature = "unstable_session_fork"))]
use crate::schema::v1::ForkSessionRequest;

/// A runtime-agnostic MCP server.
///
/// `McpServer` wraps an [`McpServerConnect`](`super::McpServerConnect`)
/// implementation. A server whose counterpart is [`role::mcp::Client`] can be
/// connected directly as a standalone MCP component. With the
/// `unstable_mcp_over_acp` feature, servers can instead be attached to ACP
/// session setup through `Builder::with_mcp_server` or
/// `SessionBuilder::with_mcp_server`. When `unstable_protocol_v2` is also
/// enabled, `Proxy.v2().with_mcp_server` attaches a server globally to draft
/// v2 setup requests. `V2SessionBuilder::with_mcp_server` attaches one to a
/// single new v2 session, while `V2ResumeSessionBuilder::with_mcp_server`
/// attaches one to a single resumed session.
///
/// # Creating an MCP Server
///
/// The `agent-client-protocol-rmcp` crate provides builder APIs for MCP tools
/// backed by the `rmcp` crate.
///
/// Or implement [`McpServerConnect`](`super::McpServerConnect`) for custom server behavior:
///
/// ```rust,ignore
/// let server = McpServer::new(MyCustomServerConnect, NullRun);
/// ```
pub struct McpServer<Counterpart: Role, Run = NullRun> {
    /// The host role that is serving up this MCP server
    phantom: PhantomData<Counterpart>,

    /// The "connect" instance
    connect: Arc<dyn McpServerConnect<Counterpart>>,

    /// The runner is a task that should be run alongside the message handler.
    /// Some futures direct messages back through channels to this future which actually
    /// handles responding to the client.
    ///
    /// Some connector implementations use this to run support tasks alongside
    /// the message handler. It has no separate readiness protocol: communication
    /// primitives needed by `connect` must already exist when the server is
    /// constructed.
    runner: Run,
}

impl<Counterpart: Role + std::fmt::Debug, Run: std::fmt::Debug> std::fmt::Debug
    for McpServer<Counterpart, Run>
{
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("McpServer")
            .field("phantom", &self.phantom)
            .field("runner", &self.runner)
            .finish_non_exhaustive()
    }
}

impl<Counterpart: Role, Run> McpServer<Counterpart, Run>
where
    Run: RunWithConnectionTo<Counterpart>,
{
    /// Create an MCP server from something that implements the [`McpServerConnect`](`super::McpServerConnect`) trait.
    ///
    /// # See also
    ///
    /// See `agent-client-protocol-rmcp` to construct MCP servers from Rust code
    /// with `rmcp`.
    pub fn new(c: impl McpServerConnect<Counterpart>, runner: Run) -> Self {
        McpServer {
            phantom: PhantomData,
            connect: Arc::new(c),
            runner,
        }
    }

    /// Split this MCP server into the message handler and a future that must be run while the handler is active.
    #[cfg(feature = "unstable_mcp_over_acp")]
    pub(crate) fn into_handler_and_runner(self) -> (McpSessionHandler<Counterpart>, Run)
    where
        Counterpart: HasPeer<Agent>,
    {
        let Self {
            phantom: _,
            connect,
            runner,
        } = self;
        let server_id = McpServerAcpId::new(format!("mcp-server:{}", Uuid::new_v4()));
        (McpSessionHandler::new(server_id, connect), runner)
    }

    /// Split this MCP server into a protocol v2 session handler and its runner.
    #[cfg(all(feature = "unstable_mcp_over_acp", feature = "unstable_protocol_v2"))]
    pub(crate) fn into_v2_handler_and_runner(self) -> (V2McpSessionHandler<Counterpart>, Run)
    where
        Counterpart: HasPeer<Agent>,
    {
        let Self {
            phantom: _,
            connect,
            runner,
        } = self;
        let server_id = McpServerAcpId::new(format!("mcp-server:{}", Uuid::new_v4()));
        (V2McpSessionHandler::new(server_id, connect), runner)
    }
}

/// Message handler created from a [`McpServer`].
#[cfg(feature = "unstable_mcp_over_acp")]
pub(crate) struct McpSessionHandler<Counterpart: Role>
where
    Counterpart: HasPeer<Agent>,
{
    server_id: McpServerAcpId,
    connect: Arc<dyn McpServerConnect<Counterpart>>,
    active_session: McpActiveSession<Counterpart, V1McpProtocol>,
}

#[cfg(feature = "unstable_mcp_over_acp")]
impl<Counterpart: Role> McpSessionHandler<Counterpart>
where
    Counterpart: HasPeer<Agent>,
{
    pub fn new(server_id: McpServerAcpId, connect: Arc<dyn McpServerConnect<Counterpart>>) -> Self {
        Self {
            active_session: McpActiveSession::new(server_id.clone(), connect.clone()),
            server_id,
            connect,
        }
    }

    /// Append this MCP server's native ACP declaration to a session setup request.
    fn append_declaration(&self, mcp_servers: &mut Vec<SchemaMcpServer>) {
        mcp_servers.push(SchemaMcpServer::Acp(McpServerAcp::new(
            self.connect.name(),
            self.server_id.clone(),
        )));
    }
}

/// Protocol v2 session adapter for the shared native MCP-over-ACP runtime.
#[cfg(all(feature = "unstable_mcp_over_acp", feature = "unstable_protocol_v2"))]
pub(crate) struct V2McpSessionHandler<Counterpart: Role>
where
    Counterpart: HasPeer<Agent>,
{
    server_id: McpServerAcpId,
    connect: Arc<dyn McpServerConnect<Counterpart>>,
    active_session: McpActiveSession<Counterpart, crate::mcp_server::active_session::V2McpProtocol>,
}

#[cfg(all(feature = "unstable_mcp_over_acp", feature = "unstable_protocol_v2"))]
impl<Counterpart: Role> V2McpSessionHandler<Counterpart>
where
    Counterpart: HasPeer<Agent>,
{
    fn new(server_id: McpServerAcpId, connect: Arc<dyn McpServerConnect<Counterpart>>) -> Self {
        Self {
            active_session: McpActiveSession::new(server_id.clone(), connect.clone()),
            server_id,
            connect,
        }
    }

    fn declaration(&self) -> crate::schema::v2::McpServer {
        crate::schema::v2::McpServer::Acp(crate::schema::v2::McpServerAcp::new(
            self.connect.name(),
            crate::schema::v2::McpServerAcpId::from(self.server_id.clone()),
        ))
    }

    fn append_declaration(&self, mcp_servers: &mut Vec<crate::schema::v2::McpServer>) {
        mcp_servers.push(self.declaration());
    }

    fn validate_session_setup(request: &UntypedMessage) -> Result<bool, crate::Error> {
        match request.method() {
            "session/new" => {
                crate::schema::v2::NewSessionRequest::parse_message(
                    request.method(),
                    request.params(),
                )?;
                Ok(true)
            }
            "session/resume" => {
                crate::schema::v2::ResumeSessionRequest::parse_message(
                    request.method(),
                    request.params(),
                )?;
                Ok(true)
            }
            #[cfg(feature = "unstable_session_fork")]
            "session/fork" => {
                crate::schema::v2::ForkSessionRequest::parse_message(
                    request.method(),
                    request.params(),
                )?;
                Ok(true)
            }
            _ => Ok(false),
        }
    }

    fn append_declaration_to_raw(&self, request: &mut UntypedMessage) -> Result<(), crate::Error> {
        let serde_json::Value::Object(params) = &mut request.params else {
            return Err(
                crate::Error::invalid_params().data("session setup parameters must be an object")
            );
        };
        let declaration =
            serde_json::to_value(self.declaration()).map_err(crate::Error::into_internal_error)?;
        match params.get_mut("mcpServers") {
            Some(serde_json::Value::Array(servers)) => servers.push(declaration),
            Some(value) => *value = serde_json::Value::Array(vec![declaration]),
            None => {
                params.insert(
                    "mcpServers".to_string(),
                    serde_json::Value::Array(vec![declaration]),
                );
            }
        }
        Ok(())
    }

    /// Attach this server to a draft protocol v2 session setup request.
    pub fn into_dynamic_handler(
        self,
        mcp_servers: &mut Vec<crate::schema::v2::McpServer>,
        cx: &crate::V2ConnectionTo<Counterpart>,
    ) -> Result<DynamicHandlerGuard<Counterpart>, crate::Error> {
        self.append_declaration(mcp_servers);
        cx.add_dynamic_handler(self.active_session)
    }
}

#[cfg(all(feature = "unstable_mcp_over_acp", feature = "unstable_protocol_v2"))]
impl<Counterpart: Role> HandleDispatchFrom<Counterpart> for V2McpSessionHandler<Counterpart>
where
    Counterpart: HasPeer<Client> + HasPeer<Agent>,
{
    async fn handle_dispatch_from(
        &mut self,
        message: Dispatch,
        cx: ConnectionTo<Counterpart>,
    ) -> Result<Handled<Dispatch>, crate::Error> {
        MatchDispatchFrom::new(message, &cx)
            .if_request_from(Client, async |mut request: UntypedMessage, responder| {
                if !Self::validate_session_setup(&request)? {
                    return Ok(Handled::No {
                        message: (request, responder),
                        retry: false,
                    });
                }

                self.append_declaration_to_raw(&mut request)?;
                Ok(Handled::No {
                    message: (request, responder),
                    retry: false,
                })
            })
            .await
            .otherwise_delegate(&mut self.active_session)
            .await
    }

    fn describe_chain(&self) -> impl std::fmt::Debug {
        format!("V2McpServer({})", self.connect.name())
    }
}

#[cfg(feature = "unstable_mcp_over_acp")]
impl<Counterpart: Role> McpSessionHandler<Counterpart>
where
    Counterpart: HasPeer<Agent>,
{
    /// Attach this server to the new session, spawning off a dynamic handler that will
    /// manage requests coming from this session.
    ///
    /// # Return value
    ///
    /// Returns a [`DynamicHandlerGuard`] for the handler that intercepts messages
    /// related to this MCP server. Once the value is dropped, the MCP server messages
    /// will no longer be received, so you need to keep this value alive as long as the session
    /// is in use. You can also invoke [`DynamicHandlerGuard::detach`] if you
    /// want to keep the handler registered for the life of the connection.
    pub fn into_dynamic_handler(
        self,
        request: &mut NewSessionRequest,
        cx: &ConnectionTo<Counterpart>,
    ) -> Result<DynamicHandlerGuard<Counterpart>, crate::Error>
    where
        Counterpart: HasPeer<Agent>,
    {
        self.append_declaration(&mut request.mcp_servers);
        cx.add_dynamic_handler(self.active_session)
    }
}

#[cfg(feature = "unstable_mcp_over_acp")]
impl<Counterpart: Role> HandleDispatchFrom<Counterpart> for McpSessionHandler<Counterpart>
where
    Counterpart: HasPeer<Client> + HasPeer<Agent>,
{
    async fn handle_dispatch_from(
        &mut self,
        message: Dispatch,
        cx: ConnectionTo<Counterpart>,
    ) -> Result<Handled<Dispatch>, crate::Error> {
        let matcher = MatchDispatchFrom::new(message, &cx)
            .if_request_from(Client, async |mut request: NewSessionRequest, responder| {
                self.append_declaration(&mut request.mcp_servers);
                Ok(Handled::No {
                    message: (request, responder),
                    retry: false,
                })
            })
            .await
            .if_request_from(
                Client,
                async |mut request: LoadSessionRequest, responder| {
                    self.append_declaration(&mut request.mcp_servers);
                    Ok(Handled::No {
                        message: (request, responder),
                        retry: false,
                    })
                },
            )
            .await
            .if_request_from(
                Client,
                async |mut request: ResumeSessionRequest, responder| {
                    self.append_declaration(&mut request.mcp_servers);
                    Ok(Handled::No {
                        message: (request, responder),
                        retry: false,
                    })
                },
            )
            .await;

        #[cfg(feature = "unstable_session_fork")]
        let matcher = matcher
            .if_request_from(
                Client,
                async |mut request: ForkSessionRequest, responder| {
                    self.append_declaration(&mut request.mcp_servers);
                    Ok(Handled::No {
                        message: (request, responder),
                        retry: false,
                    })
                },
            )
            .await;

        matcher.otherwise_delegate(&mut self.active_session).await
    }

    fn describe_chain(&self) -> impl std::fmt::Debug {
        format!("McpServer({})", self.connect.name())
    }
}

impl<Run> ConnectTo<role::mcp::Client> for McpServer<role::mcp::Client, Run>
where
    Run: RunWithConnectionTo<role::mcp::Client> + 'static,
{
    async fn connect_to(
        self,
        client: impl ConnectTo<role::mcp::Server>,
    ) -> Result<(), crate::Error> {
        let Self {
            connect,
            runner,
            phantom: _,
        } = self;

        let (tx, mut rx) = mpsc::unbounded();

        role::mcp::Server
            .builder()
            .with_runner(runner)
            .on_receive_dispatch(
                async |message_from_client: Dispatch, _cx| {
                    tx.unbounded_send(message_from_client)
                        .map_err(|_| crate::util::internal_error("nobody listening to mcp server"))
                },
                crate::on_receive_dispatch!(),
            )
            .with_spawned(async move |connection_to_client| {
                let spawned_server: DynConnectTo<role::mcp::Client> =
                    connect.connect(McpConnectionTo {
                        context: McpConnectionContext::Standalone,
                        connection: connection_to_client.clone(),
                    });

                role::mcp::Client
                    .builder()
                    .on_receive_dispatch(
                        async |message_from_server: Dispatch, _| {
                            // when we receive a message from the server, fwd to the client
                            connection_to_client.send_proxied_message(message_from_server)
                        },
                        crate::on_receive_dispatch!(),
                    )
                    .connect_with(spawned_server, async |connection_to_server| {
                        while let Some(message_from_client) = rx.next().await {
                            connection_to_server.send_proxied_message(message_from_client)?;
                        }
                        Ok(())
                    })
                    .await
            })
            .connect_to(client)
            .await
    }
}

#[cfg(all(
    test,
    feature = "unstable_mcp_over_acp",
    feature = "unstable_protocol_v2"
))]
mod tests {
    use std::{collections::BTreeMap, path::PathBuf, sync::Arc};

    use serde::Serialize;
    use serde_json::{Value, json};

    use super::V2McpSessionHandler;
    use crate::{
        Conductor, DynConnectTo, Error, UntypedMessage,
        mcp_server::{McpConnectionTo, McpServerConnect},
        role,
        schema::{
            v1::McpServerAcpId,
            v2::{self, McpServer},
        },
    };

    struct UnusedMcpConnect;

    impl McpServerConnect<Conductor> for UnusedMcpConnect {
        fn name(&self) -> String {
            "global-v2-server".to_owned()
        }

        fn connect(&self, _context: McpConnectionTo<Conductor>) -> DynConnectTo<role::mcp::Client> {
            panic!("declaration tests must not connect to the MCP server")
        }
    }

    fn handler() -> V2McpSessionHandler<Conductor> {
        V2McpSessionHandler::new(
            McpServerAcpId::new("global-v2-server-id"),
            Arc::new(UnusedMcpConnect),
        )
    }

    fn existing_server() -> McpServer {
        McpServer::Other(v2::OtherMcpServer::new(
            "_future_transport",
            BTreeMap::from([("futureOption".to_owned(), json!({ "nested": true }))]),
        ))
    }

    fn meta() -> v2::Meta {
        v2::Meta::from_iter([("preserved".to_owned(), json!({ "nested": true }))])
    }

    fn assert_raw_append_preserves_params(
        handler: &V2McpSessionHandler<Conductor>,
        method: &str,
        params: impl Serialize,
    ) -> Result<v2::McpServerAcpId, Error> {
        let mut params = serde_json::to_value(params)?;
        let Value::Object(params_object) = &mut params else {
            panic!("session setup params should serialize as an object");
        };
        params_object.insert(
            "_futureSessionField".to_owned(),
            json!({ "must": ["remain", "untouched"] }),
        );

        let mut expected = params.clone();
        expected
            .get_mut("mcpServers")
            .and_then(Value::as_array_mut)
            .expect("test request should contain mcpServers")
            .push(serde_json::to_value(handler.declaration())?);

        let mut request = UntypedMessage::new(method, params)?;
        assert!(V2McpSessionHandler::<Conductor>::validate_session_setup(
            &request
        )?);
        handler.append_declaration_to_raw(&mut request)?;

        assert_eq!(
            request.params, expected,
            "global attachment must only append its declaration"
        );

        let appended = request
            .params
            .get("mcpServers")
            .and_then(Value::as_array)
            .and_then(|servers| servers.last())
            .cloned()
            .expect("global declaration should be appended");
        match serde_json::from_value::<McpServer>(appended)? {
            McpServer::Acp(server) => {
                assert_eq!(server.name, "global-v2-server");
                Ok(server.server_id)
            }
            server => panic!("expected an ACP server declaration, got {server:?}"),
        }
    }

    #[test]
    fn v2_global_mcp_declaration_preserves_all_session_setup_params() -> Result<(), Error> {
        let handler = handler();
        let cwd = PathBuf::from("/tmp/global-v2-mcp");
        let additional_directory = PathBuf::from("/tmp/global-v2-mcp-additional");
        let session_id = v2::SessionId::new("session-to-resume");
        let existing_server = existing_server();

        let new_server_id = assert_raw_append_preserves_params(
            &handler,
            "session/new",
            v2::NewSessionRequest::new(cwd.clone())
                .additional_directories([additional_directory.clone()])
                .mcp_servers(vec![existing_server.clone()])
                .meta(meta()),
        )?;

        let resume_server_id = assert_raw_append_preserves_params(
            &handler,
            "session/resume",
            v2::ResumeSessionRequest::new(session_id.clone(), cwd.clone())
                .additional_directories([additional_directory.clone()])
                .mcp_servers(vec![existing_server.clone()])
                .replay_from(v2::ReplayFrom::Start(
                    v2::ReplayFromStart::new().meta(meta()),
                ))
                .meta(meta()),
        )?;
        assert_eq!(resume_server_id, new_server_id);

        #[cfg(feature = "unstable_session_fork")]
        {
            let fork_server_id = assert_raw_append_preserves_params(
                &handler,
                "session/fork",
                v2::ForkSessionRequest::new(session_id, cwd)
                    .additional_directories([additional_directory])
                    .mcp_servers(vec![existing_server])
                    .meta(meta()),
            )?;
            assert_eq!(fork_server_id, new_server_id);
        }

        Ok(())
    }

    #[test]
    fn v2_global_mcp_handler_ignores_non_setup_requests() -> Result<(), Error> {
        let request = UntypedMessage::new(
            "session/prompt",
            json!({
                "sessionId": "session-to-prompt",
                "prompt": []
            }),
        )?;

        assert!(!V2McpSessionHandler::<Conductor>::validate_session_setup(
            &request
        )?);
        assert_eq!(request.method(), "session/prompt");
        Ok(())
    }
}
