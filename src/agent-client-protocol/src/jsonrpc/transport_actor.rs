use std::pin::pin;

// Types re-exported from crate root
use crate::jsonrpc::{RawJsonRpcMessage, TransportBatch, TransportBatchEntry, TransportFrame};
use crate::schema::v1::Response;
use futures::StreamExt as _;
use serde::Deserialize as _;

/// Maximum bytes in one wire value (excluding its newline).
pub const MAX_FRAME_BYTES: usize = 16 * 1024 * 1024;
/// Maximum number of JSON-RPC values carried in one batch.
pub const MAX_BATCH_ENTRIES: usize = 64;

enum ParsedIncomingLine {
    Single(RawJsonRpcMessage),
    Malformed { raw: String, error: crate::Error },
    Batch(TransportBatch),
}

fn parse_incoming_line(line: &str) -> ParsedIncomingLine {
    if line.len() > MAX_FRAME_BYTES {
        return ParsedIncomingLine::Malformed {
            raw: String::new(),
            error: crate::Error::invalid_request().data("JSON-RPC frame exceeds maximum size"),
        };
    }
    let value = match serde_json::from_str::<serde_json::Value>(line) {
        Ok(value) => value,
        Err(error) => {
            tracing::debug!(?error, "Failed to parse incoming JSON-RPC JSON");
            return ParsedIncomingLine::Malformed {
                raw: line.to_owned(),
                error: crate::Error::parse_error().data(serde_json::json!({
                    "line": line.chars().take(256).collect::<String>()
                })),
            };
        }
    };

    match value {
        serde_json::Value::Array(entries) if entries.is_empty() => ParsedIncomingLine::Malformed {
            raw: line.to_owned(),
            error: crate::Error::invalid_request(),
        },
        serde_json::Value::Array(entries) if entries.len() > MAX_BATCH_ENTRIES => {
            ParsedIncomingLine::Malformed {
                raw: String::new(),
                error: crate::Error::invalid_request().data("JSON-RPC batch exceeds maximum width"),
            }
        }
        serde_json::Value::Array(entries) => {
            let entries = entries
                .into_iter()
                .map(|entry| match RawJsonRpcMessage::deserialize(&entry) {
                    Ok(message) => TransportBatchEntry::message(message),
                    Err(error) => {
                        tracing::debug!(?error, "Invalid JSON-RPC batch entry");
                        TransportBatchEntry::malformed(entry, crate::Error::invalid_request())
                    }
                })
                .collect::<Vec<_>>();

            ParsedIncomingLine::Batch(
                TransportBatch::from_entries(entries)
                    .expect("a parsed non-empty JSON array retains at least one entry"),
            )
        }
        value => match serde_json::from_value(value) {
            Ok(message) => ParsedIncomingLine::Single(message),
            Err(error) => {
                tracing::debug!(?error, "Invalid JSON-RPC message");
                ParsedIncomingLine::Malformed {
                    raw: line.to_owned(),
                    error: crate::Error::invalid_request(),
                }
            }
        },
    }
}

/// Read newline-delimited UTF-8 without allocating an unterminated line larger
/// than the frame budget. An oversized line terminates the transport explicitly.
pub fn bounded_lines<R: futures::AsyncRead + Unpin>(
    input: R,
) -> impl futures::Stream<Item = std::io::Result<String>> {
    use futures::io::BufReader;
    use futures::{AsyncBufReadExt, stream};
    stream::unfold(Some(BufReader::new(input)), |reader| async move {
        let mut reader = reader?;
        let mut bytes = Vec::new();
        loop {
            let chunk = match reader.fill_buf().await {
                Ok(chunk) => chunk,
                Err(error) => return Some((Err(error), None)),
            };
            if chunk.is_empty() {
                return if bytes.is_empty() {
                    None
                } else {
                    Some((
                        String::from_utf8(bytes)
                            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e)),
                        None,
                    ))
                };
            }
            let width = chunk
                .iter()
                .position(|&b| b == b'\n')
                .map_or(chunk.len(), |i| i + 1);
            if bytes.len() + width > MAX_FRAME_BYTES + 1 {
                return Some((
                    Err(std::io::Error::new(
                        std::io::ErrorKind::InvalidData,
                        "JSON-RPC line exceeds maximum frame size",
                    )),
                    None,
                ));
            }
            bytes.extend_from_slice(&chunk[..width]);
            reader.consume_unpin(width);
            if bytes.last() == Some(&b'\n') {
                bytes.pop();
                if bytes.last() == Some(&b'\r') {
                    bytes.pop();
                }
                return Some((
                    String::from_utf8(bytes)
                        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e)),
                    Some(reader),
                ));
            }
        }
    })
}

