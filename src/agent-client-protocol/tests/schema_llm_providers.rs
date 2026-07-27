#![cfg(feature = "unstable_llm_providers")]

use std::collections::HashMap;

use agent_client_protocol::schema::{
    ProtocolVersion,
    v1::{
        AgentCapabilities, AgentResponse, ClientRequest, DisableProviderRequest,
        DisableProviderResponse, InitializeRequest, InitializeResponse, ListProvidersRequest,
        ListProvidersResponse, LlmProtocol, ProviderCurrentConfig, ProviderInfo,
        ProvidersCapabilities, SetProviderRequest, SetProviderResponse,
    },
};
use agent_client_protocol::{
    Agent, Client, Error, JsonRpcMessage, JsonRpcRequest, JsonRpcResponse,
};
use serde_json::json;

fn provider_info() -> ProviderInfo {
    ProviderInfo::new(
        "main",
        vec![
            LlmProtocol::Anthropic,
            LlmProtocol::Other("_gateway".into()),
        ],
        true,
        ProviderCurrentConfig::new(LlmProtocol::Anthropic, "https://llm.example.test"),
    )
}

fn test_headers() -> HashMap<String, String> {
    HashMap::from([("X-Test-Routing".to_string(), "tenant-demo".to_string())])
}

fn assert_request_pair<Request, Response>()
where
    Request: JsonRpcRequest<Response = Response>,
{
}

#[test]
fn provider_requests_have_typed_v1_jsonrpc_routes() {
    assert_request_pair::<ListProvidersRequest, ListProvidersResponse>();
    assert_request_pair::<SetProviderRequest, SetProviderResponse>();
    assert_request_pair::<DisableProviderRequest, DisableProviderResponse>();

    let list = ListProvidersRequest::new();
    assert_eq!(list.method(), "providers/list");
    assert!(ListProvidersRequest::matches_method("providers/list"));
    assert!(!ListProvidersRequest::matches_method("providers/set"));
    assert!(matches!(
        ClientRequest::parse_message("providers/list", &json!({})).unwrap(),
        ClientRequest::ListProvidersRequest(_)
    ));

    let set = SetProviderRequest::new(
        "main",
        LlmProtocol::Other("_gateway".into()),
        "https://llm.example.test",
    )
    .headers(test_headers());
    let untyped = set.to_untyped_message().unwrap();
    assert_eq!(untyped.method, "providers/set");
    assert_eq!(untyped.params["providerId"], "main");
    assert_eq!(untyped.params["apiType"], "_gateway");
    assert_eq!(untyped.params["headers"]["X-Test-Routing"], "tenant-demo");
    assert!(matches!(
        ClientRequest::parse_message("providers/set", &untyped.params).unwrap(),
        ClientRequest::SetProviderRequest(_)
    ));

    let disable = DisableProviderRequest::new("main");
    assert_eq!(disable.method(), "providers/disable");
    assert!(matches!(
        ClientRequest::parse_message(
            "providers/disable",
            &disable.to_untyped_message().unwrap().params
        )
        .unwrap(),
        ClientRequest::DisableProviderRequest(_)
    ));

    assert!(matches!(
        AgentResponse::from_value("providers/list", json!({ "providers": [] })).unwrap(),
        AgentResponse::ListProvidersResponse(_)
    ));
    assert!(matches!(
        AgentResponse::from_value("providers/set", json!({})).unwrap(),
        AgentResponse::SetProviderResponse(_)
    ));
    assert!(matches!(
        AgentResponse::from_value("providers/disable", json!({})).unwrap(),
        AgentResponse::DisableProviderResponse(_)
    ));
}

#[test]
fn provider_capabilities_and_open_protocol_values_round_trip() {
    let capabilities = AgentCapabilities::new().providers(ProvidersCapabilities::new());
    assert_eq!(
        serde_json::to_value(capabilities).unwrap()["providers"],
        json!({})
    );

    let protocol: LlmProtocol = serde_json::from_value(json!("_gateway")).unwrap();
    assert_eq!(protocol, LlmProtocol::Other("_gateway".into()));

    let omitted: ProviderInfo = serde_json::from_value(json!({
        "providerId": "optional",
        "supported": ["openai"],
        "required": false
    }))
    .unwrap();
    assert!(omitted.current.is_none());

    let null: ProviderInfo = serde_json::from_value(json!({
        "providerId": "optional",
        "supported": ["openai"],
        "required": false,
        "current": null
    }))
    .unwrap();
    assert!(null.current.is_none());
}

