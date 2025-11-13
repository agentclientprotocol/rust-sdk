//! Core JSON-RPC server support.

// Re-export jsonrpcmsg for use in public API
pub use jsonrpcmsg;

// Types re-exported from crate root
use serde::{Deserialize, Serialize};
use std::fmt::Debug;
use std::panic::Location;

use boxfnonce::SendBoxFnOnce;
use futures::channel::{mpsc, oneshot};
use futures::future::{self, BoxFuture, Either};
use futures::{AsyncRead, AsyncWrite, FutureExt, StreamExt};

mod actors;
pub(crate) mod handlers;
mod task_actor;

use crate::Component;
use crate::handler::{ChainedHandler, NamedHandler};
use crate::jsonrpc::handlers::{MessageHandler, NotificationHandler, NullHandler, RequestHandler};
use crate::jsonrpc::task_actor::{PendingTask, Task};

/// Handlers are invoked when new messages arrive at the [`JrConnection`].
/// They have a chance to inspect the method and parameters and decide whether to "claim" the request
/// (i.e., handle it). If they do not claim it, the request will be passed to the next handler.
#[allow(async_fn_in_trait)]
pub trait JrMessageHandler {
    /// Attempt to claim an incoming message (request or notification).
    ///
    /// # Important: do not block
    ///
    /// The server will not process new messages until this handler returns.
    /// You should avoid blocking in this callback unless you wish to block the server (e.g., for rate limiting).
    /// The recommended approach to manage expensive operations is to the [`JrConnectionCx::spawn`] method available on the message context.
    ///
    /// # Parameters
    ///
    /// * `cx` - The context of the request. This gives access to the request ID and the method name and is used to send a reply; can also be used to send other messages to the other party.
    /// * `params` - The parameters of the request.
    ///
    /// # Returns
    ///
    /// * `Ok(Handled::Yes)` if the message was claimed. It will not be propagated further.
    /// * `Ok(Handled::No(message))` if not; the (possibly changed) message will be passed to the remaining handlers.
    /// * `Err` if an internal error occurs (this will bring down the server).
    async fn handle_message(
        &mut self,
        message: MessageAndCx,
    ) -> Result<Handled<MessageAndCx>, crate::Error>;

    /// Returns a debug description of the handler chain for diagnostics
    fn describe_chain(&self) -> impl std::fmt::Debug;
}

/// A JSON-RPC connection that can act as either a server, client, or both.
///
/// `JrConnection` provides a builder-style API for creating JSON-RPC servers and clients.
/// You start by calling [`JrHandlerChain::new`], then add message handlers, and finally
/// drive the connection with either [`serve`](JrHandlerChain::serve) or [`with_client`](JrHandlerChain::with_client),
/// providing a component implementation (e.g., [`ByteStreams`] for byte streams).
///
/// # JSON-RPC Primer
///
/// JSON-RPC 2.0 has two fundamental message types:
///
/// * **Requests** - Messages that expect a response. They have an `id` field that gets
///   echoed back in the response so the sender can correlate them.
/// * **Notifications** - Fire-and-forget messages with no `id` field. The sender doesn't
///   expect or receive a response.
///
/// # Type-Driven Message Dispatch
///
/// The handler registration methods use Rust's type system to determine which messages
/// to handle. The type parameter you provide controls what gets dispatched to your handler:
///
/// ## Single Message Types
///
/// The simplest case - handle one specific message type:
///
/// ```no_run
/// # use sacp_test::*;
/// # use sacp::schema::{InitializeRequest, InitializeResponse, SessionNotification};
/// # async fn example() -> Result<(), sacp::Error> {
/// # let connection = mock_connection();
/// connection
///     .on_receive_request(async |req: InitializeRequest, cx| {
///         // Handle only InitializeRequest messages
///         cx.respond(InitializeResponse::make())
///     })
///     .on_receive_notification(async |notif: SessionNotification, cx| {
///         // Handle only SessionUpdate notifications
///         Ok(())
///     })
/// # .serve(sacp_test::MockTransport).await?;
/// # Ok(())
/// # }
/// ```
///
/// ## Enum Message Types
///
/// You can also handle multiple related messages with a single handler by defining an enum
/// that implements the appropriate trait ([`JrRequest`] or [`JrNotification`]):
///
/// ```no_run
/// # use sacp_test::*;
/// # use sacp::{JrRequest, JrMessage, UntypedMessage};
/// # use sacp::schema::{InitializeRequest, InitializeResponse, PromptRequest, PromptResponse};
/// # async fn example() -> Result<(), sacp::Error> {
/// # let connection = mock_connection();
/// // Define an enum for multiple request types
/// #[derive(Debug)]
/// enum MyRequests {
///     Initialize(InitializeRequest),
///     Prompt(PromptRequest),
/// }
///
/// // Implement JrRequest for your enum
/// # impl JrMessage for MyRequests {
/// #     fn method(&self) -> &str { "myRequests" }
/// #     fn into_untyped_message(self) -> Result<UntypedMessage, sacp::Error> { todo!() }
/// #     fn parse_request(_method: &str, _params: &impl serde::Serialize) -> Option<Result<Self, sacp::Error>> { None }
/// #     fn parse_notification(_method: &str, _params: &impl serde::Serialize) -> Option<Result<Self, sacp::Error>> { None }
/// # }
/// impl JrRequest for MyRequests { type Response = serde_json::Value; }
///
/// // Handle all variants in one place
/// connection.on_receive_request(async |req: MyRequests, cx| {
///     match req {
///         MyRequests::Initialize(init) => { cx.respond(serde_json::json!({})) }
///         MyRequests::Prompt(prompt) => { cx.respond(serde_json::json!({})) }
///     }
/// })
/// # .serve(sacp_test::MockTransport).await?;
/// # Ok(())
/// # }
/// ```
///
/// ## Mixed Message Types
///
/// For enums containing both requests AND notifications, use [`on_receive_message`](Self::on_receive_message):
///
/// ```no_run
/// # use sacp_test::*;
/// # use sacp::MessageAndCx;
/// # use sacp::schema::{InitializeRequest, InitializeResponse, SessionNotification};
/// # async fn example() -> Result<(), sacp::Error> {
/// # let connection = mock_connection();
/// // on_receive_message receives MessageAndCx which can be either a request or notification
/// connection.on_receive_message(async |msg: MessageAndCx<InitializeRequest, SessionNotification>| {
///     match msg {
///         MessageAndCx::Request(req, request_cx) => {
///             request_cx.respond(InitializeResponse::make())
///         }
///         MessageAndCx::Notification(notif, _cx) => {
///             Ok(())
///         }
///     }
/// })
/// # .serve(sacp_test::MockTransport).await?;
/// # Ok(())
/// # }
/// ```
///
/// # Handler Registration
///
/// Register handlers using these methods (listed from most common to most flexible):
///
/// * [`on_receive_request`](Self::on_receive_request) - Handle JSON-RPC requests (messages expecting responses)
/// * [`on_receive_notification`](Self::on_receive_notification) - Handle JSON-RPC notifications (fire-and-forget)
/// * [`on_receive_message`](Self::on_receive_message) - Handle enums containing both requests and notifications
/// * [`with_handler`](Self::with_handler) - Low-level primitive for maximum flexibility
///
/// ## Handler Ordering
///
/// Handlers are tried in the order you register them. The first handler that claims a message
/// (by matching its type) will process it. Subsequent handlers won't see that message:
///
/// ```no_run
/// # use sacp_test::*;
/// # use sacp::{MessageAndCx, UntypedMessage};
/// # use sacp::schema::{InitializeRequest, InitializeResponse, PromptRequest, PromptResponse};
/// # async fn example() -> Result<(), sacp::Error> {
/// # let connection = mock_connection();
/// connection
///     .on_receive_request(async |req: InitializeRequest, cx| {
///         // This runs first for InitializeRequest
///         cx.respond(InitializeResponse::make())
///     })
///     .on_receive_request(async |req: PromptRequest, cx| {
///         // This runs first for PromptRequest
///         cx.respond(PromptResponse::make())
///     })
///     .on_receive_message(async |msg: MessageAndCx| {
///         // This runs for any message not handled above
///         msg.respond_with_error(sacp::util::internal_error("unknown method"))
///     })
/// # .serve(sacp_test::MockTransport).await?;
/// # Ok(())
/// # }
/// ```
///
/// # Event Loop and Concurrency
///
/// Understanding the event loop is critical for writing correct handlers.
///
/// ## The Event Loop
///
/// `JrConnection` runs all handler callbacks on a single async task - the event loop.
/// While a handler is running, **the server cannot receive new messages**. This means
/// any blocking or expensive work in your handlers will stall the entire connection.
///
/// To avoid blocking the event loop, use [`JrConnectionCx::spawn`] to offload serious
/// work to concurrent tasks:
///
/// ```no_run
/// # use sacp_test::*;
/// # async fn example() -> Result<(), sacp::Error> {
/// # let connection = mock_connection();
/// connection.on_receive_request(async |req: AnalyzeRequest, cx| {
///     // Clone cx for the spawned task
///     cx.connection_cx().spawn({
///         let connection_cx = cx.connection_cx();
///         async move {
///             let result = expensive_analysis(&req.data).await?;
///             connection_cx.send_notification(AnalysisComplete { result })?;
///             Ok(())
///         }
///     })?;
///
///     // Respond immediately without blocking
///     cx.respond(AnalysisStarted { job_id: 42 })
/// })
/// # .serve(sacp_test::MockTransport).await?;
/// # Ok(())
/// # }
/// ```
///
/// Note that the entire connection runs within one async task, so parallelism must be
/// managed explicitly using [`spawn`](JrConnectionCx::spawn).
///
/// ## The Connection Context
///
/// Handler callbacks receive a context object (`cx`) for interacting with the connection:
///
/// * **For request handlers** - [`JrRequestCx<R>`] provides [`respond`](JrRequestCx::respond)
///   to send the response, plus methods to send other messages
/// * **For notification handlers** - [`JrConnectionCx`] provides methods to send messages
///   and spawn tasks
///
/// Both context types support:
/// * [`send_request`](JrConnectionCx::send_request) - Send requests to the other side
/// * [`send_notification`](JrConnectionCx::send_notification) - Send notifications
/// * [`spawn`](JrConnectionCx::spawn) - Run tasks concurrently without blocking the event loop
///
/// The [`JrResponse`] returned by `send_request` provides methods like
/// [`await_when_result_received`](JrResponse::await_when_result_received) that help you
/// avoid accidentally blocking the event loop while waiting for responses.
///
/// # Driving the Connection
///
/// After adding handlers, you must drive the connection using one of two modes:
///
/// ## Server Mode: `serve()`
///
/// Use [`serve`](Self::serve) when you only need to respond to incoming messages:
///
/// ```no_run
/// # use sacp_test::*;
/// # async fn example() -> Result<(), sacp::Error> {
/// # let connection = mock_connection();
/// connection
///     .on_receive_request(async |req: MyRequest, cx| {
///         cx.respond(MyResponse { status: "ok".into() })
///     })
///     .serve(MockTransport)  // Runs until connection closes or error occurs
///     .await?;
/// # Ok(())
/// # }
/// ```
///
/// The connection will process incoming messages and invoke your handlers until the
/// connection is closed or an error occurs.
///
/// ## Client Mode: `with_client()`
///
/// Use [`with_client`](Self::with_client) when you need to both handle incoming messages
/// AND send your own requests/notifications:
///
/// ```no_run
/// # use sacp_test::*;
/// # use sacp::schema::InitializeRequest;
/// # async fn example() -> Result<(), sacp::Error> {
/// # let connection = mock_connection();
/// connection
///     .on_receive_request(async |req: MyRequest, cx| {
///         cx.respond(MyResponse { status: "ok".into() })
///     })
///     .with_client(MockTransport, async |cx| {
///         // You can send requests to the other side
///         let response = cx.send_request(InitializeRequest::make())
///             .block_task()
///             .await?;
///
///         // And send notifications
///         cx.send_notification(StatusUpdate { message: "ready".into() })?;
///
///         Ok(())
///     })
///     .await?;
/// # Ok(())
/// # }
/// ```
///
/// The connection will serve incoming messages in the background while your client closure
/// runs. When the closure returns, the connection shuts down.
///
/// # Example: Complete Agent
///
/// ```no_run
/// # use sacp::JrHandlerChain;
/// # use sacp::ByteStreams;
/// # use sacp::schema::{InitializeRequest, InitializeResponse, PromptRequest, PromptResponse, SessionNotification};
/// # use tokio_util::compat::{TokioAsyncReadCompatExt, TokioAsyncWriteCompatExt};
/// # async fn example() -> Result<(), sacp::Error> {
/// let transport = ByteStreams::new(
///     tokio::io::stdout().compat_write(),
///     tokio::io::stdin().compat(),
/// );
///
/// JrHandlerChain::new()
///     .name("my-agent")  // Optional: for debugging logs
///     .on_receive_request(async |init: InitializeRequest, cx| {
///         let response: InitializeResponse = todo!();
///         cx.respond(response)
///     })
///     .on_receive_request(async |prompt: PromptRequest, cx| {
///         // You can send notifications while processing a request
///         let notif: SessionNotification = todo!();
///         cx.connection_cx().send_notification(notif)?;
///
///         // Then respond to the request
///         let response: PromptResponse = todo!();
///         cx.respond(response)
///     })
///     .serve(transport)
///     .await?;
/// # Ok(())
/// # }
/// ```
#[must_use]
pub struct JrHandlerChain<H: JrMessageHandler> {
    name: Option<String>,

