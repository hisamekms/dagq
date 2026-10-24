//! The planning commands as the queue service's write use cases take them
//! ([Queue service], ADR-t3102-3 decisions 3 and 4): one [`QueueWrite`]
//! per planning command a person, the inbox or a planner runs (`add`,
//! `edit`, `submit`, `cancel`, `dependency add|remove`, `set-goal`,
//! `set-paths`, `set-priority`, `revisit`, `judge-follow-up`, `draft`,
//! `ready`, `goal add|edit|close|ready|review`, `proposal withdraw`,
//! `request add|decline`), with the command line's options as its params.
//! The command line and the service both run one with [`answer`] (through
//! `compose::write_queue`, which holds the queue to the repository's rules
//! first), so a write is the same application's call, authorized for the
//! caller and recorded as its actor, either way, and prints the same JSON.
//!
//! Each write is one call of the application's [`Planning`] or
//! [`Requests`], whose store makes its change in one transaction of its
//! own; nothing of a write is split across round trips with the caller.
//! `ready --inherit` is no write use case: it moves a run's branch, a Git
//! operation of the control side's (ADR-t3102-3 decision 5).
//!
//! [Queue service]: ../../docs/design/queue-service.md

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use super::commands::Gate;
use super::commands::planning::{Dependency, Planning, PlanningStore};
use super::commands::requests::{RequestStore, Requests};
use crate::domain::follow_up::{MembershipJudgement, RevisitChange, revisit_seconds};
use crate::domain::plan_request::{NewPlanRequest, RequestRef};
use crate::domain::queue_service::UseCase;
use crate::domain::worker::WorkerMode;
use crate::domain::{
    ActorContext, ActorRole, Authorizer, Capability, FindingId, GoalEdit, GoalId, GoalTag,
    GoalVerdict, NewGoal, NewTask, PlannerId, PlannerOrigin, PlannerOwner, PlannerSession,
    ProposalId, RequestId, Resource, Submission, TaskEdit, TaskId, cancel_reason::NewCancel,
};

/// A planning command, as one write use case of the service takes it.
#[derive(Debug, Clone, PartialEq)]
pub enum QueueWrite {
    Add(AddWrite),
    Edit(EditWrite),
    Submit(SubmitWrite),
    Cancel(CancelWrite),
    DependencyAdd(DependencyWrite),
    DependencyRemove(DependencyWrite),
    SetGoal(SetGoalWrite),
    SetPaths(SetPathsWrite),
    SetPriority(SetPriorityWrite),
    Revisit(RevisitWrite),
    JudgeFollowUp(JudgeFollowUpWrite),
    Draft(IdWrite),
    Ready(ReadyWrite),
    GoalAdd(GoalAddWrite),
    GoalEdit(GoalEditWrite),
    GoalClose(GoalCloseWrite),
    GoalReady(IdWrite),
    GoalReview(IdWrite),
    ProposalWithdraw(IdWrite),
    RequestAdd(RequestAddWrite),
    RequestDecline(RequestDeclineWrite),
}

/// The options of a command that names one task, goal or proposal: its id.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct IdWrite {
    pub id: i64,
}

/// `add`'s options.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct AddWrite {
    pub title: String,
    pub description: String,
    pub acceptance: String,
    pub verify: Vec<String>,
    pub depends_on: Vec<i64>,
    pub depends_on_goal: Vec<i64>,
    pub goal: Option<i64>,
    pub context: String,
    pub evidence: Vec<String>,
    pub paths: Vec<String>,
    pub priority: Option<String>,
    pub change: Option<String>,
    pub provider: Option<String>,
    pub headless: bool,
    pub interactive: bool,
    pub wait_for_build: bool,
    pub execution_class: Option<String>,
}

/// `edit`'s options: a list option replaces the list, and its `no_` flag
/// empties it.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct EditWrite {
    pub task: i64,
    pub title: Option<String>,
    pub description: Option<String>,
    pub acceptance: Option<String>,
    pub context: Option<String>,
    pub verify: Vec<String>,
    pub no_verify: bool,
    pub evidence: Vec<String>,
    pub no_evidence: bool,
    pub paths: Vec<String>,
    pub no_paths: bool,
    pub change: Option<String>,
    pub provider: Option<String>,
    pub headless: bool,
    pub interactive: bool,
    pub wait_for_build: bool,
    pub no_wait_for_build: bool,
    pub execution_class: Option<String>,
}

/// `submit`'s options. The proposal's owner is not one of them: the
/// caller's own ([`answer`]'s `owner`).
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct SubmitWrite {
    pub tasks: Vec<i64>,
    pub goal: Vec<i64>,
    pub proposal: Option<i64>,
    pub finding: Vec<i64>,
    /// What the file `--replacement` names held when the caller read it:
    /// the replacement plan (JSON) of a replanning operation's request.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub replacement: Option<String>,
}

/// `cancel`'s options.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CancelWrite {
    pub id: i64,
    #[serde(default)]
    pub reason: Option<String>,
    #[serde(default)]
    pub duplicate_of: Option<i64>,
    #[serde(default)]
    pub note: Option<String>,
}

