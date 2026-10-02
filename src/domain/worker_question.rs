//! Topic codes of a worker's `worker_question` ask (ADR-t947-2): what was
//! left undecided, next to the `reason_category` that says why a person is
//! needed. The worker gives a primary code and any secondary ones with
//! `dagq ask --topic`; the runtime records them as labels for `stats` and
//! `kpi` only (decision 4), keeps a code outside the list below as it was
//! given (decision 5), and never infers one axis from the other (decision
//! 2). The list and its definitions are the design's (ask), so a code is
//! added or removed without an ADR.

/// The topic codes with their definitions, heaviest first: the order that
/// picks the primary code when two triggers came at once.
pub const WORKER_QUESTION_TOPICS: &[(&str, &str)] = &[
    (
        "discard_work",
        "whether to throw away or redo the work done",
    ),
    (
        "adr_conflict",
        "the acceptance or description, or the only way to meet it, contradicts an accepted decision record, a design document or a person's decision, and both cannot hold",
    ),
    (
        "acceptance_conflict",
        "two acceptance criteria of the task, or a criterion and the description, cannot both hold",
    ),
    (
        "acceptance_infeasible",
        "a criterion cannot be met as written because of a fact you found (how a tool behaves, a failure that does not reproduce, permissions)",
    ),
    (
        "out_of_scope_change",
        "meeting the criteria needs a change outside the task's paths, description or verification",
    ),
    (
        "task_overlap",
        "another task in flight, or a change that just landed, overlaps or conflicts with this one",
    ),
    (
        "precondition_missing",
        "what the work starts from (the data to measure, a predecessor's landing, enough samples) is not there yet",
    ),
    (
        "host_environment",
        "a tool, setting or version on the host blocks the work or the landing, and a worker may not change the host",
    ),
    (
        "design_choice",
        "a choice of implementation that touches no criterion, decision record or scope; yours to decide, so it is counted rather than asked",
    ),
    ("other", "none of these; say what it is in the question"),
];

/// The topic an ask counts under when it carries none: every ask opened
/// before topics were kept, and every kind but `worker_question`.
pub const UNLABELED_TOPIC: &str = "unlabeled";

/// The topics as given: trimmed, blank ones dropped, the first one the
/// primary; a code twice is kept once, where it first came.
pub fn normalize_topics(topics: &[String]) -> Vec<String> {
    let mut kept: Vec<String> = Vec::new();
    for topic in topics.iter().map(|topic| topic.trim()) {
        if !topic.is_empty() && !kept.iter().any(|seen| seen == topic) {
            kept.push(topic.to_owned());
        }
    }
    kept
}

/// The codes, comma-separated, for help texts and errors.
pub fn topic_codes() -> String {
    WORKER_QUESTION_TOPICS
        .iter()
        .map(|(code, _)| *code)
        .collect::<Vec<_>>()
        .join(", ")
}

/// A `worker_question` of the run closed without its answer reaching the
/// worker (task 1372): `ask close` (by a person or the inbox) or the
/// runtime closed it, and no `ask_delivered` names it. `closed` is the
/// event that closed it, whose actor says who did.
#[derive(Debug, Clone)]
pub struct ClosedQuestion {
    pub ask_id: i64,
    pub closed: super::RunEvent,
}

