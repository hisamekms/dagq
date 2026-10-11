//! Queue tests: every change of a finding records the attributes the
//! report reads of it in the transaction that changes the row, a baseline
//! covers the findings whose records do not carry them, and the
//! measurement's fold of the EventStore (`domain::ledger::findings`) gives
//! the count and the lines the report makes of the store's findings
//! (docs/design/attribute-records.md "findingの属性").
use crate::common;

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use dagq::application::{Clock, Generators, TaskStore};
use dagq::domain::kpi::report::{DiagramSection, Report};
use dagq::domain::kpi::{ConfigReport, Kpi, Period};
use dagq::domain::ledger::findings::{self, Findings, Folded, fold};
use dagq::domain::marks::utc_text;
use dagq::domain::{
    AskKind, AskReason, ClaimOutcome, EventId, FindingAttributes, FindingId, FindingQuery,
    FindingStatus, FindingTarget, GoalId, Impact, NewAsk, NewFinding, NewTask, PlannerOrigin,
    PlannerOwner, ProposalId, Submission, TaskAction, TaskId, event_kind,
};
use dagq::infrastructure::sqlite::SqliteQueue;
use rusqlite::{Connection, params};
use serde_json::Value;

use common::queue::*;

/// A clock the test moves on, so that a new occurrence is seen at a time
/// other than the one before.
struct Stepping(Mutex<SystemTime>);

impl Stepping {
    fn advance(&self, secs: u64) {
        *self.0.lock().unwrap() += Duration::from_secs(secs);
    }
}

impl Clock for Stepping {
    fn system_time(&self) -> SystemTime {
        *self.0.lock().unwrap()
    }

    fn monotonic(&self) -> std::time::Instant {
        std::time::Instant::now()
    }
}

/// The queue on a clock the test moves on.
fn clocked(queue: SqliteQueue) -> (SqliteQueue, Arc<Stepping>) {
    let clock = Arc::new(Stepping(Mutex::new(
        UNIX_EPOCH + Duration::from_secs(1_900_000_000),
    )));
    let ids = queue.generators().ids.clone();
    let queue = queue.with_generators(Generators {
        clock: clock.clone(),
        ids,
    });
    (queue, clock)
}

/// The finding was seen again now, later than `before`: its row, the
/// record of the change and the report's line made of the fold all hold
/// the new time.
fn assert_seen_now(queue: &SqliteQueue, finding: FindingId, clock: &Stepping, before: i64) {
    let now = clock.now();
    assert_ne!(now, before);
    assert_eq!(rows(queue)[&finding].last_seen_at, now);
    assert_recorded(queue, finding, event_kind::FINDING_UPDATED);
    let line = report(&folded(queue).unsettled())
        .findings
        .into_iter()
        .find(|line| line.id == finding.as_i64())
        .unwrap();
    assert_eq!(line.last_seen_at, utc_text(now * 1000));
}

fn new_finding(target: FindingTarget, subject: &str, summary: &str) -> NewFinding {
    NewFinding {
        kind: "conflict_hotspot".into(),
        target,
        subject: subject.into(),
        summary: summary.into(),
        detail: None,
        impact: None,
        evidence: Vec::new(),
        propose: None,
        by: "observer".into(),
    }
}

fn record(queue: &mut SqliteQueue, new: NewFinding) -> FindingId {
    queue.record_finding(new).unwrap().finding.id
}

/// An event that exists, newer than any so far: new evidence.
fn evidence(queue: &SqliteQueue) -> EventId {
    queue.latest_event_id().unwrap()
}

/// Every finding as its row holds the attributes, by id.
fn rows(queue: &SqliteQueue) -> BTreeMap<FindingId, FindingAttributes> {
    queue
        .findings(&FindingQuery {
            all: true,
            ..FindingQuery::default()
        })
        .unwrap()
        .into_iter()
        .map(|view| (view.finding.id, view.finding.attributes()))
        .collect()
}

fn folded(queue: &SqliteQueue) -> Findings {
    match fold(&queue.all_events().unwrap(), None) {
        Folded::Findings(findings) => findings,
        Folded::BeforeBaseline => panic!("before the baseline"),
    }
}

