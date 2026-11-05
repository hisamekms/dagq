//! The recovery job of a failed run wired from the run's end to what its
//! verdict does (ADR-0047 decisions 39 and 40): the supervisor raises the
//! alert, starts the job with the run's material, applies the verdict it
//! may (a `retry` of a run with no commit of its own, a `resume`), asks a
//! person for the rest, and the task's next run lands.

use serde_json::{Value, json};

use super::{
    agent::{
        Script, committed_and_died, died_at_once, repair, succeeded, succeeded_at_the_head, verdict,
    },
    harness::{Wiring, connect},
    worker_trial::failed,
};
use crate::{
    application::{TaskReads, TaskStore},
    domain::{AskId, AskKind, AskReason, RunStatus, TaskDetail, TaskRun, TaskStatus},
};

/// The payloads of `detail`'s events of `kind`, oldest first.
fn payloads(detail: &TaskDetail, kind: &str) -> Vec<Value> {
    detail
        .events
        .iter()
        .filter(|event| event.kind.as_str() == kind)
        .map(|event| event.payload.clone())
        .collect()
}

/// The entries of `status`'s attention for ask `ask`.
fn ask_attention(status: &Value, ask: AskId) -> Vec<Value> {
    status["attention"]
        .as_array()
        .expect("the status has its attention")
        .iter()
        .filter(|entry| entry["ask_id"] == ask.as_i64())
        .cloned()
        .collect()
}

/// The entry of `status`'s attention for `run` itself, not for an ask.
fn run_attention<'a>(status: &'a Value, run: &TaskRun) -> Option<&'a Value> {
    status["attention"]
        .as_array()
        .expect("the status has its attention")
        .iter()
        .find(|entry| entry["run_id"] == run.id().as_str() && entry["ask_id"].is_null())
}

/// The ask `triage_finished` of `detail` names.
fn triage_ask(wiring: &Wiring, finished: &Value) -> crate::domain::Ask {
    wiring
        .queue()
        .read_ask(AskId::new(finished["ask_id"].as_i64().expect("an ask")))
        .unwrap()
}

/// The payloads of `run`'s events of `kind` in `detail`, oldest first.
fn run_payloads(detail: &TaskDetail, run: &TaskRun, kind: &str) -> Vec<Value> {
    detail
        .events
        .iter()
        .filter(|event| event.kind.as_str() == kind && event.run_id.as_ref() == Some(run.id()))
        .map(|event| event.payload.clone())
        .collect()
}

/// A run that failed with no commit of its own is taken by its recovery
/// job (`recovery_requested` with `alert: failed`), whose `retry` is
/// applied and recorded as `auto_repaired` (`layer: recovery`),
/// `triage_finished` and `recovery_finished`; the job read the run's
/// material, and the task's next run lands on main.
#[test]
fn a_failed_run_without_commits_is_retried_by_its_recovery_job_and_lands() {
    let wiring = Wiring::new(
        "a_failed_run_without_commits_is_retried_by_its_recovery_job_and_lands",
        Script {
            receipts: vec![died_at_once, succeeded],
            verdicts: vec![verdict("pass", "meets the acceptance")],
        },
    );
    let mut judged = repair(json!({"action": "retry"}), "the session died on its own");
    judged["failure_cause"] = json!("environment");
    wiring.agent.recovering(vec![judged]);
    let base = wiring.git.main();
    let task = wiring.ready_task(&[]);
    wiring.supervise_once();

    let mut queue = wiring.queue();
    let detail = queue.show(task).unwrap();
    let kinds: Vec<&str> = detail.events.iter().map(|e| e.kind.as_str()).collect();
    assert_eq!(detail.runs.len(), 2, "{kinds:?}");
    assert_eq!(detail.task.status(), TaskStatus::Completed, "{kinds:?}");
    let first = &detail.runs[0];
    assert_eq!(first.status(), RunStatus::Failed);
    assert_eq!(detail.runs[1].status(), RunStatus::Integrated);
    assert_ne!(wiring.git.main(), base);
    assert!(queue.run_leases().unwrap().is_empty());

    let requested = run_payloads(&detail, first, "recovery_requested");
    assert_eq!(requested.len(), 1, "{kinds:?}");
    assert_eq!(requested[0]["alert"], "failed");
    assert_eq!(requested[0]["attempt"], 1);
    assert_eq!(requested[0]["status"], "failed");
    assert_eq!(
        requested[0]["evidence"].as_array().map(Vec::len),
        Some(1),
        "{}",
        requested[0]
    );
    let finished = run_payloads(&detail, first, "triage_finished");
    assert_eq!(finished.len(), 1, "{kinds:?}");
    assert_eq!(finished[0]["action"], "retry");
    assert_eq!(finished[0]["alert"], "failed");
    assert_eq!(finished[0]["recovery_attempt"], 1);
    assert_eq!(finished[0]["verdict"], "repair");
    assert_eq!(finished[0]["confidence"], "high");
    assert_eq!(finished[0]["reason"], "the session died on its own");
    let repaired = run_payloads(&detail, first, "auto_repaired");
    assert_eq!(repaired.len(), 1, "{kinds:?}");
    assert_eq!(repaired[0]["layer"], "recovery");
    assert_eq!(repaired[0]["repair"], "retry");
    assert_eq!(repaired[0]["alert"], "failed");
    assert_eq!(repaired[0]["conditions"]["own_commits"], false);
    let recovered = run_payloads(&detail, first, "recovery_finished");
    assert_eq!(recovered[0]["applied"], json!(["retry"]));
    assert_eq!(recovered[0]["escalated"], false);
    assert_eq!(recovered[0]["failure_cause"], "environment");
    // No ask was opened, closed ones included.
    let all = crate::application::AskQuery {
        all: true,
        ..Default::default()
    };
    assert!(queue.asks(all).unwrap().is_empty());

    let prompts = wiring.agent.recovery_prompts();
    assert_eq!(prompts.len(), 1);
    let (prompt, dir) = &prompts[0];
    assert_eq!(dir.to_str(), first.run_dir());
    for expected in [
        "which ended failed; its session is gone",
        "raised the alert failed",
        "Acceptance criteria:\nworks",
        "Last error of the run:\nsession exited with code 1",
        "Final screen of the session",
        "Earlier runs of the task:\nnone",
        "retry_inherit",
        "\"verdict\": \"repair\" | \"escalate\"",
    ] {
        assert!(prompt.contains(expected), "{expected:?} in {prompt}");
    }
    let written = run_payloads(&detail, first, "recovery_prompt_written");
    assert_eq!(written[0]["alert"], "failed");
    assert_eq!(written[0]["attempt"], 1);
    assert_eq!(written[0]["prompt_bytes"]["total"], prompt.len());
    assert!(
        written[0]["prompt_bytes"]["sections"]["ended"]
            .as_u64()
            .unwrap()
            > 0,
        "{}",
        written[0]
    );
    for file in ["recovery-failed-1.prompt.txt", "recovery-failed-1.out"] {
        assert!(
            crate::application::RunFiles::read_to_string(&*wiring.files, &dir.join(file)).is_ok(),
            "{file}"
        );
    }
    let first_kinds: Vec<&str> = detail
        .events
        .iter()
        .filter(|event| event.run_id.as_ref() == Some(first.id()))
        .map(|event| event.kind.as_str())
        .collect();
    let at = |kind: &str| first_kinds.iter().position(|k| *k == kind).unwrap();
    assert!(
        at("recovery_requested") < at("triage_started"),
        "{first_kinds:?}"
    );
    assert!(
        at("triage_started") < at("recovery_prompt_written"),
        "{first_kinds:?}"
    );
}

