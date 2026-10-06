//! Ordinary diagnostics never format peer-controlled content, including methods,
//! IDs, malformed input, close reasons, and error text. Explicit protocol
//! recordings are a separate, opt-in facility.

use agent_client_protocol::{RawJsonRpcMessage, TransportFrame};

pub(crate) fn inbound(frame: &TransportFrame, byte_len: usize, transport: &'static str) {
    let kind = match frame {
        TransportFrame::Single(RawJsonRpcMessage::Request(_)) => "request",
        TransportFrame::Single(RawJsonRpcMessage::Notification(_)) => "notification",
        TransportFrame::Single(RawJsonRpcMessage::Response(_)) => "response",
        TransportFrame::Batch(_) => "batch",
        TransportFrame::Malformed { .. } => "malformed",
    };
    tracing::trace!(byte_len, kind, transport, "Client → agent");
}

pub(crate) fn outbound(byte_len: usize, transport: &'static str) {
    // Do not reparse serialized output solely for logging.
    tracing::trace!(byte_len, transport, "Agent → client");
}

#[cfg(test)]
mod tests {
    use std::io::Write;
    use std::sync::{Arc, Mutex};

    use super::*;

    #[derive(Clone, Default)]
    struct Capture(Arc<Mutex<Vec<u8>>>);

    impl Write for Capture {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(bytes);
            Ok(bytes.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn diagnostics_are_bounded_and_never_format_message_content() {
        let secret = "PRIVATE_PROMPT_IMAGE_FILE_CREDENTIAL".repeat(32_768);
        let messages = [
            serde_json::json!({"jsonrpc":"2.0","id":secret,"method":secret,
                "params":{"prompt":secret,"image":secret,"file":secret}})
            .to_string(),
            serde_json::json!({"jsonrpc":"2.0","method":secret,"params":[secret]}).to_string(),
            serde_json::json!({"jsonrpc":"2.0","id":secret,
                "error":{"code":-32000,"message":secret,"data":secret}})
            .to_string(),
            serde_json::json!([{"jsonrpc":"2.0","method":secret,"params":[secret]}]).to_string(),
            format!("malformed {secret}"),
        ];
        let capture = Capture::default();
        let writer = capture.clone();
        let subscriber = tracing_subscriber::fmt()
            .without_time()
            .with_ansi(false)
            .with_max_level(tracing::Level::TRACE)
            .with_writer(move || writer.clone())
            .finish();
        tracing::subscriber::with_default(subscriber, || {
            for text in &messages {
                let frame = TransportFrame::parse_json(text);
                inbound(&frame, text.len(), "HTTP POST");
                inbound(&frame, text.len(), "WebSocket");
                outbound(text.len(), "SSE");
                outbound(text.len(), "WebSocket");
            }
        });
        let output = String::from_utf8(capture.0.lock().unwrap().clone()).unwrap();
        assert!(!output.contains("PRIVATE"));
        assert!(
            output.len() < 4096,
            "diagnostics must not grow with payloads"
        );
        for kind in ["request", "notification", "response", "batch", "malformed"] {
            assert!(output.contains(&format!("kind=\"{kind}\"")));
        }
        for text in &messages {
            assert!(output.contains(&format!("byte_len={}", text.len())));
        }
    }
}
