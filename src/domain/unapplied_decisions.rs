//! A person's decisions on a run that never reached its session
//! (ADR-t4018-1): the `send_back` of its landing and the answers to its
//! `worker_question` and `decide` asks, which a retry (`retry`,
//! `retry_inherit`, by the runtime, the recovery job or a person's
//! `ready --inherit`) would otherwise lose, since the next run starts a new
//! session. Whether one reached the session is judged by the records of
//! sending it only: a `send_back` by a `resume_request_sent` after it (the
//! resume's request went to the session), an answer by an `ask_delivered`
//! naming its ask. A record that could not be written counts as not sent,
//! so a decision is handed over twice rather than lost.

use serde::Serialize;

use super::{
    EventKind, ReasonCode, RunEvent, RunId,
    run::{AskOpened, restore_payload},
};

/// What kind of a person's decision it is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DecisionKind {
    /// The `send_back` answer of an `approve_landing` ask, its text the
    /// reason the `landing_decided` recorded.
    SendBack,
    /// The answer of a `worker_question`.
    WorkerQuestion,
    /// The answer of a `decide` ask.
    Decide,
}

impl DecisionKind {
    /// The kind of the ask it answers.
    pub const fn ask_kind(self) -> &'static str {
        match self {
            Self::SendBack => "approve_landing",
            Self::WorkerQuestion => "worker_question",
            Self::Decide => "decide",
        }
    }

    /// The event kind whose payload holds its whole text: what
    /// `dagq events --full --task <ID> --kind <KIND>` prints it with.
    pub const fn event_kind(self) -> EventKind {
        match self {
            Self::SendBack => EventKind::LandingDecided,
            Self::WorkerQuestion | Self::Decide => EventKind::AskAnswered,
        }
    }
}

/// One decision of a person that never reached the run's session.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct UnappliedDecision {
    /// The ask it answered; `None` only for a `landing_decided` that names
    /// none.
    pub ask_id: Option<i64>,
    pub kind: DecisionKind,
    /// When it was recorded (the event's `created_at`).
    pub at: String,
    /// Its text as recorded; empty when the event holds none (an answer
    /// recorded before its event carried the text, which the caller takes
    /// from the ask itself).
    pub text: String,
    /// Whether its event holds the text, so that reading the event prints
    /// it whole; `false` once the caller took the text from the ask.
    pub in_event: bool,
    /// The run it was made on (the event's run).
    pub run_id: Option<RunId>,
}

/// The decisions of a person on the run of `events` (oldest first) that
/// never reached its session, in the order they were recorded: each
/// `landing_decided` with code `sent_back` that no later
/// `resume_request_sent` follows (a `resume_started` alone does not count:
/// the resume may fail before its request is sent), and each answer to a
/// `worker_question` or `decide` ask of the run that no `ask_delivered`
/// names. An answer the runtime gave to close an ask is no person's
/// decision and is left out.
pub fn unapplied_decisions(events: &[RunEvent]) -> Vec<UnappliedDecision> {
    let ask_id = |event: &RunEvent| event.payload["ask_id"].as_i64();
    let delivered = |id: i64| {
        events
            .iter()
            .any(|e| e.kind == EventKind::AskDelivered && ask_id(e) == Some(id))
    };
    let asked: Vec<(i64, DecisionKind)> = events
        .iter()
        .filter(|e| e.kind == EventKind::AskOpened)
        .filter_map(|e| {
            let kind = match restore_payload::<AskOpened>(&e.payload).kind {
                Some("worker_question") => DecisionKind::WorkerQuestion,
                Some("decide") => DecisionKind::Decide,
                _ => return None,
            };
            Some((ask_id(e)?, kind))
        })
        .collect();
    events
        .iter()
        .enumerate()
        .filter_map(|(at, event)| match event.kind.known() {
            Some(EventKind::LandingDecided)
                if event.payload["code"] == ReasonCode::SentBack.as_str() =>
            {
                let sent = events[at + 1..]
                    .iter()
                    .any(|later| later.kind == EventKind::ResumeRequestSent);
                (!sent).then(|| decision(event, ask_id(event), DecisionKind::SendBack, "reason"))
            }
            Some(EventKind::AskAnswered) => {
                let id = ask_id(event)?;
                let (_, kind) = asked.iter().find(|(asked, _)| *asked == id)?;
                let by_runtime = event.payload["runtime_closed"] == true
                    || event.payload["authority"] == "runtime";
                (!by_runtime && !delivered(id)).then(|| decision(event, Some(id), *kind, "answer"))
            }
            _ => None,
        })
        .collect()
}

/// The decision `event` records, its text the payload's `field`.
fn decision(
    event: &RunEvent,
    ask_id: Option<i64>,
    kind: DecisionKind,
    field: &str,
) -> UnappliedDecision {
    let text = text_of(&event.payload[field]);
    UnappliedDecision {
        ask_id,
        kind,
        at: event.created_at.clone(),
        in_event: !text.is_empty(),
        text,
        run_id: event.run_id.clone(),
    }
}

