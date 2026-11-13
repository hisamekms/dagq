//! `ask`: register a question for a person. Nothing is notified here: the
//! `watch --role inbox` that runs in the inbox's session tells the person
//! of each `ask_opened` it sees (ADR-t1433-1 decision 2), and the
//! supervisor, the queue service and the observer call no cmux. The
//! supervisor's own asks take this path too.

use anyhow::Result;
use serde_json::{Value, json};

use super::AskStore;
use crate::domain::{AskOutcome, HoldOutcome, NewAsk, NewHold};

/// Register `ask`. A repeated ask returns the open one (`created: false`).
pub fn ask(queue: &mut (impl AskStore + ?Sized), ask: NewAsk) -> Result<Value> {
    let outcome = queue.ask(ask)?;
    Ok(serde_json::to_value(&outcome)?)
}

/// Open the authentication or cost ask of `hold` with its run, or add the
/// run to the open one (ADR-0047 decision 42). Only a new ask is an
/// `ask_opened` for the inbox's watch to notify; a run that joins it
/// notifies nobody.
pub fn hold(queue: &mut (impl AskStore + ?Sized), hold: NewHold) -> Result<(HoldOutcome, Value)> {
    let outcome = queue.hold(hold)?;
    let mut value = serde_json::to_value(AskOutcome {
        ask: outcome.ask.clone(),
        created: outcome.created,
    })?;
    value["joined"] = json!(outcome.joined);
    Ok((outcome, value))
}

/// The path is authority from authentication, never an interpretation of words.
pub fn answered_via(
    authority: crate::domain::AnswerAuthority,
) -> Option<crate::domain::audit::AnsweredVia> {
    use crate::domain::{AnswerAuthority as A, audit::AnsweredVia as V};
    match authority {
        A::User => Some(V::HumanAttested),
        A::Delegated => Some(V::RelayedByInbox),
        A::Runtime => None,
    }
}

/// Judgment facts at each boundary. Opening captures the question before it can
/// change; answering captures authority and an optional digest of relayed words;
/// applying captures the outcome, including discarded answers.
pub fn judgment(
    ask: &crate::domain::Ask,
    phase: &str,
    answerer: Option<crate::domain::Answerer>,
    failure: Option<&str>,
) -> crate::domain::audit::SenderInput {
    use crate::domain::audit::*;
    let via = answerer
        .map(|a| a.authority)
        .or(ask.answer_authority)
        .and_then(answered_via);
    let answered_by = answerer.map(|a| a.by).or(ask.answered_by.as_deref());
    let option_index = ask
        .answer
        .as_deref()
        .and_then(|text| crate::domain::option_index(&ask.options, text));
    let chosen_option = option_index
        .and_then(|index| usize::try_from(index).ok())
        .and_then(|index| ask.options.get(index).map(String::as_str))
        .or_else(|| {
            ask.answer
                .as_deref()
                .and_then(|text| crate::domain::reasoned_option(&ask.kind, &ask.options, text))
        });
    let original = answerer
        .filter(|a| a.authority == crate::domain::AnswerAuthority::Delegated)
        .and_then(|a| a.original_text_hash)
        .map(|h| h.iter().map(|b| format!("{b:02x}")).collect::<String>());
    SenderInput {
        sender_seq: 0,
        at: String::new(),
        action: if phase == "applied" {
            AuditAction::AnswerApplied
        } else {
            AuditAction::AskAnswered
        },
        resource: Resource {
            kind: "ask".into(),
            id: ask.id.to_string(),
        },
        authority: Authority::default(),
        outcome: Outcome {
            status: if failure.is_some() {
                OutcomeStatus::Failed
            } else {
                OutcomeStatus::Succeeded
            },
            reason: failure.map(str::to_owned),
        },
        change: Some(Change {
            after: Some(json!({
                "phase": phase, "question_hash": digest(&ask.question), "options": ask.options,
                "recommendation": ask.recommendation, "confidence": ask.confidence,
                "answered_by": answered_by, "original_text_hash": original,
                "answer_hash": ask.answer.as_deref().map(digest),
                "option_index": option_index, "chosen_option": chosen_option,
            })),
            ..Change::default()
        }),
        correlation: Correlation {
            task_id: ask.task_id.map(|id| id.as_i64()),
            run_id: ask.run_id.as_ref().map(ToString::to_string),
            request_id: ask.request_id.map(|id| id.as_i64()),
            ..Correlation::default()
        },
        answered_via: via,
        landing: None,
        external: None,
    }
}

