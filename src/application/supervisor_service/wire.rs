//! How an operation's result crosses between the service and the
//! supervisor ([`Wire`]): as its own `Serialize` for a value whose JSON
//! holds all of it, and as its whole form for a task and a goal, whose
//! `Serialize` is the CLI's and leaves out what the CLI does not show
//! ([`crate::domain::task::TaskWire`], [`crate::domain::goal::GoalWire`]).

use std::collections::HashMap;
use std::hash::Hash;

use anyhow::Result;
use serde::Serialize;
use serde::de::DeserializeOwned;

use crate::application::{
    EndedRunWorkspace, EndedRunWorktree, HeartbeatWrite, LeasedRun, ResumeCandidate,
};
use crate::domain::goal::GoalWire;
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
    crate::domain::task_replan::ReplanGuard,
    crate::domain::task_replan::Fence,
    crate::domain::replan_snapshot::RestoreStart,
    crate::domain::task_hold::TaskHold,
    crate::domain::task_hold::HoldGuard,
    std::path::PathBuf,
    crate::domain::queue_service::Principal,
    crate::domain::telemetry::HeldTelemetry,
    crate::domain::telemetry::TargetFacts,
    crate::application::GraphInput,
    crate::domain::investigation::Investigations,
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
