//! Schema 1.18 additions parse into the SDK's typed v1 and v2 messages.

#![cfg(any(
    feature = "unstable_agent_skills",
    feature = "unstable_prompt_suggestions",
    feature = "unstable_protocol_v2",
    feature = "unstable_session_fork",
    feature = "unstable_speed_config_category"
))]
#![allow(dead_code, unused_imports)]

use agent_client_protocol::v1::schema as v1;
use agent_client_protocol::{JsonRpcMessage, JsonRpcResponse};
use serde_json::{Value, json};

/// Parse `params` as `method` into `E`, check the parsed method, and check
/// that the message serializes back to the same params.
fn assert_message_round_trips<E: JsonRpcMessage>(method: &str, params: &Value) -> E {
    let parsed = E::parse_message(method, params)
        .unwrap_or_else(|error| panic!("{method} should parse: {error:?}"));
    assert_eq!(parsed.method(), method);
    let untyped = parsed.to_untyped_message().unwrap();
    assert_eq!(untyped.method(), method);
    assert_eq!(untyped.params(), params, "{method} params round trip");
    parsed
}

/// Parse `result` as the response to `method` into `R` and check that it
/// serializes back to the same JSON.
fn assert_response_round_trips<R: JsonRpcResponse>(method: &str, result: &Value) {
    let parsed = R::from_json(method, result.clone())
        .unwrap_or_else(|error| panic!("{method} response should parse: {error:?}"));
    assert_eq!(
        &parsed.into_json(method).unwrap(),
        result,
        "{method} result"
    );
}

/// Parsing `params` as `method` into `E` fails with `-32602`.
fn assert_invalid_params<E: JsonRpcMessage>(method: &str, params: &Value) {
    let error = E::parse_message(method, params)
        .err()
        .unwrap_or_else(|| panic!("{method} should reject {params}"));
    assert_eq!(i32::from(error.code), -32602, "{method} error code");
}

/// A `session/update` notification's params for `update`.
fn session_update(update: Value) -> Value {
    json!({ "sessionId": "sess_1", "update": update })
}

#[cfg(feature = "unstable_prompt_suggestions")]
#[test]
fn v1_prompt_suggestions_update_round_trips() {
    let params = session_update(json!({
        "sessionUpdate": "prompt_suggestions_update",
        "suggestions": [
            { "prompt": [{ "type": "text", "text": "Run the tests" }] },
            {
                "prompt": [
                    { "type": "text", "text": "Add tests for " },
                    { "type": "resource_link", "name": "parser.rs", "uri": "file:///work/parser.rs" }
                ],
                "_meta": { "rank": 2 }
            }
        ]
    }));
    let parsed = assert_message_round_trips::<v1::AgentNotification>("session/update", &params);
    let v1::AgentNotification::SessionNotification(notification) = parsed else {
        panic!("expected a session notification");
    };
    let v1::SessionUpdate::PromptSuggestionsUpdate(update) = notification.update else {
        panic!("expected a prompt suggestions update");
    };
    assert_eq!(update.suggestions.len(), 2);

    // An empty list withdraws the suggestions.
    assert_message_round_trips::<v1::AgentNotification>(
        "session/update",
        &session_update(json!({
            "sessionUpdate": "prompt_suggestions_update",
            "suggestions": []
        })),
    );
}

#[cfg(feature = "unstable_prompt_suggestions")]
#[test]
fn v1_prompt_suggestions_update_skips_suggestions_it_cannot_parse() {
    // One bad block drops the whole suggestion. Dropping only the block would
    // change what the prompt asks for.
    let parsed = v1::AgentNotification::parse_message(
        "session/update",
        &session_update(json!({
            "sessionUpdate": "prompt_suggestions_update",
            "suggestions": [
                { "prompt": [{ "type": "text", "text": "Keep" }] },
                { "prompt": [{ "type": "text", "text": "Open " }, { "type": "text" }] },
                { "text": "No prompt" },
                "not an object"
            ]
        })),
    )
    .unwrap();
    assert_eq!(
        parsed.to_untyped_message().unwrap().params()["update"]["suggestions"],
        json!([{ "prompt": [{ "type": "text", "text": "Keep" }] }])
    );

    // The list itself is required and non-null.
    for update in [
        json!({ "sessionUpdate": "prompt_suggestions_update" }),
        json!({ "sessionUpdate": "prompt_suggestions_update", "suggestions": null }),
    ] {
        assert_invalid_params::<v1::AgentNotification>("session/update", &session_update(update));
    }
}

