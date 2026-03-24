# PLAN

## Current objective
Build a standalone, configuration-driven dev supervisor and use the blog
repository as the first client.

## Milestones

### 1. Project scaffolding
- Replace default boilerplate with project-specific docs.
- Define architecture boundaries between engine, config, and hooks.
- Capture work in `bd`.

### 2. Engine MVP
- Load a config file from a target repository.
- Start and supervise named processes.
- Watch path groups and classify changes.
- Execute ordered workflow steps.
- Capture session state in a stable file.

### 3. First client
- Add an example config for the blog repository.
- Add repo-local hooks for current post and public URL reporting.
- Verify that config can be parsed and the engine can start against the
  repo.

### 4. Refinement
- Tighten error handling and logging.
- Add health checks and restart sequencing.
- Decide which parts should remain hooks and which should become core
  features.
