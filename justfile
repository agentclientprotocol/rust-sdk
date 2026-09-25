# Keep file-based snapshots inside this checkout, even in nested worktrees.
export CARGO_WORKSPACE_DIR := justfile_directory()

# Build binaries needed for integration tests
prep-tests:
    cargo build -p agent-client-protocol-conductor --all-features
    cargo build -p agent-client-protocol-test --bin testy --all-features
    cargo build -p agent-client-protocol-test --bin mcp-echo-server --example arrow_proxy --all-features

# Run all tests, or pass a test-name filter / cargo test arguments.
test *args: prep-tests
    cargo test --all --workspace --all-features {{args}}
