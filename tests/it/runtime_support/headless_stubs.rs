//! The stub headless `claude` and `codex` of the runtime tests' headless
//! turns (ADR-t813-1), and the watchdog that ends a turn with its test
//! (task 1580).
use super::*;

/// The first lines of a stub headless `claude` or `codex` after its
/// arguments, as [`watchdog`] is a [`StubSpawner`] stub's: a turn whose test
/// process is gone (it ended on a timeout's `process::exit`, or was killed,
/// while the turn ran), or whose stub's directory (the test's) is, kills
/// its own process group, the one [`StubSpawner`] or a wrapper made, with
/// the turn script's waits, loops and `sleep`s; without it such turns
/// outlived the tests by hours (task 1580). It watches the test process
/// ([`STUB_TEST_PID`]), not its parent: a test may kill the background
/// wrapper that started the turn and check that the runtime stops the turn
/// it left. It leaves the run's worktree and streams, so that nothing of
/// the run reads it as one of the run's processes or holds its output open.
pub const HEADLESS_WATCHDOG: &str = r#"( cd / && while kill -0 $$ 2>/dev/null; do { kill -0 "$STUB_TEST_PID" 2>/dev/null && [ -d "${0%/*}" ]; } || { kill -s KILL -- -$$ 2>/dev/null || kill -s KILL $$; exit 0; }; sleep 0.2; done ) </dev/null >/dev/null 2>&1 &"#;

/// The variable of a headless stub's sidecar that names the test process
/// that made it, for [`HEADLESS_WATCHDOG`] and the stub's
/// [`common::await_file`] (which watches it instead of the parent).
pub const STUB_TEST_PID: &str = "STUB_TEST_PID";

/// The sidecar values of a headless stub on `db` made by the process
/// `test_pid`.
fn stub_env(db: &Path, test_pid: u32) -> [(&str, String); 2] {
    [
        ("STUB_DB", db.to_str().unwrap().to_owned()),
        (STUB_TEST_PID, test_pid.to_string()),
    ]
}

/// Write the stub `path` with `script` and the sidecar of [`stub_env`].
fn stub_script(path: &Path, script: String, db: &Path, test_pid: u32) {
    let env = stub_env(db, test_pid);
    let env: Vec<(&str, &str)> = env.iter().map(|(k, v)| (*k, v.as_str())).collect();
    crate::common::template::script_env(path, script, &env);
}

/// A stub `claude` for headless turns (ADR-t813-1), in `dir`: it takes
/// `claude -p`'s arguments and first prints `system/init` in stream-json
/// with the permission mode it was given (or `$PERMISSION_SAID`), before
/// it forks, so the shell's start and the log writes below fall on either
/// side of an output line instead of in one silence of a short
/// `turn_silence_secs` before the turn's output. Then it appends
/// `<start|resume> <session> <prompt's first line>` to `stub-calls.log` in
/// the run directory (`--add-dir`) and its arguments to `stub-args.log`,
/// sources `turn.sh` next to it (see [`set_turns`]) and prints a result
/// unless the turn did; a turn stopped at its `system/init` (a permission
/// mode mismatch) may end before the logs. `$TURN` is the turn's number
/// in the run, `$PROMPT` its prompt, `$MODE` `start` or `resume`,
/// `$SESSION` the session id. The turn's helpers: `say TEXT`,
/// `result [TEXT]` (with `$DENIALS` as its `permission_denials` and
/// `$COST`, 0.01 unless the turn sets it, as its `total_cost_usd`, and
/// `$EXTRA`, fields the turn may add, such as a `modelUsage`),
/// `denied` (three refusals), `fail TEXT` (an error result, exit 1),
/// `commit MESSAGE`, `receipt COMMIT [RESULT] [EVIDENCE]`, `ask QUESTION`
/// (its notification goes to `true`, not the host's cmux: a real `cmux
/// notify` that is slow under load would be a quiet process of the turn
/// for the `idle_process` watch).
pub fn headless_claude(dir: &Path, db: &Path) -> PathBuf {
    headless_claude_of(dir, db, std::process::id())
}

