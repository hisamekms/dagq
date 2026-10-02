//! End-to-end happy paths of the headless worker (ADR-t813-1): a Claude
//! worker's turn (`claude -p`) and a Codex worker's (`codex exec`, ADR-t813-3),
//! each through the session wrapper in a real cmux workspace to the landing.
use super::*;

/// A task for a headless Claude worker (ADR-t813-1) runs its turn as
/// `claude -p --output-format stream-json` under the session wrapper in a
/// real cmux workspace: the turn commits and writes its receipt, the
/// wrapper records the turn and writes the idle marker, and the run is
/// validated, reviewed and landed; its exit is the exit request in
/// `turns/`, not a `/exit` typed into the terminal.
#[test]
#[ignore = "needs a running cmux; run with --ignored"]
fn a_headless_worker_runs_its_turn_through_cmux_and_lands_on_main() {
    let fixture = fixture();
    let Fixture {
        cmux,
        repo,
        base,
        env,
        ..
    } = &fixture;
    let task_id = dagq(
        env,
        &[
            "add",
            "e2e headless task",
            "--description",
            "Add e2e.txt to the worktree. E2E-REVIEW-PASS",
            "--acceptance",
            "e2e.txt is committed and seed.txt still exists",
            "--verify",
            "test -f seed.txt",
            "--verify",
            "test -f e2e.txt",
            "--provider",
            "claude",
            "--headless",
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
    let pass = supervise_once(&fixture, &[], &[&task_id], &mut guard);
    let outcome = &pass.outcome;
    assert_eq!(outcome["errors"], Value::Array(vec![]), "{outcome}");
    assert_eq!(outcome["runs"][0]["status"], "integrated", "{outcome}");
    assert_eq!(outcome["runs"][0]["worker_mode"], "headless", "{outcome}");
    let detail = dagq(env, &["show", &task_id, "--full"]);
    assert_eq!(detail["task"]["status"], "completed");
    let run = &detail["runs"][0];
    let run_id = run["id"].as_str().unwrap();
    let run_dir = Path::new(run["run_dir"].as_str().unwrap());
    let main = git(repo, &["rev-parse", "main"]);
    assert_eq!(git(repo, &["rev-parse", "main^"]), base.as_str());
    assert_eq!(
        fs::read_to_string(repo.join("e2e.txt")).unwrap(),
        format!("written by the headless stub agent for {run_id}\n")
    );
    assert_eq!(run["result_commit"], main.as_str());
    let log = fs::read_to_string(run["log_path"].as_str().unwrap()).unwrap();
    assert!(
        log.contains(&format!(
            "turn argv: -p --output-format stream-json --session-id {run_id} --resume  --permission-mode auto --add-dir {run_dir} --settings {run_dir}/claude-headless-settings.json --model claude-opus-5-5",
            run_dir = run_dir.display()
        )),
        "{log}"
    );
    // The turn's output is kept; the exit was the exit request.
    let output = fs::read_to_string(run_dir.join("turns/turn-000001.jsonl")).unwrap();
    assert!(output.contains("\"type\":\"result\""), "{output}");
    assert!(run_dir.join("turns/exit").exists());
    let events = detail["events"].as_array().unwrap();
    let kinds: Vec<&str> = events.iter().map(|e| e["kind"].as_str().unwrap()).collect();
    let position = |kind: &str| {
        kinds
            .iter()
            .position(|k| *k == kind)
            .unwrap_or_else(|| panic!("missing {kind} in {kinds:?}"))
    };
    let order = [
        "workspace_created",
        "wrapper_started",
        "agent_started",
        "turn_started",
        "turn_finished",
        "session_idle_observed",
        "validation_finished",
        "review_finished",
        "exit_requested",
        "session_exited",
        "run_integrated",
    ];
    for pair in order.windows(2) {
        assert!(
            position(pair[0]) < position(pair[1]),
            "{} before {}: {kinds:?}",
            pair[0],
            pair[1]
        );
    }
    // The receipt is seen while the turn runs or right after it, but
    // before its idle marker ends the session's watch.
    assert!(position("receipt_observed") < position("session_idle_observed"));
    let finished = &events[position("turn_finished")]["payload"];
    assert_eq!(finished["outcome"], "succeeded", "{finished}");
    assert_eq!(finished["session_id"], run_id);
    assert_eq!(finished["usage"]["input_tokens"], 11);
    assert_eq!(finished["cost_usd"], 0.02);
    assert_eq!(
        events[position("session_exited")]["payload"]["exit_code"],
        0
    );
    wait_until_not_listed(cmux, &pass.workspaces[0].1);
}

/// Stand-in for Codex CLI's headless turns (ADR-t813-1, ADR-t813-3). It
/// accepts `codex exec --json -C <worktree> -c … -- <prompt>` (and `codex
/// exec resume --json -c … -- <thread> <prompt>`), checks that the sandbox
/// of ADR-t813-3 is given as `-c`, does the task (commit, receipt) and
/// prints Codex's JSONL: the thread, an agent message, the turn's usage.
const CODEX_STUB: &str = r#"#!/bin/sh
set -eu
if [ "${1:-}" = "--version" ]; then
  printf 'codex-cli stub\n'
  exit 0
fi
[ "${1:-}" = exec ] || { printf 'stub: not codex exec\n' >&2; exit 64; }
shift
mode=start; json= cd_dir= sandbox= approval= network= roots= effort=
if [ "${1:-}" = resume ]; then mode=resume; shift; fi
while [ $# -gt 0 ]; do
  case "$1" in
    --json) json=1; shift ;;
    -C) cd_dir=$2; shift 2 ;;
    -m) shift 2 ;;
    -c)
      case "$2" in
        'sandbox_mode="workspace-write"') sandbox=1 ;;
        'approval_policy="never"') approval=1 ;;
        sandbox_workspace_write.network_access=true) network=1 ;;
        sandbox_workspace_write.writable_roots=*) roots=$2 ;;
        model_reasoning_effort=*) effort=$2 ;;
      esac
      shift 2 ;;
    --) shift; break ;;
    *) printf 'stub: unexpected argument %s\n' "$1" >&2; exit 64 ;;
  esac
