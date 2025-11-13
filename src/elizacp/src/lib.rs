pub mod eliza;

use anyhow::Result;
use eliza::Eliza;
use sacp::schema::{
    AgentCapabilities, ContentBlock, ContentChunk, InitializeRequest, InitializeResponse,
    LoadSessionRequest, LoadSessionResponse, NewSessionRequest, NewSessionResponse, PromptRequest,
    PromptResponse, SessionId, SessionNotification, SessionUpdate, StopReason, TextContent,
};
use sacp::{Component, JrHandlerChain};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

/// Shared state across all sessions
#[derive(Clone)]
struct ElizaAgent {
    sessions: Arc<Mutex<HashMap<SessionId, Eliza>>>,
}

impl ElizaAgent {
    fn new() -> Self {
        Self {
            sessions: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    fn create_session(&self, session_id: &SessionId) {
        let mut sessions = self.sessions.lock().unwrap();
        sessions.insert(session_id.clone(), Eliza::new());
        tracing::info!("Created session: {}", session_id);
    }

    fn get_response(&self, session_id: &SessionId, input: &str) -> Option<String> {
        let mut sessions = self.sessions.lock().unwrap();
        sessions
            .get_mut(session_id)
            .map(|eliza| eliza.respond(input))
    }

    fn _end_session(&self, session_id: &SessionId) {
        let mut sessions = self.sessions.lock().unwrap();
        sessions.remove(session_id);
        tracing::info!("Ended session: {}", session_id);
    }

    async fn handle_new_session(
        &self,
        request: NewSessionRequest,
        request_cx: sacp::JrRequestCx<NewSessionResponse>,
    ) -> Result<(), sacp::Error> {
        tracing::debug!("New session request with cwd: {:?}", request.cwd);

        // Generate a new session ID
        let session_id = SessionId(Arc::from(uuid::Uuid::new_v4().to_string()));
        self.create_session(&session_id);

        let response = NewSessionResponse {
            session_id,
            modes: None,
            meta: None,
        };

        request_cx.respond(response)
    }

    async fn handle_load_session(
        &self,
        request: LoadSessionRequest,
        request_cx: sacp::JrRequestCx<LoadSessionResponse>,
    ) -> Result<(), sacp::Error> {
        tracing::debug!("Load session request: {:?}", request.session_id);

        // For Eliza, we just create a fresh session
        self.create_session(&request.session_id);

        let response = LoadSessionResponse {
            modes: None,
            meta: None,
        };

        request_cx.respond(response)
    }

    async fn handle_prompt_request(
        &self,
        request: PromptRequest,
        request_cx: sacp::JrRequestCx<PromptResponse>,
    ) -> Result<(), sacp::Error> {
        let session_id = &request.session_id;

        tracing::debug!(
            "Received prompt in session {}: {} content blocks",
            session_id,
            request.prompt.len()
        );

        // Extract text from the prompt
        let input_text = extract_text_from_prompt(&request.prompt);

        // Get Eliza's response
        let response_text = self
            .get_response(session_id, &input_text)
            .unwrap_or_else(|| {
                format!(
                    "Error: Session {} not found. Please start a new session.",
                    session_id
                )
            });

        tracing::debug!("Eliza response: {}", response_text);

        request_cx
            .connection_cx()
            .send_notification(SessionNotification {
                session_id: session_id.clone(),
                update: SessionUpdate::AgentMessageChunk(ContentChunk {
                    content: response_text.into(),
                    meta: None,
                }),
                meta: None,
            })?;

        // Complete the request
        request_cx.respond(PromptResponse {
            stop_reason: StopReason::EndTurn,
            meta: None,
        })
    }
}

/// Extract text content from prompt blocks
fn extract_text_from_prompt(blocks: &[ContentBlock]) -> String {
    blocks
        .iter()
        .filter_map(|block| match block {
            ContentBlock::Text(TextContent { text, .. }) => Some(text.clone()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// Run the Eliza ACP agent with the given input/output streams.
///
/// This is the core agent implementation that can be used both from the binary
/// and from tests as an in-process mock component.
pub async fn run_elizacp(transport: impl Component + 'static) -> Result<(), sacp::Error> {
    let agent = ElizaAgent::new();

    JrHandlerChain::new()
        .name("elizacp")
        .on_receive_request({
            async |initialize: InitializeRequest, request_cx| {
                tracing::debug!("Received initialize request");

                request_cx.respond(InitializeResponse {
                    protocol_version: initialize.protocol_version,
                    agent_capabilities: AgentCapabilities {
                        load_session: Default::default(),
                        prompt_capabilities: Default::default(),
                        mcp_capabilities: Default::default(),
                        meta: Default::default(),
                    },
                    auth_methods: Default::default(),
                    agent_info: Default::default(),
                    meta: Default::default(),
                })
            }
        })
        .on_receive_request({
            let agent = agent.clone();
            async move |request: NewSessionRequest, request_cx| {
                agent.handle_new_session(request, request_cx).await
            }
        })
        .on_receive_request({
            let agent = agent.clone();
            async move |request: LoadSessionRequest, request_cx| {
                agent.handle_load_session(request, request_cx).await
            }
        })
        .on_receive_request({
            let agent = agent.clone();
            async move |request: PromptRequest, request_cx| {
                agent.handle_prompt_request(request, request_cx).await
            }
        })
        .connect_to(transport)?
        .serve()
        .await?;

    Ok(())
}
