//! Runtime tests: The sweep of workspaces, build outputs and worktrees.
use crate::runtime_support;
use dagq::domain::EventKind;
use dagq::infrastructure::git_binary::git_executable;

use runtime_support::*;

/// Supervisor options that sweep the workspaces of ended runs on every pass.
fn sweeping_options() -> SuperviseOptions {
    SuperviseOptions {
        sweep_interval: Duration::ZERO,
        ..supervise_options(4, true)
    }
}

/// The `workspace_closed` payloads of a run.
fn closes_of(queue: &SqliteQueue, run: &TaskRun) -> Vec<Value> {
    queue
        .run_events(run.id())
        .unwrap()
        .into_iter()
        .filter(|e| e.kind == "workspace_closed")
        .map(|e| e.payload)
        .collect()
}

/// Task 180: the supervisor's sweep closes the worker workspace of a failed
/// run the triage never takes: its task was canceled, or made ready and
/// run again, or a newer run of the in-progress task took its place. The
/// latest failed run of an in-progress task is the triage's and stays
/// open, a workspace cmux does not list gets no event, and worktrees and
/// branches stay.
#[test]
fn the_sweep_closes_the_workspaces_of_failed_runs_the_triage_does_not_take() {
    let (_dir, repo, db) = fixture();
    {
        let mut queue = SqliteQueue::open(&db).unwrap();
        add_ready_task(&mut queue, "second task", &[]);
        add_ready_task(&mut queue, "third task", &[]);
    }
    let backend = TestWorkspace::new(
        &db,
        false,
        "commit work; receipt \"$(git rev-parse HEAD)\"; exit 7",
    );
    supervise(&db, &repo, &backend).unwrap();
    backend.join();
    let mut queue = SqliteQueue::open(&db).unwrap();
    let first: Vec<TaskRun> = (1..=3)
        .map(|task| queue.show(TaskId::new(task)).unwrap().runs[0].clone())
        .collect();
    for run in &first {
        // The stub `claude` gives no verdict: each waits for a person.
        assert_eq!(run.status(), RunStatus::Failed);
        assert!(run.workspace_closed_at().is_none());
    }
    assert!(backend.closed().is_empty());
    let workspace = |run: &TaskRun| run.workspace_id().unwrap().to_owned();

    // A person cancels task 1 and runs task 2 again; task 3 waits. While
    // cmux does not list task 2's first workspace, nothing is recorded of it.
    queue
        .transition(TaskId::new(1), TaskAction::Cancel)
        .unwrap();
    queue.transition(TaskId::new(2), TaskAction::Ready).unwrap();
    backend.hidden.lock().unwrap().push(workspace(&first[1]));
    supervise_with(&db, &repo, &backend, &sweeping_options()).unwrap();
    backend.join();
    let closed = closes_of(&queue, &first[0]);
    assert_eq!(
        closed,
        [json!({"workspace_id": workspace(&first[0]), "by": "supervisor", "reason": "superseded"})]
    );
    assert!(
        queue
            .run(first[0].id())
            .unwrap()
            .workspace_closed_at()
            .is_some()
    );
    assert!(backend.closed().contains(&workspace(&first[0])));
    assert!(closes_of(&queue, &first[1]).is_empty());
    let task2 = queue.show(TaskId::new(2)).unwrap();
    assert_eq!(task2.runs.len(), 2);
    assert_eq!(task2.task.status(), TaskStatus::InProgress);

    // Listed again, the first run of task 2 is no longer its task's
    // latest: the next sweep closes it; the other runs are left alone.
    backend.hidden.lock().unwrap().clear();
    supervise_with(&db, &repo, &backend, &sweeping_options()).unwrap();
    backend.join();
    assert_eq!(
        closes_of(&queue, &first[1]),
        [json!({"workspace_id": workspace(&first[1]), "by": "supervisor", "reason": "superseded"})]
    );
    assert_eq!(closes_of(&queue, &first[0]).len(), 1);
    // The triage's runs: task 3's only run, task 2's latest.
    assert!(closes_of(&queue, &first[2]).is_empty());
    assert!(!backend.closed().contains(&workspace(&first[2])));
    let latest = queue.show(TaskId::new(2)).unwrap().runs[1].clone();
    assert_eq!(latest.status(), RunStatus::Failed);
    assert!(!backend.closed().contains(&workspace(&latest)));
    // The run of the task that was readied keeps its worktree and branch;
    // the canceled task's go (task 376).
    let run = &first[1];
    assert!(Path::new(run.worktree_path().unwrap()).is_dir());
    git(
        &repo,
        &["rev-parse", "--verify", "--quiet", run.branch().unwrap()],
    );
    assert!(!Path::new(first[0].worktree_path().unwrap()).exists());
}

