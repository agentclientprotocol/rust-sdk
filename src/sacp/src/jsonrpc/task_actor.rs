use std::panic::Location;

use futures::{
    FutureExt, StreamExt,
    channel::mpsc,
    future::BoxFuture,
    stream::{FusedStream, FuturesUnordered},
};

use crate::JrConnectionCx;

pub(crate) struct Task {
    future: BoxFuture<'static, Result<(), crate::Error>>,
}

impl Task {
    pub fn new(
        location: &'static Location<'static>,
        task_future: impl Future<Output = Result<(), crate::Error>> + Send + 'static,
    ) -> Self {
        Task { future:
        futures::FutureExt::map(task_future, |result| match result {
            Ok(()) => Ok(()),
            Err(err) => {
                let data = err.data.clone();
                Err(err.with_data(serde_json::json! {
                    {
                        "spawned_at": format!("{}:{}:{}", location.file(), location.line(), location.column()),
                        "data": data,
                    }
                }))
            }
        }).boxed()
        }
    }

    /// Return a new task that executes with the given name
    fn named(self, name: Option<String>) -> Task {
        if let Some(name) = name {
            Task {
                future: crate::util::instrumented_with_connection_name(name, self.future).boxed(),
            }
        } else {
            self
        }
    }
}

/// The "task actor" manages other tasks
pub(super) async fn task_actor(
    mut task_rx: mpsc::UnboundedReceiver<Task>,
) -> Result<(), crate::Error> {
    let mut futures = FuturesUnordered::new();

    loop {
        // If we have no futures to run, wait until we do.
        if futures.is_empty() {
            match task_rx.next().await {
                Some(task) => futures.push(task.future),
                None => return Ok(()),
            }
            continue;
        }

        // If there are no more tasks coming in, just drain our queue and return.
        if task_rx.is_terminated() {
            while let Some(result) = futures.next().await {
                result?;
            }
            return Ok(());
        }

        // Otherwise, run futures until we get a request for a new task.
        futures::select! {
            result = futures.next() => if let Some(result) = result {
                result?;
            },

            task = task_rx.next() => {
                if let Some(task) = task {
                    futures.push(task.future);
                }
            }
        }
    }
}

pub(crate) struct PendingTask {
    task_fn: Box<dyn PendingTaskFn>,
}

impl PendingTask {
    pub fn new<Fut>(
        location: &'static Location<'static>,
        task_function: impl FnOnce(JrConnectionCx) -> Fut + Send + 'static,
    ) -> Self
    where
        Fut: Future<Output = Result<(), crate::Error>> + Send + 'static,
    {
        PendingTask {
            task_fn: Box::new(move |cx| Task::new(location, task_function(cx))),
        }
    }

    /// Return a new pending task that will execute with the given name
    pub fn named(self, name: Option<String>) -> Self {
        PendingTask {
            task_fn: Box::new(move |cx| self.into_task(cx).named(name)),
        }
    }

    pub fn into_task(self, cx: JrConnectionCx) -> Task {
        self.task_fn.into_task(cx)
    }
}

trait PendingTaskFn: 'static + Send {
    fn into_task(self: Box<Self>, cx: JrConnectionCx) -> Task;
}

impl<F> PendingTaskFn for F
where
    F: FnOnce(JrConnectionCx) -> Task + 'static + Send,
{
    fn into_task(self: Box<Self>, cx: JrConnectionCx) -> Task {
        (*self)(cx)
    }
}
