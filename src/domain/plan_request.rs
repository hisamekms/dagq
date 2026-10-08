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
    AskId, DomainError, EventId, FindingId, GoalId, PlannerId, PlannerOrigin, Priority, ProposalId,
    RequestId, RunId, TaskId, error::require, follow_up::DraftOrigin,
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
    /// The priority the person's words name, and only then (ADR-t1975-1
    /// decision 1): the runtime attaches it to what the request's planners
    /// make ([`goal_priority_at_creation`], [`task_priority_at_creation`]).
    pub priority: Option<Priority>,
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
    /// The priority the person gave it (`request add --priority`); none
    /// when the words named none. Stored as `plan_requests.priority`
    /// (`low`=0 … `interrupt`=4), null for a request recorded without one
    /// or before the column.
    pub priority: Option<Priority>,
    /// The role of who recorded it (`inbox`, or `user` for a person at a
    /// plain terminal) and its actor id.
    pub requested_by: String,
    pub requested_by_id: String,
    pub status: RequestStatus,
    /// Why it was declined, or how its planners ran out.
    pub status_reason: Option<String>,
    /// The proposals its planners submitted, oldest first.
    pub proposals: Vec<ProposalId>,
    /// The planners of the runtime's opened for it so far that count to
    /// [`MAX_REQUEST_PLANNERS`]: one asked to exit because only a person's
    /// answer was left does not (ADR-t1704-1 decision 5).
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

// Whether a goal or task comes from a person or the AI (ADR-t1975-1
// decision 5), recorded once at its creation and never rewritten (a
// withdrawal, a resubmission or another planner's submit leaves it):
// `unknown` only for a row from before the record that the migration could
// not decide, which plan review treats as a person's (decision 6).
string_enum!(Origin {
    Human => "human",
    Ai => "ai",
    Unknown => "unknown",
});

// What made a goal or task, finer than [`Origin`]: a person's
// (`request`: a request's planner, or a revise planner of a proposal linked
// to a request; `person`: the user or the inbox directly, or a planner a
// person opened) or the AI's (`finding`, `follow_up`, `goal_gap`,
// `reopened`, `draft`: the planner of such a draft, or the runtime
// registering one; `planner`: any other planner of the runtime's; `runtime`:
// any other actor, such as a job or the supervisor).
string_enum!(OriginKind {
    Request => "request",
    Person => "person",
    Finding => "finding",
    FollowUp => "follow_up",
    GoalGap => "goal_gap",
    Reopened => "reopened",
    Draft => "draft",
    Planner => "planner",
    Runtime => "runtime",
});

impl OriginKind {
    pub const fn origin(self) -> Origin {
        match self {
            Self::Request | Self::Person => Origin::Human,
            Self::Finding
            | Self::FollowUp
            | Self::GoalGap
            | Self::Reopened
            | Self::Draft
            | Self::Planner
            | Self::Runtime => Origin::Ai,
        }
    }

    /// The kind of a draft of `origin` (a revisit is the draft's own).
    pub const fn of_draft(origin: Option<DraftOrigin>) -> Self {
        match origin {
            Some(DraftOrigin::FollowUp) => Self::FollowUp,
            Some(DraftOrigin::GoalGap) => Self::GoalGap,
            Some(DraftOrigin::Reopened) => Self::Reopened,
            Some(DraftOrigin::Revisit) | None => Self::Draft,
        }
    }
}

/// A goal's or task's origin as the queue records it: the columns
/// `origin`, `origin_kind` and `origin_request_id` of `goals` and `tasks`,
/// shown under those names. The kind is none only for `unknown`; the
/// request only for a `request` kind.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct RecordedOrigin {
    pub origin: Origin,
    #[serde(rename = "origin_kind")]
    pub kind: Option<OriginKind>,
    #[serde(rename = "origin_request_id")]
    pub request_id: Option<RequestId>,
}

impl RecordedOrigin {
    /// A row the migration could not decide.
    pub const UNKNOWN: Self = Self {
        origin: Origin::Unknown,
        kind: None,
        request_id: None,
    };

    pub const fn of(kind: OriginKind) -> Self {
        Self {
            origin: kind.origin(),
            kind: Some(kind),
            request_id: None,
        }
    }

    pub const fn request(request: RequestId) -> Self {
        Self {
            origin: Origin::Human,
            kind: Some(OriginKind::Request),
            request_id: Some(request),
        }
    }
}

impl Default for RecordedOrigin {
    fn default() -> Self {
        Self::UNKNOWN
    }
}

