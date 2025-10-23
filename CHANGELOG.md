# Changelog

## 0.6.0 (2025-10-23)

- Provide missing `_meta` fields on certain enum variants.
- More consistent enum usage. Enums are always either newtype or struct variants within a single enum, not mixed.

## 0.5.0 (2025-10-20)

- Export necessary RPC types. Fixes an issue where certain fields weren't public enough.
- Make id types easier to create and add `PartialEq` and `Eq` impls for as many types as possible.
- Export `acp::Result<T, E = acp::Error>` for easier indication of ACP errors.
- Use `acp::Error`/`acp::Result` instead of `anyhow::Error`/`anyhow::Result` for all return types.

## 0.4.7 (2025-10-13)

- Depend on `agent-client-protocol-schema` for schema types

## 0.4.6 (2025-10-10)

### Rust

- Fix: support all valid JSON-RPC ids (int, string, null)

## 0.4.5 (2025-10-02)

- No changes

## 0.4.4 (2025-09-30)

- Provide default trait implementations for optional capability-based `Agent` and `Client` methods.

## 0.4.3 (2025-09-25)

- impl `Agent` and `Client` for `Rc<T>` and `Arc<T>` where `T` implements either trait.

## 0.4.2 (2025-09-22)

**Unstable** fix missing method for model selection in Rust library.

## 0.4.1 (2025-09-22)

**Unstable** initial support for model selection.

## 0.4.0 (2025-09-17)

- Make `Agent` and `Client` dyn compatible (you'll need to annotate them with `#[async_trait]`) [#97](https://github.com/agentclientprotocol/agent-client-protocol/pull/97)
- `ext_method` and `ext_notification` methods are now more consistent with the other trait methods [#95](https://github.com/agentclientprotocol/agent-client-protocol/pull/95)
  - There are also distinct types for `ExtRequest`, `ExtResponse`, and `ExtNotification`
- Rexport `serde_json::RawValue` for easier use [#95](https://github.com/agentclientprotocol/agent-client-protocol/pull/95)
