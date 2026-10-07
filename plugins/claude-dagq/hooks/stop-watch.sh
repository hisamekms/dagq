#!/bin/sh
# Stop hook of the claude-dagq plugin: keeps the inbox from ending a turn
# without its own `watch --role inbox` running.
#
# Only the inbox (DAGQ_ROLE=inbox) is affected. The hook reads
# `dagq status --role inbox`, whose `inbox_watcher.watching` counts the
# `watch --role inbox` processes watching now by the freshness of their
# heartbeat (the grace after a watch returned does not count here: the turn
# must leave one running). With none, it looks once more a second later (a
# watch started at the end of the turn may not have written its record yet)
# and then prints Claude Code's control JSON {"decision": "block", "reason":
# ...}, which keeps the session going with the reason, the command that
# starts the watch from the status's cursor. It blocks at most once in a
# row: with `stop_hook_active` true in its input (the session already
# continued because of a Stop hook) it lets the turn end, so it never loops.
# Every other case prints nothing and exits 0: another role, no dagq, a
# status that cannot be read, or one without `inbox_watcher` (an older
# binary). The hook never fails the session.
[ "${DAGQ_ROLE:-}" = inbox ] || exit 0

input=
[ -t 0 ] || input=$(cat)
if printf '%s' "$input" | tr -d '\n' | grep -Eq '"stop_hook_active"[[:space:]]*:[[:space:]]*true'; then
  exit 0
fi

if [ -n "${DAGQ_BIN:-}" ]; then
  [ -x "$DAGQ_BIN" ] || exit 0
elif ! command -v dagq >/dev/null 2>&1; then
  exit 0
fi

# An explicit DAGQ_DB wins, as in session-start.sh.
if [ -z "${DAGQ_DB:-}" ] && [ -n "${DAGQ_QUEUE:-}" ]; then
  DAGQ_DB=$DAGQ_QUEUE
  export DAGQ_DB
fi

launcher=$(CDPATH='' cd -- "$(dirname -- "$0")/../bin" && pwd)/dagq

# The status, and its `inbox_watcher.watching` (pretty-printed: the object's
# fields are indented by four spaces).
watching() {
  status=$("$launcher" status --role inbox 2>/dev/null) || return 1
  count=$(printf '%s\n' "$status" |
    sed -n '/^  "inbox_watcher": {/,/^  }/s/^    "watching": \([0-9][0-9]*\),\{0,1\}$/\1/p')
  [ -n "$count" ]
}

watching || exit 0
[ "$count" -gt 0 ] && exit 0
sleep 1
watching || exit 0
[ "$count" -gt 0 ] && exit 0

cursor=$(printf '%s\n' "$status" | sed -n 's/^  "cursor": \([0-9][0-9]*\),\{0,1\}$/\1/p')
command="\"$launcher\" watch --role inbox --until-attention${cursor:+ --after $cursor}"
reason="No dagq inbox watch is running (status --role inbox: inbox_watcher.watching is 0), so asks and attention would not reach the person. If a watch of this session has just returned output you have not handled, handle it and start the next watch from its cursor. Otherwise, before ending the turn, start the watch of the dagq-inbox skill (reference/watch.md) with run_in_background from cursor ${cursor:-now}, as this one command with no shell loop around it: $command. Keep one watch only: if one is still running among this session's background tasks, check that it has not failed instead of starting another."
# A JSON string: backslashes and double quotes escaped.
escaped=$(printf '%s' "$reason" | sed 's/\\/\\\\/g; s/"/\\"/g')
printf '{"decision": "block", "reason": "%s"}\n' "$escaped"
exit 0
