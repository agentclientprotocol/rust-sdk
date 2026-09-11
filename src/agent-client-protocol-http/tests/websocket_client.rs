#![cfg(feature = "client")]

use std::{future::Future, sync::Arc, time::Duration};

use agent_client_protocol::{Channel, Client, ConnectTo, RawJsonRpcMessage, TransportFrame};
use agent_client_protocol_http::HttpClient;
use async_tungstenite::{tokio::accept_hdr_async, tungstenite::handshake::server::Request};
use futures::{StreamExt, future::BoxFuture};
use serde_json::json;
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    time::timeout,
};
use tokio_rustls::{TlsAcceptor, rustls};

const DEADLINE: Duration = Duration::from_secs(10);

struct Tls {
    certificate: rustls::pki_types::CertificateDer<'static>,
    acceptor: TlsAcceptor,
}

impl Tls {
    fn new() -> Self {
        let rcgen::CertifiedKey { cert, signing_key } =
            rcgen::generate_simple_self_signed(vec!["localhost".into()]).unwrap();
        let certificate = cert.der().clone();
        let key = rustls::pki_types::PrivatePkcs8KeyDer::from(signing_key.serialize_der());
        let mut config = rustls::ServerConfig::builder_with_provider(Arc::new(
            rustls::crypto::aws_lc_rs::default_provider(),
        ))
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_no_client_auth()
        .with_single_cert(vec![certificate.clone()], key.into())
        .unwrap();
        // Put h2 first: simply enabling HTTP/2 in a downstream crate used to
        // make reqwest select it for this HTTP/1.1-only upgrade.
        config.alpn_protocols = vec![b"h2".to_vec(), b"http/1.1".to_vec()];
        Self {
            certificate,
            acceptor: TlsAcceptor::from(Arc::new(config)),
        }
    }

    fn configure(&self, builder: reqwest::ClientBuilder) -> reqwest::ClientBuilder {
        builder
            .no_proxy()
            .tls_certs_merge([reqwest::Certificate::from_der(&self.certificate).unwrap()])
    }

    fn raw_config(&self, protocol: &[u8]) -> rustls::ClientConfig {
        let mut roots = rustls::RootCertStore::empty();
        roots.add(self.certificate.clone()).unwrap();
        let mut config = rustls::ClientConfig::builder_with_provider(Arc::new(
            rustls::crypto::aws_lc_rs::default_provider(),
        ))
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_root_certificates(roots)
        .with_no_client_auth();
        config.alpn_protocols = vec![protocol.to_vec()];
        config
    }
}

async fn listen() -> TcpListener {
    TcpListener::bind("127.0.0.1:0").await.unwrap()
}

fn queued(
    client: HttpClient,
) -> (
    Channel,
    BoxFuture<'static, Result<(), agent_client_protocol::Error>>,
) {
    let (caller, transport) = ConnectTo::<Client>::into_channel_and_future(client);
    caller
        .tx
        .unbounded_send(TransportFrame::Single(
            RawJsonRpcMessage::notification("custom/queued".into(), json!({"probe": 333})).unwrap(),
        ))
        .unwrap();
    (caller, transport)
}

async fn headers(socket: &mut (impl AsyncRead + Unpin)) -> String {
    let mut bytes = Vec::new();
    while !bytes.ends_with(b"\r\n\r\n") {
        bytes.push(socket.read_u8().await.unwrap());
        assert!(bytes.len() < 16 * 1024, "oversized handshake");
    }
    String::from_utf8(bytes).unwrap()
}

// Tungstenite fixes the callback's error type to an unboxed HTTP response.
#[allow(clippy::result_large_err)]
async fn exchange(socket: impl AsyncRead + AsyncWrite + Unpin, path: &str, delay: Duration) {
    let mut ws = accept_hdr_async(socket, |request: &Request, response| {
        assert_eq!(request.uri().path(), path);
        Ok(response)
    })
    .await
    .unwrap();
    let message = ws.next().await.unwrap().unwrap();
    let value: serde_json::Value = serde_json::from_str(message.to_text().unwrap()).unwrap();
    assert_eq!(value["method"], "custom/queued");
    assert_eq!(value["params"]["probe"], 333);
    // Deliberately cross the configured request/read deadline after upgrade.
    // This is the behavior under test, not a fixture synchronization sleep.
    if !delay.is_zero() {
        tokio::time::sleep(delay).await;
    }
    ws.send(message).await.unwrap();
    // The observer drops its ACP channel after receiving the echo, allowing
    // the writer to finish successfully before the server closes its socket.
    assert!(ws.next().await.unwrap().unwrap().is_close());
}

