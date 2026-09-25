//! Runtime-neutral helpers for registering function-backed MCP tools.

use futures::{
    StreamExt,
    channel::oneshot,
    future::{self, BoxFuture, Either},
};
use schemars::JsonSchema;
use serde::{Serialize, de::DeserializeOwned};
use std::pin::Pin;

use crate::{ConnectionTo, Error, Role, RunWithConnectionTo};

use super::{McpConnectionTo, McpTool};

struct ToolCall<P, R, MyRole: Role> {
    params: P,
    mcp_connection: McpConnectionTo<MyRole>,
    result_tx: futures::channel::oneshot::Sender<Result<R, Error>>,
    done_tx: oneshot::Sender<()>,
}

struct ToolFnMutRunner<F, P, R, Counterpart: Role> {
    func: F,
    call_rx: Pin<Box<async_channel::Receiver<ToolCall<P, R, Counterpart>>>>,
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
            mut result_tx,
            done_tx,
        }) = call_rx.next().await
        {
            // The caller may have cancelled while this invocation waited behind
            // another mutable tool call. Do not start work for a gone caller.
            if result_tx.is_canceled() {
                drop(params);
                drop(mcp_connection);
                drop(result_tx);
                let _ = done_tx.send(());
                continue;
            }
            let result = {
                let cancelled = result_tx.cancellation();
                futures::pin_mut!(cancelled);
                match future::select(tool_future_fn(&mut func, params, mcp_connection), cancelled)
                    .await
                {
                    Either::Left((result, _)) => Some(result),
                    Either::Right(((), _)) => None,
                }
            };
            if let Some(result) = result {
                // Cancellation after execution is not a runner failure.
                drop(result_tx.send(result));
            }
            let _ = done_tx.send(());
        }
        Ok(())
    }
}

struct ToolFnRunner<F, P, R, Counterpart: Role> {
    func: F,
    call_rx: Pin<Box<async_channel::Receiver<ToolCall<P, R, Counterpart>>>>,
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
        call_rx
            .for_each_concurrent(64, |tool_call| {
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
                    mut result_tx: oneshot::Sender<Result<R, Error>>,
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
                            drop(result_tx);
                            let _ = done_tx.send(());
                            return;
                        }
                        let result = {
                            let cancelled = result_tx.cancellation();
                            futures::pin_mut!(cancelled);
                            match future::select(
                                tool_future_fn(func, params, mcp_connection),
                                cancelled,
                            )
                            .await
                            {
                                Either::Left((result, _)) => Some(result),
                                Either::Right(((), _)) => None,
                            }
                        };
                        if let Some(result) = result {
                            drop(result_tx.send(result));
                        }
                        let _ = done_tx.send(());
                    })
                }

                let ToolCall {
                    params,
                    mcp_connection,
                    result_tx,
                    done_tx,
                } = tool_call;

                hack(
                    &func,
                    params,
                    mcp_connection,
                    &*tool_future_fn,
                    result_tx,
                    done_tx,
                )
            })
            .await;
        Ok(())
    }
}

struct ToolFnTool<P, Ret, R: Role> {
    name: String,
    description: String,
    call_tx: async_channel::Sender<ToolCall<P, Ret, R>>,
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

        self.call_tx
            .send(ToolCall {
                params,
                mcp_connection,
                result_tx,
                done_tx,
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
    impl McpTool<Counterpart, Input = P, Output = Ret> + 'static,
    impl RunWithConnectionTo<Counterpart>,
)
where
    Counterpart: Role,
    P: JsonSchema + DeserializeOwned + 'static + Send,
    Ret: JsonSchema + Serialize + 'static + Send,
    F: AsyncFnMut(P, McpConnectionTo<Counterpart>) -> Result<Ret, Error> + Send,
{
    let (call_tx, call_rx) = async_channel::bounded(128);
    (
        ToolFnTool {
            name: name.to_string(),
            description: description.to_string(),
            call_tx,
        },
        ToolFnMutRunner {
            func,
            call_rx: Box::pin(call_rx),
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
    impl McpTool<Counterpart, Input = P, Output = Ret> + 'static,
    impl RunWithConnectionTo<Counterpart>,
)
where
    Counterpart: Role,
    P: JsonSchema + DeserializeOwned + 'static + Send,
    Ret: JsonSchema + Serialize + 'static + Send,
    F: AsyncFn(P, McpConnectionTo<Counterpart>) -> Result<Ret, Error> + Send + Sync + 'static,
{
    let (call_tx, call_rx) = async_channel::bounded(128);
    (
        ToolFnTool {
            name: name.to_string(),
            description: description.to_string(),
            call_tx,
        },
        ToolFnRunner {
            func,
            call_rx: Box::pin(call_rx),
            tool_future_fn: Box::new(tool_future_fn),
        },
    )
}
