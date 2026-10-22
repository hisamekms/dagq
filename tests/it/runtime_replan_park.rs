//! Runtime tests: the park of a headless run whose task is being replanned
//! (ADR-t1521-2 decisions 1 and 2). The test stands in for the diagnosis
//! that moves an operation past the pause's fence (`pause_requested`) and
//! for the outcome that ends it in a continuation or a withdrawal, as the
//! trusted runtime would; the supervisor does the rest: no next turn, the
//! wrapper ending between turns, the snapshot of the run's work, the park
//! that gives the slot back and, once the operation ends, the restore and
//! the resume.
use crate::common;
use crate::runtime_support;
use dagq::domain::EventKind;

use dagq::domain::replan_snapshot::SnapshotManifest;
use dagq::domain::task_replan::{
    NewReplan, ReplanMove, ReplanOperation, ReplanOrigin, ReplanStatus,
};
use dagq::domain::{ReplanId, worker::WorkerMode};
use dagq::infrastructure::location::replans_dir;
use runtime_support::containers::ContainerQueue;
use runtime_support::headless::*;
use runtime_support::*;

/// A turn of task 2 that waits for the test's `go`, then leaves work of
/// every kind in its worktree: a commit, a staged file, an unstaged change,
/// an untracked file and an untracked executable script, and a receipt for
/// its head; `extra` runs before the receipt. Task 3's turns finish at once
/// on a file of their own, and a resumed session of task 2 checks that its
/// work came back, the script's execute bit included, commits all of it
/// and finishes.
fn turns(dir: &Path, extra: &str) {
    set_turns(
        dir,
        &format!(
            r#"case "$PROMPT" in
*'dagq task 2,'*) await_file "$RUN_DIR/go"; commit "first part"; printf 'half\n' >> seed.txt; printf 'staged\n' > staged.txt; git add staged.txt; printf 'notes\n' > notes.txt; printf '#!/bin/sh\necho ran\n' > run.sh; chmod +x run.sh; {extra} receipt "$(git rev-parse HEAD)"; say paused ;;
*'paused at the end of a turn'*) test -f notes.txt && test -f staged.txt && test -x run.sh && grep -q half seed.txt || fail "the saved work is not here"; git add -A && {{ git diff --cached --quiet || git commit -qm "the rest"; }} && receipt "$(git rev-parse HEAD)"; say finished ;;
*) printf 'other\n' > other.txt && git add other.txt && git commit -qm other && receipt "$(git rev-parse HEAD)"; say finished ;;
esac"#
        ),
    );
}

/// Add a ready task for a headless Claude worker, titled `title`.
fn add_headless_task(db: &Path, title: &str) -> TaskId {
    let mut queue = SqliteQueue::open(db).unwrap();
    let task = queue
        .add(NewTask {
            title: title.into(),
            description: "small change".into(),
            acceptance: "works".into(),
            verification_commands: vec!["test -f seed.txt".into()],
            required_evidence: Vec::new(),
            paths: Vec::new(),
            priority: Default::default(),
            change: None,
            dependencies: Vec::new(),
            goal_dependencies: Vec::new(),
            goal_id: None,
            context: String::new(),
            provider: Some(dagq::domain::Provider::Claude),
            worker_mode: Some(WorkerMode::Headless),
            wait_for_build: false,
            execution_class: Default::default(),
        })
        .unwrap();
    queue
        .transition(task.id(), TaskAction::BypassReview)
        .unwrap();
    task.id()
}

/// Open an operation on `run` and move it past the pause's fence, as the
/// diagnosis that chose a replan would.
fn pause(db: &Path, run: &RunId) -> ReplanOperation {
    let mut queue = SqliteQueue::open(db).unwrap();
    let op = queue
        .record_replan(
            &NewReplan {
                run: run.clone(),
                reason: "two independent parts".into(),
                evidence: Vec::new(),
            },
            ReplanOrigin::Runtime,
            "supervisor",
            "supervisor",
        )
        .unwrap()
        .operation;
    advance(db, &op, ReplanStatus::PauseRequested)
}

/// Move `op` (as read last) to `to`, as the trusted runtime would.
fn advance(db: &Path, op: &ReplanOperation, to: ReplanStatus) -> ReplanOperation {
    SqliteQueue::open(db)
        .unwrap()
        .move_replan(
            op.id,
            op.generation,
            &ReplanMove {
                to,
                reason: format!("the test moves it to {}", to.as_str()),
                snapshot_id: None,
                diagnosis: None,
            },
        )
        .unwrap()
}

fn operation(db: &Path, id: ReplanId) -> ReplanOperation {
    SqliteQueue::open(db).unwrap().replan_operation(id).unwrap()
}

fn wait_for_status(db: &Path, id: ReplanId, status: ReplanStatus) {
    wait_until(db, common::STEP_LIMIT, |queue| {
        queue.replan_operation(id).unwrap().status == status
    });
}

/// The manifest of the snapshot `op` names.
fn manifest(db: &Path, op: &ReplanOperation) -> SnapshotManifest {
    let snapshot = op.snapshot_id.as_deref().expect("a snapshot");
    let path = replans_dir(db).join(snapshot).join("manifest.json");
    serde_json::from_slice(&fs::read(path).unwrap()).unwrap()
}

/// The file of the snapshot `op` names whose kind is `kind` (and, for an
/// untracked one, whose source is `source`).
fn saved(db: &Path, op: &ReplanOperation, kind: &str, source: Option<&str>) -> String {
    let manifest = manifest(db, op);
    let file = manifest
        .files
        .iter()
        .find(|file| file.kind.as_str() == kind && source.is_none_or(|s| file.source == s))
        .unwrap_or_else(|| panic!("no {kind} {source:?} in {:?}", manifest.files));
    let path = replans_dir(db)
        .join(op.snapshot_id.as_deref().unwrap())
        .join(&file.name);
    fs::read_to_string(path).unwrap()
}

/// Task 2's run, its turn started, parked by an operation the test opened
/// while the turn ran; the turn then left its work and its receipt. The
/// supervisor runs with one slot, and task 3, added while task 2 holds the
/// slot, lands once the park gives it back.
fn park_during_a_turn(
    extra: &str,
) -> (
    Fixture,
    PathBuf,
    PathBuf,
    Arc<TestWorkspace>,
    ReplanOperation,
) {
    let (dir, repo, db, backend) = headless_fixture(&[]);
    turns(dir.path(), extra);
    let backend = Arc::new(backend);
    let (_reviewer, supervisor) =
        supervise_thread_in(&db, &repo, backend.clone(), Default::default(), &[], 1);
    wait_until(&db, common::STEP_LIMIT, |queue| {
        !payloads(&queue.show(TASK).unwrap(), "turn_started").is_empty()
    });
    let other = add_headless_task(&db, "other task");
    let run = detail(&db).runs[0].clone();
    let op = pause(&db, run.id());
    wait_for_status(&db, op.id, ReplanStatus::Quiescing);
    fs::write(Path::new(run.run_dir().unwrap()).join("go"), "").unwrap();
    finished(&db, &backend, supervisor);
    let landed = SqliteQueue::open(&db).unwrap().show(other).unwrap();
    assert_eq!(
        landed.task.status(),
        TaskStatus::Completed,
        "the slot was given back"
    );
    let op = operation(&db, op.id);
    (dir, repo, db, backend, op)
}

/// Acceptance: an operation opened while a turn runs lets the turn end;
/// no next turn starts and the receipt it wrote is not acted on; the
/// wrapper ends between turns; the run's work (the commit and the staged,
/// unstaged and untracked changes) and its receipt are saved in a snapshot
/// with the head pinned, the worktree is left as it was, and the run is
/// parked with its slot given back, which another task takes.
#[test]
fn a_request_during_a_turn_parks_the_run_at_its_end_with_its_dirty_work_saved() {
    let (_dir, repo, db, _backend, op) = park_during_a_turn("");
    assert_eq!(op.status, ReplanStatus::Parked, "{op:?}");
    assert_eq!(op.generation, 2);
    let snapshot = op.snapshot_id.clone().unwrap();
    assert_eq!(snapshot, format!("replan-{}-g2", op.id));
    let detail = detail(&db);
    let run = &detail.runs[0];
    assert_eq!(run.status(), RunStatus::NeedsSession);
    assert!(
        run.last_error()
            .unwrap()
            .contains("parked for replan operation"),
        "{:?}",
        run.last_error()
    );
    let kinds = event_kinds(&detail);
    for kind in ["turn_withheld", "replan_parked", "lease_released"] {
        assert!(kinds.contains(&kind), "{kind} in {kinds:?}");
    }
    for kind in [
        "receipt_observed",
        "review_started",
        "revise_requested",
        "integration_started",
    ] {
        assert!(!kinds.contains(&kind), "{kind} in {kinds:?}");
    }
    assert_eq!(payloads(&detail, "turn_started").len(), 1);
    let worktree = Path::new(run.worktree_path().unwrap());
    let manifest = manifest(&db, &op);
    assert!(manifest.dirty);
    assert_eq!(manifest.head, git_out(worktree, &["rev-parse", "HEAD"]));
    assert_eq!(
        git_out(
            &repo,
            &["rev-parse", &format!("refs/dagq/replans/{}", op.id)]
        ),
        manifest.head
    );
    assert!(saved(&db, &op, "index", None).contains("staged"));
    assert!(saved(&db, &op, "worktree", None).contains("half"));
    assert_eq!(saved(&db, &op, "untracked", Some("notes.txt")), "notes\n");
    let script = manifest
        .files
        .iter()
        .find(|file| file.source == "run.sh")
        .unwrap();
    assert!(script.executable, "{script:?}");
    assert!(
        !manifest
            .files
            .iter()
            .find(|file| file.source == "notes.txt")
            .unwrap()
            .executable
    );
    assert!(saved(&db, &op, "receipt", None).contains(&manifest.head));
    assert!(saved(&db, &op, "log", Some("the run's events")).contains("turn_withheld"));
    // The session's turns: the output of the turn the park let end, and the
    // request its first turn left none of.
    let turns: Vec<&dagq::domain::replan_snapshot::SavedFile> = manifest
        .files
        .iter()
        .filter(|file| file.name.starts_with("logs/turns/"))
        .collect();
    assert!(
        turns
            .iter()
            .any(|file| file.name == "logs/turns/turn-000001.jsonl"),
        "{turns:?}"
    );
    let output = fs::read_to_string(
        replans_dir(&db)
            .join(&snapshot)
            .join("logs/turns/turn-000001.jsonl"),
    )
    .unwrap();
    assert!(output.contains("paused"), "{output}");
    // The worktree keeps its work.
    let status = git_out(worktree, &["status", "--porcelain"]);
    for line in ["A  staged.txt", "M seed.txt", "?? notes.txt"] {
        assert!(status.contains(line), "{line} in {status}");
    }
    // Parked runs stay out of the claim and the resume.
    let parked = payloads(&detail, "replan_parked")[0].clone();
    assert_eq!(parked["snapshot_id"], snapshot.as_str());
    assert_eq!(parked["status"], "needs_session");
}

/// Acceptance: a run whose turn's work is all committed is saved clean,
/// and once the operation ends in a continuation the run resumes in its
/// worktree, as it was, told it goes on under the same conditions, and
/// lands through validation and review.
#[test]
fn a_continuation_resumes_the_parked_run_on_its_saved_work_and_lands() {
    let (_dir, repo, db, backend, op) =
        park_during_a_turn("git add -A && git commit -qm everything;");
    assert_eq!(op.status, ReplanStatus::Parked, "{op:?}");
    assert!(!manifest(&db, &op).dirty);
    advance(&db, &op, ReplanStatus::ContinuingAfterPark);
    let (_reviewer, supervisor) =
        supervise_thread(&db, &repo, backend.clone(), Default::default(), &[]);
    finished(&db, &backend, supervisor);
    let detail = detail(&db);
    assert_eq!(detail.task.status(), TaskStatus::Completed);
    assert_eq!(detail.runs[0].status(), RunStatus::Integrated);
    let restored = payloads(&detail, "replan_restored");
    assert_eq!(restored.len(), 1, "{restored:?}");
    assert_eq!(restored[0]["action"], "intact");
    assert_eq!(restored[0]["operation_id"], json!(op.id));
    let kinds = event_kinds(&detail);
    let at = |kind: &str| kinds.iter().position(|k| *k == kind).unwrap();
    // The work is checked before the resume begins (no attempt is spent
    // on a worktree that changed), and its session is launched after.
    let launched = kinds
        .iter()
        .rposition(|k| *k == "wrapper_launched")
        .unwrap();
    assert!(at("replan_parked") < at("replan_restored"));
    assert!(at("replan_restored") < at("resume_started"));
    assert!(at("resume_started") < launched);
    assert!(launched < at("review_started"));
    let request =
        fs::read_to_string(Path::new(detail.runs[0].run_dir().unwrap()).join("resume-1.txt"))
            .unwrap();
    assert!(
        request.contains("was paused at the end of a turn"),
        "{request}"
    );
    for file in ["notes.txt", "staged.txt"] {
        assert!(
            git_out(&repo, &["ls-tree", "--name-only", "main"]).contains(file),
            "{file} landed"
        );
    }
}

