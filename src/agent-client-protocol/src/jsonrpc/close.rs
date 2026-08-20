//! Connection-close handlers.

use std::fmt::Debug;
use std::future::Future;
use std::marker::PhantomData;

use crate::{
    ConnectionTo,
    jsonrpc::{ConnectionContext, RawConnectionContext, connection_context},
    role::Role,
};

/// A handler that runs after the incoming transport reaches clean EOF.
///
/// Close handlers are composed by [`Builder::on_close`](crate::Builder::on_close)
/// and run sequentially in registration order. Unlike
/// [`RunWithConnectionTo`](crate::RunWithConnectionTo), they are a distinct
/// connection-lifecycle phase rather than concurrent background work.
pub trait HandleConnectionClose<Counterpart: Role>: Send {
    /// Run this handler during clean incoming-transport shutdown.
    fn handle_connection_close(
        self,
        connection: ConnectionTo<Counterpart>,
    ) -> impl Future<Output = Result<(), crate::Error>> + Send;
}

/// A close handler that does nothing.
#[derive(Debug, Default)]
pub struct NullClose;

impl<Counterpart: Role> HandleConnectionClose<Counterpart> for NullClose {
    fn handle_connection_close(
        self,
        _connection: ConnectionTo<Counterpart>,
    ) -> impl Future<Output = Result<(), crate::Error>> + Send {
        std::future::ready(Ok(()))
    }
}

pub(crate) struct CloseCallback<F, Context = RawConnectionContext> {
    callback: F,
    context: PhantomData<fn() -> Context>,
}

impl<F, Context> CloseCallback<F, Context> {
    pub(crate) fn new(callback: F) -> Self {
        Self {
            callback,
            context: PhantomData,
        }
    }
}

impl<F, Context> Debug for CloseCallback<F, Context> {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("CloseCallback")
            .finish_non_exhaustive()
    }
}

impl<Counterpart, F, Fut, Context> HandleConnectionClose<Counterpart> for CloseCallback<F, Context>
where
    Counterpart: Role,
    Context: ConnectionContext,
    F: FnOnce(Context::Connection<Counterpart>) -> Fut + Send,
    Fut: Future<Output = Result<(), crate::Error>> + Send,
{
    async fn handle_connection_close(
        self,
        connection: ConnectionTo<Counterpart>,
    ) -> Result<(), crate::Error> {
        let result = (self.callback)(connection_context::from_raw::<Context, _>(connection)).await;
        if let Err(error) = &result {
            tracing::warn!(?error, "Connection close callback failed");
        }
        result
    }
}

#[derive(Debug)]
pub(crate) struct ChainedClose<A, B> {
    first: A,
    second: B,
}

impl<A, B> ChainedClose<A, B> {
    pub(crate) fn new(first: A, second: B) -> Self {
        Self { first, second }
    }
}

impl<Counterpart, A, B> HandleConnectionClose<Counterpart> for ChainedClose<A, B>
where
    Counterpart: Role,
    A: HandleConnectionClose<Counterpart>,
    B: HandleConnectionClose<Counterpart>,
{
    async fn handle_connection_close(
        self,
        connection: ConnectionTo<Counterpart>,
    ) -> Result<(), crate::Error> {
        // Box each side to keep deeply composed close chains from producing
        // correspondingly deep connection-driver futures.
        let first = Box::pin(self.first.handle_connection_close(connection.clone())).await;
        let second = Box::pin(self.second.handle_connection_close(connection)).await;
        first.and(second)
    }
}
