use std::{
    any::Any,
    borrow::Cow,
    collections::HashMap,
    rc::Rc,
    sync::{
        Arc, Mutex,
        atomic::{AtomicI64, Ordering},
    },
};

use agent_client_protocol_schema::{
    Error, JsonRpcMessage, Notification, OutgoingMessage, Request, RequestId, Response, Result,
    Side,
};
use futures::{
    AsyncBufReadExt as _, AsyncRead, AsyncWrite, AsyncWriteExt as _, FutureExt as _,
    StreamExt as _,
    channel::{
        mpsc::{self, UnboundedReceiver, UnboundedSender},
        oneshot,
    },
    future::LocalBoxFuture,
    io::BufReader,
    select_biased,
};
use serde::{Deserialize, de::DeserializeOwned};
use serde_json::value::RawValue;

use super::stream_broadcast::{StreamBroadcast, StreamReceiver, StreamSender};

#[derive(Debug)]
pub(crate) struct RpcConnection<Local: Side, Remote: Side> {
    outgoing_tx: UnboundedSender<OutgoingMessage<Local, Remote>>,
    pending_responses: Arc<Mutex<HashMap<RequestId, PendingResponse>>>,
    next_id: AtomicI64,
    broadcast: StreamBroadcast,
}

#[derive(Debug)]
struct PendingResponse {
    deserialize: fn(&serde_json::value::RawValue) -> Result<Box<dyn Any + Send>>,
    respond: oneshot::Sender<Result<Box<dyn Any + Send>>>,
}