/// Acceptance: once a withdrawal ends the operation, a run whose worktree
/// is gone gets it made again from the snapshot (its branch at the pinned
/// head, the staged and unstaged changes and the untracked file) before
/// it resumes, and lands with all of that work.
#[test]
fn a_withdrawal_makes_the_lost_worktree_again_from_the_snapshot() {
    let (_dir, repo, db, backend, op) = park_during_a_turn("");
    let run = detail(&db).runs[0].clone();
    let worktree = run.worktree_path().unwrap().to_owned();
    git(&repo, &["worktree", "remove", "--force", &worktree]);
    assert!(!Path::new(&worktree).exists());
    let planning = advance(&db, &op, ReplanStatus::Planning);
    advance(&db, &planning, ReplanStatus::Withdrawn);
    let (_reviewer, supervisor) =
        supervise_thread(&db, &repo, backend.clone(), Default::default(), &[]);
    finished(&db, &backend, supervisor);
    let detail = detail(&db);
    assert_eq!(detail.task.status(), TaskStatus::Completed);
    let restored = payloads(&detail, "replan_restored");
    assert_eq!(restored.len(), 1, "{restored:?}");
    assert_eq!(restored[0]["action"], "recreated");
    let landed = git_out(&repo, &["show", "main:seed.txt"]);
    assert!(landed.contains("half"), "{landed}");
    for file in ["notes.txt", "staged.txt"] {
        assert!(
            git_out(&repo, &["ls-tree", "--name-only", "main"]).contains(file),
            "{file} landed"
        );
    }
    // The untracked script came back executable, and landed so.
    assert!(
        git_out(&repo, &["ls-tree", "main", "run.sh"]).starts_with("100755 "),
        "{}",
        git_out(&repo, &["ls-tree", "main", "run.sh"])
    );
}

/// Acceptance: a restore stopped half way (the runtime stopped after it
/// began to put the worktree together) is taken up again from the
/// snapshot: what the half-made scratch worktree held is thrown away (it
/// is a copy, not the run's work), the worktree is made again in full,
/// and the run resumes and lands with the saved work only.
#[test]
fn a_restore_stopped_half_way_starts_again_from_the_snapshot() {
    let (_dir, repo, db, backend, op) = park_during_a_turn("");
    let run = detail(&db).runs[0].clone();
    let worktree = run.worktree_path().unwrap().to_owned();
    let head = git_out(Path::new(&worktree), &["rev-parse", "HEAD"]);
    git(&repo, &["worktree", "remove", "--force", &worktree]);
    // What a restore stopped after making its scratch worktree left: the
    // head checked out, a diff half applied, no untracked files yet.
    let scratch = dagq::domain::replan_snapshot::scratch_worktree(Path::new(&worktree));
    git(
        &repo,
        &[
            "worktree",
            "add",
            "--detach",
            scratch.to_str().unwrap(),
            &head,
        ],
    );
    fs::write(scratch.join("half-applied.txt"), "partial\n").unwrap();
    advance(&db, &op, ReplanStatus::ContinuingAfterPark);
    let (_reviewer, supervisor) =
        supervise_thread(&db, &repo, backend.clone(), Default::default(), &[]);
    finished(&db, &backend, supervisor);
    let detail = detail(&db);
    assert_eq!(detail.task.status(), TaskStatus::Completed);
    let restored = payloads(&detail, "replan_restored");
    assert_eq!(restored.len(), 1, "{restored:?}");
    assert_eq!(restored[0]["action"], "recreated");
    assert!(!scratch.exists());
    let tree = git_out(&repo, &["ls-tree", "--name-only", "main"]);
    for file in ["notes.txt", "staged.txt", "run.sh"] {
        assert!(tree.contains(file), "{file} landed: {tree}");
    }
    assert!(!tree.contains("half-applied.txt"), "{tree}");
}

/// A run of the old layout (its worktree at `<run dir>/worktree`) whose
/// restore stopped after it checked the run's branch out in the scratch of
/// the old name (`worktree.restoring`) is restored all the same: that
/// scratch, the run's own on its branch, is discarded before the branch is
/// checked out in the new one, and the run resumes and lands with the
/// saved work only.
#[test]
fn a_restore_of_the_old_layout_stopped_in_the_old_scratch_starts_again() {
    let (_dir, repo, db, backend, op) = park_during_a_turn("");
    let run = detail(&db).runs[0].clone();
    let made = PathBuf::from(run.worktree_path().unwrap());
    let branch = run.branch().unwrap().to_owned();
    git(
        &repo,
        &["worktree", "remove", "--force", made.to_str().unwrap()],
    );
    let old = made.with_file_name("worktree");
    Connection::open(&db)
        .unwrap()
        .execute(
            "UPDATE task_runs SET worktree_path=?1 WHERE id=?2",
            rusqlite::params![old.to_str().unwrap(), run.id().as_str()],
        )
        .unwrap();
    assert_eq!(
        SqliteQueue::open(&db)
            .unwrap()
            .run(run.id())
            .unwrap()
            .worktree_path(),
        old.to_str()
    );
    let scratch = old.with_file_name("worktree.restoring");
    git(
        &repo,
        &["worktree", "add", "-q", scratch.to_str().unwrap(), &branch],
    );
    fs::write(scratch.join("half-applied.txt"), "partial\n").unwrap();
    advance(&db, &op, ReplanStatus::ContinuingAfterPark);
    let (_reviewer, supervisor) =
        supervise_thread(&db, &repo, backend.clone(), Default::default(), &[]);
    finished(&db, &backend, supervisor);
    let detail = detail(&db);
    assert_eq!(detail.task.status(), TaskStatus::Completed);
    let restored = payloads(&detail, "replan_restored");
    assert_eq!(restored.len(), 1, "{restored:?}");
    assert_eq!(restored[0]["action"], "recreated");
    assert!(!scratch.exists());
    let tree = git_out(&repo, &["ls-tree", "--name-only", "main"]);
    for file in ["notes.txt", "staged.txt", "run.sh"] {
        assert!(tree.contains(file), "{file} landed: {tree}");
    }
    assert!(!tree.contains("half-applied.txt"), "{tree}");
}

/// Acceptance: a process a turn left running in the worktree outside its
/// process group (a detached helper, which the wrapper leaves on purpose)
/// keeps the work from being saved while it may still write it: once the
/// wrapper and the turn are gone and the helper outlives a short grace,
/// the operation is `blocked` with the helper named, no snapshot is
/// recorded, the worktree keeps the work, and the slot is given back.
#[test]
fn a_helper_left_running_in_the_worktree_blocks_the_park() {
    /// Stops the helper by its pid when the test ends, passed or not.
    struct Helper(PathBuf);
    impl Drop for Helper {
        fn drop(&mut self) {
            if let Ok(pid) = fs::read_to_string(&self.0) {
                let _ = std::process::Command::new("kill")
                    .args(["-9", pid.trim()])
                    .status();
            }
        }
    }
    // The helper ends by itself once this test process is gone (a time
    // limit's exit) or its run directory is (the fixture dropped, a panic
    // included), and after two minutes at most; the guard below stops it
    // at once when the test ends.
    let helper = format!(
        r#"printf '%s\n' 'i=0; while [ $i -lt 120 ] && kill -0 {test} 2>/dev/null && [ -d "$1" ]; do sleep 1; i=$((i+1)); done' > "$RUN_DIR/helper.sh"; perl -e 'setpgrp(0, 0) or die "setpgrp: $!"; open(my $p, ">", "$ARGV[0]/helper.tmp") or die $!; print $p "$$\n"; close($p) or die $!; rename "$ARGV[0]/helper.tmp", "$ARGV[0]/helper.pid" or die $!; exec "sh", "$ARGV[0]/helper.sh", "$ARGV[0]"' "$RUN_DIR" >/dev/null 2>&1 & await_file "$RUN_DIR/helper.pid";"#,
        test = std::process::id()
    );
    let (_dir, _repo, db, _backend, op) = park_during_a_turn(&helper);
    let run = detail(&db).runs[0].clone();
    let _helper = Helper(Path::new(run.run_dir().unwrap()).join("helper.pid"));
    let pid = fs::read_to_string(Path::new(run.run_dir().unwrap()).join("helper.pid")).unwrap();
    assert_eq!(op.status, ReplanStatus::Blocked, "{op:?}");
    assert_eq!(op.return_phase, Some(ReplanStatus::Quiescing));
    assert!(op.snapshot_id.is_none());
    let reason = op.status_reason.clone().unwrap();
    assert!(reason.contains("still run"), "{reason}");
    assert!(reason.contains(pid.trim()), "{reason}");
    assert!(
        !replans_dir(&db)
            .join(format!("replan-{}-g2", op.id))
            .exists()
    );
    let detail = detail(&db);
    assert_eq!(detail.runs[0].status(), RunStatus::NeedsSession);
    let worktree = Path::new(run.worktree_path().unwrap());
    assert!(worktree.join("notes.txt").exists());
}

/// Acceptance: a turn the wrapper would run again as a new session (its
/// agent kept no thread of the one it resumed) does not start once the run
/// is being parked: the operation was opened while the resumed turn ran,
/// that turn ended with its thread missing, and the wrapper ended there
/// rather than starting the new session's turn; the run is parked. The
/// answer that turn was to give the agent never reached it, so the park
/// carries it, and the continuation's resumed session gets it and lands.
#[test]
fn a_turn_run_again_as_a_new_session_does_not_start_once_the_park_began() {
    let (dir, repo, db, backend, codex) = crate::runtime_support::codex::codex_fixture();
    set_turns(
        dir.path(),
        &format!(
            r#"case "$PROMPT" in
*'paused at the end of a turn'*) case "$PROMPT" in *'answer to ask '*'change.txt'*) {} ;; *) fail "the answer was not carried" ;; esac ;;
*) case "$TURN" in
1) ask "which file"; say asked ;;
2) await_file "$RUN_DIR/go"; echo "Error: thread/resume: thread/resume failed: no rollout found for thread id $THREAD (code -32600)" >&2; exit 1 ;;
*) say "a turn that must not start" ;;
esac ;;
esac"#,
            crate::runtime_support::codex::FINISH
        ),
    );
    // The resume's stub `claude` is never run (the worker is on Codex), but
    // the backend names one for every resume.
    backend.resume_script_for(2, "exit 99");
    let backend = Arc::new(backend);
    let supervisor = crate::runtime_support::codex::supervise_thread(
        &db,
        &repo,
        backend.clone(),
        &codex,
        &[verdict("pass", &[], "meets the acceptance")],
    );
    let ask =
        crate::runtime_support::codex::open_ask(&db, |ask| ask.kind == AskKind::WorkerQuestion);
    SqliteQueue::open(&db)
        .unwrap()
        .answer(ask.id, "change.txt")
        .unwrap();
    wait_until(&db, common::STEP_LIMIT, |queue| {
        payloads(&queue.show(TASK).unwrap(), "turn_started").len() == 2
    });
    let run = detail(&db).runs[0].clone();
    let op = pause(&db, run.id());
    wait_for_status(&db, op.id, ReplanStatus::Quiescing);
    fs::write(Path::new(run.run_dir().unwrap()).join("go"), "").unwrap();
    crate::runtime_support::codex::finished(&backend, supervisor);
    let parked = operation(&db, op.id);
    assert_eq!(parked.status, ReplanStatus::Parked);
    let detail = detail(&db);
    let calls = stub_calls(&detail.runs[0]);
    assert_eq!(calls.len(), 2, "{calls:?}");
    assert_eq!(payloads(&detail, "turn_started").len(), 2);
    let missing: Vec<&Value> = payloads(&detail, "turn_session_identified")
        .into_iter()
        .filter(|p| !p["missing"].is_null())
        .collect();
    assert_eq!(missing.len(), 1, "{missing:?}");
    assert!(event_kinds(&detail).contains(&"turn_withheld"));
    assert_eq!(detail.runs[0].status(), RunStatus::NeedsSession);
    // The answer's turn started but ran nothing: it is carried.
    let carried = payloads(&detail, "replan_parked")[0]["carried_answers"].clone();
    assert_eq!(carried[0]["ask_id"], json!(ask.id), "{carried}");

    advance(&db, &parked, ReplanStatus::ContinuingAfterPark);
    let supervisor = crate::runtime_support::codex::supervise_thread(
        &db,
        &repo,
        backend.clone(),
        &codex,
        &[verdict("pass", &[], "meets the acceptance")],
    );
    crate::runtime_support::codex::finished(&backend, supervisor);
    let detail = self::detail(&db);
    assert_eq!(detail.task.status(), TaskStatus::Completed);
    let request =
        fs::read_to_string(Path::new(run.run_dir().unwrap()).join("resume-1.txt")).unwrap();
    assert!(
        request.contains(&format!("answer to ask {}", ask.id)),
        "{request}"
    );
}

