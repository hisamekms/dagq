//! How an operation's result crosses between the service and the
//! supervisor ([`Wire`]): as its own `Serialize` for a value whose JSON
//! holds all of it, and as its whole form for a task and a goal, whose
//! `Serialize` is the CLI's and leaves out what the CLI does not show
//! ([`crate::domain::task::TaskWire`], [`crate::domain::goal::GoalWire`]).

use std::collections::{BTreeMap, HashMap, HashSet};
use std::hash::Hash;

use anyhow::Result;
use serde::Serialize;
use serde::de::DeserializeOwned;

use crate::application::{
    EndedRunWorkspace, EndedRunWorktree, HeartbeatWrite, LeasedRun, ResumeCandidate,
};
use crate::domain::goal::GoalWire;
use crate::domain::host_metrics::{HostSample, NodeSample, SampleSource, value_columns};
use crate::domain::task::TaskWire;
use crate::domain::{
    Ask, ClaimOutcome, EventId, Goal, GoalPredecessor, GoalSummary, Landing, Predecessor, RunEvent,
    RunId, RunLease, RunProcess, SupervisorRegistration, Task, TaskDetail, TaskId, TaskRun,
};

/// A result as it crosses: its form, and back.
pub trait Wire: Sized {
    type Form: Serialize + DeserializeOwned;
    fn into_form(self) -> Self::Form;
    fn from_form(form: Self::Form) -> Result<Self>;
}

/// Values whose own JSON holds all of them.
macro_rules! whole {
    ($($ty:ty),* $(,)?) => {
        $(impl Wire for $ty {
            type Form = Self;
            fn into_form(self) -> Self {
                self
            }
            fn from_form(form: Self) -> Result<Self> {
                Ok(form)
            }
        })*
    };
}

whole!(
    (),
    bool,
    usize,
    u32,
    u64,
    i64,
    String,
    EventId,
    RunId,
    TaskId,
    TaskRun,
    RunEvent,
    RunLease,
    RunProcess,
    SupervisorRegistration,
    ClaimOutcome,
    Landing,
    Ask,
    GoalSummary,
    HeartbeatWrite,
    LeasedRun,
    ResumeCandidate,
    EndedRunWorkspace,
    EndedRunWorktree,
    crate::domain::task_replan::ReplanOperation,
    crate::domain::replan_plan::ReplanBrief,
    crate::domain::replan_apply::ApplyOutcome,
    crate::domain::replan_apply::ChildCarry,
    crate::domain::replan_apply::GoalReplacements,
    crate::domain::replan_apply::UnresolvedReplacement,
    crate::application::ReviseGoalWait,
    crate::domain::task_replan::ReplanGuard,
    crate::domain::task_replan::Fence,
    crate::domain::replan_snapshot::RestoreStart,
    crate::domain::task_hold::TaskHold,
    crate::domain::task_hold::HoldGuard,
    std::path::PathBuf,
    crate::application::queue_service::Issued,
    crate::domain::queue_service::Principal,
    crate::domain::telemetry::HeldTelemetry,
    crate::domain::telemetry::TargetFacts,
    crate::domain::headless_job::ReviewTakeover,
    crate::application::GraphInput,
    crate::domain::investigation::Investigations,
    serde_json::Value,
    crate::domain::GoalId,
    crate::domain::ProposalId,
    crate::domain::FindingId,
    crate::domain::RequestId,
    crate::domain::ProposalStatus,
    crate::domain::FindingStatus,
    crate::domain::DraftOrigin,
    crate::domain::TaskChange,
    crate::domain::ci_watch::CiCheckRecorded,
    crate::domain::DraftBundleView,
    crate::domain::follow_up::GoalFollowUps,
    crate::domain::HoldOutcome,
    crate::domain::AskOutcome,
    crate::domain::ImprovementLimit,
    crate::domain::sessions::OpenSpan,
    crate::domain::plan_request::PlanRequest,
    crate::domain::RegisteredFollowUp,
    crate::domain::recurring::RecurringOutcome,
    crate::domain::NotePage,
    crate::domain::plan_review::PlanReviewCandidate,
    crate::domain::PlannerHandover,
    crate::domain::PlannerSession,
    crate::domain::related::RelatedPage,
    crate::domain::search::SearchPage,
    crate::domain::Finding,
    crate::domain::FindingView,
    crate::domain::StrandedDependency,
    crate::domain::lint::LintInput,
    crate::domain::Proposal,
    crate::application::KeptOutcome,
    crate::application::HeadlessJobRecord,
    crate::application::FindingPlannerStart,
    crate::application::GoalDecided,
    crate::application::GoalReviewApplied,
    crate::application::GoalReviewHold,
    crate::application::GoalReviewJob,
    crate::application::GoalReviewRecord,
    crate::application::PlanDecided,
    crate::application::PlanReviewApplied,
    crate::application::PlanReviewHold,
    crate::application::PlanReviewJob,
    crate::application::PlannerAnswerRoute,
    crate::application::RequestPlannerStart,
    crate::application::RevisingProposal,
    crate::application::Landing,
    crate::application::TaskPage,
    crate::application::WrittenBy,
    crate::domain::AskId,
    crate::domain::environment::RunResource,
    crate::domain::ledger::LedgerRow,
    crate::domain::session_step::SessionStep,
    crate::domain::session_step::SessionResource,
    crate::domain::session_step::SessionOwner,
    super::large::RowPart,
);