/// Task 180: a landed run's workspaces are swept too, however it landed:
/// a resume workspace the run's close left open, and the worker workspace
/// of a run landed by hand without its close. A cmux failure records
/// `cleanup_failed` and the sweep goes on to the next; a workspace cmux
/// does not list gets no event.
#[test]
fn the_sweep_closes_every_workspace_left_open_by_a_landed_run() {
    let (_dir, repo, db) = fixture();
    let mut backend = TestWorkspace::new(&db, false, IDLE_AGENT);
    let reviewer = TestReviewer::new(&[verdict("pass", &[], "meets the acceptance")]);
    let outcome = supervise_reviewed(&db, &repo, &backend, &reviewer);
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    let mut queue = SqliteQueue::open(&db).unwrap();
    let run = queue.show(TaskId::new(1)).unwrap().runs[0].clone();
    assert_eq!(run.status(), RunStatus::Integrated);
    // A resume workspace left open, one cmux no longer lists, and the
    // worker workspace nobody closed, as when a person integrated the run.
    for (workspace, attempt) in [("resume-ws", 1), ("gone-ws", 2)] {
        queue
            .record_runtime_event(
                run.id(),
                EventKind::WorkspaceCreated,
                json!({"workspace_id": workspace, "resume_attempt": attempt}),
            )
            .unwrap();
    }
    backend.list("resume-ws");
    Connection::open(&db)
        .unwrap()
        .execute(
            "UPDATE task_runs SET workspace_id='hand-ws', workspace_closed_at=NULL WHERE id=?1",
            [run.id()],
        )
        .unwrap();
    backend.list("hand-ws");
    let before = closes_of(&queue, &run).len();

    backend.close_fail = true;
    supervise_with(&db, &repo, &backend, &sweeping_options()).unwrap();
    let failures = |run: &TaskRun| -> Vec<Value> {
        queue
            .run_events(run.id())
            .unwrap()
            .into_iter()
            .filter(|e| e.kind == "cleanup_failed")
            .map(|e| e.payload)
            .collect()
    };
    let failed = failures(&run);
    let workspaces: Vec<&Value> = failed.iter().map(|f| &f["workspace_id"]).collect();
    assert_eq!(
        workspaces,
        [&json!("hand-ws"), &json!("resume-ws")],
        "{failed:?}"
    );
    assert!(failed.iter().all(|f| f["by"] == "supervisor"));
    assert_eq!(closes_of(&queue, &run).len(), before);

    backend.close_fail = false;
    supervise_with(&db, &repo, &backend, &sweeping_options()).unwrap();
    let closed = closes_of(&queue, &run);
    assert_eq!(
        closed[before..],
        [
            json!({"workspace_id": "hand-ws", "by": "supervisor", "reason": "ended"}),
            json!({"workspace_id": "resume-ws", "by": "supervisor", "reason": "ended"}),
        ]
    );
    assert!(queue.run(run.id()).unwrap().workspace_closed_at().is_some());
    assert!(backend.closed().contains(&"resume-ws".to_owned()));
    assert!(backend.closed().contains(&"hand-ws".to_owned()));
    assert!(!closed.iter().any(|c| c["workspace_id"] == "gone-ws"));

    // Nothing is listed any more: another sweep records nothing.
    supervise_with(&db, &repo, &backend, &sweeping_options()).unwrap();
    assert_eq!(closes_of(&queue, &run).len(), closed.len());
    assert_eq!(failures(&run).len(), 2);
}

/// A worker script that commits, leaves build outputs in its worktree and
/// fails.
const BUILDING_AGENT: &str = "commit work; mkdir -p target/debug/deps llvm-cov-target; \
     head -c 65536 /dev/zero > target/debug/deps/big; ln target/debug/deps/big target/debug/big; \
     echo p > llvm-cov-target/profraw; receipt \"$(git rev-parse HEAD)\"; exit 7";

/// The payloads of a run's events of `kind`.
fn payloads_of(queue: &SqliteQueue, run: &TaskRun, kind: &str) -> Vec<Value> {
    queue
        .run_events(run.id())
        .unwrap()
        .into_iter()
        .filter(|e| e.kind == kind)
        .map(|e| e.payload)
        .collect()
}

/// Whether the branch exists in `repo`.
fn branch_exists(repo: &Path, branch: &str) -> bool {
    Command::new(git_executable().expect("git executable"))
        .arg("-C")
        .arg(repo)
        .args(["rev-parse", "--verify", "--quiet"])
        .arg(format!("refs/heads/{branch}"))
        .bounded_output()
        .unwrap()
        .status
        .success()
}

