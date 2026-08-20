# Changelog

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

- Expand the `testy` binary into a deterministic ACP test agent that can exercise stable v1 agent methods, notifications, session updates, and client callbacks.
- Add default `testy` coverage for elicitation form/URL requests, session/request scopes, response actions, completion notifications, capability-negotiation prompt errors, and a direct `elicitations` prompt trigger.
- Add an opt-in native protocol v2 Testy agent and a dual-version stdio router covering the baseline session lifecycle, independent prompt acceptance, cancellation completion, and resume replay.