/// `dependency add`'s and `dependency remove`'s options: a predecessor
/// task or a goal (`goal` wins, as on the command line).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DependencyWrite {
    pub task: i64,
    #[serde(default)]
    pub predecessor: Option<i64>,
    #[serde(default)]
    pub goal: Option<i64>,
}

/// `set-goal`'s options: no goal is `--none`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SetGoalWrite {
    pub task: i64,
    #[serde(default)]
    pub goal: Option<i64>,
}

/// `set-paths`' options: no paths is `--none`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SetPathsWrite {
    pub task: i64,
    #[serde(default)]
    pub paths: Vec<String>,
}

/// `set-priority`'s options: no level is `--inherit`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SetPriorityWrite {
    pub task: i64,
    #[serde(default)]
    pub level: Option<String>,
}

/// `revisit`'s options: no time is `--clear`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RevisitWrite {
    pub task: i64,
    #[serde(default)]
    pub at: Option<String>,
    #[serde(default)]
    pub note: Option<String>,
}

/// `judge-follow-up`'s options.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct JudgeFollowUpWrite {
    pub task: i64,
    pub classification: String,
    #[serde(default)]
    pub acceptance_item: Vec<String>,
    pub reason: String,
    #[serde(default)]
    pub evidence: Vec<String>,
    #[serde(default)]
    pub destination_goal: Option<i64>,
    #[serde(default)]
    pub source_goal: Option<i64>,
    #[serde(default)]
    pub corrects: Option<i64>,
}

/// `ready`'s options, without `--inherit`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReadyWrite {
    pub id: i64,
    #[serde(default)]
    pub bypass_review: bool,
}

/// `goal add`'s options.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct GoalAddWrite {
    pub title: String,
    pub description: String,
    pub acceptance: String,
    pub constraints: String,
    pub doc: Option<String>,
    pub draft: bool,
    pub priority: Option<String>,
    pub tag: Vec<String>,
}

/// `goal edit`'s options.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct GoalEditWrite {
    pub id: i64,
    pub title: Option<String>,
    pub description: Option<String>,
    pub acceptance: Option<String>,
    pub constraints: Option<String>,
    pub doc: Option<String>,
    pub priority: Option<String>,
    pub tag: Vec<String>,
    pub no_tags: bool,
}

/// `goal close`'s options.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GoalCloseWrite {
    pub id: i64,
    pub verdict: String,
}

/// `request add`'s options, with the words read already: the caller reads
/// `--text-file`, `--note-file` and stdin, and sends what they held. No
/// `text` is a caller that did not read them because its role may not
/// record a request: it is refused (and recorded) before a file or stdin
/// is read, as the command line refuses it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RequestAddWrite {
    #[serde(default)]
    pub text: Option<String>,
    #[serde(default)]
    pub note: Option<String>,
    #[serde(default, rename = "ref")]
    pub refs: Vec<String>,
    #[serde(default)]
    pub priority: Option<String>,
}

/// `request decline`'s options.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RequestDeclineWrite {
    pub id: i64,
    pub reason: String,
    #[serde(default)]
    pub continue_run: bool,
}

/// Params that do not read as the write's: what the command line's parser
/// would refuse before the command runs.
#[derive(Debug)]
pub struct BadWrite(pub String);

impl std::fmt::Display for BadWrite {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for BadWrite {}

impl QueueWrite {
    /// The write `use_case` asks for with `params`: `None` for a use case
    /// that is no write of these. Params that do not read are a
    /// [`BadWrite`].
    pub fn parse(use_case: UseCase, params: &Value) -> Result<Option<Self>> {
        fn read<T: for<'de> Deserialize<'de>>(use_case: UseCase, params: &Value) -> Result<T> {
            let params = if params.is_null() {
                &Value::Object(serde_json::Map::new())
            } else {
                params
            };
            serde_json::from_value(params.clone()).map_err(|error| {
                BadWrite(format!(
                    "the params of {} do not read: {error}",
                    use_case.as_str()
                ))
                .into()
            })
        }
        Ok(Some(match use_case {
            UseCase::Add => Self::Add(read(use_case, params)?),
            UseCase::Edit => Self::Edit(read(use_case, params)?),
            UseCase::Submit => Self::Submit(read(use_case, params)?),
            UseCase::Cancel => Self::Cancel(read(use_case, params)?),
            UseCase::DependencyAdd => Self::DependencyAdd(read(use_case, params)?),
            UseCase::DependencyRemove => Self::DependencyRemove(read(use_case, params)?),
            UseCase::SetGoal => Self::SetGoal(read(use_case, params)?),
            UseCase::SetPaths => Self::SetPaths(read(use_case, params)?),
            UseCase::SetPriority => Self::SetPriority(read(use_case, params)?),
            UseCase::Revisit => Self::Revisit(read(use_case, params)?),
            UseCase::JudgeFollowUp => Self::JudgeFollowUp(read(use_case, params)?),
            UseCase::Draft => Self::Draft(read(use_case, params)?),
            UseCase::Ready => Self::Ready(read(use_case, params)?),
            UseCase::GoalAdd => Self::GoalAdd(read(use_case, params)?),
            UseCase::GoalEdit => Self::GoalEdit(read(use_case, params)?),
            UseCase::GoalClose => Self::GoalClose(read(use_case, params)?),
            UseCase::GoalReady => Self::GoalReady(read(use_case, params)?),
            UseCase::GoalReview => Self::GoalReview(read(use_case, params)?),
            UseCase::ProposalWithdraw => Self::ProposalWithdraw(read(use_case, params)?),
            UseCase::RequestAdd => Self::RequestAdd(read(use_case, params)?),
            UseCase::RequestDecline => Self::RequestDecline(read(use_case, params)?),
            _ => return Ok(None),
        }))
    }

