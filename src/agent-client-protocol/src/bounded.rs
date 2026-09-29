//! Opt-in admission-bounded frame channels. Legacy [`crate::Channel`] is unchanged.
use crate::{Channel, ConnectTo, Error, Role, TransportFrame};
use futures::{
    FutureExt, Stream, StreamExt,
    channel::{mpsc, oneshot},
    future::{BoxFuture, Shared},
};
use std::{
    io::Write,
    pin::Pin,
    sync::{Arc, Mutex},
    task::{Context, Poll},
};

/// Finite per-direction admission limits. Byte limits measure encoded JSON,
/// not allocator overhead or application-owned values/future captures.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ChannelLimits {
    /// Maximum encoded size of one frame.
    pub max_frame_bytes: usize,
    /// Maximum reserved encoded bytes, including queued and handed-off frames.
    pub max_buffered_bytes: usize,
    /// Maximum queued, preparing, or handed-off frames.
    pub max_buffered_frames: usize,
    /// Maximum requests awaiting a reply.
    pub max_pending_requests: usize,
    /// Maximum queued/running tasks and registered dynamic handlers (including
    /// the transport driver).
    pub max_tasks: usize,
}
impl Default for ChannelLimits {
    fn default() -> Self {
        Self {
            max_frame_bytes: 1024 * 1024,
            max_buffered_bytes: 16 * 1024 * 1024,
            max_buffered_frames: 256,
            max_pending_requests: 256,
            max_tasks: 256,
        }
    }
}
impl ChannelLimits {
    /// Reject zero limits and a byte budget smaller than one maximum frame.
    pub fn validate(self) -> Result<Self, Error> {
        if self.max_frame_bytes == 0
            || self.max_buffered_bytes < self.max_frame_bytes
            || self.max_buffered_frames == 0
            || self.max_pending_requests == 0
            || self.max_tasks == 0
        {
            return Err(limit_error("invalid channel limits"));
        }
        Ok(self)
    }
}
fn limit_error(message: &str) -> Error {
    crate::util::internal_error(message)
}
type Failure = Shared<BoxFuture<'static, Error>>;
struct Terminal {
    error: Mutex<Option<Error>>,
    signal: Mutex<Option<oneshot::Sender<Error>>>,
    failure: Failure,
}
impl Terminal {
    fn new() -> Arc<Self> {
        let (tx, rx) = oneshot::channel();
        Arc::new(Self {
            error: Mutex::new(None),
            signal: Mutex::new(Some(tx)),
            failure: rx
                .map(|r| r.unwrap_or_else(|_| limit_error("bounded channel closed")))
                .boxed()
                .shared(),
        })
    }
    fn fail(&self, message: &str) -> Error {
        let mut error = self.error.lock().expect("terminal mutex poisoned");
        let error = error.get_or_insert_with(|| limit_error(message)).clone();
        if let Some(tx) = self.signal.lock().expect("terminal signal poisoned").take() {
            drop(tx.send(error.clone()));
        }
        error
    }
    fn check(&self) -> Result<(), Error> {
        match &*self.error.lock().expect("terminal mutex poisoned") {
            Some(e) => Err(e.clone()),
            None => Ok(()),
        }
    }
}
#[derive(Default)]
struct Usage {
    frames: usize,
    bytes: usize,
    tasks: usize,
    admission_closed: bool,
}
#[derive(Clone)]
pub(crate) struct Budget {
    limits: ChannelLimits,
    usage: Arc<Mutex<Usage>>,
    terminal: Arc<Terminal>,
}
impl std::fmt::Debug for Budget {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Budget")
            .field("limits", &self.limits)
            .finish_non_exhaustive()
    }
}
impl Budget {
    pub(crate) fn limits(&self) -> ChannelLimits {
        self.limits
    }
    pub(crate) fn fail(&self, message: &str) -> Error {
        self.terminal.fail(message)
    }
    pub(crate) fn check(&self) -> Result<(), Error> {
        self.terminal.check()
    }
    pub(crate) fn check_admission(&self) -> Result<(), Error> {
        self.check()?;
        if self
            .usage
            .lock()
            .expect("budget mutex poisoned")
            .admission_closed
        {
            return Err(limit_error("bounded channel admission closed"));
        }
        Ok(())
    }
    fn close_admission(&self) {
        self.usage
            .lock()
            .expect("budget mutex poisoned")
            .admission_closed = true;
    }
    pub(crate) fn failure(&self) -> BoxFuture<'static, Error> {
        self.terminal.failure.clone().boxed()
    }
    pub(crate) fn reserve(&self) -> Result<Charge, Error> {
        self.check()?;
        let mut used = self.usage.lock().expect("budget mutex poisoned");
        if used.admission_closed {
            return Err(limit_error("bounded channel admission closed"));
        }
        let bytes = self.limits.max_frame_bytes;
        if used.frames >= self.limits.max_buffered_frames
            || bytes > self.limits.max_buffered_bytes - used.bytes
        {
            return Err(self.fail("bounded channel frame/byte admission exhausted"));
        }
        used.frames += 1;
        used.bytes += bytes;
        Ok(Charge(Arc::new(Permit {
            budget: self.clone(),
            bytes,
            task: false,
        })))
    }
    pub(crate) fn task(&self) -> Result<Charge, Error> {
        self.check()?;
        let mut used = self.usage.lock().expect("budget mutex poisoned");
        if used.admission_closed {
            return Err(limit_error("bounded channel admission closed"));
        }
        if used.tasks >= self.limits.max_tasks {
            return Err(self.fail("bounded channel task admission exhausted"));
        }
        used.tasks += 1;
        Ok(Charge(Arc::new(Permit {
            budget: self.clone(),
            bytes: 0,
            task: true,
        })))
    }
}
struct Permit {
    budget: Budget,
    bytes: usize,
    task: bool,
}
impl Drop for Permit {
    fn drop(&mut self) {
        let mut used = self.budget.usage.lock().expect("budget mutex poisoned");
        if self.task {
            used.tasks -= 1;
        } else {
            used.frames -= 1;
            used.bytes -= self.bytes;
        }
    }
}
#[derive(Clone)]
pub(crate) struct Charge(Arc<Permit>);
impl std::fmt::Debug for Charge {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Charge").finish_non_exhaustive()
    }
}

