// Types re-exported from crate root

mod typed;
pub use typed::{MatchMessage, TypeNotification};

/// Cast from `N` to `M` by serializing/deserialization to/from JSON.
pub fn json_cast<N, M>(params: N) -> Result<M, crate::Error>
where
    N: serde::Serialize,
    M: serde::de::DeserializeOwned,
{
    let json = serde_json::to_value(params).map_err(|_| crate::Error::parse_error())?;
    let m = serde_json::from_value(json).map_err(|_| crate::Error::parse_error())?;
    Ok(m)
}

/// Creates an internal error with the given message
pub fn internal_error(message: impl ToString) -> crate::Error {
    crate::Error::internal_error().with_data(message.to_string())
}

/// Creates a parse error with the given message
pub fn parse_error(message: impl ToString) -> crate::Error {
    crate::Error::parse_error().with_data(message.to_string())
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
