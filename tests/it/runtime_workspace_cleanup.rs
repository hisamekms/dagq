//! `run close-workspaces` (ADR-t1228-1 decision 6): the workspaces ended
//! runs left open while no supervisor swept them are listed (a dry run by
//! default) and, with `--apply`, closed by a run's ID, a task's ID or all
//! at once, through a stub cmux. A live or waiting run's workspace and the
//! inbox's are never closed, a close is an event with the caller as its
//! actor, and the roles the ADR does not allow are refused.
use crate::common;
use crate::runtime_support;
use dagq::domain::{PlannerOrigin, RunEvent, SessionRole};
use std::process::Output;

use common::cli::invoke_with;
use runtime_support::*;

/// A stub cmux of one window that lists `listed` but the workspaces it
/// closed, and appends each closed one to `<dir>/closed`.
fn stub_cmux(dir: &Path, listed: &[&str]) -> PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let closed = dir.join("closed");
    fs::write(dir.join("listed"), listed.join("\n") + "\n").unwrap();
    let stub = dir.join("cmux");
    fs::write(
        &stub,
        format!(
            r#"#!/bin/sh
closed='{closed}'
listed='{listed}'
touch "$closed"
case "$*" in
  *list-windows*) echo '[{{"id":"W"}}]' ;;
  *'workspace list'*)
    printf '{{"workspaces":['
    sep=''
    while read -r id; do
      [ -n "$id" ] || continue
      grep -qx "$id" "$closed" && continue
      printf '%s{{"id":"%s"}}' "$sep" "$id"
      sep=','
    done < "$listed"
    printf ']}}\n' ;;
  'workspace close '*) echo "$3" >> "$closed"; echo 'OK workspace:1' ;;
  *) echo OK ;;
esac
"#,
            closed = closed.display(),
            listed = dir.join("listed").display(),
        ),
    )
    .unwrap();
    fs::set_permissions(&stub, fs::Permissions::from_mode(0o755)).unwrap();
    stub
}

fn closed(dir: &Path) -> Vec<String> {
    fs::read_to_string(dir.join("closed"))
        .unwrap_or_default()
        .lines()
        .map(str::to_owned)
        .collect()
}

/// `run close-workspaces` with `args` as `env`, through the stub `cmux`.
fn cleanup(env: &[(&str, &str)], db: &Path, cmux: &Path, args: &[&str]) -> Output {
    let mut argv = vec!["run", "close-workspaces", "--cmux", cmux.to_str().unwrap()];
    argv.extend_from_slice(args);
    invoke_with(env, db, &argv)
}

