use futures::channel::mpsc;
use futures::{SinkExt, StreamExt};
use fxhash::FxHashMap;

use sacp::schema::NewSessionRequest;
use sacp::{
    Channel, Component, DynComponent, Handled, JrConnectionCx, JrHandlerChain, JrMessage,
    JrMessageHandler, JrRequestCx, MessageAndCx, UntypedMessage,
};
use std::sync::{Arc, Mutex};

use crate::{
    JrCxExt, McpConnectRequest, McpConnectResponse, McpDisconnectNotification,
    McpOverAcpNotification, McpOverAcpRequest, SuccessorNotification, SuccessorRequest,
};

/// Manages MCP services offered to successor proxies and agents.
///
/// Use the [`Self::add_mcp_server`] method to register MCP servers. For rmcp-based servers,
/// use the `sacp-rmcp` crate which provides convenient extension methods.
///
/// This struct is a handle to the underlying registry. Cloning the struct produces a second handle to the same registry.
///
/// # Handling requests
///
/// You must add the registry (or a clone of it) to the [`JrHandlerChain`] so that it can intercept MCP requests.
/// Typically you do this by providing it as an argument to the handler chain methods.
///
/// [`JrHandlerChain`]: sacp::JrHandlerChain
#[derive(Clone, Default, Debug)]
pub struct McpServiceRegistry {
    data: Arc<Mutex<McpServiceRegistryData>>,
}

#[derive(Default, Debug)]
struct McpServiceRegistryData {
    registered_by_name: FxHashMap<String, Arc<RegisteredMcpServer>>,
    registered_by_url: FxHashMap<String, Arc<RegisteredMcpServer>>,
    connections: FxHashMap<String, mpsc::Sender<MessageAndCx>>,
}

impl McpServiceRegistry {
    /// Creates a new empty MCP service registry
    pub fn new() -> Self {
        Self::default()
    }

    /// Add an MCP server to the registry using a custom spawner.
    ///
    /// This is the base method for adding MCP servers. Use this if you have a custom
    /// way to create Component instances for your MCP server.
    ///
    /// For rmcp-based servers, use the `sacp-rmcp` crate which provides convenient
    /// extension methods.
    ///
    /// # Parameters
    ///
    /// - `name`: The name of the server.
    /// - `spawner`: A trait object that can create Component instances.
    pub fn add_mcp_server<C: Component>(
        &self,
        name: impl ToString,
        new_fn: impl Fn() -> C + Send + Sync + 'static,
    ) -> Result<(), sacp::Error> {
        struct FnSpawner<F> {
            new_fn: F,
        }

        impl<C, F> SpawnMcpServer for FnSpawner<F>
        where
            F: Fn() -> C + Send + Sync + 'static,
            C: Component,
        {
            fn spawn(&self) -> DynComponent {
                let component = (self.new_fn)();
                DynComponent::new(component)
            }
        }

        let name = name.to_string();
        if let Some(_) = self.get_registered_server_by_name(&name) {
            return Err(sacp::util::internal_error(format!(
                "Server with name '{}' already exists",
                name
            )));
        }

        let uuid = uuid::Uuid::new_v4().to_string();
        let service = Arc::new(RegisteredMcpServer {
            name,
            url: format!("acp:{uuid}"),
            spawn: Arc::new(FnSpawner { new_fn }),
        });
        self.insert_registered_server(service);
        Ok(())
    }

    fn insert_registered_server(&self, service: Arc<RegisteredMcpServer>) {
        let mut data = self.data.lock().expect("not poisoned");
        data.registered_by_name
            .insert(service.name.clone(), service.clone());
        data.registered_by_url
            .insert(service.url.clone(), service.clone());
    }

    fn get_registered_server_by_name(&self, name: &str) -> Option<Arc<RegisteredMcpServer>> {
        self.data
            .lock()
            .expect("not poisoned")
            .registered_by_name
            .get(name)
            .cloned()
    }

    fn get_registered_server_by_url(&self, url: &str) -> Option<Arc<RegisteredMcpServer>> {
        self.data
            .lock()
            .expect("not poisoned")
            .registered_by_url
            .get(url)
            .cloned()
    }

    fn insert_connection(&self, connection_id: &str, tx: mpsc::Sender<sacp::MessageAndCx>) {
        self.data
            .lock()
            .expect("not poisoned")
            .connections
            .insert(connection_id.to_string(), tx);
    }

    fn get_connection(&self, connection_id: &str) -> Option<mpsc::Sender<sacp::MessageAndCx>> {
        self.data
            .lock()
            .expect("not poisoned")
            .connections
            .get(connection_id)
            .cloned()
    }

    fn remove_connection(&self, connection_id: &str) -> bool {
        self.data
            .lock()
            .expect("not poisoned")
            .connections
            .remove(connection_id)
            .is_some()
    }

