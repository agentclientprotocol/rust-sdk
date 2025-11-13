/// Initialize tracing subscriber for tests to output logs to stderr.
///
/// Call this at the start of any test that needs detailed logging for debugging.
/// Logs will be printed to stderr and only shown when the test fails or when
/// running with `--nocapture`.
pub fn init_test_tracing() {
    use tracing_subscriber::{EnvFilter, fmt};

    let _ = fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| EnvFilter::new("trace,sacp::jsonrpc=trace")),
        )
        .with_test_writer()
        .try_init();
}
