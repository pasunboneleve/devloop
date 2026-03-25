# Behavior Reference

This document describes how `devloop` behaves at runtime beyond the
schema in [`configuration.md`](configuration.md).

## Startup

When `devloop run` starts, it:

1. loads and validates `devloop.toml`
2. resolves `root`, `state_file`, command paths, and relative working
   directories
3. loads the session state file into memory
4. starts any processes with `autostart = true`
5. runs each workflow named in `startup_workflows` in order
6. starts watching the configured `root`

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
- Recursive workflow graphs are rejected at config-validation time.
- `write_state` renders `{{state_key}}` templates against the current
  in-memory session state.
- `log` also renders templates against the current session state before
  emitting output.

If any step fails, the workflow fails immediately and `devloop run`
returns an error.

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
- Managed child processes default to `RUST_LOG=info` unless the process
  config explicitly sets `env.RUST_LOG`.

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

## Output rendering

`devloop` uses a line-oriented, pipe-based output model.

- Terminal-native subprocess UIs are a non-goal.
- Child stdout is forwarded to `devloop` stdout.
- Child stderr is forwarded to `devloop` stderr.
- `devloop` engine and process logs are emitted through `tracing`.
- Managed-process and hook output is source-labeled with the configured
  name and executable.
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
