#!/bin/sh
# SessionStart hook (every source: startup, resume, clear, compact) of the
# claude-dagq plugin.
#
# Only the inbox `dagq up` opens and the planners `dagq plan` or the
# supervisor opens are affected: DAGQ_ROLE (inbox or planner, which they
# put in the workspace's environment with --env) selects `dagq status --role <role>` (supervisors, unfinished runs,
# the attention and asks addressed to that role, and the next cursor) on
# stdout, which Claude Code adds to the context, so the session re-orients
# itself after a start, a resume, compaction or /clear. The inbox's output
# starts with one line telling it to start `watch --role inbox
# --until-attention --after <cursor>` in the background as its first move
# (ADR-t906-1), one command with no timeout and no shell loop, so that no
# ask waits unseen for want of a watch. A planner's startup and resume
# print nothing: its initial prompt orients it, and a resume keeps its
# context. The status comes after one line of text
# naming the role and its skill, and pointing at the status's `language`
# (ADR-t616-2), whose `instruction` names the language to write for people
# in when one is set: Claude Code reads a stdout that is JSON as
# the hook's control output and drops its unknown keys, so the bare status
# would never reach the context. Every other session, workers included,
# gets no output. The hook never fails the session start: when status cannot
# be read it prints one line saying why and exits 0.
case "${DAGQ_ROLE:-}" in
  inbox | planner) role=$DAGQ_ROLE ;;
  *) exit 0 ;;
esac

# The hook's input names the source; without one (no input), print as for
# compact and clear.
input=
[ -t 0 ] || input=$(cat)
source=$(printf '%s' "$input" | tr -d '\n' | sed -n 's/.*"source"[[:space:]]*:[[:space:]]*"\([a-z]*\)".*/\1/p')
if [ "$role" = planner ]; then
  case $source in
    startup | resume) exit 0 ;;
  esac
fi

launcher=$(CDPATH='' cd -- "$(dirname -- "$0")/../bin" && pwd)/dagq

if [ -n "${DAGQ_BIN:-}" ]; then
  if [ ! -x "$DAGQ_BIN" ]; then
    printf 'dagq status unavailable: DAGQ_BIN is not an executable file: %s\n' "$DAGQ_BIN"
    exit 0
  fi
elif ! command -v dagq >/dev/null 2>&1; then
  echo "dagq status unavailable: dagq was not found on PATH and DAGQ_BIN is unset; run the dagq skill's --resolve for the install steps"
  exit 0
fi

# `up` and `plan` name the session's queue in DAGQ_QUEUE; an explicit DAGQ_DB wins.
if [ -z "${DAGQ_DB:-}" ] && [ -n "${DAGQ_QUEUE:-}" ]; then
  DAGQ_DB=$DAGQ_QUEUE
  export DAGQ_DB
fi

if output=$("$launcher" status --role "$role" 2>/dev/null); then
  if [ "$role" = inbox ]; then
    # The top-level `cursor` of the pretty-printed status.
    cursor=$(printf '%s\n' "$output" | sed -n 's/^  "cursor": \([0-9][0-9]*\),\{0,1\}$/\1/p')
    # Without a cursor (a status of another shape) the watch starts from now.
    printf 'First move, before anything else: start the watch of the dagq-inbox skill (reference/watch.md) with run_in_background from cursor %s, the one command "%s" watch --role inbox --until-attention%s with no shell loop around it, unless that watch is still running among this session'"'"'s background tasks; keep one running from then on.\n' "${cursor:-now}" "$launcher" "${cursor:+ --after $cursor}"
  fi
  printf 'This session is the dagq %s (DAGQ_ROLE=%s); follow the dagq-%s skill of the dagq plugin. The queue status for this role (dagq status --role %s); when its language.instruction is set, write for people as it says:\n' "$role" "$role" "$role" "$role"
  printf '%s\n' "$output"
else
  # Ask again for the error alone and keep it on one line; printf, because
  # sh's echo expands the `\n` escapes inside the JSON error.
  error=$("$launcher" status --role "$role" 2>&1 >/dev/null | tr -s '\n' ' ')
  printf 'dagq status failed: %s\n' "$error"
fi
exit 0