/// Acceptance: a snapshot that cannot save the run's work whole (an
/// untracked link, which is not followed) fails rather than leave it out:
/// the operation is `blocked` with why and the step it failed at, nothing
/// is recorded as saved, the worktree keeps every change, and the run is
/// parked out of its slot all the same.
#[test]
fn a_save_that_fails_keeps_the_work_and_blocks_the_operation() {
    let (_dir, _repo, db, _backend, op) = park_during_a_turn("ln -s seed.txt link.txt;");
    assert_eq!(op.status, ReplanStatus::Blocked, "{op:?}");
    assert_eq!(op.return_phase, Some(ReplanStatus::Snapshotting));
    assert!(op.snapshot_id.is_none());
    let reason = op.status_reason.clone().unwrap();
    assert!(reason.contains("link.txt"), "{reason}");
    assert!(reason.contains("not a regular file"), "{reason}");
    assert!(
        !replans_dir(&db)
            .join(format!("replan-{}-g2", op.id))
            .exists()
    );
    let detail = detail(&db);
    let run = &detail.runs[0];
    assert_eq!(run.status(), RunStatus::NeedsSession);
    let worktree = Path::new(run.worktree_path().unwrap());
    let status = git_out(worktree, &["status", "--porcelain"]);
    for line in ["A  staged.txt", "M seed.txt", "?? notes.txt", "?? link.txt"] {
        assert!(status.contains(line), "{line} in {status}");
    }
    // A blocked operation still guards the task: nothing resumes it.
    let (_reviewer, supervisor) = supervise_thread(
        &db,
        Path::new(run.repo_path().unwrap()),
        Arc::new(TestWorkspace::new(&db, false, "exit 99")),
        Default::default(),
        &[],
    );
    joined(supervisor, "the supervisor thread to return").unwrap();
    assert!(payloads(&self::detail(&db), "resume_started").is_empty());
}

/// Acceptance: an operation opened while the run is idle (its question
/// waits for a person) parks it as well: the wrapper ends without another
/// turn; the answer given after the fence is not delivered to the old
/// session, nor starts a turn, while the operation guards the task.
#[test]
fn an_idle_run_is_parked_and_a_later_answer_is_not_delivered() {
    let (dir, repo, db, backend) = headless_fixture(&[]);
    set_turns(dir.path(), r#"ask "which file"; say asked"#);
    let backend = Arc::new(backend);
    let (_reviewer, supervisor) =
        supervise_thread(&db, &repo, backend.clone(), Default::default(), &[]);
    wait_until(&db, common::STEP_LIMIT, |queue| {
        queue
            .asks(AskQuery::default())
            .unwrap()
            .iter()
            .any(|ask| ask.kind == AskKind::WorkerQuestion)
    });
    let run = detail(&db).runs[0].clone();
    let op = pause(&db, run.id());
    finished(&db, &backend, supervisor);
    let op = operation(&db, op.id);
    assert_eq!(op.status, ReplanStatus::Parked, "{op:?}");
    let mut queue = SqliteQueue::open(&db).unwrap();
    let ask = queue
        .asks(AskQuery::default())
        .unwrap()
        .into_iter()
        .find(|ask| ask.kind == AskKind::WorkerQuestion)
        .unwrap();
    queue.answer(ask.id, "change.txt").unwrap();
    let (_reviewer, supervisor) =
        supervise_thread(&db, &repo, backend.clone(), Default::default(), &[]);
    finished(&db, &backend, supervisor);
    let detail = detail(&db);
    let run = &detail.runs[0];
    assert_eq!(run.status(), RunStatus::NeedsSession);
    assert_eq!(stub_calls(run).len(), 1, "{:?}", stub_calls(run));
    let kinds = event_kinds(&detail);
    assert!(kinds.contains(&"turn_withheld"), "{kinds:?}");
    for kind in ["ask_delivered", "resume_started"] {
        assert!(!kinds.contains(&kind), "{kind} in {kinds:?}");
    }
    assert!(
        payloads(&detail, "turn_requested")
            .iter()
            .all(|p| !p["what"].as_str().unwrap_or_default().contains("answer")),
        "{:?}",
        payloads(&detail, "turn_requested")
    );
    assert_eq!(operation(&db, op.id).status, ReplanStatus::Parked);
}

/// Acceptance: a park a supervisor left half done (it died with the
/// operation at `snapshotting`) is taken up by the supervisor that adopts
/// the run, from the step the queue records: the snapshot is saved once
/// and the run parked once. A supervisor after it finds nothing to do: the
/// same operation, generation and snapshot.
#[test]
fn a_park_left_half_done_is_finished_once_by_the_adopter() {
    let (dir, repo, db, backend) = headless_fixture(&[]);
    set_turns(dir.path(), r#"printf 'notes\n' > notes.txt; say working"#);
    let backend = Arc::new(backend);
    let run = start_run_under_dead_supervisor(&repo, &db, &backend, "dead-supervisor");
    wait_until(&db, common::STEP_LIMIT, |queue| {
        !payloads(&queue.show(TASK).unwrap(), "turn_finished").is_empty()
    });
    let op = pause(&db, run.id());
    let op = advance(&db, &op, ReplanStatus::Quiescing);
    let op = advance(&db, &op, ReplanStatus::Snapshotting);
    age_lease(&db, &run, 31);
    let (_reviewer, supervisor) =
        supervise_thread(&db, &repo, backend.clone(), Default::default(), &[]);
    finished(&db, &backend, supervisor);
    let parked = operation(&db, op.id);
    assert_eq!(parked.status, ReplanStatus::Parked, "{parked:?}");
    assert_eq!(parked.generation, op.generation);
    assert_eq!(
        saved(&db, &parked, "untracked", Some("notes.txt")),
        "notes\n"
    );
    let detail = detail(&db);
    assert!(!adoption_events(&detail).is_empty());
    assert_eq!(payloads(&detail, "replan_parked").len(), 1);
    assert_eq!(detail.runs[0].status(), RunStatus::NeedsSession);

    let (_reviewer, supervisor) =
        supervise_thread(&db, &repo, backend.clone(), Default::default(), &[]);
    finished(&db, &backend, supervisor);
    let again = operation(&db, op.id);
    assert_eq!(
        (
            again.status,
            again.generation,
            again.snapshot_id.clone(),
            again.updated_at
        ),
        (
            parked.status,
            parked.generation,
            parked.snapshot_id.clone(),
            parked.updated_at
        )
    );
    assert_eq!(payloads(&self::detail(&db), "replan_parked").len(), 1);
    let snapshots: Vec<_> = fs::read_dir(replans_dir(&db))
        .unwrap()
        .map(|entry| entry.unwrap().file_name().into_string().unwrap())
        .collect();
    assert_eq!(snapshots, [parked.snapshot_id.unwrap()]);
}

/// Acceptance: the park stops and closes only the sessions of the parked
/// run, by their handle: another run that works meanwhile in the other
/// slot is not touched and lands.
#[test]
fn the_park_touches_no_other_runs_session() {
    let (dir, repo, db, backend) = headless_fixture(&[]);
    turns(dir.path(), "");
    let other = add_headless_task(&db, "other task");
    let backend = Arc::new(backend);
    let (_reviewer, supervisor) =
        supervise_thread_in(&db, &repo, backend.clone(), Default::default(), &[], 2);
    wait_until(&db, common::STEP_LIMIT, |queue| {
        !payloads(&queue.show(TASK).unwrap(), "turn_started").is_empty()
    });
    let run = detail(&db).runs[0].clone();
    let op = pause(&db, run.id());
    wait_for_status(&db, op.id, ReplanStatus::Quiescing);
    fs::write(Path::new(run.run_dir().unwrap()).join("go"), "").unwrap();
    finished(&db, &backend, supervisor);
    assert_eq!(operation(&db, op.id).status, ReplanStatus::Parked);
    let mut queue = SqliteQueue::open(&db).unwrap();
    let landed = queue.show(other).unwrap();
    assert_eq!(landed.task.status(), TaskStatus::Completed);
    let replan_stops = |detail: &dagq::domain::TaskDetail| -> Vec<Value> {
        detail
            .events
            .iter()
            .filter(|event| {
                (event.kind == EventKind::WrapperStopped && event.payload["route"] == "replan")
                    || (event.kind == EventKind::WorkspaceClosed && event.payload["by"] == "replan")
            })
            .map(|event| event.payload.clone())
            .collect()
    };
    assert!(
        replan_stops(&landed).is_empty(),
        "{:?}",
        replan_stops(&landed)
    );
    let parked = detail(&db);
    let closed = replan_stops(&parked);
    assert!(!closed.is_empty());
    let handle = run.workspace_id().unwrap();
    assert!(
        closed
            .iter()
            .all(|payload| payload["workspace_id"] == handle),
        "{closed:?}"
    );
}

/// Acceptance: a worktree that changed after its snapshot is neither
/// overwritten nor thrown away when the operation ends: the restore fails
/// once (`replan_restore_failed`), the operation goes back to `blocked`
/// with why and guards the task again, the run does not resume (no resume
/// attempt spent), and `status` lists the operation among its problems.
#[test]
fn a_worktree_changed_since_its_snapshot_keeps_the_run_from_resuming() {
    let (_dir, repo, db, backend, op) = park_during_a_turn("");
    let run = detail(&db).runs[0].clone();
    let worktree = Path::new(run.worktree_path().unwrap());
    fs::write(worktree.join("later.txt"), "someone else's\n").unwrap();
    advance(&db, &op, ReplanStatus::ContinuingAfterPark);
    for _ in 0..2 {
        let (_reviewer, supervisor) =
            supervise_thread(&db, &repo, backend.clone(), Default::default(), &[]);
        finished(&db, &backend, supervisor);
    }
    let detail = detail(&db);
    assert_eq!(detail.runs[0].status(), RunStatus::NeedsSession);
    let kinds = event_kinds(&detail);
    for kind in ["resume_started", "replan_restored"] {
        assert!(!kinds.contains(&kind), "{kind} in {kinds:?}");
    }
    let failed = payloads(&detail, "replan_restore_failed");
    assert_eq!(failed.len(), 1, "{failed:?}");
    assert!(
        failed[0]["reason"]
            .as_str()
            .unwrap()
            .contains("untracked files differ"),
        "{failed:?}"
    );
    assert_eq!(
        fs::read_to_string(worktree.join("later.txt")).unwrap(),
        "someone else's\n"
    );
    assert!(worktree.join("notes.txt").exists());
    let blocked = operation(&db, op.id);
    assert_eq!(blocked.status, ReplanStatus::Blocked, "{blocked:?}");
    assert_eq!(
        blocked.return_phase,
        Some(ReplanStatus::ContinuingAfterPark)
    );
    assert!(blocked.closed_at.is_none());
    assert!(
        blocked
            .status_reason
            .as_deref()
            .unwrap()
            .contains("could not be restored"),
        "{blocked:?}"
    );
    let status = common::cli::ok(&db, &["status"]);
    let problems = status["replans"]["problems"].as_array().unwrap().clone();
    assert_eq!(problems.len(), 1, "{problems:?}");
    assert_eq!(problems[0]["problem"], "blocked");
    assert_eq!(problems[0]["id"], json!(op.id));
    assert_eq!(problems[0]["return_phase"], "continuing_after_park");
}

/// The turns of a run that finishes its first turn at once and, resumed
/// after a park, commits once more and finishes.
fn finish_then_go_on(dir: &Path) {
    set_turns(
        dir,
        &format!(
            r#"case "$PROMPT" in
*'paused at the end of a turn'*) commit "after the park"; receipt "$(git rev-parse HEAD)"; say finished ;;
*) {FINISH} ;;
esac"#
        ),
    );
}

