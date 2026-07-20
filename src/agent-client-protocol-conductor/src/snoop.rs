use agent_client_protocol::{ConnectTo, DynConnectTo, FramedChannel, RawJsonRpcMessage, Role};
use futures_concurrency::future::TryJoin;

pub struct SnooperComponent<R: Role> {
    base_component: DynConnectTo<R>,
    incoming_message: Box<
        dyn FnMut(&RawJsonRpcMessage) -> Result<(), agent_client_protocol::Error> + Send + Sync,
    >,
    outgoing_message: Box<
        dyn FnMut(&RawJsonRpcMessage) -> Result<(), agent_client_protocol::Error> + Send + Sync,
    >,
}

impl<R: Role> SnooperComponent<R> {
    pub fn new(
        base_component: impl ConnectTo<R>,
        incoming_message: impl FnMut(&RawJsonRpcMessage) -> Result<(), agent_client_protocol::Error>
        + Send
        + Sync
        + 'static,
        outgoing_message: impl FnMut(&RawJsonRpcMessage) -> Result<(), agent_client_protocol::Error>
        + Send
        + Sync
        + 'static,
    ) -> Self {
        Self {
            base_component: DynConnectTo::new(base_component),
            incoming_message: Box::new(incoming_message),
            outgoing_message: Box::new(outgoing_message),
        }
    }
}

impl<R: Role> ConnectTo<R> for SnooperComponent<R> {
    async fn connect_to(
        self,
        client: impl ConnectTo<R::Counterpart>,
    ) -> Result<(), agent_client_protocol::Error> {
        let (client_channel, client_future) = client.into_framed_channel_and_future();
        let (base_channel, base_future) = self.base_component.into_framed_channel_and_future();
        let snoop = FramedChannel::bridge_with_inspection(
            client_channel,
            base_channel,
            self.incoming_message,
            self.outgoing_message,
        );

        (client_future, base_future, snoop).try_join().await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::{
        sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
        },
        time::Duration,
    };

    use agent_client_protocol::{ByteStreams, ConnectionTo, Responder, UntypedRole};
    use agent_client_protocol_test::{MyRequest, MyResponse};
    use serde_json::{Value, json};
    use tokio::io::{AsyncBufReadExt as _, AsyncWriteExt as _, BufReader};
    use tokio_util::compat::{TokioAsyncReadCompatExt as _, TokioAsyncWriteCompatExt as _};

    use super::*;

    const TIMEOUT: Duration = Duration::from_secs(10);

    #[tokio::test(flavor = "current_thread")]
    async fn tracing_preserves_json_rpc_batch_frames() {
        tokio::task::LocalSet::new()
            .run_until(async {
                let incoming_count = Arc::new(AtomicUsize::new(0));
                let outgoing_count = Arc::new(AtomicUsize::new(0));
                let observed_incoming = Arc::clone(&incoming_count);
                let observed_outgoing = Arc::clone(&outgoing_count);

                let (mut peer_writer, component_reader) = tokio::io::duplex(8192);
                let (component_writer, peer_reader) = tokio::io::duplex(8192);
                let transport =
                    ByteStreams::new(component_writer.compat_write(), component_reader.compat());
                let snooper = SnooperComponent::new(
                    transport,
                    move |_| {
                        observed_incoming.fetch_add(1, Ordering::SeqCst);
                        Ok(())
                    },
                    move |_| {
                        observed_outgoing.fetch_add(1, Ordering::SeqCst);
                        Ok(())
                    },
                );
                let server = UntypedRole.builder().on_receive_request(
                    async |_request: MyRequest,
                           responder: Responder<MyResponse>,
                           _cx: ConnectionTo<UntypedRole>| {
                        responder.respond(MyResponse {
                            status: "received".into(),
                        })
                    },
                    agent_client_protocol::on_receive_request!(),
                );
                let server_task = tokio::task::spawn_local(server.connect_to(snooper));

                let mut bytes = serde_json::to_vec(&json!([
                    { "jsonrpc": "2.0", "id": 1, "method": "myRequest", "params": {} },
                    { "jsonrpc": "2.0", "id": 2, "method": "myRequest", "params": {} }
                ]))
                .expect("batch should serialize");
                bytes.push(b'\n');
                peer_writer
                    .write_all(&bytes)
                    .await
                    .expect("batch write should succeed");

                let mut peer_reader = BufReader::new(peer_reader);
                let mut line = String::new();
                tokio::time::timeout(TIMEOUT, peer_reader.read_line(&mut line))
                    .await
                    .expect("timed out waiting for traced batch response")
                    .expect("batch response read should succeed");
                let response: Value =
                    serde_json::from_str(line.trim()).expect("response should be valid JSON");
                let responses = response
                    .as_array()
                    .expect("tracing must preserve one response array");
                assert_eq!(responses.len(), 2);
                assert_eq!(incoming_count.load(Ordering::SeqCst), 2);
                assert_eq!(outgoing_count.load(Ordering::SeqCst), 2);

                drop(peer_writer);
                drop(peer_reader);
                tokio::time::timeout(TIMEOUT, server_task)
                    .await
                    .expect("traced server did not stop after EOF")
                    .expect("traced server task panicked")
                    .expect("traced server connection failed");
            })
            .await;
    }
}
