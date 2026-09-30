//! The goal review on Codex (ADR-t1063-1): `[roles.goal_review]`'s
//! `provider = "codex"` starts the job as `codex exec --json` in the
//! read-only sandbox, played by a stub `codex` that writes the JSONL and the
//! rollout codex-cli 0.155.1 writes (docs/plans/codex-headless-jobs-spike.md),
//! and a Codex that cannot be used moves the job to Claude (the plan review
//! tests' stub provider) or holds it.

use crate::goal_review::{goal_done, goal_events};
use crate::plan_review::{
    Fixture, PlanWorkspace, StubReviewer, fixture, git, job_actors, options, supervise_with,
};

use dagq::{application::TaskStore, domain::GoalVerdict, infrastructure::sqlite::SqliteQueue};
use rusqlite::Connection;
use serde_json::{Value, json};
use std::{
    fs,
    path::{Path, PathBuf},
    time::Duration,
};

/// The model the stub `codex` writes to its rollouts.
const CODEX_MODEL: &str = "gpt-6-astra";

/// The home of the stub `codex`, next to the queue: its rollouts are under
/// `sessions`.
fn codex_home(fx: &Fixture) -> PathBuf {
    fx.db.parent().unwrap().join("codex-home")
}

/// A stub `codex` next to the queue: `--version` answers, and `exec`
/// appends its arguments (each ended by `|`) to `codex-args.txt` and its
/// role and actor to `codex-actors.txt`, prints `thread.started` (thread
/// `codex-thread-<call>`) and `turn.started`, then, as `codex-mode` says,
/// fails at the login (`auth`) or the usage limit (`limit`), or writes the
/// thread's rollout with its model's `turn_context` and prints the reply in
/// `codex-reply.jsonl` and `turn.completed`.
fn stub_codex(fx: &Fixture, mode: &str, verdict: &Value) -> PathBuf {
    let dir = fx.db.parent().unwrap();
    let stub = dir.join("codex");
    let reply = json!({"type": "item.completed", "item": {"id": "m1", "type": "agent_message", "text": verdict.to_string()}});
    fs::write(dir.join("codex-reply.jsonl"), format!("{reply}\n")).unwrap();
    fs::write(dir.join("codex-mode"), mode).unwrap();
    let script = format!(
        r#"#!/bin/sh
DIR='{dir}'
[ "$1" = --version ] && {{ echo "codex-cli 0.155.1"; exit 0; }}
for arg in "$@"; do printf '%s|' "$arg" | tr '\n' ' '; done >> "$DIR/codex-args.txt"
printf '\n' >> "$DIR/codex-args.txt"
printf '%s %s\n' "$DAGQ_ROLE" "$DAGQ_ACTOR_ID" >> "$DIR/codex-actors.txt"
THREAD="codex-thread-$(wc -l < "$DIR/codex-actors.txt" | tr -d ' ')"
echo "Reading additional input from stdin..." >&2
printf '{{"type":"thread.started","thread_id":"%s"}}\n{{"type":"turn.started"}}\n' "$THREAD"
case "$(cat "$DIR/codex-mode")" in
  auth)
    printf '{{"type":"error","message":"unexpected status 401 Unauthorized: Missing bearer or basic authentication in header"}}\n{{"type":"turn.failed","error":{{"message":"unexpected status 401 Unauthorized"}}}}\n'
    exit 1 ;;
  limit)
    printf '{{"type":"error","message":"You have hit your usage limit. Try again later."}}\n{{"type":"turn.failed","error":{{"message":"You have hit your usage limit."}}}}\n'
    exit 1 ;;
esac
SESSIONS="$DIR/codex-home/sessions/2026/09/29"
mkdir -p "$SESSIONS"
printf '{{"timestamp":"%s","type":"turn_context","payload":{{"model":"{CODEX_MODEL}","effort":"high"}}}}\n' "$(date -u +%Y-%m-%dT%H:%M:%S.000Z)" >> "$SESSIONS/rollout-2026-09-29T00-00-00-$THREAD.jsonl"
cat "$DIR/codex-reply.jsonl"
printf '{{"type":"turn.completed","usage":{{"input_tokens":10,"output_tokens":2}}}}\n'
"#,
        dir = dir.display(),
    );
    fs::write(&stub, script).unwrap();
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(&stub, fs::Permissions::from_mode(0o755)).unwrap();
    stub
}

