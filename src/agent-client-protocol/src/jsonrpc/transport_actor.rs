use std::pin::pin;

// Types re-exported from crate root
use crate::jsonrpc::{RawJsonRpcMessage, TransportBatch, TransportBatchEntry, TransportFrame};
use crate::schema::v1::Response;
use futures::StreamExt as _;
use futures::channel::mpsc;
use serde::Deserialize as _;

enum ParsedIncomingLine {
    Ignored,
    Single(RawJsonRpcMessage),
    Malformed { raw: String, error: crate::Error },
    Batch(TransportBatch),
}

fn message_shape(value: &serde_json::Value) -> (bool, bool) {
    value.as_object().map_or((false, false), |object| {
        (
            object.contains_key("method"),
            object.contains_key("result") || object.contains_key("error"),
        )
    })
}

fn parse_incoming_line(line: &str) -> ParsedIncomingLine {
    let value = match serde_json::from_str::<serde_json::Value>(line) {
        Ok(value) => value,
        Err(error) => {
            tracing::debug!(?error, "Failed to parse incoming JSON-RPC JSON");
            return ParsedIncomingLine::Malformed {
                raw: line.to_owned(),
                error: crate::Error::parse_error().data(serde_json::json!({ "line": line })),
            };
        }
    };

    match value {
        serde_json::Value::Array(entries) if entries.is_empty() => ParsedIncomingLine::Malformed {
            raw: line.to_owned(),
            error: crate::Error::invalid_request(),
        },
        serde_json::Value::Array(entries) => {
            let mut has_response_entry = false;
            let mut has_call_entry = false;
            let mut entries = entries
                .into_iter()
                .filter_map(|entry| {
                    let (looks_like_call, looks_like_response) = message_shape(&entry);
                    match RawJsonRpcMessage::deserialize(&entry) {
                        Ok(message) => {
                            match &message {
                                RawJsonRpcMessage::Request(_)
                                | RawJsonRpcMessage::Notification(_) => has_call_entry = true,
                                RawJsonRpcMessage::Response(_) => has_response_entry = true,
                            }
                            Some(TransportBatchEntry::message(message))
                        }
                        Err(error) => {
                            has_call_entry |= looks_like_call;
                            has_response_entry |= looks_like_response;
                            tracing::debug!(?error, "Invalid JSON-RPC batch entry");

                            // A malformed response must not itself receive a
                            // response, even when request siblings make this a
                            // mixed batch. Keep ambiguous call-shaped entries
                            // so invalid requests still receive an error.
                            (!looks_like_response || looks_like_call).then(|| {
                                TransportBatchEntry::malformed(
                                    entry,
                                    crate::Error::invalid_request(),
                                )
                            })
                        }
                    }
                })
                .collect::<Vec<_>>();

            if has_response_entry && !has_call_entry {
                // A response batch is not a request and must not itself receive
                // responses. Ignore malformed response-like siblings to avoid
                // sending error responses back and forth between peers.
                entries.retain(|entry| entry.as_result().is_ok());
            }
            match TransportBatch::from_entries(entries) {
                Some(batch) => ParsedIncomingLine::Batch(batch),
                None => ParsedIncomingLine::Ignored,
            }
        }
        value => {
            let (looks_like_call, looks_like_response) = message_shape(&value);
            match serde_json::from_value(value) {
                Ok(message) => ParsedIncomingLine::Single(message),
                Err(error) => {
                    tracing::debug!(?error, "Invalid JSON-RPC message");
                    if looks_like_response && !looks_like_call {
                        ParsedIncomingLine::Ignored
                    } else {
                        ParsedIncomingLine::Malformed {
                            raw: line.to_owned(),
                            error: crate::Error::invalid_request(),
                        }
                    }
                }
            }
        }
    }
}

impl TransportFrame {
    /// Parse one JSON-RPC wire value while preserving batch boundaries.
    ///
    /// Malformed calls are returned as explicit malformed frames or batch
    /// entries. Malformed response-shaped input is ignored, yielding `None`,
    /// because JSON-RPC responses must not themselves receive responses.
    #[must_use]
    pub fn parse_json(input: &str) -> Option<Self> {
        match parse_incoming_line(input) {
            ParsedIncomingLine::Ignored => None,
            ParsedIncomingLine::Single(message) => Some(Self::Single(message)),
            ParsedIncomingLine::Malformed { raw, error } => Some(Self::Malformed { raw, error }),
            ParsedIncomingLine::Batch(batch) => Some(Self::Batch(batch)),
        }
    }

