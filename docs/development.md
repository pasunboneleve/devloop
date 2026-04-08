# Development Guide

This guide covers local development workflows for `devloop` itself.

## Quality gates

Run the standard checks before committing:

```bash
cargo fmt
cargo test
cargo clippy --all-targets --all-features -- -D warnings
./scripts/ci-smoke.sh
```

`./scripts/ci-smoke.sh` is the fast runtime smoke test used in CI. It
checks that `devloop run` can start, begin watching, react to one file
change, and shut down cleanly.

## Opt-in watch flake smoke

The repeated-edit watch flake smoke test is intentionally opt-in. It is
useful when changing watch registration, debounce logic, or event
delivery, but it is slower and more environment-sensitive than the
standard test suite.

Run it locally with:

```bash
DEVLOOP_RUN_WATCH_FLAKE_SMOKE=1 cargo test --test watch_flake_smoke -- --nocapture
```

Without that environment variable, the test exits early so normal
`cargo test` and CI runs stay fast.

## Test policy

When a test must mutate process-global state such as environment
variables:

- serialize access with a test-local lock
- keep `unsafe` in a narrow helper
- document the safety rationale at the helper

Do not scatter raw `unsafe { std::env::set_var(...) }` calls across test
bodies.
