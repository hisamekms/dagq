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
    AskId, DomainError, EventId, FindingId, GoalId, PlannerId, ProposalId, RequestId, RunId,
    TaskId, error::require,
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

/// Where the answer of a `planner_question` about a request goes
/// (ADR-t1394-1 decision 7).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RequestAnswerRoute {
    /// To the planner of the runtime's open for the request.
    OwnPlanner,
    /// To a new planner, which carries it: the request is still `open`.
    NewPlanner,
    /// Closed by the supervisor: the request moved on (proposed, declined
    /// or out of planners).
    Close,
}

/// [`RequestAnswerRoute`] of an answer about a request in `status`, with a
/// planner of the runtime's open for it or not.
pub fn request_answer_route(planner_open: bool, status: RequestStatus) -> RequestAnswerRoute {
    if planner_open {
        RequestAnswerRoute::OwnPlanner
    } else if status == RequestStatus::Open {
        RequestAnswerRoute::NewPlanner
    } else {
        RequestAnswerRoute::Close
    }
}

/// What opening the next planner for a request does (ADR-t1394-1 decision
/// 4), once the request is still waiting for one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NextRequestPlanner {
    /// A planner is opened, the `attempt`-th for the request.
    Open { attempt: usize },
    /// [`MAX_REQUEST_PLANNERS`] planners ended without deciding it: it is
    /// `exhausted` with `reason`, and the inbox decides.
    Exhausted { attempts: usize, reason: String },
}

/// [`NextRequestPlanner`] after `opened` planners of the request: past the
/// limit the request is exhausted, unless the planner carries a person's
/// answer (`carries_answer`), which is carried past the limit as a draft's
/// and a finding's is.
pub fn next_request_planner(opened: usize, carries_answer: bool) -> NextRequestPlanner {
    if opened >= MAX_REQUEST_PLANNERS && !carries_answer {
        return NextRequestPlanner::Exhausted {
            attempts: opened,
            reason: format!(
                "{opened} planners of the runtime's ended without deciding the request (at most {MAX_REQUEST_PLANNERS})"
            ),
        };
    }
    NextRequestPlanner::Open {
        attempt: opened + 1,
    }
}

/// What a proposal submitted from a request's planner does to the request
/// (ADR-t1394-1 decision 6).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProposalLink {
    /// The request ended otherwise (declined, out of planners): left as it
    /// is.
    Skip,
    /// Linked to the request already `proposed`; nobody is told again.
    Link,
    /// Linked, and the `open` request becomes `proposed` with
    /// `request_proposed`.
    Propose,
}

/// [`ProposalLink`] of a proposal of a request in `status`.
pub const fn proposal_link(status: RequestStatus) -> ProposalLink {
    match status {
        RequestStatus::Open => ProposalLink::Propose,
        RequestStatus::Proposed => ProposalLink::Link,
        RequestStatus::Declined | RequestStatus::Exhausted => ProposalLink::Skip,
    }
}

/// Why a decline of `request` authorized for planner `authorized` is
/// refused in its own transaction, where `open` is the planner of the
/// runtime's open for the request now: another opened since is not the
/// decliner's.
pub fn stale_decliner(
    request: RequestId,
    open: Option<PlannerId>,
    authorized: Option<PlannerId>,
) -> Option<String> {
    (open != authorized).then(|| {
        format!("request {request}: its planner changed while it was declined; decline it again")
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

    // Moved here by task 1711 from the tests/it cases it removed:
    // planner_headless_turns::a_headless_request_planners_answer_reaches_a_new_one_and_undecided_ends_exhaust_the_request.
    // The kept request_planner::a_planner_question_about_a_request_reaches_its_planner_or_a_new_one_and_three_exhaust_it
    // checks the queue's transaction.
    #[test]
    fn an_answer_about_a_request_goes_to_its_planner_a_new_one_or_is_closed() {
        for status in [
            RequestStatus::Open,
            RequestStatus::Proposed,
            RequestStatus::Declined,
            RequestStatus::Exhausted,
        ] {
            assert_eq!(
                request_answer_route(true, status),
                RequestAnswerRoute::OwnPlanner,
                "{status:?}"
            );
        }
        assert_eq!(
            request_answer_route(false, RequestStatus::Open),
            RequestAnswerRoute::NewPlanner
        );
        for status in [
            RequestStatus::Proposed,
            RequestStatus::Declined,
            RequestStatus::Exhausted,
        ] {
            assert_eq!(
                request_answer_route(false, status),
                RequestAnswerRoute::Close,
                "{status:?}"
            );
        }
    }

    #[test]
    fn three_planners_exhaust_a_request_unless_the_next_carries_an_answer() {
        for opened in 0..MAX_REQUEST_PLANNERS {
            for carries in [false, true] {
                assert_eq!(
                    next_request_planner(opened, carries),
                    NextRequestPlanner::Open {
                        attempt: opened + 1
                    }
                );
            }
        }
        assert_eq!(MAX_REQUEST_PLANNERS, 3);
        assert_eq!(
            next_request_planner(3, false),
            NextRequestPlanner::Exhausted {
                attempts: 3,
                reason:
                    "3 planners of the runtime's ended without deciding the request (at most 3)"
                        .into(),
            }
        );
        // A person's answer is carried past the limit.
        assert_eq!(
            next_request_planner(3, true),
            NextRequestPlanner::Open { attempt: 4 }
        );
    }

    #[test]
    fn the_first_proposal_proposes_an_open_request_and_the_rest_are_linked() {
        assert_eq!(proposal_link(RequestStatus::Open), ProposalLink::Propose);
        assert_eq!(proposal_link(RequestStatus::Proposed), ProposalLink::Link);
        assert_eq!(proposal_link(RequestStatus::Declined), ProposalLink::Skip);
        assert_eq!(proposal_link(RequestStatus::Exhausted), ProposalLink::Skip);
    }

    #[test]
    fn a_decline_for_a_planner_no_longer_open_is_refused() {
        let id = RequestId::new(2);
        let (own, other) = (Some(PlannerId::new(4)), Some(PlannerId::new(104)));
        assert_eq!(stale_decliner(id, own, own), None);
        assert_eq!(stale_decliner(id, None, None), None);
        for (open, authorized) in [(own, other), (None, own), (own, None)] {
            assert_eq!(
                stale_decliner(id, open, authorized).as_deref(),
                Some("request 2: its planner changed while it was declined; decline it again"),
                "{open:?} {authorized:?}"
            );
        }
    }
}