    async fn handle_connect_request(
        &self,
        result: Result<SuccessorRequest<McpConnectRequest>, sacp::Error>,
        request_cx: JrRequestCx<serde_json::Value>,
    ) -> Result<Handled<JrRequestCx<serde_json::Value>>, sacp::Error> {
        // Check if we parsed this message successfully.
        let SuccessorRequest { request } = match result {
            Ok(request) => request,
            Err(err) => {
                request_cx.respond_with_error(err)?;
                return Ok(Handled::Yes);
            }
        };

        // Check if we have a registered server with the given URL. If not, don't try to handle the request.
        let Some(registered_server) = self.get_registered_server_by_url(&request.acp_url) else {
            return Ok(Handled::No(request_cx));
        };

        let request_cx = request_cx.cast::<McpConnectResponse>();

        // Create a unique connection ID and a channel for future communication
        let connection_id = format!("mcp-over-acp-connection:{}", uuid::Uuid::new_v4());
        let (mcp_server_tx, mut mcp_server_rx) = mpsc::channel(128);
        self.insert_connection(&connection_id, mcp_server_tx);

        // Create connected channel pair for client-server communication
        let (client_channel, server_channel) = Channel::duplex();

        // Create client-side handler that wraps messages and forwards to successor
        let client_component = {
            let connection_id = connection_id.clone();
            let outer_cx = request_cx.connection_cx();

            JrHandlerChain::new()
                .on_receive_message(async move |message: sacp::MessageAndCx| {
                    // Wrap the message in McpOverAcp{Request,Notification} and forward to successor
                    let wrapped = message.map(
                        |request, request_cx| {
                            (
                                McpOverAcpRequest {
                                    connection_id: connection_id.clone(),
                                    request,
                                },
                                request_cx,
                            )
                        },
                        |notification, cx| {
                            (
                                McpOverAcpNotification {
                                    connection_id: connection_id.clone(),
                                    notification,
                                },
                                cx,
                            )
                        },
                    );
                    outer_cx.send_proxied_message(wrapped)
                })
                .with_spawned(move |mcp_cx| async move {
                    while let Some(msg) = mcp_server_rx.next().await {
                        mcp_cx.send_proxied_message(msg)?;
                    }
                    Ok(())
                })
        };

        // Get the MCP server component
        let mcp_server = registered_server.spawn.spawn();

        // Spawn both sides of the connection
        let spawn_results = request_cx
            .connection_cx()
            .spawn(async move { client_component.serve(client_channel).await })
            .and_then(|()| {
                // Spawn the MCP server serving the server channel
                request_cx
                    .connection_cx()
                    .spawn(async move { mcp_server.serve(server_channel).await })
            });

        match spawn_results {
            Ok(()) => {
                request_cx.respond(McpConnectResponse { connection_id })?;
                Ok(Handled::Yes)
            }

            Err(err) => {
                request_cx.respond_with_error(err)?;
                Ok(Handled::Yes)
            }
        }
    }

    async fn handle_mcp_over_acp_request(
        &self,
        result: Result<SuccessorRequest<McpOverAcpRequest<UntypedMessage>>, sacp::Error>,
        request_cx: JrRequestCx<serde_json::Value>,
    ) -> Result<Handled<JrRequestCx<serde_json::Value>>, sacp::Error> {
        // Check if we parsed this message successfully.
        let SuccessorRequest { request } = match result {
            Ok(request) => request,
            Err(err) => {
                request_cx.respond_with_error(err)?;
                return Ok(Handled::Yes);
            }
        };

        // Check if we have a registered server with the given URL. If not, don't try to handle the request.
        let Some(mut mcp_server_tx) = self.get_connection(&request.connection_id) else {
            return Ok(Handled::No(request_cx));
        };

        mcp_server_tx
            .send(MessageAndCx::Request(request.request, request_cx))
            .await
            .map_err(sacp::Error::into_internal_error)?;

        Ok(Handled::Yes)
    }

    async fn handle_mcp_over_acp_notification(
        &self,
        result: Result<SuccessorNotification<McpOverAcpNotification<UntypedMessage>>, sacp::Error>,
        notification_cx: JrConnectionCx,
    ) -> Result<Handled<JrConnectionCx>, sacp::Error> {
        // Check if we parsed this message successfully.
        let SuccessorNotification { notification } = match result {
            Ok(request) => request,
            Err(err) => {
                notification_cx.send_error_notification(err)?;
                return Ok(Handled::Yes);
            }
        };

        // Check if we have a registered server with the given URL. If not, don't try to handle the request.
        let Some(mut mcp_server_tx) = self.get_connection(&notification.connection_id) else {
            return Ok(Handled::No(notification_cx));
        };

        mcp_server_tx
            .send(MessageAndCx::Notification(
                notification.notification,
                notification_cx.clone(),
            ))
            .await
            .map_err(sacp::Error::into_internal_error)?;

        Ok(Handled::Yes)
    }

