//! The stub `claude` of the e2e: what the runtime runs in place of Claude
//! Code, written next to each fixture's queue.

/// Stand-in for Claude Code. It accepts the argv the Claude adapter builds.
/// A worker's turn (`claude -p --output-format stream-json`, ADR-t813-1)
/// follows its prompt: work in the cwd worktree, commit, publish the receipt
/// by atomic rename and print the turn's stream; the session wrapper writes
/// the idle marker when the turn ends. No e2e case opens a planner: the
/// runtime's planners run headless (ADR-t1433-2), and the case that drove
/// an interactive one in cmux went with task 1441.
pub(crate) const STUB: &str = concat!(
    r#"#!/bin/sh
set -eu
"#,
    crate::await_file_fn!(),
    r#"
if [ "${1:-}" = "--version" ]; then
  printf 'claude-stub 0.0.0\n'
  exit 0
fi
if [ "${1:-}" = plugin ] && [ "${2:-}" = list ]; then
  # `up` without --plugin-dir checks the installed plugin (ADR-t617-2
  # decision 4).
  printf '[{"id":"claude-dagq@dagq","version":"0.0.0","scope":"user","enabled":true}]\n'
  exit 0
fi
session_id= debug_file= add_dir= settings= prompt= resume= headless= tools= denied= plugin_dir= model= effort= output= permission= mcp= strict= sources=unset
while [ $# -gt 0 ]; do
  case "$1" in
    -p) headless=1; shift ;;
    --output-format) output=$2; shift 2 ;;
    --verbose) shift ;;
    --permission-mode) permission=$2; shift 2 ;;
    --allowedTools) tools=$2; shift 2 ;;
    --disallowedTools) denied=$2; shift 2 ;;
    --session-id) session_id=$2; shift 2 ;;
    --resume) resume=$2; shift 2 ;;
    --debug-file) debug_file=$2; shift 2 ;;
    --add-dir) add_dir=$2; shift 2 ;;
    --settings) settings=$2; shift 2 ;;
    --plugin-dir) plugin_dir=$2; shift 2 ;;
    --mcp-config) mcp=$2; shift 2 ;;
    --strict-mcp-config) strict=1; shift ;;
    --model) model=$2; shift 2 ;;
    --effort) effort=$2; shift 2 ;;
    --setting-sources) sources=$2; shift 2 ;;
    --) shift; prompt=$1; shift; break ;;
    *) printf 'stub: unexpected argument %s\n' "$1" >&2; exit 64 ;;
  esac
