use std::panic::Location;
use std::sync::{Arc, Mutex};

use futures::{
    FutureExt,
    channel::{mpsc, oneshot},
    future::{self, BoxFuture, Either},
};

use crate::ConnectionTo;
use crate::role::Role;
use crate::util::process_stream_concurrently;

pub type TaskTx = mpsc::UnboundedSender<Task>;

#[must_use]
pub(crate) struct Task {
    future: BoxFuture<'static, Result<(), crate::Error>>,
}

impl Task {
    pub fn new(
        location: &'static Location<'static>,
        task_future: impl IntoFuture<Output = Result<(), crate::Error>, IntoFuture: Send + 'static>,
    ) -> Self {
        let task_future = task_future.into_future();
        Task {
            future: futures::FutureExt::map(
                task_future,
                |result| match result {
                    Ok(()) => Ok(()),
                    Err(err) => {
                        let data = err.data.clone();
                        Err(err.data(serde_json::json! {
                            {
                                "spawned_at": format!("{}:{}:{}", location.file(), location.line(), location.column()),
                                "data": data,
                            }
                        }))
                    }
                },
            )
            .boxed()
        }
    }

    pub fn spawn(self, task_tx: &TaskTx) -> Result<(), crate::Error> {
        task_tx
            .unbounded_send(self)
            .map_err(crate::util::internal_error)?;
        Ok(())
    }

    #[cfg(test)]
    pub(super) async fn run_for_test(self) -> Result<(), crate::Error> {
        self.future.await
    }
}

/// The "task actor" manages dynamically spawned tasks.
pub(super) async fn task_actor<R: Role>(
    task_rx: mpsc::UnboundedReceiver<Task>,
    cx: &ConnectionTo<R>,
) -> Result<(), crate::Error> {
    let (error_tx, error_rx) = oneshot::channel();
    let first_error = Arc::new(Mutex::new(Some(error_tx)));
    let running = process_stream_concurrently(
        task_rx,
        async |task: Task| {
            if let Err(error) = task.future.await
                && let Some(tx) = first_error
                    .lock()
                    .expect("task error mutex poisoned")
                    .take()
            {
                drop(tx.send(error));
            }
            Ok(())
        },
        |a, b| Box::pin(a(b)),
    );
    let on_error = async {
        let error = error_rx
            .await
            .expect("task driver dropped before completion");
        cx.request_shutdown();
        // Keep polling all tasks until owned supervisors acknowledge cleanup.
        // Do not join arbitrary never-ending application tasks afterwards.
        cx.wait_protected_operations().await;
        Err(error)
    };
    match future::select(Box::pin(running), Box::pin(on_error)).await {
        Either::Left((result, _)) | Either::Right((result, _)) => result,
    }
}