/// Task 376: a run that ends loses the build outputs of its worktree at
/// once (`target/`, `llvm-cov-target/`), recorded with the bytes they took;
/// its sources and commit stay. Once its task is canceled the worktree and
/// branch go; a task made ready again keeps its runs' worktrees; the sweep
/// removes build outputs left behind later; a tracked `target/` stays; the
/// checkout the supervisor was given is never touched.
#[test]
fn ended_runs_lose_their_build_outputs_and_canceled_tasks_their_worktrees() {
    let (_dir, repo, db) = fixture();
    {
        let mut queue = SqliteQueue::open(&db).unwrap();
        add_ready_task(&mut queue, "second task", &[]);
        add_ready_task(&mut queue, "third task", &[]);
    }
    // The main checkout's own build outputs are not the runtime's.
    fs::create_dir_all(repo.join("target/debug")).unwrap();
    fs::write(repo.join("target/debug/mine"), "keep").unwrap();
    let backend = TestWorkspace::new(&db, false, BUILDING_AGENT);
    supervise(&db, &repo, &backend).unwrap();
    backend.join();
    let mut queue = SqliteQueue::open(&db).unwrap();
    let first: Vec<TaskRun> = (1..=3)
        .map(|task| queue.show(TaskId::new(task)).unwrap().runs[0].clone())
        .collect();
    for run in &first {
        assert_eq!(run.status(), RunStatus::Failed);
        let worktree = Path::new(run.worktree_path().unwrap());
        assert!(!worktree.join("target").exists(), "{}", worktree.display());
        assert!(!worktree.join("llvm-cov-target").exists());
        assert!(worktree.join("change.txt").is_file());
        assert!(Path::new(run.run_dir().unwrap()).is_dir());
        let removed = payloads_of(&queue, run, "build_outputs_removed");
        assert_eq!(removed.len(), 1, "{removed:?}");
        assert_eq!(
            removed[0]["paths"],
            json!([
                worktree.join("target").to_string_lossy(),
                worktree.join("llvm-cov-target").to_string_lossy()
            ])
        );
        assert_eq!(removed[0]["by"], "supervisor");
        // The hard link is counted once.
        let bytes = removed[0]["bytes"].as_u64().unwrap();
        assert!((65536..2 * 65536).contains(&bytes), "{bytes}");
    }
    assert!(repo.join("target/debug/mine").is_file());

    // Task 3 tracks a file under `target/`; build outputs appear again in
    // task 2's worktree; a person cancels task 1 and readies task 2.
    let tracked = Path::new(first[2].worktree_path().unwrap());
    fs::create_dir_all(tracked.join("target")).unwrap();
    fs::write(tracked.join("target/tracked.txt"), "source").unwrap();
    git(tracked, &["add", "target/tracked.txt"]);
    git(tracked, &["commit", "-q", "-m", "track target"]);
    let left = Path::new(first[1].worktree_path().unwrap()).join("target/debug");
    fs::create_dir_all(&left).unwrap();
    fs::write(left.join("left"), "x").unwrap();
    queue
        .transition(TaskId::new(1), TaskAction::Cancel)
        .unwrap();
    queue.transition(TaskId::new(2), TaskAction::Ready).unwrap();
    supervise_with(&db, &repo, &backend, &sweeping_options()).unwrap();
    backend.join();

    let canceled = &first[0];
    assert!(!Path::new(canceled.worktree_path().unwrap()).exists());
    assert!(!branch_exists(&repo, canceled.branch().unwrap()));
    let removed = payloads_of(&queue, canceled, "worktree_removed");
    assert_eq!(removed.len(), 1, "{removed:?}");
    assert_eq!(removed[0]["path"], canceled.worktree_path().unwrap());
    assert_eq!(removed[0]["branch"], canceled.branch().unwrap());
    assert_eq!(removed[0]["reason"], "task_canceled");
    assert_eq!(removed[0]["by"], "supervisor");
    assert!(removed[0]["bytes"].as_u64().unwrap() > 0);
    assert!(Path::new(canceled.run_dir().unwrap()).is_dir());

    // Task 2 runs again: its first run keeps its worktree and branch but
    // loses what was built there since.
    let retried = &first[1];
    let worktree = Path::new(retried.worktree_path().unwrap());
    assert!(worktree.join("change.txt").is_file());
    assert!(!worktree.join("target").exists());
    assert!(branch_exists(&repo, retried.branch().unwrap()));
    assert_eq!(
        payloads_of(&queue, retried, "build_outputs_removed").len(),
        2
    );
    assert_eq!(queue.show(TaskId::new(2)).unwrap().runs.len(), 2);

    assert!(tracked.join("target/tracked.txt").is_file());
    assert_eq!(
        payloads_of(&queue, &first[2], "build_outputs_removed").len(),
        1
    );
    assert!(repo.join("target/debug/mine").is_file());

    // Nothing is left: another sweep records nothing.
    supervise_with(&db, &repo, &backend, &sweeping_options()).unwrap();
    backend.join();
    assert_eq!(payloads_of(&queue, canceled, "worktree_removed").len(), 1);
    assert_eq!(
        payloads_of(&queue, retried, "build_outputs_removed").len(),
        2
    );
    assert!(payloads_of(&queue, canceled, "cleanup_failed").is_empty());
}

