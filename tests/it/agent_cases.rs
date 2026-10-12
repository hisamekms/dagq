//! The production sets of an agent's cases through the supervisor loop
//! (ADR-t1728-1 (g)): a set asked for with `dagq agent cases` is made of
//! the production reviews that selected the agent, each labeled by an
//! agent job of the provider other than the review's, built on the queue's
//! data dir with its digest, and run whole by `dagq agent eval --set`,
//! which makes its files again from the events and Git. The labeling job
//! is played by the eval's stub provider. The decisions (which reviews
//! are cases, the labels, the disputes, the policy) are unit tests of
//! their modules; these are the boundary with the queue, Git and the
//! processes.

use crate::common;
use crate::plan_review::{Fixture, fixture, git};
use crate::runtime_support::agent_eval::{EvalAgent, commit_agents, events, supervise, violation};
use crate::runtime_support::{git_out, orphan};

use dagq::domain::EventKind;
use dagq::infrastructure::location::agent_cases_dir;
use dagq::infrastructure::sqlite::SqliteQueue;
use rusqlite::{Connection, params};
use serde_json::{Value, json};
use std::{
    fs,
    path::{Path, PathBuf},
};

/// A commit no ref of the repository reaches: a run's head that is gone.
const GONE: &str = "0123456789abcdef0123456789abcdef01234567";

/// Commit `config` as main's `dagq.toml`.
fn commit_config(repo: &Path, config: &str) {
    fs::write(repo.join("dagq.toml"), config).unwrap();
    git(repo, &["add", "dagq.toml"]);
    git(repo, &["commit", "-q", "-m", "config"]);
}

/// The reviewed change: a commit beside main that adds `notes.txt`; its
/// base and head.
fn reviewed_change(repo: &Path) -> (String, String) {
    let base = git_out(repo, &["rev-parse", "HEAD"]);
    git(repo, &["switch", "-q", "-c", "run-branch"]);
    fs::write(repo.join("notes.txt"), "hello\n").unwrap();
    git(repo, &["add", "notes.txt"]);
    git(repo, &["commit", "-q", "-m", "notes"]);
    let head = git_out(repo, &["rev-parse", "HEAD"]);
    git(repo, &["switch", "-q", "main"]);
    (base, head)
}

/// A change whose branch was deleted: its head's object is still in the
/// repository (no garbage collection ran), and no ref reaches it.
fn dropped_change(repo: &Path) -> String {
    git(repo, &["switch", "-q", "-c", "dropped-branch"]);
    fs::write(repo.join("dropped.txt"), "gone\n").unwrap();
    git(repo, &["add", "dropped.txt"]);
    git(repo, &["commit", "-q", "-m", "dropped"]);
    let head = git_out(repo, &["rev-parse", "HEAD"]);
    git(repo, &["switch", "-q", "main"]);
    git(repo, &["branch", "-q", "-D", "dropped-branch"]);
    head
}

/// The definition of `demo` that links rule D-1 to its section of the
/// rules' document, and the document, on main.
const LINKED: &str = "---\ndescription: checks D-1\n---\n\
- D-1: flag the notes file — [the notes rule](../../../docs/rules.md#notes).\n";

fn commit_linked_definition(repo: &Path) {
    fs::create_dir_all(repo.join("docs")).unwrap();
    fs::write(
        repo.join("docs/rules.md"),
        "# Rules\n\n## Notes\n\nNo notes.txt.\n",
    )
    .unwrap();
    fs::write(repo.join(".dagq/agents/demo/AGENT.md"), LINKED).unwrap();
    git(repo, &["add", "."]);
    git(repo, &["commit", "-q", "-m", "linked definition"]);
}

