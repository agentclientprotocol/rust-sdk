# Build binaries needed for integration tests
prep-tests:
    cargo build -p agent-client-protocol-conductor
    cargo build -p agent-client-protocol-test --bin testy
    cargo build -p agent-client-protocol-test --bin mcp-echo-server --example arrow_proxy

# Run all tests (requires prep-tests first)
test: prep-tests
    cargo test --all --workspace --all-features