/// Task 376: once a task completes, the worktree and branch of its
/// earlier failed run go too, and a worktree the landing could not remove
/// is removed by the sweep; the landed run keeps its run directory.
#[test]
fn a_completed_task_loses_the_worktrees_of_all_its_runs() {
    let (_dir, repo, db) = fixture();
    let backend = TestWorkspace::new(&db, false, BUILDING_AGENT);
    supervise(&db, &repo, &backend).unwrap();
    backend.join();
    let mut queue = SqliteQueue::open(&db).unwrap();
    let failed = queue.show(TaskId::new(1)).unwrap().runs[0].clone();
    assert_eq!(failed.status(), RunStatus::Failed);
    assert!(Path::new(failed.worktree_path().unwrap()).is_dir());

    queue.transition(TaskId::new(1), TaskAction::Ready).unwrap();
    backend.script_for(1, IDLE_AGENT);
    let reviewer = TestReviewer::new(&[verdict("pass", &[], "meets the acceptance")]);
    let outcome = supervise_reviewed(&db, &repo, &backend, &reviewer);
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    let detail = queue.show(TaskId::new(1)).unwrap();
    assert_eq!(detail.task.status(), TaskStatus::Completed);
    let landed = detail.runs[1].clone();
    assert_eq!(landed.status(), RunStatus::Integrated);

    assert!(!Path::new(failed.worktree_path().unwrap()).exists());
    assert!(!branch_exists(&repo, failed.branch().unwrap()));
    let removed = payloads_of(&queue, &failed, "worktree_removed");
    assert_eq!(removed.len(), 1, "{removed:?}");
    assert_eq!(removed[0]["reason"], "task_completed");
    assert!(Path::new(failed.run_dir().unwrap()).is_dir());
    assert!(!Path::new(landed.worktree_path().unwrap()).exists());

    // A landed run whose worktree is still there (its removal failed, or a
    // person put it back) is removed by the sweep; its branch was already
    // gone.
    git(
        &repo,
        &[
            "worktree",
            "add",
            "--detach",
            landed.worktree_path().unwrap(),
            "main",
        ],
    );
    fs::create_dir_all(Path::new(landed.worktree_path().unwrap()).join("target")).unwrap();
    supervise_with(&db, &repo, &backend, &sweeping_options()).unwrap();
    backend.join();
    assert!(!Path::new(landed.worktree_path().unwrap()).exists());
    let removed = payloads_of(&queue, &landed, "worktree_removed");
    assert_eq!(
        removed.last().unwrap()["reason"],
        "task_completed",
        "{removed:?}"
    );
    assert!(payloads_of(&queue, &landed, "cleanup_failed").is_empty());
    assert!(Path::new(landed.run_dir().unwrap()).is_dir());
}

/// Task 376: build outputs that cannot be removed record `cleanup_failed`
/// once per process and are removed by a later sweep.
#[test]
fn build_outputs_that_cannot_be_removed_are_retried_by_the_sweep() {
    use std::os::unix::fs::PermissionsExt;
    let (_dir, repo, db) = fixture();
    let backend = TestWorkspace::new(&db, false, BUILDING_AGENT);
    supervise(&db, &repo, &backend).unwrap();
    backend.join();
    let mut queue = SqliteQueue::open(&db).unwrap();
    let run = queue.show(TaskId::new(1)).unwrap().runs[0].clone();
    let locked = Path::new(run.worktree_path().unwrap()).join("target/locked");
    fs::create_dir_all(&locked).unwrap();
    fs::write(locked.join("file"), "x").unwrap();
    fs::set_permissions(&locked, fs::Permissions::from_mode(0o555)).unwrap();

    supervise_with(&db, &repo, &backend, &sweeping_options()).unwrap();
    let failed = payloads_of(&queue, &run, "cleanup_failed");
    assert_eq!(failed.len(), 1, "{failed:?}");
    assert_eq!(failed[0]["path"], run.worktree_path().unwrap());
    assert_eq!(failed[0]["by"], "supervisor");
    assert!(locked.join("file").is_file());
    assert_eq!(payloads_of(&queue, &run, "build_outputs_removed").len(), 1);

    fs::set_permissions(&locked, fs::Permissions::from_mode(0o755)).unwrap();
    supervise_with(&db, &repo, &backend, &sweeping_options()).unwrap();
    assert!(!locked.exists());
    assert_eq!(payloads_of(&queue, &run, "build_outputs_removed").len(), 2);
}

