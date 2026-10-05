//! Runtime-neutral helpers for registering function-backed MCP tools.

use futures::{
    SinkExt, StreamExt,
    channel::{mpsc, oneshot},
    future::{self, BoxFuture, Either},
};
use schemars::JsonSchema;
use serde::{Serialize, de::DeserializeOwned};

use crate::{ConnectionTo, Error, Role, RunWithConnectionTo};

use super::{McpConnectionTo, McpTool};

struct ToolCall<P, R, MyRole: Role> {
    params: P,
    mcp_connection: McpConnectionTo<MyRole>,
    result_tx: futures::channel::oneshot::Sender<Result<R, Error>>,
}

/// Dropping the result receiver cancels the invocation by dropping its user future.
async fn run_call<R>(
    future: impl Future<Output = Result<R, Error>>,
    mut result_tx: oneshot::Sender<Result<R, Error>>,
) {
    let result = {
        let cancelled = result_tx.cancellation();
        futures::pin_mut!(future, cancelled);
        match future::select(cancelled, future).await {
            Either::Left(_) => None,
            Either::Right((result, _)) => Some(result),
        }
    };
    if let Some(result) = result {
        // A caller leaving is not a failure of the shared tool runner.
        drop(result_tx.send(result));
    }
}

struct ToolFnMutRunner<F, P, R, Counterpart: Role> {
    func: F,
    call_rx: mpsc::Receiver<ToolCall<P, R, Counterpart>>,
    tool_future_fn: Box<
        dyn for<'a> Fn(
                &'a mut F,
                P,
                McpConnectionTo<Counterpart>,
            ) -> BoxFuture<'a, Result<R, Error>>
            + Send,
    >,
}

impl<F, P, R, Counterpart, Counterpart1> RunWithConnectionTo<Counterpart1>
    for ToolFnMutRunner<F, P, R, Counterpart>
where
    Counterpart: Role,
    Counterpart1: Role,
    P: Send,
    R: Send,
    F: Send,
{
    async fn run_with_connection_to(
        self,
        _connection: ConnectionTo<Counterpart1>,
    ) -> Result<(), Error> {
        let ToolFnMutRunner {
            mut func,
            mut call_rx,
            tool_future_fn,
        } = self;
        while let Some(ToolCall {
            params,
            mcp_connection,
            result_tx,
        }) = call_rx.next().await
        {
            if result_tx.is_canceled() {
                continue;
            }
            run_call(tool_future_fn(&mut func, params, mcp_connection), result_tx).await;
        }
        Ok(())
    }
}

struct ToolFnRunner<F, P, R, Counterpart: Role> {
    func: F,
    call_rx: mpsc::Receiver<ToolCall<P, R, Counterpart>>,
    tool_future_fn: Box<
        dyn for<'a> Fn(&'a F, P, McpConnectionTo<Counterpart>) -> BoxFuture<'a, Result<R, Error>>
            + Send
            + Sync,
    >,
}

impl<F, P, R, Counterpart, Counterpart1> RunWithConnectionTo<Counterpart1>
    for ToolFnRunner<F, P, R, Counterpart>
where
    Counterpart: Role,
    Counterpart1: Role,
    P: Send,
    R: Send,
    F: Send + Sync,
{
    async fn run_with_connection_to(
        self,
        _connection: ConnectionTo<Counterpart1>,
    ) -> Result<(), Error> {
        let ToolFnRunner {
            func,
            call_rx,
            tool_future_fn,
        } = self;
        crate::util::process_stream_concurrently(
            call_rx,
            async |tool_call| {
                fn hack<'a, F, P, R, MyRole>(
                    func: &'a F,
                    params: P,
                    mcp_connection: McpConnectionTo<MyRole>,
                    tool_future_fn: &'a (
                            dyn Fn(
                        &'a F,
                        P,
                        McpConnectionTo<MyRole>,
                    ) -> BoxFuture<'a, Result<R, Error>>
                                + Send
                                + Sync
                        ),
                    result_tx: oneshot::Sender<Result<R, Error>>,
                ) -> BoxFuture<'a, ()>
                where
                    MyRole: Role,
                    P: Send,
                    R: Send,
                    F: Send + Sync,
                {
                    Box::pin(async move {
                        if result_tx.is_canceled() {
                            return;
                        }
                        run_call(tool_future_fn(func, params, mcp_connection), result_tx).await;
                    })
                }

                let ToolCall {
                    params,
                    mcp_connection,
                    result_tx,
                } = tool_call;

                hack(&func, params, mcp_connection, &*tool_future_fn, result_tx).await;
                Ok(())
            },
            |a, b| Box::pin(a(b)),
        )
        .await
    }
}