fn successful_exchange(
    client: HttpClient,
    fixture: impl Future<Output = ()>,
) -> impl Future<Output = ()> {
    // Box before constructing the runner future so nested TLS state machines
    // do not inflate every caller's future.
    let fixture = Box::pin(fixture);
    async move {
        let (mut caller, transport) = queued(client);
        let observe = async {
            let message = caller.rx.next().await.expect("echo from upgraded stream");
            let value: serde_json::Value =
                serde_json::from_str(&message.to_json().unwrap()).unwrap();
            assert_eq!(value["method"], "custom/queued");
            assert_eq!(value["params"]["probe"], 333);
            drop(caller);
        };
        let (result, (), ()) = timeout(DEADLINE, async {
            futures::join!(transport, fixture, observe)
        })
        .await
        .expect("client and fixture must finish");
        result.unwrap();
    }
}

#[tokio::test]
async fn custom_roots_and_http2_feature_unification_use_http1_for_wss() {
    for exact in [false, true] {
        let tls = Tls::new();
        let listener = listen().await;
        let url = format!(
            "wss://localhost:{}/custom",
            listener.local_addr().unwrap().port()
        );
        let builder = if exact {
            HttpClient::builder_with_endpoint(&url)
        } else {
            HttpClient::builder(&url)
        };
        let client = builder
            .configure_http(|builder| tls.configure(builder).http2_prior_knowledge())
            .build()
            .unwrap();
        successful_exchange(client, async {
            let (socket, _) = listener.accept().await.unwrap();
            let socket = tls.acceptor.accept(socket).await.unwrap();
            assert_eq!(
                socket.get_ref().1.alpn_protocol(),
                Some(b"http/1.1".as_slice())
            );
            exchange(
                socket,
                if exact { "/custom" } else { "/custom/acp" },
                Duration::ZERO,
            )
            .await;
        })
        .await;
    }
}

#[tokio::test]
async fn websocket_redirects_never_reach_destination_or_send_acp() {
    for (secure, destination_scheme) in [(false, "http"), (true, "https"), (true, "http")] {
        let tls = Tls::new();
        let origin = listen().await;
        let destination = listen().await;
        let location = format!(
            "{destination_scheme}://localhost:{}/stolen",
            destination.local_addr().unwrap().port()
        );
        let scheme = if secure { "wss" } else { "ws" };
        let client = HttpClient::builder(format!(
            "{scheme}://localhost:{}",
            origin.local_addr().unwrap().port()
        ))
        .configure_http(|builder| {
            tls.configure(builder)
                .redirect(reqwest::redirect::Policy::limited(5))
        })
        .build()
        .unwrap();
        let (caller, transport) = queued(client);
        let fixture = async {
            let (socket, _) = origin.accept().await.unwrap();
            if secure {
                redirect(tls.acceptor.accept(socket).await.unwrap(), &location).await;
            } else {
                redirect(socket, &location).await;
            }
        };
        timeout(DEADLINE, async {
            tokio::select! {
                biased;
                connection = destination.accept() => panic!("redirect destination contacted: {connection:?}"),
                (result, ()) = async { futures::join!(transport, fixture) } => {
                    assert!(result.is_err(), "redirect must fail closed");
                }
            }
            // Catch a connection already queued when the transport completed.
            assert!(futures::poll!(Box::pin(destination.accept())).is_pending());
        }).await.unwrap();
        drop(caller);
    }
}

async fn redirect(mut socket: impl AsyncRead + AsyncWrite + Unpin, location: &str) {
    assert!(
        headers(&mut socket)
            .await
            .starts_with("GET /acp HTTP/1.1\r\n")
    );
    socket.write_all(format!("HTTP/1.1 302 Found\r\nLocation: {location}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").as_bytes()).await.unwrap();
    let mut received = Vec::new();
    if let Err(error) = socket.read_to_end(&mut received).await {
        // A rejected handshake may drop TLS without sending close_notify.
        assert!(
            matches!(
                error.kind(),
                std::io::ErrorKind::UnexpectedEof | std::io::ErrorKind::ConnectionReset
            ),
            "{error}"
        );
    }
    assert!(received.is_empty(), "ACP sent to redirecting origin");
}

