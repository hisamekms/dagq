//! A person's hold of a task whose run's turn goes, and its release
//! (ADR-t1879-1), wired from the hold to the supervisor's next pass: the
//! hold settles once the turn ends and the run waits for a session, a
//! release while it settles is refused and changes nothing, and a release
//! once it waits either resumes the run in its session through the
//! resume's entry (in a new session of the same run past
//! `[fresh_session]`'s threshold) or ends it for a new run on another
//! provider that carries its branch over. Which way a release goes is
//! `domain::task_hold::release_decision`'s, whether it waits
//! `domain::task_hold::check_release`'s, and which session a resume
//! takes `domain::fresh_session::next_session`'s; these hold the wiring.

use std::{path::Path, sync::mpsc};

use anyhow::Result;
use serde_json::{Value, json};

use super::{
    agent::{Script, committed_no_receipt, no_receipt, succeeded, verdict},
    git::FakeGit,
    harness::Wiring,
};
use crate::{
    application::{
        TaskReads,
        commands::holds::Holds,
        inherit::{ReleasedBranches, ReleasedRead, keep_carried_head, released_work, run_ref},
        memory_files::MemoryFiles,
    },
    domain::{
        ActorContext, CommitSha, Provider, RunStatus, StaticPolicy, TaskDetail, TaskId, TaskRun,
        TaskStatus,
        task_hold::{HoldState, HoldTarget, ReleaseChoice, ReleaseDecision, ReleaseRecorded},
        turn::session_name,
        worker_model,
    },
    infrastructure::sqlite::SqliteQueue,
};

/// The payloads of `detail`'s events of `kind`, oldest first.
fn payloads<'a>(detail: &'a TaskDetail, kind: &str) -> Vec<&'a Value> {
    detail
        .events
        .iter()
        .filter(|event| event.kind.as_str() == kind)
        .map(|event| &event.payload)
        .collect()
}

/// The `retry_inherit` the release that ended `run` recorded on it: one
/// `triage_finished`, by the person (`user`) who released it, which leaves
/// the task's one automatic `retry_inherit` unused
/// ([`crate::domain::resume::uses_automatic_inherit`]), with `head`.
fn assert_inherit_by_the_person(detail: &TaskDetail, run: &TaskRun, head: &CommitSha) {
    let retries: Vec<_> = detail
        .events
        .iter()
        .filter(|event| {
            event.kind.as_str() == "triage_finished"
                && event.run_id.as_ref() == Some(run.id())
                && event.payload["action"] == "retry_inherit"
        })
        .collect();
    let [retry] = retries.as_slice() else {
        panic!("{retries:?}")
    };
    assert_eq!(retry.payload["by"], "user", "{}", retry.payload);
    assert!(!crate::domain::resume::uses_automatic_inherit(retry));
    assert_eq!(
        retry.payload["inherit"]["head"],
        json!(head),
        "{}",
        retry.payload
    );
    assert!(
        !detail
            .events
            .iter()
            .any(crate::domain::resume::uses_automatic_inherit),
        "the task's automatic retry_inherit stays unused"
    );
}

/// Where a release reads a run's work and keeps the head it carries over:
/// the test's repository and files, as the person's `dagq` reads its own.
struct Branches<'a> {
    git: &'a FakeGit,
    files: &'a MemoryFiles,
}

impl ReleasedBranches for Branches<'_> {
    fn released_work(&self, run: &TaskRun) -> Result<ReleasedRead> {
        released_work(run, self.files, self.git, &super::host::Nothing)
    }

    fn keep(&self, run: &TaskRun, head: &CommitSha) -> Result<()> {
        keep_carried_head(self.git, run.id(), head)
    }
}

/// A person's `release` of `task` with `choice` on `queue`.
fn release(
    wiring: &Wiring,
    queue: &mut SqliteQueue,
    task: TaskId,
    choice: ReleaseChoice,
) -> Result<ReleaseRecorded> {
    Holds::new(queue, &ActorContext::user(), &StaticPolicy).release(
        &HoldTarget::Task(task),
        choice,
        &Branches {
            git: &wiring.git,
            files: &wiring.files,
        },
    )
}

