# Behavior Reference

This document describes how `devloop` behaves at runtime beyond the
schema in [`configuration.md`](configuration.md).

This reference is also available in the CLI with:

```bash
devloop docs behavior
```

## Core model

`devloop` is moving toward a pure core with an imperative shell.

- workflow progression is modeled as explicit state plus effect
  requests
- startup orchestration, watch-triggered workflow scheduling, maintain
  ticks, shutdown, and process-supervision decisions are also modeled
  as explicit state plus effect requests
- the runtime interprets those effect requests to perform process
  control, hooks, sleeps, logging, and persistence
- workflow and runtime effect interpreters sit behind replaceable
  adapter boundaries, so orchestration can be tested against mocks
  without requiring live subprocesses or file watchers

The remaining imperative shell is now mostly the concrete adapter layer
that talks to Tokio, `notify`, child processes, HTTP probes, and the
filesystem.

## Startup

When `devloop run` starts, it:

1. loads and validates `devloop.toml`
2. resolves `root`, `state_file`, command paths, and relative working
   directories
3. loads the session state file into memory
4. if external events are configured, starts a localhost event server
5. if any workflow uses `notify_reload`, starts a localhost browser
   reload event server
6. starts any processes with `autostart = true`
7. runs each workflow named in `startup_workflows` in order
8. starts watching the configured `root`

The in-memory session state is authoritative for the running process.
Edits made directly to the JSON file while `devloop` is running are not
merged back into the live session.

## Watching and debounce

`devloop` watches the configured `root` recursively.

- Only relevant file-system events are considered.
- Events are batched for `debounce_ms`.
- Matching changes are grouped by workflow name before execution.
- Each workflow receives the set of changed relative paths that matched
  it during the debounce window.

If multiple watch groups map to the same workflow, their matched paths
are merged for that workflow run.

## Workflow execution

Workflows run step by step, in order.

- A step must finish successfully before the next one begins.
- `run_workflow` executes another named workflow inline.
- `triggers` run downstream workflows after the workflow succeeds.
- Triggered workflows are deduplicated across one execution tree. If two
  trigger paths reach the same workflow, it runs once from the first
  path that reaches it.
- Recursive workflow graphs are rejected at config-validation time.
- Config validation also rejects graphs where a direct trigger target is
  separately reachable through `run_workflow`, because that would make
  ordering and duplication ambiguous.
- `write_state` renders `{{state_key}}` templates against the current
  in-memory session state.
- `log` also renders templates against the current session state before
  emitting output.
- `notify_reload` broadcasts a generic `reload` event to browser
  listeners connected to `devloop`'s browser reload event stream.

If any step fails, that workflow fails immediately and logs the error
loudly, but `devloop` itself keeps running so later file changes or
external events can retry the workflow without restarting the
supervisor.

## Processes

Managed processes are long-running child commands.

- `start_process` is a no-op if the named process is already running.
- `restart_process` stops the child, then starts it again.
- `wait_for_process` waits on the configured readiness probe, not just
  on successful spawning.
- `restart = "always"` restarts a child after any exit unless
  `devloop` is shutting down.
- `restart = "on_failure"` restarts only after unsuccessful exit.
- `restart = "never"` never restarts automatically.
- Managed child processes inherit the ambient environment unless the
  process config explicitly overrides individual variables such as
  `env.RUST_LOG`.

Liveness probes are checked on the configured interval while the process
is running. If a liveness probe fails and the restart policy allows it,
the process is restarted.

## Hooks

Hooks are one-shot commands executed inside workflows.

- Hooks run to completion before the workflow continues.
- Hook stdout and stderr are captured fully, then rendered with a source
  label if `hook.<name>.output.inherit` is enabled.
- Hook output defaults to `body_style = "dim"` so helper-command output
  is visible but visually secondary.
- Hook capture is independent of hook output rendering.
- `capture = "text"` trims stdout and stores it in `state_key`.
- `capture = "json"` parses stdout as a JSON object and merges it into
  the session state.
- A non-zero hook exit status fails the workflow after any captured
  stdout and stderr have been rendered.

Hooks can also be observed outside workflows.

