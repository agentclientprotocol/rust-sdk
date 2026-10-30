//! testy: a deterministic ACP test agent with typed JSON prompt commands.
//!
//! The agent accepts JSON-serialized [`TestyCommand`] values as prompt text. It
//! can also recognize short plain-text scenario names such as `full`,
//! `callbacks`, and `session_updates` for manual testing in clients.
//!
//! The protocol v1 agent is [`v1::Testy`] and, with `unstable_protocol_v2`,
//! the protocol v2 agent is [`v2::Testy`]. Both read the same
//! [`TestyCommand`] prompts. With `unstable_protocol_v2`, [`protocol_router`]
//! serves either version on one transport.

pub mod v1;
#[cfg(feature = "unstable_protocol_v2")]
pub mod v2;

use serde::{Deserialize, Serialize};

/// Commands that can be sent as prompt text (serialized as JSON) to a Testy agent.
///
/// Tests construct these as typed values and serialize to JSON via [`TestyCommand::to_prompt`].
/// The agent deserializes the prompt text and dispatches accordingly.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "command", rename_all = "snake_case")]
pub enum TestyCommand {
    /// Returns a short list of supported commands and scenarios.
    Help,

    /// Responds with `"Hello, world!"`.
    Greet,

    /// Echoes the given message back as the response.
    Echo { message: String },

    /// Runs a deterministic scenario that exercises part of the ACP surface.
    RunScenario { scenario: TestyScenario },

    /// Invokes an MCP tool and returns the result.
    /// The agent must have been given MCP servers in the `NewSessionRequest`.
    CallTool {
        server: String,
        tool: String,
        #[serde(default)]
        params: serde_json::Value,
    },

    /// Lists tools from the named MCP server.
    ListTools { server: String },
}

impl TestyCommand {
    /// Serialize this command to a JSON string suitable for use as prompt text.
    #[must_use]
    pub fn to_prompt(&self) -> String {
        serde_json::to_string(self).expect("TestyCommand serialization should not fail")
    }
}

/// Prompt-driven scenarios supported by [`TestyCommand::RunScenario`].
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum TestyScenario {
    /// Emits every stable `session/update` variant.
    SessionUpdates,
    /// Exercises content blocks and prompt-content parsing.
    Content,
    /// Emits tool-call create/update notifications with content, diff, and locations.
    ToolCalls,
    /// Sends every stable agent-to-client request.
    Callbacks,
    /// Runs elicitation coverage without the other stable callback requests.
    Elicitations,
    /// Reports whether the session has received a `session/cancel` notification.
    CancelStatus,
    /// Waits for `session/cancel` before completing.
    WaitForCancel,
    /// Runs all stable scenarios in a deterministic order.
    Full,
}

impl TestyScenario {
    fn all() -> Vec<Self> {
        vec![
            Self::SessionUpdates,
            Self::Content,
            Self::ToolCalls,
            Self::Callbacks,
            Self::Elicitations,
            Self::CancelStatus,
            Self::WaitForCancel,
            Self::Full,
        ]
    }

    fn from_prompt(input: &str) -> Option<Self> {
        match input.trim().to_ascii_lowercase().as_str() {
            "session_updates" | "session updates" | "updates" => Some(Self::SessionUpdates),
            "content" => Some(Self::Content),
            "tool_calls" | "tool calls" | "tools" => Some(Self::ToolCalls),
            "callbacks" | "client_callbacks" | "client callbacks" => Some(Self::Callbacks),
            "elicitations" | "elicitation" | "elicit" => Some(Self::Elicitations),
            "cancel_status" | "cancel status" | "cancel" => Some(Self::CancelStatus),
            "wait_for_cancel" | "wait for cancel" => Some(Self::WaitForCancel),
            "full" | "all" => Some(Self::Full),
            _ => None,
        }
    }

    const fn name(self) -> &'static str {
        match self {
            Self::SessionUpdates => "session_updates",
            Self::Content => "content",
            Self::ToolCalls => "tool_calls",
            Self::Callbacks => "callbacks",
            Self::Elicitations => "elicitations",
            Self::CancelStatus => "cancel_status",
            Self::WaitForCancel => "wait_for_cancel",
            Self::Full => "full",
        }
    }
}

/// Build a Testy agent that selects its native v1 or v2 implementation from
/// the client's `initialize` request.
///
/// The v1 and v2 agents keep independent session state and lifecycle
/// implementations.
#[cfg(feature = "unstable_protocol_v2")]
#[must_use]
pub fn protocol_router() -> agent_client_protocol::AgentProtocolRouter {
    agent_client_protocol::Agent
        .protocol_router()
        .with_v1(v1::Testy::new())
        .with_v2(v2::Testy::new())
}

fn parse_command(input_text: &str) -> TestyCommand {
    if input_text.trim().is_empty() {
        return TestyCommand::Greet;
    }

    if let Ok(command) = serde_json::from_str(input_text) {
        return command;
    }

    let trimmed = input_text.trim();
    if trimmed.eq_ignore_ascii_case("help") {
        return TestyCommand::Help;
    }

    if let Some(scenario) = TestyScenario::from_prompt(trimmed) {
        return TestyCommand::RunScenario { scenario };
    }

    if let Some(message) = trimmed.strip_prefix("echo ") {
        return TestyCommand::Echo {
            message: message.to_string(),
        };
    }

    TestyCommand::Greet
}
