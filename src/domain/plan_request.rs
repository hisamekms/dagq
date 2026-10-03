//! A planning request (ADR-t1394-1 decisions 2, 4, 6 and 7): a person's
//! words the inbox (or the person at a plain terminal) records for a
//! planner of the runtime's to make a plan of. The supervisor opens one
//! planner of the runtime's at a time for each `open` request, at most
//! [`MAX_REQUEST_PLANNERS`]; the planner submits a proposal (the request is
//! then `proposed`, linked to it), declines it with a reason (`declined`),
//! or asks a `planner_question` about it. Its words never change once
//! recorded: a rewording is a new request.

use serde::{Deserialize, Serialize};

use super::{
    AskId, DomainError, EventId, FindingId, GoalId, ProposalId, RequestId, RunId, TaskId,
    error::require,
};

/// How many planners of the runtime's may end without deciding a request
/// before it is `exhausted` and the inbox decides (as a draft's and a
/// finding's, ADR-t1394-1 decision 4).
pub const MAX_REQUEST_PLANNERS: usize = 3;

// Where a request is (ADR-t1394-1 decision 2): waiting for a planner or
// with one at work, a proposal submitted from it, declined by its planner,
// or left undecided by its planners up to the limit.
string_enum!(RequestStatus {
    Open => "open",
    Proposed => "proposed",
    Declined => "declined",
    Exhausted => "exhausted",
});

impl RequestStatus {
    /// Whether the request waits for a person to reword or drop it: the
    /// inbox's attention (ADR-t1394-1 decision 6).
    pub const fn needs_a_person(self) -> bool {
        matches!(self, Self::Declined | Self::Exhausted)
    }
}

/// What a request refers to, from the notice that brought it (`--ref
/// <kind>:<id>`): its planner's prompt carries what each holds.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "id", rename_all = "snake_case")]
pub enum RequestRef {
    Ask(AskId),
    Task(TaskId),
    Run(RunId),
    Event(EventId),
    Finding(FindingId),
    Goal(GoalId),
}

impl RequestRef {
    /// Read `<kind>:<id>`: `ask:3`, `task:12`, `run:<uuid>`, `event:40`,
    /// `finding:2` or `goal:7`.
    pub fn parse(value: &str) -> Result<Self, DomainError> {
        let invalid = || DomainError::RequestRefInvalid {
            value: value.to_owned(),
        };
        let (kind, id) = value.trim().split_once(':').ok_or_else(invalid)?;
        let id = id.trim();
        let number = || {
            id.parse::<i64>()
                .ok()
                .filter(|id| *id > 0)
                .ok_or_else(invalid)
        };
        Ok(match kind.trim() {
            "ask" => Self::Ask(AskId::new(number()?)),
            "task" => Self::Task(TaskId::new(number()?)),
            "run" => Self::Run(RunId::new(id).map_err(|_| invalid())?),
            "event" => Self::Event(EventId::new(number()?)),
            "finding" => Self::Finding(FindingId::new(number()?)),
            "goal" => Self::Goal(GoalId::new(number()?)),
            _ => return Err(invalid()),
        })
    }
}

impl std::fmt::Display for RequestRef {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Ask(id) => write!(f, "ask:{id}"),
            Self::Task(id) => write!(f, "task:{id}"),
            Self::Run(id) => write!(f, "run:{id}"),
            Self::Event(id) => write!(f, "event:{id}"),
            Self::Finding(id) => write!(f, "finding:{id}"),
            Self::Goal(id) => write!(f, "goal:{id}"),
        }
    }
}

/// A request to record: the person's words, what the inbox adds apart
/// from them, and what it refers to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewPlanRequest {
    /// The person's own words, not the inbox's summary.
    pub text: String,
    /// What the inbox adds, kept apart from the person's words.
    pub note: Option<String>,
    pub refs: Vec<RequestRef>,
}

impl NewPlanRequest {
    pub fn validate(&self) -> Result<(), DomainError> {
        require(!self.text.trim().is_empty(), || DomainError::Blank {
            field: "text",
        })?;
        require(
            self.note
                .as_deref()
                .is_none_or(|note| !note.trim().is_empty()),
            || DomainError::Blank { field: "note" },
        )
    }
}

