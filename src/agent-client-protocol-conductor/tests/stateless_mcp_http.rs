//! Real rmcp client -> HTTP polyfill -> ACP/conductor -> rmcp server.

use std::{
    future::Future,
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

use agent_client_protocol::{
    Agent, Client, Error,
    mcp_server::McpServer,
    schema::{
        ProtocolVersion,
        v1::{
            AgentCapabilities, InitializeRequest, InitializeResponse, McpCapabilities,
            McpServer as AcpMcpServer, NewSessionRequest, NewSessionResponse, SessionCapabilities,
        },
    },
};
use agent_client_protocol_conductor::{ConductorImpl, ProxiesAndAgent};
use agent_client_protocol_polyfill::mcp_over_acp::McpOverAcpPolyfill;
use agent_client_protocol_rmcp::McpServerExt as _;
use rmcp::{
    ErrorData, RoleServer, ServerHandler,
    model::{
        CallToolRequestParams, CallToolResponse, CallToolResult, ClientCapabilities, ClientConfig,
        Implementation, InputRequiredResult, ProtocolVersion as McpVersion, ServerCapabilities,
        ServerConfig, SubscriptionFilter, Tool, ToolAnnotations,
    },
    service::{ClientLifecycleMode, ClientServiceExt, RequestContext, SubscriptionContext},
    transport::{
        StreamableHttpClientTransport, streamable_http_client::StreamableHttpClientTransportConfig,
    },
};
use serde_json::{Value, json};
use tokio::sync::mpsc;

const TIMEOUT: Duration = Duration::from_secs(15);
const STATE: &str = "opaque/http/retry?keep=exact";

struct RealService {
    listening: mpsc::UnboundedSender<()>,
    stopped: mpsc::UnboundedSender<()>,
    lists: Arc<AtomicUsize>,
}

struct NotifyStopped(mpsc::UnboundedSender<()>);

impl Drop for NotifyStopped {
    fn drop(&mut self) {
        let _ = self.0.send(());
    }
}

impl ServerHandler for RealService {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(
            ServerCapabilities::builder()
                .enable_tools()
                .enable_tool_list_changed()
                .build(),
        )
    }

    fn list_tools(
        &self,
        _request: Option<rmcp::model::PaginatedRequestParams>,
        _cx: RequestContext<RoleServer>,
    ) -> impl Future<Output = Result<rmcp::model::ListToolsResult, ErrorData>> + Send {
        self.lists.fetch_add(1, Ordering::SeqCst);
        let schema = json!({"type": "object"}).as_object().unwrap().clone();
        let annotated_schema = json!({
            "type": "object",
            "properties": {"region": {"type": "string", "x-mcp-header": "Region"}}
        })
        .as_object()
        .unwrap()
        .clone();
        std::future::ready(Ok(rmcp::model::ListToolsResult::with_all_items(vec![
            Tool::new("retry", "MRTR round trip", schema.clone()),
            Tool::new("annotated", "Direct call", annotated_schema).with_annotations(
                ToolAnnotations::from_raw(Some("Annotated".into()), Some(true), None, None, None),
            ),
        ])))
    }

    fn call_tool(
        &self,
        request: CallToolRequestParams,
        cx: RequestContext<RoleServer>,
    ) -> impl Future<Output = Result<CallToolResponse, ErrorData>> + Send {
        std::future::ready(
            match (request.name.as_ref(), request.request_state.as_deref()) {
                ("retry", None) => {
                    let inputs = serde_json::from_value(json!({"confirmation": {
                        "method": "elicitation/create",
                        "params": {"mode": "form", "message": "Confirm",
                            "requestedSchema": {"type": "object",
                                "properties": {"approved": {"type": "boolean"}}}}
                    }}))
                    .expect("valid input request");
                    Ok(InputRequiredResult::new(Some(inputs), Some(STATE.into())).into())
                }
                ("retry", Some(STATE)) => Ok(CallToolResult::structured(json!({
                    "state": request.request_state.clone(),
                    "responses": request.input_responses,
                    "marker": cx.meta.get("example/marker"),
                }))
                .into()),
                ("annotated", None) => {
                    Ok(CallToolResult::structured(json!({"direct": true})).into())
                }
                _ => Err(ErrorData::invalid_params(
                    "unknown tool or retry state",
                    None,
                )),
            },
        )
    }

    fn accepted_subscription_filter(
        &self,
        requested: &SubscriptionFilter,
    ) -> Option<SubscriptionFilter> {
        Some(requested.clone())
    }

    async fn listen(&self, cx: SubscriptionContext) -> Result<(), ErrorData> {
        // The owned adapter may cancel by dropping this future before it polls
        // cx.cancelled() again. Observe actual cleanup, not a cooperative branch.
        let _stopped = NotifyStopped(self.stopped.clone());
        cx.sink()
            .notify_tool_list_changed()
            .await
            .map_err(|e| ErrorData::internal_error(e.to_string(), None))?;
        let _ = self.listening.send(());
        cx.cancelled().await;
        Ok(())
    }
}

