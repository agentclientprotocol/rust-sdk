use agent_client_protocol_core::{Channel, ConnectTo, DynConnectTo, Role, jsonrpcmsg};
use futures::StreamExt;
use futures_concurrency::future::TryJoin;

pub struct SnooperComponent<R: Role> {
    base_component: DynConnectTo<R>,
    incoming_message: Box<
        dyn FnMut(&jsonrpcmsg::Message) -> Result<(), agent_client_protocol_core::Error>
            + Send
            + Sync,
    >,
    outgoing_message: Box<
        dyn FnMut(&jsonrpcmsg::Message) -> Result<(), agent_client_protocol_core::Error>
            + Send
            + Sync,
    >,
}

impl<R: Role> SnooperComponent<R> {
    pub fn new(
        base_component: impl ConnectTo<R>,
        incoming_message: impl FnMut(
            &jsonrpcmsg::Message,
        ) -> Result<(), agent_client_protocol_core::Error>
        + Send
        + Sync
        + 'static,
        outgoing_message: impl FnMut(
            &jsonrpcmsg::Message,
        ) -> Result<(), agent_client_protocol_core::Error>
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
        mut self,
        client: impl ConnectTo<R::Counterpart>,
    ) -> Result<(), agent_client_protocol_core::Error> {
        let (client_a, mut client_b) = Channel::duplex();

        let client_future = client.connect_to(client_a);

        let (mut base_channel, base_future) = self.base_component.into_channel_and_future();

        // Read messages send by `client`. These are 'incoming' to our wrapped
        // component.
        let snoop_incoming = async {
            while let Some(msg) = client_b.rx.next().await {
                if let Ok(msg) = &msg {
                    (self.incoming_message)(msg)?;
                }

                base_channel
                    .tx
                    .unbounded_send(msg)
                    .map_err(agent_client_protocol_core::util::internal_error)?;
            }
            Ok(())
        };

        // Read messages send by `base`. These are 'outgoing' from our wrapped
        // component.
        let snoop_outoing = async {
            while let Some(msg) = base_channel.rx.next().await {
                if let Ok(msg) = &msg {
                    (self.outgoing_message)(msg)?;
                }

                client_b
                    .tx
                    .unbounded_send(msg)
                    .map_err(agent_client_protocol_core::util::internal_error)?;
            }
            Ok(())
        };

        (client_future, base_future, snoop_incoming, snoop_outoing)
            .try_join()
            .await?;
        Ok(())
    }
}
