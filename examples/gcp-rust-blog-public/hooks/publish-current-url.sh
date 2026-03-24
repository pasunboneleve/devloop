#!/usr/bin/env bash
set -euo pipefail

latest_post() {
  find "$DEVLOOP_ROOT/content/posts" -maxdepth 1 -type f -name '*.md' -printf '%f\n' \
    | sort \
    | tail -n1 \
    | sed 's/\.md$//'
}

tunnel_url=$(python3 - "$DEVLOOP_STATE" <<'PY'
import json
import pathlib
import sys
import time

path = pathlib.Path(sys.argv[1])
deadline = time.time() + 30
while time.time() < deadline:
    if path.exists():
        data = json.loads(path.read_text())
        tunnel_url = data.get("tunnel_url", "")
        if tunnel_url:
            print(tunnel_url)
            raise SystemExit(0)
    time.sleep(0.5)
print("")
PY
)

slug=$(latest_post)
if [[ -n "$tunnel_url" && -n "$slug" ]]; then
  post_url="${tunnel_url}/posts/${slug}"
else
  post_url=""
fi

python3 - "$slug" "$post_url" <<'PY'
import json
import sys

slug = sys.argv[1]
post_url = sys.argv[2]
print(json.dumps({
    "current_post_slug": slug,
    "current_post_url": post_url,
}, indent=2, sort_keys=True))
PY