/// Acceptance: a run nobody holds, waiting for the answer to its landing
/// question, is parked where it waits once an operation is opened on it:
/// it becomes `needs_session` (`replan_parked`) and its open question is
/// closed, so an answer given meanwhile does not land it. Once the
/// operation ends in a continuation, the run resumes on its restored work
/// and lands only through a new validation and review.
#[test]
fn a_run_waiting_for_its_landing_answer_is_parked_and_lands_only_after_a_new_review() {
    let (dir, repo, db, backend) = headless_fixture(&[]);
    finish_then_go_on(dir.path());
    let reviewer = TestReviewer::new(&[
        verdict("concern", &["a judgment"], "decide"),
        verdict("pass", &[], "meets the acceptance"),
    ]);
    let options = supervise_options(4, true);
    let outcome = supervise_reviewed_with(&db, &repo, &backend, &reviewer, &options);
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    let run = detail(&db).runs[0].clone();
    assert_eq!(run.status(), RunStatus::AwaitingIntegration);
    let mut queue = SqliteQueue::open(&db).unwrap();
    let ask = queue
        .asks(AskQuery::default())
        .unwrap()
        .into_iter()
        .find(|ask| ask.kind == AskKind::ApproveLanding)
        .unwrap();
    let op = pause(&db, run.id());
    let outcome = supervise_reviewed_with(&db, &repo, &backend, &reviewer, &options);
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    let parked = operation(&db, op.id);
    assert_eq!(parked.status, ReplanStatus::Parked, "{parked:?}");
    let detail = detail(&db);
    assert_eq!(detail.runs[0].status(), RunStatus::NeedsSession);
    assert_eq!(payloads(&detail, "replan_parked").len(), 1);
    assert!(queue.read_ask(ask.id).unwrap().closed_at.is_some());
    // An answer given now lands nothing: the question was closed.
    assert!(queue.answer(ask.id, "land").is_err());
    advance(&db, &parked, ReplanStatus::ContinuingAfterPark);
    let outcome = supervise_reviewed_with(&db, &repo, &backend, &reviewer, &options);
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    let detail = self::detail(&db);
    assert_eq!(detail.task.status(), TaskStatus::Completed);
    let kinds = event_kinds(&detail);
    let at = |kind: &str| kinds.iter().position(|k| *k == kind).unwrap();
    let last = |kind: &str| kinds.iter().rposition(|k| *k == kind).unwrap();
    assert!(at("replan_parked") < at("replan_restored"));
    assert!(at("replan_restored") < at("resume_started"));
    assert!(at("resume_started") < last("review_started"));
    assert!(last("review_started") < at("integration_started"));
    assert_eq!(payloads(&detail, "review_finished").len(), 2);
}

/// Acceptance: a run nobody holds that waits for its resume, an earlier
/// resume of which ended unresolved, is parked with its park recorded, so
/// the continuation runs a resume on the restored work rather than moving
/// the run on with the receipt it already has (no `resume_skipped`).
#[test]
fn a_run_waiting_for_its_resume_is_parked_and_the_continuation_resumes_it() {
    let (dir, repo, db, backend) = headless_fixture(&[]);
    finish_then_go_on(dir.path());
    let reviewer = TestReviewer::new(&[
        verdict("concern", &["a judgment"], "decide"),
        verdict("pass", &[], "meets the acceptance"),
    ]);
    let options = supervise_options(4, true);
    let outcome = supervise_reviewed_with(&db, &repo, &backend, &reviewer, &options);
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    let run = detail(&db).runs[0].clone();
    // As a landing deferred for a conflict and a resume that ended
    // unresolved leave it: waiting for a session, its receipt naming its
    // head.
    let queue = SqliteQueue::open(&db).unwrap();
    queue
        .record_runtime_event(
            run.id(),
            dagq::domain::EventKind::IntegrationDeferred,
            json!({"code": "rebase_conflict", "reason": "a conflict", "status": "needs_session"}),
        )
        .unwrap();
    queue
        .record_runtime_event(
            run.id(),
            dagq::domain::EventKind::ResumeFinished,
            json!({"attempt": 1, "outcome": "unresolved"}),
        )
        .unwrap();
    Connection::open(&db)
        .unwrap()
        .execute(
            "UPDATE task_runs SET status='needs_session' WHERE id=?1",
            [run.id().as_str()],
        )
        .unwrap();
    let op = pause(&db, run.id());
    let outcome = supervise_reviewed_with(&db, &repo, &backend, &reviewer, &options);
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    let parked = operation(&db, op.id);
    assert_eq!(parked.status, ReplanStatus::Parked, "{parked:?}");
    let events = queue.run_events(run.id()).unwrap();
    assert!(!dagq::domain::run::RunHistory::from_events(&events).unresolved_since_park());
    advance(&db, &parked, ReplanStatus::ContinuingAfterPark);
    let outcome = supervise_reviewed_with(&db, &repo, &backend, &reviewer, &options);
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    let detail = detail(&db);
    assert_eq!(detail.task.status(), TaskStatus::Completed);
    let kinds = event_kinds(&detail);
    assert!(!kinds.contains(&"resume_skipped"), "{kinds:?}");
    let at = |kind: &str| kinds.iter().position(|k| *k == kind).unwrap();
    assert!(at("replan_parked") < at("replan_restored"));
    assert!(at("replan_restored") < at("resume_started"));
}

/// Acceptance: a run nobody holds whose resumes are used up is neither
/// failed nor handed to the recovery job while a replanning parks it, so
/// the operation's outcome finds it with its work. Once the operation ends
/// in a continuation, its work is checked back from the snapshot first
/// (`replan_restored`), and only then is it handed on as a run out of
/// resumes (`resume_exhausted`), without another resume.
#[test]
fn a_run_out_of_resumes_is_handed_on_only_after_its_park_ends_and_its_work_is_restored() {
    let (dir, repo, db, backend) = headless_fixture(&[]);
    finish_then_go_on(dir.path());
    let reviewer = TestReviewer::new(&[verdict("concern", &["a judgment"], "decide")])
        .with_triages(&[recovery(json!({
            "verdict": "escalate",
            "confidence": "high",
            "diagnosis": "the parts need a person",
            "options": [{"kind": "recover", "text": "split the task"}],
        }))]);
    let options = supervise_options(4, true);
    let outcome = supervise_reviewed_with(&db, &repo, &backend, &reviewer, &options);
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    let run = detail(&db).runs[0].clone();
    // As a landing deferred for a conflict and three resumes that ended
    // unresolved leave it: waiting for a session with none left.
    let queue = SqliteQueue::open(&db).unwrap();
    queue
        .record_runtime_event(
            run.id(),
            dagq::domain::EventKind::IntegrationDeferred,
            json!({"code": "rebase_conflict", "reason": "a conflict", "status": "needs_session"}),
        )
        .unwrap();
    for attempt in 1..=dagq::domain::MAX_RESUME_ATTEMPTS {
        for (kind, payload) in [
            (
                dagq::domain::EventKind::ResumeStarted,
                json!({"attempt": attempt}),
            ),
            (
                dagq::domain::EventKind::ResumeFinished,
                json!({"attempt": attempt, "outcome": "unresolved"}),
            ),
        ] {
            queue.record_runtime_event(run.id(), kind, payload).unwrap();
        }
    }
    Connection::open(&db)
        .unwrap()
        .execute(
            "UPDATE task_runs SET status='needs_session' WHERE id=?1",
            [run.id().as_str()],
        )
        .unwrap();
    let op = pause(&db, run.id());
    let outcome = supervise_reviewed_with(&db, &repo, &backend, &reviewer, &options);
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    let parked = operation(&db, op.id);
    assert_eq!(parked.status, ReplanStatus::Parked, "{parked:?}");
    let waiting = detail(&db);
    assert_eq!(waiting.runs[0].status(), RunStatus::NeedsSession);
    assert!(payloads(&waiting, "recovery_requested").is_empty());
    // Another pass while it is parked does not give it up either.
    let outcome = supervise_reviewed_with(&db, &repo, &backend, &reviewer, &options);
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    assert!(payloads(&detail(&db), "recovery_requested").is_empty());
    advance(&db, &parked, ReplanStatus::ContinuingAfterPark);
    let outcome = supervise_reviewed_with(&db, &repo, &backend, &reviewer, &options);
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    let detail = detail(&db);
    assert_eq!(detail.runs[0].status(), RunStatus::Failed);
    let requested = payloads(&detail, "recovery_requested");
    assert_eq!(requested.len(), 1, "{:?}", event_kinds(&detail));
    assert_eq!(requested[0]["alert"], "resume_exhausted");
    let kinds = event_kinds(&detail);
    let at = |kind: &str| kinds.iter().position(|k| *k == kind).unwrap();
    assert!(at("replan_parked") < at("replan_restored"));
    assert!(at("replan_restored") < at("recovery_requested"));
    assert_eq!(
        payloads(&detail, "resume_started").len(),
        dagq::domain::MAX_RESUME_ATTEMPTS,
        "no resume past the used-up ones"
    );
}

/// Acceptance: an approval to land given before the park (the
/// `integration_approved` a person's `land` records) approved the work as
/// it was then: once the operation ends in a continuation, what the
/// resumed session adds goes through a new review before it lands, rather
/// than landing on the earlier approval.
#[test]
fn an_approval_from_before_the_park_does_not_land_the_continuations_work() {
    let (dir, repo, db, backend) = headless_fixture(&[]);
    finish_then_go_on(dir.path());
    let reviewer = TestReviewer::new(&[
        verdict("concern", &["a judgment"], "decide"),
        verdict("pass", &[], "meets the acceptance"),
    ]);
    let options = supervise_options(4, true);
    let outcome = supervise_reviewed_with(&db, &repo, &backend, &reviewer, &options);
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    let run = detail(&db).runs[0].clone();
    assert_eq!(run.status(), RunStatus::AwaitingIntegration);
    // A person approved the run to land, and the operation opens before
    // the approval is acted on.
    SqliteQueue::open(&db)
        .unwrap()
        .record_runtime_event(
            run.id(),
            dagq::domain::EventKind::IntegrationApproved,
            json!({"status": "awaiting_integration"}),
        )
        .unwrap();
    let op = pause(&db, run.id());
    let outcome = supervise_reviewed_with(&db, &repo, &backend, &reviewer, &options);
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    let parked = operation(&db, op.id);
    assert_eq!(parked.status, ReplanStatus::Parked, "{parked:?}");
    assert!(payloads(&detail(&db), "integration_started").is_empty());
    advance(&db, &parked, ReplanStatus::ContinuingAfterPark);
    let outcome = supervise_reviewed_with(&db, &repo, &backend, &reviewer, &options);
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    let detail = self::detail(&db);
    assert_eq!(detail.task.status(), TaskStatus::Completed);
    let reviews = payloads(&detail, "review_finished");
    assert_eq!(reviews.len(), 2, "{reviews:?}");
    assert_eq!(reviews[1]["verdict"], "pass");
    let kinds = event_kinds(&detail);
    let at = |kind: &str| kinds.iter().position(|k| *k == kind).unwrap();
    let last = |kind: &str| kinds.iter().rposition(|k| *k == kind).unwrap();
    assert!(at("replan_restored") < last("review_started"));
    assert!(last("review_finished") < at("integration_started"));
}

/// The `turn_withheld` events of run `run` that say the guard could not be
/// read, from the queue's rows (reading the task's detail reads the
/// replanning operations, which the test has taken away).
fn unreadable_withheld(db: &Path, run: &RunId) -> i64 {
    Connection::open(db)
        .unwrap()
        .query_row(
            "SELECT count(*) FROM run_events WHERE run_id=?1 AND kind='turn_withheld'
             AND json_extract(payload, '$.guard_unreadable') IS NOT NULL",
            [run.as_str()],
            |r| r.get(0),
        )
        .unwrap()
}

