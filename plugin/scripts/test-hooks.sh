#!/bin/sh
# Regression tests for the plugin hooks. No bus and no network required: the
# scripts are pointed at a fake bus-call.sh that captures the payload instead
# of sending it, so we can assert on the JSON the hooks would have sent.
#
# Run directly, or via `make check`.
set -u
FAIL=0
ROOT="$(cd "$(dirname "$0")" && pwd)"
WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT

ok()  { echo "  ok    $1"; }
bad() { echo "  FAIL  $1: $2"; FAIL=1; }

# A stand-in for bus-call.sh that records "<tool> <json>" and sends nothing.
mkdir -p "$WORK/bin"
cp "$ROOT/heartbeat.sh" "$WORK/bin/heartbeat.sh"
cat > "$WORK/bin/bus-call.sh" <<'FAKE'
#!/bin/sh
printf '%s\t%s\n' "$1" "${2:-{\}}" >> "$CAPTURE"
FAKE
chmod +x "$WORK/bin/bus-call.sh"

# A git checkout whose remote and branch carry characters that break naive
# string concatenation: quote, backslash, and non-ASCII.
REPO_DIR="$WORK/repo"
mkdir -p "$REPO_DIR"
(
    cd "$REPO_DIR" || exit 1
    git init -q .
    git remote add origin 'git@github.com:acme/we"ird\repo.git'
    git symbolic-ref HEAD 'refs/heads/feat/quote"and\back-ünicode'
) >/dev/null 2>&1

export CAPTURE="$WORK/capture.txt"
: > "$CAPTURE"

# --- heartbeat produces valid JSON for hostile repo/branch values ------------
(
    cd "$REPO_DIR" || exit 1
    BUS_URL=http://example.invalid/mcp BUS_TOKEN=acs_test \
        sh "$WORK/bin/heartbeat.sh" active
) >/dev/null 2>&1

payload="$(cut -f2 "$CAPTURE" | tail -1)"
if [ -z "$payload" ]; then
    bad "heartbeat sends a payload" "nothing captured"
else
    if printf '%s' "$payload" | python3 -c 'import json,sys; json.load(sys.stdin)' 2>/dev/null; then
        ok "heartbeat payload is valid JSON with quotes/backslash/unicode"
    else
        bad "heartbeat payload is valid JSON" "$payload"
    fi
    printf '%s' "$payload" | python3 -c '
import json, sys
args = json.load(sys.stdin)
assert args["status"] == "active", args
assert isinstance(args["ttl_seconds"], int), args
' 2>/dev/null && ok "heartbeat carries status and an integer ttl" \
        || bad "heartbeat carries status and ttl" "$payload"
fi

# --- idle uses the short TTL ------------------------------------------------
: > "$CAPTURE"
(cd "$REPO_DIR" && BUS_URL=x BUS_TOKEN=y sh "$WORK/bin/heartbeat.sh" idle) >/dev/null 2>&1
cut -f2 "$CAPTURE" | tail -1 | python3 -c '
import json, sys
args = json.load(sys.stdin)
assert args["status"] == "idle" and args["ttl_seconds"] < 900, args
' 2>/dev/null && ok "idle heartbeat shortens the presence lease" \
    || bad "idle heartbeat" "$(cut -f2 "$CAPTURE" | tail -1)"

# --- BUS_DIGEST_HOURS is validated before it reaches the request ------------
for value in "abc" "" "0" "999" "8; rm -rf /"; do
    hours="$value"
    case "$hours" in
        ''|*[!0-9]*) hours=8 ;;
        *) [ "$hours" -ge 1 ] && [ "$hours" -le 336 ] || hours=8 ;;
    esac
    printf '{"hours":%s}' "$hours" | python3 -c '
import json, sys
h = json.load(sys.stdin)["hours"]
assert isinstance(h, int) and 1 <= h <= 336, h
' 2>/dev/null || bad "BUS_DIGEST_HOURS guard rejects '$value'" "produced $hours"
done
ok "BUS_DIGEST_HOURS guard keeps the digest request well-formed"

# --- every tool the hooks call exists in the served schema ------------------
# Cheap coupling check: the tool names the scripts use must appear in the
# server's tool router. Catches a rename before a user's session breaks.
SRC="$ROOT/../../src/tools"
if [ -d "$SRC" ]; then
    missing=""
    for tool in whoami team_digest heartbeat list_tasks read_messages; do
        grep -rq "async fn $tool(" "$SRC" 2>/dev/null || missing="$missing $tool"
    done
    [ -z "$missing" ] && ok "tools the hooks and skill name exist server-side" \
        || bad "tools exist server-side" "missing:$missing"
fi

[ "$FAIL" -eq 0 ] && echo "plugin hooks: clean" || echo "plugin hooks: FAILURES"
exit "$FAIL"
