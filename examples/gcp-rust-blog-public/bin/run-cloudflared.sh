#!/usr/bin/env bash
set -euo pipefail

wait_for_server() {
  local local_port=${LOCAL_PORT:-8080}
  local deadline=$((SECONDS + 30))
  while (( SECONDS < deadline )); do
    if curl -fsS "http://127.0.0.1:${local_port}/" >/dev/null 2>&1; then
      return 0
    fi
    sleep 0.5
  done
  echo "timed out waiting for server before starting cloudflared" >&2
  return 1
}

write_state() {
  local tunnel_url=$1
  python3 - "$DEVLOOP_STATE" "$tunnel_url" <<'PY'
import json
import pathlib
import sys

state_path = pathlib.Path(sys.argv[1])
tunnel_url = sys.argv[2]
state_path.parent.mkdir(parents=True, exist_ok=True)
data = {}
if state_path.exists():
    data = json.loads(state_path.read_text())
data["tunnel_url"] = tunnel_url
state_path.write_text(json.dumps(data, indent=2, sort_keys=True))
PY
}

wait_for_server

temp_log=$(mktemp)
cleanup() {
  rm -f "$temp_log"
}
trap cleanup EXIT

cloudflared tunnel --url "http://127.0.0.1:${LOCAL_PORT:-8080}" >"$temp_log" 2>&1 &
cloudflared_pid=$!

tunnel_url=""
for _ in $(seq 1 120); do
  if grep -Eo 'https://[-a-z0-9]+\.trycloudflare\.com' "$temp_log" >/dev/null 2>&1; then
    tunnel_url=$(grep -Eo 'https://[-a-z0-9]+\.trycloudflare\.com' "$temp_log" | head -n1)
    break
  fi
  if ! kill -0 "$cloudflared_pid" >/dev/null 2>&1; then
    cat "$temp_log" >&2
    wait "$cloudflared_pid"
  fi
  sleep 0.5
done

if [[ -z "$tunnel_url" ]]; then
  cat "$temp_log" >&2
  kill "$cloudflared_pid" >/dev/null 2>&1 || true
  echo "failed to discover cloudflared tunnel url" >&2
  exit 1
fi

write_state "$tunnel_url"
printf 'cloudflared tunnel: %s\n' "$tunnel_url"
tail -n +1 -f "$temp_log" &
tail_pid=$!

forward_signal() {
  kill "$cloudflared_pid" >/dev/null 2>&1 || true
  kill "$tail_pid" >/dev/null 2>&1 || true
}

trap forward_signal INT TERM
wait "$cloudflared_pid"