/// Acceptance: while the wrapper cannot read the replanning guard of its
/// task, it starts no turn, not even for a request written before: it
/// records that it holds the turn (`turn_withheld` with the read's error)
/// and waits, the request untaken. Once the guard can be read again and
/// an operation guards the task, the wrapper ends without the request.
#[test]
fn a_request_written_before_does_not_run_while_the_guard_cannot_be_read() {
    let (dir, repo, db, backend) = headless_fixture(&[]);
    set_turns(dir.path(), "say working");
    let backend = Arc::new(backend);
    let run = start_run_under_dead_supervisor(&repo, &db, &backend, "dead-supervisor");
    wait_until(&db, common::STEP_LIMIT, |queue| {
        !payloads(&queue.show(TASK).unwrap(), "turn_finished").is_empty()
    });
    let away = |from: &str, to: &str| {
        Connection::open(&db)
            .unwrap()
            .execute(&format!("ALTER TABLE {from} RENAME TO {to}"), [])
            .unwrap();
    };
    away("replan_operations", "replan_operations_away");
    let seq = write_turn_request(&run, "a request written before the park", "test request");
    let request = dagq::domain::turn::request_path(Path::new(run.run_dir().unwrap()), seq);
    {
        let _waiting = common::within(common::STEP_LIMIT, "the wrapper to hold the turn");
        while unreadable_withheld(&db, run.id()) == 0 {
            thread::sleep(Duration::from_millis(20));
        }
    }
    assert!(request.exists(), "the request is not taken");
    assert_eq!(stub_calls(&run).len(), 1, "{:?}", stub_calls(&run));
    // The guard comes back with the table: an operation past the fence,
    // there the moment the guard can be read again.
    let op = {
        let conditions = r#"{"title":"t","description":"d","acceptance":[],"paths":[],"verify":[],"evidence":[],"predecessors":[],"successors":[],"goal":null}"#;
        let connection = Connection::open(&db).unwrap();
        connection
            .execute(
                "INSERT INTO replan_operations_away(task_id, run_id, origin, status, generation,
                   reason, conditions, task_version, diagnosis, requested_by, requested_by_id,
                   created_at, updated_at)
                 VALUES (?1, ?2, 'runtime', 'pause_requested', 2, 'replanning in the test', ?3,
                   'v', '{\"evidence\":[]}', 'supervisor', 'supervisor', unixepoch(), unixepoch())",
                rusqlite::params![TASK.as_i64(), run.id().as_str(), conditions],
            )
            .unwrap();
        connection.last_insert_rowid()
    };
    away("replan_operations_away", "replan_operations");
    backend.join();
    assert_eq!(stub_calls(&run).len(), 1, "{:?}", stub_calls(&run));
    assert!(request.exists(), "the request is never taken");
    let withheld = payloads(&detail(&db), "turn_withheld")
        .into_iter()
        .filter(|p| p["operation_id"] == json!(op))
        .count();
    assert_eq!(withheld, 1);
    assert_eq!(unreadable_withheld(&db, run.id()), 1);
}

/// The Git boundary of a restore's scratch worktree: the one it discards
/// must be the directory its path names. A link planted there to another
/// registered worktree is refused and that worktree is left whole; one
/// registered there on another branch is refused; the run's own is
/// removed, its branch kept; nothing there is nothing to do.
#[test]
fn a_discarded_scratch_worktree_is_never_a_link_nor_another_branchs() {
    let (dir, repo, _db) = fixture();
    let git = dagq::infrastructure::adapters::GitRepository::inspect(&repo).unwrap();
    let other = dir.path().join("other");
    git_out(
        &repo,
        &[
            "worktree",
            "add",
            "-q",
            "-b",
            "other",
            other.to_str().unwrap(),
        ],
    );
    let scratch = dir.path().join("run").join("worktree.restoring");
    fs::create_dir_all(scratch.parent().unwrap()).unwrap();
    std::os::unix::fs::symlink(&other, &scratch).unwrap();
    let refused = git.discard_worktree(&scratch, "dagq/r1").unwrap_err();
    assert!(refused.to_string().contains("not removed"), "{refused:#}");
    assert!(other.join("seed.txt").exists());
    fs::remove_file(&scratch).unwrap();
    // A worktree of another branch registered at the path itself.
    git_out(
        &repo,
        &[
            "worktree",
            "add",
            "-q",
            "-b",
            "elsewhere",
            scratch.to_str().unwrap(),
        ],
    );
    let refused = git.discard_worktree(&scratch, "dagq/r1").unwrap_err();
    assert!(
        refused.to_string().contains("branch elsewhere"),
        "{refused:#}"
    );
    assert!(scratch.join("seed.txt").exists());
    // The run's own scratch worktree goes, its branch stays.
    git_out(
        &repo,
        &["worktree", "remove", "--force", scratch.to_str().unwrap()],
    );
    git_out(
        &repo,
        &[
            "worktree",
            "add",
            "-q",
            "-b",
            "dagq/r1",
            scratch.to_str().unwrap(),
        ],
    );
    git.discard_worktree(&scratch, "dagq/r1").unwrap();
    assert!(!scratch.exists());
    git_out(&repo, &["rev-parse", "--verify", "dagq/r1"]);
    git.discard_worktree(&scratch, "dagq/r1").unwrap();
}

/// Acceptance: a restore is one supervisor's at a time. While another
/// supervisor holds the run for its restore (its lease, taken by
/// `begin_restore`, with its half-made scratch worktree beside the lost
/// worktree), a supervisor that takes up the withdrawal leaves the run be:
/// it neither throws that scratch away, nor resumes the run, nor moves the
/// operation back to `blocked`. Once that holder's lease is stale, the next
/// pass takes the restore over from the snapshot, and the run lands once
/// with its saved work.
#[test]
fn a_restore_another_supervisor_holds_is_left_to_it_until_its_lease_is_stale() {
    let (_dir, repo, db, backend, op) = park_during_a_turn("");
    let run = detail(&db).runs[0].clone();
    let worktree = run.worktree_path().unwrap().to_owned();
    git(&repo, &["worktree", "remove", "--force", &worktree]);
    let planning = advance(&db, &op, ReplanStatus::Planning);
    let withdrawn = advance(&db, &planning, ReplanStatus::Withdrawn);
    let other = dagq::domain::LeaseToken::new("another supervisor");
    assert_eq!(
        SqliteQueue::open(&db)
            .unwrap()
            .begin_restore(run.id(), &other, withdrawn.id, withdrawn.generation)
            .unwrap(),
        dagq::domain::replan_snapshot::RestoreStart::Owned
    );
    let scratch = PathBuf::from(format!("{worktree}.restoring"));
    fs::create_dir_all(&scratch).unwrap();
    fs::write(scratch.join("half-made.txt"), "the other's\n").unwrap();

    let (_reviewer, supervisor) =
        supervise_thread(&db, &repo, backend.clone(), Default::default(), &[]);
    finished(&db, &backend, supervisor);
    let held = detail(&db);
    for kind in ["replan_restored", "replan_restore_failed"] {
        assert!(payloads(&held, kind).is_empty(), "{kind}");
    }
    let resumes = payloads(&held, "resume_started").len();
    assert_eq!(operation(&db, op.id).status, ReplanStatus::Withdrawn);
    assert!(scratch.join("half-made.txt").exists());
    assert!(!Path::new(&worktree).exists());
    assert_eq!(
        SqliteQueue::open(&db)
            .unwrap()
            .run_lease(run.id())
            .unwrap()
            .unwrap()
            .token
            .as_str(),
        "another supervisor"
    );

    Connection::open(&db)
        .unwrap()
        .execute(
            "UPDATE run_leases SET heartbeat_at=0 WHERE run_id=?1",
            [run.id().as_str()],
        )
        .unwrap();
    let (_reviewer, supervisor) =
        supervise_thread(&db, &repo, backend.clone(), Default::default(), &[]);
    finished(&db, &backend, supervisor);
    let detail = detail(&db);
    assert_eq!(detail.task.status(), TaskStatus::Completed);
    assert_eq!(payloads(&detail, "resume_started").len(), resumes + 1);
    let restored = payloads(&detail, "replan_restored");
    assert_eq!(restored.len(), 1, "{restored:?}");
    assert_eq!(restored[0]["action"], "recreated");
    assert!(payloads(&detail, "replan_restore_failed").is_empty());
    let taken = payloads(&detail, "lease_acquired");
    assert!(
        taken.iter().any(|lease| lease["reason"] == "replan_restore"
            && lease["previous_token"] == "another supervisor"),
        "{taken:?}"
    );
    assert!(!scratch.exists());
    assert_eq!(payloads(&detail, "run_integrated").len(), 1);
    assert!(git_out(&repo, &["show", "main:seed.txt"]).contains("half"));
}

/// Acceptance: the park of a run nobody holds is one supervisor's from the
/// stop to the settle. While another supervisor holds the run (waiting for
/// its resume) for its park, with the lease `lease_for_park` gave it, a
/// supervisor leaves the run be: the operation does not move, no head is
/// pinned and no snapshot is made. Once that lease is stale the next pass
/// leases the run for its own park, replacing it, and settles the park
/// once; after the settle no park of it starts again, so no late holder
/// pins another head over the snapshot's.
#[test]
fn a_park_another_supervisor_holds_is_left_to_it_and_none_starts_after_the_settle() {
    let (dir, repo, db, backend) = headless_fixture(&[]);
    finish_then_go_on(dir.path());
    let reviewer = TestReviewer::new(&[verdict("concern", &["a judgment"], "decide")]);
    let options = supervise_options(4, true);
    let outcome = supervise_reviewed_with(&db, &repo, &backend, &reviewer, &options);
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    let run = detail(&db).runs[0].clone();
    // As a landing deferred for a conflict leaves it: waiting for a session.
    SqliteQueue::open(&db)
        .unwrap()
        .record_runtime_event(
            run.id(),
            dagq::domain::EventKind::IntegrationDeferred,
            json!({"code": "rebase_conflict", "reason": "a conflict", "status": "needs_session"}),
        )
        .unwrap();
    Connection::open(&db)
        .unwrap()
        .execute(
            "UPDATE task_runs SET status='needs_session' WHERE id=?1",
            [run.id().as_str()],
        )
        .unwrap();
    let op = pause(&db, run.id());
    let other = dagq::domain::LeaseToken::new("another supervisor");
    let lease_for_park = || {
        SqliteQueue::open(&db)
            .unwrap()
            .lease_for_park(run.id(), &other, op.id, op.generation)
            .unwrap()
    };
    assert!(lease_for_park().is_some());
    let pinned = format!("refs/dagq/replans/{}", op.id);
    let pin = || git_out(&repo, &["for-each-ref", "--format=%(objectname)", &pinned]);
    let holder = || {
        SqliteQueue::open(&db)
            .unwrap()
            .run_lease(run.id())
            .unwrap()
            .map(|lease| lease.token.as_str().to_owned())
    };

    let outcome = supervise_reviewed_with(&db, &repo, &backend, &reviewer, &options);
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    assert_eq!(operation(&db, op.id).status, ReplanStatus::PauseRequested);
    assert_eq!(pin(), "");
    assert!(!replans_dir(&db).exists() || fs::read_dir(replans_dir(&db)).unwrap().next().is_none());
    assert_eq!(holder().as_deref(), Some("another supervisor"));

    Connection::open(&db)
        .unwrap()
        .execute(
            "UPDATE run_leases SET heartbeat_at=0 WHERE run_id=?1",
            [run.id().as_str()],
        )
        .unwrap();
    let outcome = supervise_reviewed_with(&db, &repo, &backend, &reviewer, &options);
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    let parked = operation(&db, op.id);
    assert_eq!(parked.status, ReplanStatus::Parked, "{parked:?}");
    let head = manifest(&db, &parked).head;
    assert_eq!(pin(), head);
    let detail = detail(&db);
    assert_eq!(detail.runs[0].status(), RunStatus::NeedsSession);
    assert_eq!(payloads(&detail, "replan_parked").len(), 1);
    let taken: Vec<&Value> = payloads(&detail, "lease_acquired")
        .into_iter()
        .filter(|lease| {
            lease["reason"] == "replan_park" && lease["previous_token"] == "another supervisor"
        })
        .collect();
    assert_eq!(taken.len(), 1, "{taken:?}");
    assert_eq!(holder(), None);
    assert!(lease_for_park().is_none());
    assert_eq!(pin(), head);
}

