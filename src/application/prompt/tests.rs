use super::*;
use crate::application::memory_files::MemoryFiles;
use crate::application::review::{LANDING_ANSWERS_INTRO, PersonAnswer, PersonAnswers};
use crate::domain::local_checks::LocalChecks;
use crate::domain::worker::WorkerMode;
use crate::domain::{
    EvidenceCheck, GoalRecord, GoalStatus, GoalVerdict, Provider, RunId, RunRecord, TaskRecord,
    TaskStatus,
};
use serde_json::json;
use std::collections::BTreeSet;
use std::time::UNIX_EPOCH;

const SHA: &str = "1111111111111111111111111111111111111111";
const RUN: &str = "00000000-0000-4000-8000-000000000001";

#[test]
fn landed_lines_list_the_newest_tasks_by_title_up_to_the_cap() {
    let landed = |count: i64| -> Vec<LandedTask> {
        (1..=count)
            .map(|id| LandedTask {
                task_id: TaskId::new(id),
                title: format!("title {id}"),
            })
            .collect()
    };
    assert_eq!(
        landed_lines("main", "b", "m", &[]),
        ["Tasks landed on main since your base: none."]
    );

    let header = "Tasks landed on main since your base, newest first (git log b..m and git show <commit> tell what each changed):";
    let few = landed_lines("main", "b", "m", &landed(3));
    assert_eq!(
        few,
        [
            header,
            "- task 3: title 3",
            "- task 2: title 2",
            "- task 1: title 1"
        ]
    );

    let full = landed_lines("main", "b", "m", &landed(LANDED_TASK_LINES as i64));
    assert_eq!(full.len(), 1 + LANDED_TASK_LINES);
    assert_eq!(full[1], "- task 20: title 20");
    assert_eq!(full[LANDED_TASK_LINES], "- task 1: title 1");

    let many = landed_lines("main", "b", "m", &landed(23));
    assert_eq!(many.len(), 2 + LANDED_TASK_LINES);
    assert_eq!(many[0], header);
    assert_eq!(many[1], "- task 23: title 23");
    assert_eq!(many[LANDED_TASK_LINES], "- task 4: title 4");
    assert_eq!(
        many[LANDED_TASK_LINES + 1],
        "- … and 3 more; git log --oneline b..m lists them all"
    );
    assert!(many.iter().all(|line| !line.contains("summary")));
}

#[test]
fn landed_lines_cut_a_long_title_to_its_bytes_and_say_where_it_is_whole() {
    let at_cap = "t".repeat(LANDED_TITLE_BYTES);
    let fits = landed_lines(
        "main",
        "b",
        "m",
        &[LandedTask {
            task_id: TaskId::new(1),
            title: at_cap.clone(),
        }],
    );
    assert_eq!(fits[1], format!("- task 1: {at_cap}"));
    assert_eq!(fits.len(), 2);

    // Multibyte titles longer than the cap, the most lines, the
    // longest IDs, commit IDs and branch name the limit counts.
    let title = "長".repeat(1_000);
    let landed: Vec<LandedTask> = (0..LANDED_TASK_LINES as i64 + 5)
        .map(|n| LandedTask {
            task_id: TaskId::new(i64::MAX - n),
            title: title.clone(),
        })
        .collect();
    let branch = "b".repeat(256);
    let (base, main) = ("1".repeat(40), "2".repeat(40));
    let lines = landed_lines(&branch, &base, &main, &landed);
    assert_eq!(lines.len(), 1 + LANDED_TASK_LINES + 2);
    let section = lines.iter().map(|line| line.len() + 1).sum::<usize>();
    assert!(section <= LANDED_SECTION_BYTES, "{section}");
    for line in &lines[1..=LANDED_TASK_LINES] {
        let (_, kept) = line.split_once(": ").unwrap();
        assert!(kept.len() <= LANDED_TITLE_BYTES, "{}", kept.len());
        let (start, mark) = kept.split_once(LANDED_TITLE_MARK).unwrap();
        assert!(title.starts_with(start));
        assert_eq!(mark, format!("{} bytes]", title.len() - start.len()));
    }
    assert_eq!(
        lines[LANDED_TASK_LINES + 1],
        format!(
            "- titles ending … [+N bytes] are cut; each is whole as its commit's subject in git log {base}..{main}"
        )
    );
    assert_eq!(
        lines[LANDED_TASK_LINES + 2],
        format!("- … and 5 more; git log --oneline {base}..{main} lists them all")
    );
}

fn task(id: i64, title: &str, status: TaskStatus) -> Task {
    verified_task(id, title, status, Vec::new())
}

fn verified_task(
    id: i64,
    title: &str,
    status: TaskStatus,
    verification_commands: Vec<String>,
) -> Task {
    Task::restore(TaskRecord {
        goal_priority: None,
        id: TaskId::new(id),
        title: title.into(),
        description: String::new(),
        acceptance: String::new(),
        verification_commands,
        required_evidence: Vec::new(),
        paths: Vec::new(),
        priority: Default::default(),
        change: None,
        status,
        goal_id: None,
        context: String::new(),
        created_at: String::new(),
        updated_at: String::new(),
        worker: crate::domain::worker::Worker::CLAUDE_HEADLESS,
        named_mode: None,
        wait_for_build: false,
        execution_class: Default::default(),
    })
    .unwrap()
}

fn task_with_evidence(id: i64, required_evidence: Vec<EvidenceCheck>) -> Task {
    Task::restore(TaskRecord {
        goal_priority: None,
        id: TaskId::new(id),
        title: "work".into(),
        description: String::new(),
        acceptance: String::new(),
        verification_commands: Vec::new(),
        required_evidence,
        paths: Vec::new(),
        priority: Default::default(),
        change: None,
        status: TaskStatus::InProgress,
        goal_id: None,
        context: String::new(),
        created_at: String::new(),
        updated_at: String::new(),
        worker: crate::domain::worker::Worker::CLAUDE_HEADLESS,
        named_mode: None,
        wait_for_build: false,
        execution_class: Default::default(),
    })
    .unwrap()
}

fn run(task_id: i64, status: RunStatus, result_commit: Option<&str>) -> TaskRun {
    TaskRun::restore(run_record(task_id, status, result_commit)).unwrap()
}

fn run_record(task_id: i64, status: RunStatus, result_commit: Option<&str>) -> RunRecord {
    RunRecord {
        id: RunId::new(RUN).unwrap(),
        task_id: TaskId::new(task_id),
        status,
        requested_provider: Provider::Claude,
        actual_provider: Provider::Claude,
        worker_mode: crate::domain::worker::WorkerMode::Headless,
        base_commit: CommitSha::try_from(SHA).unwrap(),
        branch: Some(format!("dagq/{RUN}")),
        worktree_path: Some("/runs/run/worktree".into()),
        workspace_id: None,
        receipt_path: Some("/runs/run/receipt.json".into()),
        log_path: None,
        result_commit: result_commit.map(|sha| CommitSha::try_from(sha).unwrap()),
        repo_path: None,
        run_dir: Some("/runs/run".into()),
        last_error: None,
        workspace_closed_at: None,
        created_at: String::new(),
    }
}

#[test]
fn goal_dependencies_share_the_predecessor_section_with_short_summaries() {
    let files = MemoryFiles::default();
    let receipt = json!({
        "run_id": RUN,
        "result": "succeeded",
        "commit": SHA,
        "tests": {"status": "passed", "evidence_or_reason": "cargo test"},
        "e2e": {"status": "not_applicable", "evidence_or_reason": "none"},
        "subagent_review": {"status": "not_applicable", "evidence_or_reason": "small"},
        "summary": "word ".repeat(100),
    });
    files.put(
        Path::new("/runs/run/receipt.json"),
        UNIX_EPOCH,
        &receipt.to_string(),
    );
    let goal = Goal::restore(GoalRecord {
        priority: Default::default(),
        tags: Vec::new(),
        id: GoalId::new(4),
        title: "upstream goal".into(),
        description: String::new(),
        acceptance: String::new(),
        constraints: String::new(),
        doc: None,
        status: GoalStatus::Open,
        closed_at: Some("2026-09-25T00:00:00Z".into()),
        verdict: Some(GoalVerdict::Achieved),
        created_at: String::new(),
        updated_at: String::new(),
    })
    .unwrap();
    let landed = GoalPredecessorSummary::from_goal_predecessor(
        &files,
        &GoalPredecessor {
            goal: goal.clone(),
            tasks: vec![Predecessor {
                task: task(2, "upstream work", TaskStatus::Completed),
                integrated_run: Some(run(2, RunStatus::Integrated, Some(SHA))),
            }],
        },
    );
    let summary = landed.tasks[0].summary.clone();
    assert_eq!(summary.chars().count(), GOAL_TASK_SUMMARY_CHARS + 1);
    assert!(summary.ends_with('…'));
    let empty = GoalPredecessorSummary::from_goal_predecessor(
        &files,
        &GoalPredecessor {
            goal,
            tasks: Vec::new(),
        },
    );

    let waiting = task(9, "downstream", TaskStatus::InProgress);
    let own_run = run(9, RunStatus::Claimed, None);
    let text = prompt(
        &waiting,
        &own_run,
        None,
        &[],
        &[landed, empty],
        &[],
        None,
        &[],
        None,
    )
    .unwrap()
    .text;
    assert!(
        text.contains(&format!(
            "Predecessor tasks (their changes are already in your base commit):\n\
                 - goal 4 (closed as achieved): upstream goal; its completed tasks:\n  \
                 - task 2: upstream work; result commit {SHA}; summary: {summary}\n\
                 - goal 4 (closed as achieved): upstream goal; its completed tasks:\n  - none\n"
        )),
        "{text}"
    );
    let alone = prompt(&waiting, &own_run, None, &[], &[], &[], None, &[], None)
        .unwrap()
        .text;
    assert!(alone.contains("Predecessor tasks: none\n"));
    assert!(!alone.contains("Carried over from run"));
}

/// The worker and resume prompts tell the worker not to run the e2e,
/// which the runtime runs after the review (ADR-t1233-2): no command,
/// no marks and no exclusions for any provider. A run that cannot need
/// it reads nothing of the e2e.
#[test]
fn the_worker_and_resume_prompts_leave_the_e2e_to_the_runtime() {
    let own_run = run(7, RunStatus::Claimed, None);
    let e2e = ["tests/e2e.rs".to_owned()];
    let open = verified_task(7, "work", TaskStatus::InProgress, vec!["make gate".into()]);
    let line = "E2E: do not run the e2e yourself.";
    let codex = run_on(Provider::Codex, WorkerMode::Headless);
    for worker in [&own_run, &codex] {
        let text = prompt(&open, worker, None, &[], &[], &[], None, &e2e, None)
            .unwrap()
            .text;
        assert!(text.contains(line), "{text}");
        for gone in [
            "E2E evidence is decided by your diff",
            "E2E marks",
            "Codex worker E2E",
            "--skip",
            "cargo test --locked --test e2e",
        ] {
            assert!(!text.contains(gone), "{gone}: {text}");
        }
    }
    let none = prompt(&open, &own_run, None, &[], &[], &[], None, &[], None)
        .unwrap()
        .text;
    assert!(!none.contains("E2E"), "{none}");
    let required = task_with_evidence(8, vec![EvidenceCheck::E2e]);
    let text = prompt(&required, &own_run, None, &[], &[], &[], None, &[], None)
        .unwrap()
        .text;
    assert!(text.contains(line), "{text}");
    assert!(!text.contains("Required evidence"), "{text}");

    let request = ResumeRequest {
        main: CommitSha::try_from(SHA).unwrap(),
        branch: "main".into(),
        reason: "the e2e failed: a; see /runs/r/e2e-1.log".into(),
        kind: ResumeKind::E2e,
        reason_file: None,
        known_failures: None,
        carried_answers: Vec::new(),
        answers_file: None,
    };
    for worker in [&own_run, &codex] {
        let resumed = resume_request(&open, worker, &request, &[]).unwrap().text;
        assert!(
            resumed.contains("the e2e the runtime ran on the host before landing it failed"),
            "{resumed}"
        );
        assert!(resumed.contains("Reason: the e2e failed: a; see /runs/r/e2e-1.log"));
        assert!(
                resumed.contains("You may run a failed test by name to reproduce it, the way the repository's instructions (AGENTS.md or CLAUDE.md) say, but not the whole e2e"),
                "{resumed}"
            );
        assert!(!resumed.contains("Codex worker E2E"), "{resumed}");
        assert!(!resumed.contains("E2E marks"), "{resumed}");
    }
}

/// ADR-t1487-1 decisions 3 and 4: only a Spike's run is told how to
/// write its result and that the limit is its own to keep.
#[test]
fn only_a_spike_prompt_asks_for_the_spike_result() {
    let implementation = task_with_evidence(7, Vec::new());
    let spike = Task::restore(TaskRecord {
        goal_priority: None,
        id: TaskId::new(7),
        title: "work".into(),
        description: String::new(),
        acceptance: String::new(),
        verification_commands: Vec::new(),
        required_evidence: Vec::new(),
        paths: Vec::new(),
        priority: Default::default(),
        change: None,
        status: TaskStatus::InProgress,
        goal_id: None,
        context: String::new(),
        created_at: String::new(),
        updated_at: String::new(),
        worker: crate::domain::worker::Worker::CLAUDE_HEADLESS,
        named_mode: None,
        wait_for_build: false,
        execution_class: crate::domain::ExecutionClass::Spike,
    })
    .unwrap();
    let worker = run_on(Provider::Claude, WorkerMode::Headless);
    let text = |task: &Task| {
        prompt(task, &worker, None, &[], &[], &[], None, &[], None)
            .unwrap()
            .text
    };
    let spike = text(&spike);
    assert!(spike.contains(SPIKE_LINES), "{spike}");
    for verdict in ["holds", "does_not_hold", "unresolved"] {
        assert!(spike.contains(verdict), "{verdict}");
    }
    assert!(spike.contains("which the runtime does not enforce"));
    // Before the receipt contract, with the evidence lines.
    assert!(spike.find(SPIKE_LINES) < spike.find("Write a completion receipt"));
    let implementation = text(&implementation);
    assert!(!implementation.contains("spike_result"), "{implementation}");
    assert!(
        worker_template(Provider::Claude)
            .unwrap()
            .contains(SPIKE_LINES)
    );
}

/// Goal 52 acceptance (3): what the runtime tells a worker of building,
/// temporary files and the e2e names no build tool, build directory or
/// e2e file of one repository, on either provider and in every text the
/// session is sent; the worker on either provider still keeps its
/// temporary files under `$TMPDIR`, and the e2e stays the runtime's to run, reproduced by
/// name only on an e2e resume.
#[test]
fn worker_and_resume_prompts_name_no_cargo_build_or_e2e_specifics() {
    let open = verified_task(7, "work", TaskStatus::InProgress, vec!["make gate".into()]);
    let required = task_with_evidence(7, vec![EvidenceCheck::E2e]);
    let e2e = ["tests/e2e/**".to_owned()];
    for provider in [Provider::Claude, Provider::Codex] {
        let worker = run_on(provider, WorkerMode::Headless);
        let mut texts = session_texts(&open, &worker);
        texts.extend(session_texts(&required, &worker));
        texts.push(
            prompt(&open, &worker, None, &[], &[], &[], None, &e2e, None)
                .unwrap()
                .text,
        );
        for text in &texts {
            let lower = text.to_lowercase();
            for leak in ["cargo", "target/", "tests/e2e.rs", "--exact", "--ignored"] {
                assert!(!lower.contains(leak), "{provider:?} {leak}: {text}");
            }
            assert!(!text.contains("CARGO_TARGET_DIR"), "{provider:?}: {text}");
        }
        let first = &texts[0];
        assert!(first.contains("throwaway repositories"), "{first}");
        assert!(first.contains("under $TMPDIR (a directory the runtime made for this run"));
        assert!(first.contains("never directly in /tmp or /private/tmp"));
        let with_e2e = texts.last().unwrap();
        assert!(
            with_e2e.contains("E2E: do not run the e2e yourself."),
            "{with_e2e}"
        );
        assert!(with_e2e.contains("the runtime runs it on the host after the review passes"));
        assert!(with_e2e.contains("Report `e2e` in the receipt as not_applicable"));
        let request = ResumeRequest {
            main: CommitSha::try_from(SHA).unwrap(),
            branch: "main".into(),
            reason: "the e2e failed: a".into(),
            kind: ResumeKind::E2e,
            reason_file: None,
            known_failures: None,
            carried_answers: Vec::new(),
            answers_file: None,
        };
        let resumed = resume_request(&open, &worker, &request, &[]).unwrap().text;
        for step in [
            "fix the e2e tests that failed",
            "run a failed test by name to reproduce it, the way the repository's instructions (AGENTS.md or CLAUDE.md) say",
            "but not the whole e2e: the runtime runs it again after the review",
        ] {
            assert!(resumed.contains(step), "{step}: {resumed}");
        }
    }
}

/// A run in a container is asked to rebase onto the clone's ref main's
/// bundle reaches it as, never onto a commit its clone may not hold
/// (ADR-t2179-1 decision 6); a run on the host onto main's commit.
#[test]
fn a_container_runs_resume_names_the_clones_main_ref() {
    let task = verified_task(7, "work", TaskStatus::InProgress, vec!["make gate".into()]);
    let host = run(7, RunStatus::NeedsSession, None);
    let container = TaskRun::restore(RunRecord {
        worktree_path: Some(format!("/q/clones/{RUN}")),
        ..run_record(7, RunStatus::NeedsSession, None)
    })
    .unwrap();
    assert!(container.in_container());
    for kind in [
        ResumeKind::Landing,
        ResumeKind::Precheck,
        ResumeKind::SentBack,
        ResumeKind::Triage,
        ResumeKind::E2e,
        ResumeKind::SessionGone,
        ResumeKind::ScopeViolation,
        ResumeKind::Replan,
        ResumeKind::Hold,
    ] {
        let request = ResumeRequest {
            main: CommitSha::try_from(SHA).unwrap(),
            branch: "main".into(),
            reason: "why".into(),
            kind,
            reason_file: None,
            known_failures: None,
            carried_answers: Vec::new(),
            answers_file: None,
        };
        let text = resume_request(&task, &container, &request, &[])
            .unwrap()
            .text;
        assert!(
            text.contains(crate::domain::run_clone::CLONE_MAIN_REF),
            "{kind:?}: {text}"
        );
        let text = resume_request(&task, &host, &request, &[]).unwrap().text;
        assert!(
            !text.contains(crate::domain::run_clone::CLONE_MAIN_REF),
            "{kind:?}: {text}"
        );
    }
}

