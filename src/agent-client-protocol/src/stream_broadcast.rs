//! JSON-RPC Stream broadcasting for debugging and monitoring communication.
//!
//! This module provides functionality to observe the JSON-RPC message stream between
//! clients and agents. It's primarily used for debugging, logging, and building
//! development tools that need to monitor the protocol communication.

use std::sync::Arc;
use std::time::{Duration, Instant};

use agent_client_protocol_schema::{
    Error, Notification, OutgoingMessage, Request, RequestId, Response, Result, Side,
};
use derive_more::From;
use serde::Serialize;
use serde_json::value::RawValue;

/// A message that flows through the RPC stream.
///
/// This represents any JSON-RPC message (request, response, or notification)
/// along with its direction (incoming or outgoing) and metadata.
///
/// Stream messages are used for observing and debugging the protocol communication
/// without interfering with the actual message handling.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StreamMessage {
    /// Metadata about the message.
    pub metadata: StreamMessageMetadata,
    /// The direction of the message relative to this side of the connection.
    pub direction: StreamMessageDirection,
    /// The actual content of the message.
    pub message: StreamMessageContent,
}

/// Metadata about a stream message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StreamMessageMetadata {
    /// Timestamp when the message was observed.
    pub timestamp: Duration,
}

/// Filter configuration for stream messages.
#[derive(Debug, Clone, Default)]
#[allow(dead_code)]
pub struct MessageFilter {
    excluded_methods: Vec<Arc<str>>,
    excluded_directions: Vec<StreamMessageDirection>,
}

impl MessageFilter {
    /// Creates a new empty filter.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Adds a method to exclude from broadcasting.
    #[must_use]
    pub fn exclude_method(mut self, method: impl Into<Arc<str>>) -> Self {
        self.excluded_methods.push(method.into());
        self
    }

    /// Adds a direction to exclude from broadcasting.
    #[must_use]
    pub fn exclude_direction(mut self, direction: StreamMessageDirection) -> Self {
        self.excluded_directions.push(direction);
        self
    }

    /// Checks if a message should be broadcast based on this filter.
    #[allow(clippy::trivially_copy_pass_by_ref, clippy::collapsible_if)]
    fn should_broadcast(&self, direction: StreamMessageDirection, method: Option<&Arc<str>>) -> bool {
        if self.excluded_directions.contains(&direction) {
            return false;
        }
        if let Some(m) = method {
            if self.excluded_methods.contains(m) {
                return false;
            }
        }
        true
    }
}

/// The direction of a message in the RPC stream.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StreamMessageDirection {
    /// A message received from the other side of the connection.
    Incoming,
    /// A message sent to the other side of the connection.
    Outgoing,
}

/// The content of a stream message.
///
/// This enum represents the three types of JSON-RPC messages:
/// - Requests: Method calls that expect a response
/// - Responses: Replies to previous requests
/// - Notifications: One-way messages that don't expect a response
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StreamMessageContent {
    /// A JSON-RPC request message.
    Request {
        /// The unique identifier for this request.
        id: RequestId,
        /// The name of the method being called.
        method: Arc<str>,
        /// Optional parameters for the method.
        params: Option<serde_json::Value>,
    },
    /// A JSON-RPC response message.
    Response {
        /// The ID of the request this response is for.
        id: RequestId,
        /// The result of the request (success or error).
        result: Result<Option<serde_json::Value>>,
        /// The method that this response is for (if known).
        method: Option<Arc<str>>,
    },
    /// A JSON-RPC notification message.
    Notification {
        /// The name of the notification method.
        method: Arc<str>,
        /// Optional parameters for the notification.
        params: Option<serde_json::Value>,
    },
}

impl StreamMessageContent {
    /// Returns the method name if this message has one.
    #[allow(clippy::match_same_arms, clippy::must_use_candidate)]
    pub fn method(&self) -> Option<&Arc<str>> {
        match self {
            StreamMessageContent::Request { method, .. } | StreamMessageContent::Notification { method, .. } => Some(method),
            StreamMessageContent::Response { method, .. } => method.as_ref(),
        }
    }
}

/// A receiver for observing the message stream.
///
/// This allows you to receive copies of all messages flowing through the connection,
/// useful for debugging, logging, or building development tools.
///
/// # Example
///
/// ```no_run
/// use agent_client_protocol::{StreamReceiver, StreamMessageDirection};
///
/// async fn monitor_messages(mut receiver: StreamReceiver) {
///     while let Ok(message) = receiver.recv().await {
///         match message.direction {
///             StreamMessageDirection::Incoming => println!("← Received: {:?}", message.message),
///             StreamMessageDirection::Outgoing => println!("→ Sent: {:?}", message.message),
///         }
///     }
/// }
/// ```
#[derive(Debug, From)]
pub struct StreamReceiver(async_broadcast::Receiver<StreamMessage>);