/// A run parked with an answer the supervisor had sent as the session's
/// next turn (its ask closed as delivered) whose request still waited when
/// the park's fence came (which requests count as never run is
/// `domain::turn::answers_not_started`'s unit test): the park recorded it
/// (`carried_answers`). The answer is longer than a resolution request
/// carries, so the continuation's request names the file that holds it
/// whole.
struct ParkedWithAnAnswer {
    _dir: Fixture,
    repo: PathBuf,
    db: PathBuf,
    backend: Arc<TestWorkspace>,
    parked: ReplanOperation,
    run: RunId,
    run_dir: PathBuf,
    ask: dagq::domain::AskId,
    text: String,
}

fn parked_with_an_answer_not_started() -> ParkedWithAnAnswer {
    let (dir, repo, db, backend) = headless_fixture(&[]);
    set_turns(
        dir.path(),
        &format!(
            r#"case "$PROMPT" in
*'paused at the end of a turn'*) case "$PROMPT" in *'answer to ask '*'use notes.txt'*) printf 'notes\n' > notes.txt; git add notes.txt; git commit -qm notes; receipt "$(git rev-parse HEAD)"; say finished ;; *) fail "the answer was not carried" ;; esac ;;
*'dagq task 2,'*) ask "which file"; await_file "$RUN_DIR/go"; say asked ;;
*) {FINISH} ;;
esac"#
        ),
    );
    let backend = Arc::new(backend);
    let (_reviewer, supervisor) =
        supervise_thread_in(&db, &repo, backend.clone(), Default::default(), &[], 1);
    wait_until(&db, common::STEP_LIMIT, |queue| {
        queue
            .asks(AskQuery::default())
            .unwrap()
            .iter()
            .any(|ask| ask.kind == AskKind::WorkerQuestion)
    });
    let mut queue = SqliteQueue::open(&db).unwrap();
    let ask = queue.asks(AskQuery::default()).unwrap()[0].clone();
    let run = detail(&db).runs[0].clone();
    let op = pause(&db, run.id());
    wait_for_status(&db, op.id, ReplanStatus::Quiescing);
    // As the supervisor sends an answer between turns just before the
    // fence: its request waits in turns/, its ask closed as delivered.
    queue.answer(ask.id, "use notes.txt").unwrap();
    let run_dir = PathBuf::from(run.run_dir().unwrap());
    let turns = dagq::domain::turn::turns_dir(&run_dir);
    let seq = fs::read_dir(&turns)
        .unwrap()
        .filter_map(|entry| {
            let name = entry.ok()?.file_name();
            dagq::domain::turn::request_state(name.to_str()?).map(|(seq, _)| seq)
        })
        .max()
        .unwrap_or(0)
        + 1;
    let text = format!(
        "answer to ask {}: use notes.txt, {}",
        ask.id,
        "and why at length. ".repeat(600)
    );
    fs::write(
        dagq::domain::turn::request_path(&run_dir, seq),
        json!({"seq": seq, "what": dagq::domain::turn::answer_what(ask.id), "prompt": text})
            .to_string(),
    )
    .unwrap();
    queue.ask_delivered(ask.id, "background:test").unwrap();
    fs::write(run_dir.join("go"), "").unwrap();
    finished(&db, &backend, supervisor);
    let parked = operation(&db, op.id);
    assert_eq!(parked.status, ReplanStatus::Parked, "{parked:?}");
    let carried = payloads(&detail(&db), "replan_parked")[0]["carried_answers"].clone();
    assert_eq!(carried[0]["ask_id"], json!(ask.id), "{carried}");
    assert_eq!(carried[0]["seq"], json!(seq));
    ParkedWithAnAnswer {
        _dir: dir,
        repo,
        db,
        backend,
        parked,
        run: run.id().clone(),
        run_dir,
        ask: ask.id,
        text,
    }
}

/// The continuation of `at`'s run, resumed as attempt `attempt`, gave its
/// session the answer: the request carries its start and names the file
/// that holds it whole, which does; the resumed session worked with it and
/// the run landed.
fn assert_the_answer_reached(at: &ParkedWithAnAnswer, attempt: u64) {
    let detail = detail(&at.db);
    assert_eq!(detail.task.status(), TaskStatus::Completed);
    let request = fs::read_to_string(at.run_dir.join(format!("resume-{attempt}.txt"))).unwrap();
    let file = at.run_dir.join(format!("resume-{attempt}-answers.txt"));
    assert!(
        request.contains(&format!("answer to ask {}: use notes.txt", at.ask)),
        "{request}"
    );
    assert!(request.contains(&file.display().to_string()), "{request}");
    assert_eq!(fs::read_to_string(&file).unwrap(), at.text);
    assert!(git_out(&at.repo, &["ls-tree", "--name-only", "main"]).contains("notes.txt"));
}

/// Acceptance: an answer whose turn never started at the park's fence
/// reaches the continuation's session whole: once the operation ends in a
/// continuation, its resume gives the answer to the resumed session (the
/// answer past the request's budget in the file the request names), which
/// works with it and lands.
#[test]
fn an_answer_sent_but_untaken_at_the_park_reaches_the_continuations_session() {
    let at = parked_with_an_answer_not_started();
    advance(&at.db, &at.parked, ReplanStatus::ContinuingAfterPark);
    let (_reviewer, supervisor) = supervise_thread(
        &at.db,
        &at.repo,
        at.backend.clone(),
        Default::default(),
        &[],
    );
    finished(&at.db, &at.backend, supervisor);
    assert_the_answer_reached(&at, 1);
}

/// Supervise on a thread with one review that passes, a pause `pauses`
/// gives the test, and heartbeats `heartbeat` apart.
fn supervise_paused(
    db: &Path,
    repo: &Path,
    backend: Arc<TestWorkspace>,
    pauses: runtime::PausesPort,
    heartbeat: Duration,
) -> thread::JoinHandle<Result<Value>> {
    let reviewer = Arc::new(TestReviewer::new(&[verdict("pass", &[], "fine")]));
    let options = SuperviseOptions {
        pauses: Some(pauses),
        heartbeat_interval: heartbeat,
        ..supervise_options(4, true)
    };
    let (db, repo) = (db.to_owned(), repo.to_owned());
    thread::spawn(move || {
        runtime::supervise_with_reviewer(
            &db,
            &repo,
            &*backend,
            &claude_stub(&db),
            &*reviewer,
            Path::new(env!("CARGO_BIN_EXE_dagq")),
            &options,
        )
    })
}

/// Acceptance: a supervisor that stood still in a park (paused before it
/// looks at the run's writer, its operation past its limit) and comes back
/// after another supervisor took the run over, parked it and resumed it
/// for the continuation stops and closes nothing: its park is no longer its
/// own, so the resumed session runs on and the run lands once.
#[test]
fn a_park_resumed_after_another_took_the_run_over_stops_none_of_its_sessions() {
    let (dir, repo, db, backend) = headless_fixture(&[]);
    set_turns(
        dir.path(),
        &format!(
            r#"case "$PROMPT" in
*'paused at the end of a turn'*) await_file "$RUN_DIR/go-on"; commit "after the park"; receipt "$(git rev-parse HEAD)"; say finished ;;
*) {FINISH} ;;
esac"#
        ),
    );
    let reviewer = TestReviewer::new(&[verdict("concern", &["a judgment"], "decide")]);
    let options = supervise_options(4, true);
    let outcome = supervise_reviewed_with(&db, &repo, &backend, &reviewer, &options);
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    let run = detail(&db).runs[0].clone();
    // Waiting for a session, as a landing deferred for a conflict leaves it.
    SqliteQueue::open(&db)
        .unwrap()
        .record_runtime_event(
            run.id(),
            dagq::domain::EventKind::IntegrationDeferred,
            json!({"code": "rebase_conflict", "reason": "a conflict", "status": "needs_session"}),
        )
        .unwrap();
    let conn = Connection::open(&db).unwrap();
    conn.execute(
        "UPDATE task_runs SET status='needs_session' WHERE id=?1",
        [run.id().as_str()],
    )
    .unwrap();
    // The operation waits for the writer, long past the turn's limit.
    let op = advance(&db, &pause(&db, run.id()), ReplanStatus::Quiescing);
    conn.execute(
        "UPDATE replan_operations SET updated_at=0 WHERE id=?1",
        [op.id.as_i64()],
    )
    .unwrap();
    let backend = Arc::new(backend);
    let (paused, go) = (dir.path().join("park-paused"), dir.path().join("park-go"));
    let stale = supervise_paused(
        &db,
        &repo,
        backend.clone(),
        pause_once(runtime::PausePoint::ParkLook, paused.clone(), go.clone()),
        Duration::from_secs(3600),
    );
    wait_for_file(&paused);
    // It stood still past its lease: another supervisor takes the park over
    // and settles it, and the continuation resumes the run there.
    conn.execute(
        "UPDATE run_leases SET heartbeat_at=0 WHERE run_id=?1",
        [run.id().as_str()],
    )
    .unwrap();
    let outcome = supervise_reviewed_with(&db, &repo, &backend, &reviewer, &options);
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    let parked = operation(&db, op.id);
    assert_eq!(parked.status, ReplanStatus::Parked, "{parked:?}");
    advance(&db, &parked, ReplanStatus::ContinuingAfterPark);
    let (_reviewer, resumer) =
        supervise_thread(&db, &repo, backend.clone(), Default::default(), &[]);
    let started_after_park = |queue: &mut SqliteQueue| {
        let detail = queue.show(TASK).unwrap();
        let kinds = event_kinds(&detail);
        kinds
            .iter()
            .rposition(|kind| *kind == "replan_parked")
            .is_some_and(|park| kinds[park..].contains(&"turn_started"))
    };
    wait_until(&db, common::STEP_LIMIT, started_after_park);
    // The one that stood still comes back to its park; the resumed session
    // still runs, so only its thread is waited for.
    fs::write(&go, "").unwrap();
    let outcome = joined(stale, "the supervisor that stood still to return").unwrap();
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    let run_dir = PathBuf::from(run.run_dir().unwrap());
    fs::write(run_dir.join("go-on"), "").unwrap();
    finished(&db, &backend, resumer);
    let detail = detail(&db);
    assert_eq!(detail.task.status(), TaskStatus::Completed);
    assert_eq!(payloads(&detail, "replan_parked").len(), 1);
    let resumed = payloads(&detail, "resume_finished");
    assert_eq!(resumed.len(), 1, "{resumed:?}");
    assert_eq!(resumed[0]["outcome"], "resolved", "{resumed:?}");
    // No session was stopped or closed for the park after the resume began.
    let events = &detail.events;
    let resumed_at = events
        .iter()
        .position(|event| event.kind == EventKind::ResumeStarted)
        .unwrap();
    assert!(
        events[resumed_at..].iter().all(|event| {
            !(event.kind == EventKind::WrapperStopped && event.payload["route"] == "replan")
                && !(event.kind == EventKind::WorkspaceClosed && event.payload["by"] == "replan")
        }),
        "{:?}",
        event_kinds(&detail)
    );
    assert_eq!(payloads(&detail, "run_integrated").len(), 1);
}