/// Serialized frame with admission ownership. Receiving or forwarding it does
/// not release its reservation. Retain it until body handoff/drop, which is NOT
/// an acknowledgment of socket flush, peer parsing, or protocol dispatch.
#[derive(Debug)]
pub struct ChargedFrame {
    bytes: Box<[u8]>,
    pub(crate) charges: Vec<Charge>,
}
impl ChargedFrame {
    /// Borrow the charged JSON bytes without releasing admission.
    #[must_use]
    pub fn as_bytes(&self) -> &[u8] {
        &self.bytes
    }
    /// Decode while retaining admission. Decoded JSON has additional structural
    /// overhead; consumers must not accumulate uncharged decoded copies.
    #[must_use]
    pub fn decode(&self) -> TransportFrame {
        TransportFrame::parse_json(
            std::str::from_utf8(&self.bytes).expect("serialized frame is UTF-8"),
        )
    }
}

/// Cloneable fail-fast producer. There is no waiting-producer queue.
#[derive(Clone)]
pub struct BoundedSender {
    tx: mpsc::UnboundedSender<ChargedFrame>,
    pub(crate) budget: Budget,
}
impl std::fmt::Debug for BoundedSender {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BoundedSender")
            .field("budget", &self.budget)
            .finish_non_exhaustive()
    }
}
impl BoundedSender {
    /// Validated per-direction limits.
    #[must_use]
    pub fn limits(&self) -> ChannelLimits {
        self.budget.limits
    }
    /// Resolve when any admission/serialization failure makes this channel terminal.
    #[must_use]
    pub fn failure(&self) -> BoxFuture<'static, Error> {
        self.budget.failure()
    }
    /// Fail all escaped producers and wake the connection driver.
    #[allow(
        clippy::must_use_candidate,
        reason = "The primary effect is terminating the channel; callers may optionally propagate the error."
    )]
    pub fn fail(&self, reason: &str) -> Error {
        self.budget.fail(reason)
    }
    /// Gracefully close this direction for every sender clone.
    ///
    /// New sends fail without making the channel terminal. Already enqueued
    /// frames retain their charges and remain available to the receiver, which
    /// observes EOF after draining them. This does not signal [`Self::failure`]
    /// or close the opposite direction.
    pub fn close_channel(&self) {
        self.budget.close_admission();
        self.tx.close_channel();
    }
    /// Admit and serialize without allocating an unbounded intermediate String.
    pub fn try_send(&self, frame: TransportFrame) -> Result<(), Error> {
        let charge = self.budget.reserve()?;
        self.send_charged(frame, vec![charge])
    }
    /// Copy already serialized wire text after admission and size validation.
    /// Like TransportFrame::parse_json, malformed wire text is preserved.
    pub fn try_send_serialized(&self, json: &str) -> Result<(), Error> {
        let charge = self.budget.reserve()?;
        if json.len() > self.limits().max_frame_bytes {
            return Err(self.fail("bounded channel frame too large"));
        }
        self.enqueue(ChargedFrame {
            bytes: json.as_bytes().into(),
            charges: vec![charge],
        })
    }
    /// Forward admission ownership without releasing it at an internal handoff.
    /// Cross-budget forwarding acquires destination admission before releasing
    /// the original reservation. Same-budget forwarding does not double charge.
    pub fn try_forward(&self, mut frame: ChargedFrame) -> Result<(), Error> {
        self.budget.check_admission()?;
        if frame.bytes.len() > self.limits().max_frame_bytes {
            return Err(self.fail("bounded channel forwarded frame too large"));
        }
        if !frame
            .charges
            .iter()
            .any(|c| Arc::ptr_eq(&c.0.budget.usage, &self.budget.usage))
        {
            let charge = self.budget.reserve()?;
            frame.charges = vec![charge];
        }
        self.enqueue(frame)
    }
    fn enqueue(&self, frame: ChargedFrame) -> Result<(), Error> {
        self.budget.check_admission()?;
        // Serialize closure and enqueue so a racing close cannot turn a normal
        // rejected send into terminal failure and discard admitted frames.
        let result = {
            let used = self.budget.usage.lock().expect("budget mutex poisoned");
            if used.admission_closed {
                return Err(limit_error("bounded channel admission closed"));
            }
            self.tx.unbounded_send(frame)
        };
        result.map_err(|_| self.fail("bounded channel receiver closed"))
    }
    pub(crate) fn send_charged(
        &self,
        frame: TransportFrame,
        charges: Vec<Charge>,
    ) -> Result<(), Error> {
        let mut charges = charges;
        if !charges
            .iter()
            .any(|c| Arc::ptr_eq(&c.0.budget.usage, &self.budget.usage))
        {
            charges.push(self.budget.reserve()?);
        }
        self.budget.check_admission()?;
        let bytes = serialize_frame(&frame, self.limits().max_frame_bytes)
            .map_err(|_| self.fail("bounded channel frame serialization exceeded limit"))?;
        self.enqueue(ChargedFrame { bytes, charges })
    }
}
/// Single consumer of serialized, charged frames.
#[derive(Debug)]
pub struct BoundedReceiver {
    rx: mpsc::UnboundedReceiver<ChargedFrame>,
    budget: Budget,
    failure: Failure,
}
impl Stream for BoundedReceiver {
    type Item = ChargedFrame;
    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        if Pin::new(&mut self.failure).poll(cx).is_ready() {
            self.rx.close();
            while let Poll::Ready(Some(_)) = Pin::new(&mut self.rx).poll_next(cx) {}
            return Poll::Ready(None);
        }
        Pin::new(&mut self.rx).poll_next(cx)
    }
}
/// Opt-in bounded frame endpoint; no concrete unbounded sender is exposed.
#[derive(Debug)]
pub struct BoundedChannel {
    /// Fail-fast producer for frames sent to the peer.
    pub tx: BoundedSender,
    /// Charged frames received from the peer.
    pub rx: BoundedReceiver,
}
impl BoundedChannel {
    /// Construct two endpoints with independent directional budgets and shared
    /// terminal state. Limits must be nonzero and fit one maximum-sized frame.
    pub fn duplex(limits: ChannelLimits) -> Result<(Self, Self), Error> {
        let limits = limits.validate()?;
        let terminal = Terminal::new();
        let a = Budget {
            limits,
            usage: Arc::default(),
            terminal: terminal.clone(),
        };
        let b = Budget {
            limits,
            usage: Arc::default(),
            terminal,
        };
        let (atx, brx) = mpsc::unbounded();
        let (btx, arx) = mpsc::unbounded();
        Ok((
            Self {
                tx: BoundedSender {
                    tx: atx,
                    budget: a.clone(),
                },
                rx: BoundedReceiver {
                    rx: arx,
                    failure: b.terminal.failure.clone(),
                    budget: b.clone(),
                },
            },
            Self {
                tx: BoundedSender { tx: btx, budget: b },
                rx: BoundedReceiver {
                    rx: brx,
                    failure: a.terminal.failure.clone(),
                    budget: a,
                },
            },
        ))
    }
    /// Validated per-direction limits.
    #[must_use]
    pub fn limits(&self) -> ChannelLimits {
        self.tx.limits()
    }
}
/// Additive transport boundary. The legacy variant has no admission guarantee.
#[derive(Debug)]
pub enum TransportChannel {
    /// Compatibility-only, unbounded endpoint.
    Legacy(Channel),
    /// Opt-in admission-bounded endpoint.
    Bounded(BoundedChannel),
}
impl<R: Role> ConnectTo<R> for BoundedChannel {
    async fn connect_to(self, client: impl ConnectTo<R::Counterpart>) -> Result<(), Error> {
        async fn copy(mut rx: BoundedReceiver, tx: BoundedSender) -> Result<(), Error> {
            while let Some(frame) = rx.next().await {
                tx.try_forward(frame)?;
            }
            rx.budget.check()
        }
        let (other, future) = client.into_bounded_channel_and_future(self.limits())?;
        futures::try_join!(copy(self.rx, other.tx), copy(other.rx, self.tx), future)?;
        Ok(())
    }
    fn into_channel_and_future(self) -> (Channel, BoxFuture<'static, Result<(), Error>>) {
        drop(self);
        let (channel, peer) = Channel::duplex();
        drop(peer);
        (
            channel,
            async {
                Err(limit_error(
                    "bounded channel cannot be extracted as legacy Channel",
                ))
            }
            .boxed(),
        )
    }
    fn into_transport_and_future(
        self,
    ) -> (TransportChannel, BoxFuture<'static, Result<(), Error>>) {
        (TransportChannel::Bounded(self), async { Ok(()) }.boxed())
    }
    fn into_bounded_channel_and_future(
        self,
        limits: ChannelLimits,
    ) -> Result<(BoundedChannel, BoxFuture<'static, Result<(), Error>>), Error> {
        let limits = limits.validate()?;
        if self.limits() != limits {
            return Err(self
                .tx
                .fail("bounded endpoint limits differ from requested limits"));
        }
        Ok((self, async { Ok(()) }.boxed()))
    }
}

