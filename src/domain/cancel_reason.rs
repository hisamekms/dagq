//! The reason a task's cancel records (ADR-t947-4): the code the actor who
//! cancels gives, the one the runtime gives on its own paths, and the one
//! read off a cancel recorded before reasons were kept. A code not among
//! the ones below is recorded and read as it is (ADR-t876-1 decision 3),
//! and adding or removing one needs no ADR. The codes, chosen by where the
//! task's content is now:
//!
//! - `duplicate`, `already_done`, `absorbed`, `re_registered`: another task
//!   holds it, and `--duplicate-of` names that task (the constants below).
//! - `superseded`: a decision or policy it rested on changed and it is no
//!   longer needed; `--note` names the decision, and `--duplicate-of` the
//!   successor if any.
//! - `not_worth`: its content is right but not worth its change and checks.
//! - `decision_moot`: a task that asks for a decision whose answer is
//!   already out, or that is no longer to be decided.
//! - `not_repo_work`: not a change to the repository, but work a person or
//!   the inbox does on the host or outside, or only an observation.
//! - `stale`: its premise (a role, file or command) is gone, with no
//!   successor.
//! - `other`: none of these; `--note` says why.
//! - `answered_cancel`: the runtime applied an ask's `cancel` answer; no
//!   actor gives it.
//!
//! A cancel's `task_status_changed` carries `reason`, and `duplicate_of`
//! (the task that holds its content), `note`, and on the runtime's paths
//! `by` with the `proposal_id` or `ask_id` it applied, when it has them. Who
//! canceled is the event's actor. A goal's `abandoned` close cancels none of
//! its tasks, so it gives no reason.

use serde::Deserialize;
use serde_json::{Map, Value, json};

use super::run::payload::{restore, typed};
use super::{AskId, AskKind, DomainError, ProposalId, TaskId, TaskStatus};

/// Another task with the same content is open (`--duplicate-of` names it).
pub const DUPLICATE: &str = "duplicate";
/// The content is already done by a landed task (`--duplicate-of` names it).
pub const ALREADY_DONE: &str = "already_done";
/// The content moved to another task (`--duplicate-of` names it).
pub const ABSORBED: &str = "absorbed";
/// The same intent registered again under a new ID (`--duplicate-of` names it).
pub const RE_REGISTERED: &str = "re_registered";
/// None of the codes; the note says why.
pub const OTHER: &str = "other";
/// The runtime applied an ask's `cancel` answer; no actor gives it.
pub const ANSWERED_CANCEL: &str = "answered_cancel";
/// How the statistics count a cancel that recorded no reason and names no
/// task; never recorded.
pub const UNRECORDED: &str = "unrecorded";

/// The codes whose content another task holds: each names it.
const NEEDS_TARGET: [&str; 4] = [DUPLICATE, ALREADY_DONE, ABSORBED, RE_REGISTERED];

/// The runtime's path that applies a cancel itself (ADR-t947-4 decision 3).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CancelPath {
    /// A plan review's `cancel_duplicate` action on this proposal: the
    /// reason comes from the named task's status, as for `--duplicate-of`.
    PlanReview(ProposalId),
    /// The `cancel` answer to `ask`, an ask of kind `by` (`approve_plan`,
    /// `approve_landing`, `decide` or `approve_close`): it records
    /// [`ANSWERED_CANCEL`] with the ask.
    Answer { by: AskKind, ask: AskId },
}

/// A cancel as asked for, before the store reads the task it names.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct NewCancel {
    pub reason: Option<String>,
    pub duplicate_of: Option<TaskId>,
    pub note: Option<String>,
    /// None for an actor's `cancel`.
    pub path: Option<CancelPath>,
}

impl NewCancel {
    /// An actor's `cancel`: `reason`, the task that holds the content, and
    /// a note.
    pub fn by_hand(
        reason: Option<String>,
        duplicate_of: Option<TaskId>,
        note: Option<String>,
    ) -> Self {
        Self {
            reason,
            duplicate_of,
            note,
            path: None,
        }
    }

    /// A plan review's `cancel_duplicate` of a task of `proposal`: the
    /// reason comes from `duplicate_of`'s status, as for `--duplicate-of`.
    pub fn plan_review(duplicate_of: TaskId, proposal: ProposalId) -> Self {
        Self {
            duplicate_of: Some(duplicate_of),
            path: Some(CancelPath::PlanReview(proposal)),
            ..Self::default()
        }
    }

    /// The `cancel` answer to `ask`, of kind `by`, that the runtime applies.
    pub fn answered(by: AskKind, ask: AskId) -> Self {
        Self {
            reason: Some(ANSWERED_CANCEL.to_owned()),
            path: Some(CancelPath::Answer { by, ask }),
            ..Self::default()
        }
    }

