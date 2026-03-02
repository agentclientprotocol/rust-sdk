# yopo

**YOPO** (You Only Prompt Once) - A simple ACP client for one-shot prompts.

## What's in this crate?

YOPO is a minimal ACP client that sends a single prompt to an agent and prints the response. It auto-approves all permission requests, making it useful for quick testing and scripting.

## Usage

```bash
# With a simple command
yopo "What is 2+2?" "python my_agent.py"

# With a JSON agent configuration
yopo "Hello!" '{"type":"stdio","name":"my-agent","command":"python","args":["agent.py"],"env":[]}'
```

## How it works

1. Parses the agent configuration (command string or JSON)
2. Spawns the agent process via `sacp-tokio`
3. Initializes the ACP connection
4. Creates a new session
5. Sends the prompt
6. Prints all `AgentMessageChunk` text to stdout
7. Auto-approves any permission requests from the agent
8. Exits when the agent completes

## When to use this crate

Use `yopo` when you need to:
- Quickly test an agent from the command line
- Script agent interactions in shell pipelines
- Verify that an agent responds correctly to a prompt

For interactive or multi-turn sessions, use a full ACP client like Zed or a JetBrains IDE.

## Related Crates

- **[sacp](../sacp/)** - Core ACP SDK (use this for building agents)
- **[sacp-tokio](../sacp-tokio/)** - Tokio-specific utilities for spawning agents

## License

MIT OR Apache-2.0