fn cleaned(env: &[(&str, &str)], db: &Path, cmux: &Path, args: &[&str]) -> Value {
    let output = cleanup(env, db, cmux, args);
    assert!(
        output.status.success(),
        "{args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).unwrap()
}

/// The run IDs and outcomes of a report.
fn outcomes(report: &Value) -> Vec<(String, String)> {
    report["workspaces"]
        .as_array()
        .unwrap()
        .iter()
        .map(|w| {
            (
                w["run_id"].as_str().unwrap().to_owned(),
                w["outcome"].as_str().unwrap().to_owned(),
            )
        })
        .collect()
}

fn closes_of(queue: &SqliteQueue, run: &TaskRun) -> Vec<RunEvent> {
    queue
        .run_events(run.id())
        .unwrap()
        .into_iter()
        .filter(|e| e.kind == "workspace_closed")
        .collect()
}

#[test]
fn ended_runs_workspaces_are_listed_then_closed_by_run_task_or_all() {
    let (fixture, repo, db) = fixture();
    {
        let mut queue = SqliteQueue::open(&db).unwrap();
        for title in [
            "second task",
            "third task",
            "fourth task",
            "fifth task",
            "sixth task",
            "seventh task",
        ] {
            add_ready_task(&mut queue, title, &[]);
        }
    }
    let backend = TestWorkspace::new(
        &db,
        false,
        "commit work; receipt \"$(git rev-parse HEAD)\"; exit 7",
    );
    supervise(&db, &repo, &backend).unwrap();
    backend.join();
    let mut queue = SqliteQueue::open(&db).unwrap();
    let runs: Vec<TaskRun> = (1..=7)
        .map(|task| queue.show(TaskId::new(task)).unwrap().runs[0].clone())
        .collect();
    for run in &runs {
        assert_eq!(run.status(), RunStatus::Failed);
    }
    let workspace = |run: &TaskRun| run.workspace_id().unwrap().to_owned();
    // Task 1 is canceled: its run is the sweep's. Task 2's run is the
    // triage's (`triage by hand`). Task 3's run is live again, as a run
    // waiting for a person is. Tasks 4 to 7 are canceled, but each run's
    // workspace is one the queue keeps for a session: the inbox's and the
    // supervisor's (`up`), an open planner's (recorded in upper case) and
    // a closed planner's.
    for task in [1, 4, 5, 6, 7] {
        queue
            .transition(TaskId::new(task), TaskAction::Cancel)
            .unwrap();
    }
    Connection::open(&db)
        .unwrap()
        .execute(
            "UPDATE task_runs SET status='running' WHERE id=?1",
            [runs[2].id().as_str()],
        )
        .unwrap();
    queue
        .register_session_workspace(SessionRole::Inbox, &workspace(&runs[3]))
        .unwrap();
    queue
        .register_session_workspace(SessionRole::Supervisor, &workspace(&runs[4]))
        .unwrap();
    let open = queue.open_planner(PlannerOrigin::Person, None).unwrap();
    queue
        .planner_workspace_created(open.id, &workspace(&runs[5]).to_ascii_uppercase())
        .unwrap();
    let closed_planner = queue.open_planner(PlannerOrigin::Runtime, None).unwrap();
    queue
        .planner_workspace_created(closed_planner.id, &workspace(&runs[6]))
        .unwrap();
    queue.close_planner(closed_planner.id, None).unwrap();
    assert!(
        queue
            .planners(false)
            .unwrap()
            .iter()
            .all(|p| p.id != closed_planner.id)
    );
    let dir = fixture.dir.path().join("cmux");
    fs::create_dir(&dir).unwrap();
    let ids: Vec<String> = runs.iter().map(workspace).collect();
    let mut listed: Vec<&str> = ids.iter().map(String::as_str).collect();
    listed.push("someone-elses-workspace");
    let cmux = stub_cmux(&dir, &listed);

    // The roles the ADR does not allow are refused, and each refusal is
    // recorded; nothing is closed.
    for env in [
        vec![
            ("DAGQ_ROLE", "worker"),
            ("DAGQ_ACTOR_ID", "worker:r1"),
            ("DAGQ_RUN_ID", runs[0].id().as_str()),
            ("DAGQ_TASK_ID", "1"),
        ],
        vec![("DAGQ_ROLE", "planner"), ("DAGQ_ACTOR_ID", "planner:1")],
        vec![("DAGQ_ROLE", "supervisor")],
        vec![("DAGQ_ROLE", "recovery-job")],
        vec![("DAGQ_ROLE", "observer")],
    ] {
        let output = cleanup(&env, &db, &cmux, &["--apply"]);
        assert!(!output.status.success(), "{env:?}");
        let error: Value = serde_json::from_slice(&output.stderr).unwrap();
        assert_eq!(
            error["denied"]["capability"], "workspace.cleanup",
            "{env:?}"
        );
    }
    let denials = Connection::open(&db)
        .unwrap()
        .query_row(
            "SELECT COUNT(*) FROM run_events WHERE kind='authorization_denied'
             AND json_extract(payload,'$.capability')='workspace.cleanup'",
            [],
            |row| row.get::<_, i64>(0),
        )
        .unwrap();
    assert_eq!(denials, 5);
    assert!(closed(&dir).is_empty());

    // A dry run by default: it lists the sweep's run and closes nothing.
    let report = cleaned(&[], &db, &cmux, &[]);
    assert_eq!(report["dry_run"], true);
    assert_eq!(
        outcomes(&report),
        [(runs[0].id().to_string(), "would_close".to_owned())]
    );
    assert_eq!(report["workspaces"][0]["workspace_id"], workspace(&runs[0]));
    assert_eq!(report["workspaces"][0]["task_id"], 1);
    assert!(closed(&dir).is_empty());
    assert!(closes_of(&queue, &runs[0]).is_empty());

    // A live run named is refused.
    let output = cleanup(&[], &db, &cmux, &[runs[2].id().as_str(), "--apply"]);
    assert!(!output.status.success());
    let error: Value = serde_json::from_slice(&output.stderr).unwrap();
    assert!(
        error["error"].as_str().unwrap().contains("is running"),
        "{error}"
    );
    // Of a task it is skipped.
    let report = cleaned(&[], &db, &cmux, &["--task", "3", "--apply"]);
    assert_eq!(outcomes(&report), []);
    assert_eq!(report["skipped"][0]["run_id"], runs[2].id().as_str());
    assert!(closed(&dir).is_empty());

    // The inbox closes all: the sweep's run only, as the inbox.
    let inbox = [("DAGQ_ROLE", "inbox"), ("DAGQ_ACTOR_ID", "inbox")];
    let report = cleaned(&inbox, &db, &cmux, &["--apply"]);
    assert_eq!(report["dry_run"], false);
    assert_eq!(
        outcomes(&report),
        [(runs[0].id().to_string(), "closed".to_owned())]
    );
    assert_eq!(closed(&dir), [workspace(&runs[0])]);
    let closes = closes_of(&queue, &runs[0]);
    assert_eq!(closes.len(), 1);
    assert_eq!(
        closes[0].payload,
        json!({"workspace_id": workspace(&runs[0]), "by": "inbox", "reason": "cleanup"})
    );
    assert_eq!(
        serde_json::to_value(&closes[0].actor).unwrap(),
        json!({"role": "inbox", "id": "inbox"})
    );
    assert!(
        queue
            .run(runs[0].id())
            .unwrap()
            .workspace_closed_at()
            .is_some()
    );
    // cmux no longer lists it: another pass closes nothing.
    let report = cleaned(&inbox, &db, &cmux, &["--apply"]);
    assert_eq!(outcomes(&report), []);

    // The triage's run, by its task, as the user.
    let report = cleaned(&[], &db, &cmux, &["--task", "2", "--apply"]);
    assert_eq!(
        outcomes(&report),
        [(runs[1].id().to_string(), "closed".to_owned())]
    );
    let closes = closes_of(&queue, &runs[1]);
    assert_eq!(closes[0].payload["by"], "user");
    assert_eq!(
        serde_json::to_value(&closes[0].actor).unwrap(),
        json!({"role": "user", "id": "user"})
    );

    // The inbox's, the supervisor's and the planners' workspaces are never
    // closed, named by their run or by their task either; the bulk passes
    // above left them too.
    for (task, run) in runs.iter().enumerate().skip(3) {
        let task = (task + 1).to_string();
        for args in [
            vec![run.id().as_str()],
            vec![run.id().as_str(), "--apply"],
            vec!["--task", task.as_str(), "--apply"],
        ] {
            let report = cleaned(&[], &db, &cmux, &args);
            assert_eq!(outcomes(&report), [], "{args:?}");
            assert_eq!(report["skipped"], json!([]), "{args:?}");
        }
        assert!(closes_of(&queue, run).is_empty(), "{}", run.id());
    }
    let report = cleaned(&inbox, &db, &cmux, &["--apply"]);
    assert_eq!(outcomes(&report), []);
    assert_eq!(closed(&dir), [workspace(&runs[0]), workspace(&runs[1])]);
    assert!(closes_of(&queue, &runs[2]).is_empty());
}
