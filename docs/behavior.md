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

Before starting a managed process with a loopback HTTP readiness probe,
`devloop` checks whether it can bind that process-owned readiness address. An
occupied address, including one that is bound but not yet accepting
connections, stops startup with a non-zero error that names the process and
address. The colliding session does not enter its restart loop or disturb the
existing listener. This check is per configured address: concurrent sessions
and worktrees remain independent when they use different ports.

The in-memory session state is authoritative for the running process.
Edits made directly to the JSON file while `devloop` is running are not
merged back into the live session.

## Watching and debounce

`devloop` derives concrete filesystem watch targets from the configured
watch-group patterns and watches only those files or directories.

- Only relevant file-system events are considered.
- Events are batched for `debounce_ms`.
- Matching changes are grouped by workflow name before execution.
- Each workflow receives the set of changed relative paths that matched
  it during the debounce window.
- The default backend uses native filesystem notifications. A polling
  backend can be selected in config as a fallback for environments
  where native events are unreliable.
- Literal file targets are watched as narrowly as the backend allows.
  The polling backend scans each file's immediate parent so deleting and
  recreating the file remains observable. A configured file, or a child beneath
  a recursive target, that disappears during a scan is transient. Pathless and
  registered-root errors remain fatal. Errors confined to unconfigured siblings
  do not widen the watch group's failure boundary.
  Use a trailing `/` in the config when you mean an explicit directory
  target that should be watched recursively.

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
- `publish_artifact` is one atomic workflow effect. The engine does not expose
  partial preparation or promotion steps.

If any step fails, that workflow fails immediately and logs the error
loudly, but `devloop` itself keeps running so later file changes or
external events can retry the workflow without restarting the
supervisor.

## Artifact publication

Artifact publication isolates destructive builds from live consumers. A build
failure deletes only its private candidate. A successful build becomes a named
generation, then devloop changes the active session state and restarts the
declared consumers. HTTP readiness succeeds only when its response body equals
the active generation. A mismatch or consumer failure restores the previous
state and process generation before the workflow reports failure.

Interrupted candidates are removed at the next publication. Successful
generations are retained newest-first according to the artifact's `retain`
limit. Cleanup failure after a ready switch is logged without failing the
already committed publication. Browser reload belongs in a triggered workflow, so clients are notified
only after exact-generation readiness succeeds.

See [Transactional Artifact Generations](artifacts.md) for the agent-facing
configuration and environment contract.

## Processes

Managed processes are long-running child commands.

- `start_process` is a no-op if the named process is already running.
- `restart_process` stops the child, then starts it again.
- Every external command, including managed processes and hooks, is
  launched in its own Unix process group through an internal Rust
  companion process. At run startup, `devloop` opens and retains the
  exact companion image, so an in-place installation update cannot
  change the guardian protocol for later hooks or restarts. The guardian
  remains outside the target group, ignores terminal-oriented signals,
  and watches a private lifetime channel owned by `devloop`. Managed
  targets restore ordinary signal handling before they start. Normal
  stop/restart/shutdown terminates the target group, and abrupt
  `devloop` disappearance closes the channel so the guardian kills the
  group and reaps its direct target.
  Children, grandchildren, and deeper descendants are covered while
  they remain in the inherited process group.
- A descendant that deliberately creates a new session or process group
  escapes portable Unix process-group containment. Such commands must
  provide their own shutdown integration instead of daemonizing beneath
  `devloop`.
- `wait_for_process` requires the named process to be running while it
  waits on the configured readiness probe. Persisted readiness state from
  an earlier process instance cannot make a stopped or failed process ready.
- State keys populated by a process's output rules belong to that process
  instance. `devloop` clears them before every start attempt and when the
  process stops or exits, so a failed dependency cannot leave a stale URL or
  other process-derived value available to later workflows.
- `restart = "always"` restarts a child after any exit unless
  `devloop` is shutting down. A child that exits while a startup workflow is
  waiting for its first readiness result is treated as a failed start and is
  not restarted before that startup failure terminates the session. Later
  runtime workflows preserve the configured restart policy.
- `restart = "on_failure"` restarts only after unsuccessful exit.
- `restart = "never"` never restarts automatically.
- Restart policies use the managed command's exit status. A wrapper that
  exits successfully after its long-running child dies is not restarted by
  `on_failure`; the service remains down. For development-server wrappers,
  use `restart = "always"` or propagate the child's exit status. A liveness
  probe can detect a dead child only while its wrapper remains running; a
  readiness probe only checks startup and does not supervise ongoing health.
- Managed child processes inherit the ambient environment unless the
  process config explicitly overrides individual variables such as
  `env.RUST_LOG`.
- Before a process is spawned, `devloop` expands `$NAME` and `${NAME}`
  references in process command arguments and configured process
  environment values from its own parent environment. Use `$$` for a
  literal dollar sign.
- HTTP readiness and liveness probe URLs use the same expansion when the
  probe is checked.
- Missing or malformed environment references fail loudly with the
  field name so the configuration error is visible.
- Workflow failures include their complete causal error chain and leave the
  runtime watching in degraded mode when it is safe to continue.

Liveness probes are checked on the configured interval while the process
is running. If a liveness probe fails and the restart policy allows it,
the process is restarted.

## Hooks

Hooks are one-shot commands executed inside workflows.

- Hooks run to completion before the workflow continues.
- Hooks use the same guarded process-group lifecycle as managed
  processes, including cleanup after abrupt `devloop` termination.
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

### Session logs

Every `devloop run` also persists a per-session log under the `logs/`
directory beside its state file. With the default state file, that is
`.devloop/logs/`. `devloop` reports the selected path at startup.

The persistent log contains `tracing` output and labeled managed-process and
hook output. It records that child output even when `output.inherit` hides it
from the terminal. `devloop` creates the directory and log before starting the
runtime; a creation failure stops startup rather than running without durable
evidence.

Devloop ignores the active session-log file when classifying watch events, so
broad patterns such as `**/*` do not retrigger workflows on that log write.
Other files in the same `logs/` directory remain normal watched files.

`devloop` does not rotate or delete session logs. The client owns retention;
add `.devloop/` to the client repository's `.gitignore` when using the default
state-file location.

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
2. stops watching without requiring configured watch targets to be
   disjoint or unique
3. stops all managed processes
4. suppresses further automatic restarts
5. exits successfully

Overlapping recursive and literal watch patterns are valid. A redundant
watch registration or an already-removed backend watch cannot interrupt
process cleanup during shutdown.

## Known non-goals

- PTY emulation
- full-screen terminal UIs
- byte-exact reconstruction of combined stdout and stderr ordering after
  a child has already split output across file descriptors