/// A report with no KPIs, so that only its findings tell two apart.
fn report(findings: &[FindingAttributes]) -> Report {
    let kpi = Kpi {
        period: "day",
        utc_offset_secs: 0,
        cores: None,
        config: ConfigReport {
            min_samples: 0,
            breach_periods: 0,
            breach_weeks: 0,
            max_improvement_proposals: 0,
            sources: BTreeMap::new(),
        },
        periods: Vec::new(),
        targets: Vec::new(),
        compare: None,
    };
    Report::new(
        kpi,
        Period::Day,
        0,
        "build",
        findings,
        DiagramSection::not_drawn(Vec::new(), false, "none"),
    )
}

/// The fold gives every finding as its row holds it, and the report made
/// of the folded unsettled findings (`findings_open` and every line) is
/// the one made of what `findings` lists by default.
fn assert_folded_matches_state(queue: &SqliteQueue) {
    let folded = folded(queue);
    assert_eq!(folded.findings, rows(queue));
    let store: Vec<FindingAttributes> = queue
        .findings(&FindingQuery::default())
        .unwrap()
        .into_iter()
        .map(|view| view.finding.attributes())
        .collect();
    let from_store = report(&store);
    let from_records = report(&folded.unsettled());
    assert_eq!(from_records.findings_open, from_store.findings_open);
    assert_eq!(from_records.findings, from_store.findings);
}

/// The newest event of `kind` about `finding` carries the attributes its
/// row holds now.
fn assert_recorded(queue: &SqliteQueue, finding: FindingId, kind: &str) {
    let payload = queue
        .all_events()
        .unwrap()
        .into_iter()
        .rev()
        .find(|event| event.kind == kind && event.payload["finding_id"] == finding.as_i64())
        .unwrap_or_else(|| panic!("no {kind} about finding {finding}"))
        .payload;
    assert_eq!(
        payload[findings::VALUES],
        serde_json::to_value(&rows(queue)[&finding]).unwrap(),
        "{kind} about finding {finding}"
    );
}

/// A draft task of `goal`, waiting for task 1 so it is never claimed.
fn draft(queue: &mut SqliteQueue, goal: GoalId, title: &str) -> TaskId {
    queue
        .add(NewTask {
            goal_id: Some(goal),
            dependencies: vec![TaskId::new(1)],
            ..new_task(title)
        })
        .unwrap()
        .id()
}

/// A person's submission of `task` that remedies `findings`.
fn submit(queue: &mut SqliteQueue, task: TaskId, findings: &[FindingId]) -> ProposalId {
    queue
        .submit_linking(
            Submission {
                tasks: vec![task],
                goals: Vec::new(),
                proposal: None,
                owner: PlannerOwner {
                    origin: PlannerOrigin::Person,
                    workspace_id: Some("P1".into()),
                },
            },
            findings,
        )
        .unwrap()
        .id()
}

/// A task completed (its landing is not what is tested here).
fn complete(db: &Path, task: TaskId) {
    Connection::open(db)
        .unwrap()
        .execute(
            "UPDATE tasks SET status='completed' WHERE id=?1",
            params![task.as_i64()],
        )
        .unwrap();
}

/// A person's answer to a `blocked` ask about `finding`.
fn answer(queue: &mut SqliteQueue, finding: FindingId, text: &str) {
    let ask = queue
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
            finding_id: Some(finding),
            request_id: None,
        })
        .unwrap()
        .ask;
    queue.answer(ask.id, text).unwrap();
}

fn status(queue: &SqliteQueue, finding: FindingId) -> FindingStatus {
    rows(queue)[&finding].status
}