/// A release of `task` with `choice` is refused with `why` and changes
/// nothing: the hold stays open as it was, the task keeps its provider,
/// its runs stay as they were and no `task_released` is recorded.
fn release_refused(
    wiring: &Wiring,
    queue: &mut SqliteQueue,
    task: TaskId,
    choice: ReleaseChoice,
    why: &str,
) {
    let runs = |detail: &TaskDetail| {
        detail
            .runs
            .iter()
            .map(|run| (run.id().clone(), run.status()))
            .collect::<Vec<_>>()
    };
    let before = (queue.task_hold(task).unwrap(), queue.show(task).unwrap());
    let refused = release(wiring, queue, task, choice.clone())
        .expect_err(&format!("the release passed: {choice:?}"));
    assert!(refused.to_string().contains(why), "{why} in {refused:#}");
    let after = (queue.task_hold(task).unwrap(), queue.show(task).unwrap());
    assert_eq!(after.0, before.0, "the hold changed");
    assert!(after.0.is_some(), "the hold stays open");
    assert_eq!(after.1.task.worker(), before.1.task.worker());
    assert_eq!(runs(&after.1), runs(&before.1));
    assert!(payloads(&after.1, "task_released").is_empty());
}

/// A ready task whose first turn, held in its process, writes no receipt
/// (the wiring's script says what else it does): a person holds the
/// task while the turn goes, the hold settling, and releases are refused
/// (`hold_not_settled`) until the turn ends; the turn then ends and the
/// run waits with its session. The task and its waiting run.
fn held_after_its_turn(wiring: &Wiring) -> (TaskId, TaskRun) {
    let task = wiring.ready_task(&["true"]);
    let (go, held) = mpsc::channel();
    wiring.agent.hold_next_turn(held);
    let mut go = Some(go);
    wiring.supervise_once_while(|queue| {
        if payloads(&queue.show(task).unwrap(), "turn_started").is_empty() {
            return false;
        }
        let hold = Holds::new(queue, &ActorContext::user(), &StaticPolicy)
            .hold(&HoldTarget::Task(task), "the login expired", None)
            .expect("hold the task");
        assert_eq!(hold.state, HoldState::Settling);
        for choice in [
            ReleaseChoice {
                provider: Some(Provider::Codex),
                model: Some(worker_model::SONNET.into()),
            },
            ReleaseChoice {
                provider: Some(Provider::Codex),
                model: None,
            },
        ] {
            release_refused(wiring, queue, task, choice, "hold_not_settled");
        }
        let _ = go.take().expect("the turn is let go once").send(());
        true
    });
    let waiting = wiring.queue().show(task).unwrap();
    let kinds: Vec<&str> = waiting.events.iter().map(|e| e.kind.as_str()).collect();
    assert_eq!(waiting.runs.len(), 1, "{kinds:?}");
    assert_eq!(
        waiting.runs[0].status(),
        RunStatus::NeedsSession,
        "{kinds:?}"
    );
    assert_eq!(payloads(&waiting, "hold_parked").len(), 1, "{kinds:?}");
    assert_eq!(payloads(&waiting, "turn_finished").len(), 1, "{kinds:?}");
    assert_eq!(waiting.task.worker().provider, Provider::Claude);
    assert_eq!(
        wiring
            .queue()
            .task_hold(task)
            .unwrap()
            .map(|hold| hold.state()),
        Some(HoldState::Held)
    );
    (task, waiting.runs[0].clone())
}

/// The wiring of a test whose first turn does `first` and writes no
/// receipt, whose resumed or new run's turn commits and succeeds, and whose
/// review passes.
fn wiring(name: &str, first: super::agent::Receipt) -> Wiring {
    Wiring::new(
        name,
        Script {
            receipts: vec![first, succeeded],
            verdicts: vec![verdict("pass", "fine")],
        },
    )
}

