#!/bin/sh
# Stop hook: answer a teammate's blocking question before going quiet.
#
# A coding agent only calls MCP tools while it is processing a turn. A session
# parked at the prompt polls nothing, so a question addressed to it would sit
# unread until its human typed something. This hook closes the most useful part
# of that gap: on the way out, look for a question, and if there is one, hold
# the session open long enough to answer it.
#
# It cannot close the gap entirely — a session that has been idle for an hour
# still answers nothing. That is a property of the client, not of the bus.
#
# Always exits 0 and prints nothing when there is no question, when the bus is
# unreachable, or when the bus is not configured at all.
set -u
DIR="$(cd "$(dirname "$0")" && pwd)"
[ -n "${BUS_URL:-}" ] && [ -n "${BUS_TOKEN:-}" ] || exit 0
command -v python3 >/dev/null 2>&1 || exit 0

# Claude Code delivers the hook payload on stdin; session_id keys the loop
# guard so two sessions in different repositories do not share one.
PAYLOAD="$(cat 2>/dev/null || true)"

# only_new is false on purpose: this must not advance the read cursor. Marking
# a question read here would hide it from the read_messages call the model is
# about to make to answer it.
INBOX="$("$DIR/bus-call.sh" read_messages \
    '{"scope":"inbox","only_new":false,"limit":20}' 2>/dev/null || true)"
[ -n "$INBOX" ] || exit 0

STATE_DIR="${TMPDIR:-/tmp}"
export PAYLOAD INBOX STATE_DIR

python3 - <<'PY' 2>/dev/null || true
import json, os, re

def parse(raw):
    try:
        return json.loads(raw)
    except Exception:
        return None

inbox = parse(os.environ.get("INBOX", "")) or {}
try:
    messages = inbox["result"]["structuredContent"]["messages"]
except Exception:
    raise SystemExit(0)

# A question is a direct message the sender's agent is blocked on: ask_agent
# marks it, and post_message can too.
questions = [
    m for m in messages
    if isinstance(m, dict) and (m.get("metadata") or {}).get("question") is True
]
if not questions:
    raise SystemExit(0)
latest = questions[-1]
message_id = latest.get("id")
if not isinstance(message_id, int):
    raise SystemExit(0)

# Loop guard. A Stop hook that blocks unconditionally traps the session going
# round forever, so each question is only ever blocked on once — if the model
# does not answer, the hook does not nag.
payload = parse(os.environ.get("PAYLOAD", "")) or {}
session_key = str(payload.get("session_id") or "default")
session_key = re.sub(r"[^A-Za-z0-9_.-]", "_", session_key)[:64]
state = os.path.join(os.environ["STATE_DIR"], f"ai-crew-sync-drain-{session_key}")

try:
    with open(state) as fh:
        last = int(fh.read().strip() or 0)
except Exception:
    last = 0
if message_id <= last:
    raise SystemExit(0)

try:
    with open(state, "w") as fh:
        fh.write(str(message_id))
except Exception:
    # Without a durable marker the guard cannot hold, and blocking anyway
    # risks the loop this exists to prevent.
    raise SystemExit(0)

sender = latest.get("from") or "a teammate"
if latest.get("from_session"):
    sender = f"{sender}/{latest['from_session']}"
body = (latest.get("body") or "").strip()
if len(body) > 1500:
    body = body[:1500] + "…(truncated — read_messages has the whole thing)"

print(json.dumps({
    "decision": "block",
    "reason": f"{sender} is blocked waiting on an answer from you.",
    "hookSpecificOutput": {
        "hookEventName": "Stop",
        "additionalContext": (
            f"[ai-crew-sync] {sender} asked you a question and their agent is "
            f"blocked waiting for the reply:\n\n{body}\n\n"
            f"Answer it now with post_message (to: \"{sender}\", "
            f"reply_to: {message_id}), then finish. Address the sender's "
            "session, not just their name, or the answer reaches a different "
            "window from the one that is waiting. If you genuinely cannot "
            "answer, say so in the reply rather than leaving them blocked."
        ),
    },
}))
PY
exit 0
