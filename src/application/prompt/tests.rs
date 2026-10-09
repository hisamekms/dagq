use super::*;
use crate::application::memory_files::MemoryFiles;
use crate::domain::worker::WorkerMode;
use crate::domain::{
    EvidenceCheck, GoalRecord, GoalStatus, GoalVerdict, Provider, RunId, RunRecord, TaskRecord,
    TaskStatus,
};
use serde_json::json;
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
    })
    .unwrap()
}

fn run(task_id: i64, status: RunStatus, result_commit: Option<&str>) -> TaskRun {
    TaskRun::restore(RunRecord {
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
    })
    .unwrap()
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
    let alone = prompt(&waiting, &own_run, None, &[], &[], &[], None, &[])
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
        let text = prompt(&open, worker, None, &[], &[], &[], None, &e2e)
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
    let none = prompt(&open, &own_run, None, &[], &[], &[], None, &[])
        .unwrap()
        .text;
    assert!(!none.contains("E2E"), "{none}");
    let required = task_with_evidence(8, vec![EvidenceCheck::E2e]);
    let text = prompt(&required, &own_run, None, &[], &[], &[], None, &[])
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

/// Goal 52 acceptance (3): what the runtime tells a worker of building,
/// temporary files and the e2e names no build tool, build directory or
/// e2e file of one repository, on either provider and in every text the
/// session is sent; the Codex worker still keeps its temporary files
/// under `$TMPDIR`, and the e2e stays the runtime's to run, reproduced by
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
            prompt(&open, &worker, None, &[], &[], &[], None, &e2e)
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
        if provider == Provider::Codex {
            assert!(first.contains("throwaway repositories"), "{first}");
            assert!(first.contains("under $TMPDIR (a directory the runtime made for this run"));
            assert!(first.contains("never directly in /tmp or /private/tmp"));
        } else {
            assert!(!first.contains("$TMPDIR"), "{first}");
        }
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

/// The worker, resume and revise prompts show the verification commands
/// as integrate's to run and send the session to the repository's own
/// instructions for its checks, with the verification commands as the
/// default (task 510).
#[test]
fn sessions_run_the_repository_checks_and_leave_the_verification_to_integrate() {
    let verified = verified_task(7, "work", TaskStatus::InProgress, vec!["make gate".into()]);
    let own_run = run(7, RunStatus::Claimed, None);
    let checks = "the repository's instructions (AGENTS.md or CLAUDE.md) ask a worker to run";

    let worker = prompt(&verified, &own_run, None, &[], &[], &[], None, &[])
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
    )
    .unwrap()
    .text;
    let (_, carried) = retried.split_once("Carried over from run").unwrap();
    assert!(carried.contains(
        "inbox carried its work over by hand after it ended (the person chose retry_inherit)"
    ));
    assert!(!carried.contains("its landing kept conflicting"));
    assert!(carried.contains("rerun your checks in the worktree as above"));

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

    let revise = revise_request(&verified, &own_run, 1, &["fix it".into()], None)
        .unwrap()
        .text;
    assert!(revise.contains(&format!("2. {}", local_checks(r#"["make gate"]"#))));
    assert!(revise.contains(default), "{revise}");
}

/// A task as plan review reads it, with long fields of the size of this
/// queue's (about 3 KB, task 591's measure of prompt 157).
fn long_task(id: i64, status: TaskStatus, paths: &[&str]) -> Task {
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
        duplicates: Vec::new(),
        runs: Vec::new(),
        events: Vec::new(),
        processes: Vec::new(),
        origin: None,
        follow_up_drafts: Vec::new(),
        revisit: None,
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
            "destination_goal_id": 10, "acceptance_version": 1,
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
        sized_task(1000, TaskStatus::Submitted, 184_000),
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
    assert!(prompt.bytes.sections["tasks"] > 184_000);
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
            headless_provider_line(provider),
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
            prompt(task, &run, None, &[], &[], &[], None, &[])
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
        prompt(task, run, None, &[], &[], &[], None, &[])
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
    ] {
        let request = ResumeRequest {
            main: CommitSha::try_from(SHA).unwrap(),
            branch: "main".into(),
            reason: "why".into(),
            kind,
            reason_file: None,
        };
        texts.push(resume_request(task, run, &request, &[]).unwrap().text);
    }
    texts.push(
        revise_request(task, run, 1, &["fix it".into()], None)
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
    })
    .unwrap();
    for provider in [Provider::Claude, Provider::Codex] {
        let run = run_on(provider, WorkerMode::Headless);
        let texts = session_texts(&task, &run);
        let first = &texts[0];
        assert!(first.contains(HEADLESS_WORKER), "{first}");
        assert!(first.contains(headless_provider_line(provider)), "{first}");
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
        for request in &texts[1..13] {
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
        assert!(texts[16].ends_with(HEADLESS_GO_ON));
        assert!(texts[13].starts_with(HEADLESS_NEXT_TURN));
        assert!(texts[13].contains("dagq: the previous turn of run"));
        assert!(texts[14].starts_with("answer to ask 3: blue\n\n"));
        assert!(texts[14].ends_with(HEADLESS_GO_ON));
        assert!(texts[15].starts_with("dagq: the supervisor's recovery job for run"));
        assert!(texts[15].ends_with(HEADLESS_GO_ON));
    }
    // Codex reviews its own diff and does not owe subagent_review.
    let codex = session_texts(&task, &run_on(Provider::Codex, WorkerMode::Headless));
    assert!(codex[0].contains("You have no subagent to review your change"));
    // No worker backs e2e (ADR-t1233-2).
    assert!(!codex[0].contains("Required evidence"), "{}", codex[0]);
    let claude = session_texts(&task, &run_on(Provider::Claude, WorkerMode::Headless));
    assert!(claude[0].contains("and subagent review."));
    assert!(claude[0].contains("Required evidence: subagent_review (each"));
    // Only a Codex worker is told where its temporary files go (task
    // 1290): Claude Code's scratchpad is cleaned by task 1100's sweep.
    let tmp = "under $TMPDIR (a directory the runtime made for this run";
    assert!(codex[0].contains(tmp), "{}", codex[0]);
    assert!(codex[0].contains("never directly in /tmp or /private/tmp"));
    assert!(codex[0].contains("the repository's instructions (AGENTS.md or CLAUDE.md) to say"));
    assert!(!claude[0].contains("$TMPDIR"), "{}", claude[0]);
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
        // The nine resume requests and the revise request.
        for request in &texts[1..11] {
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
    )
    .text;
    assert!(!review.contains(ACCEPTANCE_MAP) && !review.contains(ACCEPTANCE_REMAP));
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
        for request in &texts[1..11] {
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
        for request in &texts[1..11] {
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
    )
    .text;
    assert!(!review.contains(DOCS_CHECK));
    assert!(!review.contains("the documents you checked against the diff"));
}

/// Task 1429 (ADR-t1428-1 decision 5): the run's review reads the
/// documents on the changed behavior against the diff and the summary,
/// once, before the acceptance; the verdict's JSON, the concern's
/// recommendation and the reason codes stay as they were.
#[test]
fn the_review_prompt_checks_the_documents_and_keeps_its_verdict() {
    let task = verified_task(7, "work", TaskStatus::InProgress, vec!["make gate".into()]);
    let review = review_prompt(
        &task,
        &run_on(Provider::Codex, WorkerMode::Headless),
        "r",
        None,
    )
    .text;
    assert_eq!(review.matches(REVIEW_DOCS_CHECK).count(), 1, "{review}");
    assert!(review.contains(&format!(
            "Do not change any file.\n{REVIEW_RULES}\n{REVIEW_DOCS_CHECK}\nAcceptance criteria of the task:"
        )));
    for part in [
        "named by the task's description or context or by the summary",
        "A document's diff alone does not show the change is right",
        "check the summary's reason for leaving a document as it is",
        "whether or not the task named it",
        "a changed flow, boundary, invariant or promise it misses",
        // Both directions (ADR-t1942-2 decision 4): a missing name alone
        // is not stale, and what the diff adds that copies the code or
        // carries history is reported too.
        "a name missing from a document alone is not stale",
        "Also report text the diff adds to a document when that text copies the code (lists of fields, flags, defaults, function or test names)",
        "or carries history (task numbers",
    ] {
        assert!(REVIEW_DOCS_CHECK.contains(part), "{part}");
    }
    for word in ["design", "concept", "docs/", "budget", "KiB", "byte"] {
        assert!(!REVIEW_DOCS_CHECK.contains(word), "{word}");
    }
    assert!(
        REVIEW_DOCS_CHECK.len() <= 690,
        "{}",
        REVIEW_DOCS_CHECK.len()
    );
    assert!(!REVIEW_DOCS_CHECK.contains("cargo") && !REVIEW_DOCS_CHECK.contains('`'));
    // Unchanged: the verdict's shape, the concern's recommendation and
    // the reason codes with their definitions.
    assert!(review.contains(
            "Answer with one JSON object and nothing else, matching this schema:\n\
             {\"verdict\": \"pass\" | \"revise\" | \"concern\", \"reasons\": [{\"text\": string, \"codes\": [string]}], \"summary\": string, \"recommendation\": \"land\" | \"send_back\" | null, \"confidence\": \"high\" | \"low\" | null, \"reason_category\": \"scope\" | \"discard\" | null}\n\
             reasons lists each finding (empty for pass); summary is one or two sentences; recommendation, confidence and reason_category are for a concern only (null for pass and revise).\n"
        ));
    assert!(review.contains(&format!(
        "{CONCERN_RECOMMENDATION}{}Answer with one JSON object",
        reason_codes_section(review_reason::REVIEW_CODES)
    )));
    assert!(
        CONCERN_RECOMMENDATION
            .starts_with("For a concern, also recommend what to do, and how sure you are:\n")
    );
    assert!(CONCERN_RECOMMENDATION.ends_with(
            "Leave scope and discard to the person rather than deciding them; when in doubt, say low.\n\n"
        ));
    assert!(!CONCERN_RECOMMENDATION.contains("document"));
    assert!(!reason_codes_section(review_reason::REVIEW_CODES).contains(REVIEW_DOCS_CHECK));
}

/// ADR-t1470-1 decision 2: a Claude review loads no setting sources,
/// so the worktree's CLAUDE.md is not its memory; the review prompt
/// names the repository's instructions to read instead, right after
/// the material and before the check of the documents (task 1429),
/// and says nothing else of a provider's settings.
#[test]
fn the_review_prompt_names_the_repositorys_instructions() {
    let task = verified_task(7, "work", TaskStatus::InProgress, vec!["make gate".into()]);
    let review = review_prompt(
        &task,
        &run_on(Provider::Claude, WorkerMode::Headless),
        "r",
        None,
    )
    .text;
    let material = "Read the worktree if you need more. Do not change any file.\n";
    assert!(
        review.contains(&format!("{material}{REVIEW_RULES}\n{REVIEW_DOCS_CHECK}")),
        "{review}"
    );
    assert!(REVIEW_RULES.contains("AGENTS.md and CLAUDE.md"));
    assert!(!REVIEW_RULES.contains(".claude") && !REVIEW_RULES.contains('`'));
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
    )
    .unwrap()
    .text;
    let alone_task = task(2, "alone", TaskStatus::InProgress);
    let alone = prompt(&alone_task, &own_run, None, &[], &[], &[], None, &[])
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

/// The run review of a task whose title, acceptance and required
/// subagents are huge stays within [`RUN_REVIEW_PROMPT_LIMIT`] and
/// names where in the review material the acceptance is whole.
#[test]
fn a_run_review_prompt_of_a_huge_acceptance_stays_within_its_limits() {
    let task = big_task(7, 100_000);
    let subagents = format!(
        "\nRequired review subagents: ... in /runs/run/review-subagents-1.json\n{}{}",
        big("- agent (changed: src/a.rs)\n", 50_000),
        super::super::review::SUBAGENTS_INSTRUCTION
    );
    let fitted = review_prompt(
        &task,
        &run_on(Provider::Claude, WorkerMode::Headless),
        "/runs/run/review.md",
        Some(&subagents),
    );
    within(&fitted, RUN_REVIEW_PROMPT_LIMIT);
    let bytes = &fitted.bytes;
    for section in ["title", "acceptance", "subagents"] {
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
    assert!(fitted.text.contains("in /runs/run/review-subagents-1.json"));
    assert!(
        fitted
            .text
            .contains("every agent and the paths that selected it are in the file the list names")
    );
    // How to run the agents and report them is never cut.
    assert!(
        fitted
            .text
            .ends_with(super::super::review::SUBAGENTS_INSTRUCTION)
    );
    // A small task is not cut.
    let small = review_prompt(
        &task_with_evidence(7, Vec::new()),
        &run_on(Provider::Claude, WorkerMode::Headless),
        "r",
        None,
    );
    assert!(
        small.bytes.omitted.is_empty() && small.bytes.over_limit.is_none(),
        "{:?}",
        small.bytes
    );
    within(&small, RUN_REVIEW_PROMPT_LIMIT);
}

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
        &updates,
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
    let facts = binary_facts("0.1.0", &run, &events, &updates, &landings, &holds);
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
    let facts = binary_facts("0.1.0", &run, &[], &updates, &[], &holds);
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
            handover: Some(&handover),
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
            handover: Some(&handover),
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
    })
    .unwrap();
    within(&fitted, REQUEST_PLANNER_PROMPT_LIMIT);
    sections_within(&fitted);
    let (text, bytes) = (&fitted.text, &fitted.bytes);
    for section in ["note", "refs", "goals", "asks", "answer"] {
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
    assert!(text.ends_with(headless_provider_line(Provider::Claude)));
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
    })
    .unwrap()
}

const RESUME_KINDS: [ResumeKind; 9] = [
    ResumeKind::Landing,
    ResumeKind::EvidenceMissing,
    ResumeKind::SentBack,
    ResumeKind::ScopeViolation,
    ResumeKind::Precheck,
    ResumeKind::Triage,
    ResumeKind::Recheck,
    ResumeKind::SessionGone,
    ResumeKind::E2e,
];

/// The bytes a next-turn message held to `limit` may take before the
/// language's instruction is added.
fn room(limit: usize) -> usize {
    limit - prompt_fit::LANGUAGE_ROOM
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
        let fitted = revise_request(&task, &run, 1, &reasons, Some("/runs/run/f.txt")).unwrap();
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
            let fitted = revise_request(&task, &run, 2, reasons, Some(file)).unwrap();
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
    let text = revise_request(&task, &run, 1, &huge, None).unwrap().text;
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
    let bytes = revise_request(&task, &run, 1, &["x".repeat(50_000)], None)
        .unwrap()
        .bytes;
    let read: PromptBytes = serde_json::from_value(json!(bytes)).unwrap();
    assert_eq!(read, bytes);
}

/// ADR-t2080-1 decision 2: the handoff prompt of a new session carries
/// why it is new, the commits, the uncommitted changes, the review's
/// reasons, the receipt's summary and the request it would have been
/// sent, whole when they fit, and nothing else (no transcript); with
/// the task's prompt before it, it is the new session's first turn.
#[test]
fn a_handoff_carries_the_work_the_review_the_receipt_and_the_request() {
    let run = run_on(Provider::Codex, WorkerMode::Headless);
    let findings = Path::new("/runs/run/turns/revise-findings.txt");
    let material = HandoffMaterial {
        peak_context: 200_001,
        threshold: 200_000,
        commits: "abc1234 runtime: the fix\nabc1233 runtime: the start\n",
        changes: " M src/lib.rs\n?? notes.txt\n",
        review: Some(("- the test misses the boundary", Some(findings))),
        receipt_summary: Some("did the fix"),
        request: "dagq: the review asks you to revise.",
        request_file: None,
    };
    let fitted = handoff_text(&run, &material);
    assert_eq!(
        fitted.text,
        format!(
            "dagq: this run's worker goes on in a new session: its previous turn's context took 200001 tokens, above the 200000 tokens this repository sets, so nothing of the earlier conversation carries over. The task's prompt is above; the run, its worktree, its branch and its provider are the same. The work so far is in this worktree and its branch: go on from it rather than starting over.\n\nCommits since the base {SHA}, newest first:\nabc1234 runtime: the fix\nabc1233 runtime: the start\n\nUncommitted changes in the worktree (`git status --porcelain`):\n M src/lib.rs\n?? notes.txt\n\nWhy the review sent the run back:\n- the test misses the boundary\n\nThe summary of the receipt you wrote last:\ndid the fix\n\nThe earlier session would have been sent this next; it is yours now:\n\ndagq: the review asks you to revise."
        )
    );
    assert!(fitted.bytes.omitted.is_empty(), "{:?}", fitted.bytes);
    assert_eq!(fitted.bytes.over_limit, None);
    assert_eq!(fitted.bytes.limit, HANDOFF_LIMIT);
    assert_eq!(fitted.bytes.total, fitted.text.len());
    for section in ["commits", "changes", "review", "receipt", "request"] {
        assert!(fitted.bytes.sections[section] > 0, "{section}");
    }
    // A clean worktree with no commit, no review and no receipt says
    // so, and still carries the request.
    let bare = handoff_text(
        &run,
        &HandoffMaterial {
            commits: "",
            changes: "",
            review: None,
            receipt_summary: None,
            ..material
        },
    );
    assert!(bare.text.contains("newest first:\n(none)\n\nUncommitted"));
    assert!(bare.text.contains("--porcelain`):\n(none)\n\nThe earlier"));
    assert!(!bare.text.contains("Why the review"));
    assert!(!bare.text.contains("The summary of the receipt"));
    assert!(bare.text.ends_with("dagq: the review asks you to revise."));
    // The first turn of the new session: the task's prompt, then this.
    let first = crate::domain::turn::new_session_prompt("the task's prompt", &fitted.text);
    assert!(first.starts_with("the task's prompt\n\ndagq: this run's worker goes on"));
}

/// ADR-t2080-1 decision 2 within ADR-t2072-1's limits: with the largest
/// material each section is cut to its bytes keeping its start, says
/// how many bytes it left out and where to read them, and the whole
/// stays within `HANDOFF_LIMIT` without its middle cut; the request's
/// cut is said in `over_limit`.
#[test]
fn a_handoff_with_the_largest_material_stays_within_its_limits() {
    let run = run_on(Provider::Claude, WorkerMode::Headless);
    let huge = "引".repeat(300_000);
    let findings = Path::new("/runs/run/turns/revise-findings.txt");
    let request_file = Path::new("/runs/run/turns/request-9.json");
    let material = HandoffMaterial {
        peak_context: u64::MAX,
        threshold: u64::MAX - 1,
        commits: &huge,
        changes: &huge,
        review: Some((&huge, Some(findings))),
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
            "review",
            HANDOFF_REVIEW_BYTES,
            format!("the whole of it is in {}", findings.display()),
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
