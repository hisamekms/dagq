//! Runtime tests: the limit of the conflict precheck's requests to the live
//! session of a passed run (ADR-0027 decision 4), which are conflict-only
//! attempts after a passed review (ADR-0047 decision 24, task 511).
use crate::{common, runtime_support};
use dagq::domain::EventKind;
use runtime_support::*;

use dagq::domain::AskKind;
use dagq::{application::ProcessControl, infrastructure::adapters::SystemProcesses};

/// Only this fixture's children matter: their starts are stable until they
/// end. Host process listings and repeated start probes are outside Git.
#[derive(Default)]
struct WrapperProcesses(Mutex<HashMap<u32, String>>);
impl ProcessControl for WrapperProcesses {
    fn alive(&self, pid: u32) -> bool {
        SystemProcesses.alive(pid)
    }
    fn terminate(&self, pid: u32) -> Result<()> {
        SystemProcesses.terminate(pid)
    }
    fn interrupt(&self, pid: u32) -> Result<()> {
        SystemProcesses.interrupt(pid)
    }
    fn kill(&self, pid: u32) -> Result<()> {
        SystemProcesses.kill(pid)
    }
    fn start_identity(&self, pid: u32) -> Option<String> {
        if !self.alive(pid) {
            return None;
        }
        let mut starts = self.0.lock().unwrap();
        if let Some(start) = starts.get(&pid) {
            return Some(start.clone());
        }
        let start = SystemProcesses.start_identity(pid)?;
        starts.insert(pid, start.clone());
        Some(start)
    }
    fn list(&self) -> Result<Vec<dagq::domain::recovery::ProcessInfo>> {
        Ok(vec![])
    }
    fn executables(&self) -> Result<Vec<dagq::domain::disk::ProcessExecutable>> {
        Ok(vec![])
    }
}

