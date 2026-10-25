//! Runtime tests: the claim of a child of an applied replacement
//! (ADR-t1521-2 decision 5) hands over only the part of the replaced run's
//! saved work its plan named, from the operation's snapshot, and the
//! child's run then goes through its own receipt and validation like any
//! other (an untracked executable keeps its mode), while the task that
//! waits for the child is not claimed; a snapshot that does not read back as saved fails the handover with the
//! snapshot and its ref kept and nothing landed, and a person's release
//! that starts the child afresh hands nothing over again. The applied operation is
//! written as the apply leaves it (its own tests are `replan_planning`'s).
//! The handover is no gate of its own: the child's work lands only through
//! its run's receipt, validation, review and landing.
use crate::common::cli::invoke_with;
use crate::runtime_support;

use dagq::domain::replan_snapshot::{SavedFile, SavedKind, SnapshotManifest, pinned_ref};
use dagq::domain::task_hold::HoldState;
use dagq::domain::{ClaimOutcome, EventKind, ReplanId};
use dagq::infrastructure::location::replans_dir;
use runtime_support::*;

/// The staged diff the snapshot saved.
const DIFF: &[u8] = b"diff --git a/notes.txt b/notes.txt\n";
/// The untracked executable script the snapshot saved.
const SCRIPT: &[u8] = b"#!/bin/sh\necho ran\n";

/// The output of `git args` in `repo`, which must succeed.
fn git_out(repo: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(args)
        .bounded_output()
        .unwrap();
    assert!(
        output.status.success(),
        "{args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).trim().to_owned()
}

/// What the test set up: the replaced task 1 and its run, the operation,
/// the child (2) that takes `carry` of its saved work, the task (3) that
/// waits for the child, and the saved head.
struct Applied {
    run: RunId,
    op: ReplanId,
    child: TaskId,
    successor: TaskId,
    head: String,
}

/// Task 1 replaced by child 2, which takes `carry` of the parked run's
/// work saved in snapshot `snap-1`, as the apply leaves it; task 3 waits
/// for the child. `corrupt` leaves the snapshot's diff not as saved.
fn applied(db: &Path, repo: &Path, carry: &str, corrupt: bool) -> Applied {
    let mut queue = SqliteQueue::open(db).unwrap();
    let main = git_out(repo, &["rev-parse", "HEAD"]);
    let ClaimOutcome::Claimed { run } = queue.claim(&sha(&main)).unwrap() else {
        panic!("claim")
    };
    assert_eq!(run.task_id(), TaskId::new(1));
    let run = run.id().clone();
    let child = add_ready_task(&mut queue, "child", &[]);
    let successor = add_ready_task(&mut queue, "successor", &[child]);
    let conditions = r#"{"title":"t","description":"d","acceptance":[],"paths":[],"verify":[],"evidence":[],"predecessors":[],"successors":[],"goal":null}"#;
    let connection = Connection::open(db).unwrap();
    connection
        .execute_batch(&format!(
            "UPDATE task_runs SET status='replaced' WHERE id='{run}';
             UPDATE tasks SET status='replaced' WHERE id=1;
             INSERT INTO replan_operations(task_id, run_id, origin, status, generation, reason,
               conditions, task_version, diagnosis, snapshot_id, requested_by,
               requested_by_id, created_at, updated_at, closed_at)
             VALUES (1, '{run}', 'manual', 'applied', 2, 'replaced in the test', '{conditions}',
               'v', '{{\"evidence\":[]}}', 'snap-1', 'inbox', 'inbox', unixepoch(), unixepoch(),
               unixepoch());"
        ))
        .unwrap();
    let op = ReplanId::new(connection.last_insert_rowid());
    connection
        .execute(
            "INSERT INTO replan_replacements(operation_id, from_task_id, to_task_id, carry,
               carry_order) VALUES (?1, 1, ?2, ?3, 1)",
            rusqlite::params![op, child, carry],
        )
        .unwrap();
    let head = git_out(
        repo,
        &[
            "commit-tree",
            "HEAD^{tree}",
            "-p",
            "HEAD",
            "-m",
            "the parked run's work",
        ],
    );
    git(repo, &["update-ref", &pinned_ref(op), &head]);
    let dir = replans_dir(db).join("snap-1");
    fs::create_dir_all(&dir).unwrap();
    let manifest = SnapshotManifest {
        id: "snap-1".into(),
        operation: op,
        generation: 2,
        run_id: run.clone(),
        task_id: TaskId::new(1),
        base_commit: main,
        head: head.clone(),
        branch: None,
        pinned_ref: pinned_ref(op),
        worktree: "worktree".into(),
        dirty: true,
        files: vec![
            SavedFile::of(
                "index.diff",
                SavedKind::Index,
                "git diff --cached --binary",
                DIFF,
            ),
            SavedFile {
                executable: true,
                ..SavedFile::of("untracked/0", SavedKind::Untracked, "run.sh", SCRIPT)
            },
        ],
        excluded: Vec::new(),
        conditions_version: "v".into(),
        remaining: Vec::new(),
        saved_at: 0,
    };
    fs::write(
        dir.join("manifest.json"),
        serde_json::to_vec(&manifest).unwrap(),
    )
    .unwrap();
    fs::create_dir_all(dir.join("untracked")).unwrap();
    fs::write(dir.join("untracked/0"), SCRIPT).unwrap();
    fs::write(
        dir.join("index.diff"),
        if corrupt {
            b"not the saved diff\n"
        } else {
            DIFF
        },
    )
    .unwrap();
    Applied {
        run,
        op,
        child,
        successor,
        head,
    }
}

