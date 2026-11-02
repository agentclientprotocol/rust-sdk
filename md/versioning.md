# Versioning and protocol versions

This page is for maintainers. It covers how the SDK crates are versioned and
what to change when the SDK adds an ACP protocol version.

## Release policy

- Every published crate is in the `acp` version group in `.release-plz.toml`,
  so all crates share one version number.
- The stable API, everything that is not behind an `unstable_*` feature,
  follows semver.
- Everything behind an `unstable_*` feature, including all of
  `agent_client_protocol::v2`, may change in a minor release. Stabilizing a
  protocol version only removes its feature gate.
- Moving a public dependency to a new major version is a major release of the
  whole group. This applies to `axum` and `reqwest`, whose types appear in
  `agent-client-protocol-http`'s API. A new `rmcp` major version is added as a
  new feature and module (`rmcp_4`) in a minor release. Only dropping support
  for an rmcp major version is a major release.
- `agent-client-protocol` pins `agent-client-protocol-schema` with an exact
  (`=`) requirement, and `agent-client-protocol-derive` with an exact
  requirement as well. Release the derive and core crates together.

The user-facing version of this policy is in the crate docs
(`agent_client_protocol#stability`) and in the
[v5 migration guide](./migration-v5.md#stability).

## Semver checks in CI

CI does not run a semver check yet, because there is no published 5.x release
to compare against. After 5.0.0 is on crates.io, add a job to
`.github/workflows/ci.yml` that compares each published crate with its latest
release. Pin the actions to commit SHAs like the other jobs do:

```yaml
  semver:
    name: Semver checks
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4
      - uses: obi1kenobi/cargo-semver-checks-action@v2
        with:
          # Unstable features may change in minor releases, so check the
          # stable API only.
          feature-group: default-features
          exclude: agent-client-protocol-test,agent-client-protocol-yopo
```

Run it on pull requests. A failure on a PR whose commits are not marked as
breaking (`!` or `BREAKING CHANGE:`) means the change needs a major release or
a different design.

## Adding a protocol version

Adding protocol v3 should be additive: the version-dispatching types are
`#[non_exhaustive]`, `Protocol` is sealed, and the router and connector slots
are private. Work through this list, using v2 as the template. Search for
`ProtocolVersion::V2` and `unstable_protocol_v2` to find each place.

1. **Schema.** Bump the `agent-client-protocol-schema` pin to a release with
   the new version's types. Implement the JSON-RPC traits for its messages
   in `src/agent-client-protocol/src/schema/` (see `v2_impls.rs`), including
   its proxy-chain types (`impl_proxy_jsonrpc!` in `proxy_protocol.rs`). The
   messages are re-exported as `v3::schema` in step 4, the only public path
   to them.
2. **Feature gate.** Add `unstable_protocol_v3` to
   `src/agent-client-protocol/Cargo.toml`, forwarding to the schema crate's
   feature, and to the crates that need a passthrough (the polyfill and the
   conductor). Without the feature, the SDK must behave exactly as before.
3. **Marker.** Add a `V3` marker next to `V1` and `V2` in
   `src/agent-client-protocol/src/jsonrpc/protocol.rs`. Implement the sealed
   trait: its `protocol_version`, its connection type, and its request ID
   conversions.
4. **Connection and module.** Add `jsonrpc/v3_connection.rs` and
   `src/agent-client-protocol/src/v3.rs`, re-exporting the shared names
   (`ConnectionTo`, `Builder`, the request handle aliases, and
   `pub use agent_client_protocol_schema::v3 as schema;`) and the
   version's session API. Add `Client.v3()`, `Agent.v3()` and `Proxy.v3()`.
5. **Negotiation.** Teach the wire guard in `jsonrpc/protocol_compat.rs` the
   new version: selecting it from `initialize` and `proxy/initialize`,
   `util::initialize_params_for_version`, and the new
   `SelectedInitializeResponse` variant for a peer that selects a lower
   version.
6. **Router slots.** Add a `V3` arm to `SelectedProtocol` and a `with_v3` slot
   to `AgentProtocolRouter`, `ProxyProtocolRouter` and
   `ClientProtocolConnector` in `src/agent-client-protocol/src/role/acp.rs`.
   Check every `match` on the protocol version there, including the Agent
   role's retry rule for unhandled session messages, which only applies to v1
   and unguarded connections.
7. **Conductor and polyfill.** Add a `SelectedInitializeRequest::V3` variant
   and its version selection in the conductor, and the new version's setup
   requests in the MCP-over-ACP polyfill.
8. **Tests.** Copy the v2 suites (`tests/protocol_v2.rs`, `tests/session_v2.rs`,
   `tests/proxy_protocol_router_v2.rs`, the conductor's `initialization_v2.rs`)
   for v3. Add negotiation tests between each pair of versions, and keep the
   feature-equivalence tests (`tests/v1_feature_equivalence.rs`) passing with
   and without the new feature.
9. **Docs.** Add a book page like [Protocol V2](./protocol-v2.md), update the
   crate docs and READMEs, and add the new feature to
   [Cargo Features](./features.md).
