//! Regression test for stack overflow with deeply chained handlers.
//!
//! Consumers like Zed register 10-20 typed handlers on a single connection,
//! producing a tower of nested `ChainedHandler` futures. In unoptimized
//! (debug) builds, polling that tower used to consume a large stack frame per
//! chain link, overflowing threads with small fixed stacks — notably macOS
//! GCD dispatch threads, which have 512 KiB and ignore `RUST_MIN_STACK`.
//! See zed-industries/zed#61840.
//!
//! This test drives a realistically deep handler chain on a thread restricted
//! to 512 KiB of stack, in the spirit of that environment. It aborts the
//! process (stack overflow) if dispatch frames regress.

use agent_client_protocol::role::UntypedRole;
use agent_client_protocol::{
    ConnectionTo, JsonRpcMessage, JsonRpcNotification, JsonRpcRequest, JsonRpcResponse, Responder,
    SentRequest,
};
use serde::{Deserialize, Serialize};
use tokio_util::compat::{TokioAsyncReadCompatExt, TokioAsyncWriteCompatExt};

/// The stack size of a macOS GCD (libdispatch) worker thread, the smallest
/// stack the dispatch loop is known to run on in the wild.
const GCD_STACK_SIZE: usize = 512 * 1024;

/// Test helper to block and wait for a JSON-RPC response.
async fn recv<T: JsonRpcResponse + Send>(
    response: SentRequest<T>,
) -> Result<T, agent_client_protocol::Error> {
    let (tx, rx) = tokio::sync::oneshot::channel();
    response.on_receiving_result(async move |result| {
        tx.send(result)
            .map_err(|_| agent_client_protocol::Error::internal_error())
    })?;
    rx.await
        .map_err(|_| agent_client_protocol::Error::internal_error())?
}

macro_rules! test_message {
    ($name:ident, $method:literal) => {
        #[derive(Debug, Clone, Serialize, Deserialize)]
        struct $name {
            message: String,
        }

        impl JsonRpcMessage for $name {
            fn matches_method(method: &str) -> bool {
                method == $method
            }

            fn method(&self) -> &'static str {
                $method
            }

            fn to_untyped_message(
                &self,
            ) -> Result<agent_client_protocol::UntypedMessage, agent_client_protocol::Error> {
                agent_client_protocol::UntypedMessage::new(self.method(), self)
            }

            fn parse_message(
                method: &str,
                params: &impl serde::Serialize,
            ) -> Result<Self, agent_client_protocol::Error> {
                if !Self::matches_method(method) {
                    return Err(agent_client_protocol::Error::method_not_found());
                }
                agent_client_protocol::util::json_cast(params)
            }
        }
    };
}

test_message!(PingRequest, "ping");
test_message!(NopRequest, "nop");
test_message!(NopNotification, "nop_notification");

impl JsonRpcRequest for PingRequest {
    type Response = PongResponse;
}

impl JsonRpcRequest for NopRequest {
    type Response = PongResponse;
}

impl JsonRpcNotification for NopNotification {}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct PongResponse {
    echo: String,
}

impl JsonRpcResponse for PongResponse {
    fn into_json(self, _method: &str) -> Result<serde_json::Value, agent_client_protocol::Error> {
        serde_json::to_value(self).map_err(agent_client_protocol::Error::into_internal_error)
    }

    fn from_value(
        _method: &str,
        value: serde_json::Value,
    ) -> Result<Self, agent_client_protocol::Error> {
        agent_client_protocol::util::json_cast(&value)
    }
}

/// Registers `PingRequest` first (innermost link, so a "ping" traverses the
/// entire chain before it is claimed), then a pile of handlers for messages
/// that are never sent — mirroring a consumer that registers a callback per
/// protocol method.
macro_rules! deep_chain_server {
    ($($_idx:literal),*) => {
        UntypedRole
            .builder()
            .on_receive_request(
                async |request: PingRequest,
                       responder: Responder<PongResponse>,
                       _connection: ConnectionTo<UntypedRole>| {
                    responder.respond(PongResponse {
                        echo: format!("pong: {}", request.message),
                    })
                },
                agent_client_protocol::on_receive_request!(),
            )
            $(
                .on_receive_request(
                    async |request: NopRequest,
                           responder: Responder<PongResponse>,
                           _connection: ConnectionTo<UntypedRole>| {
                        let _ = $_idx;
                        responder.respond(PongResponse {
                            echo: request.message,
                        })
                    },
                    agent_client_protocol::on_receive_request!(),
                )
                .on_receive_notification(
                    async |_notification: NopNotification, _cx: ConnectionTo<UntypedRole>| {
                        Ok(())
                    },
                    agent_client_protocol::on_receive_notification!(),
                )
            )*
    };
}

#[test]
fn deep_handler_chain_within_small_stack() {
    let (client_io, server_io) = tokio::io::duplex(1024);

    // The server side runs its whole dispatch loop on a thread with the same
    // fixed stack budget as a macOS GCD worker.
    let server = std::thread::Builder::new()
        .name("small-stack-dispatch".to_string())
        .stack_size(GCD_STACK_SIZE)
        .spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("failed to build server runtime");
            let local = tokio::task::LocalSet::new();
            local.block_on(&runtime, async move {
                let (server_reader, server_writer) = tokio::io::split(server_io);
                let transport = agent_client_protocol::ByteStreams::new(
                    server_writer.compat_write(),
                    server_reader.compat(),
                );
                // 16 request + 16 notification handlers plus the ping handler:
                // deeper than a full ACP client/agent registration. Before
                // the chain links were boxed, this many handlers did not
                // even compile (layout query depth overflow), and half as
                // many overflowed the stack below at runtime.
                let server =
                    deep_chain_server!(1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16);
                server.connect_to(transport).await
            })
        })
        .expect("failed to spawn small-stack thread");

    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("failed to build client runtime");
    let local = tokio::task::LocalSet::new();
    let result = local.block_on(&runtime, async move {
        let (client_reader, client_writer) = tokio::io::split(client_io);
        let transport = agent_client_protocol::ByteStreams::new(
            client_writer.compat_write(),
            client_reader.compat(),
        );
        UntypedRole
            .builder()
            .connect_with(transport, async |cx| {
                let response = recv(cx.send_request(PingRequest {
                    message: "hello".to_string(),
                }))
                .await?;
                assert_eq!(response.echo, "pong: hello");
                Ok(())
            })
            .await
    });
    assert!(result.is_ok(), "client failed: {result:?}");

    let server_result = server.join().expect("server thread panicked");
    assert!(server_result.is_ok(), "server failed: {server_result:?}");
}