#[tokio::test]
async fn configured_proxy_tunnels_wss_with_authentication() {
    let tls = Tls::new();
    let origin = listen().await;
    let proxy = listen().await;
    let target = format!("localhost:{}", origin.local_addr().unwrap().port());
    let client = HttpClient::builder(format!("wss://{target}"))
        .configure_http(|builder| {
            tls.configure(builder).proxy(
                reqwest::Proxy::all(format!("http://{}", proxy.local_addr().unwrap()))
                    .unwrap()
                    .basic_auth("acp", "secret"),
            )
        })
        .build()
        .unwrap();
    successful_exchange(client, async {
        let tunnel = async {
            let (mut downstream, _) = proxy.accept().await.unwrap();
            let request = headers(&mut downstream).await;
            assert!(request.starts_with(&format!("CONNECT {target} HTTP/1.1\r\n")));
            assert_eq!(
                request
                    .lines()
                    .filter_map(|line| line.split_once(':'))
                    .find(|(name, _)| name.eq_ignore_ascii_case("proxy-authorization"))
                    .map(|(_, value)| value.trim()),
                Some("Basic YWNwOnNlY3JldA==")
            );
            let mut upstream = TcpStream::connect(origin.local_addr().unwrap())
                .await
                .unwrap();
            downstream
                .write_all(b"HTTP/1.1 200 Connection Established\r\n\r\n")
                .await
                .unwrap();
            // Ends when the client and TLS server close their upgraded sockets.
            drop(tokio::io::copy_bidirectional(&mut downstream, &mut upstream).await);
        };
        let server = async {
            let (socket, _) = origin.accept().await.unwrap();
            let socket = tls.acceptor.accept(socket).await.unwrap();
            exchange(socket, "/acp", Duration::ZERO).await;
        };
        futures::join!(tunnel, server);
    })
    .await;
}

#[tokio::test]
async fn request_and_read_timeouts_do_not_expire_upgraded_stream() {
    let listener = listen().await;
    let client = HttpClient::builder(format!("ws://{}", listener.local_addr().unwrap()))
        .configure_http(|builder| {
            builder
                .no_proxy()
                .timeout(Duration::from_secs(1))
                .read_timeout(Duration::from_secs(1))
        })
        .build()
        .unwrap();
    successful_exchange(client, async {
        let (socket, _) = listener.accept().await.unwrap();
        exchange(socket, "/acp", Duration::from_secs(2)).await;
    })
    .await;
}

#[tokio::test]
async fn preconfigured_rustls_http1_supports_websocket_exchange() {
    let tls = Tls::new();
    let listener = listen().await;
    let client = HttpClient::builder(format!(
        "wss://localhost:{}",
        listener.local_addr().unwrap().port()
    ))
    .configure_http(|builder| {
        builder
            .no_proxy()
            .tls_backend_preconfigured(tls.raw_config(b"http/1.1"))
    })
    .build()
    .unwrap();
    successful_exchange(client, async {
        let (socket, _) = listener.accept().await.unwrap();
        let socket = tls.acceptor.accept(socket).await.unwrap();
        assert_eq!(
            socket.get_ref().1.alpn_protocol(),
            Some(b"http/1.1".as_slice())
        );
        exchange(socket, "/acp", Duration::ZERO).await;
    })
    .await;
}

#[tokio::test]
async fn incompatible_preconfigured_alpn_fails_without_acp_frames() {
    let tls = Tls::new();
    let listener = listen().await;
    let client = HttpClient::builder(format!(
        "wss://localhost:{}",
        listener.local_addr().unwrap().port()
    ))
    .configure_http(|builder| {
        builder
            .no_proxy()
            .tls_backend_preconfigured(tls.raw_config(b"h2"))
    })
    .build()
    .unwrap();
    let (caller, transport) = queued(client);
    let fixture = async {
        let (socket, _) = listener.accept().await.unwrap();
        let socket = tls.acceptor.accept(socket).await.unwrap();
        assert_eq!(socket.get_ref().1.alpn_protocol(), Some(b"h2".as_slice()));
        // Complete an actual h2 exchange, rather than merely provoking a TLS
        // or HTTP parsing error. The response cannot authorize a WS upgrade.
        let mut connection = h2::server::handshake(socket).await.unwrap();
        let (request, mut respond) = connection.accept().await.unwrap().unwrap();
        assert_eq!(request.method(), "GET");
        assert_eq!(request.uri().path(), "/acp");
        assert!(request.body().is_end_stream(), "ACP in handshake request");
        respond
            .send_response(
                async_tungstenite::tungstenite::http::Response::builder()
                    .status(200)
                    .body(())
                    .unwrap(),
                true,
            )
            .unwrap();
        // Driving the connection flushes the response. The failed transport
        // must close it without creating any further streams.
        assert!(!matches!(connection.accept().await, Some(Ok(_))));
    };
    let (result, ()) = timeout(DEADLINE, async { futures::join!(transport, fixture) })
        .await
        .unwrap();
    let error = result.unwrap_err();
    assert!(error.to_string().contains("expected HTTP/1.1"), "{error}");
    drop(caller);
}

