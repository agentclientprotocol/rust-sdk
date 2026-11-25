# Native MCP-over-ACP

The native binding exposes **MCP 2026-07-28** over an existing ACP connection.
Enable `unstable_mcp_over_acp`; draft ACP v2 also requires
`unstable_protocol_v2`.

## Services and operations

Attach an `mcp_server::McpServer` through the existing global or session builder
APIs. Session setup advertises an opaque `serverId`. Each `mcp/message` request
addresses that server directly; there is no MCP connect/disconnect exchange,
initialization handshake, or discovery prerequisite.

`McpService` is a reusable application service. Its `execute` method receives a
`McpRequest` and a request-scoped `McpRequestContext`. The context exposes:

- The declared server ID and logical request ID.
- The session and tool call that the agent says the request was sent for,
  when it supplies them.
- Validated MCP metadata, including version and client capabilities.
- Caller cancellation and operation cancellation caused by removal or shutdown.
- An operation-scoped notification sender.
- The host ACP connection for application tools.

Share caches, tool catalogs, and database pools deliberately. Do not infer a
request's identity or capabilities from a previous call.

`session_id()` and `tool_call_id()` return the request's `sessionId` and
`toolCallId`. `McpConnectionTo` has the same accessors, so a `tool_fn` closure
can read them from its connection. An rmcp handler reads them from a
[`McpRequestAttribution`](#in-an-rmcp-handler). Both are `None` when the agent
sent none, for example for a request it sends during session setup. A child
session's work carries the child's ID. Even when `sessionId` names the active
session, `serverId` routes the request to the provider, not to the session's
update stream.

The SDK does not check these values. Before a tool uses the session to choose
what it acts on, check that the session is active on the connection, or is a
child the agent announced for an active session, and that this server was
declared to it or to one of its ancestors. Do not reject or delay a request
because you have not seen its tool call. The agent can report the call
afterwards. See the
[RFD](https://agentclientprotocol.com/rfds/mcp-over-acp#session-and-tool-call-attribution).

### Rejecting a request for its session

A server that needs the session of a request, or that refuses a session after
its own check, decides that before the request runs. Add an admission check
to the server with `McpServer::with_admission`. The check is an async closure
of the request. Like the SDK's other async callbacks, it is followed by a
macro as the last argument, here `mcp_admission!()`. The check can capture the
provider's state and `await`:

```rust,ignore
let server = McpServer::new_service("notes", NotesService::new(), NullRun)
    .with_admission(
        async move |request| {
            let session_id = request.require_session_id()?;
            if !sessions.is_active(session_id).await {
                return Err(McpRejection::session_rejected("the session is not active"));
            }
            Ok(())
        },
        agent_client_protocol::mcp_admission!(),
    );
```

A check that needs no `await` is the same closure without one:

```rust,ignore
.with_admission(
    async |request| {
        request.require_session_id()?;
        Ok(())
    },
    agent_client_protocol::mcp_admission!(),
)
```

The check receives an `McpAdmission`: the server ID, the request ID, the inner
`method()` and `params()`, the `tool_name()` of a `tools/call` request, and the
request's `session_id()` and `tool_call_id()`. `require_session_id()` returns
the session ID, or `McpRejection::session_required()` to return with `?`. It
only reads the request. A request is rejected when the check returns an
`McpRejection`.

- The check runs for every request to the server, such as `tools/call`,
  `tools/list`, and `server/discover`. It works the same for a service, a
  connector-based server, an `McpServerBuilder` server, and a `from_rmcp`
  server, whether the server is attached to one session or to every session of
  a proxy.
- It runs after the SDK has checked the inner request's own metadata, and
  before the service, connector, or tool is called. A rejected request reaches
  none of them.
- The agent receives the outer ACP error Invalid params (`-32602`). Its `data`
  string is `this MCP server requires sessionId on mcp/message` for
  `McpRejection::session_required()`, and `sessionId rejected: {reason}` for
  `McpRejection::session_rejected(reason)`.
- A request holds its request ID while its check is pending, so another
  request with that ID is answered as a duplicate. The ID is free again once
  the request is answered, also after a rejection.
- Cancellation by the agent, removal of the server's registration, and
  connection shutdown end a pending check as they end a running request: with
  Request cancelled (`-32800`), with `-33001`, and without a response. The SDK
  then drops the check's future, so a check must not own work that needs
  cleanup.
- Calling `with_admission` again adds another check. The SDK runs them one
  after the other in the order they were added. The first rejection answers
  the request, and the checks after it do not run.
- The checks of several requests can be pending at the same time, so a check
  cannot change the state it captures. Put state that changes behind a lock.

Any `Err` from `McpService::execute` is still a backend failure (`-33002`), and
any `Err` from a function tool is still a tool result with `isError: true`.
Neither can reject a request.

A server that rejects every request without a session also rejects the
requests an agent sends during session setup, and every request from an agent
that sends no attribution. The
[HTTP re-export](./mcp-bridge.md#lifecycle-and-failure-behavior) sends none.
Give the rule to the tools that need it instead:

```rust,ignore
.with_admission(
    async |request| {
        if request.tool_name() == Some("add_note") {
            request.require_session_id()?;
        }
        Ok(())
    },
    agent_client_protocol::mcp_admission!(),
)
```

To test a check without a connection, build its request with
`McpAdmission::new(server_id, request_id, method)`, and add the optional parts
with `with_params`, `with_session_id`, and `with_tool_call_id`:

```rust,ignore
let request = McpAdmission::new("server-1", "request-1", "tools/call");
assert_eq!(
    needs_session(request.clone()).await,
    Err(McpRejection::session_required())
);
assert_eq!(needs_session(request.with_session_id("sess_1")).await, Ok(()));
```

#### Rules of one tool

A tool that the SDK registers can state its rule next to its definition. The
server awaits it before a `tools/call` request that names the tool runs, after
the server's own checks. An `McpTool` implements `admit`:

```rust,ignore
impl<R: Role> McpTool<R> for AddNote {
    // ...

    async fn admit(&self, request: McpAdmission) -> Result<(), McpRejection> {
        request.require_session_id()?;
        Ok(())
    }
}
```

A `tool_fn` or `tool_fn_mut` tool has no `McpTool` impl of its own. Add its
rule by name on the rmcp `McpServerBuilder`. The `rmcp_3` module re-exports
`mcp_admission!` next to `tool_fn!`:

```rust,ignore
let server = McpServer::builder("notes")
    .tool_fn("add_note", "Adds a note", add_note, tool_fn!())
    .tool_admission(
        "add_note",
        async |request| {
            request.require_session_id()?;
            Ok(())
        },
        mcp_admission!(),
    )?
    .build();
```

A call of a tool that is not registered or is disabled gets no tool rule. The
server reports it as it does without admission checks. A hand-written
`McpService` states its own rule in `McpService::admit`, which also runs after
the server's checks.

The `#[tool]` methods of a `from_rmcp` handler are not registered with the
SDK. Their rules go in the server's check, by `tool_name()`, as shown above.

#### Standalone connections

A standalone MCP connection has no `mcp/message` request, so no admission
check runs for it, and tools see no session. A tool that is also served
standalone has to handle a call without a session.

#### In an rmcp handler

An rmcp `ServerHandler` passed to `from_rmcp` reads the attribution of a
request from a `McpRequestAttribution`, from the `rmcp_3` module of
`agent-client-protocol-rmcp`. A `#[tool]` method takes it as a parameter:

```rust,ignore
#[tool(description = "Lists the notes of the session")]
async fn list_notes(&self, attribution: McpRequestAttribution) -> CallToolResult {
    let session_id = attribution.session_id();
    // ...
}
```

A `ServerHandler` method you write yourself gets it with
`McpRequestAttribution::of`. That works in every method that receives a
`RequestContext`, such as `call_tool`, `list_tools`, and `discover`:

```rust,ignore
async fn list_tools(
    &self,
    _request: Option<PaginatedRequestParams>,
    context: RequestContext<RoleServer>,
) -> Result<ListToolsResult, ErrorData> {
    let attribution = McpRequestAttribution::of(&context);
    // ...
}
```

`McpRequestAttribution` has `session_id()` and `tool_call_id()`. It only reads
the request. To require a session, add an admission check to the server that
`from_rmcp` returns. On a standalone connection the attribution has no session
and no tool call.

Use `McpServer::new_service` for native-only execution, or
`new_service_with_standalone` to also serve a separate standalone transport.
The connector-based constructor remains an adapter for backends that need a
fresh component per operation.

The rmcp builder and `from_rmcp` use a shared native service and rmcp's
one-request transport. Standalone serving retains its ordinary MCP lifecycle.
Scoped `tool_fn` and `tool_fn_mut` closures remain supported; cancelling a call
must not poison the runner or start already-cancelled queued work.

## Calling a server

Use `MessageMcpRequest::new(server_id, request_id, method)`. Choose a fresh
logical ID for every operation and include these inner parameters:

```json
{
  "_meta": {
    "io.modelcontextprotocol/protocolVersion": "2026-07-28",
    "io.modelcontextprotocol/clientCapabilities": {}
  }
}
```

The logical ID stays unchanged through proxies and becomes the inner MCP
JSON-RPC ID. It is separate from the outer ACP ID, which can change on each hop.

When the request is for a session's work, name the session with
`.session_id(...)`, and add `.tool_call_id(...)` for the tool call you report in
`session/update`. Use the IDs you send on `session/update` for that work, so a
child session's request carries the child's ID. Omit a value you do not know,
and do not send a tool call without its session. A provider can reject a
request that has no session.

Register a `MessageMcpNotification` handler before starting an operation that
streams notifications. Route notifications by server and logical request ID.
Use spawned work or the application's connection future to wait for peer
traffic; do not block the ACP dispatcher.

## Results and errors

First handle the outer ACP result, then match `MessageMcpResponse`:

- `Result { result, .. }` contains an opaque MCP result, including JSON null,
  metadata, and MRTR's `input_required` result.
- `Error { error, .. }` contains an MCP error. Preserve its numeric code, data
  omission versus explicit null, and unknown extensions.
- An outer ACP error describes a binding failure, not an MCP method outcome.
  A provider that needs the session answers `-32602` for a request without a
  `sessionId`, or with one it refuses.

An inner MCP `-32000` is not ACP authentication-required. Tool execution
failures with `isError` remain MCP results. For MRTR, retry with a fresh logical
ID, current metadata, input responses, and the unchanged opaque request state.

ACP v1 and v2 have independent carrier/error types. Their wire representations
currently agree; use the types for the negotiated ACP version.

## Cancellation and cleanup

A subscription is one long-running request, not shared connection state.
Acknowledgments and updates carry the logical request ID in
`_meta["io.modelcontextprotocol/subscriptionId"]`.

ACP cancellation is best effort on the wire. The Rust SDK revokes an owned
operation's notification rights, cancels its work, and joins supported cleanup.
The logical ID remains active until cleanup finishes; completion may win a
cancellation race.

Mutable function tools serialize user execution, not cancellation of queued work.
A cancelled queued call is destroyed without entering its closure or waiting for
another registration's active call. Recoverable session-local runner failures close
only that session's registrations; they do not seal connection-wide admission.

Custom services must observe `operation_cancellation()` and return only after
owned cleanup finishes. This also applies when their registration is removed,
the ACP input closes, or the application foreground completes. The SDK keeps
native supervisors and scoped runners driven during that cleanup. It cannot
roll back arbitrary detached application work.

## Example and scope

```sh
cargo run -p agent-client-protocol-rmcp \
  --example stateless_native_mcp \
  --features rmcp_3,unstable_mcp_over_acp
```

See the [wire reference](./protocol.md#native-mcp-over-acp),
[HTTP re-export](./mcp-bridge.md), and
[migration guide](./migration-stateless-mcp.md).

This change does not redesign generic ACP queues, frame types, task admission,
or byte accounting. Native MCP protocol state is request-scoped; that is not a
claim of bounded total process memory or complete optional MCP conformance.
