# Protocol Versioning

The SDK normally exposes the stable ACP v1 schema types through `agent_client_protocol::schema::*`.

For experiments against the draft ACP v2 schema, enable the `unstable_protocol_v2` feature on
`agent-client-protocol`. With that feature enabled, `schema::*` resolves to the schema crate's
`v2` types by default, while the connection layer still speaks either v1 or v2 on the wire.

## Negotiation

Protocol version negotiation is driven by the normal `initialize` request and response:

- Before any `initialize` message is observed, non-initialize ACP messages are treated as v2 when the v2 feature is enabled.
- While an `initialize` request is in flight, the requested `protocolVersion` is used provisionally for wire conversion.
- The `initialize` request is encoded according to the requested `protocolVersion`.
- The `initialize` response records the negotiated wire version on the connection.
- Later known ACP requests, responses, and notifications are downgraded to v1 or left as v2 based on that negotiated version.

The conversion is internal to the SDK. Agent and client handlers continue to use the feature-selected
`schema::*` types. With `unstable_protocol_v2` enabled, that means user code handles v2 types even
when the remote side negotiated v1.

## Scope

The adapter converts known ACP payloads at the untyped JSON-RPC boundary using the schema crate's v2
conversion module. Custom JSON-RPC methods and extension methods are passed through unchanged.

The v2 feature is intentionally separate from the existing `unstable` umbrella because it changes the
SDK's default Rust type namespace. It should be enabled explicitly by experiments that are ready to
compile against the draft v2 types.