/// Task 396: an ended run whose supervisor died before releasing its
/// lease is swept as if nobody leased it: its workspace closes and its
/// worktree is a cleanup candidate. A lease a live supervisor holds still
/// keeps the sweep away, from its Claude Code scratchpad (task 1100) and
/// its run's `tmp` (task 1290) too, which go once the lease is stale as
/// its task is completed.
#[test]
fn an_ended_run_with_a_stale_lease_is_swept_but_not_one_with_a_live_lease() {
    let (_dir, repo, db) = fixture();
    let backend = TestWorkspace::new(&db, false, IDLE_AGENT);
    let reviewer = TestReviewer::new(&[verdict("pass", &[], "meets the acceptance")]);
    let outcome = supervise_reviewed(&db, &repo, &backend, &reviewer);
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    let mut queue = SqliteQueue::open(&db).unwrap();
    let run = queue.show(TaskId::new(1)).unwrap().runs[0].clone();
    assert_eq!(run.status(), RunStatus::Integrated);
    let raw = Connection::open(&db).unwrap();
    raw.execute(
        "UPDATE task_runs SET workspace_id='left-ws', workspace_closed_at=NULL WHERE id=?1",
        [run.id()],
    )
    .unwrap();
    backend.list("left-ws");
    // A live supervisor's lease: its process is alive and its heartbeat
    // is not old.
    raw.execute(
        "INSERT INTO run_leases(run_id,token,pid,heartbeat_at) VALUES (?1,'live',?2,unixepoch()+100000)",
        rusqlite::params![run.id(), std::process::id()],
    )
    .unwrap();
    // Task 1100: the Claude Code scratchpad of its session is not removed
    // while the lease is live.
    let root = _dir.path().join("claude-tmp");
    let scratchpad = scratchpad_of(&root, run.worktree_path().unwrap());
    fs::create_dir_all(scratchpad.join("session/scratchpad")).unwrap();
    fs::write(scratchpad.join("session/scratchpad/notes"), "x").unwrap();
    // Task 1290: nor the temporary files directory its Codex turns had as
    // their `TMPDIR`, in its run directory.
    let run_dir = PathBuf::from(run.run_dir().unwrap());
    let tmp = run_dir.join("tmp");
    fs::create_dir_all(tmp.join("target/debug")).unwrap();
    fs::write(tmp.join("target/debug/big"), vec![0u8; 16384]).unwrap();
    fs::write(run_dir.join("kept.log"), "log").unwrap();
    let sweeping = SuperviseOptions {
        scratchpad_roots: Some(vec![root.clone()]),
        ..sweeping_options()
    };
    let before = closes_of(&queue, &run).len();
    let candidate = |queue: &SqliteQueue| {
        queue
            .ended_run_worktrees()
            .unwrap()
            .iter()
            .any(|w| w.run_id == *run.id())
    };
    assert!(!candidate(&queue));
    assert!(
        !queue
            .ended_run_workspaces()
            .unwrap()
            .iter()
            .any(|w| w.run_id == *run.id())
    );
    supervise_with(&db, &repo, &backend, &sweeping).unwrap();
    assert_eq!(closes_of(&queue, &run).len(), before);
    assert!(!backend.closed().contains(&"left-ws".to_owned()));
    assert!(scratchpad.join("session/scratchpad/notes").is_file());
    assert!(payloads_of(&queue, &run, "scratchpad_removed").is_empty());
    assert!(tmp.join("target/debug/big").is_file());
    assert!(payloads_of(&queue, &run, "run_tmp_removed").is_empty());

    // Its holder dies before releasing it: the lease is stale.
    raw.execute("UPDATE run_leases SET pid=?1", [dead_pid()])
        .unwrap();
    assert!(candidate(&queue));
    supervise_with(&db, &repo, &backend, &sweeping).unwrap();
    assert!(!scratchpad.exists());
    let removed = payloads_of(&queue, &run, "scratchpad_removed");
    assert_eq!(removed.len(), 1, "{removed:?}");
    assert_eq!(removed[0]["reason"], "task_completed");
    // The run's `tmp` goes with the completed task, the rest of its run
    // directory stays.
    assert!(!tmp.exists());
    let removed = payloads_of(&queue, &run, "run_tmp_removed");
    assert_eq!(removed.len(), 1, "{removed:?}");
    assert_eq!(removed[0]["paths"], json!([tmp.to_string_lossy()]));
    assert!(
        removed[0]["bytes"].as_u64().unwrap() >= 16384,
        "{removed:?}"
    );
    assert_eq!(removed[0]["by"], "supervisor");
    assert_eq!(removed[0]["reason"], "task_completed");
    assert!(run_dir.join("receipt.json").is_file());
    assert!(run_dir.join("kept.log").is_file());
    let closed = closes_of(&queue, &run);
    assert_eq!(
        closed[before..],
        [json!({"workspace_id": "left-ws", "by": "supervisor", "reason": "ended"})]
    );
    assert!(backend.closed().contains(&"left-ws".to_owned()));

    // A heartbeat older than the limit is stale too.
    raw.execute(
        "UPDATE run_leases SET pid=?1, heartbeat_at=0",
        [std::process::id()],
    )
    .unwrap();
    assert!(candidate(&queue));
}

