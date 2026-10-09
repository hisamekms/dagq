//! The eval of an agent through the supervisor loop (ADR-t1728-1 decisions
//! 5 to 9): a round asked for with `dagq agent eval` runs on the landing
//! branch's definition and cases, one agent job per run of a case, through
//! the headless jobs' one way, and its scores are recorded as events. The
//! agent job is played by a stub provider that prints a scripted verdict
//! and reports what each run cost. A supervisor that takes over stops the
//! job a gone one left and starts the runs left. The decisions (the
//! limits, the once rule, the order, the policy, the prompt) are unit
//! tests of their modules; these are the boundary with the queue, Git and
//! the processes.

use crate::common::{self, shell_path};
use crate::plan_review::{Fixture, PlanWorkspace, fixture, git, options};
use crate::runtime_support::{git_out, orphan, pid_alive, process_start};

use anyhow::{Result, bail};
use dagq::application::{AgentJobLaunch, AgentProvider, CommandSpec};
use dagq::domain::headless_job::{JobAccess, JobSession};
use dagq::domain::tokens::{ExecutionTokens, TokenUsage};
use dagq::domain::{EventId, EventKind, TaskId, TaskRun};
use dagq::infrastructure::sqlite::SqliteQueue;
use dagq::runtime;
use rusqlite::Connection;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    fs,
    path::{Path, PathBuf},
    sync::Mutex,
    thread,
    time::{Duration, Instant},
};

/// The definition the round measures.
const DEFINITION: &str =
    "---\ndescription: checks D-1\n---\nFlag D-1 when a change adds notes.txt.\n";

/// A violation of D-1 and a clean answer, as the agent prints them.
fn violation() -> String {
    json!({"agent": "demo", "status": "completed", "verdict": "revise",
           "reasons": [{"text": "adds notes.txt", "codes": ["D-1"]}], "summary": "breaks D-1"})
    .to_string()
}

fn clean() -> String {
    json!({"agent": "demo", "status": "completed", "verdict": "pass", "reasons": [], "summary": "fine"})
        .to_string()
}

/// The agent's jobs: each prints the next reply (the last one again when
/// they run out), copies its standard input to `stdin-<n>.txt` in `out`,
/// and reports $0.25 for the run.
struct EvalAgent {
    replies: Mutex<Vec<String>>,
    launches: Mutex<Vec<AgentJobLaunch>>,
    /// Whether each job's tree held the case lists or the patches.
    saw_cases: Mutex<Vec<bool>>,
    out: PathBuf,
}

impl EvalAgent {
    fn new(out: &Path, replies: &[String]) -> Self {
        Self {
            replies: Mutex::new(replies.to_vec()),
            launches: Mutex::new(Vec::new()),
            saw_cases: Mutex::new(Vec::new()),
            out: out.to_owned(),
        }
    }

    fn launches(&self) -> Vec<AgentJobLaunch> {
        self.launches.lock().unwrap().clone()
    }

    /// What the job of launch `n` read on its standard input.
    fn stdin(&self, n: usize) -> String {
        fs::read_to_string(self.out.join(format!("stdin-{n}.txt"))).unwrap()
    }
}

impl AgentProvider for EvalAgent {
    fn preflight(&self) -> Result<()> {
        Ok(())
    }
    fn headless_command(&self, _: &Path, _: &str, _: JobAccess) -> Result<CommandSpec> {
        bail!("no other job runs in these tests")
    }
    fn review_command(&self, _: &TaskRun, _: &str, _: JobAccess) -> Result<CommandSpec> {
        unreachable!("no run is reviewed in these tests")
    }
    fn agent_job_command(&self, job: &AgentJobLaunch) -> Result<CommandSpec> {
        let mut launches = self.launches.lock().unwrap();
        let n = launches.len();
        launches.push(job.clone());
        self.saw_cases.lock().unwrap().push(
            job.cwd.join(".dagq/agents/demo/evals").exists()
                || job.cwd.join(".dagq/agent-cases").exists(),
        );
        let mut replies = self.replies.lock().unwrap();
        let reply = if replies.len() > 1 {
            replies.remove(0)
        } else {
            replies[0].clone()
        };
        let copy = self.out.join(format!("stdin-{n}.txt"));
        let mut command = CommandSpec::new("/bin/sh");
        command
            .current_dir(&job.cwd)
            .arg("-c")
            .arg(format!(
                "cat > {}; printf '%s\\n' {}",
                shell_path(&copy),
                shell_path(&reply)
            ))
            .stdin(job.prompt.as_str());
        Ok(command)
    }
    fn job_session(&self, _: &str, _: Option<i64>) -> Option<JobSession> {
        Some(JobSession {
            tokens: Some(ExecutionTokens {
                tokens: Some(TokenUsage {
                    input: 1_000,
                    output: 100,
                    cost_usd: Some(0.25),
                    ..TokenUsage::default()
                }),
                ..ExecutionTokens::default()
            }),
            ..JobSession::default()
        })
    }
    fn review_timeout(&self) -> Duration {
        Duration::from_secs(30)
    }
}

