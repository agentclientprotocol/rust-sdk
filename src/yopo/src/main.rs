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
//! With command arguments:
//! ```bash
//! yopo "What is 2+2?" python my_agent.py
//! yopo "Hello!" -- cargo run --release
//! ```
//!
//! With JSON config:
//! ```bash
//! yopo "Hello!" '{"command":"python","args":["agent.py"],"env":{}}'
//! ```

use agent_client_protocol::AcpAgent;
use clap::Parser;
use tracing_subscriber::{EnvFilter, layer::SubscriberExt, util::SubscriberInitExt};

#[derive(Parser, Debug)]
#[command(author, version, about = "YOPO - You Only Prompt Once", long_about = None)]
struct Args {
    /// The prompt to send to the agent
    prompt: String,

    /// Agent command and arguments, or a single JSON configuration
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

    // Parse a single JSON value as configuration. Otherwise, preserve the
    // argument boundaries already established by the invoking shell.
    let agent = parse_agent_args(&args.agent_args)?;

    eprintln!("🚀 Spawning agent and running prompt...");

    // Use the library function with callback to print progressively
    yopo::prompt_with_callback(agent, prompt.as_str(), |block| async move {
        print!("{}", yopo::content_block_to_string(&block));
    })
    .await?;

    println!(); // Final newline
    eprintln!("✅ Agent completed!");

    Ok(())
}

fn parse_agent_args(agent_args: &[String]) -> Result<AcpAgent, agent_client_protocol::Error> {
    match agent_args {
        [configuration] if configuration.trim_start().starts_with('{') => configuration.parse(),
        arguments => AcpAgent::from_args(arguments),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn parses_json_agent_configuration() {
        let agent = parse_agent_args(&[
            r#"{"command":"python","args":["agent.py"],"env":{"RUST_LOG":"debug"}}"#.to_owned(),
        ])
        .unwrap();

        assert_eq!(agent.config().command(), Path::new("python"));
        assert_eq!(agent.config().arguments(), ["agent.py"]);
        assert_eq!(
            agent
                .config()
                .environment()
                .get("RUST_LOG")
                .map(String::as_str),
            Some("debug")
        );
    }

    #[test]
    fn preserves_single_executable_path_with_spaces() {
        let agent = parse_agent_args(&["/Applications/My Agent".to_owned()]).unwrap();

        assert_eq!(
            agent.config().command(),
            Path::new("/Applications/My Agent")
        );
        assert!(agent.config().arguments().is_empty());
    }

    #[test]
    fn accepts_agent_flags_after_argument_separator() {
        let args =
            Args::try_parse_from(["yopo", "Hello!", "--", "cargo", "run", "--release"]).unwrap();

        assert_eq!(args.agent_args, ["cargo", "run", "--release"]);
    }
}