/// A planning request as the queue records it. Times are Unix seconds.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PlanRequest {
    pub id: RequestId,
    pub text: String,
    pub note: Option<String>,
    pub refs: Vec<RequestRef>,
    /// The role of who recorded it (`inbox`, or `user` for a person at a
    /// plain terminal) and its actor id.
    pub requested_by: String,
    pub requested_by_id: String,
    pub status: RequestStatus,
    /// Why it was declined, or how its planners ran out.
    pub status_reason: Option<String>,
    /// The proposals its planners submitted, oldest first.
    pub proposals: Vec<ProposalId>,
    /// The planners of the runtime's opened for it so far.
    pub planners: usize,
    pub created_at: i64,
    pub updated_at: i64,
}

/// Decline `request` with `reason` (ADR-t1394-1 decision 6): only an
/// `open` one, with a reason.
pub fn check_decline(request: &PlanRequest, reason: &str) -> Result<(), DomainError> {
    require(!reason.trim().is_empty(), || DomainError::Blank {
        field: "reason",
    })?;
    require(request.status == RequestStatus::Open, || {
        DomainError::RequestNotOpen {
            request_id: request.id,
            status: request.status,
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(status: RequestStatus) -> PlanRequest {
        PlanRequest {
            id: RequestId::new(1),
            text: "plan it".into(),
            note: None,
            refs: Vec::new(),
            requested_by: "inbox".into(),
            requested_by_id: "inbox".into(),
            status,
            status_reason: None,
            proposals: Vec::new(),
            planners: 0,
            created_at: 0,
            updated_at: 0,
        }
    }

    #[test]
    fn a_reference_reads_each_kind_and_refuses_the_rest() {
        for (text, parsed) in [
            ("ask:3", RequestRef::Ask(AskId::new(3))),
            ("task:12", RequestRef::Task(TaskId::new(12))),
            ("event:40", RequestRef::Event(EventId::new(40))),
            ("finding:2", RequestRef::Finding(FindingId::new(2))),
            ("goal:7", RequestRef::Goal(GoalId::new(7))),
        ] {
            assert_eq!(RequestRef::parse(text).unwrap(), parsed);
            assert_eq!(parsed.to_string(), text);
        }
        let run = "0b8c0a2e-5b9d-4e8e-9d3c-111111111111";
        let parsed = RequestRef::parse(&format!("run:{run}")).unwrap();
        assert_eq!(parsed.to_string(), format!("run:{run}"));
        for bad in ["task", "task:", "task:0", "task:x", "proposal:1", ":3"] {
            assert!(
                matches!(
                    RequestRef::parse(bad),
                    Err(DomainError::RequestRefInvalid { .. })
                ),
                "{bad}"
            );
        }
        assert_eq!(
            serde_json::to_value(RequestRef::Task(TaskId::new(4))).unwrap(),
            serde_json::json!({"kind": "task", "id": 4})
        );
    }

    #[test]
    fn a_request_needs_words_and_a_note_is_not_blank() {
        let new = |text: &str, note: Option<&str>| NewPlanRequest {
            text: text.into(),
            note: note.map(str::to_owned),
            refs: Vec::new(),
        };
        assert!(new("plan it", None).validate().is_ok());
        assert!(new("plan it", Some("the inbox's")).validate().is_ok());
        assert_eq!(
            new("  ", None).validate(),
            Err(DomainError::Blank { field: "text" })
        );
        assert_eq!(
            new("plan it", Some(" ")).validate(),
            Err(DomainError::Blank { field: "note" })
        );
    }

    #[test]
    fn only_an_open_request_is_declined_and_with_a_reason() {
        assert!(check_decline(&request(RequestStatus::Open), "done already").is_ok());
        assert_eq!(
            check_decline(&request(RequestStatus::Open), " "),
            Err(DomainError::Blank { field: "reason" })
        );
        for status in [
            RequestStatus::Proposed,
            RequestStatus::Declined,
            RequestStatus::Exhausted,
        ] {
            assert_eq!(
                check_decline(&request(status), "why"),
                Err(DomainError::RequestNotOpen {
                    request_id: RequestId::new(1),
                    status,
                })
            );
        }
        assert!(RequestStatus::Declined.needs_a_person());
        assert!(RequestStatus::Exhausted.needs_a_person());
        assert!(!RequestStatus::Open.needs_a_person());
        assert!(!RequestStatus::Proposed.needs_a_person());
    }
}
