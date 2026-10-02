//! The plan review on Codex (task 1218, as the goal review of
//! ADR-t1063-1): `[roles.plan_review]`'s `provider = "codex"` starts the
//! job as `codex exec --json` in the read-only sandbox, played by the goal
//! review tests' stub `codex`, and its verdict goes through the same
//! `plan_review_finished`, send-back and `approve_plan` as Claude's. A Codex
//! that cannot be used moves the job to Claude (the plan review tests'
//! stub provider), and under `--no-claude` it never does.

use crate::goal_review_codex::{
    CODEX_MODEL, codex_config, codex_home, queue_events, roles, stub_codex, stub_lines,
};
use crate::plan_review::{
    Fixture, PlanWorkspace, StubReviewer, add, events, fixture, job_actors, options, status,
    submit, supervise_with,
};

use dagq::{
    application::TaskStore,
    domain::{AskKind, Priority, ProposalId, ProposalStatus, TaskId, TaskStatus},
    infrastructure::sqlite::SqliteQueue,
};
use rusqlite::Connection;
use serde_json::{Value, json};
use std::{path::Path, time::Duration};

/// Supervise once with `codex` as the Codex CLI and the stub's home, and
/// no runtime planner.
fn supervise(fx: &Fixture, reviewer: &StubReviewer, codex: &Path) -> Value {
    let mut options = options(0, Duration::from_secs(3600));
    options.codex = codex.to_owned();
    options.codex_home = Some(codex_home(fx));
    supervise_with(fx, &PlanWorkspace::default(), reviewer, &options)
}

fn verdict(verdict: &str, reason: &str) -> Value {
    let reasons: Vec<&str> = if reason.is_empty() {
        Vec::new()
    } else {
        vec![reason]
    };
    json!({"verdict": verdict, "reasons": reasons, "summary": format!("{verdict} it")})
}

/// A submitted proposal of one new task, waiting for the draft blocker so
/// that no task is claimed.
fn proposal(fx: &Fixture, title: &str) -> (TaskId, ProposalId) {
    let mut queue = SqliteQueue::open(&fx.db).unwrap();
    let task = add(&mut queue, title, &[TaskId::new(1)], Priority::Normal);
    (task, submit(&mut queue, &[task], None))
}

/// The proposal's `review_hold` column.
fn review_hold(fx: &Fixture, proposal: ProposalId) -> Option<String> {
    Connection::open(&fx.db)
        .unwrap()
        .query_row(
            "SELECT review_hold FROM proposals WHERE id=?1",
            [proposal.as_i64()],
            |r| r.get(0),
        )
        .unwrap()
}