    /// The use case of this write.
    pub const fn use_case(&self) -> UseCase {
        match self {
            Self::Add(_) => UseCase::Add,
            Self::Edit(_) => UseCase::Edit,
            Self::Submit(_) => UseCase::Submit,
            Self::Cancel(_) => UseCase::Cancel,
            Self::DependencyAdd(_) => UseCase::DependencyAdd,
            Self::DependencyRemove(_) => UseCase::DependencyRemove,
            Self::SetGoal(_) => UseCase::SetGoal,
            Self::SetPaths(_) => UseCase::SetPaths,
            Self::SetPriority(_) => UseCase::SetPriority,
            Self::Revisit(_) => UseCase::Revisit,
            Self::JudgeFollowUp(_) => UseCase::JudgeFollowUp,
            Self::Draft(_) => UseCase::Draft,
            Self::Ready(_) => UseCase::Ready,
            Self::GoalAdd(_) => UseCase::GoalAdd,
            Self::GoalEdit(_) => UseCase::GoalEdit,
            Self::GoalClose(_) => UseCase::GoalClose,
            Self::GoalReady(_) => UseCase::GoalReady,
            Self::GoalReview(_) => UseCase::GoalReview,
            Self::ProposalWithdraw(_) => UseCase::ProposalWithdraw,
            Self::RequestAdd(_) => UseCase::RequestAdd,
            Self::RequestDecline(_) => UseCase::RequestDecline,
        }
    }