/// The recovery job of a failed run reads the build the supervisor runs,
/// the commit each dependency of the task landed and whether that build
/// holds it, and the binary's replacements since the claim, in their own
/// sections of `recovery_prompt_written`, and the whole of it in the run
/// directory's `recovery-<alert>-<attempt>.binary.json`.
#[test]
fn the_recovery_job_reads_the_supervisors_build_and_the_dependencies_it_holds() {
    let wiring = Wiring::new(
        "the_recovery_job_reads_the_supervisors_build_and_the_dependencies_it_holds",
        Script {
            receipts: vec![succeeded, died_at_once, succeeded],
            verdicts: vec![verdict("pass", "meets the acceptance")],
        },
    );
    wiring.agent.recovering(vec![repair(
        json!({"action": "retry"}),
        "the session died on its own",
    )]);
    let first_task = wiring.ready_task(&[]);
    let dependent = wiring.ready_task_after(&[], &[first_task]);
    wiring.supervise_once();

    let mut queue = wiring.queue();
    let landed = payloads(&queue.show(first_task).unwrap(), "run_integrated")[0]["commit"]
        .as_str()
        .expect("the landed commit")
        .to_owned();
    let detail = queue.show(dependent).unwrap();
    let kinds: Vec<&str> = detail.events.iter().map(|e| e.kind.as_str()).collect();
    assert_eq!(detail.runs.len(), 2, "{kinds:?}");
    let first = &detail.runs[0];
    assert_eq!(first.status(), RunStatus::Failed);
    let prompts = wiring.agent.recovery_prompts();
    assert_eq!(prompts.len(), 1);
    let (prompt, dir) = &prompts[0];
    let version = crate::VERSION;
    for expected in [
        format!("Binary of the supervisor (the fixed binary the runtime runs) now: {version}"),
        format!("\"landed\":\"{landed}\""),
        format!("\"task\":{}", first_task.as_i64()),
        "Replacements of the binary since the run was claimed (update_installed and supervisor_handed_off, oldest first; version is the build it put in place), one JSON line each:\nnone".to_owned(),
        "is fixed by the binary's replacement".to_owned(),
    ] {
        assert!(prompt.contains(&expected), "{expected:?} in {prompt}");
    }
    let file: Value = serde_json::from_str(
        &crate::application::RunFiles::read_to_string(
            &*wiring.files,
            &dir.join("recovery-failed-1.binary.json"),
        )
        .unwrap(),
    )
    .unwrap();
    assert_eq!(file["version"], version);
    assert_eq!(file["dependencies"][0]["task"], first_task.as_i64());
    assert_eq!(file["dependencies"][0]["landed"], landed.as_str());
    // Whether the supervisor's build holds the landing, as the repository
    // tells it (the test's has no commit of the build: it does not), is in
    // the file and the prompt; a build that cannot be told is the unit
    // test's (`prompt::tests::binary_facts_of_a_build_without_a_commit_cannot_tell_what_it_holds`).
    assert_eq!(file["dependencies"][0]["held"], false, "{file}");
    assert!(prompt.contains("\"held\":false"), "{prompt}");
    assert_eq!(file["replacements"], json!([]));
    let written = run_payloads(&detail, first, "recovery_prompt_written");
    let sections = &written[0]["prompt_bytes"]["sections"];
    for section in ["binary", "dependencies", "replacements"] {
        assert!(
            sections[section].as_u64().unwrap() > 0,
            "{section}: {sections}"
        );
    }
}