fn marked_call(name: &str, marker: &str) -> CallToolRequestParams {
    let mut params = CallToolRequestParams::new(name.to_owned());
    params.meta = Some(
        serde_json::from_value(json!({
            "io.modelcontextprotocol/protocolVersion": "2026-07-28",
            "io.modelcontextprotocol/clientCapabilities": {"elicitation": {"form": {}}},
            "io.modelcontextprotocol/clientInfo": {"name": "http-integration", "version": "1"},
            "example/marker": marker,
        }))
        .expect("valid request metadata"),
    );
    params
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn real_rmcp_stateless_http_survives_subscription_cancellation() -> Result<(), Error> {
    tokio::time::timeout(TIMEOUT, async {
        let (endpoint_tx, mut endpoint_rx) = mpsc::unbounded_channel();
        let (listening_tx, mut listening_rx) = mpsc::unbounded_channel();
        let (stopped_tx, mut stopped_rx) = mpsc::unbounded_channel();
        let lists = Arc::new(AtomicUsize::new(0));
        let agent = Agent
            .builder()
            .on_receive_request(
                async |request: InitializeRequest, responder, _cx| {
                    responder.respond(
                        InitializeResponse::new(request.protocol_version).agent_capabilities(
                            AgentCapabilities::new()
                                .session_capabilities(SessionCapabilities::new())
                                .mcp_capabilities(McpCapabilities::new().http(true)),
                        ),
                    )
                },
                agent_client_protocol::on_receive_request!(),
            )
            .on_receive_request(
                async move |request: NewSessionRequest, responder, _cx| {
                    let [AcpMcpServer::Http(server)] = request.mcp_servers.as_slice() else {
                        panic!("expected a single HTTP MCP server declaration")
                    };
                    assert_eq!(server.name, "real-rmcp");
                    assert_eq!(server.headers.len(), 1);
                    assert_eq!(server.headers[0].name, "Authorization");
                    endpoint_tx
                        .send((server.url.clone(), server.headers[0].value.clone()))
                        .expect("client still waiting for HTTP declaration");
                    responder.respond(NewSessionResponse::new("real-http-session"))
                },
                agent_client_protocol::on_receive_request!(),
            );

        Client.builder().connect_with(
            ConductorImpl::new_agent(
                "http-bridge",
                ProxiesAndAgent::new(agent).proxy(McpOverAcpPolyfill::http()),
            ),
            async move |cx| {
                cx.send_request(InitializeRequest::new(ProtocolVersion::V1))
                    .block_task().await?;
                let service = Arc::new(RealService {
                    listening: listening_tx,
                    stopped: stopped_tx,
                    lists: lists.clone(),
                });
                cx.build_session(PathBuf::from("/tmp"))
                    .with_mcp_server(McpServer::<Agent>::from_rmcp(
                        "real-rmcp", move || service.clone(),
                    ))?
                    .block_task()
                    .run_until(async move |_session| {
                        let (url, bearer) = endpoint_rx.recv().await.expect("HTTP declaration");
                        let headers = [(
                            "Authorization".parse().expect("header name"),
                            bearer.parse().expect("header value"),
                        )].into_iter().collect();
                        let transport = StreamableHttpClientTransport::from_config(
                            StreamableHttpClientTransportConfig::with_uri(url)
                                .custom_headers(headers),
                        );
                        let config = ClientConfig::new(
                            serde_json::from_value::<ClientCapabilities>(
                                json!({"elicitation": {"form": {}}}),
                            ).expect("valid capabilities"),
                            Implementation::new("http-integration", "1"),
                        ).with_protocol_version(McpVersion::V_2026_07_28);
                        let client = config.serve_with_lifecycle(
                            transport,
                            ClientLifecycleMode::Discover {
                                preferred_versions: vec![McpVersion::V_2026_07_28],
                            },
                        ).await.map_err(Error::into_internal_error)?;

                        // A direct call before any list also tests absence of hidden lists.
                        let first = client.call_tool_once(marked_call("retry", "first"))
                            .await.map_err(Error::into_internal_error)?;
                        let CallToolResponse::InputRequired(first) = first else {
                            panic!("expected input_required, got {first:?}");
                        };
                        assert_eq!(lists.load(Ordering::SeqCst), 0, "no hidden tools/list");
                        assert_eq!(first.request_state.as_deref(), Some(STATE));
                        assert_eq!(
                            serde_json::to_value(&first.input_requests)
                                .map_err(Error::into_internal_error)?["confirmation"]["method"],
                            "elicitation/create"
                        );
                        let responses: Value =
                            json!({"confirmation": {"action": "accept", "content": {"approved": true}}});
                        let second = client.call_tool_once(
                            marked_call("retry", "second")
                                .with_request_state(first.request_state.expect("opaque state"))
                                .with_input_responses(serde_json::from_value(responses.clone())
                                    .map_err(Error::into_internal_error)?),
                        ).await.map_err(Error::into_internal_error)?;
                        let CallToolResponse::Complete(second) = second else {
                            panic!("expected completed retry, got {second:?}");
                        };
                        assert_eq!(second.structured_content.as_ref().unwrap()["state"], STATE);
                        assert_eq!(second.structured_content.as_ref().unwrap()["responses"], responses);
                        assert_eq!(second.structured_content.as_ref().unwrap()["marker"], "second");

                        let filter = SubscriptionFilter::builder().tools_list_changed().build();
                        let mut subscription = client.listen(filter.clone()).await
                            .map_err(Error::into_internal_error)?;
                        listening_rx.recv().await.expect("subscription service started");
                        assert_eq!(subscription.acknowledged(), &filter);
                        let notification = subscription.next().await
                            .map_err(Error::into_internal_error)?
                            .expect("filtered notification");
                        let notification_json = serde_json::to_value(&notification)
                            .map_err(Error::into_internal_error)?;
                        assert_eq!(notification_json["method"], "notifications/tools/list_changed");
                        assert_eq!(
                            notification_json["params"]["_meta"]["io.modelcontextprotocol/subscriptionId"],
                            serde_json::to_value(subscription.id())
                                .map_err(Error::into_internal_error)?
                        );

                        // An active HTTP SSE listen must not block an ordinary POST.
                        let parallel = client.call_tool_once(marked_call("annotated", "parallel"))
                            .await.map_err(Error::into_internal_error)?;
                        assert!(matches!(parallel, CallToolResponse::Complete(_)));
                        subscription.cancel().await.map_err(Error::into_internal_error)?;
                        stopped_rx.recv().await.expect("subscription cancelled upstream");
                        drop(subscription);
                        let direct = client.call_tool_once(marked_call("annotated", "after-cancel"))
                            .await.map_err(Error::into_internal_error)?;
                        assert!(matches!(direct, CallToolResponse::Complete(_)));
                        let tools = client.list_tools(None).await.map_err(Error::into_internal_error)?;
                        let annotated = tools.tools.iter().find(|tool| tool.name == "annotated")
                            .expect("annotated tool still listed");
                        assert_eq!(annotated.annotations.as_ref().unwrap().read_only_hint, Some(true));
                        assert_eq!(
                            annotated.input_schema.get("properties").unwrap()["region"],
                            json!({"type": "string"})
                        );
                        assert_eq!(lists.load(Ordering::SeqCst), 1);
                        client.cancel().await.map_err(Error::into_internal_error)?;
                        Ok(())
                    })
                    .await
            },
        ).await
    })
    .await
    .expect("rmcp/HTTP/ACP integration timed out")
}