done
if [ "$mode" = resume ]; then thread=$1; prompt=$2; else thread="e2e-thread-$DAGQ_RUN_ID"; prompt=$1; fi
[ -n "$json" ] && [ -n "$sandbox" ] && [ -n "$approval" ] && [ -n "$network" ] && [ -n "$roots" ] && [ -n "$effort" ] \
  || { printf 'stub: the sandbox is not given\n' >&2; exit 64; }
[ "$mode" = resume ] || [ "$(cd "$cd_dir" && pwd -P)" = "$(pwd -P)" ] || { printf 'stub: -C is not the worktree\n' >&2; exit 64; }
case "$roots" in *'/refs/heads/dagq"'*'/registry"'*']') ;; *) printf 'stub: writable roots %s\n' "$roots" >&2; exit 64 ;; esac
[ -f .codex/rules/dagq-deny.rules ] || { printf 'stub: no rules in the worktree\n' >&2; exit 64; }
printf '{"type":"thread.started","thread_id":"%s"}\n{"type":"turn.started"}\n' "$thread"
receipt=$(printf '%s\n' "$prompt" | sed -n 's/^Write a completion receipt to \(.*\) using a temporary file in the same directory.*/\1/p')
[ -n "$receipt" ] || { printf 'stub: prompt does not name the receipt path\n' >&2; exit 65; }
printf 'codex turn argv: %s %s %s\n' "$mode" "$thread" "$roots" > "$(dirname "$receipt")/codex-argv.log"
printf 'codex turn tmpdir: %s\n' "${TMPDIR:-}" >> "$(dirname "$receipt")/codex-argv.log"
printf 'written by the codex stub agent for %s\n' "$thread" > e2e.txt
git add e2e.txt
git commit -q -m 'feat: e2e codex stub change'
commit=$(git rev-parse HEAD)
printf '{"run_id":"%s","result":"succeeded","commit":"%s","tests":{"status":"passed","evidence_or_reason":"test -f seed.txt exited 0"},"e2e":{"status":"not_applicable","evidence_or_reason":"stub agent"},"subagent_review":{"status":"not_applicable","evidence_or_reason":"stub agent"},"summary":"added e2e.txt with codex"}\n' \
  "$DAGQ_RUN_ID" "$commit" > "$receipt.tmp"
