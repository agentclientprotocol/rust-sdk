//! Utilities for connecting to ACP agents and proxies.
//!
//! This module provides [`AcpAgent`], a convenient wrapper around [`sacp::schema::McpServer`]
//! that can be parsed from either a command string or JSON configuration.

use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::str::FromStr;
use std::task::{Context, Poll};

use serde::{Deserialize, Serialize};
use tokio::process::Child;
use tokio_util::compat::{TokioAsyncReadCompatExt, TokioAsyncWriteCompatExt};

/// A component representing an external ACP agent running in a separate process.
///
/// `AcpAgent` implements the [`sacp::Component`] trait for spawning and communicating with
/// external agents or proxies via stdio. It handles process spawning, stream setup, and
/// byte stream serialization automatically. This is the primary way to connect to agents
/// that run as separate executables.
///
/// This is a wrapper around [`sacp::schema::McpServer`] that provides convenient parsing
/// from command-line strings or JSON configurations.
///
/// # Use Cases
///
/// - **External agents**: Connect to agents written in any language (Python, Node.js, Rust, etc.)
/// - **Proxy chains**: Spawn intermediate proxies that transform or intercept messages
/// - **Conductor components**: Use with [`sacp_conductor::Conductor`] to build proxy chains
/// - **Subprocess isolation**: Run potentially untrusted code in a separate process
///
/// # Examples
///
/// Parse from a command string:
/// ```
/// # use sacp_tokio::AcpAgent;
/// # use std::str::FromStr;
/// let agent = AcpAgent::from_str("python my_agent.py --verbose").unwrap();
/// ```
///
/// Parse from JSON:
/// ```
/// # use sacp_tokio::AcpAgent;
/// # use std::str::FromStr;
/// let agent = AcpAgent::from_str(r#"{"type": "stdio", "name": "my-agent", "command": "python", "args": ["my_agent.py"], "env": []}"#).unwrap();
/// ```
///
/// Use as a component to connect to an external agent:
/// ```no_run
/// # use sacp::JrHandlerChain;
/// # use sacp_tokio::AcpAgent;
/// # use std::str::FromStr;
/// # async fn example() -> Result<(), Box<dyn std::error::Error>> {
/// let agent = AcpAgent::from_str("python my_agent.py")?;
///
/// // The agent process will be spawned automatically when served
/// JrHandlerChain::new()
///     .connect_to(agent)?
///     .with_client(|cx| async move {
///         // Use the connection to communicate with the agent process
///         Ok(())
///     })
///     .await?;
/// # Ok(())
/// # }
/// ```
///
/// [`sacp_conductor::Conductor`]: https://docs.rs/sacp-conductor/latest/sacp_conductor/struct.Conductor.html
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(transparent)]
pub struct AcpAgent {
    #[serde(flatten)]
    server: sacp::schema::McpServer,
}

impl AcpAgent {
    /// Create a new `AcpAgent` from an [`sacp::schema::McpServer`] configuration.
    pub fn new(server: sacp::schema::McpServer) -> Self {
        Self { server }
    }

    /// Get the underlying [`sacp::schema::McpServer`] configuration.
    pub fn server(&self) -> &sacp::schema::McpServer {
        &self.server
    }

    /// Convert into the underlying [`sacp::schema::McpServer`] configuration.
    pub fn into_server(self) -> sacp::schema::McpServer {
        self.server
    }

    /// Spawn the process and get stdio streams.
    /// Used internally by the Component trait implementation.
    pub fn spawn_process(
        &self,
    ) -> Result<
        (
            tokio::process::ChildStdin,
            tokio::process::ChildStdout,
            Child,
        ),
        sacp::Error,
    > {
        match &self.server {
            sacp::schema::McpServer::Stdio {
                command,
                args,
                env,
                name: _,
            } => {
                let mut cmd = tokio::process::Command::new(command);
                cmd.args(args);
                for env_var in env {
                    cmd.env(&env_var.name, &env_var.value);
                }
                cmd.stdin(std::process::Stdio::piped())
                    .stdout(std::process::Stdio::piped());

                let mut child = cmd.spawn().map_err(sacp::Error::into_internal_error)?;

                let child_stdin = child
                    .stdin
                    .take()
                    .ok_or_else(|| sacp::util::internal_error("Failed to open stdin"))?;
                let child_stdout = child
                    .stdout
                    .take()
                    .ok_or_else(|| sacp::util::internal_error("Failed to open stdout"))?;

                Ok((child_stdin, child_stdout, child))
            }
            sacp::schema::McpServer::Http { .. } => Err(sacp::util::internal_error(
                "HTTP transport not yet supported by AcpAgent",
            )),
            sacp::schema::McpServer::Sse { .. } => Err(sacp::util::internal_error(
                "SSE transport not yet supported by AcpAgent",
            )),
        }
    }
}

/// A future that holds a `Child` process and never resolves.
/// When dropped, the child process is killed.
struct ChildHolder {
    _child: Child,
}

