# Configuration Reference

`devloop` is configured with a TOML file, typically `devloop.toml` in
the client repository root.

## Top-level keys

```toml
root = "."
debounce_ms = 300
state_file = "./.devloop/state.json"
startup_workflows = ["startup"]
```

- `root`: repository root used for watches, relative commands, and path
  resolution.
- `debounce_ms`: file-watch debounce window in milliseconds.
- `state_file`: path to the session state JSON file. If omitted,
  `devloop` uses `<root>/.devloop/state.json`.
- `startup_workflows`: workflows to run after autostart processes have
  been started.

## Watch groups

Watch groups map file patterns to workflows.

```toml
[watch.rust]
paths = ["src/**/*.rs", "Cargo.toml"]
workflow = "rust"
```

- Table name: the watch-group name.
- `paths`: glob patterns evaluated relative to `root`.
- `workflow`: workflow to run when a matching file changes. If omitted,
  the watch-group name is used as the workflow name.

## Processes

Processes are long-running commands supervised by `devloop`.

```toml
[process.server]
command = ["cargo", "run"]
cwd = "."
autostart = false
restart = "always"
env = { PORT = "8080" }
output = { inherit = true, body_style = "plain" }
```

### Process keys

- `command`: command and arguments as a string array. Required.
- `cwd`: working directory for the process. Relative paths are resolved
  from `root`.
- `autostart`: whether to start the process before startup workflows.
- `restart`: one of `never`, `on_failure`, or `always`.
- `env`: extra environment variables for the process.
- `readiness`: optional readiness probe.
- `liveness`: optional liveness probe.
- `output`: inherited-output behavior and output-derived state rules.

### Output config

```toml
output = {
  inherit = true,
  body_style = "plain",
  rules = [{ state_key = "tunnel_url", extract = "url_token" }]
}
```

- `inherit`: whether process output should be forwarded by `devloop`.
  Default: `true`.
- `body_style`: visual treatment for inherited process body text.
  Default: `plain`.
- `rules`: output-derived state capture rules.

### Output body styles

- `plain`: preserve the process body text as-is, including native ANSI
  colors when present.
- `dim`: dim non-control body text so `devloop` engine logs stand out
  more strongly.

Use `plain` when subprocess color or exact body rendering matters. Use
`dim` when you want inherited process output to recede visually.

### Output rules

Each rule extracts a value from process output and writes it into the
session state.

```toml
output = { rules = [{ state_key = "tunnel_url", extract = "url_token" }] }
```

Rule keys:

- `state_key`: destination state key. Required.
- `pattern`: regex used when `extract = "regex"`.
- `extract`: one of `regex` or `url_token`.
- `capture_group`: capture-group index for regex extraction.

## Probes

### HTTP probe

```toml
[process.server.readiness]
kind = "http"
url = "http://127.0.0.1:8080/"
interval_ms = 500
timeout_ms = 30000
```

### State-key probe

```toml
[process.tunnel.readiness]
kind = "state_key"
key = "tunnel_url"
interval_ms = 250
timeout_ms = 30000
```

## Hooks

Hooks are narrow one-shot commands invoked from workflows.

```toml
[hook.current_post_slug]
command = ["./scripts/current-post-slug.sh"]
cwd = "."
capture = "text"
state_key = "current_post_slug"
```

### Hook keys

- `command`: command and arguments. Required.
- `cwd`: working directory.
- `env`: extra environment variables.
- `capture`: one of `ignore`, `text`, or `json`.
- `state_key`: required for `capture = "text"`.

### Hook capture modes

- `ignore`: discard stdout.
- `text`: write trimmed stdout into `state_key`.
- `json`: parse stdout as a JSON object and merge it into session state.

## Workflows

Workflows are ordered steps.

```toml
[workflow.startup]
steps = [
  { action = "start_process", process = "server" },
  { action = "wait_for_process", process = "server" },
  { action = "run_hook", hook = "build_css" },
]
```

### Workflow actions

- `start_process`
- `stop_process`
- `restart_process`
- `wait_for_process`
- `run_hook`
- `run_workflow`
- `sleep_ms`
- `write_state`
- `log`

### `write_state`

```toml
{ action = "write_state", key = "current_post_url", value = "{{tunnel_url}}/posts/{{current_post_slug}}" }
```

`value` supports `{{state_key}}` interpolation from the current session
state.

### `log`

```toml
{ action = "log", message = "current post url: {{current_post_url}}", style = "boxed" }
```

- `message`: rendered with session-state interpolation.
- `style`: `plain` or `boxed`.

## Session state

The state file is owned by the running `devloop` session.

Typical uses:

- hook outputs such as `current_post_slug`
- output-derived values such as `tunnel_url`
- workflow-composed values such as `current_post_url`

## Minimal example

```toml
root = "."
debounce_ms = 300
startup_workflows = ["startup"]

[watch.rust]
paths = ["src/**/*.rs"]
workflow = "rust"

[process.server]
command = ["cargo", "run"]
autostart = false
restart = "always"
output = { inherit = true, body_style = "plain" }

[process.server.readiness]
kind = "http"
url = "http://127.0.0.1:8080/"

[workflow.startup]
steps = [
  { action = "start_process", process = "server" },
  { action = "wait_for_process", process = "server" },
]

[workflow.rust]
steps = [
  { action = "restart_process", process = "server" },
  { action = "wait_for_process", process = "server" },
]
```
