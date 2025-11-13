//! YOPO (You Only Prompt Once) - A simple ACP client for one-shot prompts
//!
//! This client:
//! - Takes a prompt and agent configuration as arguments
//! - Spawns the agent
//! - Sends the prompt
//! - Auto-approves all permission requests
//! - Prints all session updates to stdout
//! - Runs until the agent completes
//!
//! # Usage
//!
//! With a command:
//! ```bash
//! yopo "What is 2+2?" "python my_agent.py"
//! ```
//!
//! With JSON config:
//! ```bash
//! yopo "Hello!" '{"type":"stdio","name":"my-agent","command":"python","args":["agent.py"],"env":[]}'
//! ```

use sacp::JrHandlerChain;
use sacp::schema::{
    ContentBlock, InitializeRequest, NewSessionRequest, PromptRequest, RequestPermissionOutcome,
    RequestPermissionRequest, RequestPermissionResponse, SessionNotification, TextContent,
    VERSION as PROTOCOL_VERSION,
};
use sacp_tokio::AcpAgent;
use std::path::PathBuf;
use std::str::FromStr;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Parse command line arguments
    let args: Vec<String> = std::env::args().collect();
    if args.len() != 3 {
        eprintln!("Usage: {} <prompt> <agent-config>", args[0]);
        eprintln!();
        eprintln!("  <prompt>       - The prompt to send to the agent");
        eprintln!("  <agent-config> - Either a command string or JSON (starting with '{{')");
        eprintln!();
        eprintln!("Examples:");
        eprintln!("  {} \"What is 2+2?\" \"python my_agent.py\"", args[0]);
        eprintln!(
            "  {} \"Hello!\" '{{\"type\":\"stdio\",\"name\":\"agent\",\"command\":\"python\",\"args\":[\"agent.py\"],\"env\":[]}}'",
            args[0]
        );
        std::process::exit(1);
    }

    let prompt = &args[1];
    let agent_config = &args[2];

    // Parse the agent configuration
    let agent = AcpAgent::from_str(agent_config)?;

    eprintln!("🚀 Spawning agent and connecting...");

    // Run the client - AcpAgent implements IntoJrTransport
    JrHandlerChain::new()
        .on_receive_notification(async move |notification: SessionNotification, _cx| {
            // Print session updates to stdout (so 2>/dev/null shows only agent output)
            match notification.update {
                sacp::schema::SessionUpdate::AgentMessageChunk(content_chunk) => {
                    match content_chunk.content {
                        ContentBlock::Text(TextContent {
                            annotations: _,
                            text,
                            meta: _,
                        }) => print!("{text}"),
                        ContentBlock::Image(_) => {}
                        ContentBlock::Audio(_) => {}
                        ContentBlock::ResourceLink(_) => {}
                        ContentBlock::Resource(_) => {}
                    }
                }
                sacp::schema::SessionUpdate::UserMessageChunk(_) => {}
                sacp::schema::SessionUpdate::AgentThoughtChunk(_) => {}
                sacp::schema::SessionUpdate::ToolCall(_) => {}
                sacp::schema::SessionUpdate::ToolCallUpdate(_) => {}
                sacp::schema::SessionUpdate::Plan(_) => {}
                sacp::schema::SessionUpdate::AvailableCommandsUpdate(_) => {}
                sacp::schema::SessionUpdate::CurrentModeUpdate(_) => {}
            }
            Ok(())
        })
        .on_receive_request(async move |request: RequestPermissionRequest, request_cx| {
            // YOPO: Auto-approve all permission requests by selecting the first option
            eprintln!("✅ Auto-approving permission request: {:?}", request);
            let option_id = request.options.first().map(|opt| opt.id.clone());
            match option_id {
                Some(id) => request_cx.respond(RequestPermissionResponse {
                    outcome: RequestPermissionOutcome::Selected { option_id: id },
                    meta: None,
                }),
                None => {
                    eprintln!("⚠️ No options provided in permission request, cancelling");
                    request_cx.respond(RequestPermissionResponse {
                        outcome: RequestPermissionOutcome::Cancelled,
                        meta: None,
                    })
                }
            }
        })
        .connect_to(agent)?
        .with_client(|cx: sacp::JrConnectionCx| async move {
            // Initialize the agent
            eprintln!("🤝 Initializing agent...");
            let init_response = cx
                .send_request(InitializeRequest {
                    protocol_version: PROTOCOL_VERSION,
                    client_capabilities: Default::default(),
                    client_info: Default::default(),
                    meta: None,
                })
                .block_task()
                .await?;

            eprintln!("✓ Agent initialized: {:?}", init_response.agent_info);

            // Create a new session
            eprintln!("📝 Creating new session...");
            let new_session_response = cx
                .send_request(NewSessionRequest {
                    mcp_servers: vec![],
                    cwd: std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/")),
                    meta: None,
                })
                .block_task()
                .await?;

            let session_id = new_session_response.session_id;
            eprintln!("✓ Session created: {}", session_id);

            // Send the prompt
            eprintln!("💬 Sending prompt: \"{}\"", prompt);
            let prompt_response = cx
                .send_request(PromptRequest {
                    session_id: session_id.clone(),
                    prompt: vec![ContentBlock::Text(TextContent {
                        text: prompt.to_string(),
                        annotations: None,
                        meta: None,
                    })],
                    meta: None,
                })
                .block_task()
                .await?;

            eprintln!("✅ Agent completed!");
            eprintln!("Stop reason: {:?}", prompt_response.stop_reason);

            Ok(())
        })
        .await?;

    println!();

    Ok(())
}
