//! A person's (or the inbox's) `replan resolve` of an operation held
//! `blocked` by anything but its apply (ADR-t1521-2 decision 2): the
//! choice is made once on the operation and generation read, a
//! continuation or a withdrawal releases the proposal's children and goes
//! on through the restore and the new session, which hold it `blocked`
//! again when they fail, and a snapshot not as saved refuses every choice.

use crate::common::cli::{ok, ok_as, queue};
use crate::common::queue::{base, new_task};
use crate::common::replans::{args, as_role, claimed, denials, park, request};
use dagq::domain::EventKind;
use dagq::{
    application::{TaskReads, TaskStore},
    domain::{
        LeaseToken, ReplanId, RunId, TaskId,
        task_replan::{ReplanMove, ReplanStatus},
    },
    infrastructure::sqlite::SqliteQueue,
};
use rusqlite::Connection;
use serde_json::{Value, json};
use std::path::Path;

/// `replan resolve OP` as `role` (a worker on `a`'s run), its JSON and
/// whether it succeeded.
fn resolve(
    db: &Path,
    role: &str,
    worker: Option<(&RunId, TaskId)>,
    op: ReplanId,
    generation: i64,
    choice: &str,
) -> (bool, Value) {
    let (op, generation) = (op.to_string(), generation.to_string());
    as_role(
        db,
        role,
        worker,
        &[
            "replan",
            "resolve",
            &op,
            "--generation",
            &generation,
            choice,
            "--reason",
            "the person decided",
        ],
    )
}

fn event_count(db: &Path) -> i64 {
    Connection::open(db)
        .unwrap()
        .query_row("SELECT count(*) FROM run_events", [], |r| r.get(0))
        .unwrap()
}