/// ADR-0048 decision 7: the supervisor closes, as inferred, the inbox and
/// planner spans whose workspace cmux no longer lists (their `SessionEnd`
/// never came); a span whose workspace is listed stays open, and nothing is
/// closed while cmux cannot list its workspaces.
#[test]
fn the_supervisor_closes_the_session_spans_of_gone_inbox_and_planner_workspaces() {
    use dagq::{
        application::SessionRegistry,
        domain::sessions::{HookEvent, INBOX, PLANNER, SessionHook},
    };
    let (_dir, repo, db) = fixture();
    let queue = {
        let mut queue = SqliteQueue::open(&db).unwrap();
        queue
            .transition(TaskId::new(1), TaskAction::Cancel)
            .unwrap();
        queue
    };
    for (kind, session, workspace) in [
        (INBOX, "s-inbox", "W-LISTED"),
        (PLANNER, "s-plan", "W-GONE"),
    ] {
        queue
            .record_session_hook(&SessionHook {
                event: HookEvent::Start {
                    source: "startup".into(),
                },
                kind,
                session_id: session.into(),
                transcript_path: None,
                cwd: None,
                workspace_id: Some(workspace.into()),
                planner_id: None,
                launch: None,
            })
            .unwrap();
    }
    let open = || -> Vec<String> {
        queue
            .hook_session_workspaces()
            .unwrap()
            .into_iter()
            .map(|(_, workspace)| workspace)
            .collect()
    };

    let mut failing = TestWorkspace::new(&db, false, "exit 0");
    failing.exists_fails = true;
    supervise(&db, &repo, &failing).unwrap();
    assert_eq!(open(), ["W-LISTED", "W-GONE"]);

    let backend = TestWorkspace::new(&db, false, "exit 0");
    backend.listed.lock().unwrap().push("w-listed".into());
    supervise(&db, &repo, &backend).unwrap();
    assert_eq!(open(), ["W-LISTED"]);
    let closed: Vec<Value> = queue
        .latest_events_of("session_closed", 10)
        .unwrap()
        .into_iter()
        .map(|event| event.payload)
        .collect();
    assert_eq!(closed.len(), 1);
    assert_eq!(closed[0]["kind"], "planner");
    assert_eq!(closed[0]["session_id"], "s-plan");
    assert_eq!(closed[0]["reason"], "inferred");
}

