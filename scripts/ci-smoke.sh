#!/usr/bin/env bash
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
fixture_src="${repo_root}/fixtures/ci-smoke"
tmp_dir="$(mktemp -d)"
cleanup() {
  if [[ -n "${devloop_pid:-}" ]] && kill -0 "${devloop_pid}" 2>/dev/null; then
    kill -INT "${devloop_pid}" 2>/dev/null || true
    wait "${devloop_pid}" || true
  fi
  rm -rf "${tmp_dir}"
}
trap cleanup EXIT

cp -R "${fixture_src}/." "${tmp_dir}/"
chmod +x "${tmp_dir}/scripts/read-watched.sh"

log_path="${tmp_dir}/devloop.log"
state_path="${tmp_dir}/.devloop/state.json"
devloop_bin="${repo_root}/target/debug/devloop"

if [[ ! -x "${devloop_bin}" ]]; then
  (cd "${repo_root}" && cargo build >/dev/null)
fi

"${devloop_bin}" run --config "${tmp_dir}/devloop.toml" >"${log_path}" 2>&1 &
devloop_pid=$!

python3 - "$state_path" <<'PY'
import json
import pathlib
import sys
import time

state_path = pathlib.Path(sys.argv[1])
deadline = time.time() + 15
while time.time() < deadline:
    if state_path.exists():
        data = json.loads(state_path.read_text())
        if data.get("current_value") == "initial":
            sys.exit(0)
    time.sleep(0.1)
raise SystemExit("timed out waiting for startup state")
PY

python3 - "$log_path" <<'PY'
import pathlib
import sys
import time

log_path = pathlib.Path(sys.argv[1])
deadline = time.time() + 15
while time.time() < deadline:
    if log_path.exists() and "watching " in log_path.read_text():
        sys.exit(0)
    time.sleep(0.1)
raise SystemExit("timed out waiting for watcher startup")
PY

python3 - "$state_path" "$log_path" "${tmp_dir}/watched.txt" <<'PY'
import json
import pathlib
import sys
import time

state_path = pathlib.Path(sys.argv[1])
log_path = pathlib.Path(sys.argv[2])
watched_path = pathlib.Path(sys.argv[3])
deadline = time.time() + 15
next_write = 0.0
while time.time() < deadline:
    now = time.time()
    if now >= next_write:
        watched_path.write_text("updated\n")
        next_write = now + 0.5
    if state_path.exists():
        data = json.loads(state_path.read_text())
        if (
            data.get("current_value") == "updated"
            and data.get("current_url") == "http://127.0.0.1:18081/updated"
        ):
            if "changed value: updated" in log_path.read_text():
                sys.exit(0)
    time.sleep(0.1)
print(log_path.read_text(), file=sys.stderr)
raise SystemExit("timed out waiting for changed state")
PY

kill -INT "${devloop_pid}"
wait "${devloop_pid}"
