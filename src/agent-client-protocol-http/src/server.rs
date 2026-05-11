use std::sync::Arc;

use agent_client_protocol::{Client, ConnectTo};
use axum::{
    Router,
    extract::WebSocketUpgrade,
    extract::ws::rejection::WebSocketUpgradeRejection,
    http::{HeaderName, Method, header},
    response::Response,
    routing::{delete, get, post},
};
use tower_http::cors::{Any, CorsLayer};

use crate::connection::ConnectionRegistry;

#[derive(Debug, Clone)]
pub struct ServerOptions {
    pub path: String,
    pub permissive_cors: bool,
    pub health_endpoint: bool,
}

impl Default for ServerOptions {
    fn default() -> Self {
        Self {
            path: "/acp".to_string(),
            permissive_cors: true,
            health_endpoint: true,
        }
    }
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

        let mut router = Router::new()
            .route(
                &path,
                post(crate::http_server::handle_post).with_state(registry.clone()),
            )
            .route(&path, get(handle_get).with_state(registry.clone()))
            .route(
                &path,
                delete(crate::http_server::handle_delete).with_state(registry),
            );

        if self.options.health_endpoint {
            router = router.route("/health", get(health));
        }

        if self.options.permissive_cors {
            router = router.layer(default_cors());
        }

        router
    }
}

async fn health() -> &'static str {
    "ok"
}

fn default_cors() -> CorsLayer {
    CorsLayer::new()
        .allow_origin(Any)
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
    axum::extract::State(registry): axum::extract::State<Arc<ConnectionRegistry>>,
    request: axum::http::Request<axum::body::Body>,
) -> Response {
    match ws_upgrade {
        Ok(ws) => crate::websocket_server::handle_ws_upgrade(registry, ws).await,
        Err(_) => crate::http_server::handle_get(registry, request).await,
    }
}