/// [`headless_claude`] whose turns end with the process `test_pid` rather
/// than with the test's (task 1580).
pub fn headless_claude_of(dir: &Path, db: &Path, test_pid: u32) -> PathBuf {
    let stub = dir.join("claude-headless");
    let script = format!(
        r#"#!/bin/sh
MODE=; SESSION=; RUN_DIR=; PERMISSION=; PROMPT=
ARGS="$*"
while [ $# -gt 0 ]; do
  case "$1" in
    --session-id) MODE=start; SESSION=$2; shift 2 ;;
    --resume) MODE=resume; SESSION=$2; shift 2 ;;
    --add-dir) RUN_DIR=$2; shift 2 ;;
    --permission-mode) PERMISSION=$2; shift 2 ;;
    --output-format|--debug-file|--settings|--model|--effort) shift 2 ;;
    --) PROMPT=$2; shift 2 ;;
    *) shift ;;
  esac
done
printf '{{"type":"system","subtype":"init","session_id":"%s","model":"stub","permissionMode":"%s"}}\n' "$SESSION" "${{PERMISSION_SAID:-$PERMISSION}}"
{watchdog}
{await_file}
DAGQ={dagq}
DB={db}
RECEIPT="$RUN_DIR/receipt.json"
printf '%s %s %s\n' "$MODE" "$SESSION" "$(printf '%s\n' "$PROMPT" | head -n 1 | cut -c1-80)" >> "$RUN_DIR/stub-calls.log"
printf '%s\n' "$ARGS" | head -n 1 >> "$RUN_DIR/stub-args.log"
TURN=$(wc -l < "$RUN_DIR/stub-calls.log" | tr -d ' ')
DENIALS=
RESULTED=
COST=0.01
EXTRA=
say() {{ printf '{{"type":"assistant","message":{{"model":"stub","content":[{{"type":"text","text":"%s"}}]}}}}\n' "$1"; }}
result() {{
  printf '{{"type":"result","subtype":"success","is_error":false,"num_turns":2,"duration_ms":5,"total_cost_usd":%s,"session_id":"%s","result":"%s","usage":{{"input_tokens":7,"output_tokens":3}},"permission_denials":[%s]%s}}\n' "$COST" "$SESSION" "${{1:-done}}" "$DENIALS" "$EXTRA"
  RESULTED=1
}}
denied() {{ DENIALS='{{"tool_name":"Bash","tool_use_id":"t1","tool_input":{{}}}},{{"tool_name":"Bash","tool_use_id":"t2","tool_input":{{}}}},{{"tool_name":"Edit","tool_use_id":"t3","tool_input":{{}}}}'; }}
fail() {{
  printf '{{"type":"result","subtype":"success","is_error":true,"api_error_status":null,"session_id":"%s","result":"%s"}}\n' "$SESSION" "$1"
  exit 1
}}
commit() {{ printf 'change by %s turn %s\n' "$SESSION" "$TURN" >> change.txt && git add change.txt && git commit -q -m "$1"; }}
receipt() {{
  printf '{{"run_id":"%s","result":"%s","commit":"%s","tests":{{"status":"passed","evidence_or_reason":"ran"}},"e2e":{{"status":"%s","evidence_or_reason":"stub e2e"}},"subagent_review":{{"status":"passed","evidence_or_reason":"reviewed"}},"summary":"turn %s"}}' "${{DAGQ_RUN_ID:-$SESSION}}" "${{2:-succeeded}}" "$1" "${{3:-not_applicable}}" "$TURN" > "$RECEIPT.tmp"
  mv "$RECEIPT.tmp" "$RECEIPT"
}}
ask() {{ "$DAGQ" ask --run "${{DAGQ_RUN_ID:-$SESSION}}" --kind worker_question --because scope --topic acceptance_conflict --question "$1" --cmux true >/dev/null; }}
. {turns}
[ -n "$RESULTED" ] || result
"#,
        dagq = shell_join(&[env!("CARGO_BIN_EXE_dagq").to_owned()]),
        db = "\"$STUB_DB\"",
        turns = "\"${0%/*}/turn.sh\"",
        watchdog = HEADLESS_WATCHDOG,
        await_file = common::AWAIT_FILE,
    );
    stub_script(&stub, script, db, test_pid);

    set_turns(dir, "say working");
    stub
}