/// Acceptance: an answer whose delivery meets a replanning's fence that
/// came after the delivery's own look at the guard (the supervisor paused
/// between the two) is put off, not failed: no `ask_delivery_failed`
/// keeps it from the resumed session, the ask stays open, and once the
/// operation ends in a continuation the resumed session's watch delivers
/// it, once, and the run lands with it.
#[test]
fn an_answer_whose_send_meets_the_fence_is_delivered_after_the_continuation() {
    let (dir, repo, db, backend) = headless_fixture(&[]);
    set_turns(
        dir.path(),
        &format!(
            r#"case "$PROMPT" in
*'answer to ask '*'use notes.txt'*) printf 'notes\n' > notes.txt; git add notes.txt; git commit -qm notes; receipt "$(git rev-parse HEAD)"; say finished ;;
*'paused at the end of a turn'*) say "waiting for the answer" ;;
*'dagq task 2,'*) ask "which file"; say asked ;;
*) {FINISH} ;;
esac"#
        ),
    );
    let backend = Arc::new(backend);
    let (paused, go) = (dir.path().join("send-paused"), dir.path().join("send-go"));
    let supervisor = supervise_paused(
        &db,
        &repo,
        backend.clone(),
        pause_once(runtime::PausePoint::AnswerSend, paused.clone(), go.clone()),
        Duration::from_millis(200),
    );
    wait_until(&db, common::STEP_LIMIT, |queue| {
        queue
            .asks(AskQuery::default())
            .unwrap()
            .iter()
            .any(|ask| ask.kind == AskKind::WorkerQuestion)
    });
    let mut queue = SqliteQueue::open(&db).unwrap();
    let ask = queue.asks(AskQuery::default()).unwrap()[0].clone();
    queue.answer(ask.id, "use notes.txt").unwrap();
    // The supervisor looked at the guard and stands still before the send:
    // the operation's fence comes now.
    wait_for_file(&paused);
    let run = detail(&db).runs[0].clone();
    let op = pause(&db, run.id());
    fs::write(&go, "").unwrap();
    finished(&db, &backend, supervisor);
    let parked = operation(&db, op.id);
    assert_eq!(parked.status, ReplanStatus::Parked, "{parked:?}");
    let held = detail(&db);
    for kind in ["ask_delivery_failed", "ask_delivered"] {
        assert!(payloads(&held, kind).is_empty(), "{kind}");
    }
    assert!(queue.read_ask(ask.id).unwrap().closed_at.is_none());

    advance(&db, &parked, ReplanStatus::ContinuingAfterPark);
    let (_reviewer, supervisor) =
        supervise_thread(&db, &repo, backend.clone(), Default::default(), &[]);
    finished(&db, &backend, supervisor);
    let detail = detail(&db);
    assert_eq!(detail.task.status(), TaskStatus::Completed);
    let delivered = payloads(&detail, "ask_delivered");
    assert_eq!(delivered.len(), 1, "{delivered:?}");
    assert!(payloads(&detail, "ask_delivery_failed").is_empty());
    assert!(git_out(&repo, &["ls-tree", "--name-only", "main"]).contains("notes.txt"));
}

/// Acceptance: an answer a park carried and the continuation was to give
/// stays owed when the run is parked again before the continuation's first
/// turn ran: the continuation's resume set the session's earlier requests
/// aside (the answer's among them) and a second operation's fence came
/// before its wrapper's first turn. The second park keeps the answer
/// (`carried_answers`), and the second continuation's resumed session gets
/// it and lands.
#[test]
fn an_answer_a_park_carried_stays_owed_when_the_continuation_is_parked_before_its_turn() {
    let at = parked_with_an_answer_not_started();
    advance(&at.db, &at.parked, ReplanStatus::ContinuingAfterPark);
    let (paused, go) = (
        at.run_dir.join("resume-paused"),
        at.run_dir.join("resume-go"),
    );
    let resumer = supervise_paused(
        &at.db,
        &at.repo,
        at.backend.clone(),
        pause_once(
            runtime::PausePoint::ResumeLaunch,
            paused.clone(),
            go.clone(),
        ),
        Duration::from_millis(200),
    );
    wait_for_file(&paused);
    let second = pause(&at.db, &at.run);
    fs::write(&go, "").unwrap();
    finished(&at.db, &at.backend, resumer);
    let parked = operation(&at.db, second.id);
    assert_eq!(parked.status, ReplanStatus::Parked, "{parked:?}");
    let detail = detail(&at.db);
    let parks = payloads(&detail, "replan_parked");
    assert_eq!(parks.len(), 2, "{parks:?}");
    assert_eq!(
        parks[1]["carried_answers"][0]["ask_id"],
        json!(at.ask),
        "{parks:?}"
    );
    // The first continuation's resume ran no turn.
    let kinds = event_kinds(&detail);
    let first_park = kinds.iter().position(|k| *k == "replan_parked").unwrap();
    assert!(!kinds[first_park..].contains(&"turn_started"), "{kinds:?}");

    advance(&at.db, &parked, ReplanStatus::ContinuingAfterPark);
    let (_reviewer, supervisor) = supervise_thread(
        &at.db,
        &at.repo,
        at.backend.clone(),
        Default::default(),
        &[],
    );
    finished(&at.db, &at.backend, supervisor);
    let detail = self::detail(&at.db);
    assert_eq!(detail.task.status(), TaskStatus::Completed);
    let last = payloads(&detail, "resume_started").last().unwrap()["attempt"]
        .as_u64()
        .unwrap();
    assert_the_answer_reached(&at, last);
}

/// A run whose review a supervisor that is no longer there had begun
/// (validated, `review_started` recorded, no job of it left), its task
/// paused for a replanning since: the supervisor that takes it over, by
/// adopting its stale lease or (`handoff`) as the process an exec handed it
/// to under the same token, starts no program and no agent's job of the
/// review, and parks the run.
fn a_review_taken_over_past_the_fence_starts_nothing(handoff: bool) {
    let (dir, repo, db, backend) = headless_fixture(&[]);
    set_turns(dir.path(), FINISH);
    let token = "gone-supervisor";
    let run = start_run_under_dead_supervisor(&repo, &db, &backend, token);
    let receipt = PathBuf::from(run.receipt_path().unwrap());
    wait_until(&db, common::STEP_LIMIT, |queue| {
        receipt.is_file() && payloads(&queue.show(TASK).unwrap(), "turn_finished").len() == 1
    });
    let head = git_out(
        Path::new(run.worktree_path().unwrap()),
        &["rev-parse", "HEAD"],
    );
    let conn = Connection::open(&db).unwrap();
    conn.execute(
        "UPDATE task_runs SET status='awaiting_integration', result_commit=?2 WHERE id=?1",
        rusqlite::params![run.id(), head],
    )
    .unwrap();
    let mut queue = SqliteQueue::open(&db).unwrap();
    for (kind, payload) in [
        (
            dagq::domain::EventKind::ValidationFinished,
            json!({"status": "awaiting_integration"}),
        ),
        (
            dagq::domain::EventKind::ReviewStarted,
            json!({"attempt": 1}),
        ),
    ] {
        queue.record_runtime_event(run.id(), kind, payload).unwrap();
    }
    if handoff {
        queue
            .register_supervisor(
                &dagq::domain::LeaseToken::new(token),
                std::process::id(),
                4,
                "0.0.1",
            )
            .unwrap();
    } else {
        conn.execute(
            "UPDATE run_leases SET heartbeat_at=0 WHERE run_id=?1",
            [run.id().as_str()],
        )
        .unwrap();
    }
    let op = pause(&db, run.id());
    let reviewer = TestReviewer::new(&[verdict("pass", &[], "unused")]);
    let options = SuperviseOptions {
        handoff_token: handoff.then(|| dagq::domain::LeaseToken::new(token)),
        ..supervise_options(4, true)
    };
    let outcome = supervise_reviewed_with(&db, &repo, &backend, &reviewer, &options);
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    let parked = operation(&db, op.id);
    assert_eq!(parked.status, ReplanStatus::Parked, "{parked:?}");
    let detail = detail(&db);
    assert_eq!(detail.runs[0].status(), RunStatus::NeedsSession);
    for kind in [
        "review_programs_started",
        "review_job_started",
        "review_finished",
    ] {
        assert!(payloads(&detail, kind).is_empty(), "{kind}");
    }
    assert!(reviewer.prompts().is_empty());
}

/// Acceptance: an adopter of a run whose review its gone supervisor had
/// begun starts none of it past a replanning's fence, and parks the run
/// ([`a_review_taken_over_past_the_fence_starts_nothing`]).
#[test]
fn an_adopted_review_past_the_fence_starts_no_job_and_the_run_is_parked() {
    a_review_taken_over_past_the_fence_starts_nothing(false);
}

/// Acceptance: the process an exec hands a run under review to starts none
/// of the review past a replanning's fence either, and parks the run
/// ([`a_review_taken_over_past_the_fence_starts_nothing`]).
#[test]
fn a_review_handed_off_past_the_fence_starts_no_job_and_the_run_is_parked() {
    a_review_taken_over_past_the_fence_starts_nothing(true);
}

/// The turns of task 1 in its container (`turn.sh` of
/// [`ContainerQueue`]): the first waits for the test's `go` in the run
/// directory, then commits, and leaves, unless `clean`, a staged file, an
/// unstaged change, an untracked file and an untracked executable script,
/// and a receipt for its head. The next turn (the continuation's resume)
/// checks that the work it left is in the clone, commits all of it and
/// writes a receipt, or says what is missing.
fn container_turns(clean: bool) -> String {
    let dirty = if clean {
        ""
    } else {
        "printf 'half\\n' >> tracked.txt; printf 'staged\\n' > staged.txt; git add staged.txt; printf 'notes\\n' > notes.txt; printf '#!/bin/sh\\necho ran\\n' > run.sh; chmod +x run.sh;"
    };
    let check = if clean {
        "grep -q one tracked.txt"
    } else {
        "test -f notes.txt && test -x run.sh && grep -q half tracked.txt && git diff --cached --name-only | grep -q staged.txt"
    };
    let go = common::await_file("\"$RUN_DIR/go\"");
    format!(
        r#"commit_all() {{ git add -A; git diff --cached --quiet || git -c user.name=worker -c user.email=worker@example.invalid commit -q -m "$1"; }}
receipt() {{ printf '{{"run_id":"%s","result":"succeeded","commit":"%s","tests":{{"status":"passed","evidence_or_reason":"ran"}},"e2e":{{"status":"not_applicable","evidence_or_reason":"stub"}},"subagent_review":{{"status":"not_applicable","evidence_or_reason":"stub"}},"summary":"in the container"}}' "$DAGQ_RUN_ID" "$(git rev-parse HEAD)" > "$RUN_DIR/receipt.json"; }}
echo turn >> "$RUN_DIR/agent-turns"
if [ ! -f "$RUN_DIR/worked" ]; then
  : > "$RUN_DIR/worked"
  {go}
  printf 'one\n' > tracked.txt; commit_all first
  {dirty}
  receipt; say paused
elif {check}; then
  commit_all "the rest"; receipt; say finished
else
  say "the saved work is not here"; : > "$RUN_DIR/work-missing"
fi
"#
    )
}

/// Park task 1's run of `queue` while its first turn runs in its container,
/// as [`park_during_a_turn`] parks a run on the host: the turn ends, its
/// executor writes the run's bundle, work and state, and the supervisor
/// saves the snapshot from them and parks the run. The operation as parked,
/// and the run's clone.
fn park_in_a_container(queue: &ContainerQueue) -> (ReplanOperation, PathBuf) {
    let supervisor = queue.supervise(TestReviewer::new(&[]));
    wait_until(&queue.db, common::STEP_LIMIT, |q| {
        !payloads(&q.show(TaskId::new(1)).unwrap(), "turn_started").is_empty()
    });
    let run = queue.detail().runs[0].clone();
    assert!(run.in_container());
    let op = pause(&queue.db, run.id());
    wait_for_status(&queue.db, op.id, ReplanStatus::Quiescing);
    fs::write(Path::new(run.run_dir().unwrap()).join("go"), "").unwrap();
    finished(&queue.db, &queue.backend, supervisor);
    let op = operation(&queue.db, op.id);
    assert_eq!(op.status, ReplanStatus::Parked, "{op:?}");
    (op, PathBuf::from(run.worktree_path().unwrap()))
}

/// The dirty work [`container_turns`] leaves is in the snapshot of `op`,
/// saved from what the executor wrote: the head the clone's (pinned by the
/// operation's ref in the repository), the diffs and the untracked files
/// with their execute bits.
fn assert_dirty_work_saved(queue: &ContainerQueue, op: &ReplanOperation, clone: &Path) {
    let manifest = manifest(&queue.db, op);
    assert!(manifest.dirty);
    assert_eq!(manifest.worktree, clone.to_string_lossy());
    assert_eq!(manifest.head, git_out(clone, &["rev-parse", "HEAD"]));
    assert_eq!(
        git_out(
            &queue.repo,
            &["rev-parse", &format!("refs/dagq/replans/{}", op.id)]
        ),
        manifest.head
    );
    assert!(saved(&queue.db, op, "index", None).contains("staged"));
    assert!(saved(&queue.db, op, "worktree", None).contains("half"));
    assert_eq!(
        saved(&queue.db, op, "untracked", Some("notes.txt")),
        "notes\n"
    );
    let script = manifest
        .files
        .iter()
        .find(|file| file.source == "run.sh")
        .unwrap();
    assert!(script.executable, "{script:?}");
    // The clone keeps its work.
    assert!(clone.join("notes.txt").exists());
}