#[cfg(feature = "unstable_prompt_suggestions")]
#[test]
fn v1_client_advertises_prompt_suggestions() {
    for prompt_suggestions in [
        json!({}),
        json!({ "image": {}, "audio": {}, "embeddedContext": {} }),
    ] {
        let params = json!({
            "protocolVersion": 1,
            "clientCapabilities": { "session": { "promptSuggestions": prompt_suggestions } }
        });
        let parsed = v1::ClientRequest::parse_message("initialize", &params).unwrap();
        // v1 also writes its default capabilities, so compare `session` only.
        assert_eq!(
            parsed.to_untyped_message().unwrap().params()["clientCapabilities"]["session"],
            params["clientCapabilities"]["session"]
        );
        let v1::ClientRequest::InitializeRequest(request) = parsed else {
            panic!("expected an initialize request");
        };
        assert!(
            request
                .client_capabilities
                .session
                .and_then(|session| session.prompt_suggestions)
                .is_some()
        );
    }

    // Only an object advertises support.
    let capabilities: v1::ClientCapabilities =
        serde_json::from_value(json!({ "session": { "promptSuggestions": true } })).unwrap();
    assert!(
        capabilities
            .session
            .and_then(|session| session.prompt_suggestions)
            .is_none()
    );
}

#[cfg(feature = "unstable_speed_config_category")]
#[test]
fn v1_speed_config_option_category_round_trips() {
    let option = json!({
        "type": "boolean",
        "id": "fast",
        "name": "Fast mode",
        "category": "speed",
        "currentValue": true
    });
    let parsed: v1::SessionConfigOption = serde_json::from_value(option.clone()).unwrap();
    assert_eq!(
        parsed.category,
        Some(v1::SessionConfigOptionCategory::Speed)
    );
    assert_eq!(serde_json::to_value(parsed).unwrap(), option);
    assert_message_round_trips::<v1::AgentNotification>(
        "session/update",
        &session_update(json!({
            "sessionUpdate": "config_option_update",
            "configOptions": [option]
        })),
    );
}

#[cfg(all(
    feature = "unstable_session_fork",
    feature = "unstable_session_options"
))]
#[test]
fn v1_session_fork_carries_initial_config_options() {
    let mut params = json!({
        "sessionId": "sess_1",
        "cwd": "/work",
        "configOptions": [
            { "configId": "model", "value": "model-2" },
            { "configId": "fast", "type": "boolean", "value": true }
        ]
    });
    let parsed = assert_message_round_trips::<v1::ClientRequest>("session/fork", &params);
    let v1::ClientRequest::ForkSessionRequest(request) = parsed else {
        panic!("expected a fork request");
    };
    assert_eq!(request.config_options.len(), 2);

    // `null` is an empty list.
    params["configOptions"] = Value::Null;
    let v1::ClientRequest::ForkSessionRequest(request) =
        v1::ClientRequest::parse_message("session/fork", &params).unwrap()
    else {
        panic!("expected a fork request");
    };
    assert_eq!(request.config_options, vec![]);

    // A malformed entry fails the request instead of being dropped.
    params["configOptions"] = json!([{ "type": "boolean", "value": true }]);
    assert_invalid_params::<v1::ClientRequest>("session/fork", &params);
}

#[cfg(feature = "unstable_protocol_v2")]
mod v2_methods {
    use agent_client_protocol::JsonRpcMessage;
    use agent_client_protocol::v2::schema as v2;
    use serde_json::{Value, json};

    use super::{
        assert_invalid_params, assert_message_round_trips, assert_response_round_trips,
        session_update,
    };