struct ToolFnTool<P, Ret, R: Role> {
    name: String,
    description: String,
    call_tx: mpsc::Sender<ToolCall<P, Ret, R>>,
}

impl<P, Ret, R> McpTool<R> for ToolFnTool<P, Ret, R>
where
    R: Role,
    P: JsonSchema + DeserializeOwned + 'static + Send,
    Ret: JsonSchema + Serialize + 'static + Send,
{
    type Input = P;
    type Output = Ret;

    fn name(&self) -> String {
        self.name.clone()
    }

    fn description(&self) -> String {
        self.description.clone()
    }

    async fn call_tool(&self, params: P, mcp_connection: McpConnectionTo<R>) -> Result<Ret, Error> {
        let (result_tx, result_rx) = oneshot::channel();

        self.call_tx
            .clone()
            .send(ToolCall {
                params,
                mcp_connection,
                result_tx,
            })
            .await
            .map_err(crate::util::internal_error)?;

        result_rx.await.map_err(crate::util::internal_error)?
    }
}

/// Create a "single-threaded" function-backed MCP tool and its runner.
///
/// Only one invocation of the tool can be running at a time.
pub fn tool_fn_mut<P, Ret, F, Counterpart>(
    name: impl ToString,
    description: impl ToString,
    func: F,
    tool_future_fn: impl for<'a> Fn(
        &'a mut F,
        P,
        McpConnectionTo<Counterpart>,
    ) -> BoxFuture<'a, Result<Ret, Error>>
    + Send
    + 'static,
) -> (
    impl McpTool<Counterpart> + 'static,
    impl RunWithConnectionTo<Counterpart>,
)
where
    Counterpart: Role,
    P: JsonSchema + DeserializeOwned + 'static + Send,
    Ret: JsonSchema + Serialize + 'static + Send,
    F: AsyncFnMut(P, McpConnectionTo<Counterpart>) -> Result<Ret, Error> + Send,
{
    let (call_tx, call_rx) = mpsc::channel(128);
    (
        ToolFnTool {
            name: name.to_string(),
            description: description.to_string(),
            call_tx,
        },
        ToolFnMutRunner {
            func,
            call_rx,
            tool_future_fn: Box::new(tool_future_fn),
        },
    )
}

/// Create a stateless function-backed MCP tool and its concurrent runner.
pub fn tool_fn<P, Ret, F, Counterpart>(
    name: impl ToString,
    description: impl ToString,
    func: F,
    tool_future_fn: impl for<'a> Fn(
        &'a F,
        P,
        McpConnectionTo<Counterpart>,
    ) -> BoxFuture<'a, Result<Ret, Error>>
    + Send
    + Sync
    + 'static,
) -> (
    impl McpTool<Counterpart> + 'static,
    impl RunWithConnectionTo<Counterpart>,
)
where
    Counterpart: Role,
    P: JsonSchema + DeserializeOwned + 'static + Send,
    Ret: JsonSchema + Serialize + 'static + Send,
    F: AsyncFn(P, McpConnectionTo<Counterpart>) -> Result<Ret, Error> + Send + Sync + 'static,
{
    let (call_tx, call_rx) = mpsc::channel(128);
    (
        ToolFnTool {
            name: name.to_string(),
            description: description.to_string(),
            call_tx,
        },
        ToolFnRunner {
            func,
            call_rx,
            tool_future_fn: Box::new(tool_future_fn),
        },
    )
}

