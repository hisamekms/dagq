//! End-to-end happy paths through the real binary, real Git, real cmux and
//! real launchd, from `add` to the squash landing by `integrate`, from
//! `up` to `down`, and from a killed supervisor to the adoption of its run.
//! Claude is replaced by a stub script that does what the prompt asks:
//! change, commit, write the receipt; resumed with `--resume` it reads the
//! supervisor's resolution request from its terminal and resolves the
//! conflict. Requires a running cmux, so it is ignored by
//! default: `cargo test --locked --test e2e -- --ignored --nocapture`.
//!
//! The launchd `up` / `down` test is temporarily off even under `--ignored`:
//! no project runs the launchd mode now, and without a cmux socket password
//! its preflight always stops `up`. It returns at once, printing why, unless
//! `DAGQ_E2E_LAUNCHD=1` is set; the in-cmux `up` / `down` test always runs.
#[path = "e2e/cleanup.rs"]
mod cleanup;
mod common;
#[path = "e2e/headless.rs"]
mod headless;

use cleanup::{
    GroupGuard, WorkspaceGuard, all_workspaces, claim_fixture_dir, cmux_retrying, listed_group,
    listed_workspace, sweep_abandoned_fixtures, wait_until_not_listed, workspace_listed,
};
use common::{Bounded, Cleanup, Waiting, WithoutActor};
use serde_json::{Value, json};
use std::{
    env, fs,
    io::Read,
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    thread,
    time::{Duration, Instant},
};

const BIN: &str = env!("CARGO_BIN_EXE_dagq");
/// The version the binary under test records on its registration.
const VERSION: &str = dagq::VERSION;
/// Longer than the supervisor's own 120 s exit-request timeout so its error
/// surfaces first.
const SUPERVISE_TIMEOUT: Duration = Duration::from_secs(180);
/// Passed to every `supervise` the tests start: the load of the host (other
/// e2e tests running in parallel, other runs) must not hold back the claims
/// a test waits for.
const NO_LOAD_HOLD: [&str; 2] = ["--max-load", "0"];
/// A whole e2e test, which the fixture holds: well above the longest test's
/// own deadlines (a few supervise passes of [`SUPERVISE_TIMEOUT`] each), so
/// those fail first with their own message. The waits inside a test are
/// timed with [`common::STEP_LIMIT`].
const TEST_LIMIT: Duration = Duration::from_secs(1800);
/// How long the timeout cleanup of a fixture's cmux group or of a supervisor
/// the test started may take before the test binary exits without it.
const CLEANUP_LIMIT: Duration = Duration::from_secs(60);
/// How long a test polls for a state it waits to reach (a status, a
/// planner's state, a pin in cmux's listing, a process's exit, a landing):
/// one value for every such wait, long enough for a loaded host (load avg
/// 10 and more, task 641), so a wait fails only when the state never comes.
/// The poll ends as soon as the state holds.
pub(crate) const WAIT_LIMIT: Duration = common::STEP_LIMIT;

/// Stand-in for Claude Code. It accepts the argv the Claude adapter builds and
/// follows the prompt: work in the cwd worktree, commit, publish the receipt by
/// atomic rename. Then it behaves like an idle interactive session: it writes
/// the idle marker the Stop hook would write and waits for `/exit` on its
/// terminal, which the supervisor types through cmux.
const STUB: &str = r#"#!/bin/sh
set -eu
if [ "${1:-}" = "--version" ]; then
  printf 'claude-stub 0.0.0\n'
  exit 0
fi
if [ "${1:-}" = plugin ] && [ "${2:-}" = list ]; then
  # `up` and `plan` without --plugin-dir check the installed plugin
  # (ADR-t617-2 decision 4).
  printf '[{"id":"claude-dagq@dagq","version":"0.0.0","scope":"user","enabled":true}]\n'
  exit 0
fi
session_id= debug_file= add_dir= settings= prompt= resume= headless= tools= denied= plugin_dir= model= effort= output= permission=
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
    --model) model=$2; shift 2 ;;
    --effort) effort=$2; shift 2 ;;
    --) shift; prompt=$1; shift; break ;;
    *) printf 'stub: unexpected argument %s\n' "$1" >&2; exit 64 ;;
  esac
done
[ $# -eq 0 ] || { printf 'stub: trailing arguments after the prompt\n' >&2; exit 64; }
if [ "${DAGQ_ROLE:-}" = planner ]; then
  # A planner session `plan` opened (ADR-0041 decision 6): register a task,
  # submit it as this planner's proposal, go idle the way the Stop hook
  # marks it, and wait for /exit on the terminal.
  [ -z "$session_id" ] && [ -n "$debug_file" ] && [ -n "$add_dir" ] && [ -n "$settings" ] && [ -n "$prompt" ] \
    || { printf 'stub: bad planner arguments\n' >&2; exit 64; }
  grep -q '"Stop"' "$settings" || { printf 'stub: planner settings lack a Stop hook\n' >&2; exit 64; }
  case "$prompt" in 'You are a planner of the dagq queue at '*) ;; *) printf 'stub: not a planner prompt\n' >&2; exit 65 ;; esac
  {
    printf 'argv: --debug-file %s --add-dir %s --settings %s --plugin-dir %s\n' "$debug_file" "$add_dir" "$settings" "$plugin_dir"
    printf 'cwd: %s\n' "$(pwd)"
    printf 'env: DAGQ_ROLE=%s DAGQ_PLANNER_ORIGIN=%s DAGQ_PLANNER_ID=%s\n' "$DAGQ_ROLE" "${DAGQ_PLANNER_ORIGIN:-}" "${DAGQ_PLANNER_ID:-}"
  } > "$debug_file"
  "$add_dir/runner" --db "$DAGQ_QUEUE" add "planned by planner ${DAGQ_PLANNER_ID:-}" --acceptance 'the e2e planner wrote it' > "$add_dir/added.json"
  task=$(sed -n 's/^  "id": \([0-9]*\),$/\1/p' "$add_dir/added.json")
  [ -n "$task" ] || { printf 'stub: add printed no task id\n' >&2; exit 66; }
  "$add_dir/runner" --db "$DAGQ_QUEUE" submit "$task" > "$add_dir/submitted.json"
  printf 'submitted task %s\n' "$task"
  idle="$add_dir/idle.json"
  printf '{"hook_event_name":"Stop","stop_hook_active":false}\n' > "$idle.tmp"
  mv "$idle.tmp" "$idle"
  printf 'idle; waiting for /exit\n'
  while read -r line; do
    [ "$line" = "/exit" ] && break
  done
  printf 'bye\n'
  exit 0
fi
if [ "$output" = stream-json ]; then
  # A headless worker's turn (ADR-t813-1): `claude -p --output-format
  # stream-json --verbose`, the session started by `--session-id` (or
  # resumed), in the run's permission mode with hook-less settings. It does
  # the task, commits, writes the receipt and prints the stream: init, a
  # message, the result.
  sid=${session_id:-$resume}
  [ -n "$headless" ] && [ -n "$sid" ] && [ "$permission" = auto ] && [ -n "$debug_file" ] && [ -n "$add_dir" ] \
    && [ -n "$settings" ] && [ -n "$prompt" ] && [ -n "$model" ] && [ -n "$effort" ] \
    || { printf 'stub: bad headless turn arguments\n' >&2; exit 64; }
  ! grep -q '"Stop"' "$settings" || { printf 'stub: headless settings have a hook\n' >&2; exit 64; }
  grep -q 'Bash(pkill:\*)' "$settings" || { printf 'stub: headless settings deny no pkill\n' >&2; exit 64; }
  printf 'turn argv: -p --output-format %s --session-id %s --resume %s --permission-mode %s --add-dir %s --settings %s --model %s\n' \
    "$output" "$session_id" "$resume" "$permission" "$add_dir" "$settings" "$model" >> "$debug_file"
  printf '{"type":"system","subtype":"init","session_id":"%s","model":"%s","permissionMode":"%s"}\n' "$sid" "$model" "$permission"
  receipt=$(printf '%s\n' "$prompt" | sed -n 's/^Write a completion receipt to \(.*\) using a temporary file in the same directory.*/\1/p')
  [ -n "$receipt" ] || { printf 'stub: prompt does not name the receipt path\n' >&2; exit 65; }
  printf 'written by the headless stub agent for %s\n' "$sid" > e2e.txt
  git add e2e.txt
  git commit -q -m 'feat: e2e headless stub change'
  sh -c 'test -f seed.txt'
  commit=$(git rev-parse HEAD)
  printf '{"run_id":"%s","result":"succeeded","commit":"%s","tests":{"status":"passed","evidence_or_reason":"test -f seed.txt exited 0"},"e2e":{"status":"not_applicable","evidence_or_reason":"stub agent"},"subagent_review":{"status":"not_applicable","evidence_or_reason":"stub agent"},"summary":"added e2e.txt headless"}\n' \
    "$sid" "$commit" > "$receipt.tmp"
  mv "$receipt.tmp" "$receipt"
  printf '{"type":"assistant","message":{"model":"%s","content":[{"type":"text","text":"committed e2e.txt"}]}}\n' "$model"
  printf '{"type":"result","subtype":"success","is_error":false,"num_turns":3,"duration_ms":40,"total_cost_usd":0.02,"session_id":"%s","result":"done","usage":{"input_tokens":11,"output_tokens":5},"permission_denials":[]}\n' "$sid"
  exit 0
fi
if [ -n "$headless" ]; then
  # The supervisor's headless review (ADR-0027): read review.md, print the
  # verdict JSON on stdout. Only a task that says E2E-REVIEW-PASS passes;
  # any other review fails, and its run waits in an approve_landing ask.
  # The runtime names the review's own session (ADR-0048 decision 4).
  [ -n "$session_id" ] && [ -n "$debug_file" ] && [ -n "$add_dir" ] && [ -n "$settings" ] && [ -n "$prompt" ] \
    && [ "$tools" = "Read,Grep,Glob" ] && [ "$denied" = "Bash,Edit,Write,NotebookEdit" ] || { printf 'stub: bad review arguments\n' >&2; exit 64; }
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
if [ -n "$resume" ]; then
  # A resumed needs_session run: wait for the supervisor's resolution
  # request on the terminal, rebase onto the main it names, resolve the
  # conflict, rewrite the receipt, go idle and wait for /exit. The request
  # is one long line, longer than a canonical-mode tty line may be.
  [ -z "$session_id" ] && [ -z "$prompt" ] && [ -n "$debug_file" ] && [ -n "$add_dir" ] && [ -n "$settings" ] \
    || { printf 'stub: bad resume arguments\n' >&2; exit 64; }
  grep -q '"Stop"' "$settings" || { printf 'stub: settings lack a Stop hook\n' >&2; exit 64; }
  [ -n "$model" ] && [ -n "$effort" ] || { printf 'stub: the resume names no model\n' >&2; exit 64; }
  printf 'argv: --resume %s --debug-file %s --add-dir %s --settings %s --model %s --effort %s\n' "$resume" "$debug_file" "$add_dir" "$settings" "$model" "$effort" > "$debug_file"
  printf 'resumed %s; waiting for the resolution request\n' "$resume"
  # Claude Code's input box: the supervisor types the request only once it
  # is drawn (task 285).
  rule=──────────────────────────────────────────────────
  printf '%s\n\342\235\257 \n%s\n  ? for shortcuts\n' "$rule" "$rule"
  stty -icanon min 1
  IFS= read -r request
  stty icanon
  printf '%s\n' "$request" > "$add_dir/resume-request-seen.txt"
  main=$(printf '%s\n' "$request" | sed -n 's/.*main is now \([0-9a-f]*\) .*/\1/p')
  [ -n "$main" ] || { printf 'stub: the request names no main\n' >&2; exit 65; }
  if ! git rebase -q "$main" >/dev/null 2>&1; then
    printf 'resolved by the session\n' > e2e.txt
    git add e2e.txt
    GIT_EDITOR=true git rebase --continue >/dev/null
  fi
  sh -c 'test -f seed.txt'
  commit=$(git rev-parse HEAD)
  receipt="$add_dir/receipt.json"
  printf '{"run_id":"%s","result":"succeeded","commit":"%s","tests":{"status":"passed","evidence_or_reason":"test -f seed.txt exited 0 after the rebase"},"e2e":{"status":"not_applicable","evidence_or_reason":"stub agent"},"subagent_review":{"status":"not_applicable","evidence_or_reason":"stub agent"},"summary":"resolved e2e.txt"}\n' \
    "$resume" "$commit" > "$receipt.tmp"
  mv "$receipt.tmp" "$receipt"
  printf 'receipt rewritten for %s\n' "$commit"
  idle="$add_dir/idle.json"
  printf '{"hook_event_name":"Stop","session_id":"%s","stop_hook_active":false}\n' "$resume" > "$idle.tmp"
  mv "$idle.tmp" "$idle"
  printf 'idle; waiting for /exit\n'
  while read -r line; do
    [ "$line" = "/exit" ] && break
  done
  printf 'bye\n'
  exit 0
fi
[ -n "$session_id" ] && [ -n "$debug_file" ] && [ -n "$add_dir" ] && [ -n "$settings" ] && [ -n "$prompt" ] \
  && [ -n "$model" ] && [ -n "$effort" ] \
  || { printf 'stub: missing arguments\n' >&2; exit 64; }
grep -q '"Stop"' "$settings" || { printf 'stub: settings lack a Stop hook\n' >&2; exit 64; }
{
  printf 'argv: --session-id %s --debug-file %s --add-dir %s --settings %s\n' "$session_id" "$debug_file" "$add_dir" "$settings"
  printf 'model: %s effort: %s\n' "$model" "$effort"
  printf 'cwd: %s\n' "$(pwd)"
  printf 'env: DAGQ_ROLE=%s DAGQ_QUEUE=%s\n' "${DAGQ_ROLE:-}" "${DAGQ_QUEUE:-}"
  printf 'run env: E2E_SHARED=%s E2E_RUN_DIR=%s\n' "${E2E_SHARED:-}" "${E2E_RUN_DIR:-}"
} > "$debug_file"
run_id=$(printf '%s\n' "$prompt" | sed -n 's/^You are executing dagq task [0-9]*, run \(.*\)\.$/\1/p')
[ "$run_id" = "$session_id" ] || { printf 'stub: prompt run %s != session %s\n' "$run_id" "$session_id" >&2; exit 65; }
receipt=$(printf '%s\n' "$prompt" | sed -n 's/^Write a completion receipt to \(.*\) using a temporary file in the same directory.*/\1/p')
[ -n "$receipt" ] || { printf 'stub: prompt does not name the receipt path\n' >&2; exit 65; }
case "$prompt" in
  *E2E-ASK*)
    # A question: register it as a worker_question ask, go idle, and wait
    # for the supervisor to type the answer into this terminal.
    "$add_dir/runner" --db "$DAGQ_QUEUE" ask --run "$session_id" --kind worker_question \
      --because scope --question 'Which word goes into answer.txt?' > "$add_dir/ask.json"
    # The new ask notified a person through the real cmux.
    grep -Eq '"notified": *true' "$add_dir/ask.json" || { printf 'stub: ask did not notify\n' >&2; exit 66; }
    idle="$add_dir/idle.json"
    printf '{"hook_event_name":"Stop","session_id":"%s","stop_hook_active":false}\n' "$session_id" > "$idle.tmp"
    mv "$idle.tmp" "$idle"
    printf 'asked; waiting for the answer\n'
    answer=
    while IFS= read -r line; do
      case "$line" in 'answer to ask '*) answer=$line; break ;; esac
    done
    [ -n "$answer" ] || { printf 'stub: no answer arrived\n' >&2; exit 66; }
    printf '%s\n' "$answer" > answer.txt
    git add answer.txt
    ;;
