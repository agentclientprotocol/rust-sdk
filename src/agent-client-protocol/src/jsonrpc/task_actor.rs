use std::panic::Location;
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicUsize, Ordering},
};

use futures::channel::oneshot;
use futures::future::{self, Either};
use futures::{FutureExt, StreamExt, future::BoxFuture};

use crate::ConnectionTo;
use crate::role::Role;

#[derive(Clone, Debug)]
pub struct TaskTx {
    sender: super::admission::Sender<Task>,
    live: Arc<AtomicUsize>,
    capacity: usize,
}

pub fn task_channel(capacity: usize) -> (TaskTx, super::admission::SimpleReceiver<Task>) {
    let capacity = capacity.max(1);
    let (sender, receiver) = super::admission::channel_with_capacity(capacity);
    (
        TaskTx {
            sender,
            live: Arc::new(AtomicUsize::new(0)),
            capacity,
        },
        receiver,
    )
}

struct LiveTask(Arc<AtomicUsize>);

impl Drop for LiveTask {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::AcqRel);
    }
}

#[must_use]
pub(crate) struct Task {
    future: BoxFuture<'static, Result<(), crate::Error>>,
    live: Option<LiveTask>,
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
            .boxed(),
            live: None,
        }
    }

    pub fn spawn(mut self, task_tx: &TaskTx) -> Result<(), crate::Error> {
        task_tx
            .live
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |live| {
                (live < task_tx.capacity).then_some(live + 1)
            })
            .map_err(|_| crate::util::internal_error("live task capacity exceeded"))?;
        self.live = Some(LiveTask(task_tx.live.clone()));
        task_tx
            .sender
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
    cx: &ConnectionTo<R>,
    max_running_tasks: usize,
) -> Result<(), crate::Error> {
    let (error_tx, error_rx) = oneshot::channel();
    let first_error = Arc::new(Mutex::new(Some(error_tx)));
    let running = task_rx.for_each_concurrent(max_running_tasks.max(1), |task| {
        let first_error = first_error.clone();
        async move {
            let Task { future, live } = task;
            let result = future.await;
            drop(live);
            if let Err(error) = result
                && let Some(tx) = first_error
                    .lock()
                    .expect("task error mutex poisoned")
                    .take()
            {
                drop(tx.send(error));
            }
        }
    });
    let on_error = async {
        let error = error_rx
            .await
            .expect("task driver dropped before completion");
        cx.incoming_closed.request_shutdown();
        // Keep polling the driver while native supervisors finish. A failed
        // disposable task cannot drop those supervisors or force us to join
        // arbitrary never-ending disposable tasks.
        cx.wait_protected_operations().await;
        Err(error)
    };
    match future::select(Box::pin(running), Box::pin(on_error)).await {
        Either::Left(((), _)) => Ok(()),
        Either::Right((result, _)) => result,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::channel::oneshot;

    #[test]
    fn running_child_occupies_total_live_capacity_not_just_queue_capacity() {
        futures::executor::block_on(async {
            let (tx, mut rx) = task_channel(1);
            let (child_done_tx, child_done_rx) = oneshot::channel::<()>();
            Task::new(Location::caller(), async move {
                let _ = child_done_rx.await;
                Ok(())
            })
            .spawn(&tx)
            .unwrap();
            // The child is no longer in the waiting queue, but remains live.
            let child = rx.next().await.unwrap();
            let (callback_dropped_tx, callback_dropped_rx) = oneshot::channel::<()>();
            let callback = async move {
                let _drop_on_rejection = callback_dropped_tx;
                futures::future::pending::<()>().await;
                Ok(())
            };
            let rejection = Task::new(Location::caller(), callback)
                .spawn(&tx)
                .expect_err("an ordered callback cannot wait behind a permanent child");
            assert!(rejection.to_string().contains("live task capacity"));
            assert!(callback_dropped_rx.now_or_never().unwrap().is_err());

            drop(child_done_tx);
            child.run_for_test().await.unwrap();
            Task::new(Location::caller(), async { Ok(()) })
                .spawn(&tx)
                .expect("child completion releases total live capacity");
        });
    }
}
