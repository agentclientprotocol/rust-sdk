//! Component abstraction for agents and proxies.
//!
//! This module provides the [`Component`] trait that defines the interface for things
//! that can be run as part of a conductor's chain - agents, proxies, or any ACP-speaking component.
//!
//! ## Usage
//!
//! Components serve by forwarding to other components, creating a chain of message processors.
//! To implement a component, implement the `serve` method:
//!
//! ```rust,ignore
//! use sacp::component::Component;
//!
//! struct MyProxy {
//!     // configuration fields
//! }
//!
//! impl Component for MyProxy {
//!     async fn serve(self, client: impl Component) -> Result<(), sacp::Error> {
//!         sacp::JrHandlerChain::new()
//!             .name("my-proxy")
//!             // configure handlers here
//!             .serve(client)
//!             .await
//!     }
//! }
//! ```

use futures::future::BoxFuture;
use std::future::Future;

use crate::Channel;

/// A component that can participate in the Agent-Client Protocol.
///
/// This trait represents anything that can communicate via JSON-RPC messages over channels -
/// agents, proxies, in-process connections, or any ACP-speaking component.
///
/// # Component Types
///
/// The trait is implemented by several built-in types representing different communication patterns:
///
/// - **[`ByteStreams`]**: A component communicating over byte streams (stdin/stdout, sockets, etc.)
/// - **[`Channel`]**: A component communicating via in-process message channels (for testing or direct connections)
/// - **[`AcpAgent`]**: An external agent running in a separate process with stdio communication
/// - **Custom components**: Proxies, transformers, or any ACP-aware service
///
/// # Two Ways to Serve
///
/// Components can be used in two ways:
///
/// 1. **`serve(client)`** - Serve by forwarding to another component (most components implement this)
/// 2. **`into_server()`** - Convert into a channel endpoint and server future (base cases implement this)
///
/// Most components only need to implement `serve(client)` - the `into_server()` method has a default
/// implementation that creates an intermediate channel and calls `serve`.
///
/// # Implementation Example
///
/// ```rust,ignore
/// use sacp::Component;
///
/// struct MyProxy {
///     config: ProxyConfig,
/// }
///
/// impl Component for MyProxy {
///     async fn serve(self, client: impl Component) -> Result<(), sacp::Error> {
///         // Set up handler chain that forwards to client
///         sacp::JrHandlerChain::new()
///             .name("my-proxy")
///             .on_receive_request(async |req: MyRequest, cx| {
///                 // Transform and forward request
///                 cx.respond(MyResponse { status: "ok".into() })
///             })
///             .serve(client)
///             .await
///     }
/// }
/// ```
///
/// # Heterogeneous Collections
///
/// For storing different component types in the same collection, use [`DynComponent`]:
///
/// ```rust,ignore
/// let components: Vec<DynComponent> = vec![
///     DynComponent::new(proxy1),
///     DynComponent::new(proxy2),
///     DynComponent::new(agent),
/// ];
/// ```
///
/// [`ByteStreams`]: crate::ByteStreams
/// [`AcpAgent`]: https://docs.rs/sacp-tokio/latest/sacp_tokio/struct.AcpAgent.html
/// [`JrHandlerChain`]: crate::JrHandlerChain
pub trait Component: Send + 'static {
    /// Serve this component by forwarding to a client component.
    ///
    /// Most components implement this method to set up their handler chain and
    /// forward messages to the provided client.
    ///
    /// # Arguments
    ///
    /// * `client` - The component to forward messages to
    ///
    /// # Returns
    ///
    /// A future that resolves when the component stops serving, either successfully
    /// or with an error. The future must be `Send`.
    fn serve(self, client: impl Component)
    -> impl Future<Output = Result<(), crate::Error>> + Send;

    /// Convert this component into a channel endpoint and server future.
    ///
    /// This method returns:
    /// - A `Channel` that can be used to communicate with this component
    /// - A `BoxFuture` that runs the component's server logic
    ///
    /// The default implementation creates an intermediate channel pair and calls `serve`
    /// on one endpoint while returning the other endpoint for the caller to use.
    ///
    /// Base cases like `Channel` and `ByteStreams` override this to avoid unnecessary copying.
    ///
    /// # Returns
    ///
    /// A tuple of `(Channel, BoxFuture)` where the channel is for the caller to use
    /// and the future must be spawned to run the server.
    fn into_server(self) -> (Channel, BoxFuture<'static, Result<(), crate::Error>>)
    where
        Self: Sized,
    {
        let (channel_a, channel_b) = Channel::duplex();
        let future = Box::pin(self.serve(channel_b));
        (channel_a, future)
    }
}

/// Type-erased component trait for object-safe dynamic dispatch.
///
/// This trait is internal and used by [`DynComponent`]. Users should implement
/// [`Component`] instead, which is automatically converted to `ErasedComponent`
/// via a blanket implementation.
trait ErasedComponent: Send {
    fn serve_erased(
        self: Box<Self>,
        client: Box<dyn ErasedComponent>,
    ) -> BoxFuture<'static, Result<(), crate::Error>>;

    fn into_server_erased(
        self: Box<Self>,
    ) -> (Channel, BoxFuture<'static, Result<(), crate::Error>>);
}

/// Blanket implementation: any `Component` can be type-erased.
impl<C: Component> ErasedComponent for C {
    fn serve_erased(
        self: Box<Self>,
        client: Box<dyn ErasedComponent>,
    ) -> BoxFuture<'static, Result<(), crate::Error>> {
        Box::pin(async move { (*self).serve(DynComponent { inner: client }).await })
    }

    fn into_server_erased(
        self: Box<Self>,
    ) -> (Channel, BoxFuture<'static, Result<(), crate::Error>>) {
        (*self).into_server()
    }
}

/// A dynamically-typed component for heterogeneous collections.
///
/// This type wraps any [`Component`] implementation and provides dynamic dispatch,
/// allowing you to store different component types in the same collection.
///
/// # Examples
///
/// ```rust,ignore
/// use sacp::DynComponent;
///
/// let components: Vec<DynComponent> = vec![
///     DynComponent::new(Proxy1),
///     DynComponent::new(Proxy2),
///     DynComponent::new(Agent),
/// ];
/// ```
pub struct DynComponent {
    inner: Box<dyn ErasedComponent>,
}

impl DynComponent {
    /// Create a new `DynComponent` from any type implementing [`Component`].
    pub fn new<C: Component>(component: C) -> Self {
        Self {
            inner: Box::new(component),
        }
    }
}

impl Component for DynComponent {
    async fn serve(self, client: impl Component) -> Result<(), crate::Error> {
        self.inner
            .serve_erased(Box::new(client) as Box<dyn ErasedComponent>)
            .await
    }

    fn into_server(self) -> (Channel, BoxFuture<'static, Result<(), crate::Error>>) {
        self.inner.into_server_erased()
    }
}