esac
case "$prompt" in
  *E2E-HOLD*)
    # Work until the test lets go: a supervisor handoff happens meanwhile.
    printf 'holding until %s/go\n' "$add_dir"
    while [ ! -f "$add_dir/go" ]; do sleep 0.2; done
    ;;
esac
printf 'written by the stub agent for %s\n' "$session_id" > e2e.txt
git add e2e.txt
git commit -q -m 'feat: e2e stub change'
sh -c 'test -f seed.txt'
commit=$(git rev-parse HEAD)
printf '{"run_id":"%s","result":"succeeded","commit":"%s","tests":{"status":"passed","evidence_or_reason":"test -f seed.txt exited 0"},"e2e":{"status":"not_applicable","evidence_or_reason":"stub agent"},"subagent_review":{"status":"not_applicable","evidence_or_reason":"stub agent"},"summary":"added e2e.txt"}\n' \
  "$session_id" "$commit" > "$receipt.tmp"
mv "$receipt.tmp" "$receipt"
printf 'receipt submitted\n'
# While a test watches the pass (`supervise_once`), the session stays until
# the test has seen its workspace listed (every task's at once), not for a
# fixed time a loaded host may outlast (task 641).
shared=${E2E_SHARED:-/nonexistent}
while [ -f "$shared/watching" ] && [ ! -f "$shared/listed" ]; do sleep 0.2; done
idle="$add_dir/idle.json"
printf '{"hook_event_name":"Stop","session_id":"%s","stop_hook_active":false}\n' "$session_id" > "$idle.tmp"
mv "$idle.tmp" "$idle"
printf 'idle; waiting for /exit\n'
while read -r line; do
  [ "$line" = "/exit" ] && break
done
printf 'bye\n'
"#;

const E2E_DAGQ_TOML: &str =
    "[run.env]\nE2E_SHARED = '${DAGQ_QUEUE_DIR}/shared'\nE2E_RUN_DIR = '${DAGQ_RUN_DIR}'\n";

/// A verification command that records the `[run.env]` it ran with in the
/// run directory it names, and fails without it.
const VERIFY_RUN_ENV: &str =
    r#"printf 'verify env: %s\n' "$E2E_SHARED" >> "${E2E_RUN_DIR:?}/verify-env.txt""#;

fn cmux_executable() -> PathBuf {
    env::var_os("DAGQ_E2E_CMUX")
        .map(PathBuf::from)
        .unwrap_or_else(|| "cmux".into())
}

/// Fail loudly, never skip, when cmux is missing: the test would prove nothing.
fn preflight(cmux: &Path) -> String {
    let hint = "the e2e test needs a running cmux; put cmux on PATH or set DAGQ_E2E_CMUX";
    let pong = |output: &std::process::Output| {
        output.status.success() && String::from_utf8_lossy(&output.stdout).trim() == "PONG"
    };
    let ping = cmux_retrying(cmux, &["ping"], pong)
        .unwrap_or_else(|error| panic!("cannot run {}: {error}; {hint}", cmux.display()));
    assert!(
        pong(&ping),
        "cmux ping failed ({}): {}{}; {hint}",
        ping.status,
        String::from_utf8_lossy(&ping.stdout),
        String::from_utf8_lossy(&ping.stderr)
    );
    let version = Command::new(cmux)
        .arg("--version")
        .bounded_output()
        .unwrap();
    String::from_utf8_lossy(&version.stdout).trim().to_owned()
}

fn git(repo: &Path, args: &[&str]) -> String {
    let result = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(args)
        .bounded_output()
        .unwrap();
    assert!(
        result.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&result.stderr)
    );
    String::from_utf8(result.stdout).unwrap().trim().to_owned()
}

/// Record in `home`'s Claude Code config that the folder trust dialog was
/// accepted at `repo`, as `up` requires before it starts anything.
fn trust_repository(home: &Path, repo: &Path) {
    let root = repo.canonicalize().unwrap();
    fs::write(
        home.join(".claude.json"),
        json!({"projects": {root.to_str().unwrap(): {"hasTrustDialogAccepted": true}}}).to_string(),
    )
    .unwrap();
}

/// The queue is resolved the way a user's shell would: from the repository as
/// the working directory, with `XDG_DATA_HOME` pointed at the disposable
/// directory instead of the developer's real data home.
struct Env {
    repo: PathBuf,
    data_home: PathBuf,
}

fn dagq(env: &Env, args: &[&str]) -> Value {
    dagq_with(env, &[], args)
}

/// `dagq status` once `condition` holds of it: what `status` reports of
/// processes that just exited or were just killed is waited for rather than
/// judged on the first look. Past the deadline the last status is returned,
/// for the caller's assertions to show.
fn status_when(env: &Env, condition: impl Fn(&Value) -> bool) -> Value {
    let deadline = Instant::now() + WAIT_LIMIT;
    loop {
        let status = dagq(env, &["status"]);
        if condition(&status) || Instant::now() >= deadline {
            return status;
        }
        thread::sleep(Duration::from_millis(200));
    }
}

fn dagq_with(env: &Env, extra: &[(&str, &Path)], args: &[&str]) -> Value {
    checked(args, dagq_output(env, extra, args))
}

/// [`dagq_with`] for a command that opens workspaces (`up`, `plan`): every
/// workspace its output names goes into `guard` before anything about the
/// output is checked, so a failing command or assertion still has them
/// closed when the test ends.
fn dagq_opening(
    env: &Env,
    extra: &[(&str, &Path)],
    args: &[&str],
    guard: &mut WorkspaceGuard,
) -> Value {
    let output = dagq_output(env, extra, args);
    guard.record_opened(&output.stdout);
    checked(args, output)
}

fn dagq_output(env: &Env, extra: &[(&str, &Path)], args: &[&str]) -> std::process::Output {
    let mut command = Command::new(BIN);
    // A person's commands, not the actor's of the session running the tests.
    command.without_actor_env();
    command
        .current_dir(&env.repo)
        .env("XDG_DATA_HOME", &env.data_home)
        .env_remove("CLAUDE_CONFIG_DIR")
        .args(args);
    for (key, value) in extra {
        command.env(key, value);
    }
    command.bounded_output().unwrap()
}