    /// Handler for incoming messages.
    handler: H,

    /// Pending tasks
    pending_tasks: Vec<PendingTask>,
}

impl JrHandlerChain<NullHandler> {
    /// Create a new JrConnection.
    /// This type follows a builder pattern; use other methods to configure and then invoke
    /// [`Self::serve`] (to use as a server) or [`Self::with_client`] to use as a client.
    pub fn new() -> Self {
        Self::new_with(NullHandler::default())
    }
}

impl<H: JrMessageHandler> JrHandlerChain<H> {
    /// Create a new handler chain with the given handler.
    pub fn new_with(handler: H) -> Self {
        Self {
            name: Default::default(),
            handler,
            pending_tasks: Default::default(),
        }
    }

    /// Set the "name" of this connection -- used only for debugging logs.
    pub fn name(mut self, name: impl ToString) -> Self {
        self.name = Some(name.to_string());
        self
    }

    /// Add a new [`JrMessageHandler`] to the chain.
    ///
    /// Prefer [`Self::on_receive_request`] or [`Self::on_receive_notification`].
    /// This is a low-level method that is not intended for general use.
    pub fn with_handler_chain<H1>(
        mut self,
        handler_chain: JrHandlerChain<H1>,
    ) -> JrHandlerChain<ChainedHandler<H, NamedHandler<H1>>>
    where
        H1: JrMessageHandler,
    {
        self.pending_tasks.extend(
            handler_chain
                .pending_tasks
                .into_iter()
                .map(|t| t.named(handler_chain.name.clone())),
        );

        JrHandlerChain {
            name: self.name,
            handler: ChainedHandler::new(
                self.handler,
                NamedHandler::new(handler_chain.name, handler_chain.handler),
            ),
            pending_tasks: self.pending_tasks,
        }
    }

    /// Add a new [`JrMessageHandler`] to the chain.
    ///
    /// Prefer [`Self::on_receive_request`] or [`Self::on_receive_notification`].
    /// This is a low-level method that is not intended for general use.
    pub fn with_handler<H1>(self, handler: H1) -> JrHandlerChain<ChainedHandler<H, H1>>
    where
        H1: JrMessageHandler,
    {
        JrHandlerChain {
            name: self.name,
            handler: ChainedHandler::new(self.handler, handler),
            pending_tasks: self.pending_tasks,
        }
    }

    /// Enqueue a task to run once the connection is actively serving traffic.
    #[track_caller]
    pub fn with_spawned<F>(
        mut self,
        task: impl FnOnce(JrConnectionCx) -> F + Send + 'static,
    ) -> Self
    where
        F: Future<Output = Result<(), crate::Error>> + Send + 'static,
    {
        let location = Location::caller();
        self.pending_tasks.push(PendingTask::new(location, task));
        self
    }

    /// Register a handler for messages that can be either requests OR notifications.
    ///
    /// Use this when you want to handle an enum type that contains both request and
    /// notification variants. Your handler receives a [`MessageAndCx<R, N>`] which
    /// is an enum with two variants:
    ///
    /// - `MessageAndCx::Request(request, request_cx)` - A request with its response context
    /// - `MessageAndCx::Notification(notification, cx)` - A notification with the connection context
    ///
    /// # Example
    ///
    /// ```no_run
    /// # use sacp_test::*;
    /// # use sacp::MessageAndCx;
    /// # async fn example() -> Result<(), sacp::Error> {
    /// # let connection = mock_connection();
    /// connection.on_receive_message(async |message: MessageAndCx<MyRequest, StatusUpdate>| {
    ///     match message {
    ///         MessageAndCx::Request(req, request_cx) => {
    ///             // Handle request and send response
    ///             request_cx.respond(MyResponse { status: "ok".into() })
    ///         }
    ///         MessageAndCx::Notification(notif, _cx) => {
    ///             // Handle notification (no response needed)
    ///             Ok(())
    ///         }
    ///     }
    /// })
    /// # .serve(sacp_test::MockTransport).await?;
    /// # Ok(())
    /// # }
    /// ```
    ///
    /// For most use cases, prefer [`on_receive_request`](Self::on_receive_request) or
    /// [`on_receive_notification`](Self::on_receive_notification) which provide cleaner APIs
    /// for handling requests or notifications separately.
    pub fn on_receive_message<R, N, F>(
        self,
        op: F,
    ) -> JrHandlerChain<ChainedHandler<H, MessageHandler<R, N, F>>>
    where
        R: JrRequest,
        N: JrNotification,
        F: AsyncFnMut(MessageAndCx<R, N>) -> Result<(), crate::Error> + Send,
    {
        self.with_handler(MessageHandler::new(op))
    }

    /// Register a handler for JSON-RPC requests of type `R`.
    ///
    /// Your handler receives two arguments:
    /// 1. The request (type `R`)
    /// 2. A [`JrRequestCx<R::Response>`] for sending the response
    ///
    /// The request context allows you to:
    /// - Send the response with [`JrRequestCx::respond`]
    /// - Send notifications to the client with [`JrConnectionCx::send_notification`]
    /// - Send requests to the client with [`JrConnectionCx::send_request`]
    ///
    /// # Example
    ///
    /// ```
    /// # use sacp::JrHandlerChain;
    /// # use sacp::schema::{PromptRequest, PromptResponse, SessionNotification};
    /// # fn example(connection: JrHandlerChain<impl sacp::JrMessageHandler>) {
    /// connection.on_receive_request(async |request: PromptRequest, request_cx| {
    ///     // Send a notification while processing
    ///     let notif: SessionNotification = todo!();
    ///     request_cx.connection_cx().send_notification(notif)?;
    ///
    ///     // Do some work...
    ///     let result = todo!("process the prompt");
    ///
    ///     // Send the response
    ///     let response: PromptResponse = todo!();
    ///     request_cx.respond(response)
    /// });
    /// # }
    /// ```
    ///
    /// # Type Parameter
    ///
    /// `R` can be either a single request type or an enum of multiple request types.
    /// See the [type-driven dispatch](Self#type-driven-message-dispatch) section for details.
    pub fn on_receive_request<R, F, T>(
        self,
        op: F,
    ) -> JrHandlerChain<ChainedHandler<H, RequestHandler<R, F>>>
    where
        R: JrRequest,
        F: AsyncFnMut(R, JrRequestCx<R::Response>) -> Result<T, crate::Error> + Send,
        T: IntoHandled<(R, JrRequestCx<R::Response>)>,
    {
        self.with_handler(RequestHandler::new(op))
    }

