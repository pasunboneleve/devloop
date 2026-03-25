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
- Keep the engine small. If a behavior can live in repo-local config,
  avoid baking it into the core.
- Do not push unless explicitly asked. This repository may not even have a
  remote during early development.
- Run quality gates for code changes: `cargo fmt`, `cargo test`,
  `cargo clippy --all-targets --all-features -- -D warnings`.

## Architecture constraints
- The engine owns orchestration: file watching, process supervision,
  health checks, event routing, and ordered workflow execution.
- Client repositories own context: watched path groups, named processes,
  workflows, and hook commands.
- Terminal-native subprocess UIs are a non-goal. `devloop` is optimized
  for line-oriented supervised output, source labeling, and output/state
  extraction through pipes rather than PTY emulation or full-screen
  terminal behavior.
- Hooks should be narrow and data-oriented. Prefer stdout or JSON output
  over large shell scripts that orchestrate nested workflows.
- Dynamic state that changes during a session, such as a tunnel URL,
  should have a stable interface such as a state file rather than a
  startup-only environment variable.

## Documentation expectations
- Keep `README.md` focused on current goals and how to run the tool.
- Keep `PLAN.md` aligned with the next reviewable milestones.
- Record follow-up work in `bd`, not as free-form TODO comments.

## Session completion
1. File issues for unfinished work or risks discovered during
   implementation.
2. Run quality gates if code changed.
3. Update issue status in `bd`.
4. Summarize the current state, verification, and next steps.
