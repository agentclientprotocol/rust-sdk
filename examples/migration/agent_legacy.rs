//! Simple agent implementation using the legacy `agent-client-protocol` crate.
//!
//! This example shows the traditional trait-based approach for building agents.

use agent_client_protocol::{Agent, AgentSideConnection, Client};
use agent_client_protocol_schema::{
    AuthenticateRequest, AuthenticateResponse, CancelNotification, Error, ExtNotification,
    ExtRequest, ExtResponse, InitializeRequest, InitializeResponse, LoadSessionRequest,
    LoadSessionResponse, NewSessionRequest, NewSessionResponse, PromptRequest, PromptResponse,
    ReadTextFileRequest, ReadTextFileResponse, Result, SetSessionModeRequest,
    SetSessionModeResponse, StopReason, WriteTextFileRequest, WriteTextFileResponse,
};
use futures::FutureExt;
use tokio_util::compat::{TokioAsyncReadCompatExt, TokioAsyncWriteCompatExt};

// ANCHOR: setup
/// A simple agent implementation using the trait-based approach.
struct MyAgent {
    name: String,
}

#[async_trait::async_trait(?Send)]
impl Agent for MyAgent {
    async fn initialize(&self, args: InitializeRequest) -> Result<InitializeResponse> {
        eprintln!("[{}] Initializing", self.name);
        Ok(InitializeResponse::new(args.protocol_version))
    }

    async fn authenticate(&self, _args: AuthenticateRequest) -> Result<AuthenticateResponse> {
        Err(Error::method_not_found())
    }

    async fn new_session(&self, _args: NewSessionRequest) -> Result<NewSessionResponse> {
        eprintln!("[{}] Creating session", self.name);
        Ok(NewSessionResponse::new("session-1"))
    }

    async fn prompt(&self, _args: PromptRequest) -> Result<PromptResponse> {
        eprintln!("[{}] Processing prompt", self.name);
        Ok(PromptResponse::new(StopReason::EndTurn))
    }

    async fn cancel(&self, _args: CancelNotification) -> Result<()> {
        Ok(())
    }

    async fn set_session_mode(
        &self,
        _args: SetSessionModeRequest,
    ) -> Result<SetSessionModeResponse> {
        Err(Error::method_not_found())
    }

    async fn load_session(&self, _args: LoadSessionRequest) -> Result<LoadSessionResponse> {
        Err(Error::method_not_found())
    }

    async fn ext_method(&self, _args: ExtRequest) -> Result<ExtResponse> {
        Err(Error::method_not_found())
    }

    async fn ext_notification(&self, _args: ExtNotification) -> Result<()> {
        Ok(())
    }
}
// ANCHOR_END: setup

// ANCHOR: client_trait
/// Implement the Client trait to handle requests from the agent.
#[async_trait::async_trait(?Send)]
impl Client for MyAgent {
    async fn write_text_file(&self, args: WriteTextFileRequest) -> Result<WriteTextFileResponse> {
        eprintln!("[{}] Writing file: {}", self.name, args.path.display());
        Err(Error::method_not_found())
    }

    async fn read_text_file(&self, args: ReadTextFileRequest) -> Result<ReadTextFileResponse> {
        eprintln!("[{}] Reading file: {}", self.name, args.path.display());
        Err(Error::method_not_found())
    }

    // ... other Client methods omitted for brevity
    // ANCHOR: client_trait_omitted
    async fn request_permission(
        &self,
        _args: agent_client_protocol_schema::RequestPermissionRequest,
    ) -> Result<agent_client_protocol_schema::RequestPermissionResponse> {
        Err(Error::method_not_found())
    }

    async fn create_terminal(
        &self,
        _args: agent_client_protocol_schema::CreateTerminalRequest,
    ) -> Result<agent_client_protocol_schema::CreateTerminalResponse> {
        Err(Error::method_not_found())
    }

    async fn terminal_output(
        &self,
        _args: agent_client_protocol_schema::TerminalOutputRequest,
    ) -> Result<agent_client_protocol_schema::TerminalOutputResponse> {
        Err(Error::method_not_found())
    }

    async fn release_terminal(
        &self,
        _args: agent_client_protocol_schema::ReleaseTerminalRequest,
    ) -> Result<agent_client_protocol_schema::ReleaseTerminalResponse> {
        Err(Error::method_not_found())
    }

    async fn wait_for_terminal_exit(
        &self,
        _args: agent_client_protocol_schema::WaitForTerminalExitRequest,
    ) -> Result<agent_client_protocol_schema::WaitForTerminalExitResponse> {
        Err(Error::method_not_found())
    }

    async fn kill_terminal_command(
        &self,
        _args: agent_client_protocol_schema::KillTerminalCommandRequest,
    ) -> Result<agent_client_protocol_schema::KillTerminalCommandResponse> {
        Err(Error::method_not_found())
    }

    async fn session_notification(
        &self,
        _args: agent_client_protocol_schema::SessionNotification,
    ) -> Result<()> {
        Ok(())
    }

    async fn ext_method(&self, _args: ExtRequest) -> Result<ExtResponse> {
        Err(Error::method_not_found())
    }

    async fn ext_notification(&self, _args: ExtNotification) -> Result<()> {
        Ok(())
    }
    // ANCHOR_END: client_trait_omitted
}
// ANCHOR_END: client_trait

// ANCHOR: connection
#[tokio::main]
async fn main() -> Result<()> {
    // The legacy SDK uses ?Send futures, so we need a LocalSet
    let local = tokio::task::LocalSet::new();

    local
        .run_until(async move {
            let agent = MyAgent {
                name: "legacy-agent".into(),
            };

            // Create the connection with explicit spawn function
            let (_connection, io_task) = AgentSideConnection::new(
                agent,
                tokio::io::stdout().compat_write(),
                tokio::io::stdin().compat(),
                |fut| {
                    tokio::task::spawn_local(fut.map(|_| ()));
                },
            );

            // Spawn the IO task
            tokio::task::spawn_local(async move {
                if let Err(e) = io_task.await {
                    eprintln!("IO task error: {}", e);
                }
            });

            // Connection is now ready to use
            // In a real agent, you might want to store `connection` to send notifications
            std::future::pending::<()>().await;

            Ok(())
        })
        .await
}
// ANCHOR_END: connection