    /// Register a handler for JSON-RPC notifications of type `N`.
    ///
    /// Notifications are fire-and-forget messages that don't expect a response.
    /// Your handler receives:
    /// 1. The notification (type `N`)
    /// 2. A [`JrConnectionCx`] for sending messages to the other side
    ///
    /// Unlike request handlers, you cannot send a response (notifications don't have IDs),
    /// but you can still send your own requests and notifications using the context.
    ///
    /// # Example
    ///
    /// ```no_run
    /// # use sacp_test::*;
    /// # async fn example() -> Result<(), sacp::Error> {
    /// # let connection = mock_connection();
    /// connection.on_receive_notification(async |notif: SessionUpdate, cx| {
    ///     // Process the notification
    ///     update_session_state(&notif)?;
    ///
    ///     // Optionally send a notification back
    ///     cx.send_notification(StatusUpdate {
    ///         message: "Acknowledged".into(),
    ///     })?;
    ///
    ///     Ok(())
    /// })
    /// # .serve(sacp_test::MockTransport).await?;
    /// # Ok(())
    /// # }
    /// ```
    ///
    /// # Type Parameter
    ///
    /// `N` can be either a single notification type or an enum of multiple notification types.
    /// See the [type-driven dispatch](Self#type-driven-message-dispatch) section for details.
    pub fn on_receive_notification<N, F, T>(
        self,
        op: F,
    ) -> JrHandlerChain<ChainedHandler<H, NotificationHandler<N, F>>>
    where
        N: JrNotification,
        F: AsyncFnMut(N, JrConnectionCx) -> Result<T, crate::Error> + Send,
        T: IntoHandled<(N, JrConnectionCx)>,
    {
        self.with_handler(NotificationHandler::new(op))
    }

    /// Connect these handlers to a transport layer.
    /// The resulting connection must then be either [served](`JrConnection::serve`) or [used as a client](`JrConnection::with_client`).
    pub fn connect_to(
        self,
        transport: impl Component + 'static,
    ) -> Result<JrConnection<H>, crate::Error> {
        let Self {
            name,
            handler,
            pending_tasks,
        } = self;

        let (outgoing_tx, outgoing_rx) = mpsc::unbounded();
        let (new_task_tx, new_task_rx) = mpsc::unbounded();
        let cx = JrConnectionCx::new(outgoing_tx, new_task_tx);

        // Convert transport into server - this returns a channel for us to use
        // and a future that runs the transport
        let transport_component = crate::DynComponent::new(transport);
        let (transport_channel, transport_future) = transport_component.into_server();
        cx.spawn(transport_future)?;

        // Destructure the channel endpoints
        let Channel {
            rx: transport_incoming_rx,
            tx: transport_outgoing_tx,
        } = transport_channel;

        // Spawn pending tasks
        for pending_task in pending_tasks {
            let task = pending_task.into_task(cx.clone());
            cx.spawn_task(task)?;
        }

        Ok(JrConnection {
            cx,
            name,
            outgoing_rx,
            new_task_rx,
            transport_outgoing_tx,
            transport_incoming_rx,
            handler,
        })
    }

    /// Apply the handler chain to a single message.
    ///
    /// This method processes one message through the entire handler chain, attempting to
    /// match it against each registered handler in order. This is useful when implementing
    /// custom message handling logic or when you need fine-grained control over message
    /// processing.
    ///
    /// # Returns
    ///
    /// - `Ok(Handled::Claimed)` - A handler claimed and processed the message
    /// - `Ok(Handled::Unclaimed(message))` - No handler matched the message
    /// - `Err(_)` - A handler encountered an error while processing
    ///
    /// # Borrow Checker Considerations
    ///
    /// You may find that [`MatchMessage`] is a better choice than this method
    /// for implementing custom handlers. It offers a very similar API to
    /// [`JrHandlerChain`] but is structured to apply each test one at a time
    /// (sequentially) instead of setting them all up at once. This sequential approach
    /// often interacts better with the borrow checker, at the cost of requiring `.await`
    /// calls between each handler and only working for processing a single message.
    ///
    /// # Example: Borrow Checker Challenges
    ///
    /// When building a handler chain with `async {}` blocks (non-move), you might encounter
    /// borrow checker errors if multiple handlers need access to the same mutable state:
    ///
    /// ```compile_fail
    /// # use sacp::{JrHandlerChain, JrRequestCx};
    /// # use sacp::schema::{InitializeRequest, InitializeResponse};
    /// # use sacp::schema::{PromptRequest, PromptResponse};
    /// # async fn example() -> Result<(), sacp::Error> {
    /// let mut state = String::from("initial");
    ///
    /// // This fails to compile because both handlers borrow `state` mutably,
    /// // and the futures are set up at the same time (even though only one will run)
    /// let chain = JrHandlerChain::new()
    ///     .on_receive_request(async |req: InitializeRequest, cx: JrRequestCx| {
    ///         state.push_str(" - initialized");  // First mutable borrow
    ///         cx.respond(InitializeResponse::make())
    ///     })
    ///     .on_receive_request(async |req: PromptRequest, cx: JrRequestCx| {
    ///         state.push_str(" - prompted");  // Second mutable borrow - ERROR!
    ///         cx.respond(PromptResponse { content: vec![], stopReason: None })
    ///     });
    /// # Ok(())
    /// # }
    /// ```
    ///
    /// You can work around this by using `apply()` to process messages one at a time,
    /// or use [`MatchMessage`] which provides a similar API but applies handlers sequentially:
    ///
    /// ```ignore
    /// use sacp::{MessageAndCx, Handled};
    /// use sacp::util::MatchMessage;
    ///
    /// async fn handle_with_state(
    ///     message: MessageAndCx,
    ///     state: &mut String,
    /// ) -> Result<Handled<MessageAndCx>, sacp::Error> {
    ///     MatchMessage::new(message)
    ///         .on_request(async |req: InitializeRequest, cx| {
    ///             state.push_str(" - initialized");  // Sequential - OK!
    ///             cx.respond(InitializeResponse::make())
    ///         })
    ///         .on_request(async |req: PromptRequest, cx| {
    ///             state.push_str(" - prompted");  // Sequential - OK!
    ///             cx.respond(PromptResponse { content: vec![], stopReason: None })
    ///         })
    ///         .otherwise(async |msg| Ok(Handled::Unclaimed(msg)))
    ///         .await
    /// }
    /// ```
    pub async fn apply(
        &mut self,
        message: MessageAndCx,
    ) -> Result<Handled<MessageAndCx>, crate::Error> {
        self.handler.handle_message(message).await
    }

    /// Convenience method to connect to a transport and serve.
    ///
    /// This is equivalent to:
    /// ```ignore
    /// handler_chain.connect_to(transport)?.serve().await
    /// ```
    pub async fn serve(self, transport: impl Component + 'static) -> Result<(), crate::Error> {
        self.connect_to(transport)?.serve().await
    }

    /// Convenience method to connect to a transport and run a client function.
    ///
    /// This is equivalent to:
    /// ```ignore
    /// handler_chain.connect_to(transport)?.with_client(main_fn).await
    /// ```
    pub async fn with_client(
        self,
        transport: impl Component + 'static,
        main_fn: impl AsyncFnOnce(JrConnectionCx) -> Result<(), crate::Error>,
    ) -> Result<(), crate::Error> {
        self.connect_to(transport)?.with_client(main_fn).await
    }
}

/// A JSON-RPC connection with an active transport.
///
/// This type represents a `JrHandlerChain` that has been connected to a transport
/// via `connect_to()`. It can be driven in two modes:
///
/// - [`serve()`](Self::serve) - Run as a server, handling incoming messages until the connection closes
/// - [`with_client()`](Self::with_client) - Run with client logic, allowing you to send requests/notifications
///
/// Most users won't construct this directly - instead use `JrHandlerChain::connect_to()` or
/// `JrHandlerChain::serve()` for convenience.
pub struct JrConnection<H: JrMessageHandler> {
    cx: JrConnectionCx,
    name: Option<String>,
    outgoing_rx: mpsc::UnboundedReceiver<OutgoingMessage>,
    new_task_rx: mpsc::UnboundedReceiver<Task>,
    transport_outgoing_tx: mpsc::UnboundedSender<Result<jsonrpcmsg::Message, crate::Error>>,
    transport_incoming_rx: mpsc::UnboundedReceiver<Result<jsonrpcmsg::Message, crate::Error>>,
    handler: H,
}

impl<H: JrMessageHandler> JrConnection<H> {
    /// Run the connection in server mode with the provided transport.
    ///
    /// This drives the connection by continuously processing messages from the transport
    /// and dispatching them to your registered handlers. The connection will run until:
    /// - The transport closes (e.g., EOF on byte streams)
    /// - An error occurs
    /// - One of your handlers returns an error
    ///
    /// The transport is responsible for serializing and deserializing `jsonrpcmsg::Message`
    /// values to/from the underlying I/O mechanism (byte streams, channels, etc.).
    ///
    /// Use this mode when you only need to respond to incoming messages and don't need
    /// to initiate your own requests. If you need to send requests to the other side,
    /// use [`with_client`](Self::with_client) instead.
    ///
    /// # Example: Byte Stream Transport
    ///
    /// ```no_run
    /// # use sacp::JrHandlerChain;
    /// # use sacp::ByteStreams;
    /// # use tokio_util::compat::{TokioAsyncReadCompatExt, TokioAsyncWriteCompatExt};
    /// # use sacp_test::*;
    /// # async fn example() -> Result<(), sacp::Error> {
    /// let transport = ByteStreams::new(
    ///     tokio::io::stdout().compat_write(),
    ///     tokio::io::stdin().compat(),
    /// );
    ///
    /// JrHandlerChain::new()
    ///     .on_receive_request(async |req: MyRequest, cx| {
    ///         cx.respond(MyResponse { status: "ok".into() })
    ///     })
    ///     .serve(transport)
    ///     .await?;
    /// # Ok(())
    /// # }
    /// ```
    pub async fn serve(self) -> Result<(), crate::Error> {
        self.with_client(async move |_cx| future::pending().await)
            .await
    }