/// Commit the agent `demo` (its definition, its dev cases and their
/// patch) and an agent `huge` whose definition is past the agent job's
/// limit on the landing branch; the cases' base commit.
fn commit_agents(repo: &Path) -> String {
    let patch = "diff --git a/notes.txt b/notes.txt\nnew file mode 100644\n--- /dev/null\n+++ b/notes.txt\n@@ -0,0 +1 @@\n+hello\n";
    let hash = format!("{:x}", Sha256::digest(patch.as_bytes()));
    let patches = repo.join(".dagq/agent-cases/patches");
    fs::create_dir_all(&patches).unwrap();
    fs::write(patches.join(format!("{hash}.patch")), patch).unwrap();
    let write = |base: &str| {
        let case = |id: &str, verdict: &str, codes: &[&str]| {
            json!({"id": id, "source": "handmade", "made_by": "test", "base_commit": base,
                   "patch": hash, "review": {"input": {}, "expected": {"verdict": verdict, "codes": codes}}})
        };
        for (agent, definition) in [
            ("demo", DEFINITION.to_owned()),
            (
                "huge",
                "x".repeat(dagq::application::prompt::AGENT_JOB_DEFINITION_BYTES + 1),
            ),
        ] {
            let dir = repo.join(format!(".dagq/agents/{agent}/evals"));
            fs::create_dir_all(&dir).unwrap();
            fs::write(dir.parent().unwrap().join("AGENT.md"), definition).unwrap();
            let list = json!({"agent": agent, "role": "review", "codes": ["D-1"], "k": 1, "cases": [
                case("needs-d1", "violation", &["D-1"]),
                case("clean-one", "clean", &[]),
            ]});
            fs::write(dir.join("dev.json"), list.to_string()).unwrap();
        }
        git(repo, &["add", "."]);
        git(repo, &["commit", "-q", "-m", "agents"]);
    };
    // Twice: the cases' base commit is the first, which already holds case
    // lists and patches, as a production case's base does.
    write(&git_out(repo, &["rev-parse", "HEAD"]));
    let base = git_out(repo, &["rev-parse", "HEAD"]);
    write(&base);
    base
}

fn supervise(fx: &Fixture, agent: &EvalAgent) -> Value {
    runtime::supervise_with_reviewer(
        &fx.db,
        &fx.repo,
        &PlanWorkspace::default(),
        &fx.claude,
        agent,
        Path::new(env!("CARGO_BIN_EXE_dagq")),
        &options(0, Duration::from_secs(3600)),
    )
    .unwrap()
}

/// The payloads of the queue's events of `kind`, oldest first.
fn events(db: &Path, kind: EventKind) -> Vec<Value> {
    let queue = SqliteQueue::open(db).unwrap();
    let upto = queue.latest_event_id().unwrap();
    queue
        .events_of_between(&[kind.as_str()], EventId::new(0), upto, 10_000)
        .unwrap()
        .into_iter()
        .map(|event| event.payload)
        .collect()
}

/// A `headless_jobs` row of the eval: kind, label, provider, outcome.
type EvalJobRow = (String, Option<String>, Option<String>, Option<String>);

fn eval_jobs(db: &Path) -> Vec<EvalJobRow> {
    Connection::open(db)
        .unwrap()
        .prepare(
            "SELECT kind, label, provider, outcome FROM headless_jobs WHERE kind='agent_eval' ORDER BY id",
        )
        .unwrap()
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap()
}

