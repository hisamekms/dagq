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
if [ -n "$headless" ] && [ "$output" != stream-json ]; then
  # The supervisor's headless review (ADR-0027): read review.md, print the
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
# --verbose`, the session started by `--session-id` (or resumed), in the
# run's permission mode (`dontAsk` for a run of `[broker] mode =
# "required"`, ADR-t838-1) with settings without a Stop hook. The turn's prompt is the
# task's, a person's answer (`answer to ask <id>: ...`) or a resolution
# request; it does what the prompt asks, commits, writes the receipt and
# prints the stream: init, a message, the result.
[ "$output" = stream-json ] || { printf 'stub: not a worker turn\n' >&2; exit 64; }
sid=${session_id:-$resume}
[ -n "$headless" ] && [ -n "$sid" ] && { [ "$permission" = auto ] || [ "$permission" = dontAsk ]; } \
  && [ -n "$debug_file" ] && [ -n "$add_dir" ] && [ -n "$settings" ] && [ -n "$prompt" ] \
  || { printf 'stub: bad headless turn arguments\n' >&2; exit 64; }
! grep -q '"Stop"' "$settings" || { printf 'stub: headless settings have a hook\n' >&2; exit 64; }
grep -q 'Bash(pkill:\*)' "$settings" || { printf 'stub: headless settings deny no pkill\n' >&2; exit 64; }
required=
if [ "$permission" = dontAsk ]; then
  # A `required` run's turn (ADR-t838-1): the broker's server and no
  # other, no settings file but the run's, the built-in file tools denied
  # and only the broker's tools and `dagq` allowed, nothing allowed by
  # `--allowedTools`.
  [ -n "$mcp" ] && [ -n "$strict" ] && [ -z "$sources" ] && [ -z "$tools" ] \
    || { printf 'stub: bad required turn arguments\n' >&2; exit 64; }
  deny=$(sed -n '/"deny": \[/,/\]/p' "$settings")
  allow=$(sed -n '/"allow": \[/,/\]/p' "$settings")
  for tool in Read Edit Write MultiEdit NotebookEdit Glob Grep LS; do
    printf '%s\n' "$deny" | grep -q "\"$tool\"" || { printf 'stub: required settings do not deny %s\n' "$tool" >&2; exit 64; }
  done
  printf '%s\n' "$allow" | grep -q '"mcp__dagq-broker"' && printf '%s\n' "$allow" | grep -q '"Bash(dagq:\*)"' \
    || { printf 'stub: required settings do not allow the broker and dagq\n' >&2; exit 64; }
  required=1
fi
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
case "$prompt" in
  *E2E-BROKER*)
    # Without the tools the plain path below would commit e2e.txt alone and
    # the run would fail only at the landing's verification (task 1255).
    [ -n "$mcp" ] || { printf 'stub: the task asks for the broker, but the worker was given no broker tools (see the run'"'"'s broker_unavailable)\n' >&2; exit 66; }
    ;;
