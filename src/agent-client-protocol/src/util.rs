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

/// Replace every environment variable and header value in a diagnostic copy.
///
/// ACP carries credentials in `env` and `headers` fields: MCP server
/// declarations in session setup, terminal auth methods, terminal creation,
/// and provider configuration. Names stay visible; values never reach logs.
/// Fields are matched by name anywhere in the value rather than by method, so
/// unrelated payloads that happen to use these keys are redacted as well.
fn redact_env_and_header_values(value: &mut serde_json::Value) {
    match value {
        serde_json::Value::Object(object) => {
            for (key, field) in object.iter_mut() {
                if key == "env" || key == "headers" {
                    redact_name_value_pairs(field);
                }
                redact_env_and_header_values(field);
            }
        }
        serde_json::Value::Array(values) => {
            values.iter_mut().for_each(redact_env_and_header_values);
        }
        _ => {}
    }
}

/// Redact `[{ "name", "value" }]` lists and `{ name: value }` maps.
fn redact_name_value_pairs(field: &mut serde_json::Value) {
    let redacted = || serde_json::Value::String("[REDACTED]".to_owned());
    match field {
        serde_json::Value::Array(entries) => {
            for entry in entries {
                if let Some(value) = entry.get_mut("value") {
                    *value = redacted();
                }
            }
        }
        serde_json::Value::Object(entries) => {
            for value in entries.values_mut() {
                *value = redacted();
            }
        }
        _ => {}
    }
}

/// Format one wire line for protocol TRACE logs with credentials redacted.
///
/// Redaction happens only when the event is actually formatted. A line that is
/// not valid JSON cannot be redacted reliably, so only its length is shown.
pub(crate) fn redacted_json_line(line: &str) -> impl std::fmt::Display + '_ {
    struct RedactedJsonLine<'a>(&'a str);

    impl std::fmt::Display for RedactedJsonLine<'_> {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            match serde_json::from_str::<serde_json::Value>(self.0) {
                Ok(mut value) => {
                    redact_env_and_header_values(&mut value);
                    write!(f, "{value}")
                }
                Err(_) => write!(f, "<{} bytes of invalid JSON omitted>", self.0.len()),
            }
        }
    }

    RedactedJsonLine(line)
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
    #[cfg(feature = "unstable_protocol_v2")]
    let data = serde_json::to_value(crate::schema::v2::UnsupportedProtocolVersionErrorData::new(
        requested,
        supported.to_vec(),
    ))
    .ok();
    #[cfg(not(feature = "unstable_protocol_v2"))]
    let data = Some(serde_json::json!({ "requested": requested, "supported": supported }));
    crate::Error::new(-32022, "Unsupported protocol version").data(data)
}

/// Rewrite the params of an `initialize` request for the protocol version
/// `selected` that a version-agnostic forwarder, such as the conductor,
/// selected for it.
///
/// The SDK's own agents negotiate the same way. A request for a newer version
/// than `selected` was written for that newer version, so it keeps only what
/// parses under `selected` instead of being rejected (for v1, only the shared
/// capabilities are carried across). A request for `selected` or an older
/// version keeps its params. In every case `protocolVersion` is set to
/// `selected`.
///
/// # Errors
///
/// Returns `-32602` when `params` has no valid `protocolVersion`, and
/// `-32600` when a newer request must be rewritten for a version this build
/// of the SDK does not support.
pub fn initialize_params_for_version(
    params: &serde_json::Value,
    selected: crate::schema::ProtocolVersion,
) -> Result<serde_json::Map<String, serde_json::Value>, crate::Error> {
    use crate::schema::ProtocolVersion;

    let invalid_version = || {
        crate::Error::invalid_params()
            .data("initialize.protocolVersion must be a valid ACP protocol version")
    };
    let serde_json::Value::Object(object) = params else {
        return Err(invalid_version());
    };
    let requested: ProtocolVersion = object
        .get("protocolVersion")
        .cloned()
        .and_then(|version| serde_json::from_value(version).ok())
        .ok_or_else(invalid_version)?;

    let mut rewritten = if requested <= selected {
        object.clone()
    } else if selected == ProtocolVersion::V1 {
        crate::jsonrpc::normalize_v1_initialize_request(params)?
    } else {
        #[cfg(feature = "unstable_protocol_v2")]
        if selected == ProtocolVersion::V2 {
            return crate::jsonrpc::normalize_newer_v2_initialize_request(object);
        }
        return Err(crate::Error::invalid_request().data(format!(
            "cannot rewrite an initialize request for ACP protocol version {requested} to version {selected}",
        )));
    };
    rewritten.insert(
        "protocolVersion".into(),
        serde_json::to_value(selected).map_err(crate::Error::into_internal_error)?,
    );
    Ok(rewritten)
}

/// The `-32600` error, sent with `id: null`, for a JSON-RPC batch array.
///
/// The SDK does not support JSON-RPC batching, and ACP protocol v2 removed
/// it.
pub(crate) fn batch_not_supported() -> crate::Error {
    crate::Error::invalid_request()
        .data("JSON-RPC batches are not supported; send each message individually")
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
