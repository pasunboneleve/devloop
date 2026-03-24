# devloop

`devloop` is a standalone local development supervisor.

It is meant to sit outside an application repository and run that
repository through configuration and hooks. The goal is to make local
developer workflows configurable, ordered, and easy to adapt as the
project changes.

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

The first client config lives at:

`examples/gcp-rust-blog-public/devloop.toml`

It models the blog workflow as configuration:

- `rust` changes restart the server, wait for health, rebuild CSS, then
  restart the tunnel and publish the current post URL
- `content` changes restart the tunnel and republish the current post URL
- `css` changes trigger a one-shot Tailwind build

The example also keeps blog-specific logic out of the engine:

- `bin/run-cloudflared.sh` manages tunnel startup and writes the current
  tunnel URL into state
- `hooks/publish-current-url.sh` derives the latest post slug and emits a
  shareable URL as JSON

## Known gap

The current blog application still treats `SITE_URL` as process startup
state. That means restarting `cloudflared` without restarting the server
does not yet update app-rendered metadata such as social tags.

This is not a `devloop` engine problem. It is a client integration
problem: the app needs a dynamic way to read tunnel state, such as a
state file or a local adapter.

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
