#!/bin/sh
# bus-call.sh <tool> [json-args]
# One stateless JSON-RPC tools/call against the crew bus. Prints the raw
# JSON-RPC response on stdout. Silently no-ops if BUS_URL/BUS_TOKEN are unset,
# so the plugin never breaks a session that has no bus configured.
set -eu
[ -n "${BUS_URL:-}" ] && [ -n "${BUS_TOKEN:-}" ] || exit 0
TOOL="$1"
ARGS="${2:-{\}}"
curl -sf --max-time "${BUS_TIMEOUT:-8}" -X POST "$BUS_URL" \
  -H "Authorization: Bearer $BUS_TOKEN" \
  -H "Content-Type: application/json" \
  -H "Accept: application/json, text/event-stream" \
  --data "{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"tools/call\",\"params\":{\"name\":\"$TOOL\",\"arguments\":$ARGS}}"