    /// Run the connection in client mode, both handling incoming messages and sending your own.
    ///
    /// This drives the connection by:
    /// 1. Running your registered handlers in the background to process incoming messages
    /// 2. Executing your `main_fn` closure with a [`JrConnectionCx`] for sending requests/notifications
    ///
    /// The connection stays active until your `main_fn` returns, then shuts down gracefully.
    /// If the connection closes unexpectedly before `main_fn` completes, this returns an error.
    ///
    /// Use this mode when you need to initiate communication (send requests/notifications)
    /// in addition to responding to incoming messages. For server-only mode where you just
    /// respond to messages, use [`serve`](Self::serve) instead.
    ///
    /// # Example
    ///
    /// ```no_run
    /// # use sacp::JrHandlerChain;
    /// # use sacp::ByteStreams;
    /// # use sacp::schema::InitializeRequest;
    /// # use tokio_util::compat::{TokioAsyncReadCompatExt, TokioAsyncWriteCompatExt};
    /// # use sacp_test::*;
    /// # async fn example() -> Result<(), sacp::Error> {
    /// let transport = ByteStreams::new(
    ///     tokio::io::stdout().compat_write(),
    ///     tokio::io::stdin().compat(),
    /// );
    ///
    /// JrHandlerChain::new()
    ///     .on_receive_request(async |req: MyRequest, cx| {
    ///         // Handle incoming requests in the background
    ///         cx.respond(MyResponse { status: "ok".into() })
    ///     })
    ///     .connect_to(transport)?
    ///     .with_client(async |cx| {
    ///         // Initialize the protocol
    ///         let init_response = cx.send_request(InitializeRequest::make())
    ///             .block_task()
    ///             .await?;
    ///
    ///         // Send more requests...
    ///         let result = cx.send_request(MyRequest {})
    ///             .block_task()
    ///             .await?;
    ///
    ///         // When this closure returns, the connection shuts down
    ///         Ok(())
    ///     })
    ///     .await?;
    /// # Ok(())
    /// # }
    /// ```
    ///
    /// # Parameters
    ///
    /// - `transport`: The transport implementation for sending/receiving messages
    /// - `main_fn`: Your client logic. Receives a [`JrConnectionCx`] for sending messages.
    ///
    /// # Errors
    ///
    /// Returns an error if the connection closes before `main_fn` completes.
    pub async fn with_client(
        self,
        main_fn: impl AsyncFnOnce(JrConnectionCx) -> Result<(), crate::Error>,
    ) -> Result<(), crate::Error> {
        let JrConnection {
            cx,
            name,
            outgoing_rx,
            new_task_rx,
            handler,
            transport_outgoing_tx,
            transport_incoming_rx,
        } = self;
        let (reply_tx, reply_rx) = mpsc::unbounded();

        crate::util::instrument_with_connection_name(name, async move {
            futures::select!(
                // Protocol layer: OutgoingMessage → jsonrpcmsg::Message
                r = actors::outgoing_protocol_actor(
                    outgoing_rx,
                    reply_tx.clone(),
                    transport_outgoing_tx,
                ).fuse() => {
                    tracing::trace!(?r, "outgoing protocol actor terminated");
                    r?;
                }
                // Protocol layer: jsonrpcmsg::Message → handler/reply routing
                r = actors::incoming_protocol_actor(
                    &cx,
                    transport_incoming_rx,
                    reply_tx,
                    handler,
                ).fuse() => {
                    tracing::trace!(?r, "incoming protocol actor terminated");
                    r?;
                }
                r = actors::reply_actor(reply_rx).fuse() => {
                    tracing::trace!(?r, "reply actor terminated");
                    r?;
                }
                r = task_actor::task_actor(new_task_rx).fuse() => {
                    tracing::trace!(?r, "task actor terminated");
                    r?;
                }
                r = main_fn(cx.clone()).fuse() => {
                    tracing::trace!(?r, "main actor terminated");
                    r?;
                }
            );
            Ok(())
        })
        .await
    }
}

/// Message sent to the reply management actor
enum ReplyMessage {
    /// Wait for a response to the given id and then send it to the given receiver
    Subscribe(
        jsonrpcmsg::Id,
        oneshot::Sender<Result<serde_json::Value, crate::Error>>,
    ),

    /// Dispatch a response to the given id and value
    Dispatch(jsonrpcmsg::Id, Result<serde_json::Value, crate::Error>),
}

/// Messages send to be serialized over the transport.
#[derive(Debug)]
enum OutgoingMessage {
    /// Send a request to the server.
    Request {
        /// method to use in the request
        method: String,

        /// parameters for the request
        params: Option<jsonrpcmsg::Params>,

        /// where to send the response when it arrives
        response_tx: oneshot::Sender<Result<serde_json::Value, crate::Error>>,
    },

    /// Send a notification to the server.
    Notification {
        /// method to use in the request
        method: String,

        /// parameters for the request
        params: Option<jsonrpcmsg::Params>,
    },

    /// Send a reponse to a message from the server
    Response {
        id: jsonrpcmsg::Id,

        response: Result<serde_json::Value, crate::Error>,
    },

    /// Send a generalized error message
    Error { error: crate::Error },
}

/// Return type from JrHandler; indicates whether the request was handled or not.
#[must_use]
pub enum Handled<T> {
    /// The message was handled
    Yes,
    /// The message was not handled; returns the original value
    No(T),
}

/// Trait for converting handler return values into [`Handled`].
///
/// This trait allows handlers to return either `()` (which becomes `Handled::Yes`)
/// or an explicit `Handled<T>` value for more control over handler chain propagation.
pub trait IntoHandled<T> {
    /// Convert this value into a `Handled<T>`.
    fn into_handled(self) -> Handled<T>;
}

impl<T> IntoHandled<T> for () {
    fn into_handled(self) -> Handled<T> {
        Handled::Yes
    }
}

impl<T> IntoHandled<T> for Handled<T> {
    fn into_handled(self) -> Handled<T> {
        self
    }
}

/// Trait for types that can provide transport for JSON-RPC messages.
///
/// Implementations of this trait bridge between the internal protocol channels
/// (which carry `jsonrpcmsg::Message`) and the actual I/O mechanism (byte streams,
/// in-process channels, network sockets, etc.).
///
/// The transport layer is responsible only for moving `jsonrpcmsg::Message` in and out.
/// It has no knowledge of protocol semantics like request/response correlation, ID assignment,
/// or handler dispatch - those are handled by the protocol layer in `JrConnection`.
///
/// # Example
///
/// See [`ByteStreams`] for the standard byte stream implementation.

/// Connection context for sending messages and spawning tasks.
///
/// This is the primary handle for interacting with the JSON-RPC connection from
/// within handler callbacks. You can use it to:
///
/// * Send requests and notifications to the other side
/// * Spawn concurrent tasks that run alongside the connection
/// * Respond to requests (via [`JrRequestCx`] which wraps this)
///
/// # Cloning
///
/// `JrConnectionCx` is cheaply cloneable - all clones refer to the same underlying connection.
/// This makes it easy to share across async tasks.
///
/// # Event Loop and Concurrency
///
/// Handler callbacks run on the event loop, which means the connection cannot process new
/// messages while your handler is running. Use [`spawn`](Self::spawn) to offload any
/// expensive or blocking work to concurrent tasks.
///
/// See the [Event Loop and Concurrency](JrConnection#event-loop-and-concurrency) section
/// for more details.
#[derive(Clone, Debug)]
pub struct JrConnectionCx {
    message_tx: mpsc::UnboundedSender<OutgoingMessage>,
    task_tx: mpsc::UnboundedSender<Task>,
}

impl JrConnectionCx {
    fn new(
        tx: mpsc::UnboundedSender<OutgoingMessage>,
        task_tx: mpsc::UnboundedSender<Task>,
    ) -> Self {
        Self {
            message_tx: tx,
            task_tx,
        }
    }

    /// Spawns a task that will run so long as the JSON-RPC connection is being served.
    ///
    /// This is the primary mechanism for offloading expensive work from handler callbacks
    /// to avoid blocking the event loop. Spawned tasks run concurrently with the connection,
    /// allowing the server to continue processing messages.
    ///
    /// # Event Loop
    ///
    /// Handler callbacks run on the event loop, which cannot process new messages while
    /// your handler is running. Use `spawn` for any expensive operations:
    ///
    /// ```no_run
    /// # use sacp_test::*;
    /// # async fn example() -> Result<(), sacp::Error> {
    /// # let connection = mock_connection();
    /// connection.on_receive_request(async |req: ProcessRequest, cx| {
    ///     // Clone cx for the spawned task
    ///     cx.connection_cx().spawn({
    ///         let connection_cx = cx.connection_cx();
    ///         async move {
    ///             let result = expensive_operation(&req.data).await?;
    ///             connection_cx.send_notification(ProcessComplete { result })?;
    ///             Ok(())
    ///         }
    ///     })?;
    ///
    ///     // Respond immediately
    ///     cx.respond(ProcessResponse { result: "started".into() })
    /// })
    /// # .serve(sacp_test::MockTransport).await?;
    /// # Ok(())
    /// # }
    /// ```
    ///
    /// # Errors
    ///
    /// If the spawned task returns an error, the entire server will shut down.
    #[track_caller]
    pub fn spawn(
        &self,
        task: impl IntoFuture<Output = Result<(), crate::Error>, IntoFuture: Send + 'static>,
    ) -> Result<(), crate::Error> {
        let location = std::panic::Location::caller();
        let task = task.into_future();
        self.spawn_task(Task::new(location, task))
    }