/// A stub `codex` for headless turns (ADR-t813-1, ADR-t813-3), in `dir`: it
/// takes `codex exec --json -C <worktree> -c … -- <prompt>` and `codex exec
/// resume --json -c … -- <thread> <prompt>`, finds the run directory among
/// the `-c` writable roots, appends `<start|resume> <thread> <prompt's
/// first line>` to `stub-calls.log` there and the arguments of the call
/// (one line each) to `stub-args.log`, prints `thread.started` (a new
/// thread `codex-thread-<turn>` when it starts, the one it resumes
/// otherwise) and `turn.started`, then sources `turn.sh` next to it (see
/// [`set_turns`]) and prints `turn.completed` unless the turn ended. `$TURN`
/// is the turn's number in the run, `$PROMPT` its prompt, `$MODE` `start`
/// or `resume`, `$THREAD` the thread. The turn's helpers: `say TEXT`,
/// `result` (`turn.completed` with its usage), `error TEXT` (an `error`
/// event), `fail TEXT` (`turn.failed`, exit 1), `refused COMMAND` (a
/// command the sandbox refused), `commit MESSAGE`, `receipt COMMIT
/// [RESULT] [EVIDENCE]`, `ask QUESTION`. A read-only job (the run's
/// review or a recovery job: its job permission profile, or `--sandbox
/// read-only`) logs its arguments and actor and prints the reply of its
/// call (`codex-review-<call>.jsonl`, else a review's `pass`) instead,
/// and writes the model of `codex-review-model` to its thread's rollout
/// when there is one; with `codex-review-failure.jsonl` it prints that
/// and fails, and with `codex-review-hang` it runs a command in a process
/// group of its own until it is stopped (or the test process or the
/// stub's directory is gone). A job sources `codex-review-hook.sh` when
/// there is one. Each turn and job logs its `RUSTC_WRAPPER` and
/// `SCCACHE_ERROR_LOG` (`stub-wrapper.log` and `stub-error-log.log` in the
/// run directory, `codex-review-wrapper.log` and
/// `codex-review-error-log.log` beside the stub).
pub fn headless_codex(dir: &Path, db: &Path) -> PathBuf {
    let test_pid = std::process::id();
    let stub = dir.join("codex-headless");
    let script = format!(
        r#"#!/bin/sh
MODE=start; THREAD=; PROMPT=; ROOTS=; REVIEW=
ARGS=
for arg in "$@"; do ARGS="$ARGS $arg|"; done
[ "$1" = --version ] && {{ echo "codex-cli 0.46.0"; exit 0; }}
[ "$1" = exec ] || {{ echo "not exec: $*" >&2; exit 2; }}
shift
if [ "$1" = resume ]; then MODE=resume; shift; fi
while [ $# -gt 0 ]; do
  case "$1" in
    -c) case "$2" in sandbox_workspace_write.writable_roots=*) ROOTS=$2 ;; default_permissions=*) REVIEW=1 ;; esac; shift 2 ;;
    --sandbox) [ "$2" = read-only ] && REVIEW=1; shift 2 ;;
    -C|-m) shift 2 ;;
    --) shift; break ;;
    *) shift ;;
  esac
