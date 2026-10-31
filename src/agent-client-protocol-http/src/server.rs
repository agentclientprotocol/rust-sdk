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

/// Configuration for an [`AcpHttpServer`].
///
/// Start with [`Self::default`] and use the fluent setters to customize it.
/// Fields remain public for reading and mutation, but this type is non-exhaustive
/// so new options can be added without breaking callers.
///
/// ```
/// use agent_client_protocol_http::{CorsOptions, ServerOptions};
///
/// let options = ServerOptions::default()
///     .with_path("/agent")
///     .with_cors(CorsOptions::allow_origins(["https://example.com"])?)
///     .with_health_endpoint(false);
/// assert_eq!(options.path, "/agent");
/// assert!(!options.health_endpoint);
/// # Ok::<(), axum::http::header::InvalidHeaderValue>(())
/// ```
///
/// Struct literals, including struct update syntax, are not supported outside
/// this crate:
///
/// ```compile_fail
/// use agent_client_protocol_http::ServerOptions;
///
/// let options = ServerOptions {
///     path: "/agent".into(),
///     ..ServerOptions::default()
/// };
/// ```
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct ServerOptions {
    /// ACP endpoint path. Defaults to `/acp`.
    pub path: String,
    /// Browser origin policy. Defaults to [`CorsOptions::disabled`].
    pub cors: CorsOptions,
    /// Whether to expose `GET /health`. Defaults to `true`.
    pub health_endpoint: bool,
}

impl ServerOptions {
    /// Set the ACP endpoint path.
    #[must_use]
    pub fn with_path(mut self, path: impl Into<String>) -> Self {
        self.path = path.into();
        self
    }

    /// Set the browser origin policy.
    ///
    /// CORS does not authenticate requests. The built-in CORS layer does not
    /// allow the `Authorization` request header or enable credentialed CORS.
    /// Authentication and custom credential policies belong to the host router.
    #[must_use]
    pub fn with_cors(mut self, cors: CorsOptions) -> Self {
        self.cors = cors;
        self
    }