/// What the queue records, at a goal's or task's creation, of the planner
/// that creates it (its `planners` row).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct CreatingPlanner {
    /// A person's planner (`person`) or the runtime's.
    pub opened_by: Option<PlannerOrigin>,
    /// The request it was opened for (`planners.request_id`).
    pub request: Option<RequestId>,
    /// The request the proposal it revises is linked to
    /// (`plan_request_proposals`, a resubmission's link included); the
    /// lowest when several.
    pub revised_request: Option<RequestId>,
    /// Opened for a finding (`planners.finding_id`).
    pub finding: bool,
    /// Opened for a draft, or a bundle of drafts: the origin of the draft
    /// (the first of the bundle), `None` inside for a draft without one.
    pub draft: Option<Option<DraftOrigin>>,
}

/// Who creates a goal or task, as the store reads it from the actor of
/// the write (and, for a planner, its row).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Creator {
    /// The user at a terminal, or the inbox at a person's word.
    Person,
    /// A planner, by what its row records.
    Planner(CreatingPlanner),
    /// The runtime registering a draft of that origin (a follow_up, a goal
    /// gap).
    Draft(DraftOrigin),
    /// Any other actor: a job, the supervisor, a worker.
    Other,
}

impl Creator {
    /// Whether a priority it sets is a person's (ADR-t1975-1 decision 3).
    pub const fn is_person(self) -> bool {
        matches!(self, Self::Person)
    }
}

/// The origin a goal or task created by `creator` records (ADR-t1975-1
/// decision 5). A request's planner makes a person's with the request; a
/// revise planner without a request takes the request of the proposal it
/// revises, when that proposal is linked to one; a finding's, a draft's
/// and any other planner of the runtime's make the AI's. The words of the
/// goal or task (its context, `from request N`) are never read.
pub fn creation_origin(creator: Creator) -> RecordedOrigin {
    match creator {
        Creator::Person => RecordedOrigin::of(OriginKind::Person),
        Creator::Draft(origin) => RecordedOrigin::of(OriginKind::of_draft(Some(origin))),
        Creator::Other => RecordedOrigin::of(OriginKind::Runtime),
        Creator::Planner(planner) => {
            if let Some(request) = planner.request {
                RecordedOrigin::request(request)
            } else if planner.opened_by == Some(PlannerOrigin::Person) {
                RecordedOrigin::of(OriginKind::Person)
            } else if planner.finding {
                RecordedOrigin::of(OriginKind::Finding)
            } else if let Some(draft) = planner.draft {
                RecordedOrigin::of(OriginKind::of_draft(draft))
            } else if let Some(request) = planner.revised_request {
                RecordedOrigin::request(request)
            } else {
                RecordedOrigin::of(OriginKind::Planner)
            }
        }
    }
}

// Who set a priority (ADR-t1975-1 decisions 2 and 3): `human`, a person
// (the user, the inbox at a person's word, or the runtime attaching a
// request's priority; a goal a person adds without one takes `normal` as
// the person's), or `ai`, anyone else, the default `normal` of a task in no
// goal included. Another axis than [`super::PrioritySource`]
// (which level the value comes from): a goal's `priority_by` column, and a
// task's for its own priority (null without one; null beside an own
// priority reads as `human`: nobody can tell who set it).
string_enum!(PriorityBy {
    Human => "human",
    Ai => "ai",
});

impl PriorityBy {
    /// Who sets a priority as `creator`.
    pub const fn of(creator: Creator) -> Self {
        if creator.is_person() {
            Self::Human
        } else {
            Self::Ai
        }
    }

    /// Who set the task's effective priority: its own's setter, else its
    /// goal's, else `ai` for the default (ADR-t1975-1 decision 2).
    pub fn effective(own: Option<Priority>, own_by: Option<Self>, goal_by: Option<Self>) -> Self {
        match own {
            Some(_) => own_by.unwrap_or(Self::Human),
            None => goal_by.unwrap_or(Self::Ai),
        }
    }
}

/// The priority of a new goal and who set it (ADR-t1975-1 decision 2):
/// for a goal of a request a person gave a priority (`request`, its ID and
/// its priority), that priority as a person's, whether the planner gives
/// none or the same, and an error naming it when the planner gives
/// another; otherwise the one `given` (or `normal`), set by `creator`.
pub fn goal_priority_at_creation(
    creator: Creator,
    request: Option<(RequestId, Priority)>,
    given: Option<Priority>,
) -> Result<(Priority, PriorityBy), DomainError> {
    if let Some((request_id, priority)) = request {
        return match given {
            Some(given) if given != priority => Err(DomainError::RequestPriorityDiffers {
                request_id,
                priority,
                given,
            }),
            _ => Ok((priority, PriorityBy::Human)),
        };
    }
    Ok((given.unwrap_or_default(), PriorityBy::of(creator)))
}