    /// Serialize this frame to its JSON-RPC wire representation.
    ///
    /// # Errors
    ///
    /// Returns an internal error if a valid message or batch cannot be
    /// serialized. Malformed frames return their original wire text unchanged.
    pub fn to_json(&self) -> Result<String, crate::Error> {
        match self {
            Self::Single(message) => {
                serde_json::to_string(message).map_err(crate::Error::into_internal_error)
            }
            Self::Malformed { raw, .. } => Ok(raw.clone()),
            Self::Batch(batch) => {
                serde_json::to_string(batch).map_err(crate::Error::into_internal_error)
            }
        }
    }
}

/// Transport outgoing actor for line streams: Serializes RawJsonRpcMessage and yields lines.
///
/// This is a line-based variant of `transport_outgoing_actor` that works with a Sink<String>
/// instead of an AsyncWrite byte stream. This enables interception of lines before they are
/// written to the underlying transport.
///
/// This actor handles transport mechanics:
/// - Serializes RawJsonRpcMessage to JSON strings
/// - Yields newline-terminated strings
/// - Handles serialization errors
///
/// This is the transport layer - it has no knowledge of protocol semantics (IDs, correlation, etc.).
async fn transport_outgoing_frames_actor(
    transport_rx: impl futures::Stream<Item = TransportFrame>,
    outgoing_lines: impl futures::Sink<String, Error = std::io::Error>,
) -> Result<(), crate::Error> {
    use futures::SinkExt;
    let mut transport_rx = pin!(transport_rx);
    let mut outgoing_lines = pin!(outgoing_lines);

    while let Some(frame) = transport_rx.next().await {
        let json_rpc_message = match frame {
            TransportFrame::Single(message) => message,
            TransportFrame::Malformed { raw, .. } => {
                let raw = malformed_line_value(raw)?;
                tracing::trace!(message = ?raw, "Relaying invalid JSON-RPC value");
                outgoing_lines
                    .send(raw)
                    .await
                    .map_err(crate::Error::into_internal_error)?;
                continue;
            }
            TransportFrame::Batch(batch) => {
                let line =
                    serde_json::to_string(&batch).map_err(crate::Error::into_internal_error)?;
                tracing::trace!(message = %line, "Sending JSON-RPC batch");
                outgoing_lines
                    .send(line)
                    .await
                    .map_err(crate::Error::into_internal_error)?;
                continue;
            }
        };
        match serde_json::to_string(&json_rpc_message) {
            Ok(line) => {
                tracing::trace!(message = %line, "Sending JSON-RPC message");
                outgoing_lines
                    .send(line)
                    .await
                    .map_err(crate::Error::into_internal_error)?;
            }

            Err(serialization_error) => {
                match json_rpc_message {
                    RawJsonRpcMessage::Request(_) | RawJsonRpcMessage::Notification(_) => {
                        // If we failed to serialize a request,
                        // just ignore it.
                        //
                        // Q: (Maybe it'd be nice to "reply" with an error?)
                        tracing::error!(
                            ?serialization_error,
                            "Failed to serialize request, ignoring"
                        );
                    }
                    RawJsonRpcMessage::Response(response) => {
                        // If we failed to serialize a *response*,
                        // send an error in response.
                        let id = match response {
                            Response::Result { id, .. } | Response::Error { id, .. } => id,
                        };
                        tracing::error!(
                            ?serialization_error,
                            ?id,
                            "Failed to serialize response, sending internal_error instead"
                        );
                        let error_line = serde_json::to_string(&RawJsonRpcMessage::response(
                            id,
                            Err(crate::Error::internal_error()),
                        ))
                        .unwrap();
                        outgoing_lines
                            .send(error_line)
                            .await
                            .map_err(crate::Error::into_internal_error)?;
                    }
                }
            }
        }
    }
    Ok(())
}

fn malformed_line_value(raw: String) -> Result<String, crate::Error> {
    if !raw.contains('\r') && !raw.contains('\n') {
        return Ok(raw);
    }

    match serde_json::from_str::<serde_json::Value>(&raw) {
        Ok(value) => serde_json::to_string(&value),
        Err(_) => serde_json::to_string(&raw),
    }
    .map_err(crate::Error::into_internal_error)
}

pub(super) async fn transport_outgoing_lines_actor(
    transport_rx: mpsc::UnboundedReceiver<TransportFrame>,
    outgoing_lines: impl futures::Sink<String, Error = std::io::Error>,
) -> Result<(), crate::Error> {
    transport_outgoing_frames_actor(transport_rx, outgoing_lines).await
}

