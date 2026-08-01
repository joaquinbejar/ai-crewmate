#!/bin/sh
# heartbeat.sh [active|idle|busy|blocked]
# Presence ping with repo/branch context from the current git checkout.
# Fire-and-forget: always exits 0, never blocks the session.
set -u
DIR="$(cd "$(dirname "$0")" && pwd)"
STATUS="${1:-active}"
TTL=900
[ "$STATUS" = "idle" ] && TTL=120

REPO="$(git config --get remote.origin.url 2>/dev/null \
  | sed -e 's#\.git$##' -e 's#.*[:/]\([^/]*/[^/]*\)$#\1#')"
BRANCH="$(git branch --show-current 2>/dev/null || true)"

# Git allows quotes, backslashes and other JSON-hostile characters in branch
# names and remote URLs, so the payload is encoded by json.dumps rather than
# string-concatenated. Without python3 we still publish presence, just without
# repo/branch — degraded detail beats a malformed request.
if command -v python3 >/dev/null 2>&1; then
    ARGS="$(STATUS="$STATUS" TTL="$TTL" REPO="$REPO" BRANCH="$BRANCH" python3 - <<'PY'
import json, os

args = {"status": os.environ["STATUS"], "ttl_seconds": int(os.environ["TTL"])}
for key in ("repo", "branch"):
    value = os.environ.get(key.upper(), "").strip()
    if value:
        args[key] = value
print(json.dumps(args))
PY
)"
else
    ARGS="{\"status\":\"$STATUS\",\"ttl_seconds\":$TTL}"
fi

"$DIR/bus-call.sh" heartbeat "$ARGS" >/dev/null 2>&1 || true
exit 0
