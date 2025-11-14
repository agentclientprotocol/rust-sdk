//! YOLO one-shot client: A simple ACP client that runs a single prompt against an agent.
//!
//! This is a simplified example showing basic ACP client usage. It only supports
//! simple command strings (not JSON configs or environment variables).
//!
//! For a more full-featured client with JSON config support, see the `yopo` binary crate.
//!
//! # Usage
//!
//! ```bash
//! cargo run --example yolo_one_shot_client -- --command "python my_agent.py" "What is 2+2?"
//! ```

use clap::Parser;
use sacp::JrHandlerChain;
use sacp::schema::{
    ContentBlock, InitializeRequest, NewSessionRequest, PromptRequest, RequestPermissionOutcome,
    RequestPermissionRequest, RequestPermissionResponse, SessionNotification, TextContent,
    VERSION as PROTOCOL_VERSION,
};
use std::path::PathBuf;
use tokio::process::Child;
use tokio_util::compat::{TokioAsyncReadCompatExt, TokioAsyncWriteCompatExt};

#[derive(Parser)]
#[command(name = "yolo-one-shot-client")]
#[command(about = "A simple ACP client for one-shot prompts", long_about = None)]
struct Cli {
    /// The command to run the agent (e.g., "python my_agent.py")
    #[arg(short, long)]
    command: String,

    /// The prompt to send to the agent
    prompt: String,
}

/// Parse a command string into command and args
fn parse_command_string(s: &str) -> Result<(PathBuf, Vec<String>), Box<dyn std::error::Error>> {
    let parts = shell_words::split(s)?;
    if parts.is_empty() {
        return Err("Command string cannot be empty".into());
    }
    let command = PathBuf::from(&parts[0]);
    let args = parts[1..].to_vec();
    Ok((command, args))
}

/// Spawn a process for the agent and get stdio streams.
fn spawn_agent_process(
    command: PathBuf,
    args: Vec<String>,
) -> Result<
    (
        tokio::process::ChildStdin,
        tokio::process::ChildStdout,
        Child,
    ),
    Box<dyn std::error::Error>,
> {
    let mut cmd = tokio::process::Command::new(&command);
    cmd.args(&args);
    cmd.stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped());

    let mut child = cmd.spawn()?;
    let child_stdin = child.stdin.take().ok_or("Failed to open stdin")?;
    let child_stdout = child.stdout.take().ok_or("Failed to open stdout")?;

    Ok((child_stdin, child_stdout, child))
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let cli = Cli::parse();

    // Parse the command string
    let (command, args) = parse_command_string(&cli.command)?;

    eprintln!("🚀 Spawning agent: {} {:?}", command.display(), args);

    // Spawn the agent process
    let (child_stdin, child_stdout, mut child) = spawn_agent_process(command, args)?;

    // Create transport and connection
    let transport = sacp::ByteStreams::new(child_stdin.compat_write(), child_stdout.compat());
    let connection = JrHandlerChain::new();

    // Run the client
    connection
        .on_receive_notification(async move |notification: SessionNotification, _cx| {
            // Print session updates to stdout (so 2>/dev/null shows only agent output)
            println!("{:?}", notification.update);
            Ok(())
        })
        .on_receive_request(async move |request: RequestPermissionRequest, request_cx| {
            // YOLO: Auto-approve all permission requests by selecting the first option
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
        .with_client(transport, |cx: sacp::JrConnectionCx| async move {
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
            eprintln!("💬 Sending prompt: \"{}\"", cli.prompt);
            let prompt_response = cx
                .send_request(PromptRequest {
                    session_id: session_id.clone(),
                    prompt: vec![ContentBlock::Text(TextContent {
                        text: cli.prompt.clone(),
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

    // Kill the child process when done
    let _ = child.kill().await;

    Ok(())
}
