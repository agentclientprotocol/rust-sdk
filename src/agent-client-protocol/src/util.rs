// Types re-exported from crate root

mod typed;
pub use typed::{MatchDispatch, MatchDispatchFrom, TypeNotification};

/// Cast from `N` to `M` by serializing/deserialization to/from JSON.
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
