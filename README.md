# devloop

[![CI](https://github.com/pasunboneleve/devloop/actions/workflows/ci.yml/badge.svg)](https://github.com/pasunboneleve/devloop/actions/workflows/ci.yml)

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
output, inherited process lines include the configured process name and
executable automatically. The label is color-coded per process, and the
body style is configurable:

```toml
[process.tunnel]
command = ["cloudflared", "tunnel", "--url", "http://127.0.0.1:18080"]
output = { inherit = true, body_style = "plain", rules = [{ state_key = "tunnel_url", extract = "url_token" }] }
```

That renders inherited lines with the process name and executable, for
example `[tunnel cloudflared] ...`, using ANSI color when stdout is a
terminal and `NO_COLOR` is not set.

For the full configuration reference, see
[`docs/configuration.md`](docs/configuration.md).

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