/// The stub's lines of `name` (one per call).
fn stub_lines(fx: &Fixture, name: &str) -> Vec<String> {
    fs::read_to_string(fx.db.parent().unwrap().join(name))
        .unwrap_or_default()
        .lines()
        .map(str::to_owned)
        .collect()
}

/// Commit `dagq.toml` with `text` to the fixture's main checkout.
fn roles(fx: &Fixture, text: &str) {
    fs::write(fx.repo.join("dagq.toml"), text).unwrap();
    git(&fx.repo, &["add", "dagq.toml"]);
    git(&fx.repo, &["commit", "-m", "roles"]);
}

/// Supervise once with `codex` as the Codex CLI and the stub's home.
fn supervise(fx: &Fixture, reviewer: &StubReviewer, codex: &Path) -> Value {
    let mut options = options(0, Duration::from_secs(3600));
    options.codex = codex.to_owned();
    options.codex_home = Some(codex_home(fx));
    supervise_with(fx, &PlanWorkspace::default(), reviewer, &options)
}

fn achieved(summary: &str) -> Value {
    json!({"verdict": "achieved", "criteria": [], "summary": summary})
}

/// The payloads of the queue's events of `kind`, oldest first.
fn queue_events(fx: &Fixture, kind: &str) -> Vec<Value> {
    let connection = Connection::open(&fx.db).unwrap();
    let mut statement = connection
        .prepare("SELECT payload FROM run_events WHERE kind=?1 ORDER BY id")
        .unwrap();
    statement
        .query_map([kind], |r| r.get::<_, String>(0))
        .unwrap()
        .map(|payload| serde_json::from_str(&payload.unwrap()).unwrap())
        .collect()
}

/// The person's `~/.codex/config.toml`, which no job may change
/// (ADR-t813-3 decision 6); `None` when there is none.
fn codex_config() -> Option<Vec<u8>> {
    let home = std::env::var_os("HOME")?;
    fs::read(Path::new(&home).join(".codex/config.toml")).ok()
}

