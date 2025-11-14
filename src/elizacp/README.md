# elizacp

A classic Eliza chatbot implemented as an ACP (Agent-Client Protocol) agent.

## Overview

Elizacp provides a simple, predictable agent implementation that's useful for:

- **Testing ACP clients** - Lightweight agent with deterministic pattern-based responses
- **Protocol development** - Verify ACP implementations without heavy AI infrastructure
- **Learning ACP** - Clean example of implementing the Agent-Client Protocol

## Features

- **Classic Eliza patterns** - Pattern matching and reflection-based responses
- **Full ACP support** - Session management, initialization, and prompt handling
- **Per-session state** - Each session maintains its own Eliza instance
- **Extensible patterns** - Easy to add new response patterns (including future tool use triggers)

## Usage

### Running the agent

```bash
# Build and run
cargo run -p elizacp

# With debug logging
cargo run -p elizacp -- --debug
```

The agent communicates over stdin/stdout using JSON-RPC, following the ACP specification.

### Testing with an ACP client

Elizacp responds to:

1. **Initialize requests** - Returns capabilities
2. **New/Load session requests** - Creates session state
3. **Prompt requests** - Responds with Eliza-style conversational replies

Example conversation:
```
User: Hello
Eliza: Hello. How are you feeling today?

User: I am sad
Eliza: Do you often feel sad?

User: I feel worried about my father
Eliza: Tell me more about your family.
```

## Implementation

- `eliza.rs` - Pattern matching engine with classic Eliza responses
- `main.rs` - ACP agent implementation over stdio

## Architecture

The agent maintains a `HashMap<SessionId, Eliza>` to track per-session state. Each session gets its own Eliza instance with independent conversation state.

## Future Extensions

The pattern database structure is designed to support:
- Tool use triggers (e.g., "what's the weather" → tool call)
- Custom response patterns
- Conversation history tracking
- Multi-turn context awareness
