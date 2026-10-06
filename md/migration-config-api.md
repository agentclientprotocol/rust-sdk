# Migrating the Config API

The upcoming 3.x release makes `agent-client-protocol-http::ServerOptions`,
`agent-client-protocol-http::CorsOptions`, and
`agent-client-protocol::AcpAgentConfig` non-exhaustive so future configuration
fields or policies can be added without another breaking release.

## HTTP server options

**Breaking:** downstream crates cannot construct `ServerOptions` with a struct
literal, including `..ServerOptions::default()` update syntax.

Before (no longer compiles outside the HTTP crate):

```rust,ignore
let options = ServerOptions {
    path: "/agent".into(),
    cors: CorsOptions::allow_origins(["https://example.com"])?,
    health_endpoint: false,
};
// Partial literals with ..ServerOptions::default() also need migration.
```

After:

```rust
use agent_client_protocol_http::{CorsOptions, ServerOptions};

let options = ServerOptions::default()
    .with_path("/agent")
    .with_cors(CorsOptions::allow_origins(["https://example.com"])?)
    .with_health_endpoint(false);
```

All existing fields remain public, so reading or assigning `options.path`,
`options.cors`, or `options.health_endpoint` still works. Destructuring patterns
must include `..`.

Defaults and runtime policies are unchanged: `/acp`, disabled cross-origin
browser access, and an enabled `/health` endpoint. Setters replace the chosen
value and preserve the other options.

## CORS policies

Existing variants and constructors remain available. **Breaking:** exhaustive
matches on `CorsOptions` outside the HTTP crate must add a wildcard arm:

```rust
use agent_client_protocol_http::CorsOptions;

fn is_disabled(policy: &CorsOptions) -> bool {
    match policy {
        CorsOptions::Disabled => true,
        _ => false,
    }
}
```

CORS does not authenticate requests. Authorization headers and credentialed
CORS are host-owned policies; see [browser authentication and credential
policy](./http-transport.md#browser-authentication-and-credential-policy).

## Agent subprocess configuration

`AcpAgentConfig` already has private fields. Keep using its existing constructor
and fluent methods; no new builder type or `Default` is required because an
executable must be specified:

```rust
use agent_client_protocol::AcpAgentConfig;

let config = AcpAgentConfig::new("python")
    .arg("agent.py")
    .args(["--mode", "fast"])
    .env("RUST_LOG", "info")
    .envs([("NO_COLOR", "1")]);
```

Its getters, serialization, required `command`, empty argument/environment
defaults, and rejection of unknown JSON fields are unchanged.