/// A run held while its turn goes, waiting for a session, released on its
/// provider (a model the provider does not take, or any model with codex,
/// refused and changing nothing; Claude's Sonnet taken) resumes once
/// through the resume's entry in the session it had, with that model, and
/// lands: one run, no new session, the task on Claude.
#[test]
fn a_held_run_released_on_its_provider_resumes_its_session_with_the_model_chosen() {
    let wiring = wiring(
        "a_held_run_released_on_its_provider_resumes_its_session_with_the_model_chosen",
        no_receipt,
    );
    let main = wiring.git.main();
    let (task, run) = held_after_its_turn(&wiring);
    let mut queue = wiring.queue();
    release_refused(
        &wiring,
        &mut queue,
        task,
        ReleaseChoice {
            provider: None,
            model: Some("gpt-9".into()),
        },
        "not one claude takes",
    );
    release_refused(
        &wiring,
        &mut queue,
        task,
        ReleaseChoice {
            provider: Some(Provider::Codex),
            model: Some(worker_model::OPUS.into()),
        },
        "codex runs its own model",
    );
    let released = release(
        &wiring,
        &mut queue,
        task,
        ReleaseChoice {
            provider: None,
            model: Some(worker_model::SONNET.into()),
        },
    )
    .expect("release the task");
    assert_eq!(
        (released.decision, released.continuation),
        (ReleaseDecision::Resume, "continue")
    );
    wiring.supervise_once();

    let detail = wiring.queue().show(task).unwrap();
    let kinds: Vec<&str> = detail.events.iter().map(|e| e.kind.as_str()).collect();
    assert_eq!(detail.task.status(), TaskStatus::Completed, "{kinds:?}");
    assert_eq!(detail.runs.len(), 1, "no new run: {kinds:?}");
    assert_eq!(detail.runs[0].status(), RunStatus::Integrated);
    assert_ne!(wiring.git.main(), main, "main moved");
    let resumed = payloads(&detail, "resume_started");
    assert_eq!(resumed.len(), 1, "{kinds:?}");
    assert_eq!(resumed[0]["model"], worker_model::SONNET);
    let recorded = payloads(&detail, "task_released");
    assert_eq!(recorded.len(), 1);
    assert_eq!(recorded[0]["model"], worker_model::SONNET);
    assert_eq!(recorded[0]["decision"], "resume");
    assert!(payloads(&detail, "session_renewed").is_empty(), "{kinds:?}");
    assert_eq!(detail.task.worker().provider, Provider::Claude);
    let sessions = wiring.agent.turn_sessions();
    assert_eq!(
        sessions,
        [
            format!("start {}", run.id()),
            format!("resume {}", run.id())
        ]
    );
}

/// Past `[fresh_session]`'s threshold (the previous turn's context above
/// it), a run held while its turn goes and released on its provider
/// resumes through the same entry in a new session of the same run,
/// worktree, branch and provider (`session_renewed`), and lands.
#[test]
fn a_held_run_released_past_the_threshold_resumes_in_a_new_session_of_the_same_run() {
    let wiring = wiring(
        "a_held_run_released_past_the_threshold_resumes_in_a_new_session_of_the_same_run",
        no_receipt,
    )
    .with_fresh_session(100);
    let (task, run) = held_after_its_turn(&wiring);
    let mut queue = wiring.queue();
    queue
        .record_runtime_event(
            run.id(),
            crate::domain::EventKind::TurnFinished,
            json!({"turn": 1, "peak_context": 500}),
        )
        .unwrap();
    let released =
        release(&wiring, &mut queue, task, ReleaseChoice::default()).expect("release the task");
    assert_eq!(released.decision, ReleaseDecision::Resume);
    wiring.supervise_once();

    let detail = wiring.queue().show(task).unwrap();
    let kinds: Vec<&str> = detail.events.iter().map(|e| e.kind.as_str()).collect();
    assert_eq!(detail.task.status(), TaskStatus::Completed, "{kinds:?}");
    assert_eq!(detail.runs.len(), 1, "no new run: {kinds:?}");
    let landed = &detail.runs[0];
    assert_eq!(landed.status(), RunStatus::Integrated);
    assert_eq!(
        (landed.worktree_path(), landed.branch()),
        (run.worktree_path(), run.branch())
    );
    assert_eq!(landed.actual_provider(), Provider::Claude);
    assert_eq!(payloads(&detail, "session_renewed").len(), 1, "{kinds:?}");
    assert_eq!(payloads(&detail, "resume_started").len(), 1, "{kinds:?}");
    let renewed = session_name(run.id().as_str(), 1);
    assert_eq!(
        wiring.agent.turn_sessions(),
        [format!("start {}", run.id()), format!("start {renewed}")]
    );
}