/// A requested dev round runs one job of the agent per run of a case, on
/// the landing branch's definition and on each case's tree, with its
/// prompt on standard input and nothing of what the case expects in it;
/// its runs and scores are events. A definition past the agent job's limit
/// starts no round and records why, with its bytes and the limit.
#[test]
fn a_requested_round_runs_one_agent_job_per_run_and_records_its_scores() {
    let fx = fixture();
    let base = commit_agents(&fx.repo);
    let head = git_out(&fx.repo, &["rev-parse", "HEAD"]);
    let requested = common::cli::ok(&fx.db, &["agent", "eval", "demo"]);
    let id = requested["eval_id"].as_i64().unwrap();
    assert_eq!(requested["status"], "waiting");
    let huge = common::cli::ok(&fx.db, &["agent", "eval", "huge"])["eval_id"]
        .as_i64()
        .unwrap();
    let out = tempfile::tempdir().unwrap();
    let agent = EvalAgent::new(out.path(), &[violation(), clean()]);
    let outcome = supervise(&fx, &agent);
    assert_eq!(outcome["errors"], json!([]), "{outcome}");

    let started = events(&fx.db, EventKind::AgentEvalStarted);
    assert_eq!(started.len(), 1, "{started:?}");
    let started = &started[0];
    assert_eq!(started["eval_id"], id);
    assert_eq!(started["provider"], "claude");
    assert_eq!(started["definition_commit"], head);
    assert_eq!(started["planned_runs"], 2);
    assert_eq!(started["estimate"]["per_run_source"], "provider_default");
    assert_eq!(started["reserved_usd"], 0.8);

    // One job per run, each on the case's tree at the patch's commit.
    let launches = agent.launches();
    assert_eq!(launches.len(), 2);
    let run_started = events(&fx.db, EventKind::AgentEvalRunStarted);
    assert_eq!(run_started.len(), 2);
    for (n, launch) in launches.iter().enumerate() {
        assert!(launch.cwd.ends_with("tree"), "{launch:?}");
        let stdin = agent.stdin(n);
        assert_eq!(
            stdin, launch.prompt,
            "the prompt is the job's standard input"
        );
        assert!(stdin.contains(DEFINITION), "the definition whole");
        assert!(stdin.contains(&base));
        assert!(stdin.contains(&launch.material.display().to_string()));
        for word in ["needs-d1", "clean-one", "violation", "holdout", "dev.json"] {
            assert!(!stdin.contains(word), "{word} in {stdin}");
        }
        let bytes = &run_started[n]["prompt_bytes"];
        assert_eq!(bytes["total"], stdin.len(), "{bytes}");
        let sections: u64 = bytes["sections"]
            .as_object()
            .unwrap()
            .values()
            .map(|bytes| bytes.as_u64().unwrap())
            .sum();
        assert_eq!(sections, stdin.len() as u64, "{bytes}");
        assert_eq!(
            bytes["limit"],
            dagq::application::prompt::AGENT_JOB_PROMPT_LIMIT
        );
    }
    // The job reads no case of its own in its tree, and its material is
    // in a directory of its own.
    assert_eq!(*agent.saw_cases.lock().unwrap(), [false, false]);
    assert!(launches[0].cwd.parent().is_some());
    assert!(launches[0].material.parent().unwrap().ends_with("material"));
    let material = fs::read_to_string(&launches[0].material).unwrap();
    assert!(material.contains("+hello"), "{material}");
    assert!(
        !launches[0].cwd.exists(),
        "the case's tree is removed once the round finished"
    );

    let finished = events(&fx.db, EventKind::AgentEvalRunFinished);
    assert_eq!(finished.len(), 2);
    for run in &finished {
        assert_eq!(
            run["cost"],
            json!({"usd": 0.25, "source": "actual"}),
            "{run}"
        );
        assert_eq!(run["abandoned"], false);
    }
    let scores = events(&fx.db, EventKind::AgentEvalFinished);
    assert_eq!(scores.len(), 1);
    let scores = &scores[0];
    assert_eq!(scores["outcome"], "complete", "{scores}");
    assert_eq!(scores["passed"], true, "{scores}");
    for metric in [
        "verdict_recall",
        "verdict_precision",
        "codes_recall",
        "codes_precision",
    ] {
        assert_eq!(scores["scores"][metric], 1.0, "{metric}: {scores}");
    }
    assert_eq!(scores["runs"], 2);
    assert_eq!(scores["failed_cases"], json!([]));
    assert_eq!(scores["cost"]["spent_usd"], 0.5);
    assert_eq!(scores["cost"]["by_source"], json!({"actual": 0.5}));
    assert_eq!(scores["definition_digest"], started["definition_digest"]);
    assert_eq!(scores["case_set_digest"], started["case_set_digest"]);

    // Recorded as the eval's agent jobs, apart from the run review's.
    let jobs = eval_jobs(&fx.db);
    assert_eq!(jobs.len(), 2);
    assert_eq!(
        jobs[0],
        (
            "agent_eval".to_owned(),
            Some(format!("demo:{id}:needs-d1:0")),
            Some("claude".to_owned()),
            Some("ended".to_owned())
        )
    );

    // The definition past the limit started no round.
    let refused = events(&fx.db, EventKind::AgentEvalRefused);
    assert_eq!(refused.len(), 1, "{refused:?}");
    assert_eq!(refused[0]["eval_id"], huge);
    assert_eq!(refused[0]["reason"], "definition_over_limit");
    assert_eq!(
        refused[0]["definition_bytes"],
        dagq::application::prompt::AGENT_JOB_DEFINITION_BYTES + 1
    );
    assert_eq!(
        refused[0]["limit"],
        dagq::application::prompt::AGENT_JOB_DEFINITION_BYTES
    );

    // Read back with the CLI.
    let shown = common::cli::ok(&fx.db, &["agent", "result", &id.to_string()]);
    assert_eq!(shown["status"], "finished");
    assert_eq!(shown["passed"], true);
    let listed = common::cli::ok(&fx.db, &["agent", "results", "--agent", "huge"]);
    assert_eq!(listed["evals"][0]["status"], "refused");
}

