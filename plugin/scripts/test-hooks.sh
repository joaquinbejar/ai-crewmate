#!/bin/sh
# Regression tests for the plugin hooks. No bus and no network required: the
# scripts are pointed at a fake bus-call.sh that captures the payload instead
# of sending it, so we can assert on the JSON the hooks would have sent.
#
# Run directly, or via `make check`.
set -u
FAIL=0
ROOT="$(cd "$(dirname "$0")" && pwd)"
WORK="$(mktemp -d "${TMPDIR:-/tmp}/acs-hooks.XXXXXX")"
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
# string concatenation. Note the split: git refuses a backslash in a ref
# name, so a branch can carry a quote and non-ASCII but not a backslash — a
# remote URL is an arbitrary config string and can carry all three.
REPO_DIR="$WORK/repo"
mkdir -p "$REPO_DIR"
(
    cd "$REPO_DIR" || exit 1
    git init -q .
    git remote add origin 'git@github.com:acme/we"ird\repo.git'
    git symbolic-ref HEAD 'refs/heads/feat/quote"and-ünicode'
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
# The whole point of encoding rather than concatenating: these survive
# intact, quote and backslash and all. A payload that silently dropped
# them would still be valid JSON, so assert their content.
assert args.get("repo") == "acme/we\"ird\\repo", args
assert "ünicode" in args.get("branch", ""), args
assert "\"" in args["branch"], args
' 2>/dev/null && ok "repo and branch round-trip with quote, backslash and unicode" \
        || bad "repo/branch round-trip" "$payload"
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
# Drives the REAL session-start hook rather than reimplementing its guard, so
# a regression in the script itself cannot pass this test.
cp "$ROOT/session-start.sh" "$WORK/bin/session-start.sh"
for value in "abc" "" "0" "999" "8; rm -rf /" "12"; do
    : > "$CAPTURE"
    (
        cd "$REPO_DIR" || exit 1
        BUS_URL=http://example.invalid/mcp BUS_TOKEN=acs_test \
            BUS_DIGEST_HOURS="$value" sh "$WORK/bin/session-start.sh"
    ) >/dev/null 2>&1

    digest="$(grep '^team_digest' "$CAPTURE" | cut -f2 | tail -1)"
    if [ -z "$digest" ]; then
        bad "session-start requests a digest with BUS_DIGEST_HOURS='$value'" "no call captured"
        continue
    fi
    printf '%s' "$digest" | python3 -c '
import json, sys
hours = json.load(sys.stdin)["hours"]
assert isinstance(hours, int) and 1 <= hours <= 336, hours
' 2>/dev/null || bad "BUS_DIGEST_HOURS='$value' produces a valid window" "$digest"
done
ok "session-start clamps BUS_DIGEST_HOURS to a window the schema accepts"

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
