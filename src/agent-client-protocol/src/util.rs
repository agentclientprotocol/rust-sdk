// Types re-exported from crate root

use futures::{
    future::BoxFuture,
    stream::{Stream, StreamExt},
};

mod typed;
pub use typed::{MatchDispatch, MatchDispatchFrom, TypeNotification};

/// Format only code-derived metadata at ordinary log levels.
///
/// Error messages, data, and even Serde parse reasons can contain private input.
/// Do not add fields from the original error here, including purported paths
/// extracted from its text. Full diagnostics belong in opt-in protocol tracing.
pub(crate) fn redacted_error(code: impl Into<i32>) -> impl std::fmt::Debug {
    struct RedactedError {
        code: i32,
        reason: &'static str,
    }

    impl std::fmt::Debug for RedactedError {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.debug_struct("Error")
                .field("code", &self.code)
                .field("reason", &self.reason)
                .finish()
        }
    }

    let code = code.into();
    let reason = match code {
        -32700 => "Parse error",
        -32600 => "Invalid request",
        -32601 => "Method not found",
        -32602 => "Invalid params",
        -32603 => "Internal error",
        _ => "Protocol or application error",
    };
    RedactedError { code, reason }
}

/// Cast from `N` to `M` by serializing/deserialization to/from JSON.
///
/// Returned diagnostics can contain the input JSON and payload-bearing parse
/// reasons. Treat them as sensitive, as with [`json_cast_params`].
pub fn json_cast<N, M>(params: N) -> Result<M, crate::Error>
where
    N: serde::Serialize,
    M: serde::de::DeserializeOwned,
{
    let json = serde_json::to_value(params).map_err(|e| {
        crate::Error::parse_error().data(serde_json::json!({
            "error": e.to_string(),
            "phase": "serialization"
        }))
    })?;
    let m = serde_json::from_value(json.clone()).map_err(|e| {
        crate::Error::parse_error().data(serde_json::json!({
            "error": e.to_string(),
            "json": json,
            "phase": "deserialization"
        }))
    })?;
    Ok(m)
}

/// Cast incoming request/notification params into a typed payload.
///
/// Like [`json_cast`], but deserialization failures become
/// [`Error::invalid_params`](`crate::Error::invalid_params`) (`-32602`)
/// instead of a parse error, which is the correct JSON-RPC error code for
/// malformed method parameters.
///
/// Returned diagnostics include the input JSON and may include input values in
/// the parse reason. Treat them as sensitive; the SDK redacts errors at WARN.
pub fn json_cast_params<N, M>(params: N) -> Result<M, crate::Error>
where
    N: serde::Serialize,
    M: serde::de::DeserializeOwned,
{
    let json = serde_json::to_value(params).map_err(|e| {
        crate::Error::internal_error().data(serde_json::json!({
            "error": e.to_string(),
            "phase": "serialization"
        }))
    })?;
    let m = serde_json::from_value(json.clone()).map_err(|e| {
        crate::Error::invalid_params().data(serde_json::json!({
            "error": e.to_string(),
            "json": json,
            "phase": "deserialization"
        }))
    })?;
    Ok(m)
}

/// Creates an internal error with the given message
pub fn internal_error(message: impl ToString) -> crate::Error {
    crate::Error::internal_error().data(message.to_string())
}

/// Creates a parse error with the given message
pub fn parse_error(message: impl ToString) -> crate::Error {
    crate::Error::parse_error().data(message.to_string())
}

/// No version supported by a v2 endpoint is at or below the requested version.
///
/// Uses the protocol wire code so legacy v1 error models can carry the v2
/// diagnostic as an unknown error code without changing the v1 contract.
#[must_use]
pub fn unsupported_protocol_version(
    requested: crate::schema::ProtocolVersion,
    supported: &[crate::schema::ProtocolVersion],
) -> crate::Error {
    crate::Error::new(-32022, "Unsupported protocol version")
        .data(serde_json::json!({ "requested": requested, "supported": supported }))
}

pub(crate) fn instrumented_with_connection_name<F>(
    name: String,
    task: F,
) -> tracing::instrument::Instrumented<F> {
    use tracing::Instrument;

    task.instrument(tracing::info_span!("connection", name = name))
}

pub(crate) async fn instrument_with_connection_name<R>(
    name: Option<String>,
    task: impl Future<Output = R>,
) -> R {
    if let Some(name) = name {
        instrumented_with_connection_name(name.clone(), task).await
    } else {
        task.await
    }
}

/// Run `background` until `foreground` completes.
///
/// Returns the result of `foreground`. If `background` errors before
/// `foreground` completes, the error is propagated. If `background`
/// completes with `Ok(())`, we continue waiting for `foreground`.
pub fn run_until<T, E>(
    background: impl Future<Output = Result<(), E>>,
    foreground: impl Future<Output = Result<T, E>>,
) -> impl Future<Output = Result<T, E>> {
    use futures::future::{Either, select};
    use std::pin::pin;

    Box::pin(async move {
        match select(pin!(background), pin!(foreground)).await {
            Either::Left((bg_result, fg_future)) => {
                // Background finished first
                bg_result?; // propagate error, or if Ok(()), keep waiting
                fg_future.await
            }
            Either::Right((fg_result, _bg_future)) => {
                // Foreground finished first, drop background
                fg_result
            }
        }
    })
}

/// Process items from a stream concurrently.
///
/// For each item received from `stream`, calls `process_fn` to create a future,
/// then runs all futures concurrently. If any future returns an error,
/// stops processing and returns that error.
///
/// This is useful for patterns where you receive work items from a channel
/// and want to process them concurrently while respecting backpressure.
pub(crate) async fn process_stream_concurrently<T, F>(
    stream: impl Stream<Item = T>,
    process_fn: F,
    process_fn_hack: impl for<'a> Fn(&'a F, T) -> BoxFuture<'a, Result<(), crate::Error>>,
) -> Result<(), crate::Error>
where
    F: AsyncFn(T) -> Result<(), crate::Error>,
{
    use std::pin::pin;

    use futures::stream::{FusedStream, FuturesUnordered};
    use futures_concurrency::future::Race;

    enum Event<T> {
        NewItem(Option<T>),
        FutureCompleted(Option<Result<(), crate::Error>>),
    }

    let mut stream = pin!(stream.fuse());
    let mut futures: FuturesUnordered<_> = FuturesUnordered::new();

    loop {
        // If we have no futures to run, wait until we do.
        if futures.is_empty() {
            match stream.next().await {
                Some(item) => futures.push(process_fn_hack(&process_fn, item)),
                None => return Ok(()),
            }
            continue;
        }

        // If there are no more items coming in, just drain our queue and return.
        if stream.is_terminated() {
            while let Some(result) = futures.next().await {
                result?;
            }
            return Ok(());
        }

        // Otherwise, race between getting a new item and completing a future.
        let event = (async { Event::NewItem(stream.next().await) }, async {
            Event::FutureCompleted(futures.next().await)
        })
            .race()
            .await;

        match event {
            Event::NewItem(Some(item)) => {
                futures.push(process_fn_hack(&process_fn, item));
            }
            Event::FutureCompleted(Some(result)) => {
                result?;
            }
            Event::NewItem(None) | Event::FutureCompleted(None) => {
                // Stream closed, loop will catch is_terminated
                // No futures were pending, shouldn't happen since we checked is_empty
            }
        }
    }
}
