# Security Notes

This document describes the trust model and security tradeoffs for
`devloop` features that accept input from outside the core watch loop.

## Threat model

`devloop` supervises local processes, runs hooks, persists session
state, and can trigger workflows that restart services or execute local
commands. Because of that, any external input path must be treated as a
potential local code-execution capability.

The main concern is not just state mutation. The real chain is:

1. an external caller changes `devloop` session state
2. that change triggers a workflow
3. the workflow may run hooks or restart processes
4. those hooks and processes may execute arbitrary local commands

So a loosely designed control endpoint would effectively become a local
command-execution surface.

## Current external event model

`devloop` supports config-declared external events over an HTTP server
bound to localhost.

Key constraints:

- The listener binds only to the configured local socket address.
- Each `devloop run` generates a fresh random bearer token.
- Child processes receive the event URLs and token through environment
  variables.
- Clients cannot choose arbitrary state keys or arbitrary workflows.
  Config maps each event name to one fixed `state_key` and one fixed
  follow-up `workflow`.
- Event payloads are data only. They are never treated as shell code.
- Optional regex validation can constrain accepted payload values.

That means the capability is intentionally narrow:

- allowed: post a value to a predeclared event such as `browser_path`
- not allowed: ask `devloop` to run an arbitrary workflow
- not allowed: write arbitrary session-state keys
- not allowed: execute commands directly

## Residual risks

This feature still has a real security cost.

- Any same-user local process that can read the bearer token can likely
  send valid events.
- If a supervised child process is compromised, it can use the token
  and event URLs that `devloop` injected into its environment.
- A bad config mapping can still produce dangerous behavior if it routes
  an untrusted event into a sensitive workflow.
- Localhost-only binding reduces exposure, but it is not a full
  security boundary on a multi-process development machine.

So the token is meant to reduce accidental or drive-by misuse. It is
not a hardened defense against a malicious same-user local process.

## Push versus polling

There are two general ways to feed dynamic local state into `devloop`.

### Push

Example: a development web server receives a browser-path update and
forwards the path to `devloop`.

Advantages:

- immediate updates
- no repetitive polling noise
- lower idle CPU and process churn
- cleaner event-driven architecture

Costs:

- larger protocol surface
- more security-sensitive
- requires capability design and documentation

### Polling

Example: an observed hook polls a local endpoint and reruns a workflow
only when session state changes.

Advantages:

- simpler to implement
- no local listener inside `devloop`
- lower security exposure

Costs:

- more latency
- repeated helper-command execution
- can be noisy if hook output is inherited

## Guidance

Use push when:

- the event is precise and user-facing
- low latency matters
- the calling process already has a clear trust relationship with
  `devloop`

Use polling when:

- the integration needs to stay simple
- the extra local listener is not justified
- a small amount of latency or redundant work is acceptable

## Non-goals

The external event system is not intended to become:

- a generic remote-control API
- an arbitrary workflow runner
- a writable key-value store for any client
- a replacement for authenticated service-to-service protocols
