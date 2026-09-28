use std::sync::Arc;

use agent_client_protocol::{Client, ConnectTo};
use axum::{
    Router,
    extract::WebSocketUpgrade,
    extract::ws::rejection::WebSocketUpgradeRejection,
    http::{HeaderName, HeaderValue, Method, StatusCode, header, header::InvalidHeaderValue},
    response::{IntoResponse, Response},
    routing::{delete, get, post},
};
use tower_http::cors::{AllowOrigin, CorsLayer};

use crate::connection::ConnectionRegistry;

#[derive(Debug, Clone)]
pub struct ServerOptions {
    pub path: String,
    pub cors: CorsOptions,
    pub health_endpoint: bool,
}

impl Default for ServerOptions {
    fn default() -> Self {
        Self {
            path: "/acp".to_string(),
            cors: CorsOptions::default(),
            health_endpoint: true,
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum CorsOptions {
    #[default]
    Disabled,
    AllowOrigins(Vec<HeaderValue>),
    AllowAnyOrigin,
}

impl CorsOptions {
    #[must_use]
    pub fn disabled() -> Self {
        Self::Disabled
    }

    #[must_use]
    pub fn allow_any_origin() -> Self {
        Self::AllowAnyOrigin
    }

    pub fn allow_origins<I, S>(origins: I) -> Result<Self, InvalidHeaderValue>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        origins
            .into_iter()
            .map(|origin| HeaderValue::from_str(origin.as_ref()))
            .collect::<Result<Vec<_>, _>>()
            .map(Self::AllowOrigins)
    }

    fn allow_origin_layer(&self) -> Option<AllowOrigin> {
        match self {
            Self::Disabled => None,
            Self::AllowOrigins(origins) => Some(AllowOrigin::list(origins.clone())),
            Self::AllowAnyOrigin => Some(AllowOrigin::any()),
        }
    }

    fn allows_origin(&self, origin: Option<&HeaderValue>) -> bool {
        let Some(origin) = origin else {
            return true;
        };
        match self {
            Self::Disabled => false,
            Self::AllowOrigins(origins) => origins.iter().any(|allowed| allowed == origin),
            Self::AllowAnyOrigin => true,
        }
    }
}

#[derive(Clone)]
struct ServerState {
    registry: Arc<ConnectionRegistry>,
    cors: CorsOptions,
}

pub struct AcpHttpServer {
    registry: Arc<ConnectionRegistry>,
    options: ServerOptions,
}

impl std::fmt::Debug for AcpHttpServer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AcpHttpServer")
            .field("options", &self.options)
            .finish_non_exhaustive()
    }
}

impl AcpHttpServer {
    pub fn new<F, C>(factory: F) -> Self
    where
        F: Fn() -> C + Send + Sync + 'static,
        C: ConnectTo<Client>,
    {
        Self {
            registry: Arc::new(ConnectionRegistry::new(Arc::new(factory))),
            options: ServerOptions::default(),
        }
    }

    #[must_use]
    pub fn with_options(mut self, options: ServerOptions) -> Self {
        self.options = options;
        self
    }

    pub fn into_router(self) -> Router {
        let registry = self.registry.clone();
        let path = self.options.path.clone();
        let cors = self.options.cors.clone();
        let state = ServerState {
            registry: registry.clone(),
            cors: cors.clone(),
        };

        let mut router = Router::new()
            .route(
                &path,
                post(crate::http_server::handle_post).with_state(registry.clone()),
            )
            .route(&path, get(handle_get).with_state(state))
            .route(
                &path,
                delete(crate::http_server::handle_delete).with_state(registry),
            );

        if self.options.health_endpoint {
            router = router.route("/health", get(health));
        }

        if let Some(allow_origin) = cors.allow_origin_layer() {
            router = router.layer(default_cors(allow_origin));
        }

        router
    }
}

async fn health() -> &'static str {
    "ok"
}

fn default_cors(allow_origin: AllowOrigin) -> CorsLayer {
    CorsLayer::new()
        .allow_origin(allow_origin)
        .allow_methods([Method::GET, Method::POST, Method::DELETE, Method::OPTIONS])
        .allow_headers([
            header::CONTENT_TYPE,
            header::ACCEPT,
            HeaderName::from_static("acp-connection-id"),
            HeaderName::from_static("acp-session-id"),
            header::SEC_WEBSOCKET_VERSION,
            header::SEC_WEBSOCKET_KEY,
            header::CONNECTION,
            header::UPGRADE,
        ])
        .expose_headers([
            HeaderName::from_static("acp-connection-id"),
            HeaderName::from_static("acp-session-id"),
        ])
}