/// One pass of the supervisor, given the `dagq` the test owns so a
/// service it starts stops with the test (`common::service`).
fn supervise_owned(db: &Path, repo: &Path, backend: &TestWorkspace) -> Value {
    let dagq = crate::common::service::owned_executable(db.parent().unwrap());
    let options = SuperviseOptions {
        retry_unreadable_review: false,
        ..supervise_options(4, true)
    };
    supervise_retrying_by(db, repo, backend, &options, &dagq).unwrap()
}

/// `dagq args` as a person, which must succeed; what it printed.
fn as_user(db: &Path, args: &[&str]) -> Value {
    let output = invoke_with(&[], db, args);
    assert!(
        output.status.success(),
        "{args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).unwrap()
}

/// The payloads of `kind` on `task`'s run `run`.
fn run_events(detail: &dagq::domain::TaskDetail, run: &TaskRun, kind: EventKind) -> Vec<Value> {
    detail
        .events
        .iter()
        .filter(|e| e.kind == kind && e.run_id.as_ref() == Some(run.id()))
        .map(|e| e.payload.clone())
        .collect()
}

/// The child's run is handed the saved uncommitted changes only (copied
/// into its run directory and named in its prompt), records what it
/// inherits, and its task completes only through its own run's receipt,
/// validation, review and landing on main.
#[test]
fn a_childs_run_is_handed_its_part_of_the_saved_work_and_goes_through_its_own_gates() {
    let (_dir, repo, db) = fixture();
    let a = applied(&db, &repo, "snapshot_diff", false);
    let backend = TestWorkspace::new(&db, false, VALID_AGENT);
    let reviewer = TestReviewer::new(&[verdict("pass", &[], "fine")]);
    let outcome = supervise_reviewed_owned(&db, &repo, &backend, &reviewer);
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    let mut queue = SqliteQueue::open(&db).unwrap();
    let detail = queue.show(a.child).unwrap();
    assert_eq!(detail.runs.len(), 1, "{:?}", event_kinds(&detail));
    let run = detail.runs[0].clone();
    let inherited = run_events(&detail, &run, EventKind::RunInherited);
    assert_eq!(inherited.len(), 1, "{inherited:?}");
    assert_eq!(inherited[0]["inherit_from_run"], json!(a.run));
    assert_eq!(inherited[0]["replan_operation_id"], json!(a.op));
    assert_eq!(inherited[0]["carry"], "snapshot_diff");
    let copied = Path::new(run.run_dir().unwrap()).join("replan-carry/index.diff");
    assert_eq!(fs::read(&copied).unwrap(), DIFF);
    // The untracked script keeps the executable bit the park saved.
    let script = Path::new(run.run_dir().unwrap()).join("replan-carry/untracked/0");
    assert_eq!(fs::read(&script).unwrap(), SCRIPT);
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = fs::metadata(&script).unwrap().permissions().mode();
        assert_ne!(mode & 0o100, 0, "{mode:o}");
    }
    let prompt = read_prompt(&run);
    for expected in [
        format!("Carried over from run {} of task 1", a.run),
        format!("replan operation {} replaced with this task", a.op),
        format!("{} (staged diff)", copied.display()),
        format!("{} (untracked executable file run.sh)", script.display()),
        "keeping an executable one's mode with `cp -p`".to_owned(),
    ] {
        assert!(prompt.contains(&expected), "{expected}\n{prompt}");
    }
    assert!(!prompt.contains("git cherry-pick"), "{prompt}");
    // Every gate of its own run, in order: its receipt validated, its
    // review, then its landing on main.
    let kinds = event_kinds(&detail);
    let validated = position(&kinds, "validation_finished");
    let reviewed = position(&kinds, "review_finished");
    let landed = position(&kinds, "run_integrated");
    assert!(validated < reviewed && reviewed < landed, "{kinds:?}");
    assert_eq!(run.status(), RunStatus::Integrated);
    assert_eq!(detail.task.status(), TaskStatus::Completed);
    git(
        &repo,
        &[
            "merge-base",
            "--is-ancestor",
            run.result_commit().unwrap().as_str(),
            "main",
        ],
    );
    assert_eq!(
        queue.show(TaskId::new(1)).unwrap().task.status(),
        TaskStatus::Replaced
    );
}

