//! ConnectTo abstraction for agents and proxies.
//!
//! This module provides the [`ConnectTo`] trait that defines the interface for things
//! that can be run as part of a conductor's chain - agents, proxies, or any ACP-speaking component.
//!
//! ## Usage
//!
//! Components connect to other components, creating a chain of message processors.
//! The type parameter `R` is the role that this component connects to (its counterpart).
//!
//! To implement a component, implement the `connect_to` method:
//!
//! ```rust
//! use agent_client_protocol::{Agent, Client, ConnectTo, Result};
//!
//! struct MyAgent;
//!
//! // An agent connects to clients
//! impl ConnectTo<Client> for MyAgent {
//!     async fn connect_to(self, client: impl ConnectTo<Agent>) -> Result<()> {
//!         Agent.builder()
//!             .name("my-agent")
//!             .connect_to(client)
//!             .await
//!     }
//! }
//! ```

use futures::future::BoxFuture;
use std::{
    fmt::Debug,
    future::Future,
    marker::PhantomData,
    pin::Pin,
    task::{Context, Poll},
};

use crate::{Channel, Result, role::Role};

/// Drives owned endpoint work.
///
/// A driver owns the endpoint: successful completion means no further
/// output is expected, and adapters must drain output already accepted before
/// terminating. An error terminates the connection immediately.
///
/// Poll the driver concurrently with channel traffic. Endpoints without owned
/// work return `None` from [`ConnectTo::into_channel_and_future`], not a driver:
/// their channel halves independently determine their lifetime.
#[must_use = "connection drivers must be polled to make progress"]
pub struct ConnectionDriver {
    future: BoxFuture<'static, Result<()>>,
    finish: Option<futures::channel::oneshot::Sender<()>>,
}

impl ConnectionDriver {
    /// Create a driver that owns the endpoint's lifetime.
    ///
    /// Custom normalized adapters must finish accepted output when their channel
    /// input closes. This constructor cannot externally flush or shut down an
    /// opaque future that waits for additional, independently owned input.
    pub fn new(future: impl Future<Output = Result<()>> + Send + 'static) -> Self {
        Self {
            future: Box::pin(future),
            finish: None,
        }
    }

    // Physical transports can finish their write half without waiting for read
    // EOF. Keep this coordination private; arbitrary futures cannot support it.
    pub(crate) fn with_finish(
        future: impl Future<Output = Result<()>> + Send + 'static,
        finish: futures::channel::oneshot::Sender<()>,
    ) -> Self {
        Self {
            future: Box::pin(future),
            finish: Some(finish),
        }
    }

    pub(crate) fn take_finish(&mut self) -> Option<futures::channel::oneshot::Sender<()>> {
        self.finish.take()
    }
}

impl Future for ConnectionDriver {
    type Output = Result<()>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        self.future.as_mut().poll(cx)
    }
}

impl Debug for ConnectionDriver {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ConnectionDriver")
            .field("finishable", &self.finish.is_some())
            .finish_non_exhaustive()
    }
}

