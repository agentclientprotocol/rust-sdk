//! Agent Client Protocol Trace Viewer Library
//!
//! Provides an interactive sequence diagram viewer for ACP trace events.
//! Can serve events from memory (for live viewing) or from a file.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use axum::{
    Router,
    extract::State,
    http::StatusCode,
    response::{Html, IntoResponse, Response},
    routing::get,
};
use tokio::net::TcpListener;

/// The HTML viewer page (embedded at compile time).
pub const VIEWER_HTML: &str = include_str!("viewer.html");

/// Source of trace events for the viewer.
#[derive(Debug, Clone)]
pub enum TraceSource {
    /// Read events from a file (re-reads on each request for live updates).
    File(PathBuf),
    /// Read events from shared memory.
    Memory(Arc<Mutex<Vec<serde_json::Value>>>),
}

/// Handle to push events when using memory-backed trace source.
#[derive(Debug, Clone)]
pub struct TraceHandle {
    events: Arc<Mutex<Vec<serde_json::Value>>>,
}

impl TraceHandle {
    /// Push a new event to the trace.
    pub fn push(&self, event: serde_json::Value) {
        self.events
            .lock()
            .expect("events mutex poisoned")
            .push(event);
    }

    /// Get the current number of events.
    #[must_use]
    pub fn len(&self) -> usize {
        self.events.lock().expect("events mutex poisoned").len()
    }

    /// Check if empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.events
            .lock()
            .expect("events mutex poisoned")
            .is_empty()
    }
}

struct AppState {
    source: TraceSource,
}

/// Configuration for the trace viewer server.
#[derive(Debug)]
pub struct TraceViewerConfig {
    /// Port to serve on (0 = auto-select).
    pub port: u16,
    /// Whether to open browser automatically.
    pub open_browser: bool,
}

impl Default for TraceViewerConfig {
    fn default() -> Self {
        Self {
            port: 0,
            open_browser: true,
        }
    }
}

/// Start the trace viewer server with a memory-backed event source.
///
/// Returns a handle to push events and a future that runs the server.
/// The server will poll for new events automatically.
///
/// # Example
///
/// ```no_run
/// # async fn example() -> anyhow::Result<()> {
/// let (handle, server) = agent_client_protocol_trace_viewer::serve_memory(Default::default())?;
///
/// // Push events from your application
/// handle.push(serde_json::json!({"type": "request", "method": "test"}));
///
/// // Run the server (or spawn it)
/// server.await?;
/// # Ok(())
/// # }
/// ```
pub fn serve_memory(
    config: TraceViewerConfig,
) -> anyhow::Result<(
    TraceHandle,
    impl std::future::Future<Output = anyhow::Result<()>>,
)> {
    let events = Arc::new(Mutex::new(Vec::new()));
    let handle = TraceHandle {
        events: events.clone(),
    };
    let source = TraceSource::Memory(events);

    let server = serve_impl(source, config);
    Ok((handle, server))
}

/// Start the trace viewer server with a file-backed event source.
///
/// The file is re-read on each request, allowing live updates as the file grows.
///
/// # Example
///
/// ```no_run
/// # use std::path::PathBuf;
/// # async fn example() -> anyhow::Result<()> {
/// agent_client_protocol_trace_viewer::serve_file(PathBuf::from("trace.jsons"), Default::default()).await?;
/// # Ok(())
/// # }
/// ```
pub async fn serve_file(path: PathBuf, config: TraceViewerConfig) -> anyhow::Result<()> {
    serve_impl(TraceSource::File(path), config).await
}

async fn serve_impl(source: TraceSource, config: TraceViewerConfig) -> anyhow::Result<()> {
    let state = Arc::new(AppState { source });

    let app = Router::new()
        .route("/", get(serve_viewer))
        .route("/events", get(serve_events))
        .with_state(state);

    let listener = TcpListener::bind(format!("127.0.0.1:{}", config.port)).await?;
    let addr = listener.local_addr()?;

    eprintln!("Trace viewer at http://{addr}");

    if config.open_browser {
        let url = format!("http://{addr}");
        if let Err(e) = open::that(&url) {
            eprintln!("Failed to open browser: {e}. Open {url} manually.");
        }
    }

    axum::serve(listener, app).await?;
    Ok(())
}

/// Serve the main viewer HTML page.
async fn serve_viewer() -> Html<&'static str> {
    Html(VIEWER_HTML)
}

/// Serve the trace events as JSON.
async fn serve_events(State(state): State<Arc<AppState>>) -> Response {
    match &state.source {
        TraceSource::File(path) => serve_events_from_file(path).await,
        TraceSource::Memory(events) => serve_events_from_memory(events),
    }
}

async fn serve_events_from_file(path: &PathBuf) -> Response {
    match tokio::fs::read_to_string(path).await {
        Ok(content) => {
            let events: Vec<serde_json::Value> = content
                .lines()
                .filter(|line| !line.trim().is_empty())
                .filter_map(|line| serde_json::from_str(line).ok())
                .collect();

            match serde_json::to_string(&events) {
                Ok(json) => {
                    (StatusCode::OK, [("content-type", "application/json")], json).into_response()
                }
                Err(e) => (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    format!("Failed to serialize events: {e}"),
                )
                    .into_response(),
            }
        }
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("Failed to read trace file: {e}"),
        )
            .into_response(),
    }
}

fn serve_events_from_memory(events: &Arc<Mutex<Vec<serde_json::Value>>>) -> Response {
    let events = events.lock().expect("events mutex poisoned");
    match serde_json::to_string(&*events) {
        Ok(json) => (StatusCode::OK, [("content-type", "application/json")], json).into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("Failed to serialize events: {e}"),
        )
            .into_response(),
    }
}