    /// What the cancel records, with `target` the status of `duplicate_of`
    /// now (none without one). Without a reason, `duplicate_of` gives it:
    /// `already_done` for a completed task, else `duplicate`. Refused
    /// without either, for a code of [`NEEDS_TARGET`] without a task, for
    /// `other` without a note, for `answered_cancel` from an actor and for
    /// [`UNRECORDED`] from anyone.
    pub fn record(&self, target: Option<TaskStatus>) -> Result<CancelRecord, DomainError> {
        let given = self
            .reason
            .as_deref()
            .map(str::trim)
            .filter(|reason| !reason.is_empty());
        let reason = match (given, self.duplicate_of) {
            (Some(reason), _) => reason.to_owned(),
            (None, Some(_)) if target == Some(TaskStatus::Completed) => ALREADY_DONE.to_owned(),
            (None, Some(_)) => DUPLICATE.to_owned(),
            (None, None) => return Err(DomainError::CancelReasonMissing),
        };
        if (self.path.is_none() && reason == ANSWERED_CANCEL) || reason == UNRECORDED {
            return Err(DomainError::CancelReasonReserved { reason });
        }
        if NEEDS_TARGET.contains(&reason.as_str()) && self.duplicate_of.is_none() {
            return Err(DomainError::CancelReasonNeedsTarget { reason });
        }
        let note = self
            .note
            .as_deref()
            .map(str::trim)
            .filter(|note| !note.is_empty())
            .map(str::to_owned);
        if reason == OTHER && note.is_none() {
            return Err(DomainError::CancelReasonNeedsNote);
        }
        Ok(CancelRecord {
            reason,
            duplicate_of: self.duplicate_of,
            note,
            path: self.path.clone(),
        })
    }
}

/// What a cancel records beside its move to `canceled`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CancelRecord {
    pub reason: String,
    pub duplicate_of: Option<TaskId>,
    pub note: Option<String>,
    pub path: Option<CancelPath>,
}

impl CancelRecord {
    /// The fields it adds to the cancel's `task_status_changed`.
    pub fn fields(&self) -> Map<String, Value> {
        let mut fields = Map::new();
        fields.insert("reason".into(), json!(self.reason));
        if let Some(target) = self.duplicate_of {
            fields.insert("duplicate_of".into(), json!(target));
        }
        if let Some(note) = &self.note {
            fields.insert("note".into(), json!(note));
        }
        match &self.path {
            Some(CancelPath::PlanReview(proposal)) => {
                fields.insert("by".into(), json!("plan_review"));
                fields.insert("proposal_id".into(), json!(proposal));
            }
            Some(CancelPath::Answer { by, ask }) => {
                fields.insert("by".into(), json!(by));
                fields.insert("ask_id".into(), json!(ask));
            }
            None => {}
        }
        fields
    }
}

/// What a `task_status_changed` recorded of a cancel, restored as typed
/// values (`run::payload::restore`): a missing key, `null` or a value of
/// another type (an older or broken writer) reads as absent, so a cancel
/// recorded before reasons is read, not refused.
#[derive(Debug, Default, Deserialize)]
pub struct Recorded {
    /// The status it moved to: a cancel's is `canceled`.
    #[serde(default, deserialize_with = "typed")]
    pub to: Option<TaskStatus>,
    #[serde(default, deserialize_with = "typed")]
    pub reason: Option<String>,
    /// The task that holds the content.
    #[serde(default, deserialize_with = "typed")]
    pub duplicate_of: Option<TaskId>,
}

impl Recorded {
    /// The cancel fields of a `task_status_changed`'s payload.
    pub fn of(payload: &Value) -> Self {
        restore(payload)
    }

    /// Whether the event canceled its task.
    pub fn canceled(&self) -> bool {
        self.to == Some(TaskStatus::Canceled)
    }

    /// Its reason: the recorded one, else for one recorded before reasons
    /// that names a task, `already_done` when that task was completed by
    /// then (`target_completed`) and `duplicate` when not; `None` for one
    /// that names none.
    pub fn reason(&self, target_completed: bool) -> Option<String> {
        match (&self.reason, self.duplicate_of) {
            (Some(reason), _) => Some(reason.clone()),
            (None, Some(_)) if target_completed => Some(ALREADY_DONE.to_owned()),
            (None, Some(_)) => Some(DUPLICATE.to_owned()),
            (None, None) => None,
        }
    }

    /// Whether it names a task it pairs as a duplicate of (ADR-t947-4
    /// decision 2).
    pub fn pairs_as_duplicate(&self) -> bool {
        self.duplicate_of.is_some() && pairs_as_duplicate(self.reason.as_deref())
    }
}

