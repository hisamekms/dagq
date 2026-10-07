//! The files the plan review expects each task to touch, by the rule of
//! the claim's deferral: an in-progress task adds what its run changed
//! (task 635), and a task that declares only globs takes the landings of
//! its most related completed tasks (ADR-t1981-1).

use crate::common::Bounded;
use crate::plan_review::{
    PlanWorkspace, StubReviewer, add_paths, add_text, fixture, git, submit, supervise,
};
use dagq::{
    application::TaskStore,
    domain::{NewTask, Priority, TaskAction, TaskId},
    infrastructure::{git_binary::git_executable, sqlite::SqliteQueue},
};
use rusqlite::Connection;
use serde_json::{Value, json};
use std::{fs, process::Command};

/// Task 635: an in-progress task is expected to touch what its run
/// changed (ADR-0069 decision 2), not only its declared paths: its run's
/// branch edits the hotspot `seed.txt` it does not declare, and the prompt
/// lists that file for it and names it on the hotspot. The run has no
/// lease: the supervisor recovers it and its recovery job cannot start,
/// which leaves the task in progress with the run as its latest.
#[test]
fn an_in_progress_tasks_expected_files_are_what_its_run_changed() {
    let fx = fixture();
    let mut queue = SqliteQueue::open(&fx.db).unwrap();
    // The one task without the draft blocker, so the only one claimed.
    let running = queue
        .add(NewTask {
            change: None,
            title: "poiuytrewq running".into(),
            description: "d".into(),
            acceptance: "a".into(),
            verification_commands: vec!["true".into()],
            required_evidence: Vec::new(),
            paths: vec!["other.txt".into()],
            priority: Some(Priority::Normal),
            dependencies: Vec::new(),
            goal_dependencies: Vec::new(),
            goal_id: None,
            context: String::new(),
            provider: None,
            worker_mode: Some(dagq::domain::worker::WorkerMode::Interactive),
            wait_for_build: false,
        })
        .unwrap()
        .id();
    queue.transition(running, TaskAction::BypassReview).unwrap();
    let head = String::from_utf8(
        Command::new(git_executable().expect("git executable"))
            .arg("-C")
            .arg(&fx.repo)
            .args(["rev-parse", "HEAD"])
            .bounded_output()
            .unwrap()
            .stdout,
    )
    .unwrap();
    let base = dagq::domain::CommitSha::parse(head.trim(), "base commit").unwrap();
    let dagq::domain::ClaimOutcome::Claimed { run } = queue.claim(&base).unwrap() else {
        panic!("task {running} was not claimed");
    };
    assert_eq!(run.task_id(), running);
    // The run's branch changes the hotspot, a file its task does not declare.
    git(&fx.repo, &["checkout", "-b", "dagq/running"]);
    fs::write(fx.repo.join("seed.txt"), "changed by the run\n").unwrap();
    git(&fx.repo, &["commit", "-am", "the run's change"]);
    git(&fx.repo, &["checkout", "main"]);
    let conn = Connection::open(&fx.db).unwrap();
    conn.execute(
        "UPDATE task_runs SET branch='dagq/running' WHERE id=?1",
        [run.id().as_str()],
    )
    .unwrap();
    let own = add_paths(&mut queue, "zxcvbnm own", &["seed.txt"]);
    submit(&mut queue, &[own], None);
    conn.execute(
        "INSERT INTO run_events(task_id, kind, payload) VALUES (1, 'conflict_precheck', ?1)",
        [json!({"main": "m", "conflicts": ["seed.txt"]}).to_string()],
    )
    .unwrap();
    let reviewer = StubReviewer::new(&[
        json!({"verdict": "pass", "reasons": [], "summary": "sound", "actions": []}),
    ]);
    let outcome = supervise(&fx, &PlanWorkspace::default(), &reviewer);
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    let prompt = reviewer.prompts().remove(0);
    let lines: Vec<Value> = prompt
        .lines()
        .filter_map(|line| serde_json::from_str(line).ok())
        .collect();
    let summary = lines
        .iter()
        .find(|line| line["id"] == json!(running) && line.get("expected_files").is_some())
        .unwrap_or_else(|| panic!("{prompt}"));
    assert_eq!(summary["status"], "in_progress", "{summary}");
    assert_eq!(
        summary["expected_files"],
        json!(["other.txt", "seed.txt"]),
        "{summary}"
    );
    let hot = lines
        .iter()
        .find(|line| line["path"] == "seed.txt" && line.get("conflicts").is_some())
        .unwrap_or_else(|| panic!("no hotspot in {prompt}"));
    assert_eq!(hot["queued_tasks"], json!([running]), "{hot}");
    assert_eq!(hot["proposal_tasks"], json!([own]), "{hot}");
}

/// Task 1981: a task that declares only globs is expected to touch what
/// the landings of its most related completed tasks changed, as one that
/// declares no paths (ADR-t1981-1); one that also declares a concrete path
/// is expected to touch only that path.
#[test]
fn a_task_declaring_only_globs_is_expected_to_touch_its_related_landings() {
    let fx = fixture();
    let mut queue = SqliteQueue::open(&fx.db).unwrap();
    fs::create_dir_all(fx.repo.join("docs")).unwrap();
    fs::write(fx.repo.join("docs/landed.md"), "landed\n").unwrap();
    git(&fx.repo, &["add", "docs/landed.md"]);
    git(&fx.repo, &["commit", "-m", "the related landing"]);
    let head = String::from_utf8(
        Command::new(git_executable().expect("git executable"))
            .arg("-C")
            .arg(&fx.repo)
            .args(["rev-parse", "HEAD"])
            .bounded_output()
            .unwrap()
            .stdout,
    )
    .unwrap();
    let done = add_text(
        &mut queue,
        "docs: glossary of claim deferral words",
        "rewrites docs/glossary.md",
        "the glossary reads well",
    );
    let raw = Connection::open(&fx.db).unwrap();
    raw.execute_batch("PRAGMA foreign_keys = OFF").unwrap();
    raw.execute(
        "UPDATE tasks SET status = 'completed' WHERE id = ?1",
        [done.as_i64()],
    )
    .unwrap();
    raw.execute(
        "INSERT INTO landed_commits (run_id, task_id, commit_sha, message, landed_at)
         VALUES ('run-landed', ?1, ?2, 'docs: glossary of claim deferral words', 'now')",
        rusqlite::params![done.as_i64(), head.trim()],
    )
    .unwrap();
    let globs = add_paths(
        &mut queue,
        "docs: rewrite docs/glossary.md again",
        &["docs/**", "*.md"],
    );
    let mixed = add_paths(
        &mut queue,
        "docs: rewrite docs/glossary.md, mixed",
        &["docs/**", "other.txt"],
    );
    submit(&mut queue, &[globs, mixed], None);
    let reviewer = StubReviewer::new(&[
        json!({"verdict": "pass", "reasons": [], "summary": "sound", "actions": []}),
    ]);
    let outcome = supervise(&fx, &PlanWorkspace::default(), &reviewer);
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    let prompt = reviewer.prompts().remove(0);
    let expected = |task: TaskId| {
        prompt
            .lines()
            .filter_map(|line| serde_json::from_str::<Value>(line).ok())
            .find(|line| line["task_id"] == json!(task) && line.get("expected_files").is_some())
            .unwrap_or_else(|| panic!("{prompt}"))["expected_files"]
            .clone()
    };
    assert_eq!(expected(globs), json!(["docs/landed.md"]), "{prompt}");
    assert_eq!(expected(mixed), json!(["other.txt"]), "{prompt}");
}