/// Transport incoming actor for line streams: Parses lines into RawJsonRpcMessage values.
///
/// This is a line-based variant of `transport_incoming_actor` that works with a
/// Stream<Item = io::Result<String>> instead of an AsyncRead byte stream. This enables
/// interception of lines before they are parsed.
///
/// This actor handles transport mechanics:
/// - Reads lines from the stream
/// - Parses individual messages and retains batch arrays in entry order
/// - Handles malformed JSON, empty batches, and invalid batch entries
///
/// This is the transport layer - it has no knowledge of protocol semantics.
pub(super) async fn transport_incoming_lines_actor(
    incoming_lines: impl futures::Stream<Item = std::io::Result<String>>,
    transport_tx: mpsc::UnboundedSender<TransportFrame>,
) -> Result<(), crate::Error> {
    let mut incoming_lines = pin!(incoming_lines);
    while let Some(line_result) = incoming_lines.next().await {
        let line = line_result.map_err(crate::Error::into_internal_error)?;
        tracing::trace!(message = %line, "Received JSON-RPC message");

        match parse_incoming_line(&line) {
            ParsedIncomingLine::Ignored => {}
            ParsedIncomingLine::Single(message) => {
                transport_tx
                    .unbounded_send(TransportFrame::Single(message))
                    .map_err(crate::Error::into_internal_error)?;
            }
            ParsedIncomingLine::Malformed { raw, error } => {
                transport_tx
                    .unbounded_send(TransportFrame::Malformed { raw, error })
                    .map_err(crate::Error::into_internal_error)?;
            }
            ParsedIncomingLine::Batch(entries) => {
                transport_tx
                    .unbounded_send(TransportFrame::Batch(entries))
                    .map_err(crate::Error::into_internal_error)?;
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use super::*;
    use crate::ErrorCode;

    #[test]
    fn parses_batch_entries_independently() {
        let ParsedIncomingLine::Batch(batch) = parse_incoming_line(
            r#"[
                {"jsonrpc":"2.0","id":1,"method":"one","params":{}},
                17,
                {"jsonrpc":"2.0","method":"two","params":{}}
            ]"#,
        ) else {
            panic!("expected a JSON-RPC batch");
        };

        let entries = batch.iter_results().collect::<Vec<_>>();
        assert_eq!(entries.len(), 3);
        assert!(matches!(entries[0], Ok(RawJsonRpcMessage::Request(_))));
        assert_eq!(entries[1].unwrap_err().code, ErrorCode::InvalidRequest);
        assert!(matches!(entries[2], Ok(RawJsonRpcMessage::Notification(_))));
    }

    #[test]
    fn ignores_invalid_members_of_response_batches() {
        let ParsedIncomingLine::Batch(batch) = parse_incoming_line(
            r#"[
                {"jsonrpc":"2.0","id":1,"result":{"ok":true}},
                17,
                {"jsonrpc":"2.0","id":2,"result":null,"error":{"code":-32603,"message":"Internal error"}},
                {"jsonrpc":"2.0","id":3,"error":{"code":-32603,"message":"Internal error"}}
            ]"#,
        ) else {
            panic!("expected a JSON-RPC batch");
        };

        assert_eq!(batch.len(), 2);
        assert!(
            batch
                .iter_results()
                .all(|entry| matches!(entry, Ok(RawJsonRpcMessage::Response(_))))
        );
    }

    #[test]
    fn ignores_entirely_malformed_response_shaped_batch() {
        assert!(matches!(
            parse_incoming_line(
                r#"[
                {"jsonrpc":"2.0","id":1,"result":null,"error":{"code":-32603,"message":"Internal error"}}
            ]"#
            ),
            ParsedIncomingLine::Ignored
        ));
    }

    #[test]
    fn preserves_invalid_call_shaped_member_beside_response() {
        let ParsedIncomingLine::Batch(batch) = parse_incoming_line(
            r#"[
                {"jsonrpc":"2.0","id":1,"result":null},
                {"jsonrpc":"2.0","method":1}
            ]"#,
        ) else {
            panic!("expected a JSON-RPC batch");
        };

        let entries = batch.iter_results().collect::<Vec<_>>();
        assert_eq!(entries.len(), 2);
        assert!(matches!(entries[0], Ok(RawJsonRpcMessage::Response(_))));
        assert_eq!(entries[1].unwrap_err().code, ErrorCode::InvalidRequest);
    }

    #[test]
    fn ignores_malformed_response_shaped_member_beside_request() {
        let ParsedIncomingLine::Batch(batch) = parse_incoming_line(
            r#"[
                {"jsonrpc":"2.0","id":1,"method":"one","params":{}},
                {"jsonrpc":"2.0","id":2,"result":null,"error":{"code":-32603,"message":"Internal error"}}
            ]"#,
        ) else {
            panic!("expected a JSON-RPC batch");
        };

        let entries = batch.iter_results().collect::<Vec<_>>();
        assert_eq!(entries.len(), 1);
        assert!(matches!(entries[0], Ok(RawJsonRpcMessage::Request(_))));
    }

    #[test]
    fn ignores_malformed_standalone_response() {
        assert!(matches!(
            parse_incoming_line(
                r#"{"jsonrpc":"2.0","id":1,"result":null,"error":{"code":-32603,"message":"Internal error"}}"#
            ),
            ParsedIncomingLine::Ignored
        ));
    }

    #[test]
    fn preserves_malformed_call_shaped_standalone_message() {
        let ParsedIncomingLine::Malformed { error, .. } =
            parse_incoming_line(r#"{"jsonrpc":"2.0","id":1,"method":"one","result":null}"#)
        else {
            panic!("expected one invalid-request error");
        };

        assert_eq!(error.code, ErrorCode::InvalidRequest);
    }

    #[test]
    fn parses_valid_standalone_response() {
        assert!(matches!(
            parse_incoming_line(r#"{"jsonrpc":"2.0","id":1,"result":{"ok":true}}"#),
            ParsedIncomingLine::Single(RawJsonRpcMessage::Response(_))
        ));
    }

    #[test]
    fn all_invalid_batch_defaults_to_call_errors() {
        let ParsedIncomingLine::Batch(batch) = parse_incoming_line("[1, 2, 3]") else {
            panic!("expected a JSON-RPC batch");
        };

        assert_eq!(batch.len(), 3);
        assert!(
            batch
                .iter_results()
                .all(|entry| entry.unwrap_err().code == ErrorCode::InvalidRequest)
        );
    }

    #[test]
    fn empty_batch_is_an_invalid_request() {
        let ParsedIncomingLine::Malformed { raw, error } = parse_incoming_line("[]") else {
            panic!("expected one invalid-request error");
        };

        assert_eq!(raw, "[]");
        assert_eq!(error.code, ErrorCode::InvalidRequest);
    }

    #[test]
    fn malformed_json_is_a_parse_error() {
        let ParsedIncomingLine::Malformed { raw, error } = parse_incoming_line("[") else {
            panic!("expected one parse error");
        };

        assert_eq!(raw, "[");
        assert_eq!(error.code, ErrorCode::ParseError);
    }

    #[test]
    fn valid_json_with_an_invalid_envelope_is_an_invalid_request() {
        let ParsedIncomingLine::Malformed { raw, error } = parse_incoming_line("17") else {
            panic!("expected one invalid-request error");
        };

        assert_eq!(raw, "17");
        assert_eq!(error.code, ErrorCode::InvalidRequest);
    }

    #[tokio::test]
    async fn multiline_malformed_frame_is_written_as_one_line_value() {
        let raw = "not json\r\n{\"jsonrpc\":\"2.0\",\"method\":\"injected\"}".to_string();
        let captured = Arc::new(Mutex::new(Vec::new()));
        let outgoing = futures::sink::unfold(captured.clone(), |captured, line| async move {
            captured.lock().unwrap().push(line);
            Ok::<_, std::io::Error>(captured)
        });

        transport_outgoing_frames_actor(
            futures::stream::iter([TransportFrame::Malformed {
                raw: raw.clone(),
                error: crate::Error::parse_error(),
            }]),
            outgoing,
        )
        .await
        .unwrap();

        let lines = captured.lock().unwrap();
        assert_eq!(lines.len(), 1);
        assert!(!lines[0].contains('\r') && !lines[0].contains('\n'));
        assert_eq!(serde_json::from_str::<String>(&lines[0]).unwrap(), raw);
    }

    #[test]
    fn multiline_invalid_json_rpc_value_is_compacted_without_changing_value() {
        let raw = "{\n  \"jsonrpc\": \"2.0\",\n  \"method\": 1\n}".to_string();
        let expected = serde_json::from_str::<serde_json::Value>(&raw).unwrap();
        let line = malformed_line_value(raw).unwrap();

        assert!(!line.contains('\r') && !line.contains('\n'));
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&line).unwrap(),
            expected
        );
    }
}
