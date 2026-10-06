/// Direction of a line being sent or received by a native transport.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LineDirection {
    /// Line being sent to the agent (stdin).
    Stdin,
    /// Line being received from the agent (stdout).
    Stdout,
    /// Line being received from the agent (stderr).
    Stderr,
}