esac
if [ -n "$required" ]; then
  # The broker's tools are the worker's only way to the worktree
  # (`[broker] mode = "required"`, ADR-t838-1): every step goes through
  # the client's MCP server, one `tools/call` per process as Claude Code
  # would make it, with the command and env its configuration names, and
  # the receipt is written by `write_receipt`. Nothing here touches the
  # worktree or git itself.
  client=$(sed -n 's/^ *"command": "\(.*\)",$/\1/p' "$mcp")
  url=$(sed -n 's/^ *"DAGQ_BROKER_URL": "\(.*\)",*$/\1/p' "$mcp")
  token_file=$(sed -n 's/^ *"DAGQ_BROKER_TOKEN_FILE": "\(.*\)",*$/\1/p' "$mcp")
  receipt_file=$(sed -n 's/^ *"DAGQ_RECEIPT_FILE": "\(.*\)",*$/\1/p' "$mcp")
  [ "$receipt_file" = "$receipt" ] || { printf 'stub: the configuration names receipt %s, the prompt %s\n' "$receipt_file" "$receipt" >&2; exit 64; }
  printf 'required turn: --permission-mode %s --mcp-config %s --strict-mcp-config --setting-sources "%s"\n' "$permission" "$mcp" "$sources" > "$add_dir/broker-required-turn.txt"
  # The tool's answer in `$answer`; a tool error ends the turn.
  call() {
    answer=$(printf '{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"%s","arguments":%s}}\n' "$1" "$2" \
      | DAGQ_BROKER_URL=$url DAGQ_BROKER_TOKEN_FILE=$token_file DAGQ_RECEIPT_FILE=$receipt_file "$client" mcp 2>> "$add_dir/broker.err")
    printf '%s %s\n' "$1" "$answer" >> "$add_dir/broker-mcp.jsonl"
    case "$answer" in
      *'"isError":false'*) ;;
      *) printf 'stub: the broker tool %s failed: %s\n' "$1" "$answer" >&2; exit 66 ;;
    esac
  }
  call read_file '{"path":"seed.txt"}'
  case "$answer" in *fixture*) ;; *) printf 'stub: read_file did not read seed.txt\n' >&2; exit 66 ;; esac
  call write_file "{\"path\":\"e2e.txt\",\"content\":\"written through the required broker for $sid\"}"
  call exec '{"argv":["sh","-c","test -f seed.txt && printf \"run by the broker\\n\" > exec.txt"]}'
  case "$answer" in *'exit_code\":0'*) ;; *) printf 'stub: the broker exec failed\n' >&2; exit 66 ;; esac
  call package_install '{"name":"e2e-touch"}'
  case "$answer" in *'exit_code\":0'*) ;; *) printf 'stub: the broker package install failed\n' >&2; exit 66 ;; esac
  call git_add '{"paths":["e2e.txt","exec.txt","package.txt"]}'
  call git_commit '{"message":"feat: e2e stub change through the required broker"}'
  commit=$(printf '%s\n' "$answer" | sed -n 's/.*commit\\":\\"\([0-9a-f]*\)\\".*/\1/p')
  [ -n "$commit" ] || { printf 'stub: git_commit named no commit\n' >&2; exit 66; }
  call write_receipt "{\"receipt\":{\"run_id\":\"$sid\",\"result\":\"succeeded\",\"commit\":\"$commit\",\"tests\":{\"status\":\"passed\",\"evidence_or_reason\":\"test -f seed.txt exited 0 through the broker\"},\"e2e\":{\"status\":\"not_applicable\",\"evidence_or_reason\":\"stub agent\"},\"subagent_review\":{\"status\":\"not_applicable\",\"evidence_or_reason\":\"stub agent\"},\"summary\":\"added e2e.txt, exec.txt and package.txt through the broker\"}}"
  [ -f "$receipt" ] || { printf 'stub: write_receipt wrote no receipt\n' >&2; exit 66; }
elif [ -n "$mcp" ]; then
  # The resource broker's tools (`[broker] mode = "preferred"`): the stub
  # drives the client of its MCP configuration from the shell, with the URL
  # and the token file the configuration names, as its MCP server would.
  [ "$tools" = mcp__dagq-broker ] || { printf 'stub: the broker tools are not allowed\n' >&2; exit 64; }
  client=$(sed -n 's/^ *"command": "\(.*\)",$/\1/p' "$mcp")
  DAGQ_BROKER_URL=$(sed -n 's/^ *"DAGQ_BROKER_URL": "\(.*\)",*$/\1/p' "$mcp")
  DAGQ_BROKER_TOKEN_FILE=$(sed -n 's/^ *"DAGQ_BROKER_TOKEN_FILE": "\(.*\)",*$/\1/p' "$mcp")
  export DAGQ_BROKER_URL DAGQ_BROKER_TOKEN_FILE
  "$client" fs write e2e.txt --content "written through the broker for $sid" > "$add_dir/broker-fs.json" 2>> "$add_dir/broker.err"
  "$client" exec -- sh -c 'printf "run by the broker\n" > exec.txt' > "$add_dir/broker-exec.json" 2>> "$add_dir/broker.err"
  grep -q '"exit_code":0' "$add_dir/broker-exec.json" || { printf 'stub: the broker exec failed\n' >&2; exit 66; }
  # What the host sees before the broker stages it, for a failure to show.
  git --no-optional-locks status --porcelain --untracked-files=all > "$add_dir/broker-status-before-add.txt" 2>&1 || true
  "$client" git add e2e.txt exec.txt > "$add_dir/broker-add.json" 2>> "$add_dir/broker.err" \
    || { printf 'stub: the broker git add failed\n' >&2; exit 66; }
  "$client" git commit --message 'feat: e2e stub change through the broker' > "$add_dir/broker-commit.json" 2>> "$add_dir/broker.err" \
    || { printf 'stub: the broker git commit failed\n' >&2; exit 66; }
else
  printf 'written by the stub agent for %s\n' "$sid" > e2e.txt
  git add e2e.txt
  git commit -q -m 'feat: e2e stub change'
fi
[ -n "$required" ] || write_receipt 'test -f seed.txt exited 0' 'added e2e.txt'
# While a test watches the pass (`supervise_once`), the turn stays until
# the test has seen its background wrapper run (every task's at once), not
# for a fixed time a loaded host may outlast (task 641).
shared=${E2E_SHARED:-/nonexistent}
while [ -f "$shared/watching" ] && [ ! -f "$shared/listed" ]; do sleep 0.2; done
end_turn 'committed e2e.txt'
"#
);