/// Whether a session of the run of `events` took its prompt in: one of its
/// turns succeeded. A wrapper that was launched (`wrapper_launched`) may
/// still fail to register or to start its agent before any turn ran, so
/// that alone is no sign the prompt reached anyone.
pub fn prompt_delivered(events: &[RunEvent]) -> bool {
    events.iter().any(|e| {
        e.kind == EventKind::TurnFinished
            && e.payload["outcome"] == super::turn::TurnOutcome::Succeeded.as_str()
    })
}

fn text_of(value: &serde_json::Value) -> String {
    value.as_str().unwrap_or_default().trim().to_owned()
}

/// Who retried a run for a new one of its task.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RetriedBy {
    /// The runtime, its resumes used up on conflicts with main after its
    /// review passed (ADR-0047 decision 24).
    RuntimeOnConflicts,
    /// The recovery job's verdict, or the runtime applying one a hold
    /// kept.
    RecoveryJob,
    /// A person (`user` or `inbox`) by hand (`ready --inherit`).
    ByHand(String),
    /// A person's answer to the recovery's `decide` ask.
    Answer,
}

/// How a run ended for a new run of its task: retried, with its branch
/// carried over or not, by whom and why.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RetryEnd {
    /// Whether the next run carries its branch over (`retry_inherit`).
    pub inherit: bool,
    pub by: RetriedBy,
    /// The reason the retry recorded; empty when it recorded none.
    pub reason: String,
}

/// The retry that ended the run of `events`: its latest `triage_finished`
/// or `triage_decided`, when that is a `retry` or a `retry_inherit`.
pub fn retry_end(events: &[RunEvent]) -> Option<RetryEnd> {
    let event = events.iter().rev().find(|e| {
        matches!(
            e.kind.known(),
            Some(EventKind::TriageFinished | EventKind::TriageDecided)
        )
    })?;
    let payload = &event.payload;
    let reason = text_of(&payload["reason"]);
    if event.kind == EventKind::TriageDecided {
        return (payload["answer"] == "retry").then_some(RetryEnd {
            inherit: false,
            by: RetriedBy::Answer,
            reason,
        });
    }
    let inherit = match payload["action"].as_str() {
        Some(super::resume::RETRY_INHERIT) => true,
        Some("retry") => false,
        _ => return None,
    };
    let by = match payload["by"].as_str() {
        Some(role @ ("user" | "inbox")) => RetriedBy::ByHand(role.to_owned()),
        Some("runtime") if payload["code"] == ReasonCode::ResumeExhausted.as_str() => {
            RetriedBy::RuntimeOnConflicts
        }
        _ => RetriedBy::RecoveryJob,
    };
    Some(RetryEnd {
        inherit,
        by,
        reason,
    })
}

#[cfg(test)]
mod tests {
    use serde_json::{Value, json};

    use super::*;
    use crate::domain::EventId;

    fn event(kind: &str, payload: Value) -> RunEvent {
        RunEvent {
            id: EventId::new(0),
            task_id: None,
            goal_id: None,
            run_id: None,
            kind: kind.into(),
            payload,
            created_at: "2026-11-02T11:50:00Z".into(),
            actor: None,
        }
    }

    fn sent_back(reason: &str) -> RunEvent {
        event(
            "landing_decided",
            json!({"code": "sent_back", "ask_id": 1424, "answer": "send_back",
                   "reason": reason, "status": "needs_session"}),
        )
    }

    fn asked(id: i64, kind: &str) -> RunEvent {
        event(
            "ask_opened",
            json!({"ask_id": id, "kind": kind, "asked_by": "worker"}),
        )
    }

    fn answered(id: i64, kind: &str, answer: &str) -> RunEvent {
        event(
            "ask_answered",
            json!({"ask_id": id, "kind": kind, "answer": answer, "authority": "person"}),
        )
    }

    /// (a) A `send_back` the run never resumed after, its resumes used up
    /// and the run sent to the recovery: its reason is returned.
    #[test]
    fn a_send_back_with_no_resume_after_it_is_unapplied() {
        let events = [
            sent_back("keep the old flag"),
            event("triage_finished", json!({"action": "retry_inherit"})),
        ];
        assert_eq!(
            unapplied_decisions(&events),
            [UnappliedDecision {
                ask_id: Some(1424),
                kind: DecisionKind::SendBack,
                at: "2026-11-02T11:50:00Z".into(),
                text: "keep the old flag".into(),
                in_event: true,
                run_id: None,
            }]
        );
    }

    /// (b) A resume that started after the `send_back` but failed before
    /// its request was sent did not deliver it.
    #[test]
    fn a_send_back_whose_resume_started_but_sent_no_request_is_unapplied() {
        let events = [
            sent_back("keep the old flag"),
            event("resume_started", json!({"attempt": 3})),
            event("resume_finished", json!({"outcome": "failed"})),
            event("triage_finished", json!({"action": "retry_inherit"})),
        ];
        let decisions = unapplied_decisions(&events);
        assert_eq!(decisions.len(), 1, "{decisions:?}");
        assert_eq!(decisions[0].text, "keep the old flag");
    }