fn initialize(
    client: HttpClient,
) -> (
    Channel,
    BoxFuture<'static, Result<(), agent_client_protocol::Error>>,
) {
    let (caller, transport) = ConnectTo::<Client>::into_channel_and_future(client);
    caller
        .tx
        .unbounded_send(TransportFrame::Single(
            RawJsonRpcMessage::request(
                "initialize".into(),
                json!({}),
                agent_client_protocol::schema::v1::RequestId::Number(1),
            )
            .unwrap(),
        ))
        .unwrap();
    (caller, transport)
}

#[tokio::test]
async fn http_builder_retains_redirect_policy() {
    let origin = listen().await;
    let destination = listen().await;
    let client = HttpClient::builder(format!("http://{}", origin.local_addr().unwrap()))
        .configure_http(|builder| {
            builder
                .no_proxy()
                .redirect(reqwest::redirect::Policy::limited(1))
        })
        .build()
        .unwrap();
    let (caller, transport) = initialize(client);
    let fixture = async {
        let (mut socket, _) = origin.accept().await.unwrap();
        observe_initialize(&mut socket).await;
        socket.write_all(format!(
            "HTTP/1.1 307 Temporary Redirect\r\nLocation: http://{}/acp\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
            destination.local_addr().unwrap(),
        ).as_bytes()).await.unwrap();
        drop(socket);
        let (mut socket, _) = destination.accept().await.unwrap();
        observe_initialize(&mut socket).await;
        socket
            .write_all(
                b"HTTP/1.1 400 Bad Request\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
            )
            .await
            .unwrap();
    };
    let (result, ()) = timeout(DEADLINE, async { futures::join!(transport, fixture) })
        .await
        .unwrap();
    assert!(result.is_err(), "dummy initialize response should fail");
    drop(caller);
}

async fn observe_initialize(socket: &mut TcpStream) {
    let request = headers(socket).await;
    assert!(request.starts_with("POST /acp HTTP/1.1\r\n"));
    let length: usize = request
        .lines()
        .filter_map(|line| line.split_once(':'))
        .find(|(name, _)| name.eq_ignore_ascii_case("content-length"))
        .unwrap()
        .1
        .trim()
        .parse()
        .unwrap();
    assert!(length < 16 * 1024);
    let mut body = vec![0; length];
    socket.read_exact(&mut body).await.unwrap();
    let message: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(message["method"], "initialize");
}

#[tokio::test]
async fn http_builder_retains_http2_prior_knowledge() {
    let listener = listen().await;
    let client = HttpClient::builder(format!("http://{}", listener.local_addr().unwrap()))
        .configure_http(|builder| builder.no_proxy().http2_prior_knowledge())
        .build()
        .unwrap();
    let (caller, transport) = initialize(client);
    let fixture = async {
        let (socket, _) = listener.accept().await.unwrap();
        let mut connection = h2::server::handshake(socket).await.unwrap();
        let (request, mut respond) = connection.accept().await.unwrap().unwrap();
        assert_eq!(request.method(), "POST");
        assert_eq!(request.uri().path(), "/acp");
        let body = async {
            let mut stream = request.into_body();
            let mut body = Vec::new();
            while let Some(bytes) = stream.data().await {
                body.extend_from_slice(&bytes.unwrap());
                assert!(body.len() < 16 * 1024);
            }
            let message: serde_json::Value = serde_json::from_slice(&body).unwrap();
            assert_eq!(message["method"], "initialize");
            respond
                .send_response(
                    async_tungstenite::tungstenite::http::Response::builder()
                        .status(400)
                        .body(())
                        .unwrap(),
                    true,
                )
                .unwrap();
        };
        let drive = async {
            assert!(!matches!(connection.accept().await, Some(Ok(_))));
        };
        futures::join!(body, drive);
    };
    let (result, ()) = timeout(DEADLINE, async { futures::join!(transport, fixture) })
        .await
        .unwrap();
    assert!(result.is_err(), "dummy initialize response should fail");
    drop(caller);
}