    fn spawn_task(&self, task: Task) -> Result<(), crate::Error> {
        self.task_tx
            .unbounded_send(task)
            .map_err(crate::util::internal_error)
    }

    /// Spawn a JSON-RPC connection in the background and return a [`JrConnectionCx`] for sending messages to it.
    ///
    /// This is useful for creating multiple connections that communicate with each other,
    /// such as implementing proxy patterns or connecting to multiple backend services.
    ///
    /// # Arguments
    ///
    /// - `connection`: The `JrConnection` to spawn (typically created via `JrHandlerChain::connect_to()`)
    /// - `serve_future`: A function that drives the connection (usually `|c| Box::pin(c.serve())`)
    ///
    /// # Returns
    ///
    /// A `JrConnectionCx` that you can use to send requests and notifications to the spawned connection.
    ///
    /// # Example: Proxying to a backend connection
    ///
    /// ```
    /// # use sacp::{JrHandlerChain, JrConnectionCx};
    /// # use sacp_test::*;
    /// # async fn example(cx: JrConnectionCx) -> Result<(), sacp::Error> {
    /// // Set up a backend connection
    /// let backend = JrHandlerChain::new()
    ///     .on_receive_request(async |req: MyRequest, request_cx| {
    ///         request_cx.respond(MyResponse { status: "ok".into() })
    ///     })
    ///     .connect_to(MockTransport)?;
    ///
    /// // Spawn it and get a context to send requests to it
    /// let backend_cx = cx.spawn_connection(backend, |c| Box::pin(c.serve()))?;
    ///
    /// // Now you can forward requests to the backend
    /// let response = backend_cx.send_request(MyRequest {}).block_task().await?;
    /// # Ok(())
    /// # }
    /// ```
    #[track_caller]
    pub fn spawn_connection<H: JrMessageHandler>(
        &self,
        connection: JrConnection<H>,
        serve_future: impl FnOnce(JrConnection<H>) -> BoxFuture<'static, Result<(), crate::Error>>,
    ) -> Result<JrConnectionCx, crate::Error> {
        let cx = connection.cx.clone();
        let future = serve_future(connection);
        let task = Task::new(std::panic::Location::caller(), future);
        self.spawn_task(task)?;
        Ok(cx)
    }

    /// Send a request/notification and forward the response appropriately.
    ///
    /// The request context's response type matches the request's response type,
    /// enabling type-safe message forwarding.
    pub fn send_proxied_message<R, N>(
        &self,
        message: MessageAndCx<R, N>,
    ) -> Result<(), crate::Error>
    where
        R: JrRequest<Response: Send>,
        N: JrNotification,
    {
        match message {
            MessageAndCx::Request(request, request_cx) => {
                self.send_request(request).forward_to_request_cx(request_cx)
            }
            MessageAndCx::Notification(notification, _) => self.send_notification(notification),
        }
    }

    /// Send an outgoing request and return a [`JrResponse`] for handling the reply.
    ///
    /// The returned [`JrResponse`] provides methods for receiving the response without
    /// blocking the event loop:
    ///
    /// * [`await_when_result_received`](JrResponse::await_when_result_received) - Schedule
    ///   a callback to run when the response arrives (doesn't block the event loop)
    /// * [`block_task`](JrResponse::block_task) - Block the current task until the response
    ///   arrives (only safe in spawned tasks, not in handlers)
    ///
    /// # Anti-Footgun Design
    ///
    /// The API intentionally makes it difficult to block on the result directly to prevent
    /// the common mistake of blocking the event loop while waiting for a response:
    ///
    /// ```compile_fail
    /// # use sacp_test::*;
    /// # async fn example(cx: sacp::JrConnectionCx) -> Result<(), sacp::Error> {
    /// // ❌ This doesn't compile - prevents blocking the event loop
    /// let response = cx.send_request(MyRequest {}).await?;
    /// # Ok(())
    /// # }
    /// ```
    ///
    /// ```no_run
    /// # use sacp_test::*;
    /// # async fn example(cx: sacp::JrConnectionCx) -> Result<(), sacp::Error> {
    /// // ✅ Option 1: Schedule callback (safe in handlers)
    /// cx.send_request(MyRequest {})
    ///     .await_when_result_received(async |result| {
    ///         // Handle the response
    ///         Ok(())
    ///     })?;
    ///
    /// // ✅ Option 2: Block in spawned task (safe because task is concurrent)
    /// cx.spawn({
    ///     let cx = cx.clone();
    ///     async move {
    ///         let response = cx.send_request(MyRequest {})
    ///             .block_task()
    ///             .await?;
    ///         // Process response...
    ///         Ok(())
    ///     }
    /// })?;
    /// # Ok(())
    /// # }
    /// ```
    pub fn send_request<Req: JrRequest>(&self, request: Req) -> JrResponse<Req::Response> {
        let method = request.method().to_string();
        let (response_tx, response_rx) = oneshot::channel();
        match request.into_untyped_message() {
            Ok(untyped) => {
                let params = crate::util::json_cast(untyped.params).ok();
                let message = OutgoingMessage::Request {
                    method: method.clone(),
                    params,
                    response_tx,
                };

                match self.message_tx.unbounded_send(message) {
                    Ok(()) => (),
                    Err(error) => {
                        let OutgoingMessage::Request {
                            method,
                            response_tx,
                            ..
                        } = error.into_inner()
                        else {
                            unreachable!();
                        };

                        response_tx
                            .send(Err(communication_failure(format!(
                                "failed to send outgoing request `{method}"
                            ))))
                            .unwrap();
                    }
                }
            }

            Err(_) => {
                response_tx
                    .send(Err(communication_failure(format!(
                        "failed to create untyped request for `{method}"
                    ))))
                    .unwrap();
            }
        }

        JrResponse::new(method.clone(), self.clone(), response_rx)
            .map(move |json| <Req::Response>::from_value(&method, json))
    }

    /// Send an outgoing notification (no reply expected).
    ///
    /// Notifications are fire-and-forget messages that don't have IDs and don't expect responses.
    /// This method sends the notification immediately and returns.
    ///
    /// ```no_run
    /// # use sacp_test::*;
    /// # async fn example(cx: sacp::JrConnectionCx) -> Result<(), sacp::Error> {
    /// cx.send_notification(StatusUpdate {
    ///     message: "Processing...".into(),
    /// })?;
    /// # Ok(())
    /// # }
    /// ```
    pub fn send_notification<N: JrNotification>(
        &self,
        notification: N,
    ) -> Result<(), crate::Error> {
        let untyped = notification.into_untyped_message()?;
        let params = crate::util::json_cast(untyped.params).ok();
        self.send_raw_message(OutgoingMessage::Notification {
            method: untyped.method,
            params,
        })
    }

    /// Send an error notification (no reply expected).
    pub fn send_error_notification(&self, error: crate::Error) -> Result<(), crate::Error> {
        self.send_raw_message(OutgoingMessage::Error { error })
    }

    fn send_raw_message(&self, message: OutgoingMessage) -> Result<(), crate::Error> {
        match &message {
            OutgoingMessage::Response { id, response } => match response {
                Ok(_) => tracing::debug!(?id, "send_raw_message: queuing success response"),
                Err(e) => tracing::warn!(?id, ?e, "send_raw_message: queuing error response"),
            },
            _ => {}
        }
        self.message_tx
            .unbounded_send(message)
            .map_err(communication_failure)
    }
}

/// The context to respond to an incoming request.
///
/// This context is provided to request handlers and serves a dual role:
///
/// 1. **Respond to the request** - Use [`respond`](Self::respond) or
///    [`respond_with_result`](Self::respond_with_result) to send the response
/// 2. **Send other messages** - Use [`connection_cx`](Self::connection_cx) to access the
///    underlying [`JrConnectionCx`], giving access to
///    [`send_request`](JrConnectionCx::send_request),
///    [`send_notification`](JrConnectionCx::send_notification), and
///    [`spawn`](JrConnectionCx::spawn)
///
/// # Example
///
/// ```no_run
/// # use sacp_test::*;
/// # async fn example() -> Result<(), sacp::Error> {
/// # let connection = mock_connection();
/// connection.on_receive_request(async |req: ProcessRequest, cx| {
///     // Send a notification while processing
///     cx.connection_cx().send_notification(StatusUpdate {
///         message: "processing".into(),
///     })?;
///
///     // Do some work...
///     let result = process(&req.data)?;
///
///     // Respond to the request
///     cx.respond(ProcessResponse { result })
/// })
/// # .serve(sacp_test::MockTransport).await?;
/// # Ok(())
/// # }
/// ```
///
/// # Event Loop Considerations
///
/// Like all handlers, request handlers run on the event loop. Use
/// [`spawn`](JrConnectionCx::spawn) for expensive operations to avoid blocking
/// the connection.
///
/// See the [Event Loop and Concurrency](JrConnection#event-loop-and-concurrency)
/// section for more details.
#[must_use]
pub struct JrRequestCx<T: JrResponsePayload> {
    /// The context to use to send outgoing messages and replies.
    cx: JrConnectionCx,

    /// The method of the request.
    method: String,

    /// The `id` of the message we are replying to.
    id: jsonrpcmsg::Id,

    /// Function to send the response `T` to a request with the given method and id.
    make_json: SendBoxFnOnce<
        'static,
        (String, Result<T, crate::Error>),
        Result<serde_json::Value, crate::Error>,
    >,
}

impl<T: JrResponsePayload> std::fmt::Debug for JrRequestCx<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("JrRequestCx")
            .field("cx", &self.cx)
            .field("method", &self.method)
            .field("id", &self.id)
            .field("response_type", &std::any::type_name::<T>())
            .finish()
    }
}

impl JrRequestCx<serde_json::Value> {
    /// Create a new method context.
    fn new(cx: &JrConnectionCx, method: String, id: jsonrpcmsg::Id) -> Self {
        Self {
            cx: cx.clone(),
            method,
            id,
            make_json: SendBoxFnOnce::new(move |_method, value| value),
        }
    }