/// `provider = "codex"`: each job is `codex exec --json` in the read-only
/// sandbox with the role's model and effort and the job's role in its
/// environment; pass readies the task, revise sends the proposal back and
/// concern opens `approve_plan`, as a Claude job's verdicts do. The
/// launch, the job's end, its span, `headless_jobs`, `stats` and `kpi`
/// record Codex and its model.
#[test]
fn a_plan_review_on_codex_runs_read_only_and_applies_its_verdicts() {
    let config = codex_config();
    let fx = fixture();
    roles(
        &fx,
        "[roles.plan_review]\nprovider = \"codex\"\nmodel = \"gpt-6-astra\"\neffort = \"high\"\n",
    );
    let claude = StubReviewer::new(&[verdict("pass", "")]);
    let mut queue = SqliteQueue::open(&fx.db).unwrap();
    let _service = crate::common::service::Served::start(&fx.db);

    let (passed, first) = proposal(&fx, "passed");
    let codex = stub_codex(&fx, "ok", &verdict("pass", ""));
    supervise(&fx, &claude, &codex);
    assert_eq!(status(&mut queue, passed), TaskStatus::Ready);
    assert_eq!(
        queue.show_proposal(first).unwrap().status(),
        ProposalStatus::Accepted
    );

    let (revised, second) = proposal(&fx, "revised");
    stub_codex(&fx, "ok", &verdict("revise", "split the parser out first"));
    supervise(&fx, &claude, &codex);
    assert_eq!(
        queue.show_proposal(second).unwrap().status(),
        ProposalStatus::Revising
    );
    assert_eq!(status(&mut queue, revised), TaskStatus::Draft);
    let finished = &events(&mut queue, revised, "plan_review_finished")[0];
    assert_eq!(finished["decision"], "revise");
    assert_eq!(finished["reasons"], json!(["split the parser out first"]));

    let (concerned, _) = proposal(&fx, "concerned");
    stub_codex(&fx, "ok", &verdict("concern", "looks already implemented"));
    supervise(&fx, &claude, &codex);
    assert_eq!(status(&mut queue, concerned), TaskStatus::Submitted);
    let asks = queue.asks(Default::default()).unwrap();
    assert_eq!(asks.len(), 1, "{asks:?}");
    assert_eq!(asks[0].kind, AskKind::ApprovePlan);
    assert_eq!(asks[0].task_id, Some(concerned));
    assert!(asks[0].question.contains("looks already implemented"));

    assert!(claude.prompts().is_empty(), "Claude ran no plan review");
    // The job's environment names the queue service, not the queue's path
    // (goal 82's stage (3)).
    let env = std::fs::read_to_string(fx.db.parent().unwrap().join("codex-env.txt")).unwrap();
    assert!(env.contains("DAGQ_SERVICE_SOCKET="), "{env}");
    // Its `dagq` reached the service and was let read: the stub asks for
    // goal 1, which this queue does not have, and the service says so.
    let read = std::fs::read_to_string(fx.db.parent().unwrap().join("codex-goal.err")).unwrap();
    assert!(read.contains("goal 1 does not exist"), "{read}");
    assert!(
        !env.lines()
            .any(|line| line.starts_with("DAGQ_QUEUE=") || line.contains(fx.db.to_str().unwrap())),
        "{env}"
    );
    assert!(job_actors(&fx.db).is_empty());
    let calls = stub_lines(&fx, "codex-args.txt");
    assert_eq!(calls.len(), 3, "{calls:?}");
    let repo = fx.repo.canonicalize().unwrap();
    // The goal review's profile: read-only and offline, its `dagq` reaching
    // only the queue service's socket (ADR-t1233-5 decision 4).
    let socket = dagq::infrastructure::queue_service::socket_path(fx.db.parent().unwrap());
    let profile: String = dagq::infrastructure::codex::job_service_config(&socket)
        .unwrap()
        .iter()
        .map(|config| format!("-c|{config}|"))
        .collect();
    assert!(profile.contains(r#"permissions.dagq_job.extends=":read-only""#));
    let expected = format!(
        "exec|--json|-C|{}|-m|gpt-6-astra|-c|model_reasoning_effort=\"high\"|{profile}--|",
        repo.display()
    );
    for call in &calls {
        assert!(call.starts_with(&expected), "{call}");
        // The prompt has the repository read its own instructions.
        assert!(call.contains("AGENTS.md"), "{call}");
        for refused in ["dangerously", "bypass", "writable_roots", "--session-id"] {
            assert!(!call.contains(refused), "{refused}: {call}");
        }
    }
    assert_eq!(
        stub_lines(&fx, "codex-actors.txt")[0],
        format!("plan-review-job plan-review-job:{first}:1")
    );

    let started = &events(&mut queue, passed, "plan_review_started")[0];
    assert_eq!(
        started["launch"],
        json!({"role": "plan_review", "provider": "codex", "model": "gpt-6-astra",
               "effort": "high", "source": "dagq.toml"})
    );
    assert!(started["session_id"].is_null(), "Codex names its thread");
    for (task, thread) in [(passed, 1), (revised, 2), (concerned, 3)] {
        let finished = &events(&mut queue, task, "plan_review_finished")[0];
        assert_eq!(finished["session_id"], format!("codex-thread-{thread}"));
        assert_eq!(finished["model"], CODEX_MODEL);
    }
    // The job's span has the thread and the model, not a Claude transcript.
    let closed = events(&mut queue, passed, "session_closed");
    assert_eq!(closed.len(), 1, "{closed:?}");
    assert_eq!(closed[0]["kind"], "plan_review");
    assert_eq!(closed[0]["session_id"], "codex-thread-1");
    assert_eq!(closed[0]["model"], CODEX_MODEL);
    let providers: Vec<String> = Connection::open(&fx.db)
        .unwrap()
        .prepare("SELECT provider FROM headless_jobs WHERE kind='plan_review' ORDER BY id")
        .unwrap()
        .query_map([], |r| r.get(0))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap();
    assert_eq!(providers, ["codex", "codex", "codex"]);

    let jobs = &crate::common::cli::ok(&fx.db, &["stats", "--full"])["jobs"]["plan_review"];
    let codex_jobs = &jobs["by_provider"]["codex"];
    assert_eq!(codex_jobs["count"], 3, "{jobs}");
    for decision in ["pass", "revise", "concern"] {
        assert_eq!(codex_jobs["verdicts"][decision], 1, "{jobs}");
    }
    assert!(jobs["by_provider"].get("claude").is_none(), "{jobs}");
    assert_eq!(jobs["by_model"][CODEX_MODEL]["count"], 3, "{jobs}");
    let kpi = crate::common::cli::ok(&fx.db, &["kpi", "--last", "1"]);
    let count = &kpi["periods"][0]["kpis"]["job.count.plan_review"];
    assert_eq!(count["provider=codex"]["value"], 3.0, "{count}");

    let doctor = crate::common::cli::ok(&fx.db, &["doctor"]);
    assert_eq!(
        doctor["roles"]["plan_review"],
        json!({"provider": "codex", "source": "dagq.toml", "model": "gpt-6-astra", "effort": "high"})
    );
    assert_eq!(codex_config(), config, "the person's Codex settings");
}

/// A Codex whose login ran out (ADR-t1063-1 decision 4): the job's failure
/// says its provider could not be used, Codex is held, the proposal is not
/// held for a person, and it is reviewed again at once on Claude, whose
/// launch says from which provider and why.
#[test]
fn a_codex_that_cannot_log_in_moves_the_plan_review_to_claude() {
    let fx = fixture();
    roles(&fx, "[roles.plan_review]\nprovider = \"codex\"\n");
    let (task, proposal) = proposal(&fx, "moved");
    let codex = stub_codex(&fx, "auth", &verdict("pass", ""));
    let claude = StubReviewer::new(&[verdict("pass", "")]);
    supervise(&fx, &claude, &codex);
    supervise(&fx, &claude, &codex);
    assert_eq!(stub_lines(&fx, "codex-args.txt").len(), 1);
    assert_eq!(claude.prompts().len(), 1, "Claude reviewed it after");

    let mut queue = SqliteQueue::open(&fx.db).unwrap();
    let failed = events(&mut queue, task, "plan_review_failed");
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
    let started = events(&mut queue, task, "plan_review_started");
    assert_eq!(started.len(), 2);
    assert_eq!(started[0]["launch"]["provider"], "codex");
    assert_eq!(started[1]["launch"]["provider"], "claude");
    assert_eq!(started[1]["launch"]["switched_from"], "codex");
    assert_eq!(started[1]["launch"]["switch_reason"], "authentication");
    assert!(started[1]["session_id"].is_string());
    assert_eq!(status(&mut queue, task), TaskStatus::Ready);
    assert_eq!(
        queue.show_proposal(proposal).unwrap().status(),
        ProposalStatus::Accepted
    );
    let jobs = &crate::common::cli::ok(&fx.db, &["stats", "--full"])["jobs"]["plan_review"];
    assert_eq!(jobs["by_provider"]["codex"]["failed"], 1, "{jobs}");
    assert_eq!(
        jobs["by_provider"]["claude"]["verdicts"]["pass"], 1,
        "{jobs}"
    );
    // The interrupted job is no failure a person looks at.
    let inbox = crate::common::cli::ok(&fx.db, &["status", "--role", "inbox"]);
    assert!(
        !inbox.to_string().contains("plan review by hand"),
        "{inbox}"
    );
    let watched = dagq::watch::events(&fx.db, dagq::domain::EventId::new(0), 100, false).unwrap();
    assert!(
        !watched.to_string().contains("plan review by hand"),
        "{watched}"
    );
}

/// A supervisor with no Codex that runs starts the plan review on Claude
/// (`executable_missing`), without calling any Codex.
#[test]
fn without_codex_the_plan_review_starts_on_claude() {
    let fx = fixture();
    roles(&fx, "[roles.plan_review]\nprovider = \"codex\"\n");
    let (task, _) = proposal(&fx, "on claude");
    let claude = StubReviewer::new(&[verdict("pass", "")]);
    let missing = fx.db.parent().unwrap().join("no-such-codex");
    supervise(&fx, &claude, &missing);
    assert_eq!(claude.prompts().len(), 1);
    let mut queue = SqliteQueue::open(&fx.db).unwrap();
    let started = events(&mut queue, task, "plan_review_started");
    assert_eq!(started.len(), 1);
    assert_eq!(started[0]["launch"]["provider"], "claude");
    assert_eq!(started[0]["launch"]["switched_from"], "codex");
    assert_eq!(started[0]["launch"]["switch_reason"], "executable_missing");
    assert_eq!(status(&mut queue, task), TaskStatus::Ready);
}

/// Under `--no-claude` (ADR-t1204-1), a plan review whose role names Codex
/// starts on Codex rather than going to a person; a Codex that fails or
/// cannot be used never moves it to Claude, and the proposal goes to a
/// person (plan review by hand) told why.
#[test]
fn no_claude_plan_review_runs_on_codex_and_never_falls_back() {
    for mode in ["ok", "auth", "missing"] {
        let mut fx = fixture();
        roles(&fx, "[roles.plan_review]\nprovider = 'codex'\n");
        let (task, proposal) = proposal(&fx, "no claude");
        let codex = if mode == "missing" {
            fx.repo.join("missing-codex")
        } else {
            stub_codex(&fx, mode, &verdict("pass", ""))
        };
        fx.claude = fx.repo.join("missing-claude");
        let reviewer = StubReviewer::new(&[verdict("pass", "")]);
        let mut opts = options(1, Duration::from_secs(3600));
        opts.no_claude = true;
        opts.codex = codex;
        opts.codex_home = Some(codex_home(&fx));
        supervise_with(&fx, &PlanWorkspace::default(), &reviewer, &opts);
        supervise_with(&fx, &PlanWorkspace::default(), &reviewer, &opts);
        assert!(reviewer.prompts().is_empty(), "{mode}");
        let mut queue = SqliteQueue::open(&fx.db).unwrap();
        let started = events(&mut queue, task, "plan_review_started");
        for event in &started {
            assert_eq!(event["launch"]["provider"], "codex", "{mode}");
        }
        assert!(queue.asks(Default::default()).unwrap().is_empty(), "{mode}");
        let calls = stub_lines(&fx, "codex-args.txt").len();
        match mode {
            "ok" => {
                assert_eq!(calls, 1);
                assert_eq!(status(&mut queue, task), TaskStatus::Ready);
                assert!(events(&mut queue, task, "plan_review_failed").is_empty());
            }
            _ => {
                assert_eq!(calls, usize::from(mode == "auth"), "{mode}");
                let failed = events(&mut queue, task, "plan_review_failed");
                let error = failed.last().unwrap()["error"].as_str().unwrap().to_owned();
                let reason = if mode == "auth" {
                    "authentication"
                } else {
                    "executable_missing"
                };
                assert!(error.contains("provider_disabled"), "{mode}: {error}");
                assert!(
                    error.contains(&format!("codex cannot be used ({reason})")),
                    "{mode}: {error}"
                );
                assert_eq!(review_hold(&fx, proposal).as_deref(), Some("failed"));
                assert_eq!(status(&mut queue, task), TaskStatus::Submitted);
            }
        }
    }
}