impl Wire for Task {
    type Form = TaskWire;
    fn into_form(self) -> TaskWire {
        self.into()
    }
    fn from_form(form: TaskWire) -> Result<Self> {
        Ok(form.into())
    }
}

impl Wire for Goal {
    type Form = GoalWire;
    fn into_form(self) -> GoalWire {
        self.into()
    }
    fn from_form(form: GoalWire) -> Result<Self> {
        Ok(form.into())
    }
}

/// A task's detail as it crosses: every part as its own JSON but the
/// task, whole.
#[derive(serde::Serialize, serde::Deserialize)]
pub struct TaskDetailForm {
    membership_judgements: Vec<serde_json::Value>,
    task: TaskWire,
    dependencies: Vec<TaskId>,
    goal_dependencies: Vec<crate::domain::GoalId>,
    duplicate_of: Option<TaskId>,
    cancel_reason: Option<String>,
    duplicates: Vec<TaskId>,
    duplicate_reasons: std::collections::BTreeMap<TaskId, String>,
    runs: Vec<TaskRun>,
    events: Vec<RunEvent>,
    processes: Vec<RunProcess>,
    origin: Option<crate::domain::TaskOrigin>,
    follow_up_drafts: Vec<crate::domain::FollowUpDraft>,
    revisit: Option<crate::domain::DraftRevisit>,
    asks: Vec<Ask>,
    replans: Vec<crate::domain::task_replan::ReplanOperation>,
    hold: Option<crate::domain::task_hold::TaskHold>,
}

impl Wire for TaskDetail {
    type Form = TaskDetailForm;
    fn into_form(self) -> TaskDetailForm {
        let TaskDetail {
            membership_judgements,
            task,
            dependencies,
            goal_dependencies,
            duplicate_of,
            cancel_reason,
            duplicates,
            duplicate_reasons,
            runs,
            events,
            processes,
            origin,
            follow_up_drafts,
            revisit,
            asks,
            replans,
            hold,
        } = self;
        TaskDetailForm {
            membership_judgements,
            task: task.into(),
            dependencies,
            goal_dependencies,
            duplicate_of,
            cancel_reason,
            duplicates,
            duplicate_reasons,
            runs,
            events,
            processes,
            origin,
            follow_up_drafts,
            revisit,
            asks,
            replans,
            hold,
        }
    }
    fn from_form(form: TaskDetailForm) -> Result<Self> {
        let TaskDetailForm {
            membership_judgements,
            task,
            dependencies,
            goal_dependencies,
            duplicate_of,
            cancel_reason,
            duplicates,
            duplicate_reasons,
            runs,
            events,
            processes,
            origin,
            follow_up_drafts,
            revisit,
            asks,
            replans,
            hold,
        } = form;
        Ok(Self {
            membership_judgements,
            task: task.into(),
            dependencies,
            goal_dependencies,
            duplicate_of,
            cancel_reason,
            duplicates,
            duplicate_reasons,
            runs,
            events,
            processes,
            origin,
            follow_up_drafts,
            revisit,
            asks,
            replans,
            hold,
        })
    }
}

/// A predecessor as it crosses.
#[derive(serde::Serialize, serde::Deserialize)]
pub struct PredecessorForm {
    task: TaskWire,
    integrated_run: Option<TaskRun>,
}

impl Wire for Predecessor {
    type Form = PredecessorForm;
    fn into_form(self) -> PredecessorForm {
        PredecessorForm {
            task: self.task.into(),
            integrated_run: self.integrated_run,
        }
    }
    fn from_form(form: PredecessorForm) -> Result<Self> {
        Ok(Self {
            task: form.task.into(),
            integrated_run: form.integrated_run,
        })
    }
}

