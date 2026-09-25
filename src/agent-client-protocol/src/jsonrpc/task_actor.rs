use std::panic::Location;

use futures::{FutureExt, StreamExt, future::BoxFuture};

use crate::ConnectionTo;
use crate::role::Role;

pub type TaskTx = super::admission::Sender<Task>;

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
    task_rx: super::admission::SimpleReceiver<Task>,
    _cx: &ConnectionTo<R>,
    max_running_tasks: usize,
) -> Result<(), crate::Error> {
    use futures::TryStreamExt as _;
    task_rx
        .map(Ok::<_, crate::Error>)
        .try_for_each_concurrent(max_running_tasks.max(1), |task| task.future)
        .await
}
