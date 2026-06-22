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
    use axum::body::Body;
    use tower::{Layer as _, ServiceExt as _, service_fn};

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
