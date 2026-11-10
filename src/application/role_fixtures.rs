//! Recorded verdict-role snapshots that the role harness's tests and the
//! compose wiring tests both start from.
use super::role_snapshot::{
    Input, Material, PlanInput, ReviewChange, RulesInput, RunInput, Snapshot, Whole,
};
use crate::domain::{
    CommitSha, Provider, RunId, RunRecord, RunStatus, Task, TaskId, TaskRecord, TaskRun,
    TaskStatus, worker::WorkerMode,
};
use serde_json::json;

/// A plan review recorded at `main`, its repository at a production path.
pub(crate) fn plan_snapshot(main: &str) -> Snapshot {
    Snapshot {
        main_commit: main.into(),
        change: None,
        materials: vec![],
        input: Input::Plan(Box::new(PlanInput {
            proposal: serde_json::from_value(json!({
                "id":1,"status":"submitted","owner":{"origin":"person","workspace_id":null},
                "submitted_at":"now","revise_count":0,"task_ids":[],"goal_ids":[],"created_at":"","updated_at":""
            }))
            .unwrap(),
            tasks: Whole(vec![]),
            goals: Whole(vec![]),
            lint: vec![],
            others: vec![],
            queued: vec![],
            queued_left_out: 0,
            expected: Default::default(),
            precedents: vec![],
            hotspots: vec![],
            candidates: vec![],
            repo_root: "/production/repository".into(),
            language: None,
            replacement: None,
        })),
    }
}

/// Task 7's run `id` as the production run directory knows it.
pub(crate) fn run(id: &str) -> TaskRun {
    TaskRun::restore(RunRecord {
        id: RunId::new(id).unwrap(),
        task_id: TaskId::new(7),
        status: RunStatus::Claimed,
        requested_provider: Provider::Codex,
        actual_provider: Provider::Codex,
        worker_mode: WorkerMode::Headless,
        base_commit: CommitSha::try_from("1".repeat(40).as_str()).unwrap(),
        branch: Some(format!("dagq/{id}")),
        worktree_path: Some("/production/runs/run/worktree".into()),
        workspace_id: None,
        receipt_path: Some("/production/runs/run/receipt.json".into()),
        log_path: None,
        result_commit: None,
        repo_path: None,
        run_dir: Some("/production/runs/run".into()),
        last_error: None,
        workspace_closed_at: None,
        created_at: String::new(),
    })
    .unwrap()
}

/// A recorded whole run review: its range from `base`, its job's tree
/// `tree`, review.md and the whole rules saved, known failures none.
pub(crate) fn run_snapshot(base: &str, tree: Option<&str>) -> Snapshot {
    let task = Task::restore(TaskRecord {
        goal_priority: None,
        id: TaskId::new(7),
        title: "work".into(),
        description: String::new(),
        acceptance: "it works".into(),
        verification_commands: vec!["make gate".into()],
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
        execution_class: Default::default(),
    })
    .unwrap();
    Snapshot {
        main_commit: "c".repeat(40),
        change: Some(ReviewChange {
            base: base.into(),
            head: "b".repeat(40),
            tree_commit: "b".repeat(40),
            tree: tree.map(Into::into),
            patch: "review-1-change.patch".into(),
        }),
        materials: vec![
            Material {
                source: "/production/run/review.md".into(),
                at: "review.md".into(),
                bytes: b"the material\0\xff".to_vec(),
            },
            Material {
                source: "/production/run/review-1-builtin-acceptance.md".into(),
                at: "builtin-acceptance.md".into(),
                bytes: b"rules".to_vec(),
            },
        ],
        input: Input::Run(Box::new(RunInput {
            task: Whole(task),
            run: run("00000000-0000-4000-8000-000000000001"),
            review_path: "/production/run/review.md".into(),
            known_failures: None,
            rules: Some(RulesInput {
                path: ".dagq/review/builtin-acceptance.md".into(),
                commit: "c".repeat(40),
                text: "rules".into(),
                whole_at: "/production/run/review-1-builtin-acceptance.md".into(),
            }),
            answers: Default::default(),
            language: None,
        })),
    }
}
