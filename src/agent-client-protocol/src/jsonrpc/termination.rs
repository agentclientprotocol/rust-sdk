//! Terminal lifecycle event for driven connections.

use std::sync::{Arc, Mutex};

/// Why a driven connection stopped.
///
/// A connection driven by [`Builder::connect_to`](crate::Builder::connect_to)
/// or [`Builder::connect_with`](crate::Builder::connect_with) publishes
/// exactly one termination, observable through
/// [`ConnectionTo::terminated`](crate::ConnectionTo::terminated) and
/// [`ConnectionTo::termination`](crate::ConnectionTo::termination).
///
/// Unlike [`ConnectionTo::incoming_closed`](crate::ConnectionTo::incoming_closed),
/// which only reports clean incoming EOF, termination covers every way the
/// connection driver can end.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum ConnectionTermination {
    /// The incoming transport reached clean EOF and the driver finished
    /// successfully.
    ///
    /// This is also reported when a `connect_with` foreground returns `Ok`
    /// after observing clean EOF.
    IncomingClosed,
    /// The `connect_with` foreground returned `Ok` before incoming EOF,
    /// initiating a local shutdown that completed successfully.
    LocalShutdown,
    /// The `connect_with` foreground returned an error.
    ForegroundError(crate::Error),
    /// The transport driver failed to read, write, or drain.
    TransportError(crate::Error),
    /// A message handler, close callback, runner, or spawned task failed.
    HandlerError(crate::Error),
    /// The driver future was dropped before it completed.
    ///
    /// Only synchronous bookkeeping happens on drop: pending requests are
    /// failed and this termination is published. No asynchronous cleanup,
    /// close callbacks, or output draining run after an arbitrary drop.
    Dropped,
}

impl ConnectionTermination {
    /// The error that ended the connection, if any.
    #[must_use]
    pub fn error(&self) -> Option<&crate::Error> {
        match self {
            Self::ForegroundError(error)
            | Self::TransportError(error)
            | Self::HandlerError(error) => Some(error),
            Self::IncomingClosed | Self::LocalShutdown | Self::Dropped => None,
        }
    }

    /// Whether the driver completed without error.
    #[must_use]
    pub fn is_clean(&self) -> bool {
        matches!(self, Self::IncomingClosed | Self::LocalShutdown)
    }

    /// Payload-free discriminator stored in pending-request errors.
    pub(super) fn kind(&self) -> &'static str {
        match self {
            Self::IncomingClosed => "incoming_closed",
            Self::LocalShutdown => "local_shutdown",
            Self::ForegroundError(_) => "foreground_error",
            Self::TransportError(_) => "transport_error",
            Self::HandlerError(_) => "handler_error",
            Self::Dropped => "dropped",
        }
    }
}

/// Stable discriminator stored in the `data.reason` field of errors produced
/// when a connection terminates for a reason other than clean incoming EOF
/// before a request receives its response.
pub const CONNECTION_TERMINATED_REASON: &str = "connection_terminated";

/// Return whether `error` reports that the connection terminated before a
/// request received its response.
///
/// Requests failed by clean incoming EOF instead satisfy
/// [`is_incoming_transport_closed`](crate::is_incoming_transport_closed).
#[must_use]
pub fn is_connection_terminated(error: &crate::Error) -> bool {
    error
        .data
        .as_ref()
        .and_then(|data| data.get("reason"))
        .and_then(serde_json::Value::as_str)
        == Some(CONNECTION_TERMINATED_REASON)
}

pub(super) fn connection_terminated_error(
    method: &str,
    termination: &ConnectionTermination,
) -> crate::Error {
    let mut error = crate::Error::internal_error();
    error.message = "Connection terminated".to_string();
    error.data(serde_json::json!({
        "reason": CONNECTION_TERMINATED_REASON,
        "method": method,
        "termination": termination.kind(),
    }))
}

/// Which part of the driver produced an error.
#[derive(Clone, Copy, Debug)]
pub(super) enum TerminationSource {
    Transport,
    Handler,
    Foreground,
}

/// Records errors by origin so the driver's final result can be classified.
#[derive(Clone, Default)]
pub(super) struct TerminationSources {
    errors: Arc<Mutex<Vec<(TerminationSource, crate::Error)>>>,
}

impl TerminationSources {
    pub(super) fn record<T>(&self, source: TerminationSource, result: &Result<T, crate::Error>) {
        if let Err(error) = result {
            self.errors
                .lock()
                .expect("termination sources mutex poisoned")
                .push((source, error.clone()));
        }
    }

    /// Classify the driver's returned error. Several actors may fail during
    /// one shutdown; the source of the error actually returned wins.
    pub(super) fn classify(&self, error: crate::Error) -> ConnectionTermination {
        let source = {
            let errors = self
                .errors
                .lock()
                .expect("termination sources mutex poisoned");
            errors
                .iter()
                .find(|(_, recorded)| *recorded == error)
                .or_else(|| errors.first())
                .map_or(TerminationSource::Handler, |(source, _)| *source)
        };
        match source {
            TerminationSource::Transport => ConnectionTermination::TransportError(error),
            TerminationSource::Handler => ConnectionTermination::HandlerError(error),
            TerminationSource::Foreground => ConnectionTermination::ForegroundError(error),
        }
    }
}