/// The own priority of a new task and who set it (ADR-t1975-1 decision 2),
/// `goal` being the priority of the goal it joins and who set it. For a
/// task of a request a person gave a priority, the task takes that
/// priority as a person's own, unless its goal already has it as a
/// person's (then it inherits): a goal of another value, a value an AI set
/// or no goal all get it. The planner gives none; the same is taken, and
/// another is refused naming the request's. Otherwise the one `given`, set
/// by `creator`.
pub fn task_priority_at_creation(
    creator: Creator,
    request: Option<(RequestId, Priority)>,
    given: Option<Priority>,
    goal: Option<(Priority, PriorityBy)>,
) -> Result<(Option<Priority>, Option<PriorityBy>), DomainError> {
    if let Some((request_id, priority)) = request {
        if let Some(given) = given.filter(|given| *given != priority) {
            return Err(DomainError::RequestPriorityDiffers {
                request_id,
                priority,
                given,
            });
        }
        if given.is_none() && goal == Some((priority, PriorityBy::Human)) {
            return Ok((None, None));
        }
        return Ok((Some(priority), Some(PriorityBy::Human)));
    }
    Ok((given, given.map(|_| PriorityBy::of(creator))))
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
            priority: None,
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
            priority: None,
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

    fn planner(edit: impl FnOnce(&mut CreatingPlanner)) -> Creator {
        let mut planner = CreatingPlanner {
            opened_by: Some(PlannerOrigin::Runtime),
            ..CreatingPlanner::default()
        };
        edit(&mut planner);
        Creator::Planner(planner)
    }

    #[test]
    fn a_goals_or_tasks_origin_is_a_persons_by_its_request_or_creator_and_the_ais_otherwise() {
        let request = RequestId::new(44);
        // A request's planner, whatever else its row records.
        assert_eq!(
            creation_origin(planner(|p| {
                p.request = Some(request);
                p.finding = true;
            })),
            RecordedOrigin {
                origin: Origin::Human,
                kind: Some(OriginKind::Request),
                request_id: Some(request),
            }
        );
        // The user's or the inbox's direct add, and a person's planner.
        assert_eq!(
            creation_origin(Creator::Person),
            RecordedOrigin::of(OriginKind::Person)
        );
        assert_eq!(
            creation_origin(planner(|p| p.opened_by = Some(PlannerOrigin::Person))),
            RecordedOrigin::of(OriginKind::Person)
        );
        // A finding's and a draft's planner, by the draft's origin.
        assert_eq!(
            creation_origin(planner(|p| p.finding = true)),
            RecordedOrigin::of(OriginKind::Finding)
        );
        for (draft, kind) in [
            (Some(DraftOrigin::FollowUp), OriginKind::FollowUp),
            (Some(DraftOrigin::GoalGap), OriginKind::GoalGap),
            (Some(DraftOrigin::Reopened), OriginKind::Reopened),
            (Some(DraftOrigin::Revisit), OriginKind::Draft),
            (None, OriginKind::Draft),
        ] {
            let origin = creation_origin(planner(|p| p.draft = Some(draft)));
            assert_eq!(origin, RecordedOrigin::of(kind), "{draft:?}");
            assert_eq!(origin.origin, Origin::Ai);
        }
        // A revise planner without a request takes the request of the
        // proposal it revises (a resubmission's link included), else the AI's.
        assert_eq!(
            creation_origin(planner(|p| p.revised_request = Some(request))),
            RecordedOrigin::request(request)
        );
        assert_eq!(
            creation_origin(planner(|_| {})),
            RecordedOrigin::of(OriginKind::Planner)
        );
        assert_eq!(
            creation_origin(Creator::Planner(CreatingPlanner::default())),
            RecordedOrigin::of(OriginKind::Planner)
        );
        // The runtime registering a follow_up or a goal gap, and any other
        // actor.
        assert_eq!(
            creation_origin(Creator::Draft(DraftOrigin::FollowUp)),
            RecordedOrigin::of(OriginKind::FollowUp)
        );
        assert_eq!(
            creation_origin(Creator::Draft(DraftOrigin::GoalGap)),
            RecordedOrigin::of(OriginKind::GoalGap)
        );
        assert_eq!(
            creation_origin(Creator::Other),
            RecordedOrigin::of(OriginKind::Runtime)
        );
        assert_eq!(RecordedOrigin::default().origin, Origin::Unknown);
        assert_eq!(
            serde_json::to_value(RecordedOrigin::request(request)).unwrap(),
            serde_json::json!({"origin": "human", "origin_kind": "request", "origin_request_id": 44})
        );
    }

    #[test]
    fn a_requests_goal_takes_its_priority_as_a_persons_and_another_is_refused() {
        let request = RequestId::new(44);
        let planner = planner(|p| p.request = Some(request));
        let interrupt = Some((request, Priority::Interrupt));
        for given in [None, Some(Priority::Interrupt)] {
            assert_eq!(
                goal_priority_at_creation(planner, interrupt, given),
                Ok((Priority::Interrupt, PriorityBy::Human)),
                "{given:?}"
            );
        }
        let refused = goal_priority_at_creation(planner, interrupt, Some(Priority::Normal));
        assert_eq!(
            refused,
            Err(DomainError::RequestPriorityDiffers {
                request_id: request,
                priority: Priority::Interrupt,
                given: Priority::Normal,
            })
        );
        assert!(
            refused
                .unwrap_err()
                .to_string()
                .contains("request 44 carries the priority a person gave it, interrupt")
        );
        // A request without a priority: the planner's value, or normal, is
        // the AI's; a person's own goal is a person's.
        assert_eq!(
            goal_priority_at_creation(planner, None, Some(Priority::High)),
            Ok((Priority::High, PriorityBy::Ai))
        );
        assert_eq!(
            goal_priority_at_creation(planner, None, None),
            Ok((Priority::Normal, PriorityBy::Ai))
        );
        assert_eq!(
            goal_priority_at_creation(Creator::Person, None, Some(Priority::Low)),
            Ok((Priority::Low, PriorityBy::Human))
        );
        assert_eq!(
            goal_priority_at_creation(Creator::Person, None, None),
            Ok((Priority::Normal, PriorityBy::Human))
        );
    }

    #[test]
    fn a_requests_task_takes_its_priority_unless_its_goal_has_it_as_a_persons() {
        let request = RequestId::new(44);
        let planner = planner(|p| p.request = Some(request));
        let urgent = Some((request, Priority::Urgent));
        let own = Ok((Some(Priority::Urgent), Some(PriorityBy::Human)));
        // Its goal already has it as a person's: it inherits.
        assert_eq!(
            task_priority_at_creation(
                planner,
                urgent,
                None,
                Some((Priority::Urgent, PriorityBy::Human))
            ),
            Ok((None, None))
        );
        // An existing goal of another value, of the value an AI set, or no
        // goal: the task gets it as a person's own.
        for goal in [
            Some((Priority::Normal, PriorityBy::Human)),
            Some((Priority::Urgent, PriorityBy::Ai)),
            None,
        ] {
            assert_eq!(
                task_priority_at_creation(planner, urgent, None, goal),
                own,
                "{goal:?}"
            );
        }
        assert_eq!(
            task_priority_at_creation(planner, urgent, Some(Priority::Urgent), None),
            own
        );
        assert_eq!(
            task_priority_at_creation(planner, urgent, Some(Priority::Low), None),
            Err(DomainError::RequestPriorityDiffers {
                request_id: request,
                priority: Priority::Urgent,
                given: Priority::Low,
            })
        );
        // A request without a priority, and any other creator: its own as
        // given, set by the creator.
        assert_eq!(
            task_priority_at_creation(planner, None, None, None),
            Ok((None, None))
        );
        assert_eq!(
            task_priority_at_creation(planner, None, Some(Priority::High), None),
            Ok((Some(Priority::High), Some(PriorityBy::Ai)))
        );
        assert_eq!(
            task_priority_at_creation(Creator::Person, None, Some(Priority::High), None),
            Ok((Some(Priority::High), Some(PriorityBy::Human)))
        );
    }

    #[test]
    fn a_tasks_priority_is_set_by_its_own_setter_else_its_goals() {
        let high = Some(Priority::High);
        assert_eq!(
            PriorityBy::effective(high, Some(PriorityBy::Ai), Some(PriorityBy::Human)),
            PriorityBy::Ai
        );
        // Nobody can tell who set an own priority: a person's.
        assert_eq!(
            PriorityBy::effective(high, None, Some(PriorityBy::Ai)),
            PriorityBy::Human
        );
        assert_eq!(
            PriorityBy::effective(None, None, Some(PriorityBy::Human)),
            PriorityBy::Human
        );
        // The default of a task in no goal is nobody's.
        assert_eq!(PriorityBy::effective(None, None, None), PriorityBy::Ai);
        assert_eq!(PriorityBy::of(Creator::Person), PriorityBy::Human);
        assert_eq!(PriorityBy::of(Creator::Other), PriorityBy::Ai);
    }
}