/// A component that can exchange JSON-RPC messages to an endpoint playing the role `R`
/// (e.g., an ACP [`Agent`](`crate::role::acp::Agent`) or an MCP [`Server`](`crate::role::mcp::Server`)).
///
/// This trait represents anything that can communicate via JSON-RPC messages over channels -
/// agents, proxies, in-process connections, or any ACP-speaking component.
///
/// The type parameter `R` is the role that this component connects to (its counterpart).
/// For example:
/// - An agent implements `ConnectTo<Client>` to connect to clients
/// - A proxy implements `ConnectTo<Conductor>` to connect to conductors
/// - Transports like `Channel` implement `ConnectTo<R>` for every `R` because they are role-agnostic
///
/// # Component Types
///
/// The trait is implemented by several built-in types representing different communication patterns:
///
/// - **[`Lines`]**: A component communicating over asynchronous line streams
/// - **[`ByteStreams`]**: A component communicating over byte streams (stdin/stdout, sockets, etc.)
/// - **[`Channel`]**: A component communicating via in-process message channels (for testing or direct connections)
/// - **Custom components**: Proxies, transformers, or any ACP-aware service
#[cfg_attr(
    not(target_family = "wasm"),
    doc = "- **[`AcpAgent`]**: An external agent running in a separate process with stdio communication"
)]
///
/// # Two Ways to Connect
///
/// Components can be used in two ways:
///
/// 1. **`connect_to(client)`** - Connect directly to another component (most components implement this)
/// 2. **`into_channel_and_future()`** - Obtain a channel endpoint and optional owned driver
///
/// Most components only need to implement `connect_to(client)`. The
/// `into_channel_and_future()` method has a default implementation that creates an intermediate
/// channel and calls `connect_to`.
///
/// # Implementation Example
///
/// ```rust
/// use agent_client_protocol::{Agent, Client, ConnectTo, Result};
///
/// struct MyAgent;
///
/// impl ConnectTo<Client> for MyAgent {
///     async fn connect_to(self, client: impl ConnectTo<Agent>) -> Result<()> {
///         Agent.builder()
///             .name("my-agent")
///             .connect_to(client)
///             .await
///     }
/// }
/// ```
///
/// # Heterogeneous Collections
///
/// For storing different component types in the same collection, use [`DynConnectTo`]:
///
/// ```rust
/// use agent_client_protocol::{Channel, Client, DynConnectTo};
///
/// let (first, _first_peer) = Channel::duplex();
/// let (second, _second_peer) = Channel::duplex();
/// let components: Vec<DynConnectTo<Client>> = vec![
///     DynConnectTo::new(first),
///     DynConnectTo::new(second),
/// ];
/// assert_eq!(components.len(), 2);
/// ```
///
/// [`ByteStreams`]: crate::ByteStreams
/// [`Lines`]: crate::Lines
/// [`Builder`]: crate::Builder
#[cfg_attr(not(target_family = "wasm"), doc = "[`AcpAgent`]: crate::AcpAgent")]
pub trait ConnectTo<R: Role>: Send + 'static {
    /// Connect this component to another component.
    ///
    /// Most components implement this method to set up their connection and
    /// exchange messages with the provided component.
    ///
    /// # Arguments
    ///
    /// * `client` - The component to connect to (implements `ConnectTo<R::Counterpart>`)
    ///
    /// # Returns
    ///
    /// A future that resolves when the connection ends, either successfully
    /// or with an error. The future must be `Send`.
    ///
    /// A component that buffers outbound messages should not return `Ok(())`
    /// merely because its client completed: it should first finish messages the
    /// client already transferred to it. This lets wrappers preserve graceful
    /// drain guarantees through to the physical transport sink. Errors may
    /// still terminate the connection immediately.
    fn connect_to(
        self,
        client: impl ConnectTo<R::Counterpart>,
    ) -> impl Future<Output = Result<()>> + Send;

    /// Convert this component into a channel endpoint and optional owned driver.
    ///
    /// The returned [`Channel`] is the canonical frame-aware boundary. It carries
    /// complete [`TransportFrame`](crate::TransportFrame) values so default
    /// adapters preserve batch grouping.
    ///
    /// This method returns:
    /// - A `Channel` that can be used to communicate with this component
    /// - `Some(ConnectionDriver)` when the component owns work to drive
    /// - `None` when the channel halves alone own the endpoint's lifetime
    ///
    /// The default implementation creates an intermediate channel pair and calls `connect_to`
    /// on one endpoint while returning the other endpoint for the caller to use.
    ///
    /// Base cases like `Channel` and `ByteStreams` override this to avoid unnecessary copying.
    ///
    /// # Returns
    ///
    /// A tuple of `(Channel, Option<ConnectionDriver>)`. Owned drivers must be
    /// polled concurrently with channel traffic. Successful owned completion
    /// ends the endpoint after draining accepted output. `None` is not EOF:
    /// preserve both independent channel half-closes.
    ///
    /// Absence must be handled explicitly; the optional driver is not awaitable:
    ///
    /// ```compile_fail,E0277
    /// use agent_client_protocol::{Channel, ConnectTo, UntypedRole};
    ///
    /// # async fn example() -> agent_client_protocol::Result<()> {
    /// let (channel, _peer) = Channel::duplex();
    /// let (_channel, driver) = ConnectTo::<UntypedRole>::into_channel_and_future(channel);
    /// driver.await?;
    /// # Ok(())
    /// # }
    /// ```
    ///
    /// Once present, the owned driver itself is awaitable:
    ///
    /// ```no_run
    /// use agent_client_protocol::{Channel, ConnectionDriver, Result};
    ///
    /// async fn drive_owned_work((_channel, driver): (Channel, Option<ConnectionDriver>)) -> Result<()> {
    ///     if let Some(driver) = driver {
    ///         // In a real adapter, also poll the channel traffic concurrently.
    ///         driver.await?;
    ///     }
    ///     Ok(())
    /// }
    /// ```
    fn into_channel_and_future(self) -> (Channel, Option<ConnectionDriver>)
    where
        Self: Sized,
    {
        let (channel_a, channel_b) = Channel::duplex();
        let future = ConnectionDriver::new(self.connect_to(channel_b));
        (channel_a, Some(future))
    }
}