/// Record round `id` as started by `owner` on `head` with one run of
/// `needs-d1` started, as a supervisor that ran it leaves it.
fn started_by(db: &Path, id: i64, head: &str, owner: &str) {
    let queue = SqliteQueue::open(db).unwrap();
    for (kind, payload) in [
        (
            EventKind::AgentEvalStarted,
            json!({"eval_id": id, "agent": "demo", "split": "dev", "provider": "claude",
                   "definition_commit": head, "definition_digest": "d", "case_set_digest": "s",
                   "planned": [["needs-d1", 1], ["clean-one", 1]], "planned_runs": 2,
                   "estimate": {"per_run_usd": 0.4}, "reserved_usd": 0.8,
                   "max_cost_usd": 30.0, "concurrency": 4, "threshold": 0.9,
                   "supervisor": owner}),
        ),
        (
            EventKind::AgentEvalRunStarted,
            json!({"eval_id": id, "case": "needs-d1", "round": 0, "provider": "claude"}),
        ),
    ] {
        queue.record_queue_event(kind, payload).unwrap();
    }
}

/// Record the agent job of `owner`'s run of `needs-d1` in round `id`,
/// whose process is `pid`.
fn job_of(db: &Path, id: i64, owner: &str, pid: u32) {
    Connection::open(db)
        .unwrap()
        .execute(
            "INSERT INTO headless_jobs(kind, label, attempt, pid, process_start, supervisor_token, started_at, provider)
             VALUES ('agent_eval', ?1, 0, ?2, ?3, ?4, unixepoch(), 'claude')",
            rusqlite::params![format!("demo:{id}:needs-d1:0"), pid, process_start(pid), owner],
        )
        .unwrap();
}