    /// Enable or disable the `GET /health` endpoint.
    #[must_use]
    pub fn with_health_endpoint(mut self, enabled: bool) -> Self {
        self.health_endpoint = enabled;
        self
    }
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

/// Browser origin policy for HTTP CORS and WebSocket upgrades.
///
/// This is not authentication: requests without an `Origin` header are accepted
/// by the origin check. Enforce authentication in the host router.
///
/// Build a policy with [`Self::disabled`] (the default),
/// [`Self::allow_origins`], or [`Self::allow_any_origin`]. Policies compare
/// with `==`:
///
/// ```
/// use agent_client_protocol_http::CorsOptions;
///
/// assert_eq!(CorsOptions::default(), CorsOptions::disabled());
/// assert_ne!(CorsOptions::allow_any_origin(), CorsOptions::disabled());
/// ```
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CorsOptions {
    policy: CorsPolicy,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
enum CorsPolicy {
    #[default]
    Disabled,
    AllowOrigins(Vec<HeaderValue>),
    AllowAnyOrigin,
}

impl CorsOptions {
    /// Disable cross-origin HTTP access and reject WebSocket browser origins.
    #[must_use]
    pub fn disabled() -> Self {
        Self {
            policy: CorsPolicy::Disabled,
        }
    }

    /// Allow any browser origin, without enabling credentialed CORS.
    #[must_use]
    pub fn allow_any_origin() -> Self {
        Self {
            policy: CorsPolicy::AllowAnyOrigin,
        }
    }

    /// Allow only the given browser origins.
    ///
    /// # Errors
    ///
    /// Returns [`InvalidHeaderValue`] if an origin is not a valid HTTP header
    /// value.
    pub fn allow_origins<I, S>(origins: I) -> Result<Self, InvalidHeaderValue>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        let origins = origins
            .into_iter()
            .map(|origin| HeaderValue::from_str(origin.as_ref()))
            .collect::<Result<Vec<_>, _>>()?;
        Ok(Self {
            policy: CorsPolicy::AllowOrigins(origins),
        })
    }

    fn allow_origin_layer(&self) -> Option<AllowOrigin> {
        match &self.policy {
            CorsPolicy::Disabled => None,
            CorsPolicy::AllowOrigins(origins) => Some(AllowOrigin::list(origins.clone())),
            CorsPolicy::AllowAnyOrigin => Some(AllowOrigin::any()),
        }
    }

    fn allows_origin(&self, origin: Option<&HeaderValue>) -> bool {
        let Some(origin) = origin else {
            return true;
        };
        match &self.policy {
            CorsPolicy::Disabled => false,
            CorsPolicy::AllowOrigins(origins) => origins.iter().any(|allowed| allowed == origin),
            CorsPolicy::AllowAnyOrigin => true,
        }
    }
}

#[derive(Clone)]
struct ServerState {
    registry: Arc<ConnectionRegistry>,
    cors: CorsOptions,
}

/// Serves ACP agents over HTTP/SSE and WebSocket as an [`axum::Router`].
///
/// [`Self::new`] takes a factory that builds one agent component per
/// connection. Each `initialize` POST or WebSocket upgrade calls it once, and
/// the component runs until the client sends `DELETE` or disconnects.
///
/// The transport forwards JSON-RPC messages unchanged, so the component decides
/// which protocol versions it speaks. To serve ACP v1 and v2 on one endpoint,
/// return an `AgentProtocolRouter`
/// from the factory (requires the core `unstable_protocol_v2` feature):
///
/// ```no_run
/// use agent_client_protocol::schema::{v1, v2};
/// use agent_client_protocol::{Agent, Client, ConnectTo, on_receive_request};
/// use agent_client_protocol_http::AcpHttpServer;
///
/// fn v1_agent() -> impl ConnectTo<Client> {
///     Agent.v1().on_receive_request(
///         async |init: v1::InitializeRequest, responder, _cx| {
///             responder.respond(v1::InitializeResponse::new(init.protocol_version))
///         },
///         on_receive_request!(),
///     )
/// }
///
/// fn v2_agent() -> impl ConnectTo<Client> {
///     Agent.v2().on_receive_request(
///         async |init: v2::InitializeRequest, responder, _cx| {
///             responder.respond(v2::InitializeResponse::new(
///                 init.protocol_version,
///                 v2::Implementation::new("example", "0.1.0"),
///             ))
///         },
///         on_receive_request!(),
///     )
/// }
///
/// # async fn serve() -> Result<(), Box<dyn std::error::Error>> {
/// let app = AcpHttpServer::new(|| {
///     Agent
///         .protocol_router()
///         .with_v1(v1_agent())
///         .with_v2(v2_agent())
/// })
/// .into_router();
///
/// let listener = tokio::net::TcpListener::bind("127.0.0.1:8080").await?;
/// axum::serve(listener, app).await?;
/// # Ok(())
/// # }
/// ```
///
/// Use [`Self::with_options`] to change the endpoint path, the browser origin
/// policy, or the `/health` route.
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
    /// Create a server that calls `factory` once per ACP connection.
    ///
    /// The server starts with [`ServerOptions::default`].
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

    /// Replace the server options.
    #[must_use]
    pub fn with_options(mut self, options: ServerOptions) -> Self {
        self.options = options;
        self
    }

    /// Build the axum router.
    ///
    /// The router serves `POST`, `GET`, and `DELETE` on
    /// [`ServerOptions::path`] and, unless disabled, `GET /health`. Nest or
    /// merge it into a larger router to add authentication or other routes.
    ///
    /// # Panics
    ///
    /// Panics if [`ServerOptions::path`] is empty or does not start with `/`,
    /// or if it is `/health` while [`ServerOptions::health_endpoint`] is
    /// enabled. These are the conditions under which [`Router::route`] panics.
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

    fn build_router(options: ServerOptions) {
        drop(
            AcpHttpServer::new(|| agent_client_protocol::Agent.v1())
                .with_options(options)
                .into_router(),
        );
    }

    #[test]
    #[should_panic(expected = "Paths must start with a `/`")]
    fn into_router_panics_on_a_path_without_a_leading_slash() {
        build_router(ServerOptions::default().with_path("acp"));
    }

    #[test]
    #[should_panic(expected = "Paths must start with a `/`")]
    fn into_router_panics_on_an_empty_path() {
        build_router(ServerOptions::default().with_path(""));
    }

    #[test]
    #[should_panic(expected = "Overlapping method route")]
    fn into_router_panics_when_the_path_is_the_health_route() {
        build_router(ServerOptions::default().with_path("/health"));
    }

    #[test]
    fn into_router_accepts_the_health_path_without_the_health_route() {
        build_router(
            ServerOptions::default()
                .with_path("/health")
                .with_health_endpoint(false),
        );
    }

    #[test]
    fn cors_is_disabled_by_default() {
        assert_eq!(ServerOptions::default().cors, CorsOptions::disabled());
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
