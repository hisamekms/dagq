//! The queue service's proposal and finding use cases
//! (docs/design/queue-service.md, ADR-t1222-1, ADR-t1233-5): every role
//! reads the proposals as `dagq proposal list|show` prints them; the
//! observer records, updates and resolves findings and raises a `blocked`
//! ask on one, written as the observer as the command line writes them,
//! so that the observer's own events still do not wake it
//! (`events_besides`); what is not the observer's, and findings for the
//! worker and the jobs, the service refuses and records on its side.

use crate::common;
use crate::queue_service::{Queue, call, events, queue, start, token};

use common::cli::{invoke_with, ok, submit_from};
use dagq::{
    domain::{
        ActorContext, ActorRole, EventId,
        queue_service::{Principal, UseCase},
    },
    infrastructure::sqlite::SqliteQueue,
};
use serde_json::{Value, json};

fn latest(queue: &Queue) -> EventId {
    SqliteQueue::open(&queue.db)
        .unwrap()
        .latest_event_id()
        .unwrap()
}

/// The events after `after` that are not the observer's own.
fn besides_observer(queue: &Queue, after: EventId) -> i64 {
    SqliteQueue::open(&queue.db)
        .unwrap()
        .events_besides("observer", "observer", after)
        .unwrap()
}

fn succeeded(response: &Value) -> Value {
    assert_eq!(response["ok"], true, "{response}");
    response["result"].clone()
}

fn denied(response: &Value) {
    assert_eq!(
        response["error"]["code"], "authorization_denied",
        "{response}"
    );
}

/// A payload without the fields that differ between two findings.
fn shape(event: &Value) -> Value {
    let mut payload = event["payload"].clone();
    for field in ["finding_id", "id", "subject"] {
        payload.as_object_mut().unwrap().remove(field);
    }
    payload
}

#[test]
fn every_role_reads_the_proposals_as_the_command_line_prints_them() {
    let queue = queue();
    start(&queue);
    let added = ok(&queue.db, &["add", "planned"])["id"].clone();
    let submitted = submit_from(&queue.db, Some("W-1"), None, &[&added.to_string()]);
    assert!(submitted.status.success());
    let observer = token(
        &queue,
        &Principal::of(&ActorContext::instance(ActorRole::Observer, "obs-1")),
    );
    let worker = token(
        &queue,
        &Principal::worker(queue.run.id(), queue.run.task_id()),
    );
    for token in [&observer, &worker] {
        let listed = call(&queue, Some(token), UseCase::ProposalList, json!({}));
        assert_eq!(succeeded(&listed), ok(&queue.db, &["proposal", "list"]));
        let all = call(
            &queue,
            Some(token),
            UseCase::ProposalList,
            json!({"all": true}),
        );
        assert_eq!(
            succeeded(&all),
            ok(&queue.db, &["proposal", "list", "--all"])
        );
        let shown = call(&queue, Some(token), UseCase::ProposalShow, json!({"id": 1}));
        assert_eq!(succeeded(&shown), ok(&queue.db, &["proposal", "show", "1"]));
        assert_eq!(succeeded(&shown)["task_ids"], json!([added]));
    }
    let missing = call(
        &queue,
        Some(&observer),
        UseCase::ProposalShow,
        json!({"id": 99}),
    );
    assert_eq!(missing["error"]["code"], "failed", "{missing}");
}