/// A supervisor died while a round ran: one run of it had started and its
/// job still runs. The supervisor that takes over stops that job with the
/// headless jobs' takeover (`headless_job_stopped`), ends the run as
/// abandoned with its estimate spent, reads the round back from its
/// events and starts the runs left, and the round finishes with every run.
#[test]
fn a_round_a_gone_supervisor_left_is_taken_up_and_its_left_runs_started() {
    let fx = fixture();
    commit_agents(&fx.repo);
    let head = git_out(&fx.repo, &["rev-parse", "HEAD"]);
    let id = common::cli::ok(&fx.db, &["agent", "eval", "demo"])["eval_id"]
        .as_i64()
        .unwrap();
    started_by(&fx.db, id, &head, "dead-supervisor");
    let old = orphan("sleep 120 & wait");
    job_of(&fx.db, id, "dead-supervisor", old);
    let out = tempfile::tempdir().unwrap();
    let agent = EvalAgent::new(out.path(), &[violation(), clean()]);
    let outcome = supervise(&fx, &agent);
    assert_eq!(outcome["errors"], json!([]), "{outcome}");

    let deadline = Instant::now() + Duration::from_secs(10);
    while pid_alive(old) && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(20));
    }
    assert!(!pid_alive(old), "the gone supervisor's job is stopped");
    let taken = events(&fx.db, EventKind::AgentEvalTakenUp);
    assert_eq!(taken.len(), 1, "{taken:?}");
    assert_eq!(taken[0]["from"], "dead-supervisor");
    let stopped = events(&fx.db, EventKind::HeadlessJobStopped);
    assert!(
        stopped
            .iter()
            .any(|stop| stop["kind"] == "agent_eval" && stop["pid"] == old),
        "{stopped:?}"
    );
    assert_eq!(eval_jobs(&fx.db)[0].3.as_deref(), Some("taken_over"));

    let finished = events(&fx.db, EventKind::AgentEvalRunFinished);
    assert_eq!(finished.len(), 3, "{finished:?}");
    assert_eq!(finished[0]["case"], "needs-d1");
    assert_eq!(finished[0]["abandoned"], true);
    assert_eq!(
        finished[0]["cost"],
        json!({"usd": 0.4, "source": "estimated"})
    );
    // Both runs left were started on the takeover, through the agent job.
    assert_eq!(agent.launches().len(), 2);
    let scores = &events(&fx.db, EventKind::AgentEvalFinished)[0];
    assert_eq!(scores["outcome"], "complete", "{scores}");
    assert_eq!(scores["passed"], true, "{scores}");
    assert_eq!(scores["runs"], 2);
    assert_eq!(
        scores["cost"]["by_source"],
        json!({"actual": 0.5, "estimated": 0.4})
    );
}

/// A round another supervisor on the queue runs (ADR-0054 decision 3) and
/// that still lives is left to it (ADR-t1728-1 decision 9): this
/// supervisor neither takes it up, nor stops or charges its run, nor
/// starts it or another round beside it.
#[test]
fn a_round_a_live_supervisor_runs_is_left_to_it() {
    let fx = fixture();
    commit_agents(&fx.repo);
    let head = git_out(&fx.repo, &["rev-parse", "HEAD"]);
    let id = common::cli::ok(&fx.db, &["agent", "eval", "demo"])["eval_id"]
        .as_i64()
        .unwrap();
    common::cli::ok(&fx.db, &["agent", "eval", "demo", "--k", "2"]);
    started_by(&fx.db, id, &head, "live-supervisor");
    // The other supervisor's process and its agent job's.
    let (owner, job) = (orphan("sleep 120"), orphan("sleep 120"));
    Connection::open(&fx.db)
        .unwrap()
        .execute(
            "INSERT INTO supervisors(token, pid, parallel, heartbeat_at)
             VALUES ('live-supervisor', ?1, 1, unixepoch())",
            [owner],
        )
        .unwrap();
    job_of(&fx.db, id, "live-supervisor", job);
    let out = tempfile::tempdir().unwrap();
    let agent = EvalAgent::new(out.path(), &[violation(), clean()]);
    let outcome = supervise(&fx, &agent);
    assert_eq!(outcome["errors"], json!([]), "{outcome}");

    let left_alone = pid_alive(job);
    for pid in [owner, job] {
        // SAFETY: kill(2) takes no pointer; both are this test's processes.
        unsafe { libc::kill(pid as libc::pid_t, libc::SIGKILL) };
    }
    assert!(left_alone, "the live supervisor's job is not stopped");
    assert!(agent.launches().is_empty(), "no run started");
    for kind in [
        EventKind::AgentEvalTakenUp,
        EventKind::AgentEvalRunFinished,
        EventKind::AgentEvalFinished,
        EventKind::HeadlessJobStopped,
    ] {
        assert_eq!(events(&fx.db, kind), Vec::<Value>::new(), "{kind:?}");
    }
    assert_eq!(events(&fx.db, EventKind::AgentEvalStarted).len(), 1);
    assert_eq!(events(&fx.db, EventKind::AgentEvalRunStarted).len(), 1);
}