/// Goal 54 (1): the supervisor's sweep closes the record of a planner,
/// a person's or the runtime's, whose workspace cmux does not list and
/// whose wrapper is dead, so `planners` stops showing it; a planner whose
/// wrapper is alive stays, and nothing is closed while cmux cannot list its
/// workspaces.
#[test]
fn the_sweep_closes_the_records_of_planners_whose_workspace_and_wrapper_are_gone() {
    use dagq::domain::{PlannerId, PlannerOrigin};
    let (_dir, repo, db) = fixture();
    let mut queue = SqliteQueue::open(&db).unwrap();
    queue
        .transition(TaskId::new(1), TaskAction::Cancel)
        .unwrap();
    let dead_pid = {
        let mut child = std::process::Command::new("true").spawn().unwrap();
        let pid = child.id();
        child.wait().unwrap();
        pid
    };
    let record = |origin, workspace: &str, pid| {
        let planner = queue.open_planner(origin, None).unwrap();
        queue
            .planner_workspace_created(planner.id, workspace)
            .unwrap();
        queue.register_planner_wrapper(planner.id, pid).unwrap();
        planner.id
    };
    let person = record(PlannerOrigin::Person, "W-PERSON", dead_pid);
    let runtime = record(PlannerOrigin::Runtime, "W-RUNTIME", dead_pid);
    let alive = record(PlannerOrigin::Person, "W-ALIVE", std::process::id());
    let listed = record(PlannerOrigin::Person, "W-LISTED", dead_pid);
    let open = || -> Vec<PlannerId> {
        queue
            .planners(false)
            .unwrap()
            .into_iter()
            .map(|planner| planner.id)
            .collect()
    };

    let mut failing = TestWorkspace::new(&db, false, "exit 0");
    failing.exists_fails = true;
    supervise_with(&db, &repo, &failing, &sweeping_options()).unwrap();
    assert_eq!(open(), [person, runtime, alive, listed]);

    let backend = TestWorkspace::new(&db, false, "exit 0");
    backend.listed.lock().unwrap().push("w-listed".into());
    supervise_with(&db, &repo, &backend, &sweeping_options()).unwrap();
    assert_eq!(open(), [alive, listed]);
    for id in [person, runtime] {
        assert!(queue.planner(id).unwrap().closed_at.is_some());
    }
    // Each close is recorded once, with why (ADR-t1300-1).
    let mut closes: Vec<(i64, String, String, bool)> = queue
        .latest_events_of("planner_closed", 10)
        .unwrap()
        .into_iter()
        .map(|event| {
            (
                event.payload["planner_id"].as_i64().unwrap(),
                event.payload["origin"].as_str().unwrap().to_owned(),
                event.payload["code"].as_str().unwrap().to_owned(),
                event.payload["workspace_closed"].as_bool().unwrap(),
            )
        })
        .collect();
    closes.sort();
    assert_eq!(
        closes,
        [
            (person.as_i64(), "person".into(), "abandoned".into(), false),
            (
                runtime.as_i64(),
                "runtime".into(),
                "abandoned".into(),
                false
            ),
        ]
    );
}

/// Task 696: the supervisor's sweep removes the runner of every planner
/// whose wrapper is done (its exit recorded, or its process dead), whether
/// its row is closed or still open, and keeps the runner of a live wrapper,
/// a closed row's included, and the rest of each directory.
#[test]
fn the_sweep_removes_the_runners_of_planners_whose_wrapper_is_done() {
    use dagq::{domain::PlannerOrigin, infrastructure::location::planners_dir};
    let (_dir, repo, db) = fixture();
    let mut queue = SqliteQueue::open(&db).unwrap();
    queue
        .transition(TaskId::new(1), TaskAction::Cancel)
        .unwrap();
    let dead_pid = {
        let mut child = std::process::Command::new("true").spawn().unwrap();
        let pid = child.id();
        child.wait().unwrap();
        pid
    };
    let alive_pid = std::process::id();
    let record = |workspace: &str, pid| {
        let planner = queue.open_planner(PlannerOrigin::Person, None).unwrap();
        queue
            .planner_workspace_created(planner.id, workspace)
            .unwrap();
        queue.register_planner_wrapper(planner.id, pid).unwrap();
        let dir = planners_dir(&db).join(planner.id.to_string());
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("runner"), "binary").unwrap();
        fs::write(dir.join("prompt.txt"), "prompt").unwrap();
        (planner.id, dir)
    };
    let (exited, exited_dir) = record("W-EXITED", alive_pid);
    queue.planner_exited(exited, alive_pid, 0).unwrap();
    // Listed, so its row stays open.
    let (_, dead_dir) = record("W-DEAD", dead_pid);
    let (closed, closed_dir) = record("W-CLOSED", alive_pid);
    queue.close_planner(closed, None).unwrap();
    let (_, alive_dir) = record("W-ALIVE", alive_pid);

    let backend = TestWorkspace::new(&db, false, "exit 0");
    backend
        .listed
        .lock()
        .unwrap()
        .extend(["w-dead".into(), "w-alive".into()]);
    supervise_with(&db, &repo, &backend, &sweeping_options()).unwrap();
    for dir in [&exited_dir, &dead_dir] {
        assert!(!dir.join("runner").exists(), "{}", dir.display());
    }
    for dir in [&closed_dir, &alive_dir] {
        assert!(dir.join("runner").is_file(), "{}", dir.display());
    }
    for dir in [&exited_dir, &dead_dir, &closed_dir, &alive_dir] {
        assert!(dir.join("prompt.txt").is_file());
    }
}

