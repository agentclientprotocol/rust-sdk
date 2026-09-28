use std::time::Duration;

use agent_client_protocol::{
    BudgetedFrame, Channel, ConnectionLimits, Error, JsonRpcRequest, JsonRpcResponse,
    RawJsonRpcMessage, TransportFrame, UntypedRole,
};
use futures::{StreamExt as _, channel::oneshot};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, JsonRpcRequest)]
#[request(method = "_test/callback", response = CallbackResponse)]
struct CallbackRequest {}

#[derive(Debug, Clone, Serialize, Deserialize, JsonRpcResponse)]
struct CallbackResponse {}

#[tokio::test]
async fn persistent_child_cannot_strand_an_ordered_response_callback() -> Result<(), Error> {
    tokio::time::timeout(Duration::from_secs(10), async {
        let (transport, mut peer) = Channel::duplex_with_limits(ConnectionLimits {
            max_queued_frames: 1,
            ..ConnectionLimits::default()
        });
        let (published_tx, published_rx) = oneshot::channel();
        let (reply_tx, reply_rx) = oneshot::channel::<()>();
        let (child_transport, child_peer) = Channel::duplex();
        let connection = UntypedRole
            .builder()
            .connect_with(transport, async move |cx| {
                let _child = cx.spawn_connection(UntypedRole.builder(), child_transport)?;
                let (callback_tx, callback_rx) = oneshot::channel();
                let request = cx.send_request(CallbackRequest {});
                published_rx.await.map_err(Error::into_internal_error)?;
                let error = request
                    .on_receiving_result(async move |_result| {
                        let _ = callback_tx.send(());
                        Ok(())
                    })
                    .expect_err("the child reserves the sole live task slot");
                assert!(error.to_string().contains("live task capacity"), "{error}");
                assert!(
                    callback_rx.await.is_err(),
                    "rejected callback must release its captured resources"
                );
                let _ = reply_tx.send(());
                cx.incoming_closed().await;
                Ok(())
            });
        let reply_and_close = async move {
            let request = loop {
                match peer.rx.next().await.map(BudgetedFrame::into_frame) {
                    Some(TransportFrame::Single(RawJsonRpcMessage::Request(request))) => {
                        break request;
                    }
                    Some(TransportFrame::Single(RawJsonRpcMessage::Notification(_))) => {}
                    other => panic!("parent request did not reach the peer: {other:?}"),
                }
            };
            let _ = published_tx.send(());
            let _ = reply_rx.await;
            peer.tx
                .send_frame(TransportFrame::Single(RawJsonRpcMessage::response(
                    request.id,
                    Ok(serde_json::json!({})),
                )))
                .await
                .unwrap();
            // Half-close input but continue draining any cancellation/output.
            // Dropping both directions here would make a legitimate late write
            // fail for reasons unrelated to the response acknowledgment.
            peer.tx.close_channel();
            while peer.rx.next().await.is_some() {}
        };
        let (connection, ()) = futures::future::join(connection, reply_and_close).await;
        drop(child_peer);
        connection
    })
    .await
    .expect("persistent child stranded the response dispatcher")
}