mv "$receipt.tmp" "$receipt"
printf '{"type":"item.started","item":{"id":"c1","type":"command_execution","command":"git commit","status":"in_progress"}}\n'
printf '{"type":"item.completed","item":{"id":"c1","type":"command_execution","command":"git commit","exit_code":0,"aggregated_output":"","status":"completed"}}\n'
printf '{"type":"item.completed","item":{"id":"m1","type":"agent_message","text":"committed e2e.txt"}}\n'
printf '{"type":"turn.completed","usage":{"input_tokens":13,"cached_input_tokens":6,"output_tokens":4,"reasoning_output_tokens":1}}\n'
"#;

/// A task for the Codex worker (`--provider codex`, headless) runs its turn
/// as `codex exec --json` with the sandbox of ADR-t813-3 under the session
/// wrapper in a real cmux workspace: the thread Codex names is recorded on
/// the run, the turn's usage too, and the run is validated, reviewed and
/// landed.
#[test]
#[ignore = "needs a running cmux; run with --ignored"]
fn a_codex_worker_runs_its_turn_through_cmux_and_lands_on_main() {
    let fixture = fixture();
    let Fixture {
        cmux,
        repo,
        base,
        env,
        stub,
        ..
    } = &fixture;
    let codex = stub.with_file_name("codex-stub");
    fs::write(&codex, CODEX_STUB).unwrap();
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(&codex, fs::Permissions::from_mode(0o755)).unwrap();
    let task_id = dagq(
        env,
        &[
            "add",
            "e2e codex task",
            "--description",
            "Add e2e.txt to the worktree. E2E-REVIEW-PASS",
            "--acceptance",
            "e2e.txt is committed and seed.txt still exists",
            "--verify",
            "test -f seed.txt",
            "--verify",
            "test -f e2e.txt",
            "--provider",
            "codex",
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
    let codex_arg = codex.to_str().unwrap();
    let pass = supervise_once(&fixture, &["--codex", codex_arg], &[&task_id], &mut guard);
    let outcome = &pass.outcome;
    assert_eq!(outcome["errors"], Value::Array(vec![]), "{outcome}");
    assert_eq!(outcome["runs"][0]["status"], "integrated", "{outcome}");
    assert_eq!(outcome["runs"][0]["worker_mode"], "headless", "{outcome}");
    let detail = dagq(env, &["show", &task_id, "--full"]);
    assert_eq!(detail["task"]["status"], "completed");
    let run = &detail["runs"][0];
    let run_id = run["id"].as_str().unwrap();
    let run_dir = Path::new(run["run_dir"].as_str().unwrap());
    let thread = format!("e2e-thread-{run_id}");
    let main = git(repo, &["rev-parse", "main"]);
    assert_eq!(git(repo, &["rev-parse", "main^"]), base.as_str());
    assert_eq!(
        fs::read_to_string(repo.join("e2e.txt")).unwrap(),
        format!("written by the codex stub agent for {thread}\n")
    );
    assert_eq!(run["result_commit"], main.as_str());
    assert_eq!(run["actual_provider"], "codex", "{run}");
    // The sandbox's writable roots name the run directory, and the turn's
    // `TMPDIR` is the run directory's `tmp` (task 1290).
    let argv = fs::read_to_string(run_dir.join("codex-argv.log")).unwrap();
    assert!(
        argv.starts_with(&format!("codex turn argv: start {thread} ")),
        "{argv}"
    );
    assert!(
        argv.contains(&format!("\"{}\"", run_dir.display())),
        "{argv}"
    );
    assert!(
        argv.contains(&format!(
            "\ncodex turn tmpdir: {}\n",
            run_dir.join("tmp").display()
        )),
        "{argv}"
    );
    // The rules stay out of Git.
    assert!(
        fs::read_to_string(repo.join(".git/info/exclude"))
            .unwrap()
            .contains(".codex/rules/dagq-deny.rules")
    );
    let events = detail["events"].as_array().unwrap();
    let kinds: Vec<&str> = events.iter().map(|e| e["kind"].as_str().unwrap()).collect();
    let position = |kind: &str| {
        kinds
            .iter()
            .position(|k| *k == kind)
            .unwrap_or_else(|| panic!("missing {kind} in {kinds:?}"))
    };
    for pair in [
        "turn_started",
        "turn_session_identified",
        "turn_finished",
        "session_idle_observed",
        "review_finished",
        "run_integrated",
    ]
    .windows(2)
    {
        assert!(
            position(pair[0]) < position(pair[1]),
            "{} before {}: {kinds:?}",
            pair[0],
            pair[1]
        );
    }
    let identified = &events[position("turn_session_identified")]["payload"];
    assert_eq!(identified["session_id"], thread.as_str(), "{identified}");
    assert_eq!(identified["provider"], "codex");
    let finished = &events[position("turn_finished")]["payload"];
    assert_eq!(finished["outcome"], "succeeded", "{finished}");
    assert_eq!(finished["session_id"], thread.as_str());
    assert_eq!(finished["usage"]["input_tokens"], 13);
    assert_eq!(finished["usage"]["reasoning_output_tokens"], 1);
    assert_eq!(finished["message"], "committed e2e.txt");
    wait_until_not_listed(cmux, &pass.workspaces[0].1);
}

/// The production CLI flag through real cmux: no Claude executable is
/// present, an interactive Claude task runs on Codex, and its
/// lease is released so a person's integration can land the receipt.
#[test]
#[ignore = "needs a running cmux; run with --ignored"]
fn no_claude_runs_codex_through_cmux_and_allows_manual_landing() {
    let fixture = fixture();
    let Fixture {
        cmux, env, stub, ..
    } = &fixture;
    let codex = stub.with_file_name("codex-stub");
    fs::write(&codex, CODEX_STUB).unwrap();
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(&codex, fs::Permissions::from_mode(0o755)).unwrap();
    fs::remove_file(stub).unwrap();
    let task_id = dagq(
        env,
        &[
            "add",
            "e2e manual landing",
            "--interactive",
            "--description",
            "Add e2e.txt to the worktree.",
            "--acceptance",
            "e2e.txt is committed",
            "--verify",
            "test -f e2e.txt",
        ],
    )["id"]
        .to_string();
    dagq(env, &["ready", &task_id, "--bypass-review"]);
    let mut guard = WorkspaceGuard {
        cmux: cmux.clone(),
        ids: Vec::new(),
    };
    let pass = supervise_once(
        &fixture,
        &["--no-claude", "--codex", codex.to_str().unwrap()],
        &[&task_id],
        &mut guard,
    );
    assert_eq!(pass.outcome["errors"], json!([]), "{}", pass.outcome);
    let detail = dagq(env, &["show", &task_id, "--full"]);
    assert_eq!(detail["runs"][0]["status"], "awaiting_integration");
    assert_eq!(detail["runs"][0]["actual_provider"], "codex");
    let events = detail["events"].as_array().unwrap();
    assert!(!events.iter().any(|e| e["kind"] == "review_started"));
    let landed = dagq(env, &["integrate", &task_id]);
    assert_eq!(landed["outcome"], "integrated", "{landed}");
    wait_until_not_listed(cmux, &pass.workspaces[0].1);
}
