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

ARGS="{\"status\":\"$STATUS\",\"ttl_seconds\":$TTL"
[ -n "$REPO" ] && ARGS="$ARGS,\"repo\":\"$REPO\""
[ -n "$BRANCH" ] && ARGS="$ARGS,\"branch\":\"$BRANCH\""
ARGS="$ARGS}"

"$DIR/bus-call.sh" heartbeat "$ARGS" >/dev/null 2>&1 || true
exit 0
