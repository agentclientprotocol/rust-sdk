//! Simple client implementation using the legacy `agent-client-protocol` crate.
//!
//! This example shows how to connect to an agent and send requests.

use agent_client_protocol::{
    Agent, Client, ClientSideConnection, CreateTerminalRequest, CreateTerminalResponse, Error,
    ExtNotification, ExtRequest, ExtResponse, InitializeRequest, KillTerminalCommandRequest,
    KillTerminalCommandResponse, NewSessionRequest, PromptRequest, ProtocolVersion,
    ReadTextFileRequest, ReadTextFileResponse, ReleaseTerminalRequest, ReleaseTerminalResponse,
    RequestPermissionRequest, RequestPermissionResponse, Result, SessionNotification,
    TerminalOutputRequest, TerminalOutputResponse, WaitForTerminalExitRequest,
    WaitForTerminalExitResponse, WriteTextFileRequest, WriteTextFileResponse,
};
use futures::FutureExt;
use tokio_util::compat::{TokioAsyncReadCompatExt, TokioAsyncWriteCompatExt};

// ANCHOR: client_impl
/// A simple client implementation using the trait-based approach.
struct MyClient {
    name: String,
}

#[async_trait::async_trait(?Send)]
impl Client for MyClient {
    async fn request_permission(
        &self,
        _args: RequestPermissionRequest,
    ) -> Result<RequestPermissionResponse> {
        eprintln!("[{}] Permission requested", self.name);
        Err(Error::method_not_found())
    }

    async fn write_text_file(&self, _args: WriteTextFileRequest) -> Result<WriteTextFileResponse> {
        eprintln!("[{}] Writing file", self.name);
        Err(Error::method_not_found())
    }

    async fn read_text_file(&self, _args: ReadTextFileRequest) -> Result<ReadTextFileResponse> {
        eprintln!("[{}] Reading file", self.name);
        Err(Error::method_not_found())
    }

    async fn session_notification(&self, args: SessionNotification) -> Result<()> {
        eprintln!("[{}] Session update: {:?}", self.name, args.update);
        Ok(())
    }

    // ... other Client methods omitted for brevity
    // ANCHOR: client_impl_omitted
    async fn create_terminal(
        &self,
        _args: CreateTerminalRequest,
    ) -> Result<CreateTerminalResponse> {
        Err(Error::method_not_found())
    }

    async fn terminal_output(
        &self,
        _args: TerminalOutputRequest,
    ) -> Result<TerminalOutputResponse> {
        Err(Error::method_not_found())
    }

    async fn release_terminal(
        &self,
        _args: ReleaseTerminalRequest,
    ) -> Result<ReleaseTerminalResponse> {
        Err(Error::method_not_found())
    }

    async fn wait_for_terminal_exit(
        &self,
        _args: WaitForTerminalExitRequest,
    ) -> Result<WaitForTerminalExitResponse> {
        Err(Error::method_not_found())
    }

    async fn kill_terminal_command(
        &self,
        _args: KillTerminalCommandRequest,
    ) -> Result<KillTerminalCommandResponse> {
        Err(Error::method_not_found())
    }

    async fn ext_method(&self, _args: ExtRequest) -> Result<ExtResponse> {
        Err(Error::method_not_found())
    }

    async fn ext_notification(&self, _args: ExtNotification) -> Result<()> {
        Ok(())
    }
    // ANCHOR_END: client_impl_omitted
}
// ANCHOR_END: client_impl

// ANCHOR: connection_and_requests
#[tokio::main]
async fn main() -> Result<()> {
    // The legacy SDK uses ?Send futures, so we need a LocalSet
    let local = tokio::task::LocalSet::new();

    local
        .run_until(async move {
            let client = MyClient {
                name: "legacy-client".into(),
            };

            // Create the connection
            let (connection, io_task) = ClientSideConnection::new(
                client,
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

            // Now we can send requests using the connection
            // ANCHOR: send_requests
            // Initialize the agent
            let init_response = connection
                .initialize(InitializeRequest::new(ProtocolVersion::LATEST))
                .await?;

            eprintln!("Agent initialized: {:?}", init_response.agent_info);

            // Create a session
            let session_response = connection.new_session(NewSessionRequest::new("/")).await?;

            eprintln!("Session created: {}", session_response.session_id);

            // Send a prompt
            let prompt_response = connection
                .prompt(PromptRequest::new(session_response.session_id, vec![]))
                .await?;

            eprintln!("Prompt completed: {:?}", prompt_response.stop_reason);
            // ANCHOR_END: send_requests

            Ok(())
        })
        .await
}
// ANCHOR_END: connection_and_requests