/// Record run `run` of a completed task `task`, landed, with a review of
/// `base...head` that selected `agents`, and `verdict` as `demo`'s
/// judgment, at `at`.
fn reviewed(
    db: &Path,
    (task, run): (i64, &str),
    (base, head): (&str, &str),
    agents: &[&str],
    verdict: &str,
    at: &str,
) {
    let conn = Connection::open(db).unwrap();
    conn.execute(
        "INSERT INTO tasks(id,title,description,acceptance,verification_commands,status)
         VALUES (?1,'reviewed','','criteria','[]','completed')",
        params![task],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO task_runs(id,task_id,status,requested_provider,actual_provider,base_commit,result_commit,workspace_closed_at)
         VALUES (?1,?2,'integrated','claude','claude',?3,?4,unixepoch())",
        params![run, task, base, head],
    )
    .unwrap();
    let started = json!({"attempt": 1, "subagents": {"commit": base, "base": base, "head": head,
        "agents": agents.iter().map(|agent| json!({"agent": agent})).collect::<Vec<_>>()}});
    let finished = json!({"attempt": 1, "verdict": verdict, "agents": [
        {"agent": "demo", "status": "completed", "verdict": verdict,
         "reasons": ["what production said"], "summary": "s"}]});
    for (kind, payload) in [("review_started", started), ("review_finished", finished)] {
        conn.execute(
            "INSERT INTO run_events(task_id,run_id,kind,payload,created_at) VALUES (?1,?2,?3,?4,?5)",
            params![task, run, kind, payload.to_string(), at],
        )
        .unwrap();
    }
}