impl StreamReceiver {
    /// Receives the next message from the stream.
    ///
    /// This method will wait until a message is available or the sender is dropped.
    ///
    /// # Returns
    ///
    /// - `Ok(StreamMessage)` when a message is received
    /// - `Err` when the sender is dropped or the receiver is lagged
    pub async fn recv(&mut self) -> Result<StreamMessage> {
        self.0
            .recv()
            .await
            .map_err(|e| Error::internal_error().data(e.to_string()))
    }
}

/// Internal sender for broadcasting stream messages.
///
/// This is used internally by the RPC system to broadcast messages to all receivers.
/// You typically won't interact with this directly.
#[derive(Clone, Debug, From)]
pub(crate) struct StreamSender {
    sender: async_broadcast::Sender<StreamMessage>,
    start_time: Instant,
}

impl StreamSender {
    /// Broadcasts an outgoing message to all receivers.
    pub(crate) fn outgoing<L: Side, R: Side>(&self, message: &OutgoingMessage<L, R>) {
        if self.sender.receiver_count() == 0 {
            return;
        }

        let timestamp = self.start_time.elapsed();

        let message = StreamMessage {
            metadata: StreamMessageMetadata { timestamp },
            direction: StreamMessageDirection::Outgoing,
            message: match message {
                OutgoingMessage::Request(Request { id, method, params }) => {
                    StreamMessageContent::Request {
                        id: id.clone(),
                        method: method.clone(),
                        params: serde_json::to_value(params).ok(),
                    }
                }
                OutgoingMessage::Response(Response::Result { id, result }) => {
                    StreamMessageContent::Response {
                        id: id.clone(),
                        result: Ok(serde_json::to_value(result).ok()),
                        method: None,
                    }
                }
                OutgoingMessage::Response(Response::Error { id, error }) => {
                    StreamMessageContent::Response {
                        id: id.clone(),
                        result: Err(error.clone()),
                        method: None,
                    }
                }
                OutgoingMessage::Notification(Notification { method, params }) => {
                    StreamMessageContent::Notification {
                        method: method.clone(),
                        params: serde_json::to_value(params).ok(),
                    }
                }
            },
        };

        self.sender.try_broadcast(message).ok();
    }

    /// Broadcasts an incoming request to all receivers.
    pub(crate) fn incoming_request(
        &self,
        id: RequestId,
        method: impl Into<Arc<str>>,
        params: &impl Serialize,
    ) {
        if self.sender.receiver_count() == 0 {
            return;
        }

        let timestamp = self.start_time.elapsed();
        let method: Arc<str> = method.into();

        let message = StreamMessage {
            metadata: StreamMessageMetadata { timestamp },
            direction: StreamMessageDirection::Incoming,
            message: StreamMessageContent::Request {
                id,
                method: method.clone(),
                params: serde_json::to_value(params).ok(),
            },
        };

        self.sender.try_broadcast(message).ok();
    }

    /// Broadcasts an incoming response to all receivers.
    pub(crate) fn incoming_response(
        &self,
        id: RequestId,
        method: Option<Arc<str>>,
        result: Result<Option<&RawValue>, &Error>,
    ) {
        if self.sender.receiver_count() == 0 {
            return;
        }

        let timestamp = self.start_time.elapsed();

        let result = match result {
            Ok(Some(value)) => Ok(serde_json::from_str(value.get()).ok()),
            Ok(None) => Ok(None),
            Err(err) => Err(err.clone()),
        };

        let message = StreamMessage {
            metadata: StreamMessageMetadata { timestamp },
            direction: StreamMessageDirection::Incoming,
            message: StreamMessageContent::Response { id, result, method },
        };

        self.sender.try_broadcast(message).ok();
    }

    /// Broadcasts an incoming notification to all receivers.
    pub(crate) fn incoming_notification(
        &self,
        method: impl Into<Arc<str>>,
        params: &impl Serialize,
    ) {
        if self.sender.receiver_count() == 0 {
            return;
        }

        let timestamp = self.start_time.elapsed();

        let message = StreamMessage {
            metadata: StreamMessageMetadata { timestamp },
            direction: StreamMessageDirection::Incoming,
            message: StreamMessageContent::Notification {
                method: method.into(),
                params: serde_json::to_value(params).ok(),
            },
        };

        self.sender.try_broadcast(message).ok();
    }
}

impl From<async_broadcast::Sender<StreamMessage>> for StreamSender {
    fn from(sender: async_broadcast::Sender<StreamMessage>) -> Self {
        Self {
            sender,
            start_time: Instant::now(),
        }
    }
}

/// A message broadcaster for broadcasting RPC messages with filtering support.
///
/// This provides a cleaner abstraction over the raw StreamSender,
/// allowing the RPC IO loop to broadcast messages without coupling
/// to the underlying broadcast implementation. It supports filtering
/// messages by method name or direction.
#[derive(Clone, Debug)]
pub(crate) struct MessageBroadcaster {
    sender: StreamSender,
    filter: MessageFilter,
}