/// A `retry` of a run whose branch holds commits would throw them away, so
/// the runtime does not apply it: the whole verdict becomes the `decide`
/// ask, with the job's diagnosis, its actions as the recommendation, its
/// options sorted (one of `retry`'s kind merged into the runtime's
/// `retry`, its own marked as going back to it, one that reads as the
/// task's end left out) and its reason category; the run is no attention,
/// its ask is.
#[test]
fn a_retry_of_a_run_with_commits_is_refused_and_asked_with_the_jobs_options() {
    let wiring = Wiring::new(
        "a_retry_of_a_run_with_commits_is_refused_and_asked_with_the_jobs_options",
        Script {
            receipts: vec![committed_and_died],
            verdicts: vec![verdict("pass", "unused")],
        },
    );
    wiring.agent.recovering(vec![json!({
        "verdict": "repair",
        "confidence": "high",
        "diagnosis": "flaky",
        "actions": [{"action": "retry"}],
        "options": [
            {"kind": "retry", "text": "retry anyway"},
            {"kind": "recover", "text": "Mark task 1 done (or cancel it as obsolete)"},
            {"kind": "recover", "text": "ask the planner to split it"},
        ],
        "reason_category": "discard",
        "failure_cause": "environment",
    })]);
    let task = wiring.ready_task(&[]);
    wiring.supervise_once();

    let detail = wiring.queue().show(task).unwrap();
    let kinds: Vec<&str> = detail.events.iter().map(|e| e.kind.as_str()).collect();
    assert_eq!(detail.runs.len(), 1, "{kinds:?}");
    let run = detail.runs[0].clone();
    assert_eq!(run.status(), RunStatus::Failed, "{kinds:?}");
    assert_eq!(detail.task.status(), TaskStatus::InProgress);
    assert!(payloads(&detail, "auto_repaired").is_empty());
    let finished = payloads(&detail, "triage_finished");
    assert_eq!(finished.len(), 1, "{kinds:?}");
    assert_eq!(finished[0]["action"], "ask");
    assert_eq!(finished[0]["status"], "failed");
    assert_eq!(
        finished[0]["options_merged"],
        json!([{"text": "retry anyway", "into": "retry"}])
    );
    assert_eq!(
        finished[0]["options_dropped"][0]["text"],
        "Mark task 1 done (or cancel it as obsolete)"
    );
    let recovered = payloads(&detail, "recovery_finished");
    assert_eq!(recovered[0]["escalated"], true);
    assert_eq!(recovered[0]["reason_category"], "discard");
    // The repair was refused; the job's judgement stays.
    assert_eq!(recovered[0]["failure_cause"], "environment");
    let ask = triage_ask(&wiring, &finished[0]);
    assert_eq!(recovered[0]["ask_id"], json!(ask.id));
    assert_eq!(ask.kind, AskKind::Decide);
    assert_eq!(ask.run_id.as_ref(), Some(run.id()));
    assert_eq!(ask.asked_by, "supervisor");
    let split = format!(
        "ask the planner to split it{}",
        crate::domain::recovery::RECOVER_LABEL
    );
    assert_eq!(ask.options, ["retry", "resume", "cancel", &split]);
    assert_eq!(ask.reason_category, AskReason::Discard);
    assert!(!ask.question.contains("dagq edit"), "{}", ask.question);
    for part in [
        "alert: failed",
        "its branch holds commits of its own",
        "Why a person: discard",
        "Diagnosis: flaky",
        "Recommended: [{\"action\":\"retry\"}]",
        "Last error: session exited with code 1",
        "recovery-failed-1.prompt.txt",
        "The recovery job recommends `retry`: retry anyway",
        "\"Mark task 1 done (or cancel it as obsolete)\": it reads as `done`",
        "the runtime moves nothing for it, and the recovery job runs again with your choice",
    ] {
        assert!(ask.question.contains(part), "{part}: {}", ask.question);
    }
    assert!(ask.is_open());
    let status = wiring.status();
    assert!(run_attention(&status, &run).is_none(), "{status}");
    assert_eq!(
        ask_attention(&status, ask.id)[0]["next"],
        format!("answer ask {}", ask.id)
    );
}

