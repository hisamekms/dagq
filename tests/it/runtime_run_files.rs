//! A worker's run directory is not a way out of its sandbox (task 1184).
use crate::common;
use crate::runtime_codex::{FINISH, TASK, codex_fixture, detail, finished, supervise_thread};
use crate::runtime_support::*;

/// Check real supervisor passes while the live worker leaves unreadable
/// markers. The worker waits for a test handshake, not a fixed delay.
fn markers_do_not_hold_passes(fifos: bool) {
    let (dir, repo, db, backend, codex) = codex_fixture();
    let outside = dir.path().join("outside.json");
    let secret = r#"{"hook_event_name":"Stop","dagq_turn":{"turn":99,"outcome":"succeeded"}}"#;
    fs::write(&outside, secret).unwrap();
    let install = if fifos {
        r#"mkfifo "$RUN_DIR/idle.json" "$RUN_DIR/receipt.json""#.to_owned()
    } else {
        format!(
            r#"ln -s '{}' "$RUN_DIR/idle.json"; ln -s '{}' "$RUN_DIR/receipt.json""#,
            outside.display(),
            outside.display()
        )
    };
    set_turns(
        dir.path(),
        &format!(
            r#"
{install}
touch "$RUN_DIR/installed"
while [ ! -f "$RUN_DIR/release" ]; do sleep 0.02; done
rm "$RUN_DIR/idle.json" "$RUN_DIR/receipt.json"
{FINISH}
"#
        ),
    );
    let backend = Arc::new(backend);
    let passes = Arc::new(AtomicU64::new(0));
    let options = SuperviseOptions {
        codex,
        passes: passes.clone(),
        ..supervise_options(4, true)
    };
    let supervisor = {
        let (db, repo, backend) = (db.clone(), repo.clone(), backend.clone());
        thread::spawn(move || {
            let reviewer = TestReviewer::new(&[verdict("pass", &[], "safe")]);
            runtime::supervise_with_reviewer(
                &db,
                &repo,
                &*backend,
                &claude_stub(&db),
                &reviewer,
                Path::new(env!("CARGO_BIN_EXE_dagq")),
                &options,
            )
        })
    };
    wait_until(&db, common::STEP_LIMIT, |queue| {
        queue
            .show(TASK)
            .unwrap()
            .runs
            .first()
            .and_then(|run| run.run_dir())
            .is_some_and(|dir| Path::new(dir).join("installed").exists())
    });
    await_passes(&passes, SOME_PASSES);
    let before = detail(&db);
    assert!(payloads(&before, "receipt_observed").is_empty());
    assert!(payloads(&before, "turn_requested").is_empty());
    assert_eq!(fs::read_to_string(&outside).unwrap(), secret);
    fs::write(
        Path::new(before.runs[0].run_dir().unwrap()).join("release"),
        "",
    )
    .unwrap();
    let outcome = finished(&backend, supervisor);
    assert_eq!(outcome["runs"][0]["status"], "integrated", "{outcome}");
    assert_eq!(fs::read_to_string(&outside).unwrap(), secret);
}

#[test]
fn fifo_markers_do_not_block_supervisor_passes() {
    markers_do_not_hold_passes(true);
}

#[test]
fn linked_markers_are_not_read_by_the_supervisor() {
    markers_do_not_hold_passes(false);
}

#[test]
fn linked_exit_and_temporary_files_leave_the_target_untouched() {
    let (dir, repo, db, backend, codex) = codex_fixture();
    let outside = dir.path().join("auth.json");
    fs::write(&outside, "secret").unwrap();
    set_turns(
        dir.path(),
        &format!(
            r#"case "$TURN" in
1)
    for name in idle.json idle.json.tmp turns/exit turns/request-000001.json.tmp; do
        ln -s '{}' "$RUN_DIR/$name"
    done
    say working ;;
*) {FINISH} ;;
esac"#,
            outside.display()
        ),
    );
    let backend = Arc::new(backend);
    let supervisor = supervise_thread(
        &db,
        &repo,
        backend.clone(),
        &codex,
        &[verdict("pass", &[], "safe")],
    );
    let outcome = finished(&backend, supervisor);
    assert_eq!(outcome["runs"][0]["status"], "integrated", "{outcome}");
    assert_eq!(fs::read_to_string(&outside).unwrap(), "secret");
    assert!(payloads(&detail(&db), "turn_started").len() >= 2);
}

/// Directory redirection is rejected for writes, listings, renames and
/// removals too, through the same concrete port the supervisor uses.
#[test]
fn linked_run_or_turns_directories_are_never_used_by_run_files() {
    use dagq::application::RunFiles;
    use dagq::infrastructure::run_files::LocalRunFiles;
    use std::os::unix::fs::symlink;
    let temp = tempfile::tempdir().unwrap();
    // A run directory is one below the queue's `runs/`; the host's
    // directories above it may be links and are followed.
    let run = temp.path().join("runs/run");
    let outside = temp.path().join("outside");
    fs::create_dir_all(outside.join("turns")).unwrap();
    fs::create_dir_all(&run).unwrap();
    fs::write(outside.join("turns/request-000001.json"), "secret").unwrap();
    let files = LocalRunFiles;
    for whole_run in [false, true] {
        if whole_run {
            fs::remove_file(run.join("turns")).unwrap();
            fs::remove_dir(&run).unwrap();
            symlink(&outside, &run).unwrap();
        } else {
            symlink(outside.join("turns"), run.join("turns")).unwrap();
        }
        let turns = run.join("turns");
        let request = turns.join("request-000001.json");
        assert!(files.create_dir_all(&turns).is_err());
        assert!(files.read_dir(&turns).is_err());
        assert!(files.read(&request).is_err());
        assert!(files.write(&turns.join("exit"), b"").is_err());
        assert!(files.write(&turns.join("limits.json"), b"{}").is_err());
        assert!(
            files
                .rename(&request, &turns.join("request-000001.dropped"))
                .is_err()
        );
        assert!(files.remove_file(&request).is_err());
        assert_eq!(
            fs::read_to_string(outside.join("turns/request-000001.json")).unwrap(),
            "secret"
        );
        assert_eq!(fs::read_dir(outside.join("turns")).unwrap().count(), 1);
    }
}

/// Unreadable requests take the wrapper's existing error/exit path;
/// O_NONBLOCK makes that path reachable even without a FIFO writer.
#[test]
fn a_fifo_request_does_not_hold_the_wrapper() {
    let (dir, repo, db, backend, codex) = codex_fixture();
    set_turns(
        dir.path(),
        r#"mkfifo "$RUN_DIR/turns/request-000001.json"; say working"#,
    );
    let backend = Arc::new(backend);
    let supervisor = supervise_thread(&db, &repo, backend.clone(), &codex, &[]);
    let outcome = joined(supervisor, "the supervisor to observe the refused FIFO").unwrap();
    assert_eq!(outcome["errors"], json!([]));
    let workers: Vec<_> = backend
        .sessions
        .lock()
        .unwrap()
        .iter_mut()
        .filter_map(|(_, session)| session.worker.take())
        .collect();
    assert_eq!(workers.len(), 1);
    for worker in workers {
        let error = joined(worker, "the wrapper to refuse the FIFO").unwrap_err();
        assert!(
            format!("{error:#}").contains("not a regular file"),
            "{error:#}"
        );
    }
    let detail = detail(&db);
    assert_eq!(detail.runs[0].status(), RunStatus::Failed);
    assert!(payloads(&detail, "provider_switched").is_empty());
    assert!(!payloads(&detail, "recovery_requested").is_empty());
}
