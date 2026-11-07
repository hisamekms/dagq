//! Production verdict jobs publish replay inputs before their providers start.
use super::{
    agent::{Script, succeeded, verdict},
    harness::Wiring,
};
use crate::application::role_snapshot::{Input, Snapshot};
use crate::application::{RepositoryReads, RunFiles, TaskReads, TaskStore};
use crate::domain::{NewGoal, NewTask, PlannerOrigin, PlannerOwner, Submission, TaskStatus};
use serde_json::json;
use std::path::{Path, PathBuf};

fn draft(queue: &mut impl TaskStore, title: &str) -> crate::domain::TaskId {
    queue
        .add(NewTask {
            title: title.into(),
            description: "small".into(),
            acceptance: "works".into(),
            verification_commands: vec!["gate".into()],
            required_evidence: vec![],
            paths: vec![],
            priority: Default::default(),
            change: None,
            dependencies: vec![],
            goal_dependencies: vec![],
            goal_id: None,
            context: String::new(),
            provider: None,
            worker_mode: None,
            wait_for_build: false,
            execution_class: Default::default(),
        })
        .unwrap()
        .id()
}
fn read(w: &Wiring, path: &Path) -> Snapshot {
    serde_json::from_slice(&w.files.read(path).unwrap()).unwrap()
}
fn snapshots(w: &Wiring, dir: &str) -> Vec<PathBuf> {
    w.files
        .read_dir(&w.db.parent().unwrap().join(dir))
        .unwrap()
        .into_iter()
        .filter_map(|dir| {
            let path = dir.join("input-snapshot.json");
            w.files.is_file(&path).then_some(path)
        })
        .collect()
}

fn check_plan_record(fail: bool) {
    let w = Wiring::new(
        if fail {
            "plan-snapshot-failure"
        } else {
            "plan-snapshot"
        },
        Script {
            receipts: vec![],
            verdicts: vec![json!({"verdict": "pass", "reasons": [], "summary": "sound"})],
        },
    );
    if fail {
        w.files.reject_writes_ending_with("input-snapshot.json.tmp");
    }
    let mut queue = w.queue();
    let blocker = draft(&mut queue, "blocker");
    let task = draft(&mut queue, "planned");
    queue.add_dependency(task, blocker).unwrap();
    queue
        .submit(Submission {
            tasks: vec![task],
            goals: vec![],
            proposal: None,
            owner: PlannerOwner {
                origin: PlannerOrigin::Person,
                workspace_id: None,
            },
        })
        .unwrap();
    let main = w.git.main_head().unwrap().to_string();
    w.supervise_once();
    assert_eq!(
        w.queue().show(task).unwrap().task.status(),
        TaskStatus::Ready
    );
    let paths = snapshots(&w, "plan-reviews");
    assert_eq!(paths.len(), usize::from(!fail));
    if !fail {
        let snapshot = read(&w, &paths[0]);
        assert_eq!(snapshot.main_commit, main);
        let Input::Plan(input) = snapshot.input else {
            panic!("plan")
        };
        let prompt = input.build().unwrap().text;
        assert_eq!(
            prompt,
            w.files
                .read_to_string(&paths[0].parent().unwrap().join("prompt.txt"))
                .unwrap()
        );
        assert!(w.agent.recovery_prompts().iter().any(|(p, _)| p == &prompt));
    }
}

fn check_goal_record(fail: bool) {
    let w = Wiring::new(
        if fail {
            "goal-snapshot-failure"
        } else {
            "goal-snapshot"
        },
        Script {
            receipts: vec![succeeded],
            verdicts: vec![
                verdict("pass", "landed"),
                json!({"verdict": "achieved", "criteria": [], "summary": "done"}),
            ],
        },
    );
    if fail {
        w.files.reject_writes_ending_with("input-snapshot.json.tmp");
    }
    let mut q = w.queue();
    let goal = q
        .add_goal(NewGoal {
            title: "goal".into(),
            description: "purpose".into(),
            acceptance: "done".into(),
            constraints: String::new(),
            doc: None,
            draft: false,
            tags: vec![],
            priority: Default::default(),
        })
        .unwrap()
        .id();
    let task = draft(&mut q, "completed work");
    q.set_goal(task, Some(goal)).unwrap();
    q.transition(task, crate::domain::TaskAction::BypassReview)
        .unwrap();
    w.supervise_once();
    assert!(
        w.queue()
            .show_goal(goal)
            .unwrap()
            .goal
            .closed_at()
            .is_some()
    );
    let paths = snapshots(&w, "goal-reviews");
    assert_eq!(paths.len(), usize::from(!fail));
    if !fail {
        let snapshot = read(&w, &paths[0]);
        assert_eq!(snapshot.main_commit, w.git.main_head().unwrap().to_string());
        let Input::Goal(input) = snapshot.input else {
            panic!("goal")
        };
        let prompt = input.build().text;
        assert_eq!(
            prompt,
            w.files
                .read_to_string(&paths[0].parent().unwrap().join("prompt.txt"))
                .unwrap()
        );
        assert!(w.agent.recovery_prompts().iter().any(|(p, _)| p == &prompt));
    }
}

