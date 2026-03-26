# Changelog

All notable changes to `devloop` will be recorded in this file.

## [Unreleased]

### Fixed
- Runtime requests for missing workflows now fail explicitly instead of
  being logged and skipped, and external events return `503` if their
  workflow trigger cannot be dispatched.
- Watcher callback delivery failures are now surfaced as errors instead
  of being dropped silently.
- Unexpected watcher and external-event channel disconnects now fail the
  engine explicitly instead of silently disabling those input paths.
- Accepted macOS `notify` event paths reported under `/private/...`
  for watched roots configured under `/var/...`, so file changes in
  temp directories are no longer dropped by the watch classifier.
- Made the CI smoke test wait for file watching to start before editing
  the watched fixture file, and retry the watched write until the state
  change is observed, avoiding startup races on macOS runners.
- Added a hard wall-clock timeout and bounded shutdown to the CI smoke
  harness so failed runs die loudly instead of hanging in CI.

### Changed
- Split Linux and macOS CI into separate badgeable workflows backed by
  one reusable workflow definition, and limited release archives to the
  supported Linux x86_64 and macOS Apple Silicon targets.

## [0.6.1] - 2026-03-26

### Changed
- Render `devloop docs <topic>` output as terminal-friendly text instead of printing literal Markdown.

## [0.6.0] - 2026-03-26

### Added
- Added observed hooks, allowing a hook to be polled on the runtime
  maintenance tick and trigger a workflow only when its session-state
  output changes.
- Added localhost external events with per-run bearer tokens, fixed
  event-to-state/workflow mappings, and child-process environment
  injection so trusted local clients can push state changes into
  `devloop` without polling.
- Added dedicated security documentation for external events and the
  push-versus-polling tradeoffs in [`docs/security.md`](docs/security.md).
- Added `devloop docs <topic>` so the configuration, behavior, and
  security references can be read directly from the CLI without
  duplicating the source material.
- Added a tag-driven GitHub release workflow that verifies the Cargo
  version, builds release archives for Linux and macOS, and publishes
  them as GitHub Release assets.

### Changed
- Moved workflow progression into a pure state/effect core so ordered
  workflow execution is planned through explicit transition data before
  the runtime interprets the requested side effects.
- Moved startup orchestration, watch-triggered workflow scheduling,
  maintain ticks, shutdown handling, and process-supervision decisions
  into pure runtime/process cores with explicit effect planning.
- Added replaceable adapter boundaries for workflow and runtime effect
  interpretation so orchestration can be tested against mocks instead of
  live subprocesses and file watchers.
- Added direct tests for the concrete log-prefix rendering path and
  mock-based tests for workflow/runtime effect interpreters so output
  coloring and orchestration changes can be validated without manual
  runs.

### Fixed
- Removed bright white from inherited output label colors and dimmed
  source labels alongside dimmed inherited process bodies.
- Restored managed child-process environment inheritance so `devloop`
  and supervised processes read the same ambient `RUST_LOG` unless repo
  config explicitly overrides it.
- Prefixed internal dependency logs under `devloop`, for example
  `[devloop hyper_util ...]`, and reordered managed-process labels to
  `[executable process-name]` so the emitting process is visible first.

## [0.4.0] - 2026-03-25

### Added
- Source-labeled managed process output so mixed logs show which
  configured process and executable emitted each line.
- Stable per-process label colors and dimmed managed-process bodies so
  `devloop` workflow and engine logs stand out by contrast.
- Source-labeled hook stdout and stderr with dimmed bodies by default so
  short-lived helper commands remain visible without dominating the main
  process logs.
- Detailed runtime behavior reference under [`docs/behavior.md`](docs/behavior.md).

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
