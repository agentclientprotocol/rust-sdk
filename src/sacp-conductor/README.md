# sacp-conductor

Binary for orchestrating ACP proxy chains.

## What is the conductor?

The conductor is a tool that manages proxy chains - it spawns proxy components and the base agent, then routes messages between them. From the editor's perspective, the conductor appears as a single ACP agent.

```
Editor ← stdio → Conductor → Proxy 1 → Proxy 2 → Agent
```

## Usage

### Agent Mode

Orchestrate a chain of proxies in front of an agent:

```bash
# Chain format: proxy1 proxy2 ... agent
sacp-conductor agent "python proxy1.py" "python proxy2.py" "python base-agent.py"
```

The conductor:
1. Spawns each component as a subprocess
2. Connects them in a chain
3. Presents as a single agent on stdin/stdout
4. Manages the lifecycle of all processes

### MCP Bridge Mode

Connect stdio to a TCP-based MCP server:

```bash
# Bridge stdio to MCP server on localhost:8080
sacp-conductor mcp 8080
```

This allows stdio-based tools to communicate with TCP MCP servers.

## How It Works

**Component Communication:**
- Editor talks to conductor via stdio
- Conductor uses `_proxy/successor/*` protocol extensions to route messages
- Each proxy can intercept, transform, or forward messages
- Final agent receives standard ACP messages

**Process Management:**
- All components are spawned as child processes
- When conductor exits, all children are terminated
- Errors in any component bring down the entire chain

## Example Use Case

Add Sparkle embodiment + custom tools to any agent:

```bash
sacp-conductor agent \
  "sparkle-acp-proxy" \
  "my-custom-tools-proxy" \
  "claude-agent"
```

This creates a stack where:
1. Sparkle proxy injects MCP servers and prepends embodiment
2. Custom tools proxy adds domain-specific functionality  
3. Base agent handles the actual AI responses

## Building

```bash
cargo build --release -p sacp-conductor
```

Binary will be at `target/release/sacp-conductor`.

## Related Crates

- **[sacp-proxy](../sacp-proxy/)** - Framework for building proxy components
- **[sacp](../sacp/)** - Core ACP SDK
- **[sacp-tokio](../sacp-tokio/)** - Tokio utilities for process spawning

## License

MIT OR Apache-2.0
