//! Planners the runtime opens for findings (ADR-0044 decision 19) through
//! the supervisor loop: a finding the observer marked for a proposal, or
//! one a person answered `propose` about, gets one planner of the
//! runtime's; its submission makes the finding `proposed` with the
//! proposal, which plan review (a stub provider) readies. The backend (the
//! planners' wrappers parked in the background) and the stub reviewer are
//! plan review's.

use crate::plan_review::{
    PlanWorkspace, StubReviewer, add, events, fixture, open_goal, options, planner_prompt, status,
    supervise, supervise_with,
};
use crate::runtime_support::planner_turns::{exit_requested, idle, turn_requests};
use dagq::{
    application::TaskStore,
    domain::{
        AskKind, AskReason, FindingId, FindingQuery, FindingStatus, FindingTarget, NewAsk,
        NewFinding, NewTask, PlannerOrigin, PlannerOwner, Priority, Submission, TaskAction, TaskId,
        TaskStatus,
    },
    infrastructure::sqlite::SqliteQueue,
    runtime,
};
use rusqlite::Connection;
use serde_json::{Value, json};
use std::{path::Path, time::Duration};

fn record(
    queue: &mut SqliteQueue,
    target: FindingTarget,
    subject: &str,
    propose: Option<&str>,
) -> FindingId {
    queue
        .record_finding(NewFinding {
            kind: "conflict_hotspot".into(),
            target,
            subject: subject.into(),
            summary: format!("{subject} conflicts in most landings"),
            detail: Some("seven of the last ten landings rebased over it".into()),
            impact: None,
            evidence: Vec::new(),
            propose: propose.map(str::to_owned),
            by: "observer".into(),
        })
        .unwrap()
        .finding
        .id
}

fn finding(queue: &SqliteQueue, id: FindingId) -> dagq::domain::Finding {
    queue
        .findings(&FindingQuery {
            id: Some(id),
            ..FindingQuery::default()
        })
        .unwrap()
        .remove(0)
        .finding
}

/// The payloads of the queue's events of `kind` about finding `id`.
fn finding_events(db: &Path, id: FindingId, kind: &str) -> Vec<Value> {
    let connection = Connection::open(db).unwrap();
    let mut statement = connection
        .prepare(
            "SELECT payload FROM run_events WHERE kind=?1
             AND json_extract(payload,'$.finding_id')=?2 ORDER BY id",
        )
        .unwrap();
    statement
        .query_map(rusqlite::params![kind, id.as_i64()], |r| {
            r.get::<_, String>(0)
        })
        .unwrap()
        .map(|payload| serde_json::from_str(&payload.unwrap()).unwrap())
        .collect()
}

/// A draft task of `goal` that waits for the fixture's blocker, so it is
/// never claimed once ready.
fn draft(queue: &mut SqliteQueue, goal: dagq::domain::GoalId, title: &str) -> TaskId {
    draft_at(queue, goal, title, Priority::Normal)
}

fn draft_at(
    queue: &mut SqliteQueue,
    goal: dagq::domain::GoalId,
    title: &str,
    priority: Priority,
) -> TaskId {
    queue
        .add(NewTask {
            change: None,
            title: title.into(),
            description: format!("{title}: split the file"),
            acceptance: format!("{title} is split"),
            verification_commands: vec!["true".into()],
            required_evidence: Vec::new(),
            paths: Vec::new(),
            priority: Some(priority),
            dependencies: vec![TaskId::new(1)],
            goal_dependencies: Vec::new(),
            goal_id: Some(goal),
            context: String::new(),
            provider: None,
            worker_mode: Some(dagq::domain::worker::WorkerMode::Headless),
            wait_for_build: false,
            execution_class: Default::default(),
        })
        .unwrap()
        .id()
}

fn runtime_owner(workspace: &str) -> PlannerOwner {
    PlannerOwner {
        origin: PlannerOrigin::Runtime,
        workspace_id: Some(workspace.into()),
    }
}