/// A round outside the run slots (ADR-t1728-1 decision 9): with one slot,
/// a round that runs while the runs go on takes none of it. The second
/// task is claimed after the round started, and its review starts while
/// the round still runs: the agent jobs wait until that review has
/// started (its script makes the gate), and both runs land.
#[test]
fn a_running_round_takes_no_run_slot_and_holds_no_review() {
    use crate::runtime_support::{
        TestReviewer, TestWorkspace, VALID_AGENT, add_ready_task, fixture as run_fixture,
        supervise_options, supervise_reviewed_with, verdict,
    };
    let (_fx, repo, db) = run_fixture();
    commit_agents(&repo);
    let id = common::cli::ok(&db, &["agent", "eval", "demo"])["eval_id"]
        .as_i64()
        .unwrap();
    let mut queue = SqliteQueue::open(&db).unwrap();
    let first = TaskId::new(
        Connection::open(&db)
            .unwrap()
            .query_row("SELECT id FROM tasks WHERE title='test task'", [], |r| {
                r.get(0)
            })
            .unwrap(),
    );
    let second = add_ready_task(&mut queue, "second", &[first]);
    let gate = db.parent().unwrap().join("second-review-started");
    let gate_path = common::shell_path(&gate);
    let backend = TestWorkspace::new(&db, false, VALID_AGENT);
    let reviewer = TestReviewer::new(&[
        verdict("pass", &[], "fine"),
        format!("touch {gate_path}; {}", verdict("pass", &[], "fine")),
    ])
    .with_agent_jobs(&[format!(
        "cat >/dev/null; {}; printf '%s\\n' {}",
        common::await_path(&gate),
        shell_path(clean())
    )]);
    let outcome =
        supervise_reviewed_with(&db, &repo, &backend, &reviewer, &supervise_options(1, true));
    assert_eq!(outcome["errors"], json!([]), "{outcome}");

    let all = SqliteQueue::open(&db).unwrap().all_events().unwrap();
    let at = |kind: &str, task: Option<TaskId>| {
        all.iter()
            .find(|event| event.kind == kind && (task.is_none() || event.task_id == task))
            .unwrap_or_else(|| panic!("no {kind}"))
            .id
    };
    let started = at("agent_eval_started", None);
    let finished = at("agent_eval_finished", None);
    let claimed = at("run_claimed", Some(second));
    let reviewed = at("review_started", Some(second));
    assert!(
        started < claimed && claimed < reviewed && reviewed < finished,
        "{all:?}"
    );
    for task in [first, second] {
        assert!(
            all.iter()
                .any(|event| event.kind == "run_integrated" && event.task_id == Some(task)),
            "{task} landed"
        );
    }
    let scores = events(&db, EventKind::AgentEvalFinished);
    assert_eq!(scores[0]["eval_id"], id);
    assert_eq!(scores[0]["outcome"], "complete", "{scores:?}");
}