/// `escalate`: a `decide` ask for the inbox with the job's diagnosis and
/// question, the run left `failed` and no attention of its own; the answer
/// is the runtime's to deliver, and the next supervisor applies it
/// (`cancel` here, the cancel's reason the answer), records
/// `triage_decided` and closes the ask.
#[test]
fn an_escalation_waits_for_a_person_and_the_supervisor_applies_the_answer() {
    let wiring = Wiring::new(
        "an_escalation_waits_for_a_person_and_the_supervisor_applies_the_answer",
        Script {
            receipts: vec![died_at_once],
            verdicts: vec![verdict("pass", "unused")],
        },
    );
    wiring.agent.recovering(vec![json!({
        "verdict": "escalate",
        "confidence": "high",
        "diagnosis": "the acceptance cannot be met",
        "question": "Is task 1 still wanted?",
        "failure_cause": "task",
    })]);
    let task = wiring.ready_task(&[]);
    wiring.supervise_once();

    let mut queue = wiring.queue();
    let detail = queue.show(task).unwrap();
    let kinds: Vec<&str> = detail.events.iter().map(|e| e.kind.as_str()).collect();
    let run = detail.runs[0].clone();
    assert_eq!(run.status(), RunStatus::Failed, "{kinds:?}");
    assert_eq!(detail.task.status(), TaskStatus::InProgress);
    assert_eq!(
        payloads(&detail, "recovery_finished")[0]["failure_cause"],
        "task"
    );
    let finished = payloads(&detail, "triage_finished");
    assert_eq!(finished[0]["action"], "ask", "{kinds:?}");
    assert_eq!(finished[0]["status"], "failed");
    let ask = triage_ask(&wiring, &finished[0]);
    assert_eq!(ask.options, ["retry", "resume", "cancel"]);
    assert_eq!(ask.reason_category, AskReason::RecoveryFailed);
    for part in [
        "the recovery job could not repair it",
        "Why a person: recovery_failed",
        "Diagnosis: the acceptance cannot be met",
        "Question: Is task 1 still wanted?",
        "Last error: session exited with code 1",
        "recovery-failed-1.prompt.txt",
    ] {
        assert!(ask.question.contains(part), "{part}: {}", ask.question);
    }
    assert!(run_attention(&wiring.status(), &run).is_none());

    queue.answer(ask.id, "cancel").unwrap();
    let answered = queue
        .run_events(run.id())
        .unwrap()
        .into_iter()
        .rfind(|event| event.kind.as_str() == "ask_answered")
        .unwrap();
    assert_eq!(answered.payload["runtime_delivers"], true);
    assert_eq!(
        ask_attention(&wiring.status(), ask.id)[0]["next"],
        format!("applying the answer of ask {} (runtime)", ask.id)
    );
    wiring.supervise_once();
    let detail = queue.show(task).unwrap();
    assert_eq!(detail.task.status(), TaskStatus::Canceled);
    assert_eq!(detail.runs[0].status(), RunStatus::Failed);
    let decided = payloads(&detail, "triage_decided");
    assert_eq!(decided.len(), 1);
    assert_eq!(decided[0]["answer"], "cancel");
    assert_eq!(decided[0]["ask_id"], ask.id.as_i64());
    let canceled = payloads(&detail, "task_status_changed").pop().unwrap();
    assert_eq!(
        (&canceled["reason"], &canceled["by"], &canceled["ask_id"]),
        (&json!("answered_cancel"), &json!("decide"), &json!(ask.id))
    );
    assert!(queue.read_ask(ask.id).unwrap().closed_at.is_some());
    assert!(ask_attention(&wiring.status(), ask.id).is_empty());
}

/// `wait` of a run that ended changes nothing until its recheck; then the
/// job runs again for the same alert, and a low-confidence repair is not
/// applied but asked.
#[test]
fn a_wait_runs_the_job_again_and_a_low_confidence_repair_is_asked() {
    let wiring = Wiring::new(
        "a_wait_runs_the_job_again_and_a_low_confidence_repair_is_asked",
        Script {
            receipts: vec![died_at_once],
            verdicts: vec![verdict("pass", "unused")],
        },
    );
    wiring.agent.recovering(vec![
        repair(json!({"action": "wait", "recheck_after_secs": 0}), "slow"),
        json!({
            "verdict": "repair",
            "confidence": "low",
            "diagnosis": "maybe the machine slept",
            "actions": [{"action": "wait", "recheck_after_secs": 0}],
        }),
    ]);
    let task = wiring.ready_task(&[]);
    wiring.supervise_once();

    let detail = wiring.queue().show(task).unwrap();
    let kinds: Vec<&str> = detail.events.iter().map(|e| e.kind.as_str()).collect();
    assert_eq!(detail.runs.len(), 1, "{kinds:?}");
    let finished = payloads(&detail, "triage_finished");
    assert_eq!(finished.len(), 2, "{kinds:?}");
    assert_eq!(finished[0]["action"], "wait");
    assert!(finished[0]["recheck_at"].as_i64().is_some());
    assert_eq!(finished[1]["action"], "ask");
    assert_eq!(finished[1]["verdict"], "repair");
    assert_eq!(finished[1]["confidence"], "low");
    assert!(payloads(&detail, "auto_repaired").is_empty());
    let ask = triage_ask(&wiring, &finished[1]);
    assert!(ask.question.contains("confidence low"), "{}", ask.question);
    assert_eq!(wiring.agent.recovery_prompts().len(), 2);
}