    /// (c) A resume that sent its request after the `send_back` delivered
    /// it; only a later, unsent one is returned.
    #[test]
    fn a_send_back_a_resume_request_followed_is_applied() {
        let events = [
            sent_back("first"),
            event("resume_started", json!({})),
            event("resume_request_sent", json!({"head": "abc"})),
        ];
        assert!(unapplied_decisions(&events).is_empty());
        let mut later = events.to_vec();
        later.push(sent_back("second"));
        let decisions = unapplied_decisions(&later);
        assert_eq!(decisions.len(), 1, "{decisions:?}");
        assert_eq!(decisions[0].text, "second");
    }

    /// (d) An answer to a `worker_question` or a `decide` ask the run ended
    /// without delivering is returned; (e) one delivered is not, nor one
    /// the runtime gave, nor an answer to another kind of ask.
    #[test]
    fn answers_no_ask_delivered_names_are_unapplied() {
        let events = [
            asked(7, "worker_question"),
            answered(7, "worker_question", "use the second option"),
            asked(8, "decide"),
            answered(8, "decide", "retry: start from main"),
            asked(9, "worker_question"),
            answered(9, "worker_question", "delivered already"),
            event("ask_delivered", json!({"ask_id": 9, "workspace_id": "w"})),
            asked(10, "decide"),
            event(
                "ask_answered",
                json!({"ask_id": 10, "kind": "decide", "answer": "closed",
                       "runtime_closed": true, "authority": "runtime"}),
            ),
            asked(11, "approve_close"),
            answered(11, "approve_close", "close"),
        ];
        let decisions = unapplied_decisions(&events);
        let got: Vec<(Option<i64>, DecisionKind, &str)> = decisions
            .iter()
            .map(|d| (d.ask_id, d.kind, d.text.as_str()))
            .collect();
        assert_eq!(
            got,
            [
                (
                    Some(7),
                    DecisionKind::WorkerQuestion,
                    "use the second option"
                ),
                (Some(8), DecisionKind::Decide, "retry: start from main"),
            ]
        );
        assert_eq!(
            DecisionKind::SendBack.event_kind(),
            EventKind::LandingDecided
        );
        assert_eq!(DecisionKind::Decide.event_kind(), EventKind::AskAnswered);
    }

    /// The retry that ended a run, and who made it: the runtime on
    /// conflicts, the recovery job, a person by hand or by answer; none for
    /// a run that ended otherwise.
    #[test]
    fn the_retry_that_ended_a_run_says_who_and_why() {
        let finished = |payload: Value| retry_end(&[event("triage_finished", payload)]);
        assert_eq!(
            finished(json!({"action": "retry_inherit", "by": "runtime",
                            "code": "resume_exhausted", "reason": "used up"})),
            Some(RetryEnd {
                inherit: true,
                by: RetriedBy::RuntimeOnConflicts,
                reason: "used up".into(),
            })
        );
        assert_eq!(
            finished(json!({"action": "retry_inherit", "reason": "the person sent it back"}))
                .map(|end| end.by),
            Some(RetriedBy::RecoveryJob)
        );
        assert_eq!(
            finished(json!({"action": "retry", "by": "inbox", "reason": "x"})),
            Some(RetryEnd {
                inherit: false,
                by: RetriedBy::ByHand("inbox".into()),
                reason: "x".into(),
            })
        );
        assert_eq!(finished(json!({"action": "resume"})), None);
        assert_eq!(
            retry_end(&[event(
                "triage_decided",
                json!({"ask_id": 3, "answer": "retry", "reason": "r"})
            )])
            .map(|end| end.by),
            Some(RetriedBy::Answer)
        );
        assert_eq!(retry_end(&[]), None);
    }

    /// An answer recorded before its event carried the text is returned
    /// with an empty text, for the caller to take from the ask.
    #[test]
    fn an_answer_whose_event_holds_no_text_is_returned_for_the_ask_to_fill() {
        let events = [
            asked(7, "worker_question"),
            event(
                "ask_answered",
                json!({"ask_id": 7, "kind": "worker_question", "authority": "person"}),
            ),
        ];
        let decisions = unapplied_decisions(&events);
        assert_eq!(decisions.len(), 1, "{decisions:?}");
        assert_eq!(decisions[0].text, "");
        assert!(!decisions[0].in_event);
    }

    /// Only a turn that succeeded shows the prompt reached a session: a
    /// launched wrapper whose first turn failed or never ran does not.
    #[test]
    fn only_a_succeeded_turn_shows_the_prompt_was_taken_in() {
        let launched = event("wrapper_launched", json!({"prompt_bytes": 10}));
        assert!(!prompt_delivered(std::slice::from_ref(&launched)));
        let failed = event(
            "turn_finished",
            json!({"outcome": "failed", "failure": "launch"}),
        );
        assert!(!prompt_delivered(&[launched.clone(), failed]));
        let succeeded = event("turn_finished", json!({"outcome": "succeeded"}));
        assert!(prompt_delivered(&[launched, succeeded]));
    }
}