done
if [ "$MODE" = resume ]; then THREAD=$1; PROMPT=$2; else PROMPT=$1; fi
if [ -n "$REVIEW" ]; then
  printf '%s\n' "$ARGS" >> {dir}/codex-review-args.log
  printf '%s %s\n' "$DAGQ_ROLE" "$DAGQ_ACTOR_ID" >> {dir}/codex-review-actors.log
  printf '%s\n' "${{RUSTC_WRAPPER-unset}}" >> {dir}/codex-review-wrapper.log
  printf '%s\n' "${{SCCACHE_ERROR_LOG-unset}}" >> {dir}/codex-review-error-log.log
  [ -f {dir}/codex-review-hook.sh ] && . {dir}/codex-review-hook.sh
  REVIEW_CALL=$(wc -l < {dir}/codex-review-actors.log | tr -d ' ')
  printf '{{"type":"thread.started","thread_id":"codex-review-thread"}}\n{{"type":"turn.started"}}\n'
  # The review's model goes to its thread's rollout only, as Codex writes it.
  if [ -f {review_model} ]; then
    SESSIONS={home}/sessions/$(date +%Y/%m/%d)
    mkdir -p "$SESSIONS"
    printf '{{"timestamp":"%s","type":"turn_context","payload":{{"model":"%s","effort":"high"}}}}\n' "$(date -u +%Y-%m-%dT%H:%M:%S.000Z)" "$(cat {review_model})" >> "$SESSIONS/rollout-2026-09-29T00-00-00-codex-review-thread.jsonl"
    # Its turn and the tokens of its one response, written twice.
    NOW=$(date -u +%Y-%m-%dT%H:%M:%S.000Z)
    printf '{{"timestamp":"%s","type":"event_msg","payload":{{"type":"task_started","turn_id":"review-turn-%s"}}}}\n' "$NOW" "$REVIEW_CALL" >> "$SESSIONS/rollout-2026-09-29T00-00-00-codex-review-thread.jsonl"
    for _ in 1 2; do
      printf '{{"timestamp":"%s","type":"token_usage_record","payload":{{"thread_id":"codex-review-thread","session_id":"codex-review-thread","turn_id":"review-turn-%s","root_turn_id":"review-turn-%s","response_id":"resp-%s","usage":{{"input_tokens":13,"cached_input_tokens":3,"output_tokens":4}}}}}}\n' "$NOW" "$REVIEW_CALL" "$REVIEW_CALL" "$REVIEW_CALL" >> "$SESSIONS/rollout-2026-09-29T00-00-00-codex-review-thread.jsonl"
    done
    printf '{{"timestamp":"%s","type":"event_msg","payload":{{"type":"token_count","info":{{"last_token_usage":{{"input_tokens":13}},"model_context_window":258400}}}}}}\n' "$NOW" >> "$SESSIONS/rollout-2026-09-29T00-00-00-codex-review-thread.jsonl"
  fi
  if [ -f {dir}/codex-review-failure.jsonl ]; then
    cat {dir}/codex-review-failure.jsonl
    exit 1
  fi
  # A job that runs a command in a process group of its own until it is
  # stopped (its pid in `codex-review-child.pid`). The command ends by
  # itself once the test process or the stub's directory is gone, or after
  # two minutes.
  if [ -f {dir}/codex-review-hang ]; then
    perl -e 'setpgrp(0,0) or die "setpgrp: $!"; open(my $f, ">", $ARGV[0]) or die $!; print $f "$$\n"; close($f) or die $!; for (1..600) {{ last unless kill(0, $ARGV[2]) && -d $ARGV[1]; select(undef, undef, undef, 0.2) }}' {dir}/codex-review-child.pid {dir} "$STUB_TEST_PID" &
    wait
    exit 0
  fi
  if [ -f {dir}/codex-review-$REVIEW_CALL.jsonl ]; then
    cat {dir}/codex-review-$REVIEW_CALL.jsonl
  else
    printf '{{"type":"item.completed","item":{{"id":"review","type":"agent_message","text":"{{\\"verdict\\":\\"pass\\",\\"reasons\\":[],\\"summary\\":\\"codex passed\\"}}"}}}}\n'
  fi
  printf '{{"type":"turn.completed","usage":{{"input_tokens":10,"output_tokens":2}}}}\n'
  exit 0