pub(crate) struct CappedWriter {
    bytes: Vec<u8>,
    limit: usize,
}
impl CappedWriter {
    pub(crate) fn new(limit: usize) -> Self {
        Self {
            bytes: Vec::new(),
            limit,
        }
    }
    pub(crate) fn finish(self) -> Box<[u8]> {
        self.bytes.into_boxed_slice()
    }
}
impl Write for CappedWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        if buf.len() > self.limit - self.bytes.len() {
            return Err(std::io::Error::other("encoded JSON limit exceeded"));
        }
        self.bytes
            .try_reserve_exact(buf.len())
            .map_err(std::io::Error::other)?;
        self.bytes.extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}
pub(crate) fn serialize<T: serde::Serialize + ?Sized>(
    value: &T,
    limit: usize,
) -> Result<Box<[u8]>, Error> {
    let mut out = CappedWriter::new(limit);
    serde_json::to_writer(&mut out, value)?;
    Ok(out.finish())
}
fn serialize_frame(frame: &TransportFrame, limit: usize) -> Result<Box<[u8]>, Error> {
    match frame {
        TransportFrame::Single(value) => serialize(value, limit),
        TransportFrame::Batch(value) => serialize(value, limit),
        TransportFrame::Malformed { raw, .. } => {
            if raw.len() > limit {
                return Err(limit_error("frame too large"));
            }
            Ok(raw.as_bytes().into())
        }
    }
}