/// The output of `git args` in `repo`, which must succeed.
fn git_out(repo: &Path, args: &[&str]) -> String {
    use crate::common::Bounded;
    let output = std::process::Command::new("git")
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

/// Save snapshot `snap-1` of operation `id` as the park leaves it (a
/// manifest of the operation and its run with one staged diff, and the
/// run's head, a commit on top of the repository's, pinned by the
/// operation's ref in a repository the queue is bound to) and move the
/// operation from `snapshotting` to `parked` with it. The repository.
fn parked_with_snapshot(
    db: &Path,
    queue: &mut SqliteQueue,
    id: ReplanId,
    generation: i64,
) -> std::path::PathBuf {
    use dagq::domain::replan_snapshot::{SavedFile, SavedKind, SnapshotManifest, pinned_ref};
    let repo = db.parent().unwrap().join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    crate::common::template::repository(&repo, "replan resolve");
    let base = git_out(&repo, &["rev-parse", "HEAD"]);
    let head = git_out(
        &repo,
        &[
            "commit-tree",
            "HEAD^{tree}",
            "-p",
            "HEAD",
            "-m",
            "the parked run's work",
        ],
    );
    git_out(&repo, &["update-ref", &pinned_ref(id), &head]);
    let common = repo.join(".git").canonicalize().unwrap();
    queue.bind_repository(common.to_str().unwrap()).unwrap();
    let op = queue.replan_operation(id).unwrap();
    let dir = dagq::infrastructure::location::replans_dir(db).join("snap-1");
    std::fs::create_dir_all(&dir).unwrap();
    let diff = b"diff --git a/notes.txt b/notes.txt\n";
    std::fs::write(dir.join("index.diff"), diff).unwrap();
    let manifest = SnapshotManifest {
        id: "snap-1".into(),
        operation: id,
        generation,
        run_id: op.run_id.clone(),
        task_id: op.task_id,
        base_commit: base,
        head,
        branch: None,
        pinned_ref: pinned_ref(id),
        worktree: "worktree".into(),
        dirty: true,
        files: vec![SavedFile::of(
            "index.diff",
            SavedKind::Index,
            "git diff --cached --binary",
            diff,
        )],
        excluded: Vec::new(),
        conditions_version: op.task_version.clone(),
        remaining: op.conditions.acceptance.clone(),
        saved_at: 0,
    };
    std::fs::write(
        dir.join("manifest.json"),
        serde_json::to_vec(&manifest).unwrap(),
    )
    .unwrap();
    queue
        .move_replan(
            id,
            generation,
            &ReplanMove {
                to: ReplanStatus::Parked,
                reason: "parked with snap-1".into(),
                snapshot_id: Some("snap-1".into()),
                diagnosis: None,
            },
        )
        .unwrap();
    repo
}

/// A person's resolution of an operation held `blocked` (ADR-t1521-2
/// decision 2) through the CLI: only the user and the inbox give one (a
/// worker on its own run, the planner, the observer and a job are refused
/// and recorded); one read under an older generation, and the same one
/// given twice, write nothing; a new plan of one whose work was not saved
/// goes back to the save, and of one with its snapshot to `parked` with
/// its planning request; each is recorded with who chose what, which
/// `replans ID` and `timeline` show.
#[test]
fn a_person_resolves_a_blocked_operation_once_on_what_they_read() {
    let (_dir, db) = queue();
    let (mut queue, a, _, run) = claimed(&db);
    let recorded = ok_as("inbox", &db, &args(&request(&run, "split it")));
    let id = ReplanId::new(recorded["operation"]["id"].as_i64().unwrap());
    let request_id = recorded["request"]["id"].as_i64().unwrap();
    let generation = park(&mut queue, id, 1, ReplanStatus::PauseRequested);
    park(&mut queue, id, generation, ReplanStatus::Quiescing);
    park(&mut queue, id, generation, ReplanStatus::Blocked);

    for (role, worker) in [
        ("worker", Some((&run, a))),
        ("planner", None),
        ("observer", None),
        ("plan-review-job", None),
    ] {
        let (resolved, refusal) = resolve(&db, role, worker, id, generation, "--replan");
        assert!(!resolved, "{role}");
        assert_eq!(refusal["denied"]["capability"], "replan.resolve", "{role}");
    }
    assert_eq!(denials(&db, "replan.resolve"), 4);

    // Read under an older generation: nothing is written.
    let before = event_count(&db);
    let (resolved, refusal) = resolve(&db, "inbox", None, id, 1, "--replan");
    assert!(!resolved);
    assert!(
        refusal
            .to_string()
            .contains(&format!("generation {generation}, not 1")),
        "{refusal}"
    );
    assert_eq!(event_count(&db), before);

    // Its work was never saved: a new plan goes back to the save.
    let (resolved, answer) = resolve(&db, "inbox", None, id, generation, "--replan");
    assert!(resolved, "{answer}");
    assert_eq!(answer["resolution"], "replan");
    assert_eq!(answer["blocked_at"], "quiescing");
    assert_eq!(answer["operation"]["status"], "quiescing");
    let raised = generation + 1;
    assert_eq!(answer["operation"]["generation"], raised);
    // The same again, under the generation read before it: nothing.
    let before = event_count(&db);
    let (again, refusal) = resolve(&db, "inbox", None, id, generation, "--replan");
    assert!(!again);
    assert!(refusal.to_string().contains("not blocked"), "{refusal}");
    assert_eq!(event_count(&db), before);

    // Saved and then held: a new plan goes to `parked`, its request kept.
    park(&mut queue, id, raised, ReplanStatus::Snapshotting);
    parked_with_snapshot(&db, &mut queue, id, raised);
    park(&mut queue, id, raised, ReplanStatus::Blocked);
    let (resolved, answer) = resolve(&db, "user", None, id, raised, "--replan");
    assert!(resolved, "{answer}");
    assert_eq!(answer["operation"]["status"], "parked");
    assert_eq!(answer["operation"]["snapshot_id"], "snap-1");
    assert_eq!(answer["operation"]["request_id"], request_id);
    assert_eq!(answer["operation"]["request_status"], "open");

    let shown = ok_as("observer", &db, &["replans", &id.to_string()]);
    let resolutions = shown["replans"][0]["resolutions"].as_array().unwrap();
    let made: Vec<(&str, &str, &str)> = resolutions
        .iter()
        .map(|r| {
            (
                r["by"].as_str().unwrap(),
                r["blocked_at"].as_str().unwrap(),
                r["to"].as_str().unwrap(),
            )
        })
        .collect();
    assert_eq!(
        made,
        [
            ("inbox", "quiescing", "quiescing"),
            ("user", "parked", "parked")
        ]
    );
    let timeline = ok(&db, &["timeline", run.as_str()]).to_string();
    assert!(timeline.contains("replan_resolved"), "{timeline}");
}

/// A continuation and a withdrawal of an operation held `blocked` check,
/// in the one transaction that writes them, that no child of its
/// replacement proposal may be claimed, and are refused with nothing
/// changed while its snapshot does not read back as saved. The
/// continuation then closes the operation on the run it parked: the
/// proposal's children go back to draft, its planning request is declined
/// with the outcome, and the task keeps its conditions and the task that
/// waits for it; the saved work is not touched.
#[test]
fn a_continuation_releases_the_children_and_refuses_a_snapshot_not_as_saved() {
    use dagq::domain::{PlannerOrigin, PlannerOwner, Submission, TaskStatus};
    let (_dir, db) = queue();
    let (mut queue, a, b, run) = claimed(&db);
    let recorded = ok_as("inbox", &db, &args(&request(&run, "split it")));
    let id = ReplanId::new(recorded["operation"]["id"].as_i64().unwrap());
    let generation = park(&mut queue, id, 1, ReplanStatus::PauseRequested);
    park(&mut queue, id, generation, ReplanStatus::Quiescing);
    park(&mut queue, id, generation, ReplanStatus::Snapshotting);
    let repo = parked_with_snapshot(&db, &mut queue, id, generation);
    let goal = queue.show(a).unwrap().task.goal_id();
    let child = queue
        .add(dagq::domain::NewTask {
            goal_id: goal,
            ..new_task("A, the API part")
        })
        .unwrap()
        .id();
    let proposal = queue
        .submit(Submission {
            tasks: vec![child],
            goals: Vec::new(),
            proposal: None,
            owner: PlannerOwner {
                origin: PlannerOrigin::Runtime,
                workspace_id: None,
            },
        })
        .unwrap()
        .id();
    let conn = Connection::open(&db).unwrap();
    conn.execute(
        "UPDATE replan_operations SET proposal_id=?1 WHERE id=?2",
        rusqlite::params![proposal, id],
    )
    .unwrap();
    park(&mut queue, id, generation, ReplanStatus::Blocked);
    let conditions = queue.replan_operation(id).unwrap().task_version;
    let status = |queue: &mut SqliteQueue, task: TaskId| queue.show(task).unwrap().task.status();

    // A snapshot that does not read back as saved refuses every choice.
    let saved = dagq::infrastructure::location::replans_dir(&db).join("snap-1/index.diff");
    let diff = std::fs::read(&saved).unwrap();
    std::fs::write(&saved, b"changed since").unwrap();
    let before = event_count(&db);
    for choice in ["--continue", "--withdraw", "--replan"] {
        let (resolved, refusal) = resolve(&db, "user", None, id, generation, choice);
        assert!(!resolved, "{choice}");
        assert!(
            refusal.to_string().contains("does not read back as saved"),
            "{refusal}"
        );
    }
    std::fs::write(&saved, &diff).unwrap();
    // A pinned ref moved off the saved head refuses every choice too: the
    // caller reads it with Git before it sends the resolution.
    let pin = dagq::domain::replan_snapshot::pinned_ref(id);
    let pinned = git_out(&repo, &["rev-parse", &pin]);
    git_out(&repo, &["update-ref", &pin, "HEAD"]);
    for choice in ["--continue", "--withdraw", "--replan"] {
        let (resolved, refusal) = resolve(&db, "user", None, id, generation, choice);
        assert!(!resolved, "{choice}");
        assert!(
            refusal
                .to_string()
                .contains(&format!("{pin} does not name the saved head {pinned}")),
            "{refusal}"
        );
    }
    git_out(&repo, &["update-ref", &pin, &pinned]);
    // A child that may be claimed, read in the same transaction.
    conn.execute("UPDATE tasks SET status='ready' WHERE id=?1", [child])
        .unwrap();
    let (resolved, refusal) = resolve(&db, "user", None, id, generation, "--withdraw");
    assert!(!resolved);
    assert!(
        refusal.to_string().contains(&format!("task {child}")),
        "{refusal}"
    );
    conn.execute("UPDATE tasks SET status='submitted' WHERE id=?1", [child])
        .unwrap();
    assert_eq!(event_count(&db), before);
    assert_eq!(
        queue.replan_operation(id).unwrap().status,
        ReplanStatus::Blocked
    );

    let (resolved, answer) = resolve(&db, "user", None, id, generation, "--continue");
    assert!(resolved, "{answer}");
    let op = queue.replan_operation(id).unwrap();
    assert_eq!(op.status, ReplanStatus::ContinuingAfterPark);
    assert!(op.closed_at.is_some());
    assert_eq!(op.generation, generation + 1);
    assert_eq!(op.run_id, run);
    assert_eq!(op.snapshot_id.as_deref(), Some("snap-1"));
    assert_eq!(answer["operation"]["request_status"], "declined");
    assert_eq!(status(&mut queue, child), TaskStatus::Draft);
    assert_eq!(status(&mut queue, a), TaskStatus::InProgress);
    assert_eq!(queue.show(b).unwrap().dependencies, [a]);
    assert_eq!(op.task_version, conditions);
    assert!(
        dagq::infrastructure::location::replans_dir(&db)
            .join("snap-1/manifest.json")
            .is_file()
    );
    // Closed: a second continuation writes nothing.
    let before = event_count(&db);
    let (again, refusal) = resolve(&db, "user", None, id, generation + 1, "--continue");
    assert!(!again);
    assert!(refusal.to_string().contains("closed"), "{refusal}");
    assert_eq!(event_count(&db), before);
}

/// A continuation whose new session could not be started goes back to
/// `blocked` with the work kept (ADR-t1521-2 decision 2), as a failed
/// restore does, also when a request on the run opened another operation
/// since (superseded, the task keeping one): only while the supervisor
/// that resumes the run holds it, under the operation's generation, and
/// only for the continuation's first resume; anything else writes nothing.
#[test]
fn a_continuation_whose_session_does_not_start_is_blocked_again() {
    use dagq::domain::replan_snapshot::RestoreStart;
    let (_dir, db) = queue();
    let (mut queue, _, _, run) = claimed(&db);
    let recorded = ok(&db, &args(&request(&run, "split it")));
    let id = ReplanId::new(recorded["operation"]["id"].as_i64().unwrap());
    let generation = park(&mut queue, id, 1, ReplanStatus::PauseRequested);
    park(&mut queue, id, generation, ReplanStatus::Quiescing);
    park(&mut queue, id, generation, ReplanStatus::Snapshotting);
    let moved = |queue: &mut SqliteQueue, to: ReplanStatus, snapshot: Option<String>| {
        queue
            .move_replan(
                id,
                generation,
                &ReplanMove {
                    to,
                    reason: format!("the test moves it to {}", to.as_str()),
                    snapshot_id: snapshot,
                    diagnosis: None,
                },
            )
            .unwrap();
    };
    moved(&mut queue, ReplanStatus::Parked, Some("snap-1".into()));
    moved(&mut queue, ReplanStatus::ContinuingAfterPark, None);
    let conn = Connection::open(&db).unwrap();
    conn.execute("DELETE FROM run_leases WHERE run_id=?1", [run.as_str()])
        .unwrap();
    conn.execute(
        "UPDATE task_runs SET status='needs_session' WHERE id=?1",
        [run.as_str()],
    )
    .unwrap();
    let (holder, other) = (LeaseToken::new("holder"), LeaseToken::new("other"));
    assert_eq!(
        queue.begin_restore(&run, &holder, id, generation).unwrap(),
        RestoreStart::Owned
    );
    let failed = |queue: &SqliteQueue| {
        queue
            .run_events(&run)
            .unwrap()
            .into_iter()
            .filter(|event| event.kind.as_str() == "replan_restore_failed")
            .collect::<Vec<_>>()
    };
    let fail = |queue: &mut SqliteQueue, token: &LeaseToken, generation: i64| {
        queue
            .fail_continuation_start(&run, token, id, generation, "the wrapper did not start")
            .unwrap()
    };

    // Another supervisor's, or under another generation: nothing.
    assert!(!fail(&mut queue, &other, generation));
    assert!(!fail(&mut queue, &holder, generation + 1));
    assert!(failed(&queue).is_empty());
    assert_eq!(
        queue.replan_operation(id).unwrap().status,
        ReplanStatus::ContinuingAfterPark
    );

    // A request on the run since opens another operation; the failure is
    // still recorded, and the continuation held `blocked` supersedes it,
    // the task keeping one open operation.
    let later = ok(&db, &args(&request(&run, "split it after all")));
    let later = ReplanId::new(later["operation"]["id"].as_i64().unwrap());
    assert_ne!(later, id);
    assert!(fail(&mut queue, &holder, generation));
    let superseded = queue.replan_operation(later).unwrap();
    assert_eq!(superseded.status, ReplanStatus::Continuing);
    let op = queue.replan_operation(id).unwrap();
    assert_eq!(op.status, ReplanStatus::Blocked);
    assert_eq!(op.return_phase, Some(ReplanStatus::ContinuingAfterPark));
    assert_eq!(op.snapshot_id.as_deref(), Some("snap-1"));
    let recorded = failed(&queue);
    assert_eq!(recorded.len(), 1);
    assert_eq!(recorded[0].payload["start_failed"], true);
    assert_eq!(recorded[0].payload["operation_id"], id.as_i64());
    // Blocked: not again.
    assert!(!fail(&mut queue, &holder, generation));

    // Once a resume of the continuation started and finished, a later
    // failure to start is the run's own.
    moved(&mut queue, ReplanStatus::ContinuingAfterPark, None);
    for (kind, payload) in [
        ("resume_started", r#"{"attempt":1}"#),
        ("resume_finished", r#"{"outcome":"unresolved"}"#),
    ] {
        conn.execute(
            "INSERT INTO run_events(task_id, run_id, kind, payload)
             SELECT task_id, id, ?2, ?3 FROM task_runs WHERE id=?1",
            rusqlite::params![run.as_str(), kind, payload],
        )
        .unwrap();
    }
    assert!(!fail(&mut queue, &holder, generation));
    assert_eq!(failed(&queue).len(), 1);
}

/// A continuation whose work was restored but whose new session did not
/// start is restored again once a person resolves it again: the restore
/// of the earlier continuation is not this one's, so the worktree is
/// checked against the snapshot before the run resumes, and a session of
/// this one that does not start holds it `blocked` again. Which resume is
/// a continuation's first, whichever order the earlier resume's end and
/// the resolution come in, is `domain::task_replan`'s (unit tests).
#[test]
fn a_continuation_resolved_again_after_its_session_did_not_start_is_restored_again() {
    use dagq::domain::replan_snapshot::RestoreStart;
    use dagq::domain::task_replan::{ReplanResolution, ResolveReplan};
    let (_dir, db) = queue();
    let (mut queue, _, _, run) = claimed(&db);
    let recorded = ok(&db, &args(&request(&run, "split it")));
    let id = ReplanId::new(recorded["operation"]["id"].as_i64().unwrap());
    let generation = park(&mut queue, id, 1, ReplanStatus::PauseRequested);
    park(&mut queue, id, generation, ReplanStatus::Quiescing);
    park(&mut queue, id, generation, ReplanStatus::Snapshotting);
    for (to, snapshot) in [
        (ReplanStatus::Parked, Some("snap-1".to_owned())),
        (ReplanStatus::ContinuingAfterPark, None),
    ] {
        queue
            .move_replan(
                id,
                generation,
                &ReplanMove {
                    to,
                    reason: format!("the test moves it to {}", to.as_str()),
                    snapshot_id: snapshot,
                    diagnosis: None,
                },
            )
            .unwrap();
    }
    let conn = Connection::open(&db).unwrap();
    // The run waits for a session, held by no one.
    let unheld = || {
        conn.execute("DELETE FROM run_leases WHERE run_id=?1", [run.as_str()])
            .unwrap();
        conn.execute(
            "UPDATE task_runs SET status='needs_session' WHERE id=?1",
            [run.as_str()],
        )
        .unwrap();
    };
    let holder = LeaseToken::new("holder");
    let restore = |queue: &mut SqliteQueue, generation: i64| {
        assert_eq!(
            queue.begin_restore(&run, &holder, id, generation).unwrap(),
            RestoreStart::Owned
        );
        assert!(
            queue
                .finish_restore(
                    &run,
                    &holder,
                    id,
                    generation,
                    Ok(json!({"operation_id": id, "snapshot_id": "snap-1", "action": "kept"})),
                )
                .unwrap()
        );
        assert_eq!(
            queue.begin_restore(&run, &holder, id, generation).unwrap(),
            RestoreStart::Done
        );
    };
    // The resume takes the run and its new session does not start.
    let unstarted = |queue: &mut SqliteQueue, generation: i64| {
        assert!(
            queue
                .begin_resume(
                    &run,
                    &holder,
                    &base(),
                    None,
                    dagq::domain::resume::ResumeConfig::default(),
                )
                .unwrap()
                .is_some()
        );
        queue
            .fail_continuation_start(&run, &holder, id, generation, "the wrapper did not start")
            .unwrap()
    };

    unheld();
    restore(&mut queue, generation);
    assert!(unstarted(&mut queue, generation));
    let op = queue.replan_operation(id).unwrap();
    assert_eq!(op.status, ReplanStatus::Blocked);
    // The resume that gave up ends, as `give_up_resume` ends it.
    queue
        .finish_resume(
            &run,
            &holder,
            None,
            Some("the wrapper did not start"),
            false,
            json!({"reason": "worker_failed"}),
        )
        .unwrap();

    let resolved = queue
        .resolve_replan(
            &ResolveReplan {
                operation: id,
                generation: op.generation,
                resolution: ReplanResolution::Continue,
                reason: "the person decided".into(),
            },
            None,
            "user",
            "user",
        )
        .unwrap();
    let again = resolved.operation.generation;
    assert_eq!(resolved.operation.status, ReplanStatus::ContinuingAfterPark);
    assert_ne!(again, generation);
    unheld();
    // Restored again, not taken as done by the earlier continuation's.
    restore(&mut queue, again);
    let restored = queue
        .run_events(&run)
        .unwrap()
        .into_iter()
        .filter(|event| event.kind == EventKind::ReplanRestored)
        .map(|event| event.payload["generation"].as_i64())
        .collect::<Vec<_>>();
    assert_eq!(restored, [Some(generation), Some(again)]);
    // Its own first session that does not start holds it blocked again,
    // the earlier resume's end not counted as this one's.
    assert!(unstarted(&mut queue, again));
    assert_eq!(
        queue.replan_operation(id).unwrap().status,
        ReplanStatus::Blocked
    );
}
