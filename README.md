# devloop

[![Linux CI](https://github.com/pasunboneleve/devloop/actions/workflows/linux-ci.yml/badge.svg)](https://github.com/pasunboneleve/devloop/actions/workflows/linux-ci.yml)
[![macOS CI](https://github.com/pasunboneleve/devloop/actions/workflows/macos-ci.yml/badge.svg)](https://github.com/pasunboneleve/devloop/actions/workflows/macos-ci.yml)

**Keep the local loop cheap.**

`devloop` is a config-driven tool for running multi-process systems
locally.

Most local setups become expensive to change:

- restarting everything
- losing state
- waiting for rebuilds
- coordinating multiple services

`devloop` keeps everything alive so you can change one thing at a
time.

<br>

<p align="center" style="margin: 0.35rem 0 0.35rem 0;">
  <a href="https://patents.google.com/patent/US1948860A"
  target="_blank"
  rel="noopener noreferrer">
    <img
        src="docs/images/us1948860a-page1-drawing-mid-yellow-card.png"
        alt="Ball bearing patent drawing"
        style="width:58.5%;"
        />
  </a>
</p>

<p align="center" style="margin: 0 0 1.25rem 0;">
    <sub>Motion is constrained. Parts keep moving.</sub>
</p>

## A concrete example

Working on a blog post with a public preview:

- change Rust -> the server rebuilds and restarts
- the browser reconnects and reloads
- `cloudflared` restarts -> new public URL
- the current page path is combined with the new tunnel URL
- the final URL is printed, ready to paste into LinkedIn validator

One change -> everything updates -> copy and paste once.

Without `devloop`:

- restarting the server does not automatically coordinate the rest of the loop
- you still need to manage CSS rebuilds separately
- the tunnel keeps the old URL unless you restart `cloudflared` yourself
- you rebuild the full public URL by hand every time

The pieces exist.\
They just don’t know about each other.

## Install

Install the latest published `main` branch directly from GitHub:

```bash
cargo install --git https://github.com/pasunboneleve/devloop.git
```

Tagged releases are also published automatically on GitHub with
prebuilt release archives for Linux x86_64 and macOS Apple Silicon.
Each supported platform publishes its release asset independently, so a
failure on one platform does not block the other asset from being
attached to the GitHub release.

Supported prebuilt release targets:

- `x86_64-unknown-linux-gnu`
- `aarch64-apple-darwin`

For local development from a checkout:

```bash
cargo install --path .
```

## Usage

Run `devloop` in a repository with a `devloop.toml` config:

```bash
devloop run
```
The tool will:
* start declared processes
* watch configured paths
* execute workflows on change

Built-in reference docs are also available from the CLI:

```bash
devloop docs config
devloop docs behavior
devloop docs development
devloop docs security
```

## Design

The tool has three layers:

1. Engine
   Watches files, supervises processes, executes workflows, and stores
   session state.

2. Repository config
   Declares watch groups, named processes, workflow steps, and hook
   commands.

3. Repository hooks
   Small commands that answer project-specific questions such as "what is
   the current post slug?" or "what public URL should be printed now?"

Internally, `devloop` is being refactored toward a pure core plus an
imperative shell: workflow orchestration is planned as explicit
state/effect transitions, runtime scheduling and process-supervision
decisions are planned the same way, and replaceable adapters interpret
those effects at the edges.

The session state file is owned by `devloop` while it is running.
External edits to that file are not merged back into the live session;
restart the supervisor if you need to seed a different initial state.

## Example use case

Used as the primary local development workflow for
[`gcp-rust-blog-public`](https://github.com/pasunboneleve/gcp-rust-blog-public).

The generic example config lives at:

[`examples/blog/devloop.toml`](examples/blog/devloop.toml)

The real client config lives in the client repository itself:

[`gcp-rust-blog-public/devloop.toml`](https://github.com/pasunboneleve/gcp-rust-blog-public/blob/main/devloop.toml)

It models a blog workflow as configuration:

* `rust` changes restart the server, wait for health, refresh the
  current post slug, restart the tunnel, and publish the current post URL
* `content` changes refresh the current post slug, restart the tunnel,
  and republish the current post URL
* CSS is handled by a long-running Tailwind watch process started by the
  startup workflow

The example expects repo-owned helper scripts:

* `./scripts/build-css.sh`
* `./scripts/current-post-slug.sh`

At the same time, the tunnel itself is described as a managed process:

* `cloudflared` is started directly by the engine
* stdout is scanned with regex rules
* the matched tunnel URL is written into session state
* readiness waits for the state key to be populated
* restart policy keeps the process alive if it exits
* inherited process output is source-labeled without wrapper scripts

When you need to identify which managed process emitted a line in mixed
output, inherited process lines include the executable first and the
configured process name second. The label is color-coded per process,
and the body style is configurable:

```toml
[process.tunnel]
command = ["cloudflared", "tunnel", "--url", "http://127.0.0.1:18080"]
output = { inherit = true, body_style = "plain", rules = [{ state_key = "tunnel_url", extract = "url_token" }] }
```

That renders inherited lines with the executable and process name, for
example `[cloudflared tunnel] ...`, using ANSI color when stdout is a
terminal and `NO_COLOR` is not set.

For the runtime behavior reference, see
[`docs/behavior.md`](docs/behavior.md).

For the full configuration reference, see
[`docs/configuration.md`](docs/configuration.md).

For local contributor workflow details, including the opt-in watch
flake smoke test, see [`docs/development.md`](docs/development.md).

For the external-event trust model and push-versus-polling tradeoffs,
see [`docs/security.md`](docs/security.md).

The client config can then compose derived values with `write_state`
steps, for example:

```toml
step = { action = "write_state", key = "current_post_url", value = "{{tunnel_url}}/posts/{{current_post_slug}}"}
```

Workflows can also emit rendered log lines:

```toml
step = { action = "log", message = "current post url: {{current_post_url}}"}
```

For high-visibility output in a mixed process log, use the boxed style:

```toml
step = { action = "log", message = "current post url: {{current_post_url}}", style = "boxed"}
```

Repeated setup can be factored into helper workflows and reused with
`run_workflow`, for example a `publish_post_url` workflow that waits for
the tunnel and then writes the derived URL.

Downstream orchestration should usually be declared with workflow
`triggers`, so users can read directly what a successful workflow
causes next:

```toml
[workflow.css]
steps = [
  { action = "run_hook", hook = "build_css" },
]
triggers = ["browser_reload"]

[workflow.browser_reload]
steps = [
  { action = "notify_reload" },
]
```

Workflows can also trigger a generic browser refresh after successful
rebuild/restart steps:

```toml
step = { action = "notify_reload" }
```

If any workflow uses `notify_reload`, `devloop` starts a localhost SSE
endpoint and exposes its URL to child processes as
`DEVLOOP_BROWSER_EVENTS_URL` so client repositories can attach a tiny
browser-side `EventSource` listener.

Triggered workflows are deduplicated within one execution. If two
trigger paths both reach the same workflow, `devloop` runs it once from
the first path that reaches it. Config validation also rejects graphs
where a direct trigger target is separately reachable through
`run_workflow`, because that would make ordering ambiguous.

Hooks can also be observed on the runtime tick when external state
changes are not represented by file edits. For example:

```toml
[hook.current_post_slug]
command = ["./scripts/current-post-slug.sh"]
capture = "text"
state_key = "current_post_slug"
observe = { workflow = "publish_post_url", interval_ms = 1000 }
```

That lets a helper hook refresh session state from something like a
development server endpoint, and rerun the follow-up workflow only when
the state actually changes.

For more precise local event flows, `devloop` can also accept
capability-scoped pushed events over a localhost HTTP server. A trusted
client process can post a value to a configured event, `devloop`
updates the mapped session-state key, and then runs the mapped workflow
if the value changed. This is the preferred model for things like
browser-path updates, while observed hooks remain a simpler fallback.

---

## Known gap

Real working configs should live in the client repository, not under
`devloop/examples/`. The example here is intentionally generic.

---

## Development

Quality gates:

```bash
cargo fmt
cargo test
cargo clippy --all-targets --all-features -- -D warnings
```

Git hook setup:

```bash
git config core.hooksPath .githooks
```

That enables the versioned pre-commit hook in [`.githooks/pre-commit`](.githooks/pre-commit),
which runs `cargo fmt` before each commit.

Task tracking:

```bash
bd ready
bd show <issue>
bd update <issue> --status in_progress
bd close <issue>
```