#[derive(Debug)]
pub(crate) struct IncomingFrame {
    pub(crate) frame: TransportFrame,
    pub(crate) charges: Vec<Charge>,
}
impl From<TransportFrame> for IncomingFrame {
    fn from(frame: TransportFrame) -> Self {
        Self {
            frame,
            charges: vec![],
        }
    }
}
impl From<ChargedFrame> for IncomingFrame {
    fn from(frame: ChargedFrame) -> Self {
        Self {
            frame: frame.decode(),
            charges: frame.charges,
        }
    }
}
pub(crate) enum TransportSender {
    Legacy(mpsc::UnboundedSender<TransportFrame>),
    Bounded(BoundedSender),
}
impl From<mpsc::UnboundedSender<TransportFrame>> for TransportSender {
    fn from(tx: mpsc::UnboundedSender<TransportFrame>) -> Self {
        Self::Legacy(tx)
    }
}
impl TransportSender {
    pub(crate) fn send(&self, frame: TransportFrame, charges: Vec<Charge>) -> Result<(), Error> {
        match self {
            Self::Legacy(tx) => tx
                .unbounded_send(frame)
                .map_err(crate::util::internal_error),
            Self::Bounded(tx) => tx.send_charged(frame, charges),
        }
    }
}
impl TransportChannel {
    pub(crate) fn split(
        self,
    ) -> (
        futures::stream::BoxStream<'static, IncomingFrame>,
        TransportSender,
        Option<Budget>,
    ) {
        match self {
            Self::Legacy(channel) => (
                channel.rx.map(IncomingFrame::from).boxed(),
                TransportSender::Legacy(channel.tx),
                None,
            ),
            Self::Bounded(channel) => {
                let budget = channel.tx.budget.clone();
                (
                    channel.rx.map(IncomingFrame::from).boxed(),
                    TransportSender::Bounded(channel.tx),
                    Some(budget),
                )
            }
        }
    }
}

