use futures::channel::oneshot;
use futures::future::BoxFuture;
use uuid::Uuid;

use crate::role::Role;
use crate::{ConnectionTo, Dispatch, HandleDispatchFrom, Handled};

/// Internal dyn-safe wrapper around [`HandleDispatchFrom`].
///
/// The type parameter is the role's counterpart (who we connect to).
pub(crate) trait DynHandleDispatchFrom<Counterpart: Role>: Send {
    fn dyn_handle_dispatch_from(
        &mut self,
        message: Dispatch,
        cx: ConnectionTo<Counterpart>,
    ) -> BoxFuture<'_, Result<Handled<Dispatch>, crate::Error>>;

    fn dyn_describe_chain(&self) -> String;
}

impl<Counterpart: Role, H: HandleDispatchFrom<Counterpart>> DynHandleDispatchFrom<Counterpart>
    for H
{
    fn dyn_handle_dispatch_from(
        &mut self,
        message: Dispatch,
        cx: ConnectionTo<Counterpart>,
    ) -> BoxFuture<'_, Result<Handled<Dispatch>, crate::Error>> {
        Box::pin(HandleDispatchFrom::handle_dispatch_from(self, message, cx))
    }

    fn dyn_describe_chain(&self) -> String {
        format!("{:?}", H::describe_chain(self))
    }
}

/// Messages used to add/remove dynamic handlers
pub(crate) enum DynamicHandlerMessage<Counterpart: Role> {
    AddDynamicHandler(Uuid, Box<dyn DynHandleDispatchFrom<Counterpart>>),
    RemoveDynamicHandler(Uuid),
    /// Marks the end of updates queued during ordered response processing.
    Barrier,
    /// Acknowledges after every preceding dynamic-handler message has been
    /// applied by the incoming protocol actor.
    AcknowledgedBarrier(oneshot::Sender<()>),
}

impl<Counterpart: Role> std::fmt::Debug for DynamicHandlerMessage<Counterpart> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::AddDynamicHandler(arg0, arg1) => f
                .debug_tuple("AddDynamicHandler")
                .field(arg0)
                .field(&arg1.dyn_describe_chain())
                .finish(),
            Self::RemoveDynamicHandler(arg0) => {
                f.debug_tuple("RemoveDynamicHandler").field(arg0).finish()
            }
            Self::Barrier => f.write_str("Barrier"),
            Self::AcknowledgedBarrier(_) => f.write_str("AcknowledgedBarrier"),
        }
    }
}
