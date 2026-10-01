//! Finite queues for synchronously invoked dispatcher APIs.
//!
//! Dispatch callbacks cannot await capacity: the receiver may depend on that
//! callback returning. External producers can await `send` instead.
use futures::Stream;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

use super::{FrameAdmission, FramePermit};

pub const QUEUE_CAPACITY: usize = 32;

pub struct Sender<T> {
    inner: Arc<SenderInner<T>>,
}

struct SenderInner<T> {
    tx: async_channel::Sender<T>,
    urgent_tx: Option<async_channel::Sender<T>>,
    admission: Option<Admission<T>>,
    capacity: usize,
}

struct Admission<T> {
    budget: FrameAdmission,
    measure: fn(&T) -> Result<usize, crate::Error>,
    attach: fn(T, FramePermit) -> T,
    control: fn(&T) -> bool,
    urgent: fn(&T) -> bool,
}

impl<T> Clone for Sender<T> {
    fn clone(&self) -> Self {
        Self {
            inner: self.inner.clone(),
        }
    }
}

impl<T> std::fmt::Debug for Sender<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AdmissionSender").finish_non_exhaustive()
    }
}

impl<T> Sender<T> {
    pub fn byte_admission(&self) -> Option<FrameAdmission> {
        self.inner
            .admission
            .as_ref()
            .map(|admission| admission.budget.clone())
    }

    pub fn queue_capacity(&self) -> usize {
        self.inner.capacity
    }

    pub fn unbounded_send(&self, item: T) -> Result<(), SendError<T>> {
        // A readiness-blocked consumer polls only the urgent lane, regardless
        // of ordinary queue occupancy. The outgoing actor settles cancellation
        // locally if its request has not yet been published.
        let urgent = self
            .inner
            .admission
            .as_ref()
            .is_some_and(|admission| (admission.urgent)(&item));
        let item = if let Some(admission) = &self.inner.admission {
            let bytes = (admission.measure)(&item).map_err(|error| SendError {
                item: None,
                reason: error.to_string(),
            });
            // Preserve ownership of the rejected message even when sizing fails.
            let bytes = match bytes {
                Ok(bytes) => bytes,
                Err(error) => {
                    return Err(SendError {
                        item: Some(item),
                        ..error
                    });
                }
            };
            let permit = admission
                .budget
                .try_reserve_bytes(bytes, !(admission.control)(&item))
                .ok_or_else(|| SendError {
                    item: None,
                    reason: "outgoing application byte capacity exceeded".into(),
                });
            let permit = match permit {
                Ok(permit) => permit,
                Err(error) => {
                    return Err(SendError {
                        item: Some(item),
                        ..error
                    });
                }
            };
            (admission.attach)(item, permit)
        } else {
            item
        };
        let tx = if urgent {
            self.inner.urgent_tx.as_ref().expect("urgent lane exists")
        } else {
            &self.inner.tx
        };
        tx.try_send(item).map_err(|error| SendError {
            item: Some(error.into_inner()),
            reason: "outgoing application queue full or closed".into(),
        })
    }

    pub async fn send(&self, item: T) -> Result<(), crate::Error> {
        self.send_inner(item, true).await
    }

    /// Await a queue slot, but reject byte pressure immediately. Partial batch
    /// replies retain their bytes until every sibling arrives, so waiting for
    /// those bytes could prevent the only frames that can release them.
    #[cfg(feature = "unstable_mcp_over_acp")]
    pub async fn send_with_immediate_byte_admission(&self, item: T) -> Result<(), crate::Error> {
        self.send_inner(item, false).await
    }

    async fn send_inner(&self, item: T, wait_for_bytes: bool) -> Result<(), crate::Error> {
        let urgent = self
            .inner
            .admission
            .as_ref()
            .is_some_and(|admission| (admission.urgent)(&item));
        let tx = if urgent {
            self.inner.urgent_tx.as_ref().expect("urgent lane exists")
        } else {
            &self.inner.tx
        };
        let item = if let Some(admission) = &self.inner.admission {
            let bytes = (admission.measure)(&item)?;
            let data = !(admission.control)(&item);
            let permit = if wait_for_bytes {
                let reserve = admission.budget.reserve_bytes(bytes, data);
                match futures::future::select(Box::pin(reserve), Box::pin(tx.closed())).await {
                    futures::future::Either::Left((permit, _)) => permit?,
                    futures::future::Either::Right(_) => {
                        return Err(crate::util::internal_error(
                            "outgoing application queue closed",
                        ));
                    }
                }
            } else {
                admission
                    .budget
                    .try_reserve_bytes(bytes, data)
                    .ok_or_else(|| {
                        crate::util::internal_error("outgoing application byte capacity exceeded")
                    })?
            };
            (admission.attach)(item, permit)
        } else {
            item
        };
        tx.send(item).await.map_err(crate::util::internal_error)
    }
}