    async fn handle_mcp_disconnect_notification(
        &self,
        result: Result<SuccessorNotification<McpDisconnectNotification>, sacp::Error>,
        notification_cx: JrConnectionCx,
    ) -> Result<Handled<JrConnectionCx>, sacp::Error> {
        // Check if we parsed this message successfully.
        let SuccessorNotification { notification } = match result {
            Ok(request) => request,
            Err(err) => {
                notification_cx.send_error_notification(err)?;
                return Ok(Handled::Yes);
            }
        };

        // Remove connection if we have it. Otherwise, do not handle the notification.
        if self.remove_connection(&notification.connection_id) {
            Ok(Handled::Yes)
        } else {
            Ok(Handled::No(notification_cx))
        }
    }

    async fn handle_new_session_request(
        &self,
        result: Result<NewSessionRequest, sacp::Error>,
        request_cx: JrRequestCx<serde_json::Value>,
    ) -> Result<Handled<JrRequestCx<serde_json::Value>>, sacp::Error> {
        // Check if we parsed this message successfully.
        let mut request = match result {
            Ok(request) => request,
            Err(err) => {
                request_cx.connection_cx().send_error_notification(err)?;
                return Ok(Handled::Yes);
            }
        };

        // Add the MCP servers into the session/new request.
        //
        // Q: Do we care if there are already servers with that name?
        {
            let data = self.data.lock().expect("not poisoned");
            for server in data.registered_by_url.values() {
                request.mcp_servers.push(server.acp_mcp_server());
            }
        }

        // Forward it to the successor.
        request_cx
            .connection_cx()
            .send_request_to_successor(request)
            .forward_to_request_cx(request_cx.cast())?;

        Ok(Handled::Yes)
    }
}

impl JrMessageHandler for McpServiceRegistry {
    fn describe_chain(&self) -> impl std::fmt::Debug {
        "McpServiceRegistry"
    }

    async fn handle_message(
        &mut self,
        message: sacp::MessageAndCx,
    ) -> Result<sacp::Handled<sacp::MessageAndCx>, sacp::Error> {
        match message {
            sacp::MessageAndCx::Request(msg, mut cx) => {
                let params = msg.params();

                if let Some(result) =
                    <SuccessorRequest<McpConnectRequest>>::parse_request(cx.method(), params)
                {
                    cx = match self.handle_connect_request(result, cx).await? {
                        Handled::Yes => return Ok(Handled::Yes),
                        Handled::No(cx) => cx,
                    };
                }

                if let Some(result) =
                    <SuccessorRequest<McpOverAcpRequest<UntypedMessage>>>::parse_request(
                        cx.method(),
                        params,
                    )
                {
                    cx = match self.handle_mcp_over_acp_request(result, cx).await? {
                        Handled::Yes => return Ok(Handled::Yes),
                        Handled::No(cx) => cx,
                    };
                }

                if let Some(result) = <NewSessionRequest>::parse_request(cx.method(), params) {
                    cx = match self.handle_new_session_request(result, cx).await? {
                        Handled::Yes => return Ok(Handled::Yes),
                        Handled::No(cx) => cx,
                    };
                }

                Ok(Handled::No(sacp::MessageAndCx::Request(msg, cx)))
            }
            sacp::MessageAndCx::Notification(msg, mut cx) => {
                let params = msg.params();

                if let Some(result) =
                    <SuccessorNotification<McpOverAcpNotification<UntypedMessage>>>::parse_notification(
                        msg.method(),
                        params,
                    )
                {
                    cx = match self.handle_mcp_over_acp_notification(result, cx).await? {
                        Handled::Yes => return Ok(Handled::Yes),
                        Handled::No(cx) => cx,
                    };
                }

                if let Some(result) =
                    <SuccessorNotification<McpDisconnectNotification>>::parse_notification(
                        msg.method(),
                        params,
                    )
                {
                    cx = match self.handle_mcp_disconnect_notification(result, cx).await? {
                        Handled::Yes => return Ok(Handled::Yes),
                        Handled::No(cx) => cx,
                    };
                }

                Ok(sacp::Handled::No(sacp::MessageAndCx::Notification(msg, cx)))
            }
        }
    }
}

#[derive(Clone)]
struct RegisteredMcpServer {
    name: String,
    url: String,
    spawn: Arc<dyn SpawnMcpServer>,
}

impl RegisteredMcpServer {
    fn acp_mcp_server(&self) -> sacp::schema::McpServer {
        sacp::schema::McpServer::Http {
            name: self.name.clone(),
            url: self.url.clone(),
            headers: vec![],
        }
    }
}

impl std::fmt::Debug for RegisteredMcpServer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RegisteredMcpServer")
            .field("name", &self.name)
            .field("url", &self.url)
            .finish()
    }
}

/// Trait for spawning MCP server components.
///
/// This trait allows creating MCP server instances that implement the `Component` trait.
trait SpawnMcpServer: Send + Sync + 'static {
    /// Create a new MCP server component.
    ///
    /// Returns a `DynComponent` that can be used with the Component API.
    fn spawn(&self) -> sacp::DynComponent;
}

impl AsRef<McpServiceRegistry> for McpServiceRegistry {
    fn as_ref(&self) -> &McpServiceRegistry {
        self
    }
}