done
[ $# -eq 0 ] || { printf 'stub: trailing arguments after the prompt\n' >&2; exit 64; }
if [ -n "$headless" ] && [ -z "$permission" ]; then
  # The supervisor's headless review (ADR-0027), started without a worker
  # turn's permission mode: read review.md, print the
  # verdict JSON on stdout. Only a task that says E2E-REVIEW-PASS passes;
  # any other review fails, and its run waits in an approve_landing ask.
  # The runtime names the review's own session (ADR-0048 decision 4), and
  # the review loads no setting sources (ADR-t1470-1). Its prompt comes on
  # stdin, never as an argument (task 1560).
  [ -z "$prompt" ] || { printf 'stub: the review prompt is an argument\n' >&2; exit 64; }
  prompt=$(cat)
  [ -n "$session_id" ] && [ -n "$debug_file" ] && [ -n "$add_dir" ] && [ -n "$settings" ] && [ -n "$prompt" ] \
    && [ "$tools" = "Read,Grep,Glob" ] && [ "$denied" = "Bash,Edit,Write,NotebookEdit,ScheduleWakeup,CronCreate" ] \
    && [ -z "$sources" ] || { printf 'stub: bad review arguments\n' >&2; exit 64; }
  ! grep -q '"Stop"' "$settings" || { printf 'stub: review settings would write the idle marker\n' >&2; exit 64; }
  review=$(printf '%s\n' "$prompt" | sed -n 's/^Read the review material at \(.*\): the task.*/\1/p')
  [ -f "$review" ] || { printf 'stub: no review material at %s\n' "$review" >&2; exit 65; }
  printf 'argv: -p --debug-file %s --add-dir %s --settings %s\nreview: %s\nsession: %s\n' "$debug_file" "$add_dir" "$settings" "$review" "$session_id" > "$debug_file"
  if grep -q 'E2E-REVIEW-PASS' "$review"; then
    printf '{"verdict":"pass","reasons":[],"summary":"the stub reviewer found e2e.txt committed"}\n'
    exit 0
  fi
  printf 'stub: no verdict for this task\n' >&2
  exit 3
fi
# A worker's turn (ADR-t813-1): `claude -p --output-format stream-json
# --verbose`, the session started by `--session-id` (or resumed), in
# `auto` with settings without a Stop hook. The turn's prompt is the
# task's, a person's answer (`answer to ask <id>: ...`) or a resolution
# request; it does what the prompt asks, commits, writes the receipt and
# prints the stream: init, a message, the result.
[ "$output" = stream-json ] || { printf 'stub: not a worker turn\n' >&2; exit 64; }
sid=${session_id:-$resume}
[ -n "$headless" ] && [ -n "$sid" ] && [ "$permission" = auto ] \
  && [ -n "$debug_file" ] && [ -n "$add_dir" ] && [ -n "$settings" ] && [ -n "$prompt" ] \
  || { printf 'stub: bad headless turn arguments\n' >&2; exit 64; }
! grep -q '"Stop"' "$settings" || { printf 'stub: headless settings have a hook\n' >&2; exit 64; }
grep -q 'Bash(pkill:\*)' "$settings" || { printf 'stub: headless settings deny no pkill\n' >&2; exit 64; }
{
  printf 'turn argv: -p --output-format %s --session-id %s --resume %s --permission-mode %s --add-dir %s --settings %s --model %s\n' \
    "$output" "$session_id" "$resume" "$permission" "$add_dir" "$settings" "$model"
  printf 'model: %s effort: %s\n' "$model" "$effort"
  printf 'cwd: %s\n' "$(pwd)"
  printf 'env: DAGQ_ROLE=%s DAGQ_QUEUE=%s DAGQ_SERVICE_SOCKET=%s\n' "${DAGQ_ROLE:-}" "${DAGQ_QUEUE:-}" "${DAGQ_SERVICE_SOCKET:-}"
  printf 'run env: E2E_SHARED=%s E2E_RUN_DIR=%s\n' "${E2E_SHARED:-}" "${E2E_RUN_DIR:-}"
} >> "$debug_file"
printf '{"type":"system","subtype":"init","session_id":"%s","model":"%s","permissionMode":"%s"}\n' "$sid" "$model" "$permission"
# The first turn's prompt names the receipt; a later turn's does not, and
# the receipt is the run directory's.
receipt=$(printf '%s\n' "$prompt" | sed -n 's/^Write a completion receipt to \(.*\) using a temporary file in the same directory.*/\1/p')
[ -n "$receipt" ] || receipt="$add_dir/receipt.json"
# Ends the turn: an assistant message and the result.
end_turn() {
  printf '{"type":"assistant","message":{"model":"%s","content":[{"type":"text","text":"%s"}]}}\n' "$model" "$1"
  printf '{"type":"result","subtype":"success","is_error":false,"num_turns":3,"duration_ms":40,"total_cost_usd":0.02,"session_id":"%s","result":"done","usage":{"input_tokens":11,"output_tokens":5},"permission_denials":[]}\n' "$sid"
  exit 0
}
# Writes the receipt for the branch head by atomic rename.
write_receipt() {
  sh -c 'test -f seed.txt'
  commit=$(git rev-parse HEAD)
  printf '{"run_id":"%s","result":"succeeded","commit":"%s","tests":{"status":"passed","evidence_or_reason":"%s"},"e2e":{"status":"not_applicable","evidence_or_reason":"stub agent"},"subagent_review":{"status":"not_applicable","evidence_or_reason":"stub agent"},"summary":"%s"}\n' \
    "$sid" "$commit" "$1" "$2" > "$receipt.tmp"
  mv "$receipt.tmp" "$receipt"
}
main=$(printf '%s\n' "$prompt" | sed -n 's/.*main is now \([0-9a-f]*\) .*/\1/p' | head -n 1)
if [ -n "$main" ]; then
  # A resolution request (a resume, a landing recheck): rebase onto the
  # main it names, resolve the conflict and rewrite the receipt.
  printf '%s\n' "$prompt" > "$add_dir/resume-request-seen.txt"
  if ! git rebase -q "$main" >/dev/null 2>&1; then
    # Names the run: two resumed runs resolve to different files, so the
    # later one's rebase on the earlier's landing is not empty.
    printf 'resolved by the session for %s\n' "$sid" > e2e.txt
    git add e2e.txt
    GIT_EDITOR=true git rebase --continue >/dev/null
  fi
  write_receipt 'test -f seed.txt exited 0 after the rebase' 'resolved e2e.txt'
  end_turn "resolved e2e.txt on $main"
fi
case "$prompt" in
  'answer to ask '*)
    # A person's answer, the prompt of the turn after the ask: commit it
    # with the task's change below.
    printf '%s\n' "$prompt" | sed -n 1p > answer.txt
    git add answer.txt
    ;;
  *E2E-ASK*)
    # A question: register it as a worker_question ask and end the turn;
    # the answer is the prompt of the next one.
    # The worker's dagq goes to the queue service (goal 82's stage (3)).
    "$add_dir/runner" ask --run "$sid" --kind worker_question \
      --because scope --topic acceptance_conflict --question 'Which word goes into answer.txt?' > "$add_dir/ask.json"
    # The ask notifies nobody: the inbox's watch tells the person of it.
    grep -q '"notified"' "$add_dir/ask.json" && { printf 'stub: ask reported a notification\n' >&2; exit 66; }
    grep -Eq '"created": *true' "$add_dir/ask.json" || { printf 'stub: ask opened no new ask\n' >&2; exit 66; }
    end_turn 'asked which word goes into answer.txt'
    ;;
esac
case "$prompt" in
  *E2E-HOLD*)
    # Work until the test lets go: a supervisor handoff happens meanwhile.
    # The wait ends with the test's directory or the wrapper (task 1580).
    await_file "$add_dir/go"
    ;;
esac
case "$prompt" in
  *E2E-MIGRATION*)
    # A migration whose number main already has.
    mkdir -p migrations
    printf 'CREATE TABLE e2e (id INTEGER);\n' > migrations/0001_e2e.sql
    git add migrations/0001_e2e.sql
    ;;
esac
printf 'written by the stub agent for %s\n' "$sid" > e2e.txt
git add e2e.txt
git commit -q -m 'feat: e2e stub change'
write_receipt 'test -f seed.txt exited 0' 'added e2e.txt'
# While a test watches the pass (`supervise_once`), the turn stays until
# the test has seen its background wrapper run (every task's at once), not
# for a fixed time a loaded host may outlast (task 641).
shared=${E2E_SHARED:-/nonexistent}
while [ -f "$shared/watching" ] && [ ! -f "$shared/listed" ]; do sleep 0.2; done
end_turn 'committed e2e.txt'
"#
);