/// A set asked for is made of the period's reviews that selected the
/// agent, a review whose head is gone left out with why; each case is
/// labeled by a job of the provider other than the review's (Claude here,
/// the review's being Codex) from the rules alone, its tree without the
/// agents' definitions; the case whose label disagrees with the production
/// verdict is disputed and asked of a person; the set's files and digest
/// are on the queue's side; and a person's decision clears the dispute.
#[test]
fn a_set_is_made_of_the_agents_reviews_and_labeled_by_another_provider() {
    let fx = fixture();
    commit_agents(&fx.repo);
    commit_linked_definition(&fx.repo);
    commit_config(&fx.repo, "[roles.review]\nprovider = 'codex'\n");
    let (base, head) = reviewed_change(&fx.repo);
    let dropped = dropped_change(&fx.repo);
    let at = "2026-10-06T00:00:00.000Z";
    reviewed(&fx.db, (11, "r1"), (&base, &head), &["demo"], "pass", at);
    reviewed(&fx.db, (12, "r2"), (&base, GONE), &["demo"], "revise", at);
    reviewed(
        &fx.db,
        (15, "r5"),
        (&base, &dropped),
        &["demo"],
        "revise",
        at,
    );
    reviewed(&fx.db, (13, "r3"), (&base, &head), &["other"], "pass", at);
    let early = "2026-09-01T00:00:00.000Z";
    reviewed(&fx.db, (14, "r4"), (&base, &head), &["demo"], "pass", early);
    let asked = common::cli::ok(&fx.db, &["agent", "cases", "demo", "--since", "2026-10-01"]);
    let set = asked["set_id"].as_i64().unwrap();
    assert_eq!(asked["status"], "waiting");

    let out = tempfile::tempdir().unwrap();
    let agent = EvalAgent::new(out.path(), &[violation()]);
    let outcome = supervise(&fx, &agent);
    assert_eq!(outcome["errors"], json!([]), "{outcome}");

    let prepared = &events(&fx.db, EventKind::AgentCasesPrepared)[0];
    assert_eq!(prepared["set_id"], set);
    let cases = prepared["cases"].as_array().unwrap();
    assert_eq!(cases.len(), 1, "{prepared}");
    assert_eq!(cases[0]["id"], "prod-r1-a1");
    assert_eq!(cases[0]["base"], base.as_str());
    assert_eq!(cases[0]["head"], head.as_str());
    assert_eq!(cases[0]["production"]["verdict"], "pass");
    // A head no ref reaches is left out with why, whether its object is
    // gone or kept.
    let skipped = prepared["skipped"].as_array().unwrap();
    assert_eq!(skipped.len(), 2, "{prepared}");
    for (skip, (run, commit)) in skipped.iter().zip([("r2", GONE), ("r5", dropped.as_str())]) {
        assert_eq!(skip["run_id"], run);
        let why = skip["reason"].as_str().unwrap();
        assert!(why.contains(commit) && why.contains("no ref"), "{prepared}");
    }
    assert_eq!(prepared["frame"]["codes"], json!(["D-1"]));
    assert_eq!(
        prepared["frame"]["sections"],
        json!({"D-1": ["docs/rules.md#notes (the notes rule)"]})
    );

    // One labeling job, on Claude, from the rules alone.
    let launches = agent.launches();
    assert_eq!(launches.len(), 1);
    let stdin = agent.stdin(0);
    // The rule's section reaches the job, the definition's words do not.
    assert!(
        !stdin.contains("flag the notes file"),
        "the definition is not given: {stdin}"
    );
    assert!(stdin.contains("- docs/rules.md\n"), "{stdin}");
    assert!(
        stdin.contains("- D-1: docs/rules.md#notes (the notes rule)"),
        "{stdin}"
    );
    assert_eq!(*agent.saw_cases.lock().unwrap(), [false]);
    let labeled = &events(&fx.db, EventKind::AgentCasesLabeled)[0];
    assert_eq!(labeled["provider"], "claude");
    assert_eq!(labeled["label"]["verdict"], "violation");
    assert_eq!(labeled["label"]["codes"], json!(["D-1"]));

    // Built on the queue's side, the disputed case asked of a person.
    let built = &events(&fx.db, EventKind::AgentCasesBuilt)[0];
    assert_eq!(built["cases"], json!(["prod-r1-a1"]), "{built}");
    assert_eq!(built["disputed"][0]["case"], "prod-r1-a1", "{built}");
    let ask_id = built["ask_id"].as_i64().unwrap();
    let asks = common::cli::ok(&fx.db, &["asks"]);
    let ask = asks["asks"]
        .as_array()
        .unwrap()
        .iter()
        .find(|ask| ask["id"] == ask_id)
        .unwrap_or_else(|| panic!("ask {ask_id} in {asks}"));
    assert_eq!(ask["kind"], "decide");
    assert!(
        ask["question"]
            .as_str()
            .unwrap()
            .contains("dagq agent decide"),
        "{ask}"
    );
    let dir = agent_cases_dir(&fx.db).join(set.to_string());
    let list: Value =
        serde_json::from_str(&fs::read_to_string(dir.join("cases.json")).unwrap()).unwrap();
    let patch = list["cases"][0]["patch"].as_str().unwrap();
    let text = fs::read_to_string(dir.join("patches").join(format!("{patch}.patch"))).unwrap();
    assert!(text.contains("+hello"), "{text}");
    assert_ne!(list["cases"][0]["disputed"], Value::Null);

    let shown = common::cli::ok(&fx.db, &["agent", "set", &set.to_string()]);
    assert_eq!(shown["status"], "built");
    assert_eq!(shown["digest"], built["digest"]);
    common::cli::ok(
        &fx.db,
        &[
            "agent",
            "decide",
            &set.to_string(),
            "prod-r1-a1",
            "--verdict",
            "violation",
            "--codes",
            "D-1",
            "--reason",
            "notes.txt breaks D-1",
        ],
    );
    let shown = common::cli::ok(&fx.db, &["agent", "set", &set.to_string()]);
    assert_eq!(shown["disputed"], json!([]), "{shown}");
    let decided = &events(&fx.db, EventKind::AgentCasesDecided)[0];
    assert_eq!(decided["decided_by"], "user");
}