/// A passed run already used its conflict request limit. Its next real
/// Git conflict starts no further turn; its conflicting landing parks it
/// and the retry inherits its own commit and lands. The counter's frontier
/// is unit-tested; live-session resolution has its own Git representative.
#[test]
fn precheck_conflicts_up_to_the_conflict_only_limit_retry_the_task_with_its_branch() {
    // The counter's default and configured frontiers are pure unit tests.
    // Start at the frontier so this Git case runs only the exhausted
    // precheck, the real conflicting landing and the inherited retry.
    let limit = 1;
    let (_dir, repo, db) = fixture();
    fs::write(
        repo.join("dagq.toml"),
        "[resume]\nconflict_only_limit = 1\n",
    )
    .unwrap();
    git(&repo, &["add", "dagq.toml"]);
    git(&repo, &["commit", "-q", "-m", "set the resume limit"]);
    let first = "\"$(git rev-parse --path-format=absolute --git-common-dir)/first-run\"";
    // Host retries receive the earlier branch through the prompt and ref;
    // the worker must take its changes over before writing its new receipt.
    let agent = format!(
        "git cherry-pick --no-commit \"refs/dagq/runs/$(cat {first})\" || {{ \
         printf 'resolved by the retry\\n' > change.txt; git add change.txt; }}; \
         git commit -q -m 'carry the earlier work'; \
         receipt \"$(git rev-parse HEAD)\"; idle; await_exit"
    );
    let processes = Arc::new(WrapperProcesses::default());
    let mut backend = TestWorkspace::new(&db, false, &agent);
    backend.wrapper_processes = Some(processes.clone());
    let handle = backend.stand_in();
    let pid = dagq::domain::background_wrapper::BackgroundHandle::parse(&handle)
        .unwrap()
        .pid;
    let run = orphan_run(&repo, &db, "earlier-supervisor", pid, pid);
    fs::write(repo.join(".git/first-run"), run.id().as_str()).unwrap();
    let tree = Path::new(run.worktree_path().unwrap());
    fs::write(tree.join("change.txt"), "the run's work\n").unwrap();
    fs::write(tree.join("carried.txt"), "carried from the first run\n").unwrap();
    git(tree, &["add", "change.txt", "carried.txt"]);
    git(
        tree,
        &["commit", "-q", "-m", "work before the last conflict"],
    );
    let head = git_out(tree, &["rev-parse", "HEAD"]);
    write_receipt(&run, &head, "succeeded", "work before the last conflict");
    Connection::open(&db)
        .unwrap()
        .execute(
            "UPDATE task_runs SET status='awaiting_integration', result_commit=?2 WHERE id=?1",
            rusqlite::params![run.id(), head],
        )
        .unwrap();
    let queue = SqliteQueue::open(&db).unwrap();
    let run = queue.run(run.id()).unwrap();
    queue.record_runtime_event(run.id(), EventKind::ValidationFinished,
        json!({"status": "awaiting_integration", "receipt": session_receipt(&run, &head, "succeeded", "work before the last conflict")})).unwrap();
    // One earlier resolution request already used this test's limit.
    queue
        .record_runtime_event(
            run.id(),
            EventKind::ConflictPrecheck,
            json!({"attempt": 1, "requested": true}),
        )
        .unwrap();
    // This pass completed the earlier resolution: adoption must precheck
    // the new main, rather than wait again for the historical request.
    queue
        .record_runtime_event(
            run.id(),
            EventKind::ReviewFinished,
            bound_as_judged(
                &repo,
                &run,
                EventKind::ReviewFinished,
                json!({"attempt": 1, "verdict": "pass", "reasons": [], "summary": "meets it"}),
            ),
        )
        .unwrap();
    age_lease(&db, &run, 31);
    fs::write(repo.join("change.txt"), "main moved\n").unwrap();
    git(&repo, &["add", "change.txt"]);
    git(&repo, &["commit", "-q", "-m", "main moves into the run"]);
    let reviewer = TestReviewer::new(&[verdict("pass", &[], "meets the acceptance")]);
    // Only Git is this case's boundary. The earlier wrapper is a port
    // stand-in: acknowledge its exit request without another worker turn.
    // The scoped thread is joined on every path and its poll is bounded.
    let exit = dagq::domain::turn::exit_path(Path::new(run.run_dir().unwrap()));
    let deadline = common::deadline(Duration::from_secs(20));
    let outcome = std::thread::scope(|scope| {
        let wrapper = scope.spawn(|| {
            while !exit.is_file() && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(5));
            }
            let observed = exit.is_file();
            backend
                .stands
                .lock()
                .unwrap()
                .retain(|(id, _)| id != &handle);
            let mut q = SqliteQueue::open(&db).unwrap();
            dagq::application::RunCoordination::wrapper_exited(&mut q, run.id(), pid, 0).unwrap();
            observed
        });
        let mut options = supervise_options(4, true);
        options.processes = Some(runtime::ProcessesPort(processes));
        let outcome = supervise_reviewed_with(&db, &repo, &backend, &reviewer, &options);
        assert!(
            wrapper.join().unwrap(),
            "the earlier wrapper received no exit request"
        );
        outcome
    });
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    let mut queue = SqliteQueue::open(&db).unwrap();
    let detail = queue.show(TaskId::new(1)).unwrap();
    assert_eq!(detail.runs.len(), 2, "{:?}", event_kinds(&detail));
    let first = detail.runs[0].clone();
    assert_eq!(first.status(), RunStatus::Failed);
    assert_eq!(detail.runs[1].status(), RunStatus::Integrated);
    assert_eq!(detail.task.status(), TaskStatus::Completed);
    assert!(
        repo.join("carried.txt").is_file(),
        "first head {:?}; expected {head}; inherited {:?}; events {:?}",
        first.result_commit(),
        payloads(&detail, "run_inherited"),
        event_kinds(&detail)
    );
    assert_eq!(
        fs::read_to_string(repo.join("carried.txt")).unwrap(),
        "carried from the first run\n"
    );
    let of_first = |kind: &str| -> Vec<&Value> {
        detail
            .events
            .iter()
            .filter(|e| e.kind.as_str() == kind && e.run_id.as_ref() == Some(first.id()))
            .map(|e| &e.payload)
            .collect()
    };
    let prechecks = of_first("conflict_precheck");
    let requested: Vec<&Value> = prechecks.iter().map(|p| &p["requested"]).collect();
    let mut expected = vec![&json!(true); limit];
    expected.push(&json!(false));
    assert_eq!(requested, expected);
    let last = prechecks[limit];
    assert_eq!(last["attempt"], limit + 1);
    assert_eq!(last["exhausted"], true);
    assert!(last.get("asked").is_none(), "{last}");
    assert!(
        session_texts(&first).is_empty(),
        "the exhausted run gets no new request"
    );
    // Its landing conflicted and parked it; no resume started.
    let deferred = of_first("integration_deferred");
    assert_eq!(deferred.len(), 1, "{:?}", event_kinds(&detail));
    assert_eq!(deferred[0]["code"], "rebase_conflict");
    assert!(of_first("resume_started").is_empty());
    // Retried with its branch carried over, with nobody asked.
    let finished = of_first("triage_finished");
    assert_eq!(finished.len(), 1, "{:?}", event_kinds(&detail));
    assert_eq!(finished[0]["by"], "runtime");
    assert_eq!(finished[0]["action"], "retry_inherit");
    assert_eq!(finished[0]["counted_resumes"], 0);
    assert_eq!(finished[0]["conflict_only_resumes"], 0);
    assert_eq!(finished[0]["conflict_requests"], limit);
    let repaired = of_first("auto_repaired");
    assert!(
        repaired.iter().any(
            |p| p["repair"] == "inherit_retry" && p["conditions"]["conflict_requests"] == limit
        ),
        "{repaired:?}"
    );
    let last_error = first.last_error().unwrap();
    assert!(
        last_error.starts_with(&format!(
            "resumed 0 times (0 of at most 3 counted) and asked {limit} times by the conflict precheck ({limit} of at most {limit} attempts for conflicts only after its review passed)"
        )),
        "{last_error}"
    );
    let asks = queue
        .asks(AskQuery {
            all: true,
            ..Default::default()
        })
        .unwrap();
    assert!(asks.is_empty(), "{asks:?}");
    assert!(
        detail
            .events
            .iter()
            .any(|e| e.kind == EventKind::RunInherited
                && e.run_id.as_ref() == Some(detail.runs[1].id())),
        "{:?}",
        event_kinds(&detail)
    );
}