/// The run's `worker_question`s closed without their answer delivered and
/// not yet told to the worker (no `stall_nudged` names it as its
/// `closed_ask`), in the order they were closed.
pub fn closed_undelivered(events: &[super::RunEvent]) -> Vec<ClosedQuestion> {
    use super::event_kind::{ASK_ANSWERED, ASK_CLOSED, ASK_DELIVERED, ASK_OPENED, STALL_NUDGED};
    let id_of = |e: &super::RunEvent| e.payload["ask_id"].as_i64();
    let names = |kind: &str, field: &str, id: i64| {
        events
            .iter()
            .any(|e| e.kind == kind && e.payload[field].as_i64() == Some(id))
    };
    let questions: Vec<i64> = events
        .iter()
        .filter(|e| e.kind == ASK_OPENED && e.payload["kind"] == "worker_question")
        .filter_map(id_of)
        .collect();
    let mut closed: Vec<ClosedQuestion> = events
        .iter()
        .filter(|e| {
            e.kind == ASK_CLOSED || (e.kind == ASK_ANSWERED && e.payload["runtime_closed"] == true)
        })
        .filter_map(|e| id_of(e).map(|id| (id, e)))
        .filter(|(id, _)| questions.contains(id))
        .filter(|(id, _)| !names(ASK_DELIVERED, "ask_id", *id))
        .filter(|(id, _)| !names(STALL_NUDGED, "closed_ask", *id))
        .map(|(ask_id, e)| ClosedQuestion {
            ask_id,
            closed: e.clone(),
        })
        .collect();
    closed.dedup_by_key(|c| c.ask_id);
    closed
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{EventId, RunEvent};
    use serde_json::json;

    fn event(id: i64, kind: &str, payload: serde_json::Value) -> RunEvent {
        RunEvent {
            id: EventId::new(id),
            task_id: None,
            goal_id: None,
            run_id: None,
            kind: kind.into(),
            payload,
            created_at: "2026-10-02T00:00:00Z".into(),
            actor: None,
        }
    }

    /// A question closed without `ask_delivered` is one, until a notice
    /// names it; a delivered one, another kind's close and a close the
    /// runtime wrote with an answer of its own are told apart.
    #[test]
    fn only_questions_closed_without_their_answer_delivered_are_told() {
        let opened =
            |id: i64, kind: &str| event(id, "ask_opened", json!({"ask_id": id, "kind": kind}));
        let mut events = vec![
            opened(1, "worker_question"),
            opened(2, "worker_question"),
            opened(3, "stalled"),
            opened(4, "worker_question"),
            event(10, "ask_answered", json!({"ask_id": 1})),
            event(11, "ask_delivered", json!({"ask_id": 1})),
            event(12, "ask_answered", json!({"ask_id": 2})),
            event(13, "ask_closed", json!({"ask_id": 2})),
            event(14, "ask_closed", json!({"ask_id": 3})),
            event(
                15,
                "ask_answered",
                json!({"ask_id": 4, "runtime_closed": true}),
            ),
        ];
        let ids = |events: &[RunEvent]| -> Vec<i64> {
            closed_undelivered(events)
                .iter()
                .map(|c| c.ask_id)
                .collect()
        };
        assert_eq!(ids(&events), [2, 4]);
        assert_eq!(closed_undelivered(&events)[0].closed.id, EventId::new(13));
        events.push(event(16, "stall_nudged", json!({"closed_ask": 2})));
        assert_eq!(ids(&events), [4]);
        assert!(closed_undelivered(&[]).is_empty());
    }

    #[test]
    fn topics_are_trimmed_and_kept_once_in_their_order() {
        let given = [
            "  task_overlap",
            "",
            "adr_conflict ",
            "task_overlap",
            "new_one",
        ]
        .map(str::to_owned);
        assert_eq!(
            normalize_topics(&given),
            ["task_overlap", "adr_conflict", "new_one"]
        );
        assert!(normalize_topics(&[" ".to_owned()]).is_empty());
    }

    #[test]
    fn the_list_names_each_code_once_and_ends_with_other() {
        let codes = topic_codes();
        assert!(codes.starts_with("discard_work, adr_conflict"), "{codes}");
        assert!(codes.ends_with(", other"), "{codes}");
        let mut names: Vec<_> = WORKER_QUESTION_TOPICS.iter().map(|(c, _)| c).collect();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), WORKER_QUESTION_TOPICS.len());
        assert!(!names.contains(&&UNLABELED_TOPIC));
    }
}