#[derive(Debug)]
pub struct SendError<T> {
    item: Option<T>,
    reason: String,
}

impl<T> SendError<T> {
    pub fn into_inner(self) -> T {
        self.item.expect("send errors retain their rejected item")
    }
}

impl<T> std::fmt::Display for SendError<T> {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.reason)
    }
}

impl<T: std::fmt::Debug> std::error::Error for SendError<T> {}

#[cfg(test)]
pub fn channel<T>() -> (Sender<T>, SimpleReceiver<T>) {
    channel_with_capacity(QUEUE_CAPACITY)
}

pub fn channel_with_capacity<T>(capacity: usize) -> (Sender<T>, SimpleReceiver<T>) {
    let capacity = capacity.max(1);
    let (tx, rx) = async_channel::bounded(capacity);
    (
        Sender {
            inner: Arc::new(SenderInner {
                tx,
                urgent_tx: None,
                admission: None,
                capacity,
            }),
        },
        SimpleReceiver(Box::pin(rx)),
    )
}

pub struct SimpleReceiver<T>(Pin<Box<async_channel::Receiver<T>>>);

impl<T> Stream for SimpleReceiver<T> {
    type Item = T;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<T>> {
        self.0.as_mut().poll_next(cx)
    }
}

pub(super) trait ReceiverClose: Stream {
    fn close(&mut self);
    fn poll_urgent(&mut self, _cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        Poll::Pending
    }
}

impl<T> ReceiverClose for SimpleReceiver<T> {
    fn close(&mut self) {
        self.0.close();
    }
}

pub struct Receiver<T> {
    normal: SimpleReceiver<T>,
    urgent: SimpleReceiver<T>,
}

impl<T> Stream for Receiver<T> {
    type Item = T;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<T>> {
        let this = self.get_mut();
        let urgent_closed = match Pin::new(&mut this.urgent).poll_next(cx) {
            Poll::Ready(Some(item)) => return Poll::Ready(Some(item)),
            Poll::Ready(None) => true,
            Poll::Pending => false,
        };
        match Pin::new(&mut this.normal).poll_next(cx) {
            Poll::Ready(Some(item)) => Poll::Ready(Some(item)),
            Poll::Ready(None) if urgent_closed => Poll::Ready(None),
            _ => Poll::Pending,
        }
    }
}

impl<T> ReceiverClose for Receiver<T> {
    fn close(&mut self) {
        self.normal.close();
        self.urgent.close();
    }

    fn poll_urgent(&mut self, cx: &mut Context<'_>) -> Poll<Option<T>> {
        match Pin::new(&mut self.urgent).poll_next(cx) {
            Poll::Ready(None) => Poll::Pending,
            result => result,
        }
    }
}

pub fn budgeted_channel<T>(
    admission: FrameAdmission,
    measure: fn(&T) -> Result<usize, crate::Error>,
    attach: fn(T, FramePermit) -> T,
    control: fn(&T) -> bool,
    urgent: fn(&T) -> bool,
) -> (Sender<T>, Receiver<T>) {
    let capacity = admission.limits().max_queued_frames.max(1);
    let (tx, rx) = async_channel::bounded(capacity);
    let (urgent_tx, urgent_rx) = async_channel::bounded(capacity);
    (
        Sender {
            inner: Arc::new(SenderInner {
                tx,
                urgent_tx: Some(urgent_tx),
                admission: Some(Admission {
                    budget: admission,
                    measure,
                    attach,
                    control,
                    urgent,
                }),
                capacity,
            }),
        },
        Receiver {
            normal: SimpleReceiver(Box::pin(rx)),
            urgent: SimpleReceiver(Box::pin(urgent_rx)),
        },
    )
}
