# Migrating Proxy Method Names

The proxy protocol now uses the method names defined by the
[ACP proxy chains RFD](https://agentclientprotocol.com/rfds/proxy-chains).
Earlier SDK releases used `_`-prefixed names, which ACP reserves for vendor
extensions.

| Old method | New method |
| --- | --- |
| `_proxy/initialize` | `proxy/initialize` |
| `_proxy/successor` | `proxy/successor` |

The message shapes are unchanged. `proxy/successor` still flattens the inner
ACP message into its params and sends optional metadata as `_meta` (`meta` is
also accepted on receive).

## Upgrade conductors and proxies together

This is a wire-breaking rename with no compatibility fallback: the SDK no
longer sends or accepts the old `_proxy/*` names. A conductor built with this
release cannot drive a proxy built with an earlier release, and vice versa.
Upgrade the conductor and every proxy in a chain at the same time, including
nested conductors running in proxy mode.

Code that uses the SDK types (`InitializeProxyRequest`, `SuccessorMessage`,
`METHOD_INITIALIZE_PROXY`, `METHOD_SUCCESSOR_MESSAGE`, or the `Proxy` and
`Conductor` roles) needs no source changes. Update any hard-coded method
strings, such as in raw JSON-RPC handlers, test fixtures, or trace filters.

## `ConductorHostRole` is sealed

`agent-client-protocol-conductor::ConductorHostRole` can no longer be
implemented outside the conductor crate. Its methods take a `ConductorRunner`,
whose fields are private, so no external implementation could be written
before either. Use the provided `Agent` and `Proxy` host roles.