    /// Cast this request context to a different response type
    pub fn cast<T: JrResponsePayload>(self) -> JrRequestCx<T> {
        self.wrap_params(move |method, value| match value {
            Ok(value) => T::into_json(value, &method),
            Err(e) => Err(e),
        })
    }
}

impl<T: JrResponsePayload> JrRequestCx<T> {
    /// Method of the incoming request
    pub fn method(&self) -> &str {
        &self.method
    }

    /// Convert to a `JrRequestCx` that expects a JSON value
    /// and which checks (dynamically) that the JSON value it receives
    /// can be converted to `T`.
    pub fn erase_to_json(self) -> JrRequestCx<serde_json::Value> {
        self.wrap_params(|method, value| T::from_value(&method, value?))
    }

    /// Return a new JrResponse that expects a response of type U and serializes it.
    pub fn wrap_method(self, method: String) -> JrRequestCx<T> {
        JrRequestCx {
            cx: self.cx,
            method,
            id: self.id,
            make_json: self.make_json,
        }
    }

    /// Return a new JrResponse that expects a response of type U and serializes it.
    ///
    /// `wrap_fn` will be invoked with the method name and the result of the wrapped function.
    pub fn wrap_params<U: JrResponsePayload>(
        self,
        wrap_fn: impl FnOnce(&str, Result<U, crate::Error>) -> Result<T, crate::Error> + Send + 'static,
    ) -> JrRequestCx<U> {
        JrRequestCx {
            cx: self.cx,
            method: self.method,
            id: self.id,
            make_json: SendBoxFnOnce::new(move |method: String, input: Result<U, crate::Error>| {
                let t_value = wrap_fn(&method, input);
                self.make_json.call(method, t_value)
            }),
        }
    }

    /// Get the underlying JSON RPC context.
    pub fn connection_cx(&self) -> JrConnectionCx {
        self.cx.clone()
    }

    /// Respond to the JSON-RPC request with either a value (`Ok`) or an error (`Err`).
    pub fn respond_with_result(
        self,
        response: Result<T, crate::Error>,
    ) -> Result<(), crate::Error> {
        tracing::debug!(id = ?self.id, "respond called");
        let json = self.make_json.call_tuple((self.method.clone(), response));
        self.cx.send_raw_message(OutgoingMessage::Response {
            id: self.id,
            response: json,
        })
    }

    /// Respond to the JSON-RPC request with a value.
    pub fn respond(self, response: T) -> Result<(), crate::Error> {
        self.respond_with_result(Ok(response))
    }

    /// Respond to the JSON-RPC request with an internal error containing a message.
    pub fn respond_with_internal_error(self, message: impl ToString) -> Result<(), crate::Error> {
        self.respond_with_error(crate::util::internal_error(message))
    }

    /// Respond to the JSON-RPC request with an error.
    pub fn respond_with_error(self, error: crate::Error) -> Result<(), crate::Error> {
        tracing::debug!(id = ?self.id, ?error, "respond_with_error called");
        self.respond_with_result(Err(error))
    }
}

/// Common bounds for any JSON-RPC message.
pub trait JrMessage: 'static + Debug + Sized + Send {
    /// The parameters for the request.
    fn into_untyped_message(self) -> Result<UntypedMessage, crate::Error>;

    /// The method name for the request.
    fn method(&self) -> &str;

    /// Attempt to parse this type from a JSON-RPC request.
    ///
    /// Returns:
    /// - `None` if this type does not recognize the method name or recognizes it as a notification
    /// - `Some(Ok(value))` if the method is recognized as a request and deserialization succeeds
    /// - `Some(Err(error))` if the method is recognized as a request but deserialization fails
    fn parse_request(_method: &str, _params: &impl Serialize)
    -> Option<Result<Self, crate::Error>>;

    /// Attempt to parse this type from a JSON-RPC notification.
    ///
    /// Returns:
    /// - `None` if this type does not recognize the method name or recognizes it as a request
    /// - `Some(Ok(value))` if the method is recognized as a notification and deserialization succeeds
    /// - `Some(Err(error))` if the method is recognized as a notification but deserialization fails
    fn parse_notification(
        _method: &str,
        _params: &impl Serialize,
    ) -> Option<Result<Self, crate::Error>>;
}

/// Defines the "payload" of a successful response to a JSON-RPC request.
pub trait JrResponsePayload: 'static + Debug + Sized + Send {
    /// Convert this message into a JSON value.
    fn into_json(self, method: &str) -> Result<serde_json::Value, crate::Error>;

    /// Parse a JSON value into the response type.
    fn from_value(method: &str, value: serde_json::Value) -> Result<Self, crate::Error>;
}

impl JrResponsePayload for serde_json::Value {
    fn from_value(_method: &str, value: serde_json::Value) -> Result<Self, crate::Error> {
        Ok(value)
    }

    fn into_json(self, _method: &str) -> Result<serde_json::Value, crate::Error> {
        Ok(self)
    }
}

/// A struct that represents a notification (JSON-RPC message that does not expect a response).
pub trait JrNotification: JrMessage {}

/// A struct that represents a request (JSON-RPC message expecting a response).
pub trait JrRequest: JrMessage {
    /// The type of data expected in response.
    type Response: JrResponsePayload;
}

/// An enum capturing an in-flight request or notification.
/// In the case of a request, also includes the context used to respond to the request.
///
/// Type parameters allow specifying the concrete request and notification types.
/// By default, both are `UntypedMessage` for dynamic dispatch.
/// The request context's response type matches the request's response type.
#[derive(Debug)]
pub enum MessageAndCx<R: JrRequest = UntypedMessage, N: JrMessage = UntypedMessage> {
    /// Incoming request and the context where the response should be sent.
    Request(R, JrRequestCx<R::Response>),

    /// Incoming notification.
    Notification(N, JrConnectionCx),
}

impl<R: JrRequest, N: JrMessage> MessageAndCx<R, N> {
    /// Map the request and notification types to new types.
    pub fn map<R1, N1>(
        self,
        map_request: impl FnOnce(R, JrRequestCx<R::Response>) -> (R1, JrRequestCx<R1::Response>),
        map_notification: impl FnOnce(N, JrConnectionCx) -> (N1, JrConnectionCx),
    ) -> MessageAndCx<R1, N1>
    where
        R1: JrRequest<Response: Send>,
        N1: JrMessage,
    {
        match self {
            MessageAndCx::Request(request, cx) => {
                let (new_request, new_cx) = map_request(request, cx);
                MessageAndCx::Request(new_request, new_cx)
            }
            MessageAndCx::Notification(notification, cx) => {
                let (new_notification, new_cx) = map_notification(notification, cx);
                MessageAndCx::Notification(new_notification, new_cx)
            }
        }
    }

    /// Respond to the message with an error.
    ///
    /// If this message is a request, this error becomes the reply to the request.
    ///
    /// If this message is a notification, the error is sent as a notification.
    pub fn respond_with_error(self, error: crate::Error) -> Result<(), crate::Error> {
        match self {
            MessageAndCx::Request(_, cx) => cx.respond_with_error(error),
            MessageAndCx::Notification(_, cx) => cx.send_error_notification(error),
        }
    }

    /// Convert to a `JrRequestCx` that expects a JSON value
    /// and which checks (dynamically) that the JSON value it receives
    /// can be converted to `T`.
    pub fn erase_to_json(self) -> Result<MessageAndCx, crate::Error> {
        match self {
            MessageAndCx::Request(response, request_cx) => Ok(MessageAndCx::Request(
                response.into_untyped_message()?,
                request_cx.erase_to_json(),
            )),
            MessageAndCx::Notification(notification, cx) => Ok(MessageAndCx::Notification(
                notification.into_untyped_message()?,
                cx,
            )),
        }
    }
}

impl MessageAndCx {
    /// Returns the method of the message (only available for UntypedMessage).
    pub fn method(&self) -> &str {
        match self {
            MessageAndCx::Request(msg, _) => &msg.method,
            MessageAndCx::Notification(msg, _) => &msg.method,
        }
    }

    /// Returns the message of the message (only available for UntypedMessage).
    pub fn message(&self) -> &UntypedMessage {
        match self {
            MessageAndCx::Request(msg, _) => msg,
            MessageAndCx::Notification(msg, _) => msg,
        }
    }
}

/// An incoming JSON message without any typing. Can be a request or a notification.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct UntypedMessage {
    /// The JSON-RPC method name
    pub method: String,
    /// The JSON-RPC parameters as a raw JSON value
    pub params: serde_json::Value,
}

impl UntypedMessage {
    /// Returns an untyped message with the given method and parameters.
    pub fn new(method: &str, params: impl Serialize) -> Result<Self, crate::Error> {
        let params = serde_json::to_value(params)?;
        Ok(Self {
            method: method.to_string(),
            params,
        })
    }

    /// Returns the method name
    pub fn method(&self) -> &str {
        &self.method
    }

    /// Returns the parameters as a JSON value
    pub fn params(&self) -> &serde_json::Value {
        &self.params
    }

    /// Consumes this message and returns the method and params
    pub fn into_parts(self) -> (String, serde_json::Value) {
        (self.method, self.params)
    }
}

impl JrMessage for UntypedMessage {
    fn method(&self) -> &str {
        &self.method
    }

    fn into_untyped_message(self) -> Result<UntypedMessage, crate::Error> {
        Ok(self)
    }

    fn parse_request(method: &str, params: &impl Serialize) -> Option<Result<Self, crate::Error>> {
        Some(UntypedMessage::new(method, params))
    }

    fn parse_notification(
        method: &str,
        params: &impl Serialize,
    ) -> Option<Result<Self, crate::Error>> {
        Some(UntypedMessage::new(method, params))
    }
}

impl JrRequest for UntypedMessage {
    type Response = serde_json::Value;
}

impl JrNotification for UntypedMessage {}