#[test]
fn a_marked_finding_gets_one_planner_whose_proposal_plan_review_readies() {
    let fx = fixture();
    let mut queue = SqliteQueue::open(&fx.db).unwrap();
    let goal = open_goal(&mut queue);
    let marked = record(
        &mut queue,
        FindingTarget::Queue,
        "src/main.rs",
        Some("it conflicts again and again"),
    );
    // A finding nobody marked waits for no planner.
    let unmarked = record(&mut queue, FindingTarget::Queue, "src/lib.rs", None);
    let reviewer = StubReviewer::new(&[json!({"verdict": "pass", "reasons": [], "summary": "ok"})]);
    let backend = PlanWorkspace::default();

    supervise(&fx, &backend, &reviewer);
    let planners = queue.planners(false).unwrap();
    assert_eq!(planners.len(), 1, "{planners:?}");
    assert_eq!(planners[0].origin, PlannerOrigin::Runtime);
    assert_eq!(planners[0].finding_id, Some(marked));
    let handle = planners[0].workspace_id.clone().unwrap();
    assert_eq!(backend.launched(), std::slice::from_ref(&handle));
    let prompt = planner_prompt(&fx.db, planners[0].id);
    crate::runtime_support::planner_prompt_bytes::assert_planner_prompt_bytes(
        &fx.db,
        planners[0].id,
        "finding",
        dagq::application::prompt::FINDING_PLANNER_PROMPT_LIMIT,
    );
    for expected in [
        format!("finding {marked} of the queue"),
        "src/main.rs conflicts in most landings".to_owned(),
        "seven of the last ten landings".to_owned(),
        "why a proposal: it conflicts again and again".to_owned(),
        "dagq search".to_owned(),
        format!("--finding {marked}"),
        format!("dagq finding dismiss {marked}"),
        format!("dagq ask --finding {marked} --kind planner_question"),
        "`dagq goal list`".to_owned(),
        "`dagq events --full --task ID`".to_owned(),
        "`dagq timeline RUN`".to_owned(),
    ] {
        assert!(prompt.contains(&expected), "{expected}\n{prompt}");
    }
    assert!(!prompt.contains("events --all"), "{prompt}");
    let opened_events = finding_events(&fx.db, marked, "finding_planner_opened");
    assert_eq!(opened_events.len(), 1);
    assert_eq!(opened_events[0]["attempt"], 1);
    assert!(finding_events(&fx.db, unmarked, "finding_planner_opened").is_empty());

    // Never two for one finding: the next pass, a higher limit and a
    // direct attempt all leave the one planner.
    supervise_with(
        &fx,
        &backend,
        &reviewer,
        &options(3, Duration::from_secs(3600)),
    );
    assert_eq!(queue.planners(false).unwrap().len(), 1);
    assert!(matches!(
        queue.open_finding_planner(marked, None, 2).unwrap(),
        dagq::application::FindingPlannerStart::Skipped
    ));

    // The planner adds a task to the open goal and submits it as its own:
    // the finding is proposed with that proposal.
    let task = draft(&mut queue, goal, "split main");
    let proposal = queue
        .submit_linking(
            Submission {
                tasks: vec![task],
                goals: Vec::new(),
                proposal: None,
                owner: runtime_owner(&handle),
            },
            &[],
        )
        .unwrap();
    let proposed = finding(&queue, marked);
    assert_eq!(proposed.status, FindingStatus::Proposed);
    assert_eq!(proposed.proposal_id, Some(proposal.id()));
    let changed = finding_events(&fx.db, marked, "finding_status_changed");
    assert_eq!(changed.last().unwrap()["to"], "proposed");
    assert_eq!(changed.last().unwrap()["by"], "planner");
    // A finding already proposed with another proposal is not linked again.
    let other = draft(&mut queue, goal, "other");
    let refused = queue
        .submit_linking(
            Submission {
                tasks: vec![other],
                goals: Vec::new(),
                proposal: None,
                owner: runtime_owner("RT9"),
            },
            &[marked],
        )
        .unwrap_err()
        .to_string();
    assert!(refused.contains("proposed"), "{refused}");
    assert_eq!(status(&mut queue, other), TaskStatus::Draft);

    // Plan review passes the proposal, and the planner, idle, is asked to
    // exit.
    idle(&queue, &fx.db, planners[0].id);
    supervise(&fx, &backend, &reviewer);
    assert_eq!(status(&mut queue, task), TaskStatus::Ready);
    assert_eq!(reviewer.prompts().len(), 1);
    assert!(exit_requested(&fx.db, planners[0].id));

    // Its task canceled, the proposal came to nothing: the finding is open
    // again without its mark, and no planner is opened for it until it is
    // marked again.
    queue.transition(task, TaskAction::Cancel).unwrap();
    queue
        .planner_exited(planners[0].id, std::process::id(), 0)
        .unwrap();
    supervise(&fx, &backend, &reviewer);
    supervise(&fx, &backend, &reviewer);
    let reopened = finding(&queue, marked);
    assert_eq!(reopened.status, FindingStatus::Open);
    assert_eq!(
        (reopened.proposal_id, reopened.propose_reason),
        (None, None)
    );
    assert_eq!(
        finding_events(&fx.db, marked, "finding_status_changed")
            .last()
            .unwrap()["reason"],
        "every task of its proposal was canceled"
    );
    assert!(queue.planners(false).unwrap().is_empty());
    assert_eq!(
        finding_events(&fx.db, marked, "finding_planner_opened").len(),
        1
    );
}