/// The worker found the base already meets the task but wrote `succeeded`
/// naming its base, so validation failed the run as `commit_mismatch`. The
/// recovery job's `hand_off` applies (no commit past the base): the run
/// becomes `validating` with no `result_commit`, recorded as
/// `triage_finished` and `auto_repaired` (`repair: hand_off`), and goes
/// down the claim's course: the task's verification on main, the review's
/// judgment carrying the job's diagnosis, and on a `high` pass the task
/// closes without a commit and with no ask.
#[test]
fn a_failed_run_with_no_commit_handed_off_by_its_recovery_job_is_verified_judged_and_closed() {
    let wiring = Wiring::new(
        "a_failed_run_with_no_commit_handed_off_by_its_recovery_job_is_verified_judged_and_closed",
        Script {
            receipts: vec![succeeded_at_the_head],
            verdicts: vec![json!({"verdict": "pass", "reasons": ["seed.txt is there"],
                "summary": "main meets it", "confidence": "high", "recommendation": "close"})],
        },
    );
    wiring.agent.recovering(vec![repair(
        json!({"action": "hand_off"}),
        "seed.txt is on the base already",
    )]);
    let task = wiring.ready_task(&["test -f seed.txt"]);
    wiring.supervise_once();

    let mut queue = wiring.queue();
    let detail = queue.show(task).unwrap();
    let kinds: Vec<&str> = detail.events.iter().map(|e| e.kind.as_str()).collect();
    assert_eq!(detail.runs.len(), 1, "{kinds:?}");
    let run = detail.runs[0].clone();
    assert!(
        queue.asks(Default::default()).unwrap().is_empty(),
        "{kinds:?}"
    );
    assert_eq!(detail.task.status(), TaskStatus::Completed, "{kinds:?}");
    assert_eq!(run.status(), RunStatus::Succeeded);
    assert_eq!(run.result_commit(), None);
    assert!(queue.run_lease(run.id()).unwrap().is_none());
    assert_eq!(
        payloads(&detail, "validation_finished")[0]["code"],
        "commit_mismatch"
    );
    let finished = payloads(&detail, "triage_finished");
    assert_eq!(finished.len(), 1);
    assert_eq!(finished[0]["action"], "hand_off");
    assert_eq!(finished[0]["status"], "validating");
    let repaired = payloads(&detail, "auto_repaired");
    assert_eq!(repaired[0]["repair"], "hand_off");
    assert_eq!(repaired[0]["conditions"]["own_commits"], false);
    assert_eq!(
        payloads(&detail, "recovery_finished")[0]["applied"],
        json!(["hand_off"])
    );
    let at = |kind: &str| kinds.iter().position(|k| *k == kind).unwrap();
    for (before, after) in [
        ("triage_finished", "run_satisfied_snapshot"),
        ("run_satisfied_snapshot", "run_satisfied_verified"),
        ("run_satisfied_verified", "run_satisfied_review_started"),
        ("run_satisfied_judged", "run_satisfied_closed"),
    ] {
        assert!(at(before) < at(after), "{before} < {after}: {kinds:?}");
    }
    assert_eq!(
        payloads(&detail, "run_satisfied_verified")[0]["passed"],
        true
    );
    assert_eq!(
        payloads(&detail, "run_satisfied_closed")[0]["closed_by"],
        "judgment"
    );
    let prompt = crate::application::RunFiles::read_to_string(
        &*wiring.files,
        &std::path::Path::new(run.run_dir().unwrap()).join("satisfied-review-prompt-1.txt"),
    )
    .unwrap();
    for said in [
        "did not report already_satisfied",
        "seed.txt is on the base already",
    ] {
        assert!(prompt.contains(said), "{said}: {prompt}");
    }
}

/// The hand-off's preconditions broken: the worker committed and its
/// session ended before any receipt, so the run did not fail as a receipt
/// naming its base, and the hand-off would throw the commit away. The
/// runtime does not apply it: the verdict becomes the `decide` ask saying
/// why, and the run stays `failed`.
#[test]
fn a_hand_off_of_a_run_that_did_not_fail_at_its_base_is_refused_and_asked() {
    let wiring = Wiring::new(
        "a_hand_off_of_a_run_that_did_not_fail_at_its_base_is_refused_and_asked",
        Script {
            receipts: vec![committed_and_died],
            verdicts: vec![verdict("pass", "unused")],
        },
    );
    wiring.agent.recovering(vec![repair(
        json!({"action": "hand_off"}),
        "the base meets it",
    )]);
    let task = wiring.ready_task(&[]);
    wiring.supervise_once();

    let detail = wiring.queue().show(task).unwrap();
    let kinds: Vec<&str> = detail.events.iter().map(|e| e.kind.as_str()).collect();
    assert_eq!(detail.runs.len(), 1, "{kinds:?}");
    assert_eq!(detail.runs[0].status(), RunStatus::Failed, "{kinds:?}");
    assert_eq!(detail.task.status(), TaskStatus::InProgress);
    assert!(payloads(&detail, "auto_repaired").is_empty());
    assert!(payloads(&detail, "run_satisfied_snapshot").is_empty());
    let finished = payloads(&detail, "triage_finished");
    assert_eq!(finished.len(), 1, "{kinds:?}");
    assert_eq!(finished[0]["action"], "ask");
    let ask = triage_ask(&wiring, &finished[0]);
    assert_eq!(ask.kind, AskKind::Decide);
    for part in [
        "the hand-off does not apply",
        "its receipt was never validated",
    ] {
        assert!(ask.question.contains(part), "{part}: {}", ask.question);
    }
}

