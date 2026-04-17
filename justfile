# Build binaries needed for integration tests
prep-tests:
    cargo build -p agent-client-protocol-conductor --all-features
    cargo build -p agent-client-protocol-test --bin testy --all-features
    cargo build -p agent-client-protocol-test --bin mcp-echo-server --example arrow_proxy --all-features

# Run all tests (requires prep-tests first)
test: prep-tests
    cargo test --all --workspace --all-features
