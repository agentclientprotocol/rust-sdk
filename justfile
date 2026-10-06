# Build ordinary binaries needed by subprocess tests and check every example.
# One workspace invocation avoids separate per-package preparation builds.
prep-tests:
    cargo build --workspace --bins --examples --all-features --locked

# Prepare subprocess fixtures, then run all tests, including doctests.
test: prep-tests
    cargo test --workspace --all-features --locked
