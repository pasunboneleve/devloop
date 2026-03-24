# devloop

`devloop` is a standalone local development supervisor.

It is meant to sit outside an application repository and run that
repository through configuration and hooks. The goal is to make local
developer workflows configurable, ordered, and easy to adapt as the
project changes.

## Install

Install the latest published `main` branch directly from GitHub:

```bash
cargo install --git https://github.com/pasunboneleve/devloop.git
```

For local development from a checkout:

```bash
cargo install --path .
```

## Releases

`devloop` is still early-stage. For now, the most reliable release
signal is the tagged commit history and the passing CI workflow on
GitHub.

## Why this exists

Common local setups start simple and then drift into a tangle of shell
wrappers:

- one process watches Rust code
- another watches CSS
- another starts a tunnel
- some repo-specific script prints a shareable URL

That works until the workflow needs ordering, dynamic state, or a new
directory layout. At that point the scripts become bespoke
orchestration.

`devloop` is an attempt to keep the orchestration generic while allowing
the client repository to keep its own context.

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

## MVP scope

The initial MVP should support:

- loading a config file from a client repo
- named watch groups over path globs
- named long-running processes
- ordered workflows per watch group
- ordered startup workflows
- hook commands
- session state persisted to a local file
- a first working client against the `gcp-rust-blog-public` repo

## First client

The generic example config lives at:

`examples/blog/devloop.toml`

It models a blog workflow as configuration:

- `rust` changes restart the server, wait for health, rebuild CSS, then
  restart the tunnel and publish the current post URL
- `content` changes restart the tunnel and republish the current post URL
- `css` changes trigger a one-shot Tailwind build

The example expects repo-owned helper scripts:

- `./scripts/build-css.sh`
- `./scripts/current-post-slug.sh`

At the same time, the tunnel itself is described as a managed
process, not a wrapper script:

- `cloudflared` is started directly by the engine
- stdout is scanned with regex rules
- the matched tunnel URL is written into session state
- readiness waits for the state key to be populated
- restart policy keeps the process alive if it exits

The client config can then compose derived values with `write_state`
steps, for example:

```toml
{ action = "write_state", key = "current_post_url", value = "{{tunnel_url}}/posts/{{current_post_slug}}" }
```

Workflows can also emit rendered log lines:

```toml
{ action = "log", message = "current post url: {{current_post_url}}" }
```

For high-visibility output in a mixed process log, use the boxed style:

```toml
{ action = "log", message = "current post url: {{current_post_url}}", style = "boxed" }
```

Repeated setup can be factored into helper workflows and reused with
`run_workflow`, for example a `publish_post_url` workflow that waits for
the tunnel and then writes the derived URL.

## Known gap

Real working configs should live in the client repository, not under
`devloop/examples/`. The example here is intentionally generic.

## Development

Quality gates:

```bash
cargo fmt
cargo test
cargo clippy --all-targets --all-features -- -D warnings
```

Task tracking:

```bash
bd ready
bd show <issue>
bd update <issue> --status in_progress
bd close <issue>
```
