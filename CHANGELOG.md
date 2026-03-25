# Changelog

All notable changes to `devloop` will be recorded in this file.

## [Unreleased]

### Added
- Source-labeled managed process output so mixed logs show which
  configured process and executable emitted each line.
- Stable per-process label colors and dimmed managed-process bodies so
  `devloop` workflow and engine logs stand out by contrast.

## [0.2.2] - 2026-03-25

### Fixed
- Preserved ANSI color escape sequences from inherited subprocess output
  so native colored logs such as Rust server tracing output render
  correctly under `devloop`.

## [0.2.1] - 2026-03-25

### Fixed
- Restored inherited process output for processes that omit an explicit
  `output` block by defaulting `output.inherit` to `true` at the
  `ProcessSpec` level as intended.

## [0.2.0] - 2026-03-24

### Added
- Config-driven process supervision with startup workflows, readiness checks,
  liveness checks, and restart policies.
- Output-derived session state capture for long-running processes such as
  `cloudflared`.
- Generic `write_state` interpolation for composing derived values from session
  state.
- Reusable `run_workflow` steps with validation against missing nested
  workflows and recursive workflow graphs.
- Generic blog example config under [`examples/blog/devloop.toml`].
- Human-readable CLI help text for the top-level command and subcommands.

### Changed
- Moved the real working blog config out of `devloop` and into the client
  repository.
- Resolved repo-local hook commands relative to the client repository root.
- Reworked session state ownership to be in-memory and shared across the
  running engine.
- Avoided redundant state-file writes and released the in-memory state lock
  before file I/O.

## [0.1.0] - 2026-03-24

### Added
- Initial `devloop` bootstrap with config loading, file watching, process
  management, and workflow execution.