/// ADR-0051 decisions 25 and 26: the improvements running (the planners
/// of the runtime's for an open finding and the proposals linked to a
/// finding with a task to do) stop new planners at `[kpi]
/// max_improvement_proposals` of the checkout's `dagq.toml`; the finding
/// waits, `open`, and `findings` says why, until one ends. Plan review's
/// pass takes an improvement's own priority off for its goal's
/// (ADR-t1971-1 decision 4).
#[test]
fn improvement_planners_wait_at_the_limit_and_their_tasks_are_normal_at_most() {
    let fx = fixture();
    std::fs::write(
        fx.repo.join("dagq.toml"),
        "[kpi]\nmax_improvement_proposals = 1\n",
    )
    .unwrap();
    let mut queue = SqliteQueue::open(&fx.db).unwrap();
    let goal = open_goal(&mut queue);
    let first = record(&mut queue, FindingTarget::Queue, "src/a.rs", Some("a"));
    let second = record(&mut queue, FindingTarget::Queue, "src/b.rs", Some("b"));
    let pass = json!({"verdict": "pass", "reasons": [], "summary": "ok"});
    let reviewer = StubReviewer::new(&[pass.clone(), pass]);
    let backend = PlanWorkspace::default();
    let three = options(3, Duration::from_secs(3600));

    // Room for three planners, but one improvement: the oldest mark's.
    supervise_with(&fx, &backend, &reviewer, &three);
    let planners = queue.planners(false).unwrap();
    assert_eq!(planners.len(), 1, "{planners:?}");
    assert_eq!(planners[0].finding_id, Some(first));
    assert!(finding_events(&fx.db, second, "finding_planner_opened").is_empty());
    assert!(matches!(
        queue.open_finding_planner(second, None, 1).unwrap(),
        dagq::application::FindingPlannerStart::AtLimit(dagq::domain::ImprovementLimit {
            running: 1,
            limit: 1
        })
    ));
    let one_shot = dagq::compose::OneShot::new(dagq::infrastructure::clock::system());
    assert_eq!(
        one_shot.improvements_of(&queue).unwrap(),
        json!({"running": 1, "limit": 1, "reached": true,
               "waiting": [{"finding_id": second, "reason": "improvement_limit"}]})
    );

    // Its planner submits a high task: the proposal is the improvement
    // running now, and plan review's pass takes the task's own priority
    // off, so it takes its goal's.
    let mut planner =
        SqliteQueue::open(&fx.db)
            .unwrap()
            .with_actor(dagq::domain::ActorContext::instance(
                dagq::domain::ActorRole::Planner,
                planners[0].id,
            ));
    let task = draft_at(&mut planner, goal, "split a", Priority::High);
    planner
        .submit_linking(
            Submission {
                tasks: vec![task],
                goals: Vec::new(),
                proposal: None,
                owner: runtime_owner(planners[0].workspace_id.as_deref().unwrap()),
            },
            &[],
        )
        .unwrap();
    assert_eq!(finding(&queue, first).status, FindingStatus::Proposed);
    assert_eq!(queue.show(task).unwrap().task.priority(), Priority::High);
    idle(&queue, &fx.db, planners[0].id);
    queue
        .planner_exited(planners[0].id, std::process::id(), 0)
        .unwrap();
    supervise_with(&fx, &backend, &reviewer, &three);
    assert_eq!(status(&mut queue, task), TaskStatus::Ready);
    let inherited = queue.show(task).unwrap().task;
    assert_eq!(
        (inherited.priority(), inherited.own_priority()),
        (Priority::Normal, None)
    );
    let lowered = events(&mut queue, task, "task_priority_changed");
    assert_eq!(lowered.len(), 1, "{lowered:?}");
    assert_eq!(
        (&lowered[0]["from"], &lowered[0]["to"], &lowered[0]["by"]),
        (&json!("high"), &json!("normal"), &json!("plan_review"))
    );
    assert_ne!(lowered[0]["to_source"], "task");
    let finished = events(&mut queue, task, "plan_review_finished");
    assert_eq!(inherited.origin().origin.as_str(), "ai");
    assert_eq!(finished[0]["priorities_inherited"], json!([task]));
    assert!(finding_events(&fx.db, second, "finding_planner_opened").is_empty());
    assert_eq!(finding(&queue, second).status, FindingStatus::Open);

    // The improvement ends: the waiting finding gets its planner.
    queue.transition(task, TaskAction::Cancel).unwrap();
    supervise_with(&fx, &backend, &reviewer, &three);
    supervise_with(&fx, &backend, &reviewer, &three);
    let planners = queue.planners(false).unwrap();
    assert_eq!(planners.len(), 1, "{planners:?}");
    assert_eq!(planners[0].finding_id, Some(second));
    assert_eq!(
        one_shot.improvements_of(&queue).unwrap()["running"],
        json!(1)
    );

    // A proposal a person submits from a planner of their own is not
    // counted.
    let third = record(&mut queue, FindingTarget::Queue, "src/c.rs", None);
    let own = draft_at(&mut queue, goal, "split c", Priority::High);
    queue
        .submit_linking(
            Submission {
                tasks: vec![own],
                goals: Vec::new(),
                proposal: None,
                owner: PlannerOwner {
                    origin: PlannerOrigin::Person,
                    workspace_id: Some("P1".into()),
                },
            },
            &[third],
        )
        .unwrap();
    assert_eq!(finding(&queue, third).status, FindingStatus::Proposed);
    assert_eq!(
        one_shot.improvements_of(&queue).unwrap()["running"],
        json!(1)
    );
}

