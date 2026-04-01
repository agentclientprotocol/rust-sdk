//! Test for trace generation.
//!
//! This test verifies that:
//! 1. Conductor correctly generates trace events when trace_to() is enabled
//! 2. Trace file contains valid JSON lines
//! 3. Events capture the message flow through the conductor
//!
//! Run `just prep-tests` before running this test.

use agent_client_protocol_conductor::{ConductorImpl, ProxiesAndAgent};
use agent_client_protocol_test::test_binaries::{arrow_proxy_example, testy};
use agent_client_protocol_test::testy::TestyCommand;
use agent_client_protocol_tokio::AcpAgent;
use tokio::io::duplex;
use tokio_util::compat::{TokioAsyncReadCompatExt, TokioAsyncWriteCompatExt};

#[tokio::test]
async fn test_trace_generation() -> Result<(), agent_client_protocol_core::Error> {
    // Enable tracing if RUST_LOG is set
    let _ = tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .with_ansi(false)
        .try_init();
    // Create a temp file for the trace
    let trace_path = std::env::temp_dir().join(format!("trace_test_{}.jsons", std::process::id()));

    // Create the component chain: arrow_proxy -> eliza
    // Uses pre-built binaries to avoid cargo run races during `cargo test --all`
    let arrow_proxy_agent =
        AcpAgent::from_args([arrow_proxy_example().to_string_lossy().to_string()])?;
    let eliza_agent = testy();

    // Create duplex streams for editor <-> conductor communication
    let (editor_write, conductor_read) = duplex(8192);
    let (conductor_write, editor_read) = duplex(8192);

    let trace_path_clone = trace_path.clone();

    // Spawn the conductor with tracing enabled
    let conductor_handle = tokio::spawn(async move {
        ConductorImpl::new_agent(
            "conductor".to_string(),
            ProxiesAndAgent::new(eliza_agent).proxy(arrow_proxy_agent),
            Default::default(),
        )
        .trace_to_path(&trace_path_clone)
        .expect("Failed to create trace writer")
        .run(agent_client_protocol_core::ByteStreams::new(
            conductor_write.compat_write(),
            conductor_read.compat(),
        ))
        .await
    });

    // Run a simple prompt through the conductor
    let result = tokio::time::timeout(std::time::Duration::from_secs(30), async move {
        let result = agent_client_protocol_yopo::prompt(
            agent_client_protocol_core::ByteStreams::new(
                editor_write.compat_write(),
                editor_read.compat(),
            ),
            TestyCommand::Greet.to_prompt(),
        )
        .await?;

        Ok::<String, agent_client_protocol_core::Error>(result)
    })
    .await
    .expect("Test timed out")
    .expect("Editor failed");

    conductor_handle.abort();

    // Read and verify the trace file
    let trace_content = std::fs::read_to_string(&trace_path).expect("Failed to read trace file");

    // Parse each line as JSON
    let events: Vec<serde_json::Value> = trace_content
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| serde_json::from_str(line).expect("Invalid JSON in trace"))
        .collect();

    println!("Trace file: {}", trace_path.display());
    println!("Generated {} events", events.len());
    for (i, event) in events.iter().enumerate() {
        let event_type = event.get("type").and_then(|v| v.as_str()).unwrap_or("?");
        let from = event.get("from").and_then(|v| v.as_str()).unwrap_or("?");
        let to = event.get("to").and_then(|v| v.as_str()).unwrap_or("?");
        let method = event.get("method").and_then(|v| v.as_str()).unwrap_or("-");
        let protocol = event
            .get("protocol")
            .and_then(|v| v.as_str())
            .unwrap_or("acp");
        println!("  [{i}] {event_type} {from} -> {to} {method} ({protocol})");
    }

    // Verify we got some events
    assert!(!events.is_empty(), "Expected trace events, got none");

    // Verify we have requests and responses
    let has_request = events
        .iter()
        .any(|e| e.get("type").and_then(|v| v.as_str()) == Some("request"));
    let has_response = events
        .iter()
        .any(|e| e.get("type").and_then(|v| v.as_str()) == Some("response"));

    assert!(has_request, "Expected at least one request event");
    assert!(has_response, "Expected at least one response event");

    // Check that events have required fields
    for event in &events {
        assert!(
            event.get("ts").is_some(),
            "Event missing 'ts' field: {event:?}"
        );
        assert!(
            event.get("type").is_some(),
            "Event missing 'type' field: {event:?}"
        );
    }

    // Clean up
    let _ = std::fs::remove_file(&trace_path);

    println!("Test passed! Response: {result}");

    Ok(())
}
