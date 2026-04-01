//! YOPO (You Only Prompt Once) - A simple ACP client for one-shot prompts
//!
//! This client:
//! - Takes a prompt and agent command as arguments
//! - Spawns the agent
//! - Sends the prompt
//! - Auto-approves all permission requests
//! - Prints content progressively as it arrives
//! - Runs until the agent completes
//!
//! # Usage
//!
//! With a command (arguments are concatenated):
//! ```bash
//! yopo "What is 2+2?" python my_agent.py
//! yopo "Hello!" cargo run --release
//! ```
//!
//! With JSON config:
//! ```bash
//! yopo "Hello!" '{"type":"stdio","name":"my-agent","command":"python","args":["agent.py"],"env":[]}'
//! ```

use agent_client_protocol_tokio::AcpAgent;
use clap::Parser;
use tracing_subscriber::{EnvFilter, layer::SubscriberExt, util::SubscriberInitExt};

#[derive(Parser, Debug)]
#[command(author, version, about = "YOPO - You Only Prompt Once", long_about = None)]
struct Args {
    /// The prompt to send to the agent
    prompt: String,

    /// Agent command (multiple arguments are joined with spaces) or JSON config
    #[arg(required = true, num_args = 1..)]
    agent_args: Vec<String>,

    /// Set logging level (trace, debug, info, warn, error)
    #[arg(short, long)]
    log: Option<String>,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args = Args::parse();

    // Initialize tracing to stderr
    let env_filter = if let Some(level) = args.log {
        EnvFilter::new(format!("yopo={level}"))
    } else {
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("yopo=info"))
    };

    tracing_subscriber::registry()
        .with(env_filter)
        .with(
            tracing_subscriber::fmt::layer()
                .with_target(true)
                .with_writer(std::io::stderr),
        )
        .init();

    let prompt = &args.prompt;

    // Parse the agent configuration from args
    let agent = AcpAgent::from_args(args.agent_args)?;

    eprintln!("🚀 Spawning agent and running prompt...");

    // Use the library function with callback to print progressively
    Box::pin(agent_client_protocol_yopo::prompt_with_callback(
        agent,
        prompt.as_str(),
        |block| async move {
            print!(
                "{}",
                agent_client_protocol_yopo::content_block_to_string(&block)
            );
        },
    ))
    .await?;

    println!(); // Final newline
    eprintln!("✅ Agent completed!");

    Ok(())
}