#[allow(dead_code)]
impl MessageBroadcaster {
    /// Creates a new message broadcaster from a stream sender.
    pub(crate) fn new(sender: StreamSender) -> Self {
        Self {
            sender,
            filter: MessageFilter::new(),
        }
    }

    /// Creates a new message broadcaster with a custom filter.
    pub(crate) fn with_filter(sender: StreamSender, filter: MessageFilter) -> Self {
        Self { sender, filter }
    }

    /// Updates the filter for this broadcaster.
    pub(crate) fn set_filter(&mut self, filter: MessageFilter) {
        self.filter = filter;
    }

    /// Returns a reference to the current filter.
    pub(crate) fn filter(&self) -> &MessageFilter {
        &self.filter
    }

    /// Checks if a message should be broadcast based on the current filter.
    fn should_broadcast(&self, direction: StreamMessageDirection, method: Option<&Arc<str>>) -> bool {
        self.filter.should_broadcast(direction, method)
    }

    /// Broadcasts an outgoing message.
    #[allow(clippy::match_same_arms)]
    pub(crate) fn outgoing<L: Side, R: Side>(&self, message: &OutgoingMessage<L, R>) {
        let method = match message {
            OutgoingMessage::Response(_) => None,
            OutgoingMessage::Request(Request { method, .. }) | OutgoingMessage::Notification(Notification { method, .. }) => Some(method),
        };

        if self.should_broadcast(StreamMessageDirection::Outgoing, method) {
            self.sender.outgoing(message);
        }
    }

    /// Broadcasts an incoming request.
    pub(crate) fn incoming_request(
        &self,
        id: RequestId,
        method: impl Into<Arc<str>>,
        params: &impl Serialize,
    ) {
        let method: Arc<str> = method.into();
        if self.should_broadcast(StreamMessageDirection::Incoming, Some(&method)) {
            self.sender.incoming_request(id, method, params);
        }
    }

    /// Broadcasts an incoming response.
    pub(crate) fn incoming_response(
        &self,
        id: RequestId,
        method: Option<Arc<str>>,
        result: Result<Option<&RawValue>, &Error>,
    ) {
        if self.should_broadcast(StreamMessageDirection::Incoming, method.as_ref()) {
            self.sender.incoming_response(id, method, result);
        }
    }

    /// Broadcasts an incoming notification.
    pub(crate) fn incoming_notification(
        &self,
        method: impl Into<Arc<str>>,
        params: &impl Serialize,
    ) {
        let method: Arc<str> = method.into();
        if self.should_broadcast(StreamMessageDirection::Incoming, Some(&method)) {
            self.sender.incoming_notification(method, params);
        }
    }
}

/// A broadcast for observing RPC message streams.
///
/// This is used internally by the RPC connection to allow multiple receivers
/// to observe the message stream.
#[derive(Debug, Clone)]
pub(crate) struct StreamBroadcast {
    receiver: async_broadcast::InactiveReceiver<StreamMessage>,
}

impl StreamBroadcast {
    /// Creates a new broadcast.
    ///
    /// Returns a message broadcaster for broadcasting messages and the broadcast instance
    /// for creating receivers.
    pub(crate) fn new() -> (MessageBroadcaster, Self) {
        let (sender, receiver) = async_broadcast::broadcast(1);
        (
            MessageBroadcaster::new(sender.into()),
            Self {
                receiver: receiver.deactivate(),
            },
        )
    }

    /// Creates a new receiver for observing the message stream.
    ///
    /// Each receiver will get its own copy of every message.
    pub(crate) fn receiver(&self) -> StreamReceiver {
        let was_empty = self.receiver.receiver_count() == 0;
        let mut new_receiver = self.receiver.activate_cloned();
        if was_empty {
            // Grow capacity once we actually have a receiver
            new_receiver.set_capacity(64);
        }
        new_receiver.into()
    }
}

impl Default for StreamMessageMetadata {
    fn default() -> Self {
        Self {
            timestamp: Duration::ZERO,
        }
    }
}

impl<Local: Side, Remote: Side> From<OutgoingMessage<Local, Remote>> for StreamMessage {
    fn from(message: OutgoingMessage<Local, Remote>) -> Self {
        Self {
            metadata: StreamMessageMetadata::default(),
            direction: StreamMessageDirection::Outgoing,
            message: match message {
                OutgoingMessage::Request(Request { id, method, params }) => {
                    StreamMessageContent::Request {
                        id,
                        method,
                        params: serde_json::to_value(params).ok(),
                    }
                }
                OutgoingMessage::Response(Response::Result { id, result }) => {
                    StreamMessageContent::Response {
                        id,
                        result: Ok(serde_json::to_value(result).ok()),
                        method: None,
                    }
                }
                OutgoingMessage::Response(Response::Error { id, error }) => {
                    StreamMessageContent::Response {
                        id,
                        result: Err(error),
                        method: None,
                    }
                }
                OutgoingMessage::Notification(Notification { method, params }) => {
                    StreamMessageContent::Notification {
                        method,
                        params: serde_json::to_value(params).ok(),
                    }
                }
            },
        }
    }
}