impl TransportFrame {
    /// Parse one JSON-RPC wire value while preserving batch boundaries.
    ///
    /// Every malformed value is retained as an explicit malformed frame or
    /// batch entry. Standalone malformed input keeps its original text; batch
    /// entries keep their parsed JSON values, source order, and batch boundary,
    /// though reserialization may normalize whitespace. Protocol actors decide
    /// whether a malformed value is call-shaped and requires an Error Response.
    #[must_use]
    pub fn parse_json(input: &str) -> Self {
        match parse_incoming_line(input) {
            ParsedIncomingLine::Single(message) => Self::Single(message),
            ParsedIncomingLine::Malformed { raw, error } => Self::Malformed { raw, error },
            ParsedIncomingLine::Batch(batch) => Self::Batch(batch),
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

/// Transport outgoing actor for line streams: serializes [`TransportFrame`] values and yields
/// lines.
///
/// This is a line-based variant of `transport_outgoing_actor` that works with a Sink<String>
/// instead of an AsyncWrite byte stream. This enables interception of lines before they are
/// written to the underlying transport.
///
/// This actor handles transport mechanics:
/// - Serializes single messages, malformed values, and batches to JSON strings
/// - Yields newline-terminated strings
/// - Handles serialization errors
///
/// This is the transport layer - it has no knowledge of protocol semantics (IDs, correlation, etc.).
async fn transport_outgoing_frames_actor(
    transport_rx: impl futures::Stream<Item = super::BudgetedFrame>,
    outgoing_lines: impl futures::Sink<String, Error = std::io::Error>,
) -> Result<(), crate::Error> {
    use futures::SinkExt;
    let mut transport_rx = pin!(transport_rx);
    let mut outgoing_lines = pin!(outgoing_lines);

    while let Some(budgeted) = transport_rx.next().await {
        let (frame, _permit) = budgeted.into_parts();
        let json_rpc_message = match frame {
            TransportFrame::Single(message) => message,
            TransportFrame::Malformed { raw, .. } => {
                let raw = malformed_line_value(raw)?;
                tracing::trace!(message = ?raw, "Relaying invalid JSON-RPC value");
                ensure_frame_size(&raw)?;
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
                ensure_frame_size(&line)?;
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
                ensure_frame_size(&line)?;
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
                        ensure_frame_size(&error_line)?;
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

fn ensure_frame_size(line: &str) -> Result<(), crate::Error> {
    if line.len() > MAX_FRAME_BYTES {
        Err(crate::Error::invalid_request().data("outgoing JSON-RPC frame exceeds maximum size"))
    } else {
        Ok(())
    }
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
    transport_rx: super::FrameReceiver,
    outgoing_lines: impl futures::Sink<String, Error = std::io::Error>,
) -> Result<(), crate::Error> {
    transport_outgoing_frames_actor(transport_rx, outgoing_lines).await
}

/// Transport incoming actor for line streams: parses lines into [`TransportFrame`] values.
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
    transport_tx: super::FrameSender,
) -> Result<(), crate::Error> {
    let mut incoming_lines = pin!(incoming_lines);
    while let Some(line_result) = incoming_lines.next().await {
        let line = line_result.map_err(crate::Error::into_internal_error)?;
        tracing::trace!(message = %line, "Received JSON-RPC message");

        match parse_incoming_line(&line) {
            ParsedIncomingLine::Single(message) => {
                transport_tx
                    .send_frame(TransportFrame::Single(message))
                    .await
                    .map_err(crate::Error::into_internal_error)?;
            }
            ParsedIncomingLine::Malformed { raw, error } => {
                transport_tx
                    .send_frame(TransportFrame::Malformed { raw, error })
                    .await
                    .map_err(crate::Error::into_internal_error)?;
            }
            ParsedIncomingLine::Batch(entries) => {
                transport_tx
                    .send_frame(TransportFrame::Batch(entries))
                    .await
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
    fn rejects_batches_over_width_limit() {
        let batch = format!("[{}]", vec!["null"; MAX_BATCH_ENTRIES + 1].join(","));
        let ParsedIncomingLine::Malformed { error, .. } = parse_incoming_line(&batch) else {
            panic!("oversized batch must be rejected");
        };
        assert_eq!(error.code, ErrorCode::InvalidRequest);
    }

    #[tokio::test]
    async fn oversized_unterminated_line_fails_before_eof() {
        let input = futures::io::Cursor::new(vec![b'x'; MAX_FRAME_BYTES + 2]);
        let mut lines = Box::pin(bounded_lines(input));
        let error = lines
            .next()
            .await
            .expect("explicit framing failure")
            .unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
        assert!(lines.next().await.is_none());
    }

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
    fn preserves_every_invalid_member_of_response_batches() {
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

        let entries = batch.iter_results().collect::<Vec<_>>();
        assert_eq!(entries.len(), 4);
        assert!(matches!(entries[0], Ok(RawJsonRpcMessage::Response(_))));
        assert_eq!(entries[1].unwrap_err().code, ErrorCode::InvalidRequest);
        assert_eq!(entries[2].unwrap_err().code, ErrorCode::InvalidRequest);
        assert!(matches!(entries[3], Ok(RawJsonRpcMessage::Response(_))));
    }

    #[test]
    fn preserves_invalid_value_beside_malformed_response() {
        let ParsedIncomingLine::Batch(batch) = parse_incoming_line(
            r#"[
                17,
                {"jsonrpc":"2.0","id":1,"result":null,"error":{"code":-32603,"message":"Internal error"}}
            ]"#,
        ) else {
            panic!("expected a JSON-RPC batch");
        };

        let entries = batch.iter_results().collect::<Vec<_>>();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].unwrap_err().code, ErrorCode::InvalidRequest);
        assert_eq!(entries[1].unwrap_err().code, ErrorCode::InvalidRequest);
    }

    #[test]
    fn preserves_entirely_malformed_response_shaped_batch() {
        let ParsedIncomingLine::Batch(batch) = parse_incoming_line(
            r#"[
                {"jsonrpc":"2.0","id":1,"result":null,"error":{"code":-32603,"message":"Internal error"}}
            ]"#,
        ) else {
            panic!("expected a retained JSON-RPC batch");
        };

        let entries = batch.iter_results().collect::<Vec<_>>();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].unwrap_err().code, ErrorCode::InvalidRequest);
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
    fn preserves_malformed_response_shaped_member_beside_request() {
        let ParsedIncomingLine::Batch(batch) = parse_incoming_line(
            r#"[
                {"jsonrpc":"2.0","id":1,"method":"one","params":{}},
                {"jsonrpc":"2.0","id":2,"result":null,"error":{"code":-32603,"message":"Internal error"}}
            ]"#,
        ) else {
            panic!("expected a JSON-RPC batch");
        };

        let entries = batch.iter_results().collect::<Vec<_>>();
        assert_eq!(entries.len(), 2);
        assert!(matches!(entries[0], Ok(RawJsonRpcMessage::Request(_))));
        assert_eq!(entries[1].unwrap_err().code, ErrorCode::InvalidRequest);
    }

    #[test]
    fn preserves_malformed_standalone_response() {
        let ParsedIncomingLine::Malformed { error, .. } = parse_incoming_line(
            r#"{"jsonrpc":"2.0","id":1,"result":null,"error":{"code":-32603,"message":"Internal error"}}"#,
        ) else {
            panic!("expected one retained invalid response");
        };

        assert_eq!(error.code, ErrorCode::InvalidRequest);
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

        let (source, destination) = crate::Channel::duplex();
        source
            .tx
            .send_frame(TransportFrame::Malformed {
                raw: raw.clone(),
                error: crate::Error::parse_error(),
            })
            .await
            .unwrap();
        drop(source);
        transport_outgoing_frames_actor(destination.rx, outgoing)
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