/// A case's program reviews run before its agent (ADR-t1728-1 (i)), each
/// read with its script from the landing branch's commit and run against
/// the case's tree: a case one of them rejects reaches no agent, is out of
/// the agent's scores and is named with the program in the round's
/// `program_stopped`, though its patch rewrites the script to pass; a
/// program whose paths a case does not touch does not run for it, and a
/// case each of whose programs passes goes to its agent.
#[test]
fn a_case_a_program_rejects_reaches_no_agent_and_is_out_of_the_scores() {
    use std::os::unix::fs::PermissionsExt;
    let fx = fixture();
    let scripts = fx.repo.join("scripts");
    fs::create_dir_all(&scripts).unwrap();
    for (name, text) in [
        // Passes in a case's tree only: the patch's files are there.
        ("intree.sh", "#!/bin/sh\ntest -e notes.txt -o -e bad.txt\n"),
        ("never.sh", "#!/bin/sh\nexit 1\n"),
        (
            "nobad.sh",
            "#!/bin/sh\necho bad.txt is not allowed >&2\nexit 1\n",
        ),
    ] {
        let path = scripts.join(name);
        fs::write(&path, text).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
    }
    let config = fx.repo.join("dagq.toml");
    let mut text = fs::read_to_string(&config).unwrap_or_default();
    text.push_str(concat!(
        "\n[review.programs.intree]\nscript = \"scripts/intree.sh\"\npaths = [\"**\"]\n",
        "\n[review.programs.never]\nscript = \"scripts/never.sh\"\npaths = [\"nothing/**\"]\n",
        "\n[review.programs.nobad]\nscript = \"scripts/nobad.sh\"\npaths = [\"bad.txt\"]\n",
    ));
    fs::write(&config, text).unwrap();
    git(&fx.repo, &["add", "."]);
    git(&fx.repo, &["commit", "-q", "-m", "programs"]);
    let base = git_out(&fx.repo, &["rev-parse", "HEAD"]);

    let notes = "diff --git a/notes.txt b/notes.txt\nnew file mode 100644\n--- /dev/null\n+++ b/notes.txt\n@@ -0,0 +1 @@\n+hello\n";
    // Adds bad.txt and rewrites the script that rejects it to pass.
    let bad = concat!(
        "diff --git a/bad.txt b/bad.txt\nnew file mode 100644\n--- /dev/null\n+++ b/bad.txt\n@@ -0,0 +1 @@\n+bad\n",
        "diff --git a/scripts/nobad.sh b/scripts/nobad.sh\n--- a/scripts/nobad.sh\n+++ b/scripts/nobad.sh\n",
        "@@ -1,3 +1,3 @@\n #!/bin/sh\n echo bad.txt is not allowed >&2\n-exit 1\n+exit 0\n",
    );
    let patches = fx.repo.join(".dagq/agent-cases/patches");
    fs::create_dir_all(&patches).unwrap();
    let mut hashes = Vec::new();
    for patch in [notes, bad] {
        let hash = format!("{:x}", Sha256::digest(patch.as_bytes()));
        fs::write(patches.join(format!("{hash}.patch")), patch).unwrap();
        hashes.push(hash);
    }
    let case = |id: &str, patch: &str, verdict: &str, codes: &[&str]| {
        json!({"id": id, "source": "handmade", "made_by": "test", "base_commit": base,
               "patch": patch, "review": {"input": {}, "expected": {"verdict": verdict, "codes": codes}}})
    };
    let dir = fx.repo.join(".dagq/agents/demo/evals");
    fs::create_dir_all(&dir).unwrap();
    fs::write(dir.parent().unwrap().join("AGENT.md"), DEFINITION).unwrap();
    let list = json!({"agent": "demo", "role": "review", "codes": ["D-1"], "k": 1, "cases": [
        case("needs-d1", &hashes[0], "violation", &["D-1"]),
        case("has-bad", &hashes[1], "clean", &[]),
    ]});
    fs::write(dir.join("dev.json"), list.to_string()).unwrap();
    git(&fx.repo, &["add", "."]);
    git(&fx.repo, &["commit", "-q", "-m", "agents"]);

    let id = common::cli::ok(&fx.db, &["agent", "eval", "demo"])["eval_id"]
        .as_i64()
        .unwrap();
    let out = tempfile::tempdir().unwrap();
    let agent = EvalAgent::new(out.path(), &[violation()]);
    let outcome = supervise(&fx, &agent);
    assert_eq!(outcome["errors"], json!([]), "{outcome}");

    // Only the case whose programs passed reached the agent.
    let launches = agent.launches();
    assert_eq!(launches.len(), 1, "{launches:?}");
    let run_started = events(&fx.db, EventKind::AgentEvalRunStarted);
    assert_eq!(run_started.len(), 1);
    assert_eq!(run_started[0]["case"], "needs-d1");

    let checked = events(&fx.db, EventKind::AgentEvalCaseChecked);
    let of = |case: &str| {
        checked
            .iter()
            .find(|check| check["case"] == case)
            .unwrap_or_else(|| panic!("no check of {case}: {checked:?}"))
    };
    assert_eq!(checked.len(), 2, "{checked:?}");
    let passed = of("needs-d1");
    assert_eq!(passed["eval_id"], id);
    assert_eq!(passed["outcome"], "passed", "{passed}");
    assert_eq!(passed["programs"], json!(["intree"]), "never does not run");
    let stopped = of("has-bad");
    assert_eq!(stopped["outcome"], "stopped", "{stopped}");
    assert_eq!(stopped["program"], "nobad");
    assert_eq!(stopped["programs"], json!(["intree", "nobad"]));
    assert!(
        stopped["stderr_tail"]
            .as_str()
            .unwrap()
            .contains("bad.txt is not allowed"),
        "the landing branch's script ran: {stopped}"
    );

    let scores = events(&fx.db, EventKind::AgentEvalFinished);
    assert_eq!(scores.len(), 1);
    let scores = &scores[0];
    assert_eq!(scores["outcome"], "complete", "{scores}");
    assert_eq!(scores["passed"], true, "{scores}");
    assert_eq!(scores["runs"], 1);
    assert_eq!(scores["failed_cases"], json!([]), "{scores}");
    assert_eq!(
        scores["program_stopped"],
        json!({"count": 1, "cases": [{"id": "has-bad", "program": "nobad"}]})
    );
    assert_eq!(scores["cost"]["spent_usd"], 0.25, "programs spend nothing");

    // Recorded as the eval's program jobs, apart from its agent jobs.
    let labels: Vec<Option<String>> = Connection::open(&fx.db)
        .unwrap()
        .prepare("SELECT label FROM headless_jobs WHERE kind='agent_eval_program' ORDER BY id")
        .unwrap()
        .query_map([], |r| r.get(0))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap();
    let mut labels: Vec<String> = labels.into_iter().flatten().collect();
    labels.sort();
    assert_eq!(
        labels,
        [
            format!("demo:{id}:has-bad:intree"),
            format!("demo:{id}:has-bad:nobad"),
            format!("demo:{id}:needs-d1:intree"),
        ]
    );
    assert_eq!(eval_jobs(&fx.db).len(), 1);
}

