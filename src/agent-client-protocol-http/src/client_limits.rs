//! Separate opt-in HTTP path: never converts charged frames into legacy channels.
use super::*;
use agent_client_protocol::{BoundedChannel, ChannelLimits, ChargedFrame, TransportChannel};
use std::task::Poll;

/// Finite per-client limits. Every value must be nonzero.
///
/// Byte accounting measures serialized wire bytes, not allocator overhead or
/// reqwest/TLS/socket buffers. The HTTP budget covers retained POST bodies and
/// pending metadata, capped response workspaces, and fixed SSE parser workspaces.
/// Core channel budgets apply separately in each direction, including producer
/// admission. Exhaustion is terminal and never waits for capacity. A full POST
/// lane fails rather than parking on the shared outgoing FIFO. Response-only
/// callback frames have an independent lane; mixed batches retain request order.
#[derive(Clone, Debug)]
pub struct HttpClientLimits {
    /// Independent bounded core limits in each channel direction.
    pub channel: ChannelLimits,
    /// HTTP wire-byte reservations, including conservative metadata allowances.
    pub max_buffered_bytes: usize,
    /// Simultaneous HTTP reservations. Bodies, parser workspaces and metadata
    /// each consume a slot; this is conservative relative to actual wire frames.
    pub max_buffered_frames: usize,
    /// Individual RPC entries awaiting a reply, including duplicate/null IDs.
    pub max_pending_requests: usize,
    /// Queued plus active request/mixed/notification POSTs (one active at a time).
    pub max_request_posts: usize,
    /// Independent queued plus active response-only POSTs (one active at a time).
    pub max_response_posts: usize,
    /// Includes the connection stream and establishing session streams.
    pub max_sse_streams: usize,
    /// Collected response bytes, for initialization and success/error POSTs.
    pub max_response_bytes: usize,
    /// Raw bytes in an incomplete SSE line, including comment/ignored lines.
    pub max_sse_line_bytes: usize,
    /// Includes comments, ignored fields, and delimiters, not only data fields.
    /// CRLF is normalized to one delimiter byte for this limit.
    pub max_sse_event_bytes: usize,
    /// Largest accepted reqwest SSE chunk before SDK copying or parsing.
    pub max_sse_chunk_bytes: usize,
}

impl Default for HttpClientLimits {
    fn default() -> Self {
        Self {
            channel: ChannelLimits::default(),
            max_buffered_bytes: 64 * 1024 * 1024,
            max_buffered_frames: 128,
            max_pending_requests: 128,
            max_request_posts: 32,
            max_response_posts: 32,
            max_sse_streams: 8,
            max_response_bytes: 1024 * 1024,
            max_sse_line_bytes: 1024 * 1024,
            max_sse_event_bytes: 1024 * 1024,
            max_sse_chunk_bytes: 1024 * 1024,
        }
    }
}

fn failure(message: impl Into<String>) -> AcpError {
    AcpError::internal_error().data(format!("bounded HTTP: {}", message.into()))
}

impl HttpClientLimits {
    fn workspace(&self) -> Result<usize, AcpError> {
        self.max_sse_line_bytes
            .checked_add(self.max_sse_event_bytes)
            .and_then(|n| n.checked_add(self.max_sse_chunk_bytes))
            .ok_or_else(|| failure("SSE workspace arithmetic overflow"))
    }

    fn validate(&self) -> Result<(), AcpError> {
        if [
            self.max_buffered_bytes,
            self.max_buffered_frames,
            self.max_pending_requests,
            self.max_request_posts,
            self.max_response_posts,
            self.max_sse_streams,
            self.max_response_bytes,
            self.max_sse_line_bytes,
            self.max_sse_event_bytes,
            self.max_sse_chunk_bytes,
        ]
        .contains(&0)
        {
            return Err(failure("limits must be nonzero"));
        }
        if self.workspace()? > self.max_buffered_bytes
            || self.max_response_bytes > self.max_buffered_bytes
        {
            return Err(failure("workspace exceeds HTTP byte budget"));
        }
        Ok(())
    }
}

/// An HTTP-only transport with bounded admission. Construct with
/// [`HttpClient::with_limits`]; legacy channel extraction fails closed.
///
/// There are no background SSE tasks, observer mailboxes, or hidden unbounded
/// bridges. The transport future directly polls all streams and POSTs. On exit,
/// it awaits one best-effort HTTP DELETE, bounded to five seconds, for an admitted
/// connection ID. Dropping it releases local streams and POSTs and, if teardown
/// has not started, schedules that DELETE when a Tokio runtime is available.
/// Cancellation can interrupt DELETE and never guarantees peer cleanup. No
/// uncertain or accepted POST is ever retried.
pub struct BoundedHttpClient {
    client: HttpClient,
    limits: HttpClientLimits,
    caller: BoundedChannel,
    transport: BoundedChannel,
}

impl std::fmt::Debug for BoundedHttpClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BoundedHttpClient")
            .field("client", &self.client)
            .field("limits", &self.limits)
            .finish_non_exhaustive()
    }
}

impl BoundedHttpClient {
    pub(super) fn new(client: HttpClient, limits: HttpClientLimits) -> Result<Self, AcpError> {
        limits.validate()?;
        if !matches!(client.endpoint.scheme(), "http" | "https") {
            return Err(failure("bounded client supports only http/https"));
        }
        let (caller, transport) = BoundedChannel::duplex(limits.channel)?;
        Ok(Self {
            client,
            limits,
            caller,
            transport,
        })
    }

    /// Extract the charged channel without a legacy adapter. Poll the returned
    /// future to drive HTTP; dropping it cancels streams and POSTs. Connection
    /// cleanup is best effort, as described on [`BoundedHttpClient`].
    #[must_use]
    pub fn into_bounded_channel_and_future(
        self,
    ) -> (BoundedChannel, BoxFuture<'static, Result<(), AcpError>>) {
        (
            self.caller,
            run_bounded(self.client, self.limits, self.transport).boxed(),
        )
    }
}

impl ConnectTo<Client> for BoundedHttpClient {
    async fn connect_to(self, client: impl ConnectTo<Agent>) -> Result<(), AcpError> {
        let (channel, transport) = self.into_bounded_channel_and_future();
        let shutdown = channel.tx.clone();
        let application = client.connect_to(channel).boxed();
        match futures::future::select(application, transport).await {
            futures::future::Either::Left((result, transport)) => {
                result?;
                shutdown.close_channel();
                transport.await
            }
            futures::future::Either::Right((result, application)) => {
                result?;
                // A final admitted initialization rejection must reach the
                // application before its closed inbound queue is discarded.
                application.await
            }
        }
    }