#[cfg(test)]
mod tests {
    use std::{
        pin::Pin,
        sync::Mutex,
        task::{Context, Poll},
    };

    use futures::FutureExt as _;

    use super::*;
    use crate::{Channel, mcp_server::McpConnectionContext, role::mcp};

    type ResultReceiver = oneshot::Receiver<Result<u32, Error>>;

    #[derive(Default)]
    struct State {
        entered: Mutex<Vec<u32>>,
        dropped: Mutex<Vec<u32>>,
        discard_result: Mutex<Option<ResultReceiver>>,
    }

    /// Observe the actual user future's destructor, not a runner completion signal.
    struct UserFuture<'a> {
        state: &'a State,
        id: u32,
    }

    impl Future for UserFuture<'_> {
        type Output = Result<u32, Error>;

        fn poll(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Self::Output> {
            if self.id == 0 {
                Poll::Pending
            } else {
                // Cancellation is first polled while the receiver is alive.
                // Dropping it and completing in this same poll forces send to
                // fail in the delivery-race test.
                drop(self.state.discard_result.lock().unwrap().take());
                Poll::Ready(Ok(self.id))
            }
        }
    }

    impl Drop for UserFuture<'_> {
        fn drop(&mut self) {
            self.state.dropped.lock().unwrap().push(self.id);
        }
    }

    #[derive(Clone, Copy)]
    enum Mode {
        Mutable,
        Concurrent,
    }

    fn runner(
        mode: Mode,
        state: &State,
        call_rx: mpsc::Receiver<ToolCall<u32, u32, mcp::Client>>,
        connection: ConnectionTo<mcp::Client>,
    ) -> BoxFuture<'_, Result<(), Error>> {
        match mode {
            Mode::Mutable => Box::pin(
                ToolFnMutRunner {
                    // Borrow external state and mutable closure state across
                    // suspension, as supported by the existing public API.
                    func: (state, Vec::<u32>::new()),
                    call_rx,
                    tool_future_fn: Box::new(|func, id, _connection| {
                        func.0.entered.lock().unwrap().push(id);
                        Box::pin(async move {
                            let result = UserFuture { state: func.0, id }.await;
                            func.1.push(id);
                            result
                        })
                    }),
                }
                .run_with_connection_to(connection),
            ),
            Mode::Concurrent => Box::pin(
                ToolFnRunner {
                    func: state,
                    call_rx,
                    tool_future_fn: Box::new(|state, id, _connection| {
                        // Entry is recorded before polling the user future.
                        state.entered.lock().unwrap().push(id);
                        Box::pin(UserFuture { state, id })
                    }),
                }
                .run_with_connection_to(connection),
            ),
        }
    }

    async fn enqueue(
        tool: &ToolFnTool<u32, u32, mcp::Client>,
        id: u32,
        connection: &McpConnectionTo<mcp::Client>,
    ) -> ResultReceiver {
        let (result_tx, result_rx) = oneshot::channel();
        // Await admission while the runner is paused: cancellation cannot
        // accidentally happen before the call is actually queued.
        tool.call_tx
            .clone()
            .send(ToolCall {
                params: id,
                mcp_connection: connection.clone(),
                result_tx,
            })
            .await
            .unwrap();
        result_rx
    }

    fn assert_pending(future: impl Future) {
        assert!(future.now_or_never().is_none());
    }

    #[derive(Clone, Copy)]
    enum Case {
        Running,
        Queued,
        DeliveryRace,
        ConcurrentProgress,
    }

    fn check(mode: Mode, case: Case) {
        let (channel, _peer) = Channel::duplex();
        futures::executor::block_on(mcp::Server.builder().connect_with(
            channel,
            async |connection| {
                let context = McpConnectionTo {
                    context: McpConnectionContext::Standalone,
                    connection: connection.clone(),
                };
                let state = State::default();
                let (call_tx, call_rx) = mpsc::channel(128);
                let tool = ToolFnTool {
                    name: "test".into(),
                    description: "test".into(),
                    call_tx,
                };
                let mut runner = runner(mode, &state, call_rx, connection);

                match case {
                    Case::Running | Case::ConcurrentProgress => {
                        let mut first = Box::pin(tool.call_tool(0, context.clone()));
                        assert_pending(first.as_mut());
                        assert_pending(runner.as_mut());
                        assert_eq!(*state.entered.lock().unwrap(), [0]);
                        assert!(state.dropped.lock().unwrap().is_empty());

                        if matches!(case, Case::ConcurrentProgress) {
                            let mut next = enqueue(&tool, 1, &context).await;
                            assert_pending(runner.as_mut());
                            assert_eq!(next.try_recv().unwrap().unwrap().unwrap(), 1);
                            // The second call finished while the first remained
                            // suspended, proving concurrent execution.
                            assert_eq!(*state.dropped.lock().unwrap(), [1]);
                        }

                        drop(first);
                        assert_pending(runner.as_mut());
                        assert!(state.dropped.lock().unwrap().contains(&0));
                    }
                    Case::Queued => {
                        drop(enqueue(&tool, 2, &context).await);
                        assert!(state.entered.lock().unwrap().is_empty());

                        let first = enqueue(&tool, 0, &context).await;
                        assert_pending(runner.as_mut());
                        assert_eq!(*state.entered.lock().unwrap(), [0]);
                        assert!(state.dropped.lock().unwrap().is_empty());

                        // Also cancel queued work while the first call is
                        // running, without polling the runner between send/drop.
                        drop(enqueue(&tool, 3, &context).await);
                        drop(first);
                        assert_pending(runner.as_mut());
                        assert_eq!(*state.entered.lock().unwrap(), [0]);
                        assert_eq!(*state.dropped.lock().unwrap(), [0]);
                    }
                    Case::DeliveryRace => {
                        let receiver = enqueue(&tool, 4, &context).await;
                        *state.discard_result.lock().unwrap() = Some(receiver);
                        assert_pending(runner.as_mut());
                        assert!(state.discard_result.lock().unwrap().is_none());
                        assert_eq!(*state.entered.lock().unwrap(), [4]);
                        assert_eq!(*state.dropped.lock().unwrap(), [4]);
                    }
                }

                // Every scenario leaves the runner usable for another call.
                let mut next = Box::pin(tool.call_tool(5, context));
                assert_pending(next.as_mut());
                assert_pending(runner.as_mut());
                assert_eq!(next.now_or_never().unwrap().unwrap(), 5);
                assert_eq!(state.entered.lock().unwrap().last(), Some(&5));
                assert_eq!(state.dropped.lock().unwrap().last(), Some(&5));
                drop(tool);
                runner.now_or_never().unwrap().unwrap();
                Ok(())
            },
        ))
        .unwrap();
    }

    #[test]
    fn mutable_running_cancellation_drops_user_future_and_allows_next_call() {
        check(Mode::Mutable, Case::Running);
    }

    #[test]
    fn concurrent_running_cancellation_drops_user_future_and_allows_next_call() {
        check(Mode::Concurrent, Case::Running);
    }

    #[test]
    fn mutable_cancelled_queued_calls_never_enter_closure() {
        check(Mode::Mutable, Case::Queued);
    }

    #[test]
    fn concurrent_cancelled_queued_calls_never_enter_closure() {
        check(Mode::Concurrent, Case::Queued);
    }

    #[test]
    fn mutable_failed_result_delivery_does_not_stop_runner() {
        check(Mode::Mutable, Case::DeliveryRace);
    }

    #[test]
    fn concurrent_failed_result_delivery_does_not_stop_runner() {
        check(Mode::Concurrent, Case::DeliveryRace);
    }

    #[test]
    fn concurrent_borrowed_futures_make_independent_progress() {
        check(Mode::Concurrent, Case::ConcurrentProgress);
    }
}