/// A set of `demo` asked for, prepared and labeled, as a supervisor that
/// labels leaves it, with its one case's patch written: its queue, id,
/// directory and the patch's hash. The case `prod-r1-a1` reviewed the
/// change that adds `notes.txt`, which production and the label both found
/// a violation of D-1.
fn labeled_set(fx: &Fixture) -> (SqliteQueue, i64, PathBuf, String) {
    commit_agents(&fx.repo);
    let (base, head) = reviewed_change(&fx.repo);
    let queue = SqliteQueue::open(&fx.db).unwrap();
    let set = common::cli::ok(&fx.db, &["agent", "cases", "demo"])["set_id"]
        .as_i64()
        .unwrap();
    let dir = agent_cases_dir(&fx.db).join(set.to_string());
    // The patch as the supervisor writes it.
    let patch = dagq::application::agent_cases::write_patch(
        &dagq::infrastructure::adapters::GitRepository::inspect(&fx.repo).unwrap(),
        &dagq::infrastructure::run_files::LocalRunFiles,
        &dir,
        (&base, &head),
    )
    .unwrap();
    for (kind, payload) in [
        (
            EventKind::AgentCasesPrepared,
            json!({"set_id": set, "frame": {"codes": ["D-1"], "k": 1, "rules": [], "commit": head},
                   "cases": [{"id": "prod-r1-a1", "run_id": "r1", "attempt": 1, "base": base,
                              "head": head, "production": {"verdict": "revise", "reasons": []},
                              "reviewed_at": "t", "patch": patch}],
                   "skipped": []}),
        ),
        (
            EventKind::AgentCasesLabeled,
            json!({"set_id": set, "case": "prod-r1-a1", "made_by": "codex",
                   "label": {"verdict": "violation", "codes": ["D-1"], "reason": "adds notes.txt"}}),
        ),
    ] {
        queue.record_queue_event(kind, payload).unwrap();
    }
    (queue, set, dir, patch)
}

/// A built set is run whole as production by `agent eval --set`, never
/// with `--cases`; with its files gone, the round makes them again from
/// the events and Git, and its cases' digest is the set's.
#[test]
fn a_set_is_run_whole_and_its_files_are_made_again() {
    let fx = fixture();
    let (queue, set, dir, patch) = labeled_set(&fx);
    // The supervisor builds the labeled set, whose files are then lost.
    let out = tempfile::tempdir().unwrap();
    let agent = EvalAgent::new(out.path(), &[violation()]);
    supervise(&fx, &agent);
    let built = events(&fx.db, EventKind::AgentCasesBuilt);
    assert_eq!(built.len(), 1, "{built:?}");
    let digest = built[0]["digest"].clone();
    assert!(dir.join("cases.json").exists());
    fs::remove_dir_all(&dir).unwrap();

    let refused = common::cli::invoke(
        &fx.db,
        &[
            "agent",
            "eval",
            "demo",
            "--set",
            &set.to_string(),
            "--cases",
            "prod-r1-a1",
        ],
    );
    assert!(!refused.status.success());
    let said = String::from_utf8_lossy(&refused.stdout) + String::from_utf8_lossy(&refused.stderr);
    assert!(said.contains("takes no --cases"), "{said}");
    let asked = common::cli::ok(
        &fx.db,
        &["agent", "eval", "demo", "--set", &set.to_string()],
    );
    assert_eq!(asked["split"], "production");
    assert_eq!(asked["set"], set);
    let outcome = supervise(&fx, &agent);
    assert_eq!(outcome["errors"], json!([]), "{outcome}");

    let started = &events(&fx.db, EventKind::AgentEvalStarted)[0];
    assert_eq!(started["case_set_digest"], digest, "{started}");
    assert_eq!(started["cases"], json!(["prod-r1-a1"]));
    assert_eq!(started["set"], set);
    let requested = &events(&fx.db, EventKind::AgentEvalRequested)[0];
    assert_eq!(requested["cases"], Value::Null);
    assert_eq!(requested["set"], set);
    assert!(dir.join("cases.json").exists(), "made again");
    assert!(dir.join("patches").join(format!("{patch}.patch")).exists());
    let scores = &events(&fx.db, EventKind::AgentEvalFinished)[0];
    assert_eq!(scores["outcome"], "complete", "{scores}");
    assert_eq!(scores["passed"], true, "{scores}");

    // The set's key ran: a second round of it is refused, but a person's
    // rerun.
    common::cli::ok(
        &fx.db,
        &["agent", "eval", "demo", "--set", &set.to_string()],
    );
    supervise(&fx, &agent);
    let refused = events(&fx.db, EventKind::AgentEvalRefused);
    assert_eq!(refused[0]["reason"], "holdout_used", "{refused:?}");
    // A case whose head no ref reaches any more is left out of the view
    // with why, though its patch's file is still there.
    git(&fx.repo, &["branch", "-q", "-D", "run-branch"]);
    let events_of_sets =
        dagq::application::agent_cases::CaseSetStore::cases_events(&queue).unwrap();
    let sets = dagq::application::agent_cases::sets_of(&events_of_sets);
    let made = dagq::application::agent_cases::view(
        &sets[0],
        &dagq::infrastructure::adapters::GitRepository::inspect(&fx.repo).unwrap(),
        &dagq::infrastructure::run_files::LocalRunFiles,
        &dir,
    )
    .unwrap();
    assert!(dir.join("patches").join(format!("{patch}.patch")).exists());
    assert_eq!(made.list["cases"], json!([]), "{:?}", made.left_out);
    assert_eq!(made.left_out.len(), 1);
    assert_eq!(made.left_out[0].0, "prod-r1-a1");
    assert!(made.left_out[0].1.contains("no ref"), "{:?}", made.left_out);
}

