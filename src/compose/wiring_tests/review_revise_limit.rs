//! A review's `revise` reaches the run's live headless session as the
//! request of its next turn, and the run's revises are counted across the
//! run (ADR-t2599-1): across a person's `send_back` and a landing's
//! resume, up to the limit the landing branch's commit writes
//! (ADR-t3033-1). Past them a review is judged by its reasons' codes: with
//! no code finding the run lands and its reasons become follow_up drafts,
//! otherwise a person decides in `approve_landing`. The review agents'
//! judgments route a round the same way: back to the session in one
//! revise, or to a person. The decisions are the unit tests'
//! (`domain::review_reason`, `domain::run::history::decide_revise`,
//! `supervise::landing`); these keep their wiring.

use serde_json::{Value, json};

use super::{
    agent::{Script, succeeded},
    harness::Wiring,
};
use crate::{
    application::TaskReads,
    domain::{Ask, AskKind, RunStatus, TaskDetail, TaskId, TaskStatus},
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

/// The kinds of `detail`'s events, oldest first.
fn kinds(detail: &TaskDetail) -> Vec<&str> {
    detail
        .events
        .iter()
        .map(|event| event.kind.as_str())
        .collect()
}

/// Where the first event of `kind` is among `kinds`.
fn position(kinds: &[&str], kind: &str) -> usize {
    kinds
        .iter()
        .position(|at| *at == kind)
        .unwrap_or_else(|| panic!("no {kind} in {kinds:?}"))
}

/// A review's verdict `decision` with plain `reasons` and `summary`.
fn said(decision: &str, reasons: &[&str], summary: &str) -> Value {
    json!({"verdict": decision, "reasons": reasons, "summary": summary})
}

/// A `revise` verdict whose reasons carry codes: each `(text, codes)`.
fn coded_revise(reasons: &[(&str, &[&str])]) -> Value {
    let reasons: Vec<Value> = reasons
        .iter()
        .map(|(text, codes)| json!({"text": text, "codes": codes}))
        .collect();
    json!({"verdict": "revise", "reasons": reasons, "summary": "not yet"})
}

/// The one open ask of the queue, an `approve_landing`.
fn open_landing_ask(queue: &mut SqliteQueue) -> Ask {
    let open: Vec<Ask> = queue
        .asks(Default::default())
        .unwrap()
        .into_iter()
        .filter(|ask| ask.closed_at.is_none())
        .collect();
    assert_eq!(open.len(), 1, "{open:?}");
    let ask = open[0].clone();
    assert_eq!(ask.kind, AskKind::ApproveLanding);
    assert_eq!(ask.options, ["land", "send_back", "cancel"]);
    ask
}

/// Whether some worker's turn of `wiring` was given a request that says
/// `says`.
fn told(wiring: &Wiring, says: &str) -> bool {
    wiring
        .agent
        .turn_prompts()
        .iter()
        .any(|prompt| prompt.contains(says))
}

/// The run landed: integrated, its task completed and main moved past
/// `base` to the head pushed.
fn assert_landed(wiring: &Wiring, detail: &TaskDetail, base: &crate::domain::CommitSha) {
    assert_eq!(
        detail.runs[0].status(),
        RunStatus::Integrated,
        "{:?}",
        kinds(detail)
    );
    assert_eq!(detail.task.status(), TaskStatus::Completed);
    assert_ne!(&wiring.git.main(), base);
    assert_eq!(wiring.git.pushed(), Some(wiring.git.main()));
}

/// The revises are counted across the run: a person's `send_back`
/// resumes the run, and the resumed session gets the run's second revise
/// (`revise 2 of 2 of the run`), not the first of a new count. The next
/// review has a code finding past the limit: it asks a person, naming the
/// code findings left, and sends nothing to the session. The review after
/// a `send_back` past the limit is
/// `the_review_after_a_send_back_past_the_limit_is_judged_without_a_revise`'s.
#[test]
fn revises_are_counted_across_a_send_back_and_past_them_a_code_finding_asks() {
    let wiring = Wiring::new(
        "revises_are_counted_across_a_send_back_and_past_them_a_code_finding_asks",
        Script {
            receipts: vec![succeeded; 5],
            verdicts: vec![
                said("revise", &["still short"], "not yet"),
                said("concern", &["is the flag wanted"], "unsure"),
                said("revise", &["still short"], "not yet"),
                coded_revise(&[
                    ("the loop never ends", &["local_slip", "code_defect"]),
                    ("a typo", &["local_slip"]),
                ]),
            ],
        },
    );
    let task = wiring.ready_task(&["true"]);
    wiring.supervise_once();
    let mut queue = wiring.queue();
    let ask = open_landing_ask(&mut queue);
    assert!(
        ask.question.contains("returned concern"),
        "{}",
        ask.question
    );
    queue.answer(ask.id, "send_back").unwrap();
    wiring.supervise_once();

    let detail = queue.show(task).unwrap();
    let kinds = kinds(&detail);
    assert_eq!(
        detail.runs[0].status(),
        RunStatus::AwaitingIntegration,
        "{kinds:?}"
    );
    let requested: Vec<&Value> = payloads(&detail, "revise_requested")
        .iter()
        .map(|p| &p["attempt"])
        .collect();
    assert_eq!(requested, [&json!(1), &json!(2)], "{kinds:?}");
    let second = kinds
        .iter()
        .enumerate()
        .filter(|(_, kind)| **kind == "revise_requested")
        .map(|(at, _)| at)
        .nth(1)
        .unwrap();
    assert!(position(&kinds, "resume_started") < second, "{kinds:?}");
    assert!(told(&wiring, "(revise 2 of 2 of the run)"));
    assert_eq!(
        payloads(&detail, "revise_limit_judged"),
        [
            &json!({"attempt": 4, "revises": 2, "revise_limit": 2, "converged": false, "code_paths": null,
                 "code_findings": [{"text": "the loop never ends",
                                    "codes": ["local_slip", "code_defect"], "paths": [],
                                    "security": false}]})
        ]
    );
    assert!(payloads(&detail, "follow_up_registered").is_empty());
    let ask = open_landing_ask(&mut queue);
    assert!(
        ask.question.contains(
            "(the review still asks for changes after the run's 2 revises, and code findings are left (local_slip, code_defect)): not yet"
        ),
        "{}",
        ask.question
    );
    assert!(
        ask.question.contains("\n- the loop never ends"),
        "{}",
        ask.question
    );
}

/// Past the limit, the review after a person's `send_back` is judged by
/// its reasons' codes as the one before it, without a revise: the run's
/// two revises and a code finding ask a person, the `send_back` resumes
/// the run, and its resumed session's review, with no code finding left,
/// lands the run and its reason becomes a follow_up draft.
#[test]
fn the_review_after_a_send_back_past_the_limit_is_judged_without_a_revise() {
    let wiring = Wiring::new(
        "the_review_after_a_send_back_past_the_limit_is_judged_without_a_revise",
        Script {
            receipts: vec![succeeded; 5],
            verdicts: vec![
                said("revise", &["still short"], "not yet"),
                said("revise", &["still short"], "not yet"),
                coded_revise(&[
                    ("the loop never ends", &["local_slip", "code_defect"]),
                    ("a typo", &["local_slip"]),
                ]),
                coded_revise(&[("a typo", &["local_slip"])]),
            ],
        },
    );
    let base = wiring.git.main();
    let task = wiring.ready_task(&["true"]);
    wiring.supervise_once();
    let mut queue = wiring.queue();
    let ask = open_landing_ask(&mut queue);
    assert!(
        ask.question.contains(
            "after the run's 2 revises, and code findings are left (local_slip, code_defect)"
        ),
        "{}",
        ask.question
    );

    queue.answer(ask.id, "send_back").unwrap();
    wiring.supervise_once();
    let detail = queue.show(task).unwrap();
    assert_landed(&wiring, &detail, &base);
    assert_eq!(payloads(&detail, "resume_started").len(), 1);
    // No revise after the limit: the resume's review is judged at once.
    assert_eq!(payloads(&detail, "revise_requested").len(), 2);
    assert!(!told(&wiring, "(revise 3 of"));
    assert_eq!(payloads(&detail, "review_finished").len(), 4);
    let judged: Vec<&Value> = payloads(&detail, "revise_limit_judged")
        .iter()
        .map(|p| &p["converged"])
        .collect();
    assert_eq!(judged, [&json!(false), &json!(true)]);
    let registered = payloads(&detail, "follow_up_registered");
    assert_eq!(registered.len(), 1, "{registered:?}");
    assert_eq!(registered[0]["review_attempt"], 4);
}

/// Past the run's revises, a review whose every reason has only codes of
/// no code finding lands the run without an ask, through the landing like
/// a pass, and each reason becomes a follow_up draft of the category its
/// first code maps to.
#[test]
fn a_review_past_the_limit_with_no_code_finding_lands_and_follows_up_its_reasons() {
    let wiring = Wiring::new(
        "a_review_past_the_limit_with_no_code_finding_lands_and_follows_up_its_reasons",
        Script {
            receipts: vec![succeeded; 3],
            verdicts: vec![
                coded_revise(&[("add a line", &["acceptance_unmet"])]),
                coded_revise(&[("add a test", &["test_gap"])]),
                coded_revise(&[
                    ("a stale sentence in docs/a.md", &["docs_drift"]),
                    (
                        "a line out of order",
                        &["local_slip", "repo_rule_violation"],
                    ),
                ]),
            ],
        },
    );
    let base = wiring.git.main();
    let task = wiring.ready_task(&["true"]);
    wiring.supervise_once();
    let mut queue = wiring.queue();
    let detail = queue.show(task).unwrap();
    let run = detail.runs[0].clone();
    assert_landed(&wiring, &detail, &base);
    assert!(queue.asks(Default::default()).unwrap().is_empty());
    assert_eq!(payloads(&detail, "revise_requested").len(), 2);
    assert_eq!(payloads(&detail, "review_finished").len(), 3);
    assert!(payloads(&detail, "integration_approved").is_empty());
    assert_eq!(
        payloads(&detail, "revise_limit_judged"),
        [
            &json!({"attempt": 3, "revises": 2, "revise_limit": 2, "converged": true, "code_paths": null,
                 "code_findings": []})
        ]
    );
    let kinds = kinds(&detail);
    for (earlier, later) in [
        ("follow_up_registered", "revise_limit_judged"),
        ("revise_limit_judged", "exit_requested"),
        ("exit_requested", "run_integrated"),
    ] {
        assert!(
            position(&kinds, earlier) < position(&kinds, later),
            "{earlier} before {later}: {kinds:?}"
        );
    }
    let registered: Vec<&Value> = payloads(&detail, "follow_up_registered")
        .into_iter()
        .filter(|p| p["from"] == "review")
        .collect();
    assert_eq!(registered.len(), 2, "{registered:?}");
    for (payload, (category, text)) in registered.iter().zip([
        ("docs_drift", "a stale sentence in docs/a.md"),
        ("improvement", "a line out of order"),
    ]) {
        assert_eq!(payload["review_attempt"], 3);
        let draft = queue
            .show(TaskId::new(payload["task_id"].as_i64().unwrap()))
            .unwrap()
            .task;
        assert_eq!(draft.status(), TaskStatus::Draft);
        assert_eq!(draft.title(), format!("{category}: {text}"));
        assert!(
            draft.description().starts_with(text),
            "{}",
            draft.description()
        );
        let (_, material) = queue.draft_origin(draft.id()).unwrap().unwrap();
        assert_eq!(material["category"], category);
        assert_eq!(material["source_run_id"], run.id().as_str());
    }
}

/// A resume of a landing that stopped does not start the count again
/// either. The run's first review sends it one revise and the next passes,
/// but its landing's verification fails, so the landing is parked and the
/// run resumed. Its resumed session is reviewed again, every review with a
/// code finding: it gets the run's second revise (`revise 2 of 2 of the
/// run`), and the review after it, past the limit, asks a person. That the
/// count reads only the run's revises, whatever stopped the landing (a
/// rebase conflict alike), and that a run already at the limit asks at its
/// first review after the resume, is `domain::run::history::decide_revise`'s.
#[test]
fn a_resume_after_a_failed_landing_verification_keeps_the_runs_count() {
    let defect = || coded_revise(&[("the loop never ends", &["code_defect"])]);
    let wiring = Wiring::new(
        "a_resume_after_a_failed_landing_verification_keeps_the_runs_count",
        Script {
            receipts: vec![succeeded; 5],
            verdicts: vec![defect(), said("pass", &[], "looks fine"), defect()],
        },
    );
    wiring.verifier.fail_once("landing-check");
    let task = wiring.ready_task(&["landing-check"]);
    wiring.supervise_once();

    let mut queue = wiring.queue();
    let detail = queue.show(task).unwrap();
    let kinds = kinds(&detail);
    assert_eq!(
        detail.runs[0].status(),
        RunStatus::AwaitingIntegration,
        "{kinds:?}"
    );
    assert_eq!(
        payloads(&detail, "integration_deferred")[0]["code"],
        "verification_failed",
        "{kinds:?}"
    );
    assert_eq!(payloads(&detail, "resume_started").len(), 1, "{kinds:?}");
    let requested: Vec<&Value> = payloads(&detail, "revise_requested")
        .iter()
        .map(|p| &p["attempt"])
        .collect();
    assert_eq!(requested.len(), 2, "{kinds:?}");
    let second = kinds
        .iter()
        .enumerate()
        .filter(|(_, kind)| **kind == "revise_requested")
        .map(|(at, _)| at)
        .nth(1)
        .unwrap();
    assert!(position(&kinds, "resume_started") < second, "{kinds:?}");
    let judged: Vec<Value> = payloads(&detail, "revise_limit_judged")
        .iter()
        .map(|p| json!([p["revises"], p["converged"], p["code_findings"]]))
        .collect();
    assert_eq!(
        judged,
        [
            json!([2, false, [{"text": "the loop never ends", "codes": ["code_defect"], "paths": [], "security": false}]])
        ]
    );
    assert!(told(&wiring, "(revise 2 of 2 of the run)"));
    let ask = open_landing_ask(&mut queue);
    assert!(
        ask.question
            .contains("after the run's 2 revises, and code findings are left (code_defect)"),
        "{}",
        ask.question
    );
}

/// The revise limit each `review_started` recorded, in order.
fn started_limits(detail: &TaskDetail) -> Vec<Value> {
    payloads(detail, "review_started")
        .iter()
        .map(|p| p["revise_limit"].clone())
        .collect()
}

/// (ADR-t3033-1) A `[review] revise_limit` the landing branch's commit
/// writes bounds the run's revises in place of the default: three here,
/// counted across a person's `send_back`, the third named `revise 3 of 3
/// of the run`. The review past them is judged as past the default limit:
/// a code finding asks a person, naming the three revises. That no review
/// reads the run's own branch is the Git boundary's
/// (`runtime_review_revise_limit` in tests/it).
#[test]
fn the_landing_branchs_revise_limit_bounds_the_runs_revises_across_a_send_back() {
    let wiring = Wiring::new(
        "the_landing_branchs_revise_limit_bounds_the_runs_revises_across_a_send_back",
        Script {
            receipts: vec![succeeded; 6],
            verdicts: vec![
                said("revise", &["still short"], "not yet"),
                said("concern", &["is the flag wanted"], "unsure"),
                said("revise", &["still short"], "not yet"),
                said("revise", &["still short"], "not yet"),
                coded_revise(&[("the loop never ends", &["code_defect"])]),
            ],
        },
    );
    wiring
        .git
        .commit_files(&[("dagq.toml", "[review]\nrevise_limit = 3\n")], &[]);
    let task = wiring.ready_task(&["true"]);
    wiring.supervise_once();
    let mut queue = wiring.queue();
    let ask = open_landing_ask(&mut queue);
    assert!(
        ask.question.contains("returned concern"),
        "{}",
        ask.question
    );
    queue.answer(ask.id, "send_back").unwrap();
    wiring.supervise_once();

    let detail = queue.show(task).unwrap();
    let kinds = kinds(&detail);
    assert_eq!(
        detail.runs[0].status(),
        RunStatus::AwaitingIntegration,
        "{kinds:?}"
    );
    assert_eq!(payloads(&detail, "resume_started").len(), 1);
    let requested: Vec<&Value> = payloads(&detail, "revise_requested")
        .iter()
        .map(|p| &p["attempt"])
        .collect();
    assert_eq!(requested, [&json!(1), &json!(2), &json!(3)], "{kinds:?}");
    assert_eq!(started_limits(&detail), vec![json!(3); 5]);
    for number in 1..=3 {
        let says = format!("(revise {number} of 3 of the run)");
        assert!(told(&wiring, &says), "{says}");
    }
    assert_eq!(
        payloads(&detail, "revise_limit_judged"),
        [
            &json!({"attempt": 5, "revises": 3, "revise_limit": 3, "converged": false, "code_paths": null,
                 "code_findings": [{"text": "the loop never ends", "codes": ["code_defect"],
                                    "paths": [], "security": false}]})
        ]
    );
    let why = "the review still asks for changes after the run's 3 revises, and code findings are left (code_defect)";
    let ask = open_landing_ask(&mut queue);
    assert!(ask.question.contains(why), "{}", ask.question);
}

/// (ADR-t3033-1) Past the run's revises under the landing branch's
/// `revise_limit` of three, a review with no code finding lands the run
/// and follows up its reason, judged against the written limit: every
/// attempt records it, and the judgment names three revises of three.
#[test]
fn past_the_landing_branchs_revise_limit_a_review_with_no_code_finding_lands() {
    let wiring = Wiring::new(
        "past_the_landing_branchs_revise_limit_a_review_with_no_code_finding_lands",
        Script {
            receipts: vec![succeeded; 5],
            verdicts: vec![
                said("revise", &["still short"], "not yet"),
                said("revise", &["still short"], "not yet"),
                said("revise", &["still short"], "not yet"),
                coded_revise(&[("a typo", &["local_slip"])]),
            ],
        },
    );
    wiring
        .git
        .commit_files(&[("dagq.toml", "[review]\nrevise_limit = 3\n")], &[]);
    let base = wiring.git.main();
    let task = wiring.ready_task(&["true"]);
    wiring.supervise_once();
    let mut queue = wiring.queue();
    let detail = queue.show(task).unwrap();
    assert_landed(&wiring, &detail, &base);
    assert!(queue.asks(Default::default()).unwrap().is_empty());
    assert_eq!(payloads(&detail, "revise_requested").len(), 3);
    assert_eq!(started_limits(&detail), vec![json!(3); 4]);
    for number in 1..=3 {
        let says = format!("(revise {number} of 3 of the run)");
        assert!(told(&wiring, &says), "{says}");
    }
    let judged: Vec<Value> = payloads(&detail, "revise_limit_judged")
        .iter()
        .map(|p| json!([p["revises"], p["revise_limit"], p["converged"]]))
        .collect();
    assert_eq!(judged, [json!([3, 3, true])]);
    let registered = payloads(&detail, "follow_up_registered");
    assert_eq!(registered.len(), 1, "{registered:?}");
    assert_eq!(registered[0]["review_attempt"], 4);
}

/// (ADR-t3033-1) A `[review] revise_limit` the landing branch's commit
/// writes but that is not a whole number of at least 1 (here 0) leaves the
/// attempt unpassed: the review fails to a person with why, no job runs and
/// nothing falls back to the default. The other values refused are
/// `infrastructure::run_env`'s unit test.
#[test]
fn a_revise_limit_of_the_landing_branch_that_does_not_parse_fails_the_review() {
    let wiring = Wiring::new(
        "a_revise_limit_of_the_landing_branch_that_does_not_parse_fails_the_review",
        Script {
            receipts: vec![succeeded],
            verdicts: vec![said("pass", &[], "would pass")],
        },
    );
    wiring
        .git
        .commit_files(&[("dagq.toml", "[review]\nrevise_limit = 0\n")], &[]);
    let task = wiring.ready_task(&["true"]);
    wiring.supervise_once();
    assert!(wiring.agent.review_prompts().is_empty());
    let detail = wiring.queue().show(task).unwrap();
    assert_eq!(
        detail.runs[0].status(),
        RunStatus::AwaitingIntegration,
        "{:?}",
        kinds(&detail)
    );
    assert!(payloads(&detail, "review_started").is_empty());
    assert!(payloads(&detail, "review_finished").is_empty());
    let failed = payloads(&detail, "review_failed");
    assert_eq!(failed.len(), 1);
    let error = failed[0]["error"].as_str().unwrap();
    assert!(
        error.contains("parse dagq.toml in the landing branch's commit")
            && error.contains("dagq.toml:2: value of revise_limit")
            && error.contains("positive number, not 0"),
        "{error}"
    );
}

/// Two review agents, `design` and `tests`, both required for
/// `change.txt`; no program review runs.
const TWO_AGENTS: &str = "[review.subagents.design]\npaths = [\"change.txt\"]\n\
                          [review.subagents.tests]\npaths = [\"change.txt\"]\n\
                          [review.programs.\"builtin:secrets\"]\nenabled = false\n";

/// The agents' judgments decide where the run goes, over two rounds, in
/// the run's live session. Round 1: the whole review passes, but `design`
/// recommends `send_back` with high confidence and `tests` asks for a
/// change: the run goes back to the session in one revise with each
/// agent's reasons, and the session's next turn takes it. Round 2: an
/// agent's concern that needs a person (`scope`) beside a whole review
/// that would land on high confidence asks a person, naming the agent and
/// why; the run does not land. The routes and the ask's words of each
/// judgment are `supervise::landing`'s unit tests.
#[test]
fn the_agents_judgments_send_the_run_back_and_then_ask_a_person() {
    let wiring = Wiring::new(
        "the_agents_judgments_send_the_run_back_and_then_ask_a_person",
        Script {
            receipts: vec![succeeded; 3],
            verdicts: vec![
                said("pass", &[], "looks fine"),
                json!({"verdict": "concern", "reasons": [], "summary": "lands",
                       "recommendation": "land", "confidence": "high"}),
            ],
        },
    );
    wiring.git.commit_files(
        &[
            ("dagq.toml", TWO_AGENTS),
            (
                ".dagq/agents/design/AGENT.md",
                "---\ndescription: design checks\n---\nCheck the design.\n",
            ),
            (
                ".dagq/agents/tests/AGENT.md",
                "---\ndescription: test checks\n---\nCheck the tests.\n",
            ),
        ],
        &["change.txt"],
    );
    wiring.agent.review_agent_answers(
        "design",
        vec![
            json!({"verdict": "concern", "reasons": ["departs"],
                   "recommendation": "send_back", "confidence": "high"}),
            json!({"verdict": "concern", "reasons": ["departs from the ADR"],
                   "recommendation": "land", "confidence": "high",
                   "reason_category": "scope"}),
        ],
    );
    wiring
        .agent
        .review_agent_says("tests", &[("revise", &["add a test"]), ("pass", &[])]);
    let task = wiring.ready_task(&["true"]);
    wiring.supervise_once();

    let mut queue = wiring.queue();
    let detail = queue.show(task).unwrap();
    let kinds = kinds(&detail);
    assert_eq!(
        detail.runs[0].status(),
        RunStatus::AwaitingIntegration,
        "{kinds:?}"
    );
    let requested = payloads(&detail, "revise_requested");
    assert_eq!(requested.len(), 1, "{kinds:?}");
    assert_eq!(
        requested[0]["reasons"],
        json!(["design: departs", "tests: add a test"])
    );
    // The revise went to the live session: its next turn, not a resume.
    let sessions = wiring.agent.turn_sessions();
    assert_eq!(sessions.len(), 2, "{sessions:?} {kinds:?}");
    assert!(payloads(&detail, "resume_started").is_empty(), "{kinds:?}");
    assert!(
        told(&wiring, "design: departs") && told(&wiring, "tests: add a test"),
        "{:?}",
        wiring.agent.turn_prompts()
    );
    let finished = payloads(&detail, "review_finished");
    assert_eq!(finished.len(), 2);
    assert_eq!(finished[0]["route"]["destination"], "send_back");
    assert_eq!(finished[0]["route"]["parent_lighter"], true);
    assert_eq!(finished[1]["route"]["destination"], "ask");
    assert_eq!(finished[1]["route"]["parent"], "land");
    // Neither whole review's own judgment decided: nothing to decide.
    assert!(payloads(&detail, "concern_decided").is_empty());
    assert!(payloads(&detail, "integration_approved").is_empty());
    let ask = open_landing_ask(&mut queue);
    for part in [
        "the subagent design returned concern recommending land (high confidence, scope)",
        "the review's own verdict concern was lighter",
        "- design: departs from the ADR",
    ] {
        assert!(ask.question.contains(part), "{part} in {}", ask.question);
    }
}