    /// The use case and the params a client sends for this write: what
    /// [`Self::parse`] reads back as the same write.
    pub fn request(&self) -> Result<(UseCase, Value)> {
        let params = match self {
            Self::Add(write) => serde_json::to_value(write),
            Self::Edit(write) => serde_json::to_value(write),
            Self::Submit(write) => serde_json::to_value(write),
            Self::Cancel(write) => serde_json::to_value(write),
            Self::DependencyAdd(write) | Self::DependencyRemove(write) => {
                serde_json::to_value(write)
            }
            Self::SetGoal(write) => serde_json::to_value(write),
            Self::SetPaths(write) => serde_json::to_value(write),
            Self::SetPriority(write) => serde_json::to_value(write),
            Self::Revisit(write) => serde_json::to_value(write),
            Self::JudgeFollowUp(write) => serde_json::to_value(write),
            Self::Draft(write)
            | Self::GoalReady(write)
            | Self::GoalReview(write)
            | Self::ProposalWithdraw(write) => serde_json::to_value(write),
            Self::Ready(write) => serde_json::to_value(write),
            Self::GoalAdd(write) => serde_json::to_value(write),
            Self::GoalEdit(write) => serde_json::to_value(write),
            Self::GoalClose(write) => serde_json::to_value(write),
            Self::RequestAdd(write) => serde_json::to_value(write),
            Self::RequestDecline(write) => serde_json::to_value(write),
        }?;
        Ok((self.use_case(), params))
    }
}

/// Run `write` as `actor` on `store`, authorized by `authorizer` in the
/// application ([`Planning`], [`Requests`]), which records a refusal as
/// `authorization_denied`: the JSON the command prints. `owner` gives the
/// planner a `submit`'s proposal belongs to, asked only for a submit.
pub fn answer<S: PlanningStore + RequestStore>(
    store: &mut S,
    actor: &ActorContext,
    authorizer: &dyn Authorizer,
    write: &QueueWrite,
    owner: impl FnOnce() -> Result<PlannerOwner>,
) -> Result<Value> {
    macro_rules! planning {
        () => {
            Planning::new(store, actor, authorizer)
        };
    }
    Ok(match write.clone() {
        QueueWrite::Add(write) => serde_json::to_value(
            planning!().add(NewTask {
                title: write.title,
                description: write.description,
                acceptance: write.acceptance,
                verification_commands: write.verify,
                dependencies: write.depends_on.into_iter().map(TaskId::new).collect(),
                goal_dependencies: write.depends_on_goal.into_iter().map(GoalId::new).collect(),
                goal_id: write.goal.map(GoalId::new),
                context: write.context,
                required_evidence: write
                    .evidence
                    .iter()
                    .map(|name| name.parse())
                    .collect::<Result<_, _>>()?,
                paths: write.paths,
                priority: parsed(&write.priority)?,
                change: parsed(&write.change)?,
                provider: parsed(&write.provider)?,
                worker_mode: worker_mode(write.headless, write.interactive),
                wait_for_build: write.wait_for_build,
                execution_class: parsed(&write.execution_class)?.unwrap_or_default(),
            })?,
        )?,
        QueueWrite::Edit(write) => {
            let replaced =
                |values: Vec<String>, none: bool| (none || !values.is_empty()).then_some(values);
            let required_evidence = replaced(write.evidence, write.no_evidence)
                .map(|names| names.iter().map(|name| name.parse()).collect())
                .transpose()?;
            serde_json::to_value(
                planning!().edit(
                    TaskId::new(write.task),
                    TaskEdit {
                        title: write.title,
                        description: write.description,
                        acceptance: write.acceptance,
                        verification_commands: replaced(write.verify, write.no_verify),
                        required_evidence,
                        paths: replaced(write.paths, write.no_paths),
                        context: write.context,
                        change: parsed(&write.change)?,
                        provider: parsed(&write.provider)?,
                        worker_mode: worker_mode(write.headless, write.interactive),
                        wait_for_build: (write.wait_for_build || write.no_wait_for_build)
                            .then_some(write.wait_for_build),
                        execution_class: parsed(&write.execution_class)?,
                    },
                )?,
            )?
        }
        QueueWrite::Submit(write) => {
            let owner = owner()?;
            serde_json::to_value(
                planning!().submit(
                    Submission {
                        tasks: write.tasks.into_iter().map(TaskId::new).collect(),
                        goals: write.goal.into_iter().map(GoalId::new).collect(),
                        proposal: write.proposal.map(ProposalId::new),
                        owner,
                    },
                    &write
                        .finding
                        .into_iter()
                        .map(FindingId::new)
                        .collect::<Vec<_>>(),
                )?,
            )?
        }
        QueueWrite::Cancel(write) => serde_json::to_value(planning!().cancel(
            TaskId::new(write.id),
            NewCancel::by_hand(
                write.reason,
                write.duplicate_of.map(TaskId::new),
                write.note,
            ),
        )?)?,
        QueueWrite::DependencyAdd(write) => {
            let on = dependency(&write)?;
            serde_json::to_value(planning!().add_dependency(TaskId::new(write.task), on)?)?
        }
        QueueWrite::DependencyRemove(write) => {
            let on = dependency(&write)?;
            serde_json::to_value(planning!().remove_dependency(TaskId::new(write.task), on)?)?
        }
        QueueWrite::SetGoal(write) => serde_json::to_value(
            planning!().set_goal(TaskId::new(write.task), write.goal.map(GoalId::new))?,
        )?,
        QueueWrite::SetPaths(write) => {
            serde_json::to_value(planning!().set_paths(TaskId::new(write.task), write.paths)?)?
        }
        QueueWrite::SetPriority(write) => {
            // Without a level, --inherit clears the task's own priority.
            let level = parsed(&write.level)?;
            serde_json::to_value(planning!().set_priority(TaskId::new(write.task), level)?)?
        }
        QueueWrite::Revisit(write) => {
            let change = match write.at {
                Some(at) => RevisitChange::Set {
                    at: revisit_seconds(&at).with_context(|| {
                        format!("--at {at} is not an RFC 3339 time (2026-10-04T12:00:00Z)")
                    })?,
                    note: write.note,
                },
                None => RevisitChange::Clear,
            };
            let revisit = planning!().revisit(TaskId::new(write.task), change)?;
            json!({"task_id": write.task, "revisit": revisit})
        }
        QueueWrite::JudgeFollowUp(write) => planning!().judge_follow_up(
            TaskId::new(write.task),
            MembershipJudgement {
                classification: write.classification.parse()?,
                acceptance_items: write.acceptance_item,
                reason: write.reason,
                evidence: write.evidence,
                destination_goal_id: write.destination_goal.map(GoalId::new),
                source_goal_id: write.source_goal.map(GoalId::new),
                corrects: write.corrects,
            },
        )?,
        QueueWrite::Draft(write) => {
            serde_json::to_value(planning!().draft(TaskId::new(write.id))?)?
        }
        QueueWrite::Ready(write) => {
            serde_json::to_value(planning!().ready(TaskId::new(write.id), write.bypass_review)?)?
        }
        QueueWrite::GoalAdd(write) => serde_json::to_value(planning!().add_goal(NewGoal {
            title: write.title,
            description: write.description,
            acceptance: write.acceptance,
            constraints: write.constraints,
            doc: write.doc,
            draft: write.draft,
            priority: parsed(&write.priority)?,
            tags: goal_tags(&write.tag)?,
        })?)?,
        QueueWrite::GoalEdit(write) => serde_json::to_value(
            planning!().edit_goal(
                GoalId::new(write.id),
                GoalEdit {
                    title: write.title,
                    description: write.description,
                    acceptance: write.acceptance,
                    constraints: write.constraints,
                    doc: write.doc,
                    priority: parsed(&write.priority)?,
                    tags: (!write.tag.is_empty() || write.no_tags)
                        .then(|| goal_tags(&write.tag))
                        .transpose()?,
                },
            )?,
        )?,
        QueueWrite::GoalClose(write) => serde_json::to_value(
            planning!().close_goal(GoalId::new(write.id), write.verdict.parse::<GoalVerdict>()?)?,
        )?,
        QueueWrite::GoalReady(write) => {
            serde_json::to_value(planning!().ready_goal(GoalId::new(write.id))?)?
        }
        QueueWrite::GoalReview(write) => planning!().review_goal(GoalId::new(write.id))?,
        QueueWrite::ProposalWithdraw(write) => {
            serde_json::to_value(planning!().withdraw(ProposalId::new(write.id))?)?
        }
        QueueWrite::RequestAdd(write) => {
            let Some(text) = write.text else {
                Gate { actor, authorizer }.authorize(
                    &*store,
                    Capability::RequestRecord,
                    &Resource::Queue,
                )?;
                return Err(BadWrite(
                    "request_add carries no words: the caller read none, its role taken for one \
                     that may not record a request"
                        .to_owned(),
                )
                .into());
            };
            let request = NewPlanRequest {
                text,
                note: write.note,
                refs: write
                    .refs
                    .iter()
                    .map(|value| RequestRef::parse(value))
                    .collect::<Result<_, _>>()?,
                priority: parsed(&write.priority)?,
            };
            serde_json::to_value(Requests::new(store, actor, authorizer).record(&request)?)?
        }
        QueueWrite::RequestDecline(write) => {
            serde_json::to_value(Requests::new(store, actor, authorizer).decline(
                RequestId::new(write.id),
                &write.reason,
                write.continue_run,
            )?)?
        }
    })
}

/// An option given as its text, read as the command line's parser reads it.
fn parsed<T: std::str::FromStr>(value: &Option<String>) -> Result<Option<T>>
where
    T::Err: std::error::Error + Send + Sync + 'static,
{
    Ok(value.as_deref().map(str::parse).transpose()?)
}

/// What the owner of a submit's proposal is read from: the record of the
/// runtime's planner a principal names.
pub trait PlannerRecords {
    /// Planner `id`'s record; a planner the queue does not hold is an
    /// error.
    fn planner_record(&self, id: PlannerId) -> Result<PlannerSession>;
}

/// The planner a proposal `actor` submits belongs to, as the queue service
/// finds it from the caller's principal rather than its environment: a
/// planner's own (the origin and the background handle of its record,
/// ADR-t1404-1 decision 10; a handle of no background wrapper names none),
/// a person's for any other actor (the user, the inbox). A planner whose
/// actor id names no planner, or one the queue does not hold, is an error.
pub fn principal_owner(
    planners: &dyn PlannerRecords,
    actor: &ActorContext,
) -> Result<PlannerOwner> {
    if actor.role() != ActorRole::Planner {
        return Ok(PlannerOwner {
            origin: PlannerOrigin::Person,
            workspace_id: None,
        });
    }
    let id = actor
        .actor_id()
        .strip_prefix("planner:")
        .and_then(|id| id.trim().parse().ok())
        .with_context(|| format!("the planner {} names no planner", actor.actor_id()))?;
    let planner = planners.planner_record(PlannerId::new(id))?;
    Ok(PlannerOwner {
        origin: planner.origin,
        workspace_id: planner
            .workspace_id
            .filter(|id| crate::domain::background_wrapper::is_background(id)),
    })
}

/// The worker mode `--headless` and `--interactive` name.
const fn worker_mode(headless: bool, interactive: bool) -> Option<WorkerMode> {
    match (headless, interactive) {
        (true, _) => Some(WorkerMode::Headless),
        (_, true) => Some(WorkerMode::Interactive),
        _ => None,
    }
}

/// What a `dependency` command names: `goal` wins over a predecessor, and
/// one of them is needed, as the command line's parser requires.
fn dependency(write: &DependencyWrite) -> Result<Dependency> {
    match (write.predecessor, write.goal) {
        (_, Some(goal)) => Ok(Dependency::Goal(GoalId::new(goal))),
        (Some(predecessor), None) => Ok(Dependency::Task(TaskId::new(predecessor))),
        (None, None) => {
            Err(BadWrite("a dependency names a predecessor or a goal".to_owned()).into())
        }
    }
}

/// The goal tags given (`--tag`), each checked for its form (ADR-t1639-1
/// decision 6).
fn goal_tags(tags: &[String]) -> Result<Vec<GoalTag>> {
    Ok(tags
        .iter()
        .map(|tag| tag.parse())
        .collect::<Result<_, _>>()?)
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;

    use anyhow::anyhow;

    use super::*;
    use crate::application::commands::DenialLog;
    use crate::domain::authorization::DenyReason;
    use crate::domain::plan_request::PlanRequest;
    use crate::domain::{
        ActorRole, AuthorizationError, DraftRevisit, Goal, PlannerId, PlannerOrigin, Priority,
        Proposal, StaticPolicy, Task, TaskAction, TaskDetail, TaskStatus,
    };

    /// A store whose tasks are drafts, whose proposals are planner 1's and
    /// whose requests planner 1 was opened for; it records the refusals
    /// and answers every change with the error `reached`, so a test sees
    /// whether a write got past the policy.
    #[derive(Default)]
    struct Store {
        denials: RefCell<Vec<Value>>,
    }

    fn reached<T>() -> Result<T> {
        Err(anyhow!("reached"))
    }

    impl PlanningStore for Store {
        fn task_status(&self, _: TaskId) -> Result<TaskStatus> {
            Ok(TaskStatus::Draft)
        }
        fn proposal_owner(&self, _: ProposalId) -> Result<Option<String>> {
            Ok(Some("planner:1".into()))
        }
        fn record_denial(&self, payload: Value) -> Result<()> {
            self.denials.borrow_mut().push(payload);
            Ok(())
        }
        fn add(&mut self, _: NewTask) -> Result<Task> {
            reached()
        }
        fn edit_task(&mut self, _: TaskId, _: TaskEdit, _: TaskStatus) -> Result<Task> {
            reached()
        }
        fn judge_follow_up(&mut self, _: TaskId, _: MembershipJudgement, _: &str) -> Result<Value> {
            reached()
        }
        fn set_goal(&mut self, _: TaskId, _: Option<GoalId>, _: TaskStatus) -> Result<Task> {
            reached()
        }
        fn set_paths(&mut self, _: TaskId, _: Vec<String>, _: TaskStatus) -> Result<Task> {
            reached()
        }
        fn set_priority(&mut self, _: TaskId, _: Option<Priority>, _: TaskStatus) -> Result<Task> {
            reached()
        }
        fn revisit_draft(
            &mut self,
            _: TaskId,
            _: RevisitChange,
            _: &str,
            _: &str,
        ) -> Result<Option<DraftRevisit>> {
            reached()
        }
        fn transition(&mut self, _: TaskId, _: TaskAction, _: TaskStatus) -> Result<Task> {
            reached()
        }
        fn cancel(&mut self, _: TaskId, _: NewCancel, _: TaskStatus) -> Result<Task> {
            reached()
        }
        fn add_dependency(&mut self, _: TaskId, _: Dependency, _: TaskStatus) -> Result<()> {
            reached()
        }
        fn remove_dependency(&mut self, _: TaskId, _: Dependency, _: TaskStatus) -> Result<()> {
            reached()
        }
        fn show(&mut self, _: TaskId) -> Result<TaskDetail> {
            reached()
        }
        fn submit(&mut self, _: Submission, _: &[FindingId]) -> Result<Proposal> {
            reached()
        }
        fn withdraw_proposal(&mut self, _: ProposalId) -> Result<Proposal> {
            reached()
        }
        fn add_goal(&mut self, _: NewGoal) -> Result<Goal> {
            reached()
        }
        fn edit_goal(&mut self, _: GoalId, _: GoalEdit) -> Result<Goal> {
            reached()
        }
        fn ready_goal(&mut self, _: GoalId) -> Result<Goal> {
            reached()
        }
        fn close_goal(&mut self, _: GoalId, _: GoalVerdict) -> Result<Goal> {
            reached()
        }
        fn rearm_goal_review(&mut self, _: GoalId) -> Result<Value> {
            reached()
        }
    }

    impl DenialLog for Store {
        fn record_denial(&self, payload: Value) -> Result<()> {
            PlanningStore::record_denial(self, payload)
        }
    }

    impl RequestStore for Store {
        fn record_request(&mut self, _: &NewPlanRequest, _: &str, _: &str) -> Result<PlanRequest> {
            reached()
        }
        fn request_planner(&self, _: RequestId) -> Result<Option<PlannerId>> {
            Ok(Some(PlannerId::new(1)))
        }
        fn decline_request(
            &mut self,
            _: RequestId,
            _: &str,
            _: &str,
            _: Option<PlannerId>,
            _: bool,
        ) -> Result<PlanRequest> {
            reached()
        }
    }

    /// One write of each planning command, with the capability the
    /// command line asks for it (docs/design/authorization.md).
    fn every_write() -> Vec<(QueueWrite, Capability)> {
        let id = IdWrite { id: 1 };
        let dependency = DependencyWrite {
            task: 1,
            predecessor: Some(2),
            goal: None,
        };
        vec![
            (
                QueueWrite::Add(AddWrite {
                    title: "t".into(),
                    ..Default::default()
                }),
                Capability::TaskWrite,
            ),
            (
                QueueWrite::Edit(EditWrite {
                    task: 1,
                    title: Some("t".into()),
                    ..Default::default()
                }),
                Capability::TaskWrite,
            ),
            (
                QueueWrite::Submit(SubmitWrite {
                    tasks: vec![1],
                    replacement: Some("{}".into()),
                    ..Default::default()
                }),
                Capability::ProposalSubmit,
            ),
            (
                QueueWrite::Cancel(CancelWrite {
                    id: 1,
                    reason: Some("stale".into()),
                    duplicate_of: None,
                    note: None,
                }),
                Capability::TaskCancel,
            ),
            (
                QueueWrite::DependencyAdd(dependency.clone()),
                Capability::TaskWrite,
            ),
            (
                QueueWrite::DependencyRemove(dependency),
                Capability::TaskWrite,
            ),
            (
                QueueWrite::SetGoal(SetGoalWrite {
                    task: 1,
                    goal: None,
                }),
                Capability::TaskWrite,
            ),
            (
                QueueWrite::SetPaths(SetPathsWrite {
                    task: 1,
                    paths: Vec::new(),
                }),
                Capability::TaskWrite,
            ),
            (
                QueueWrite::SetPriority(SetPriorityWrite {
                    task: 1,
                    level: Some("high".into()),
                }),
                Capability::TaskWrite,
            ),
            (
                QueueWrite::Revisit(RevisitWrite {
                    task: 1,
                    at: None,
                    note: None,
                }),
                Capability::TaskWrite,
            ),
            (
                QueueWrite::JudgeFollowUp(JudgeFollowUpWrite {
                    task: 1,
                    classification: "undecided".into(),
                    acceptance_item: Vec::new(),
                    reason: "r".into(),
                    evidence: Vec::new(),
                    destination_goal: None,
                    source_goal: None,
                    corrects: None,
                }),
                Capability::FollowUpJudge,
            ),
            (QueueWrite::Draft(id.clone()), Capability::TaskWrite),
            (
                QueueWrite::Ready(ReadyWrite {
                    id: 1,
                    bypass_review: false,
                }),
                Capability::TaskReady,
            ),
            (
                QueueWrite::Ready(ReadyWrite {
                    id: 1,
                    bypass_review: true,
                }),
                Capability::TaskReadyBypassReview,
            ),
            (
                QueueWrite::GoalAdd(GoalAddWrite {
                    title: "g".into(),
                    ..Default::default()
                }),
                Capability::GoalWrite,
            ),
            (
                QueueWrite::GoalEdit(GoalEditWrite {
                    id: 1,
                    title: Some("g".into()),
                    ..Default::default()
                }),
                Capability::GoalWrite,
            ),
            (
                QueueWrite::GoalClose(GoalCloseWrite {
                    id: 1,
                    verdict: "abandoned".into(),
                }),
                Capability::GoalClose,
            ),
            (QueueWrite::GoalReady(id.clone()), Capability::GoalReady),
            (
                QueueWrite::GoalReview(id.clone()),
                Capability::GoalReviewRequest,
            ),
            (
                QueueWrite::ProposalWithdraw(id),
                Capability::ProposalWithdraw,
            ),
            (
                QueueWrite::RequestAdd(RequestAddWrite {
                    text: Some("plan it".into()),
                    note: None,
                    refs: Vec::new(),
                    priority: None,
                }),
                Capability::RequestRecord,
            ),
            (
                QueueWrite::RequestDecline(RequestDeclineWrite {
                    id: 1,
                    reason: "r".into(),
                    continue_run: true,
                }),
                Capability::RequestDecline,
            ),
        ]
    }

    /// The refusal `write` meets as `actor`, if any: `None` when it reached
    /// the store.
    fn refusal(actor: &ActorContext, write: &QueueWrite) -> (Option<AuthorizationError>, usize) {
        let mut store = Store::default();
        let error = answer(&mut store, actor, &StaticPolicy, write, || {
            Ok(PlannerOwner {
                origin: PlannerOrigin::Runtime,
                workspace_id: None,
            })
        })
        .unwrap_err();
        let refused = error.downcast_ref::<AuthorizationError>().cloned();
        if refused.is_none() {
            assert_eq!(error.to_string(), "reached", "{write:?}");
        }
        (refused, store.denials.borrow().len())
    }

    #[test]
    fn a_planner_is_refused_what_the_command_line_refuses_it_and_each_refusal_is_recorded() {
        let planner = ActorContext::instance(ActorRole::Planner, 1);
        let refused: Vec<UseCase> = every_write()
            .into_iter()
            .filter_map(|(write, capability)| {
                let (refusal, recorded) = refusal(&planner, &write);
                let refusal = refusal?;
                assert_eq!(refusal.capability, capability, "{write:?}");
                assert_eq!(refusal.reason, DenyReason::NotGranted, "{write:?}");
                assert_eq!(recorded, 1, "{write:?}");
                Some(write.use_case())
            })
            .collect();
        // docs/design/authorization.md: a planner has neither `ready`
        // (with or without the bypass) nor `goal ready`, `goal review` and
        // `request add`; its own proposal and its request's decline it has.
        assert_eq!(
            refused,
            [
                UseCase::Ready,
                UseCase::Ready,
                UseCase::GoalReady,
                UseCase::GoalReview,
                UseCase::RequestAdd,
            ]
        );
    }

    #[test]
    fn the_user_and_the_inbox_run_every_planning_command_but_the_planners_decline() {
        for actor in [
            ActorContext::user(),
            ActorContext::new(ActorRole::Inbox, "inbox:1"),
        ] {
            for (write, capability) in every_write() {
                let (refusal, recorded) = refusal(&actor, &write);
                if capability == Capability::RequestDecline {
                    let refusal = refusal.expect("a person declines no request");
                    assert_eq!(refusal.capability, capability);
                    assert_eq!(recorded, 1);
                } else {
                    assert!(refusal.is_none(), "{actor:?} {write:?}: {refusal:?}");
                    assert_eq!(recorded, 0);
                }
            }
        }
    }

    #[test]
    fn a_write_and_its_params_read_back_as_the_same_write() {
        for (write, _) in every_write() {
            let (use_case, params) = write.request().unwrap();
            assert_eq!(use_case, write.use_case());
            assert_eq!(QueueWrite::parse(use_case, &params).unwrap(), Some(write));
        }
        // A field the command has not is refused, and no use case of these
        // is no write.
        let unknown = QueueWrite::parse(UseCase::Draft, &json!({"id": 1, "full": true}));
        assert!(unknown.unwrap_err().downcast_ref::<BadWrite>().is_some());
        assert_eq!(
            QueueWrite::parse(UseCase::Show, &json!({"id": 1})).unwrap(),
            None
        );
        assert_eq!(QueueWrite::parse(UseCase::Mark, &json!({})).unwrap(), None);
    }

    #[test]
    fn a_request_add_that_read_no_words_is_refused_and_recorded_or_else_refused_unread() {
        let unread = QueueWrite::RequestAdd(RequestAddWrite {
            text: None,
            note: None,
            refs: Vec::new(),
            priority: None,
        });
        let (refusal, recorded) = refusal(&ActorContext::instance(ActorRole::Planner, 1), &unread);
        assert_eq!(refusal.unwrap().capability, Capability::RequestRecord);
        assert_eq!(recorded, 1);
        // A role its caller took for one that may not record, which may.
        let mut store = Store::default();
        let error = answer(
            &mut store,
            &ActorContext::user(),
            &StaticPolicy,
            &unread,
            || unreachable!("no submit"),
        )
        .unwrap_err();
        assert!(error.downcast_ref::<BadWrite>().is_some(), "{error}");
        assert!(store.denials.borrow().is_empty());
    }

    #[test]
    fn a_dependency_names_a_predecessor_or_a_goal_and_the_goal_wins() {
        let write = |predecessor, goal| DependencyWrite {
            task: 1,
            predecessor,
            goal,
        };
        assert_eq!(
            dependency(&write(Some(2), Some(3))).unwrap(),
            Dependency::Goal(GoalId::new(3))
        );
        assert_eq!(
            dependency(&write(Some(2), None)).unwrap(),
            Dependency::Task(TaskId::new(2))
        );
        assert!(
            dependency(&write(None, None))
                .unwrap_err()
                .downcast_ref::<BadWrite>()
                .is_some()
        );
    }
}

#[cfg(test)]
mod owner_tests {
    use super::*;
    use crate::domain::PlannerRoute;