/// A person's `resume` to the `decide` ask of a run whose recovery job
/// escalated is applied by the next supervisor: `triage_decided` (the
/// answer, `code: triage_resume`, `needs_session`) closes the ask, the
/// supervisor resumes the run's session with the answer's reason, and the
/// resumed run lands as the one run of its task.
#[test]
fn a_resume_answer_to_the_triages_ask_resumes_the_run_and_it_lands() {
    let wiring = Wiring::new(
        "a_resume_answer_to_the_triages_ask_resumes_the_run_and_it_lands",
        Script {
            receipts: vec![committed_and_died, succeeded_at_the_head],
            verdicts: vec![verdict("pass", "meets the acceptance")],
        },
    );
    wiring.agent.recovering(vec![json!({
        "verdict": "escalate", "confidence": "high",
        "diagnosis": "the session died after its commit",
    })]);
    let base = wiring.git.main();
    let task = wiring.ready_task(&[]);
    wiring.supervise_once();
    let mut queue = wiring.queue();
    let detail = queue.show(task).unwrap();
    let kinds: Vec<&str> = detail.events.iter().map(|e| e.kind.as_str()).collect();
    assert_eq!(detail.runs[0].status(), RunStatus::Failed, "{kinds:?}");
    let ask = triage_ask(&wiring, &payloads(&detail, "triage_finished")[0]);
    assert_eq!(ask.options, ["retry", "resume", "cancel"]);

    queue.answer(ask.id, "resume").unwrap();
    wiring.supervise_once();
    let detail = queue.show(task).unwrap();
    let kinds: Vec<&str> = detail.events.iter().map(|e| e.kind.as_str()).collect();
    let decided = payloads(&detail, "triage_decided");
    assert_eq!(decided.len(), 1, "{kinds:?}");
    assert_eq!(decided[0]["answer"], "resume");
    assert_eq!(decided[0]["ask_id"], json!(ask.id));
    assert_eq!(decided[0]["code"], "triage_resume");
    assert_eq!(decided[0]["status"], "needs_session");
    assert!(queue.read_ask(ask.id).unwrap().closed_at.is_some());
    let started = payloads(&detail, "resume_started");
    assert_eq!(started.len(), 1, "{kinds:?}");
    assert_eq!(started[0]["reason"], decided[0]["reason"]);
    let at = |kind: &str| kinds.iter().rposition(|k| *k == kind).unwrap();
    assert!(at("triage_decided") < at("resume_started"), "{kinds:?}");
    assert_eq!(detail.runs.len(), 1, "{kinds:?}");
    assert_eq!(detail.runs[0].status(), RunStatus::Integrated, "{kinds:?}");
    assert_eq!(detail.task.status(), TaskStatus::Completed);
    assert_ne!(wiring.git.main(), base);
    assert!(ask_attention(&wiring.status(), ask.id).is_empty());
    assert!(queue.run_leases().unwrap().is_empty());
}

/// A triage whose lease another token took while its recovery job runs
/// (as a stalled supervisor's round looks to the next one) is not triaged
/// again into a second slot by the supervisor that still drives it: its
/// slot is dropped first (the one error it returns), nothing is written
/// about that round, and a later pass starts the next round, once, from
/// the stale lease it takes over; a stop asked while that round runs
/// drains the one slot that triages the run.
#[test]
fn a_triage_whose_lease_moved_is_triaged_again_only_after_its_slot_is_dropped() {
    let wiring = Wiring::new(
        "a_triage_whose_lease_moved_is_triaged_again_only_after_its_slot_is_dropped",
        Script {
            receipts: vec![died_at_once],
            verdicts: vec![verdict("pass", "unused")],
        },
    );
    let escalate = || {
        json!({"verdict": "escalate", "confidence": "high",
               "diagnosis": "the wiring test does not repair"})
    };
    wiring.agent.recovering(vec![escalate(), escalate()]);
    // Each round's job runs until the test lets it go.
    let (first_release, first_held) = std::sync::mpsc::channel();
    let (second_release, second_held) = std::sync::mpsc::channel();
    wiring.agent.hold_next_recovery(first_held);
    wiring.agent.hold_next_recovery(second_held);
    let task = wiring.ready_task(&[]);
    let stale = i64::try_from(super::host::STOPPED_AT.as_secs()).unwrap() - 31;
    let mut first_release = Some(first_release);
    let mut second_release = Some(second_release);
    let mut moved = false;
    let mut stopping = false;
    let mut draining = None;
    let outcome = wiring.supervise_once_while_stoppable(|queue, stop| {
        let detail = queue.show(task).unwrap();
        let started = payloads(&detail, "triage_started").len();
        if !moved && started == 1 {
            let taken = connect(&wiring.db)
                .execute(
                    "UPDATE run_leases SET token='taken', heartbeat_at=?2 WHERE run_id=?1",
                    rusqlite::params![detail.runs[0].id().as_str(), stale],
                )
                .unwrap();
            moved = taken == 1;
        }
        if started == 2 && !stopping {
            // The first round's job, if anything still waits on it, ends;
            // the supervisor drains while the second round's runs.
            first_release.take();
            stop();
            stopping = true;
        }
        if stopping {
            draining = queue
                .all_events()
                .unwrap()
                .into_iter()
                .find(|event| event.kind.as_str() == "supervisor_draining")
                .map(|event| event.payload["runs"].clone());
            if draining.is_some() {
                second_release.take();
                return true;
            }
        }
        false
    });
    drop((first_release, second_release));
    assert!(moved, "the lease moved while the first round ran");
    assert_eq!(outcome["outcome"], "stopped", "{outcome}");
    let detail = wiring.queue().show(task).unwrap();
    let kinds: Vec<&str> = detail.events.iter().map(|e| e.kind.as_str()).collect();
    let run = detail.runs[0].clone();
    // One slot triages the run, not the dropped one beside the new round.
    assert_eq!(draining, Some(json!([run.id()])), "{kinds:?}");
    let disowned: Vec<&Value> = outcome["errors"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|error| {
            error["message"]
                .as_str()
                .unwrap()
                .contains("held by another process")
        })
        .collect();
    assert_eq!(disowned.len(), 1, "{outcome}");
    assert_eq!(outcome["errors"].as_array().unwrap().len(), 1, "{outcome}");
    let rounds = |kind: &str| -> Vec<Value> {
        run_payloads(&detail, &run, kind)
            .into_iter()
            .map(|payload| payload["attempt"].clone())
            .collect()
    };
    assert_eq!(rounds("triage_started"), [json!(1), json!(2)], "{kinds:?}");
    // Nothing more about the first round once its lease moved.
    assert_eq!(rounds("triage_finished"), [json!(2)], "{kinds:?}");
    assert!(rounds("triage_failed").is_empty());
    let acquired: Vec<Value> = run_payloads(&detail, &run, "lease_acquired")
        .into_iter()
        .filter(|payload| payload["reason"] == "triage")
        .map(|payload| payload["previous_token"].clone())
        .collect();
    assert_eq!(acquired.len(), 2, "{acquired:?}");
    assert_eq!(acquired[1], "taken");
    assert_eq!(wiring.agent.recovery_prompts().len(), 2);
}