    /// Round-trip `update` through the v2 `session/update` route and return
    /// the typed update.
    fn round_trip_update(update: Value) -> v2::SessionUpdate {
        let parsed = assert_message_round_trips::<v2::AgentNotification>(
            "session/update",
            &session_update(update),
        );
        let v2::AgentNotification::UpdateSessionNotification(notification) = parsed else {
            panic!("expected a session update notification");
        };
        notification.update
    }

    #[test]
    fn v2_replay_complete_round_trips() {
        assert!(matches!(
            round_trip_update(json!({ "sessionUpdate": "replay_complete" })),
            v2::SessionUpdate::ReplayComplete(_)
        ));
        assert!(matches!(
            round_trip_update(json!({
                "sessionUpdate": "replay_complete",
                "_meta": { "com.example/replayed": 12 }
            })),
            v2::SessionUpdate::ReplayComplete(replay) if replay.meta.is_some()
        ));
        assert_eq!(
            serde_json::to_value(v2::SessionUpdate::ReplayComplete(v2::ReplayComplete::new()))
                .unwrap(),
            json!({ "sessionUpdate": "replay_complete" })
        );
    }

    /// Subagent updates, session-directed messages, the `unknown` state,
    /// `subagent` background work and `session` tool call content are part of
    /// v2 without `unstable_subagents`.
    #[test]
    fn v2_subagent_surface_is_typed_without_the_subagents_feature() {
        assert!(matches!(
            round_trip_update(json!({
                "sessionUpdate": "subagent_update",
                "sessionId": "child",
                "title": "Researcher",
                "capabilities": { "cancel": {} }
            })),
            v2::SessionUpdate::SubagentUpdate(update)
                if update.session_id == v2::SessionId::new("child")
        ));
        assert!(matches!(
            round_trip_update(json!({
                "sessionUpdate": "session_message",
                "messageId": "received_1",
                "senderSessionId": "parent",
                "recipientSessionId": "child"
            })),
            v2::SessionUpdate::SessionMessage(_)
        ));
        assert!(matches!(
            round_trip_update(json!({
                "sessionUpdate": "session_message_chunk",
                "messageId": "received_1",
                "content": { "type": "text", "text": "Please investigate" }
            })),
            v2::SessionUpdate::SessionMessageChunk(_)
        ));

        let v2::SessionUpdate::StateUpdate(v2::StateUpdate::Unknown(unknown)) =
            round_trip_update(json!({
                "sessionUpdate": "state_update",
                "state": "unknown",
                "background": [
                    { "type": "tool_call", "toolCallId": "call_1" },
                    { "type": "subagent", "sessionId": "child" }
                ]
            }))
        else {
            panic!("expected the unknown state");
        };
        assert!(matches!(
            unknown.background.as_deref(),
            Some([
                v2::BackgroundWork::ToolCall(_),
                v2::BackgroundWork::Subagent(subagent)
            ]) if subagent.session_id == v2::SessionId::new("child")
        ));

        let v2::SessionUpdate::ToolCallUpdate(tool_call) = round_trip_update(json!({
            "sessionUpdate": "tool_call_update",
            "toolCallId": "call_1",
            "content": [{ "type": "session", "sessionId": "child" }]
        })) else {
            panic!("expected a tool call update");
        };
        assert!(matches!(
            tool_call.content.value().map(Vec::as_slice),
            Some([v2::ToolCallContent::Session(_)])
        ));
    }

    /// A malformed known variant is dropped like other malformed display
    /// items. It is not read as a custom variant.
    #[test]
    fn v2_malformed_session_tool_call_content_is_dropped() {
        let parsed = v2::AgentNotification::parse_message(
            "session/update",
            &session_update(json!({
                "sessionUpdate": "tool_call_update",
                "toolCallId": "call_1",
                "content": [{ "type": "session" }]
            })),
        )
        .unwrap();
        let untyped = parsed.to_untyped_message().unwrap();
        assert_eq!(untyped.params()["update"]["content"], json!([]));
    }

