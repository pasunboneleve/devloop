#!/usr/bin/env bash
set -euo pipefail

printf 'ready\n'
exec tail -f /dev/null