/// Every path that writes a finding's row records its attributes after
/// the change in its own transaction: the creation on each target, a
/// merge that changes the summary, the impact, the occurrences and the
/// last seen time, the reopening of a resolved one, a resolve and a
/// dismissal by hand, the submission that makes it proposed, the end of
/// the proposal that opens it again or resolves it, and a person's
/// `propose` and `dismiss` answers. A record that changes nothing writes
/// nothing. The fold follows the rows at every step.
#[test]
fn every_write_path_records_the_attributes_after_the_change() {
    let (dir, q) = fixture();
    let (mut q, clock) = clocked(q);
    let db = dir.path().join("queue.db");
    let blocker = q.add(new_task("blocker")).unwrap().id();
    let goal = q.add_goal(new_goal("remedies")).unwrap().id();
    let running = q.add(new_task("running")).unwrap().id();
    q.transition(running, TaskAction::BypassReview).unwrap();
    let ClaimOutcome::Claimed { run, .. } = q.claim(&base()).unwrap() else {
        panic!("claim")
    };
    let on_run = record(
        &mut q,
        new_finding(FindingTarget::Run(run.id().clone()), "", "the run waits"),
    );
    assert_recorded(&q, on_run, event_kind::FINDING_RECORDED);
    assert_eq!(
        (rows(&q)[&on_run].run_id.as_ref(), rows(&q)[&on_run].task_id),
        (Some(run.id()), Some(running))
    );
    let on_queue = record(
        &mut q,
        new_finding(FindingTarget::Queue, "src/a.rs", "a conflicts"),
    );
    assert_recorded(&q, on_queue, event_kind::FINDING_RECORDED);
    let on_task = record(
        &mut q,
        new_finding(FindingTarget::Task(blocker), "", "the task waits"),
    );
    assert_recorded(&q, on_task, event_kind::FINDING_RECORDED);
    let on_goal = record(
        &mut q,
        new_finding(FindingTarget::Goal(goal), "", "the goal stalls"),
    );
    assert_recorded(&q, on_goal, event_kind::FINDING_RECORDED);
    assert_eq!(rows(&q)[&on_task].task_id, Some(blocker));
    assert_eq!(rows(&q)[&on_goal].goal_id, Some(goal));
    assert_folded_matches_state(&q);

    // A merge: the summary and the impact, then a new occurrence.
    let merged = q
        .record_finding(NewFinding {
            impact: Some(Impact::High),
            ..new_finding(FindingTarget::Queue, "src/a.rs", "a conflicts often")
        })
        .unwrap();
    assert_eq!(merged.changed, ["summary", "impact"]);
    assert_recorded(&q, on_queue, event_kind::FINDING_UPDATED);
    assert_folded_matches_state(&q);
    let seen = evidence(&q);
    let before = rows(&q)[&on_queue].last_seen_at;
    clock.advance(60);
    q.record_finding(NewFinding {
        evidence: vec![seen],
        ..new_finding(FindingTarget::Queue, "src/a.rs", "a conflicts often")
    })
    .unwrap();
    assert_eq!(rows(&q)[&on_queue].occurrences, 2);
    assert_seen_now(&q, on_queue, &clock, before);
    assert_folded_matches_state(&q);
    // The same record again changes nothing and writes nothing.
    let events = q.all_events().unwrap().len();
    let unchanged = q
        .record_finding(NewFinding {
            evidence: vec![seen],
            ..new_finding(FindingTarget::Queue, "src/a.rs", "a conflicts often")
        })
        .unwrap();
    assert!(unchanged.changed.is_empty());
    assert_eq!(q.all_events().unwrap().len(), events);

    // Resolved by hand, then reopened by a new occurrence.
    q.set_finding_status(on_task, FindingStatus::Resolved, "it moved", "human")
        .unwrap();
    assert_recorded(&q, on_task, event_kind::FINDING_STATUS_CHANGED);
    assert_folded_matches_state(&q);
    let again = evidence(&q);
    let before = rows(&q)[&on_task].last_seen_at;
    clock.advance(60);
    q.record_finding(NewFinding {
        evidence: vec![again],
        ..new_finding(FindingTarget::Task(blocker), "", "the task waits")
    })
    .unwrap();
    assert_eq!(status(&q, on_task), FindingStatus::Open);
    assert_recorded(&q, on_task, event_kind::FINDING_STATUS_CHANGED);
    assert_seen_now(&q, on_task, &clock, before);
    assert_folded_matches_state(&q);

    // Proposed by a submission, open again when its task is canceled.
    let canceled = draft(&mut q, goal, "canceled remedy");
    submit(&mut q, canceled, &[on_queue]);
    assert_eq!(status(&q, on_queue), FindingStatus::Proposed);
    assert_recorded(&q, on_queue, event_kind::FINDING_STATUS_CHANGED);
    assert_folded_matches_state(&q);
    q.transition(canceled, TaskAction::Cancel).unwrap();
    assert_eq!(q.settle_findings().unwrap().len(), 1);
    assert_eq!(status(&q, on_queue), FindingStatus::Open);
    assert_recorded(&q, on_queue, event_kind::FINDING_STATUS_CHANGED);
    assert_folded_matches_state(&q);
    // Proposed again and resolved when its task completes.
    let landed = draft(&mut q, goal, "landed remedy");
    submit(&mut q, landed, &[on_queue]);
    complete(&db, landed);
    q.settle_findings().unwrap();
    assert_eq!(status(&q, on_queue), FindingStatus::Resolved);
    assert_recorded(&q, on_queue, event_kind::FINDING_STATUS_CHANGED);
    assert_folded_matches_state(&q);

    // A person's `propose` opens the resolved one again with its mark; a
    // `dismiss` dismisses the goal's.
    answer(&mut q, on_queue, "propose: split it");
    assert_eq!(status(&q, on_queue), FindingStatus::Open);
    assert_recorded(&q, on_queue, event_kind::FINDING_STATUS_CHANGED);
    assert_recorded(&q, on_queue, event_kind::FINDING_UPDATED);
    assert_folded_matches_state(&q);
    answer(&mut q, on_goal, "dismiss: not ours");
    assert_eq!(status(&q, on_goal), FindingStatus::Dismissed);
    assert_recorded(&q, on_goal, event_kind::FINDING_STATUS_CHANGED);
    assert_folded_matches_state(&q);
    q.set_finding_status(on_task, FindingStatus::Dismissed, "noise", "human")
        .unwrap();
    assert_recorded(&q, on_task, event_kind::FINDING_STATUS_CHANGED);
    assert_folded_matches_state(&q);
}