/// `resume`: the failed run whose work is committed becomes
/// `needs_session` with the job's instruction, the supervisor resumes its
/// session with a request naming it, and the resumed run is validated,
/// reviewed and landed like any other, as the one run of its task.
#[test]
fn a_failed_run_the_recovery_job_resumes_is_resumed_in_its_session_and_lands() {
    let wiring = Wiring::new(
        "a_failed_run_the_recovery_job_resumes_is_resumed_in_its_session_and_lands",
        Script {
            receipts: vec![committed_and_died, succeeded_at_the_head],
            verdicts: vec![verdict("pass", "meets the acceptance")],
        },
    );
    wiring.agent.recovering(vec![repair(
        json!({"action": "resume", "instruction": "write the receipt for your commit"}),
        "the work is committed but the receipt is missing",
    )]);
    let base = wiring.git.main();
    let task = wiring.ready_task(&[]);
    wiring.supervise_once();

    let detail = wiring.queue().show(task).unwrap();
    let kinds: Vec<&str> = detail.events.iter().map(|e| e.kind.as_str()).collect();
    assert_eq!(detail.runs.len(), 1, "{kinds:?}");
    assert_eq!(detail.runs[0].status(), RunStatus::Integrated, "{kinds:?}");
    assert_eq!(detail.task.status(), TaskStatus::Completed);
    assert_ne!(wiring.git.main(), base);
    let finished = payloads(&detail, "triage_finished");
    assert_eq!(finished.len(), 1, "{kinds:?}");
    assert_eq!(finished[0]["action"], "resume");
    assert_eq!(finished[0]["status"], "needs_session");
    assert_eq!(
        finished[0]["instruction"],
        "write the receipt for your commit"
    );
    let repaired = payloads(&detail, "auto_repaired");
    assert_eq!(repaired[0]["repair"], "resume");
    assert_eq!(repaired[0]["layer"], "recovery");
    let at = |kind: &str| kinds.iter().rposition(|k| *k == kind).unwrap();
    assert!(at("triage_finished") < at("resume_started"), "{kinds:?}");
    let started = payloads(&detail, "resume_started");
    assert_eq!(started[0]["reason"], "write the receipt for your commit");
    // The resumed session keeps the claim's model and effort, and its turn
    // starts with them.
    let claimed = &payloads(&detail, "run_claimed")[0];
    assert_eq!(
        (
            &started[0]["model"],
            &started[0]["effort"],
            &started[0]["group"]
        ),
        (&claimed["model"], &claimed["effort"], &Value::Null)
    );
    let launch = (
        claimed["model"].as_str().unwrap().to_owned(),
        claimed["effort"].as_str().unwrap().to_owned(),
    );
    assert_eq!(wiring.agent.turn_models(), [launch.clone(), launch]);
    // The first session's wrapper ended with its agent: nothing closes it;
    // the accepted resumed session is the one closed.
    let first = payloads(&detail, "workspace_created")[0]["workspace_id"].clone();
    let closed = payloads(&detail, "workspace_closed");
    assert!(!closed.is_empty(), "{kinds:?}");
    assert!(
        closed.iter().all(|closed| closed["workspace_id"] != first),
        "{kinds:?}"
    );
    assert!(at("resume_started") < at("workspace_closed"), "{kinds:?}");
    let prompts = wiring.agent.turn_prompts();
    assert_eq!(prompts.len(), 2, "{kinds:?}");
    let text = &prompts[1];
    for expected in [
        "the supervisor's triage sent it back to this session to finish",
        "Reason: write the receipt for your commit",
        "1. Do what the reason asks in this worktree and commit",
    ] {
        assert!(text.contains(expected), "{expected:?} in {text}");
    }
}

