# agent-client-protocol-trace-viewer

A local web viewer for conductor protocol traces, rendered as interactive
ACP/MCP sequence diagrams.

```bash
cargo install agent-client-protocol-trace-viewer
agent-client-protocol-trace-viewer ./trace.jsons
```

The viewer binds to loopback and opens a browser. Use `--port PORT` to choose a
port or `--no-open` to suppress browser launch. File-backed traces are reread
as they grow, so a recording can be viewed during capture.

Capture a trace with the conductor:

```bash
agent-client-protocol-conductor --trace ./trace.jsons agent "base-agent"
```

The library also provides `serve_file`, `serve_memory`, and a `TraceHandle` for
embedding the viewer and adding in-memory events. Trace capture belongs to the
conductor; the viewer does not record traffic itself.

Trace files contain protocol payloads and should be treated as sensitive.

- [API reference](https://docs.rs/agent-client-protocol-trace-viewer)
- [Capture, event format, and viewer guide](https://agentclientprotocol.github.io/rust-sdk/trace-viewer.html)

## License

Apache-2.0. The published package includes `LICENSE`.
