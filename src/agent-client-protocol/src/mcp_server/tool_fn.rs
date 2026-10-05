//! Runtime-neutral helpers for registering function-backed MCP tools.

use futures::{
    SinkExt, StreamExt,
    channel::{mpsc, oneshot},
    future::{self, BoxFuture, Either},
};
use schemars::JsonSchema;
use serde::{Serialize, de::DeserializeOwned};
use std::sync::{Arc, Mutex};

use crate::{ConnectionTo, Error, Role, RunWithConnectionTo};

use super::{McpConnectionTo, McpTool};

struct ToolCall<P, R, MyRole: Role> {
    params: P,
    mcp_connection: McpConnectionTo<MyRole>,
    result_tx: futures::channel::oneshot::Sender<Result<R, Error>>,
    done_tx: oneshot::Sender<()>,
}

/// The channel owns a queue slot, not exclusive ownership of its
/// payload. Either the caller or the runner may claim that payload exactly once.
/// This lets cancellation destroy queued work even while a mutable invocation
/// borrows the closure and the runner cannot receive another queue entry.
struct QueuedCall<P, R, MyRole: Role>(Arc<Mutex<Option<ToolCall<P, R, MyRole>>>>);

impl<P, R, MyRole: Role> QueuedCall<P, R, MyRole> {
    fn share(&self) -> Self {
        Self(self.0.clone())
    }

    fn take(&self) -> Option<ToolCall<P, R, MyRole>> {
        self.0.lock().unwrap().take()
    }
}

impl<P, R, MyRole: Role> Drop for QueuedCall<P, R, MyRole> {
    fn drop(&mut self) {
        if let Some(ToolCall {
            params,
            mcp_connection,
            result_tx,
            done_tx,
        }) = self.take()
        {
            // Release the lock before user destructors. Cleanup must observe
            // destruction of both queued input and its host context first.
            drop(params);
            drop(mcp_connection);
            drop(result_tx);
            let _finished = done_tx.send(());
        }
    }
}

struct CallResult<P, R, MyRole: Role> {
    // Fields drop in declaration order: cancel running work before trying to
    // reclaim queued work. A runner that won take() owns the cleanup ack.
    result_rx: oneshot::Receiver<Result<R, Error>>,
    queued_call: QueuedCall<P, R, MyRole>,
}

/// A result receiver's lifetime is the invocation's cancellation scope. Signal
/// completion only after the actual user future has completed or been dropped.
async fn run_call<R>(
    future: impl Future<Output = Result<R, Error>>,
    mut result_tx: oneshot::Sender<Result<R, Error>>,
    done_tx: oneshot::Sender<()>,
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
    let _finished = done_tx.send(());
}

struct ToolFnMutRunner<F, P, R, Counterpart: Role> {
    func: F,
    call_rx: mpsc::Receiver<QueuedCall<P, R, Counterpart>>,
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
        while let Some(queued_call) = call_rx.next().await {
            let Some(ToolCall {
                params,
                mcp_connection,
                result_tx,
                done_tx,
            }) = queued_call.take()
            else {
                continue;
            };
            if result_tx.is_canceled() {
                drop(params);
                drop(mcp_connection);
                let _finished = done_tx.send(());
                continue;
            }
            run_call(
                tool_future_fn(&mut func, params, mcp_connection),
                result_tx,
                done_tx,
            )
            .await;
        }
        Ok(())
    }
}