async fn handle_get(
    ws_upgrade: Result<WebSocketUpgrade, WebSocketUpgradeRejection>,
    axum::extract::State(state): axum::extract::State<ServerState>,
    request: axum::http::Request<axum::body::Body>,
) -> Response {
    match ws_upgrade {
        Ok(ws) => {
            if !state
                .cors
                .allows_origin(request.headers().get(header::ORIGIN))
            {
                return (StatusCode::FORBIDDEN, "WebSocket origin not allowed").into_response();
            }
            crate::websocket_server::handle_ws_upgrade(state.registry, ws)
        }
        Err(_) => crate::http_server::handle_get(state.registry, request).await,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_client_protocol::{
        Channel, ConnectTo, RawJsonRpcMessage, TransportBatch, TransportFrame,
        schema::v1::RequestId,
    };
    use axum::body::Body;
    use futures::{StreamExt, future::BoxFuture};
    use serde_json::json;
    use tokio::{
        net::TcpListener,
        time::{Duration, timeout},
    };
    use tower::{Layer as _, ServiceExt as _, service_fn};

    struct HistoryAgent;

    impl crate::connection::AgentFactory for HistoryAgent {
        fn spawn_agent(
            &self,
        ) -> (
            Channel,
            BoxFuture<'static, agent_client_protocol::Result<()>>,
        ) {
            let (mut agent, transport) = Channel::duplex();
            let run = Box::pin(async move {
                while let Some(frame) = agent.rx.next().await {
                    let messages = match frame.into_frame() {
                        TransportFrame::Single(message) => vec![message],
                        TransportFrame::Batch(batch) => batch
                            .entries()
                            .filter_map(|entry| match entry {
                                agent_client_protocol::TransportBatchEntry::Message(message) => {
                                    Some(message.clone())
                                }
                                agent_client_protocol::TransportBatchEntry::Malformed {
                                    ..
                                } => None,
                            })
                            .collect(),
                        TransportFrame::Malformed { .. } => continue,
                    };
                    for message in messages {
                        let RawJsonRpcMessage::Request(request) = message else {
                            continue;
                        };
                        if request.method.as_ref() != "initialize" {
                            let Some(agent_client_protocol::RawJsonRpcParams::Object(params)) =
                                request.params.as_ref()
                            else {
                                panic!("session request must have object params");
                            };
                            for index in 0..2 {
                                agent
                                    .tx
                                    .send_frame(TransportFrame::Single(
                                        RawJsonRpcMessage::notification(
                                            "session/update".into(),
                                            json!({"sessionId": params["sessionId"], "index": index}),
                                        )
                                        .unwrap(),
                                    ))
                                    .await
                                    .unwrap();
                            }
                        }
                        agent
                            .tx
                            .send_frame(TransportFrame::Single(RawJsonRpcMessage::response(
                                request.id,
                                Ok(json!({})),
                            )))
                            .await
                            .unwrap();
                    }
                }
                Ok(())
            });
            (transport, run)
        }
    }

    #[tokio::test]
    async fn cold_session_post_registers_stream_before_history_for_single_and_batch() {
        let registry = Arc::new(ConnectionRegistry::new(Arc::new(HistoryAgent)));
        let app = AcpHttpServer {
            registry: registry.clone(),
            options: ServerOptions::default(),
        }
        .into_router();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        for (index, method) in ["session/load", "session/resume"].into_iter().enumerate() {
            let client = crate::client::HttpClient::new(format!("http://{address}")).unwrap();
            let (mut caller, driver) = client.into_channel_and_future();
            let driver = tokio::spawn(driver);
            caller
                .tx
                .send_frame(TransportFrame::Single(
                    RawJsonRpcMessage::request(
                        "initialize".into(),
                        json!({}),
                        RequestId::Number(1),
                    )
                    .unwrap(),
                ))
                .await
                .unwrap();
            let init = timeout(Duration::from_secs(2), caller.rx.next())
                .await
                .unwrap()
                .unwrap();
            assert!(matches!(
                init.frame(),
                TransportFrame::Single(RawJsonRpcMessage::Response(_))
            ));
            drop(init);
            let request = RawJsonRpcMessage::request(
                method.into(),
                json!({"sessionId": "persisted"}),
                RequestId::Number(2),
            )
            .unwrap();
            let frame = if index == 0 {
                TransportFrame::Single(request)
            } else {
                let second = RawJsonRpcMessage::request(
                    method.into(),
                    json!({"sessionId": "other-persisted"}),
                    RequestId::Number(3),
                )
                .unwrap();
                TransportFrame::Batch(TransportBatch::from_messages([request, second]).unwrap())
            };
            caller.tx.send_frame(frame).await.unwrap();
            let mut seen = std::collections::BTreeMap::<String, Vec<u64>>::new();
            let session_count = index + 1;
            let mut responses = 0;
            for _ in 0..session_count * 3 {
                let frame = timeout(Duration::from_secs(3), caller.rx.next())
                    .await
                    .unwrap()
                    .unwrap();
                match frame.frame() {
                    TransportFrame::Single(RawJsonRpcMessage::Notification(notification)) => {
                        let agent_client_protocol::RawJsonRpcParams::Object(params) =
                            notification.params.as_ref().unwrap()
                        else {
                            panic!("history update must have object params");
                        };
                        seen.entry(params["sessionId"].as_str().unwrap().to_owned())
                            .or_default()
                            .push(params["index"].as_u64().unwrap());
                    }
                    TransportFrame::Single(RawJsonRpcMessage::Response(_)) => {
                        let response: serde_json::Value =
                            serde_json::from_str(&frame.frame().to_json().unwrap()).unwrap();
                        let session = match response["id"].as_u64().unwrap() {
                            2 => "persisted",
                            3 => "other-persisted",
                            id => panic!("unexpected response ID: {id}"),
                        };
                        assert_eq!(
                            seen.get(session).map(Vec::as_slice),
                            Some([0, 1].as_slice()),
                            "{method} must deliver history before its response"
                        );
                        responses += 1;
                    }
                    other => panic!("unexpected history frame: {other:?}"),
                }
            }
            assert_eq!(seen.len(), session_count);
            assert_eq!(responses, session_count);
            drop(caller);
            timeout(Duration::from_secs(3), driver)
                .await
                .unwrap()
                .unwrap()
                .unwrap();
        }
        assert_eq!(registry.len().await, 0);
        server.abort();
    }

    #[test]
    fn cors_is_disabled_by_default() {
        assert_eq!(ServerOptions::default().cors, CorsOptions::Disabled);
    }

    #[test]
    fn disabled_cors_rejects_browser_origin_for_websockets() {
        let origin = HeaderValue::from_static("http://localhost:5173");

        assert!(CorsOptions::disabled().allows_origin(None));
        assert!(!CorsOptions::disabled().allows_origin(Some(&origin)));
    }

    #[test]
    fn cors_allowlist_matches_configured_origins() {
        let allowed = HeaderValue::from_static("http://localhost:5173");
        let denied = HeaderValue::from_static("http://localhost:3000");
        let cors = CorsOptions::allow_origins(["http://localhost:5173"]).unwrap();

        assert!(cors.allows_origin(None));
        assert!(cors.allows_origin(Some(&allowed)));
        assert!(!cors.allows_origin(Some(&denied)));
    }

    #[test]
    fn explicit_allow_any_origin_accepts_browser_origins() {
        let origin = HeaderValue::from_static("https://example.com");

        assert!(CorsOptions::allow_any_origin().allows_origin(Some(&origin)));
    }

    #[tokio::test]
    async fn allow_any_origin_uses_wildcard_cors_header() {
        let response = default_cors(
            CorsOptions::allow_any_origin()
                .allow_origin_layer()
                .expect("CORS layer"),
        )
        .layer(service_fn(|_: axum::http::Request<Body>| async {
            Ok::<_, std::convert::Infallible>(Response::new(Body::empty()))
        }))
        .oneshot(
            axum::http::Request::builder()
                .header(header::ORIGIN, "https://example.com")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

        assert_eq!(
            response.headers().get(header::ACCESS_CONTROL_ALLOW_ORIGIN),
            Some(&HeaderValue::from_static("*"))
        );
        assert!(response.headers().get(header::VARY).is_none());
    }

    #[tokio::test]
    async fn allowlisted_origins_vary_by_origin() {
        let response = default_cors(
            CorsOptions::allow_origins(["https://example.com"])
                .unwrap()
                .allow_origin_layer()
                .expect("CORS layer"),
        )
        .layer(service_fn(|_: axum::http::Request<Body>| async {
            Ok::<_, std::convert::Infallible>(Response::new(Body::empty()))
        }))
        .oneshot(
            axum::http::Request::builder()
                .header(header::ORIGIN, "https://example.com")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

        assert_eq!(
            response.headers().get(header::ACCESS_CONTROL_ALLOW_ORIGIN),
            Some(&HeaderValue::from_static("https://example.com"))
        );
        assert_eq!(
            response.headers().get(header::VARY),
            Some(&HeaderValue::from_static("origin"))
        );
    }
}