#[tokio::test(flavor = "current_thread")]
async fn v1_client_can_list_set_and_disable_providers() -> Result<(), Error> {
    let agent = Agent
        .builder()
        .on_receive_request(
            async |request: InitializeRequest, responder, _cx| {
                responder.respond(
                    InitializeResponse::new(request.protocol_version).agent_capabilities(
                        AgentCapabilities::new().providers(ProvidersCapabilities::new()),
                    ),
                )
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            async |_request: ListProvidersRequest, responder, _cx| {
                responder.respond(ListProvidersResponse::new(vec![provider_info()]))
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            async |request: SetProviderRequest, responder, _cx| {
                assert_eq!(request.provider_id.to_string(), "main");
                assert_eq!(request.headers, test_headers());
                responder.respond(SetProviderResponse::new())
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            async |request: DisableProviderRequest, responder, _cx| {
                assert_eq!(request.provider_id.to_string(), "main");
                responder.respond(DisableProviderResponse::new())
            },
            agent_client_protocol::on_receive_request!(),
        );

    Client
        .builder()
        .connect_with(agent, async |cx| {
            let initialize = cx
                .send_request(InitializeRequest::new(ProtocolVersion::V1))
                .block_task()
                .await?;
            assert!(initialize.agent_capabilities.providers.is_some());

            let listed = cx
                .send_request(ListProvidersRequest::new())
                .block_task()
                .await?;
            assert_eq!(listed.providers, vec![provider_info()]);

            cx.send_request(
                SetProviderRequest::new("main", LlmProtocol::Anthropic, "https://llm.example.test")
                    .headers(test_headers()),
            )
            .block_task()
            .await?;
            cx.send_request(DisableProviderRequest::new("main"))
                .block_task()
                .await?;
            Ok(())
        })
        .await
}

#[cfg(feature = "unstable_protocol_v2")]
#[test]
fn provider_requests_have_typed_draft_v2_aggregate_routes() -> Result<(), Error> {
    use agent_client_protocol::schema::v2;

    assert_request_pair::<v2::ListProvidersRequest, v2::ListProvidersResponse>();
    assert_request_pair::<v2::SetProviderRequest, v2::SetProviderResponse>();
    assert_request_pair::<v2::DisableProviderRequest, v2::DisableProviderResponse>();

    assert!(matches!(
        v2::ClientRequest::parse_message("providers/list", &json!({}))?,
        v2::ClientRequest::ListProvidersRequest(_)
    ));
    assert!(matches!(
        v2::ClientRequest::parse_message(
            "providers/set",
            &json!({
                "providerId": "main",
                "apiType": "anthropic",
                "baseUrl": "https://llm.example.test"
            })
        )?,
        v2::ClientRequest::SetProviderRequest(_)
    ));
    assert!(matches!(
        v2::ClientRequest::parse_message("providers/disable", &json!({ "providerId": "main" }))?,
        v2::ClientRequest::DisableProviderRequest(_)
    ));

    assert!(matches!(
        v2::AgentResponse::from_value("providers/list", json!({ "providers": [] }))?,
        v2::AgentResponse::ListProvidersResponse(_)
    ));
    assert!(matches!(
        v2::AgentResponse::from_value("providers/set", json!({}))?,
        v2::AgentResponse::SetProviderResponse(_)
    ));
    assert!(matches!(
        v2::AgentResponse::from_value("providers/disable", json!({}))?,
        v2::AgentResponse::DisableProviderResponse(_)
    ));
    Ok(())
}

#[cfg(feature = "unstable_protocol_v2")]
#[tokio::test(flavor = "current_thread")]
async fn draft_v2_client_can_list_set_and_disable_providers() -> Result<(), Error> {
    use agent_client_protocol::schema::v2;

    fn implementation() -> v2::Implementation {
        v2::Implementation::new("provider-test", env!("CARGO_PKG_VERSION"))
    }

    let agent = Agent
        .v2()
        .on_receive_request(
            async |request: v2::InitializeRequest, responder, _cx| {
                responder.respond(
                    v2::InitializeResponse::new(request.protocol_version, implementation())
                        .capabilities(
                            v2::AgentCapabilities::new()
                                .providers(v2::ProvidersCapabilities::new()),
                        ),
                )
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            async |_request: v2::ListProvidersRequest, responder, _cx| {
                responder.respond(v2::ListProvidersResponse::new(vec![v2::ProviderInfo::new(
                    "main",
                    vec![v2::LlmProtocol::Anthropic],
                    true,
                    v2::ProviderCurrentConfig::new(
                        v2::LlmProtocol::Anthropic,
                        "https://llm.example.test",
                    ),
                )]))
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            async |request: v2::SetProviderRequest, responder, _cx| {
                assert_eq!(request.provider_id.to_string(), "main");
                assert_eq!(request.headers, test_headers());
                responder.respond(v2::SetProviderResponse::new())
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            async |request: v2::DisableProviderRequest, responder, _cx| {
                assert_eq!(request.provider_id.to_string(), "main");
                responder.respond(v2::DisableProviderResponse::new())
            },
            agent_client_protocol::on_receive_request!(),
        );

    Client
        .v2()
        .connect_with(agent, async |cx| {
            let initialize = cx
                .send_request(v2::InitializeRequest::new(
                    ProtocolVersion::V2,
                    implementation(),
                ))
                .block_task()
                .await?;
            assert!(initialize.capabilities.providers.is_some());

            let listed = cx
                .send_request(v2::ListProvidersRequest::new())
                .block_task()
                .await?;
            assert_eq!(listed.providers.len(), 1);

            cx.send_request(
                v2::SetProviderRequest::new(
                    "main",
                    v2::LlmProtocol::Anthropic,
                    "https://llm.example.test",
                )
                .headers(test_headers()),
            )
            .block_task()
            .await?;
            cx.send_request(v2::DisableProviderRequest::new("main"))
                .block_task()
                .await?;
            Ok(())
        })
        .await
}
