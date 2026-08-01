#!/bin/sh
# SessionStart hook: announce presence on the bus and inject a compact
# team catch-up (whoami + team_digest) into the session context.
# Always exits 0; produces no output when the bus is unreachable.
set -u
DIR="$(cd "$(dirname "$0")" && pwd)"
[ -n "${BUS_URL:-}" ] && [ -n "${BUS_TOKEN:-}" ] || exit 0

"$DIR/heartbeat.sh" active >/dev/null 2>&1 || true
command -v python3 >/dev/null 2>&1 || exit 0

WHO="$("$DIR/bus-call.sh" whoami 2>/dev/null || true)"
DIG="$("$DIR/bus-call.sh" team_digest "{\"hours\":${BUS_DIGEST_HOURS:-8}}" 2>/dev/null || true)"
export WHO DIG BUS_DIGEST_HOURS="${BUS_DIGEST_HOURS:-8}"

python3 - <<'PY' 2>/dev/null || true
import json, os

def sc(raw):
    try:
        return json.loads(raw)["result"]["structuredContent"]
    except Exception:
        return None

who = sc(os.environ.get("WHO", ""))
dig = sc(os.environ.get("DIG", ""))
if not who and not dig:
    raise SystemExit(0)

lines = []
if who:
    lines.append(
        f"[ai-crew-sync] You are agent '{who.get('agent')}' on team '{who.get('team')}'. "
        "The team coordination bus (MCP server 'ai-crew-sync') is connected."
    )
    dm = who.get("unread_direct_messages") or 0
    ct = who.get("open_claimed_tasks") or 0
    if dm:
        lines.append(f"- {dm} unread direct message(s) for you. Read them with read_messages before starting work.")
    if ct:
        lines.append(f"- {ct} task(s) claimed by you are still open (list_tasks mine=true).")
if dig:
    hours = os.environ.get("BUS_DIGEST_HOURS", "8")
    compact = json.dumps(dig, ensure_ascii=False, separators=(",", ":"))
    if len(compact) > 2500:
        compact = compact[:2500] + "…(truncated — call team_digest for the full picture)"
    lines.append(f"- Team activity, last {hours}h (team_digest): {compact}")
lines.append(
    "- Conventions: claim_task before working on shared tasks, post progress to the relevant "
    "channel, and use wait_for_updates when you need a teammate's reply."
)

print(json.dumps({
    "hookSpecificOutput": {
        "hookEventName": "SessionStart",
        "additionalContext": "\n".join(lines),
    }
}))
PY
exit 0