/// SHA-256 of the exact UTF-8 text, without normalizing a person's words.
pub fn digest(text: &str) -> String {
    use sha2::{Digest, Sha256};
    format!("{:x}", Sha256::digest(text.as_bytes()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::audit::{AnsweredVia, OutcomeStatus};
    use crate::domain::{AnswerAuthority, Answerer, Ask};

    fn question() -> Ask {
        serde_json::from_value(json!({
            "id": 7, "kind": "approve_landing", "task_id": 3, "run_id": "r1",
            "question": "abc", "options": ["land", "send_back"], "answer": "land",
            "asked_by": "supervisor", "reason_category": "scope",
            "recommendation": "land", "confidence": "high", "created_at": 1,
            "answered_at": 2, "closed_at": null
        }))
        .unwrap()
    }

    #[test]
    fn judgment_keeps_question_facts_and_maps_authority_without_reading_answer_words() {
        assert_eq!(
            serde_json::to_value(answered_via(AnswerAuthority::User)).unwrap(),
            json!("human_attested")
        );
        assert_eq!(
            serde_json::to_value(answered_via(AnswerAuthority::Delegated)).unwrap(),
            json!("relayed_by_inbox")
        );
        let ask = question();
        let opened = judgment(&ask, "opened", None, None);
        let facts = opened.change.unwrap().after.unwrap();
        assert_eq!(
            facts["question_hash"],
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        assert_eq!(facts["options"], json!(["land", "send_back"]));
        assert_eq!(facts["recommendation"], "land");
        assert_eq!(facts["confidence"], "high");
        assert_eq!(facts["option_index"], 0);
        assert_eq!(facts["chosen_option"], "land");
        for (authority, expected) in [
            (AnswerAuthority::User, Some(AnsweredVia::HumanAttested)),
            (
                AnswerAuthority::Delegated,
                Some(AnsweredVia::RelayedByInbox),
            ),
            (AnswerAuthority::Runtime, None),
        ] {
            let input = judgment(
                &ask,
                "answered",
                Some(Answerer {
                    authority,
                    ..Answerer::PERSON
                }),
                None,
            );
            assert_eq!(input.answered_via, expected);
            assert_eq!(
                input.change.unwrap().after.unwrap()["original_text_hash"],
                Value::Null
            );
        }
        let relayed = judgment(
            &ask,
            "answered",
            Some(Answerer {
                original_text_hash: Some([0xab; 32]),
                ..Answerer::INBOX
            }),
            None,
        );
        assert_eq!(
            relayed.change.unwrap().after.unwrap()["original_text_hash"],
            "ab".repeat(32)
        );
        // Even supplied evidence cannot turn direct authority into a relay.
        let direct = judgment(
            &ask,
            "answered",
            Some(Answerer {
                original_text_hash: Some([0xab; 32]),
                ..Answerer::PERSON
            }),
            None,
        );
        assert_eq!(
            direct.change.unwrap().after.unwrap()["original_text_hash"],
            Value::Null
        );
    }

    #[test]
    fn an_application_failure_is_distinct_from_a_successful_application() {
        let mut ask = question();
        ask.answer_authority = Some(AnswerAuthority::User);
        ask.answered_by = Some("person".into());
        let success = judgment(&ask, "applied", None, None);
        assert_eq!(success.answered_via, Some(AnsweredVia::HumanAttested));
        assert_eq!(
            success.change.unwrap().after.unwrap()["answered_by"],
            "person"
        );
        assert_eq!(
            judgment(&ask, "applied", None, None).outcome.status,
            OutcomeStatus::Succeeded
        );
        let failure = judgment(&ask, "applied", None, Some("application_failed"));
        assert_eq!(failure.outcome.status, OutcomeStatus::Failed);
        assert_eq!(failure.answered_via, Some(AnsweredVia::HumanAttested));
        assert_eq!(
            failure.outcome.reason.as_deref(),
            Some("application_failed")
        );
    }
}
