//! Debug logging for conductor

use chrono::Local;
use std::io::Write;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;
use tokio::fs::OpenOptions;
use tokio::io::AsyncWriteExt;
use tokio::sync::Mutex;

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
                .write_all(format!("  {i}: {cmd}\n").as_bytes())
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
    ) -> impl Fn(&str, agent_client_protocol_tokio::LineDirection) + Send + Sync + 'static {
        let writer = self.writer.clone();
        let start_time = self.start_time;
        move |line: &str, direction: agent_client_protocol_tokio::LineDirection| {
            let writer = writer.clone();
            let component_label = component_label.clone();
            let line = line.to_string();
            let elapsed_ms = start_time.elapsed().as_millis();
            tokio::spawn(async move {
                let arrow = match direction {
                    agent_client_protocol_tokio::LineDirection::Stdin => "→",
                    agent_client_protocol_tokio::LineDirection::Stdout => "←",
                    agent_client_protocol_tokio::LineDirection::Stderr => "!",
                };

                // Strip ANSI escape codes from stderr to keep logs clean
                let cleaned_line = if matches!(
                    direction,
                    agent_client_protocol_tokio::LineDirection::Stderr
                ) {
                    let bytes = strip_ansi_escapes::strip(&line);
                    String::from_utf8_lossy(&bytes).to_string()
                } else {
                    line
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
            let line = self.buffer.drain(..).collect::<Vec<_>>();
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