/// The queue as a binary before the record leaves it: the findings'
/// events carry no attributes.
fn strip_records(db: &Path) {
    Connection::open(db)
        .unwrap()
        .execute(
            "UPDATE run_events SET payload=json_remove(payload,'$.finding')
             WHERE kind IN ('finding_recorded','finding_updated','finding_status_changed')",
            [],
        )
        .unwrap();
}

fn baselines(queue: &SqliteQueue) -> Vec<Value> {
    queue
        .all_events()
        .unwrap()
        .into_iter()
        .filter(|event| event.kind == event_kind::FINDING_BASELINE)
        .map(|event| event.payload)
        .collect()
}

/// The baseline is written once for the findings whose records do not
/// carry their attributes, open, proposed and settled alike, and from it
/// and the later records the fold gives the rows and the report's count
/// and lines: through a merge of the summary and the impact, a new
/// occurrence, open to proposed and back, proposed to resolved, a
/// dismissal and the reopening of a resolved one. A window before the
/// baseline is told apart.
#[test]
fn the_baseline_is_idempotent_and_the_fold_from_it_follows_the_store() {
    let (dir, q) = fixture();
    let (mut q, clock) = clocked(q);
    let db = dir.path().join("queue.db");
    q.add(new_task("blocker")).unwrap();
    let goal = q.add_goal(new_goal("remedies")).unwrap().id();
    let open = record(
        &mut q,
        new_finding(FindingTarget::Queue, "src/a.rs", "a conflicts"),
    );
    let proposed = record(
        &mut q,
        new_finding(FindingTarget::Queue, "src/b.rs", "b conflicts"),
    );
    let remedy = draft(&mut q, goal, "remedy b");
    submit(&mut q, remedy, &[proposed]);
    let resolved = record(
        &mut q,
        new_finding(FindingTarget::Goal(goal), "", "the goal stalls"),
    );
    q.set_finding_status(resolved, FindingStatus::Resolved, "it moved", "human")
        .unwrap();
    let dismissed = record(
        &mut q,
        new_finding(FindingTarget::Queue, "src/c.rs", "c conflicts"),
    );
    q.set_finding_status(dismissed, FindingStatus::Dismissed, "noise", "human")
        .unwrap();
    strip_records(&db);
    // Before the baseline the older findings are not in the records.
    assert!(folded(&q).findings.is_empty());
    assert_eq!(q.record_findings_baseline().unwrap(), Some(4));
    assert_eq!(q.record_findings_baseline().unwrap(), None);
    // The four findings, then the mark that closes the group.
    let written = baselines(&q);
    assert_eq!(written.len(), 5);
    assert_eq!(written[4]["findings"], 4);
    assert_eq!(
        fold(&q.all_events().unwrap(), Some(0)),
        Folded::BeforeBaseline
    );
    assert_folded_matches_state(&q);
    // A finding the queue's own binary made needs none.
    let newer = record(
        &mut q,
        new_finding(FindingTarget::Queue, "src/d.rs", "d conflicts"),
    );
    assert_eq!(q.record_findings_baseline().unwrap(), None);
    assert_folded_matches_state(&q);

    // The summary and the impact merged, then a new occurrence.
    q.record_finding(NewFinding {
        impact: Some(Impact::Low),
        ..new_finding(FindingTarget::Queue, "src/a.rs", "a conflicts less")
    })
    .unwrap();
    assert_folded_matches_state(&q);
    let seen = evidence(&q);
    let before = rows(&q)[&newer].last_seen_at;
    clock.advance(60);
    q.record_finding(NewFinding {
        evidence: vec![seen],
        ..new_finding(FindingTarget::Queue, "src/d.rs", "d conflicts")
    })
    .unwrap();
    assert_seen_now(&q, newer, &clock, before);
    assert_folded_matches_state(&q);
    // Open to proposed, and back when its task is canceled.
    let canceled = draft(&mut q, goal, "remedy a");
    submit(&mut q, canceled, &[open]);
    assert_eq!(status(&q, open), FindingStatus::Proposed);
    assert_folded_matches_state(&q);
    q.transition(canceled, TaskAction::Cancel).unwrap();
    q.settle_findings().unwrap();
    assert_eq!(status(&q, open), FindingStatus::Open);
    assert_folded_matches_state(&q);
    // The older proposed one resolved when its task completes.
    complete(&db, remedy);
    q.settle_findings().unwrap();
    assert_eq!(status(&q, proposed), FindingStatus::Resolved);
    assert_folded_matches_state(&q);
    // A dismissal, and the older resolved one occurring again.
    q.set_finding_status(newer, FindingStatus::Dismissed, "noise", "human")
        .unwrap();
    assert_folded_matches_state(&q);
    let again = evidence(&q);
    let before = rows(&q)[&resolved].last_seen_at;
    clock.advance(60);
    q.record_finding(NewFinding {
        evidence: vec![again],
        ..new_finding(FindingTarget::Goal(goal), "", "the goal stalls")
    })
    .unwrap();
    assert_eq!(status(&q, resolved), FindingStatus::Open);
    assert_seen_now(&q, resolved, &clock, before);
    assert_folded_matches_state(&q);
    // Every change after the baseline carried its record: none to write.
    assert_eq!(q.record_findings_baseline().unwrap(), None);
}

