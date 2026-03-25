# Changelog

All notable changes to `devloop` will be recorded in this file.

## [Unreleased]

### Added
- Source-labeled managed process output so mixed logs show which
  configured process and executable emitted each line.
- Stable per-process label colors and dimmed managed-process bodies so
  `devloop` workflow and engine logs stand out by contrast.

### Fixed
- Preserved UTF-8 multibyte characters in inherited subprocess output
  so watch tools render units such as `μs` correctly.
- Reapplied dim styling after child ANSI SGR sequences when
  `output.body_style = "dim"` so colored subprocess logs can still
  recede visually without losing their tint entirely.

## [0.3.0] - 2026-03-25

### Added
- Configurable inherited process body styling via `output.body_style`,
  allowing developers to choose between preserving native subprocess
  colors and dimming inherited output bodies.
- Detailed configuration reference docs under [`docs/`](docs/README.md).

## [0.2.3] - 2026-03-25

### Changed
- Routed inherited child stdout and stderr to matching sinks instead of
  collapsing them into a single output stream.
- Stopped dimming inherited process output bodies so native subprocess
  colors survive more cleanly.

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
