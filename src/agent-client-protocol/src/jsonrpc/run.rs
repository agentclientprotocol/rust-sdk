//! Run trait for background tasks that run alongside a connection.
//!
//! Run implementations are composable background tasks that run while a connection is active.
//! They're used for things like MCP tool handlers that need to receive calls through
//! channels and invoke user-provided closures.

use std::future::Future;
use std::marker::PhantomData;

use futures::future::{Either, select};

use crate::{
    ConnectionTo,
    jsonrpc::{ConnectionContext, RawConnectionContext, connection_context},
    role::Role,
};

/// A background task that runs alongside a connection.
///
/// `RunIn<R>` means "run in the context of being role R". The task receives
/// a `ConnectionTo<R::Counterpart>` for communicating with the other side.
///
/// Implementations are composed using [`ChainRun`] and run in parallel
/// when the connection is active.
pub trait RunWithConnectionTo<Counterpart: Role>: Send {
    /// Run this task to completion.
    fn run_with_connection_to(
        self,
        cx: ConnectionTo<Counterpart>,
    ) -> impl Future<Output = Result<(), crate::Error>> + Send;
}

/// A no-op RunIn that completes immediately.
#[derive(Debug, Default)]
pub struct NullRun;

impl<Counterpart: Role> RunWithConnectionTo<Counterpart> for NullRun {
    fn run_with_connection_to(
        self,
        _cx: ConnectionTo<Counterpart>,
    ) -> impl Future<Output = Result<(), crate::Error>> + Send {
        std::future::ready(Ok(()))
    }
}

/// Chains two RunIn implementations to run in parallel.
#[derive(Debug)]
pub struct ChainRun<A, B> {
    a: A,
    b: B,
}

impl<A, B> ChainRun<A, B> {
    /// Create a new chained RunIn from two RunIn implementations.
    pub fn new(a: A, b: B) -> Self {
        Self { a, b }
    }
}

impl<Counterpart: Role, A, B> RunWithConnectionTo<Counterpart> for ChainRun<A, B>
where
    A: RunWithConnectionTo<Counterpart>,
    B: RunWithConnectionTo<Counterpart>,
{
    async fn run_with_connection_to(
        self,
        cx: ConnectionTo<Counterpart>,
    ) -> Result<(), crate::Error> {
        // Box the futures to avoid stack overflow with deeply nested RunIn chains
        let a_fut = Box::pin(self.a.run_with_connection_to(cx.clone()));
        let b_fut = Box::pin(self.b.run_with_connection_to(cx.clone()));
        match select(a_fut, b_fut).await {
            Either::Left((Ok(()), b)) => b.await,
            Either::Right((Ok(()), a)) => a.await,
            Either::Left((Err(error), b)) => {
                finish_runner_after_error(b, &cx).await;
                Err(error)
            }
            Either::Right((Err(error), a)) => {
                finish_runner_after_error(a, &cx).await;
                Err(error)
            }
        }
    }
}

async fn finish_runner_after_error<R: Role>(
    runner: impl Future<Output = Result<(), crate::Error>>,
    cx: &ConnectionTo<R>,
) {
    cx.request_shutdown();
    // A sibling runner can own the actual scoped operation. Keep it polled
    // while its supervisor awaits cleanup, but never join unrelated user work.
    match select(Box::pin(runner), Box::pin(cx.wait_protected_operations())).await {
        Either::Left((_, cleanup)) => cleanup.await,
        Either::Right(((), _)) => {}
    }
}

/// A RunIn created from a closure via [`with_spawned`](crate::Builder::with_spawned).
pub struct SpawnedRun<F, Context = RawConnectionContext> {
    task_fn: F,
    location: &'static std::panic::Location<'static>,
    context: PhantomData<fn() -> Context>,
}

impl<F, Context> SpawnedRun<F, Context> {
    /// Create a new spawned RunIn from a closure.
    pub fn new(location: &'static std::panic::Location<'static>, task_fn: F) -> Self {
        Self {
            task_fn,
            location,
            context: PhantomData,
        }
    }
}

impl<Counterpart, F, Fut, Context> RunWithConnectionTo<Counterpart> for SpawnedRun<F, Context>
where
    Counterpart: Role,
    Context: ConnectionContext,
    F: FnOnce(Context::Connection<Counterpart>) -> Fut + Send,
    Fut: Future<Output = Result<(), crate::Error>> + Send,
{
    async fn run_with_connection_to(
        self,
        connection: ConnectionTo<Counterpart>,
    ) -> Result<(), crate::Error> {
        let location = self.location;
        (self.task_fn)(connection_context::from_raw::<Context, _>(connection))
            .await
            .map_err(|err| {
                let data = err.data.clone();
                err.data(serde_json::json!({
                    "spawned_at": format!("{}:{}:{}", location.file(), location.line(), location.column()),
                    "data": data,
                }))
            })
    }
}
