# Configurable LLM Providers

The core SDK exposes the draft configurable-provider API through the
`unstable_llm_providers` feature:

```toml
agent-client-protocol = { version = "...", features = ["unstable_llm_providers"] }
```

Draft protocol v2 applications must enable both `unstable_protocol_v2` and
`unstable_llm_providers`.

An agent advertises support with `AgentCapabilities.providers`. After
initialization, clients can use three typed client-to-agent requests:

- `providers/list` discovers configurable providers, supported API protocols,
  whether each provider is required, and its current non-secret routing target.
- `providers/set` replaces one provider's API protocol, base URL, and complete
  header map.
- `providers/disable` disables a non-required provider.

Clients should configure providers before creating or loading sessions. Provider
configuration is process-scoped and should not be persisted. Disabling an
unknown provider is idempotent, while attempts to disable a required provider
must be rejected.

The SDK supplies typed wire routing only. Applications remain responsible for
capability checks, validating provider IDs and API protocols, enforcing required
providers, applying configuration to sessions, and storing header values
securely. Since headers may contain credentials, `providers/list` intentionally
returns only `apiType` and `baseUrl` and must never echo configured headers.

> **Sensitive logging:** SDK debug and trace instrumentation can include complete
> JSON-RPC bodies. Treat those logs as sensitive and do not enable body-level
> logging where provider headers may contain credentials.