/// Type-erased connect trait for object-safe dynamic dispatch.
///
/// This trait is internal and used by [`DynConnectTo`]. Users should implement
/// [`ConnectTo`] instead, which is automatically converted to `ErasedConnectTo`
/// via a blanket implementation.
trait ErasedConnectTo<R: Role>: Send {
    fn type_name(&self) -> &'static str;

    fn connect_to_erased(
        self: Box<Self>,
        client: Box<dyn ErasedConnectTo<R::Counterpart>>,
    ) -> BoxFuture<'static, Result<()>>;

    fn into_channel_and_future_erased(self: Box<Self>) -> (Channel, Option<ConnectionDriver>);
}

/// Blanket implementation: any `ConnectTo<R>` can be type-erased.
impl<C: ConnectTo<R>, R: Role> ErasedConnectTo<R> for C {
    fn type_name(&self) -> &'static str {
        std::any::type_name::<C>()
    }

    fn connect_to_erased(
        self: Box<Self>,
        client: Box<dyn ErasedConnectTo<R::Counterpart>>,
    ) -> BoxFuture<'static, Result<()>> {
        Box::pin(async move {
            (*self)
                .connect_to(DynConnectTo {
                    inner: client,
                    _marker: PhantomData,
                })
                .await
        })
    }

    fn into_channel_and_future_erased(self: Box<Self>) -> (Channel, Option<ConnectionDriver>) {
        (*self).into_channel_and_future()
    }
}

/// A dynamically-typed component for heterogeneous collections.
///
/// This type wraps any [`ConnectTo`] implementation and provides dynamic dispatch,
/// allowing you to store different component types in the same collection.
///
/// The type parameter `R` is the role that all components in the
/// collection connect to (their counterpart).
///
/// # Examples
///
/// ```rust
/// use agent_client_protocol::{Channel, Client, DynConnectTo};
///
/// let (first, _first_peer) = Channel::duplex();
/// let (second, _second_peer) = Channel::duplex();
/// let components: Vec<DynConnectTo<Client>> = vec![
///     DynConnectTo::new(first),
///     DynConnectTo::new(second),
/// ];
/// assert_eq!(components.len(), 2);
/// ```
pub struct DynConnectTo<R: Role> {
    inner: Box<dyn ErasedConnectTo<R>>,
    _marker: PhantomData<R>,
}

impl<R: Role> DynConnectTo<R> {
    /// Create a new `DynConnectTo` from any type implementing [`ConnectTo`].
    pub fn new<C: ConnectTo<R>>(component: C) -> Self {
        Self {
            inner: Box::new(component),
            _marker: PhantomData,
        }
    }

    /// Returns the type name of the wrapped component.
    #[must_use]
    pub fn type_name(&self) -> &'static str {
        self.inner.type_name()
    }
}