/// A goal predecessor as it crosses.
#[derive(serde::Serialize, serde::Deserialize)]
pub struct GoalPredecessorForm {
    goal: GoalWire,
    tasks: Vec<PredecessorForm>,
}

impl Wire for GoalPredecessor {
    type Form = GoalPredecessorForm;
    fn into_form(self) -> GoalPredecessorForm {
        GoalPredecessorForm {
            goal: self.goal.into(),
            tasks: self.tasks.into_iter().map(Wire::into_form).collect(),
        }
    }
    fn from_form(form: GoalPredecessorForm) -> Result<Self> {
        Ok(Self {
            goal: form.goal.into(),
            tasks: form
                .tasks
                .into_iter()
                .map(Predecessor::from_form)
                .collect::<Result<_>>()?,
        })
    }
}

/// A sample of a node's load as it crosses: its second and its value of
/// each column the NodeSampleStore keeps, in their order
/// ([`value_columns`]), a value missing as null. The columns' names stay
/// out, so a day's samples cross in about the bytes of its CSV file; a
/// column the store does not keep does not cross, as the store drops it.
#[derive(Serialize, serde::Deserialize)]
pub struct HostSampleForm(i64, Vec<Option<f64>>);

impl Wire for HostSample {
    type Form = HostSampleForm;
    fn into_form(self) -> HostSampleForm {
        let values = value_columns()
            .iter()
            .map(|column| self.values.get(column).copied())
            .collect();
        HostSampleForm(self.unix, values)
    }
    fn from_form(HostSampleForm(unix, values): HostSampleForm) -> Result<Self> {
        anyhow::ensure!(
            values.len() == value_columns().len(),
            "a sample of {} columns is not one of the store's {}",
            values.len(),
            value_columns().len()
        );
        let mut sample = HostSample::new(unix);
        for (column, value) in value_columns().iter().zip(values) {
            sample.set(column, value);
        }
        Ok(sample)
    }
}

/// A row of the NodeSampleStore as it crosses, its sample as
/// [`HostSampleForm`].
#[derive(Serialize, serde::Deserialize)]
pub struct NodeSampleForm {
    id: i64,
    node_id: String,
    local_day: i64,
    source: SampleSource,
    sample: HostSampleForm,
    resources: crate::domain::environment::Resources,
}

impl Wire for NodeSample {
    type Form = NodeSampleForm;
    fn into_form(self) -> NodeSampleForm {
        NodeSampleForm {
            id: self.id,
            node_id: self.node_id,
            local_day: self.local_day,
            source: self.source,
            sample: self.sample.into_form(),
            resources: self.resources,
        }
    }
    fn from_form(form: NodeSampleForm) -> Result<Self> {
        Ok(Self {
            id: form.id,
            node_id: form.node_id,
            local_day: form.local_day,
            source: form.source,
            sample: HostSample::from_form(form.sample)?,
            resources: form.resources,
        })
    }
}

impl<T: Wire> Wire for Vec<T> {
    type Form = Vec<T::Form>;
    fn into_form(self) -> Self::Form {
        self.into_iter().map(Wire::into_form).collect()
    }
    fn from_form(form: Self::Form) -> Result<Self> {
        form.into_iter().map(T::from_form).collect()
    }
}

impl<T: Wire> Wire for Option<T> {
    type Form = Option<T::Form>;
    fn into_form(self) -> Self::Form {
        self.map(Wire::into_form)
    }
    fn from_form(form: Self::Form) -> Result<Self> {
        form.map(T::from_form).transpose()
    }
}

impl<A: Wire, B: Wire> Wire for (A, B) {
    type Form = (A::Form, B::Form);
    fn into_form(self) -> Self::Form {
        (self.0.into_form(), self.1.into_form())
    }
    fn from_form(form: Self::Form) -> Result<Self> {
        Ok((A::from_form(form.0)?, B::from_form(form.1)?))
    }
}

impl<K, V> Wire for HashMap<K, V>
where
    K: Wire + Eq + Hash,
    K::Form: Eq + Hash,
    V: Wire,
{
    type Form = HashMap<K::Form, V::Form>;
    fn into_form(self) -> Self::Form {
        self.into_iter()
            .map(|(key, value)| (key.into_form(), value.into_form()))
            .collect()
    }
    fn from_form(form: Self::Form) -> Result<Self> {
        form.into_iter()
            .map(|(key, value)| Ok((K::from_form(key)?, V::from_form(value)?)))
            .collect()
    }
}

