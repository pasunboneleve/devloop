# Transactional Artifact Generations

Use artifact generations when a build command replaces a directory that a
long-running HTTP server is already serving. A status-only readiness probe can
observe the server while its asset manifest still points at deleted files.
`publish_artifact` prevents that mixed state.

This guide is also available in the CLI:

```bash
devloop docs artifacts
```

## Agent rule

If a rebuild deletes or replaces served output, configure an artifact. Do not
teach a workflow to delete the live directory, restart the server, or poll `/`
with a status-only probe. Devloop must own the candidate directory, generation
switch, exact readiness check, rollback, and retention.

## Complete configuration

```toml
root = "."
startup_workflows = ["build_site"]

[watch.site]
paths = ["src/**", "public/"]
workflow = "build_site"

[hook.build_site]
command = ["./scripts/build-site.sh"]

[process.site]
command = ["./scripts/serve-site.sh"]
autostart = false

[process.site.readiness]
kind = "http"
url = "http://127.0.0.1:8787/__devloop_generation"
expect_body = "{{ artifact.site.generation }}"
interval_ms = 250
timeout_ms = 30000

[artifact.site]
build_hook = "build_site"
consumers = ["site"]
retain = 2

[workflow.build_site]
steps = [{ action = "publish_artifact", artifact = "site" }]
triggers = ["browser_reload"]

[workflow.browser_reload]
steps = [{ action = "notify_reload" }]
```

The build hook writes only to `DEVLOOP_ARTIFACT_CANDIDATE`. The consumer reads
`DEVLOOP_ARTIFACT_SITE_DIR` and `DEVLOOP_ARTIFACT_SITE_GENERATION` from its
environment, serves that directory, and returns the generation value as the
complete response body of `/__devloop_generation`. Artifact names are converted
to uppercase environment components; non-alphanumeric characters become `_`.
Artifact names must start with a lowercase letter and contain only lowercase
letters, digits, and underscores.

Set artifact consumers to `autostart = false` and put the publication workflow
in `startup_workflows`. The publish action starts each stopped consumer after a
successful initial build.

## Guarantees

For each publication, devloop:

1. removes incomplete candidate directories left by an interrupted build
2. creates a private candidate directory
3. runs the build hook with candidate and generation environment variables
4. preserves the live process and active generation if the build fails
5. makes the completed directory immutable by generation name
6. switches session state and restarts the declared consumers
7. accepts readiness only when the response body matches the expected generation
8. restores and restarts the previous generation if switching fails
9. removes old generations beyond `retain`
10. runs downstream triggers, including browser reload, only after success

The workflow API deliberately exposes only `publish_artifact`. Partial
`prepare_artifact` or `promote_artifact` steps do not exist because their order
would be easy to misconfigure.

Once consumers pass exact-generation readiness, the switch is committed.
Failure to remove an older retained directory is logged but cannot turn that
successful switch into a failed workflow. An interrupted, unverified switch is
marked in session state and conservatively restored on the next publication.

## Environment

During the build hook:

- `DEVLOOP_ARTIFACT`: configured artifact name
- `DEVLOOP_ARTIFACT_GENERATION`: candidate generation identifier
- `DEVLOOP_ARTIFACT_CANDIDATE`: absolute candidate directory
- `DEVLOOP_ARTIFACT_<NAME>_GENERATION`: same candidate identifier
- `DEVLOOP_ARTIFACT_<NAME>_DIR`: same candidate directory

After promotion, every hook and managed process receives the named `DIR` and
`GENERATION` variables for every active artifact. Project code remains
responsible only for writing to the supplied directory, serving the supplied
directory, and returning the supplied generation from its readiness endpoint.