    #[cfg(feature = "unstable_agent_skills")]
    #[test]
    fn v2_skill_commands_round_trip() {
        let v2::SessionUpdate::AvailableCommandsUpdate(update) = round_trip_update(json!({
            "sessionUpdate": "available_commands_update",
            "availableCommands": [
                {
                    "type": "skill",
                    "name": "pdf-processing",
                    "description": "Extract text and tables from PDF files",
                    "path": "/home/alice/.agents/skills/pdf-processing/SKILL.md"
                },
                { "type": "command", "name": "test", "description": "Run project tests" }
            ]
        })) else {
            panic!("expected an available commands update");
        };
        let [
            v2::AvailableCommand::Skill(skill),
            v2::AvailableCommand::Command(_),
        ] = update.available_commands.as_slice()
        else {
            panic!("expected a skill and a command");
        };
        assert_eq!(skill.name, "pdf-processing");
        assert!(skill.path.is_some());

        // A malformed skill is skipped like any other invalid command. It is
        // not kept as an unknown command type.
        let parsed = v2::AgentNotification::parse_message(
            "session/update",
            &session_update(json!({
                "sessionUpdate": "available_commands_update",
                "availableCommands": [
                    { "type": "skill", "name": "no-description" },
                    { "type": "command", "name": "test", "description": "Run project tests" }
                ]
            })),
        )
        .unwrap();
        assert_eq!(
            parsed.to_untyped_message().unwrap().params()["update"]["availableCommands"],
            json!([{ "type": "command", "name": "test", "description": "Run project tests" }])
        );
    }

    #[cfg(feature = "unstable_agent_skills")]
    #[test]
    fn v2_prompt_carries_skill_references() {
        let mut params = json!({
            "sessionId": "sess_1",
            "prompt": [
                { "type": "skill", "name": "pdf-processing" },
                { "type": "text", "text": "Summarize report.pdf" }
            ]
        });
        let parsed = assert_message_round_trips::<v2::ClientRequest>("session/prompt", &params);
        let v2::ClientRequest::PromptRequest(request) = parsed else {
            panic!("expected a prompt request");
        };
        assert!(matches!(
            request.prompt.as_slice(),
            [v2::ContentBlock::Skill(skill), v2::ContentBlock::Text(_)]
                if skill.name == "pdf-processing"
        ));

        // The agent acts on `name`, so a malformed reference fails the prompt
        // with `-32602`. It is not kept as an unknown content block.
        params["prompt"] = json!([{ "type": "skill" }]);
        assert_invalid_params::<v2::ClientRequest>("session/prompt", &params);

        // The agent reports the inserted user message with the block in place.
        assert!(matches!(
            round_trip_update(json!({
                "sessionUpdate": "user_message",
                "messageId": "msg_1",
                "content": [
                    { "type": "skill", "name": "pdf-processing" },
                    { "type": "text", "text": "Summarize report.pdf" }
                ]
            })),
            v2::SessionUpdate::UserMessage(_)
        ));
    }

    #[cfg(feature = "unstable_agent_skills")]
    #[test]
    fn v2_skill_capabilities_round_trip() {
        // The client advertises that it understands `skill` command entries.
        let params = json!({
            "protocolVersion": 2,
            "info": { "name": "client", "version": "1.0.0" },
            "capabilities": { "session": { "commands": { "skill": {} } } }
        });
        let parsed = assert_message_round_trips::<v2::ClientRequest>("initialize", &params);
        let v2::ClientRequest::InitializeRequest(request) = parsed else {
            panic!("expected an initialize request");
        };
        assert!(
            request
                .capabilities
                .session
                .and_then(|session| session.commands)
                .and_then(|commands| commands.skill)
                .is_some()
        );

        // The agent advertises where a prompt may carry `skill` blocks: `{}`
        // allows one leading block, `inline` allows any number anywhere.
        for (skill, inline) in [(json!({}), false), (json!({ "inline": {} }), true)] {
            let capabilities: v2::AgentCapabilities = serde_json::from_value(json!({
                "session": { "prompt": { "skill": skill } }
            }))
            .unwrap();
            let prompt_skill = capabilities
                .session
                .as_ref()
                .and_then(|session| session.prompt.as_ref())
                .and_then(|prompt| prompt.skill.as_ref())
                .expect("the skill prompt capability should be advertised");
            assert_eq!(prompt_skill.inline.is_some(), inline);
            assert_eq!(
                serde_json::to_value(&capabilities).unwrap()["session"]["prompt"]["skill"],
                skill
            );
        }
    }