#[test]
fn the_observer_writes_findings_through_the_service_as_the_command_line_does() {
    let queue = queue();
    start(&queue);
    let actor = ActorContext::instance(ActorRole::Observer, "obs-1");
    let observer = token(&queue, &Principal::of(&actor));
    let task = queue.run.task_id();
    let evidence = latest(&queue).as_i64();
    let before = latest(&queue);

    // A new finding, as the observer whatever the params say.
    let recorded = succeeded(&call(
        &queue,
        Some(&observer),
        UseCase::FindingRecord,
        json!({"kind": "stall", "task": task, "subject": "served", "summary": "stuck",
               "evidence": [evidence], "impact": "high"}),
    ));
    assert_eq!(recorded["created"], true, "{recorded}");
    let id = recorded["id"].as_i64().unwrap();
    // The same kind, target and subject again: nothing new changes
    // nothing; new evidence updates its count and its evidence.
    let again = succeeded(&call(
        &queue,
        Some(&observer),
        UseCase::FindingRecord,
        json!({"kind": "stall", "task": task, "subject": "served", "summary": "stuck",
               "evidence": [evidence]}),
    ));
    assert_eq!(again["id"], id);
    assert_eq!(again["changed"], json!([]), "{again}");
    let newer = latest(&queue).as_i64();
    let updated = succeeded(&call(
        &queue,
        Some(&observer),
        UseCase::FindingRecord,
        json!({"kind": "stall", "task": task, "subject": "served", "summary": "stuck",
               "evidence": [newer]}),
    ));
    assert_eq!(updated["id"], id);
    assert_eq!(updated["created"], false);
    assert_ne!(updated["changed"], json!([]), "{updated}");
    let listed = ok(&queue.db, &["findings", &id.to_string(), "--full"]);
    assert_eq!(listed["findings"][0]["occurrences"], 2, "{listed}");

    // A blocked ask on the finding carries the observer's reading as its
    // recommendation (ADR-t451-1 decision 2): without one the service
    // refuses it with the reason, as the command line does.
    let without = call(
        &queue,
        Some(&observer),
        UseCase::Ask,
        json!({"kind": "blocked", "because": "scope", "question": "stuck: what now?",
               "finding_id": id}),
    );
    assert_ne!(without["ok"], true, "{without}");
    assert!(
        without["error"]["message"]
            .as_str()
            .is_some_and(|message| message.contains("a blocked ask needs --recommend")),
        "{without}"
    );
    let asked = succeeded(&call(
        &queue,
        Some(&observer),
        UseCase::Ask,
        json!({"kind": "blocked", "because": "scope", "question": "stuck: what now?",
               "finding_id": id, "recommend": "propose", "confidence": "high"}),
    ));
    let asks = ok(&queue.db, &["asks", "--all"])["asks"].clone();
    assert_eq!(asks[0]["asked_by"], "observer", "{asks} {asked}");
    assert_eq!(asks[0]["finding_id"], id);

    // What is not the observer's: refused on the service's side and
    // recorded as the observer.
    for (use_case, params) in [
        (UseCase::FindingDismiss, json!({"id": id, "reason": "no"})),
        (UseCase::Note, json!({"task": task, "text": "x"})),
        (
            UseCase::Ask,
            json!({"kind": "blocked", "because": "scope", "question": "q", "task_id": task}),
        ),
        (
            UseCase::Ask,
            json!({"kind": "worker_question", "because": "scope", "topics": ["other"],
                   "question": "q", "run_id": queue.run.id()}),
        ),
    ] {
        denied(&call(&queue, Some(&observer), use_case, params));
    }
    let denials = events(&queue.db, "authorization_denied");
    assert_eq!(denials.len(), 4, "{denials:?}");
    for denial in &denials {
        assert_eq!(denial["actor"]["role"], "observer", "{denial}");
        assert_eq!(denial["actor"]["id"], actor.actor_id(), "{denial}");
    }

    let resolved = succeeded(&call(
        &queue,
        Some(&observer),
        UseCase::FindingResolve,
        json!({"id": id, "reason": "moving again"}),
    ));
    assert_eq!(resolved["status"], "resolved", "{resolved}");

    // Each write is the observer's: `by` / `asked_by` and the actor.
    for kind in [
        "finding_recorded",
        "finding_updated",
        "finding_status_changed",
    ] {
        for event in events(&queue.db, kind) {
            assert_eq!(event["payload"]["by"], "observer", "{event}");
            assert_eq!(event["actor"]["role"], "observer", "{event}");
            assert_eq!(event["actor"]["id"], actor.actor_id(), "{event}");
        }
    }
    let opened = events(&queue.db, "ask_opened");
    assert_eq!(opened[0]["payload"]["asked_by"], "observer", "{opened:?}");
    // ... so none of them wakes the observer.
    assert_eq!(besides_observer(&queue, before), 0);

    // The command line's observer writes the same records.
    let by_cli = invoke_with(
        &[
            ("DAGQ_ROLE", "observer"),
            ("DAGQ_ACTOR_ID", actor.actor_id()),
        ],
        &queue.db,
        &[
            "finding",
            "record",
            "--kind",
            "stall",
            "--task",
            &task.to_string(),
            "--subject",
            "other",
            "--summary",
            "stuck",
            "--impact",
            "high",
            "--evidence",
            &evidence.to_string(),
        ],
    );
    assert!(
        by_cli.status.success(),
        "{}",
        String::from_utf8_lossy(&by_cli.stderr)
    );
    let recorded = events(&queue.db, "finding_recorded");
    assert_eq!(recorded.len(), 2, "{recorded:?}");
    assert_eq!(shape(&recorded[0]), shape(&recorded[1]));
    assert_eq!(recorded[0]["actor"], recorded[1]["actor"]);
    assert_eq!(besides_observer(&queue, before), 0);
}

#[test]
fn the_worker_and_the_jobs_write_no_findings() {
    let queue = queue();
    start(&queue);
    let task = queue.run.task_id();
    let observer = token(
        &queue,
        &Principal::of(&ActorContext::instance(ActorRole::Observer, "obs-1")),
    );
    let id = succeeded(&call(
        &queue,
        Some(&observer),
        UseCase::FindingRecord,
        json!({"kind": "stall", "queue": true, "summary": "slow"}),
    ))["id"]
        .clone();
    let others = [
        Principal::worker(queue.run.id(), task),
        Principal::of(&ActorContext::review_job(queue.run.id(), 1)),
        Principal::of(&ActorContext::plan_review_job(1, 1)),
    ];
    for principal in &others {
        let other = token(&queue, principal);
        for (use_case, params) in [
            (
                UseCase::FindingRecord,
                json!({"kind": "stall", "queue": true, "summary": "slow"}),
            ),
            (UseCase::FindingResolve, json!({"id": id, "reason": "gone"})),
            (UseCase::FindingDismiss, json!({"id": id, "reason": "no"})),
            (
                UseCase::Ask,
                json!({"kind": "blocked", "because": "scope", "question": "q",
                       "finding_id": id}),
            ),
        ] {
            denied(&call(&queue, Some(&other), use_case, params));
        }
    }
    let denials = events(&queue.db, "authorization_denied");
    assert_eq!(denials.len(), 12, "{denials:?}");
    // The finding is as the observer left it.
    let listed = ok(&queue.db, &["findings", &id.to_string()]);
    assert_eq!(listed["findings"][0]["status"], "open", "{listed}");
    // A forged token is no principal at all.
    let forged = call(
        &queue,
        Some(&"0".repeat(64)),
        UseCase::FindingRecord,
        json!({"kind": "stall", "queue": true, "summary": "slow"}),
    );
    assert_eq!(forged["error"]["code"], "unauthenticated", "{forged}");
}