/// Represents a pending response of type `R` from an outgoing request.
///
/// Returned by [`JrConnectionCx::send_request`], this type provides methods for handling
/// the response without blocking the event loop. The API is intentionally designed to make
/// it difficult to accidentally block.
///
/// # Anti-Footgun Design
///
/// You cannot directly `.await` a `JrResponse`. Instead, you must choose how to handle
/// the response:
///
/// ## Option 1: Schedule a Callback (Safe in Handlers)
///
/// Use [`await_when_result_received`](Self::await_when_result_received) to schedule a task
/// that runs when the response arrives. This doesn't block the event loop:
///
/// ```no_run
/// # use sacp_test::*;
/// # async fn example(cx: sacp::JrConnectionCx) -> Result<(), sacp::Error> {
/// cx.send_request(MyRequest {})
///     .await_when_result_received(async |result| {
///         match result {
///             Ok(response) => {
///                 // Handle successful response
///                 Ok(())
///             }
///             Err(error) => {
///                 // Handle error
///                 Err(error)
///             }
///         }
///     })?;
/// # Ok(())
/// # }
/// ```
///
/// ## Option 2: Block in a Spawned Task (Safe Only in `spawn`)
///
/// Use [`block_task`](Self::block_task) to block until the response arrives, but **only**
/// in a spawned task (never in a handler):
///
/// ```no_run
/// # use sacp_test::*;
/// # async fn example(cx: sacp::JrConnectionCx) -> Result<(), sacp::Error> {
/// // ✅ Safe: Spawned task runs concurrently
/// cx.spawn({
///     let cx = cx.clone();
///     async move {
///         let response = cx.send_request(MyRequest {})
///             .block_task()
///             .await?;
///         // Process response...
///         Ok(())
///     }
/// })?;
/// # Ok(())
/// # }
/// ```
///
/// ```no_run
/// # use sacp_test::*;
/// # async fn example() -> Result<(), sacp::Error> {
/// # let connection = mock_connection();
/// // ❌ NEVER do this in a handler - blocks the event loop!
/// connection.on_receive_request(async |req: MyRequest, cx| {
///     let response = cx.connection_cx().send_request(MyRequest {})
///         .block_task()  // This will deadlock!
///         .await?;
///     cx.respond(response)
/// })
/// # .serve(sacp_test::MockTransport).await?;
/// # Ok(())
/// # }
/// ```
///
/// # Why This Design?
///
/// If you block the event loop while waiting for a response, the connection cannot process
/// the incoming response message, creating a deadlock. This API design prevents that footgun
/// by making blocking explicit and encouraging non-blocking patterns.
pub struct JrResponse<R> {
    method: String,
    connection_cx: JrConnectionCx,
    response_rx: oneshot::Receiver<Result<serde_json::Value, crate::Error>>,
    to_result: Box<dyn Fn(serde_json::Value) -> Result<R, crate::Error> + Send>,
}

impl JrResponse<serde_json::Value> {
    fn new(
        method: String,
        connection_cx: JrConnectionCx,
        response_rx: oneshot::Receiver<Result<serde_json::Value, crate::Error>>,
    ) -> Self {
        Self {
            method,
            response_rx,
            connection_cx,
            to_result: Box::new(Ok),
        }
    }
}

impl<R: JrResponsePayload> JrResponse<R> {
    /// The method of the request this is in response to.
    pub fn method(&self) -> &str {
        &self.method
    }

    /// Create a new response that maps the result of the response to a new type.
    pub fn map<U>(
        self,
        map_fn: impl Fn(R) -> Result<U, crate::Error> + 'static + Send,
    ) -> JrResponse<U>
    where
        U: JrResponsePayload,
    {
        JrResponse {
            method: self.method,
            response_rx: self.response_rx,
            connection_cx: self.connection_cx,
            to_result: Box::new(move |value| map_fn((self.to_result)(value)?)),
        }
    }

    /// Forward the response (success or error) to a request context when it arrives.
    ///
    /// This is a convenience method for proxying messages between connections. When the
    /// response arrives, it will be automatically sent to the provided request context,
    /// whether it's a successful response or an error.
    ///
    /// # Example: Proxying requests
    ///
    /// ```
    /// # use sacp::{JrHandlerChain, JrConnectionCx};
    /// # use sacp_test::*;
    /// # async fn example(cx: JrConnectionCx) -> Result<(), sacp::Error> {
    /// // Set up backend connection
    /// let backend = JrHandlerChain::new()
    ///     .on_receive_request(async |req: MyRequest, request_cx| {
    ///         request_cx.respond(MyResponse { status: "ok".into() })
    ///     })
    ///     .connect_to(MockTransport)?;
    ///
    /// // Spawn backend and get a context to send to it
    /// let backend_cx = cx.spawn_connection(backend, |c| Box::pin(c.serve()))?;
    ///
    /// // Set up proxy that forwards requests to backend
    /// JrHandlerChain::new()
    ///     .on_receive_request({
    ///         let backend_cx = backend_cx.clone();
    ///         async move |req: MyRequest, request_cx| {
    ///             // Forward the request to backend and proxy the response back
    ///             backend_cx.send_request(req)
    ///                 .forward_to_request_cx(request_cx)?;
    ///             Ok(())
    ///         }
    ///     });
    /// # Ok(())
    /// # }
    /// ```
    ///
    /// # Type Safety
    ///
    /// The request context's response type must match the request's response type,
    /// ensuring type-safe message forwarding.
    ///
    /// # When to Use
    ///
    /// Use this when:
    /// - You're implementing a proxy or gateway pattern
    /// - You want to forward responses without processing them
    /// - The response types match between the outgoing request and incoming request
    ///
    /// This is equivalent to calling `await_when_result_received` and manually forwarding
    /// the result, but more concise.
    pub fn forward_to_request_cx(self, request_cx: JrRequestCx<R>) -> Result<(), crate::Error>
    where
        R: Send,
    {
        self.await_when_result_received(async move |result| request_cx.respond_with_result(result))
    }

    /// Block the current task until the response is received.
    ///
    /// **Warning:** This method blocks the current async task. It is **only safe** to use
    /// in spawned tasks created with [`JrConnectionCx::spawn`]. Using it directly in a
    /// handler callback will deadlock the connection.
    ///
    /// # Safe Usage (in spawned tasks)
    ///
    /// ```no_run
    /// # use sacp_test::*;
    /// # async fn example() -> Result<(), sacp::Error> {
    /// # let connection = mock_connection();
    /// connection.on_receive_request(async |req: MyRequest, cx| {
    ///     // Spawn a task to handle the request
    ///     cx.connection_cx().spawn({
    ///         let connection_cx = cx.connection_cx();
    ///         async move {
    ///             // Safe: We're in a spawned task, not blocking the event loop
    ///             let response = connection_cx.send_request(OtherRequest {})
    ///                 .block_task()
    ///                 .await?;
    ///
    ///             // Process the response...
    ///             Ok(())
    ///         }
    ///     })?;
    ///
    ///     // Respond immediately
    ///     cx.respond(MyResponse { status: "ok".into() })
    /// })
    /// # .serve(sacp_test::MockTransport).await?;
    /// # Ok(())
    /// # }
    /// ```
    ///
    /// # Unsafe Usage (in handlers - will deadlock!)
    ///
    /// ```no_run
    /// # use sacp_test::*;
    /// # async fn example() -> Result<(), sacp::Error> {
    /// # let connection = mock_connection();
    /// connection.on_receive_request(async |req: MyRequest, cx| {
    ///     // ❌ DEADLOCK: Handler blocks event loop, which can't process the response
    ///     let response = cx.connection_cx().send_request(OtherRequest {})
    ///         .block_task()
    ///         .await?;
    ///
    ///     cx.respond(MyResponse { status: response.value })
    /// })
    /// # .serve(sacp_test::MockTransport).await?;
    /// # Ok(())
    /// # }
    /// ```
    ///
    /// # When to Use
    ///
    /// Use this method when:
    /// - You're in a spawned task (via [`JrConnectionCx::spawn`])
    /// - You need the response value to proceed with your logic
    /// - Linear control flow is more natural than callbacks
    ///
    /// For handler callbacks, use [`await_when_result_received`](Self::await_when_result_received) instead.
    pub async fn block_task(self) -> Result<R, crate::Error>
    where
        R: Send,
    {
        match self.response_rx.await {
            Ok(Ok(json_value)) => match (self.to_result)(json_value) {
                Ok(value) => Ok(value),
                Err(err) => Err(err),
            },
            Ok(Err(err)) => Err(err),
            Err(err) => Err(crate::util::internal_error(format!(
                "response to `{}` never received: {}",
                self.method, err
            ))),
        }
    }

    /// Schedule an async task to run when a successful response is received.
    ///
    /// This is a convenience wrapper around [`await_when_result_received`](Self::await_when_result_received)
    /// for the common pattern of forwarding errors to a request context while only processing
    /// successful responses.
    ///
    /// # Behavior
    ///
    /// - If the response is `Ok(value)`, your task receives the value and the request context
    /// - If the response is `Err(error)`, the error is automatically sent to `request_cx`
    ///   and your task is not called
    ///
    /// # Example: Chaining requests
    ///
    /// ```no_run
    /// # use sacp_test::*;
    /// # async fn example() -> Result<(), sacp::Error> {
    /// # let connection = mock_connection();
    /// connection.on_receive_request(async |req: ValidateRequest, request_cx| {
    ///     // Send initial request
    ///     request_cx.connection_cx().send_request(ValidateRequest { data: req.data.clone() })
    ///         .await_when_ok_response_received(request_cx, async |validation, request_cx| {
    ///             // Only runs if validation succeeded
    ///             if validation.is_valid {
    ///                 // Respond to original request
    ///                 request_cx.respond(ValidateResponse { is_valid: true, error: None })
    ///             } else {
    ///                 request_cx.respond_with_error(sacp::util::internal_error("validation failed"))
    ///             }
    ///         })?;
    ///
    ///     Ok(())
    /// })
    /// # .serve(sacp_test::MockTransport).await?;
    /// # Ok(())
    /// # }
    /// ```
    ///
    /// # When to Use
    ///
    /// Use this when:
    /// - You need to respond to a request based on another request's result
    /// - You want errors to automatically propagate to the request context
    /// - You only care about the success case
    ///
    /// For more control over error handling, use [`await_when_result_received`](Self::await_when_result_received).
    #[track_caller]
    pub fn await_when_ok_response_received<F>(
        self,
        request_cx: JrRequestCx<R>,
        task: impl FnOnce(R, JrRequestCx<R>) -> F + 'static + Send,
    ) -> Result<(), crate::Error>
    where
        F: Future<Output = Result<(), crate::Error>> + 'static + Send,
        R: Send,
    {
        self.await_when_result_received(async move |result| match result {
            Ok(value) => task(value, request_cx).await,
            Err(err) => request_cx.respond_with_error(err),
        })
    }