impl<Local, Remote> RpcConnection<Local, Remote>
where
    Local: Side + 'static,
    Remote: Side + 'static,
{
    pub(crate) fn new<Handler>(
        handler: Handler,
        outgoing_bytes: impl Unpin + AsyncWrite,
        incoming_bytes: impl Unpin + AsyncRead,
        spawn: impl Fn(LocalBoxFuture<'static, ()>) + 'static,
    ) -> (Self, impl futures::Future<Output = Result<()>>)
    where
        Handler: MessageHandler<Local> + 'static,
    {
        let (incoming_tx, incoming_rx) = mpsc::unbounded();
        let (outgoing_tx, outgoing_rx) = mpsc::unbounded();

        let pending_responses = Arc::new(Mutex::new(HashMap::default()));
        let (broadcast_tx, broadcast) = StreamBroadcast::new();

        let io_task = {
            let pending_responses = pending_responses.clone();
            async move {
                let result = Self::handle_io(
                    incoming_tx,
                    outgoing_rx,
                    outgoing_bytes,
                    incoming_bytes,
                    pending_responses.clone(),
                    broadcast_tx,
                )
                .await;
                pending_responses.lock().unwrap().clear();
                result
            }
        };

        Self::handle_incoming(outgoing_tx.clone(), incoming_rx, handler, spawn);

        let this = Self {
            outgoing_tx,
            pending_responses,
            next_id: AtomicI64::new(0),
            broadcast,
        };

        (this, io_task)
    }

    pub(crate) fn subscribe(&self) -> StreamReceiver {
        self.broadcast.receiver()
    }

    pub(crate) fn notify(
        &self,
        method: impl Into<Arc<str>>,
        params: Option<Remote::InNotification>,
    ) -> Result<()> {
        self.outgoing_tx
            .unbounded_send(OutgoingMessage::Notification(Notification {
                method: method.into(),
                params,
            }))
            .map_err(|_| Error::internal_error().data("failed to send notification"))
    }

    pub(crate) fn request<Out: DeserializeOwned + Send + 'static>(
        &self,
        method: impl Into<Arc<str>>,
        params: Option<Remote::InRequest>,
    ) -> impl Future<Output = Result<Out>> {
        let (tx, rx) = oneshot::channel();
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let id = RequestId::Number(id);

        if self
            .outgoing_tx
            .unbounded_send(OutgoingMessage::Request(Request {
                id: id.clone(),
                method: method.into(),
                params,
            }))
            .is_err()
        {
            return async move {
                drop(rx.await);
                Err(Error::internal_error().data("connection closed before request could be sent"))
            }.boxed();
        }

        self.pending_responses.lock().unwrap().insert(
            id,
            PendingResponse {
                deserialize: |value| {
                    serde_json::from_str::<Out>(value.get())
                        .map(|out| Box::new(out) as _)
                        .map_err(|_| Error::internal_error().data("failed to deserialize response"))
                },
                respond: tx,
            },
        );

        async move {
            let result = rx
                .await
                .map_err(|_| Error::internal_error().data("server shut down unexpectedly"))??
                .downcast::<Out>()
                .map_err(|_| Error::internal_error().data("failed to deserialize response"))?;

            Ok(*result)
        }.boxed()
    }

    async fn handle_io(
        incoming_tx: UnboundedSender<IncomingMessage<Local>>,
        mut outgoing_rx: UnboundedReceiver<OutgoingMessage<Local, Remote>>,
        mut outgoing_bytes: impl Unpin + AsyncWrite,
        incoming_bytes: impl Unpin + AsyncRead,
        pending_responses: Arc<Mutex<HashMap<RequestId, PendingResponse>>>,
        broadcast: StreamSender,
    ) -> Result<()> {
        // TODO: Create nicer abstraction for broadcast
        let mut input_reader = BufReader::new(incoming_bytes);
        let mut outgoing_line = Vec::new();
        let mut incoming_line = String::new();
        loop {
            select_biased! {
                message = outgoing_rx.next() => {
                    if let Some(message) = message {
                        outgoing_line.clear();
                        serde_json::to_writer(&mut outgoing_line, &JsonRpcMessage::wrap(&message)).map_err(Error::into_internal_error)?;
                        log::trace!("send: {}", String::from_utf8_lossy(&outgoing_line));
                        outgoing_line.push(b'\n');
                        outgoing_bytes.write_all(&outgoing_line).await.ok();
                        broadcast.outgoing(&message);
                    } else {
                        break;
                    }
                }
                bytes_read = input_reader.read_line(&mut incoming_line).fuse() => {
                    if bytes_read.map_err(Error::into_internal_error)? == 0 {
                        break
                    }
                    log::trace!("recv: {}", &incoming_line);

                    match serde_json::from_str::<RawIncomingMessage<'_>>(&incoming_line) {
                        Ok(message) => {
                            if let Some(id) = message.id {
                                if let Some(method) = message.method {
                                    // Request
                                    match Local::decode_request(&method, message.params) {
                                        Ok(request) => {
                                            broadcast.incoming_request(id.clone(), &*method, &request);
                                            incoming_tx.unbounded_send(IncomingMessage::Request { id, request }).ok();
                                        }
                                        Err(error) => {
                                            outgoing_line.clear();
                                            let error_response = OutgoingMessage::<Local, Remote>::Response(Response::Error {
                                                id,
                                                error,
                                            });

                                            serde_json::to_writer(&mut outgoing_line, &JsonRpcMessage::wrap(&error_response))?;
                                            log::trace!("send: {}", String::from_utf8_lossy(&outgoing_line));
                                            outgoing_line.push(b'\n');
                                            outgoing_bytes.write_all(&outgoing_line).await.ok();
                                            broadcast.outgoing(&error_response);
                                        }
                                    }
                                } else if let Some(pending_response) = pending_responses.lock().unwrap().remove(&id) {
                                    // Response
                                    if let Some(result_value) = message.result {
                                        broadcast.incoming_response(id, Ok(Some(result_value)));

                                        let result = (pending_response.deserialize)(result_value);
                                        pending_response.respond.send(result).ok();
                                    } else if let Some(error) = message.error {
                                        broadcast.incoming_response(id, Err(&error));

                                        pending_response.respond.send(Err(error)).ok();
                                    } else {
                                        broadcast.incoming_response(id, Ok(None));

                                        let result = (pending_response.deserialize)(&RawValue::from_string("null".into()).unwrap());
                                        pending_response.respond.send(result).ok();
                                    }
                                } else {
                                    log::error!("received response for unknown request id: {id:?}");
                                }
                            } else if let Some(method) = message.method {
                                // Notification
                                match Local::decode_notification(&method, message.params) {
                                    Ok(notification) => {
                                        broadcast.incoming_notification(&*method, &notification);
                                        incoming_tx.unbounded_send(IncomingMessage::Notification { notification }).ok();
                                    }
                                    Err(err) => {
                                        log::error!("failed to decode {:?}: {err}", message.params);
                                    }
                                }
                            } else {
                                log::error!("received message with neither id nor method");
                            }
                        }
                        Err(error) => {
                            log::error!("failed to parse incoming message: {error}. Raw: {incoming_line}");
                        }
                    }
                    incoming_line.clear();
                }
            }
        }
        Ok(())
    }

    fn handle_incoming<Handler: MessageHandler<Local> + 'static>(
        outgoing_tx: UnboundedSender<OutgoingMessage<Local, Remote>>,
        mut incoming_rx: UnboundedReceiver<IncomingMessage<Local>>,
        handler: Handler,
        spawn: impl Fn(LocalBoxFuture<'static, ()>) + 'static,
    ) {
        let spawn = Rc::new(spawn);
        let handler = Rc::new(handler);
        spawn({
            let spawn = spawn.clone();
            async move {
                while let Some(message) = incoming_rx.next().await {
                    match message {
                        IncomingMessage::Request { id, request } => {
                            let outgoing_tx = outgoing_tx.clone();
                            let handler = handler.clone();
                            spawn(
                                async move {
                                    let result = handler.handle_request(request).await;
                                    outgoing_tx
                                        .unbounded_send(OutgoingMessage::Response(Response::new(
                                            id, result,
                                        )))
                                        .ok();
                                }
                                .boxed_local(),
                            );
                        }
                        IncomingMessage::Notification { notification } => {
                            let handler = handler.clone();
                            spawn(
                                async move {
                                    if let Err(err) =
                                        handler.handle_notification(notification).await
                                    {
                                        log::error!("failed to handle notification: {err:?}");
                                    }
                                }
                                .boxed_local(),
                            );
                        }
                    }
                }
            }
            .boxed_local()
        });
    }
}

