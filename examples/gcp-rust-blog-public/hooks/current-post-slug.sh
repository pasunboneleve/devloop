#!/usr/bin/env bash
set -euo pipefail

find "$DEVLOOP_ROOT/content/posts" -maxdepth 1 -type f -name '*.md' -printf '%f\n' \
  | sort \
  | tail -n1 \
  | sed 's/\.md$//'
