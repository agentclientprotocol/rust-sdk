//! Application-owned resume/replay/close coordination, not a public SDK API.
//!
//! Run with an agent that can resume an existing session:
//! `cargo run -p agent-client-protocol --features unstable_protocol_v2
//! --example v2_session_coordination -- --command my-agent --session-id ID`
//!
//! See md/session-operation-coordination.md for the policies and limitations.

use std::{
    cell::{Cell, RefCell},
    collections::{HashMap, HashSet},
    path::PathBuf,
    rc::{Rc, Weak},
    str::FromStr,
};

use agent_client_protocol::{
    AcpAgent, Agent, Client, ConnectTo, Error, Responder, V2ConnectionTo,
    schema::{ProtocolVersion, v2},
};
use clap::Parser;
use futures::{
    FutureExt as _, StreamExt as _,
    channel::{mpsc, oneshot},
    future::Either,
};

type View = Rc<RefCell<Projection>>;
type Reply = oneshot::Sender<Result<View, Error>>;
type Waiters = HashMap<u64, Reply>;

/// A lossless update log, deliberately not a message/tool reducer.
struct Projection {
    response: v2::ResumeSessionResponse,
    updates: Vec<v2::SessionUpdate>,
    disconnected: bool,
}

#[derive(Clone)]
struct Sessions {
    commands: mpsc::UnboundedSender<Command>,
    next_ticket: Rc<Cell<u64>>,
}

impl Sessions {
    /// Register synchronously, so even an unpolled load has an abandonment guard.
    fn load(&self, session_id: v2::SessionId) -> Result<Load, Error> {
        let ticket = self.next_ticket.get();
        self.next_ticket
            .set(ticket.checked_add(1).expect("ticket space exhausted"));
        let (reply, response) = oneshot::channel();
        self.commands
            .unbounded_send(Command::Acquire {
                session_id: session_id.clone(),
                ticket,
                reply,
            })
            .map_err(|_| stopped())?;
        Ok(Load {
            response,
            lease: Rc::new(Lease {
                commands: self.commands.clone(),
                session_id,
                ticket,
            }),
        })
    }
}

struct Lease {
    commands: mpsc::UnboundedSender<Command>,
    session_id: v2::SessionId,
    ticket: u64,
}

impl Drop for Lease {
    fn drop(&mut self) {
        // Receiver loss means the connection driver has already stopped.
        drop(self.commands.unbounded_send(Command::Release {
            session_id: self.session_id.clone(),
            ticket: self.ticket,
        }));
    }
}

struct Load {
    response: oneshot::Receiver<Result<View, Error>>,
    lease: Rc<Lease>,
}

impl Load {
    async fn wait(self) -> Result<Session, Error> {
        let view = self.response.await.map_err(|_| stopped())??;
        Ok(Session {
            view,
            _lease: self.lease,
        })
    }
}

#[derive(Clone)]
struct Session {
    view: View,
    // Clones share one lease; only the last clone releases it.
    _lease: Rc<Lease>,
}

enum Command {
    Acquire {
        session_id: v2::SessionId,
        ticket: u64,
        reply: Reply,
    },
    Release {
        session_id: v2::SessionId,
        ticket: u64,
    },
}

/// All inbound traffic uses one FIFO. No event contains an application owner.
enum WireEvent {
    Update(Box<v2::UpdateSessionNotification>),
    Resumed(v2::SessionId, Result<v2::ResumeSessionResponse, Error>),
    ClosedSession(v2::SessionId, Result<v2::CloseSessionResponse, Error>),
    Disconnected,
}

enum Phase {
    Resuming {
        waiters: Waiters,
        // None means abandoned. Discard remaining replay, but keep the wire slot.
        replay: Option<Vec<v2::SessionUpdate>>,
    },
    Ready {
        leases: HashSet<u64>,
        view: Weak<RefCell<Projection>>,
    },
    Closing,
    // A failed close leaves remote state uncertain. Do not automatically reopen.
    Blocked(Error),
}

struct Operation {
    phase: Phase,
    next: Waiters,
}

enum Effect {
    Resume(v2::SessionId),
    Close(v2::SessionId),
}

struct Driver {
    operations: HashMap<v2::SessionId, Operation>,
    commands: mpsc::UnboundedReceiver<Command>,
    incoming: mpsc::UnboundedReceiver<WireEvent>,
    wire_events: mpsc::UnboundedSender<WireEvent>,
    cwd: PathBuf,
}

impl Driver {
    async fn run(mut self, connection: V2ConnectionTo<Agent>) -> Result<(), Error> {
        loop {
            // Honor already-queued acquisition/release decisions before exposing
            // a response. Wire events still retain their own FIFO order.
            let event = futures::select_biased! {
                command = self.commands.next().fuse() => Either::Left(command),
                wire = self.incoming.next().fuse() => Either::Right(wire),
            };
            let effect = match event {
                // Only application handles/leases own command senders. In particular,
                // pending SDK callbacks cannot keep this branch from being reached.
                Either::Left(None) | Either::Right(None | Some(WireEvent::Disconnected)) => {
                    return Ok(());
                }
                Either::Left(Some(command)) => self.command(command),
                Either::Right(Some(wire)) => self.wire(wire),
            };
            if let Some(effect) = effect {
                self.send(effect, &connection)?;
            }
        }
    }