#[derive(Debug, Deserialize)]
pub struct RawIncomingMessage<'a> {
    id: Option<RequestId>,
    #[serde(borrow)]
    method: Option<Cow<'a, str>>,
    #[serde(borrow)]
    params: Option<&'a RawValue>,
    #[serde(borrow)]
    result: Option<&'a RawValue>,
    error: Option<Error>,
}

#[derive(Debug)]
pub enum IncomingMessage<Local: Side> {
    Request {
        id: RequestId,
        request: Local::InRequest,
    },
    Notification {
        notification: Local::InNotification,
    },
}

pub trait MessageHandler<Local: Side> {
    fn handle_request(
        &self,
        request: Local::InRequest,
    ) -> impl Future<Output = Result<Local::OutResponse>>;

    fn handle_notification(
        &self,
        notification: Local::InNotification,
    ) -> impl Future<Output = Result<()>>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_raw_incoming_message_with_escaped_slash() {
        // JSON with escaped forward slash in method name (valid per RFC 8259).
        // Some JSON encoders (especially behind WebSocket proxies) produce
        // `\/` instead of `/`.  The Cow<str> field in RawIncomingMessage ensures
        // serde can allocate a new String when unescaping is required.
        //
        // Before the fix, this would fail because `&'a str` cannot hold an
        // unescaped value that differs from the source bytes.
        let json_str = r#"{"jsonrpc":"2.0","id":1,"method":"session\/update","params":{}}"#;
        let parsed: RawIncomingMessage<'_> = serde_json::from_str(json_str).unwrap();
        assert_eq!(parsed.method.unwrap(), "session/update");
        assert_eq!(parsed.params.unwrap().to_string(), "{}");
    }

    #[test]
    fn test_raw_incoming_message_without_escape() {
        // Normal method name without escapes should still work (zero-copy borrow via Cow::Borrowed).
        let json_str = r#"{"jsonrpc":"2.0","id":2,"method":"session/update","params":{}}"#;
        let parsed: RawIncomingMessage<'_> = serde_json::from_str(json_str).unwrap();
        assert_eq!(parsed.method.unwrap(), "session/update");
        assert_eq!(parsed.params.unwrap().to_string(), "{}");
    }
}