impl<R: Role> ConnectTo<R> for DynConnectTo<R> {
    async fn connect_to(self, client: impl ConnectTo<R::Counterpart>) -> Result<()> {
        self.inner
            .connect_to_erased(Box::new(client) as Box<dyn ErasedConnectTo<R::Counterpart>>)
            .await
    }

    fn into_channel_and_future(self) -> (Channel, Option<ConnectionDriver>) {
        self.inner.into_channel_and_future_erased()
    }
}

impl<R: Role> Debug for DynConnectTo<R> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DynConnectTo")
            .field("type_name", &self.type_name())
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::role::UntypedRole;
    use futures::FutureExt as _;

    struct OwnedWork(BoxFuture<'static, Result<()>>);

    impl ConnectTo<UntypedRole> for OwnedWork {
        async fn connect_to(self, _client: impl ConnectTo<UntypedRole>) -> Result<()> {
            self.0.await
        }
    }

    #[test]
    fn raw_channel_has_no_owned_work() {
        let (channel, _other) = Channel::duplex();
        let (_, driver) = ConnectTo::<UntypedRole>::into_channel_and_future(channel);
        assert!(driver.is_none());
    }

    #[test]
    fn owned_driver_preserves_errors_and_polls_unpinned() {
        let error = crate::Error::internal_error().data("driver failure");
        let mut driver = ConnectionDriver::new(futures::future::ready(Err(error.clone())));
        assert_eq!(futures::executor::block_on(&mut driver), Err(error));
    }

    #[test]
    fn default_conversion_owns_real_work_until_completion() {
        let (done_tx, done_rx) = futures::channel::oneshot::channel();
        let component = OwnedWork(async move { done_rx.await.unwrap() }.boxed());
        let (_channel, driver) = component.into_channel_and_future();
        let mut driver = driver.expect("default conversion always owns its connect_to work");
        assert!((&mut driver).now_or_never().is_none());

        let error = crate::Error::internal_error().data("owned work failed");
        done_tx.send(Err(error.clone())).unwrap();
        assert_eq!(futures::executor::block_on(driver), Err(error));
    }

    #[test]
    fn dropping_optional_owned_driver_cancels_unpolled_work() {
        let (done_tx, done_rx) = futures::channel::oneshot::channel::<Result<()>>();
        let component = OwnedWork(async move { done_rx.await.unwrap() }.boxed());
        let (_channel, driver) = component.into_channel_and_future();
        assert!(driver.is_some());
        assert!(!done_tx.is_canceled());
        drop(driver);
        assert!(done_tx.is_canceled());
    }

    #[test]
    fn type_erasure_preserves_owned_work_and_finish_metadata() {
        let outgoing = futures::sink::unfold((), |(), _line: String| {
            futures::future::ready(Ok::<_, std::io::Error>(()))
        });
        // Independent physical input remains open: only a preserved explicit
        // finish handle can complete this driver without read EOF.
        let incoming = futures::stream::pending::<std::io::Result<String>>();
        let component = DynConnectTo::<UntypedRole>::new(crate::Lines::new(outgoing, incoming));
        let (_channel, driver) = component.into_channel_and_future();
        let mut driver = driver.expect("erasure must retain ownership");
        assert!((&mut driver).now_or_never().is_none());
        driver
            .take_finish()
            .expect("erasure must retain physical finish coordination")
            .send(())
            .unwrap();
        futures::executor::block_on(driver).unwrap();
    }

    #[test]
    fn type_erasure_preserves_passive_lifetime() {
        let (channel, _other) = Channel::duplex();
        let (_, driver) = DynConnectTo::<UntypedRole>::new(channel).into_channel_and_future();
        assert!(driver.is_none());
    }

    #[test]
    fn dyn_connect_to_reports_static_type_name_and_correct_debug_label() {
        let (channel, _other) = Channel::duplex();
        let component = DynConnectTo::<UntypedRole>::new(channel);

        let type_name: &'static str = component.type_name();
        assert_eq!(type_name, std::any::type_name::<Channel>());
        assert_eq!(
            format!("{component:?}"),
            format!("DynConnectTo {{ type_name: {type_name:?} }}")
        );
    }
}