/// What a binary before the record changes without its record leaves the
/// records apart from the row until the next comparison, which writes a
/// baseline of that finding only; then they agree again.
#[test]
fn a_change_an_older_binary_makes_without_its_record_is_baselined_again() {
    let (dir, mut q) = fixture();
    let db = dir.path().join("queue.db");
    let kept = record(
        &mut q,
        new_finding(FindingTarget::Queue, "src/a.rs", "a conflicts"),
    );
    let changed = record(
        &mut q,
        new_finding(FindingTarget::Queue, "src/b.rs", "b conflicts"),
    );
    assert_eq!(q.record_findings_baseline().unwrap(), None);
    Connection::open(&db)
        .unwrap()
        .execute(
            "UPDATE findings SET impact='high', occurrences=3 WHERE id=?1",
            params![changed.as_i64()],
        )
        .unwrap();
    assert_ne!(folded(&q).findings, rows(&q));
    assert_eq!(q.record_findings_baseline().unwrap(), Some(1));
    let written = baselines(&q);
    assert_eq!(written[0]["finding_id"], changed.as_i64());
    assert_eq!(written.len(), 2);
    assert_eq!(q.record_findings_baseline().unwrap(), None);
    assert_eq!(folded(&q).findings[&kept], rows(&q)[&kept]);
    assert_folded_matches_state(&q);
}

