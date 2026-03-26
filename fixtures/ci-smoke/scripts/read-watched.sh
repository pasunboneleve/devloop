#!/usr/bin/env bash
set -euo pipefail

tr -d '\r' < watched.txt | head -n 1