- If `hook.<name>.observe` is configured, the runtime polls that hook on
  the configured interval during normal maintenance ticks.
- If running the hook changes session state, the configured observe
  workflow is scheduled immediately.
- If the hook leaves session state unchanged, no follow-up workflow is
  run.

Observed hooks remain useful as a cheap fallback when push integration
is not worth the extra control surface. For lower-latency and less
noisy event flows, prefer external events instead.

## External events

If `event.*` config is present, `devloop` starts a localhost HTTP server
for constrained event ingestion.

- Each configured event maps to one fixed session-state key and one
  fixed workflow.
- Child processes receive the event URLs and bearer token in their
  environment.
- Posting the same value again does not rerun the workflow.
- Posting a new accepted value updates session state first, then
  schedules the configured workflow immediately.
- Invalid tokens are rejected.
- Values that fail the configured regex pattern are rejected.

## Browser reload events

If any workflow uses `notify_reload`, `devloop` starts a localhost SSE
server for browser listeners.

- Child processes and hooks receive `DEVLOOP_BROWSER_EVENTS_URL` in
  their environment.
- `notify_reload` broadcasts a single `reload` message to all connected
  listeners.
- In phase 1, client repositories still need a tiny dev-only listener
  script that subscribes to the SSE stream and calls
  `window.location.reload()` when asked.

## Output rendering

`devloop` uses a line-oriented, pipe-based output model.

- Terminal-native subprocess UIs are a non-goal.
- Child stdout is forwarded to `devloop` stdout.
- Child stderr is forwarded to `devloop` stderr.
- `devloop` engine and process logs are emitted through `tracing`.
- Managed-process and hook output is source-labeled as
  `[executable process-name]`.
- Internal `devloop` and dependency logs are grouped under
  `[devloop ...]` labels so the emitting supervisor remains visible
  first.
- When output color is enabled, labels are colorized per source.

### Color rules

Colorized output is enabled when stdout is a terminal and `NO_COLOR` is
not set.

- `body_style = "plain"` preserves subprocess body text as-is.
- `body_style = "dim"` dims both the inherited source label and body
  text.
- When a subprocess emits ANSI SGR color sequences while `body_style =
  "dim"`, `devloop` reapplies dim after each SGR sequence so the
  original tint is preserved as much as the terminal allows.
- Source-label colors intentionally avoid bright white because it is too
  visually aggressive in mixed logs.

### Carriage returns and line boundaries

`devloop` prefers visibility over terminal redraw semantics.

- `\r` is treated as a visible line boundary.
- `\r\n` does not double-print.
- Output is buffered by line before each write so prefixes do not split
  mid-line.
- UTF-8 multibyte sequences are buffered before decoding so characters
  such as `μ` survive inherited output rendering.

This is meant for readable supervised logs, not PTY emulation.

## Output-derived state

Long-running processes can write values into session state by matching
their inherited output against configured rules.

- Rules run on the raw output stream, line by line.
- Regex extraction uses the configured `capture_group`.
- `url_token` extracts the first token that looks like a
  `trycloudflare.com` URL.
- State keys configured in output rules are cleared before the process
  starts.

This is how a process such as `cloudflared` can publish a readiness
value without wrapper scripts.

## Readiness and liveness probes

HTTP probes succeed on an HTTP success status.

State-key probes succeed when the referenced session-state key exists
and is not empty after trimming.

These probe types are used both for workflow waiting and for ongoing
liveness checks.

## Session state

Session state is shared across the running engine, workflows, hooks, and
output-derived updates.

- `root` is written into session state when the engine starts.
- `last_workflow` and `last_changed_files` are updated for top-level
  workflow runs triggered by watches or startup execution.
- Nested `run_workflow` calls reuse the same session state without
  overwriting the top-level change context.
- Triggered workflows inherit that same top-level change context.

## Shutdown

On `ctrl-c`, `devloop`:

1. marks itself as shutting down
2. stops all managed processes
3. suppresses further automatic restarts
4. exits

## Known non-goals

- PTY emulation
- full-screen terminal UIs
- byte-exact reconstruction of combined stdout and stderr ordering after
  a child has already split output across file descriptors
