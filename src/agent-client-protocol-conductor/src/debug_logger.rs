//! Debug logging for conductor

use chrono::Local;
use std::io::Write;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;
use tokio::fs::OpenOptions;
use tokio::io::AsyncWriteExt;
use tokio::sync::Mutex;

/// Redact credential values in a JSON line (protocol messages, or a JSON
/// component config). Other text is kept as-is: it is not an ACP message.
fn redact_json_line(line: &str) -> String {
    match serde_json::from_str::<serde_json::Value>(line) {
        Ok(mut value) => {
            crate::trace::redact_credentials(&mut value);
            value.to_string()
        }
        Err(_) => line.to_owned(),
    }
}

/// A debug logger that writes lines to a timestamped log file
pub struct DebugLogger {
    writer: Arc<Mutex<tokio::io::BufWriter<tokio::fs::File>>>,
    start_time: Instant,
}

impl DebugLogger {
    /// Create a new debug logger with a timestamped log file
    pub async fn new(
        debug_dir: Option<PathBuf>,
        component_commands: &[String],
    ) -> Result<Self, std::io::Error> {
        // Create log directory
        let log_dir = debug_dir.unwrap_or_else(|| PathBuf::from("."));
        tokio::fs::create_dir_all(&log_dir).await?;

        // Create timestamped log file
        let timestamp = Local::now().format("%Y%m%d-%H%M%S");
        let log_file = log_dir.join(format!("{timestamp}.log"));

        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&log_file)
            .await?;

        let mut writer = tokio::io::BufWriter::new(file);

        // Write header
        writer.write_all(b"=== Conductor Debug Log ===\n").await?;
        writer
            .write_all(format!("Started: {}\n", Local::now().to_rfc3339()).as_bytes())
            .await?;
        writer.write_all(b"Components:\n").await?;
        for (i, cmd) in component_commands.iter().enumerate() {
            writer
                .write_all(format!("  {i}: {}\n", redact_json_line(cmd)).as_bytes())
                .await?;
        }
        writer.write_all(b"========================\n").await?;
        writer.flush().await?;

        eprintln!("Debug logging to: {}", log_file.display());

        Ok(Self {
            writer: Arc::new(Mutex::new(writer)),
            start_time: Instant::now(),
        })
    }

    /// Get the elapsed time since the logger started, in milliseconds
    fn elapsed_ms(&self) -> u128 {
        self.start_time.elapsed().as_millis()
    }

    /// Create a callback for a specific component
    pub fn create_callback(
        &self,
        component_label: String,
    ) -> impl Fn(&str, agent_client_protocol::LineDirection) + Send + Sync + 'static {
        let writer = self.writer.clone();
        let start_time = self.start_time;
        move |line: &str, direction: agent_client_protocol::LineDirection| {
            let writer = writer.clone();
            let component_label = component_label.clone();
            let line = line.to_string();
            let elapsed_ms = start_time.elapsed().as_millis();
            tokio::spawn(async move {
                let arrow = match direction {
                    agent_client_protocol::LineDirection::Stdin => "→",
                    agent_client_protocol::LineDirection::Stdout => "←",
                    agent_client_protocol::LineDirection::Stderr => "!",
                };

                let cleaned_line = match direction {
                    // Strip ANSI escape codes from stderr to keep logs clean
                    agent_client_protocol::LineDirection::Stderr => {
                        let bytes = strip_ansi_escapes::strip(&line);
                        String::from_utf8_lossy(&bytes).to_string()
                    }
                    agent_client_protocol::LineDirection::Stdin
                    | agent_client_protocol::LineDirection::Stdout => redact_json_line(&line),
                };

                let log_line =
                    format!("{component_label} {arrow} +{elapsed_ms}ms {cleaned_line}\n");
                let mut writer = writer.lock().await;
                drop(writer.write_all(log_line.as_bytes()).await);
                drop(writer.flush().await);
            });
        }
    }

    /// Write a tracing log line to the debug file
    /// This is synchronous and blocks, suitable for use with tracing's MakeWriter
    pub fn write_tracing_log(&self, line: &str) {
        let writer = self.writer.clone();
        let line = line.to_string();
        let elapsed_ms = self.elapsed_ms();
        tokio::spawn(async move {
            // Strip ANSI escape codes to keep logs clean
            let bytes = strip_ansi_escapes::strip(&line);
            let cleaned_line = String::from_utf8_lossy(&bytes);

            let log_line = format!("C ! +{}ms {}\n", elapsed_ms, cleaned_line.trim_end());
            let mut writer = writer.lock().await;
            drop(writer.write_all(log_line.as_bytes()).await);
            drop(writer.flush().await);
        });
    }

    /// Create a writer for tracing logs
    pub fn create_tracing_writer(&self) -> DebugLogWriter {
        DebugLogWriter {
            logger: self.clone(),
            buffer: Vec::new(),
        }
    }
}

impl Clone for DebugLogger {
    fn clone(&self) -> Self {
        Self {
            writer: self.writer.clone(),
            start_time: self.start_time,
        }
    }
}

/// A writer that sends tracing logs to the debug logger
pub struct DebugLogWriter {
    logger: DebugLogger,
    buffer: Vec<u8>,
}

impl Write for DebugLogWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.buffer.extend_from_slice(buf);

        // Write complete lines
        while let Some(newline_pos) = self.buffer.iter().position(|&b| b == b'\n') {
            let line = self.buffer.drain(..=newline_pos).collect::<Vec<_>>();
            let line_str = String::from_utf8_lossy(&line);
            self.logger.write_tracing_log(&line_str);
        }

        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        if !self.buffer.is_empty() {
            let line = std::mem::take(&mut self.buffer);
            let line_str = String::from_utf8_lossy(&line);
            self.logger.write_tracing_log(&line_str);
        }
        Ok(())
    }
}

impl Clone for DebugLogWriter {
    fn clone(&self) -> Self {
        Self {
            logger: self.logger.clone(),
            buffer: Vec::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::redact_json_line;

    #[test]
    fn protocol_lines_and_component_configs_keep_only_credential_names() {
        let line = r#"{"jsonrpc":"2.0","id":1,"method":"session/new","params":{"cwd":"/work","mcpServers":[{"name":"s","command":"/bin/s","args":[],"env":[{"name":"API_KEY","value":"private"}]},{"type":"http","name":"h","url":"https://example.test/mcp","headers":[{"name":"X-Token","value":"private"}]}]}}"#;
        let logged = redact_json_line(line);
        assert!(!logged.contains("private"), "{logged}");
        assert!(logged.contains("API_KEY") && logged.contains("X-Token"));

        let config = r#"{"command":"agent","env":{"OPENAI_API_KEY":"private"}}"#;
        let logged = redact_json_line(config);
        assert!(!logged.contains("private"), "{logged}");
        assert!(logged.contains("OPENAI_API_KEY"));

        assert_eq!(redact_json_line("python agent.py"), "python agent.py");
    }
}