    fn command(&mut self, command: Command) -> Option<Effect> {
        match command {
            Command::Acquire {
                session_id,
                ticket,
                reply,
            } => {
                if reply.is_canceled() {
                    return None;
                }
                let Some(operation) = self.operations.get_mut(&session_id) else {
                    return self.resume(session_id, HashMap::from([(ticket, reply)]));
                };
                match &mut operation.phase {
                    Phase::Resuming {
                        waiters,
                        replay: Some(_),
                    } => {
                        waiters.insert(ticket, reply);
                    }
                    Phase::Ready { leases, view } => {
                        if let Some(view) = view.upgrade() {
                            if reply.send(Ok(view)).is_ok() {
                                leases.insert(ticket);
                            }
                        } else {
                            // The last delivered-but-unconsumed load may have dropped
                            // before its Release command was processed.
                            operation.next.insert(ticket, reply);
                            operation.phase = Phase::Closing;
                            return Some(Effect::Close(session_id));
                        }
                    }
                    Phase::Resuming { replay: None, .. } | Phase::Closing => {
                        operation.next.insert(ticket, reply);
                    }
                    Phase::Blocked(error) => drop(reply.send(Err(error.clone()))),
                }
            }
            Command::Release { session_id, ticket } => {
                let operation = self.operations.get_mut(&session_id)?;
                operation.next.remove(&ticket);
                match &mut operation.phase {
                    Phase::Resuming { waiters, replay } => {
                        waiters.remove(&ticket);
                        if waiters.is_empty() {
                            // Retain only the unfinished operation, not its accumulated data.
                            *replay = None;
                        }
                    }
                    Phase::Ready { leases, .. } => {
                        leases.remove(&ticket);
                        if leases.is_empty() {
                            operation.phase = Phase::Closing;
                            return Some(Effect::Close(session_id));
                        }
                    }
                    Phase::Closing | Phase::Blocked(_) => {}
                }
            }
        }
        None
    }

    fn wire(&mut self, event: WireEvent) -> Option<Effect> {
        match event {
            WireEvent::Update(notification) => {
                if let Some(operation) = self.operations.get_mut(&notification.session_id) {
                    match &mut operation.phase {
                        Phase::Resuming {
                            replay: Some(updates),
                            ..
                        } => {
                            updates.push(notification.update);
                        }
                        Phase::Ready { view, .. } => {
                            if let Some(view) = view.upgrade() {
                                view.borrow_mut().updates.push(notification.update);
                            }
                        }
                        _ => {} // Abandoned replay and closing traffic have no recipient.
                    }
                }
            }
            WireEvent::Resumed(session_id, result) => {
                let mut operation = self.operations.remove(&session_id)?;
                let Phase::Resuming { waiters, replay } = operation.phase else {
                    unreachable!("only one wire operation per session");
                };
                let response = match result {
                    Ok(response) => response,
                    Err(error) => {
                        fail(waiters, &error);
                        return self.resume(session_id, operation.next);
                    }
                };

                let view = Rc::new(RefCell::new(Projection {
                    response,
                    updates: replay.unwrap_or_default(),
                    disconnected: false,
                }));
                let mut leases = HashSet::new();
                for (ticket, reply) in waiters {
                    if reply.send(Ok(view.clone())).is_ok() {
                        leases.insert(ticket);
                    }
                }
                let effect = if leases.is_empty() {
                    // Install cleanup BEFORE considering a replacement load. A wire
                    // response alone is not permission to register a new replay target.
                    operation.phase = Phase::Closing;
                    Some(Effect::Close(session_id.clone()))
                } else {
                    operation.phase = Phase::Ready {
                        leases,
                        view: Rc::downgrade(&view),
                    };
                    None
                };
                self.operations.insert(session_id, operation);
                return effect;
            }
            WireEvent::ClosedSession(session_id, result) => {
                let mut operation = self.operations.remove(&session_id)?;
                assert!(matches!(operation.phase, Phase::Closing));
                if let Err(error) = result {
                    fail(std::mem::take(&mut operation.next), &error);
                    operation.phase = Phase::Blocked(error);
                    self.operations.insert(session_id, operation);
                } else {
                    return self.resume(session_id, operation.next);
                }
            }
            WireEvent::Disconnected => unreachable!("handled by the driver loop"),
        }
        None
    }

    fn resume(&mut self, session_id: v2::SessionId, mut waiters: Waiters) -> Option<Effect> {
        waiters.retain(|_, reply| !reply.is_canceled());
        if waiters.is_empty() {
            return None;
        }
        self.operations.insert(
            session_id.clone(),
            Operation {
                phase: Phase::Resuming {
                    waiters,
                    replay: Some(Vec::new()),
                },
                next: HashMap::new(),
            },
        );
        Some(Effect::Resume(session_id))
    }