fn check_run_record(fail: bool) {
    let w = Wiring::new(
        if fail {
            "run-snapshot-failure"
        } else {
            "run-snapshot"
        },
        Script {
            receipts: vec![succeeded, succeeded],
            verdicts: vec![verdict("revise", "fix this"), verdict("pass", "done")],
        },
    );
    w.git.commit_files(
        &[(
            ".dagq/review/builtin-acceptance.md",
            "project rules in full",
        )],
        &[],
    );
    if fail {
        w.files.reject_writes_ending_with("-snapshot.json.tmp");
    }
    let task = w.ready_task(&["gate"]);
    let main = w.git.main_head().unwrap().to_string();
    w.supervise_once();
    let detail = w.queue().show(task).unwrap();
    assert_eq!(detail.task.status(), TaskStatus::Completed);
    assert_eq!(w.agent.review_prompts().len(), 2);
    let dir = Path::new(detail.runs[0].run_dir().unwrap());
    for attempt in 1..=2 {
        let path = dir.join(format!("review-{attempt}-snapshot.json"));
        assert_eq!(w.files.is_file(&path), !fail);
        if fail {
            continue;
        }
        let snapshot = read(&w, &path);
        assert_eq!(snapshot.main_commit, main);
        let change = snapshot.change.as_ref().unwrap();
        assert_eq!(change.head, change.tree_commit);
        assert_eq!(change.tree, format!("tree-{}", change.head));
        assert_ne!(change.base, change.head);
        assert!(!w.files.read(&dir.join(&change.patch)).unwrap().is_empty());
        let Input::Run(input) = &snapshot.input else {
            panic!("run")
        };
        assert_eq!(input.build().text, w.agent.review_prompts()[attempt - 1]);
        assert_eq!(
            input.build().text,
            w.files
                .read_to_string(&dir.join(format!("review-prompt-{attempt}.txt")))
                .unwrap()
        );
        let material = snapshot
            .materials
            .iter()
            .find(|m| m.at == Path::new("review.md"))
            .unwrap();
        assert!(String::from_utf8_lossy(&material.bytes).contains(&change.head));
        assert_eq!(
            snapshot
                .materials
                .iter()
                .find(|m| m.at == Path::new("builtin-acceptance.md"))
                .unwrap()
                .bytes,
            b"project rules in full"
        );
        // The shared review.md has already been replaced by attempt 2.
        let replay = snapshot
            .for_case(
                &*w.files,
                Path::new("/case/material"),
                Path::new("/case/tree"),
            )
            .unwrap();
        assert!(replay.build().unwrap().contains("/case/material/review.md"));
        assert_eq!(
            w.files.read(Path::new("/case/material/review.md")).unwrap(),
            material.bytes
        );
    }
}

#[test]
fn plan_job_records_its_inputs_before_starting() {
    check_plan_record(false);
}
#[test]
fn plan_job_starts_after_a_snapshot_write_failure() {
    check_plan_record(true);
}
#[test]
fn goal_job_records_its_inputs_before_starting() {
    check_goal_record(false);
}
#[test]
fn goal_job_starts_after_a_snapshot_write_failure() {
    check_goal_record(true);
}
#[test]
fn whole_run_review_records_each_attempts_material_and_change() {
    check_run_record(false);
}
#[test]
fn whole_run_review_starts_after_a_snapshot_write_failure() {
    check_run_record(true);
}