fn checked(args: &[&str], output: std::process::Output) -> Value {
    assert!(
        output.status.success(),
        "dagq {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).unwrap()
}

/// The workspace is pinned, has the sidebar color `color` (cmux lists a
/// named color by its hex value) and carries the status pill `pill` as
/// `cmux list-status` prints it.
fn assert_look(cmux: &Path, id: &str, color: &str, pill: &str) {
    let listed = listed_workspace(cmux, id).unwrap();
    assert_eq!(listed["pinned"], true, "{listed}");
    assert_eq!(listed["custom_color"], color, "{listed}");
    let status = Command::new(cmux)
        .args(["list-status", "--workspace", id])
        .bounded_output()
        .unwrap();
    assert!(status.status.success(), "{status:?}");
    let status = String::from_utf8_lossy(&status.stdout);
    assert!(status.lines().any(|line| line == pill), "{status}");
}

/// `cmux workspace env <id> --json`: the environment the workspace was
/// created with.
fn workspace_env(cmux: &Path, id: &str) -> Value {
    let output = Command::new(cmux)
        .args(["workspace", "env", id, "--json"])
        .bounded_output()
        .unwrap();
    assert!(
        output.status.success(),
        "cmux workspace env {id}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice::<Value>(&output.stdout).unwrap()["env"].clone()
}

/// Kills a still-running supervisor when an assertion fails mid-run, and
/// when a wait times out and the test binary exits without unwinding.
struct ChildGuard(Child, Option<Cleanup>);

impl ChildGuard {
    fn new(child: Child) -> Self {
        let pid = child.id();
        let cleanup = common::on_timeout(CLEANUP_LIMIT, format!("kill process {pid}"), move || {
            // SAFETY: kill(2) takes no pointers. The hook is unregistered
            // once the test reaps the child or the guard drops.
            unsafe { libc::kill(pid as libc::pid_t, libc::SIGKILL) };
        });
        Self(child, Some(cleanup))
    }

    /// The child was waited for: its pid is no longer ours to kill on a
    /// timeout.
    fn reaped(&mut self) {
        self.1 = None;
    }
}

impl Drop for ChildGuard {
    fn drop(&mut self) {
        if self.0.try_wait().ok().flatten().is_none() {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
}

/// Join a thread the test started, within [`common::STEP_LIMIT`].
fn joined<T>(handle: thread::JoinHandle<T>, what: &str) -> T {
    let _waiting = common::within(common::STEP_LIMIT, format!("{what} to return"));
    handle.join().unwrap()
}

fn reader(mut source: impl Read + Send + 'static) -> thread::JoinHandle<String> {
    thread::spawn(move || {
        let mut text = String::new();
        source.read_to_string(&mut text).unwrap();
        text
    })
}

/// The directory name of every fixture's disposable repository. The runtime
/// names a queue's workspace group and titles after the repository's
/// directory (`[dagq-e2e]`, `[dagq-e2e]worker#…`), so a group an e2e leaves
/// in cmux says where it came from (task 710; the manual smoke's
/// repositories are named in docs/design/manual-smoke.md).
const E2E_REPO_NAME: &str = "dagq-e2e";

/// Disposable repository, queue and stub agent, all outside this repository.
struct Fixture {
    group: GroupGuard,
    _dir: tempfile::TempDir,
    cmux: PathBuf,
    repo: PathBuf,
    stub: PathBuf,
    base: String,
    db: PathBuf,
    env: Env,
    /// Held for the fixture's lifetime, and dropped after `_dir` is gone;
    /// see [`OWNER_FILE`].
    _owner: fs::File,
    _test: Waiting,
}

fn fixture() -> Fixture {
    let test = common::within(TEST_LIMIT, "the test to finish");
    let cmux = cmux_executable();
    let cmux_version = preflight(&cmux);
    eprintln!("cmux: {cmux_version}");
    sweep_abandoned_fixtures(&cmux);
    let dir = tempfile::tempdir().unwrap();
    let owner = claim_fixture_dir(dir.path());
    let repo = dir.path().join(E2E_REPO_NAME);
    fs::create_dir(&repo).unwrap();
    git(&repo, &["init", "-b", "main"]);
    git(&repo, &["config", "user.name", "e2e"]);
    git(&repo, &["config", "user.email", "e2e@example.invalid"]);
    fs::write(repo.join("seed.txt"), "fixture\n").unwrap();
    // ADR-0023 decision 3: every run gets this env in its workspace and
    // its verification commands.
    fs::write(repo.join("dagq.toml"), E2E_DAGQ_TOML).unwrap();
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-q", "-m", "seed"]);
    let base = git(&repo, &["rev-parse", "HEAD"]);
    let stub = dir.path().join("claude-stub");
    fs::write(&stub, STUB).unwrap();
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(&stub, fs::Permissions::from_mode(0o755)).unwrap();
    let env = Env {
        repo: repo.clone(),
        data_home: dir.path().join("data"),
    };
    let init = dagq(&env, &["init"]);
    assert_eq!(
        init["schema_version"],
        dagq::infrastructure::sqlite::SqliteQueue::SCHEMA_VERSION
    );
    let db = PathBuf::from(init["db"].as_str().unwrap());
    assert!(db.starts_with(env.data_home.join("dagq")));
    assert_eq!(dagq(&env, &["locate"])["db_exists"], true);
    Fixture {
        // A repository queue's directory is named after the queue hash,
        // which is the external ID of its workspace group.
        group: GroupGuard::new(
            cmux.clone(),
            db.parent()
                .unwrap()
                .file_name()
                .unwrap()
                .to_str()
                .unwrap()
                .to_owned(),
        ),
        _dir: dir,
        cmux,
        repo,
        stub,
        base,
        db,
        env,
        _owner: owner,
        _test: test,
    }
}

impl Fixture {
    fn group(&self) -> Option<Value> {
        listed_group(&self.cmux, &self.group.external_id)
    }
}

/// Register a ready task whose acceptance the stub agent satisfies.
fn add_ready_task(env: &Env, title: &str, dependencies: &[&str]) -> String {
    add_ready_task_verifying(env, title, dependencies, &[])
}

fn add_ready_task_verifying(
    env: &Env,
    title: &str,
    dependencies: &[&str],
    verify: &[&str],
) -> String {
    add_ready_task_described(
        env,
        title,
        "Add e2e.txt to the worktree",
        dependencies,
        verify,
    )
}

fn add_ready_task_described(
    env: &Env,
    title: &str,
    description: &str,
    dependencies: &[&str],
    verify: &[&str],
) -> String {
    let mut args = vec![
        "add",
        title,
        "--description",
        description,
        "--acceptance",
        "e2e.txt is committed and seed.txt still exists",
        "--verify",
        "test -f seed.txt",
        "--verify",
        "test -f e2e.txt",
    ];
    for command in verify {
        args.extend(["--verify", command]);
    }
    for dependency in dependencies {
        args.extend(["--depends-on", dependency]);
    }
    let id = dagq(env, &args)["id"].to_string();
    assert_eq!(
        dagq(env, &["ready", &id, "--bypass-review"])["status"],
        "ready"
    );
    id
}

/// What one `supervise` pass produced, plus what the test observed while it ran.
struct Pass {
    outcome: Value,
    stderr: String,
    /// Workspace id per task, in the order they were first seen.
    workspaces: Vec<(String, String)>,
    /// Whether every workspace was listed by cmux at one moment; for one task
    /// this is simply "it was listed".
    listed_together: bool,
    /// Each workspace's cmux listing entry from the moment they were listed together.
    listings: Vec<Value>,
}

/// Run `supervise --once` with the given extra arguments and watch the runs of
/// `tasks` until it exits: their workspace ids must appear in the queue and in
/// cmux's own list before the sessions end. The stub agents of the pass keep
/// their sessions until the test has seen that (the `watching` and `listed`
/// files in the run env's `E2E_SHARED`), so it does not depend on how fast
/// the host is (task 641).
fn supervise_once(
    fixture: &Fixture,
    extra: &[&str],
    tasks: &[&str],
    guard: &mut WorkspaceGuard,
) -> Pass {
    let shared = fixture.db.with_file_name("shared");
    fs::create_dir_all(&shared).unwrap();
    let watching = shared.join("watching");
    let listed = shared.join("listed");
    let _ = fs::remove_file(&listed);
    fs::write(&watching, "").unwrap();
    let started = Instant::now();
    let mut child = ChildGuard::new(
        Command::new(BIN)
            // A person's supervisor, not the session's running the tests.
            .without_actor_env()
            .current_dir(&fixture.repo)
            .env("XDG_DATA_HOME", &fixture.env.data_home)
            .arg("supervise")
            .arg("--once")
            .args(NO_LOAD_HOLD)
            .args(extra)
            .arg("--cmux")
            .arg(&fixture.cmux)
            .arg("--claude")
            .arg(&fixture.stub)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap(),
    );
    let stdout = reader(child.0.stdout.take().unwrap());
    let stderr = reader(child.0.stderr.take().unwrap());
    let mut workspaces: Vec<(String, String)> = Vec::new();
    let mut listed_together = false;
    let mut listings = Vec::new();
    let status = loop {
        if let Some(status) = child.0.try_wait().unwrap() {
            child.reaped();
            break status;
        }
        assert!(
            started.elapsed() < SUPERVISE_TIMEOUT,
            "supervise did not finish within {SUPERVISE_TIMEOUT:?}"
        );
        for task in tasks {
            if workspaces.iter().any(|(t, _)| t == task) {
                continue;
            }
            let detail = dagq(&fixture.env, &["show", task, "--full"]);
            if let Some(id) = detail["runs"]
                .as_array()
                .unwrap()
                .last()
                .and_then(|r| r["workspace_id"].as_str())
            {
                guard.record(id);
                uuid::Uuid::parse_str(id).expect("workspace id is a UUID");
                eprintln!(
                    "task {task} workspace {id} registered after {:?}",
                    started.elapsed()
                );
                workspaces.push((task.to_string(), id.to_owned()));
            }
        }
        if workspaces.len() == tasks.len() && !listed_together {
            let entries: Vec<Value> = workspaces
                .iter()
                .filter_map(|(_, id)| listed_workspace(&fixture.cmux, id))
                .collect();
            listed_together = entries.len() == workspaces.len();
            if listed_together {
                listings = entries;
                fs::write(&listed, "").unwrap();
            }
        }
        thread::sleep(Duration::from_millis(200));
    };
    let supervise_took = started.elapsed();
    fs::remove_file(&watching).unwrap();
    let stdout = joined(stdout, "the supervisor's stdout reader");
    let stderr = joined(stderr, "the supervisor's stderr reader");
    eprintln!("supervise finished in {supervise_took:?}\n{stderr}");
    assert!(status.success(), "supervise failed ({status}): {stderr}");
    let outcome: Value = serde_json::from_str(&stdout).unwrap();
    assert_eq!(outcome["outcome"], "finished", "{outcome}");
    Pass {
        outcome,
        stderr,
        workspaces,
        listed_together,
        listings,
    }
}

#[test]
#[ignore = "needs a running cmux; run with --ignored"]
fn happy_path_runs_a_stub_agent_through_cmux_and_lands_on_main() {
    let fixture = fixture();
    let Fixture {
        cmux,
        repo,
        base,
        db,
        env,
        ..
    } = &fixture;
    // The stub reviewer passes a task that says E2E-REVIEW-PASS.
    let task_id = add_ready_task_described(
        env,
        "e2e stub task",
        "Add e2e.txt to the worktree. E2E-REVIEW-PASS",
        &[],
        &[VERIFY_RUN_ENV],
    );
    assert_eq!(dagq(env, &["candidates"]).as_array().unwrap().len(), 1);
    // The supervisor pushes what it lands to the repository's bare origin.
    let origin = repo.parent().unwrap().join("origin.git");
    git(
        repo.parent().unwrap(),
        &[
            "init",
            "-q",
            "--bare",
            "-b",
            "main",
            origin.to_str().unwrap(),
        ],
    );
    git(repo, &["remote", "add", "origin", origin.to_str().unwrap()]);

    let mut guard = WorkspaceGuard {
        cmux: cmux.clone(),
        ids: Vec::new(),
    };
    let pass = supervise_once(&fixture, &[], &[&task_id], &mut guard);
    let workspace = pass.workspaces[0].1.clone();
    assert!(
        pass.listed_together,
        "workspace {workspace} never appeared in cmux workspace list"
    );
    let outcome = &pass.outcome;
    assert_eq!(outcome["runs"].as_array().unwrap().len(), 1, "{outcome}");
    assert_eq!(outcome["errors"], Value::Array(vec![]), "{outcome}");
    // Accepted, reviewed and landed by the supervisor alone (ADR-0027).
    assert_eq!(outcome["runs"][0]["status"], "integrated", "{outcome}");
    let stderr = &pass.stderr;
    let base = base.as_str();
    let repo = repo.as_path();
    let db = db.as_path();

    let detail = dagq(env, &["show", &task_id, "--full"]);
    assert_eq!(detail["task"]["status"], "completed");
    let runs = detail["runs"].as_array().unwrap();
    assert_eq!(runs.len(), 1);
    let run = &runs[0];
    let run_id = run["id"].as_str().unwrap();
    assert_eq!(run["status"], "integrated");
    assert_eq!(run["workspace_id"], workspace.as_str());
    assert_eq!(run["base_commit"], base);
    assert!(run["last_error"].is_null());
    // ADR-0018: the name carries the repository, task and title; the run
    // ID lives in the description.
    let listing = &pass.listings[0];
    assert_eq!(
        listing["custom_title"],
        format!(
            "[{}]worker#{task_id} - e2e stub task",
            repo.file_name().unwrap().to_string_lossy()
        ),
        "{listing}"
    );
    assert_eq!(
        listing["description"],
        format!(
            "dagq role=worker queue={} run={run_id} task={task_id}",
            fixture.group.external_id
        ),
        "{listing}"
    );
    assert_eq!(run["branch"], format!("dagq/{run_id}"));
    assert!(run["workspace_closed_at"].is_number(), "{run}");
    // cmux accepted the close; its list can lag behind it for a moment.
    wait_until_not_listed(cmux, &workspace);
    assert!(stderr.contains("review 1: pass"), "{stderr}");
    assert!(stderr.contains("integrated"), "{stderr}");

    // The run lives next to the queue.
    let run_dir = Path::new(run["run_dir"].as_str().unwrap());
    assert_eq!(
        run_dir,
        db.canonicalize()
            .unwrap()
            .with_file_name("runs")
            .join(run_id)
    );
    let worktree = Path::new(run["worktree_path"].as_str().unwrap());
    assert_eq!(worktree, run_dir.join("worktree"));
    // The run's own history is kept under its ref; the landing is one
    // squash commit on main with its tree, pushed to origin.
    let head = git(repo, &["rev-parse", &format!("refs/dagq/runs/{run_id}")]);
    assert_ne!(head, base);
    let main = git(repo, &["rev-parse", "main"]);
    assert_ne!(main, head);
    assert_eq!(run["result_commit"], main.as_str());
    assert_eq!(git(&origin, &["rev-parse", "main"]), main);
    assert_eq!(git(repo, &["rev-parse", "main^"]), base);
    assert_eq!(
        git(repo, &["rev-parse", "main^{tree}"]),
        git(repo, &["rev-parse", &format!("{head}^{{tree}}")])
    );
    assert_eq!(
        git(repo, &["log", "-1", "--format=%B", "main"]),
        format!("e2e stub task\n\nadded e2e.txt\n\nDagq-Task: {task_id}\nDagq-Run: {run_id}")
    );
    assert_eq!(git(repo, &["status", "--porcelain"]), ""); // The checkout moved with main.
    assert_eq!(
        fs::read_to_string(repo.join("e2e.txt")).unwrap(),
        format!("written by the stub agent for {run_id}\n")
    );
    assert!(!worktree.exists(), "landed worktree was not removed");
    assert_eq!(
        git(repo, &["branch", "--list", &format!("dagq/{run_id}")]),
        ""
    );

    let receipt: Value =
        serde_json::from_str(&fs::read_to_string(run["receipt_path"].as_str().unwrap()).unwrap())
            .unwrap();
    assert_eq!(receipt["run_id"], run_id);
    assert_eq!(receipt["commit"], head.as_str());
    let log = fs::read_to_string(run["log_path"].as_str().unwrap()).unwrap();
    assert!(
        log.contains(&format!(
            "argv: --session-id {run_id} --debug-file {} --add-dir {run_dir} --settings {run_dir}/claude-settings.json",
            run["log_path"].as_str().unwrap(),
            run_dir = run["run_dir"].as_str().unwrap()
        )),
        "{log}"
    );
    // The model and effort were given explicitly (ADR-0079 decision 3).
    assert!(
        log.contains("model: claude-opus-5-5 effort: medium"),
        "{log}"
    );
    // The run workspace's `--env` reached the agent through its shell.
    assert!(
        log.contains(&format!(
            "env: DAGQ_ROLE=worker DAGQ_QUEUE={}",
            fixture.db.canonicalize().unwrap().display()
        )),
        "{log}"
    );
    // dagq.toml's [run.env], expanded, reached the agent's shell; the
    // verification commands get it at the landing, the only place they run.
    let shared = db.canonicalize().unwrap().with_file_name("shared");
    assert!(
        log.contains(&format!(
            "run env: E2E_SHARED={} E2E_RUN_DIR={}",
            shared.display(),
            run_dir.display()
        )),
        "{log}"
    );
    assert_eq!(
        fs::read_to_string(run_dir.join("verify-env.txt")).unwrap(),
        format!("verify env: {}\n", shared.display())
    );
    // The headless review ran `claude -p` with the run directory's hook-less
    // settings and read review.md there.
    let review_log = fs::read_to_string(run_dir.join("claude-review.log")).unwrap();
    assert!(
        review_log.contains(&format!(
            "argv: -p --debug-file {run_dir}/claude-review.log --add-dir {run_dir} --settings {run_dir}/claude-review-settings.json",
            run_dir = run_dir.display()
        )),
        "{review_log}"
    );
    assert!(
        review_log.contains(&format!("review: {}/review.md", run_dir.display())),
        "{review_log}"
    );
    // With a session id of its own, recorded in `review_started` and in the
    // review's span (ADR-0048).
    let review_session = review_log
        .lines()
        .find_map(|line| line.strip_prefix("session: "))
        .unwrap_or_default()
        .to_owned();
    assert!(
        !review_session.is_empty() && review_session != run_id,
        "{review_log}"
    );
    // The run workspace joined the queue's group, made by its external ID.
    assert!(
        fixture.group().is_some(),
        "no workspace group for the queue"
    );

    let events = detail["events"].as_array().unwrap();
    let kinds: Vec<&str> = events.iter().map(|e| e["kind"].as_str().unwrap()).collect();
    let position = |kind: &str| {
        kinds
            .iter()
            .position(|k| *k == kind)
            .unwrap_or_else(|| panic!("missing {kind} in {kinds:?}"))
    };
    // The session stays open through validation and the review; /exit is
    // sent on the passing verdict, and the landing follows the close.
    let order = [
        "lease_acquired",
        "worktree_created",
        "workspace_created",
        "wrapper_started",
        "agent_started",
        "receipt_observed",
        "session_idle_observed",
        "supervision_finished",
        "validation_finished",
        "review_started",
        "review_finished",
        "exit_requested",
        "session_exited",
        "workspace_closed",
        "landing_queued",
        "integration_started",
        "integration_rebased",
        "run_integrated",
        "worktree_removed",
        "push_finished",
    ];
    for pair in order.windows(2) {
        assert!(
            position(pair[0]) < position(pair[1]),
            "{} before {}: {kinds:?}",
            pair[0],
            pair[1]
        );
    }
    let event = |kind: &str| events.iter().find(|e| e["kind"] == kind).unwrap();
    assert_eq!(
        event("workspace_created")["payload"]["workspace_id"],
        workspace.as_str()
    );
    assert_eq!(
        event("session_idle_observed")["payload"]["session_id"],
        run_id
    );
    assert_eq!(
        event("supervision_finished")["payload"],
        serde_json::json!({"status": "validating", "exit_code": null, "session_live": true})
    );
    assert_eq!(
        event("review_started")["payload"]["session_live"],
        true,
        "the session must be alive during the review"
    );
    let review = &event("review_finished")["payload"];
    assert_eq!(review["verdict"], "pass");
    assert_eq!(review["attempt"], 1);
    assert_eq!(
        review["summary"],
        "the stub reviewer found e2e.txt committed"
    );
    assert_eq!(
        event("exit_requested")["payload"]["workspace_id"],
        workspace.as_str()
    );
    assert!(!kinds.contains(&"exit_request_timed_out"), "{kinds:?}");
    assert!(!kinds.contains(&"integration_approved"), "{kinds:?}");
    assert_eq!(event("session_exited")["payload"]["exit_code"], 0);
    let finished = event("validation_finished");
    assert_eq!(finished["payload"]["status"], "awaiting_integration");
    assert_eq!(finished["payload"]["result_commit"], head.as_str());
    assert_eq!(finished["payload"]["receipt"]["summary"], "added e2e.txt");
    // The verification commands ran once, after the rebase, with the run env.
    let verifications: Vec<&Value> = events
        .iter()
        .filter(|e| e["kind"] == "verification_command")
        .collect();
    assert_eq!(verifications.len(), 3);
    assert!(
        verifications
            .iter()
            .all(|e| e["payload"]["exit_code"] == 0 && e["payload"]["phase"] == "integration")
    );
    assert!(!kinds.contains(&"cleanup_failed"), "{kinds:?}");

    let processes = detail["processes"].as_array().unwrap();
    assert_eq!(processes.len(), 2);
    assert!(processes.iter().all(|p| p["exit_code"] == 0));

    assert_eq!(dagq(env, &["candidates"]).as_array().unwrap().len(), 0);
    let status = dagq(env, &["status"]);
    assert_eq!(status["supervisors"], Value::Array(vec![]), "{status}");
    assert_eq!(status["runs"], Value::Array(vec![]), "{status}");
    // Only the stopped `--once` supervisor is left; no run waits for anyone.
    assert!(
        status["attention"]
            .as_array()
            .unwrap()
            .iter()
            .all(|a| a["run_id"].is_null()),
        "{status}"
    );
    assert_eq!(
        dagq(env, &["integrate", "--next"])["outcome"],
        "no_run_awaiting"
    );
}

/// The stub worker asks a `worker_question` (its task says `E2E-ASK`) and
/// goes idle; once the ask is answered the supervisor types the answer into
/// the worker's cmux terminal, closes the ask, and the worker commits it.
#[test]
#[ignore = "needs a running cmux; run with --ignored"]
fn a_worker_question_is_answered_through_the_worker_terminal() {
    let fixture = fixture();
    let Fixture { cmux, env, .. } = &fixture;
    let task_id = dagq(
        env,
        &[
            "add",
            "e2e asking task",
            "--description",
            "E2E-ASK: ask which word goes into answer.txt, then add e2e.txt",
            "--acceptance",
            "answer.txt holds the answer and e2e.txt is committed",
            "--verify",
            "test -f answer.txt",
            "--verify",
            "test -f e2e.txt",
        ],
    )["id"]
        .to_string();
    assert_eq!(
        dagq(env, &["ready", &task_id, "--bypass-review"])["status"],
        "ready"
    );
    let mut guard = WorkspaceGuard {
        cmux: cmux.clone(),
        ids: Vec::new(),
    };
    // Answers the ask as the inbox would, once the worker registered it.
    let answerer = {
        let env = Env {
            repo: env.repo.clone(),
            data_home: env.data_home.clone(),
        };
        thread::spawn(move || {
            let started = Instant::now();
            loop {
                assert!(
                    started.elapsed() < SUPERVISE_TIMEOUT,
                    "the worker asked nothing"
                );
                let asks = dagq(&env, &["asks", "--open"]);
                if let Some(ask) = asks["asks"].as_array().unwrap().first() {
                    assert_eq!(ask["kind"], "worker_question", "{ask}");
                    assert_eq!(ask["asked_by"], "worker", "{ask}");
                    let id = ask["id"].to_string();
                    dagq(&env, &["answer", &id, "--text", "blue"]);
                    return ask["id"].as_i64().unwrap();
                }
                thread::sleep(Duration::from_millis(200));
            }
        })
    };
    let pass = supervise_once(&fixture, &[], &[&task_id], &mut guard);
    let ask_id = joined(answerer, "the answering thread");
    assert_eq!(
        pass.outcome["errors"],
        Value::Array(vec![]),
        "{}",
        pass.outcome
    );
    assert_eq!(
        pass.outcome["runs"][0]["status"], "awaiting_integration",
        "{}",
        pass.outcome
    );
    let detail = dagq(env, &["show", &task_id, "--full"]);
    let run = &detail["runs"][0];
    let worktree = Path::new(run["worktree_path"].as_str().unwrap());
    assert_eq!(
        fs::read_to_string(worktree.join("answer.txt")).unwrap(),
        format!("answer to ask {ask_id}: blue\n")
    );
    let delivered: Vec<&Value> = detail["events"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|e| e["kind"] == "ask_delivered")
        .collect();
    assert_eq!(delivered.len(), 1, "{detail}");
    assert_eq!(delivered[0]["payload"]["ask_id"], ask_id);
    let asks = dagq(env, &["asks", "--all"]);
    assert!(asks["asks"][0]["closed_at"].is_number(), "{asks}");
    // The only open ask is the one of the failed stub review (task 328).
    let open = dagq(env, &["asks"]);
    let open = open["asks"].as_array().unwrap();
    assert_eq!(open.len(), 1, "{open:?}");
    assert_eq!(open[0]["kind"], "approve_landing");
}

/// Two independent tasks run in two cmux workspaces at once; the task that
/// depends on one of them waits for its integration and then starts from
/// the main that contains it.
#[test]
#[ignore = "needs a running cmux; run with --ignored"]
fn two_independent_tasks_run_concurrently_and_a_dependent_follows_integration() {
    let fixture = fixture();
    let Fixture {
        cmux,
        repo,
        base,
        env,
        ..
    } = &fixture;
    let first = add_ready_task(env, "e2e first", &[]);
    let second = add_ready_task(env, "e2e second", &[]);
    let third = add_ready_task(env, "e2e dependent", &[&first]);
    assert_eq!(dagq(env, &["candidates"]).as_array().unwrap().len(), 2);

    let mut guard = WorkspaceGuard {
        cmux: cmux.clone(),
        ids: Vec::new(),
    };
    let pass = supervise_once(
        &fixture,
        &["--parallel", "2"],
        &[&first, &second],
        &mut guard,
    );
    assert!(
        pass.listed_together,
        "both workspaces were never open at the same time: {:?}",
        pass.workspaces
    );
    assert_ne!(pass.workspaces[0].1, pass.workspaces[1].1);
    let outcome = &pass.outcome;
    assert_eq!(outcome["errors"], Value::Array(vec![]), "{outcome}");
    let runs = outcome["runs"].as_array().unwrap();
    assert_eq!(runs.len(), 2, "{outcome}");
    assert!(
        runs.iter().all(|r| r["status"] == "awaiting_integration"),
        "{outcome}"
    );
    for task in [&first, &second] {
        let detail = dagq(env, &["show", task, "--full"]);
        assert_eq!(detail["task"]["status"], "in_progress");
        let run = &detail["runs"][0];
        assert_eq!(run["status"], "awaiting_integration");
        assert_eq!(run["base_commit"], base.as_str());
        assert!(run["workspace_closed_at"].is_number(), "{run}");
        assert!(run["last_error"].is_null(), "{run}");
        let worktree = Path::new(run["worktree_path"].as_str().unwrap());
        assert_eq!(git(worktree, &["status", "--porcelain"]), "");
    }
    // The dependent never started: awaiting integration is not completion.
    let detail = dagq(env, &["show", &third, "--full"]);
    assert_eq!(detail["task"]["status"], "ready");
    assert_eq!(detail["runs"], Value::Array(vec![]));
    assert_eq!(dagq(env, &["candidates"]).as_array().unwrap().len(), 0);
    assert_eq!(
        dagq(env, &["doctor", "--full"])["runs"],
        Value::Array(vec![])
    );

    // Land the first task; the dependent becomes claimable from the landed main.
    let first_run = dagq(env, &["show", &first, "--full"])["runs"][0].clone();
    let first_commit = first_run["result_commit"].as_str().unwrap().to_owned();
    assert_eq!(dagq(env, &["integrate", &first])["outcome"], "integrated");
    let first_landed = git(repo, &["rev-parse", "main"]);
    assert_ne!(first_landed, first_commit);
    assert_eq!(git(repo, &["rev-parse", "main^"]), base.as_str());
    assert_eq!(dagq(env, &["candidates"])[0]["id"].to_string(), third);
    let pass = supervise_once(&fixture, &["--parallel", "2"], &[&third], &mut guard);
    assert_eq!(pass.outcome["runs"].as_array().unwrap().len(), 1);
    let run = dagq(env, &["show", &third, "--full"])["runs"][0].clone();
    assert_eq!(run["status"], "awaiting_integration", "{run}");
    assert_eq!(run["base_commit"], first_landed.as_str());
    assert_eq!(git(repo, &["rev-parse", "main"]), first_landed);
    assert_eq!(dagq(env, &["status"])["supervisors"], Value::Array(vec![]));

    // The merge queue is FIFO by validation time: --next takes the second
    // task first. It rewrote the same file as the first, so the runtime
    // cannot rebase it and parks it for a session; the next --next lands the
    // dependent, which sits on the first landing.
    let parked = dagq(env, &["integrate", "--next"]);
    assert_eq!(parked["outcome"], "needs_session", "{parked}");
    assert_eq!(parked["run"]["task_id"].to_string(), second);
    assert!(
        parked["reason"]
            .as_str()
            .unwrap()
            .contains("conflicted in e2e.txt"),
        "{parked}"
    );
    let next = dagq(env, &["integrate", "--next"]);
    assert_eq!(next["outcome"], "integrated", "{next}");
    assert_eq!(next["task"]["id"].to_string(), third);
    let third_landed = git(repo, &["rev-parse", "main"]);
    assert_eq!(git(repo, &["rev-parse", "main^"]), first_landed);

    // The integrate call approved it, so the next supervisor pass resumes
    // its session (the stub plays Claude), which resolves the conflict on
    // top of main and rewrites the receipt; the runtime then lands it.
    let run = dagq(env, &["show", &second, "--full"])["runs"][0].clone();
    assert_eq!(run["status"], "needs_session");
    let worktree = Path::new(run["worktree_path"].as_str().unwrap());
    assert_eq!(
        git(worktree, &["rev-parse", "HEAD"]),
        run["result_commit"].as_str().unwrap()
    );
    assert_eq!(
        dagq(env, &["integrate", "--next"])["outcome"],
        "no_run_awaiting"
    );
    let status = dagq(env, &["status"]);
    let attention = status["attention"]
        .as_array()
        .unwrap()
        .iter()
        .find(|a| a["run_id"] == run["id"])
        .cloned()
        .unwrap();
    assert_eq!(attention["next"], "resuming (runtime)", "{status}");
    let pass = supervise_once(&fixture, &["--parallel", "2"], &[], &mut guard);
    let outcome = &pass.outcome;
    assert_eq!(outcome["errors"], Value::Array(vec![]), "{outcome}");
    assert!(
        pass.stderr.contains("resolution request sent"),
        "{}",
        pass.stderr
    );
    let detail = dagq(env, &["show", &second, "--full"]);
    let run = &detail["runs"][0];
    assert_eq!(run["status"], "integrated", "{detail}");
    let run_dir = Path::new(run["run_dir"].as_str().unwrap());
    let seen = fs::read_to_string(run_dir.join("resume-request-seen.txt")).unwrap();
    assert!(
        seen.contains(&format!("main is now {third_landed} ")),
        "{seen}"
    );
    assert!(seen.contains("task 1: e2e first"), "{seen}");
    assert!(seen.contains("task 3: e2e dependent"), "{seen}");
    let kinds: Vec<&str> = detail["events"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["kind"].as_str().unwrap())
        .collect();
    for kind in [
        "integration_approved",
        "resume_started",
        "resume_finished",
        "run_integrated",
    ] {
        assert!(kinds.contains(&kind), "{kind} missing: {kinds:?}");
    }
    let finished = detail["events"]
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["kind"] == "resume_finished")
        .unwrap();
    if let Some(id) = finished["payload"]["workspace_id"].as_str() {
        guard.record(id);
    }
    assert_eq!(finished["payload"]["outcome"], "resolved", "{finished}");
    assert_eq!(finished["payload"]["workspace_closed"], true, "{finished}");
    let resume_workspace = finished["payload"]["workspace_id"].as_str().unwrap();
    wait_until_not_listed(cmux, resume_workspace);
    assert_eq!(git(repo, &["rev-parse", "main^"]), third_landed);
    assert_eq!(
        git(repo, &["rev-list", "--count", &format!("{base}..main")]),
        "3"
    );
    assert_eq!(
        fs::read_to_string(repo.join("e2e.txt")).unwrap(),
        "resolved by the session\n"
    );
    for task in [&first, &second, &third] {
        let detail = dagq(env, &["show", task, "--full"]);
        assert_eq!(detail["task"]["status"], "completed", "{task}");
        assert!(
            !Path::new(detail["runs"][0]["worktree_path"].as_str().unwrap()).exists(),
            "{task}"
        );
    }
    assert_eq!(
        git(repo, &["for-each-ref", "refs/dagq/runs/"])
            .lines()
            .count(),
        3
    );
}

/// The supervisor is killed while the stub worker is running (the task 15
/// incident: a binary update or a `kill` took the resident supervisor with
/// it). The wrapper keeps heartbeating in its cmux workspace and the stub
/// writes its receipt regardless. The next `supervise --once` adopts the
/// run from the dead supervisor's stale lease (ADR-0012), sends `/exit`
/// once, validates it, and `integrate` lands it: nothing is redone.
#[test]
#[ignore = "needs a running cmux; run with --ignored"]
fn killed_supervisor_run_is_adopted_by_the_next_supervisor_and_lands() {
    let fixture = fixture();
    let Fixture {
        cmux, repo, env, ..
    } = &fixture;
    let task_id = add_ready_task(env, "e2e adopted task", &[]);
    let mut guard = WorkspaceGuard {
        cmux: cmux.clone(),
        ids: Vec::new(),
    };

    // A resident supervisor starts the run; it is killed once the worker runs.
    let mut victim = ChildGuard::new(
        Command::new(BIN)
            // A person's supervisor, not the session's running the tests.
            .without_actor_env()
            .current_dir(&fixture.repo)
            .env("XDG_DATA_HOME", &fixture.env.data_home)
            .args(["supervise", "--parallel", "1"])
            .args(NO_LOAD_HOLD)
            .arg("--cmux")
            .arg(&fixture.cmux)
            .arg("--claude")
            .arg(&fixture.stub)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap(),
    );
    let victim_stderr = reader(victim.0.stderr.take().unwrap());
    let victim_pid = victim.0.id();
    let started = Instant::now();
    let run = loop {
        assert!(
            victim.0.try_wait().unwrap().is_none(),
            "the supervisor exited before the worker started"
        );
        assert!(
            started.elapsed() < SUPERVISE_TIMEOUT,
            "the worker did not start within {SUPERVISE_TIMEOUT:?}"
        );
        let detail = dagq(env, &["show", &task_id, "--full"]);
        if let Some(run) = detail["runs"].as_array().unwrap().last()
            && run["status"] == "running"
        {
            break run.clone();
        }
        thread::sleep(Duration::from_millis(200));
    };
    let run_id = run["id"].as_str().unwrap().to_owned();
    let workspace = run["workspace_id"].as_str().unwrap().to_owned();
    guard.record(&workspace);
    eprintln!(
        "worker of run {run_id} started after {:?}; killing supervisor {victim_pid}",
        started.elapsed()
    );
    victim.0.kill().unwrap();
    victim.0.wait().unwrap();
    victim.reaped();
    eprintln!(
        "killed supervisor stderr:\n{}",
        joined(victim_stderr, "the killed supervisor's stderr reader")
    );
    assert!(!pid_alive(victim_pid));

    // What the inbox sees before anyone adopts: the registration and
    // the lease are stale by pid, the wrapper is alive, and the run keeps going.
    let status = status_when(env, |status| {
        status["supervisors"]
            .as_array()
            .is_some_and(|s| s.len() == 1 && s[0]["stale"] == true)
            && status["runs"][0]["lease"]["alive"] == false
    });
    let supervisors = status["supervisors"].as_array().unwrap();
    assert_eq!(supervisors.len(), 1, "{status}");
    assert_eq!(supervisors[0]["pid"], victim_pid);
    assert_eq!(supervisors[0]["alive"], false);
    assert_eq!(supervisors[0]["stale"], true);
    assert_eq!(
        supervisors[0]["run_ids"],
        Value::Array(vec![Value::String(run_id.clone())])
    );
    assert_eq!(status["runs"][0]["run_id"], run_id.as_str());
    assert_eq!(status["runs"][0]["lease"]["pid"], victim_pid);
    assert_eq!(status["runs"][0]["lease"]["alive"], false);
    let doctor = dagq(env, &["doctor", "--full"]);
    assert_eq!(doctor["runs"][0]["recoverable"], false, "{doctor}");
    let wrapper = doctor["runs"][0]["processes"]
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["role"] == "wrapper")
        .unwrap()
        .clone();
    assert_eq!(wrapper["alive"], true, "{doctor}");
    assert!(workspace_listed(cmux, &workspace));

    // The next supervisor adopts the run instead of leaving it to recover.
    let pass = supervise_once(&fixture, &["--parallel", "1"], &[&task_id], &mut guard);
    let outcome = &pass.outcome;
    assert_eq!(outcome["errors"], Value::Array(vec![]), "{outcome}");
    assert_eq!(outcome["runs"].as_array().unwrap().len(), 1, "{outcome}");
    assert_eq!(outcome["runs"][0]["id"], run_id.as_str());
    assert_eq!(outcome["runs"][0]["status"], "awaiting_integration");
    assert!(
        pass.stderr
            .contains(&format!("run {run_id} adopted from supervisor")),
        "{}",
        pass.stderr
    );
    assert_eq!(pass.workspaces, vec![(task_id.clone(), workspace.clone())]);

    let detail = dagq(env, &["show", &task_id, "--full"]);
    assert_eq!(detail["runs"].as_array().unwrap().len(), 1); // Not rerun.
    let run = &detail["runs"][0];
    assert_eq!(run["status"], "awaiting_integration");
    assert_eq!(run["workspace_id"], workspace.as_str());
    assert!(run["last_error"].is_null(), "{run}");
    assert!(run["workspace_closed_at"].is_number(), "{run}");
    // cmux accepted the close; its list can lag behind it for a moment.
    wait_until_not_listed(cmux, &workspace);
    let events = detail["events"].as_array().unwrap();
    let kinds: Vec<&str> = events.iter().map(|e| e["kind"].as_str().unwrap()).collect();
    let count = |kind: &str| kinds.iter().filter(|k| **k == kind).count();
    assert_eq!(count("run_adopted"), 1, "{kinds:?}");
    assert_eq!(count("exit_requested"), 1, "{kinds:?}");
    assert_eq!(count("session_exited"), 1, "{kinds:?}");
    assert_eq!(count("validation_finished"), 1, "{kinds:?}");
    assert!(!kinds.contains(&"run_recovered"), "{kinds:?}");
    assert!(!kinds.contains(&"runtime_error"), "{kinds:?}");
    let adopted = events.iter().find(|e| e["kind"] == "run_adopted").unwrap();
    assert_eq!(adopted["payload"]["previous_pid"], victim_pid);
    assert_eq!(adopted["payload"]["wrapper"]["pid"], wrapper["pid"]);
    assert_eq!(adopted["payload"]["wrapper"]["alive"], true);
    let position = |kind: &str| kinds.iter().position(|k| *k == kind).unwrap();
    assert!(
        position("agent_started") < position("run_adopted"),
        "{kinds:?}"
    );
    assert!(
        position("run_adopted") < position("exit_requested"),
        "{kinds:?}"
    );
    // `exit_requested` is recorded before `/exit` is sent, so a session that
    // exits before the send returns still lands after it.
    assert!(
        position("exit_requested") < position("session_exited"),
        "{kinds:?}"
    );
    // The adopter deregistered on exit; the killed one's row stays for `up` to prune.
    let status = status_when(env, |status| {
        status["supervisors"].as_array().is_some_and(|s| {
            s.len() == 1 && s[0]["pid"] == victim_pid && s[0]["run_ids"] == Value::Array(vec![])
        }) && status["runs"] == Value::Array(vec![])
    });
    let supervisors = status["supervisors"].as_array().unwrap();
    let diagnosis = format!(
        "victim pid {victim_pid}; supervisors {supervisors:#?}; adopter stderr:\n{}",
        pass.stderr
    );
    assert_eq!(supervisors.len(), 1, "{diagnosis}");
    assert_eq!(supervisors[0]["pid"], victim_pid, "{diagnosis}");
    assert_eq!(
        supervisors[0]["run_ids"],
        Value::Array(vec![]),
        "{diagnosis}"
    );
    assert_eq!(status["runs"], Value::Array(vec![]));

    let integrated = dagq(env, &["integrate", &task_id]);
    assert_eq!(integrated["outcome"], "integrated", "{integrated}");
    assert_eq!(integrated["task"]["status"], "completed");
    // No origin in this repository: the push is skipped and the landing stands.
    assert_eq!(integrated["push"]["outcome"], "skipped", "{integrated}");
    assert_eq!(
        integrated["push"]["reason"],
        "the repository has no remote origin"
    );
    assert_eq!(
        fs::read_to_string(repo.join("e2e.txt")).unwrap(),
        format!("written by the stub agent for {run_id}\n")
    );
    assert_eq!(
        git(
            repo,
            &["rev-list", "--count", &format!("{}..main", fixture.base)]
        ),
        "1"
    );
}

/// Unloads the LaunchAgent the test bootstrapped if it is still there when
/// the test ends, so a failed assertion does not leave a supervisor
/// restarting forever against a deleted queue.
struct AgentGuard {
    label: String,
    plist: PathBuf,
}

impl AgentGuard {
    fn loaded(&self) -> bool {
        Command::new("launchctl")
            .args(["print", &format!("gui/{}/{}", uid(), self.label)])
            .bounded_output()
            .unwrap()
            .status
            .success()
    }
}

impl Drop for AgentGuard {
    fn drop(&mut self) {
        if self.loaded() {
            let output = Command::new("launchctl")
                .args(["bootout", &format!("gui/{}/{}", uid(), self.label)])
                .bounded_output()
                .unwrap();
            eprintln!(
                "booted out {} ({}): {}",
                self.label,
                output.status,
                String::from_utf8_lossy(&output.stderr)
            );
        }
        if self.plist.exists() {
            let _ = fs::remove_file(&self.plist);
            eprintln!("removed {}", self.plist.display());
        }
    }
}

/// The records of a JSON Lines log; each line must be one JSON object.
fn log_records(path: &Path) -> Vec<Value> {
    fs::read_to_string(path)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap_or_else(|e| panic!("{e}: {line}")))
        .collect()
}

fn log_messages(path: &Path) -> Vec<String> {
    log_records(path)
        .iter()
        .map(|record| record["message"].as_str().unwrap().to_owned())
        .collect()
}

fn uid() -> u32 {
    // SAFETY: getuid has no preconditions.
    unsafe { libc::getuid() }
}

fn pid_alive(pid: u32) -> bool {
    Command::new("kill")
        .args(["-0", &pid.to_string()])
        .bounded_output()
        .is_ok_and(|output| output.status.success())
}

/// Whether the launchd `up` / `down` e2e runs. It is temporarily off unless
/// `DAGQ_E2E_LAUNCHD=1`: no project runs the launchd mode now, and without a
/// cmux socket password `up`'s preflight always stops it, which would fail
/// every `--ignored` run. To bring it back, drop this check.
fn launchd_e2e_enabled() -> bool {
    if env::var("DAGQ_E2E_LAUNCHD").as_deref() == Ok("1") {
        return true;
    }
    eprintln!(
        "skipping the launchd up/down e2e: the launchd mode is temporarily out of the \
         default e2e because no project runs it now and, without a cmux socket password, \
         its preflight always stops `up`; set DAGQ_E2E_LAUNCHD=1 to run it"
    );
    false
}

/// `up` bootstraps the supervisor as a LaunchAgent of the real launchd and
/// opens the inbox workspace in the real cmux; `status` lists the
/// supervisor through its registration; `down --wait` unloads the agent
/// and returns once the supervisor has drained and deregistered.
///
/// Runs only with `DAGQ_E2E_LAUNCHD=1`; see [`launchd_e2e_enabled`].
#[test]
#[ignore = "needs a running cmux and launchd; run with --ignored and DAGQ_E2E_LAUNCHD=1"]
fn up_starts_a_launchd_supervisor_that_status_lists_and_down_wait_stops_it() {
    if !launchd_e2e_enabled() {
        return;
    }
    let fixture = fixture();
    let Fixture {
        cmux,
        repo,
        stub,
        db,
        env,
        ..
    } = &fixture;
    // The agent's plist goes under a disposable HOME, not the developer's.
    let home = fixture._dir.path().join("home");
    fs::create_dir(&home).unwrap();
    trust_repository(&home, &env.repo);
    let located = dagq_with(env, &[("HOME", home.as_path())], &["locate"]);
    let label = located["label"].as_str().unwrap().to_owned();
    let plist = PathBuf::from(located["launch_agent"].as_str().unwrap());
    assert!(plist.starts_with(&home));
    let log_dir = PathBuf::from(located["log_dir"].as_str().unwrap());
    let agent = AgentGuard {
        label: label.clone(),
        plist: plist.clone(),
    };
    let mut workspaces = WorkspaceGuard {
        cmux: cmux.clone(),
        ids: Vec::new(),
    };

    let up_args = [
        "up",
        "--parallel",
        "2",
        "--cmux",
        cmux.to_str().unwrap(),
        "--claude",
        stub.to_str().unwrap(),
        // The load of a busy host would hold every claim (task 680).
        NO_LOAD_HOLD[0],
        NO_LOAD_HOLD[1],
    ];
    let started = Instant::now();
    // A supervisor that never registers is diagnosed from launchd.log,
    // which `up` names in its error; show it before failing.
    let output = dagq_output(env, &[("HOME", home.as_path())], &up_args);
    workspaces.record_opened(&output.stdout);
    if !output.status.success() {
        let launchd_log = log_dir.join("launchd.log");
        eprintln!(
            "launchd.log:\n{}",
            fs::read_to_string(&launchd_log).unwrap_or_else(|_| "(not written)".into())
        );
        panic!("dagq up: {}", String::from_utf8_lossy(&output.stderr));
    }
    let first: Value = serde_json::from_slice(&output.stdout).unwrap();
    eprintln!("up took {:?}: {first}", started.elapsed());
    assert_eq!(first["supervisor"]["outcome"], "started", "{first}");
    let pid = u32::try_from(first["supervisor"]["pid"].as_u64().unwrap()).unwrap();
    assert!(pid_alive(pid));
    assert_eq!(first["supervisor"]["plist"], plist.to_str().unwrap());
    assert_eq!(first["supervisor"]["log_dir"], log_dir.to_str().unwrap());
    assert_eq!(first["pruned_supervisors"], Value::Array(vec![]));
    let repo_name = repo.file_name().unwrap().to_str().unwrap();
    // The inbox is the one session `up` opens (ADR-0041 decision 6).
    assert_eq!(first.get("planner"), None, "{first}");
    let sessions: Vec<String> = ["inbox"]
        .into_iter()
        .map(|key| {
            assert_eq!(first[key]["outcome"], "created", "{first}");
            assert_eq!(first[key]["name"], format!("[{repo_name}]{key}"));
            let id = first[key]["workspace_id"].as_str().unwrap().to_owned();
            uuid::Uuid::parse_str(&id).expect("workspace id is a UUID");
            assert!(workspace_listed(cmux, &id));
            id
        })
        .collect();
    // launchd knows the agent, and the plist is what `up` described.
    assert!(
        agent.loaded(),
        "launchctl print gui/{}/{label} failed",
        uid()
    );
    let contents = fs::read_to_string(&plist).unwrap();
    assert!(contents.contains(&format!("<string>{label}</string>")));
    assert!(contents.contains("<string>supervise</string>"));
    assert!(contents.contains(&format!("<string>{}</string>", db.display())));
    assert!(contents.contains("<key>KeepAlive</key>\n\t<true/>"));
    assert!(
        contents.contains("\t\t<string>--max-load</string>\n\t\t<string>0</string>\n"),
        "{contents}"
    );
    // The launchd-run supervisor wrote its own log.
    let logs: Vec<PathBuf> = fs::read_dir(&log_dir)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .filter(|path| {
            path.file_name()
                .unwrap()
                .to_str()
                .unwrap()
                .ends_with(&format!("-{pid}.jsonl"))
        })
        .collect();
    assert_eq!(logs.len(), 1, "{logs:?}");
    let log = log_messages(&logs[0]);
    assert!(
        log.iter().any(|m| m.contains(&format!(
            "started: version {VERSION}, pid {pid}, parallel 2"
        ))),
        "{log:?}"
    );

    let status = dagq(env, &["status"]);
    let supervisors = status["supervisors"].as_array().unwrap();
    assert_eq!(supervisors.len(), 1, "{status}");
    assert_eq!(supervisors[0]["pid"], pid);
    assert_eq!(supervisors[0]["registered"], true);
    assert_eq!(supervisors[0]["alive"], true);
    assert_eq!(supervisors[0]["stale"], false);
    assert_eq!(supervisors[0]["parallel"], 2);
    // The supervisor recorded the build it runs, which is what the next
    // `up` compares itself against (ADR-0014).
    assert_eq!(supervisors[0]["binary_version"], VERSION, "{status}");
    assert_eq!(status["runs"], Value::Array(vec![]));

    // Idempotent: nothing is started or opened twice.
    let second = dagq_opening(env, &[("HOME", home.as_path())], &up_args, &mut workspaces);
    assert_eq!(second["supervisor"]["outcome"], "reused", "{second}");
    assert_eq!(second["supervisor"]["version"], VERSION, "{second}");
    assert_eq!(second["supervisor"]["pid"], pid);
    for (key, id) in ["inbox"].into_iter().zip(&sessions) {
        assert_eq!(second[key]["outcome"], "reused", "{second}");
        assert_eq!(second[key]["workspace_id"], id.as_str());
    }
    assert_eq!(second["pruned_supervisors"], Value::Array(vec![]));

    let started = Instant::now();
    let down = dagq_with(env, &[("HOME", home.as_path())], &["down", "--wait"]);
    eprintln!("down --wait took {:?}: {down}", started.elapsed());
    assert_eq!(down["outcome"], "stopped", "{down}");
    assert_eq!(down["pid"], pid);
    assert_eq!(down["launch_agent_unloaded"], true);
    assert!(!pid_alive(pid), "supervisor {pid} is still alive");
    assert!(!agent.loaded(), "the agent is still loaded");
    assert!(!plist.exists(), "the plist was not removed");
    let status = dagq(env, &["status"]);
    assert_eq!(status["supervisors"], Value::Array(vec![]), "{status}");
    let log = log_messages(&logs[0]);
    assert!(
        log.iter()
            .any(|m| m.contains("exiting: {\"errors\":[],\"outcome\":\"stopped\"")),
        "{log:?}"
    );
    // The inbox workspace is left open by `down`; the guard closes it.
    for id in &sessions {
        assert!(workspace_listed(cmux, id));
    }
    let again = dagq_with(env, &[("HOME", home.as_path())], &["down"]);
    assert_eq!(again["outcome"], "not_running", "{again}");
}

/// `up --in-cmux` needs no socket password: the supervisor runs inside a
/// cmux workspace of its own, so it is a child of a cmux terminal like any
/// other client. Nothing about launchd is touched, `status` reports the
/// mode, and `down --wait` interrupts the supervisor and closes the
/// workspace once it has drained.
#[test]
#[ignore = "needs a running cmux; run with --ignored"]
fn up_in_cmux_starts_a_supervisor_in_a_workspace_that_down_wait_stops_and_closes() {
    let fixture = fixture();
    let Fixture {
        cmux,
        repo,
        stub,
        env,
        ..
    } = &fixture;
    // A disposable HOME, so a stray plist could only land there; none should.
    let home = fixture._dir.path().join("home");
    fs::create_dir(&home).unwrap();
    trust_repository(&home, &env.repo);
    let located = dagq_with(env, &[("HOME", home.as_path())], &["locate"]);
    let plist = PathBuf::from(located["launch_agent"].as_str().unwrap());
    let label = located["label"].as_str().unwrap().to_owned();
    let log_dir = PathBuf::from(located["log_dir"].as_str().unwrap());
    let mut workspaces = WorkspaceGuard {
        cmux: cmux.clone(),
        ids: Vec::new(),
    };

    let up_args = [
        "up",
        "--in-cmux",
        "--parallel",
        "2",
        "--cmux",
        cmux.to_str().unwrap(),
        "--claude",
        stub.to_str().unwrap(),
        // The load of a busy host would hold every claim (task 680).
        NO_LOAD_HOLD[0],
        NO_LOAD_HOLD[1],
    ];
    let started = Instant::now();
    let first = dagq_opening(env, &[("HOME", home.as_path())], &up_args, &mut workspaces);
    eprintln!("up --in-cmux took {:?}: {first}", started.elapsed());
    assert_eq!(first["supervisor"]["outcome"], "started", "{first}");
    assert_eq!(first["supervisor"]["mode"], "in_cmux");
    assert_eq!(first["supervisor"]["plist"], Value::Null);
    let repo_name = repo.file_name().unwrap().to_str().unwrap();
    assert_eq!(repo_name, E2E_REPO_NAME);
    assert_eq!(
        first["supervisor"]["name"],
        format!("[{repo_name}]supervisor")
    );
    let supervisor_workspace = first["supervisor"]["workspace_id"]
        .as_str()
        .unwrap()
        .to_owned();
    uuid::Uuid::parse_str(&supervisor_workspace).expect("workspace id is a UUID");
    assert!(workspace_listed(cmux, &supervisor_workspace));
    let pid = u32::try_from(first["supervisor"]["pid"].as_u64().unwrap()).unwrap();
    assert!(pid_alive(pid));
    let ps = Command::new("ps")
        .args(["-o", "command=", "-p", &pid.to_string()])
        .bounded_output()
        .unwrap();
    let command = String::from_utf8_lossy(&ps.stdout);
    assert!(command.contains(" --max-load 0"), "{command}");
    assert_eq!(first["inbox"]["outcome"], "created", "{first}");
    let inbox = first["inbox"]["workspace_id"].as_str().unwrap().to_owned();
    // `up` opens no planner (ADR-0041 decision 6); `plan` does.
    assert_eq!(first.get("planner"), None, "{first}");

    // Every workspace carries its role and the queue in its own
    // environment, and all joined the queue's group (ADR-0026).
    let db = fixture.db.canonicalize().unwrap();
    for (id, role) in [(&supervisor_workspace, "supervisor"), (&inbox, "inbox")] {
        let env = workspace_env(cmux, id);
        assert_eq!(env["DAGQ_ROLE"], role, "{env}");
        assert_eq!(env["DAGQ_QUEUE"], db.to_str().unwrap(), "{env}");
    }
    let group = fixture.group().expect("the queue's workspace group exists");
    assert_eq!(group["name"], "[dagq-e2e]", "{group}");
    let members: Vec<String> = group["member_workspace_ids"]
        .as_array()
        .unwrap()
        .iter()
        .map(|id| id.as_str().unwrap().to_ascii_lowercase())
        .collect();
    for id in [&supervisor_workspace, &inbox] {
        assert!(members.contains(&id.to_ascii_lowercase()), "{group}");
    }
    assert_eq!(
        listed_workspace(cmux, &inbox).unwrap()["description"],
        format!("dagq role=inbox queue={}", fixture.group.external_id)
    );
    // The inbox is Amber, pinned with its role's pill (ADR-0031).
    assert_look(cmux, &inbox, "#7D6608", "dagq_role=inbox icon=tray");
    assert_eq!(
        listed_workspace(cmux, &supervisor_workspace).unwrap()["pinned"],
        false
    );
    // A person renames the inbox workspace; `up` still knows it.
    let rename = Command::new(cmux)
        .args(["workspace", "rename", &inbox, "--title", "renamed by hand"])
        .bounded_output()
        .unwrap();
    assert!(rename.status.success(), "{rename:?}");

    // launchd knows nothing about this queue, and no plist was written.
    assert!(!plist.exists(), "{} exists", plist.display());
    assert!(
        !Command::new("launchctl")
            .args(["print", &format!("gui/{}/{label}", uid())])
            .bounded_output()
            .unwrap()
            .status
            .success(),
        "launchd has an agent for {label}"
    );
    // The supervisor in the workspace writes its JSON Lines log to the
    // queue's log directory, exactly as the launchd-run one does
    // (ADR-0033): the file outlives the workspace.
    let logs: Vec<PathBuf> = fs::read_dir(&log_dir)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .filter(|path| {
            let name = path.file_name().unwrap().to_str().unwrap();
            name.starts_with("supervise-") && name.ends_with(&format!("-{pid}.jsonl"))
        })
        .collect();
    assert_eq!(logs.len(), 1, "{logs:?} in {}", log_dir.display());
    let supervisor_log = logs[0].clone();
    assert!(
        log_messages(&supervisor_log)
            .iter()
            .any(|m| m.contains(&format!(
                "started: version {VERSION}, pid {pid}, parallel 2"
            )))
    );

    // The in-cmux supervisor lands a task, and its integrate progress and
    // the failed push of main (origin does not exist) are records in that
    // file, not only lines on the workspace's screen.
    let task_id = add_ready_task_described(
        env,
        "logged landing",
        "Add e2e.txt to the worktree. E2E-REVIEW-PASS",
        &[],
        &[],
    );
    let missing_origin = repo.parent().unwrap().join("missing-origin.git");
    git(
        repo,
        &["remote", "add", "origin", missing_origin.to_str().unwrap()],
    );
    let deadline = Instant::now() + WAIT_LIMIT;
    let run_id = loop {
        let detail = dagq(env, &["show", &task_id, "--full"]);
        if let Some(id) = detail["runs"][0]["workspace_id"].as_str() {
            workspaces.record(id);
        }
        if detail["task"]["status"] == "completed" {
            break detail["runs"][0]["id"].as_str().unwrap().to_owned();
        }
        assert!(
            Instant::now() < deadline,
            "task {task_id} did not land: {detail}\n{:?}",
            log_messages(&supervisor_log)
        );
        thread::sleep(Duration::from_millis(500));
    };
    // main is pushed after the task completes; wait for its record.
    let pushed = format!("run {run_id}: push of the landing branch failed");
    let records = loop {
        let records = log_records(&supervisor_log);
        if records
            .iter()
            .any(|r| r["message"].as_str().unwrap().contains(&pushed))
        {
            break records;
        }
        assert!(Instant::now() < deadline, "no push record: {records:#?}");
        thread::sleep(Duration::from_millis(200));
    };
    let find = |needle: &str| {
        records
            .iter()
            .find(|r| r["message"].as_str().unwrap().contains(needle))
            .unwrap_or_else(|| panic!("no record with {needle:?} in {records:#?}"))
            .clone()
    };
    let integrating = find(&format!(
        "run {run_id} integrating task {task_id} onto main"
    ));
    assert_eq!(integrating["level"], "INFO");
    assert_eq!(integrating["target"], "dagq::application::integrate");
    assert_eq!(integrating["fields"]["op"], "integrate");
    assert_eq!(integrating["fields"]["run_id"], run_id.as_str());
    assert_eq!(integrating["fields"]["task_id"], task_id.as_str());
    assert_eq!(integrating["spans"][0]["name"], "integrate");
    let landed = find(&format!("task {task_id} landed as"));
    assert_eq!(landed["fields"]["run_id"], run_id.as_str());
    let push = find(&pushed);
    assert_eq!(push["level"], "WARN");
    assert_eq!(push["fields"]["op"], "push");
    assert!(push["fields"]["error"].as_str().is_some(), "{push}");
    eprintln!("in-cmux supervisor log {}:", supervisor_log.display());
    for record in [&integrating, &landed, &push] {
        eprintln!("{record}");
    }

    let status = dagq(env, &["status"]);
    let supervisors = status["supervisors"].as_array().unwrap();
    assert_eq!(supervisors.len(), 1, "{status}");
    assert_eq!(supervisors[0]["pid"], pid);
    assert_eq!(supervisors[0]["mode"], "in_cmux");
    assert_eq!(
        supervisors[0]["workspace_id"],
        supervisor_workspace.as_str()
    );
    assert_eq!(supervisors[0]["stale"], false);
    let doctor = dagq(env, &["doctor", "--full"]);
    assert_eq!(doctor["supervisors"][0]["mode"], "in_cmux", "{doctor}");
    assert_eq!(
        doctor["supervisors"][0]["binary_version"], VERSION,
        "{doctor}"
    );

    // Idempotent: the live supervisor is of this binary's own version, so
    // it is reused with its mode and workspace and nothing is replaced.
    let second = dagq_opening(env, &[("HOME", home.as_path())], &up_args, &mut workspaces);
    assert_eq!(second["supervisor"]["outcome"], "reused", "{second}");
    assert_eq!(second["supervisor"]["version"], VERSION, "{second}");
    assert_eq!(second["supervisor"]["mode"], "in_cmux");
    assert_eq!(
        second["supervisor"]["workspace_id"],
        supervisor_workspace.as_str()
    );
    assert_eq!(second["inbox"]["outcome"], "reused", "{second}");
    assert_eq!(second["inbox"]["workspace_id"], inbox.as_str());
    // The look is put back on a reused workspace that lost it.
    let unpin = Command::new(cmux)
        .args([
            "workspace-action",
            "--action",
            "unpin",
            "--workspace",
            &inbox,
        ])
        .bounded_output()
        .unwrap();
    assert!(unpin.status.success(), "{unpin:?}");
    let third = dagq_opening(env, &[("HOME", home.as_path())], &up_args, &mut workspaces);
    assert_eq!(third["inbox"]["outcome"], "reused", "{third}");
    assert_eq!(third["warnings"], serde_json::json!([]), "{third}");
    assert_look(cmux, &inbox, "#7D6608", "dagq_role=inbox icon=tray");

    // cmux refuses to close a pinned workspace; dagq's close unpins it
    // first, so `down` closes the supervisor's workspace even when a person
    // pinned it.
    let pin = Command::new(cmux)
        .args([
            "workspace-action",
            "--action",
            "pin",
            "--workspace",
            &supervisor_workspace,
        ])
        .bounded_output()
        .unwrap();
    assert!(pin.status.success(), "{pin:?}");
    let deadline = Instant::now() + WAIT_LIMIT;
    while listed_workspace(cmux, &supervisor_workspace).unwrap()["pinned"] != true {
        assert!(Instant::now() < deadline, "the pin never showed up");
        thread::sleep(Duration::from_millis(200));
    }
    let refused = Command::new(cmux)
        .args(["workspace", "close", &supervisor_workspace])
        .bounded_output()
        .unwrap();
    eprintln!("cmux workspace close of a pinned workspace: {refused:?}");
    assert!(!refused.status.success(), "{refused:?}");

    let started = Instant::now();
    let down = dagq_with(env, &[("HOME", home.as_path())], &["down", "--wait"]);
    eprintln!("down --wait took {:?}: {down}", started.elapsed());
    assert_eq!(down["outcome"], "stopped", "{down}");
    assert_eq!(down["pid"], pid);
    assert_eq!(down["launch_agent_unloaded"], false);
    assert_eq!(
        down["supervisor_workspaces"],
        serde_json::json!([{"workspace_id": supervisor_workspace, "outcome": "closed"}]),
        "{down}"
    );
    assert!(!pid_alive(pid), "supervisor {pid} is still alive");
    wait_until_not_listed(cmux, &supervisor_workspace);
    assert_eq!(dagq(env, &["status"])["supervisors"], Value::Array(vec![]));
    // The inbox workspace is left open by `down`; the guard closes it.
    assert!(workspace_listed(cmux, &inbox));
}

/// The planners of the queue as `planners --all` lists them, by ID.
fn planners(env: &Env) -> Vec<Value> {
    dagq(env, &["planners", "--all"])["planners"]
        .as_array()
        .unwrap()
        .clone()
}

/// Type `/exit` at a session's prompt the way the supervisor does.
fn send_exit(cmux: &Path, workspace: &str) {
    for args in [
        &["send", "--workspace", workspace, "--", "/exit"][..],
        &["send-key", "--workspace", workspace, "--", "enter"],
    ] {
        let output = Command::new(cmux).args(args).bounded_output().unwrap();
        assert!(output.status.success(), "{args:?}: {output:?}");
    }
}

/// `dagq plan` opens planners on demand, side by side (ADR-0041 decision
/// 6): each in its own workspace `[<repo>]planner#<id>` with the
/// planner's role, queue, origin and ID in its environment, in the queue's
/// group, Blue and not pinned. Each runs the session wrapper, whose agent
/// (the stub) submits a proposal owned by that workspace and goes idle;
/// `planners` reports each alive and idle, then the one sent `/exit` as no
/// longer alive with its exit code, and closed once its workspace is gone.
#[test]
#[ignore = "needs a running cmux; run with --ignored"]
fn plan_opens_planners_side_by_side_that_submit_go_idle_and_exit() {
    let fixture = fixture();
    let Fixture {
        cmux,
        repo,
        stub,
        env,
        db,
        ..
    } = &fixture;
    let mut workspaces = WorkspaceGuard {
        cmux: cmux.clone(),
        ids: Vec::new(),
    };
    let plugin_dir = fixture._dir.path().join("plugin");
    fs::create_dir(&plugin_dir).unwrap();
    let plan_args = [
        "plan",
        "--cmux",
        cmux.to_str().unwrap(),
        "--claude",
        stub.to_str().unwrap(),
        "--plugin-dir",
        plugin_dir.to_str().unwrap(),
    ];
    let first = dagq_opening(env, &[], &plan_args, &mut workspaces);
    let second = dagq_opening(env, &[], &plan_args, &mut workspaces);
    eprintln!("plan: {first}\nplan: {second}");
    let repo_name = repo.file_name().unwrap().to_str().unwrap();
    let db = db.canonicalize().unwrap();
    let group = fixture.group().expect("the queue's workspace group exists");
    let members: Vec<String> = group["member_workspace_ids"]
        .as_array()
        .unwrap()
        .iter()
        .map(|id| id.as_str().unwrap().to_ascii_lowercase())
        .collect();
    let mut ids = Vec::new();
    for (report, planner) in [(&first, 1), (&second, 2)] {
        assert_eq!(report["planner"]["id"], planner, "{report}");
        assert_eq!(report["name"], format!("[{repo_name}]planner#{planner}"));
        assert_eq!(report["warnings"], json!([]), "{report}");
        let id = report["planner"]["workspace_id"]
            .as_str()
            .unwrap()
            .to_owned();
        uuid::Uuid::parse_str(&id).expect("workspace id is a UUID");
        let listed = listed_workspace(cmux, &id).expect("the planner workspace is listed");
        assert_eq!(listed["title"], format!("[{repo_name}]planner#{planner}"));
        assert_eq!(listed["pinned"], false, "{listed}");
        assert_eq!(listed["custom_color"], "#1565C0", "{listed}");
        let workspace_env = workspace_env(cmux, &id);
        assert_eq!(workspace_env["DAGQ_ROLE"], "planner", "{workspace_env}");
        assert_eq!(workspace_env["DAGQ_QUEUE"], db.to_str().unwrap());
        assert_eq!(workspace_env["DAGQ_PLANNER_ORIGIN"], "person");
        assert_eq!(workspace_env["DAGQ_PLANNER_ID"], planner.to_string());
        assert!(members.contains(&id.to_ascii_lowercase()), "{group}");
        ids.push(id);
    }
    assert_ne!(ids[0], ids[1]);

    // Both planners run at once: each submits its own proposal and goes
    // idle, which `planners` reports from its wrapper and idle marker.
    let deadline = Instant::now() + WAIT_LIMIT;
    loop {
        let listed = planners(env);
        if listed.iter().all(|planner| planner["state"] == "idle") && listed.len() == 2 {
            for planner in &listed {
                assert_eq!(planner["alive"], true, "{planner}");
                assert!(planner["idle_since"].is_i64(), "{planner}");
                assert!(planner["wrapper_pid"].is_u64(), "{planner}");
                assert!(planner["agent_pid"].is_u64(), "{planner}");
            }
            break;
        }
        assert!(
            Instant::now() < deadline,
            "planners never went idle: {listed:#?}"
        );
        thread::sleep(Duration::from_millis(300));
    }
    let proposals = dagq(env, &["proposal", "list"]);
    let mut owners: Vec<String> = proposals["proposals"]
        .as_array()
        .unwrap()
        .iter()
        .map(|proposal| {
            assert_eq!(proposal["owner"]["origin"], "person", "{proposal}");
            proposal["owner"]["workspace_id"]
                .as_str()
                .unwrap()
                .to_ascii_lowercase()
        })
        .collect();
    owners.sort();
    let mut expected: Vec<String> = ids.iter().map(|id| id.to_ascii_lowercase()).collect();
    expected.sort();
    assert_eq!(owners, expected, "{proposals}");
    for (report, planner) in [(&first, 1), (&second, 2)] {
        let dir = PathBuf::from(report["dir"].as_str().unwrap());
        let debug = fs::read_to_string(dir.join("claude.log")).unwrap();
        eprintln!("planner {planner}: {debug}");
        assert!(
            debug.contains(&format!(
                "--plugin-dir {}",
                plugin_dir.canonicalize().unwrap().display()
            )),
            "{debug}"
        );
        assert!(
            debug.contains(&format!(
                "env: DAGQ_ROLE=planner DAGQ_PLANNER_ORIGIN=person DAGQ_PLANNER_ID={planner}"
            )),
            "{debug}"
        );
    }

    // The first planner is sent /exit: its wrapper records the exit and it
    // is no longer alive; the second is still idle.
    send_exit(cmux, &ids[0]);
    let deadline = Instant::now() + WAIT_LIMIT;
    let exited = loop {
        let listed = planners(env);
        if listed[0]["exit_code"] == 0 {
            break listed;
        }
        assert!(
            Instant::now() < deadline,
            "planner 1 never exited: {listed:#?}"
        );
        thread::sleep(Duration::from_millis(300));
    };
    assert!(
        exited[0]["state"] == "exited" || exited[0]["state"] == "closed",
        "{exited:#?}"
    );
    assert_eq!(exited[0]["alive"], false);
    assert_eq!(exited[1]["state"], "idle", "{exited:#?}");

    // A person closes the first planner's workspace (cmux may already have
    // closed it with its command): `planners` reports it closed, and the
    // next `plan` opens a third.
    if workspace_listed(cmux, &ids[0]) {
        let close = Command::new(cmux)
            .args(["workspace", "close", &ids[0]])
            .bounded_output()
            .unwrap();
        assert!(close.status.success(), "{close:?}");
    }
    wait_until_not_listed(cmux, &ids[0]);
    let third = dagq_opening(env, &[], &plan_args, &mut workspaces);
    assert_eq!(third["planner"]["id"], 3, "{third}");
    let states: Vec<Value> = planners(env)
        .iter()
        .map(|planner| planner["state"].clone())
        .collect();
    assert_eq!(states[0], "closed", "{states:?}");
    assert_eq!(states[1], "idle", "{states:?}");
    assert_eq!(states.len(), 3, "{states:?}");
    send_exit(cmux, &ids[1]);
}

/// `install` hands a supervisor over to the new binary while its worker
/// still works (ADR-0045 decision 10): the supervisor process execs the
/// installed file under its own pid and token, the session in its cmux
/// workspace is not touched, and the continued supervisor watches the run
/// through its receipt, `/exit`, review and landing on main. `--rollback`
/// hands it over again to the binary the install kept.
#[test]
#[ignore = "needs a running cmux; run with --ignored"]
fn install_hands_the_supervisor_over_while_a_session_works_and_the_run_lands() {
    let fixture = fixture();
    let Fixture {
        cmux, repo, env, ..
    } = &fixture;
    let task_id = add_ready_task_described(
        env,
        "e2e handoff task",
        "Add e2e.txt to the worktree. E2E-HOLD E2E-REVIEW-PASS",
        &[],
        &[],
    );
    let mut guard = WorkspaceGuard {
        cmux: cmux.clone(),
        ids: Vec::new(),
    };
    // The fixed binary the supervisor runs and `install` replaces.
    let fixed = fixture._dir.path().join("bin").join("dagq");
    fs::create_dir_all(fixed.parent().unwrap()).unwrap();
    fs::copy(BIN, &fixed).unwrap();
    let fixed_text = fixed.to_str().unwrap();
    let mut supervisor = ChildGuard::new(
        Command::new(&fixed)
            // A person's supervisor, not the session's running the tests.
            .without_actor_env()
            .current_dir(repo)
            .env("XDG_DATA_HOME", &env.data_home)
            .args(["supervise", "--parallel", "1", "--observe-interval", "0"])
            .args(NO_LOAD_HOLD)
            .arg("--cmux")
            .arg(cmux)
            .arg("--claude")
            .arg(&fixture.stub)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap(),
    );
    let stderr = reader(supervisor.0.stderr.take().unwrap());
    let pid = supervisor.0.id();
    let started = Instant::now();
    let run = loop {
        assert!(
            supervisor.0.try_wait().unwrap().is_none(),
            "the supervisor exited before the worker started"
        );
        assert!(
            started.elapsed() < SUPERVISE_TIMEOUT,
            "the worker did not start within {SUPERVISE_TIMEOUT:?}"
        );
        let detail = dagq(env, &["show", &task_id, "--full"]);
        if let Some(run) = detail["runs"].as_array().unwrap().last()
            && run["status"] == "running"
        {
            break run.clone();
        }
        thread::sleep(Duration::from_millis(200));
    };
    let run_id = run["id"].as_str().unwrap().to_owned();
    let workspace = run["workspace_id"].as_str().unwrap().to_owned();
    guard.record(&workspace);
    let run_dir = PathBuf::from(run["run_dir"].as_str().unwrap());

    let installed = dagq(
        env,
        &[
            "install",
            "--from",
            BIN,
            "--to",
            fixed_text,
            "--handoff-timeout",
            "120",
        ],
    );
    eprintln!("install: {installed}");
    assert_eq!(installed["outcome"], "installed", "{installed}");
    assert_eq!(installed["version"], VERSION);
    assert_eq!(installed["supervisors"][0]["pid"], pid, "{installed}");
    let token = installed["supervisors"][0]["token"]
        .as_str()
        .unwrap()
        .to_owned();
    assert!(fixed.with_file_name("dagq.previous").is_file());
    // The same process, the same registration; the session goes on.
    assert!(supervisor.0.try_wait().unwrap().is_none());
    let status = dagq(env, &["status"]);
    let supervisors = status["supervisors"].as_array().unwrap();
    assert_eq!(supervisors.len(), 1, "{status}");
    assert_eq!(supervisors[0]["pid"], pid);
    assert_eq!(supervisors[0]["binary_version"], VERSION);
    assert_eq!(status["runs"][0]["run_id"], run_id.as_str(), "{status}");
    assert_eq!(status["runs"][0]["status"], "running", "{status}");
    assert!(workspace_listed(cmux, &workspace));

    // Let the worker finish: the continued supervisor lands the run.
    fs::write(run_dir.join("go"), "").unwrap();
    let mut stderr = Some(stderr);
    let landed = loop {
        if started.elapsed() >= SUPERVISE_TIMEOUT * 2 {
            let _ = supervisor.0.kill();
            let log = joined(stderr.take().unwrap(), "the supervisor's stderr reader");
            panic!("the run did not land; supervisor stderr:\n{log}");
        }
        let detail = dagq(env, &["show", &task_id, "--full"]);
        if detail["task"]["status"] == "completed" {
            break detail;
        }
        thread::sleep(Duration::from_millis(300));
    };
    let events = landed["events"].as_array().unwrap();
    let kinds: Vec<&str> = events.iter().map(|e| e["kind"].as_str().unwrap()).collect();
    let count = |kind: &str| kinds.iter().filter(|k| **k == kind).count();
    assert_eq!(count("supervisor_handed_off"), 1, "{kinds:?}");
    assert_eq!(count("run_adopted"), 0, "{kinds:?}");
    assert_eq!(count("exit_requested"), 1, "{kinds:?}");
    assert_eq!(count("lease_acquired"), 1, "{kinds:?}");
    assert!(!kinds.contains(&"runtime_error"), "{kinds:?}");
    let position = |kind: &str| kinds.iter().position(|k| *k == kind).unwrap();
    assert!(position("supervisor_handed_off") < position("receipt_observed"));
    assert_eq!(landed["runs"].as_array().unwrap().len(), 1);
    assert_eq!(landed["runs"][0]["status"], "integrated");
    assert_eq!(
        git(repo, &["show", "main:e2e.txt"]),
        format!("written by the stub agent for {run_id}")
    );

    // Back to the binary the install kept, the same way.
    let rolled = dagq(
        env,
        &[
            "install",
            "--rollback",
            "--to",
            fixed_text,
            "--handoff-timeout",
            "120",
        ],
    );
    assert_eq!(rolled["supervisors"][0]["pid"], pid, "{rolled}");
    assert_eq!(rolled["supervisors"][0]["token"], token.as_str());
    assert!(supervisor.0.try_wait().unwrap().is_none());

    // SIGINT drains the continued supervisor like any other.
    unsafe { libc::kill(pid as i32, libc::SIGINT) };
    let deadline = Instant::now() + WAIT_LIMIT;
    let exit = loop {
        if let Some(exit) = supervisor.0.try_wait().unwrap() {
            supervisor.reaped();
            break exit;
        }
        assert!(Instant::now() < deadline, "the supervisor did not stop");
        thread::sleep(Duration::from_millis(200));
    };
    let stderr = joined(stderr.take().unwrap(), "the supervisor's stderr reader");
    eprintln!("supervisor stderr:\n{stderr}");
    assert!(exit.success(), "{exit}");
    assert_eq!(
        stderr
            .matches(&format!("supervisor {token} handed off: version {VERSION}"))
            .count(),
        2
    );
    assert!(!stderr.contains("could not exec"), "{stderr}");
    assert!(
        dagq(env, &["status"])["supervisors"]
            .as_array()
            .unwrap()
            .is_empty()
    );
}

/// `supervise --auto-update` (ADR-0045 decision 17) with a real cmux worker
/// at work: a commit that changes the runtime lands on main, the supervisor's
/// job builds it in the queue's own checkout (a stub build copies the
/// binary under test), puts it in place and hands the supervisor over under
/// its pid and token, and the worker's run goes on and lands.
#[test]
#[ignore = "needs a running cmux; run with --ignored"]
fn auto_update_hands_the_supervisor_over_while_a_session_works_and_the_run_lands() {
    let fixture = fixture();
    let Fixture {
        cmux,
        repo,
        env,
        db,
        ..
    } = &fixture;
    let task_id = add_ready_task_described(
        env,
        "e2e auto-update task",
        "Add e2e.txt to the worktree. E2E-HOLD E2E-REVIEW-PASS",
        &[],
        &[],
    );
    let mut guard = WorkspaceGuard {
        cmux: cmux.clone(),
        ids: Vec::new(),
    };
    // The automatic update builds only dagq's source (ADR-t614-1).
    fs::write(repo.join("Cargo.toml"), "[package]\nname = \"dagq\"\n").unwrap();
    git(repo, &["add", "Cargo.toml"]);
    git(repo, &["commit", "-q", "-m", "dagq's manifest"]);
    let fixed = fixture._dir.path().join("bin").join("dagq");
    fs::create_dir_all(fixed.parent().unwrap()).unwrap();
    fs::copy(BIN, &fixed).unwrap();
    let build = format!(
        "mkdir -p \"$CARGO_TARGET_DIR/release\" && cp '{BIN}' \"$CARGO_TARGET_DIR/release/dagq\""
    );
    let mut supervisor = ChildGuard::new(
        Command::new(&fixed)
            // A person's supervisor, not the session's running the tests.
            .without_actor_env()
            .current_dir(repo)
            .env("XDG_DATA_HOME", &env.data_home)
            .args(["supervise", "--parallel", "1", "--observe-interval", "0"])
            .args(NO_LOAD_HOLD)
            .arg("--cmux")
            .arg(cmux)
            .arg("--claude")
            .arg(&fixture.stub)
            .args(["--auto-update", "--update-interval", "1"])
            .arg("--update-build-command")
            .arg(&build)
            // The e2e gate (ADR-t963-1) passes without running the e2e
            // again inside this one.
            .args(["--update-e2e-command", "true"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap(),
    );
    let stderr = reader(supervisor.0.stderr.take().unwrap());
    let pid = supervisor.0.id();
    let updates = || {
        dagq::infrastructure::sqlite::SqliteQueue::open(db)
            .unwrap()
            .update_events(100)
            .unwrap()
    };
    let installed = |sha: &str| {
        updates()
            .iter()
            .any(|u| u.kind == "update_installed" && u.payload["commit"] == sha)
    };
    let started = Instant::now();
    let run = loop {
        assert!(
            supervisor.0.try_wait().unwrap().is_none(),
            "the supervisor exited before the worker started"
        );
        assert!(
            started.elapsed() < SUPERVISE_TIMEOUT,
            "the worker did not start within {SUPERVISE_TIMEOUT:?}"
        );
        let detail = dagq(env, &["show", &task_id, "--full"]);
        if let Some(run) = detail["runs"].as_array().unwrap().last()
            && run["status"] == "running"
        {
            break run.clone();
        }
        thread::sleep(Duration::from_millis(200));
    };
    let run_id = run["id"].as_str().unwrap().to_owned();
    let workspace = run["workspace_id"].as_str().unwrap().to_owned();
    guard.record(&workspace);
    let run_dir = PathBuf::from(run["run_dir"].as_str().unwrap());

    // A change of the runtime lands on main while the worker works.
    fs::create_dir_all(repo.join("src")).unwrap();
    fs::write(repo.join("src").join("lib.rs"), "// landed\n").unwrap();
    git(repo, &["add", "src/lib.rs"]);
    git(repo, &["commit", "-q", "-m", "runtime change"]);
    let landed = git(repo, &["rev-parse", "main"]).trim().to_owned();
    let mut stderr = Some(stderr);
    while !installed(&landed) {
        if started.elapsed() >= SUPERVISE_TIMEOUT {
            let _ = supervisor.0.kill();
            let log = joined(stderr.take().unwrap(), "the supervisor's stderr reader");
            panic!(
                "{landed} was not installed: {:?}\nsupervisor stderr:\n{log}",
                updates()
            );
        }
        thread::sleep(Duration::from_millis(300));
    }
    assert!(fixed.with_file_name("dagq.previous").is_file());
    assert!(supervisor.0.try_wait().unwrap().is_none());
    let status = dagq(env, &["status"]);
    assert_eq!(status["auto_update"]["state"], "installed", "{status}");
    assert_eq!(status["auto_update"]["commit"], landed.as_str(), "{status}");
    let supervisors = status["supervisors"].as_array().unwrap();
    assert_eq!(supervisors.len(), 1, "{status}");
    assert_eq!(supervisors[0]["pid"], pid);
    assert_eq!(supervisors[0]["auto_update"], true);
    assert_eq!(status["runs"][0]["run_id"], run_id.as_str(), "{status}");
    assert_eq!(status["runs"][0]["status"], "running", "{status}");
    assert!(workspace_listed(cmux, &workspace));

    // The worker finishes; the continued supervisor lands its run.
    fs::write(run_dir.join("go"), "").unwrap();
    let detail = loop {
        if started.elapsed() >= SUPERVISE_TIMEOUT * 2 {
            let _ = supervisor.0.kill();
            let log = joined(stderr.take().unwrap(), "the supervisor's stderr reader");
            panic!("the run did not land; supervisor stderr:\n{log}");
        }
        let detail = dagq(env, &["show", &task_id, "--full"]);
        if detail["task"]["status"] == "completed" {
            break detail;
        }
        thread::sleep(Duration::from_millis(300));
    };
    let kinds: Vec<&str> = detail["events"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["kind"].as_str().unwrap())
        .collect();
    assert!(kinds.contains(&"supervisor_handed_off"), "{kinds:?}");
    assert!(!kinds.contains(&"run_adopted"), "{kinds:?}");
    assert_eq!(detail["runs"][0]["status"], "integrated");
    // The run's timeline shows the update that happened while it worked
    // (ADR-0073 decision 17, task 496).
    let timeline = dagq(env, &["timeline", &run_id]);
    assert!(
        timeline["events"]
            .as_array()
            .unwrap()
            .iter()
            .any(|e| e["kind"] == "update_installed" && e["commit"] == landed.as_str()),
        "{timeline}"
    );

    unsafe { libc::kill(pid as i32, libc::SIGINT) };
    let deadline = Instant::now() + WAIT_LIMIT;
    let exit = loop {
        if let Some(exit) = supervisor.0.try_wait().unwrap() {
            supervisor.reaped();
            break exit;
        }
        assert!(Instant::now() < deadline, "the supervisor did not stop");
        thread::sleep(Duration::from_millis(200));
    };
    let stderr = joined(stderr.take().unwrap(), "the supervisor's stderr reader");
    assert!(exit.success(), "{exit}\n{stderr}");
    assert!(!stderr.contains("could not exec"), "{stderr}");
}

/// Run `cmux args`, which must succeed, and return what it printed.
fn cmux_ok(cmux: &Path, args: &[&str]) -> String {
    let output = Command::new(cmux).args(args).bounded_output().unwrap();
    assert!(output.status.success(), "cmux {args:?}: {output:?}");
    String::from_utf8(output.stdout).unwrap()
}

/// A cmux window the test opened, closed with whatever is left in it when
/// the test ends.
struct WindowGuard {
    cmux: PathBuf,
    id: String,
    /// The workspaces the window may be closed with: the one cmux opened in
    /// it and those the test moved there.
    own: Vec<String>,
}

impl Drop for WindowGuard {
    fn drop(&mut self) {
        // Workspaces opened elsewhere without `--window` (another e2e, a
        // supervisor's run) can land in this window while it is focused:
        // leave it open rather than close them with it.
        let others: Vec<String> = Command::new(&self.cmux)
            .args(["--json", "--id-format", "uuids", "workspace", "list"])
            .args(["--window", &self.id])
            .bounded_output()
            .ok()
            .and_then(|output| serde_json::from_slice::<Value>(&output.stdout).ok())
            .and_then(|list| list["workspaces"].as_array().cloned())
            .unwrap_or_default()
            .iter()
            .filter_map(|w| w["id"].as_str().map(str::to_owned))
            .filter(|id| !self.own.iter().any(|own| own.eq_ignore_ascii_case(id)))
            .collect();
        if !others.is_empty() {
            eprintln!(
                "left window {} open: it holds workspaces the test did not open: {others:?}",
                self.id
            );
            return;
        }
        match Command::new(&self.cmux)
            .args(["close-window", "--window", &self.id])
            .bounded_output()
        {
            Ok(output) if output.status.success() => eprintln!("closed window {}", self.id),
            Ok(output) => eprintln!("closing window {} failed: {output:?}", self.id),
            Err(error) => eprintln!("closing window {} failed: {error}", self.id),
        }
    }
}

/// Move `ids` to `window` and wait until its listing shows them: cmux
/// confirms a move before its listing does.
fn move_to_window(cmux: &Path, ids: &[&String], window: &str) {
    for id in ids {
        cmux_ok(
            cmux,
            &[
                "move-workspace-to-window",
                "--workspace",
                id,
                "--window",
                window,
            ],
        );
    }
    let deadline = Instant::now() + WAIT_LIMIT;
    loop {
        let in_window: Value = serde_json::from_str(&cmux_ok(
            cmux,
            &[
                "--json",
                "--id-format",
                "uuids",
                "workspace",
                "list",
                "--window",
                window,
            ],
        ))
        .unwrap();
        let moved = |id: &&String| {
            in_window["workspaces"]
                .as_array()
                .unwrap()
                .iter()
                .any(|w| w["id"].as_str().unwrap().eq_ignore_ascii_case(id))
        };
        if ids.iter().all(moved) {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "not moved to window {window}: {in_window}"
        );
        thread::sleep(Duration::from_millis(200));
    }
}

/// Whether the sweep e2e moves its workspaces to a window of its own. It is
/// off unless `DAGQ_E2E_WINDOWS=1`: while a window opens or closes, cmux
/// answers `workspace list --window` for it with "TabManager not available",
/// and the runtime's listing of every window (`planners`, `up`, the
/// supervisor) fails on that, so a default e2e that opens a window would
/// break the other e2e tests and runs on the host that list at that moment.
fn window_e2e_enabled() -> bool {
    if env::var("DAGQ_E2E_WINDOWS").as_deref() == Ok("1") {
        return true;
    }
    eprintln!(
        "the sweep e2e keeps its workspaces in this window; set DAGQ_E2E_WINDOWS=1 to \
         move them to a new window (only when nothing else on the host lists cmux)"
    );
    false
}

/// What an e2e leaves in cmux when a workspace escaped its guard and the
/// fixture's temporary directory is gone (2026-09-25: a pinned inbox and
/// planner pointing into a removed `$TMPDIR/.tmp…`) is found by the next
/// sweep: it unpins and closes the workspace and deletes its queue's group.
/// A workspace of a live fixture, whose directory exists, is left alone.
/// With `DAGQ_E2E_WINDOWS=1` both sit in another window, where
/// `listed_workspace` and the sweep still find them.
#[test]
#[ignore = "needs a running cmux; run with --ignored"]
fn the_sweep_closes_workspaces_left_in_any_window_after_their_fixture_dir_is_gone() {
    let fixture = fixture();
    let cmux = &fixture.cmux;
    // Declared first so it drops last, after the guard below has unpinned
    // and closed the workspaces in it.
    let mut window = window_e2e_enabled().then(|| {
        // cmux 0.64 prints `OK <window UUID>`, `--json` or not.
        let created = cmux_ok(cmux, &["--json", "--id-format", "uuids", "new-window"]);
        let id = created
            .split(|c: char| !(c.is_ascii_hexdigit() || c == '-'))
            .find(|word| uuid::Uuid::parse_str(word).is_ok())
            .unwrap_or_else(|| panic!("new-window printed no window UUID: {created}"))
            .to_owned();
        let own = serde_json::from_str::<Value>(&cmux_ok(
            cmux,
            &[
                "--json",
                "--id-format",
                "uuids",
                "workspace",
                "list",
                "--window",
                &id,
            ],
        ))
        .unwrap()["workspaces"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|w| w["id"].as_str().map(str::to_owned))
            .collect();
        WindowGuard {
            cmux: cmux.clone(),
            id,
            own,
        }
    });
    let mut guard = WorkspaceGuard {
        cmux: cmux.clone(),
        ids: Vec::new(),
    };
    // A fixture directory of a dead e2e, with its queue's group, and one
    // workspace of each: the dead one's and the live fixture's.
    let gone = tempfile::tempdir().unwrap();
    let hash = format!("e2esweep{}", uuid::Uuid::new_v4().simple());
    let _group = GroupGuard::new(cmux.clone(), hash.clone());
    let group: Value = serde_json::from_str(&cmux_ok(
        cmux,
        &[
            "--json",
            "--id-format",
            "uuids",
            "workspace-group",
            "create",
            "--name",
            "[dagq-e2e-sweep]",
            "--external-id",
            &hash,
        ],
    ))
    .unwrap();
    let group_id = group["group"]["id"].as_str().unwrap();
    let queue = gone
        .path()
        .canonicalize()
        .unwrap()
        .join("data/dagq")
        .join(&hash)
        .join("queue.db");
    let shared = fixture._dir.path().join("data/dagq/shared");
    let mut create = |name: &str, env: String, group: Option<&str>| {
        let mut args = vec!["--json", "--id-format", "uuids", "workspace", "create"];
        args.extend(["--name", name, "--env", &env, "--focus", "false"]);
        if let Some(group) = group {
            args.extend(["--group", group]);
        }
        let output = Command::new(cmux).args(&args).bounded_output().unwrap();
        guard.record_opened(&output.stdout);
        assert!(output.status.success(), "{output:?}");
        let created: Value = serde_json::from_slice(&output.stdout).unwrap();
        created["workspace_id"].as_str().unwrap().to_owned()
    };
    let left = create(
        "e2e-sweep-left",
        format!("DAGQ_QUEUE={}", queue.display()),
        Some(group_id),
    );
    let live = create(
        "e2e-sweep-live",
        format!("E2E_SHARED={}", shared.display()),
        None,
    );
    for id in [&left, &live] {
        cmux_ok(
            cmux,
            &["workspace-action", "--action", "pin", "--workspace", id],
        );
    }
    if let Some(window) = &mut window {
        window.own.extend([left.clone(), live.clone()]);
        move_to_window(cmux, &[&left, &live], &window.id);
    }
    for id in [&left, &live] {
        let listed = listed_workspace(cmux, id).expect("the workspace is listed");
        assert_eq!(listed["pinned"], true, "{listed}");
    }
    eprintln!(
        "{} workspaces in {} windows",
        all_workspaces(cmux).unwrap().len(),
        cmux_ok(cmux, &["--json", "list-windows"])
            .matches("\"id\"")
            .count()
    );
    assert!(listed_group(cmux, &hash).is_some());

    drop(gone);
    sweep_abandoned_fixtures(cmux);
    wait_until_not_listed(cmux, &left);
    assert_eq!(listed_group(cmux, &hash), None, "the group was not deleted");
    let live = listed_workspace(cmux, &live).expect("the live fixture's workspace is left alone");
    assert_eq!(live["pinned"], true, "{live}");
}