#[test]
fn a_propose_answer_marks_the_finding_and_a_planner_takes_it() {
    let fx = fixture();
    let mut queue = SqliteQueue::open(&fx.db).unwrap();
    let task = add(&mut queue, "hot", &[TaskId::new(1)], Priority::Normal);
    let found = record(&mut queue, FindingTarget::Task(task), "src/a.rs", None);
    let asked = queue
        .ask(NewAsk {
            recommendation: None,
            confidence: None,
            topics: Vec::new(),
            kind: AskKind::Blocked,
            task_id: None,
            run_id: None,
            question: "src/a.rs keeps conflicting; split it?".into(),
            options: vec!["look at it".into()],
            asked_by: "observer".into(),
            reason_category: AskReason::Scope,
            finding_id: Some(found),
            request_id: None,
        })
        .unwrap()
        .ask;
    // An ask about a finding offers to propose or dismiss it.
    assert_eq!(asked.options, ["look at it", "propose", "dismiss"]);
    let answered = queue.answer(asked.id, "propose: split it in two").unwrap();
    // The runtime applied it: the ask is closed and no attention is left.
    assert!(answered.closed_at.is_some());
    let marked = finding(&queue, found);
    assert_eq!(marked.status, FindingStatus::Open);
    assert_eq!(marked.propose_reason.as_deref(), Some("split it in two"));
    let status_now = runtime::status_for(&fx.db, None).unwrap();
    assert!(
        status_now["attention"]
            .as_array()
            .unwrap()
            .iter()
            .all(|a| a["ask_id"] != asked.id.as_i64()),
        "{status_now}"
    );

    // Another finding answered `dismiss` is dismissed, and no planner is
    // opened for it.
    let other = record(&mut queue, FindingTarget::Queue, "docs", None);
    let dismiss = queue
        .ask(NewAsk {
            recommendation: None,
            confidence: None,
            topics: Vec::new(),
            kind: AskKind::Blocked,
            task_id: None,
            run_id: None,
            question: "docs lag behind".into(),
            options: Vec::new(),
            asked_by: "observer".into(),
            reason_category: AskReason::Scope,
            finding_id: Some(other),
            request_id: None,
        })
        .unwrap()
        .ask;
    queue.answer(dismiss.id, "dismiss").unwrap();
    let dismissed = finding(&queue, other);
    assert_eq!(dismissed.status, FindingStatus::Dismissed);
    assert_eq!(
        dismissed.status_reason.as_deref(),
        Some(format!("a person answered dismiss to ask {}", dismiss.id).as_str())
    );

    // A person's planner asking about a finding the runtime never planned
    // for gets its answer through the inbox, not a runtime planner.
    let mine = queue
        .ask(NewAsk {
            recommendation: None,
            confidence: None,
            topics: Vec::new(),
            kind: AskKind::PlannerQuestion,
            task_id: None,
            run_id: None,
            question: "is docs worth a goal?".into(),
            options: Vec::new(),
            asked_by: "planner".into(),
            reason_category: AskReason::Scope,
            finding_id: Some(other),
            request_id: None,
        })
        .unwrap()
        .ask;
    let answered = queue.answer(mine.id, "no").unwrap();
    assert_eq!(
        queue.planner_answer_route(&answered).unwrap(),
        dagq::application::PlannerAnswerRoute::Person
    );
    queue.close_ask(mine.id).unwrap();

    // A `stalled` ask offers `propose` too: the answer records a finding
    // of its own, marked, and leaves the ask to the stall watch.
    let stalled = queue
        .ask(NewAsk {
            recommendation: None,
            confidence: None,
            topics: Vec::new(),
            kind: AskKind::Stalled,
            task_id: Some(task),
            run_id: None,
            question: "The session is idle without a receipt.\nMore lines.".into(),
            options: vec!["wait".into(), "intervene".into()],
            asked_by: "supervisor".into(),
            reason_category: AskReason::RecoveryFailed,
            finding_id: None,
            request_id: None,
        })
        .unwrap()
        .ask;
    assert_eq!(stalled.options, ["wait", "intervene", "propose"]);
    let answered = queue.answer(stalled.id, "propose").unwrap();
    assert!(answered.closed_at.is_none());
    let own = answered.finding_id.expect("the answer names its finding");
    let own_finding = finding(&queue, own);
    assert_eq!(own_finding.kind, "stalled");
    assert_eq!(own_finding.task_id, Some(task));
    assert_eq!(
        own_finding.summary,
        "The session is idle without a receipt."
    );
    assert_eq!(own_finding.evidence.len(), 1);
    assert_eq!(
        own_finding.propose_reason,
        Some(format!("a person answered propose to ask {}", stalled.id))
    );

    let reviewer = StubReviewer::new(&[json!({"verdict": "pass", "reasons": [], "summary": "ok"})]);
    let backend = PlanWorkspace::default();
    supervise_with(
        &fx,
        &backend,
        &reviewer,
        &options(3, Duration::from_secs(3600)),
    );
    let planners = queue.planners(false).unwrap();
    assert_eq!(
        planners.iter().map(|p| p.finding_id).collect::<Vec<_>>(),
        [Some(found), Some(own)]
    );
    let prompt = planner_prompt(&fx.db, planners[0].id);
    for expected in [
        "src/a.rs keeps conflicting; split it?",
        "answer: propose: split it in two",
        "why a proposal: split it in two",
        "## Goal",
    ] {
        assert!(prompt.contains(expected), "{expected}\n{prompt}");
    }
    assert!(finding_events(&fx.db, other, "finding_planner_opened").is_empty());
}