fi
RUN_DIR=$(printf '%s' "${{ROOTS#*=}}" | tr -d '[]' | tr ',' '
' | sed -n 5p | tr -d '"')
{watchdog}
{await_file}
DAGQ={dagq}
DB={db}
RECEIPT="$RUN_DIR/receipt.json"
TURN=$(( $(cat "$RUN_DIR/stub-calls.log" 2>/dev/null | wc -l) + 1 ))
[ -n "$THREAD" ] || THREAD="codex-thread-$TURN"
printf '%s %s %s
' "$MODE" "$THREAD" "$(printf '%s
' "$PROMPT" | head -n 1 | cut -c1-80)" >> "$RUN_DIR/stub-calls.log"
printf '%s
' "$ARGS" >> "$RUN_DIR/stub-args.log"
printf '%s
' "${{RUSTC_WRAPPER-unset}}" >> "$RUN_DIR/stub-wrapper.log"
printf '%s
' "${{SCCACHE_ERROR_LOG-unset}}" >> "$RUN_DIR/stub-error-log.log"
ENDED=
say() {{ printf '{{"type":"item.completed","item":{{"id":"m%s","type":"agent_message","text":"%s"}}}}
' "$TURN" "$1"; }}
result() {{
  printf '{{"type":"turn.completed","usage":{{"input_tokens":%s,"cached_input_tokens":%s,"output_tokens":%s,"reasoning_output_tokens":%s}}}}
' $((11 * TURN)) $((4 * TURN)) $((5 * TURN)) $((2 * TURN))
  ENDED=1
}}
error() {{ printf '{{"type":"error","message":"%s"}}
' "$1"; }}
fail() {{
  printf '{{"type":"error","message":"%s"}}
{{"type":"turn.failed","error":{{"message":"%s"}}}}
' "$1" "$1"
  exit 1
}}
refused() {{ printf '{{"type":"item.completed","item":{{"id":"c%s","type":"command_execution","command":"%s","exit_code":1,"aggregated_output":"%s: Operation not permitted","status":"failed"}}}}
' "$TURN" "$1" "$1"; }}
commit() {{ printf 'change by %s turn %s
' "$THREAD" "$TURN" >> change.txt && git add change.txt && git commit -q -m "$1"; }}
receipt() {{
  printf '{{"run_id":"%s","result":"%s","commit":"%s","tests":{{"status":"passed","evidence_or_reason":"ran"}},"e2e":{{"status":"%s","evidence_or_reason":"stub e2e"}},"subagent_review":{{"status":"passed","evidence_or_reason":"reviewed"}},"summary":"turn %s"}}' "$DAGQ_RUN_ID" "${{2:-succeeded}}" "$1" "${{3:-not_applicable}}" "$TURN" > "$RECEIPT.tmp"
  mv "$RECEIPT.tmp" "$RECEIPT"
}}
ask() {{ "$DAGQ" ask --run "$DAGQ_RUN_ID" --kind worker_question --because scope --topic acceptance_conflict --question "$1" --cmux true >/dev/null; }}
echo "Reading additional input from stdin..." >&2
printf '{{"type":"thread.started","thread_id":"%s"}}
{{"type":"turn.started"}}
' "$THREAD"
# The model goes to the thread's rollout only, as Codex writes it.
if [ -f {model} ]; then
  SESSIONS={home}/sessions
  ROLLOUT=$(ls "$SESSIONS"/*/*/*/rollout-*-"$THREAD".jsonl 2>/dev/null | head -n 1)
  if [ -z "$ROLLOUT" ]; then
    mkdir -p "$SESSIONS/$(date +%Y/%m/%d)"
    ROLLOUT="$SESSIONS/$(date +%Y/%m/%d)/rollout-2026-09-29T00-00-00-$THREAD.jsonl"
    printf '{{"type":"session_meta","payload":{{"id":"%s"}}}}
' "$THREAD" > "$ROLLOUT"
  fi
  printf '{{"timestamp":"%s","type":"turn_context","payload":{{"model":"%s","effort":"medium"}}}}
' "$(date -u +%Y-%m-%dT%H:%M:%S.000Z)" "$(cat {model})" >> "$ROLLOUT"
  # A newer Codex's rollout: the turn, and the tokens of its one response
  # (what the stub's thread total grows by), written twice.
  if [ -f {usage_records} ]; then
    NOW=$(date -u +%Y-%m-%dT%H:%M:%S.000Z)
    printf '{{"timestamp":"%s","type":"event_msg","payload":{{"type":"task_started","turn_id":"turn-%s"}}}}
' "$NOW" "$TURN" >> "$ROLLOUT"
    for _ in 1 2; do
      printf '{{"timestamp":"%s","type":"token_usage_record","payload":{{"thread_id":"%s","session_id":"%s","turn_id":"turn-%s","root_turn_id":"turn-%s","response_id":"resp-%s","usage":{{"input_tokens":11,"cached_input_tokens":4,"output_tokens":5,"reasoning_output_tokens":2}}}}}}
' "$NOW" "$THREAD" "$THREAD" "$TURN" "$TURN" "$TURN" >> "$ROLLOUT"
    done
    # The context of the turn: a call as large as 1000 per turn so far,
    # and one compaction of its own.
    printf '{{"timestamp":"%s","type":"event_msg","payload":{{"type":"token_count","info":{{"last_token_usage":{{"input_tokens":%s}},"model_context_window":258400}}}}}}
{{"timestamp":"%s","type":"compacted","payload":{{"message":""}}}}
' "$NOW" "$((TURN * 1000))" "$NOW" >> "$ROLLOUT"
  fi
fi
. {turns}
[ -n "$ENDED" ] || result
"#,
        dagq = shell_join(&[env!("CARGO_BIN_EXE_dagq").to_owned()]),
        db = "\"$STUB_DB\"",
        turns = "\"${0%/*}/turn.sh\"",
        model = "\"${0%/*}/codex-model\"",
        usage_records = "\"${0%/*}/codex-usage-records\"",
        review_model = "\"${0%/*}/codex-review-model\"",
        home = "\"${0%/*}/codex-home\"",
        dir = "\"${0%/*}\"",
        watchdog = HEADLESS_WATCHDOG,
        await_file = common::AWAIT_FILE,
    );
    stub_script(&stub, script, db, test_pid);

    set_turns(dir, "say working");
    stub
}