    #[cfg(feature = "unstable_prompt_suggestions")]
    #[test]
    fn v2_prompt_suggestions_round_trip() {
        let v2::SessionUpdate::PromptSuggestionsUpdate(update) = round_trip_update(json!({
            "sessionUpdate": "prompt_suggestions_update",
            "suggestions": [
                { "prompt": [{ "type": "text", "text": "Run the tests" }] }
            ]
        })) else {
            panic!("expected a prompt suggestions update");
        };
        assert_eq!(update.suggestions.len(), 1);
        round_trip_update(json!({
            "sessionUpdate": "prompt_suggestions_update",
            "suggestions": []
        }));

        // A suggestion the client cannot parse is skipped, and the list
        // itself is required.
        let parsed = v2::AgentNotification::parse_message(
            "session/update",
            &session_update(json!({
                "sessionUpdate": "prompt_suggestions_update",
                "suggestions": [
                    { "prompt": [{ "type": "text", "text": "Keep" }] },
                    { "prompt": "not a list" }
                ]
            })),
        )
        .unwrap();
        assert_eq!(
            parsed.to_untyped_message().unwrap().params()["update"]["suggestions"],
            json!([{ "prompt": [{ "type": "text", "text": "Keep" }] }])
        );
        assert_invalid_params::<v2::AgentNotification>(
            "session/update",
            &session_update(json!({ "sessionUpdate": "prompt_suggestions_update" })),
        );

        let params = json!({
            "protocolVersion": 2,
            "info": { "name": "client", "version": "1.0.0" },
            "capabilities": { "session": { "promptSuggestions": { "image": {} } } }
        });
        let parsed = assert_message_round_trips::<v2::ClientRequest>("initialize", &params);
        let v2::ClientRequest::InitializeRequest(request) = parsed else {
            panic!("expected an initialize request");
        };
        assert!(
            request
                .capabilities
                .session
                .and_then(|session| session.prompt_suggestions)
                .and_then(|suggestions| suggestions.image)
                .is_some()
        );
    }

    #[cfg(feature = "unstable_speed_config_category")]
    #[test]
    fn v2_speed_config_option_category_round_trips() {
        let option = json!({
            "type": "boolean",
            "configId": "fast",
            "name": "Fast mode",
            "category": "speed",
            "currentValue": true
        });
        let parsed: v2::SessionConfigOption = serde_json::from_value(option.clone()).unwrap();
        assert_eq!(
            parsed.category,
            Some(v2::SessionConfigOptionCategory::Speed)
        );
        assert_eq!(serde_json::to_value(parsed).unwrap(), option);
        round_trip_update(json!({
            "sessionUpdate": "config_option_update",
            "configOptions": [option]
        }));
    }

    #[cfg(all(
        feature = "unstable_session_fork",
        feature = "unstable_session_options"
    ))]
    #[test]
    fn v2_session_fork_carries_initial_config_options() {
        let mut params = json!({
            "sessionId": "sess_1",
            "cwd": "/work",
            "configOptions": [
                { "configId": "model", "type": "id", "value": "model-2" },
                { "configId": "fast", "type": "boolean", "value": true }
            ]
        });
        let parsed = assert_message_round_trips::<v2::ClientRequest>("session/fork", &params);
        let v2::ClientRequest::ForkSessionRequest(request) = parsed else {
            panic!("expected a fork request");
        };
        assert_eq!(request.config_options.len(), 2);

        // Entries need `type`, and a malformed entry fails the request.
        for entry in [
            json!({ "configId": "model", "value": "model-2" }),
            json!({ "configId": "fast", "type": "boolean", "value": "yes" }),
        ] {
            params["configOptions"] = json!([entry]);
            assert_invalid_params::<v2::ClientRequest>("session/fork", &params);
        }
    }
}