    /// The runtime's planners 1 (in a background wrapper) and 2 (whose
    /// handle names no background wrapper); no other.
    struct Planners;

    impl PlannerRecords for Planners {
        fn planner_record(&self, id: PlannerId) -> Result<PlannerSession> {
            let workspace = match id.as_i64() {
                1 => "background:41:start",
                2 => "4f0c-cmux-uuid",
                _ => anyhow::bail!("planner {id} not found"),
            };
            Ok(PlannerSession {
                id,
                origin: PlannerOrigin::Runtime,
                proposal_id: None,
                draft_task_id: None,
                finding_id: None,
                request_id: None,
                workspace_id: Some(workspace.to_owned()),
                wrapper_pid: None,
                agent_pid: None,
                heartbeat_at: None,
                exit_code: None,
                exited_at: None,
                closed_at: None,
                error: None,
                created_at: 0,
                route: PlannerRoute::Headless,
                answer_wait_at: None,
            })
        }
    }

    #[test]
    fn a_proposal_belongs_to_the_principal_s_planner_or_else_to_a_person() {
        let person = PlannerOwner {
            origin: PlannerOrigin::Person,
            workspace_id: None,
        };
        for actor in [
            ActorContext::user(),
            ActorContext::new(ActorRole::Inbox, "inbox:7:then"),
        ] {
            assert_eq!(principal_owner(&Planners, &actor).unwrap(), person);
        }
        // A planner's from its record: its origin, and its handle only when
        // it names a background wrapper.
        assert_eq!(
            principal_owner(&Planners, &ActorContext::instance(ActorRole::Planner, 1)).unwrap(),
            PlannerOwner {
                origin: PlannerOrigin::Runtime,
                workspace_id: Some("background:41:start".to_owned()),
            }
        );
        assert_eq!(
            principal_owner(&Planners, &ActorContext::instance(ActorRole::Planner, 2)).unwrap(),
            PlannerOwner {
                origin: PlannerOrigin::Runtime,
                workspace_id: None,
            }
        );
        // A planner the queue does not hold, and one whose actor id names
        // none, own nothing.
        let missing =
            principal_owner(&Planners, &ActorContext::instance(ActorRole::Planner, 9)).unwrap_err();
        assert!(missing.to_string().contains("not found"), "{missing}");
        let unnamed = principal_owner(&Planners, &ActorContext::new(ActorRole::Planner, "planner"))
            .unwrap_err();
        assert!(
            unnamed.to_string().contains("names no planner"),
            "{unnamed}"
        );
    }
}