    fn into_channel_and_future(self) -> (Channel, BoxFuture<'static, Result<(), AcpError>>) {
        let (caller, other) = Channel::duplex();
        drop(other);
        drop(self);
        (
            caller,
            async {
                Err(failure(
                    "legacy channel extraction is disabled; use bounded transport extraction",
                ))
            }
            .boxed(),
        )
    }

    fn into_bounded_channel_and_future(
        self,
        limits: ChannelLimits,
    ) -> Result<(BoundedChannel, BoxFuture<'static, Result<(), AcpError>>), AcpError> {
        let limits = limits.validate()?;
        if self.limits.channel != limits {
            return Err(self
                .caller
                .tx
                .fail("bounded HTTP client limits differ from requested limits"));
        }
        Ok(BoundedHttpClient::into_bounded_channel_and_future(self))
    }

    fn into_transport_and_future(
        self,
    ) -> (TransportChannel, BoxFuture<'static, Result<(), AcpError>>) {
        let (channel, future) = self.into_bounded_channel_and_future();
        (TransportChannel::Bounded(channel), future)
    }
}

#[derive(Clone)]
struct Budget(Arc<StdMutex<Usage>>);
struct Usage {
    bytes: usize,
    frames: usize,
    max_bytes: usize,
    max_frames: usize,
}
struct Lease {
    budget: Budget,
    bytes: usize,
}
impl Budget {
    fn new(limits: &HttpClientLimits) -> Self {
        Self(Arc::new(StdMutex::new(Usage {
            bytes: 0,
            frames: 0,
            max_bytes: limits.max_buffered_bytes,
            max_frames: limits.max_buffered_frames,
        })))
    }
    fn reserve(&self, bytes: usize) -> Result<Arc<Lease>, AcpError> {
        let mut usage = self.0.lock().expect("budget mutex poisoned");
        if bytes > usage.max_bytes.saturating_sub(usage.bytes) || usage.frames >= usage.max_frames {
            return Err(failure("HTTP aggregate byte/frame limit exhausted"));
        }
        usage.bytes += bytes;
        usage.frames += 1;
        Ok(Arc::new(Lease {
            budget: self.clone(),
            bytes,
        }))
    }
}
impl Drop for Lease {
    fn drop(&mut self) {
        let mut usage = self.budget.0.lock().expect("budget mutex poisoned");
        usage.bytes -= self.bytes;
        usage.frames -= 1;
    }
}

// Streaming bodies have no clone/replay implementation, preventing reqwest
// retry policies and 307/308 redirects from resending these POSTs.
fn non_replayable_body(bytes: Vec<u8>) -> reqwest::Body {
    reqwest::Body::wrap_stream(futures::stream::once(async move {
        Ok::<_, std::io::Error>(bytes)
    }))
}