/// A snapshot whose diff does not read back as saved fails the child's
/// handover: `replan_carry_failed` on its run, which is abandoned with the
/// error for its recovery while the supervisor goes on claiming another
/// task, with the snapshot and its ref kept, no worktree made and main
/// untouched.
#[test]
fn a_handover_from_a_snapshot_not_as_saved_fails_with_the_snapshot_kept() {
    let (_dir, repo, db) = fixture();
    let main = git_out(&repo, &["rev-parse", "HEAD"]);
    let a = applied(&db, &repo, "snapshot_diff", true);
    let other = add_ready_task(&mut SqliteQueue::open(&db).unwrap(), "other", &[]);
    let backend = TestWorkspace::new(&db, false, VALID_AGENT);
    let outcome = supervise_owned(&db, &repo, &backend);
    assert!(
        outcome["errors"]
            .to_string()
            .contains("could not be handed"),
        "{outcome}"
    );
    let mut queue = SqliteQueue::open(&db).unwrap();
    let detail = queue.show(a.child).unwrap();
    assert_eq!(detail.runs.len(), 1, "{:?}", event_kinds(&detail));
    let run = detail.runs[0].clone();
    // Abandoned, then taken over by the recovery of a run nobody holds.
    assert!(
        matches!(run.status(), RunStatus::Starting | RunStatus::Interrupted),
        "{run:?}"
    );
    assert!(
        run.last_error().unwrap().contains("could not be handed"),
        "{:?}",
        run.last_error()
    );
    assert!(!Path::new(run.worktree_path().unwrap()).exists());
    let failed = run_events(&detail, &run, EventKind::ReplanCarryFailed);
    assert_eq!(failed.len(), 1, "{failed:?}");
    assert_eq!(failed[0]["operation_id"], json!(a.op));
    assert!(
        failed[0]["reason"]
            .as_str()
            .unwrap()
            .contains("the snapshot and its ref are kept"),
        "{failed:?}"
    );
    assert_ne!(detail.task.status(), TaskStatus::Completed);
    assert!(replans_dir(&db).join("snap-1/manifest.json").is_file());
    assert_eq!(git_out(&repo, &["rev-parse", &pinned_ref(a.op)]), a.head);
    assert_eq!(git_out(&repo, &["rev-parse", "main"]), main);
    assert!(queue.show(a.successor).unwrap().runs.is_empty());
    // The claims went on: another ready task got its run.
    assert_eq!(queue.show(other).unwrap().runs.len(), 1);
}

/// A child whose handover failed, then held and released by a person for a
/// new run from the current main (`retry_fresh`), is handed none of the
/// saved work on its next claim: the person chose to start afresh over
/// that work too, so the handover is not tried, nor failed, again. The
/// snapshot and its ref stay.
#[test]
fn a_child_released_to_start_afresh_is_handed_none_of_the_saved_work() {
    let (_dir, repo, db) = fixture();
    let a = applied(&db, &repo, "snapshot_diff", true);
    let backend = TestWorkspace::new(&db, false, VALID_AGENT);
    let outcome = supervise_owned(&db, &repo, &backend);
    assert!(
        outcome["errors"]
            .to_string()
            .contains("could not be handed"),
        "{outcome}"
    );
    let child = a.child.to_string();
    as_user(&db, &["hold", &child, "--reason", "the handover failed"]);
    // The failed run's recovery goes on until the hold settles.
    for _ in 0..3 {
        let hold = SqliteQueue::open(&db).unwrap().task_hold(a.child).unwrap();
        if hold.unwrap().state() == HoldState::Held {
            break;
        }
        supervise_owned(&db, &repo, &backend);
    }
    let released = as_user(&db, &["release", &child]);
    assert_eq!(released["decision"], "retry_fresh", "{released}");
    let outcome = supervise_owned(&db, &repo, &backend);
    backend.join();
    assert!(
        !outcome["errors"]
            .to_string()
            .contains("could not be handed"),
        "{outcome}"
    );
    let detail = SqliteQueue::open(&db).unwrap().show(a.child).unwrap();
    assert_eq!(detail.runs.len(), 2, "{:?}", event_kinds(&detail));
    let fresh = &detail.runs[1];
    assert!(run_events(&detail, fresh, EventKind::RunInherited).is_empty());
    assert!(run_events(&detail, fresh, EventKind::ReplanCarryFailed).is_empty());
    assert!(
        !Path::new(fresh.run_dir().unwrap())
            .join("replan-carry")
            .exists()
    );
    let prompt = read_prompt(fresh);
    assert!(!prompt.contains("Carried over from run"), "{prompt}");
    assert!(replans_dir(&db).join("snap-1/manifest.json").is_file());
    assert_eq!(git_out(&repo, &["rev-parse", &pinned_ref(a.op)]), a.head);
}
