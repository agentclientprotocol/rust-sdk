//! Pattern: Bridge ordered SDK dispatch onto an application's executor.
//!
//! A notification handler finishing means the SDK has delivered that update,
//! not that a UI task has applied it. If handlers enqueue updates elsewhere,
//! enqueue response results and connection closure on **the same FIFO queue**.
//! Its single consumer applies each update before exposing the following
//! response to application code.
//!
//! This is useful for v2 resume: replay precedes the response on the wire, but
//! awaiting `block_task()` separately from an update queue does not drain that
//! queue. An [`on_receiving_result`] callback can enqueue a response marker
//! behind the replay and before later inbound traffic. Register it immediately,
//! before yielding or handing the request to another task: a response already
//! routed without a barrier cannot acquire one retroactively.
//!
//! # Example
//!
//! This observer resumes one session and delivers events until the peer closes
//! its incoming transport. Poll it on the application's executor; `apply` need
//! not be `Send` and is called only by the foreground consumer, never by an SDK
//! handler. The handlers capture queue senders, not application state or an
//! owning connection. Add interactive request handlers as shown in
//! [`connecting_as_client`](crate::connecting_as_client) for a complete client.
//!
//! ```
//! use std::path::PathBuf;
//! use agent_client_protocol::{Agent, Client, ConnectTo, Error, V2ConnectionTo};
//! use agent_client_protocol::schema::{ProtocolVersion, v2};
//! use futures::{channel::mpsc, StreamExt as _};
//!
//! enum ApplicationEvent {
//!     Update(Box<v2::UpdateSessionNotification>),
//!     ResumeFinished(Result<v2::ResumeSessionResponse, Error>),
//!     Closed,
//! }
//!
//! async fn observe_session(
//!     transport: impl ConnectTo<Client> + 'static,
//!     session_id: v2::SessionId,
//!     cwd: PathBuf,
//!     mut apply: impl FnMut(ApplicationEvent),
//! ) -> Result<(), Error> {
//!     let (events_tx, mut events_rx) = mpsc::unbounded();
//!     let updates_tx = events_tx.clone();
//!     let closed_tx = events_tx.clone();
//!
//!     Client.v2()
//!         .on_receive_notification(
//!             async move |update: v2::UpdateSessionNotification,
//!                         _connection: V2ConnectionTo<Agent>| {
//!                 // A dropped receiver means the application stopped observing.
//!                 drop(updates_tx.unbounded_send(ApplicationEvent::Update(Box::new(update))));
//!                 Ok(())
//!             },
//!             agent_client_protocol::on_receive_notification!(),
//!         )
//!         .on_close(async move |_connection| {
//!             drop(closed_tx.unbounded_send(ApplicationEvent::Closed));
//!             Ok(())
//!         })
//!         .connect_with(transport, async move |connection| {
//!             // Initialization does not need an application projection barrier.
//!             let initialized = connection.send_request(v2::InitializeRequest::new(
//!                 ProtocolVersion::V2,
//!                 v2::Implementation::new("ordered-client", "0.1.0"),
//!             )).block_task().await?;
//!             if initialized.capabilities.session.is_none() {
//!                 return Err(Error::invalid_params().data("agent has no session support"));
//!             }
//!
//!             let resume = v2::ResumeSessionRequest::new(session_id.clone(), cwd)
//!                 .replay_from(v2::ReplayFrom::from(v2::ReplayFromStart::new()));
//!             connection.send_request(resume).on_receiving_result(async move |result| {
//!                 drop(events_tx.unbounded_send(ApplicationEvent::ResumeFinished(result)));
//!                 // Do not wait for the consumer or for another inbound response here.
//!                 Ok(())
//!             })?;
//!
//!             while let Some(event) = events_rx.next().await {
//!                 if let ApplicationEvent::Update(update) = &event {
//!                     if update.session_id != session_id {
//!                         continue;
//!                     }
//!                 }
//!                 let closed = matches!(event, ApplicationEvent::Closed);
//!                 // Update: apply the patch. ResumeFinished: expose the result only
//!                 // now, after applying the preceding replay. Closed: invalidate
//!                 // remaining application operations and mark the connection closed.
//!                 apply(event);
//!                 if closed {
//!                     break;
//!                 }
//!             }
//!             Ok(())
//!         })
//!         .await
//! }
//! ```
//!
//! # Ordering and liveness
//!
//! - Await application work sequentially in the **consumer**, if needed. Merely
//!   spawning independent UI tasks for every event loses the application-side
//!   ordering again.
//! - An ordered callback must not await another inbound response, notification,
//!   or permission exchange on the same connection: dispatch is waiting for
//!   that callback to return. Use the foreground consumer or [`ConnectionTo::spawn`]
//!   for work needing later traffic. If a projection-drained acknowledgement is
//!   needed, send it from the consumer after applying the marker, not by making
//!   the SDK callback wait.
//! - [`on_close`] queues closure after already-dispatched notifications and
//!   timely ordered **wire responses**. EOF also fails pending requests, but
//!   those synthetic errors have no response barrier; their callbacks can run
//!   after `Closed`. Treat `Closed` as terminal for application operations and
//!   tolerate late completions rather than waiting for one callback per request.
//!   Transport or handler failures still propagate from `connect_with`.
//! - A callback registered after its response was routed, or a response routed
//!   later through a retained `ResponseRouter`, does not impose a barrier on
//!   subsequent wire traffic. See the SDK's [ordering contract].
//! - This unbounded queue keeps dispatch nonblocking for clarity. Production
//!   integrations need an explicit memory/backpressure policy. A bounded queue
//!   must not wait on a consumer that is itself awaiting later inbound traffic.
//! - A prompt response is an acceptance event, **not** prompt completion.
//!   `state_update` describes session-wide foreground state. Do not attribute
//!   the next `Idle` to a particular prompt or invent a turn boundary; v2 updates
//!   do not carry a prompt ID.
//!
//! [`on_receiving_result`]: agent_client_protocol::SentRequest::on_receiving_result
//! [`on_close`]: agent_client_protocol::Builder::on_close
//! [`ConnectionTo::spawn`]: agent_client_protocol::ConnectionTo::spawn
//! [ordering contract]: agent_client_protocol::concepts::ordering