    /// Schedule an async task to run when the response is received.
    ///
    /// This is the recommended way to handle responses in handler callbacks, as it doesn't
    /// block the event loop. The task will be spawned automatically when the response arrives.
    ///
    /// # Example: Handle response in callback
    ///
    /// ```no_run
    /// # use sacp_test::*;
    /// # async fn example() -> Result<(), sacp::Error> {
    /// # let connection = mock_connection();
    /// connection.on_receive_request(async |req: MyRequest, cx| {
    ///     // Send a request and schedule a callback for the response
    ///     cx.connection_cx().send_request(QueryRequest { id: 22 })
    ///         .await_when_result_received({
    ///             let connection_cx = cx.connection_cx();
    ///             async move |result| {
    ///                 match result {
    ///                     Ok(response) => {
    ///                         println!("Got response: {:?}", response);
    ///                         // Can send more messages here
    ///                         connection_cx.send_notification(QueryComplete {})?;
    ///                         Ok(())
    ///                 }
    ///                     Err(error) => {
    ///                         eprintln!("Request failed: {}", error);
    ///                         Err(error)
    ///                     }
    ///                 }
    ///             }
    ///         })?;
    ///
    ///     // Handler continues immediately without waiting
    ///     cx.respond(MyResponse { status: "processing".into() })
    /// })
    /// # .serve(sacp_test::MockTransport).await?;
    /// # Ok(())
    /// # }
    /// ```
    ///
    /// # Event Loop Safety
    ///
    /// Unlike [`block_task`](Self::block_task), this method is safe to use in handlers because
    /// it schedules the task to run later rather than blocking the current task. The event loop
    /// remains free to process messages, including the response itself.
    ///
    /// # Error Handling
    ///
    /// If the scheduled task returns `Err`, the entire server will shut down. Make sure to handle
    /// errors appropriately within your task.
    ///
    /// # When to Use
    ///
    /// Use this method when:
    /// - You're in a handler callback (not a spawned task)
    /// - You want to process the response asynchronously
    /// - You don't need the response value immediately
    ///
    /// For spawned tasks where you need linear control flow, consider [`block_task`](Self::block_task).
    #[track_caller]
    pub fn await_when_result_received<F>(
        self,
        task: impl FnOnce(Result<R, crate::Error>) -> F + 'static + Send,
    ) -> Result<(), crate::Error>
    where
        F: Future<Output = Result<(), crate::Error>> + 'static + Send,
        R: Send,
    {
        let connection_cx = self.connection_cx.clone();
        let block_task = self.block_task();
        connection_cx.spawn(async move { task(block_task.await).await })
    }
}

const COMMUNICATION_FAILURE: i32 = -32000;

fn communication_failure(err: impl ToString) -> crate::Error {
    crate::Error::new((COMMUNICATION_FAILURE, err.to_string()))
}

// ============================================================================
// IntoJrConnectionTransport Implementations
// ============================================================================

/// A component that communicates over byte streams (stdin/stdout, sockets, pipes, etc.).
///
/// `ByteStreams` implements the [`Component`] trait for any pair of `AsyncRead` and `AsyncWrite`
/// streams, handling serialization of JSON-RPC messages to/from newline-delimited JSON.
/// This is the standard way to communicate with external processes or network connections.
///
/// # Use Cases
///
/// - **Stdio communication**: Connect to agents or proxies via stdin/stdout
/// - **Network sockets**: TCP, Unix domain sockets, or other stream-based protocols
/// - **Named pipes**: Cross-process communication on the same machine
/// - **File I/O**: Reading from and writing to file descriptors
///
/// # Example
///
/// Connecting to an agent via stdio:
///
/// ```no_run
/// use sacp::ByteStreams;
/// use tokio_util::compat::{TokioAsyncReadCompatExt, TokioAsyncWriteCompatExt};
///
/// # async fn example() -> Result<(), sacp::Error> {
/// let component = ByteStreams::new(
///     tokio::io::stdout().compat_write(),
///     tokio::io::stdin().compat(),
/// );
///
/// // Use as a component in a handler chain
/// sacp::JrHandlerChain::new()
///     .name("my-client")
///     .serve(component)
///     .await?;
/// # Ok(())
/// # }
/// ```
///
/// [`Component`]: crate::Component
pub struct ByteStreams<OB, IB> {
    /// Outgoing byte stream (where we write serialized messages)
    pub outgoing: OB,
    /// Incoming byte stream (where we read and parse messages)
    pub incoming: IB,
}

impl<OB, IB> ByteStreams<OB, IB>
where
    OB: AsyncWrite + Send + 'static,
    IB: AsyncRead + Send + 'static,
{
    /// Create a new byte stream transport.
    pub fn new(outgoing: OB, incoming: IB) -> Self {
        Self { outgoing, incoming }
    }
}

impl<OB, IB> Component for ByteStreams<OB, IB>
where
    OB: AsyncWrite + Send + 'static,
    IB: AsyncRead + Send + 'static,
{
    async fn serve(self, client: impl Component) -> Result<(), crate::Error> {
        let (channel, serve_self) = self.into_server();
        match futures::future::select(Box::pin(client.serve(channel)), serve_self).await {
            Either::Left((result, _)) => result,
            Either::Right((result, _)) => result,
        }
    }

    fn into_server(self) -> (Channel, BoxFuture<'static, Result<(), crate::Error>>) {
        let Self { outgoing, incoming } = self;

        // Create a channel pair for the client to use
        let (channel_for_caller, channel_for_bytestreams) = Channel::duplex();

        // Create the server future that runs the byte stream actors
        let server_future = Box::pin(async move {
            let Channel { rx, tx } = channel_for_bytestreams;

            // Run both actors concurrently
            let outgoing_future = actors::transport_outgoing_actor(rx, outgoing);
            let incoming_future = actors::transport_incoming_actor(incoming, tx);

            // Wait for both to complete
            futures::try_join!(outgoing_future, incoming_future)?;

            Ok(())
        });

        (channel_for_caller, server_future)
    }
}

/// A channel endpoint representing one side of a bidirectional message channel.
///
/// `Channel` represents a single endpoint's view of a bidirectional communication channel.
/// Each endpoint has:
/// - `rx`: A receiver for incoming messages (or errors) from the counterpart
/// - `tx`: A sender for outgoing messages (or errors) to the counterpart
///
/// # Example
///
/// ```no_run
/// # use sacp::{Channel, JrHandlerChain};
/// # async fn example() -> Result<(), sacp::Error> {
/// // Create a pair of connected channels
/// let (channel_a, channel_b) = Channel::duplex();
///
/// // Each channel can be used by a different component
/// JrHandlerChain::new()
///     .name("connection-a")
///     .serve(channel_a)
///     .await?;
/// # Ok(())
/// # }
/// ```
pub struct Channel {
    /// Receives messages (or errors) from the counterpart.
    pub rx: mpsc::UnboundedReceiver<Result<jsonrpcmsg::Message, crate::Error>>,
    /// Sends messages (or errors) to the counterpart.
    pub tx: mpsc::UnboundedSender<Result<jsonrpcmsg::Message, crate::Error>>,
}

impl Channel {
    /// Create a pair of connected channel endpoints.
    ///
    /// Returns two `Channel` instances that are connected to each other:
    /// - Messages sent via `channel_a.tx` are received on `channel_b.rx`
    /// - Messages sent via `channel_b.tx` are received on `channel_a.rx`
    ///
    /// # Returns
    ///
    /// A tuple `(channel_a, channel_b)` of connected channel endpoints.
    pub fn duplex() -> (Self, Self) {
        // Create channels: A sends Result<Message> which B receives as Message
        let (a_tx, b_rx) = mpsc::unbounded();
        let (b_tx, a_rx) = mpsc::unbounded();

        let channel_a = Self { rx: a_rx, tx: a_tx };
        let channel_b = Self { rx: b_rx, tx: b_tx };

        (channel_a, channel_b)
    }

    /// Copy messages from `rx` to `tx`.
    ///
    /// # Returns
    ///
    /// A `Result` indicating success or failure.
    pub async fn copy(mut self) -> Result<(), crate::Error> {
        while let Some(msg) = self.rx.next().await {
            self.tx
                .unbounded_send(msg)
                .map_err(crate::util::internal_error)?;
        }
        Ok(())
    }
}

impl Component for Channel {
    async fn serve(self, client: impl Component) -> Result<(), crate::Error> {
        let (client_channel, client_serve) = client.into_server();

        match futures::try_join!(
            Channel {
                rx: client_channel.rx,
                tx: self.tx
            }
            .copy(),
            Channel {
                rx: self.rx,
                tx: client_channel.tx
            }
            .copy(),
            client_serve
        ) {
            Ok(((), (), ())) => Ok(()),
            Err(err) => Err(err),
        }
    }

    fn into_server(self) -> (Channel, BoxFuture<'static, Result<(), crate::Error>>) {
        (self, Box::pin(future::ready(Ok(()))))
    }
}