/// A record that cannot be written rolls the change of the row back with
/// it, on every path that writes a finding's row: the creation, a merge,
/// a resolve or dismissal by hand, the submission that makes it proposed,
/// the end of its proposal and a person's `propose` and `dismiss`.
#[test]
fn a_record_that_cannot_be_written_rolls_back_the_change_on_every_path() {
    let (dir, mut q) = fixture();
    let db = dir.path().join("queue.db");
    q.add(new_task("blocker")).unwrap();
    let goal = q.add_goal(new_goal("remedies")).unwrap().id();
    let found = record(
        &mut q,
        new_finding(FindingTarget::Queue, "src/a.rs", "a conflicts"),
    );
    let raw = Connection::open(&db).unwrap();
    let refuse = |raw: &Connection| {
        raw.execute_batch(
            "CREATE TRIGGER refuse_the_record BEFORE INSERT ON run_events
             WHEN new.kind IN ('finding_recorded','finding_updated','finding_status_changed')
             BEGIN SELECT RAISE(ABORT, 'refused'); END;",
        )
        .unwrap();
    };
    let allow = |raw: &Connection| {
        raw.execute_batch("DROP TRIGGER refuse_the_record;")
            .unwrap()
    };
    // Each path fails while the record is refused and leaves the rows and
    // the events as they were, then goes through once it is allowed.
    let refused = |q: &mut SqliteQueue, path: &str, write: &dyn Fn(&mut SqliteQueue) -> bool| {
        let (rows_before, events_before) = (rows(q), q.all_events().unwrap().len());
        refuse(&raw);
        assert!(!write(q), "{path} went through a refused record");
        assert_eq!(rows(q), rows_before, "{path}");
        assert_eq!(q.all_events().unwrap().len(), events_before, "{path}");
        allow(&raw);
        assert!(write(q), "{path}");
    };
    refused(&mut q, "creation", &|q| {
        q.record_finding(new_finding(FindingTarget::Queue, "src/b.rs", "b conflicts"))
            .is_ok()
    });
    refused(&mut q, "merge", &|q| {
        q.record_finding(new_finding(
            FindingTarget::Queue,
            "src/a.rs",
            "a conflicts often",
        ))
        .is_ok()
    });
    let remedy = draft(&mut q, goal, "remedy a");
    let owner = PlannerOwner {
        origin: PlannerOrigin::Person,
        workspace_id: Some("P1".into()),
    };
    refused(&mut q, "submission", &|q| {
        q.submit_linking(
            Submission {
                tasks: vec![remedy],
                goals: Vec::new(),
                proposal: None,
                owner: owner.clone(),
            },
            &[found],
        )
        .is_ok()
    });
    assert_eq!(status(&q, found), FindingStatus::Proposed);
    q.transition(remedy, TaskAction::Cancel).unwrap();
    refused(&mut q, "the end of its proposal", &|q| {
        q.settle_findings().is_ok_and(|settled| settled.len() == 1)
    });
    assert_eq!(status(&q, found), FindingStatus::Open);
    refused(&mut q, "resolve", &|q| {
        q.set_finding_status(found, FindingStatus::Resolved, "it moved", "human")
            .is_ok()
    });
    let ask = |q: &mut SqliteQueue| {
        q.ask(NewAsk {
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
        .ask
        .id
    };
    let proposing = ask(&mut q);
    refused(&mut q, "propose", &|q| {
        q.answer(proposing, "propose: split it").is_ok()
    });
    assert_eq!(status(&q, found), FindingStatus::Open);
    let dismissing = ask(&mut q);
    refused(&mut q, "dismiss", &|q| {
        q.answer(dismissing, "dismiss: not ours").is_ok()
    });
    assert_eq!(status(&q, found), FindingStatus::Dismissed);
    assert_folded_matches_state(&q);
}