#[test]
fn a_planner_question_about_a_finding_reaches_its_planner_as_a_turn_and_undecided_planners_end_in_the_inbox()
 {
    let fx = fixture();
    let mut queue = SqliteQueue::open(&fx.db).unwrap();
    let found = record(
        &mut queue,
        FindingTarget::Queue,
        "src/x.rs",
        Some("recurs daily"),
    );
    let reviewer = StubReviewer::new(&[json!({"verdict": "pass", "reasons": [], "summary": "ok"})]);
    let backend = PlanWorkspace::default();
    supervise(&fx, &backend, &reviewer);
    let planner = queue.planners(false).unwrap()[0].clone();

    // It cannot decide and asks about the finding, on no task.
    let asked = queue
        .ask(NewAsk {
            recommendation: None,
            confidence: None,
            topics: Vec::new(),
            kind: AskKind::PlannerQuestion,
            task_id: None,
            run_id: None,
            question: "a new goal for this?".into(),
            options: vec!["propose".into(), "dismiss".into()],
            asked_by: "planner".into(),
            reason_category: AskReason::Scope,
            finding_id: Some(found),
            request_id: None,
        })
        .unwrap()
        .ask;
    idle(&queue, &fx.db, planner.id);
    // Answered while the planner is still there (before a pass ended it
    // for the wait, ADR-t1704-1 decision 2).
    let answered = queue.answer(asked.id, "propose").unwrap();
    // The planner's question is the planner's to act on, not the finding's.
    assert!(answered.closed_at.is_none());
    // The runtime carries it: the inbox sees it delivered, not to deliver.
    let delivered = runtime::status_for(&fx.db, None).unwrap();
    let entry = delivered["attention"]
        .as_array()
        .unwrap()
        .iter()
        .find(|a| a["ask_id"] == asked.id.as_i64())
        .cloned()
        .unwrap();
    assert_eq!(
        entry["next"],
        format!("delivering the answer of ask {} (runtime)", asked.id)
    );
    supervise(&fx, &backend, &reviewer);
    let requests = turn_requests(&fx.db, planner.id);
    assert_eq!(requests.len(), 1, "{requests:?}");
    assert_eq!(
        requests[0]["prompt"],
        format!("answer to ask {}: propose", asked.id)
    );
    assert!(queue.asks(Default::default()).unwrap().is_empty());
    assert!(!exit_requested(&fx.db, planner.id));

    // The planners end without deciding it: another is opened each time,
    // three in all, and then the inbox is told.
    queue
        .planner_exited(planner.id, std::process::id(), 0)
        .unwrap();
    for attempt in 2..=4 {
        supervise(&fx, &backend, &reviewer);
        supervise(&fx, &backend, &reviewer);
        let opened = finding_events(&fx.db, found, "finding_planner_opened");
        assert_eq!(opened.len(), attempt.min(3), "{opened:?}");
        if let Some(open) = queue
            .planners(false)
            .unwrap()
            .into_iter()
            .find(|p| p.finding_id == Some(found))
        {
            queue.register_planner_wrapper(open.id, 1).unwrap();
            queue.register_planner_agent(open.id, 1, 1).unwrap();
            queue.planner_exited(open.id, 1, 0).unwrap();
        }
    }
    supervise(&fx, &backend, &reviewer);
    let exhausted = finding_events(&fx.db, found, "finding_planner_exhausted");
    assert_eq!(exhausted.len(), 1, "{exhausted:?}");
    assert!(queue.planners(false).unwrap().is_empty());
    let status_now = runtime::status_for(&fx.db, None).unwrap();
    let attention: Vec<_> = status_now["attention"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|a| a["kind"] == "finding_planner_exhausted")
        .collect();
    assert_eq!(attention.len(), 1, "{status_now}");
    assert_eq!(attention[0]["next"], "request a plan for the finding");
    let watched = dagq::compose::events(&fx.db, dagq::domain::EventId::new(0), 100, false).unwrap();
    assert!(
        watched["events"].as_array().unwrap().iter().any(|e| {
            e["kind"] == "finding_planner_exhausted"
                && e["next"] == "request a plan for the finding"
        }),
        "{watched}"
    );

    // A person's planner submits the remedy naming the finding.
    let goal = open_goal(&mut queue);
    let task = draft(&mut queue, goal, "remedy");
    let proposal = queue
        .submit_linking(
            Submission {
                tasks: vec![task],
                goals: Vec::new(),
                proposal: None,
                owner: PlannerOwner {
                    origin: PlannerOrigin::Person,
                    workspace_id: None,
                },
            },
            &[found],
        )
        .unwrap();
    assert_eq!(finding(&queue, found).status, FindingStatus::Proposed);
    assert_eq!(finding(&queue, found).proposal_id, Some(proposal.id()));
    assert!(queue.exhausted_findings().unwrap().is_empty());
    assert!(events(&mut queue, task, "task_submitted").len() == 1);
}

