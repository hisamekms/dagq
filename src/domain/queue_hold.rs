//! The wait and the answer of the authentication and usage-limit
//! `queue_hold` asks (ADR-0047 decision 42, task 437). While one is open
//! the supervisor claims no new run and starts no headless job, since each
//! would stop at the same wall; the runs in flight keep their leases. The
//! runtime applies the answer: `done` ends the hold, types the fixed
//! [`CONTINUE_TEXT`] into the held sessions and starts again the jobs that
//! failed while the ask was open; `cancel_affected` gives the held runs up
//! as an abandon does. The disk's `cost` ask (`subject: disk`, task 377)
//! is not one: it holds the claims and landings its own way.

use super::claim_hold::{HoldReason, QueueHold};
use super::{Ask, AskKind, AskReason, disk::DISK_SUBJECT};

/// The `subject` of the `cost` ask about Claude Code's usage limit.
pub const USAGE_LIMIT_SUBJECT: &str = "usage_limit";

/// The answer that ends the hold: a person logged in, or the limit is back.
pub const DONE: &str = "done";
/// The answer that gives the held runs up.
pub const CANCEL_AFFECTED: &str = "cancel_affected";

/// Recorded on the queue when the supervisor applied the answer of a
/// `queue_hold` ask (`ask_id`, `answer`, `reason_category`, `subject`,
/// `continued`, `released`, `moved_on`, `restarted`, `elsewhere`, `supervisor`).
pub const QUEUE_HOLD_APPLIED: &str = super::event_kind::QUEUE_HOLD_APPLIED;
/// Recorded on a held run when the fixed text to go on was typed into its
/// session after `done` (`ask_id`, `workspace_id`, `submitted`).
pub const HOLD_CONTINUE_SENT: &str = super::event_kind::HOLD_CONTINUE_SENT;
/// Recorded on a `failed` or `interrupted` run whose recovery job (the
/// triage) failed while the ask was open, when the runtime has it recovered
/// again after `done` (`job: triage`, `ask_id`, the failure's `event_id`):
/// the run's triage is due again ([`super::triage_state`]).
pub const JOB_RESTARTED: &str = super::event_kind::JOB_RESTARTED;

/// The fixed text typed into a session held by the ask once a person
/// answered `done` (the send is checked as every typed text is: ADR-0047
/// decision 31).
pub const CONTINUE_TEXT: &str = "The login or the usage limit that stopped this session is fixed now (a person answered the queue's hold ask `done`). Continue the task from where you stopped: run again what failed with the authentication or usage-limit error, then commit and write the receipt as the task says.";

/// The hold of `ask`, when it is an open authentication or usage-limit
/// `queue_hold` ask; the disk's `cost` ask is none.
pub fn hold_of(ask: &Ask) -> Option<QueueHold> {
    let reason = reason_of(ask)?;
    ask.is_open().then(|| QueueHold {
        reason,
        ask_id: ask.id.as_i64(),
        affected: ask.affected.len(),
    })
}

/// Whether `ask` is an authentication or usage-limit `queue_hold` ask,
/// open or not, and which hold it is.
pub fn reason_of(ask: &Ask) -> Option<HoldReason> {
    if ask.kind != AskKind::QueueHold {
        return None;
    }
    match (ask.reason_category, ask.subject.as_deref()) {
        (AskReason::Authentication, _) => Some(HoldReason::Authentication),
        (AskReason::Cost, Some(USAGE_LIMIT_SUBJECT)) => Some(HoldReason::UsageLimit),
        (AskReason::Cost, Some(DISK_SUBJECT)) => None,
        // A cost ask about something else holds like the usage limit:
        // its runs stopped at a wall only a person moves.
        (AskReason::Cost, _) => Some(HoldReason::UsageLimit),
        _ => None,
    }
}

/// Whether the runtime applies `answer` to the `queue_hold` ask with
/// `options`: one of them, as offered (the disk's `done` / `wait` too).
pub fn applies(options: &[String], answer: &str) -> bool {
    options.iter().any(|option| option == answer.trim())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::AskId;

    fn ask(reason: AskReason, subject: Option<&str>) -> Ask {
        Ask {
            id: AskId::new(4),
            kind: AskKind::QueueHold,
            task_id: None,
            run_id: None,
            question: "q".into(),
            options: vec![DONE.into(), CANCEL_AFFECTED.into()],
            answer: None,
            asked_by: "supervisor".into(),
            reason_category: reason,
            subject: subject.map(str::to_owned),
            affected: vec!["r1".into(), "r2".into()],
            created_at: 0,
            answered_at: None,
            closed_at: None,
            finding_id: None,
            answered_by: None,
            option_index: None,
            answer_authority: None,
            answer_approval: None,
        }
    }

    #[test]
    fn authentication_and_usage_limit_asks_hold_while_open_and_the_disk_does_not() {
        let auth = ask(AskReason::Authentication, None);
        assert_eq!(
            hold_of(&auth),
            Some(QueueHold {
                reason: HoldReason::Authentication,
                ask_id: 4,
                affected: 2,
            })
        );
        let limit = ask(AskReason::Cost, Some(USAGE_LIMIT_SUBJECT));
        assert_eq!(hold_of(&limit).unwrap().reason, HoldReason::UsageLimit);
        assert_eq!(
            hold_of(&ask(AskReason::Cost, None)).unwrap().reason,
            HoldReason::UsageLimit
        );
        assert_eq!(hold_of(&ask(AskReason::Cost, Some(DISK_SUBJECT))), None);
        let answered = Ask {
            answered_at: Some(1),
            answer: Some("done".into()),
            ..auth.clone()
        };
        assert_eq!(hold_of(&answered), None);
        assert_eq!(reason_of(&answered), Some(HoldReason::Authentication));
        let other = Ask {
            kind: AskKind::Blocked,
            ..auth
        };
        assert_eq!(reason_of(&other), None);
    }

    #[test]
    fn only_an_offered_option_is_applied() {
        let options = vec![DONE.to_owned(), CANCEL_AFFECTED.to_owned()];
        assert!(applies(&options, "done"));
        assert!(applies(&options, " cancel_affected\n"));
        assert!(!applies(&options, "logged in, go on"));
    }
}
