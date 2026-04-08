# AGENTS.md

## Purpose
`devloop` is a standalone developer-experience tool. It should provide a
configuration-driven engine for local watch/rebuild/reload workflows
without hard-coding knowledge of any one repository.

## Working rules
- Use `bd` for task tracking. Create or update issues before substantial
  edits.
- Prefer stable abstractions over repo-specific shortcuts. Put
  project-specific behavior behind config or hooks.
- Prefer small, focused libraries over bespoke implementations when
  they solve one problem cleanly and reduce local complexity. Avoid
  large dependencies that bring in broad architectural weight or make
  the code harder to understand.
- Keep the engine small. If a behavior can live in repo-local config,
  avoid baking it into the core.
- Do not push unless explicitly asked. This repository may not even have a
  remote during early development.
- Do not use `sleep` to resolve races. Races must be resolved with
  deterministic logic, explicit readiness signals, or ordered state
  transitions.
- Run quality gates for code changes: `cargo fmt`, `cargo test`,
  `cargo clippy --all-targets --all-features -- -D warnings`.

## Architecture constraints
- Prefer a pure core with an imperative shell. Runtime behavior should
  be modeled as explicit state transitions plus effect requests, with
  side effects interpreted at the edges.
- Thread real runtime state through explicit data structures rather than
  hiding behavior inside ambient side effects.
- New orchestration logic should default to pure functions that can be
  unit-tested directly. Process control, file watching, timers,
  logging, and persistence should sit behind replaceable effect
  adapters or interpreters.
- Prefer explicit pushed events over polling when external state changes
  need to drive workflows precisely. Polling is acceptable as a simpler
  fallback when integration cost or security surface must stay lower.
- The engine owns orchestration: file watching, process supervision,
  health checks, event routing, and ordered workflow execution.
- Client repositories own context: watched path groups, named processes,
  workflows, and hook commands.
- Terminal-native subprocess UIs are a non-goal. `devloop` is optimized
  for line-oriented supervised output, source labeling, and output/state
  extraction through pipes rather than PTY emulation or full-screen
  terminal behavior.
- Silent failure is unacceptable. Every user-visible path should have a
  corresponding sad path with explicit, observable failure behavior:
  non-zero exits, surfaced errors, timeouts, logs, or other signals
  that make the failure diagnosable instead of disappearing quietly.
- Prefer noisy, non-fatal runtime failures when supervision can
  continue safely. A transient readiness timeout, failed rebuild, or
  broken watched edit should complain loudly in logs without taking down
  the whole `devloop` engine unless the supervisor itself can no longer
  make progress.
- When forwarding inherited process output, prefer visible output over
  suppressing noise. Carriage-return-driven updates should be rendered
  as visible labeled lines; if a tool is too noisy, fix that at the
  tool or script layer rather than allowing it to appear silent in
  `devloop`.
- Hooks should be narrow and data-oriented. Prefer stdout or JSON output
  over large shell scripts that orchestrate nested workflows.
- Dynamic state that changes during a session, such as a tunnel URL,
  should have a stable interface such as a state file rather than a
  startup-only environment variable.
- External control surfaces must be capability-scoped. Prefer localhost
  listeners with per-run tokens and fixed config-declared event-to-state
  mappings over generic endpoints that can trigger arbitrary workflows
  or state writes.

## Documentation expectations
- Keep `README.md` focused on current goals and how to run the tool.
- Record security-sensitive design constraints in dedicated docs under
  `docs/`, not just in code comments or commit messages.
- Keep `PLAN.md` aligned with the next reviewable milestones.
- User-visible functionality changes and behavior changes must update
  `CHANGELOG.md` as part of the same change before commit.
- `devloop` uses semantic versioning. Update versions intentionally to
  match the scope of delivered changes.
- Record follow-up work in `bd`, not as free-form TODO comments.

## Session completion
1. File issues for unfinished work or risks discovered during
   implementation.
2. Run quality gates if code changed.
3. Update issue status in `bd`.
4. Summarize the current state, verification, and next steps.