/// A case's program that cannot start (its script's `#!` line names no
/// interpreter there is) leaves the case's violations unknown: no agent job
/// starts, the case's check is recorded as failed, and the round closes
/// incomplete (`program_failed`) and does not pass (ADR-t1728-1 (i)).
#[test]
fn a_program_that_cannot_start_closes_the_round_incomplete() {
    use std::os::unix::fs::PermissionsExt;
    let fx = fixture();
    let scripts = fx.repo.join("scripts");
    fs::create_dir_all(&scripts).unwrap();
    let script = scripts.join("noexec.sh");
    fs::write(&script, "#!/nonexistent/interpreter\nexit 0\n").unwrap();
    fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).unwrap();
    let config = fx.repo.join("dagq.toml");
    let mut text = fs::read_to_string(&config).unwrap_or_default();
    text.push_str("\n[review.programs.noexec]\nscript = \"scripts/noexec.sh\"\npaths = [\"**\"]\n");
    fs::write(&config, text).unwrap();
    git(&fx.repo, &["add", "."]);
    git(&fx.repo, &["commit", "-q", "-m", "programs"]);
    commit_agents(&fx.repo);
    let id = common::cli::ok(&fx.db, &["agent", "eval", "demo"])["eval_id"]
        .as_i64()
        .unwrap();
    let out = tempfile::tempdir().unwrap();
    let agent = EvalAgent::new(out.path(), &[violation(), clean()]);
    let outcome = supervise(&fx, &agent);
    assert_eq!(outcome["errors"], json!([]), "{outcome}");

    assert!(agent.launches().is_empty(), "no agent job starts");
    assert!(events(&fx.db, EventKind::AgentEvalRunStarted).is_empty());
    let checked = events(&fx.db, EventKind::AgentEvalCaseChecked);
    assert_eq!(checked.len(), 1, "nothing starts after it: {checked:?}");
    assert_eq!(checked[0]["eval_id"], id);
    assert_eq!(checked[0]["outcome"], "failed", "{}", checked[0]);
    assert_eq!(checked[0]["program"], "noexec");
    assert_eq!(checked[0]["failure"], "start_failed");
    let scores = events(&fx.db, EventKind::AgentEvalFinished);
    assert_eq!(scores.len(), 1);
    assert_eq!(scores[0]["outcome"], "incomplete", "{}", scores[0]);
    assert_eq!(scores[0]["incomplete_reason"], "program_failed");
    assert_eq!(scores[0]["passed"], false);
}
