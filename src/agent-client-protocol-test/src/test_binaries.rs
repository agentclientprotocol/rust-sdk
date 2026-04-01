//! Utilities for locating pre-built test binaries.
//!
//! Integration tests that spawn subprocesses should use pre-built binaries
//! rather than `cargo run` to avoid recursive cargo invocations during
//! `cargo test --all`.
//!
//! Run `just prep-tests` before running tests to build all required binaries.

use std::path::{Path, PathBuf};

/// Returns the workspace root directory.
fn workspace_root() -> PathBuf {
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    manifest_dir
        .parent()
        .expect("agent-client-protocol-test should be in src/")
        .parent()
        .expect("src/ should be in workspace root")
        .to_path_buf()
}

/// Returns the path to a binary in the target/debug directory.
#[must_use]
pub fn debug_binary(name: &str) -> PathBuf {
    workspace_root().join("target/debug").join(name)
}

/// Returns the path to an example binary in the target/debug/examples directory.
#[must_use]
pub fn debug_example(name: &str) -> PathBuf {
    workspace_root().join("target/debug/examples").join(name)
}

/// Asserts that a binary exists, panicking with a helpful message if not.
///
/// # Panics
///
/// Panics if the binary does not exist, with a message instructing the user
/// to run `just prep-tests`.
pub fn require_binary(path: &Path) {
    assert!(
        path.exists(),
        "Binary not found at {}.\n\
         Run `just prep-tests` before running these tests.",
        path.display(),
    );
}

/// Returns the path to the agent-client-protocol-conductor binary, asserting it exists.
#[must_use]
pub fn conductor_binary() -> PathBuf {
    let path = debug_binary("agent-client-protocol-conductor");
    require_binary(&path);
    path
}

/// Returns the path to the test-agent binary, asserting it exists.
#[must_use]
pub fn testy_binary() -> PathBuf {
    let path = debug_binary("testy");
    require_binary(&path);
    path
}

/// Returns an AcpAgent configured for the test agent.
#[must_use]
pub fn testy() -> agent_client_protocol_tokio::AcpAgent {
    agent_client_protocol_tokio::AcpAgent::from_args([testy_binary().to_string_lossy().to_string()])
        .expect("failed to create test agent")
}

/// Returns the path to the mcp-echo-server binary, asserting it exists.
#[must_use]
pub fn mcp_echo_server_binary() -> PathBuf {
    let path = debug_binary("mcp-echo-server");
    require_binary(&path);
    path
}

/// Returns the path to the arrow_proxy example, asserting it exists.
#[must_use]
pub fn arrow_proxy_example() -> PathBuf {
    let path = debug_example("arrow_proxy");
    require_binary(&path);
    path
}