/// A run whose counted resumes (a failed verification, missing evidence)
/// are used up is not retried by the precheck: its conflict with main asks
/// a person, as before, without a request to its session.
#[test]
fn a_precheck_conflict_after_the_counted_resumes_asks_a_person() {
    let (_dir, repo, db) = fixture();
    let backend = TestWorkspace::new(&db, false, IDLE_AGENT);
    let run = start_run_under_dead_supervisor(&repo, &db, &backend, "dead-supervisor");
    let idle = run.idle_marker_path().unwrap();
    wait_until(&db, Duration::from_secs(20), |_| idle.is_file());
    let head = git_out(
        Path::new(run.worktree_path().unwrap()),
        &["rev-parse", "HEAD"],
    );
    Connection::open(&db)
        .unwrap()
        .execute(
            "UPDATE task_runs SET status='awaiting_integration', result_commit=?2 WHERE id=?1",
            rusqlite::params![run.id(), head],
        )
        .unwrap();
    // Main moves into the run's change.
    fs::write(repo.join("change.txt"), "main moved\n").unwrap();
    git(&repo, &["add", "change.txt"]);
    git(&repo, &["commit", "-q", "-m", "main moves"]);
    let mut queue = SqliteQueue::open(&db).unwrap();
    for attempt in 1..=3 {
        queue
            .record_runtime_event(
                run.id(),
                EventKind::ResumeStarted,
                json!({"attempt": attempt, "counted": true}),
            )
            .unwrap();
    }
    // The last resume went on in the session that lives (the one the stub
    // runs), whose receipt was validated.
    queue
        .record_runtime_event(
            run.id(),
            EventKind::ResumeFinished,
            json!({"attempt": 3, "status": "validating", "workspace_id": background_session(&run)}),
        )
        .unwrap();
    queue
        .record_runtime_event(
            run.id(),
            EventKind::ReviewFinished,
            json!({"attempt": 1, "verdict": "pass", "reasons": [], "summary": "meets the acceptance"}),
        )
        .unwrap();
    age_lease(&db, &run, 31);
    let reviewer = TestReviewer::new(&[verdict("pass", &[], "meets the acceptance")]);
    let outcome = supervise_reviewed(&db, &repo, &backend, &reviewer);
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    let detail = queue.show(TaskId::new(1)).unwrap();
    assert_eq!(detail.runs.len(), 1);
    assert_eq!(detail.runs[0].status(), RunStatus::AwaitingIntegration);
    let prechecks = payloads(&detail, "conflict_precheck");
    assert_eq!(prechecks.len(), 1, "{:?}", event_kinds(&detail));
    assert_eq!(prechecks[0]["requested"], false);
    assert!(prechecks[0].get("exhausted").is_none());
    let asked = prechecks[0]["asked"].as_str().unwrap();
    assert!(
        asked.ends_with("after 0 conflict requests and 3 counted resumes (at most 3)"),
        "{asked}"
    );
    assert!(session_texts(&detail.runs[0]).is_empty());
    assert_exit_sent(&backend, &detail.runs[0], 1);
    assert!(payloads(&detail, "triage_finished").is_empty());
    let asks = queue.asks(Default::default()).unwrap();
    assert_eq!(asks.len(), 1);
    assert_eq!(asks[0].kind, AskKind::ApproveLanding);
    assert_eq!(asks[0].run_id.as_ref(), Some(run.id()));
    assert!(asks[0].question.contains(asked), "{}", asks[0].question);
}