pub(crate) struct DriverLifetime {
    budget: Budget,
    graceful: bool,
}
impl DriverLifetime {
    pub(crate) fn new(budget: Budget) -> Self {
        Self {
            budget,
            graceful: false,
        }
    }
    pub(crate) fn finish_gracefully(&mut self) {
        self.budget.close_admission();
        self.graceful = true;
    }
}
impl Drop for DriverLifetime {
    fn drop(&mut self) {
        if !self.graceful {
            self.budget.fail("bounded protocol driver stopped");
        }
    }
}

#[cfg(test)]
impl Budget {
    pub(crate) fn snapshot(&self) -> (usize, usize, usize) {
        let used = self.usage.lock().unwrap();
        (used.frames, used.bytes, used.tasks)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn limits() -> ChannelLimits {
        ChannelLimits {
            max_frame_bytes: 64,
            max_buffered_bytes: 128,
            max_buffered_frames: 2,
            max_pending_requests: 2,
            max_tasks: 2,
        }
    }

    #[test]
    fn byte_and_frame_admission_persist_after_dequeue_and_forward() {
        let (a, mut b) = BoundedChannel::duplex(limits()).unwrap();
        a.tx.try_send_serialized("{}").unwrap();
        let frame = b.rx.next().now_or_never().unwrap().unwrap();
        assert_eq!(a.tx.budget.snapshot(), (1, 64, 0));
        a.tx.try_forward(frame).unwrap();
        assert_eq!(a.tx.budget.snapshot(), (1, 64, 0));
        let frame = b.rx.next().now_or_never().unwrap().unwrap();
        drop(frame);
        assert_eq!(a.tx.budget.snapshot(), (0, 0, 0));
        a.tx.try_send_serialized(&"x".repeat(64)).unwrap();
        assert!(a.tx.try_send_serialized(&"x".repeat(65)).is_err());
        assert!(a.tx.failure().now_or_never().is_some());
        assert!(b.rx.next().now_or_never().unwrap().is_none());
        assert_eq!(a.tx.budget.snapshot(), (0, 0, 0));
    }

    #[test]
    fn graceful_close_rejects_clones_but_drains_charged_frames_without_failure() {
        let (a, mut b) = BoundedChannel::duplex(limits()).unwrap();
        let clone = a.tx.clone();
        a.tx.try_send_serialized("{}").unwrap();
        a.tx.try_send_serialized("[]").unwrap();
        a.tx.close_channel();
        a.tx.close_channel();
        assert!(clone.try_send_serialized("rejected").is_err());
        assert!(clone.try_send(TransportFrame::parse_json("{}")).is_err());
        assert!(a.tx.failure().now_or_never().is_none());
        assert_eq!(a.tx.budget.snapshot(), (2, 128, 0));
        let first = b.rx.next().now_or_never().unwrap().unwrap();
        let second = b.rx.next().now_or_never().unwrap().unwrap();
        assert_eq!(first.as_bytes(), b"{}");
        assert_eq!(second.as_bytes(), b"[]");
        assert!(b.rx.next().now_or_never().unwrap().is_none());
        assert_eq!(a.tx.budget.snapshot(), (2, 128, 0));
        drop((first, second));
        assert_eq!(a.tx.budget.snapshot(), (0, 0, 0));
        b.tx.try_send_serialized("{}").unwrap();
        assert!(a.tx.failure().now_or_never().is_none());
    }

    #[test]
    fn producer_flood_before_poll_is_terminal_and_drains_charges() {
        let (a, mut b) = BoundedChannel::duplex(limits()).unwrap();
        let escaped = a.tx.clone();
        a.tx.try_send_serialized("{}").unwrap();
        a.tx.try_send_serialized("{}").unwrap();
        assert!(escaped.try_send_serialized("{}").is_err());
        assert_eq!(a.tx.budget.snapshot(), (2, 128, 0));
        assert!(b.rx.next().now_or_never().unwrap().is_none());
        assert_eq!(a.tx.budget.snapshot(), (0, 0, 0));
        assert!(escaped.try_send_serialized("{}").is_err());
    }

    #[test]
    fn cross_budget_forward_reserves_destination_and_releases_source() {
        let (a, mut b) = BoundedChannel::duplex(limits()).unwrap();
        let (c, mut d) = BoundedChannel::duplex(limits()).unwrap();
        a.tx.try_send_serialized("{}").unwrap();
        let frame = b.rx.next().now_or_never().unwrap().unwrap();
        c.tx.try_forward(frame).unwrap();
        assert_eq!(a.tx.budget.snapshot(), (0, 0, 0));
        assert_eq!(c.tx.budget.snapshot(), (1, 64, 0));
        drop(d.rx.next().now_or_never().unwrap().unwrap());
        assert_eq!(c.tx.budget.snapshot(), (0, 0, 0));
    }

    #[test]
    fn dropped_receiver_and_serialization_errors_release_reservations() {
        let (a, b) = BoundedChannel::duplex(limits()).unwrap();
        drop(b);
        assert!(a.tx.try_send_serialized("{}").is_err());
        assert_eq!(a.tx.budget.snapshot(), (0, 0, 0));
        let (a, _b) = BoundedChannel::duplex(limits()).unwrap();
        let frame = TransportFrame::Single(
            crate::RawJsonRpcMessage::notification(
                "large".into(),
                serde_json::json!({"payload": "x".repeat(65)}),
            )
            .unwrap(),
        );
        assert!(a.tx.try_send(frame).is_err());
        assert_eq!(a.tx.budget.snapshot(), (0, 0, 0));
    }

    #[test]
    fn dyn_transport_preserves_bounded_variant_and_legacy_extraction_fails() {
        let (a, _b) = BoundedChannel::duplex(limits()).unwrap();
        let erased = crate::DynConnectTo::<crate::UntypedRole>::new(a);
        let (channel, _) = erased.into_transport_and_future();
        assert!(matches!(channel, TransportChannel::Bounded(_)));
        let (a, _b) = BoundedChannel::duplex(limits()).unwrap();
        let (mut channel, future) =
            <BoundedChannel as ConnectTo<crate::UntypedRole>>::into_channel_and_future(a);
        assert!(future.now_or_never().unwrap().is_err());
        assert!(channel.rx.next().now_or_never().unwrap().is_none());
    }
}