/// A run held while its turn goes, its turn having made a commit, and
/// released to Codex once it waits, ends (`retry_inherit`), its head kept
/// under its run's ref, and the task goes to Codex; a new run on Codex
/// carries that head over (`run_inherited`) and lands, with no resume.
#[test]
fn a_held_run_released_to_codex_ends_and_a_new_run_carries_its_commits_over_and_lands() {
    let wiring = wiring(
        "a_held_run_released_to_codex_ends_and_a_new_run_carries_its_commits_over_and_lands",
        committed_no_receipt,
    )
    .with_codex();
    let (task, run) = held_after_its_turn(&wiring);
    let head = wiring
        .git
        .worktree_head(Path::new(run.worktree_path().unwrap()))
        .unwrap();
    assert_ne!(&head, run.base_commit(), "the turn committed");
    let mut queue = wiring.queue();
    let released = release(
        &wiring,
        &mut queue,
        task,
        ReleaseChoice {
            provider: Some(Provider::Codex),
            model: None,
        },
    )
    .expect("release the task");
    assert_eq!(released.decision, ReleaseDecision::RetryInherit);
    assert_eq!(released.provider, Some(Provider::Codex));
    let carried = serde_json::to_value(&released).unwrap()["inherit"]["head"].clone();
    assert_eq!(carried, json!(head));
    assert_eq!(
        crate::application::Repository::ref_value(&wiring.git, &run_ref(run.id())).unwrap(),
        Some(head.clone())
    );
    let ended = wiring.queue().show(task).unwrap();
    assert_eq!(ended.task.status(), TaskStatus::Ready);
    assert_eq!(ended.task.worker().provider, Provider::Codex);
    assert_eq!(ended.runs[0].status(), RunStatus::Failed);
    assert!(
        ended.runs[0]
            .last_error()
            .is_some_and(|error| error.starts_with("released: ")),
        "{:?}",
        ended.runs[0].last_error()
    );
    wiring.supervise_once();

    let detail = wiring.queue().show(task).unwrap();
    let kinds: Vec<&str> = detail.events.iter().map(|e| e.kind.as_str()).collect();
    assert_eq!(detail.task.status(), TaskStatus::Completed, "{kinds:?}");
    assert_eq!(detail.runs.len(), 2, "one new run: {kinds:?}");
    assert_eq!(detail.runs[0].status(), RunStatus::Failed);
    assert_eq!(detail.runs[1].actual_provider(), Provider::Codex);
    assert_eq!(detail.runs[1].status(), RunStatus::Integrated);
    assert!(payloads(&detail, "resume_started").is_empty(), "no resume");
    assert_eq!(payloads(&detail, "run_claimed").len(), 2);
    let inherited = payloads(&detail, "run_inherited");
    assert_eq!(inherited.len(), 1, "{kinds:?}");
    assert_eq!(inherited[0]["head"], json!(head));
    let recorded = payloads(&detail, "task_released");
    assert_eq!(recorded.len(), 1);
    assert_eq!(recorded[0]["decision"], "retry_inherit");
    assert_inherit_by_the_person(&detail, &run, &head);
}

/// A run held while its turn goes, its worktree's head still its base (no
/// commit of its own) and its worktree clean, released to Codex once it
/// waits, ends with nothing to carry over (`retry_fresh`, no work kept
/// under its run's ref, no replacement's handover ended); a new run on
/// Codex starts from main with nothing inherited (no `run_inherited`, no
/// carried-over run in its prompt) and lands.
#[test]
fn a_held_run_with_no_commit_of_its_own_released_to_codex_starts_afresh_from_main() {
    let wiring = wiring(
        "a_held_run_with_no_commit_of_its_own_released_to_codex_starts_afresh_from_main",
        no_receipt,
    )
    .with_codex();
    let (task, run) = held_after_its_turn(&wiring);
    let mut queue = wiring.queue();
    let released = release(
        &wiring,
        &mut queue,
        task,
        ReleaseChoice {
            provider: Some(Provider::Codex),
            model: None,
        },
    )
    .expect("release the task");
    assert_eq!(released.decision, ReleaseDecision::RetryFresh);
    assert!(released.inherit.is_none(), "{released:?}");
    assert!(released.replan_carry_ended.is_none(), "{released:?}");
    assert_eq!(
        crate::application::Repository::ref_value(&wiring.git, &run_ref(run.id())).unwrap(),
        None
    );
    wiring.supervise_once();

    let detail = wiring.queue().show(task).unwrap();
    let kinds: Vec<&str> = detail.events.iter().map(|e| e.kind.as_str()).collect();
    assert_eq!(detail.task.status(), TaskStatus::Completed, "{kinds:?}");
    assert_eq!(detail.runs.len(), 2, "one new run: {kinds:?}");
    let new = &detail.runs[1];
    assert_eq!(new.actual_provider(), Provider::Codex);
    assert_eq!(new.status(), RunStatus::Integrated);
    assert!(payloads(&detail, "run_inherited").is_empty(), "{kinds:?}");
    assert_eq!(
        payloads(&detail, "task_released")[0]["decision"],
        "retry_fresh"
    );
    let prompt = crate::application::RunFiles::read_to_string(
        &*wiring.files,
        &Path::new(new.run_dir().unwrap()).join("prompt.txt"),
    )
    .unwrap();
    assert!(!prompt.contains("Carried over from run"), "{prompt}");
}