/// The run of `queue` landed the dirty work [`container_turns`] left, the
/// script's execute bit included, the resumed turn's agent having found it
/// in the clone; the executor's answer is recorded once as `action`.
fn assert_landed_restored(queue: &ContainerQueue, action: &str) {
    let detail = queue.detail();
    let run_dir = PathBuf::from(detail.runs[0].run_dir().unwrap());
    assert!(!run_dir.join("work-missing").exists());
    assert_eq!(detail.task.status(), TaskStatus::Completed);
    assert_eq!(detail.runs[0].status(), RunStatus::Integrated);
    let restored = payloads(&detail, "replan_restored");
    assert_eq!(restored.len(), 1, "{restored:?}");
    assert_eq!(restored[0]["action"], action, "{restored:?}");
    assert_eq!(restored[0]["by"], "executor", "{restored:?}");
    assert!(payloads(&detail, "replan_restore_failed").is_empty());
    let tree = git_out(&queue.repo, &["ls-tree", "-r", "main"]);
    for (mode, path) in [
        ("100644", "notes.txt"),
        ("100755", "run.sh"),
        ("100644", "staged.txt"),
    ] {
        assert!(
            tree.lines()
                .any(|line| line.starts_with(mode) && line.ends_with(path)),
            "{mode} {path} in {tree}"
        );
    }
    assert_eq!(
        git_out(&queue.repo, &["show", "main:tracked.txt"]),
        "one\nhalf"
    );
}

/// Acceptance (b) of the container's clone: a run in a container parked
/// with dirty work (a commit, a staged file, an unstaged change, an
/// untracked file and an untracked executable script) is saved from what
/// its executor wrote at the turn's end, with no git of the control side in
/// its clone (whose every git refuses a clone's path). Once the clone is
/// gone and the operation ends in a continuation, the control side hands
/// the restore to the executor and makes the clone again at the base; the
/// executor puts the head, the index, the worktree and the untracked files
/// back before the resumed turn's agent runs, which finds them and lands
/// its work, and its answer is recorded from the resume's slot.
#[test]
fn a_container_runs_dirty_work_is_saved_from_its_executor_and_restored_into_a_clone_made_again() {
    let _test = crate::common::test();
    let queue = ContainerQueue::new(Some(&container_turns(false)));
    let (op, clone) = park_in_a_container(&queue);
    assert_dirty_work_saved(&queue, &op, &clone);
    fs::remove_dir_all(&clone).unwrap();
    advance(&queue.db, &op, ReplanStatus::ContinuingAfterPark);
    let supervisor = queue.supervise(TestReviewer::new(&[verdict("pass", &[], "fine")]));
    finished(&queue.db, &queue.backend, supervisor);
    assert_landed_restored(&queue, "recreated");
}

/// Acceptance (b), a withdrawal: a run in a container parked with dirty
/// work whose clone is left in place, once the operation is withdrawn, has
/// its clone compared as it is with the snapshot by its executor before
/// the resumed turn's agent runs: found intact and recorded so, it resumes
/// there and lands the work.
#[test]
fn a_withdrawn_container_runs_clone_left_in_place_is_checked_by_its_executor_and_lands() {
    let _test = crate::common::test();
    let queue = ContainerQueue::new(Some(&container_turns(false)));
    let (op, clone) = park_in_a_container(&queue);
    assert_dirty_work_saved(&queue, &op, &clone);
    let planning = advance(&queue.db, &op, ReplanStatus::Planning);
    advance(&queue.db, &planning, ReplanStatus::Withdrawn);
    let supervisor = queue.supervise(TestReviewer::new(&[verdict("pass", &[], "fine")]));
    finished(&queue.db, &queue.backend, supervisor);
    assert_landed_restored(&queue, "intact");
}

/// Acceptance (b), a clean save, and the clone that changed since: a run
/// in a container whose work is all committed is saved clean from its
/// executor's files. A file put in its clone after the park, which the
/// executor's last written work does not show, is found by the executor
/// before the continuation's agent runs: the clone is not touched, no
/// agent runs, and the refusal is recorded from the resume's slot as
/// `replan_restore_failed` with the operation back to `blocked`, not as the
/// provider's wall; the run rests out of its slot and does not land. Once
/// the file is taken out and the operation continues again, the restore is
/// a new attempt the earlier refusal does not end: the executor compares
/// the clone again, finds it intact, and the run lands.
#[test]
fn a_container_runs_clone_changed_since_its_clean_snapshot_blocks_the_continuation_until_mended() {
    let _test = crate::common::test();
    let queue = ContainerQueue::new(Some(&container_turns(true)));
    let (op, clone) = park_in_a_container(&queue);
    assert!(!manifest(&queue.db, &op).dirty);
    fs::write(clone.join("stray.txt"), "stray\n").unwrap();
    advance(&queue.db, &op, ReplanStatus::ContinuingAfterPark);
    let supervisor = queue.supervise(TestReviewer::new(&[]));
    finished(&queue.db, &queue.backend, supervisor);
    let blocked = operation(&queue.db, op.id);
    assert_eq!(blocked.status, ReplanStatus::Blocked, "{blocked:?}");
    let detail = queue.detail();
    let failed = payloads(&detail, "replan_restore_failed");
    assert_eq!(failed.len(), 1, "{failed:?}");
    assert!(
        failed[0]["reason"]
            .as_str()
            .is_some_and(|why| why.contains("changed since the park")),
        "{failed:?}"
    );
    assert!(payloads(&detail, "replan_restored").is_empty());
    assert_ne!(detail.task.status(), TaskStatus::Completed);
    assert_ne!(detail.runs[0].status(), RunStatus::Integrated);
    // Only the first turn's agent ran, and the clone is as it was left.
    let run_dir = PathBuf::from(detail.runs[0].run_dir().unwrap());
    assert_eq!(
        fs::read_to_string(run_dir.join("agent-turns")).unwrap(),
        "turn\n"
    );
    assert_eq!(
        fs::read_to_string(clone.join("stray.txt")).unwrap(),
        "stray\n"
    );
    // No provider's wall: no ask, no move to another provider.
    let kinds = event_kinds(&detail);
    assert!(
        !kinds.iter().any(|kind| kind.starts_with("provider")),
        "{kinds:?}"
    );
    assert!(
        SqliteQueue::open(&queue.db)
            .unwrap()
            .asks(AskQuery::default())
            .unwrap()
            .is_empty()
    );
    // Mended, and continued again: a new attempt.
    fs::remove_file(clone.join("stray.txt")).unwrap();
    advance(&queue.db, &blocked, ReplanStatus::ContinuingAfterPark);
    let supervisor = queue.supervise(TestReviewer::new(&[verdict("pass", &[], "fine")]));
    finished(&queue.db, &queue.backend, supervisor);
    let detail = queue.detail();
    assert_eq!(detail.task.status(), TaskStatus::Completed);
    assert_eq!(detail.runs[0].status(), RunStatus::Integrated);
    assert_eq!(payloads(&detail, "replan_restore_failed").len(), 1);
    let restored = payloads(&detail, "replan_restored");
    assert_eq!(restored.len(), 1, "{restored:?}");
    assert_eq!(restored[0]["action"], "intact", "{restored:?}");
    assert_eq!(restored[0]["by"], "executor", "{restored:?}");
    assert_eq!(
        fs::read_to_string(run_dir.join("agent-turns")).unwrap(),
        "turn\nturn\n"
    );
}

/// A run in a container out of resumes whose clone changed since its park:
/// no turn of its clone follows, in which alone its executor sees the clone
/// as it is now, while the work it wrote before the park shows nothing
/// changed since. Once the operation ends in a continuation the restore
/// fails (`replan_restore_failed`, the operation back to `blocked`) rather
/// than taking the clone for intact: the run is not handed on to the
/// recovery job, no agent runs, and the clone is left as it is.
#[test]
fn a_container_run_out_of_resumes_is_not_handed_on_over_a_clone_no_turn_checks() {
    let _test = crate::common::test();
    let queue = ContainerQueue::new(Some(&container_turns(true)));
    let (op, clone) = park_in_a_container(&queue);
    let run = queue.detail().runs[0].clone();
    assert_eq!(run.status(), RunStatus::NeedsSession);
    let store = SqliteQueue::open(&queue.db).unwrap();
    for attempt in 1..=dagq::domain::MAX_RESUME_ATTEMPTS {
        for (kind, payload) in [
            (EventKind::ResumeStarted, json!({"attempt": attempt})),
            (
                EventKind::ResumeFinished,
                json!({"attempt": attempt, "outcome": "unresolved"}),
            ),
        ] {
            store.record_runtime_event(run.id(), kind, payload).unwrap();
        }
    }
    fs::write(clone.join("stray.txt"), "stray\n").unwrap();
    advance(&queue.db, &op, ReplanStatus::ContinuingAfterPark);
    let supervisor = queue.supervise(TestReviewer::new(&[]));
    finished(&queue.db, &queue.backend, supervisor);
    let blocked = operation(&queue.db, op.id);
    assert_eq!(blocked.status, ReplanStatus::Blocked, "{blocked:?}");
    let detail = queue.detail();
    let failed = payloads(&detail, "replan_restore_failed");
    assert_eq!(failed.len(), 1, "{failed:?}");
    assert!(
        failed[0]["reason"]
            .as_str()
            .is_some_and(|why| why.contains("resumes are used up")),
        "{failed:?}"
    );
    assert!(payloads(&detail, "replan_restored").is_empty());
    assert!(payloads(&detail, "recovery_requested").is_empty());
    assert_eq!(detail.runs[0].status(), RunStatus::NeedsSession);
    let run_dir = PathBuf::from(detail.runs[0].run_dir().unwrap());
    assert_eq!(
        fs::read_to_string(run_dir.join("agent-turns")).unwrap(),
        "turn\n"
    );
    assert_eq!(
        fs::read_to_string(clone.join("stray.txt")).unwrap(),
        "stray\n"
    );
}

/// Restart and adopt: a supervisor that handed the restore of a container
/// run whose clone was gone (the request written, the clone made again)
/// and stood still before it gave the restore's lease back is taken over
/// by another, which adopts the run's stale lease and goes on from what the
/// run directory and the queue hold: the snapshot is not saved again, the
/// clone not made again over the one made, and the executor fills it once;
/// the restore is recorded once and the run lands once. The first
/// supervisor, back, finds the run no longer its own.
#[test]
fn a_container_restore_handed_before_a_supervisor_stood_still_is_taken_over_and_done_once() {
    let _test = crate::common::test();
    let queue = ContainerQueue::new(Some(&container_turns(false)));
    let (op, clone) = park_in_a_container(&queue);
    fs::remove_dir_all(&clone).unwrap();
    advance(&queue.db, &op, ReplanStatus::ContinuingAfterPark);
    let run_dir = PathBuf::from(queue.detail().runs[0].run_dir().unwrap());
    let (paused, go) = (run_dir.join("resume-paused"), run_dir.join("resume-go"));
    let pauses = pause_once(
        runtime::PausePoint::RestoreHanded,
        paused.clone(),
        go.clone(),
    );
    let first = queue.supervise_with(
        TestReviewer::new(&[verdict("pass", &[], "fine")]),
        |options| {
            options.pauses = Some(pauses);
            options.heartbeat_interval = Duration::from_secs(3600);
        },
    );
    wait_for_file(&paused);
    // Handed: the request is in the run directory and the clone made again
    // at the base, and nothing is recorded yet.
    assert!(
        run_dir
            .join(dagq::domain::clone_work::RUN_RESTORE_FILE)
            .is_file()
    );
    assert_eq!(
        git_out(&clone, &["rev-parse", "HEAD"]),
        queue.detail().runs[0].base_commit().as_str()
    );
    assert!(payloads(&queue.detail(), "replan_restored").is_empty());
    // It stood still past its lease: another supervisor takes the run over.
    Connection::open(&queue.db)
        .unwrap()
        .execute(
            "UPDATE run_leases SET heartbeat_at=0 WHERE run_id=?1",
            [queue.detail().runs[0].id().as_str()],
        )
        .unwrap();
    let second = queue.supervise(TestReviewer::new(&[verdict("pass", &[], "fine")]));
    finished(&queue.db, &queue.backend, second);
    fs::write(&go, "").unwrap();
    let _ = joined(first, "the supervisor that stood still to return");
    assert_landed_restored(&queue, "recreated");
    let snapshots: Vec<_> = fs::read_dir(replans_dir(&queue.db))
        .unwrap()
        .map(|entry| entry.unwrap().file_name())
        .collect();
    assert_eq!(snapshots.len(), 1, "{snapshots:?}");
    assert_eq!(payloads(&queue.detail(), "replan_parked").len(), 1);
}