/// ADR-t1704-1 decisions 1, 2, 4 and 5 (acceptance (c), (e)): a finding's
/// planner whose question about the finding is all that is left is ended,
/// and its row closes as `runtime_answer_wait`; the finding gets no
/// planner while the question is open, and the answer opens one that
/// carries it, the first by the count since the wait is not counted.
#[test]
fn a_finding_planner_waiting_on_a_person_ends_and_the_answer_opens_the_next() {
    let fx = fixture();
    let mut queue = SqliteQueue::open(&fx.db).unwrap();
    let found = record(
        &mut queue,
        FindingTarget::Queue,
        "src/x.rs",
        Some("recurs daily"),
    );
    let reviewer = StubReviewer::new(&[]);
    let backend = PlanWorkspace::default();
    supervise(&fx, &backend, &reviewer);
    let planner = queue.planners(false).unwrap()[0].clone();
    let asked = queue
        .ask(NewAsk {
            recommendation: None,
            confidence: None,
            topics: Vec::new(),
            kind: AskKind::PlannerQuestion,
            task_id: None,
            run_id: None,
            question: "a new goal for this?".into(),
            options: vec!["propose".into(), "dismiss".into()],
            asked_by: "planner".into(),
            reason_category: AskReason::Scope,
            finding_id: Some(found),
            request_id: None,
        })
        .unwrap()
        .ask;
    idle(&queue, &fx.db, planner.id);
    supervise(&fx, &backend, &reviewer);
    assert!(exit_requested(&fx.db, planner.id));
    queue
        .planner_exited(planner.id, std::process::id(), 0)
        .unwrap();
    supervise(&fx, &backend, &reviewer);
    supervise(&fx, &backend, &reviewer);
    assert!(queue.planners(false).unwrap().is_empty());
    assert!(
        queue
            .asks(Default::default())
            .unwrap()
            .iter()
            .any(|ask| ask.id == asked.id)
    );

    queue.answer(asked.id, "dismiss").unwrap();
    supervise(&fx, &backend, &reviewer);
    let next = queue.planners(false).unwrap();
    assert_eq!(next.len(), 1, "{next:?}");
    assert_eq!(next[0].finding_id, Some(found));
    let prompt = crate::plan_review::planner_prompt(&fx.db, next[0].id);
    assert!(
        prompt.contains(&format!("answer to ask {}: dismiss", asked.id)),
        "{prompt}"
    );
    let opened = finding_events(&fx.db, found, "finding_planner_opened");
    assert_eq!(opened.len(), 2, "{opened:?}");
    assert_eq!(opened[1]["attempt"], 1, "the wait is not counted");
    assert_eq!(opened[1]["ask_id"], asked.id.as_i64());
    assert!(queue.asks(Default::default()).unwrap().is_empty());
}