/// `provider = "codex"`: the job is `codex exec --json` in the read-only
/// sandbox with the role's model and effort and the job's role in its
/// environment; the verdict, thread and model are read from its JSONL and
/// rollout, and the verdict is applied as a Claude job's is. The launch,
/// the job's end, its span and `stats` record Codex and its model, and
/// `doctor` shows the role's provider from `dagq.toml`.
#[test]
fn a_goal_review_on_codex_runs_read_only_and_records_its_thread_and_model() {
    let config = codex_config();
    let fx = fixture();
    roles(
        &fx,
        "[roles.goal_review]\nprovider = \"codex\"\nmodel = \"gpt-6-astra\"\neffort = \"high\"\n",
    );
    let (goal, done) = goal_done(&fx);
    let codex = stub_codex(&fx, "ok", &achieved("both items landed"));
    let claude = StubReviewer::new(&[achieved("from Claude")]);
    let outcome = supervise(&fx, &claude, &codex);
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    assert!(claude.prompts().is_empty(), "Claude ran no goal review");
    assert!(job_actors(&fx.db).is_empty());

    let calls = stub_lines(&fx, "codex-args.txt");
    assert_eq!(calls.len(), 1, "{calls:?}");
    let repo = fx.repo.canonicalize().unwrap();
    let expected = format!(
        "exec|--json|--sandbox|read-only|-C|{}|-m|gpt-6-astra|-c|model_reasoning_effort=\"high\"|--|",
        repo.display()
    );
    assert!(calls[0].starts_with(&expected), "{}", calls[0]);
    assert!(calls[0].contains("the median landing is under 5 minutes"));
    for refused in ["dangerously", "bypass", "writable_roots", "--session-id"] {
        assert!(!calls[0].contains(refused), "{refused}: {}", calls[0]);
    }
    // The job's role and actor reach the sandbox's `dagq` (spike 2.).
    assert_eq!(
        stub_lines(&fx, "codex-actors.txt"),
        [format!("goal-review-job goal-review-job:{goal}:1")]
    );

    let mut queue = SqliteQueue::open(&fx.db).unwrap();
    assert_eq!(
        queue.show_goal(goal).unwrap().goal.verdict(),
        Some(GoalVerdict::Achieved)
    );
    let started = &goal_events(&mut queue, goal, "goal_review_started")[0];
    assert_eq!(
        started["launch"],
        json!({"role": "goal_review", "provider": "codex", "model": "gpt-6-astra",
               "effort": "high", "source": "dagq.toml"})
    );
    assert!(started["session_id"].is_null(), "Codex names its thread");
    let finished = &goal_events(&mut queue, goal, "goal_review_finished")[0];
    assert_eq!(finished["decision"], "achieved");
    assert_eq!(finished["session_id"], "codex-thread-1");
    assert_eq!(finished["model"], CODEX_MODEL);
    let closed = &goal_events(&mut queue, goal, "goal_closed")[0];
    assert_eq!(closed["reason"], "both items landed");
    // The job's span has the thread and the model, not a Claude transcript.
    let mut span = |kind: &str| -> Vec<Value> {
        queue
            .show(done)
            .unwrap()
            .events
            .into_iter()
            .filter(|e| e.kind == kind)
            .map(|e| e.payload)
            .collect()
    };
    let closed = span("session_closed");
    assert_eq!(closed.len(), 1);
    assert_eq!(closed[0]["kind"], "goal_review");
    assert_eq!(closed[0]["session_id"], "codex-thread-1");
    assert_eq!(closed[0]["model"], CODEX_MODEL);
    assert_eq!(closed[0]["active_unavailable"], "transcript_not_claude");
    let provider: String = Connection::open(&fx.db)
        .unwrap()
        .query_row(
            "SELECT provider FROM headless_jobs WHERE kind='goal_review'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(provider, "codex");
    let jobs = &crate::common::cli::ok(&fx.db, &["stats", "--full"])["jobs"]["goal_review"];
    assert_eq!(jobs["by_provider"]["codex"]["verdicts"]["achieved"], 1);
    assert_eq!(jobs["by_model"][CODEX_MODEL]["count"], 1);

    let doctor = crate::common::cli::ok(&fx.db, &["doctor"]);
    assert_eq!(
        doctor["roles"]["goal_review"],
        json!({"provider": "codex", "source": "dagq.toml", "model": "gpt-6-astra", "effort": "high"})
    );
    assert_eq!(doctor["roles"]["review"]["provider"], "claude");
    assert_eq!(doctor["roles"]["review"]["source"], "default");
    // Codex for a role it has no implementation for is a mistake in the
    // file: `doctor` says so, and every role starts as before meanwhile.
    fs::write(
        fx.repo.join("dagq.toml"),
        "[roles.review]\nprovider = \"codex\"\n",
    )
    .unwrap();
    let doctor = crate::common::cli::ok(&fx.db, &["doctor"]);
    let error = doctor["roles"]["error"].as_str().unwrap();
    assert!(
        error.contains("[roles.review]: provider codex cannot run the review role"),
        "{error}"
    );
    assert_eq!(doctor["roles"]["review"]["provider"], "claude");
    assert_eq!(codex_config(), config, "the person's Codex settings");
}

/// A Codex whose login ran out (ADR-t1063-1 decision 4): the job's failure
/// says its provider could not be used, Codex is held, and the goal is
/// reviewed again at once on Claude, whose launch says from which provider
/// and why.
#[test]
fn a_codex_that_cannot_log_in_moves_the_goal_review_to_claude() {
    let fx = fixture();
    roles(&fx, "[roles.goal_review]\nprovider = \"codex\"\n");
    let (goal, _) = goal_done(&fx);
    let codex = stub_codex(&fx, "auth", &achieved("unused"));
    let claude = StubReviewer::new(&[achieved("from Claude")]);
    supervise(&fx, &claude, &codex);
    supervise(&fx, &claude, &codex);
    assert_eq!(stub_lines(&fx, "codex-args.txt").len(), 1);
    assert_eq!(claude.prompts().len(), 1, "Claude reviewed it after");

    let mut queue = SqliteQueue::open(&fx.db).unwrap();
    let failed = goal_events(&mut queue, goal, "goal_review_failed");
    assert_eq!(failed.len(), 1);
    assert_eq!(
        failed[0]["provider_unusable"],
        json!({"provider": "codex", "reason": "authentication"})
    );
    assert_eq!(failed[0]["session_id"], "codex-thread-1");
    let held = queue_events(&fx, "provider_held");
    assert_eq!(held.len(), 1);
    assert_eq!(
        (&held[0]["provider"], &held[0]["reason"]),
        (&json!("codex"), &json!("authentication"))
    );
    let started = goal_events(&mut queue, goal, "goal_review_started");
    assert_eq!(started.len(), 2);
    assert_eq!(started[0]["launch"]["provider"], "codex");
    assert_eq!(
        started[1]["launch"],
        json!({"role": "goal_review", "provider": "claude", "model": null, "effort": null,
               "source": "default", "switched_from": "codex", "switch_reason": "authentication"})
    );
    assert!(started[1]["session_id"].is_string());
    let detail = queue.show_goal(goal).unwrap();
    assert_eq!(detail.goal.verdict(), Some(GoalVerdict::Achieved));
    // The interrupted job is no failure a person looks at.
    let holds = crate::common::cli::ok(&fx.db, &["status", "--role", "inbox"]);
    assert!(!holds.to_string().contains("goal_review_failed"), "{holds}");
}

/// A supervisor with no Codex that runs starts the goal review on Claude
/// (`executable_missing`), without calling any Codex.
#[test]
fn without_codex_the_goal_review_starts_on_claude() {
    let fx = fixture();
    roles(&fx, "[roles.goal_review]\nprovider = \"codex\"\n");
    let (goal, _) = goal_done(&fx);
    let claude = StubReviewer::new(&[achieved("from Claude")]);
    let missing = fx.db.parent().unwrap().join("no-such-codex");
    supervise(&fx, &claude, &missing);
    assert_eq!(claude.prompts().len(), 1);
    let mut queue = SqliteQueue::open(&fx.db).unwrap();
    let started = goal_events(&mut queue, goal, "goal_review_started");
    assert_eq!(started.len(), 1);
    assert_eq!(started[0]["launch"]["provider"], "claude");
    assert_eq!(started[0]["launch"]["switched_from"], "codex");
    assert_eq!(started[0]["launch"]["switch_reason"], "executable_missing");
    assert_eq!(
        queue.show_goal(goal).unwrap().goal.verdict(),
        Some(GoalVerdict::Achieved)
    );
}

/// A role that names Claude moves to Codex at Claude's usage limit, which
/// holds the queue's jobs as before; when Codex is at its limit too, the
/// goal review waits and nothing starts (ADR-t1063-1 decision 5).
#[test]
fn a_goal_review_waits_while_both_providers_are_held() {
    let fx = fixture();
    roles(&fx, "[roles.goal_review]\nprovider = \"claude\"\n");
    let (goal, _) = goal_done(&fx);
    let codex = stub_codex(&fx, "limit", &achieved("unused"));
    let claude = StubReviewer::limited_then(&achieved("from Claude"));
    supervise(&fx, &claude, &codex);
    supervise(&fx, &claude, &codex);
    assert_eq!(claude.prompts().len(), 1);
    assert_eq!(stub_lines(&fx, "codex-args.txt").len(), 1);

    let mut queue = SqliteQueue::open(&fx.db).unwrap();
    let started = goal_events(&mut queue, goal, "goal_review_started");
    assert_eq!(started.len(), 2, "{started:?}");
    assert_eq!(started[0]["launch"]["provider"], "claude");
    assert_eq!(started[1]["launch"]["provider"], "codex");
    assert_eq!(started[1]["launch"]["switched_from"], "claude");
    assert_eq!(started[1]["launch"]["switch_reason"], "usage_limit");
    let failed = goal_events(&mut queue, goal, "goal_review_failed");
    assert_eq!(
        failed
            .iter()
            .map(|f| f["provider_unusable"]["provider"].clone())
            .collect::<Vec<_>>(),
        [json!("claude"), json!("codex")]
    );
    // Claude's limit opened the queue's hold ask as before; Codex's is a
    // hold of its own.
    let asks = queue.asks(Default::default()).unwrap();
    assert_eq!(asks.len(), 1, "{asks:?}");
    assert_eq!(asks[0].kind, dagq::domain::AskKind::QueueHold);
    assert_eq!(asks[0].subject.as_deref(), Some("usage_limit"));
    let held = queue_events(&fx, "provider_held");
    assert_eq!(held.len(), 1);
    assert_eq!(held[0]["provider"], "codex");
    assert_eq!(queue.show_goal(goal).unwrap().goal.verdict(), None);
}

/// Claude's hold ask, opened in an earlier pass while no Codex could take
/// the goal review, does not stop it once Codex can: it starts on Codex
/// with the ask still open (ADR-t1063-1 decision 5).
#[test]
fn a_goal_review_runs_on_codex_while_claudes_hold_ask_is_open() {
    let fx = fixture();
    roles(&fx, "[roles.goal_review]\nprovider = \"claude\"\n");
    let (goal, _) = goal_done(&fx);
    let claude = StubReviewer::limited_then(&achieved("from Claude"));
    let missing = fx.db.parent().unwrap().join("no-such-codex");
    supervise(&fx, &claude, &missing);
    let mut queue = SqliteQueue::open(&fx.db).unwrap();
    assert_eq!(
        goal_events(&mut queue, goal, "goal_review_started").len(),
        1
    );
    assert_eq!(queue.asks(Default::default()).unwrap().len(), 1);

    let codex = stub_codex(&fx, "ok", &achieved("from Codex"));
    supervise(&fx, &claude, &codex);
    assert_eq!(claude.prompts().len(), 1, "Claude is held");
    let started = goal_events(&mut queue, goal, "goal_review_started");
    assert_eq!(started.len(), 2);
    assert_eq!(started[1]["launch"]["provider"], "codex");
    assert_eq!(started[1]["launch"]["switched_from"], "claude");
    assert_eq!(started[1]["launch"]["switch_reason"], "usage_limit");
    assert_eq!(
        queue.show_goal(goal).unwrap().goal.verdict(),
        Some(GoalVerdict::Achieved)
    );
    let asks = queue.asks(Default::default()).unwrap();
    assert_eq!(asks.len(), 1);
    assert_eq!(asks[0].answer, None, "the ask is still open");
}

#[test]
fn no_claude_goal_review_runs_on_codex_and_never_falls_back() {
    for mode in ["ok", "auth", "missing"] {
        let mut fx = fixture();
        roles(&fx, "[roles.goal_review]\nprovider = 'codex'\n");
        let (goal, _) = goal_done(&fx);
        let codex = if mode == "missing" {
            fx.repo.join("missing-codex")
        } else {
            stub_codex(&fx, mode, &achieved("done"))
        };
        fx.claude = fx.repo.join("missing-claude");
        let reviewer = StubReviewer::new(&[achieved("must never run")]);
        let mut opts = options(1, Duration::from_secs(3600));
        opts.no_claude = true;
        opts.codex = codex;
        opts.codex_home = Some(codex_home(&fx));
        supervise_with(&fx, &PlanWorkspace::default(), &reviewer, &opts);
        assert!(reviewer.prompts().is_empty());
        let mut queue = SqliteQueue::open(&fx.db).unwrap();
        let started = goal_events(&mut queue, goal, "goal_review_started");
        assert_eq!(started.len(), usize::from(mode != "missing"));
        for event in started {
            assert_eq!(event["launch"]["provider"], "codex");
        }
        assert!(queue.asks(Default::default()).unwrap().is_empty());
    }
}