impl<T> Wire for HashSet<T>
where
    T: Wire + Eq + Hash,
    T::Form: Eq + Hash,
{
    type Form = Vec<T::Form>;
    fn into_form(self) -> Self::Form {
        self.into_iter().map(Wire::into_form).collect()
    }
    fn from_form(form: Self::Form) -> Result<Self> {
        form.into_iter().map(T::from_form).collect()
    }
}

impl<K, V> Wire for BTreeMap<K, V>
where
    K: Wire + Ord,
    V: Wire,
{
    type Form = Vec<(K::Form, V::Form)>;
    fn into_form(self) -> Self::Form {
        self.into_iter()
            .map(|(key, value)| (key.into_form(), value.into_form()))
            .collect()
    }
    fn from_form(form: Self::Form) -> Result<Self> {
        form.into_iter()
            .map(|(key, value)| Ok((K::from_form(key)?, V::from_form(value)?)))
            .collect()
    }
}

/// A draft the runtime opens a planner for, as it crosses: the task whole.
#[derive(serde::Serialize, serde::Deserialize)]
pub struct DraftTargetForm {
    task: TaskWire,
    origin: crate::domain::DraftOrigin,
    material: serde_json::Value,
    planners: usize,
    revisit: Option<crate::domain::DraftRevisit>,
}

impl Wire for crate::domain::follow_up::DraftTarget {
    type Form = DraftTargetForm;
    fn into_form(self) -> DraftTargetForm {
        DraftTargetForm {
            task: self.task.into(),
            origin: self.origin,
            material: self.material,
            planners: self.planners,
            revisit: self.revisit,
        }
    }
    fn from_form(form: DraftTargetForm) -> Result<Self> {
        Ok(Self {
            task: form.task.into(),
            origin: form.origin,
            material: form.material,
            planners: form.planners,
            revisit: form.revisit,
        })
    }
}

/// A goal's detail as it crosses: every part as its own JSON but the
/// goal, whole.
#[derive(serde::Serialize, serde::Deserialize)]
pub struct GoalDetailForm {
    acceptance_version: i64,
    follow_up_memberships: Vec<serde_json::Value>,
    goal: GoalWire,
    closed: bool,
    tasks: Vec<crate::domain::GoalTask>,
    dependents: Vec<crate::domain::GoalTask>,
    events: Vec<RunEvent>,
}

impl Wire for crate::domain::GoalDetail {
    type Form = GoalDetailForm;
    fn into_form(self) -> GoalDetailForm {
        let crate::domain::GoalDetail {
            acceptance_version,
            follow_up_memberships,
            goal,
            closed,
            tasks,
            dependents,
            events,
        } = self;
        GoalDetailForm {
            acceptance_version,
            follow_up_memberships,
            goal: goal.into(),
            closed,
            tasks,
            dependents,
            events,
        }
    }
    fn from_form(form: GoalDetailForm) -> Result<Self> {
        let GoalDetailForm {
            acceptance_version,
            follow_up_memberships,
            goal,
            closed,
            tasks,
            dependents,
            events,
        } = form;
        Ok(Self {
            acceptance_version,
            follow_up_memberships,
            goal: goal.into(),
            closed,
            tasks,
            dependents,
            events,
        })
    }
}

/// How the opening of a draft planner ended, as it crosses: each draft
/// as [`DraftTargetForm`].
#[derive(serde::Serialize, serde::Deserialize)]
pub enum DraftPlannerStartForm {
    Opened {
        planner: Box<crate::domain::PlannerSession>,
        key: crate::domain::BundleKey,
        members: Vec<(DraftTargetForm, usize)>,
        exhausted: Vec<TaskId>,
    },
    Exhausted {
        drafts: Vec<TaskId>,
    },
    Skipped,
}

impl Wire for crate::application::DraftPlannerStart {
    type Form = DraftPlannerStartForm;
    fn into_form(self) -> DraftPlannerStartForm {
        match self {
            Self::Opened {
                planner,
                key,
                members,
                exhausted,
            } => DraftPlannerStartForm::Opened {
                planner,
                key,
                members: members.into_form(),
                exhausted,
            },
            Self::Exhausted { drafts } => DraftPlannerStartForm::Exhausted { drafts },
            Self::Skipped => DraftPlannerStartForm::Skipped,
        }
    }
    fn from_form(form: DraftPlannerStartForm) -> Result<Self> {
        Ok(match form {
            DraftPlannerStartForm::Opened {
                planner,
                key,
                members,
                exhausted,
            } => Self::Opened {
                planner,
                key,
                members: Wire::from_form(members)?,
                exhausted,
            },
            DraftPlannerStartForm::Exhausted { drafts } => Self::Exhausted { drafts },
            DraftPlannerStartForm::Skipped => Self::Skipped,
        })
    }
}
