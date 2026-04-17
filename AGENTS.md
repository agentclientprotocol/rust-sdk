This repository is the Rust SDK for the Agent Client Protocol (ACP).

The mdbook Table of Contents in `md/SUMMARY.md` contains all relevant documentation. You should keep it up to date as you work.

The crates are found in `src/*`. Each crate's `README.md` and `CHANGELOG.md` describe its purpose and history; the `md/introduction.md` chapter gives a map of how the crates fit together.

## Crate Layout

- `agent-client-protocol` – Core protocol SDK (roles, connections, handlers, schema)
- `agent-client-protocol-tokio` – Tokio utilities (process spawning, stdio transports)
- `agent-client-protocol-rmcp` – Integration with the `rmcp` crate
- `agent-client-protocol-cookbook` – Usage patterns rendered as rustdoc
- `agent-client-protocol-derive` – Proc macros
- `agent-client-protocol-conductor` – Conductor binary and library for proxy chains
- `agent-client-protocol-test` – Shared test utilities and fixtures
- `agent-client-protocol-trace-viewer` – Interactive sequence diagram viewer
- `yopo` – "You Only Prompt Once" example client

## Working With the Book

- Source lives under `md/` and is configured by `book.toml`.
- The `mdbook-mermaid` preprocessor is required for diagrams; install both tools with `cargo install mdbook mdbook-mermaid`.
- Before the first local build, run `mdbook-mermaid install .` from the repo root to drop `mermaid.min.js` and `mermaid-init.js` alongside `book.toml` (both files are git-ignored).
- Build the book with `mdbook build`; preview with `mdbook serve`.
- Publishing is automated by `.github/workflows/mdbook.yml`, which installs the mermaid assets and deploys to GitHub Pages on pushes to `main`.

## Conventional Commits

This repository uses **Conventional Commits** for automated releases via release-plz. All commit messages should follow this format:

```
<type>[optional scope]: <description>

[optional body]

[optional footer(s)]
```

### Commit Types

- **feat:** A new feature (triggers minor version bump)
- **fix:** A bug fix (triggers patch version bump)
- **docs:** Documentation only changes
- **style:** Code style changes (formatting, missing semicolons, etc.)
- **refactor:** Code changes that neither fix bugs nor add features
- **perf:** Performance improvements
- **test:** Adding or updating tests
- **chore:** Maintenance tasks, dependency updates, etc.
- **ci:** CI/CD configuration changes
- **build:** Build system or external dependency changes

### Breaking Changes

Add `!` after the type to indicate breaking changes (triggers major version bump):

```
feat!: change API to use async traits
```

Or include `BREAKING CHANGE:` in the footer:

```
feat: redesign conductor protocol

BREAKING CHANGE: conductor now requires explicit capability registration
```

### Examples

```
feat(conductor): add support for dynamic proxy chains
fix(agent-client-protocol): resolve deadlock in message routing
docs: update README with installation instructions
chore: bump tokio to 1.40
```

### Scope Guidelines

Common scopes for this repository (typically the crate name without the `agent-client-protocol-` prefix where unambiguous):

- `acp` – Core protocol changes
- `conductor` – Conductor-specific changes
- `tokio` – Tokio utility changes
- `rmcp` – rmcp integration changes
- `cookbook` – Cookbook patterns and docs
- `trace-viewer` – Trace viewer changes
- `deps` – Dependency updates

**Agents should help ensure commit messages follow this format.**