    fn send(&self, effect: Effect, connection: &V2ConnectionTo<Agent>) -> Result<(), Error> {
        let events = self.wire_events.clone();
        match effect {
            Effect::Resume(session_id) => connection
                .send_request(
                    v2::ResumeSessionRequest::new(session_id.clone(), self.cwd.clone())
                        .replay_from(v2::ReplayFrom::from(v2::ReplayFromStart::new())),
                )
                .on_receiving_result(async move |result| {
                    drop(events.unbounded_send(WireEvent::Resumed(session_id, result)));
                    Ok(())
                }),
            Effect::Close(session_id) => connection
                .send_request(v2::CloseSessionRequest::new(session_id.clone()))
                .on_receiving_result(async move |result| {
                    drop(events.unbounded_send(WireEvent::ClosedSession(session_id, result)));
                    Ok(())
                }),
        }
    }
}

impl Drop for Driver {
    fn drop(&mut self) {
        // Also runs if the connection fails or the embedding application drops
        // the whole connection future, rather than receiving clean EOF.
        let error = stopped();
        for (_, operation) in self.operations.drain() {
            fail(operation.next, &error);
            match operation.phase {
                Phase::Resuming { waiters, .. } => fail(waiters, &error),
                Phase::Ready { view, .. } => {
                    if let Some(view) = view.upgrade() {
                        view.borrow_mut().disconnected = true;
                    }
                }
                Phase::Closing | Phase::Blocked(_) => {}
            }
        }
    }
}

fn stopped() -> Error {
    Error::internal_error().data("session coordinator stopped")
}

fn fail(waiters: Waiters, error: &Error) {
    for reply in waiters.into_values() {
        drop(reply.send(Err(error.clone())));
    }
}

async fn with_sessions(
    transport: impl ConnectTo<Client> + 'static,
    cwd: PathBuf,
    application: impl AsyncFnOnce(Sessions) -> Result<(), Error>,
) -> Result<(), Error> {
    let (events, incoming) = mpsc::unbounded();
    let updates = events.clone();
    let closed = events.clone();
    Client
        .v2()
        .on_receive_notification(
            async move |update: v2::UpdateSessionNotification, _cx: V2ConnectionTo<Agent>| {
                drop(updates.unbounded_send(WireEvent::Update(Box::new(update))));
                Ok(())
            },
            agent_client_protocol::on_receive_notification!(),
        )
        .on_receive_request(
            async |_request: v2::RequestPermissionRequest,
                   responder: Responder<v2::RequestPermissionResponse>,
                   _cx: V2ConnectionTo<Agent>| {
                responder.respond(v2::RequestPermissionResponse::new(
                    v2::RequestPermissionOutcome::Cancelled,
                ))
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_close(async move |_cx| {
            drop(closed.unbounded_send(WireEvent::Disconnected));
            Ok(())
        })
        .connect_with(transport, async move |connection| {
            let initialized = connection
                .send_request(v2::InitializeRequest::new(
                    ProtocolVersion::V2,
                    v2::Implementation::new("session-coordination-example", "0.1.0"),
                ))
                .block_task()
                .await?;
            if initialized.capabilities.session.is_none() {
                return Err(Error::invalid_params().data("agent has no session support"));
            }

            // Application ownership starts after initialization. The caller owns
            // and can cancel the enclosing connection future during the handshake.
            let (commands, receiver) = mpsc::unbounded();
            let sessions = Sessions {
                commands,
                next_ticket: Rc::new(Cell::new(0)),
            };
            let driver = Driver {
                operations: HashMap::new(),
                commands: receiver,
                incoming,
                wire_events: events,
                cwd,
            };
            futures::try_join!(driver.run(connection), application(sessions))?;
            Ok(())
        })
        .await
}

#[derive(Parser)]
struct Cli {
    /// An ACP v2 agent with an existing resumable session.
    #[arg(long)]
    command: String,
    #[arg(long)]
    session_id: String,
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let cli = Cli::parse();
    with_sessions(
        AcpAgent::from_str(&cli.command)?,
        std::env::current_dir()?,
        async move |sessions| {
            let id = v2::SessionId::new(cli.session_id);
            let first = sessions.load(id.clone())?;
            let second = sessions.load(id.clone())?;
            let (first, second) = futures::try_join!(first.wait(), second.wait())?;
            println!(
                "Shared replay: {} updates",
                first.view.borrow().updates.len()
            );
            println!("Resume setup: {:?}", first.view.borrow().response);
            drop((first, second));

            let reopened = sessions.load(id)?.wait().await?;
            println!(
                "Fresh replay after close: {} updates",
                reopened.view.borrow().updates.len()
            );
            // The last application owner disappearing stops the transport rather
            // than waiting indefinitely for remote cleanup.
            drop((reopened, sessions));
            Ok(())
        },
    )
    .await?;
    Ok(())
}

#[cfg(test)]
#[path = "v2_session_coordination/tests.rs"]
mod tests;
