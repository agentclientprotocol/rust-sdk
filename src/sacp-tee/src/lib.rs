//! sacp-tee - A debugging proxy that logs all ACP traffic
//!
//! This proxy sits transparently between two ACP endpoints, passing messages through
//! while logging them to a file for debugging purposes.

use anyhow::Result;
use sacp::component::Component;
use sacp::{Handled, JrMessageHandler, MessageAndCx};
use sacp_proxy::AcpProxyExt;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use tokio::sync::mpsc;

/// A JSON-RPC message representation for logging
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum JsonRpcMessage {
    Request {
        id: serde_json::Value,
        #[serde(flatten)]
        message: sacp::UntypedMessage,
    },
    Notification {
        #[serde(flatten)]
        message: sacp::UntypedMessage,
    },
    Reply {
        id: serde_json::Value,
        result: serde_json::Value,
    },
}

/// A log entry representing a message passing through the proxy
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LogEntry {
    pub timestamp: String,
    pub direction: String,
    pub message: JsonRpcMessage,
}

impl LogEntry {
    fn new(direction: &str, message: JsonRpcMessage) -> Self {
        Self {
            timestamp: chrono::Utc::now().to_rfc3339(),
            direction: direction.to_string(),
            message,
        }
    }
}

/// Log writer actor that receives log entries and writes them to disk
pub struct LogWriter {
    log_file: PathBuf,
    receiver: mpsc::UnboundedReceiver<LogEntry>,
}

impl LogWriter {
    pub fn new(log_file: PathBuf) -> (Self, mpsc::UnboundedSender<LogEntry>) {
        let (tx, rx) = mpsc::unbounded_channel();
        (
            Self {
                log_file,
                receiver: rx,
            },
            tx,
        )
    }

    /// Run the log writer actor
    pub async fn run(mut self) -> Result<()> {
        use tokio::io::AsyncWriteExt;

        let file = tokio::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.log_file)
            .await?;
        let mut writer = tokio::io::BufWriter::new(file);

        while let Some(entry) = self.receiver.recv().await {
            let json = serde_json::to_string(&entry)?;
            writer.write_all(json.as_bytes()).await?;
            writer.write_all(b"\n").await?;
            writer.flush().await?;
        }

        Ok(())
    }
}

/// Handler that logs messages passing through
pub struct TeeHandler {
    log_tx: mpsc::UnboundedSender<LogEntry>,
    next_id: u64,
}

impl TeeHandler {
    pub fn new(log_tx: mpsc::UnboundedSender<LogEntry>) -> Self {
        Self { log_tx, next_id: 1 }
    }

    fn log_entry(&self, entry: LogEntry) {
        // Fire and forget - if the channel is closed, we just drop the log
        let _ = self.log_tx.send(entry);
    }

    fn allocate_id(&mut self) -> u64 {
        let id = self.next_id;
        self.next_id += 1;
        id
    }
}

impl JrMessageHandler for TeeHandler {
    fn describe_chain(&self) -> impl std::fmt::Debug {
        "tee"
    }

    async fn handle_message(
        &mut self,
        message: MessageAndCx,
    ) -> Result<Handled<MessageAndCx>, sacp::Error> {
        match message {
            MessageAndCx::Request(request, request_cx) => {
                // Allocate a synthetic ID for tracking this request/response pair
                let synthetic_id = self.allocate_id();

                // Log the outgoing request
                let json_msg = JsonRpcMessage::Request {
                    id: serde_json::json!(synthetic_id),
                    message: request.clone(),
                };
                let entry = LogEntry::new("downstream", json_msg);
                self.log_entry(entry);

                // Wrap the request context to log the response when it comes back
                let log_tx = self.log_tx.clone();

                let wrapped_cx = request_cx.wrap_params(move |_method, result| {
                    // Log the response
                    let result_value = match &result {
                        Ok(value) => serde_json::to_value(value).unwrap_or(serde_json::Value::Null),
                        Err(e) => serde_json::json!({ "error": e.to_string() }),
                    };

                    let json_msg = JsonRpcMessage::Reply {
                        id: serde_json::json!(synthetic_id),
                        result: result_value,
                    };
                    let entry = LogEntry::new("upstream", json_msg);

                    let _ = log_tx.send(entry);

                    result
                });

                // Return unhandled with the wrapped context
                Ok(Handled::No(MessageAndCx::Request(request, wrapped_cx)))
            }
            MessageAndCx::Notification(notification, cx) => {
                // Log the notification
                let json_msg = JsonRpcMessage::Notification {
                    message: notification.clone(),
                };
                let entry = LogEntry::new("downstream", json_msg);
                self.log_entry(entry);

                // Return unhandled so it continues down the chain
                Ok(Handled::No(MessageAndCx::Notification(notification, cx)))
            }
        }
    }
}

/// The Tee component - can be used as a component in a larger proxy chain
/// or run standalone as a binary
pub struct Tee {
    log_file: PathBuf,
}

impl Tee {
    pub fn new(log_file: PathBuf) -> Self {
        Self { log_file }
    }
}

impl Component for Tee {
    async fn serve(self, client: impl Component) -> Result<(), sacp::Error> {
        // Create the log writer actor
        let (log_writer, log_tx) = LogWriter::new(self.log_file.clone());

        // Spawn the log writer
        tokio::spawn(async move {
            if let Err(e) = log_writer.run().await {
                tracing::error!("Log writer failed: {}", e);
            }
        });

        // Create the handler chain
        sacp::JrHandlerChain::new()
            .name("sacp-tee")
            .with_handler(TeeHandler::new(log_tx))
            .proxy()
            .connect_to(client)?
            .serve()
            .await
    }
}

/// Run the tee proxy as a standalone binary connected to stdio
pub async fn run(log_file: PathBuf) -> Result<()> {
    // Initialize tracing
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    tracing::info!("Starting sacp-tee, logging to: {}", log_file.display());

    Tee::new(log_file).serve(sacp_tokio::Stdio).await?;

    Ok(())
}