impl Future for ChildHolder {
    type Output = Result<(), sacp::Error>;

    fn poll(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Self::Output> {
        // Never ready - just hold the child process alive
        Poll::Pending
    }
}

impl Drop for ChildHolder {
    fn drop(&mut self) {
        let _: Result<_, _> = self._child.start_kill();
    }
}

impl sacp::Component for AcpAgent {
    async fn serve(self, client: impl sacp::Component) -> Result<(), sacp::Error> {
        let (child_stdin, child_stdout, child) = self.spawn_process()?;

        // Hold the child process - it will be killed when this future completes
        let _child_holder = ChildHolder { _child: child };

        // Create the ByteStreams component and serve it
        sacp::ByteStreams::new(child_stdin.compat_write(), child_stdout.compat())
            .serve(client)
            .await
    }
}

impl FromStr for AcpAgent {
    type Err = sacp::Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let trimmed = s.trim();

        // If it starts with '{', try to parse as JSON
        if trimmed.starts_with('{') {
            let server: sacp::schema::McpServer = serde_json::from_str(trimmed)
                .map_err(|e| sacp::util::internal_error(format!("Failed to parse JSON: {}", e)))?;
            return Ok(Self { server });
        }

        // Otherwise, parse as a command string
        parse_command_string(trimmed)
    }
}

fn parse_command_string(s: &str) -> Result<AcpAgent, sacp::Error> {
    // Split the command string into words, respecting quotes
    let parts = shell_words::split(s)
        .map_err(|e| sacp::util::internal_error(format!("Failed to parse command: {}", e)))?;

    if parts.is_empty() {
        return Err(sacp::util::internal_error("Command string cannot be empty"));
    }

    let command = PathBuf::from(&parts[0]);
    let args = parts[1..].to_vec();

    // Generate a name from the command
    let name = command
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("agent")
        .to_string();

    Ok(AcpAgent {
        server: sacp::schema::McpServer::Stdio {
            name,
            command,
            args,
            env: vec![],
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_simple_command() {
        let agent = AcpAgent::from_str("python agent.py").unwrap();
        match agent.server {
            sacp::schema::McpServer::Stdio {
                name,
                command,
                args,
                env,
            } => {
                assert_eq!(name, "python");
                assert_eq!(command, PathBuf::from("python"));
                assert_eq!(args, vec!["agent.py"]);
                assert_eq!(env, vec![]);
            }
            _ => panic!("Expected Stdio variant"),
        }
    }

    #[test]
    fn test_parse_command_with_args() {
        let agent = AcpAgent::from_str("node server.js --port 8080 --verbose").unwrap();
        match agent.server {
            sacp::schema::McpServer::Stdio {
                name,
                command,
                args,
                env,
            } => {
                assert_eq!(name, "node");
                assert_eq!(command, PathBuf::from("node"));
                assert_eq!(args, vec!["server.js", "--port", "8080", "--verbose"]);
                assert_eq!(env, vec![]);
            }
            _ => panic!("Expected Stdio variant"),
        }
    }

    #[test]
    fn test_parse_command_with_quotes() {
        let agent = AcpAgent::from_str(r#"python "my agent.py" --name "Test Agent""#).unwrap();
        match agent.server {
            sacp::schema::McpServer::Stdio {
                name,
                command,
                args,
                env,
            } => {
                assert_eq!(name, "python");
                assert_eq!(command, PathBuf::from("python"));
                assert_eq!(args, vec!["my agent.py", "--name", "Test Agent"]);
                assert_eq!(env, vec![]);
            }
            _ => panic!("Expected Stdio variant"),
        }
    }

    #[test]
    fn test_parse_json_stdio() {
        let json = r#"{
            "type": "stdio",
            "name": "my-agent",
            "command": "/usr/bin/python",
            "args": ["agent.py", "--verbose"],
            "env": []
        }"#;
        let agent = AcpAgent::from_str(json).unwrap();
        match agent.server {
            sacp::schema::McpServer::Stdio {
                name,
                command,
                args,
                env,
            } => {
                assert_eq!(name, "my-agent");
                assert_eq!(command, PathBuf::from("/usr/bin/python"));
                assert_eq!(args, vec!["agent.py", "--verbose"]);
                assert_eq!(env, vec![]);
            }
            _ => panic!("Expected Stdio variant"),
        }
    }

    #[test]
    fn test_parse_json_http() {
        let json = r#"{
            "type": "http",
            "name": "remote-agent",
            "url": "https://example.com/agent",
            "headers": []
        }"#;
        let agent = AcpAgent::from_str(json).unwrap();
        match agent.server {
            sacp::schema::McpServer::Http { name, url, headers } => {
                assert_eq!(name, "remote-agent");
                assert_eq!(url, "https://example.com/agent");
                assert_eq!(headers, vec![]);
            }
            _ => panic!("Expected Http variant"),
        }
    }
}