struct ToolFnRunner<F, P, R, Counterpart: Role> {
    func: F,
    call_rx: mpsc::Receiver<QueuedCall<P, R, Counterpart>>,
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
                    done_tx: oneshot::Sender<()>,
                ) -> BoxFuture<'a, ()>
                where
                    MyRole: Role,
                    P: Send,
                    R: Send,
                    F: Send + Sync,
                {
                    Box::pin(async move {
                        if result_tx.is_canceled() {
                            drop(params);
                            drop(mcp_connection);
                            let _finished = done_tx.send(());
                            return;
                        }
                        run_call(
                            tool_future_fn(func, params, mcp_connection),
                            result_tx,
                            done_tx,
                        )
                        .await;
                    })
                }

                let Some(ToolCall {
                    params,
                    mcp_connection,
                    result_tx,
                    done_tx,
                }) = tool_call.take()
                else {
                    return Ok(());
                };

                hack(
                    &func,
                    params,
                    mcp_connection,
                    &*tool_future_fn,
                    result_tx,
                    done_tx,
                )
                .await;
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
    call_tx: mpsc::Sender<QueuedCall<P, Ret, R>>,
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
        let (done_tx, done_rx) = oneshot::channel();
        #[cfg(feature = "unstable_mcp_over_acp")]
        mcp_connection.register_cleanup(done_rx);
        #[cfg(not(feature = "unstable_mcp_over_acp"))]
        let _done_rx = done_rx;

        let mut call = CallResult {
            result_rx,
            queued_call: QueuedCall(Arc::new(Mutex::new(Some(ToolCall {
                params,
                mcp_connection,
                result_tx,
                done_tx,
            })))),
        };
        self.call_tx
            .clone()
            .send(call.queued_call.share())
            .await
            .map_err(crate::util::internal_error)?;

        (&mut call.result_rx)
            .await
            .map_err(crate::util::internal_error)?
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

    type ResultReceiver = CallResult<u32, u32, mcp::Client>;

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
        call_rx: mpsc::Receiver<QueuedCall<u32, u32, mcp::Client>>,
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
        let (done_tx, done_rx) = oneshot::channel();
        drop(done_rx);
        let call = CallResult {
            result_rx,
            queued_call: QueuedCall(Arc::new(Mutex::new(Some(ToolCall {
                params: id,
                mcp_connection: connection.clone(),
                result_tx,
                done_tx,
            })))),
        };
        // Await admission while the runner is paused: cancellation cannot
        // accidentally happen before the call is actually queued.
        tool.call_tx
            .clone()
            .send(call.queued_call.share())
            .await
            .unwrap();
        call
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
                    #[cfg(feature = "unstable_mcp_over_acp")]
                    cleanup: None,
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
                            assert_eq!(next.result_rx.try_recv().unwrap().unwrap().unwrap(), 1);
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

                        // A real call future is definitely admitted behind A
                        // before cancellation. No further runner poll is needed
                        // to destroy B and acknowledge its cleanup.
                        let queued_context = context.clone();
                        #[cfg(feature = "unstable_mcp_over_acp")]
                        let queued_context = McpConnectionTo {
                            cleanup: Some(Arc::new(Mutex::new(Vec::new()))),
                            ..queued_context
                        };
                        let mut queued = Box::pin(tool.call_tool(3, queued_context.clone()));
                        assert_pending(queued.as_mut());
                        drop(queued);
                        #[cfg(feature = "unstable_mcp_over_acp")]
                        queued_context.wait_cleanup().now_or_never().unwrap();
                        assert_eq!(*state.entered.lock().unwrap(), [0]);
                        assert!(state.dropped.lock().unwrap().is_empty());
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

    #[cfg(feature = "unstable_mcp_over_acp")]
    #[derive(serde::Deserialize, JsonSchema)]
    struct DropParams {
        #[serde(skip)]
        #[schemars(skip)]
        on_drop: Option<Box<dyn FnOnce() + Send>>,
    }

    #[cfg(feature = "unstable_mcp_over_acp")]
    impl Drop for DropParams {
        fn drop(&mut self) {
            if let Some(on_drop) = self.on_drop.take() {
                on_drop();
            }
        }
    }

    #[cfg(feature = "unstable_mcp_over_acp")]
    #[test]
    fn cancellation_during_enqueue_destroys_payload_before_cleanup_ack() {
        let (channel, _peer) = Channel::duplex();
        futures::executor::block_on(mcp::Server.builder().connect_with(
            channel,
            async |connection| {
                type QueueState = Mutex<Option<ToolCall<DropParams, u32, mcp::Client>>>;

                let cleanup = Arc::new(Mutex::new(Vec::<oneshot::Receiver<()>>::new()));
                let context = McpConnectionTo {
                    context: McpConnectionContext::Standalone,
                    connection,
                    cleanup: Some(cleanup.clone()),
                };
                // Force send() to suspend in its flush, with the payload
                // already in the channel but no runner receiving it.
                let (call_tx, mut call_rx) = mpsc::channel(0);
                let tool = ToolFnTool::<DropParams, u32, _> {
                    name: "test".into(),
                    description: "test".into(),
                    call_tx,
                };
                let dropped = Arc::new(Mutex::new(false));
                let queue_state = Arc::new(Mutex::new(None::<std::sync::Weak<QueueState>>));
                let params = DropParams {
                    on_drop: Some(Box::new({
                        let dropped = dropped.clone();
                        let cleanup = cleanup.clone();
                        let queue_state = queue_state.clone();
                        move || {
                            assert!(
                                cleanup.lock().unwrap()[0].try_recv().unwrap().is_none(),
                                "cleanup ack preceded queued params destructor"
                            );
                            let queue = queue_state
                                .lock()
                                .unwrap()
                                .as_ref()
                                .unwrap()
                                .upgrade()
                                .unwrap();
                            assert!(
                                queue.try_lock().is_ok(),
                                "params destructor ran under queue lock"
                            );
                            *dropped.lock().unwrap() = true;
                        }
                    })),
                };
                let mut call = Box::pin(tool.call_tool(params, context.clone()));
                assert_pending(call.as_mut());
                assert_eq!(cleanup.lock().unwrap().len(), 1);
                let queued = call_rx.next().now_or_never().unwrap().unwrap();
                *queue_state.lock().unwrap() = Some(Arc::downgrade(&queued.0));
                // Do not poll the caller after receive: its send future is
                // still suspended, not yet awaiting result_rx.
                drop(call);
                assert!(*dropped.lock().unwrap());
                assert!(
                    queued.take().is_none(),
                    "cancelled payload retained by queue"
                );
                assert_eq!(
                    Arc::strong_count(&cleanup),
                    2,
                    "queued host context survived cleanup acknowledgment"
                );
                context.wait_cleanup().now_or_never().unwrap();
                Ok(())
            },
        ))
        .unwrap();
    }

    #[test]
    fn running_cleanup_ack_follows_actual_future_destructor() {
        struct PendingUser {
            done: Arc<Mutex<oneshot::Receiver<()>>>,
            dropped: Arc<Mutex<bool>>,
        }
        impl Future for PendingUser {
            type Output = Result<(), Error>;
            fn poll(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<Self::Output> {
                Poll::Pending
            }
        }
        impl Drop for PendingUser {
            fn drop(&mut self) {
                assert!(self.done.lock().unwrap().try_recv().unwrap().is_none());
                *self.dropped.lock().unwrap() = true;
            }
        }
        let (result_tx, result_rx) = oneshot::channel();
        let (done_tx, done_rx) = oneshot::channel();
        let done = Arc::new(Mutex::new(done_rx));
        let dropped = Arc::new(Mutex::new(false));
        let mut call = Box::pin(run_call(
            PendingUser {
                done: done.clone(),
                dropped: dropped.clone(),
            },
            result_tx,
            done_tx,
        ));
        assert_pending(call.as_mut());
        drop(result_rx);
        call.now_or_never().unwrap();
        assert!(*dropped.lock().unwrap());
        assert_eq!(done.lock().unwrap().try_recv().unwrap(), Some(()));
    }
}
