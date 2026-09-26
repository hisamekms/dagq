#!/bin/sh
# SessionStart (every source) and SessionEnd hook of the claude-dagq plugin:
# `session-event.sh open` at a SessionStart, `session-event.sh close` at a
# SessionEnd.
#
# Records the session span of the inbox and planner sessions (ADR-0048
# decision 6), which the runtime does not start headless: only a session
# with DAGQ_ROLE inbox or planner and DAGQ_QUEUE (which `up`, `dagq plan`
# and the supervisor put in the workspace's environment with --env) passes
# the hook's stdin (session_id, transcript_path, cwd, source or reason) to
# `dagq session-event open|close`, which reads the span's kind from
# DAGQ_SESSION_KIND (else DAGQ_ROLE) and the workspace from
# CMUX_WORKSPACE_ID. Every other session, workers included, is left alone.
# The hook prints nothing (a SessionStart's stdout would reach the context)
# and always exits 0: a missing dagq, a queue that cannot be opened or a
# failed record never stops the session, and the status the
# session-start.sh hook prints is not touched.
case "${1:-}" in
  open | close) event=$1 ;;
  *) exit 0 ;;
esac
case "${DAGQ_ROLE:-}" in
  inbox | planner) ;;
  *) exit 0 ;;
esac
[ -n "${DAGQ_QUEUE:-}" ] || exit 0

if [ -n "${DAGQ_BIN:-}" ]; then
  [ -x "$DAGQ_BIN" ] || exit 0
elif ! command -v dagq >/dev/null 2>&1; then
  exit 0
fi

# An explicit DAGQ_DB wins, as in session-start.sh.
if [ -z "${DAGQ_DB:-}" ]; then
  DAGQ_DB=$DAGQ_QUEUE
  export DAGQ_DB
fi

launcher=$(CDPATH='' cd -- "$(dirname -- "$0")/../bin" && pwd)/dagq
"$launcher" session-event "$event" >/dev/null 2>&1 || true
exit 0