/// A run whose `integrate` verification fails on a broken verify command,
/// and whose resumed session gives up, goes to its recovery job. Its
/// `decide` asks offer the verify fix; a person answers the job's own
/// option (once without its label) until the alert's three jobs are used
/// up: each answer is `triage_decided` with `action: recover`, and the
/// next round's job reads it without the label. The used-up ask, opened
/// with no job, still offers the fix: the task's verify is edited and the
/// fix answered, the job gets one more round that reads the corrected
/// commands and the edit, chooses `retry_inherit`, and the next run lands
/// through the corrected verification with the work kept.
#[test]
fn a_corrected_verify_gets_a_round_past_the_used_up_limit_and_lands_inherited() {
    let mut wiring = Wiring::new(
        "a_corrected_verify_gets_a_round_past_the_used_up_limit_and_lands_inherited",
        Script {
            receipts: vec![succeeded, failed, succeeded],
            verdicts: vec![verdict("pass", "fine")],
        },
    );
    wiring.verifier = std::sync::Arc::new(super::host::FakeVerifier::new(
        wiring.files.clone(),
        &["false"],
    ));
    let escalate = || {
        json!({
            "verdict": "escalate", "confidence": "high", "diagnosis": "the verify is wrong",
            "options": [{"kind": "recover", "text": "try again"}], "reason_category": "recovery_failed"
        })
    };
    let try_again = format!("try again{}", crate::domain::recovery::RECOVER_LABEL);
    let option = "edit the task's --verify, then retry_inherit";
    wiring.agent.recovering(vec![escalate()]);
    let task = wiring.ready_task(&["false"]);
    wiring.supervise_once();

    let mut queue = wiring.queue();
    let decide = |queue: &mut crate::infrastructure::sqlite::SqliteQueue| {
        queue
            .asks(Default::default())
            .unwrap()
            .into_iter()
            .find(|ask| ask.kind == AskKind::Decide && ask.is_open())
            .expect("an open decide ask")
    };
    let first = queue.show(task).unwrap().runs[0].clone();
    assert_eq!(
        first.status(),
        RunStatus::Failed,
        "{:?}",
        first.last_error()
    );
    for round in 1..=3 {
        let ask = decide(&mut queue);
        assert!(
            ask.options.contains(&option.to_owned()),
            "{:?}",
            ask.options
        );
        assert!(
            ask.question
                .contains(&format!("dagq edit {} --verify", task.as_i64())),
            "{}",
            ask.question
        );
        // The second round answers without the label, as a person may.
        let answer = if round == 2 { "try again" } else { &try_again };
        queue.answer(ask.id, answer).unwrap();
        let jobs = wiring.agent.recovery_prompts().len();
        if round < 3 {
            wiring.agent.recovering(vec![escalate()]);
        }
        wiring.supervise_once();
        let ran = wiring.agent.recovery_prompts().len() - jobs;
        assert_eq!(ran, usize::from(round < 3), "round {round}");
        let detail = queue.show(task).unwrap();
        let decided = run_payloads(&detail, &first, "triage_decided");
        assert_eq!(decided.len(), round);
        assert_eq!(decided[round - 1]["answer"], json!(try_again));
        assert_eq!(decided[round - 1]["action"], "recover");
        assert_eq!(decided[round - 1]["status"], "failed");
        assert_eq!(decided[round - 1]["ask_id"], json!(ask.id));
        assert!(queue.read_ask(ask.id).unwrap().closed_at.is_some());
        if round < 3 {
            let requested = run_payloads(&detail, &first, "recovery_requested");
            assert_eq!(requested.len(), round + 1, "{requested:?}");
            assert_eq!(requested[round]["attempt"], round + 1);
            assert_eq!(requested[round]["alert"], "failed");
            let finished = run_payloads(&detail, &first, "triage_finished");
            assert_eq!(finished.last().unwrap()["recovery_attempt"], round + 1);
            assert_eq!(finished.last().unwrap()["action"], "ask");
            // The job reads the answer without the label.
            assert_eq!(requested[round]["person_answer"]["answer"], "try again");
            assert_eq!(requested[round]["person_answer"]["ask_id"], json!(ask.id));
            let (prompt, _) = wiring.agent.recovery_prompts().pop().unwrap();
            assert!(prompt.contains("try again"), "{prompt}");
        }
    }
    // The used-up ask still offers the fix; answered after the edit, the
    // job runs once more.
    let ask = decide(&mut queue);
    assert!(
        ask.question
            .contains("the recovery job ran 3 times for this alert already (at most 3)"),
        "{}",
        ask.question
    );
    let used_up = run_payloads(&queue.show(task).unwrap(), &first, "triage_finished");
    let used_up = used_up.last().unwrap();
    assert_eq!(used_up["action"], "ask");
    assert!(used_up["verdict"].is_null());
    assert!(
        ask.options.contains(&option.to_owned()),
        "{:?}",
        ask.options
    );
    queue
        .edit_task(
            task,
            crate::domain::TaskEdit {
                verification_commands: Some(vec!["test -f seed.txt".into()]),
                ..Default::default()
            },
            TaskStatus::InProgress,
        )
        .unwrap();
    queue.answer(ask.id, option).unwrap();
    wiring.agent.recovering(vec![repair(
        json!({"action": "retry_inherit"}),
        "verify corrected",
    )]);
    wiring.supervise_once();
    let (prompt, _) = wiring.agent.recovery_prompts().pop().unwrap();
    for read in ["test -f seed.txt", "task_edited", option] {
        assert!(prompt.contains(read), "{read}: {prompt}");
    }
    let detail = queue.show(task).unwrap();
    let kinds: Vec<&str> = detail.events.iter().map(|e| e.kind.as_str()).collect();
    assert_eq!(detail.runs.len(), 2, "{kinds:?}");
    assert_eq!(
        detail.runs[1].status(),
        RunStatus::Integrated,
        "{kinds:?}; last error {:?}",
        detail.runs[1].last_error()
    );
    let requested = run_payloads(&detail, &first, "recovery_requested");
    assert_eq!(requested.len(), 4, "{requested:?}");
    assert_eq!(requested[3]["person_answer"]["answer"], option);
    let inherited = run_payloads(&detail, &detail.runs[1], "run_inherited");
    assert_eq!(inherited[0]["inherit_from_run"], json!(first.id()));
    let verified = run_payloads(&detail, &detail.runs[1], "verification_command");
    assert_eq!(verified.len(), 1, "{verified:?}");
    assert_eq!(verified[0]["command"], "test -f seed.txt");
}