async fn capped_body(mut response: reqwest::Response, cap: usize) -> Result<Vec<u8>, AcpError> {
    if response.content_length().is_some_and(|n| n > cap as u64) {
        return Err(failure("HTTP response body exceeds limit"));
    }
    let mut body = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(|e| failure(e.to_string()))? {
        if chunk.len() > cap.saturating_sub(body.len()) {
            return Err(failure("HTTP response body exceeds limit"));
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

/// Raw bytes are checked before UTF-8 conversion. CR, LF, CRLF, split UTF-8,
/// a leading BOM, ignored fields and comments all share bounded state.
struct Parser {
    line: Vec<u8>,
    data: Vec<u8>,
    event_bytes: usize,
    after_cr: bool,
    first_line: bool,
    has_data: bool,
    max_line: usize,
    max_event: usize,
}
impl Parser {
    fn new(limits: &HttpClientLimits) -> Self {
        Self {
            line: Vec::new(),
            data: Vec::new(),
            event_bytes: 0,
            after_cr: false,
            first_line: true,
            has_data: false,
            max_line: limits.max_sse_line_bytes,
            max_event: limits.max_sse_event_bytes,
        }
    }
    fn byte(&mut self, byte: u8) -> Result<Option<Vec<u8>>, AcpError> {
        if self.after_cr {
            self.after_cr = false;
            if byte == b'\n' {
                return Ok(None);
            }
        }
        if self.event_bytes == self.max_event {
            return Err(failure("SSE event limit exhausted"));
        }
        self.event_bytes += 1;
        if byte != b'\r' && byte != b'\n' {
            if self.line.len() == self.max_line {
                return Err(failure("SSE line limit exhausted"));
            }
            self.line.push(byte);
            return Ok(None);
        }
        self.after_cr = byte == b'\r';
        let line = std::str::from_utf8(&self.line).map_err(|_| failure("invalid SSE UTF-8"))?;
        let line = if self.first_line {
            line.strip_prefix('\u{feff}').unwrap_or(line)
        } else {
            line
        };
        self.first_line = false;
        if line.is_empty() {
            self.line.clear();
            self.event_bytes = 0;
            if self.has_data {
                self.has_data = false;
                self.data.pop(); // final data-field newline
                return Ok(Some(std::mem::take(&mut self.data)));
            }
            return Ok(None);
        }
        let (field, value) = line.split_once(':').unwrap_or((line, ""));
        if field == "data" {
            let value = value.strip_prefix(' ').unwrap_or(value);
            if value.len().saturating_add(1) > self.max_event.saturating_sub(self.data.len()) {
                return Err(failure("SSE data limit exhausted"));
            }
            self.data.extend_from_slice(value.as_bytes());
            self.data.push(b'\n');
            self.has_data = true;
        }
        self.line.clear();
        Ok(None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn parser_limits(line: usize, event: usize) -> HttpClientLimits {
        HttpClientLimits {
            max_sse_line_bytes: line,
            max_sse_event_bytes: event,
            ..Default::default()
        }
    }

    #[test]
    fn parser_handles_split_utf8_bom_and_crlf_without_replay() {
        let mut parser = Parser::new(&parser_limits(64, 128));
        let mut events = Vec::new();
        for byte in "\u{feff}data: hé\r\nid: ignored\r\n\r\ndata: next\n\n".bytes() {
            if let Some(event) = parser.byte(byte).unwrap() {
                events.push(event);
            }
        }
        assert_eq!(events, vec!["hé".as_bytes(), b"next"]);
    }

    #[test]
    fn parser_caps_partial_lines_comments_events_and_utf8() {
        let mut parser = Parser::new(&parser_limits(4, 32));
        for byte in b":abc" {
            parser.byte(*byte).unwrap();
        }
        assert!(parser.byte(b'd').is_err());
        let mut parser = Parser::new(&parser_limits(32, 8));
        for byte in b":a\n:b\n:c" {
            parser.byte(*byte).unwrap();
        }
        assert!(parser.byte(b'\n').is_err());
        let mut parser = Parser::new(&parser_limits(32, 32));
        parser.byte(0xc3).unwrap();
        assert!(parser.byte(b'\n').is_err());
    }

    #[test]
    fn parser_exact_event_boundary_and_many_events() {
        let mut parser = Parser::new(&parser_limits(7, 9));
        for _ in 0..1024 {
            let mut event = None;
            for byte in b"data: a\n\n" {
                event = parser.byte(*byte).unwrap().or(event);
            }
            assert_eq!(event.unwrap(), b"a");
        }
        let mut crlf = Parser::new(&parser_limits(7, 9));
        let mut event = None;
        for byte in b"data: a\r\n\r\n" {
            event = crlf.byte(*byte).unwrap().or(event);
        }
        assert_eq!(event.unwrap(), b"a");
        let mut parser = Parser::new(&parser_limits(7, 8));
        for byte in b"data: a\n" {
            parser.byte(*byte).unwrap();
        }
        assert!(parser.byte(b'\n').is_err());
    }

    #[test]
    fn leases_survive_handoffs_and_release_exactly_once() {
        let limits = HttpClientLimits {
            max_buffered_bytes: 10,
            max_buffered_frames: 1,
            ..Default::default()
        };
        let budget = Budget::new(&limits);
        let lease = budget.reserve(10).unwrap();
        let pending_entry = lease.clone();
        drop(lease);
        assert!(budget.reserve(1).is_err());
        drop(pending_entry);
        let restored = budget.reserve(10).unwrap();
        assert!(budget.reserve(0).is_err());
        drop(restored);
        assert_eq!(budget.0.lock().unwrap().bytes, 0);
        assert_eq!(budget.0.lock().unwrap().frames, 0);
    }

    #[tokio::test]
    async fn success_and_error_bodies_share_the_same_cap() {
        for status in [200, 500] {
            let response = axum::http::Response::builder()
                .status(status)
                .body("1234")
                .unwrap();
            assert_eq!(capped_body(response.into(), 4).await.unwrap(), b"1234");
            let response = axum::http::Response::builder()
                .status(status)
                .body("12345")
                .unwrap();
            assert!(capped_body(response.into(), 4).await.is_err());
        }
    }

    #[test]
    fn bounded_extraction_validates_requested_limits() {
        let configured = ChannelLimits::default();
        let client = || {
            HttpClient::new("http://localhost")
                .unwrap()
                .with_limits(HttpClientLimits {
                    channel: configured,
                    ..Default::default()
                })
                .unwrap()
        };
        let (channel, future) =
            ConnectTo::<Client>::into_bounded_channel_and_future(client(), configured).unwrap();
        assert_eq!(channel.limits(), configured);
        drop((channel, future));
        let different = ChannelLimits {
            max_pending_requests: configured.max_pending_requests + 1,
            ..configured
        };
        assert!(ConnectTo::<Client>::into_bounded_channel_and_future(client(), different).is_err());
        let zero = ChannelLimits {
            max_buffered_frames: 0,
            ..configured
        };
        assert!(ConnectTo::<Client>::into_bounded_channel_and_future(client(), zero).is_err());
    }

    #[cfg(feature = "server")]
    #[tokio::test]
    async fn bounded_server_rejects_http_factory_with_different_core_limits() {
        use tower::ServiceExt;
        let calls = Arc::new(AtomicUsize::new(0));
        let factory_calls = calls.clone();
        let limits = crate::ServerLimits {
            channel_limits: ChannelLimits {
                max_pending_requests: 1,
                ..Default::default()
            },
            ..Default::default()
        };
        let router = crate::AcpHttpServer::new_bounded(
            move || {
                factory_calls.fetch_add(1, Ordering::SeqCst);
                HttpClient::new("http://localhost:9")
                    .unwrap()
                    .with_limits(HttpClientLimits::default())
                    .unwrap()
            },
            limits,
        )
        .unwrap()
        .into_router();
        let request = axum::http::Request::builder()
            .method("POST")
            .uri("/acp")
            .header("Content-Type", "application/json")
            .body(axum::body::Body::from(initialize().to_json().unwrap()))
            .unwrap();
        let response = router.oneshot(request).await.unwrap();
        assert_eq!(
            response.status(),
            axum::http::StatusCode::INTERNAL_SERVER_ERROR
        );
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn invalid_limits_and_websockets_are_rejected() {
        let limits = HttpClientLimits {
            max_request_posts: 0,
            ..Default::default()
        };
        assert!(
            HttpClient::new("http://localhost")
                .unwrap()
                .with_limits(limits)
                .is_err()
        );
        assert!(
            HttpClient::new("ws://localhost")
                .unwrap()
                .with_limits(HttpClientLimits::default())
                .is_err()
        );
        let limits = HttpClientLimits {
            max_sse_line_bytes: usize::MAX,
            ..Default::default()
        };
        assert!(
            HttpClient::new("http://localhost")
                .unwrap()
                .with_limits(limits)
                .is_err()
        );
    }

    async fn fixture() -> (String, Arc<AtomicUsize>, tokio::task::JoinHandle<()>) {
        use axum::{Router, body::Body, response::Response, routing::post};
        let count = Arc::new(AtomicUsize::new(0));
        let counter = count.clone();
        let app = Router::new().route(
            "/acp",
            post(move || {
                let counter = counter.clone();
                async move {
                    if counter.fetch_add(1, Ordering::SeqCst) == 0 {
                        Response::builder()
                            .header(HEADER_CONNECTION_ID, "bounded-test")
                            .body(Body::from(r#"{"jsonrpc":"2.0","id":0,"result":{}}"#))
                            .unwrap()
                    } else {
                        Response::builder().status(202).body(Body::empty()).unwrap()
                    }
                }
            })
            .get(|| async {
                Response::builder()
                    .header("Content-Type", "text/event-stream")
                    .body(Body::from_stream(futures::stream::pending::<
                        Result<String, std::convert::Infallible>,
                    >()))
                    .unwrap()
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        (format!("http://{address}/acp"), count, server)
    }

    // Loopback HTTP boundary; DELETE headers are withheld until explicitly released.
    async fn teardown_fixture(
        initialize_body: &'static str,
    ) -> (
        String,
        Arc<tokio::sync::Notify>,
        Arc<tokio::sync::Semaphore>,
        tokio::task::JoinHandle<()>,
    ) {
        use axum::{Router, body::Body, response::Response, routing::post};
        let started = Arc::new(tokio::sync::Notify::new());
        let release = Arc::new(tokio::sync::Semaphore::new(0));
        let delete_started = started.clone();
        let delete_release = release.clone();
        let app = Router::new().route(
            "/acp",
            post(move || async move {
                Response::builder()
                    .header(HEADER_CONNECTION_ID, "teardown-test")
                    .body(Body::from(initialize_body))
                    .unwrap()
            })
            .get(|| async {
                Response::builder()
                    .header("Content-Type", "text/event-stream")
                    .body(Body::from_stream(futures::stream::pending::<
                        Result<String, std::convert::Infallible>,
                    >()))
                    .unwrap()
            })
            .delete(move |headers: axum::http::HeaderMap| {
                let started = delete_started.clone();
                let release = delete_release.clone();
                async move {
                    assert_eq!(headers.get(HEADER_CONNECTION_ID).unwrap(), "teardown-test");
                    started.notify_one();
                    release.acquire().await.unwrap().forget();
                    // Headers complete DELETE; an infinite body must not delay it.
                    Response::builder()
                        .body(Body::from_stream(futures::stream::pending::<
                            Result<String, std::convert::Infallible>,
                        >()))
                        .unwrap()
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        (format!("http://{address}/acp"), started, release, server)
    }

    const INITIALIZED: &str = r#"{"jsonrpc":"2.0","id":0,"result":{}}"#;

    #[tokio::test]
    async fn graceful_eof_waits_for_delete_headers_not_body() {
        let (url, started, release, server) = teardown_fixture(INITIALIZED).await;
        let (mut channel, transport) = HttpClient::with_endpoint(url)
            .unwrap()
            .with_limits(HttpClientLimits::default())
            .unwrap()
            .into_bounded_channel_and_future();
        channel.tx.try_send(initialize()).unwrap();
        let driver = tokio::spawn(transport);
        assert!(channel.rx.next().await.is_some());
        channel.tx.close_channel();
        tokio::time::timeout(std::time::Duration::from_secs(3), started.notified())
            .await
            .unwrap();
        assert!(!driver.is_finished(), "EOF must await DELETE headers");
        release.add_permits(1);
        tokio::time::timeout(std::time::Duration::from_secs(3), driver)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        server.abort();
    }

    #[tokio::test]
    async fn malformed_initialize_closes_admission_and_awaits_delete() {
        let (url, started, release, server) = teardown_fixture("not JSON").await;
        let (channel, transport) = HttpClient::with_endpoint(url)
            .unwrap()
            .with_limits(HttpClientLimits::default())
            .unwrap()
            .into_bounded_channel_and_future();
        channel.tx.try_send(initialize()).unwrap();
        let driver = tokio::spawn(transport);
        tokio::time::timeout(std::time::Duration::from_secs(3), started.notified())
            .await
            .unwrap();
        assert!(channel.tx.try_send(initialize()).is_err());
        assert!(!driver.is_finished());
        release.add_permits(1);
        let error = tokio::time::timeout(std::time::Duration::from_secs(3), driver)
            .await
            .unwrap()
            .unwrap()
            .unwrap_err();
        assert!(error.to_string().contains("initialize response"));
        server.abort();
    }

    #[tokio::test]
    async fn cancellation_closes_locally_without_waiting_for_remote_delete() {
        let (url, started, release, server) = teardown_fixture(INITIALIZED).await;
        let (mut channel, transport) = HttpClient::with_endpoint(url)
            .unwrap()
            .with_limits(HttpClientLimits::default())
            .unwrap()
            .into_bounded_channel_and_future();
        channel.tx.try_send(initialize()).unwrap();
        let driver = tokio::spawn(transport);
        assert!(channel.rx.next().await.is_some());
        driver.abort();
        assert!(driver.await.unwrap_err().is_cancelled());
        assert!(channel.tx.try_send(initialize()).is_err());
        tokio::time::timeout(std::time::Duration::from_secs(3), started.notified())
            .await
            .unwrap();
        // Local cancellation completed while the server still withholds DELETE
        // completion. Best-effort dispatch is not a remote-deletion guarantee.
        assert_eq!(release.available_permits(), 0);
        release.add_permits(1);
        server.abort();
    }

    #[tokio::test]
    async fn cancellation_during_graceful_delete_preserves_admitted_response() {
        let (url, started, _release, server) = teardown_fixture(INITIALIZED).await;
        let (mut channel, transport) = HttpClient::with_endpoint(url)
            .unwrap()
            .with_limits(HttpClientLimits::default())
            .unwrap()
            .into_bounded_channel_and_future();
        channel.tx.try_send(initialize()).unwrap();
        channel.tx.close_channel();
        let driver = tokio::spawn(transport);
        tokio::time::timeout(std::time::Duration::from_secs(3), started.notified())
            .await
            .unwrap();
        driver.abort();
        assert!(driver.await.unwrap_err().is_cancelled());
        let response = channel
            .rx
            .next()
            .await
            .expect("admitted response survives graceful cleanup cancellation");
        assert_eq!(response.decode(), TransportFrame::parse_json(INITIALIZED));
        server.abort();
    }

    #[tokio::test]
    async fn delete_without_response_headers_has_finite_deadline() {
        let (url, started, _release, server) = teardown_fixture(INITIALIZED).await;
        let (mut channel, transport) = HttpClient::with_endpoint(url)
            .unwrap()
            .with_limits(HttpClientLimits::default())
            .unwrap()
            .into_bounded_channel_and_future();
        channel.tx.try_send(initialize()).unwrap();
        let driver = tokio::spawn(transport);
        assert!(channel.rx.next().await.is_some());
        channel.tx.close_channel();
        tokio::time::timeout(std::time::Duration::from_secs(3), started.notified())
            .await
            .unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(10), driver)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        server.abort();
    }

    #[tokio::test]
    async fn non_replayable_posts_ignore_redirect_and_retry_policies() {
        use axum::{Router, body::Body, response::Response, routing::post};
        for status in [307, 308, 503] {
            let count = Arc::new(AtomicUsize::new(0));
            let counter = count.clone();
            let app = Router::new().route(
                "/",
                post(move || {
                    let counter = counter.clone();
                    async move {
                        counter.fetch_add(1, Ordering::SeqCst);
                        Response::builder()
                            .status(status)
                            .header("Location", "/")
                            .body(Body::empty())
                            .unwrap()
                    }
                }),
            );
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let url = format!("http://{}/", listener.local_addr().unwrap());
            let server = tokio::spawn(async move {
                axum::serve(listener, app).await.unwrap();
            });
            let http = reqwest::Client::builder()
                .retry(
                    reqwest::retry::for_host("127.0.0.1")
                        .no_budget()
                        .max_retries_per_request(2)
                        .classify_fn(|response| {
                            if response.status().is_some_and(|status| {
                                status.is_server_error() || status.is_redirection()
                            }) {
                                response.retryable()
                            } else {
                                response.success()
                            }
                        }),
                )
                .build()
                .unwrap();
            let result = http
                .post(url)
                .body(non_replayable_body(b"request".to_vec()))
                .send()
                .await;
            assert_eq!(result.unwrap().status().as_u16(), status);
            assert_eq!(count.load(Ordering::SeqCst), 1);
            server.abort();
        }
    }

    #[tokio::test]
    async fn producer_exhaustion_cancels_stalled_initialize() {
        use axum::{Router, response::Response, routing::post};
        let started = Arc::new(tokio::sync::Notify::new());
        let notify = started.clone();
        let app = Router::new().route(
            "/acp",
            post(move || {
                let notify = notify.clone();
                async move {
                    notify.notify_one();
                    futures::future::pending::<Response>().await
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/acp", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        let limits = HttpClientLimits {
            channel: ChannelLimits {
                max_buffered_frames: 1,
                ..Default::default()
            },
            ..Default::default()
        };
        let (channel, transport) = HttpClient::with_endpoint(url)
            .unwrap()
            .with_limits(limits)
            .unwrap()
            .into_bounded_channel_and_future();
        channel.tx.try_send(initialize()).unwrap();
        let driver = tokio::spawn(transport);
        tokio::time::timeout(std::time::Duration::from_secs(3), started.notified())
            .await
            .unwrap();
        assert!(channel.tx.try_send(initialize()).is_err());
        assert!(
            tokio::time::timeout(std::time::Duration::from_secs(3), driver)
                .await
                .unwrap()
                .unwrap()
                .is_err()
        );
        server.abort();
    }

    #[tokio::test]
    async fn response_post_bypasses_stalled_request_lane() {
        use axum::{
            Router,
            body::{Body, Bytes},
            response::Response,
            routing::post,
        };
        let start_callback = Arc::new(tokio::sync::Notify::new());
        let response_received = Arc::new(tokio::sync::Notify::new());
        let request_finished = Arc::new(tokio::sync::Notify::new());
        let start = start_callback.clone();
        let received = response_received.clone();
        let finished = request_finished.clone();
        let app = Router::new().route(
            "/acp",
            post(move |body: Bytes| {
                let (start, received, finished) =
                    (start.clone(), received.clone(), finished.clone());
                async move {
                    let value: serde_json::Value = serde_json::from_slice(&body).unwrap();
                    if value["method"] == "initialize" {
                        return Response::builder()
                            .header(HEADER_CONNECTION_ID, "callback-test")
                            .body(Body::from(r#"{"jsonrpc":"2.0","id":0,"result":{}}"#))
                            .unwrap();
                    }
                    if value.get("method").is_some() {
                        start.notify_one();
                        received.notified().await;
                        finished.notify_one();
                    } else {
                        received.notify_one();
                    }
                    Response::builder().status(202).body(Body::empty()).unwrap()
                }
            })
            .get(move || {
                let start = start_callback.clone();
                async move {
                    let events = futures::stream::once(async move {
                        start.notified().await;
                        Ok::<_, std::convert::Infallible>(
                            "data: {\"jsonrpc\":\"2.0\",\"id\":77,\"method\":\"callback\"}\n\n",
                        )
                    })
                    .chain(futures::stream::pending());
                    Response::builder()
                        .header("Content-Type", "text/event-stream")
                        .body(Body::from_stream(events))
                        .unwrap()
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/acp", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        let limits = HttpClientLimits {
            max_request_posts: 1,
            max_response_posts: 1,
            ..Default::default()
        };
        let (mut channel, transport) = HttpClient::with_endpoint(url)
            .unwrap()
            .with_limits(limits)
            .unwrap()
            .into_bounded_channel_and_future();
        channel.tx.try_send(initialize()).unwrap();
        let driver = tokio::spawn(transport);
        assert!(
            tokio::time::timeout(std::time::Duration::from_secs(3), channel.rx.next())
                .await
                .unwrap()
                .is_some()
        );
        channel
            .tx
            .try_send(TransportFrame::parse_json(
                r#"{"jsonrpc":"2.0","id":1,"method":"ping"}"#,
            ))
            .unwrap();
        let callback = tokio::time::timeout(std::time::Duration::from_secs(3), channel.rx.next())
            .await
            .unwrap()
            .unwrap();
        assert!(String::from_utf8_lossy(callback.as_bytes()).contains("callback"));
        channel
            .tx
            .try_send(TransportFrame::parse_json(
                r#"{"jsonrpc":"2.0","id":77,"result":{}}"#,
            ))
            .unwrap();
        tokio::time::timeout(
            std::time::Duration::from_secs(3),
            request_finished.notified(),
        )
        .await
        .unwrap();
        driver.abort();
        assert!(driver.await.unwrap_err().is_cancelled());
        assert!(channel.tx.try_send(initialize()).is_err());
        server.abort();
    }

    fn initialize() -> TransportFrame {
        TransportFrame::parse_json(r#"{"jsonrpc":"2.0","id":0,"method":"initialize","params":{}}"#)
    }

    #[tokio::test]
    async fn duplicate_batch_entries_are_rejected_before_post() {
        let (url, count, server) = fixture().await;
        let limits = HttpClientLimits {
            max_pending_requests: 1,
            ..Default::default()
        };
        let (mut channel, transport) = HttpClient::with_endpoint(url)
            .unwrap()
            .with_limits(limits)
            .unwrap()
            .into_bounded_channel_and_future();
        channel.tx.try_send(initialize()).unwrap();
        let driver = tokio::spawn(transport);
        assert!(
            tokio::time::timeout(std::time::Duration::from_secs(3), channel.rx.next())
                .await
                .unwrap()
                .is_some()
        );
        channel.tx.try_send(TransportFrame::parse_json(r#"[{"jsonrpc":"2.0","id":1,"method":"ping"},{"jsonrpc":"2.0","id":1,"method":"ping"}]"#)).unwrap();
        let error = tokio::time::timeout(std::time::Duration::from_secs(3), driver)
            .await
            .unwrap()
            .unwrap()
            .unwrap_err();
        assert!(error.to_string().contains("outstanding RPC"));
        assert_eq!(count.load(Ordering::SeqCst), 1);
        assert!(channel.tx.try_send(initialize()).is_err());
        server.abort();
    }

    #[tokio::test]
    async fn http_acceptance_does_not_release_outstanding_rpc_slot() {
        let (url, count, server) = fixture().await;
        let limits = HttpClientLimits {
            max_pending_requests: 1,
            ..Default::default()
        };
        let (mut channel, transport) = HttpClient::with_endpoint(url)
            .unwrap()
            .with_limits(limits)
            .unwrap()
            .into_bounded_channel_and_future();
        channel.tx.try_send(initialize()).unwrap();
        let driver = tokio::spawn(transport);
        assert!(
            tokio::time::timeout(std::time::Duration::from_secs(3), channel.rx.next())
                .await
                .unwrap()
                .is_some()
        );
        let request = || TransportFrame::parse_json(r#"{"jsonrpc":"2.0","id":1,"method":"ping"}"#);
        channel.tx.try_send(request()).unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(3), async {
            while count.load(Ordering::SeqCst) != 2 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        channel.tx.try_send(request()).unwrap();
        assert!(
            tokio::time::timeout(std::time::Duration::from_secs(3), driver)
                .await
                .unwrap()
                .unwrap()
                .is_err()
        );
        assert_eq!(count.load(Ordering::SeqCst), 2);
        server.abort();
    }
}

struct Pending {
    id: RequestId,
    method: String,
    _lease: Arc<Lease>,
}
struct Post {
    frame: ChargedFrame,
    sessions: Vec<String>,
    session_header: Option<String>,
    lease: Arc<Lease>,
    response_workspace: Arc<Lease>,
}
#[derive(Default)]
struct Lane {
    queue: VecDeque<Post>,
    active: bool,
}
impl Lane {
    fn count(&self) -> usize {
        self.queue.len() + usize::from(self.active)
    }
}
type PostFuture = BoxFuture<'static, Result<bool, AcpError>>;

fn messages(frame: &TransportFrame) -> Result<Vec<&RawJsonRpcMessage>, AcpError> {
    match frame {
        TransportFrame::Single(message) => Ok(vec![message]),
        TransportFrame::Batch(batch) => batch
            .entries()
            .map(|entry| match entry {
                TransportBatchEntry::Message(message) => Ok(message),
                TransportBatchEntry::Malformed { .. } => Err(failure("malformed batch entry")),
            })
            .collect(),
        TransportFrame::Malformed { .. } => Err(failure("malformed frame")),
    }
}

struct Streams {
    registered: HashMap<Option<String>, Arc<Lease>>,
    ready: HashSet<Option<String>>,
    futures: FuturesUnordered<SseFuture>,
}
impl Streams {
    fn new() -> Self {
        Self {
            registered: HashMap::new(),
            ready: HashSet::new(),
            futures: FuturesUnordered::new(),
        }
    }
    fn open(
        &mut self,
        client: &HttpClient,
        connection: &str,
        key: Option<String>,
        limits: &HttpClientLimits,
        budget: &Budget,
    ) -> Result<(), AcpError> {
        if self.registered.contains_key(&key) {
            return Ok(());
        }
        if self.registered.len() >= limits.max_sse_streams {
            return Err(failure("SSE stream limit exhausted"));
        }
        // Account for the registry, readiness set and live stream's key copies.
        let key_bytes = key
            .as_ref()
            .map_or(0, String::len)
            .checked_mul(4)
            .and_then(|n| n.checked_add(connection.len()))
            .ok_or_else(|| failure("session metadata arithmetic overflow"))?;
        let metadata = budget.reserve(key_bytes)?;
        let future = open_sse(client, connection, key.clone(), limits, budget)?;
        self.registered.insert(key, metadata);
        self.futures.push(future);
        Ok(())
    }
}

fn launch(
    lane: &mut Lane,
    response_lane: bool,
    streams: &Streams,
    client: &HttpClient,
    connection: &str,
    cap: usize,
    posts: &mut FuturesUnordered<PostFuture>,
) {
    if lane.active || !streams.ready.contains(&None) {
        return;
    }
    let Some(post) = lane.queue.front() else {
        return;
    };
    if post.sessions.iter().any(|id| {
        !streams
            .ready
            .iter()
            .any(|key| key.as_deref() == Some(id.as_str()))
    }) {
        return;
    }
    let post = lane.queue.pop_front().expect("front checked");
    let mut request = client
        .http
        .post(client.endpoint.clone())
        .header("Content-Type", "application/json")
        .header("Accept", "application/json")
        .header(HEADER_CONNECTION_ID, connection);
    if let Some(session) = &post.session_header {
        request = request.header(HEADER_SESSION_ID, session);
    }
    lane.active = true;
    let expected_url = client.endpoint.clone();
    posts.push(
        async move {
            // The charged core frame stays alive through reqwest's body handoff.
            // Waiting until response headers is conservative; this is NOT peer ACK.
            let request = request.body(non_replayable_body(post.frame.as_bytes().to_vec()));
            let result = request.send().await.map_err(|e| failure(e.to_string()));
            drop(post.frame);
            let response = result?;
            if response.url() != &expected_url {
                return Err(failure("POST redirect is not supported"));
            }
            let status = response.status();
            drop(capped_body(response, cap).await?);
            drop(post.response_workspace);
            drop(post.lease);
            if !status.is_success() {
                return Err(failure(format!("POST HTTP {status}")));
            }
            Ok(response_lane)
        }
        .boxed(),
    );
}

// Single-owner teardown: no registry or task per POST. The connection metadata
// remains charged even if cancellation transfers cleanup to a background task.
struct ConnectionCleanup {
    client: HttpClient,
    connection: Option<(String, Arc<Lease>)>,
}

impl ConnectionCleanup {
    async fn close(&mut self) {
        if let Some(connection) = self.connection.take() {
            Self::send_close(
                self.client.http.clone(),
                self.client.endpoint.clone(),
                connection,
            )
            .await;
        }
    }

    async fn send_close(
        http: reqwest::Client,
        endpoint: url::Url,
        (connection, lease): (String, Arc<Lease>),
    ) {
        // Do not consume the response body: it may be arbitrarily large or never
        // end. The request deadline also bounds a peer that never sends headers.
        if let Err(error) = http
            .delete(endpoint)
            .header(HEADER_CONNECTION_ID, connection)
            .timeout(std::time::Duration::from_secs(5))
            .send()
            .await
        {
            debug!("bounded HTTP DELETE failed (ignored): {error}");
        }
        drop(lease);
    }
}

impl Drop for ConnectionCleanup {
    fn drop(&mut self) {
        let Some(connection) = self.connection.take() else {
            return;
        };
        if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            drop(runtime.spawn(Self::send_close(
                self.client.http.clone(),
                self.client.endpoint.clone(),
                connection,
            )));
        }
    }
}

async fn run_bounded(
    client: HttpClient,
    limits: HttpClientLimits,
    channel: BoundedChannel,
) -> Result<(), AcpError> {
    struct Terminate {
        sender: agent_client_protocol::BoundedSender,
        armed: bool,
    }
    impl Drop for Terminate {
        fn drop(&mut self) {
            if self.armed {
                drop(self.sender.fail("bounded HTTP transport ended"));
            }
        }
    }
    let mut terminal = Terminate {
        sender: channel.tx.clone(),
        armed: true,
    };
    let mut cleanup = ConnectionCleanup {
        client,
        connection: None,
    };
    let failed = terminal.sender.failure();
    let running = run_bounded_inner(&mut cleanup, limits, channel).boxed();
    let result = match futures::future::select(failed, running).await {
        futures::future::Either::Left((error, running)) => {
            drop(running);
            Err(error)
        }
        futures::future::Either::Right((result, failed)) => {
            drop(failed);
            result
        }
    };
    // Preserve already-admitted inbound responses on graceful EOF, including
    // cancellation during teardown. Errors close local admission before DELETE.
    terminal.armed = result.is_err();
    if result.is_err() {
        drop(terminal.sender.fail("bounded HTTP transport ended"));
    }
    cleanup.close().await;
    result
}

async fn run_bounded_inner(
    cleanup: &mut ConnectionCleanup,
    limits: HttpClientLimits,
    channel: BoundedChannel,
) -> Result<(), AcpError> {
    enum Event {
        Outgoing(Option<ChargedFrame>),
        Sse(Box<Result<SseEvent, AcpError>>),
        Post(Result<bool, AcpError>),
    }
    let client = &cleanup.client;
    let BoundedChannel {
        tx: incoming,
        rx: mut outgoing,
    } = channel;
    let budget = Budget::new(&limits);
    let Some(first) = outgoing.next().await else {
        return Ok(());
    };
    let frame = first.decode();
    if !matches!(&frame, TransportFrame::Single(message) if is_initialize_request(message)) {
        return Err(failure("first frame must be a single initialize request"));
    }
    let init_bytes = first
        .as_bytes()
        .len()
        .checked_mul(2)
        .and_then(|n| n.checked_add(limits.max_response_bytes))
        .ok_or_else(|| failure("initialize budget arithmetic overflow"))?;
    let initialization = budget.reserve(init_bytes)?;
    let response = client
        .http
        .post(client.endpoint.clone())
        .header("Content-Type", "application/json")
        .header("Accept", "application/json")
        .body(non_replayable_body(first.as_bytes().to_vec()))
        .send()
        .await
        .map_err(|e| failure(e.to_string()))?;
    drop(first);
    let expected_id = match &frame {
        TransportFrame::Single(RawJsonRpcMessage::Request(request)) => request.id.clone(),
        _ => unreachable!("initialize shape checked"),
    };
    drop(frame);
    if response.url() != &client.endpoint {
        return Err(failure("initialize redirect is not supported"));
    }
    let status = response.status();
    if let Some(header) = response.headers().get(HEADER_CONNECTION_ID) {
        if header.as_bytes().len() > limits.channel.max_frame_bytes {
            return Err(failure("connection header exceeds frame limit"));
        }
        let connection = header
            .to_str()
            .map_err(|_| failure("invalid connection header"))?;
        let lease = budget.reserve(header.as_bytes().len())?;
        // Capture before any body await so failed initialization is cleaned up.
        cleanup.connection = Some((connection.to_owned(), lease));
    }
    let body = capped_body(response, limits.max_response_bytes).await?;
    if !status.is_success() {
        return Err(failure(format!("initialize HTTP {status}")));
    }
    let text = std::str::from_utf8(&body).map_err(|_| failure("invalid initialize UTF-8"))?;
    let reply = TransportFrame::parse_json(text);
    let rejected = match &reply {
        TransportFrame::Single(RawJsonRpcMessage::Response(RpcResponse::Error { .. })) => true,
        TransportFrame::Single(RawJsonRpcMessage::Response(_)) => false,
        _ => return Err(failure("initialize response must be one JSON-RPC response")),
    };
    if !matches!(&reply, TransportFrame::Single(message) if message.response_id() == Some(&expected_id))
    {
        return Err(failure("initialize response ID mismatch"));
    }
    incoming.try_send(reply)?;
    drop(expected_id);
    drop(body);
    drop(initialization);
    if rejected {
        return Ok(());
    }
    let (connection, _) = cleanup
        .connection
        .as_ref()
        .ok_or_else(|| failure("missing connection ID"))?;
    let mut streams = Streams::new();
    streams.open(client, connection, None, &limits, &budget)?;
    let mut pending = VecDeque::<Pending>::new();
    let mut ordered = Lane::default();
    let mut responses = Lane::default();
    let mut posts = FuturesUnordered::<PostFuture>::new();
    let mut closed = false;
    loop {
        launch(
            &mut ordered,
            false,
            &streams,
            client,
            connection,
            limits.max_response_bytes,
            &mut posts,
        );
        launch(
            &mut responses,
            true,
            &streams,
            client,
            connection,
            limits.max_response_bytes,
            &mut posts,
        );
        if closed && ordered.count() == 0 && responses.count() == 0 && pending.is_empty() {
            return Ok(());
        }
        let event = {
            let next_outgoing = async {
                if closed {
                    futures::future::pending().await
                } else {
                    outgoing.next().await
                }
            }
            .fuse();
            let next_sse = async {
                match streams.futures.next().await {
                    Some(event) => event,
                    None => futures::future::pending().await,
                }
            }
            .fuse();
            let next_post = async {
                match posts.next().await {
                    Some(event) => event,
                    None => futures::future::pending().await,
                }
            }
            .fuse();
            pin_mut!(next_outgoing, next_sse, next_post);
            futures::select! {
                frame = next_outgoing => Event::Outgoing(frame),
                event = next_sse => Event::Sse(Box::new(event)),
                event = next_post => Event::Post(event),
            }
        };
        match event {
            Event::Outgoing(None) => closed = true,
            Event::Outgoing(Some(charged)) => {
                let frame = charged.decode();
                let is_response = is_response_only_frame(&frame);
                let lane = if is_response {
                    &mut responses
                } else {
                    &mut ordered
                };
                let cap = if is_response {
                    limits.max_response_posts
                } else {
                    limits.max_request_posts
                };
                if lane.count() >= cap {
                    return Err(failure("queued plus active POST limit exhausted"));
                }
                let entries = messages(&frame)?;
                let requests = entries
                    .iter()
                    .filter(|message| matches!(message, RawJsonRpcMessage::Request(_)))
                    .count();
                if requests > limits.max_pending_requests.saturating_sub(pending.len()) {
                    return Err(failure("outstanding RPC entry limit exhausted"));
                }
                let bookkeeping = FrameBookkeeping::for_frame(&frame).map_err(failure)?;
                // Body, pending ID/method, session list and header copies
                // can coexist; reserve four body-sized wire allowances.
                let retained = charged
                    .as_bytes()
                    .len()
                    .checked_mul(4)
                    .and_then(|n| n.checked_add(connection.len()))
                    .ok_or_else(|| failure("POST metadata arithmetic overflow"))?;
                let lease = budget.reserve(retained)?;
                let workspace = budget.reserve(limits.max_response_bytes)?;
                let session_header = match &frame {
                    TransportFrame::Single(message) => {
                        validated_session_id(message).map_err(failure)?
                    }
                    _ => None,
                };
                // Every entry, including duplicate and null IDs, consumes a slot.
                // The body-sized lease remains until all entries complete, not
                // merely until the HTTP POST is accepted.
                for message in entries {
                    if let RawJsonRpcMessage::Request(request) = message {
                        pending.push_back(Pending {
                            id: request.id.clone(),
                            method: request.method.to_string(),
                            _lease: lease.clone(),
                        });
                    }
                }
                for session in &bookkeeping.session_ids {
                    streams.open(client, connection, Some(session.clone()), &limits, &budget)?;
                }
                lane.queue.push_back(Post {
                    frame: charged,
                    sessions: bookkeeping.session_ids,
                    session_header,
                    lease,
                    response_workspace: workspace,
                });
            }
            Event::Post(result) => {
                if result? {
                    responses.active = false;
                } else {
                    ordered.active = false;
                }
            }
            Event::Sse(result) => match (*result)? {
                SseEvent::Ready(stream) => {
                    streams.ready.insert(stream.key.clone());
                    streams.futures.push(stream.next());
                }
                SseEvent::Frame(stream, bytes) => {
                    if bytes.len() > limits.channel.max_frame_bytes {
                        return Err(failure("SSE frame exceeds core frame limit"));
                    }
                    let text =
                        std::str::from_utf8(&bytes).map_err(|_| failure("invalid SSE UTF-8"))?;
                    let frame = TransportFrame::parse_json(text);
                    for message in messages(&frame)? {
                        let RawJsonRpcMessage::Response(response) = message else {
                            continue;
                        };
                        let Some(id) = message.response_id() else {
                            continue;
                        };
                        if let Some(index) = pending.iter().position(|entry| &entry.id == id) {
                            let entry = pending.remove(index).expect("position checked");
                            if is_session_opening_method(&entry.method)
                                && let RpcResponse::Result { result, .. } = response
                                && let Some(session) =
                                    result.get("sessionId").and_then(|v| v.as_str())
                            {
                                streams.open(
                                    client,
                                    connection,
                                    Some(session.to_owned()),
                                    &limits,
                                    &budget,
                                )?;
                            }
                        }
                    }
                    incoming.try_send(frame)?;
                    streams.futures.push(stream.next());
                }
            },
        }
    }
}

struct Sse {
    key: Option<String>,
    response: reqwest::Response,
    parser: Parser,
    chunk: Vec<u8>,
    offset: usize,
    max_chunk: usize,
    _workspace: Arc<Lease>,
}
enum SseEvent {
    Ready(Sse),
    Frame(Sse, Vec<u8>),
}
type SseFuture = BoxFuture<'static, Result<SseEvent, AcpError>>;

fn open_sse(
    client: &HttpClient,
    connection: &str,
    key: Option<String>,
    limits: &HttpClientLimits,
    budget: &Budget,
) -> Result<SseFuture, AcpError> {
    let workspace = budget.reserve(limits.workspace()?)?;
    let mut request = client
        .http
        .get(client.endpoint.clone())
        .header("Accept", "text/event-stream")
        .header(HEADER_CONNECTION_ID, connection);
    if let Some(key) = &key {
        request = request.header(HEADER_SESSION_ID, key);
    }
    let limits = limits.clone();
    Ok(async move {
        let response = request.send().await.map_err(|e| failure(e.to_string()))?;
        if !response.status().is_success() {
            // Error text is deliberately not collected, so an endless error body
            // cannot allocate or delay teardown.
            return Err(failure(format!("SSE HTTP {}", response.status())));
        }
        Ok(SseEvent::Ready(Sse {
            key,
            response,
            parser: Parser::new(&limits),
            chunk: Vec::new(),
            offset: 0,
            max_chunk: limits.max_sse_chunk_bytes,
            _workspace: workspace,
        }))
    }
    .boxed())
}
impl Sse {
    fn next(mut self) -> SseFuture {
        async move {
            let mut work = 0;
            loop {
                if self.offset == self.chunk.len() {
                    self.chunk.clear();
                    self.offset = 0;
                    let chunk = self
                        .response
                        .chunk()
                        .await
                        .map_err(|e| failure(e.to_string()))?
                        .ok_or_else(|| failure("SSE ended; resume/replay is disabled"))?;
                    if chunk.len() > self.max_chunk {
                        return Err(failure("SSE chunk limit exhausted"));
                    }
                    self.chunk.extend_from_slice(&chunk);
                }
                while self.offset < self.chunk.len() {
                    let byte = self.chunk[self.offset];
                    self.offset += 1;
                    if let Some(data) = self.parser.byte(byte)?
                        && !data.is_empty()
                    {
                        return Ok(SseEvent::Frame(self, data));
                    }
                    work += 1;
                    if work == 8192 {
                        // Cooperative yielding even for a continuously ready
                        // comment-only stream, without a spawned observer task.
                        let mut yielded = false;
                        futures::future::poll_fn(|cx| {
                            if yielded {
                                Poll::Ready(())
                            } else {
                                yielded = true;
                                cx.waker().wake_by_ref();
                                Poll::Pending
                            }
                        })
                        .await;
                        work = 0;
                    }
                }
            }
        }
        .boxed()
    }
}