/// Whether a cancel of recorded `reason` (none for one recorded before
/// reasons) that names a task pairs it as a duplicate (ADR-t947-4
/// decision 2): what `related`, `search`, `stats`' `duplicate_cancels`,
/// `kpi` and a runtime planner's bundle outcome read. `absorbed`,
/// `re_registered` and the other codes do not.
pub fn pairs_as_duplicate(reason: Option<&str>) -> bool {
    matches!(reason, None | Some(DUPLICATE | ALREADY_DONE))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hand(reason: Option<&str>, target: Option<i64>, note: Option<&str>) -> NewCancel {
        NewCancel::by_hand(
            reason.map(str::to_owned),
            target.map(TaskId::new),
            note.map(str::to_owned),
        )
    }

    #[test]
    fn a_duplicate_of_alone_gives_the_reason_from_the_targets_status() {
        let open = hand(None, Some(2), None)
            .record(Some(TaskStatus::Ready))
            .unwrap();
        assert_eq!(open.reason, DUPLICATE);
        let done = hand(None, Some(2), None)
            .record(Some(TaskStatus::Completed))
            .unwrap();
        assert_eq!(done.reason, ALREADY_DONE);
        assert_eq!(
            done.fields(),
            json!({"reason": "already_done", "duplicate_of": 2})
                .as_object()
                .unwrap()
                .clone()
        );
        // A reason given wins over the target's status.
        let absorbed = hand(Some("absorbed"), Some(2), Some("moved"))
            .record(Some(TaskStatus::Completed))
            .unwrap();
        assert_eq!(absorbed.reason, ABSORBED);
        assert_eq!(absorbed.note.as_deref(), Some("moved"));
    }

    /// Each refusal and the words the CLI prints for it: no reason, a
    /// blank one, a code without the task that holds the content, `other`
    /// without a note, and a code kept for the runtime.
    #[test]
    fn refuses_a_cancel_without_what_its_reason_needs() {
        let refused = |cancel: NewCancel, error: DomainError, words: &str| {
            assert_eq!(cancel.record(None), Err(error.clone()), "{words}");
            assert!(error.to_string().contains(words), "{error} lacks {words}");
        };
        let missing =
            "cancel needs --reason CODE, or --duplicate-of X for a task X that holds its content";
        refused(
            hand(None, None, Some("why")),
            DomainError::CancelReasonMissing,
            missing,
        );
        refused(
            hand(Some("  "), None, None),
            DomainError::CancelReasonMissing,
            missing,
        );
        for reason in NEEDS_TARGET {
            refused(
                hand(Some(reason), None, None),
                DomainError::CancelReasonNeedsTarget {
                    reason: reason.into(),
                },
                &format!("cancel reason {reason} needs --duplicate-of X"),
            );
        }
        refused(
            hand(Some("other"), None, Some(" ")),
            DomainError::CancelReasonNeedsNote,
            "cancel reason other needs --note",
        );
        for reserved in [ANSWERED_CANCEL, UNRECORDED] {
            refused(
                hand(Some(reserved), None, None),
                DomainError::CancelReasonReserved {
                    reason: reserved.into(),
                },
                &format!("cancel reason {reserved} is kept for the runtime"),
            );
        }
        // A code the list does not have is recorded as it is.
        let unknown = hand(Some("later"), None, None).record(None).unwrap();
        assert_eq!(unknown.reason, "later");
        assert_eq!(unknown.note, None);
    }

    #[test]
    fn the_runtimes_paths_record_their_reason_and_ids() {
        let plan_review = NewCancel::plan_review(TaskId::new(3), ProposalId::new(7))
            .record(Some(TaskStatus::Submitted))
            .unwrap();
        assert_eq!(
            Value::Object(plan_review.fields()),
            json!({"reason": "duplicate", "duplicate_of": 3, "by": "plan_review", "proposal_id": 7})
        );
        let answered = NewCancel::answered(AskKind::Decide, AskId::new(9))
            .record(None)
            .unwrap();
        assert_eq!(
            Value::Object(answered.fields()),
            json!({"reason": "answered_cancel", "by": "decide", "ask_id": 9})
        );
    }

    #[test]
    fn reads_a_cancel_recorded_before_reasons_by_its_target() {
        let read = |payload: Value, completed: bool| Recorded::of(&payload).reason(completed);
        assert_eq!(
            read(json!({"to": "canceled", "reason": "stale"}), true).as_deref(),
            Some("stale")
        );
        let legacy = json!({"to": "canceled", "duplicate_of": 4});
        assert_eq!(read(legacy.clone(), true).as_deref(), Some(ALREADY_DONE));
        assert_eq!(read(legacy.clone(), false).as_deref(), Some(DUPLICATE));
        assert_eq!(read(json!({"to": "canceled"}), true), None);
        // A key of another type, as a broken writer left it, is absent.
        assert_eq!(read(json!({"duplicate_of": "4", "reason": 3}), false), None);
        assert!(Recorded::of(&json!("not an object")).to.is_none());
        assert!(Recorded::of(&legacy).canceled());
        assert!(!Recorded::of(&json!({"to": "draft"})).canceled());
        let pairs = |payload: Value| Recorded::of(&payload).pairs_as_duplicate();
        assert!(pairs(legacy));
        assert!(pairs(json!({"reason": "already_done", "duplicate_of": 4})));
        assert!(!pairs(json!({"reason": "absorbed", "duplicate_of": 4})));
        assert!(!pairs(json!({"reason": "duplicate"})));
        assert!(!pairs(json!({"duplicate_of": null})));
    }
}