/// Task 1100: once a run's task is over, the sweep removes the Claude Code
/// scratchpad of its session (named after its worktree) under each root,
/// recorded as `scratchpad_removed` with the paths and the bytes. A run
/// whose task goes on keeps its scratchpad; a link in its place is not
/// followed, a root without it and other directories are left alone, and
/// another sweep records nothing.
#[test]
fn a_task_over_loses_the_claude_scratchpads_of_its_runs() {
    let (dir, repo, db) = fixture();
    {
        let mut queue = SqliteQueue::open(&db).unwrap();
        add_ready_task(&mut queue, "second task", &[]);
    }
    let backend = TestWorkspace::new(&db, false, BUILDING_AGENT);
    supervise(&db, &repo, &backend).unwrap();
    backend.join();
    let mut queue = SqliteQueue::open(&db).unwrap();
    let canceled = queue.show(TaskId::new(1)).unwrap().runs[0].clone();
    let going_on = queue.show(TaskId::new(2)).unwrap().runs[0].clone();
    assert_eq!(canceled.status(), RunStatus::Failed);

    let (first, second, empty) = (
        dir.path().join("tmp-a"),
        dir.path().join("tmp-b"),
        dir.path().join("tmp-c"),
    );
    let fill = |dir: &Path| {
        fs::create_dir_all(dir.join("session/scratchpad/dagq-worker/target")).unwrap();
        fs::write(
            dir.join("session/scratchpad/dagq-worker/target/built"),
            vec![0u8; 8192],
        )
        .unwrap();
    };
    let removed_a = scratchpad_of(&first, canceled.worktree_path().unwrap());
    let removed_b = scratchpad_of(&second, canceled.worktree_path().unwrap());
    let kept = scratchpad_of(&first, going_on.worktree_path().unwrap());
    fill(&removed_a);
    fill(&removed_b);
    fill(&kept);
    let other = first.join("-Users-someone-elsewhere");
    fill(&other);
    // In the third root, a link where the scratchpad would be.
    let outside = dir.path().join("outside");
    fill(&outside);
    fs::create_dir_all(&empty).unwrap();
    let link = scratchpad_of(&empty, canceled.worktree_path().unwrap());
    std::os::unix::fs::symlink(&outside, &link).unwrap();

    queue
        .transition(TaskId::new(1), TaskAction::Cancel)
        .unwrap();
    let options = SuperviseOptions {
        scratchpad_roots: Some(vec![first.clone(), second.clone(), empty.clone()]),
        ..sweeping_options()
    };
    supervise_with(&db, &repo, &backend, &options).unwrap();
    backend.join();

    assert!(!removed_a.exists());
    assert!(!removed_b.exists());
    assert!(first.is_dir() && second.is_dir());
    assert!(
        kept.join("session/scratchpad/dagq-worker/target/built")
            .is_file()
    );
    assert!(
        other
            .join("session/scratchpad/dagq-worker/target/built")
            .is_file()
    );
    assert!(
        fs::symlink_metadata(&link)
            .unwrap()
            .file_type()
            .is_symlink()
    );
    assert!(
        outside
            .join("session/scratchpad/dagq-worker/target/built")
            .is_file()
    );
    let removed = payloads_of(&queue, &canceled, "scratchpad_removed");
    assert_eq!(removed.len(), 1, "{removed:?}");
    assert_eq!(
        removed[0]["paths"],
        json!([removed_a.to_string_lossy(), removed_b.to_string_lossy()])
    );
    assert_eq!(removed[0]["reason"], "task_canceled");
    assert_eq!(removed[0]["by"], "supervisor");
    let bytes = removed[0]["bytes"].as_u64().unwrap();
    assert!(bytes >= 2 * 8192, "{bytes}");
    assert_eq!(payloads_of(&queue, &canceled, "worktree_removed").len(), 1);
    assert!(payloads_of(&queue, &going_on, "scratchpad_removed").is_empty());

    // Nothing is left: another sweep records nothing and fails nothing.
    supervise_with(&db, &repo, &backend, &options).unwrap();
    backend.join();
    assert_eq!(
        payloads_of(&queue, &canceled, "scratchpad_removed").len(),
        1
    );
    assert!(payloads_of(&queue, &canceled, "cleanup_failed").is_empty());
    assert!(payloads_of(&queue, &going_on, "cleanup_failed").is_empty());
}
