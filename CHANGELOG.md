# Changelog

All notable changes to `devloop` will be recorded in this file.

## [Unreleased]

## [0.11.1] - 2026-09-02

### Fixed

- Made transactional artifact guidance discoverable from root help, the bare
  `devloop docs` index, and artifact validation errors.
- Made the runtime smoke test select its required informational log level
  instead of inheriting a caller setting that could hide its readiness signal.

## [0.11.0] - 2026-09-02

### Added

- Added transactional artifact generations through the single
  `publish_artifact` workflow action. Devloop now builds in isolated candidate
  directories, switches declared consumers only after build success, requires
  exact-generation HTTP readiness, rolls back failed switches, cleans crash
  remnants, and bounds retained generations.
- Added `devloop docs artifacts` with an agent-oriented configuration contract
  and complete lifecycle guidance.

## [0.10.5] - 2026-09-02

### Fixed

- Kept polling sessions alive when a watched file is deleted, while preserving
  delete-and-recreate events and leaving non-transient watcher errors fatal.

## [0.10.4] - 2026-09-01

### Fixed

- Made startup stop with a clear non-zero error when a managed process's local
  readiness address is already occupied, without restarting the failed process
  or disturbing the existing listener. Concurrent sessions remain supported
  when they use different ports.

## [0.10.3] - 2026-08-26

### Fixed

- Made `ctrl-c` shutdown tolerate redundant or overlapping watch targets, so
  native watcher teardown cannot skip managed-process cleanup or turn a normal
  exit into an error.

## [0.10.2] - 2026-08-26

### Fixed

- Invalidated process-output state before every start attempt and when the
  process stops or exits, preventing stale values such as tunnel URLs from
  surviving a missing executable or dead process.
- Made `wait_for_process` reject stopped and failed processes even when their
  readiness state persists from an earlier run.
- Logged complete workflow failure chains while explicitly continuing the
  runtime in degraded mode.

## [0.10.1] - 2026-08-26

### Changed

- Gave Linux and macOS CI distinct required-check names so `main`
  protection can require both platforms without an ambiguous status.

### Fixed

- Guard every managed process and hook with a pinned Rust companion and
  parent-death channel, so an abrupt `devloop` exit kills children,
  grandchildren, and deeper descendants that remain in the command's
  process group. The companion has a distinct process identity and
  stays consistent across in-place installation updates.

## [0.10.0] - 2026-07-23

### Added

- `devloop run` now persists a unique session log under the state-file
  directory's `logs/` subdirectory (by default `.devloop/logs/`), including
  engine, process, and hook output even when terminal inheritance is disabled.

## [0.9.3] - 2026-07-22

### Changed
- Clarified how to watch sibling directories by setting `root` to their
  common parent, and documented the `restart = "never"` default plus the
  wrapper exit-status behavior of `on_failure`.

### Fixed
- Made the cross-platform runtime smoke test use output-derived state
  readiness instead of a fixture-local HTTP server, removing its flaky
  loopback startup dependency while preserving workflow and watch coverage.

## [0.9.2] - 2026-07-01

### Fixed
- Managed processes are now started in their own Unix process groups
  and stopped as groups, so child processes spawned by a managed command
  do not survive `stop_process`, `restart_process`, or `ctrl-c`
  shutdown.
- HTTP readiness and liveness probe attempts are now request-bounded, so
  one stuck request cannot stall the probe loop or shutdown path.

## [0.9.1] - 2026-06-24

### Changed
- Updated the GitHub release publishing action to
  `softprops/action-gh-release@v3.0.0`, which uses the Node 24 action
  runtime.

### Fixed
- Literal trailing-slash directory watch patterns (for example
  `content/`) now match files nested inside the directory, so edits
  under a watched directory trigger its workflow. The directory was
  already watched recursively, but the change-to-workflow matcher used
  the bare pattern, which `globset` treats as a literal that never
  matches nested files, so no workflow ran.

## [0.9.0] - 2026-05-04