/// A set has one owner (ADR-t1728-1 (g)): a set another live supervisor
/// took is left to it (nothing prepared, no job), and once that owner is
/// gone it is taken over and made.
#[test]
fn a_set_another_live_supervisor_makes_is_left_to_it_until_it_is_gone() {
    let fx = fixture();
    commit_agents(&fx.repo);
    commit_config(&fx.repo, "[roles.review]\nprovider = 'codex'\n");
    let (base, head) = reviewed_change(&fx.repo);
    reviewed(
        &fx.db,
        (11, "r1"),
        (&base, &head),
        &["demo"],
        "revise",
        "2026-10-06T00:00:00.000Z",
    );
    let set = common::cli::ok(&fx.db, &["agent", "cases", "demo"])["set_id"]
        .as_i64()
        .unwrap();
    let owner = orphan("sleep 120");
    let conn = Connection::open(&fx.db).unwrap();
    conn.execute(
        "INSERT INTO supervisors(token, pid, parallel, heartbeat_at)
         VALUES ('live-supervisor', ?1, 1, unixepoch())",
        [owner],
    )
    .unwrap();
    dagq::infrastructure::sqlite::SqliteQueue::open(&fx.db)
        .unwrap()
        .record_queue_event(
            EventKind::AgentCasesTaken,
            json!({"set_id": set, "supervisor": "live-supervisor"}),
        )
        .unwrap();
    let out = tempfile::tempdir().unwrap();
    let agent = EvalAgent::new(out.path(), &[violation()]);
    let outcome = supervise(&fx, &agent);
    // SAFETY: kill(2) takes no pointer; the process is this test's.
    unsafe { libc::kill(owner as libc::pid_t, libc::SIGKILL) };
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    assert!(agent.launches().is_empty(), "no label job");
    assert!(events(&fx.db, EventKind::AgentCasesPrepared).is_empty());
    assert_eq!(events(&fx.db, EventKind::AgentCasesTaken).len(), 1);
    // The take is one write transaction over the queue's events and the
    // registered supervisors: the live owner keeps its set, unrecorded,
    // and another is refused it.
    let queue = dagq::infrastructure::sqlite::SqliteQueue::open(&fx.db).unwrap();
    let alive = |_: u32| true;
    assert!(queue.take_case_set(set, "live-supervisor", &alive).unwrap());
    assert!(!queue.take_case_set(set, "another", &alive).unwrap());
    assert_eq!(events(&fx.db, EventKind::AgentCasesTaken).len(), 1);

    conn.execute(
        "DELETE FROM supervisors WHERE token = 'live-supervisor'",
        [],
    )
    .unwrap();
    supervise(&fx, &agent);
    let taken = events(&fx.db, EventKind::AgentCasesTaken);
    assert_eq!(taken.len(), 2, "{taken:?}");
    assert_eq!(taken[1]["from"], "live-supervisor");
    assert_eq!(agent.launches().len(), 1);
    assert_eq!(events(&fx.db, EventKind::AgentCasesBuilt).len(), 1);
    // A built set is nobody's to take.
    let owner = taken[1]["supervisor"].as_str().unwrap();
    assert!(!queue.take_case_set(set, owner, &alive).unwrap());
}