/// The worker, resume and revise prompts show the verification commands
/// as integrate's to run and send the session to the repository's own
/// instructions for its checks, with the verification commands as the
/// default (task 510).
#[test]
fn sessions_run_the_repository_checks_and_leave_the_verification_to_integrate() {
    let verified = verified_task(7, "work", TaskStatus::InProgress, vec!["make gate".into()]);
    let own_run = run(7, RunStatus::Claimed, None);
    let checks = "the repository's instructions (AGENTS.md or CLAUDE.md) ask a worker to run";

    let worker = prompt(&verified, &own_run, None, &[], &[], &[], None, &[], None)
        .unwrap()
        .text;
    assert!(worker.contains(
            "Verification commands (integrate runs them once after rebasing onto main; that run is the verification of record for the commit):\n[\n  \"make gate\"\n]\n"
        ));
    assert!(worker.contains(checks), "{worker}");
    assert!(worker.contains(
        "when the instructions name no such checks, run the verification commands above."
    ));
    assert!(!worker.contains("run in the worktree):"));
    assert!(worker.contains(
            "the worker section of the repository instructions (AGENTS.md or CLAUDE.md), the task context"
        ));
    let inheritance = Inheritance {
        run_id: RunId::new(RUN).unwrap(),
        base: CommitSha::try_from(SHA).unwrap(),
        head: SHA.into(),
        branch: None,
        receipt_path: None,
        summary: "earlier".into(),
        by_hand: None,
        replan: None,
    };
    let retried = prompt(
        &verified,
        &own_run,
        None,
        &[],
        &[],
        &[],
        Some(&inheritance),
        &[],
        None,
    )
    .unwrap()
    .text;
    let (before, carried) = retried.split_once("Carried over from run").unwrap();
    assert!(before.contains(checks));
    assert!(carried.contains("rerun your checks in the worktree as above"));
    assert!(carried.contains("its landing kept conflicting"));
    let by_hand = Inheritance {
        by_hand: Some(("inbox".into(), "the person chose retry_inherit".into())),
        ..inheritance.clone()
    };
    let retried = prompt(
        &verified,
        &own_run,
        None,
        &[],
        &[],
        &[],
        Some(&by_hand),
        &[],
        None,
    )
    .unwrap()
    .text;
    let (_, carried) = retried.split_once("Carried over from run").unwrap();
    assert!(carried.contains(
        "inbox carried its work over by hand after it ended (the person chose retry_inherit)"
    ));
    assert!(!carried.contains("its landing kept conflicting"));
    assert!(carried.contains("rerun your checks in the worktree as above"));
    // A child of a replacement takes only the part of the saved work its
    // plan named: the commits, or the uncommitted changes copied into its
    // run directory, verified as its own work.
    let replan = crate::application::prompt::ReplanCarried {
        operation: crate::domain::ReplanId::new(4),
        task: TaskId::new(7),
        carry: crate::domain::replan_plan::CarrySource::SnapshotHead,
        pinned_ref: "refs/dagq/replans/4".into(),
        files: Vec::new(),
    };
    let child = |replan: crate::application::prompt::ReplanCarried| {
        let carried = Inheritance {
            replan: Some(replan),
            ..inheritance.clone()
        };
        let text = prompt(
            &verified,
            &own_run,
            None,
            &[],
            &[],
            &[],
            Some(&carried),
            &[],
            None,
        )
        .unwrap()
        .text;
        text.split_once("Carried over from run")
            .unwrap()
            .1
            .to_owned()
    };
    // A child's container clone fetches the ref that pins the saved head,
    // a retry's the run's own.
    let pinned = Inheritance {
        replan: Some(replan.clone()),
        ..inheritance.clone()
    };
    assert_eq!(pinned.clone_ref(), "refs/dagq/replans/4");
    assert_eq!(
        inheritance.clone_ref(),
        format!("refs/dagq/runs/{}", inheritance.run_id)
    );
    let head = child(replan.clone());
    for expected in [
        "of task 7, which replan operation 4 replaced with this task",
        "kept as refs/dagq/replans/4",
        &format!("git cherry-pick {SHA}..{SHA}"),
        "rerun your checks in the worktree as above",
        "passed no review as this task's work",
    ] {
        assert!(head.contains(expected), "{expected}\n{head}");
    }
    assert!(!head.contains("its landing kept conflicting"), "{head}");
    let diff = child(crate::application::prompt::ReplanCarried {
        carry: crate::domain::replan_plan::CarrySource::SnapshotDiff,
        files: vec![(
            "/runs/r/replan-carry/index.diff".into(),
            "staged diff".into(),
        )],
        ..replan
    });
    assert!(
        diff.contains("/runs/r/replan-carry/index.diff (staged diff)"),
        "{diff}"
    );
    assert!(diff.contains("git apply --index"), "{diff}");
    assert!(!diff.contains("git cherry-pick"), "{diff}");

    let default = r#"when the instructions name no such checks, run the verification commands ["make gate"]."#;
    let reproduce = "If the reason is a verification command that failed after integrate's rebase, you may also run that command in the worktree";
    for kind in [
        ResumeKind::Landing,
        ResumeKind::EvidenceMissing,
        ResumeKind::SentBack,
        ResumeKind::ScopeViolation,
        ResumeKind::Precheck,
        ResumeKind::Triage,
        ResumeKind::SessionGone,
    ] {
        let request = ResumeRequest {
            main: CommitSha::try_from(SHA).unwrap(),
            branch: "main".into(),
            reason: "why".into(),
            kind,
            reason_file: None,
            known_failures: None,
            carried_answers: Vec::new(),
            answers_file: None,
        };
        let text = resume_request(&verified, &own_run, &request, &[])
            .unwrap()
            .text;
        assert!(text.contains(checks), "{kind:?}: {text}");
        assert!(text.contains(default), "{kind:?}: {text}");
        assert!(!text.contains("Rerun the verification commands"), "{text}");
        assert_eq!(
            text.contains(reproduce),
            kind == ResumeKind::Landing,
            "{kind:?}: {text}"
        );
    }

    let revise = revise_request(
        &verified,
        &own_run,
        (1, ReviseLimit::default()),
        &["fix it".into()],
        None,
    )
    .unwrap()
    .text;
    assert!(revise.contains(&format!("2. {}", local_checks(r#"["make gate"]"#))));
    assert!(revise.contains(default), "{revise}");
}

/// A task as plan review reads it, with long fields of the size of this
/// queue's (about 3 KB, task 591's measure of prompt 157).
fn long_task(id: i64, status: TaskStatus, paths: &[&str]) -> Task {
    classed_task(id, status, paths, Default::default())
}

/// [`long_task`] of the execution class `execution_class`.
fn classed_task(
    id: i64,
    status: TaskStatus,
    paths: &[&str],
    execution_class: crate::domain::ExecutionClass,
) -> Task {
    Task::restore(TaskRecord {
        goal_priority: None,
        id: TaskId::new(id),
        title: format!("task {id}"),
        description: format!("description of {id} ").repeat(100),
        acceptance: "acceptance ".repeat(60),
        verification_commands: vec!["cargo test".into()],
        required_evidence: Vec::new(),
        paths: paths.iter().map(|path| (*path).to_owned()).collect(),
        priority: Default::default(),
        change: None,
        status,
        goal_id: None,
        context: "context ".repeat(80),
        created_at: String::new(),
        updated_at: String::new(),
        worker: crate::domain::worker::Worker::CLAUDE_HEADLESS,
        named_mode: None,
        wait_for_build: false,
        execution_class,
    })
    .unwrap()
}

/// The material of a plan review prompt, as the tests vary it.
struct PlanCase {
    tasks: Vec<TaskDetail>,
    goals: Vec<Goal>,
    queued: Vec<TaskListItem>,
    left_out: usize,
    expected: BTreeMap<TaskId, Vec<String>>,
    precedents: Vec<Ask>,
    hotspots: Vec<ConflictHotspot>,
    candidates: Vec<DuplicateCandidates>,
    language: Option<crate::domain::language::Language>,
}

impl PlanCase {
    fn prompt(&self) -> PlanReviewPrompt {
        use crate::domain::{PlannerOrigin, PlannerOwner, ProposalRecord, ProposalStatus};
        let proposal = Proposal::restore(ProposalRecord {
            id: ProposalId::new(1),
            status: ProposalStatus::Submitted,
            owner: PlannerOwner {
                origin: PlannerOrigin::Person,
                workspace_id: None,
            },
            submitted_at: "now".into(),
            revise_count: 0,
            task_ids: self.tasks.iter().map(|detail| detail.task.id()).collect(),
            goal_ids: Vec::new(),
            created_at: String::new(),
            updated_at: String::new(),
        })
        .unwrap();
        plan_review_prompt(&PlanReviewMaterial {
            proposal: &proposal,
            tasks: &self.tasks,
            goals: &self.goals,
            lint: &[],
            others: &[],
            queued: &self.queued,
            queued_left_out: self.left_out,
            expected: &self.expected,
            precedents: &self.precedents,
            hotspots: &self.hotspots,
            candidates: &self.candidates,
            repo_root: Path::new("/repo"),
            language: self.language.as_ref(),
            replacement: None,
        })
        .unwrap()
    }
}

/// A task of the proposal as plan review reads it.
fn proposal_task(task: Task, dependencies: Vec<TaskId>) -> TaskDetail {
    TaskDetail {
        membership_judgements: Vec::new(),
        task,
        dependencies,
        goal_dependencies: Vec::new(),
        duplicate_of: None,
        cancel_reason: None,
        duplicates: Vec::new(),
        duplicate_reasons: Default::default(),
        runs: Vec::new(),
        events: Vec::new(),
        processes: Vec::new(),
        origin: None,
        follow_up_drafts: Vec::new(),
        revisit: None,
        replans: Vec::new(),
        hold: None,
        asks: Vec::new(),
    }
}

/// A ready task as the queue lists it.
fn queued_task(task: Task) -> TaskListItem {
    TaskListItem::new(task, vec![TaskId::new(1)], Vec::new(), None, None, true)
}

/// A file the landings conflicted in.
fn hotspot(path: &str) -> ConflictHotspot {
    ConflictHotspot {
        path: path.into(),
        conflicts: 3,
        tasks: 2,
        task_ids: Vec::new(),
        landings: Some(4),
        ratio: Some(0.75),
        last_conflict_at: "then".into(),
        state: "present",
        renamed_to: None,
        alert: true,
    }
}

/// The plan review material with `queued` ready tasks: the proposal's
/// task 1000 touches `src/hot.rs`, and so does the ready task 5.
fn plan_case(queued: i64, left_out: usize) -> PlanCase {
    use crate::domain::related::RelatedTask;
    let tasks = vec![proposal_task(
        long_task(1000, TaskStatus::Submitted, &["src/hot.rs"]),
        Vec::new(),
    )];
    let items: Vec<TaskListItem> = (1..=queued)
        .map(|id| {
            let paths: &[&str] = if id == 5 { &["src/*.rs"] } else { &[] };
            queued_task(long_task(id, TaskStatus::Ready, paths))
        })
        .collect();
    let mut expected = BTreeMap::new();
    expected.insert(TaskId::new(1000), vec!["src/hot.rs".to_owned()]);
    for item in &items {
        let files = match item.id.as_i64() {
            5 => vec!["src/*.rs".to_owned()],
            6 => (0..30).map(|n| format!("src/other{n}.rs")).collect(),
            _ => vec!["src/cold.rs".to_owned()],
        };
        expected.insert(item.id, files);
    }
    let candidates = vec![DuplicateCandidates {
        task_id: TaskId::new(1000),
        related: vec![RelatedTask {
            id: 7,
            status: "ready".into(),
            title: "task 7".into(),
            score: 1.0,
            clues: Vec::new(),
            duplicate_of: None,
        }],
        search: Vec::new(),
    }];
    PlanCase {
        tasks,
        goals: Vec::new(),
        queued: items,
        left_out,
        expected,
        precedents: Vec::new(),
        hotspots: vec![hotspot("src/hot.rs"), hotspot("src/cold.rs")],
        candidates,
        language: None,
    }
}

/// The plan review prompt of [`plan_case`], and the full text of its
/// queue.
fn plan_prompt(queued: i64, left_out: usize) -> (String, usize) {
    let case = plan_case(queued, left_out);
    let old_size = case
        .queued
        .iter()
        .map(|item| serde_json::to_string(item).unwrap().len())
        .sum();
    (case.prompt().text, old_size)
}

/// The reopen line gives the limit and asks for a send-back keeping the
/// registered tasks working when many would break (ADR-t2751-1).
#[test]
fn plan_review_reopens_a_few_tasks_and_sends_back_a_change_that_breaks_many() {
    let prompt = plan_case(1, 0).prompt().text;
    let line = prompt
        .lines()
        .find(|line| line.contains("name it in reopen"))
        .unwrap();
    for part in [
        &format!(
            "Reopen only the few tasks that have to change, at most {}",
            crate::domain::plan_review::REOPEN_LIMIT
        ),
        "when many registered tasks would break, reopen none and send this proposal back (revise) asking it to keep them working on its own side (compatibility)",
        "past the limit the runtime reopens none of them and sends this proposal back itself",
    ] {
        assert!(line.contains(part), "{part} in {line}");
    }
}

/// ADR-t1487-1: the checks of an unchecked premise taken as a fact (a
/// Spike missing included) and of an implementation made ready on an
/// unresolved premise are in every plan review prompt, an implementation
/// alone included; the checks of a Spike's form, of a run a worker can do
/// and of a further Spike against the goal's limit of Spike cycles only in
/// that of a proposal with a Spike. None names a value of a repository's
/// (no limit, no build tool).
#[test]
fn plan_review_asks_of_premises_always_and_of_spikes_only_with_one() {
    use crate::domain::ExecutionClass;
    let always = [
        "- a premise the plan rests on, not yet checked and taken as a fact",
        "belongs in a Spike task (execution_class spike) submitted first",
        "with no Spike for it, is a revise",
        "- an implementation task that rests on a premise not yet resolved and would become ready with this proposal",
        "A dependency on a Spike as the only thing that holds such an implementation back",
    ];
    let spike_only = [
        "- a Spike task (execution_class spike) of the proposal: its description or acceptance holds its question",
        "a negative result counting as a success",
        "- that a worker can run the Spike with its own permissions",
        "no manual work on a disposable queue, no setting applied by hand to production and no tool installed on the host",
        "- a further Spike (one that follows earlier Spikes whose results were replanned",
        "the goal's limit of Spike cycles",
        "one with no goal, or put in another goal, escapes the limit, a revise",
        "one past it here was a person's own submit, the person's decision on the limit: pass it on that point and do not ask again",
    ];
    for part in spike_only {
        assert!(SPIKE_CHECKS.contains(part), "{part} not in {SPIKE_CHECKS}");
    }
    assert!(
        !SPIKE_CHECKS.chars().any(|c| c.is_ascii_digit()),
        "{SPIKE_CHECKS}"
    );
    for word in ["cargo", "Rust", "clippy", "nextest"] {
        assert!(!SPIKE_CHECKS.contains(word), "{word} in {SPIKE_CHECKS}");
    }
    // An implementation alone, with no Spike for what it rests on.
    let alone = plan_case(1, 0).prompt().text;
    // A Spike with an implementation beside it.
    let mut case = plan_case(1, 0);
    case.tasks = vec![
        proposal_task(
            classed_task(1000, TaskStatus::Submitted, &[], ExecutionClass::Spike),
            Vec::new(),
        ),
        proposal_task(long_task(1001, TaskStatus::Submitted, &[]), Vec::new()),
    ];
    let with_spike = case.prompt().text;
    for part in always {
        assert!(alone.contains(part), "{part} not in {alone}");
        assert!(with_spike.contains(part), "{part} not in {with_spike}");
    }
    assert!(with_spike.contains(SPIKE_CHECKS), "{with_spike}");
    for part in spike_only {
        assert!(!alone.contains(part), "{part} in {alone}");
    }
    // Among the meaning checks, before the lint.
    assert!(
        with_spike.find(SPIKE_CHECKS)
            < with_spike.find("- every finding of `dagq lint` is one to fix")
    );
    // The checks asked of every proposal name no value either.
    for start in [always[0], always[3]] {
        let line = alone.lines().find(|line| line.starts_with(start)).unwrap();
        assert!(!line.chars().any(|c| c.is_ascii_digit()), "{line}");
    }
}

/// A follow_up of the proposal carries its membership judgements with
/// the version check, another task none, and the prompt asks plan
/// review to start from the planner's mapping and read the evidence
/// around a doubtful one (ADR-t1504-2 decision 7).
#[test]
fn plan_review_checks_follow_up_membership_from_the_planner_s_mapping() {
    use crate::domain::{BundleKey, DraftOrigin, TaskOrigin};
    let mut case = plan_case(1, 0);
    let material = serde_json::json!({"source_task_id": 3, "source_run_id": "r",
            "source_goal_id": 9, "source_goal_state": "open", "source_goal_provenance": "recorded",
            "membership_proposal": {"classification": "required", "acceptance_items": ["(1)"], "reason": "w"}});
    case.tasks[0].origin = Some(TaskOrigin {
        origin: DraftOrigin::FollowUp,
        material: material.clone(),
        source_task_id: Some(TaskId::new(3)),
        source_run_id: Some("r".into()),
        index: Some(0),
        bundle_key: BundleKey::of(DraftOrigin::FollowUp, &material, TaskId::new(1000)),
        bundles: Vec::new(),
    });
    case.tasks[0].membership_judgements = vec![serde_json::json!({
            "id": 4, "classification": "out_of_scope", "acceptance_items": ["(2)"],
            "reason": "(2) holds without it", "evidence": ["receipt:r"],
            "destination_goal_id": 10, "destination_acceptance_items": ["(3) of goal 10"],
            "acceptance_version": 1,
            "current_acceptance_version": 2, "needs_recheck": true})];
    case.tasks.push(proposal_task(
        long_task(1001, TaskStatus::Submitted, &[]),
        Vec::new(),
    ));
    let prompt = case.prompt().text;
    let task = |id: i64| -> Value {
        prompt
            .lines()
            .filter_map(|line| serde_json::from_str::<Value>(line).ok())
            .find(|line| line["id"] == id && line.get("acceptance").is_some())
            .unwrap_or_else(|| panic!("no task {id} in {prompt}"))
    };
    let membership = &task(1000)["follow_up_membership"];
    assert_eq!(membership["origin"]["source_goal_id"], 9);
    // The worker's proposal reaches plan review as written (task 1508).
    assert_eq!(
        membership["origin"]["membership_proposal"],
        serde_json::json!({"classification": "required", "acceptance_items": ["(1)"], "reason": "w"})
    );
    assert_eq!(membership["latest_classification"], "out_of_scope");
    assert_eq!(membership["latest_version_matches"], false);
    let row = &membership["judgements"][0];
    for field in [
        "acceptance_items",
        "reason",
        "evidence",
        "destination_goal_id",
    ] {
        assert!(!row[field].is_null(), "{field}");
    }
    // The destination's items a planner named reach plan review to check
    // against the destination's acceptance (ADR-t3325-1).
    assert_eq!(
        row["destination_acceptance_items"],
        serde_json::json!(["(3) of goal 10"])
    );
    assert_eq!(
        (
            row["acceptance_version"].clone(),
            row["current_acceptance_version"].clone()
        ),
        (serde_json::json!(1), serde_json::json!(2))
    );
    assert!(task(1001).get("follow_up_membership").is_none());
    for instruction in [
        "start from the planner's mapping",
        "need not repeat the whole investigation",
        "never pass a judgement on its form alone",
        "for out_of_scope that each destination item is in the destination goal's acceptance",
        "the reason names no item of the acceptance",
        "the evidence disagrees with the receipt or the diff",
        "an unrelated catch-all goal",
        "the acceptance was weakened",
        "read the evidence around it",
    ] {
        assert!(prompt.contains(instruction), "{instruction}");
    }
    // Where each task and goal comes from and who set its priority, by
    // their records, and what plan review does with a person's
    // (ADR-t1971-1, ADR-t1975-1 decisions 3 and 7).
    assert!(
            prompt.contains(
                "Each task of the proposal and each goal below records where it comes from (origin: human, by a person's request or a person's own add; ai, by a planner or the runtime on its own; unknown, from before the queue recorded it, which counts as a person's) and who set its priority (priority_by: human for a person;"
            ),
            "{prompt}"
        );
    assert_eq!(
        (&task(1000)["origin"], &task(1000)["priority_by"]),
        (&serde_json::json!("unknown"), &serde_json::json!("ai"))
    );
    for instruction in [
        "- a priority a person set (priority_by human), and a task or goal of a person's (origin human or unknown): its priority and the goals its tasks belong to are the person's decision. Do not change them: the runtime refuses to change a priority a person set, and does not apply a lower_priority to it.",
        "When one looks wrong, say so in a concern with reason_category scope.",
        "- the AI's task (origin ai): it has no priority of its own and takes its goal's (a pass takes off an own priority the AI set)",
        "a new goal's priority follows the repository's rules for goal priorities",
        "each task put in an existing goal is needed by that goal's acceptance; when either does not hold, revise",
        "lower_priority (never raise one; on the AI's task it takes the task's own priority off, so the task takes its goal's)",
        "stays below high by the goal it belongs to, not by a priority of its tasks",
    ] {
        assert!(prompt.contains(instruction), "{instruction}");
    }
    for gone in [
        "keeps its tasks at normal or low",
        "a pass lowers any you miss",
        "It comes from",
        "a person's proposal",
    ] {
        assert!(!prompt.contains(gone), "{gone}");
    }
}

#[test]
fn plan_review_lists_the_queue_in_summary_and_in_full_only_what_it_meets() {
    let (prompt, _) = plan_prompt(10, 3);
    let lines: Vec<Value> = prompt
        .lines()
        .filter_map(|line| serde_json::from_str(line).ok())
        .collect();
    let summary = |id: i64| {
        lines
            .iter()
            .find(|line| line["id"] == id && line.get("expected_files").is_some())
            .unwrap_or_else(|| panic!("no summary of {id} in {prompt}"))
    };
    let full: Vec<i64> = lines
        .iter()
        .filter(|line| line.get("description").is_some() && line["id"] != 1000)
        .map(|line| line["id"].as_i64().unwrap())
        .collect();
    // In full: the related candidate (7) and the task on the proposal's
    // hotspot (5); not the ones on the hotspot nobody of the proposal
    // touches.
    assert_eq!(full, [5, 7]);
    assert_eq!(summary(5)["full_text_below"], true);
    assert_eq!(summary(5)["paths"], json!(["src/*.rs"]));
    assert_eq!(summary(2).get("full_text_below"), None);
    assert_eq!(summary(2).get("description"), None);
    assert_eq!(summary(2)["expected_files"], json!(["src/cold.rs"]));
    assert_eq!(summary(2)["dependencies"], json!([1]));
    // A long list of expected files is cut, with the count left out.
    assert_eq!(
        summary(6)["expected_files"].as_array().unwrap().len(),
        SUMMARY_EXPECTED_FILES
    );
    assert_eq!(summary(6)["more_expected_files"], 20);
    // Each hotspot with the tasks expected to touch it.
    let hot = |path: &str| {
        lines
            .iter()
            .find(|line| line["path"] == path)
            .unwrap_or_else(|| panic!("no {path} in {prompt}"))
    };
    assert_eq!(hot("src/hot.rs")["proposal_tasks"], json!([1000]));
    assert_eq!(hot("src/hot.rs")["queued_tasks"], json!([5]));
    assert_eq!(hot("src/cold.rs")["proposal_tasks"], json!([]));
    assert_eq!(
        hot("src/cold.rs")["queued_tasks"].as_array().unwrap().len(),
        9
    );
    assert!(
            prompt.contains("(3 more ready or in-progress tasks, those of the lowest IDs, are left out of this list)"),
            "{prompt}"
        );
    assert!(prompt.contains("not pass but revise, saying in the reason which part to cut"));
    assert!(prompt.contains("Sharing a file or a hotspot alone is no reason for a dependency"));
    assert!(prompt.contains("only when its work needs the other's landing first"));
    assert!(prompt.contains(
        "A dependency of a task of a higher-priority goal on a task of a lower-priority goal"
    ));
    assert!(
        prompt
            .contains("revise when it only avoids a conflict on the same files or gives no reason")
    );
    assert!(!prompt.contains(
        "add a dependency (add_dependency, the task of the proposal waiting for the queued one)"
    ));
    assert!(
        !prompt
            .contains("for each hotspot whose proposal_tasks and queued_tasks are both non-empty")
    );
    let (prompt, _) = plan_prompt(4, 0);
    assert!(!prompt.contains("are left out of this list"), "{prompt}");
}

/// The prompts carry no rules of dagq's own repository (its ADRs, a
/// Rust linter): they send each session to the repository's rules, and
/// say where to find them when the repository has no AGENTS.md.
#[test]
fn prompts_take_the_rules_from_the_repository_in_order() {
    let db = Path::new("/q/queue.db");
    let (plan_review, _) = plan_prompt(4, 0);
    let revise = runtime_planner_prompt(
        db,
        ProposalId::new(3),
        &[],
        &["fix".into()],
        Some(TaskId::new(42)),
        Carried::default(),
    )
    .unwrap()
    .text;
    let review = review_prompt(
        &task(7, "work", TaskStatus::InProgress),
        &run(7, RunStatus::Succeeded, Some(SHA)),
        "/r/review.md",
        None,
        None,
        &PersonAnswers::default(),
    )
    .text;
    let order = "in this order: its AGENTS.md; without one, its CLAUDE.md; without either, what its README, CI configuration and build configuration show; when none of them settles it, ";
    assert!(
            revise.contains(&format!(
                "{order}decide them yourself from the source, the decisions the repository records and a person's precedents, and ask a person with a `planner_question` ask"
            )),
            "{revise}"
        );
    assert!(plan_review.contains("Where the repository has no AGENTS.md, judge a task's verification, paths and evidence in this order: CLAUDE.md; then what the README, the CI configuration and the build configuration show; when none of them settles it, it needs a person: a concern."));
    assert!(
        plan_review.contains(
            "the documents and rules they name (the plan review's part of them above all)"
        )
    );
    // A concern carries its recommendation and confidence, and what is
    // left to a person (ADR-t451-1 decisions 1 and 4).
    assert!(plan_review.contains("recommendation is ready"));
    assert!(
        plan_review.contains("- concern: findings that need a judgment beyond a planner's fix")
    );
    assert!(!plan_review.contains("- concern: findings that need a person's judgment: a doubtful"));
    assert!(plan_review.contains("confidence is high when"));
    assert!(plan_review.contains("reason_category is scope when"));
    assert!(plan_review.contains("\"reason_category\": \"scope\" | \"discard\" | null"));
    assert!(review.contains("findings of the repository's formatter, linter or other checks"));
    // A concern's recommendation, and what it leaves to a person
    // (ADR-t451-1 decisions 1 and 3).
    for part in [
        "\"recommendation\": \"land\" | \"send_back\" | null",
        "\"confidence\": \"high\" | \"low\" | null",
        "\"reason_category\": \"scope\" | \"discard\" | null",
        "Leave scope and discard to the person rather than deciding them; when in doubt, say low.",
        "- concern: findings that call for a judgment rather than a mechanical fix",
        "A concern does not by itself go to a person: you judge it below",
        "only the rest (low, scope, discard) reaches a person.",
    ] {
        assert!(review.contains(part), "{part} in {review}");
    }
    for text in [&plan_review, &revise, &review, &inbox_prompt(db).unwrap()] {
        for dagq_own in ["ADR", "docs/adr", "clippy"] {
            assert!(!text.contains(dagq_own), "{dagq_own} in {text}");
        }
    }
    assert!(!RECORD_READING.contains("ADR"));
}

#[test]
fn plan_review_prompt_of_a_queue_of_170_ready_tasks_is_a_fraction_of_their_full_text() {
    let (prompt, full_text) = plan_prompt(170, 0);
    println!(
        "prompt {} bytes, full text of the queue {full_text} bytes",
        prompt.len()
    );
    // Before task 591 the list alone was the full text of every task.
    assert!(full_text > 500_000, "{full_text}");
    assert!(
        prompt.len() * 8 < full_text,
        "{} bytes against {full_text}",
        prompt.len()
    );
}

/// A task whose description takes about `bytes`.
fn sized_task(id: i64, status: TaskStatus, bytes: usize) -> Task {
    Task::restore(TaskRecord {
        goal_priority: None,
        id: TaskId::new(id),
        title: format!("task {id}"),
        description: "d".repeat(bytes),
        acceptance: "acceptance".into(),
        verification_commands: vec!["cargo test".into()],
        required_evidence: Vec::new(),
        paths: Vec::new(),
        priority: Default::default(),
        change: None,
        status,
        goal_id: None,
        context: String::new(),
        created_at: String::new(),
        updated_at: String::new(),
        worker: crate::domain::worker::Worker::CLAUDE_HEADLESS,
        named_mode: None,
        wait_for_build: false,
        execution_class: Default::default(),
    })
    .unwrap()
}

/// The `dagq ...` commands a prompt names in backticks, each number
/// written `ID`.
fn named_commands(text: &str) -> BTreeSet<String> {
    let mut commands = BTreeSet::new();
    for part in text.split('`').skip(1).step_by(2) {
        if let Some(command) = part.strip_prefix("dagq ") {
            let mut normal = String::new();
            let mut in_number = false;
            for c in format!("dagq {command}").chars() {
                if c.is_ascii_digit() {
                    if !in_number {
                        normal.push_str("ID");
                    }
                    in_number = true;
                } else {
                    in_number = false;
                    normal.push(c);
                }
            }
            commands.insert(normal);
        }
    }
    commands
}

/// What a prompt's notes name to read beyond the commands any plan
/// review prompt names is one of [`PLAN_REVIEW_READS`].
fn assert_reads_are_the_jobs(prompt: &str) {
    let base = named_commands(&plan_prompt(4, 0).0);
    for command in named_commands(prompt) {
        assert!(
            base.contains(&command) || PLAN_REVIEW_READS.contains(&command.as_str()),
            "{command} is not a read of the plan review job"
        );
    }
}

/// The prompt's sections add up to its bytes, which keep to the limit.
fn assert_within_limit(prompt: &PlanReviewPrompt) {
    assert_eq!(prompt.bytes.total, prompt.text.len());
    assert_eq!(
        prompt.bytes.sections.values().sum::<usize>(),
        prompt.bytes.total
    );
    assert_eq!(prompt.bytes.limit, PLAN_REVIEW_PROMPT_LIMIT);
    assert!(
        prompt.bytes.total <= PLAN_REVIEW_PROMPT_LIMIT,
        "{:?}",
        prompt.bytes
    );
}

/// The JSON lines of `prompt` carrying `field`, by id.
fn lines_with(prompt: &str, field: &str) -> BTreeMap<i64, Value> {
    prompt
        .lines()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .filter(|line| line.get(field).is_some())
        .filter_map(|line| Some((line["id"].as_i64()?, line)))
        .collect()
}

/// Task 1561: the full text keeps to its limit of tasks, and takes the
/// most related first: one a task of the proposal depends on, then the
/// most hotspots shared, the best place among the candidates, the
/// newest; the rest are named with how to read them in full.
#[test]
fn plan_review_gives_in_full_the_most_related_tasks_first_within_its_limit() {
    use crate::domain::related::RelatedTask;
    let mut case = plan_case(0, 0);
    case.tasks = vec![proposal_task(
        long_task(1000, TaskStatus::Submitted, &[]),
        vec![TaskId::new(30)],
    )];
    case.expected.insert(
        TaskId::new(1000),
        vec!["src/hot.rs".to_owned(), "src/hot2.rs".to_owned()],
    );
    case.hotspots = vec![hotspot("src/hot.rs"), hotspot("src/hot2.rs")];
    case.candidates[0].related = vec![RelatedTask {
        id: 3,
        status: "ready".into(),
        title: "task 3".into(),
        score: 1.0,
        clues: Vec::new(),
        duplicate_of: None,
    }];
    case.queued = (1..=60)
        .rev()
        .map(|id| queued_task(long_task(id, TaskStatus::Ready, &[])))
        .collect();
    for id in 1..=60 {
        let files = if id == 10 || id == 11 {
            vec!["src/hot.rs".to_owned(), "src/hot2.rs".to_owned()]
        } else {
            vec!["src/hot.rs".to_owned()]
        };
        case.expected.insert(TaskId::new(id), files);
    }
    let prompt = case.prompt();
    assert_within_limit(&prompt);
    let full: Vec<i64> = prompt
        .text
        .lines()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .filter(|line| line.get("description").is_some() && line["id"] != 1000)
        .map(|line| line["id"].as_i64().unwrap())
        .collect();
    let mut expected = vec![30, 11, 10, 3];
    expected.extend((45..=60).rev());
    assert_eq!(full, expected);
    assert_eq!(full.len(), QUEUED_FULL_TASKS);
    let summaries = lines_with(&prompt.text, "expected_files");
    assert_eq!(summaries[&45]["full_text_below"], true);
    assert_eq!(summaries[&44].get("full_text_below"), None);
    assert_eq!(prompt.bytes.omitted["full_text"], 40);
    assert!(
            prompt.text.contains(&format!(
                "(40 more ready or in-progress tasks that meet this rule are left out by its limit of {QUEUED_FULL_TASKS} tasks and {QUEUED_FULL_BYTES} bytes, the most related to the proposal first: 44, 43, 42,"
            )),
            "{}",
            prompt.text
        );
    assert!(
        prompt
            .text
            .contains("read one in full with `dagq show ID --full`)")
    );
    assert!(prompt.text.contains("The material below is held to limits"));
    assert_reads_are_the_jobs(&prompt.text);
}

/// Task 1561: one huge task does not take the full text's room: it is
/// skipped and named, and the next ones fill the bytes left.
#[test]
fn a_huge_task_is_left_out_of_the_full_text_and_the_next_ones_fill_its_bytes() {
    let mut case = plan_case(0, 0);
    case.candidates.clear();
    case.queued = (1..=40)
        .rev()
        .map(|id| {
            let bytes = if id == 40 { 2_000_000 } else { 9_000 };
            queued_task(sized_task(id, TaskStatus::Ready, bytes))
        })
        .collect();
    for id in 1..=40 {
        case.expected
            .insert(TaskId::new(id), vec!["src/hot.rs".to_owned()]);
    }
    let prompt = case.prompt();
    assert_within_limit(&prompt);
    let full = lines_with(&prompt.text, "description");
    assert!(!full.contains_key(&40), "the huge task is not in full");
    let queued_full: usize = full
        .iter()
        .filter(|(id, _)| **id != 1000)
        .map(|(_, line)| line.to_string().len() + 1)
        .sum();
    assert!(queued_full <= QUEUED_FULL_BYTES, "{queued_full}");
    assert!(
        full.contains_key(&39) && full.len() > 5,
        "{:?}",
        full.keys()
    );
    assert!(prompt.bytes.sections["full_text"] <= QUEUED_FULL_BYTES + OMISSION_NOTE_BYTES);
    let summaries = lines_with(&prompt.text, "expected_files");
    assert_eq!(summaries[&40].get("full_text_below"), None);
    assert!(
        prompt
            .text
            .contains("the most related to the proposal first: 40, "),
        "{}",
        prompt.text
    );
}

/// Task 1561: a queue of hundreds of ready tasks that all touch the
/// proposal's hotspot, with long precedents, many hotspots and the
/// language's instruction, keeps to the overall limit: the summaries,
/// the precedents and the full text are cut, each saying how many and
/// how to read them.
#[test]
fn plan_review_prompt_of_hundreds_of_ready_tasks_on_a_shared_hotspot_keeps_to_its_limit() {
    let mut case = plan_case(600, 0);
    for id in 1..=600 {
        case.expected
            .insert(TaskId::new(id), vec!["src/hot.rs".to_owned()]);
    }
    case.hotspots = (0..15)
        .map(|n| hotspot(&format!("src/hot{n}.rs")))
        .collect();
    case.hotspots[0] = hotspot("src/hot.rs");
    case.precedents = (1..=30)
        .map(|id| {
            serde_json::from_value(json!({
                "id": id, "kind": "decide", "task_id": 5, "run_id": null,
                "question": "長い質問".repeat(200), "options": [],
                "answer": "長い答え".repeat(200), "asked_by": "worker",
                "reason_category": "scope", "created_at": 0,
                "answered_at": 1, "closed_at": null,
            }))
            .unwrap()
        })
        .collect();
    case.language = Some(crate::domain::language::Language {
        tag: "ja".into(),
        source: crate::domain::language::LanguageSource::Repository,
    });
    let prompt = case.prompt();
    assert_within_limit(&prompt);
    let instruction = crate::domain::language::instruction("ja");
    assert!(prompt.text.ends_with(&instruction));
    assert_eq!(prompt.bytes.sections["language"], instruction.len() + 2);
    assert_eq!(prompt.bytes.omitted["full_text"], 600 - QUEUED_FULL_TASKS);
    let shown = lines_with(&prompt.text, "expected_files").len();
    assert!(shown < 600);
    assert_eq!(prompt.bytes.omitted["summaries"], 600 - shown);
    assert!(prompt.bytes.sections["summaries"] <= QUEUED_SUMMARY_BYTES + OMISSION_NOTE_BYTES);
    assert!(prompt.text.contains(&format!(
            "({} more ready or in-progress tasks, the least related to the proposal, are left out of this list by its limit of {QUEUED_SUMMARY_BYTES} bytes; list them with `dagq list --status ready,in_progress --limit 200` and read one with `dagq show ID --full`)",
            600 - shown
        )));
    let quoted = prompt.text.matches("\n- precedent: ask ").count();
    assert!(quoted <= PRECEDENT_ASKS, "{quoted}");
    assert_eq!(prompt.bytes.omitted["precedents"], 30 - quoted);
    assert!(prompt.bytes.sections["precedents"] <= PRECEDENT_BYTES + OMISSION_NOTE_BYTES);
    assert!(prompt.text.contains("read them with `dagq asks --all`)"));
    assert!(prompt.text.contains("\"path\":\"src/hot14.rs\""));
    assert_reads_are_the_jobs(&prompt.text);
}

/// Task 1561: required sections over their limit (a huge task of the
/// proposal, a huge goal) are not cut silently: their largest pieces
/// are replaced by the command that reads them, which the job's role
/// may run, the prompt and `over_limit` say why, and the whole keeps to
/// its limit; a proposal of thousands of tasks becomes one note.
#[test]
fn required_sections_over_their_limit_are_read_with_the_commands_the_job_may_run() {
    let mut case = plan_case(10, 0);
    case.tasks = vec![
        proposal_task(
            sized_task(1000, TaskStatus::Submitted, 1_000_000),
            Vec::new(),
        ),
        proposal_task(long_task(1001, TaskStatus::Submitted, &[]), Vec::new()),
    ];
    case.goals = vec![
        Goal::restore(GoalRecord {
            priority: Default::default(),
            tags: Vec::new(),
            id: GoalId::new(4),
            title: "a huge goal".into(),
            description: "g".repeat(300_000),
            acceptance: String::new(),
            constraints: String::new(),
            doc: None,
            status: GoalStatus::Open,
            closed_at: None,
            verdict: None,
            created_at: String::new(),
            updated_at: String::new(),
        })
        .unwrap(),
    ];
    let prompt = case.prompt();
    assert_within_limit(&prompt);
    let reason = prompt.bytes.over_limit.as_deref().unwrap();
    assert!(
        reason.contains(&format!("over their limit of {PLAN_REVIEW_REQUIRED_LIMIT}")),
        "{reason}"
    );
    assert!(prompt.text.contains(reason));
    assert_eq!(prompt.bytes.omitted["tasks"], 1);
    assert_eq!(prompt.bytes.omitted["goals"], 1);
    let stubs = lines_with(&prompt.text, "read_with");
    assert_eq!(stubs[&1000]["read_with"], "dagq show 1000 --full");
    assert_eq!(stubs[&1000]["title"], "task 1000");
    assert_eq!(stubs[&4]["read_with"], "dagq goal show 4 --full");
    for stub in stubs.values() {
        let read = named_commands(&format!("`{}`", stub["read_with"].as_str().unwrap()));
        assert!(PLAN_REVIEW_READS.contains(&read.first().unwrap().as_str()));
    }
    // The smaller task of the proposal stays whole.
    assert!(prompt.text.contains("description of 1001"));
    assert!(
        prompt.bytes.sections["tasks"] + prompt.bytes.sections["goals"]
            < PLAN_REVIEW_REQUIRED_LIMIT
    );
    assert_reads_are_the_jobs(&prompt.text);

    case.tasks = (1000..4000)
        .map(|id| proposal_task(long_task(id, TaskStatus::Submitted, &[]), Vec::new()))
        .collect();
    let prompt = case.prompt();
    assert_within_limit(&prompt);
    assert_eq!(prompt.bytes.omitted["tasks"], 3000);
    assert!(prompt.text.contains(
            "(3000 tasks, left out: list them with `dagq proposal show 1` and read each with `dagq show ID --full`)"
        ));
    assert!(prompt.text.contains(
        "every submitted task of the proposal (3000 of them; `dagq proposal show 1` lists them)"
    ));
    assert_reads_are_the_jobs(&prompt.text);
}

/// Task 1561: with the required sections near their limit, the optional
/// ones share what is left of the overall limit in their order: the
/// summaries, last, get what the others left, and the whole keeps to
/// the limit.
#[test]
fn the_optional_sections_share_the_room_the_required_ones_leave() {
    use crate::domain::related::RelatedTask;
    let mut case = plan_case(0, 0);
    case.tasks = vec![proposal_task(
        sized_task(1000, TaskStatus::Submitted, 182_000),
        Vec::new(),
    )];
    case.queued = (1..=600)
        .rev()
        .map(|id| queued_task(sized_task(id, TaskStatus::Ready, 9_000)))
        .collect();
    for id in 1..=600 {
        case.expected
            .insert(TaskId::new(id), vec!["src/hot.rs".to_owned()]);
    }
    case.candidates[0].related = (1..=5)
        .map(|id| RelatedTask {
            id,
            status: "ready".into(),
            title: "t".repeat(9_000),
            score: 1.0,
            clues: Vec::new(),
            duplicate_of: None,
        })
        .collect();
    case.precedents = (1..=20)
        .map(|id| {
            serde_json::from_value(json!({
                "id": id, "kind": "decide", "task_id": 5, "run_id": null,
                "question": "q".repeat(400), "options": [],
                "answer": "a".repeat(400), "asked_by": "worker",
                "reason_category": "scope", "created_at": 0,
                "answered_at": 1, "closed_at": null,
            }))
            .unwrap()
        })
        .collect();
    let prompt = case.prompt();
    assert_within_limit(&prompt);
    assert_eq!(prompt.bytes.over_limit, None);
    assert!(prompt.bytes.sections["tasks"] > 182_000);
    assert!(
        prompt.bytes.sections["full_text"] > 90_000,
        "{:?}",
        prompt.bytes
    );
    assert!(
        prompt.bytes.sections["summaries"] < QUEUED_SUMMARY_BYTES,
        "{:?}",
        prompt.bytes
    );
    assert!(
        prompt.bytes.total > PLAN_REVIEW_PROMPT_LIMIT - 20_000,
        "{:?}",
        prompt.bytes
    );
    assert!(
        prompt
            .text
            .contains("are left out of this list by its limit of")
    );
}

/// ADR-t1433-2: the nudge is the next turn of a headless session, also
/// for a run recorded as interactive before, which is resumed headless.
#[test]
fn the_nudge_is_the_next_turn_whatever_mode_the_run_recorded() {
    let headless = stall_nudge(&run_on(Provider::Claude, WorkerMode::Headless))
        .unwrap()
        .text;
    assert!(headless.starts_with(HEADLESS_NEXT_TURN), "{headless}");
    assert!(
        headless.contains("Do one of these in this turn:"),
        "{headless}"
    );
    let recorded = stall_nudge(&run_on(Provider::Claude, WorkerMode::Interactive))
        .unwrap()
        .text;
    assert_eq!(recorded, headless);
}

/// Task 1372: the notice of a question closed without its answer says
/// who closed it and what was recorded with it (or that nothing was),
/// and that the worker decides or writes a failed receipt instead of
/// asking again, in this turn.
#[test]
fn the_notice_of_a_closed_question_names_its_closer_and_what_to_do() {
    let run = run_on(Provider::Claude, WorkerMode::Headless);
    let notice = closed_question_notice(&run, 5, Some("inbox"), Some("ask the planner"))
        .unwrap()
        .text;
    assert!(
            notice.contains(&format!(
                "\ndagq: ask 5 (your worker_question on run {RUN}) was closed by inbox without an answer delivered to you."
            )),
            "{notice}"
        );
    assert!(
        notice.contains("What was recorded with it when it was closed: ask the planner"),
        "{notice}"
    );
    assert!(notice.contains("Do not ask the same question again. Do one of these in this turn:"));
    assert!(notice.contains("decide it yourself"), "{notice}");
    assert!(notice.contains("write a failed receipt at /runs/run/receipt.json"));
    assert!(!notice.contains("dagq ask"), "{notice}");
    let headless = run_on(Provider::Codex, WorkerMode::Headless);
    let notice = closed_question_notice(&headless, 5, None, Some("  "))
        .unwrap()
        .text;
    assert!(
        notice.contains("closed by someone (not recorded)"),
        "{notice}"
    );
    assert!(notice.contains("No reason was recorded with the close."));
    assert!(notice.contains("Do one of these in this turn:"), "{notice}");
    assert!(
        notice.contains("hands the run to its recovery job"),
        "{notice}"
    );
    // Nothing a headless session is never told (task 817).
    assert!(notice.starts_with(HEADLESS_NEXT_TURN), "{notice}");
    assert!(notice.contains(HEADLESS_STOP), "{notice}");
    for never in ["/exit", "this terminal", "went idle"] {
        assert!(!notice.contains(never), "{never}: {notice}");
    }
}

/// The worker's template (goal 113) is the prompt with every section
/// and a placeholder for each value: no task's value is in it, so a
/// task's title or description never changes its hash, and a changed
/// fixed sentence does.
#[test]
fn the_worker_template_holds_every_section_and_no_tasks_values() {
    use crate::domain::instructions::template_hash;
    for provider in [Provider::Claude, Provider::Codex] {
        let template = worker_template(provider).unwrap();
        assert_eq!(worker_template(provider).unwrap(), template);
        for fixed in [
            "Task title: <title>",
            "Goal (the higher-level problem",
            "Context (why this task exists",
            "Predecessor tasks (their changes",
            "- goal 2 (closed as achieved): <goal predecessor title>",
            "Sibling tasks in progress (other tasks",
            "Carried over from run",
            "Required evidence:",
            "Paths you may change",
            "E2E: do not run the e2e",
            "Goal: none, this task stands alone",
            "Context: none",
            "Predecessor tasks: none",
            "Sibling tasks in progress: none",
            "<by> carried its work over by hand",
            WORKER_READING,
            ACCEPTANCE_MAP,
            DOCS_CHECK,
            HEADLESS_STOP,
            headless_provider_line(provider).as_str(),
        ] {
            assert!(template.contains(fixed), "{provider:?} {fixed}");
        }
        // A fixed sentence changed is another template.
        let hash = template_hash(&template);
        assert_ne!(
            template_hash(&template.replacen(DOCS_CHECK, "Then check.\n", 1)),
            hash
        );
        // The prompts of two tasks differ by their values; the template
        // is neither, and holds none of them.
        let first = task(7, "first title", TaskStatus::InProgress);
        let second = task(7, "second title", TaskStatus::InProgress);
        let run = run_on(provider, WorkerMode::Headless);
        let prompt_of = |task: &Task| {
            prompt(task, &run, None, &[], &[], &[], None, &[], None)
                .unwrap()
                .text
        };
        assert_ne!(prompt_of(&first), prompt_of(&second));
        for value in ["first title", "second title"] {
            assert!(!template.contains(value), "{value}");
        }
    }
    assert_ne!(
        worker_template(Provider::Claude).unwrap(),
        worker_template(Provider::Codex).unwrap()
    );
}

/// Task 7's claimed run, whose worker is `provider` in `mode`.
fn run_on(provider: Provider, mode: WorkerMode) -> TaskRun {
    TaskRun::restore(RunRecord {
        id: RunId::new(RUN).unwrap(),
        task_id: TaskId::new(7),
        status: RunStatus::Claimed,
        requested_provider: provider,
        actual_provider: provider,
        worker_mode: mode,
        base_commit: CommitSha::try_from(SHA).unwrap(),
        branch: Some(format!("dagq/{RUN}")),
        worktree_path: Some("/runs/run/worktree".into()),
        workspace_id: Some("ws".into()),
        receipt_path: Some("/runs/run/receipt.json".into()),
        log_path: None,
        result_commit: None,
        repo_path: None,
        run_dir: Some("/runs/run".into()),
        last_error: None,
        workspace_closed_at: None,
        created_at: String::new(),
    })
    .unwrap()
}

/// Every text a worker's session is sent: its prompt, each resolution
/// request, the revise, the receipt mismatch, the stale receipt, the
/// nudge, an answer and a recovery job's instruction.
fn session_texts(task: &Task, run: &TaskRun) -> Vec<String> {
    let mut texts = vec![
        prompt(task, run, None, &[], &[], &[], None, &[], None)
            .unwrap()
            .text,
    ];
    for kind in [
        ResumeKind::Landing,
        ResumeKind::EvidenceMissing,
        ResumeKind::SentBack,
        ResumeKind::ScopeViolation,
        ResumeKind::Precheck,
        ResumeKind::Triage,
        ResumeKind::Recheck,
        ResumeKind::SessionGone,
        ResumeKind::E2e,
        ResumeKind::Replan,
        ResumeKind::Hold,
    ] {
        let request = ResumeRequest {
            main: CommitSha::try_from(SHA).unwrap(),
            branch: "main".into(),
            reason: "why".into(),
            kind,
            reason_file: None,
            known_failures: None,
            carried_answers: Vec::new(),
            answers_file: None,
        };
        texts.push(resume_request(task, run, &request, &[]).unwrap().text);
    }
    texts.push(
        revise_request(
            task,
            run,
            (1, ReviseLimit::default()),
            &["fix it".into()],
            None,
        )
        .unwrap()
        .text,
    );
    texts.push(
        revise_mismatch_request(run, "the revise", "stale")
            .unwrap()
            .text,
    );
    let head = CommitSha::try_from("2222222222222222222222222222222222222222").unwrap();
    texts.push(stale_receipt_nudge(run, SHA, &head).unwrap().text);
    texts.push(stall_nudge(run).unwrap().text);
    texts.push(answer_text(run, 3, "blue").text);
    texts.push(recovery_instruction(run, "stalled", "write the receipt").text);
    texts.push(continue_text(run).text);
    texts
}

/// Acceptance (1) of task 817: a headless worker, on Claude or Codex,
/// is told to do its work in one turn, to end the turn with an ask when
/// it needs a decision and to read the repository's AGENTS.md; nothing
/// it is sent names /exit, a terminal typed into, or background work
/// left running for later.
#[test]
fn headless_sessions_are_told_to_finish_in_a_turn_and_never_about_exit() {
    let task = Task::restore(TaskRecord {
        goal_priority: None,
        id: TaskId::new(7),
        title: "work".into(),
        description: String::new(),
        acceptance: String::new(),
        verification_commands: vec!["make gate".into()],
        required_evidence: vec![EvidenceCheck::E2e, EvidenceCheck::SubagentReview],
        paths: Vec::new(),
        priority: Default::default(),
        change: None,
        status: TaskStatus::InProgress,
        goal_id: None,
        context: String::new(),
        created_at: String::new(),
        updated_at: String::new(),
        worker: crate::domain::worker::Worker::CLAUDE_HEADLESS,
        named_mode: None,
        wait_for_build: false,
        execution_class: Default::default(),
    })
    .unwrap();
    for provider in [Provider::Claude, Provider::Codex] {
        let run = run_on(provider, WorkerMode::Headless);
        let texts = session_texts(&task, &run);
        let first = &texts[0];
        assert!(first.contains(HEADLESS_WORKER), "{first}");
        assert!(first.contains(&headless_provider_line(provider)), "{first}");
        assert!(first.contains("Do the whole task in this turn"), "{first}");
        assert!(first.contains("report briefly that you asked, and end the turn"));
        assert!(first.contains("as the prompt of your next turn in this same session"));
        assert!(first.contains("AGENTS.md"), "{first}");
        assert!(first.contains(HEADLESS_STOP), "{first}");
        for text in &texts {
            for never in [
                "/exit",
                "this terminal",
                "to the terminal",
                "if any is left",
                "write here what you wait for",
                "went idle",
            ] {
                assert!(!text.contains(never), "{provider:?} {never}: {text}");
            }
            assert!(
                text.contains("in this turn") || text.contains("end the turn"),
                "{provider:?}: {text}"
            );
        }
        for request in &texts[1..15] {
            assert!(request.starts_with(HEADLESS_NEXT_TURN), "{request}");
            assert!(request.contains(HEADLESS_STOP), "{request}");
            assert!(request.contains("pkill"), "{request}");
            assert!(request.contains("AGENTS.md"), "{request}");
            assert!(
                request.contains(&format!("dagq ask --run {RUN} --kind worker_question")),
                "{request}"
            );
            assert!(request.contains("and end the turn"), "{request}");
        }
        assert!(texts[18].ends_with(HEADLESS_GO_ON));
        assert!(texts[15].starts_with(HEADLESS_NEXT_TURN));
        assert!(texts[15].contains("dagq: the previous turn of run"));
        assert!(texts[16].starts_with("answer to ask 3: blue\n\n"));
        assert!(texts[16].ends_with(HEADLESS_GO_ON));
        assert!(texts[17].starts_with("dagq: the supervisor's recovery job for run"));
        assert!(texts[17].ends_with(HEADLESS_GO_ON));
    }
    // Codex reviews its own diff and does not owe subagent_review.
    let codex = session_texts(&task, &run_on(Provider::Codex, WorkerMode::Headless));
    assert!(codex[0].contains("You have no subagent to review your change"));
    // No worker backs e2e (ADR-t1233-2).
    assert!(!codex[0].contains("Required evidence"), "{}", codex[0]);
    let claude = session_texts(&task, &run_on(Provider::Claude, WorkerMode::Headless));
    assert!(claude[0].contains("and subagent review."));
    assert!(claude[0].contains("Required evidence: subagent_review (each"));
    // A worker on either provider is told where its temporary files go:
    // under the `TMPDIR` the runtime gives its turns and removes after the
    // run.
    let tmp = "under $TMPDIR (a directory the runtime made for this run";
    for first in [&codex[0], &claude[0]] {
        assert!(first.contains(tmp), "{first}");
        assert!(first.contains("throwaway repositories"), "{first}");
        assert!(first.contains("never directly in /tmp or /private/tmp"));
        assert!(first.contains("the repository's instructions (AGENTS.md or CLAUDE.md) to say"));
        assert!(first.contains(RUN_TMP_LINE), "{first}");
    }
}

/// Task 1420 (ADR-t1420-1): every worker's prompt, on Claude or Codex,
/// maps the acceptance criteria before the receipt once, right before
/// the receipt's instructions; every
/// resume and revise request maps them again before it rewrites the
/// receipt; the Codex review line leaves the comparison with the
/// criteria to that step, and the run's review prompt is untouched.
#[test]
fn every_worker_text_that_writes_a_receipt_maps_the_acceptance_once() {
    let task = verified_task(7, "work", TaskStatus::InProgress, vec!["make gate".into()]);
    for (provider, mode) in [
        (Provider::Claude, WorkerMode::Headless),
        (Provider::Codex, WorkerMode::Headless),
    ] {
        let run = run_on(provider, mode);
        let texts = session_texts(&task, &run);
        let first = &texts[0];
        assert_eq!(first.matches(ACCEPTANCE_MAP).count(), 1, "{first}");
        assert!(
            first.contains(&format!(
                "{ACCEPTANCE_MAP}{DOCS_CHECK}Write a completion receipt to"
            )),
            "{first}"
        );
        assert_eq!(
            first.matches("map each acceptance criterion").count(),
            1,
            "{first}"
        );
        assert!(!first.contains(ACCEPTANCE_REMAP), "{first}");
        // The ten resume requests and the revise request.
        for request in &texts[1..12] {
            assert!(
                request.contains(&format!("renaming it. {ACCEPTANCE_REMAP}")),
                "{provider:?} {mode:?}: {request}"
            );
            assert!(!request.contains(ACCEPTANCE_MAP), "{request}");
        }
    }
    let codex = review_line(Route(Provider::Codex));
    assert!(
        !codex.contains("against the acceptance criteria"),
        "{codex}"
    );
    assert!(!codex.contains("map each acceptance criterion"), "{codex}");
    assert!(codex.contains("as you map the acceptance criteria to it"));
    // The steps ask for no new command, and stay short.
    for text in [ACCEPTANCE_MAP, ACCEPTANCE_REMAP] {
        assert!(text.len() <= 600, "{}", text.len());
        assert!(!text.contains("cargo") && !text.contains('`'), "{text}");
    }
    let review = review_prompt(
        &task,
        &run_on(Provider::Claude, WorkerMode::Headless),
        "r",
        None,
        None,
        &PersonAnswers::default(),
    )
    .text;
    assert!(!review.contains(ACCEPTANCE_MAP) && !review.contains(ACCEPTANCE_REMAP));
}

/// ADR-t1818-1 decision 1: every worker's prompt says once, after the
/// receipt's checks, how to report a base that already meets the
/// acceptance, and asks for no gate a worker does not run itself: the
/// runtime runs the whole verification on main.
#[test]
fn every_worker_prompt_says_how_to_report_a_base_already_satisfied() {
    let task = verified_task(7, "work", TaskStatus::InProgress, vec!["make gate".into()]);
    for provider in [Provider::Claude, Provider::Codex] {
        let run = run_on(provider, WorkerMode::Headless);
        let first = &session_texts(&task, &run)[0];
        assert_eq!(first.matches(ALREADY_SATISFIED).count(), 1, "{first}");
        assert!(
            first.contains(&format!(
                "a reason when not_applicable.\n{ALREADY_SATISFIED}follow_ups is optional"
            )),
            "{first}"
        );
        assert!(first.contains("\"result\":\"succeeded, failed or already_satisfied\""));
    }
    assert!(ALREADY_SATISFIED.contains("base_checks"));
    assert!(!ALREADY_SATISFIED.contains("cargo") && !ALREADY_SATISFIED.contains('`'));
}

/// Task 1508 (ADR-t1504-2 decision 11): every worker's prompt shows a
/// follow_up's membership proposal in the receipt's example and says
/// once how to write it, as a proposal and not a judgement; every
/// resume and revise request says it again in short.
#[test]
fn every_worker_text_that_writes_a_receipt_proposes_follow_up_membership() {
    let task = verified_task(7, "work", TaskStatus::InProgress, vec!["make gate".into()]);
    for (provider, mode) in [
        (Provider::Claude, WorkerMode::Headless),
        (Provider::Codex, WorkerMode::Headless),
    ] {
        let run = run_on(provider, mode);
        let texts = session_texts(&task, &run);
        let first = &texts[0];
        assert_eq!(first.matches(FOLLOW_UP_PROPOSAL).count(), 1, "{first}");
        assert!(
                first.contains(r#""membership_proposal":{"classification":"required, out_of_scope or undecided","acceptance_items":["..."],"reason":"..."}"#),
                "{first}"
            );
        assert!(!first.contains(FOLLOW_UP_PROPOSAL_AGAIN), "{first}");
        for request in &texts[1..12] {
            assert!(
                request.contains(&format!("{ACCEPTANCE_REMAP} {FOLLOW_UP_PROPOSAL_AGAIN}")),
                "{provider:?} {mode:?}: {request}"
            );
        }
    }
    for part in [
        "write the problem and its evidence",
        "required when you think the goal's acceptance cannot be met without it, out_of_scope when it can, undecided when you cannot tell",
        "acceptance_items, the goal's acceptance items it bears on",
        "It is only a proposal: the planner decides where the follow_up belongs, so do not judge or move it yourself.",
    ] {
        assert!(FOLLOW_UP_PROPOSAL.contains(part), "{part}");
    }
    assert!(FOLLOW_UP_PROPOSAL_AGAIN.contains("the problem and its evidence"));
    assert!(FOLLOW_UP_PROPOSAL_AGAIN.contains("not a judgement"));
    for text in [FOLLOW_UP_PROPOSAL, FOLLOW_UP_PROPOSAL_AGAIN] {
        assert!(!text.contains("cargo") && !text.contains('`'), "{text}");
    }
}

/// Task 1428 (ADR-t1428-1): every worker's prompt, on Claude or Codex,
/// checks the documents against the diff
/// once, right after the acceptance map and as part of it, not as a
/// second map; the Codex review line does not say it again; every
/// resume and revise request keeps the record of the check inside the
/// remap's phrase, not as another sentence; the run's review prompt is
/// untouched. ADR-t1942-2: the check finds candidates by a search for
/// each changed name and gives the names searched in summary, writes a
/// document only when a flow, boundary, invariant or promise changed,
/// adds no list of identifiers and no history, and still names no
/// command, tool, repository path or document rule of one repository.
#[test]
fn every_worker_text_that_writes_a_receipt_checks_the_documents_once() {
    let task = verified_task(7, "work", TaskStatus::InProgress, vec!["make gate".into()]);
    for (provider, mode) in [
        (Provider::Claude, WorkerMode::Headless),
        (Provider::Codex, WorkerMode::Headless),
    ] {
        let run = run_on(provider, mode);
        let texts = session_texts(&task, &run);
        let first = &texts[0];
        assert_eq!(first.matches(DOCS_CHECK).count(), 1, "{first}");
        assert!(first.contains(&format!("{ACCEPTANCE_MAP}{DOCS_CHECK}")));
        for phrase in ["the documents on what you changed", "the names searched"] {
            assert_eq!(
                first.matches(phrase).count(),
                1,
                "{provider:?} {mode:?}: {first}"
            );
        }
        for request in &texts[1..12] {
            assert!(!request.contains(DOCS_CHECK), "{request}");
            assert_eq!(
                request
                    .matches("the documents you checked against the diff")
                    .count(),
                1,
                "{provider:?} {mode:?}: {request}"
            );
        }
    }
    // The check names neither a criterion map nor the own-diff reading.
    assert!(!DOCS_CHECK.contains("map each acceptance criterion"));
    assert!(ACCEPTANCE_REMAP.contains("rewrite its phrase in summary, with the documents"));
    // The Codex review line reads the own diff; the check of the
    // documents is said once, in DOCS_CHECK, not again there.
    let codex = review_line(Route(Provider::Codex));
    assert!(codex.contains("read your own diff"), "{codex}");
    assert!(!codex.contains("documents") && !codex.contains("docs_drift"));
    assert!(!DOCS_CHECK.contains("own diff"));
    // Candidates come from a search for each changed name, and summary
    // gives the names searched (ADR-t1942-2 decisions 1 and 3).
    assert!(DOCS_CHECK.contains("searching the repository for each changed name"));
    assert!(DOCS_CHECK.contains("in summary give the names searched"));
    // A document is written when a flow, boundary, invariant or promise
    // changed; a missing name is not drift, no list of identifiers and
    // no history go in, and the detail goes to a doc comment
    // (ADR-t1942-2 decision 2).
    for part in [
        "only when a flow, boundary, invariant or promise the code does not show changed",
        "a changed name missing from it is not drift",
        "Add no list of fields, flags, defaults or names and no history (task numbers",
        "put what a name means in a doc comment by its definition",
    ] {
        assert!(DOCS_CHECK.contains(part), "{part}");
    }
    // No new command, no tool, no repository path or document rule of
    // one repository, and short (ADR-t1428-1, ADR-t1453-2).
    assert!(DOCS_CHECK.len() <= 840, "{}", DOCS_CHECK.len());
    for word in [
        "cargo", "`", "docs/", "grep", "design", "concept", "map", "budget", "KiB", "byte",
    ] {
        assert!(!DOCS_CHECK.contains(word), "{word}");
    }
    let review = review_prompt(
        &task,
        &run_on(Provider::Claude, WorkerMode::Headless),
        "r",
        None,
        None,
        &PersonAnswers::default(),
    )
    .text;
    assert!(!review.contains(DOCS_CHECK));
    assert!(!review.contains("the documents you checked against the diff"));
}

/// The whole review sends back with every finding that sends the run back
/// in the same round, keeping none for a later one; plan review and the
/// claim's review do not say so, and the shared reason codes do not carry it.
#[test]
fn only_the_whole_review_lists_every_finding_that_sends_back_in_one_round() {
    use crate::domain::satisfied::SnapshotVerification;
    const ONE_ROUND: &str = "For revise or concern, list in reasons, in this round, every finding of the diff that is a reason to send the run back: do not leave one for a later round.";
    let task = verified_task(7, "work", TaskStatus::InProgress, vec!["make gate".into()]);
    let run = run_on(Provider::Claude, WorkerMode::Headless);
    let review = review_prompt(&task, &run, "r", None, None, &PersonAnswers::default()).text;
    assert_eq!(review.matches(ONE_ROUND).count(), 1, "{review}");
    assert!(REVIEW_VERDICTS.contains(ONE_ROUND));
    // No narrowing of a later round to the last fix's diff.
    assert!(!review.contains("previous round"), "{review}");
    assert!(
        !reason_codes_section(review_reason::REVIEW_CODES, &run_review_codes_use())
            .contains("later round")
    );

    let plan = plan_case(1, 0).prompt().text;
    assert!(!plan.contains(ONE_ROUND), "{plan}");
    assert!(!plan.contains("later round"), "{plan}");

    let receipt = Receipt::parse(
        &json!({"run_id": RUN, "result": "already_satisfied", "commit": SHA,
            "tests": {"status": "passed", "evidence_or_reason": "e"},
            "e2e": {"status": "not_applicable", "evidence_or_reason": "r"},
            "subagent_review": {"status": "not_applicable", "evidence_or_reason": "r"},
            "summary": "the base meets it",
            "base_checks": [{"command": "test -f seed.txt", "result": "passed"}]})
        .to_string(),
    )
    .unwrap();
    let claim = claim_review_prompt(
        &task_with_evidence(7, Vec::new()),
        &run,
        (Some(&receipt), "/runs/run/receipt.json"),
        &SnapshotVerification {
            head: CommitSha::try_from(SHA).unwrap(),
            commands: 2,
            failure: None,
        },
        Path::new("/q/satisfied/run"),
        None,
    )
    .text;
    assert!(!claim.contains(ONE_ROUND), "{claim}");
    assert!(!claim.contains("later round"), "{claim}");
}

/// The whole review lists the defects to look for and reads every file the
/// diff changes before its verdict, once, right before the verdicts; plan
/// review, the claim's review and the shared reason codes do not carry it,
/// it hands nothing of an earlier round, and the digest covers it.
#[test]
fn only_the_whole_review_looks_for_the_listed_defects_in_every_changed_file() {
    use crate::domain::satisfied::SnapshotVerification;
    let task = verified_task(7, "work", TaskStatus::InProgress, vec!["make gate".into()]);
    let run = run_on(Provider::Claude, WorkerMode::Headless);
    let review = review_prompt(&task, &run, "r", None, None, &PersonAnswers::default()).text;
    assert_eq!(review.matches(REVIEW_DEFECTS).count(), 1, "{review}");
    assert!(review.contains(&format!("{REVIEW_DEFECTS}{REVIEW_VERDICTS}")));
    for part in [
        "Before you decide the verdict, read every file the diff changes, one by one and each whole",
        "concurrency and races",
        "a stop and a restart",
        "records and tokens an older version wrote",
        "records written halfway or missing",
        "permissions",
        "the validation of input at a boundary",
    ] {
        assert_eq!(REVIEW_DEFECTS.matches(part).count(), 1, "{part}");
    }
    for word in ["previous", "earlier round", "last fix", "later round"] {
        assert!(!REVIEW_DEFECTS.contains(word), "{word}");
    }
    let codes = reason_codes_section(review_reason::REVIEW_CODES, &run_review_codes_use());
    assert!(!codes.contains("concurrency"));
    assert!(
        !reason_codes_section(review_reason::PLAN_REVIEW_CODES, PLAN_REVIEW_CODES_USE)
            .contains("concurrency")
    );

    let plan = plan_case(1, 0).prompt().text;
    assert!(!plan.contains(REVIEW_DEFECTS), "{plan}");
    assert!(!plan.contains("concurrency and races"), "{plan}");

    let receipt = Receipt::parse(
        &json!({"run_id": RUN, "result": "already_satisfied", "commit": SHA,
            "tests": {"status": "passed", "evidence_or_reason": "e"},
            "e2e": {"status": "not_applicable", "evidence_or_reason": "r"},
            "subagent_review": {"status": "not_applicable", "evidence_or_reason": "r"},
            "summary": "the base meets it",
            "base_checks": [{"command": "test -f seed.txt", "result": "passed"}]})
        .to_string(),
    )
    .unwrap();
    let claim = claim_review_prompt(
        &task_with_evidence(7, Vec::new()),
        &run,
        (Some(&receipt), "/runs/run/receipt.json"),
        &SnapshotVerification {
            head: CommitSha::try_from(SHA).unwrap(),
            commands: 2,
            failure: None,
        },
        Path::new("/q/satisfied/run"),
        None,
    )
    .text;
    assert!(!claim.contains(REVIEW_DEFECTS), "{claim}");
    assert!(!claim.contains("concurrency and races"), "{claim}");

    // The digest of the fixed sentences changes with this section.
    let without = digest_of(&[
        REVIEW_MATERIAL,
        REVIEW_RULES_HEADER,
        REVIEW_DOCS_CHECK,
        REVIEW_ACCEPTANCE_HEADER,
        REVIEW_VERDICTS,
        CONCERN_RECOMMENDATION,
        &codes,
        REVIEW_ANSWER,
        LANDING_ANSWERS_INTRO,
        ACCEPTED_CONCERNS,
    ]);
    let with = digest_of(&[
        REVIEW_MATERIAL,
        REVIEW_RULES_HEADER,
        REVIEW_DOCS_CHECK,
        REVIEW_ACCEPTANCE_HEADER,
        REVIEW_DEFECTS,
        REVIEW_VERDICTS,
        CONCERN_RECOMMENDATION,
        &codes,
        REVIEW_ANSWER,
        LANDING_ANSWERS_INTRO,
        ACCEPTED_CONCERNS,
    ]);
    assert_eq!(acceptance_digest(), with);
    assert_ne!(acceptance_digest(), without);
}

/// A run's review, the whole one and each agent's job, carries a person's
/// answers to the run's `approve_landing` asks with the rule on the
/// concerns they accepted, once, and nothing of them without an answer;
/// plan review and the claim's review carry none of it, and the whole
/// review's digest covers it.
#[test]
fn a_runs_review_reads_a_persons_landing_answers_and_does_not_ask_again() {
    use crate::application::agent_job::{AgentJob, build};
    use crate::domain::review_subagents::{AgentRole, AgentTools};
    use crate::domain::satisfied::SnapshotVerification;
    let task = verified_task(7, "work", TaskStatus::InProgress, vec!["make gate".into()]);
    let run = run_on(Provider::Claude, WorkerMode::Headless);
    let answers = PersonAnswers {
        answers: vec![PersonAnswer {
            ask: crate::domain::AskId::new(893),
            round: Some(5),
            answer: "send_back: the added permissions are as designed".into(),
        }],
        older: 0,
    };
    let line = "- ask 893, after review 5: send_back: the added permissions are as designed\n";
    for part in [
        "Do not raise such a concern again, as scope or otherwise, while the diff since the answer does not widen what was accepted",
        "new permission, write target, outside connection",
        "name it in summary as accepted by the person",
        "make only the widened part the concern",
        "goes to a person as before",
    ] {
        assert_eq!(ACCEPTED_CONCERNS.matches(part).count(), 1, "{part}");
    }
    let review = review_prompt(&task, &run, "/runs/r/review.md", None, None, &answers);
    let section = format!("{LANDING_ANSWERS_INTRO}\n{line}\n{ACCEPTED_CONCERNS}");
    assert_eq!(review.text.matches(&section).count(), 1, "{}", review.text);
    assert_eq!(review.text.matches(ACCEPTED_CONCERNS).count(), 1);
    assert!(review.bytes.sections["landing_answers"] > 0);
    within(&review, RUN_REVIEW_PROMPT_LIMIT);
    let none = review_prompt(
        &task,
        &run,
        "/runs/r/review.md",
        None,
        None,
        &PersonAnswers::default(),
    );
    assert!(!none.text.contains(ACCEPTED_CONCERNS), "{}", none.text);
    assert!(!none.text.contains(LANDING_ANSWERS_INTRO), "{}", none.text);
    assert!(!none.bytes.sections.contains_key("landing_answers"));
    assert_eq!(review.text.replace(&section, ""), none.text);

    let definition = "---\ndescription: checks security\ntools: [read]\n---\nCheck it.\n";
    let tools = AgentTools::declared(AgentRole::Review, definition).unwrap();
    let agent = |answers: Option<&PersonAnswers>| {
        build(&AgentJob {
            agent: "security",
            definition,
            tools: &tools,
            rules: None,
            base: SHA,
            head: SHA,
            material: Path::new("/runs/r/review.md"),
            cwd: Path::new("/runs/r/worktree"),
            dir: Path::new("/runs/r/review-1-agents/security"),
            language: None,
            reads: None,
            answers,
        })
        .unwrap()
        .prompt
    };
    let with = agent(Some(&answers));
    assert_eq!(with.text.matches(&section).count(), 1, "{}", with.text);
    assert_eq!(with.text.matches(ACCEPTED_CONCERNS).count(), 1);
    within(&with, AGENT_JOB_PROMPT_LIMIT);
    for without in [agent(None), agent(Some(&PersonAnswers::default()))] {
        assert!(
            !without.text.contains(ACCEPTED_CONCERNS),
            "{}",
            without.text
        );
        assert!(
            !without.text.contains(LANDING_ANSWERS_INTRO),
            "{}",
            without.text
        );
    }

    let plan = plan_case(1, 0).prompt().text;
    assert!(!plan.contains(ACCEPTED_CONCERNS), "{plan}");
    let receipt = Receipt::parse(
        &json!({"run_id": RUN, "result": "already_satisfied", "commit": SHA,
            "tests": {"status": "passed", "evidence_or_reason": "e"},
            "e2e": {"status": "not_applicable", "evidence_or_reason": "r"},
            "subagent_review": {"status": "not_applicable", "evidence_or_reason": "r"},
            "summary": "the base meets it",
            "base_checks": [{"command": "test -f seed.txt", "result": "passed"}]})
        .to_string(),
    )
    .unwrap();
    let claim = claim_review_prompt(
        &task_with_evidence(7, Vec::new()),
        &run,
        (Some(&receipt), "/runs/run/receipt.json"),
        &SnapshotVerification {
            head: CommitSha::try_from(SHA).unwrap(),
            commands: 2,
            failure: None,
        },
        Path::new("/q/satisfied/run"),
        None,
    )
    .text;
    assert!(!claim.contains(ACCEPTED_CONCERNS), "{claim}");
    assert!(!claim.contains(LANDING_ANSWERS_INTRO), "{claim}");
}

/// The answers a run's review is handed stay within their limit in the
/// whole review's prompt at the largest input, the acceptance and the
/// rules at their limits too.
#[test]
fn a_persons_landing_answers_at_their_limits_stay_within_the_review_prompt() {
    let long = "y".repeat(crate::application::review::LANDING_ANSWER_BYTES * 3);
    let answers = PersonAnswers {
        answers: (0..crate::application::review::LANDING_ANSWERS as i64)
            .map(|id| PersonAnswer {
                ask: crate::domain::AskId::new(id),
                round: Some(9),
                answer: long.clone(),
            })
            .collect(),
        older: 4,
    };
    let rules_text = "r".repeat(RUN_REVIEW_RULES_BYTES * 2);
    let fitted = review_prompt(
        &big_task(7, 100_000),
        &run_on(Provider::Claude, WorkerMode::Headless),
        "/runs/run/review.md",
        None,
        Some(AcceptanceRules {
            path: ".dagq/review/builtin-acceptance.md",
            commit: SHA,
            text: &rules_text,
            whole_at: "/runs/run/review-1-builtin-acceptance.md",
        }),
        &answers,
    );
    within(&fitted, RUN_REVIEW_PROMPT_LIMIT);
    assert!(
        fitted.bytes.sections["landing_answers"]
            <= LANDING_ANSWERS_PROMPT_BYTES
                + ACCEPTED_CONCERNS.len()
                + LANDING_ANSWERS_INTRO.len()
                + 8
    );
    assert_eq!(fitted.bytes.omitted.get("landing_answers"), Some(&4));
    assert_eq!(fitted.text.matches(ACCEPTED_CONCERNS).count(), 1);
}

/// ADR-t1428-1 decision 5 as ADR-t2352-1 amends it: the run's review reads the
/// documents on the changed behavior against the diff and the summary,
/// once, before the acceptance, sending back only for a document the diff
/// changes and a broken promise, and listing a stale document the diff did
/// not touch in the verdict's follow_ups; the concern's recommendation and
/// the reason codes stay as they were.
#[test]
fn the_review_prompt_checks_the_documents_and_keeps_its_verdict() {
    let task = verified_task(7, "work", TaskStatus::InProgress, vec!["make gate".into()]);
    let review = review_prompt(
        &task,
        &run_on(Provider::Codex, WorkerMode::Headless),
        "r",
        None,
        None,
        &PersonAnswers::default(),
    )
    .text;
    assert_eq!(review.matches(REVIEW_DOCS_CHECK).count(), 1, "{review}");
    assert!(review.contains(&format!(
        "Do not change any file.\n{REVIEW_DOCS_CHECK}\nAcceptance criteria of the task:"
    )));
    for part in [
        "named by the task's description or context or by the summary",
        "A document's diff alone does not show the change is right",
        "check the summary's reason for leaving a document as it is",
        "whether or not the task named it",
        // What goes in reasons and what in follow_ups (ADR-t2352-1).
        "Report as docs_drift in reasons, whether or not the task named it, only a document the diff changes that disagrees with the diff, and a change of the diff that breaks an invariant, promise or boundary a document states.",
        "A document the diff does not touch that is stale (before the diff, or on a flow, boundary, invariant or promise the diff changes) is no reason to send the run back: list it in follow_ups with its path, section and why, whatever the verdict.",
        "a name missing from a document alone is not stale",
    ] {
        assert!(
            REVIEW_DOCS_CHECK
                .to_lowercase()
                .contains(&part.to_lowercase()),
            "{part}"
        );
    }
    // The project's rules judge what makes a document's text a defect
    // (copies of the code, history), not the runtime (ADR-t2352-1).
    for word in [
        "design",
        "concept",
        "docs/",
        "budget",
        "KiB",
        "byte",
        "copies the code",
        "history",
        "task numbers",
        "flags",
        "defaults",
    ] {
        assert!(!REVIEW_DOCS_CHECK.contains(word), "{word}");
    }
    assert!(
        REVIEW_DOCS_CHECK.len() <= 830,
        "{}",
        REVIEW_DOCS_CHECK.len()
    );
    assert!(!REVIEW_DOCS_CHECK.contains("cargo") && !REVIEW_DOCS_CHECK.contains('`'));
    // The verdict's shape with its follow_ups (ADR-t2352-1), the
    // concern's recommendation and the reason codes with their
    // definitions.
    assert!(review.contains(
            "Answer with one JSON object and nothing else, matching this schema:\n\
             {\"verdict\": \"pass\" | \"revise\" | \"concern\", \"reasons\": [{\"text\": string, \"codes\": [string], \"paths\": [string]}], \"summary\": string, \"recommendation\": \"land\" | \"send_back\" | null, \"confidence\": \"high\" | \"low\" | null, \"reason_category\": \"scope\" | \"discard\" | null, \"follow_ups\": [{\"text\": string, \"path\": string, \"section\": string, \"codes\": [string]}]}\n\
             reasons lists each finding (empty for pass) with the repository-relative paths it is on (empty when it is on no file); summary is one or two sentences; recommendation, confidence and reason_category are for a concern only (null for pass and revise); follow_ups lists the stale documents the diff does not touch (empty when none), for any verdict, and never decides it.\n"
        ));
    assert!(review.contains(&format!(
        "{CONCERN_RECOMMENDATION}{}Answer with one JSON object",
        reason_codes_section(review_reason::REVIEW_CODES, &run_review_codes_use())
    )));
    assert!(
        CONCERN_RECOMMENDATION
            .starts_with("For a concern, also recommend what to do, and how sure you are:\n")
    );
    assert!(CONCERN_RECOMMENDATION.ends_with(
            "Leave scope and discard to the person rather than deciding them; when in doubt, say low.\n\n"
        ));
    assert!(!CONCERN_RECOMMENDATION.contains("document"));
    assert!(
        !reason_codes_section(review_reason::REVIEW_CODES, &run_review_codes_use())
            .contains(REVIEW_DOCS_CHECK)
    );
}

/// The whole review is builtin:acceptance (ADR-t2259-1 decisions 3 and
/// 6): its prompt names no instructions file of the worktree to read, and
/// the project's rules are the landing branch's file only, right after the
/// material and before the check of the documents; without the file the
/// prompt has no rules of the project. Rules past their limit are cut,
/// saying so and where the whole file is, within the prompt's limit.
#[test]
fn the_review_prompt_takes_the_projects_rules_from_the_landing_branch_only() {
    let task = verified_task(7, "work", TaskStatus::InProgress, vec!["make gate".into()]);
    let run = run_on(Provider::Claude, WorkerMode::Headless);
    let without = review_prompt(&task, &run, "r", None, None, &PersonAnswers::default()).text;
    let material = "Read the worktree if you need more. Do not change any file.\n";
    assert!(
        without.contains(&format!("{material}{REVIEW_DOCS_CHECK}")),
        "{without}"
    );
    let rules = AcceptanceRules {
        path: ".dagq/review/builtin-acceptance.md",
        commit: "abc123",
        text: "Check the changelog.\n",
        whole_at: "/runs/r/review-1-builtin-acceptance.md",
    };
    let with = review_prompt(
        &task,
        &run,
        "r",
        None,
        Some(rules),
        &PersonAnswers::default(),
    )
    .text;
    assert!(
        with.contains(&format!(
            "{material}{REVIEW_RULES_HEADER} (.dagq/review/builtin-acceptance.md on the landing branch's commit abc123):\nCheck the changelog.\n{REVIEW_DOCS_CHECK}"
        )),
        "{with}"
    );
    for prompt in [&without, &with] {
        // What makes a document's text a defect (copies of the code,
        // history) is the project's rules, never the runtime's fixed
        // sentences (ADR-t2389-1 decision 2).
        for word in [
            "AGENTS.md",
            "CLAUDE.md",
            "instructions at the root",
            "copies the code",
            "history",
        ] {
            assert!(!prompt.contains(word), "{word}: {prompt}");
        }
        assert!(
            !prompt.contains("The repository's rules are in"),
            "{prompt}"
        );
    }
    assert!(!without.contains(REVIEW_RULES_HEADER));
    // Rules past their limit are cut, said, and named where they are whole.
    let long = "rule. ".repeat(RUN_REVIEW_RULES_BYTES);
    let cut = review_prompt(
        &task,
        &run,
        "r",
        None,
        Some(AcceptanceRules {
            text: &long,
            ..rules
        }),
        &PersonAnswers::default(),
    );
    within(&cut, RUN_REVIEW_PROMPT_LIMIT);
    assert!(
        cut.text
            .contains("read it whole at /runs/r/review-1-builtin-acceptance.md]"),
        "{}",
        cut.text
    );
    assert_eq!(cut.bytes.omitted.get("rules"), Some(&1));
    assert!(
        cut.bytes
            .over_limit
            .as_deref()
            .unwrap()
            .starts_with("rules: ")
    );
    assert!(cut.bytes.sections["rules"] <= RUN_REVIEW_RULES_BYTES + 200);
    // The fixed sentences' digest does not depend on the task or the rules.
    assert_eq!(acceptance_digest(), acceptance_digest());
    assert_eq!(acceptance_digest().len(), 64);
}

/// Task 978: a task that needs a path outside its declared paths ends
/// in a failed receipt naming them, not in an ask (ADR-0029 decision
/// 5); and before each worker_question the prompts first send the
/// worker to the repository's rules on what is not asked.
#[test]
fn a_path_outside_the_scope_is_a_failed_receipt_and_asks_follow_the_repository_rules() {
    let scoped = Task::restore(TaskRecord {
        goal_priority: None,
        id: TaskId::new(7),
        title: "work".into(),
        description: String::new(),
        acceptance: String::new(),
        verification_commands: vec!["make gate".into()],
        required_evidence: Vec::new(),
        paths: vec!["docs/**".into()],
        priority: Default::default(),
        change: None,
        status: TaskStatus::InProgress,
        goal_id: None,
        context: String::new(),
        created_at: String::new(),
        updated_at: String::new(),
        worker: crate::domain::worker::Worker::CLAUDE_HEADLESS,
        named_mode: None,
        wait_for_build: false,
        execution_class: Default::default(),
    })
    .unwrap();
    let texts = session_texts(&scoped, &run_on(Provider::Claude, WorkerMode::Headless));
    let first = &texts[0];
    assert!(first.contains("Paths you may change"), "{first}");
    assert!(!first.contains("ask instead of changing it"), "{first}");
    assert!(first.contains(
            "If the task needs another path, do not change it and do not run dagq ask: write the receipt with result failed and name in summary the paths it needs"
        ), "{first}");
    assert!(
            first.contains(&format!(
                "{ASK_RULES_FIRST} When you need a decision you cannot make from the task and the repository, do not end the turn with the question in your reply: run `dagq ask"
            )),
            "{first}"
        );
    // The nudge, fourth from the end of `session_texts` (before the
    // answer, the recovery instruction and the go-on), offers the ask
    // behind the same check.
    let nudge = texts.len() - 4;
    assert!(
        texts[nudge].contains(&format!(
            "2. {ASK_RULES_FIRST} Otherwise, if you need a decision, run `dagq ask"
        )),
        "{}",
        texts[nudge]
    );
    for request in &texts[1..nudge] {
        assert!(
            request.contains(&format!(
                "{ASK_RULES_FIRST} If you need a decision, run `dagq ask"
            )),
            "{request}"
        );
    }
    assert!(ASK_RULES_FIRST.contains("AGENTS.md or CLAUDE.md"));
    assert!(ASK_RULES_FIRST.contains("failed receipt"));
}

/// ADR-t1433-2: the worker's texts have no interactive branch. A run
/// recorded as interactive before is sent what a headless Claude run is,
/// the documents' check once among them (task 1688), and nothing of a
/// terminal or an `/exit`.
#[test]
fn a_run_recorded_as_interactive_is_sent_the_headless_claude_texts() {
    let task = verified_task(7, "work", TaskStatus::InProgress, vec!["make gate".into()]);
    let recorded = session_texts(&task, &run_on(Provider::Claude, WorkerMode::Interactive));
    let headless = session_texts(&task, &run_on(Provider::Claude, WorkerMode::Headless));
    assert_eq!(recorded, headless);
    let first = &recorded[0];
    assert_eq!(first.matches(DOCS_CHECK).count(), 1, "{first}");
    assert!(first.contains(HEADLESS_WORKER), "{first}");
    for text in &recorded {
        for never in ["/exit", "this terminal", "if any is left"] {
            assert!(!text.contains(never), "{never}: {text}");
        }
    }
}

/// A headless run's recovery job is never offered an action on a
/// screen, a dialog or an /exit, and reads its turns instead.
#[test]
fn a_headless_recovery_job_is_never_offered_a_dialog() {
    let task = task(7, "work", TaskStatus::InProgress);
    let facts = json!({});
    let binary = no_binary();
    let recovery = |run: &TaskRun, alert| {
        recovery_prompt(
            &task,
            run,
            1,
            &RecoveryMaterial {
                alert,
                ended: None,
                facts: &facts,
                workspace: "ws",
                screen: "turns",
                processes: Ok(Vec::new()),
                git_status: "",
                head: SHA,
                receipt_commit: None,
                history: &[],
                allowed: &[
                    "send_instruction",
                    "answer_known_dialog",
                    "close_and_proceed",
                    "wait",
                ],
                binary: &binary,
            },
        )
        .unwrap()
        .text
    };
    let headless = recovery(
        &run_on(Provider::Codex, WorkerMode::Headless),
        RecoveryAlert::Stalled,
    );
    for never in [
        "answer_known_dialog",
        "close_and_proceed",
        "Last lines of the session's screen",
        "idle at its prompt",
        "known one",
    ] {
        assert!(!headless.contains(never), "{never}: {headless}");
    }
    assert!(headless.contains("Last turns of the headless session"));
    assert!(headless.contains("reason turn_without_receipt"));
    assert!(headless.contains("as the prompt of the session's next turn"));
    let interactive = recovery(
        &run_on(Provider::Claude, WorkerMode::Interactive),
        RecoveryAlert::Stalled,
    );
    for retired in [
        "answer_known_dialog",
        "close_and_proceed",
        "Last lines of the session's screen",
        "idle at its prompt",
    ] {
        assert!(!interactive.contains(retired), "{retired}: {interactive}");
    }
    assert!(interactive.contains("Last turns of the headless session"));
}

/// The `dagq <command> ...` spans in backquotes of `text`, without
/// `dagq`.
fn dagq_commands(text: &str) -> Vec<&str> {
    text.split('`')
        .skip(1)
        .step_by(2)
        .filter_map(|span| span.strip_prefix("dagq "))
        .collect()
}

/// The goal review is shown the goal's follow-ups and told that an
/// out-of-scope one is left out of the acceptance and a required one is
/// judged with it (ADR-t1504-2 decision 8).
#[test]
fn the_goal_review_judges_required_follow_ups_and_leaves_out_of_scope_ones_out() {
    let prompt = goal_review_prompt(&GoalReviewMaterial {
        goal: json!({"id": 7}),
        tasks: Vec::new(),
        replacements: Vec::new(),
        follow_ups: vec![json!({"task_id": 9, "judgements": [{"classification": "out_of_scope"}]})],
        events: Vec::new(),
        previous: Vec::new(),
        gaps_in_a_row: 0,
        repo_root: Path::new("/repo"),
    })
    .text;
    assert!(prompt.contains(r#""task_id":9"#), "{prompt}");
    assert!(prompt.contains(
            "A follow-up judged out_of_scope is not part of the acceptance: leave its work out of your judgement and do not wait for it or list it as a gap."
        ));
    assert!(
        prompt.contains("A follow-up judged required is part of it: judge the goal with its work")
    );
}

/// The goal review is shown what replaced the goal's replaced tasks, and
/// told to judge a replaced task's items through its children's landings
/// and the mappings (ADR-t1521-2 decision 5).
#[test]
fn the_goal_review_judges_a_replaced_task_through_its_replacement() {
    let prompt = goal_review_prompt(&GoalReviewMaterial {
        goal: json!({"id": 7}),
        tasks: vec![
            json!({"id": 12, "status": "completed", "replaces_in_this_goal": true, "goal_id": 8}),
        ],
        replacements: vec![
            json!({"operation": 3, "replaced_task": 10, "children": [11, 12],
                                  "mappings": [{"ordinal": 2, "child_task_id": 12}]}),
        ],
        follow_ups: Vec::new(),
        events: Vec::new(),
        previous: Vec::new(),
        gaps_in_a_row: 0,
        repo_root: Path::new("/repo"),
    })
    .text;
    for expected in [
        r#""replaced_task":10"#,
        r#""child_task_id":12"#,
        r#""replaces_in_this_goal":true"#,
        "replaced by tasks that all ended",
        "judge its items only through the children's landings and the mappings",
    ] {
        assert!(prompt.contains(expected), "{expected}\n{prompt}");
    }
}

/// Every `dagq` command the prompts of the jobs that read the queue
/// (the plan review, the goal review, the observer and the throughput
/// review with the dagq skill's procedure it carries) and the
/// worker's and planners' shared reading lines name goes to a use case
/// of the queue service, so their client-mode `dagq` takes it
/// (ADR-t1233-5 decision 1, docs/design/queue-service.md). The review
/// and recovery jobs run no command (`JobAccess::ReadFiles`).
#[test]
fn every_dagq_command_a_job_s_prompt_names_is_a_use_case_of_the_queue_service() {
    use crate::domain::queue_service::UseCase;
    use crate::domain::throughput_review::{HOUR_MS, ReviewMode, window};
    let (plan, _) = plan_prompt(2, 0);
    let goal = goal_review_prompt(&GoalReviewMaterial {
        goal: json!({"id": 7}),
        tasks: Vec::new(),
        replacements: vec![json!({"operation": 1})],
        follow_ups: Vec::new(),
        events: Vec::new(),
        previous: Vec::new(),
        gaps_in_a_row: 0,
        repo_root: Path::new("/repo"),
    })
    .text;
    // With every section cut, so the prompt names each read of what it
    // left out.
    let observer = crate::application::observer::observer_prompt(
            crate::application::observer::ObserveMode::Hourly,
            "dagq",
            Some(crate::domain::EventId::new(12)),
            "1791005872",
            &json!({"kpi": {"breaches": [{}], "config": {}}, "open_asks": [{}], "findings": [{}], "notes": [{}],
                    "stats": {"alerts": [{}], "running_alerts": [{}], "overall": {}},
                    "improvements": {"running": 0}, "graph": {"candidates": [1], "critical": [1]}}),
            1,
        )
        .unwrap()
        .text;
    let throughput = [ReviewMode::Hourly, ReviewMode::Daily, ReviewMode::Weekly]
        .map(|mode| {
            crate::application::throughput_review::review_prompt(
                &window(mode, 1_790_655_900_000, 9 * HOUR_MS),
                "dagq",
                &json!({}),
                Path::new("/q/input.json"),
            )
            .unwrap()
        })
        .join("\n");
    let mut named = std::collections::BTreeSet::new();
    for (who, text) in [
        ("plan review", plan.as_str()),
        ("goal review", &goal),
        ("observer", &observer),
        ("throughput review", &throughput),
        ("record reading", RECORD_READING),
        ("worker reading", WORKER_READING),
    ] {
        let commands = dagq_commands(text);
        assert!(!commands.is_empty(), "{who} names no dagq command");
        for command in commands {
            let words: Vec<&str> = command.split_whitespace().collect();
            let use_case = UseCase::of_command(&words);
            assert!(
                use_case.is_some(),
                "the {who} prompt names `dagq {command}`, which no use case of the queue service answers"
            );
            named.insert(use_case.unwrap().as_str());
        }
    }
    // The reads the prompts name, as docs/design/queue-service.md lists
    // them.
    for read in [
        "show",
        "proposal_show",
        "events",
        "timeline",
        "stats",
        "kpi",
        "marks",
        "search",
        "related",
        "findings",
        "goal_show",
        "forecast",
        "lint",
        "observe_history",
        "observe_input",
    ] {
        assert!(named.contains(read), "no prompt names {read}: {named:?}");
    }
}

/// The draft planner decides what it can recommend and records why;
/// only what it cannot settle, low confidence or a follow_up requiring
/// person adoption for registration-time facts or depth goes to a person,
/// with a recommendation and confidence (ADR-t451-1 decision 5).
#[test]
fn the_draft_planner_decides_what_it_can_recommend() {
    let material = json!({"source_run_id": RUN, "source_task_id": 3, "index": 0});
    let draft = task(9, "follow", TaskStatus::Draft);
    let key = BundleKey::of(DraftOrigin::FollowUp, &material, draft.id());
    let members = [(
        DraftTarget {
            task: draft,
            origin: DraftOrigin::FollowUp,
            material,
            planners: 0,
            revisit: None,
        },
        1,
    )];
    let prompt = draft_planner_prompt(&DraftPlannerMaterial {
        db: Path::new("/q/queue.db"),
        key: &key,
        members: &members,
        source: None,
        receipt: None,
        goals: &[],
        answer: None,
        handover: None,
        revisits: &[],
    })
    .unwrap()
    .text;
    for part in [
        "its Basic policy above all: decide what you can recommend yourself and go on, asking no one, and leave why in the record",
        "Say in its `--context` why you adopted it.",
        "record why with `dagq note --task 9 --text '<why>'`",
        // A drop records its reason (ADR-t947-4).
        "2. Drop: cancel it with the reason that says where its content is now",
        "`dagq cancel 9 --duplicate-of <that task>` for a duplicate or a completed task that already did it",
        "`--reason absorbed --duplicate-of <task>`",
        "Otherwise `dagq cancel 9 --reason <code>`: `not_worth`",
        "3. Ask: only for a draft you cannot decide yourself:",
        "that the queue, the repository (its code and the decisions it records) and a person's precedents cannot settle",
        "(b) your confidence in the decision is low; or (c)",
        "when none of them settles it, decide them yourself from the source, the decisions the repository records and a person's precedents, and ask a person with a `planner_question` ask as below only when that material cannot settle them and the decision is a person's (`scope` or `discard`) or your confidence in it is low.",
        "(c) it is a follow_up draft past the runtime's follow_up limit, 3 or more follow-ups from a person's judgement, a source goal that was missing, closed or unknown at registration (even if its current goal is open), or no current goal or a closed current goal",
        "dagq ask --task 9 --kind planner_question --because scope --recommend <adopt|cancel|keep_draft> --confidence <high|low>",
        "on keep_draft leave the draft as it is, record why with `dagq note --task 9",
        "A draft kept so stays a draft until a person has the inbox record a planning request that names it; no planner of the runtime's is opened for it again, unless it has a revisit time.",
        "keep it with `dagq revisit 9 --at <RFC 3339 time, e.g. 2026-10-04T12:00:00Z> --note '<what to look at then>'`",
        "You may keep a draft so yourself, without asking, when that is your recommendation",
        "The runtime refuses your submit of a follow_up draft past that limit unless a person answered adopt or already adopted it",
        "Membership changes (`set-goal` or `judge-follow-up`) do not count as adoption or reset depth; an existing person's adopt remains valid",
    ] {
        assert!(prompt.contains(part), "{part} in {prompt}");
    }
    assert!(
        !prompt.contains("when you cannot decide without a person"),
        "{prompt}"
    );
    // The runtime's prompts carry no rules of dagq's own repository.
    assert!(!prompt.contains("ADR"), "{prompt}");
}

/// A run's review's follow_up is a draft of the run's bundle like its
/// receipt's (ADR-t2352-1): its planner reads which came from the review,
/// a stale document the diff did not touch, and which from the receipt.
#[test]
fn a_reviews_follow_up_draft_s_planner_reads_it_came_from_the_review() {
    let review = json!({"source_run_id": RUN, "source_task_id": 3, "from": "review",
                        "path": "docs/a.md", "section": "s", "category": "docs_drift"});
    let receipt = json!({"source_run_id": RUN, "source_task_id": 3, "index": 0});
    let member = |id: i64, material: &Value| {
        (
            DraftTarget {
                task: task(id, "follow", TaskStatus::Draft),
                origin: DraftOrigin::FollowUp,
                material: material.clone(),
                planners: 0,
                revisit: None,
            },
            1,
        )
    };
    let prompt = |members: &[(DraftTarget, usize)]| {
        let key = BundleKey::of(
            DraftOrigin::FollowUp,
            &members[0].0.material,
            members[0].0.task.id(),
        );
        draft_planner_prompt(&DraftPlannerMaterial {
            db: Path::new("/q/queue.db"),
            key: &key,
            members,
            source: None,
            receipt: None,
            goals: &[],
            answer: None,
            handover: None,
            revisits: &[],
        })
        .unwrap()
        .text
    };
    let alone = prompt(&[member(9, &review)]);
    assert!(
        alone.contains(&format!("A review of run {RUN} of task 3 proposed it as a follow_up: a stale document its diff did not touch, which did not send the run back.\n")),
        "{alone}"
    );
    assert!(
        alone.contains(&format!(
            "`--context` beginning with `follow-up draft (proposed by the review of run {RUN} of task 3)`"
        )),
        "{alone}"
    );
    let both = prompt(&[member(9, &review), member(10, &receipt)]);
    assert!(
        both.contains(&format!("The receipt and a review of run {RUN} of task 3 proposed them (task 9 is its review's follow_up on docs/a.md, task 10 is its follow_up 0) as a follow_up: a receipt's is work its worker found outside that task, a review's a stale document its diff did not touch.\n")),
        "{both}"
    );
    let receipts = prompt(&[member(10, &receipt)]);
    assert!(
        receipts.contains(&format!("The receipt of run {RUN} of task 3 proposed it as a follow_up: work its worker found outside that task.\n")),
        "{receipts}"
    );
}

/// ADR-t1540-1: the planner opened for a draft whose revisit time came
/// reads the time, who set it and why, and the last decision about it
/// (its question, recommendation and answer, and its notes); a draft a
/// person added reads where it came from as theirs.
#[test]
fn a_revisited_draft_s_planner_reads_the_last_decision() {
    let draft = task(9, "measure again", TaskStatus::Draft);
    let revisit = crate::domain::DraftRevisit {
        task_id: draft.id(),
        revisit_at: 1_791_115_200,
        revisit_at_utc: "2026-10-04T12:00:00.000Z".into(),
        note: Some("after task 1429 lands".into()),
        set_by: "planner".into(),
        set_by_id: "planner:815".into(),
        created_at: 0,
        opened_at: None,
        planner_id: None,
    };
    let mut ask: Ask = serde_json::from_value(json!({
        "id": 358, "kind": "planner_question", "task_id": 9, "run_id": null,
        "question": "measure goal 90 again now?", "options": ["adopt", "cancel", "keep_draft"],
        "answer": "keep_draft", "asked_by": "planner", "reason_category": "scope",
        "recommendation": "keep_draft", "confidence": "high",
        "created_at": 0, "answered_at": 1, "closed_at": 2,
    }))
    .unwrap();
    ask.task_id = Some(draft.id());
    let history = [RevisitHistory {
        task: draft.id(),
        asks: vec![ask],
        notes: vec!["fill it in at noon UTC on 10-04".into()],
    }];
    for (origin, material, whence) in [
        (
            DraftOrigin::FollowUp,
            json!({"source_run_id": RUN, "source_task_id": 3, "index": 0}),
            "The receipt of run",
        ),
        (
            DraftOrigin::Revisit,
            json!({}),
            "A person added it (the runtime and the jobs did not)",
        ),
    ] {
        let members = [(
            DraftTarget {
                task: draft.clone(),
                origin,
                material,
                planners: 1,
                revisit: Some(revisit.clone()),
            },
            2,
        )];
        let key = members[0].0.bundle_key();
        assert_eq!(key.kind.as_str(), "task_id");
        let fitted = draft_planner_prompt(&DraftPlannerMaterial {
            db: Path::new("/q/queue.db"),
            key: &key,
            members: &members,
            source: None,
            receipt: None,
            goals: &[],
            answer: None,
            handover: None,
            revisits: &history,
        })
        .unwrap();
        let prompt = fitted.text;
        for part in [
            whence,
            "## Revisit of draft 9",
            "Its revisit time came: 2026-10-04T12:00:00.000Z (set by planner planner:815). What to look at then: after task 1429 lands",
            "- ask 358: measure goal 90 again now?\n  Recommended: keep_draft (high). Answer: keep_draft",
            "- fill it in at noon UTC on 10-04",
            "dagq revisit 9 --at",
        ] {
            assert!(prompt.contains(part), "{part} in {prompt}");
        }
        assert!(fitted.bytes.sections.contains_key("revisit"));
        assert!(!prompt.contains("ADR"), "{prompt}");
    }
}

/// A recurring draft's planner (ADR-0072) reads its key, the previous
/// registration and the setting, checks whether the work is needed, adopts
/// it made concrete or cancels it with what it checked, does not ask for
/// the interval or the lack of anything new, and gives a goal-less draft an
/// open goal when it adopts it.
#[test]
fn a_recurring_draft_s_planner_checks_the_need_and_decides() {
    let draft = task(9, "chore: raise Rust", TaskStatus::Draft);
    let material = json!({
        "key": "rust-toolchain",
        "previous_task_id": 4,
        "previous_registered_at": 1_791_115_200,
        "setting": {"key": "rust-toolchain", "interval_days": 42, "paths": ["rust-toolchain.toml"]},
    });
    let members = [(
        DraftTarget {
            task: draft,
            origin: DraftOrigin::Recurring,
            material,
            planners: 0,
            revisit: None,
        },
        1,
    )];
    let key = members[0].0.bundle_key();
    assert_eq!(key.kind.as_str(), "task_id");
    let prompt = draft_planner_prompt(&DraftPlannerMaterial {
        db: Path::new("/q/queue.db"),
        key: &key,
        members: &members,
        source: None,
        receipt: None,
        goals: &[],
        answer: None,
        handover: None,
        revisits: &[],
    })
    .unwrap()
    .text;
    for part in [
        "The recurring draft is not ready: the supervisor registered it because the interval of a recurring proposal in dagq.toml came",
        "## Where it came from: recurring",
        "recurring proposal with key `rust-toolchain`",
        "registered every 42 days; the previous draft of the key was task 4, registered at 2026-10-04T12:00:00.000Z",
        "\"paths\": [",
        "the latest official stable release",
        "cancel it with `--reason not_worth` or `--reason stale`",
        "is a reason to ask a person",
        "keep it only with a revisit time",
        "`--context` beginning with `recurring draft (dagq.toml [[recurring]] key rust-toolchain)`",
        "give it an open goal that needs the work with `dagq set-goal ID <goal>`",
    ] {
        assert!(prompt.contains(part), "{part} in {prompt}");
    }
    assert!(!prompt.contains("Membership proposal"), "{prompt}");
    assert!(!prompt.contains("ADR"), "{prompt}");
}

/// Task 1508 (ADR-t1504-1, ADR-t1504-2): a follow_up draft's planner
/// sees the worker's membership proposal, and judges and records where
/// the draft belongs before it adopts, drops or asks: membership apart
/// from adoption and priority, an existing goal found first, no
/// unrelated large goal, no acceptance weakened. A goal_gap draft gets
/// neither.
#[test]
fn the_follow_up_draft_planner_records_membership_before_it_decides() {
    let planner = |origin, material: Value| {
        let draft = task(9, "follow", TaskStatus::Draft);
        let key = BundleKey::of(origin, &material, draft.id());
        let members = [(
            DraftTarget {
                task: draft,
                origin,
                material,
                planners: 0,
                revisit: None,
            },
            1,
        )];
        draft_planner_prompt(&DraftPlannerMaterial {
            db: Path::new("/q/queue.db"),
            key: &key,
            members: &members,
            source: None,
            receipt: None,
            goals: &[],
            answer: None,
            handover: None,
            revisits: &[],
        })
        .unwrap()
        .text
    };
    let proposal =
        json!({"classification": "out_of_scope", "acceptance_items": ["(1)"], "reason": "r"});
    let prompt = planner(
        DraftOrigin::FollowUp,
        json!({"source_run_id": RUN, "source_task_id": 3, "index": 0, "membership_proposal": proposal,
                "source_goal_id": 12, "source_goal_state": "open", "source_goal_provenance": "recorded"}),
    );
    assert!(prompt.contains("Source goal (at registration; judge against its acceptance with `dagq goal show <id>`, not the current goal's): goal 12 (open, recorded)\n"), "{prompt}");
    for part in [
        "Read its earlier judgements with `dagq show 9` (membership_judgements): one that still holds needs no new row; to change a required or out_of_scope one, record the other with `--corrects <its id>`; it never goes back to undecided.",
        "A draft you drop (a duplicate, already done, not worth doing) may skip the record when it would need a new goal",
        "when that goal is above low, also `--destination-acceptance-item '<the item of its acceptance that needs the draft>'`",
        "record undecided with why, ask as step 3 says with the membership question in it, and on the answer record required or out_of_scope before you do what it says",
    ] {
        assert!(prompt.contains(part), "{part} in {prompt}");
    }
    let line = format!(
        "Membership proposal (the worker's; where you start, not a judgement): {proposal}\n"
    );
    assert!(prompt.contains(&line), "{prompt}");
    for part in [
        "first judge where it belongs, apart from whether it is worth doing and from its priority",
        "Start from the worker's membership proposal and decide the meaning yourself: can the source goal's acceptance be met without this draft?",
        "When it cannot, it is required and belongs to the source goal; when it can, it is out_of_scope and belongs to another goal",
        "look for a fitting existing goal with `dagq search` first, make one only when none fits, and never park it in an unrelated large goal",
        "Record the judgement before you adopt, drop or ask: `dagq judge-follow-up 9 --classification <required|out_of_scope|undecided> --acceptance-item",
        "`--destination-goal <goal>` for out_of_scope",
        "Moving a draft to another goal neither adopts it nor raises its priority.",
        "Never weaken a goal's acceptance to leave a follow_up out",
    ] {
        assert!(prompt.contains(part), "{part} in {prompt}");
    }
    let step = prompt.find("first judge where it belongs").unwrap();
    assert!(step < prompt.find("Then do exactly one of these three").unwrap());
    let none = planner(
        DraftOrigin::FollowUp,
        json!({"source_run_id": RUN, "source_task_id": 3, "index": 0}),
    );
    assert!(none.contains(
        "Membership proposal (the worker's; where you start, not a judgement): (none)\n"
    ));
    assert!(
        none.contains("not the current goal's): unknown\n"),
        "{none}"
    );
    let text = planner(
        DraftOrigin::FollowUp,
        json!({"source_run_id": RUN, "source_task_id": 3, "index": 0, "membership_proposal": "unsure", "source_goal_id": null, "source_goal_state": "none"}),
    );
    assert!(text.contains("not the current goal's): none\n"), "{text}");
    assert!(text.contains("not a judgement): \"unsure\"\n"), "{text}");
    let gap = planner(
        DraftOrigin::GoalGap,
        json!({"goal_id": 1, "goal_review_id": 2, "criterion": "c", "summary": "s"}),
    );
    assert!(!gap.contains("Membership proposal"), "{gap}");
    let reopened = planner(
        DraftOrigin::Reopened,
        json!({"reason": "r", "proposal_id": 4, "reviewed_proposal_id": 5}),
    );
    assert!(!reopened.contains("Membership proposal"), "{reopened}");
    assert!(
        !reopened.contains("first judge where it belongs"),
        "{reopened}"
    );
    assert!(!gap.contains("first judge where it belongs"), "{gap}");
    assert!(!prompt.contains("ADR"), "{prompt}");
}

/// The finding planner proposes or dismisses on its own and records
/// why; it asks only what it cannot settle or holds with low
/// confidence, with a recommendation (ADR-t451-1 decision 5).
#[test]
fn the_finding_planner_decides_what_it_can_recommend() {
    use crate::domain::{
        FindingId,
        finding::{Finding, FindingStatus, Impact},
    };
    let view = FindingView {
        finding: Finding {
            id: FindingId::new(4),
            kind: "conflict".into(),
            target: "queue".into(),
            task_id: None,
            run_id: None,
            goal_id: None,
            subject: String::new(),
            summary: "it conflicts".into(),
            detail: String::new(),
            impact: Impact::Normal,
            first_seen_at: 0,
            last_seen_at: 0,
            occurrences: 1,
            evidence: Vec::new(),
            status: FindingStatus::Open,
            status_reason: None,
            proposal_id: None,
            covered_by_task: None,
            propose_reason: None,
            propose_requested_at: None,
            recorded_by: "observer".into(),
            updated_at: 0,
        },
        proposal_status: None,
        open_asks: Vec::new(),
        evidence_events: None,
    };
    let prompt = finding_planner_prompt(&FindingPlannerMaterial {
        db: Path::new("/q/queue.db"),
        finding: &view,
        attempt: 1,
        asks: &[],
        goal: None,
        goal_closed: false,
        siblings: &[],
        answer: None,
        handover: None,
    })
    .unwrap()
    .text;
    for part in [
        "its Basic policy above all: decide what you can recommend yourself and go on, asking no one",
        "`from finding 4 (conflict)` and saying why you chose this remedy",
        "`dagq finding dismiss 4 --reason '<why>'`, the reason saying why you decided so",
        "4. Ask: only when you cannot decide it yourself:",
        "(b) your confidence in the decision is low",
        "dagq ask --finding 4 --kind planner_question --because scope --recommend <propose|dismiss> --confidence <high|low>",
        // No own priority: the goal's priority and membership hold an
        // improvement below high (ADR-t1971-1 decision 4).
        "Give an improvement's tasks no `--priority`: each takes its goal's (`normal` with no goal).",
        "Put a task in an existing goal only when that goal's acceptance needs it, and an improvement only in a goal of `normal` or lower",
        "or leave the task with no goal",
    ] {
        assert!(prompt.contains(part), "{part} in {prompt}");
    }
    for gone in [
        "plan review lowers a higher one to normal",
        "`--priority normal` or `low`",
    ] {
        assert!(!prompt.contains(gone), "{gone} in {prompt}");
    }
    assert!(
        !prompt.contains("a change that is large and hard to undo"),
        "{prompt}"
    );
    assert!(
            prompt.contains("only when that material cannot settle them and the decision is a person's (`scope` or `discard`) or your confidence in it is low."),
            "{prompt}"
        );
    assert!(!prompt.contains("rules above do not settle"), "{prompt}");
    assert!(!prompt.contains("ADR"), "{prompt}");
    assert!(!prompt.contains("## A CI failure"), "{prompt}");
    // A CI failure's planner copies the detail into the fix task and
    // covers it with a task that already fixes the same tests
    // (ADR-t1920-1).
    let mut ci = view.clone();
    ci.finding.kind = "ci_failure".into();
    let prompt = finding_planner_prompt(&FindingPlannerMaterial {
        db: Path::new("/q/queue.db"),
        finding: &ci,
        attempt: 1,
        asks: &[],
        goal: None,
        goal_closed: false,
        siblings: &[],
        answer: None,
        handover: None,
    })
    .unwrap()
    .text;
    for part in [
        "## A CI failure",
        "Copy each of them into the fix task's `--description`.",
        "`dagq finding dismiss 4 --covered-by <task> --reason '<why>'`",
        "Give the fix task no `--priority`: like any improvement it takes its goal's",
    ] {
        assert!(prompt.contains(part), "{part} in {prompt}");
    }
    assert!(!prompt.contains("--priority normal"), "{prompt}");
}

/// A planner opened for a step of an agent's watch (ADR-t1728-1 (g)) is
/// given that step, the set or the hold-out to ask for and the finding to
/// resolve, or the refusal to dismiss, in place of the proposal, the
/// dismissal or the question every other finding's planner chooses
/// exactly one of; a score under the threshold is an improvement like any
/// other finding.
#[test]
fn a_watch_findings_planner_does_its_step_and_plans_no_task() {
    use crate::domain::{
        FindingId,
        agent_eval::watch::{SCORE_FINDING_KIND, WATCH_FINDING_KIND},
        finding::{Finding, FindingStatus, Impact},
    };
    let finding = |kind: &str| FindingView {
        finding: Finding {
            id: FindingId::new(9),
            kind: kind.into(),
            target: "queue".into(),
            task_id: None,
            run_id: None,
            goal_id: None,
            subject: "rules/due since the start".into(),
            summary: "rules's definition is due to be watched".into(),
            detail: json!({"step": "due", "agent": "rules",
                "ask": "agent cases rules --until 2026-10-20T00:00:00.000Z"})
            .to_string(),
            impact: Impact::Normal,
            first_seen_at: 0,
            last_seen_at: 0,
            occurrences: 1,
            evidence: Vec::new(),
            status: FindingStatus::Open,
            status_reason: None,
            proposal_id: None,
            covered_by_task: None,
            propose_reason: Some("ask for the production set".into()),
            propose_requested_at: None,
            recorded_by: "supervisor".into(),
            updated_at: 0,
        },
        proposal_status: None,
        open_asks: Vec::new(),
        evidence_events: None,
    };
    let prompt_of = |view: &FindingView, answer: Option<&Ask>| {
        finding_planner_prompt(&FindingPlannerMaterial {
            db: Path::new("/q/queue.db"),
            finding: view,
            attempt: 1,
            asks: &[],
            goal: None,
            goal_closed: false,
            siblings: &[],
            answer,
            handover: None,
        })
        .unwrap()
        .text
    };
    let watch = finding(WATCH_FINDING_KIND);
    let prompt = prompt_of(&watch, None);
    for part in [
        "The finding is a step of the supervisor's watch of an agent's definition: do that step",
        "## What to do: a step of an agent's watch",
        "plan no task and submit nothing for it",
        // The detail's command is shown and is the one to run.
        "agent cases rules --until 2026-10-20T00:00:00.000Z",
        "- `due`: ask for the production set of the period: run `dagq <ask>`",
        "then `dagq finding resolve 9 --reason 'set <its id> asked for'`",
        "- `holdout`:",
        "`dagq agent results --agent <agent>`",
        "the hold-out's once rule refuses it: run `dagq finding dismiss 9 --reason '<why>'`",
        "then `dagq finding resolve 9 --reason 'eval <its id> asked for'`",
        "- `refused`:",
        "run `dagq finding dismiss 9 --reason '<the refusal's reason>'`",
        "Never ask to run a set again (`--rerun` is a person's)",
        "a command that is refused is not tried again",
    ] {
        assert!(prompt.contains(part), "{part} in {prompt}");
    }
    // None of the general steps: no proposal, no exclusive choice.
    for gone in [
        "Then do exactly one of these",
        "marked for a proposal",
        "dagq submit",
        "dagq add --goal",
        "planner_question",
        "## A CI failure",
    ] {
        assert!(!prompt.contains(gone), "{gone} in {prompt}");
    }
    // An answer a planner before it carried applies to the step.
    let answer = asked(31, 40);
    let carried = prompt_of(&watch, Some(&answer));
    assert!(
        carried.contains("Apply this answer to the step above."),
        "{carried}"
    );
    assert!(!carried.contains("as step 4 says"), "{carried}");
    // A score under the threshold is an improvement: the general steps.
    let score = prompt_of(&finding(SCORE_FINDING_KIND), None);
    assert!(score.contains("Then do exactly one of these"), "{score}");
    assert!(score.contains("dagq submit ID... --finding 9"), "{score}");
    assert!(!score.contains("a step of an agent's watch"), "{score}");
}

/// A task in progress with `goal` and `context`.
fn grouped_task(id: i64, title: &str, goal: Option<i64>, context: &str) -> Task {
    Task::restore(TaskRecord {
        goal_priority: None,
        id: TaskId::new(id),
        title: title.into(),
        description: "small change".into(),
        acceptance: "works".into(),
        verification_commands: vec!["test -f seed.txt".into()],
        required_evidence: Vec::new(),
        paths: Vec::new(),
        priority: Default::default(),
        change: None,
        status: TaskStatus::InProgress,
        goal_id: goal.map(GoalId::new),
        context: context.into(),
        created_at: String::new(),
        updated_at: String::new(),
        worker: crate::domain::worker::Worker::CLAUDE_HEADLESS,
        named_mode: None,
        wait_for_build: false,
        execution_class: Default::default(),
    })
    .unwrap()
}

/// The Sibling section of a task with a goal lists only the in-progress
/// tasks of that goal; a task without a goal sees every task in
/// progress; neither lists itself.
#[test]
fn siblings_are_the_tasks_in_progress_of_the_same_goal() {
    let in_progress = || {
        vec![
            grouped_task(1, "test task", None, ""),
            grouped_task(2, "a first", Some(1), ""),
            grouped_task(3, "b only", Some(2), ""),
            grouped_task(4, "a second", Some(1), ""),
            grouped_task(5, "alone", None, ""),
        ]
    };
    let ids = |task: &Task| -> Vec<i64> {
        siblings_in_progress(task, in_progress())
            .iter()
            .map(|task| task.id().as_i64())
            .collect()
    };
    // Goal a: its other task only, not task 1 without a goal.
    assert_eq!(ids(&grouped_task(4, "a second", Some(1), "")), [2]);
    assert_eq!(ids(&grouped_task(2, "a first", Some(1), "")), [4]);
    // Goal b: nothing else of it is in progress.
    assert!(ids(&grouped_task(3, "b only", Some(2), "")).is_empty());
    // No goal: every task in progress but itself, in the given order.
    assert_eq!(ids(&grouped_task(5, "alone", None, "")), [1, 2, 3, 4]);
    assert!(siblings_in_progress(&grouped_task(9, "x", None, ""), Vec::new()).is_empty());
}

/// A task's prompt carries its goal as a Goal section (ID, title,
/// description, acceptance, constraints and the doc path, unread), its
/// context as a Context section, its predecessors and its siblings; a
/// task without them says so in the same place, so both prompts have
/// the same sequence of sections.
#[test]
fn prompt_describes_the_goal_the_context_and_its_company_and_keeps_one_shape_without_them() {
    let goal = Goal::restore(GoalRecord {
        priority: Default::default(),
        tags: Vec::new(),
        id: GoalId::new(3),
        title: "goal title".into(),
        description: "goal description\nsecond line".into(),
        acceptance: "goal acceptance".into(),
        constraints: "goal constraints".into(),
        doc: Some("docs/plans/goal.md".into()),
        status: GoalStatus::Open,
        closed_at: None,
        verdict: None,
        created_at: String::new(),
        updated_at: String::new(),
    })
    .unwrap();
    let own_run = run(2, RunStatus::Claimed, None);
    let grouped_task = grouped_task(
        2,
        "grouped",
        Some(3),
        "why this task exists\nread docs/design/x.md first",
    );
    let landed = PredecessorSummary {
        task_id: TaskId::new(1),
        title: "test task".into(),
        result_commit: SHA.into(),
        summary: "done".into(),
    };
    let sibling = task(4, "a second", TaskStatus::InProgress);
    let grouped = prompt(
        &grouped_task,
        &own_run,
        Some(&goal),
        &[landed],
        &[],
        &[sibling],
        None,
        &[],
        None,
    )
    .unwrap()
    .text;
    let alone_task = task(2, "alone", TaskStatus::InProgress);
    let alone = prompt(&alone_task, &own_run, None, &[], &[], &[], None, &[], None)
        .unwrap()
        .text;

    assert!(
            grouped.contains(
                "Goal (the higher-level problem this task and its sibling tasks solve together):\n\
                 Goal ID: 3\nGoal title: goal title\nGoal description:\ngoal description\nsecond line\n\
                 Goal acceptance:\ngoal acceptance\nGoal constraints:\ngoal constraints\n\
                 Goal doc: docs/plans/goal.md (a path in the repository; read it for the full picture)\n"
            ),
            "{grouped}"
        );
    assert!(
        grouped.contains(
            "Context (why this task exists and what to read first):\n\
                 why this task exists\nread docs/design/x.md first\n"
        ),
        "{grouped}"
    );
    assert!(
        grouped.contains(&format!(
            "Predecessor tasks (their changes are already in your base commit):\n\
                 - task 1: test task; result commit {SHA}; summary: done\n"
        )),
        "{grouped}"
    );
    assert!(
        grouped.contains(
            "Sibling tasks in progress (other tasks executing now, each owning its own scope):\n\
                 - task 4: a second\n"
        ),
        "{grouped}"
    );
    for absent in [
        "Goal: none",
        "Context: none",
        "Predecessor tasks: none",
        "Sibling tasks in progress: none",
    ] {
        assert!(!grouped.contains(absent), "{absent}: {grouped}");
    }
    for none in [
        "Goal: none, this task stands alone\n",
        "Context: none\n",
        "Predecessor tasks: none\n",
        "Sibling tasks in progress: none\n",
    ] {
        assert!(alone.contains(none), "{none}: {alone}");
    }
    assert!(!alone.contains("goal title"), "{alone}");
    // Both prompts have the same sections in the same order, between the
    // verification commands and the receipt contract.
    let order = |text: &str| -> Vec<usize> {
        [
            "Task title:",
            "Verification commands",
            "Goal",
            "Context",
            "Predecessor tasks",
            "Sibling tasks in progress",
            "Your assignment is this task only.",
            "Write a completion receipt",
        ]
        .iter()
        .map(|heading| {
            text.find(heading)
                .unwrap_or_else(|| panic!("{heading}: {text}"))
        })
        .collect()
    };
    for text in [&grouped, &alone] {
        assert!(
            order(text).windows(2).all(|pair| pair[0] < pair[1]),
            "{text}"
        );
        // The receipt example shows the optional follow_ups, and the scope rule names it.
        assert!(
                text.contains(
                    "\"summary\":\"...\",\"follow_ups\":[{\"title\":\"...\",\"description\":\"...\",\"category\":\"...\",\"membership_proposal\":{\"classification\":\"required, out_of_scope or undecided\",\"acceptance_items\":[\"...\"],\"reason\":\"...\"}}]}\n"
                ),
                "{text}"
            );
        assert!(
                text.contains(
                    "Your assignment is this task only. Do not change what a sibling task owns; \
                     if you find work outside this task, record it in the receipt as follow_ups instead of doing it.\n"
                ),
                "{text}"
            );
        assert!(text.contains("follow_ups is optional"), "{text}");
        // Each follow_up carries a category (ADR-t947-3), a worker_question a topic (ADR-t947-2).
        assert!(text.contains(&follow_up_categories_line()), "{text}");
        assert!(text.contains("flaky_test ("), "{text}");
        assert!(text.contains(&worker_question_topics_line()), "{text}");
        assert!(text.contains("--topic <code>"), "{text}");
        assert!(text.contains("task_overlap ("), "{text}");
        // The worker reads only what its run needs, never the queue.
        assert!(text.contains(WORKER_READING), "{text}");
        assert!(
            text.contains("the worker section of the repository instructions"),
            "{text}"
        );
        assert!(text.contains("Do not run `dagq list`"), "{text}");
        assert!(!text.contains("Read its repository instructions"), "{text}");
    }
}

/// Task 1571 (ADR-t1566-1 decisions 4 to 6): how much a prompt held to
/// its limits may take before the language's instruction is added.
fn within(fitted: &FittedPrompt, limit: usize) {
    assert_eq!(fitted.bytes.limit, limit);
    assert_eq!(fitted.bytes.total, fitted.text.len());
    assert!(
        fitted.text.len() <= limit - prompt_fit::LANGUAGE_ROOM,
        "{} bytes past the limit of {limit}: {:?}",
        fitted.text.len(),
        fitted.bytes
    );
    assert_eq!(
        fitted.bytes.sections.values().sum::<usize>(),
        fitted.bytes.total,
        "{:?}",
        fitted.bytes
    );
    // The language's instruction is counted when it is added, and the
    // whole stays within the limit.
    let language = crate::domain::language::Language {
        tag: "ja".into(),
        source: crate::domain::language::LanguageSource::Repository,
    };
    let with = fitted.clone().with_language(Some(&language));
    assert_eq!(with.bytes.total, with.text.len());
    assert!(with.bytes.total <= limit, "{}", with.bytes.total);
    assert!(with.bytes.sections.contains_key("language"));
}

fn big(what: &str, bytes: usize) -> String {
    format!("{what} ").repeat(bytes / (what.len() + 1) + 1)
}

fn goal_of(id: i64, bytes: usize) -> Goal {
    Goal::restore(GoalRecord {
        priority: Default::default(),
        tags: Vec::new(),
        id: GoalId::new(id),
        title: big("goal title", 2_000),
        description: big("goal description", bytes),
        acceptance: big("goal acceptance", bytes),
        constraints: big("goal constraints", bytes),
        doc: Some("docs/plans/goal.md".into()),
        status: GoalStatus::Open,
        closed_at: None,
        verdict: None,
        created_at: String::new(),
        updated_at: String::new(),
    })
    .unwrap()
}

fn goal_tasks(n: i64) -> Vec<GoalTask> {
    (1..=n)
        .map(|id| GoalTask {
            id: TaskId::new(id),
            title: big("a goal task's title", 500),
            status: TaskStatus::Completed,
            priority: crate::domain::Priority::Normal,
            priority_source: crate::domain::PrioritySource::Goal,
            priority_by: crate::domain::plan_request::PriorityBy::Ai,
        })
        .collect()
}

fn asked(id: i64, bytes: usize) -> Ask {
    serde_json::from_value(json!({
        "id": id, "kind": "planner_question", "task_id": null, "run_id": null,
        "question": big("question", bytes), "options": [], "answer": big("answer", bytes),
        "asked_by": "planner", "reason_category": "scope", "created_at": 0,
        "answered_at": 1, "closed_at": null,
    }))
    .unwrap()
}

/// What a planner before took the most of: notes and drafts well past
/// the handover's limit.
fn big_handover() -> PlannerHandover {
    PlannerHandover {
        planner_id: crate::domain::PlannerId::new(9),
        notes: (1..=50)
            .map(|n| (100 + n, big(&format!("note {n}"), 5_000)))
            .collect(),
        drafts: (1..=500)
            .map(|id| GoalTask {
                id: TaskId::new(id),
                title: big("draft", 500),
                status: TaskStatus::Draft,
                priority: Default::default(),
                priority_source: crate::domain::PrioritySource::Goal,
                priority_by: crate::domain::plan_request::PriorityBy::Ai,
            })
            .collect(),
    }
}

/// No section went past its own limit: the whole was not cut in its
/// middle.
fn sections_within(fitted: &FittedPrompt) {
    assert!(
        !fitted
            .bytes
            .over_limit
            .as_deref()
            .unwrap_or_default()
            .contains("its middle was cut"),
        "{:?}",
        fitted.bytes
    );
}

fn no_binary() -> BinaryFacts {
    BinaryFacts {
        version: "0.1.0".into(),
        commit: None,
        dependencies: Vec::new(),
        replacements: Vec::new(),
        replacements_unread: 0,
    }
}

fn event_of(id: i64, bytes: usize) -> RunEvent {
    RunEvent {
        id: crate::domain::EventId::new(id),
        task_id: Some(TaskId::new(7)),
        goal_id: None,
        run_id: None,
        kind: "turn_finished".into(),
        payload: json!({"message": big("evidence", bytes)}),
        created_at: String::new(),
        actor: None,
    }
}

fn big_task(id: i64, bytes: usize) -> Task {
    Task::restore(TaskRecord {
        goal_priority: None,
        id: TaskId::new(id),
        title: big("title", 5_000),
        description: big("description", bytes),
        acceptance: big("acceptance", bytes),
        verification_commands: vec![big("cargo test", bytes)],
        required_evidence: Vec::new(),
        paths: Vec::new(),
        priority: Default::default(),
        change: None,
        status: TaskStatus::Draft,
        goal_id: Some(GoalId::new(1)),
        context: big("context", bytes),
        created_at: String::new(),
        updated_at: String::new(),
        worker: crate::domain::worker::Worker::CLAUDE_HEADLESS,
        named_mode: None,
        wait_for_build: false,
        execution_class: Default::default(),
    })
    .unwrap()
}

/// The goal review of a goal with many tasks, each with a long receipt
/// (job 24 of production had 50 tasks in 200,200 bytes), many notes and
/// earlier reviews, and a goal past its own limit stays within
/// [`GOAL_REVIEW_PROMPT_LIMIT`]: the tasks that landed come first, the
/// newest first, the rest are stubs with how to read them, and the
/// required goal is cut and said so.
#[test]
fn a_goal_review_prompt_of_many_large_tasks_stays_within_its_limits() {
    let tasks: Vec<Value> = (1..=200)
            .map(|id| {
                let mut task = json!({"id": id, "title": format!("task {id}"), "status": "completed",
                    "description": big("description", 3_000), "acceptance": big("acceptance", 1_000)});
                // The even ones landed, with a receipt of 10 KB.
                if id % 2 == 0 {
                    task["landed"] = json!({"run_id": RUN, "summary": big("summary", 10_000)});
                }
                task
            })
            .collect();
    let material = GoalReviewMaterial {
        goal: json!({"id": 7, "description": big("goal", 50_000)}),
        tasks,
        replacements: (1..=50)
            .map(|id| json!({"operation": id, "mappings": big("mapping", 2_000)}))
            .collect(),
        follow_ups: (1..=100)
            .map(|id| json!({"task_id": 1000 + id, "reason": big("why", 2_000)}))
            .collect(),
        events: (1..=200)
            .map(|id| json!({"kind": "observation", "at": id, "payload": big("note", 1_000)}))
            .collect(),
        previous: (1..=50)
            .map(|id| json!({"id": id, "summary": big("review", 3_000)}))
            .collect(),
        gaps_in_a_row: 1,
        repo_root: Path::new("/repo"),
    };
    let fitted = goal_review_prompt(&material);
    within(&fitted, GOAL_REVIEW_PROMPT_LIMIT);
    let text = &fitted.text;
    let bytes = &fitted.bytes;
    // Every section left something out, and says how to read it.
    for section in ["goal", "tasks", "follow_ups", "events", "previous"] {
        assert!(
            bytes.omitted.get(section).is_some_and(|n| *n > 0),
            "{section}: {bytes:?}"
        );
    }
    assert!(
        bytes.over_limit.as_deref().unwrap().starts_with("goal: "),
        "{bytes:?}"
    );
    assert!(text.contains("read it whole with `dagq goal show 7 --full`"));
    assert!(text.contains("The tasks left out, in summary:"));
    assert!(text.contains("tasks left out by this section's limit:"));
    assert!(text.contains("`dagq show ID --full` for each, and `dagq events --full --task ID --kind integration_receipt`"));
    assert!(text.contains("notes and edits (the oldest) left out by this section's limit"));
    assert!(text.contains("To read them: `dagq events --full --all --goal 7`."));
    assert!(
        text.contains("To read them: `dagq events --full --goal 7 --kind goal_review_finished`.")
    );
    // The newest landed task is in full; the oldest one that did not
    // land is left out to a stub.
    assert!(
        text.contains(r#""id":200,"#),
        "the newest landed task is shown"
    );
    assert!(text.contains(r#"{"id":1,"landed":false,"status":"completed","title":"task 1"}"#));
    // A task past its own limit is cut and names how to read it.
    assert!(text.contains("bytes left out; read it whole with `dagq show ID --full`"));
    // The instructions and the schema are whole.
    assert!(text.contains("Answer with one JSON object and nothing else"));
    for read in GOAL_REVIEW_READS {
        let form = read.replace("ID", "7");
        let named = text.contains(&form) || text.contains(read);
        assert!(named, "{read} is not named");
    }
}

/// The run review of a task whose title and acceptance are huge stays
/// within [`RUN_REVIEW_PROMPT_LIMIT`] and
/// names where in the review material the acceptance is whole.
#[test]
fn a_run_review_prompt_of_a_huge_acceptance_stays_within_its_limits() {
    let task = big_task(7, 100_000);
    let fitted = review_prompt(
        &task,
        &run_on(Provider::Claude, WorkerMode::Headless),
        "/runs/run/review.md",
        None,
        None,
        &PersonAnswers::default(),
    );
    within(&fitted, RUN_REVIEW_PROMPT_LIMIT);
    let bytes = &fitted.bytes;
    for section in ["title", "acceptance"] {
        assert_eq!(bytes.omitted.get(section), Some(&1), "{section}: {bytes:?}");
    }
    assert!(
        bytes
            .over_limit
            .as_deref()
            .unwrap()
            .starts_with("acceptance: ")
    );
    assert!(fitted.text.contains(
        "read it whole in the review material at /runs/run/review.md, its section Acceptance"
    ));
    // A small task is not cut.
    let small = review_prompt(
        &task_with_evidence(7, Vec::new()),
        &run_on(Provider::Claude, WorkerMode::Headless),
        "r",
        None,
        None,
        &PersonAnswers::default(),
    );
    assert!(
        small.bytes.omitted.is_empty() && small.bytes.over_limit.is_none(),
        "{:?}",
        small.bytes
    );
    within(&small, RUN_REVIEW_PROMPT_LIMIT);
}

/// The judgment of an `already_satisfied` claim names the snapshot's head,
/// the tree checked out at it, the task's acceptance and verification
/// commands and the worker's grounds, and asks for the claim's own
/// verdict; huge material stays within [`CLAIM_REVIEW_PROMPT_LIMIT`], the
/// receipt's parts cut naming the receipt, and a small claim is not cut.
#[test]
fn a_claims_review_prompt_names_its_snapshot_and_stays_within_its_limits() {
    use crate::domain::satisfied::SnapshotVerification;
    let verification = SnapshotVerification {
        head: CommitSha::try_from(SHA).unwrap(),
        commands: 2,
        failure: None,
    };
    let receipt = |summary: &str, check: &str| {
        Receipt::parse(
            &json!({"run_id": RUN, "result": "already_satisfied", "commit": SHA,
                "tests": {"status": "passed", "evidence_or_reason": "e"},
                "e2e": {"status": "not_applicable", "evidence_or_reason": "r"},
                "subagent_review": {"status": "not_applicable", "evidence_or_reason": "r"},
                "summary": summary,
                "base_checks": [{"command": check, "result": "passed"}]})
            .to_string(),
        )
        .unwrap()
    };
    let run = run_on(Provider::Claude, WorkerMode::Headless);
    let tree = Path::new("/q/satisfied/run");
    let small = receipt("the base meets it", "test -f seed.txt");
    let fitted = claim_review_prompt(
        &task_with_evidence(7, Vec::new()),
        &run,
        (Some(&small), "/runs/run/receipt.json"),
        &verification,
        tree,
        None,
    );
    within(&fitted, CLAIM_REVIEW_PROMPT_LIMIT);
    assert!(
        fitted.bytes.omitted.is_empty() && fitted.bytes.over_limit.is_none(),
        "{:?}",
        fitted.bytes
    );
    for said in [
        &format!("main at {SHA} (the snapshot)"),
        "/q/satisfied/run is a checkout of main",
        "verification commands, which passed",
        "the base meets it",
        "- test -f seed.txt: passed",
        "\"verdict\": \"pass\" | \"revise\" | \"concern\"",
        "\"recommendation\": \"close\" | \"retry\" | \"cancel\"",
        "whatever the verdict",
    ] {
        assert!(fitted.text.contains(said), "{said}: {}", fitted.text);
    }
    let huge = receipt(&big("summary", 50_000), &big("cargo test", 50_000));
    let fitted = claim_review_prompt(
        &big_task(7, 100_000),
        &run,
        (Some(&huge), "/runs/run/receipt.json"),
        &verification,
        tree,
        None,
    );
    within(&fitted, CLAIM_REVIEW_PROMPT_LIMIT);
    for section in ["title", "acceptance", "verify", "summary", "base_checks"] {
        assert_eq!(
            fitted.bytes.omitted.get(section),
            Some(&1),
            "{section}: {:?}",
            fitted.bytes
        );
    }
    assert!(
        fitted
            .text
            .contains("read it whole in the receipt at /runs/run/receipt.json, its base_checks")
    );
    assert!(
        fitted.text.ends_with(CLAIM_REVIEW_ANSWER_END),
        "{}",
        fitted.text
    );
    // A receipt that cannot be read leaves its grounds as none.
    let unread = claim_review_prompt(
        &task_with_evidence(7, Vec::new()),
        &run,
        (None, "/runs/run/receipt.json"),
        &verification,
        tree,
        None,
    );
    assert!(
        unread.text.contains("The worker's summary:\n(none)"),
        "{}",
        unread.text
    );
    // A run the recovery job handed off says so and carries its
    // diagnosis, cut past its limit.
    let handed = claim_review_prompt(
        &task_with_evidence(7, Vec::new()),
        &run,
        (Some(&small), "/runs/run/receipt.json"),
        &verification,
        tree,
        Some("seed.txt is on main already"),
    );
    for said in [
        "did not report already_satisfied",
        "the recovery job then handed it over",
        "seed.txt is on main already",
        "the base meets it",
    ] {
        assert!(handed.text.contains(said), "{said}: {}", handed.text);
    }
    assert!(
        !handed
            .text
            .contains("Its worker reported already_satisfied")
    );
    let long = claim_review_prompt(
        &task_with_evidence(7, Vec::new()),
        &run,
        (Some(&small), "/runs/run/receipt.json"),
        &verification,
        tree,
        Some(&big("diagnosis", 20_000)),
    );
    within(&long, CLAIM_REVIEW_PROMPT_LIMIT);
    assert_eq!(
        long.bytes.omitted.get("hand_off"),
        Some(&1),
        "{:?}",
        long.bytes
    );
}

/// The last words of a claim's judgment prompt, its schema's explanation.
const CLAIM_REVIEW_ANSWER_END: &str = "summary is one or two sentences.\n";

/// The recovery job of a headless run whose last turns, alert facts,
/// history, processes, worktree and ended material are huge stays
/// within [`RECOVERY_PROMPT_LIMIT`] (the largest in production had
/// 46,559 bytes of turns): what is cut names the file in the run
/// directory it is in, or says the job cannot read it.
#[test]
fn a_recovery_prompt_of_huge_turns_and_facts_stays_within_its_limits() {
    let task = big_task(7, 50_000);
    let facts = json!({"reason": "idle_process", "processes": big("pid 1 idle", 100_000)});
    let screen = big("turn 3: ended", 200_000);
    let history: Vec<Value> = (1..=100).map(|id| json!({"id": id, "kind": "recovery_finished", "diagnosis": big("diagnosis", 1_000)})).collect();
    let processes: Vec<ProcessInfo> = (1..=100)
        .map(|pid| ProcessInfo {
            pid,
            ppid: 1,
            elapsed_secs: 10,
            cpu_ms: None,
            cwd: Some("/runs/run/worktree".into()),
            command: big("cargo test", 300),
        })
        .collect();
    let status = big(" M src/a.rs", 50_000);
    let binary = BinaryFacts {
            version: format!("0.1.0-dev+{SHA}"),
            commit: Some(SHA.into()),
            dependencies: (1..=500)
                .map(|id| DependencyLanding {
                    task: TaskId::new(id),
                    landed: Some(SHA.into()),
                    held: Some(id % 2 == 0),
                    why: None,
                })
                .collect(),
            replacements: (1..=500)
                .map(|id| json!({"event_id": id, "kind": "update_installed", "at": "t", "previous_version": format!("0.1.0-dev+{SHA}"), "version": format!("0.1.0-dev+{SHA}"), "commit": SHA}))
                .collect(),
            replacements_unread: 3,
        };
    for (run, ended) in [
        (run_on(Provider::Claude, WorkerMode::Headless), None),
        (
            run(7, RunStatus::Failed, None),
            Some(big("Verification log", 100_000)),
        ),
    ] {
        let fitted = recovery_prompt(
            &task,
            &run,
            1,
            &RecoveryMaterial {
                alert: RecoveryAlert::IdleProcess,
                ended: ended.clone(),
                facts: &facts,
                workspace: "ws",
                screen: &screen,
                processes: Ok(processes.clone()),
                git_status: &status,
                head: SHA,
                receipt_commit: None,
                history: &history,
                allowed: &["wait", "stop_processes"],
                binary: &binary,
            },
        )
        .unwrap();
        within(&fitted, RECOVERY_PROMPT_LIMIT);
        let (text, bytes) = (&fitted.text, &fitted.bytes);
        for section in [
            "task",
            "facts",
            "screen",
            "processes",
            "git_status",
            "history",
            "dependencies",
            "replacements",
        ] {
            assert!(
                bytes.omitted.get(section).is_some_and(|n| *n > 0),
                "{section}: {bytes:?}"
            );
        }
        assert!(
            bytes.over_limit.as_deref().unwrap().contains("task: "),
            "{bytes:?}"
        );
        assert!(
            text.contains("the task as the run claimed it is in prompt.txt of the run directory")
        );
        assert!(text.contains(NOT_READABLE));
        assert!(text.contains("of them (the oldest) left out by this section's limit"));
        assert!(text.contains("read the files of the worktree at /runs/run/worktree"));
        assert!(text.contains("Answer with one JSON object and nothing else"));
        // The binary's sections (task 1633) keep their limits, the
        // newest replacements and the dependencies the build does not
        // hold first, and name the run directory's file for the rest.
        assert!(bytes.sections["binary"] <= RECOVERY_BINARY_BYTES);
        assert!(bytes.sections["dependencies"] <= RECOVERY_DEPENDENCIES_BYTES + 600);
        assert!(bytes.sections["replacements"] <= RECOVERY_REPLACEMENTS_BYTES + 600);
        assert_eq!(
            bytes.omitted["replacements"],
            500 - RECOVERY_REPLACEMENTS,
            "{bytes:?}"
        );
        assert!(text.contains(&format!(
            "{} of them (the oldest) left out by this section's limit: 1, 2,",
            500 - RECOVERY_REPLACEMENTS
        )));
        // Those the queue's read left out past its limit are counted too.
        assert!(text.contains(
            "(3 older update_installed since the claim were left out by the queue's read limit:"
        ));
        assert!(text.contains("\"event_id\":500,"));
        assert!(!text.contains("\"event_id\":490,"));
        assert!(text.contains("of them (those the build does not hold were chosen first) left out by this section's limit: task 2, task 4,"));
        assert!(text.contains("\"task\":1}"));
        assert!(text.contains(
            "To read them: read recovery-idle_process-1.binary.json of the run directory."
        ));
        if ended.is_some() {
            assert_eq!(bytes.omitted.get("ended"), Some(&1));
            assert!(text.contains("the run directory has the receipt, the verification logs"));
        } else {
            assert!(text.contains(
                "each turn's whole output is in turns/turn-NNNNNN.jsonl of the run directory"
            ));
        }
    }
}

fn binary_event(id: i64, run: Option<&str>, kind: &str, payload: Value) -> RunEvent {
    RunEvent {
        id: crate::domain::EventId::new(id),
        task_id: run.map(|_| TaskId::new(7)),
        goal_id: None,
        run_id: run.map(|run| RunId::new(run).unwrap()),
        kind: kind.into(),
        payload,
        created_at: format!("t{id}"),
        actor: None,
    }
}

/// The recovery job's binary facts (task 1633) name the build now, the
/// binary's replacements since the run's claim (not before it, not of
/// the plugin only, not another run's handoff) and whether the build
/// holds each dependency's landing.
#[test]
fn binary_facts_name_the_build_its_replacements_since_the_claim_and_what_it_holds() {
    const OTHER: &str = "00000000-0000-4000-8000-000000000002";
    let run = run(7, RunStatus::Failed, None);
    let events = vec![
        binary_event(10, Some(RUN), "run_claimed", json!({})),
        binary_event(
            11,
            Some(OTHER),
            "supervisor_handed_off",
            json!({"version": "x"}),
        ),
        binary_event(
            21,
            Some(RUN),
            "supervisor_handed_off",
            json!({"previous_version": "0.1.0-dev+aaa", "version": "0.1.0-dev+bbb", "status": "running"}),
        ),
    ];
    // Newest first, as the queue reads them.
    let updates = vec![
        binary_event(
            20,
            None,
            "update_installed",
            json!({"previous_version": "0.1.0-dev+aaa", "version": "0.1.0-dev+bbb", "commit": "bbb"}),
        ),
        binary_event(
            15,
            None,
            "update_installed",
            json!({"plugin_only": true, "version": "0.1.0"}),
        ),
        binary_event(12, None, "update_started", json!({"commit": "bbb"})),
        binary_event(
            5,
            None,
            "update_installed",
            json!({"version": "0.1.0-dev+aaa"}),
        ),
    ];
    let landings = vec![
        (TaskId::new(1), Some("aaa".to_owned())),
        (TaskId::new(2), Some("ccc".to_owned())),
        (TaskId::new(3), None),
        (TaskId::new(4), Some("zzz".to_owned())),
    ];
    let holds = |commit: &str, build: &str| -> Result<bool> {
        assert_eq!(build, "bbb");
        match commit {
            "aaa" => Ok(true),
            "ccc" => Ok(false),
            _ => anyhow::bail!("unknown commit {commit}"),
        }
    };
    let facts = binary_facts(
        "0.1.0-dev+bbb.dirty",
        &run,
        &events,
        (&updates, 0),
        &landings,
        &holds,
    );
    assert_eq!(facts.version, "0.1.0-dev+bbb.dirty");
    assert_eq!(facts.commit.as_deref(), Some("bbb"));
    assert_eq!(
        facts.replacements,
        vec![
            json!({"event_id": 20, "kind": "update_installed", "at": "t20", "previous_version": "0.1.0-dev+aaa", "version": "0.1.0-dev+bbb", "commit": "bbb"}),
            json!({"event_id": 21, "kind": "supervisor_handed_off", "at": "t21", "previous_version": "0.1.0-dev+aaa", "version": "0.1.0-dev+bbb"}),
        ]
    );
    let held: Vec<(Option<bool>, Option<&str>)> = facts
        .dependencies
        .iter()
        .map(|d| (d.held, d.why.as_deref()))
        .collect();
    assert_eq!(
        held,
        vec![
            (Some(true), None),
            (Some(false), None),
            (None, Some("it has not landed")),
            (None, Some("unknown commit zzz")),
        ]
    );
    assert_eq!(
        serde_json::to_value(&facts.dependencies[0]).unwrap(),
        json!({"task": 1, "landed": "aaa", "held": true})
    );
}

/// Without a replacement since the claim, and for a build that names
/// no commit, the facts say so rather than guess.
#[test]
fn binary_facts_of_a_build_without_a_commit_cannot_tell_what_it_holds() {
    let run = run(7, RunStatus::Failed, None);
    let events = vec![binary_event(10, Some(RUN), "run_claimed", json!({}))];
    let updates = vec![binary_event(
        5,
        None,
        "update_installed",
        json!({"version": "0.1.0"}),
    )];
    let landings = vec![(TaskId::new(1), Some("aaa".to_owned()))];
    let holds = |_: &str, _: &str| -> Result<bool> { panic!("nothing to ask git") };
    let facts = binary_facts("0.1.0", &run, &events, (&updates, 0), &landings, &holds);
    assert_eq!(facts.commit, None);
    assert!(facts.replacements.is_empty());
    assert_eq!(facts.dependencies[0].held, None);
    assert!(
        facts.dependencies[0]
            .why
            .as_deref()
            .unwrap()
            .contains("names no commit")
    );
    // A run with no event of its own yet counts from its claim time.
    let facts = binary_facts("0.1.0", &run, &[], (&updates, 0), &[], &holds);
    assert_eq!(facts.replacements.len(), 1);
}

/// The recovery prompt carries the binary's facts right after the
/// alert's, counted in their own sections, and tells a job that may
/// retry that a run the binary failed is retried once the build holds
/// the landing it waited for (task 1633).
#[test]
fn a_recovery_prompt_carries_the_binary_and_when_a_replacement_lets_it_retry() {
    let task = task(7, "work", TaskStatus::InProgress);
    let facts = json!({"alert": "failed"});
    let binary = BinaryFacts {
        version: format!("0.1.0-dev+{SHA}"),
        commit: Some(SHA.into()),
        dependencies: vec![DependencyLanding {
            task: TaskId::new(1065),
            landed: Some("338d7a5".into()),
            held: Some(true),
            why: None,
        }],
        replacements: vec![json!({"event_id": 9, "kind": "update_installed", "version": "v2"})],
        replacements_unread: 0,
    };
    let prompt = |allowed: &[&str], binary: &BinaryFacts| {
        recovery_prompt(
            &task,
            &run(7, RunStatus::Failed, None),
            2,
            &RecoveryMaterial {
                alert: RecoveryAlert::Failed,
                ended: Some("Last error of the run: ...".into()),
                facts: &facts,
                workspace: "ws",
                screen: "",
                processes: Ok(Vec::new()),
                git_status: "",
                head: SHA,
                receipt_commit: None,
                history: &[],
                allowed,
                binary,
            },
        )
        .unwrap()
    };
    let fitted = prompt(&crate::domain::recovery::ENDED_ACTIONS, &binary);
    let text = &fitted.text;
    assert!(text.contains(&format!(
            "Binary of the supervisor (the fixed binary the runtime runs) now: 0.1.0-dev+{SHA} (commit {SHA})"
        )));
    assert!(text.contains("{\"held\":true,\"landed\":\"338d7a5\",\"task\":1065}"));
    assert!(text.contains("\"event_id\":9"));
    assert!(text.contains(RECOVERY_BINARY_RULE));
    let facts_at = text.find("Alert facts:").unwrap();
    let binary_at = text.find("Binary of the supervisor").unwrap();
    assert!(facts_at < binary_at && binary_at < text.find("Last error of the run").unwrap());
    for section in ["binary", "dependencies", "replacements"] {
        assert!(fitted.bytes.sections[section] > 0, "{section}");
    }
    assert!(fitted.bytes.omitted.is_empty(), "{:?}", fitted.bytes);
    // A live session's job that may not retry gets no such rule, and
    // a task without dependencies or replacements says none.
    let fitted = prompt(&["wait", "stop_processes"], &no_binary());
    assert!(!fitted.text.contains(RECOVERY_BINARY_RULE));
    assert!(
        fitted
            .text
            .contains("0.1.0 (it names no commit: a release or an unknown build)")
    );
    assert!(fitted.text.contains("one JSON line each:\nnone\n"));
}

/// The planner opened for a revise of a proposal with many long tasks
/// and reasons stays within [`RUNTIME_PLANNER_PROMPT_LIMIT`].
#[test]
fn a_runtime_planner_prompt_of_many_reasons_and_carried_answers_stays_within_its_limits() {
    let tasks: Vec<Task> = (1..=500)
        .map(|id| task(id, &big("title", 2_000), TaskStatus::Submitted))
        .collect();
    let reasons: Vec<String> = (1..=100)
        .map(|n| big(&format!("reason {n}"), 5_000))
        .collect();
    // With the most a planner before it can leave (ADR-t1704-1).
    let answers: Vec<Ask> = (1..=20).map(|id| asked(id, 20_000)).collect();
    let handover = big_handover();
    let fitted = runtime_planner_prompt(
        Path::new("/q/queue.db"),
        ProposalId::new(3),
        &tasks,
        &reasons,
        Some(TaskId::new(42)),
        Carried {
            answers: &answers,
            handovers: std::slice::from_ref(&handover),
            replacement: None,
        },
    )
    .unwrap();
    within(&fitted, RUNTIME_PLANNER_PROMPT_LIMIT);
    sections_within(&fitted);
    assert!(
        fitted.bytes.omitted["answer"] > 0 && fitted.bytes.omitted["handover"] > 0,
        "{:?}",
        fitted.bytes
    );
    assert!(
        fitted
            .text
            .contains("answered asks left out by this section's limit")
    );
    let (text, bytes) = (&fitted.text, &fitted.bytes);
    assert!(
        bytes.omitted["tasks"] > 0 && bytes.omitted["reasons"] > 0,
        "{bytes:?}"
    );
    assert!(text.contains("tasks (the oldest) left out by this section's limit"));
    assert!(text.contains("To read them: `dagq proposal show 3`."));
    assert!(text.contains("more reasons left out by this section's limit. read it whole with `dagq events --full --task 42 --kind plan_review_finished`."));
    assert!(text.contains(
        "read it whole with `dagq events --full --task 42 --kind plan_review_finished`]"
    ));
    assert!(!text.contains("for a task of proposal"));
    // The first reasons are kept, cut to their own limit.
    assert!(text.contains("- reason 1 reason 1"));
    assert!(text.contains("dagq submit --proposal 3"));
}

/// A revise of a proposal that replaces a parked task, opened after the
/// planner of the operation's request ended, carries what that planner
/// read (the conditions as the operation fixed them, the snapshot, the
/// latest plan, each cut with a way to read the rest) and the steps of
/// submitting the proposal again with `--replacement` whole, past every
/// section's limit; it never asks for a submit without the plan
/// (ADR-t1521-2 decisions 3 and 4).
#[test]
fn a_revise_planner_of_a_replacement_gets_its_material_and_resubmits_with_the_plan() {
    use crate::domain::replan_plan::{
        CarrySource, ItemDisposition, PlanChild, PlanChoice, PlanMapping, PlanRevision,
        ReplacementPlan,
    };
    let mut brief = huge_replan_brief();
    let op = &brief.operation;
    brief.plan = Some(PlanRevision {
        revision: 2,
        proposal_id: ProposalId::new(3),
        plan: ReplacementPlan {
            operation_id: op.id,
            task_version: op.task_version.clone(),
            goal_acceptance_version: None,
            choice: PlanChoice::Split,
            rationale: "why ".repeat(15_000),
            children: vec![PlanChild {
                task_id: TaskId::new(11),
                carry: CarrySource::SnapshotHead,
                order: Some(1),
            }],
            mappings: vec![PlanMapping {
                ordinal: 1,
                hash: op.conditions.acceptance[0].hash.clone(),
                disposition: ItemDisposition::Split,
                children: Vec::new(),
                landed_task_id: None,
                authority_ask_id: None,
                reason: None,
            }],
            successors: Vec::new(),
        },
        hash: "h".into(),
        approved_at: None,
    });
    let tasks: Vec<Task> = (1..=500)
        .map(|id| task(id, &big("title", 2_000), TaskStatus::Submitted))
        .collect();
    let reasons: Vec<String> = (1..=100)
        .map(|n| big(&format!("reason {n}"), 5_000))
        .collect();
    let answers: Vec<Ask> = (1..=20).map(|id| asked(id, 20_000)).collect();
    let handover = big_handover();
    let fitted = runtime_planner_prompt(
        Path::new("/q/queue.db"),
        ProposalId::new(3),
        &tasks,
        &reasons,
        Some(TaskId::new(42)),
        Carried {
            answers: &answers,
            handovers: std::slice::from_ref(&handover),
            replacement: Some((&brief, Some(Path::new("/q/replans/snap-1")))),
        },
    )
    .unwrap();
    within(&fitted, RUNTIME_REPLACEMENT_PROMPT_LIMIT);
    sections_within(&fitted);
    assert!(fitted.bytes.omitted["replan"] > 0, "{:?}", fitted.bytes);
    let text = &fitted.text;
    for expected in [
        "## Replan operation 1 of task 1".to_owned(),
        format!("task_version `{}`", brief.operation.task_version),
        format!(
            "- 1 `{}` item 1",
            brief.operation.conditions.acceptance[0].hash
        ),
        "read it with `dagq replans 1` and `dagq show 1 --full`".to_owned(),
        "with `dagq replans 1`. Fix what the reasons point at".to_owned(),
        "]\nThis proposal replaces task 1 of replan operation 1:".to_owned(),
        "`dagq submit --proposal 3 --replacement <plan.json>`".to_owned(),
        "Submit refuses this proposal without `--replacement`".to_owned(),
        "The plan holds `operation_id` 1".to_owned(),
        "`successors` (one per task that waits for task 1".to_owned(),
        "Never edit task 1, its run or its worktree.".to_owned(),
        "- reason 1 reason 1".to_owned(),
        // The deletion's ask is about the parked task, with an option per
        // item and `keep`, and the resubmit names it (ADR-t1521-1
        // decision 3).
        "ask it about task 1 itself, `dagq ask --task 1 --kind planner_question --because scope".to_owned(),
        "--option delete:<ordinal>:<the first 12 characters of its hash> --option keep` (one delete option per item)".to_owned(),
        "map the item `disposition` `deleted` with `authority_ask_id` naming that ask and submit again as below".to_owned(),
        "An ask about a child, or one asked before the operation began, allows no deletion.".to_owned(),
        "raise it to the inbox with `dagq ask --task 1 --kind planner_question --because scope` (about task 1, never a child)".to_owned(),
    ] {
        assert!(text.contains(&expected), "{expected}");
    }
    assert!(!text.contains("submit it again with `dagq submit --proposal 3`."));
    assert!(!text.contains("`dagq ask --task ID"));
    // What the material names to read the rest by is a planner's read.
    for (named, form) in [
        ("`dagq replans 1`", "dagq replans ID"),
        ("`dagq show 1 --full`", "dagq show ID --full"),
    ] {
        assert!(text.contains(named), "{named}");
        assert!(PLANNER_READS.contains(&form), "{form}");
    }
}

/// The planner opened for a revise of a proposal that replaces no parked
/// task is given the prompt it had: the plain resubmit, and nothing of a
/// replanning operation.
#[test]
fn a_revise_planner_of_another_proposal_keeps_its_prompt() {
    let fitted = runtime_planner_prompt(
        Path::new("/q/queue.db"),
        ProposalId::new(3),
        &[task(5, "work", TaskStatus::Submitted)],
        &["fix it".into()],
        Some(TaskId::new(42)),
        Carried::default(),
    )
    .unwrap();
    assert_eq!(fitted.bytes.limit, RUNTIME_PLANNER_PROMPT_LIMIT);
    let text = &fitted.text;
    assert!(text.contains(
        "Its tasks:\n- task 5 (submitted): work\nFollow the dagq-planner skill of the dagq plugin: read the proposal with `dagq proposal show 3` and each task with `dagq show ID`, fix what the reasons point at, and submit it again with `dagq submit --proposal 3`.\n"
    ), "{text}");
    assert!(text.contains(
        "A fix that changes the plan's intent (acceptance, scope, the relation to the goal) needs a person: raise it to the inbox with `dagq ask --task ID --kind planner_question --because scope` as the skill describes, stop,"
    ), "{text}");
    for absent in [
        "replacement",
        "Replan operation",
        "dagq replans",
        "delete:<ordinal>",
        "authority_ask_id",
        "never a child",
    ] {
        assert!(!text.contains(absent), "{absent}\n{text}");
    }
    assert!(!fitted.bytes.sections.contains_key("replan"));
}

/// ADR-t1704-1 decision 3: a planner the runtime opens for a revise
/// with the answer the planner before it stopped at carries the
/// question, the answer and what that planner left, each held to its
/// limit: the notes the newest first, each cut, the ones left out and
/// the draft lines left out counted in `handover` with how to read
/// them, and the section's bytes recorded.
#[test]
fn a_planner_carrying_an_answer_shows_what_the_planner_before_it_left_within_its_limits() {
    let handover = PlannerHandover {
        planner_id: crate::domain::PlannerId::new(9),
        notes: (1..=10)
            .map(|n| (100 + n, big(&format!("note {n}"), 3_000)))
            .collect(),
        drafts: (1..=200)
            .map(|id| GoalTask {
                id: TaskId::new(id),
                title: big("draft", 100),
                status: TaskStatus::Draft,
                priority: Default::default(),
                priority_source: crate::domain::PrioritySource::Goal,
                priority_by: crate::domain::plan_request::PriorityBy::Ai,
            })
            .collect(),
    };
    let answers = [asked(5, 100)];
    let fitted = runtime_planner_prompt(
        Path::new("/q/queue.db"),
        ProposalId::new(3),
        &[],
        &["split it".to_owned()],
        Some(TaskId::new(42)),
        Carried {
            answers: &answers,
            handovers: std::slice::from_ref(&handover),
            replacement: None,
        },
    )
    .unwrap();
    within(&fitted, RUNTIME_PLANNER_PROMPT_LIMIT);
    let (text, bytes) = (&fitted.text, &fitted.bytes);
    assert!(text.contains("answer to ask 5: answer answer"), "{text}");
    assert!(text.contains("## What planner 9 before you left"));
    // The newest notes are kept, cut to their own limit.
    assert!(text.contains("- (event 110) note 10"));
    assert!(text.contains("read it whole with `dagq events --full --all --after 109 --limit 1`"));
    assert!(!text.contains("- (event 101) note 1 "));
    assert!(text.contains("notes (the oldest; event IDs) left out by this section's limit: 101"));
    assert!(text.contains("To read them: `dagq show ID --full` for each."));
    // Each note counts once, left out or cut; each draft line too.
    let notes_kept = (1..=10)
        .filter(|n| text.contains(&format!("- (event {}) note", 100 + n)))
        .count();
    let lines_kept = (1..=200)
        .filter(|id| text.contains(&format!("- task {id} (draft)")))
        .count();
    assert_eq!(
        bytes.omitted["handover"],
        10 + (200 - lines_kept),
        "{notes_kept} notes kept: {bytes:?}"
    );
    assert!(bytes.sections["handover"] > 0);
    assert!(bytes.sections["answer"] > 0);
    assert!(text.contains(BEFORE_YOU_STOP_AT_A_QUESTION));
}

/// ADR-t1704-1 decision 3: a revise carrying the answers of two
/// planners' questions shows what each left, in a section of its own,
/// the two within the handover's one limit, with the notes and draft
/// lines left out counted and named the same way.
#[test]
fn a_planner_carrying_answers_of_two_planners_shows_what_each_left_within_one_limit() {
    let of = |planner: i64, first_note: i64, first_draft: i64| PlannerHandover {
        planner_id: crate::domain::PlannerId::new(planner),
        notes: (0..10)
            .map(|n| (first_note + n, big(&format!("note {planner}-{n}"), 3_000)))
            .collect(),
        drafts: (first_draft..first_draft + 100)
            .map(|id| GoalTask {
                id: TaskId::new(id),
                title: big("draft", 100),
                status: TaskStatus::Draft,
                priority: Default::default(),
                priority_source: crate::domain::PrioritySource::Goal,
                priority_by: crate::domain::plan_request::PriorityBy::Ai,
            })
            .collect(),
    };
    let handovers = [of(9, 100, 1), of(12, 200, 1_001)];
    let answers = [asked(5, 100), asked(6, 100)];
    let fitted = runtime_planner_prompt(
        Path::new("/q/queue.db"),
        ProposalId::new(3),
        &[],
        &["split it".to_owned()],
        Some(TaskId::new(42)),
        Carried {
            answers: &answers,
            handovers: &handovers,
            replacement: None,
        },
    )
    .unwrap();
    within(&fitted, RUNTIME_PLANNER_PROMPT_LIMIT);
    sections_within(&fitted);
    let (text, bytes) = (&fitted.text, &fitted.bytes);
    assert!(text.contains("## What planner 9 before you left"), "{text}");
    assert!(
        text.contains("## What planner 12 before you left"),
        "{text}"
    );
    // Each planner's newest note is kept, the oldest named as left out.
    assert!(text.contains("- (event 109) note 9-9"));
    assert!(text.contains("- (event 209) note 12-9"));
    assert!(text.contains("notes (the oldest; event IDs) left out by this section's limit: 100"));
    assert!(text.contains("notes (the oldest; event IDs) left out by this section's limit: 200"));
    assert!(text.contains("- task 100 (draft)") && text.contains("- task 1100 (draft)"));
    // The two together, headings and lines of what was left out
    // included, take no more than the one limit.
    assert!(
        bytes.sections["handover"] <= PLANNER_HANDOVER_BYTES,
        "{bytes:?}"
    );
    let lines_kept = (1..=100)
        .chain(1_001..=1_100)
        .filter(|id| text.contains(&format!("- task {id} (draft)")))
        .count();
    assert_eq!(
        bytes.omitted["handover"],
        20 + (200 - lines_kept),
        "{bytes:?}"
    );
}

/// A planner's handover with `notes` notes and `drafts` drafts of a few
/// bytes each: planner `planner`'s notes are events `planner * 100 + n`
/// and its drafts tasks `planner * 100 + n`.
fn small_handover(planner: i64, notes: i64, drafts: i64) -> PlannerHandover {
    PlannerHandover {
        planner_id: crate::domain::PlannerId::new(planner),
        notes: (1..=notes)
            .map(|n| (planner * 100 + n, big(&format!("note {planner}-{n}"), 200)))
            .collect(),
        drafts: (1..=drafts)
            .map(|n| GoalTask {
                id: TaskId::new(planner * 100 + n),
                title: format!("draft {planner}-{n}"),
                status: TaskStatus::Draft,
                priority: Default::default(),
                priority_source: crate::domain::PrioritySource::Goal,
                priority_by: crate::domain::plan_request::PriorityBy::Ai,
            })
            .collect(),
    }
}

fn revise_prompt_carrying(handovers: &[PlannerHandover]) -> FittedPrompt {
    let answers = [asked(5, 100)];
    let fitted = runtime_planner_prompt(
        Path::new("/q/queue.db"),
        ProposalId::new(3),
        &[],
        &["split it".to_owned()],
        Some(TaskId::new(42)),
        Carried {
            answers: &answers,
            handovers,
            replacement: None,
        },
    )
    .unwrap();
    within(&fitted, RUNTIME_PLANNER_PROMPT_LIMIT);
    sections_within(&fitted);
    assert!(
        fitted.bytes.sections["handover"] <= PLANNER_HANDOVER_BYTES,
        "{:?}",
        fitted.bytes
    );
    fitted
}

/// Three planners whose notes and drafts are small: each gets its section
/// with all of them, and nothing is left out.
#[test]
fn a_planner_carrying_the_small_material_of_three_planners_shows_all_of_it() {
    let handovers = [
        small_handover(1, 1, 1),
        small_handover(2, 2, 3),
        small_handover(3, 1, 2),
    ];
    let fitted = revise_prompt_carrying(&handovers);
    let text = &fitted.text;
    for handover in &handovers {
        let planner = handover.planner_id;
        assert!(
            text.contains(&format!("## What planner {planner} before you left")),
            "{text}"
        );
        for (id, _) in &handover.notes {
            assert!(text.contains(&format!("- (event {id}) note")), "{text}");
        }
        for task in &handover.drafts {
            assert!(
                text.contains(&format!("- task {} (draft)", task.id)),
                "{text}"
            );
        }
    }
    assert!(!text.contains("left out by this section's limit"), "{text}");
    assert!(
        !fitted.bytes.omitted.contains_key("handover"),
        "{:?}",
        fitted.bytes
    );
}

/// How a line naming what was left out names `ids`: at most 40, then how
/// many more.
fn named(ids: &[String]) -> String {
    let mut text = ids.iter().take(40).cloned().collect::<Vec<_>>().join(", ");
    if ids.len() > 40 {
        text.push_str(&format!(" and {} more", ids.len() - 40));
    }
    text
}

/// The notes and draft lines of `handovers` the prompt's text shows.
fn shown_items(text: &str, handovers: &[PlannerHandover]) -> usize {
    handovers
        .iter()
        .map(|h| {
            h.notes
                .iter()
                .filter(|(id, _)| text.contains(&format!("- (event {id}) ")))
                .count()
                + h.drafts
                    .iter()
                    .filter(|t| text.contains(&format!("- task {} (draft)", t.id)))
                    .count()
        })
        .sum()
}

/// More planners than the handover's limit has sections for: the first
/// get theirs, and the rest are named with each of their notes and
/// drafts, counted one by one as the shown planners' are, and how to read
/// every draft such a planner created or edited.
#[test]
fn a_planner_carrying_answers_of_many_planners_names_the_notes_and_drafts_past_the_limit() {
    let handovers: Vec<PlannerHandover> = (1..=7).map(|p| small_handover(p, 4, 10)).collect();
    let fitted = revise_prompt_carrying(&handovers);
    let (text, bytes) = (&fitted.text, &fitted.bytes);
    let (shown, _) = handover_budgets(&handovers);
    assert!((2..7).contains(&shown), "{shown}");
    for planner in 1..=7 {
        assert_eq!(
            text.contains(&format!("## What planner {planner} before you left")),
            planner <= shown as i64,
            "{text}"
        );
    }
    assert!(text.contains("## What the other planners before you left"));
    let past: Vec<String> = ((shown as i64 + 1)..=7).map(|p| p.to_string()).collect();
    assert!(text.contains(&format!(
        "({} planners (planner IDs) left out by this section's limit: {}. To read them: `dagq events --all --kind observation --kind task_created --kind task_edited --full --limit 1000`, the events whose actor is each planner.)",
        past.len(),
        named(&past)
    )));
    let ids = |of: fn(&PlannerHandover) -> Vec<i64>| -> Vec<String> {
        handovers[shown..]
            .iter()
            .flat_map(of)
            .map(|id| id.to_string())
            .collect()
    };
    let notes = ids(|h| h.notes.iter().map(|(id, _)| *id).collect());
    let drafts = ids(|h| h.drafts.iter().map(|task| task.id.as_i64()).collect());
    assert!(text.contains(&format!(
        "({} notes (event IDs) left out by this section's limit: {}.",
        notes.len(),
        named(&notes)
    )));
    assert!(text.contains(&format!(
        "({} drafts they created or edited, still drafts (task IDs) left out by this section's limit: {}. To read them: `dagq show ID --full` for each.)",
        drafts.len(),
        named(&drafts)
    )));
    // Every note and draft line not shown counts once, the past planners'
    // too.
    assert_eq!(
        bytes.omitted["handover"],
        7 * 14 - shown_items(text, &handovers[..shown]),
        "{bytes:?}"
    );
}

/// However many planners asked, with nothing left or with notes and
/// drafts of long IDs, the handover takes no more than its limit, its
/// headings and the lines naming what was left out included; the
/// planners, notes and drafts past it are counted and named at most 40 at
/// a time with how many more.
#[test]
fn a_planner_carrying_answers_of_thousands_of_planners_stays_within_the_handover_limit() {
    let empty: Vec<PlannerHandover> = (1..=2_000)
        .map(|planner| PlannerHandover {
            planner_id: crate::domain::PlannerId::new(planner),
            notes: Vec::new(),
            drafts: Vec::new(),
        })
        .collect();
    let fitted = revise_prompt_carrying(&empty);
    assert!(
        fitted
            .text
            .contains("planners (planner IDs) left out by this section's limit:")
    );
    assert!(
        fitted
            .text
            .contains(" more. To read them: `dagq events --all --kind observation")
    );
    assert!(
        !fitted.bytes.omitted.contains_key("handover"),
        "{:?}",
        fitted.bytes
    );
    let long = 1_000_000_000_000_i64;
    let full: Vec<PlannerHandover> = (1..=2_000)
        .map(|planner| PlannerHandover {
            planner_id: crate::domain::PlannerId::new(long + planner),
            notes: (0..3)
                .map(|n| {
                    (
                        long + planner * 10 + n,
                        big(&format!("note {planner}-{n}"), 3_000),
                    )
                })
                .collect(),
            drafts: (0..30)
                .map(|n| GoalTask {
                    id: TaskId::new(long + planner * 100 + n),
                    title: big("draft", 100),
                    status: TaskStatus::Draft,
                    priority: Default::default(),
                    priority_source: crate::domain::PrioritySource::Goal,
                    priority_by: crate::domain::plan_request::PriorityBy::Ai,
                })
                .collect(),
        })
        .collect();
    let fitted = revise_prompt_carrying(&full);
    let (shown, _) = handover_budgets(&full);
    assert!(shown >= 1);
    assert!(
        fitted
            .text
            .contains("drafts they created or edited, still drafts (task IDs) left out")
    );
    assert_eq!(
        fitted.bytes.omitted["handover"],
        // The shown notes are cut, so each counts too.
        2_000 * 33 - shown_items(&fitted.text, &full[..shown])
            + full[..shown]
                .iter()
                .flat_map(|h| &h.notes)
                .filter(|(id, _)| fitted.text.contains(&format!("- (event {id}) ")))
                .count(),
        "{:?}",
        fitted.bytes
    );
}

/// One planner gets the limit as before; several share what their
/// headings, the lines naming what was left out and the paragraph naming
/// the planners past them leave, one that needs less keeping all it needs
/// and leaving the rest to the others.
#[test]
fn the_handover_limit_is_shared_by_what_each_planner_needs() {
    let all = PLANNER_HANDOVER_BYTES;
    assert_eq!(
        handover_budgets(&[big_handover()]),
        (1, vec![(all - all / 4, all / 4)])
    );
    let small = [
        small_handover(1, 1, 1),
        small_handover(2, 2, 0),
        small_handover(3, 0, 2),
    ];
    assert_eq!(
        handover_budgets(&small),
        (3, small.iter().map(handover_needs).collect())
    );
    let pair = [big_handover(), small_handover(2, 2, 1)];
    let (shown, budgets) = handover_budgets(&pair);
    assert_eq!(shown, 2);
    let (notes, drafts) = handover_needs(&pair[1]);
    assert_eq!(budgets[1], (notes, drafts));
    let left = all - handover_fixed(&pair[0]) - handover_fixed(&pair[1]) - notes - drafts;
    assert_eq!(budgets[0], (left - left / 4, left / 4));
    for planners in [2, 3, 7, 12, 40] {
        let handovers: Vec<PlannerHandover> = (1..=planners)
            .map(|planner| PlannerHandover {
                planner_id: crate::domain::PlannerId::new(planner),
                ..big_handover()
            })
            .collect();
        let (shown, budgets) = handover_budgets(&handovers);
        let fixed: usize = handovers[..shown].iter().map(handover_fixed).sum();
        let used: usize = budgets.iter().map(|(n, d)| n + d).sum();
        assert!(used + fixed <= all, "{planners}: {budgets:?}");
        assert!(
            budgets
                .iter()
                .all(|&(n, d)| n + d >= PLANNER_HANDOVER_MIN_SHARE),
            "{planners}: {budgets:?}"
        );
    }
}

#[test]
fn a_runtime_planner_without_a_review_anchor_names_no_read_command() {
    let fitted = runtime_planner_prompt(
        Path::new("/q/queue.db"),
        ProposalId::new(3),
        &[],
        &[big("reason", 5_000)],
        None,
        Carried::default(),
    )
    .unwrap();
    assert!(fitted.text.contains(
        "the plan review anchor is unknown, so there is no known way to read the full reasons"
    ));
    assert!(!fitted.text.contains("--kind plan_review_finished"));
}

/// The planner opened for a large bundle of follow_up drafts with a
/// huge source task, receipt, goals and an answer stays within
/// [`DRAFT_PLANNER_PROMPT_LIMIT`].
#[test]
fn a_draft_planner_prompt_of_a_large_bundle_stays_within_its_limits() {
    let members: Vec<(DraftTarget, usize)> = (10..60)
        .map(|id| {
            (
                DraftTarget {
                    task: big_task(id, 20_000),
                    origin: DraftOrigin::FollowUp,
                    material: json!({"source_run_id": RUN, "source_task_id": 3, "index": id}),
                    planners: 0,
                    revisit: None,
                },
                1,
            )
        })
        .collect();
    let key = BundleKey::of(
        DraftOrigin::FollowUp,
        &members[0].0.material,
        members[0].0.task.id(),
    );
    let source = big_task(3, 50_000);
    let receipt = json!({"summary": big("summary", 50_000), "follow_ups": (0..100).map(|n| json!({"title": format!("f{n}"), "description": big("d", 1_000)})).collect::<Vec<_>>()});
    let goals: Vec<(Goal, bool, Vec<GoalTask>)> = (1..=10)
        .map(|id| (goal_of(id, 20_000), false, goal_tasks(500)))
        .collect();
    let handover = big_handover();
    let answer = asked(9, 20_000);
    let fitted = draft_planner_prompt(&DraftPlannerMaterial {
        db: Path::new("/q/queue.db"),
        key: &key,
        members: &members,
        source: Some(&source),
        receipt: Some(&receipt),
        goals: &goals,
        answer: Some(&answer),
        handover: Some(&handover),
        revisits: &[],
    })
    .unwrap();
    within(&fitted, DRAFT_PLANNER_PROMPT_LIMIT);
    sections_within(&fitted);
    let (text, bytes) = (&fitted.text, &fitted.bytes);
    for section in ["drafts", "origin", "goals", "answer"] {
        assert!(
            bytes.omitted.get(section).is_some_and(|n| *n > 0),
            "{section}: {bytes:?}"
        );
    }
    assert!(
        bytes.over_limit.as_deref().unwrap().starts_with("drafts: "),
        "{bytes:?}"
    );
    assert!(text.contains("## Drafts left out"));
    assert!(text.contains("`dagq show ID --full` for each"));
    assert!(text.contains(&format!(
        "read it whole with `dagq events --full --run {RUN} --kind integration_receipt`"
    )));
    assert!(text.contains("## Goals left out"));
    assert!(text.contains("read it whole with `dagq goal show 1 --full`"));
    assert!(text.contains("read ask 9 whole with `dagq asks --all`"));
    assert!(text.contains("Apply this answer as step 3 says."));
    assert!(text.contains("## What to do"));
}

#[test]
fn draft_planner_origin_counts_inner_and_whole_cuts_once() {
    let members = vec![(
        DraftTarget {
            task: big_task(10, 0),
            origin: DraftOrigin::FollowUp,
            material: json!({"source_run_id": RUN, "source_task_id": 3, "index": 0}),
            planners: 0,
            revisit: None,
        },
        1,
    )];
    let key = BundleKey::of(
        DraftOrigin::FollowUp,
        &members[0].0.material,
        members[0].0.task.id(),
    );
    // No cut, several inner cuts, and inner plus whole section cuts.
    for (source_bytes, receipt_bytes, whole_cut, any_cut) in [
        (0, 0, false, false),
        (5_000, 0, false, true),
        (50_000, 50_000, true, true),
    ] {
        let source = big_task(3, source_bytes);
        let receipt = json!({"summary": big("summary", receipt_bytes), "follow_ups": big("follow_ups", receipt_bytes)});
        let fitted = draft_planner_prompt(&DraftPlannerMaterial {
            db: Path::new("/q/queue.db"),
            key: &key,
            members: &members,
            source: Some(&source),
            receipt: Some(&receipt),
            goals: &[],
            answer: None,
            handover: None,
            revisits: &[],
        })
        .unwrap();
        assert_eq!(
            fitted.bytes.omitted.get("origin").copied().unwrap_or(0),
            usize::from(any_cut),
            "{source_bytes}, {receipt_bytes}: {:?}",
            fitted.bytes
        );
        let section = fitted
            .text
            .split("## Where it came from:")
            .nth(1)
            .unwrap()
            .split("## What to do")
            .next()
            .unwrap();
        assert_eq!(
            section.contains("for the drafts and their source task"),
            whole_cut,
            "{source_bytes}, {receipt_bytes}: {section}"
        );
        if any_cut {
            assert!(section.contains("bytes left out by the prompt's limit"));
            assert!(section.contains("dagq show"));
        }
    }
}

/// A draft of `title`, `description` and `context` bytes.
fn task_sized(id: i64, title: usize, description: usize, context: usize) -> Task {
    Task::restore(TaskRecord {
        goal_priority: None,
        id: TaskId::new(id),
        title: big("title", title),
        description: big("description", description),
        acceptance: String::new(),
        verification_commands: Vec::new(),
        required_evidence: Vec::new(),
        paths: Vec::new(),
        priority: Default::default(),
        change: None,
        status: TaskStatus::Draft,
        goal_id: Some(GoalId::new(1)),
        context: big("context", context),
        created_at: String::new(),
        updated_at: String::new(),
        worker: crate::domain::worker::Worker::CLAUDE_HEADLESS,
        named_mode: None,
        wait_for_build: false,
        execution_class: Default::default(),
    })
    .unwrap()
}

fn follow_up_draft(task: Task) -> (DraftTarget, usize) {
    let index = task.id().as_i64();
    (
        DraftTarget {
            task,
            origin: DraftOrigin::FollowUp,
            material: json!({"source_run_id": RUN, "source_task_id": 3, "index": index}),
            planners: 0,
            revisit: None,
        },
        1,
    )
}

fn draft_prompt_of(members: &[(DraftTarget, usize)], revisits: &[RevisitHistory]) -> FittedPrompt {
    let key = members[0].0.bundle_key();
    draft_planner_prompt(&DraftPlannerMaterial {
        db: Path::new("/q/queue.db"),
        key: &key,
        members,
        source: None,
        receipt: None,
        goals: &[],
        answer: None,
        handover: None,
        revisits,
    })
    .unwrap()
}

fn omitted(bytes: &PromptBytes, section: &str) -> usize {
    bytes.omitted.get(section).copied().unwrap_or(0)
}

/// The recovery job's `task` is one item: cut in none of its fields it
/// counts 0, cut in one or in all of them 1; a cut of a required field
/// is still said in `over_limit`.
#[test]
fn recovery_task_counts_once_whichever_fields_are_cut() {
    let bytes_of = |title: usize, description: usize, acceptance: usize, verify: usize| {
        let task = Task::restore(TaskRecord {
            goal_priority: None,
            id: TaskId::new(7),
            title: big("title", title),
            description: big("description", description),
            acceptance: big("acceptance", acceptance),
            verification_commands: vec![big("cargo test", verify)],
            required_evidence: Vec::new(),
            paths: Vec::new(),
            priority: Default::default(),
            change: None,
            status: TaskStatus::Draft,
            goal_id: None,
            context: String::new(),
            created_at: String::new(),
            updated_at: String::new(),
            worker: crate::domain::worker::Worker::CLAUDE_HEADLESS,
            named_mode: None,
            wait_for_build: false,
            execution_class: Default::default(),
        })
        .unwrap();
        recovery_prompt(
            &task,
            &run_on(Provider::Claude, WorkerMode::Headless),
            1,
            &RecoveryMaterial {
                alert: RecoveryAlert::IdleProcess,
                ended: None,
                facts: &json!({}),
                workspace: "ws",
                screen: "",
                processes: Ok(Vec::new()),
                git_status: "",
                head: SHA,
                receipt_commit: None,
                history: &[],
                allowed: &["wait"],
                binary: &no_binary(),
            },
        )
        .unwrap()
        .bytes
    };
    let (title, description, acceptance, verify) = (
        RECOVERY_TITLE_BYTES * 2,
        RECOVERY_DESCRIPTION_BYTES * 2,
        RECOVERY_ACCEPTANCE_BYTES * 2,
        RECOVERY_VERIFY_BYTES * 2,
    );
    // (bytes, how many times `task` counts, the required cuts said)
    for (bytes, counted, said) in [
        (bytes_of(10, 10, 10, 10), 0, 0),
        (bytes_of(title, 10, 10, 10), 1, 0),
        (bytes_of(10, description, 10, 10), 1, 1),
        (bytes_of(title, description, acceptance, verify), 1, 3),
    ] {
        assert_eq!(omitted(&bytes, "task"), counted, "{bytes:?}");
        assert_eq!(
            bytes
                .over_limit
                .as_deref()
                .unwrap_or_default()
                .matches("task: ")
                .count(),
            said,
            "{bytes:?}"
        );
    }
}

fn finding_view(summary: usize, subject: usize, why: usize, detail: usize) -> FindingView {
    FindingView {
        finding: crate::domain::Finding {
            id: crate::domain::FindingId::new(4),
            kind: "conflict".into(),
            target: "queue".into(),
            task_id: None,
            run_id: None,
            goal_id: None,
            subject: big("subject", subject),
            summary: big("summary", summary),
            detail: big("detail", detail),
            impact: crate::domain::Impact::Normal,
            first_seen_at: 0,
            last_seen_at: 0,
            occurrences: 1,
            evidence: Vec::new(),
            status: crate::domain::FindingStatus::Open,
            status_reason: None,
            proposal_id: None,
            covered_by_task: None,
            propose_reason: Some(big("why", why)),
            propose_requested_at: None,
            recorded_by: "observer".into(),
            updated_at: 0,
        },
        proposal_status: None,
        open_asks: Vec::new(),
        evidence_events: Some(Vec::new()),
    }
}

/// The finding planner's `finding` is one item: cut in none of its
/// summary, subject, why and detail it counts 0, cut in one or in all
/// of them 1.
#[test]
fn finding_planner_finding_counts_once_whichever_fields_are_cut() {
    let short = FINDING_SHORT_BYTES * 2;
    for ((summary, subject, why, detail), counted) in [
        ((10, 10, 10, 10), 0),
        ((short, 10, 10, 10), 1),
        ((10, 10, 10, FINDING_DETAIL_BYTES * 2), 1),
        ((short, short, short, FINDING_DETAIL_BYTES * 2), 1),
    ] {
        let view = finding_view(summary, subject, why, detail);
        let fitted = finding_planner_prompt(&FindingPlannerMaterial {
            db: Path::new("/q/queue.db"),
            finding: &view,
            attempt: 1,
            asks: &[],
            goal: None,
            goal_closed: false,
            siblings: &[],
            answer: None,
            handover: None,
        })
        .unwrap();
        assert_eq!(
            omitted(&fitted.bytes, "finding"),
            counted,
            "{:?}",
            fitted.bytes
        );
        // The cut of the required detail is still said.
        assert_eq!(
            fitted
                .bytes
                .over_limit
                .as_deref()
                .is_some_and(|said| said.starts_with("finding: ")),
            detail > FINDING_DETAIL_BYTES,
            "{:?}",
            fitted.bytes
        );
    }
}

/// A planner's `answer` is one item, the ask it carries: it counts 0
/// when neither its question nor its answer is cut, and 1 when one or
/// both are. Every planner that carries an answer this way takes it
/// from [`planner_answer`].
#[test]
fn planner_answer_counts_once_whichever_fields_are_cut() {
    let big_bytes = PLANNER_ANSWER_BYTES * 2;
    for ((question, answer), counted) in [
        ((10, 10), 0),
        ((big_bytes, 10), 1),
        ((10, big_bytes), 1),
        ((big_bytes, big_bytes), 1),
    ] {
        let mut ask = asked(1, 10);
        ask.question = big("question", question);
        ask.answer = Some(big("answer", answer));
        let mut fit = Fit::new(100_000);
        let (question, text) = planner_answer(&mut fit, &ask);
        assert_eq!(
            question.contains("read ask 1 whole") || text.contains("read ask 1 whole"),
            counted == 1
        );
        let bytes = fit.finish(format!("{question}{text}")).bytes;
        assert_eq!(omitted(&bytes, "answer"), counted, "{bytes:?}");
    }
}

/// A draft counts once in `drafts`: kept with its title, description
/// and context all cut, left out with them cut, or left out uncut.
#[test]
fn draft_planner_drafts_count_each_draft_once() {
    // Each `cut` draft has its three fields cut (about 11 KB); an
    // uncut one of about 9.5 KB does not fit after one of them.
    let cut = |id| task_sized(id, 5_000, 10_000, 10_000);
    let uncut = |id| task_sized(id, 10, 5_500, 3_800);
    for (members, left_out, counted) in [
        (vec![uncut(10)], vec![], 0),
        (vec![cut(10)], vec![], 1),
        (vec![cut(10), cut(11)], vec![11], 2),
        (vec![cut(10), uncut(11)], vec![11], 2),
        (vec![uncut(10), uncut(11), cut(12)], vec![12], 1),
    ] {
        let members: Vec<_> = members.into_iter().map(follow_up_draft).collect();
        let fitted = draft_prompt_of(&members, &[]);
        let shown: Vec<i64> = members
            .iter()
            .map(|(target, _)| target.task.id().as_i64())
            .filter(|id| fitted.text.contains(&format!("\nTask {id}: ")))
            .collect();
        let all: Vec<i64> = members.iter().map(|(t, _)| t.task.id().as_i64()).collect();
        assert_eq!(
            all.iter()
                .filter(|id| !shown.contains(id))
                .copied()
                .collect::<Vec<_>>(),
            left_out,
            "{:?}",
            fitted.bytes
        );
        assert_eq!(
            omitted(&fitted.bytes, "drafts"),
            counted,
            "{:?}",
            fitted.bytes
        );
    }
}

/// An ask counts once in `asks`: kept with its question and answer
/// cut, left out with them cut, or left out uncut.
#[test]
fn planner_asks_count_each_ask_once() {
    // A cut ask takes about 2 KB, an uncut one of 900 bytes each
    // about 1.9 KB; the newest are taken first within 8 KB, and one
    // that does not fit is skipped for an older smaller one.
    for (asks, left_out, counted) in [
        (vec![asked(1, 900)], 0, 0),
        (vec![asked(1, 5_000)], 0, 1),
        (
            vec![
                asked(1, 5_000),
                asked(2, 5_000),
                asked(3, 5_000),
                asked(4, 5_000),
                asked(5, 5_000),
            ],
            2,
            5,
        ),
        (
            vec![
                asked(1, 900),
                asked(2, 900),
                asked(3, 900),
                asked(4, 900),
                asked(5, 900),
            ],
            1,
            1,
        ),
        (
            vec![
                asked(1, 900),
                asked(2, 5_000),
                asked(3, 5_000),
                asked(4, 5_000),
                asked(5, 5_000),
            ],
            1,
            4,
        ),
    ] {
        let mut fit = Fit::new(100_000);
        let text = planner_asks(&mut fit, &asks, false);
        let shown = asks
            .iter()
            .filter(|ask| text.contains(&format!("- ask {}: ", ask.id)))
            .count();
        let bytes = fit.finish(text).bytes;
        assert_eq!(asks.len() - shown, left_out, "{bytes:?}");
        assert_eq!(omitted(&bytes, "asks"), counted, "{bytes:?}");
    }
}

/// A goal counts once in `goals`, its task lines with it: kept with
/// its fields cut or task lines left out, left out cut, or left out
/// uncut.
#[test]
fn planner_goals_count_each_goal_once_with_its_task_lines() {
    let section = |goal: Goal, tasks: Vec<GoalTask>| {
        (
            goal.id(),
            planner_goal(
                &format!("\n## Goal {}", goal.id()),
                &goal,
                (&tasks, "Its tasks"),
                true,
            ),
        )
    };
    // A cut goal takes about 10 KB, its 50 task lines 4 KB more, an
    // uncut one about 5.5 KB; the goals take 16 KB.
    for (goals, left_out, counted) in [
        (vec![section(goal_of(1, 100), goal_tasks(2))], 0, 0),
        (vec![section(goal_of(1, 5_000), Vec::new())], 0, 1),
        (vec![section(goal_of(1, 100), goal_tasks(50))], 0, 1),
        (
            vec![
                section(goal_of(1, 5_000), goal_tasks(50)),
                section(goal_of(2, 5_000), goal_tasks(50)),
            ],
            1,
            2,
        ),
        (
            vec![
                section(goal_of(1, 5_000), goal_tasks(50)),
                section(goal_of(2, 1_800), Vec::new()),
            ],
            1,
            2,
        ),
        (
            vec![
                section(goal_of(1, 1_800), Vec::new()),
                section(goal_of(2, 1_800), Vec::new()),
                section(goal_of(3, 1_800), Vec::new()),
            ],
            1,
            1,
        ),
    ] {
        let n = goals.len();
        let mut fit = Fit::new(100_000);
        let text = planner_goals(&mut fit, goals);
        let shown = (1..=n)
            .filter(|id| text.contains(&format!("\n## Goal {id}\n")))
            .count();
        let bytes = fit.finish(text).bytes;
        assert_eq!(n - shown, left_out, "{bytes:?}");
        assert_eq!(omitted(&bytes, "goals"), counted, "{bytes:?}");
    }
}

/// A reason counts once in `reasons`: kept cut, left out cut, or left
/// out uncut.
#[test]
fn runtime_planner_reasons_count_each_reason_once() {
    // A cut reason takes about 4 KB, an uncut one about 3 KB; the
    // reasons take 12 KB in their order.
    let (cut, uncut) = (big("reason", 10_000), big("reason", 3_000));
    for (reasons, left_out, counted) in [
        (vec![uncut.clone()], 0, 0),
        (vec![cut.clone()], 0, 1),
        (
            vec![cut.clone(), uncut.clone(), uncut.clone(), uncut.clone()],
            1,
            2,
        ),
        (
            vec![
                cut.clone(),
                uncut.clone(),
                uncut.clone(),
                cut.clone(),
                uncut.clone(),
            ],
            2,
            3,
        ),
    ] {
        let fitted = runtime_planner_prompt(
            Path::new("/q/queue.db"),
            ProposalId::new(3),
            &[],
            &reasons,
            Some(TaskId::new(42)),
            Carried::default(),
        )
        .unwrap();
        assert_eq!(
            fitted
                .text
                .contains(&format!("({left_out} more reasons left out")),
            left_out > 0,
            "{:?}",
            fitted.bytes
        );
        assert_eq!(
            omitted(&fitted.bytes, "reasons"),
            counted,
            "{:?}",
            fitted.bytes
        );
    }
}

/// A reference counts once in `refs`: kept cut, left out cut, or left
/// out uncut.
#[test]
fn request_planner_refs_count_each_reference_once() {
    let request = crate::domain::plan_request::PlanRequest {
        id: crate::domain::RequestId::new(6),
        text: "plan it".into(),
        note: None,
        refs: Vec::new(),
        priority: None,
        requested_by: "inbox".into(),
        requested_by_id: "inbox".into(),
        status: crate::domain::plan_request::RequestStatus::Open,
        status_reason: None,
        proposals: Vec::new(),
        planners: 0,
        spike_task: None,
        replan_operation: None,
        created_at: 0,
        updated_at: 0,
    };
    // A cut ask takes about 8 KB, an uncut one of 3,000 bytes each
    // about 6 KB; the references take 32 KB in their order.
    let refer = |id, bytes| RequestRefMaterial::Ask(asked(id, bytes));
    for (refs, left_out, counted) in [
        (vec![refer(1, 3_000)], 0, 0),
        (vec![refer(1, 20_000)], 0, 1),
        (
            vec![
                refer(1, 20_000),
                refer(2, 20_000),
                refer(3, 20_000),
                refer(4, 20_000),
                refer(5, 20_000),
            ],
            2,
            5,
        ),
        (
            vec![
                refer(1, 20_000),
                refer(2, 3_000),
                refer(3, 3_000),
                refer(4, 3_000),
                refer(5, 3_000),
            ],
            1,
            2,
        ),
    ] {
        let fitted = request_planner_prompt(&RequestPlannerMaterial {
            db: Path::new("/q/queue.db"),
            request: &request,
            handed: "The person's words are in /q/planners/1/request.md.",
            attempt: 1,
            refs: &refs,
            goals: &[],
            asks: &[],
            answer: None,
            handover: None,
            other_spikes: &[],
            replan: None,
        })
        .unwrap();
        let shown = (1..=refs.len())
            .filter(|id| fitted.text.contains(&format!("### Ask {id} ")))
            .count();
        assert_eq!(refs.len() - shown, left_out, "{:?}", fitted.bytes);
        assert_eq!(
            omitted(&fitted.bytes, "refs"),
            counted,
            "{:?}",
            fitted.bytes
        );
    }
}

/// The revisit section is one composite item: it counts once in
/// `revisit` when a question or note in it, the section as a whole,
/// or both were cut.
#[test]
fn draft_planner_revisit_counts_inner_and_whole_cuts_once() {
    let mut members = vec![follow_up_draft(task_sized(10, 10, 100, 100))];
    members[0].0.revisit = Some(crate::domain::DraftRevisit {
        task_id: TaskId::new(10),
        revisit_at: 0,
        revisit_at_utc: "1970-01-01T00:00:00.000Z".into(),
        note: Some("look again".into()),
        set_by: "inbox".into(),
        set_by_id: "inbox:1".into(),
        created_at: 0,
        opened_at: None,
        planner_id: None,
    });
    // No cut, inner cuts only, the whole section only (ten asks of
    // about 1 KB each), and inner plus whole section cuts.
    for (asks, notes, whole_cut, counted) in [
        (1, 100, false, 0),
        (1, 5_000, false, 1),
        (10, 100, true, 1),
        (10, 5_000, true, 1),
    ] {
        let history = [RevisitHistory {
            task: TaskId::new(10),
            asks: (1..=asks).map(|id| asked(id, 500)).collect(),
            notes: vec![big("noted", notes), big("noted", notes)],
        }];
        let fitted = draft_prompt_of(&members, &history);
        assert_eq!(
            fitted.text.contains("`dagq show ID --full` for each draft"),
            whole_cut,
            "{:?}",
            fitted.bytes
        );
        assert_eq!(
            omitted(&fitted.bytes, "revisit"),
            counted,
            "{:?}",
            fitted.bytes
        );
    }
}

/// ADR-t1540-1: with every other section at its limit, a revisited
/// draft's long history (questions and notes far past their limits)
/// is cut to [`DRAFT_REVISIT_BYTES`], newest first, and the whole stays
/// within [`DRAFT_PLANNER_PROMPT_LIMIT`].
#[test]
fn a_long_revisit_history_stays_within_the_draft_planner_s_limits() {
    let mut members: Vec<(DraftTarget, usize)> = (10..60)
        .map(|id| {
            (
                DraftTarget {
                    task: big_task(id, 20_000),
                    origin: DraftOrigin::FollowUp,
                    material: json!({"source_run_id": RUN, "source_task_id": 3, "index": id}),
                    planners: 0,
                    revisit: None,
                },
                1,
            )
        })
        .collect();
    let lead = members[0].0.task.id();
    members[0].0.revisit = Some(crate::domain::DraftRevisit {
        task_id: lead,
        revisit_at: 0,
        revisit_at_utc: "1970-01-01T00:00:00.000Z".into(),
        note: Some(big("note", 20_000)),
        set_by: "inbox".into(),
        set_by_id: "inbox:1".into(),
        created_at: 0,
        opened_at: None,
        planner_id: None,
    });
    let history = [RevisitHistory {
        task: lead,
        asks: (1..=10).map(|id| asked(id, 20_000)).collect(),
        notes: (0..10).map(|_| big("noted", 20_000)).collect(),
    }];
    let key = members[0].0.bundle_key();
    let source = big_task(3, 50_000);
    let receipt = json!({"summary": big("summary", 50_000), "follow_ups": (0..100).map(|n| json!({"title": format!("f{n}"), "description": big("d", 1_000)})).collect::<Vec<_>>()});
    let goals: Vec<(Goal, bool, Vec<GoalTask>)> = (1..=10)
        .map(|id| (goal_of(id, 20_000), false, goal_tasks(500)))
        .collect();
    let answer = asked(99, 20_000);
    let fitted = draft_planner_prompt(&DraftPlannerMaterial {
        db: Path::new("/q/queue.db"),
        key: &key,
        members: &members,
        source: Some(&source),
        receipt: Some(&receipt),
        goals: &goals,
        answer: Some(&answer),
        handover: None,
        revisits: &history,
    })
    .unwrap();
    within(&fitted, DRAFT_PLANNER_PROMPT_LIMIT);
    let bytes = &fitted.bytes;
    assert!(
        bytes.sections["revisit"] <= DRAFT_REVISIT_BYTES,
        "{bytes:?}"
    );
    assert!(
        bytes.omitted.get("revisit").is_some_and(|n| *n > 0),
        "{bytes:?}"
    );
    assert!(fitted.text.contains("## Revisit of draft 10"));
    assert!(fitted.text.contains("`dagq show 10 --full`"));
}

/// The planner opened for a finding whose evidence is huge (finding 44
/// of production had 270,669 bytes of it) stays within
/// [`FINDING_PLANNER_PROMPT_LIMIT`], keeping the newest evidence.
#[test]
fn a_finding_planner_prompt_of_huge_evidence_stays_within_its_limits() {
    let view = FindingView {
        finding: crate::domain::Finding {
            id: crate::domain::FindingId::new(4),
            kind: "conflict".into(),
            target: "queue".into(),
            task_id: None,
            run_id: None,
            goal_id: None,
            subject: big("subject", 5_000),
            summary: big("summary", 5_000),
            detail: big("detail", 50_000),
            impact: crate::domain::Impact::Normal,
            first_seen_at: 0,
            last_seen_at: 0,
            occurrences: 500,
            evidence: Vec::new(),
            status: crate::domain::FindingStatus::Open,
            status_reason: None,
            proposal_id: None,
            covered_by_task: None,
            propose_reason: Some(big("why", 5_000)),
            propose_requested_at: None,
            recorded_by: "observer".into(),
            updated_at: 0,
        },
        proposal_status: None,
        open_asks: Vec::new(),
        evidence_events: Some((1..=500).map(|id| event_of(id, 5_000)).collect()),
    };
    let asks: Vec<Ask> = (1..=100).map(|id| asked(id, 2_000)).collect();
    let goal = goal_of(5, 20_000);
    let siblings = goal_tasks(500);
    let handover = big_handover();
    let answer = asked(200, 20_000);
    let fitted = finding_planner_prompt(&FindingPlannerMaterial {
        db: Path::new("/q/queue.db"),
        finding: &view,
        attempt: 1,
        asks: &asks,
        goal: Some(&goal),
        goal_closed: false,
        siblings: &siblings,
        answer: Some(&answer),
        handover: Some(&handover),
    })
    .unwrap();
    within(&fitted, FINDING_PLANNER_PROMPT_LIMIT);
    sections_within(&fitted);
    let (text, bytes) = (&fitted.text, &fitted.bytes);
    for section in ["finding", "evidence", "asks", "goals", "answer"] {
        assert!(
            bytes.omitted.get(section).is_some_and(|n| *n > 0),
            "{section}: {bytes:?}"
        );
    }
    assert!(
        bytes
            .over_limit
            .as_deref()
            .unwrap()
            .starts_with("finding: "),
        "{bytes:?}"
    );
    assert!(text.contains("evidence events (the oldest) left out by this section's limit"));
    assert!(text.contains("`dagq findings 4 --full`, or one event with `dagq events --full --all --after <its ID - 1> --limit 1`"));
    // The newest evidence is kept, the oldest left out.
    assert!(text.contains(r#""id":500,"#));
    assert!(!text.contains(r#""id":1,"kind":"turn_finished""#));
    assert!(text.contains("asks (the oldest) left out by this section's limit"));
    assert!(text.contains("To read them: `dagq asks --all`."));
    assert!(text.contains("To read them: `dagq goal show 5 --full`."));
    assert!(text.contains("## What to do"));
}

/// The planner opened for a planning request that refers to many huge
/// tasks, runs, events, asks and findings, with a huge note, goals and
/// asks, stays within [`REQUEST_PLANNER_PROMPT_LIMIT`]. Production had
/// no request planner yet: this input measures what the sections'
/// limits add up to.
#[test]
fn a_request_planner_prompt_of_huge_references_stays_within_its_limits() {
    let request = crate::domain::plan_request::PlanRequest {
        id: crate::domain::RequestId::new(6),
        text: "plan it".into(),
        note: Some(big("note", 50_000)),
        refs: Vec::new(),
        priority: None,
        requested_by: "inbox".into(),
        requested_by_id: "inbox".into(),
        status: crate::domain::plan_request::RequestStatus::Open,
        status_reason: None,
        proposals: Vec::new(),
        planners: 0,
        spike_task: None,
        replan_operation: None,
        created_at: 0,
        updated_at: 0,
    };
    let receipt = json!({"summary": big("summary", 20_000), "follow_ups": []});
    let mut refs = vec![RequestRefMaterial::Unreadable {
        reference: crate::domain::plan_request::RequestRef::Task(TaskId::new(999)),
        error: big("unreadable", 20_000),
    }];
    for n in 1..=10 {
        refs.push(RequestRefMaterial::Task {
            task: Box::new(big_task(n, 20_000)),
            receipt: Some(receipt.clone()),
        });
        refs.push(RequestRefMaterial::Ask(asked(n, 20_000)));
        refs.push(RequestRefMaterial::Event(event_of(100 + n, 20_000)));
        refs.push(RequestRefMaterial::Run {
            run: RunId::new(RUN).unwrap(),
            task: Some(TaskId::new(n)),
            receipt: Some(receipt.clone()),
        });
    }
    let goals: Vec<(Goal, bool, Vec<GoalTask>)> = (1..=10)
        .map(|id| (goal_of(id, 20_000), false, goal_tasks(500)))
        .collect();
    let asks: Vec<Ask> = (1..=100).map(|id| asked(id, 2_000)).collect();
    let handover = big_handover();
    let answer = asked(300, 20_000);
    let brief = huge_replan_brief();
    let fitted = request_planner_prompt(&RequestPlannerMaterial {
        db: Path::new("/q/queue.db"),
        request: &request,
        handed: "The person's words are in /q/planners/1/request.md.",
        attempt: 1,
        refs: &refs,
        goals: &goals,
        asks: &asks,
        answer: Some(&answer),
        handover: Some(&handover),
        other_spikes: &[],
        replan: Some((&brief, Some(Path::new("/q/replans/snap-1")))),
    })
    .unwrap();
    within(&fitted, REQUEST_PLANNER_PROMPT_LIMIT);
    sections_within(&fitted);
    let (text, bytes) = (&fitted.text, &fitted.bytes);
    for section in ["note", "refs", "goals", "asks", "answer", "replan"] {
        assert!(
            bytes.omitted.get(section).is_some_and(|n| *n > 0),
            "{section}: {bytes:?}"
        );
    }
    assert!(text.contains("it could not be read, so there is no way to read it"));
    assert!(!text.contains("read it whole with nothing"));
    assert!(!text.contains("read it whole with it could not be read"));
    assert!(text.contains("read it whole with `dagq requests 6`"));
    assert!(text.contains("references left out by this section's limit"));
    assert!(text.contains("read it whole with `dagq show 1 --full` and `dagq events --full --task 1 --kind integration_receipt`"));
    assert!(text.contains("`dagq events --full --all --after 100 --limit 1`"));
    assert!(text.contains(&format!(
        "`dagq events --full --run {RUN} --kind integration_receipt`"
    )));
    assert!(text.contains("## Goals left out"));
    assert!(text.contains("Apply this answer as step 3 says."));
    // What the largest input takes, for the limit's reason in
    // docs/design/supervisor-lifecycle/prompt.md.
    assert!(bytes.total > 60_000, "{bytes:?}");
}

/// A replanning operation's brief whose every part is huge: its
/// acceptance, its reviews' reasons, its successors and its plan.
fn huge_replan_brief() -> crate::domain::replan_plan::ReplanBrief {
    use crate::domain::replan_plan::{ReplanBrief, ReviewFinding, SuccessorBrief};
    use crate::domain::task_replan::{
        Diagnosis, OriginalConditions, ReplanOperation, ReplanOrigin, ReplanStatus,
        acceptance_items,
    };
    let conditions = OriginalConditions {
        title: "big".into(),
        description: big("description", 20_000),
        acceptance: acceptance_items(
            &(1..=200)
                .map(|n| format!("item {n} {}", big("item", 200)))
                .collect::<Vec<_>>()
                .join("\n"),
        ),
        paths: vec!["src/**".into()],
        verify: vec!["cargo test".into()],
        evidence: json!([]),
        predecessors: Vec::new(),
        successors: Vec::new(),
        goal: None,
    };
    ReplanBrief {
        operation: ReplanOperation {
            id: crate::domain::ReplanId::new(1),
            task_id: TaskId::new(1),
            run_id: RunId::new(RUN).unwrap(),
            goal_id: None,
            origin: ReplanOrigin::Manual,
            status: ReplanStatus::Planning,
            status_reason: None,
            generation: 2,
            return_phase: None,
            reason: big("reason", 10_000),
            task_version: conditions.version(),
            conditions,
            goal_acceptance_version: None,
            diagnosis: Diagnosis::default(),
            snapshot_id: Some("snap-1".into()),
            request_id: None,
            request_status: None,
            proposal_id: None,
            requested_by: "inbox".into(),
            requested_by_id: "inbox".into(),
            mappings: Vec::new(),
            replaced_by: Vec::new(),
            created_at: 0,
            updated_at: 0,
            closed_at: None,
        },
        turns: vec![("revise".into(), 40), ("resume".into(), 3)],
        reviews: (0..50)
            .map(|_| ReviewFinding {
                verdict: "revise".into(),
                reasons: vec![big("reason", 2_000)],
            })
            .collect(),
        successors: (2..100)
            .map(|n| SuccessorBrief {
                task_id: TaskId::new(n),
                status: TaskStatus::Ready,
                title: big("title", 300),
            })
            .collect(),
        plan: None,
        problems: Vec::new(),
    }
}

/// A task of the worker's prompt with every field of its own `size`
/// bytes (in 3-byte characters), its goal and its context included.
fn worker_task(id: i64, size: usize, goal: Option<i64>) -> Task {
    let text = |name: &str| format!("{name}{}", "あ".repeat(size / 3));
    Task::restore(TaskRecord {
        goal_priority: None,
        id: TaskId::new(id),
        title: text("title"),
        description: text("description"),
        acceptance: text("acceptance"),
        verification_commands: vec![text("verify")],
        required_evidence: vec![EvidenceCheck::Tests],
        paths: (0..size / 20).map(|i| format!("src/p{i}/**")).collect(),
        priority: Default::default(),
        change: None,
        status: TaskStatus::InProgress,
        goal_id: goal.map(GoalId::new),
        context: text("context"),
        created_at: String::new(),
        updated_at: String::new(),
        worker: crate::domain::worker::Worker::CLAUDE_HEADLESS,
        named_mode: None,
        wait_for_build: false,
        execution_class: Default::default(),
    })
    .unwrap()
}

fn worker_goal(id: i64, size: usize) -> Goal {
    let text = |name: &str| format!("{name}{}", "い".repeat(size / 3));
    Goal::restore(GoalRecord {
        priority: Default::default(),
        tags: Vec::new(),
        id: GoalId::new(id),
        title: text("goal title"),
        description: text("goal description"),
        acceptance: text("goal acceptance"),
        constraints: text("goal constraints"),
        doc: Some("docs/plans/goal.md".into()),
        status: GoalStatus::Open,
        closed_at: None,
        verdict: None,
        created_at: String::new(),
        updated_at: String::new(),
    })
    .unwrap()
}

fn worker_predecessor(id: i64, size: usize) -> PredecessorSummary {
    PredecessorSummary {
        task_id: TaskId::new(id),
        title: format!("predecessor {id} {}", "う".repeat(size / 3_000)),
        result_commit: SHA.into(),
        summary: format!("summary {id} {}", "え".repeat(size / 3)),
    }
}

/// Material of the worker's prompt: `count` items in each list, each
/// text of about `size` bytes.
#[allow(clippy::type_complexity)]
fn worker_material(
    count: i64,
    size: usize,
) -> (
    Task,
    Goal,
    Vec<PredecessorSummary>,
    Vec<GoalPredecessorSummary>,
    Vec<Task>,
    Inheritance,
) {
    // The first summary is long; the rest pack the section.
    let predecessors = (1..=count)
        .map(|id| worker_predecessor(id, if id == 1 { size } else { size.min(5_000) }))
        .collect();
    let goals = (1..=count)
        .map(|g| GoalPredecessorSummary {
            goal_id: GoalId::new(100 + g),
            title: format!("goal {g} {}", "お".repeat(size / 3_000)),
            tasks: (1..=count)
                .map(|t| worker_predecessor(1_000 * g + t, size.min(600)))
                .collect(),
        })
        .collect();
    let siblings = (1..=count)
        .map(|id| worker_task(10_000 + id, size.min(300), Some(1)))
        .collect();
    let inherited = Inheritance {
        run_id: RunId::new(RUN).unwrap(),
        base: CommitSha::try_from(SHA).unwrap(),
        head: SHA.into(),
        branch: Some("dagq/earlier".into()),
        receipt_path: Some("/runs/earlier/receipt.json".into()),
        summary: format!("inherited {}", "か".repeat(size / 3)),
        by_hand: Some(("inbox".into(), format!("reason {}", "き".repeat(size / 3)))),
        replan: None,
    };
    (
        worker_task(9, size, Some(1)),
        worker_goal(1, size),
        predecessors,
        goals,
        siblings,
        inherited,
    )
}

/// The worker's prompt holds each section to its limit and the whole
/// to [`WORKER_PROMPT_LIMIT`] with the largest input (ADR-t2072-1):
/// what a list left out is counted and named with how to read it with
/// git or in the worktree, never with a `dagq` command, and the task's
/// own title, description, acceptance and verification are cut only
/// past their own limits, said in `over_limit`.
#[test]
fn the_worker_prompt_stays_within_its_limits_with_the_largest_input() {
    let (task, goal, predecessors, goals, siblings, inherited) = worker_material(300, 400_000);
    let own_run = run(9, RunStatus::Claimed, None);
    let fitted = prompt(
        &task,
        &own_run,
        Some(&goal),
        &predecessors,
        &goals,
        &siblings,
        Some(&inherited),
        &[],
        None,
    )
    .unwrap();
    let (text, bytes) = (&fitted.text, &fitted.bytes);
    assert!(
        text.len() <= WORKER_PROMPT_LIMIT - prompt_fit::LANGUAGE_ROOM,
        "{}",
        text.len()
    );
    assert_eq!(bytes.total, text.len());
    assert_eq!(bytes.limit, WORKER_PROMPT_LIMIT);
    assert_eq!(bytes.sections.values().sum::<usize>(), text.len());
    let over = bytes.over_limit.as_deref().unwrap();
    assert!(!over.contains("past its limit"), "{over}");
    for (what, max) in [
        ("title", WORKER_TITLE_BYTES),
        ("description", WORKER_DESCRIPTION_BYTES),
        ("acceptance", WORKER_ACCEPTANCE_BYTES),
        ("verify", WORKER_VERIFY_BYTES),
    ] {
        assert!(text.contains(&format!("{what}ああ")), "{what}");
        assert!(over.contains(&format!("by its limit of {max}")), "{over}");
    }
    assert!(over.contains("paths: "), "{over}");
    assert!(text.contains("Task title: titleあ"));
    assert!(text.contains("Paths you may change"));
    for section in [
        "goal",
        "context",
        "predecessors",
        "goal_predecessors",
        "siblings",
        "inherited",
    ] {
        assert!(bytes.omitted[section] > 0, "{section}: {bytes:?}");
    }
    for (what, read) in [
        ("predecessor tasks", "`git log --grep '^Dagq-Task: <id>$'`"),
        (
            "goals depended on",
            "`git log` in your worktree shows each landing commit",
        ),
        (
            "completed tasks of the goals",
            "`git log --grep '^Dagq-Task: <id>$'`",
        ),
        ("sibling tasks", NOT_READABLE),
    ] {
        let note = text
            .lines()
            .find(|line| line.contains(&format!(" {what} left out by")))
            .unwrap_or_else(|| panic!("{what}"));
        assert!(note.contains(read), "{note}");
    }
    assert!(text.contains(&format!(
        "`git show --no-patch {SHA}` in your worktree prints the whole summary"
    )));
    assert!(text.contains("the goal doc docs/plans/goal.md in your worktree"));
    assert!(text.contains(&format!(
        "{NOT_READABLE}; `git log {SHA}..{SHA}` in your worktree shows its work, not this summary"
    )));
    for (at, _) in text.match_indices("left out by") {
        let note = text[at..].split([']', ')']).next().unwrap();
        assert!(!note.contains("dagq "), "{note}");
    }
    // The fixed instructions stay whole after the sections.
    assert!(text.contains("Write a completion receipt to /runs/run/receipt.json"));
    assert!(text.ends_with(&headless_provider_line(Provider::Claude)));
    // What the largest input takes, for the limit's reason.
    assert!(bytes.total > 80_000, "{bytes:?}");
}

/// The direct predecessors are taken before the goals' tasks, and of
/// those the newest (the highest ID) first; each list keeps its own
/// order.
#[test]
fn the_worker_prompt_takes_direct_predecessors_first_and_goal_tasks_newest_first() {
    let (task, _, predecessors, goals, _, _) = worker_material(30, 2_400);
    let own_run = run(9, RunStatus::Claimed, None);
    let text = prompt(
        &task,
        &own_run,
        None,
        &predecessors,
        &goals[..1],
        &[],
        None,
        &[],
        None,
    )
    .unwrap()
    .text;
    let ids: Vec<i64> = text
        .lines()
        .filter_map(|line| line.trim_start().strip_prefix("- task "))
        .map(|rest| rest.split(':').next().unwrap().parse().unwrap())
        .collect();
    let direct: Vec<i64> = ids.iter().copied().filter(|id| *id < 1_000).collect();
    let of_goal: Vec<i64> = ids.iter().copied().filter(|id| *id > 1_000).collect();
    assert_eq!(direct, (1..=direct.len() as i64).collect::<Vec<_>>());
    assert!(direct.len() < 30, "{direct:?}");
    assert_eq!(*of_goal.last().unwrap(), 1_030, "{of_goal:?}");
    assert!(of_goal.windows(2).all(|w| w[0] < w[1]), "{of_goal:?}");
    assert!(of_goal.len() < 30 && of_goal[0] > 1_001, "{of_goal:?}");
}

/// Within its limits the worker's prompt is what it was without them:
/// every section whole and in its old form, nothing counted as left out.
#[test]
fn within_its_limits_the_worker_prompt_is_unchanged() {
    let (task, goal, mut predecessors, goals, siblings, inherited) = worker_material(2, 600);
    // A title longer than a list's title elsewhere stays whole.
    predecessors[0].title = "長".repeat(300);
    let own_run = run(9, RunStatus::Claimed, None);
    let fitted = prompt(
        &task,
        &own_run,
        Some(&goal),
        &predecessors,
        &goals,
        &siblings,
        Some(&inherited),
        &[],
        None,
    )
    .unwrap();
    let text = &fitted.text;
    assert!(fitted.bytes.omitted.is_empty(), "{:?}", fitted.bytes);
    assert_eq!(fitted.bytes.over_limit, None);
    assert!(text.contains(&format!(
        "Task title: {}\nDescription:\n{}\nAcceptance criteria:\n{}\n",
        task.title(),
        task.description(),
        task.acceptance()
    )));
    assert!(text.contains(&format!(
            "Goal ID: 1\nGoal title: {}\nGoal description:\n{}\nGoal acceptance:\n{}\nGoal constraints:\n{}\nGoal doc: docs/plans/goal.md (a path in the repository; read it for the full picture)\n",
            goal.title(),
            goal.description(),
            goal.acceptance(),
            goal.constraints()
        )));
    assert!(text.contains(&format!(
        "Context (why this task exists and what to read first):\n{}\n",
        task.context()
    )));
    let mut expected =
        "Predecessor tasks (their changes are already in your base commit):\n".to_owned();
    for p in &predecessors {
        expected.push_str(&format!(
            "- task {}: {}; result commit {}; summary: {}\n",
            p.task_id, p.title, p.result_commit, p.summary
        ));
    }
    for g in &goals {
        expected.push_str(&format!(
            "- goal {} (closed as achieved): {}; its completed tasks:\n",
            g.goal_id, g.title
        ));
        for t in &g.tasks {
            expected.push_str(&format!(
                "  - task {}: {}; result commit {}; summary: {}\n",
                t.task_id, t.title, t.result_commit, t.summary
            ));
        }
    }
    expected.push_str(
        "Sibling tasks in progress (other tasks executing now, each owning its own scope):\n",
    );
    for s in &siblings {
        expected.push_str(&format!("- task {}: {}\n", s.id(), s.title()));
    }
    expected.push_str(&inherited.section());
    assert!(text.contains(&expected), "{text}");
    assert!(text.contains(&format!(
            "Paths you may change (globs from the repository root; `*` stays in one directory, `**` spans any depth): {}.",
            task.paths().join(", ")
        )));
    assert!(!text.contains("left out by"), "{text}");
}

/// Task 7 with `paths` and `verification_commands`.
fn task_with_lists(paths: Vec<String>, verification_commands: Vec<String>) -> Task {
    Task::restore(TaskRecord {
        goal_priority: None,
        id: TaskId::new(7),
        title: "work".into(),
        description: String::new(),
        acceptance: String::new(),
        verification_commands,
        required_evidence: Vec::new(),
        paths,
        priority: Default::default(),
        change: None,
        status: TaskStatus::InProgress,
        goal_id: None,
        context: String::new(),
        created_at: String::new(),
        updated_at: String::new(),
        worker: crate::domain::worker::Worker::CLAUDE_HEADLESS,
        named_mode: None,
        wait_for_build: false,
        execution_class: Default::default(),
    })
    .unwrap()
}

const RESUME_KINDS: [ResumeKind; 11] = [
    ResumeKind::Landing,
    ResumeKind::EvidenceMissing,
    ResumeKind::SentBack,
    ResumeKind::ScopeViolation,
    ResumeKind::Precheck,
    ResumeKind::Triage,
    ResumeKind::Recheck,
    ResumeKind::SessionGone,
    ResumeKind::E2e,
    ResumeKind::Replan,
    ResumeKind::Hold,
];

/// The bytes a next-turn message held to `limit` may take before the
/// language's instruction is added.
fn room(limit: usize) -> usize {
    limit - prompt_fit::LANGUAGE_ROOM
}

/// A run resumed after its task's hold tells its session that a person
/// held the task and released it, that its worktree is as it was, and
/// carries the answers its session had not taken when it was parked.
#[test]
fn a_resume_after_a_hold_goes_on_where_the_run_stopped_with_its_answers() {
    let task = task_with_lists(vec!["src/".into()], vec!["cargo test".into()]);
    let run = run_on(Provider::Claude, WorkerMode::Headless);
    let request = ResumeRequest {
        main: CommitSha::try_from("2".repeat(40).as_str()).unwrap(),
        branch: "main".into(),
        reason: "task 1 was held: the login expired".into(),
        kind: ResumeKind::Hold,
        reason_file: None,
        known_failures: None,
        carried_answers: vec!["answer to ask 4: use the staging host".into()],
        answers_file: None,
    };
    let text = resume_request(&task, &run, &request, &[]).unwrap().text;
    for part in [
        "while a person held its task",
        "the hold is released",
        "answer to ask 4: use the staging host",
        "where you left off",
        "the login expired",
    ] {
        assert!(text.contains(part), "{part} in {text}");
    }
}

/// The answers a continuation after a replanning's park carries take at
/// most their own budget of the resolution request: six answers of about
/// 8 KB each are cut to it, the request names the file the supervisor
/// writes them whole to as where the session reads every one, and it stays
/// within its limit; answers within the budget are carried whole.
#[test]
fn carried_answers_past_their_budget_are_cut_and_named_whole_in_their_file() {
    let task = task_with_lists(vec!["src/".into()], vec!["cargo test".into()]);
    let run = run_on(Provider::Claude, WorkerMode::Headless);
    let file = "/runs/run/resume-1-answers.txt";
    let request = |answers: Vec<String>| ResumeRequest {
        main: CommitSha::try_from("2".repeat(40).as_str()).unwrap(),
        branch: "main".into(),
        reason: "parked for replan operation 1".into(),
        kind: ResumeKind::Replan,
        reason_file: None,
        known_failures: None,
        carried_answers: answers,
        answers_file: Some(file.into()),
    };
    let large: Vec<String> = (1..=6)
        .map(|n| format!("answer to ask {n}: {}", "答".repeat(2_700)))
        .collect();
    let fitted = resume_request(&task, &run, &request(large), &[]).unwrap();
    assert!(
        fitted.text.len() <= room(RESUME_REQUEST_LIMIT),
        "{}",
        fitted.text.len()
    );
    assert!(
        fitted.bytes.omitted.contains_key("answers"),
        "{:?}",
        fitted.bytes
    );
    assert!(
        fitted
            .text
            .contains(&format!("every answer is whole in {file}")),
        "{}",
        fitted.text
    );
    assert!(fitted.text.contains("answer to ask 1: "));
    let small = vec!["answer to ask 1: use notes.txt".to_owned()];
    let fitted = resume_request(&task, &run, &request(small), &[]).unwrap();
    assert!(
        !fitted.bytes.omitted.contains_key("answers"),
        "{:?}",
        fitted.bytes
    );
    assert!(fitted.text.contains("answer to ask 1: use notes.txt"));
    assert!(!fitted.text.contains(file));
}

/// ADR-t2072-1: with the largest input (a huge reason, paths and
/// verification commands, a landing branch of 256 bytes and more
/// landed tasks than the section lists, each with a huge title and the
/// longest ID) every resolution request stays within
/// `RESUME_REQUEST_LIMIT` without its middle cut, keeps all its steps
/// and the section on landed tasks as that section bounds it, and says
/// how many bytes it cut and where the rest is.
#[test]
fn a_resolution_request_stays_within_its_limit_with_the_largest_input() {
    let reason = "理".repeat(400_000);
    let task = task_with_lists(
        (0..10_000).map(|n| format!("src/{n}/")).collect(),
        vec!["c".repeat(100_000)],
    );
    let landed: Vec<LandedTask> = (0..LANDED_TASK_LINES as i64 + 5)
        .map(|n| LandedTask {
            task_id: TaskId::new(i64::MAX - n),
            title: "長".repeat(1_000),
        })
        .collect();
    let file = "/runs/run/resume-1-reason.txt";
    for provider in [Provider::Claude, Provider::Codex] {
        let run = run_on(provider, WorkerMode::Headless);
        for kind in RESUME_KINDS {
            let request = ResumeRequest {
                main: CommitSha::try_from("2".repeat(40).as_str()).unwrap(),
                branch: "b".repeat(256),
                reason: reason.clone(),
                kind,
                reason_file: Some(file.into()),
                known_failures: None,
                carried_answers: vec!["答".repeat(100_000), "答".repeat(100_000)],
                answers_file: Some("/runs/run/resume-1-answers.txt".into()),
            };
            let fitted = resume_request(&task, &run, &request, &landed).unwrap();
            let text = &fitted.text;
            assert!(
                text.len() <= room(RESUME_REQUEST_LIMIT),
                "{kind:?}: {}",
                text.len()
            );
            assert_eq!(fitted.bytes.total, text.len());
            assert_eq!(fitted.bytes.limit, RESUME_REQUEST_LIMIT);
            let over = fitted.bytes.over_limit.clone().unwrap();
            assert!(!over.contains("its middle was cut"), "{over}");
            assert!(over.contains("verify: "), "{over}");
            assert_eq!(
                over.contains("paths: "),
                kind == ResumeKind::ScopeViolation,
                "{over}"
            );
            assert_eq!(fitted.bytes.omitted["reason"], 1);
            let (kept, note) = text
                .split_once("Reason: ")
                .unwrap()
                .1
                .split_once("\n[… ")
                .unwrap();
            assert!(kept.len() <= RESUME_REASON_BYTES && reason.starts_with(kept));
            assert!(
                note.starts_with(&format!(
                    "{} bytes left out by the prompt's limit; the whole reason is in {file}]",
                    reason.len() - kept.len()
                )),
                "{note}"
            );
            assert!(fitted.bytes.sections["landed"] <= LANDED_SECTION_BYTES);
            assert!(text.contains("- … and 5 more; git log --oneline"));
            for step in 1..=7 {
                assert!(text.contains(&format!("\n{step}. ")), "{kind:?} {step}");
            }
            assert!(text.contains(HEADLESS_STOP) && text.ends_with("end the turn."));
        }
    }
    // Without a file to point at, the cut reason is in no file.
    let request = ResumeRequest {
        main: CommitSha::try_from(SHA).unwrap(),
        branch: "main".into(),
        reason,
        kind: ResumeKind::Triage,
        reason_file: None,
        known_failures: None,
        carried_answers: Vec::new(),
        answers_file: None,
    };
    let run = run_on(Provider::Claude, WorkerMode::Headless);
    let text = resume_request(&task, &run, &request, &[]).unwrap().text;
    assert!(
        text.contains(&format!("by the prompt's limit; {NOT_READABLE}]")),
        "{text}"
    );
}

/// ADR-t2072-1: within their limits (no text, or each at its limit)
/// the next-turn messages carry their texts whole as before, with no
/// note of what was left out, and record no cut.
#[test]
fn within_its_limits_a_next_turn_message_carries_its_texts_whole() {
    let run = run_on(Provider::Claude, WorkerMode::Headless);
    let task = task_with_lists(vec!["src/".into()], vec!["make gate".into()]);
    let head = CommitSha::try_from("2".repeat(40).as_str()).unwrap();
    for reason in [String::new(), "r".repeat(RESUME_REASON_BYTES)] {
        for kind in RESUME_KINDS {
            let request = ResumeRequest {
                main: CommitSha::try_from(SHA).unwrap(),
                branch: "main".into(),
                reason: reason.clone(),
                kind,
                reason_file: Some("/runs/run/resume-1-reason.txt".into()),
                known_failures: None,
                carried_answers: Vec::new(),
                answers_file: None,
            };
            let fitted = resume_request(&task, &run, &request, &[]).unwrap();
            assert!(fitted.text.contains(&format!("\nReason: {reason}\n")));
            assert!(
                fitted
                    .text
                    .contains("run the verification commands [\"make gate\"].")
            );
            if kind == ResumeKind::ScopeViolation {
                assert!(fitted.text.contains("the task's --paths (src/), so"));
            }
            assert!(fitted.bytes.omitted.is_empty(), "{:?}", fitted.bytes);
            assert_eq!(fitted.bytes.over_limit, None);
            assert!(!fitted.text.contains("left out by"));
        }
    }
    let answer = "a".repeat(NEXT_TURN_TEXT_BYTES);
    let name = "n".repeat(NEXT_TURN_NAME_BYTES);
    let why = "w".repeat(NEXT_TURN_WHY_BYTES);
    let messages = [
        (
            answer_text(&run, 3, &answer),
            format!("answer to ask 3: {answer}\n\n{HEADLESS_GO_ON}"),
        ),
        (
            answer_text(&run, 3, ""),
            format!("answer to ask 3: \n\n{HEADLESS_GO_ON}"),
        ),
        (
            recovery_instruction(&run, "stalled", &format!(" {answer}\n")),
            format!(
                "dagq: the supervisor's recovery job for run {RUN} (alert stalled) asks: {answer}\n\n{HEADLESS_GO_ON}"
            ),
        ),
        (
            revise_mismatch_request(&run, "the revise", &why).unwrap(),
            format!("of run {RUN} cannot be accepted: {why}.\n"),
        ),
        (
            stale_receipt_nudge(&run, &name, &head).unwrap(),
            format!("its receipt names commit {name} while"),
        ),
        (
            closed_question_notice(&run, 5, Some(&name), Some(&answer)).unwrap(),
            format!("closed by {name} without"),
        ),
        (
            closed_question_notice(&run, 5, Some(&name), Some(&answer)).unwrap(),
            format!("What was recorded with it when it was closed: {answer}\n"),
        ),
        (
            closed_question_notice(&run, 5, None, None).unwrap(),
            "No reason was recorded with the close.".to_owned(),
        ),
        (stall_nudge(&run).unwrap(), "Do one of these".to_owned()),
    ];
    for (fitted, whole) in messages {
        assert!(fitted.text.contains(&whole), "{whole}: {}", fitted.text);
        assert!(fitted.bytes.omitted.is_empty(), "{:?}", fitted.bytes);
        assert_eq!(fitted.bytes.over_limit, None);
        assert_eq!(fitted.bytes.limit, NEXT_TURN_LIMIT);
        assert_eq!(fitted.bytes.total, fitted.text.len());
        assert!(!fitted.text.contains("left out by"));
    }
}

/// ADR-t2072-1: with the largest input (a huge answer, instruction,
/// why, receipt commit and closer) the other next-turn messages stay
/// within `NEXT_TURN_LIMIT` without their middle cut, keep their fixed
/// steps, and say how many bytes they cut and where (or that nowhere)
/// the rest can be read.
#[test]
fn the_other_next_turn_messages_stay_within_their_limit_with_the_largest_input() {
    let huge = "答".repeat(300_000);
    let head = CommitSha::try_from("2".repeat(40).as_str()).unwrap();
    for provider in [Provider::Claude, Provider::Codex] {
        let run = run_on(provider, WorkerMode::Headless);
        let not_readable = format!("by the prompt's limit; {NOT_READABLE}]");
        let messages = [
            (
                answer_text(&run, i64::MAX, &huge),
                vec!["answer"],
                vec![HEADLESS_GO_ON.to_owned(), not_readable.clone()],
            ),
            (
                recovery_instruction(&run, "stalled", &huge),
                vec!["instruction"],
                vec![HEADLESS_GO_ON.to_owned(), not_readable.clone()],
            ),
            (
                revise_mismatch_request(&run, "the revise", &huge).unwrap(),
                vec!["why"],
                vec!["\n4. ".to_owned(), not_readable.clone()],
            ),
            (
                stale_receipt_nudge(&run, &huge, &head).unwrap(),
                vec!["receipt_commit"],
                vec![
                    "\n3. ".to_owned(),
                    "by the prompt's limit; the receipt at /runs/run/receipt.json has it whole]"
                        .to_owned(),
                ],
            ),
            (
                closed_question_notice(&run, i64::MAX, Some(&huge), Some(&huge)).unwrap(),
                vec!["closer", "answer"],
                vec![
                    "\n3. ".to_owned(),
                    HEADLESS_STOP.to_owned(),
                    not_readable.clone(),
                ],
            ),
            (stall_nudge(&run).unwrap(), vec![], vec!["\n3. ".to_owned()]),
        ];
        for (fitted, cut, kept) in messages {
            let text = &fitted.text;
            assert!(text.len() <= room(NEXT_TURN_LIMIT), "{}", text.len());
            assert_eq!(fitted.bytes.total, text.len());
            assert_eq!(fitted.bytes.over_limit, None, "{text}");
            for section in &cut {
                assert_eq!(fitted.bytes.omitted[section], 1, "{section}");
            }
            assert_eq!(fitted.bytes.omitted.len(), cut.len());
            for kept in kept {
                assert!(text.contains(&kept), "{kept}: {text}");
            }
            if !cut.is_empty() {
                assert!(text.contains(" bytes left out by the prompt's limit; "));
            }
        }
    }
}

/// A revise request names its number of the run's revise limit the latest
/// review attempt read, the default or a `dagq.toml`'s (ADR-t3033-1).
#[test]
fn a_revise_request_names_its_number_of_the_runs_limit() {
    let run = run_on(Provider::Claude, WorkerMode::Headless);
    let task = task_with_lists(vec!["src/".into()], vec!["make gate".into()]);
    for (number, limit, says) in [
        (1, ReviseLimit::default(), "(revise 1 of 2 of the run)."),
        (
            3,
            ReviseLimit::new(4).unwrap(),
            "(revise 3 of 4 of the run).",
        ),
    ] {
        let text = revise_request(&task, &run, (number, limit), &["fix it".into()], None)
            .unwrap()
            .text;
        assert!(text.contains(says), "{text}");
    }
}

/// ADR-t2072-1: within its limits (no findings, a few, or all of them
/// at the section's limit) a revise request carries its findings, one
/// line each, and its steps as before, with no note of what was left
/// out, and records no cut; so does the text to go on after a hold.
#[test]
fn within_its_limits_a_revise_request_carries_its_findings_whole() {
    let run = run_on(Provider::Claude, WorkerMode::Headless);
    let task = task_with_lists(vec!["src/".into()], vec!["make gate".into()]);
    // Each finding line is "- " and the finding, joined by newlines.
    let at_limit = vec!["f".repeat(REVISE_FINDINGS_BYTES - 2)];
    let few = vec!["name the file".to_owned(), "add a test".to_owned()];
    for reasons in [vec![], few, at_limit] {
        let fitted = revise_request(
            &task,
            &run,
            (1, ReviseLimit::default()),
            &reasons,
            Some("/runs/run/f.txt"),
        )
        .unwrap();
        let findings = reasons
            .iter()
            .map(|reason| format!("- {reason}\n"))
            .collect::<String>();
        let steps = format!(
            "Findings:\n{findings}Steps:\n1. Fix the findings in this worktree and commit.\n2. {}\n3. Keep the worktree clean.\n4. {HEADLESS_STOP}\n5. Rewrite the receipt at /runs/run/receipt.json with the new head commit",
            local_checks(r#"["make gate"]"#)
        );
        assert!(fitted.text.contains(&steps), "{}", fitted.text);
        assert!(fitted.text.contains("\n6. "));
        assert!(fitted.bytes.omitted.is_empty(), "{:?}", fitted.bytes);
        assert_eq!(fitted.bytes.over_limit, None);
        assert_eq!(fitted.bytes.limit, REVISE_REQUEST_LIMIT);
        assert_eq!(fitted.bytes.total, fitted.text.len());
        assert!(!fitted.text.contains("left out by"));
    }
    let fitted = continue_text(&run);
    assert_eq!(
        fitted.text,
        format!(
            "{}\n\n{HEADLESS_GO_ON}",
            crate::domain::queue_hold::CONTINUE_TEXT
        )
    );
    assert_eq!(fitted.bytes.limit, NEXT_TURN_LIMIT);
    assert_eq!(fitted.bytes.total, fitted.text.len());
    assert!(fitted.bytes.omitted.is_empty());
}

/// ADR-t2072-1: with the largest input (one huge finding, or very many,
/// and huge verification commands) a revise request stays within
/// `REVISE_REQUEST_LIMIT` without its middle cut, keeps all its steps,
/// cuts the verification commands as the worker's prompt does (said in
/// `over_limit`), and says how many bytes of the findings it cut and
/// where they are whole, or that they are in no file.
#[test]
fn a_revise_request_stays_within_its_limit_with_the_largest_input() {
    let task = task_with_lists(vec!["src/".into()], vec!["c".repeat(100_000)]);
    let huge = vec!["指".repeat(300_000)];
    let many: Vec<String> = (0..20_000).map(|n| format!("finding {n}")).collect();
    let file = "/runs/run/revise-1-findings.txt";
    for provider in [Provider::Claude, Provider::Codex] {
        let run = run_on(provider, WorkerMode::Headless);
        for reasons in [&huge, &many] {
            let fitted = revise_request(
                &task,
                &run,
                (2, ReviseLimit::default()),
                reasons,
                Some(file),
            )
            .unwrap();
            let text = &fitted.text;
            assert!(text.len() <= room(REVISE_REQUEST_LIMIT), "{}", text.len());
            assert_eq!(fitted.bytes.total, text.len());
            let over = fitted.bytes.over_limit.clone().unwrap();
            assert!(!over.contains("its middle was cut"), "{over}");
            assert!(over.contains("verify: "), "{over}");
            assert_eq!(fitted.bytes.omitted["findings"], 1);
            let whole = revise_findings(reasons);
            let (kept, note) = text
                .split_once("Findings:\n")
                .unwrap()
                .1
                .split_once("\n[… ")
                .unwrap();
            assert!(kept.len() <= REVISE_FINDINGS_BYTES && whole.starts_with(kept));
            assert!(
                    note.starts_with(&format!(
                        "{} bytes left out by the prompt's limit; the whole findings are in {file}]\nSteps:\n",
                        whole.len() - kept.len()
                    )),
                    "{note}"
                );
            for step in 1..=6 {
                assert!(text.contains(&format!("\n{step}. ")), "{step}");
            }
            assert!(text.contains(HEADLESS_STOP) && text.ends_with("end the turn."));
        }
    }
    // Without a file to point at, the cut findings are in no file.
    let run = run_on(Provider::Claude, WorkerMode::Headless);
    let text = revise_request(&task, &run, (1, ReviseLimit::default()), &huge, None)
        .unwrap()
        .text;
    assert!(
        text.contains(&format!("by the prompt's limit; {NOT_READABLE}]")),
        "{text}"
    );
}

/// ADR-t2072-1: a request sent again from its file without what it
/// took is measured anew as one section: whole within its limit, and
/// one past it (written before the limits) keeps its start within the
/// limit and names the file that has it whole.
#[test]
fn a_restored_request_is_measured_anew_and_cut_to_its_limit() {
    let file = Path::new("/runs/run/resume-1.txt");
    let whole = restored_request("rebase", RESUME_REQUEST_LIMIT, file);
    assert_eq!(whole.text, "rebase");
    assert_eq!(whole.bytes.total, 6);
    assert_eq!(whole.bytes.limit, RESUME_REQUEST_LIMIT);
    assert_eq!(whole.bytes.sections["request"], 6);
    assert!(whole.bytes.omitted.is_empty() && whole.bytes.over_limit.is_none());
    let old = "古".repeat(100_000);
    for limit in [RESUME_REQUEST_LIMIT, REVISE_REQUEST_LIMIT] {
        let cut = restored_request(&old, limit, file);
        assert!(cut.text.len() <= limit, "{}", cut.text.len());
        assert_eq!(cut.bytes.total, cut.text.len());
        assert_eq!(cut.bytes.omitted["request"], 1);
        assert!(cut.bytes.over_limit.unwrap().starts_with("request: "));
        let (kept, note) = cut.text.split_once("\n[… ").unwrap();
        assert!(old.starts_with(kept));
        assert_eq!(
            note,
            format!(
                "{} bytes left out by the prompt's limit; the whole request is in /runs/run/resume-1.txt]",
                old.len() - kept.len()
            )
        );
    }
}

/// What a request took is read back as it was written (a resume's
/// `handoff.json` carries it to the next process).
#[test]
fn prompt_bytes_are_read_back_as_written() {
    let task = task_with_lists(vec!["src/".into()], vec!["c".repeat(100_000)]);
    let run = run_on(Provider::Claude, WorkerMode::Headless);
    let bytes = revise_request(
        &task,
        &run,
        (1, ReviseLimit::default()),
        &["x".repeat(50_000)],
        None,
    )
    .unwrap()
    .bytes;
    let read: PromptBytes = serde_json::from_value(json!(bytes)).unwrap();
    assert_eq!(read, bytes);
}

/// The tree of the head the handoff tests' worktree is at, and another.
const HEAD_TREE: &str = "2222222222222222222222222222222222222222";
const OTHER_TREE: &str = "3333333333333333333333333333333333333333";

/// No name a path gives a test.
static NO_NAMES: BTreeSet<String> = BTreeSet::new();

/// A record of a local check whose tree was `start` and `end`, which
/// exited with `exit` after 22 minutes; `None` for a broken one.
type CheckRecord<'a> = Option<(&'a str, &'a str, i64, &'a str)>;

/// `records` judged against HEAD's tree as a review judges them.
fn judged_checks(records: &[CheckRecord<'_>]) -> LocalChecks {
    let mut checks = LocalChecks::default();
    for record in records {
        let bytes = record.map(|(start, end, exit, command)| {
            json!({
                "command": command, "exit_status": exit, "started_at": "2026-10-19T01:02:03Z",
                "duration_ms": 1_320_000, "kind": null, "tree_start": start, "tree_end": end,
                "log": "1.log",
            })
            .to_string()
        });
        checks.add(crate::domain::local_checks::judge(
            bytes.as_deref().map(str::as_bytes),
            HEAD_TREE,
        ));
    }
    checks
}

/// No record at all.
static NO_CHECKS: LocalChecks = LocalChecks {
    matched: Vec::new(),
    other_tree: 0,
    broken: 0,
};

/// The material of a handoff with no work, record or list yet.
fn bare_handoff(request: &str) -> HandoffMaterial<'_> {
    HandoffMaterial {
        peak_context: 200_001,
        threshold: 200_000,
        resume_timeout: 3600,
        commits: "",
        changes: "",
        head_tree: Some(HEAD_TREE),
        checks: &NO_CHECKS,
        older_checks: 0,
        known_failures: None,
        touched: &NO_NAMES,
        receipt_summary: None,
        request,
        request_file: None,
    }
}

/// ADR-t2080-1 decision 2 as ADR-t2852-1 amends it: the handoff prompt
/// of a new session carries why it is new, not to read the documents
/// again first, the resume timeout, the commits, the uncommitted
/// changes, the local checks' records, the receipt's summary and the
/// request it would have been sent, whole when they fit, and nothing
/// else (no transcript); with the task's prompt before it, it is the new
/// session's first turn.
#[test]
fn a_handoff_carries_the_work_the_checks_the_receipt_and_the_request() {
    let run = run_on(Provider::Codex, WorkerMode::Headless);
    let material = HandoffMaterial {
        commits: "abc1234 runtime: the fix\nabc1233 runtime: the start\n",
        changes: " M src/lib.rs\n?? notes.txt\n",
        receipt_summary: Some("did the fix"),
        ..bare_handoff("dagq: the review asks you to revise.")
    };
    let fitted = handoff_text(&run, &material);
    assert_eq!(
        fitted.text,
        format!(
            "dagq: this run's worker goes on in a new session: its previous turn's context took 200001 tokens, above the 200000 tokens this repository sets, so nothing of the earlier conversation carries over. The task's prompt is above; the run, its worktree, its branch and its provider are the same. The work so far is in this worktree and its branch: go on from it rather than starting over.\n\nDo not start by reading again the instructions and documents the task's prompt names: what the task asks is above and below. Open a document only when the work needs it.\n\nThe supervisor stops this session unless you rewrite the receipt within 3600 seconds of this request (the resume timeout). Once your fixes are made and the checks the final tree needs have run, rewrite the receipt before any other work, such as a subagent review again.\n\nCommits since the base {SHA}, newest first:\nabc1234 runtime: the fix\nabc1233 runtime: the start\n\nUncommitted changes in the worktree (`git status --porcelain`):\n M src/lib.rs\n?? notes.txt\n\nYour local checks on the tree {HEAD_TREE} of HEAD, as the project's scripts recorded them in the directory `$DAGQ_LOCAL_CHECKS_DIR` names, newest first:\n(none)\n\nThe summary of the receipt you wrote last:\ndid the fix\n\nThe earlier session would have been sent this next; it is yours now:\n\ndagq: the review asks you to revise."
        )
    );
    assert!(fitted.bytes.omitted.is_empty(), "{:?}", fitted.bytes);
    assert_eq!(fitted.bytes.over_limit, None);
    assert_eq!(fitted.bytes.limit, HANDOFF_LIMIT);
    assert_eq!(fitted.bytes.total, fitted.text.len());
    for section in ["commits", "changes", "checks", "receipt", "request"] {
        assert!(fitted.bytes.sections[section] > 0, "{section}");
    }
    // A clean worktree with no commit and no receipt says so, and still
    // carries the request.
    let bare = handoff_text(&run, &bare_handoff("dagq: the review asks you to revise."));
    assert!(bare.text.contains("newest first:\n(none)\n\nUncommitted"));
    assert!(
        bare.text
            .contains("--porcelain`):\n(none)\n\nYour local checks")
    );
    assert!(!bare.text.contains("The summary of the receipt"));
    assert!(bare.text.ends_with("dagq: the review asks you to revise."));
    // The first turn of the new session: the task's prompt, then this.
    let first = crate::domain::turn::new_session_prompt("the task's prompt", &fitted.text);
    assert!(first.starts_with("the task's prompt\n\ndagq: this run's worker goes on"));
}

/// ADR-t2852-1: only the local checks' records on HEAD's tree are shown,
/// each with its command, its result and how long it took, the newest
/// first; those of another tree, whose tree changed while it ran or that
/// are broken are left out as a review's material leaves them out
/// (ADR-t2589-1 decision 4) and only counted, said to be no ground but a
/// clue in the records' directory; after a change the final tree's
/// checks run again (ADR-t2589-1 unchanged), never only those the diff
/// touches.
#[test]
fn a_handoff_shows_only_the_local_checks_on_heads_tree() {
    let run = run_on(Provider::Claude, WorkerMode::Headless);
    let checks = judged_checks(&[
        Some((HEAD_TREE, HEAD_TREE, 1, "sh scripts/first.sh")),
        Some((OTHER_TREE, OTHER_TREE, 101, "sh scripts/stress.sh old")),
        Some((OTHER_TREE, HEAD_TREE, 0, "sh scripts/changed.sh")),
        None,
        Some((HEAD_TREE, HEAD_TREE, 0, "sh scripts/lint.sh")),
    ]);
    let material = HandoffMaterial {
        checks: &checks,
        older_checks: 3,
        ..bare_handoff("the request")
    };
    let text = handoff_text(&run, &material).text;
    let lint = text
        .find("- exit 0, took 22m 0s, started 2026-10-19T01:02:03Z: sh scripts/lint.sh\n")
        .unwrap_or_else(|| panic!("{text}"));
    let first = text
        .find("- exit 1, took 22m 0s, started 2026-10-19T01:02:03Z: sh scripts/first.sh\n")
        .unwrap_or_else(|| panic!("{text}"));
    assert!(lint < first, "newest first");
    assert!(text.contains(&format!(
        "Your local checks on the tree {HEAD_TREE} of HEAD"
    )));
    // The others' commands never reach the prompt; they are counted.
    for excluded in ["scripts/stress.sh", "scripts/changed.sh", "exit 101"] {
        assert!(!text.contains(excluded), "{excluded}");
    }
    for said in [
        "(2 records of another tree, or whose tree changed while it ran, are not shown: they are no ground for the receipt. the directory `$DAGQ_LOCAL_CHECKS_DIR` names holds them, a clue to go on from: what passed, what failed and how long it took.)",
        "(1 records could not be read or are over their limits.)",
        "(3 older records were not read: past the most records a run's handoff reads.)",
        "A record shown here ran on HEAD's tree: while nothing changes, it is a ground for the receipt as it is, so do not run it again.",
        "Once you change anything, run again on the final tree the checks it needs",
    ] {
        assert!(text.contains(said), "{said}\n{text}");
    }
    for unsaid in ["only the checks", "only those", "the diff touches"] {
        assert!(!text.contains(unsaid), "{unsaid}");
    }
    // HEAD's tree unread: the supervisor judges against no tree, so no
    // record is on it and none is shown.
    let unread_checks = {
        let mut checks = LocalChecks::default();
        for _ in 0..3 {
            checks.add(crate::domain::local_checks::judge(
                Some(
                    json!({"command": "sh scripts/lint.sh", "exit_status": 0, "started_at": "t",
                           "duration_ms": 1, "tree_start": HEAD_TREE, "tree_end": HEAD_TREE, "log": "1.log"})
                    .to_string()
                    .as_bytes(),
                ),
                "",
            ));
        }
        checks
    };
    let unread = handoff_text(
        &run,
        &HandoffMaterial {
            head_tree: None,
            checks: &unread_checks,
            ..material
        },
    )
    .text;
    assert!(unread.contains("HEAD's tree could not be read, so none is shown"));
    assert!(!unread.contains("scripts/lint.sh"));
    assert!(unread.contains("(3 records of another tree"));
    // No record: said, and no rule of records follows.
    let none = handoff_text(&run, &bare_handoff("the request")).text;
    assert!(none.contains(" of HEAD, as the project's scripts recorded them in the directory `$DAGQ_LOCAL_CHECKS_DIR` names, newest first:\n(none)\n\nThe earlier session"));
    assert!(!none.contains("is a ground for the receipt"));
}

/// ADR-t2852-1: the new session is told not to read the documents again
/// first but to open them when the work needs them, and the resume
/// timeout's seconds, before which it rewrites the receipt ahead of any
/// other work once the final tree's checks have run.
#[test]
fn a_handoff_says_to_read_only_what_is_needed_and_the_resume_timeout() {
    let run = run_on(Provider::Claude, WorkerMode::Headless);
    let text = handoff_text(
        &run,
        &HandoffMaterial {
            resume_timeout: 1234,
            ..bare_handoff("the request")
        },
    )
    .text;
    let reading = text
        .find(
            "Do not start by reading again the instructions and documents the task's prompt names",
        )
        .unwrap();
    assert!(text.contains("Open a document only when the work needs it."));
    assert!(!text.contains(WORKER_READING.trim_end()));
    assert!(reading < text.find("Commits since the base").unwrap());
    assert!(text.contains(
        "unless you rewrite the receipt within 1234 seconds of this request (the resume timeout)"
    ));
    assert!(text.contains("Once your fixes are made and the checks the final tree needs have run, rewrite the receipt before any other work, such as a subagent review again."));
}

/// ADR-t2852-1: a revise's handoff carries the review's findings once,
/// in the request it would have sent.
#[test]
fn a_revises_handoff_carries_the_findings_once() {
    let run = run_on(Provider::Claude, WorkerMode::Headless);
    let task = task_with_lists(vec!["src/".into()], vec!["cargo test".into()]);
    let finding = "the boundary test misses the empty list";
    let request = revise_request(
        &task,
        &run,
        (1, ReviseLimit::default()),
        &[format!("- {finding}")],
        None,
    )
    .unwrap();
    let text = handoff_text(&run, &bare_handoff(&request.text)).text;
    assert_eq!(text.matches(finding).count(), 1, "{text}");
}

/// ADR-t2852-1: the handoff lists of the tests that fail already on the
/// landing branch's CI the task's own and those in the files the run
/// touches, and counts the rest with the file that lists them all.
#[test]
fn a_handoff_narrows_the_known_failures_to_the_files_the_run_touches() {
    let run = run_on(Provider::Claude, WorkerMode::Headless);
    let list = known(
        &[
            (
                "dagq::it runtime_fresh_session::a_new_session_starts",
                3,
                None,
            ),
            ("dagq::it planner_headless::a_planner_ends", 2, Some(5)),
            ("dagq application::prompt::tests::a_prompt_fits", 1, None),
            ("dagq::it runtime_codex::a_codex_review", 1, None),
        ],
        &["dagq::it runtime_own::the_fixed_test"],
    );
    let touched = touched_names(&[
        "tests/it/runtime_fresh_session.rs".into(),
        "src/application/prompt/tests.rs".into(),
    ]);
    let text = handoff_text(
        &run,
        &HandoffMaterial {
            known_failures: Some(&list),
            touched: &touched,
            ..bare_handoff("the request")
        },
    )
    .text;
    for kept in [
        "runtime_fresh_session::a_new_session_starts",
        "application::prompt::tests::a_prompt_fits",
        "runtime_own::the_fixed_test",
    ] {
        assert!(text.contains(kept), "{kept}");
    }
    for left in ["planner_headless", "runtime_codex"] {
        assert!(!text.contains(left), "{left}");
    }
    assert!(text.contains(&format!(
        "(2 more failing tests are in none of those files and are left out here; {KNOWN_FILE} lists them all. Do not fix them.)"
    )));
    // None of the others touched: said, with the count.
    let none = handoff_text(
        &run,
        &HandoffMaterial {
            known_failures: Some(&known(&[("dagq::it runtime_codex::a", 1, None)], &[])),
            touched: &touched,
            ..bare_handoff("the request")
        },
    )
    .text;
    assert!(none.contains("the task declares: none\n(1 more failing tests"));
}

/// ADR-t2852-1: an ordinary change that adds an integration test file,
/// registers it in the binary's `main.rs` and touches the package's
/// `dagq.toml` keeps only the tests of the files it changes. The binary's
/// and the package's names, which every test shares, match no test by
/// themselves, nor does a test's own name; the rest is counted with the
/// file that lists them all.
#[test]
fn an_ordinary_change_keeps_only_the_failures_of_its_test_files() {
    let run = run_on(Provider::Claude, WorkerMode::Headless);
    let list = known(
        &[
            (
                "dagq::it runtime_fresh_session::a_new_session_starts",
                5,
                None,
            ),
            ("dagq::it planner_headless::a_planner_ends", 4, None),
            ("dagq::it runtime_codex::it_holds_codex", 3, None),
            (
                "dagq application::prompt::worker::tests::a_prompt_fits",
                2,
                None,
            ),
            ("dagq domain::turn::tests::a_turn_ends", 1, None),
            ("dagq::it main::dagq", 1, None),
        ],
        &[],
    );
    let touched = touched_names(&[
        "tests/it/main.rs".into(),
        "tests/it/runtime_fresh_session.rs".into(),
        "dagq.toml".into(),
        "Cargo.lock".into(),
        "src/application/prompt/worker.rs".into(),
    ]);
    let text = handoff_text(
        &run,
        &HandoffMaterial {
            known_failures: Some(&list),
            touched: &touched,
            ..bare_handoff("the request")
        },
    )
    .text;
    for kept in [
        "runtime_fresh_session::a_new_session_starts",
        "application::prompt::worker::tests::a_prompt_fits",
    ] {
        assert!(text.contains(kept), "{kept}\n{text}");
    }
    for left in [
        "planner_headless",
        "runtime_codex",
        "domain::turn",
        "main::dagq",
    ] {
        assert!(!text.contains(left), "{left}\n{text}");
    }
    assert!(text.contains(&format!(
        "(4 more failing tests are in none of those files and are left out here; {KNOWN_FILE} lists them all. Do not fix them.)"
    )));
}

/// ADR-t2852-1 decision 5: a test's own name is never matched, also when
/// its name has no module path: a file named like the test, or like the
/// binary's last part, keeps none of them, while a test of a touched
/// module is kept.
#[test]
fn a_tests_own_name_keeps_no_failure_even_without_a_module_path() {
    let run = run_on(Provider::Claude, WorkerMode::Headless);
    let list = known(
        &[
            ("pkg::integration check_status", 3, None),
            ("check_status", 2, None),
            ("pkg::integration status::check_status", 1, None),
        ],
        &[],
    );
    let touched = touched_names(&["docs/check_status.md".into(), "src/integration.rs".into()]);
    let text = handoff_text(
        &run,
        &HandoffMaterial {
            known_failures: Some(&list),
            touched: &touched,
            ..bare_handoff("the request")
        },
    )
    .text;
    assert!(!text.contains("check_status (CI run"), "{text}");
    assert!(text.contains(&format!(
        "the task declares: none\n(3 more failing tests are in none of those files and are left out here; {KNOWN_FILE} lists them all. Do not fix them.)"
    )), "{text}");
    // A test of a touched module is kept.
    let touched = touched_names(&["src/status.rs".into()]);
    let text = handoff_text(
        &run,
        &HandoffMaterial {
            known_failures: Some(&list),
            touched: &touched,
            ..bare_handoff("the request")
        },
    )
    .text;
    assert!(
        text.contains("- pkg::integration status::check_status (CI run"),
        "{text}"
    );
    assert!(
        text.contains("(2 more failing tests are in none of those files"),
        "{text}"
    );
}

/// The names paths give a test: a file's stem, an index file's
/// directory, a glob's last plain part.
#[test]
fn the_paths_a_run_touches_name_tests_by_their_files() {
    let names = touched_names(&[
        "tests/it/runtime_claim.rs".into(),
        "src/application/prompt/mod.rs".into(),
        "src/domain/**".into(),
        "docs/**/*.md".into(),
        "**".into(),
        "src/lib.rs".into(),
    ]);
    let names: Vec<&str> = names.iter().map(String::as_str).collect();
    assert_eq!(names, ["docs", "domain", "prompt", "runtime_claim", "src"]);
}

/// ADR-t2080-1 decision 2 within ADR-t2072-1's limits: with the largest
/// material each section is cut to its bytes keeping its start (the
/// local checks' newest records), says how many bytes or records it left
/// out and where to read them, and the whole stays within
/// `HANDOFF_LIMIT` without its middle cut; the request's cut is said in
/// `over_limit`.
#[test]
fn a_handoff_with_the_largest_material_stays_within_its_limits() {
    let run = run_on(Provider::Claude, WorkerMode::Headless);
    let huge = "引".repeat(300_000);
    let request_file = Path::new("/runs/run/turns/request-9.json");
    let commands: Vec<String> = (0..crate::domain::local_checks::MAX_RECORDS)
        .map(|n| format!("{n} {}", "c".repeat(4000)))
        .collect();
    let records: Vec<CheckRecord<'_>> = commands
        .iter()
        .enumerate()
        .map(|(n, command)| Some((HEAD_TREE, HEAD_TREE, n as i64, command.as_str())))
        .collect();
    let checks = judged_checks(&records);
    let names: Vec<String> = (0..1000)
        .map(|n| format!("t{n}::{}", "n".repeat(1000)))
        .collect();
    let others: Vec<(&str, i64, Option<i64>)> =
        names.iter().map(|name| (name.as_str(), 1, None)).collect();
    let list = known(&others, &[]);
    let touched: BTreeSet<String> = (0..1000).map(|n| format!("t{n}")).collect();
    let material = HandoffMaterial {
        peak_context: u64::MAX,
        threshold: u64::MAX - 1,
        resume_timeout: u64::MAX,
        commits: &huge,
        changes: &huge,
        head_tree: Some(HEAD_TREE),
        checks: &checks,
        older_checks: 7,
        known_failures: Some(&list),
        touched: &touched,
        receipt_summary: Some(&huge),
        request: &huge,
        request_file: Some(request_file),
    };
    let fitted = handoff_text(&run, &material);
    assert!(fitted.text.len() <= HANDOFF_LIMIT - prompt_fit::LANGUAGE_ROOM);
    assert_eq!(fitted.bytes.total, fitted.text.len());
    assert!(
        !fitted
            .bytes
            .over_limit
            .as_deref()
            .unwrap_or_default()
            .contains("its middle was cut"),
        "{:?}",
        fitted.bytes
    );
    assert!(
        fitted
            .bytes
            .over_limit
            .as_deref()
            .unwrap()
            .starts_with("request: ")
    );
    for (section, bytes, read) in [
        (
            "commits",
            HANDOFF_COMMITS_BYTES,
            format!("`git log --oneline {SHA}..HEAD` in your worktree lists them all"),
        ),
        (
            "changes",
            HANDOFF_CHANGES_BYTES,
            "`git status` and `git diff` in your worktree show them all".to_owned(),
        ),
        (
            "receipt",
            HANDOFF_RECEIPT_BYTES,
            "the receipt at /runs/run/receipt.json holds the whole summary".to_owned(),
        ),
        (
            "request",
            HANDOFF_REQUEST_BYTES,
            format!("the whole of it is in {}", request_file.display()),
        ),
    ] {
        assert_eq!(fitted.bytes.omitted[section], 1, "{section}");
        assert!(fitted.bytes.sections[section] <= bytes, "{section}");
        assert!(
            fitted
                .text
                .contains(&format!("by the prompt's limit; {read}]")),
            "{section}"
        );
    }
    // The newest records kept, each command cut, the older counted.
    assert!(fitted.bytes.omitted["checks"] > 100, "{:?}", fitted.bytes);
    let last = crate::domain::local_checks::MAX_RECORDS - 1;
    assert!(fitted.text.contains(&format!("exit {last}, took")));
    assert!(!fitted.text.contains("exit 0, took"));
    assert!(fitted.text.contains("older records on HEAD's tree left out by this section's limit; the directory `$DAGQ_LOCAL_CHECKS_DIR` names holds them all."));
    assert!(fitted.text.contains("(7 older records were not read"));
    assert!(
        fitted
            .text
            .contains("the directory `$DAGQ_LOCAL_CHECKS_DIR` names holds the record whole]")
    );
    assert!(
        fitted
            .text
            .contains(&format!("{KNOWN_FILE} lists them all"))
    );
    assert!(
        fitted
            .text
            .starts_with("dagq: this run's worker goes on in a new session")
    );
    assert!(
        fitted
            .text
            .contains("The earlier session would have been sent this next")
    );
    // With the task's prompt at its own limit, the first turn stays
    // within the two limits.
    let task_prompt = "p".repeat(WORKER_PROMPT_LIMIT);
    let first = crate::domain::turn::new_session_prompt(&task_prompt, &fitted.text);
    assert!(first.len() <= WORKER_PROMPT_LIMIT + HANDOFF_LIMIT);
}

/// A handoff kept in a run file in place of a revise's or a resume's
/// request is held to its own limit when another supervisor sends it, so
/// none of it is cut there; any other request keeps its kind's limit.
#[test]
fn a_kept_handoff_is_held_to_the_handoffs_limit() {
    let run = run_on(Provider::Claude, WorkerMode::Headless);
    let handoff = handoff_text(&run, &bare_handoff("the resolution request"));
    for limit in [REVISE_REQUEST_LIMIT, RESUME_REQUEST_LIMIT] {
        assert_eq!(kept_request_limit(&handoff.text, limit), HANDOFF_LIMIT);
        assert_eq!(
            kept_request_limit("dagq: the supervisor's review", limit),
            limit
        );
    }
}

/// The file the known failures' tests write the whole list to.
const KNOWN_FILE: &str = "/runs/r/known-failures.json";

/// The known failures of `ci failures --task` (ADR-t1920-1 decision 5):
/// `others` each `(name, run_id, fix task)`, `fixing` the task's own.
fn known(others: &[(&str, i64, Option<i64>)], fixing: &[&str]) -> KnownFailures {
    let item = |name: &str, run: i64, fix: Option<i64>| {
        json!({"name": name, "kind": "test",
               "added": {"run_id": run, "attempt": 1, "sha": "s", "url": format!("https://ci/runs/{run}"), "at": "t"},
               "finding_id": 4, "fix_tasks": fix.into_iter().collect::<Vec<_>>()})
    };
    let view = json!({
        "failures": others.iter().map(|(name, run, fix)| item(name, *run, *fix)).collect::<Vec<_>>(),
        "kept_for_task": fixing.iter().map(|name| item(name, 9, Some(9))).collect::<Vec<_>>(),
    });
    KnownFailures::from_view(&view, KNOWN_FILE).unwrap()
}

/// What the worker is told of the tests it does not fix.
const DO_NOT_FIX: &str = "If a test of this list or of its file fails in your checks and your change does not cause it, do not fix it: name it in the receipt's summary, since the task that fixes it owns it.";

/// What the whole review is told of the tests the run's task does not fix.
const DO_NOT_SEND_BACK: &str = "Do not send the run back for a failure of a test of this list or of its file unless the diff causes it: the task that fixes it owns it.\n";

/// The list keeps the CI run that failed last first, with each item's URL
/// and fix tasks, and is no list when it holds nothing for the run.
#[test]
fn the_known_failures_are_read_newest_first_and_none_without_any() {
    let list = known(&[("old", 1, Some(5)), ("new", 3, None)], &["own"]);
    let names: Vec<&str> = list.others.iter().map(|f| f.name.as_str()).collect();
    assert_eq!(names, ["new", "old"]);
    assert_eq!(list.others[1].url.as_deref(), Some("https://ci/runs/1"));
    assert_eq!(list.others[1].fix_tasks, [TaskId::new(5)]);
    assert_eq!(list.fixing[0].name, "own");
    assert_eq!(list.file, KNOWN_FILE);
    let empty = json!({"enabled": false, "state": "unknown", "watch": "disabled", "failures": []});
    assert_eq!(KnownFailures::from_view(&empty, KNOWN_FILE), None);
}

/// AC 1, 2: the claim's prompt and every resolution request carry the
/// tests that fail already, with their CI run and fix task, and tell the
/// worker not to fix one its change does not cause but to name it in the
/// receipt; without a list there is no section.
#[test]
fn the_worker_prompt_and_its_requests_carry_the_known_failures_only_when_there_are_some() {
    let task = task_with_evidence(7, Vec::new());
    let own_run = run(7, RunStatus::Claimed, None);
    let list = known(
        &[
            ("dagq::it a::fails", 3, Some(12)),
            ("dagq::it b::fails", 2, None),
        ],
        &[],
    );
    let lines = [
        "Tests that fail already on the landing branch's CI, each with the CI run that failed and the task that fixes it:\n",
        "- dagq::it a::fails (CI run: https://ci/runs/3; fixed by task 12)\n",
        "- dagq::it b::fails (CI run: https://ci/runs/2; no task fixes it yet)\n",
        DO_NOT_FIX,
    ];
    let with = prompt(&task, &own_run, None, &[], &[], &[], None, &[], Some(&list)).unwrap();
    for line in lines {
        assert!(with.text.contains(line), "{line}: {}", with.text);
    }
    assert!(with.bytes.sections["known_failures"] > 0);
    assert!(!with.bytes.omitted.contains_key("known_failures"));
    assert!(!with.text.contains("This task fixes these tests"));
    let without = prompt(&task, &own_run, None, &[], &[], &[], None, &[], None).unwrap();
    assert!(!without.text.contains("fail already"), "{}", without.text);
    assert!(!without.bytes.sections.contains_key("known_failures"));
    let run = run_on(Provider::Claude, WorkerMode::Headless);
    for kind in RESUME_KINDS {
        for list in [Some(list.clone()), None] {
            let request = ResumeRequest {
                main: CommitSha::try_from(SHA).unwrap(),
                branch: "main".into(),
                reason: "why".into(),
                kind,
                reason_file: None,
                known_failures: list.clone(),
                carried_answers: Vec::new(),
                answers_file: None,
            };
            let text = resume_request(&task, &run, &request, &[]).unwrap().text;
            for line in lines {
                assert_eq!(
                    text.contains(line.trim_end()),
                    list.is_some(),
                    "{kind:?}: {text}"
                );
            }
        }
    }
}

/// AC 2: a run of a task that fixes a CI failure is shown its tests as
/// what it makes pass, without naming itself as their fixer, and the
/// others as before.
#[test]
fn a_fix_tasks_run_is_shown_its_failing_tests_to_make_pass() {
    let task = task_with_evidence(9, Vec::new());
    let own_run = run(9, RunStatus::Claimed, None);
    let list = known(&[("other", 2, Some(12))], &["dagq::it mine::fails"]);
    let text = prompt(&task, &own_run, None, &[], &[], &[], None, &[], Some(&list))
        .unwrap()
        .text;
    let fixing = "This task fixes these tests, which fail already on the landing branch's CI: make them pass:\n- dagq::it mine::fails (CI run: https://ci/runs/9)\n";
    assert!(text.contains(fixing), "{text}");
    assert!(text.contains("- other (CI run: https://ci/runs/2; fixed by task 12)\n"));
    assert!(text.contains(DO_NOT_FIX));
    // Only its own: no line on the tests others fix.
    let only = known(&[], &["dagq::it mine::fails"]);
    let text = prompt(&task, &own_run, None, &[], &[], &[], None, &[], Some(&only))
        .unwrap()
        .text;
    assert!(text.contains(fixing), "{text}");
    assert!(!text.contains(DO_NOT_FIX), "{text}");
}

/// AC 3: the whole review's prompt carries the list as material for its
/// verdict, and an agent's job, which does not decide the send back,
/// carries none.
#[test]
fn the_whole_review_carries_the_known_failures_and_an_agents_job_does_not() {
    use crate::application::agent_job::{AgentJob, build};
    use crate::domain::review_subagents::{AgentRole, AgentTools};
    let task = task_with_evidence(9, Vec::new());
    let run = run_on(Provider::Claude, WorkerMode::Headless);
    let list = known(&[("other", 2, Some(12))], &["mine"]);
    let review = review_prompt(
        &task,
        &run,
        "/runs/r/review.md",
        Some(&list),
        None,
        &PersonAnswers::default(),
    );
    for line in [
        "This task fixes these tests, which fail already on the landing branch's CI: judge whether the diff makes them pass:\n- mine (CI run: https://ci/runs/9)\n",
        "- other (CI run: https://ci/runs/2; fixed by task 12)\n",
        DO_NOT_SEND_BACK,
    ] {
        assert!(review.text.contains(line), "{line}: {}", review.text);
    }
    assert!(review.bytes.sections["known_failures"] > 0);
    within(&review, RUN_REVIEW_PROMPT_LIMIT);
    let without = review_prompt(
        &task,
        &run,
        "/runs/r/review.md",
        None,
        None,
        &PersonAnswers::default(),
    );
    assert!(!without.text.contains("fail already"), "{}", without.text);
    let definition = "---\ndescription: checks the design\ntools: [read]\n---\nCheck it.\n";
    let tools = AgentTools::declared(AgentRole::Review, definition).unwrap();
    let agent = build(&AgentJob {
        agent: "design",
        definition,
        tools: &tools,
        rules: None,
        base: SHA,
        head: SHA,
        material: Path::new("/runs/r/review.md"),
        cwd: Path::new("/runs/r/worktree"),
        dir: Path::new("/runs/r/review-1-agents/design"),
        language: None,
        reads: None,
        answers: None,
    })
    .unwrap();
    assert!(!agent.prompt.text.contains("fail already"));
    assert!(!agent.prompt.bytes.sections.contains_key("known_failures"));
}

/// AC 4, 5: with 1,000 known failures of 1 KB each (and the largest input
/// of each prompt), the worker's prompt, every resolution request and the
/// whole review's prompt stay within their whole limits, count the
/// section's bytes and what it left out, and say how many it left out and
/// that the run directory's file lists them all.
#[test]
fn a_thousand_known_failures_stay_within_each_prompts_limit_and_name_their_file() {
    let names: Vec<String> = (0..1_000)
        .map(|n| format!("{n:04} {}", "t".repeat(1_019)))
        .collect();
    let others: Vec<(&str, i64, Option<i64>)> = names
        .iter()
        .enumerate()
        .map(|(n, name)| (name.as_str(), n as i64, Some(n as i64)))
        .collect();
    let list = known(&others, &[]);
    let note = |left: usize| {
        format!(
            "({left} failing tests left out by this section's limit. To read them: {KNOWN_FILE} lists them all, with the tasks that fix each as fix_tasks.)\n"
        )
    };
    let check = |fitted: &FittedPrompt, limit: usize| {
        within(fitted, limit);
        let bytes = &fitted.bytes;
        assert!(
            bytes.sections["known_failures"] <= KNOWN_FAILURES_LIMIT,
            "{bytes:?}"
        );
        // Each kept item had its name cut; with those left out every
        // item is counted once.
        assert_eq!(bytes.omitted["known_failures"], 1_000, "{bytes:?}");
        let kept = fitted.text.matches("(CI run: https://ci/runs/").count();
        assert!(kept > 0 && kept <= KNOWN_FAILURES_COUNT, "{kept}");
        assert!(fitted.text.contains(&note(1_000 - kept)), "{}", fitted.text);
        // The CI run that failed last first.
        assert!(fitted.text.contains("- 0999 "), "the newest is kept");
        assert!(
            fitted
                .text
                .contains(&format!("{KNOWN_FILE} holds it whole"))
        );
    };
    let (task, goal, predecessors, goals, siblings, inherited) = worker_material(300, 400_000);
    let own_run = run(9, RunStatus::Claimed, None);
    let worker = prompt(
        &task,
        &own_run,
        Some(&goal),
        &predecessors,
        &goals,
        &siblings,
        Some(&inherited),
        &[],
        Some(&list),
    )
    .unwrap();
    check(&worker, WORKER_PROMPT_LIMIT);
    let reason = "理".repeat(400_000);
    let big = task_with_lists(
        (0..10_000).map(|n| format!("src/{n}/")).collect(),
        vec!["c".repeat(100_000)],
    );
    let landed: Vec<LandedTask> = (0..LANDED_TASK_LINES as i64 + 5)
        .map(|n| LandedTask {
            task_id: TaskId::new(i64::MAX - n),
            title: "長".repeat(1_000),
        })
        .collect();
    let run = run_on(Provider::Claude, WorkerMode::Headless);
    for kind in RESUME_KINDS {
        let request = ResumeRequest {
            main: CommitSha::try_from("2".repeat(40).as_str()).unwrap(),
            branch: "b".repeat(256),
            reason: reason.clone(),
            kind,
            reason_file: Some("/runs/r/resume-1-reason.txt".into()),
            known_failures: Some(list.clone()),
            carried_answers: Vec::new(),
            answers_file: None,
        };
        let fitted = resume_request(&big, &run, &request, &landed).unwrap();
        check(&fitted, RESUME_REQUEST_LIMIT);
        let over = fitted.bytes.over_limit.clone().unwrap();
        assert!(!over.contains("its middle was cut"), "{kind:?}: {over}");
    }
    let review = review_prompt(
        &big_task(7, 100_000),
        &run,
        "/runs/r/review.md",
        Some(&list),
        None,
        &PersonAnswers::default(),
    );
    check(&review, RUN_REVIEW_PROMPT_LIMIT);
    let over = review.bytes.over_limit.clone().unwrap();
    assert!(!over.contains("its middle was cut"), "{over}");
}

/// Review revise 2: when the task's own tests fill the section, the
/// others' are all in the file only, and the worker and the whole review
/// are still told how to treat them, with the count left out and the file.
#[test]
fn the_others_tests_left_out_whole_still_get_their_instruction() {
    let own: Vec<String> = (0..KNOWN_FAILURES_COUNT)
        .map(|n| format!("own {n}"))
        .collect();
    let own: Vec<&str> = own.iter().map(String::as_str).collect();
    let list = known(&[("other a", 2, Some(12)), ("other b", 1, None)], &own);
    let note = format!(
        "(2 failing tests left out by this section's limit. To read them: {KNOWN_FILE} lists them all, with the tasks that fix each as fix_tasks.)\n"
    );
    let task = task_with_evidence(9, Vec::new());
    let worker = prompt(
        &task,
        &run(9, RunStatus::Claimed, None),
        None,
        &[],
        &[],
        &[],
        None,
        &[],
        Some(&list),
    )
    .unwrap();
    let review = review_prompt(
        &task,
        &run_on(Provider::Claude, WorkerMode::Headless),
        "/runs/r/review.md",
        Some(&list),
        None,
        &PersonAnswers::default(),
    );
    for (fitted, instruction) in [(&worker, DO_NOT_FIX), (&review, DO_NOT_SEND_BACK)] {
        let text = &fitted.text;
        assert!(
            text.contains("- own 19 (CI run: https://ci/runs/9)\n"),
            "{text}"
        );
        assert!(!text.contains("- other a"), "{text}");
        assert!(!text.contains("Tests that fail already"), "{text}");
        assert!(text.contains(&note), "{text}");
        assert!(text.contains(instruction.trim_end()), "{text}");
        assert_eq!(fitted.bytes.omitted["known_failures"], 2);
    }
}

/// ADR-t2599-1 decision 7: the run's review is told the judgment past the
/// run's revise limit reads its codes and lands the run with no code
/// finding left, while the plan review, whose codes still decide nothing,
/// keeps being told they are for statistics only.
#[test]
fn only_the_run_review_is_told_its_codes_land_the_run_past_the_limit() {
    const STATISTICS: &str = "The codes are recorded for statistics only and change nothing of how the verdict is applied.";
    let task = verified_task(7, "work", TaskStatus::InProgress, vec!["make gate".into()]);
    let review = review_prompt(
        &task,
        &run_on(Provider::Claude, WorkerMode::Headless),
        "r",
        None,
        None,
        &PersonAnswers::default(),
    )
    .text;
    assert!(!review.contains(STATISTICS), "{review}");
    assert!(!review.contains("statistics only"));
    for part in [
        "The verdict (pass, revise, concern) and how a concern is applied do not read the codes.",
        "once the run's automatic revises are used up",
        "lets the runtime land the run",
        "every finding of a security review agent count as code findings",
        "never only codes from it",
        "an acceptance_unmet whose paths all lie outside them is no code finding either, so give each finding the paths it is on.",
    ] {
        assert!(review.contains(part), "{part}");
    }
    // The codes that let the run land are the judgment's own list, and
    // the codes of a code finding are named nowhere in the sentence.
    let usage = run_review_codes_use();
    let (_, listed) = usage.split_once("being one of ").unwrap();
    let (listed, _) = listed.split_once(", lets").unwrap();
    let listed: Vec<&str> = listed.split(", ").collect();
    let converged: Vec<&str> = review_reason::CONVERGED_CODES
        .iter()
        .map(|(code, _)| *code)
        .collect();
    assert_eq!(listed, converged);
    for (code, _) in review_reason::REVIEW_CODES {
        let finding = review_reason::Finding {
            text: String::new(),
            codes: vec![(*code).to_owned()],
            paths: Vec::new(),
            security: false,
        };
        assert_eq!(
            listed.contains(code),
            !finding.is_code_finding(None),
            "{code}"
        );
    }
    assert!(review.contains(&usage));
    let plan = plan_case(0, 0).prompt().text;
    assert!(
        plan.contains(&format!("{STATISTICS} The codes:\n")),
        "{plan}"
    );
    assert!(!plan.contains(&usage));
    assert!(plan.contains(PLAN_REVIEW_CODES_USE));
}

/// ADR-t1818-2: the recovery job is told to declare each option's kind,
/// that its own options move nothing and come back to it, and that one
/// which reads as a runtime operation or the task's end is left out: the
/// ended run's prompt with its kinds and the hand-off, the live session's
/// with its own; the verify fix is the runtime's.
#[test]
fn a_recovery_prompt_tells_the_job_how_to_declare_its_options() {
    let task = task(7, "work", TaskStatus::InProgress);
    let facts = json!({});
    let binary = no_binary();
    let prompt = |run: &TaskRun, alert, ended: Option<String>| {
        recovery_prompt(
            &task,
            run,
            1,
            &RecoveryMaterial {
                alert,
                ended,
                facts: &facts,
                workspace: "ws",
                screen: "turns",
                processes: Ok(Vec::new()),
                git_status: "",
                head: SHA,
                receipt_commit: None,
                history: &[],
                allowed: &["retry", "resume", "wait"],
                binary: &binary,
            },
        )
        .unwrap()
        .text
    };
    let ended = prompt(
        &run(7, RunStatus::Failed, None),
        RecoveryAlert::Failed,
        Some("failed".into()),
    );
    assert!(ended.contains(RECOVERY_ENDED_OPTIONS_RULE), "{ended}");
    assert!(!ended.contains(RECOVERY_LIVE_OPTIONS_RULE), "{ended}");
    assert!(
        ended.contains(r#""options": [{"kind": string, "text": string}, ...]"#),
        "{ended}"
    );
    for part in [
        "\"recover\" is an answer that comes back to you: choosing it moves nothing",
        "the runtime cannot mark a task done or cancel it from your option",
        "the runtime itself offers the person `edit the task's --verify, then retry_inherit`",
        "\"hand_off\" is a recover option that asks your next round to hand the run to the check that the base already meets the acceptance",
    ] {
        assert!(RECOVERY_ENDED_OPTIONS_RULE.contains(part), "{part}");
    }
    let live = prompt(
        &run_on(Provider::Claude, WorkerMode::Headless),
        RecoveryAlert::IdleProcess,
        None,
    );
    assert!(live.contains(RECOVERY_LIVE_OPTIONS_RULE), "{live}");
    assert!(!live.contains(RECOVERY_ENDED_OPTIONS_RULE), "{live}");
    assert!(RECOVERY_LIVE_OPTIONS_RULE.contains(
        "choosing it moves nothing and sends nothing to the session, and your next job for the alert runs with the person's choice"
    ));
}

/// A replacement plan past the section's limit is cut and named to read
/// with `dagq replans ID`, and the checks of its meaning stay whole after
/// it (ADR-t1566-1 decision 2).
#[test]
fn a_replacement_section_past_its_limit_keeps_its_checks_whole() {
    use crate::domain::replan_plan::{
        CarrySource, ItemDisposition, PlanChild, PlanChoice, PlanMapping, PlanRevision,
        ReplacementPlan, ReplanBrief,
    };
    use crate::domain::task_replan::{
        Diagnosis, OriginalConditions, ReplanOperation, ReplanOrigin, ReplanStatus,
        acceptance_items,
    };
    use crate::domain::{GoalId, ReplanId};
    let conditions = OriginalConditions {
        title: "big".into(),
        description: "d".into(),
        acceptance: acceptance_items("the API behaves\nthe operations are documented"),
        paths: vec!["src/**".into()],
        verify: vec!["cargo test".into()],
        evidence: json!([]),
        predecessors: Vec::new(),
        successors: Vec::new(),
        goal: None,
    };
    let operation = ReplanOperation {
        id: ReplanId::new(7),
        task_id: TaskId::new(10),
        run_id: RunId::new(RUN).unwrap(),
        goal_id: Some(GoalId::new(3)),
        origin: ReplanOrigin::Manual,
        status: ReplanStatus::Reviewing,
        status_reason: None,
        generation: 2,
        return_phase: None,
        reason: "too big".into(),
        task_version: conditions.version(),
        conditions,
        goal_acceptance_version: Some(4),
        diagnosis: Diagnosis::default(),
        snapshot_id: Some("s".into()),
        request_id: None,
        request_status: None,
        proposal_id: Some(ProposalId::new(1)),
        requested_by: "inbox".into(),
        requested_by_id: "inbox".into(),
        mappings: Vec::new(),
        replaced_by: Vec::new(),
        created_at: 0,
        updated_at: 0,
        closed_at: None,
    };
    let plan = ReplacementPlan {
        operation_id: operation.id,
        task_version: operation.task_version.clone(),
        goal_acceptance_version: Some(4),
        choice: PlanChoice::Split,
        rationale: "why ".repeat(15_000),
        children: vec![PlanChild {
            task_id: TaskId::new(11),
            carry: CarrySource::SnapshotHead,
            order: Some(1),
        }],
        mappings: vec![PlanMapping {
            ordinal: 1,
            hash: operation.conditions.acceptance[0].hash.clone(),
            disposition: ItemDisposition::Split,
            children: Vec::new(),
            landed_task_id: None,
            authority_ask_id: None,
            reason: None,
        }],
        successors: Vec::new(),
    };
    let brief = ReplanBrief {
        turns: Vec::new(),
        reviews: Vec::new(),
        successors: Vec::new(),
        plan: Some(PlanRevision {
            revision: 1,
            proposal_id: ProposalId::new(1),
            plan,
            hash: "h".into(),
            approved_at: None,
        }),
        problems: Vec::new(),
        operation,
    };
    let proposal = Proposal::restore(crate::domain::ProposalRecord {
        id: ProposalId::new(1),
        status: crate::domain::ProposalStatus::Submitted,
        owner: crate::domain::PlannerOwner {
            origin: crate::domain::PlannerOrigin::Runtime,
            workspace_id: None,
        },
        submitted_at: "now".into(),
        revise_count: 0,
        task_ids: vec![TaskId::new(11)],
        goal_ids: Vec::new(),
        created_at: String::new(),
        updated_at: String::new(),
    })
    .unwrap();
    let section = plan_review::replacement_section(&PlanReviewMaterial {
        proposal: &proposal,
        tasks: &[],
        goals: &[],
        lint: &[],
        others: &[],
        queued: &[],
        queued_left_out: 0,
        expected: &BTreeMap::new(),
        precedents: &[],
        hotspots: &[],
        candidates: &[],
        repo_root: Path::new("/repo"),
        language: None,
        replacement: Some((&brief, None)),
    });
    let header = "\n\nThe replacement of a parked task:\n";
    assert!(section.starts_with(header), "{section}");
    assert!(section.len() <= header.len() + PLAN_REVIEW_REPLACEMENT_BYTES);
    assert!(
        section.ends_with(&format!("\n{}", plan_review::REPLACEMENT_CHECKS)),
        "{}",
        &section[section.len() - 2_000..]
    );
    assert!(section.contains("read it with `dagq replans 7`"));
    assert!(section.contains("the operations are documented"));
    assert_reads_are_the_jobs(&section);
}