/// A task held after its run's wrapper started and before its first turn:
/// the hold settles (a release then is refused with `hold_not_settled` and
/// changes nothing) until the wrapper's fence ends the session with no turn
/// (`turn_withheld`), and the run waits with no session of its provider. A
/// person commits to its branch while it waits. Released on the same
/// provider, the run, having no session to resume, ends (`retry_inherit`,
/// that commit kept under its run's ref and recorded as the person's), and a
/// new run carries that commit over and lands, with no resume.
#[test]
fn a_run_held_after_its_wrapper_started_and_before_its_first_turn_is_released_to_a_new_run() {
    let wiring = Wiring::new(
        "a_run_held_after_its_wrapper_started_and_before_its_first_turn_is_released_to_a_new_run",
        Script {
            receipts: vec![succeeded],
            verdicts: vec![verdict("pass", "fine")],
        },
    );
    let task = wiring.ready_task(&["true"]);
    let (go, gate) = mpsc::channel();
    wiring.sessions.hold_next_launch(gate);
    let mut go = Some(go);
    wiring.supervise_once_while(|queue| {
        if payloads(&queue.show(task).unwrap(), "wrapper_launched").is_empty() {
            return false;
        }
        let hold = Holds::new(queue, &ActorContext::user(), &StaticPolicy)
            .hold(&HoldTarget::Task(task), "the login expired", None)
            .expect("hold the task");
        assert_eq!(hold.state, HoldState::Settling);
        release_refused(
            &wiring,
            queue,
            task,
            ReleaseChoice::default(),
            "hold_not_settled",
        );
        let _ = go.take().expect("the wrapper is let go once").send(());
        true
    });
    let waiting = wiring.queue().show(task).unwrap();
    let kinds: Vec<&str> = waiting.events.iter().map(|e| e.kind.as_str()).collect();
    assert_eq!(waiting.runs.len(), 1, "{kinds:?}");
    let run = waiting.runs[0].clone();
    assert_eq!(run.status(), RunStatus::NeedsSession, "{kinds:?}");
    assert_eq!(payloads(&waiting, "hold_parked").len(), 1, "{kinds:?}");
    assert_eq!(payloads(&waiting, "turn_withheld").len(), 1, "{kinds:?}");
    assert!(payloads(&waiting, "turn_started").is_empty(), "{kinds:?}");
    assert!(wiring.agent.turn_sessions().is_empty());
    let head = wiring
        .git
        .commit_in(Path::new(run.worktree_path().unwrap()))
        .expect("a person's commit while the run waits");

    let mut queue = wiring.queue();
    let released =
        release(&wiring, &mut queue, task, ReleaseChoice::default()).expect("release the task");
    assert_eq!(
        (released.decision, released.continuation),
        (ReleaseDecision::RetryInherit, "new_run")
    );
    let carried = serde_json::to_value(&released).unwrap()["inherit"]["head"].clone();
    assert_eq!(carried, json!(head));
    assert_eq!(
        crate::application::Repository::ref_value(&wiring.git, &run_ref(run.id())).unwrap(),
        Some(head.clone())
    );
    let ended = wiring.queue().show(task).unwrap();
    assert_eq!(ended.task.status(), TaskStatus::Ready);
    assert_eq!(ended.runs[0].status(), RunStatus::Failed);
    assert!(
        ended.runs[0]
            .last_error()
            .is_some_and(|error| error.starts_with("released: ")),
        "{:?}",
        ended.runs[0].last_error()
    );
    wiring.supervise_once();

    let detail = wiring.queue().show(task).unwrap();
    let kinds: Vec<&str> = detail.events.iter().map(|e| e.kind.as_str()).collect();
    assert_eq!(detail.task.status(), TaskStatus::Completed, "{kinds:?}");
    assert_eq!(detail.runs.len(), 2, "one new run: {kinds:?}");
    assert_eq!(detail.runs[1].status(), RunStatus::Integrated);
    assert_eq!(detail.runs[1].actual_provider(), Provider::Claude);
    assert!(payloads(&detail, "resume_started").is_empty(), "no resume");
    let inherited = payloads(&detail, "run_inherited");
    assert_eq!(inherited.len(), 1, "{kinds:?}");
    assert_eq!(inherited[0]["head"], json!(head));
    assert_eq!(
        payloads(&detail, "task_released")[0]["decision"],
        "retry_inherit"
    );
    assert_inherit_by_the_person(&detail, &run, &head);
}