### Added
- Added shell-free parent-environment interpolation for process
  command arguments, process environment values, and HTTP probe URLs,
  so client configs can share values such as `CONTAINER_PORT` without
  repo-local wrapper scripts.

## [0.8.0] - 2026-04-08

### Added
- Added a configurable watcher backend with non-breaking `native`
  default behavior plus a `poll` fallback mode for environments where
  native filesystem notifications are unreliable.
- Added a Rust repeated-edit watch flake smoke test that can be run
  locally with `DEVLOOP_RUN_WATCH_FLAKE_SMOKE=1 cargo test --test
  watch_flake_smoke -- --nocapture`.
- Added explicit trailing-slash syntax for literal directory watch
  targets, for example `content/`, so recursive directory intent is
  preserved even when the directory does not yet exist at startup.
- Added a development guide under [`docs/development.md`](docs/development.md)
  and exposed it in the CLI as `devloop docs development`.

### Changed
- `devloop` now derives concrete watch targets from configured watch
  patterns and asks the backend to watch only those files or
  directories instead of always watching the whole repository root.
- The watch flake smoke test is now opt-in instead of running during
  every default `cargo test` or CI run. The existing runtime smoke test
  remains in CI.

### Fixed
- Native watch registration now resolves file and directory targets at
  runtime, so startup no longer depends on those paths already existing
  when config is parsed.
- Fixed a real watch flake where the debounce batch could be dropped if
  another `tokio::select!` branch won the race while filesystem events
  were already buffered.
- Test-only environment mutation now lives behind locked helpers with
  documented safety rationale instead of scattered raw unsafe blocks.

## [0.7.0] - 2026-03-27

### Added
- Added a browser reload event server and a `notify_reload` workflow
  action so workflows can explicitly tell downstream browser listeners
  to refresh after successful rebuild/restart steps.
- Added declarative workflow `triggers` so downstream orchestration can
  be expressed directly in config instead of being inferred through
  secondary file watches.

### Fixed
- Workflow failures such as process-readiness timeouts now log loudly
  but do not terminate `devloop`, so the watcher stays alive and the
  next successful edit can recover a broken local build.
- Triggered workflows now run as part of the same execution tree,
  including when their parent workflow was reached via `run_workflow`.
- Config validation now rejects ambiguous trigger graphs where a direct
  trigger target is also reachable through `run_workflow`, and
  triggered workflows are documented as single-run deduplicated within
  one execution.
- Trigger-overlap validation now walks the full execution tree, so
  nested trigger graphs cannot schedule the same workflow once as a
  trigger target and again through an inline `run_workflow` path.
- Fixed a false positive in that validator so inline workflows with
  independent trigger targets are allowed.
- Added regression coverage for both the allowed independent-trigger
  case and the rejected case where a parent workflow and an inline child
  share the same trigger target.
- Platform-specific release workflows no longer duplicate GitHub release notes when both assets are published to the same tag.

## [0.6.2] - 2026-03-26

### Fixed
- Runtime requests for missing workflows now fail explicitly instead of being logged and skipped, and external events return `503` if their workflow trigger cannot be dispatched.
- Watcher callback delivery failures are now surfaced as errors instead of being dropped silently.
- Unexpected watcher and external-event channel disconnects now fail the engine explicitly instead of silently disabling those input paths.
- Accepted macOS `notify` event paths reported under `/private/...` for watched roots configured under `/var/...`, so file changes in temp directories are no longer dropped by the watch classifier.
- Made the CI smoke test wait for file watching to start before editing the watched fixture file, and retry the watched write until the state change is observed, avoiding startup races on macOS runners.
- Added a hard wall-clock timeout and bounded shutdown to the CI smoke harness so failed runs die loudly instead of hanging in CI.

### Changed
- Split Linux and macOS CI into separate badgeable workflows backed by one reusable workflow definition, and limited release archives to the supported Linux x86_64 and macOS Apple Silicon targets.
- Split release publishing into separate Linux and macOS workflows backed by one reusable workflow definition so each platform publishes its asset independently.

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