/// A round of a set reads the set's labels as they were when it started
/// (ADR-t1728-1 (g)): a person's decision recorded after the start does not
/// reach a supervisor that takes the round up, so the round is scored on
/// the expectation it started with, as its first supervisor would have.
/// Without that, the decision (clean) would make the agent's violation a
/// false positive.
#[test]
fn a_set_round_taken_up_is_scored_on_the_labels_it_started_with() {
    let fx = fixture();
    let (queue, set, _, _) = labeled_set(&fx);
    let out = tempfile::tempdir().unwrap();
    let agent = EvalAgent::new(out.path(), &[violation()]);
    supervise(&fx, &agent);
    assert_eq!(events(&fx.db, EventKind::AgentCasesBuilt).len(), 1);
    let id = common::cli::ok(
        &fx.db,
        &["agent", "eval", "demo", "--set", &set.to_string()],
    )["eval_id"]
        .as_i64()
        .unwrap();
    // A supervisor now gone started the round on the set as it was then.
    let through = queue.latest_event_id().unwrap().as_i64();
    let commit = git_out(&fx.repo, &["rev-parse", "HEAD"]);
    queue
        .record_queue_event(
            EventKind::AgentEvalStarted,
            json!({"eval_id": id, "agent": "demo", "split": "production", "provider": "claude",
                   "definition_commit": commit, "definition_digest": "d", "case_set_digest": "s",
                   "planned": [["prod-r1-a1", 1]], "planned_runs": 1,
                   "estimate": {"per_run_usd": 0.4}, "reserved_usd": 0.4,
                   "max_cost_usd": 30.0, "concurrency": 4, "threshold": 0.9,
                   "supervisor": "dead-supervisor", "set": set, "set_through": through}),
        )
        .unwrap();
    // Then a person decides the case's label otherwise.
    common::cli::ok(
        &fx.db,
        &[
            "agent",
            "decide",
            &set.to_string(),
            "prod-r1-a1",
            "--verdict",
            "clean",
            "--reason",
            "the rule allows it",
        ],
    );
    let outcome = supervise(&fx, &agent);
    assert_eq!(outcome["errors"], json!([]), "{outcome}");

    let taken = events(&fx.db, EventKind::AgentEvalTakenUp);
    assert_eq!(taken.len(), 1, "{taken:?}");
    assert_eq!(taken[0]["from"], "dead-supervisor");
    let scores = &events(&fx.db, EventKind::AgentEvalFinished)[0];
    assert_eq!(scores["eval_id"], id);
    assert_eq!(scores["outcome"], "complete", "{scores}");
    assert_eq!(scores["runs"], 1, "{scores}");
    assert_eq!(
        scores["passed"], true,
        "the start's label, violation: {scores}"
    );
    assert_eq!(scores["failed_cases"], json!([]), "{scores}");
    // The set itself has the person's decision for its next round.
    let shown = common::cli::ok(&fx.db, &["agent", "set", &set.to_string()]);
    assert_eq!(
        shown["cases"][0]["review"]["expected"]["verdict"], "clean",
        "{shown}"
    );
}
