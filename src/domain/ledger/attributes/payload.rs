//! The payloads the fold of the attributes reads, restored as typed values
//! with [`restore`]: each type names the keys it reads once. They restore
//! what earlier binaries wrote too, so they never refuse a payload: a
//! missing key, `null` or a value of another type reads as absent.

use serde::Deserialize;

use crate::domain::run::payload::{nullable, typed};

pub(super) use crate::domain::run::payload::restore;

/// `task_created` and a task's `attributes_baseline`; the baseline adds
/// the origin, the predecessors, the goal dependencies and the unfinished
/// run. A binary before the record wrote no `title`.
#[derive(Debug, Default, Deserialize)]
pub(super) struct TaskRecord {
    #[serde(default, deserialize_with = "typed")]
    pub(super) title: Option<String>,
    #[serde(default, deserialize_with = "typed")]
    pub(super) status: Option<String>,
    #[serde(default, deserialize_with = "typed")]
    pub(super) goal_id: Option<i64>,
    #[serde(default, deserialize_with = "typed")]
    pub(super) change: Option<String>,
    #[serde(default, deserialize_with = "typed")]
    pub(super) execution_class: Option<String>,
    /// Its own priority, absent to inherit.
    #[serde(default, deserialize_with = "typed")]
    pub(super) priority: Option<String>,
    /// Who set its own priority, as stored.
    #[serde(default, deserialize_with = "typed")]
    pub(super) priority_by: Option<String>,
    #[serde(default, deserialize_with = "typed")]
    pub(super) origin: Option<String>,
    #[serde(default, deserialize_with = "typed")]
    pub(super) depends_on: Option<Vec<i64>>,
    #[serde(default, deserialize_with = "typed")]
    pub(super) goal_dependencies: Option<Vec<i64>>,
    #[serde(default, deserialize_with = "typed")]
    pub(super) unfinished_run: Option<String>,
}

/// `task_edited`: the new values of the fields it changed.
#[derive(Debug, Default, Deserialize)]
pub(super) struct TaskEdited {
    #[serde(default, deserialize_with = "typed")]
    pub(super) to: Option<EditedTo>,
}

#[derive(Debug, Default, Deserialize)]
pub(super) struct EditedTo {
    #[serde(default, deserialize_with = "typed")]
    pub(super) title: Option<String>,
    /// Present when the change changed; `null` takes it away.
    #[serde(default, deserialize_with = "nullable")]
    pub(super) change: Option<Option<String>>,
    #[serde(default, deserialize_with = "typed")]
    pub(super) execution_class: Option<String>,
}

/// `task_status_changed` and `goal_status_changed`.
#[derive(Debug, Default, Deserialize)]
pub(super) struct StatusChanged {
    #[serde(default, deserialize_with = "typed")]
    pub(super) to: Option<String>,
}

/// `task_goal_changed`: the goal it moved to, absent out of any goal.
#[derive(Debug, Default, Deserialize)]
pub(super) struct GoalChanged {
    #[serde(default, deserialize_with = "typed")]
    pub(super) to: Option<i64>,
}

/// `run_unfinished_changed`.
#[derive(Debug, Default, Deserialize)]
pub(super) struct UnfinishedChanged {
    #[serde(default, deserialize_with = "typed")]
    pub(super) unfinished: Option<bool>,
}

/// `task_priority_changed`. A binary before the record wrote no `own`.
#[derive(Debug, Default, Deserialize)]
pub(super) struct PriorityChanged {
    /// The own priority after the change; `null` to inherit.
    #[serde(default, deserialize_with = "nullable")]
    pub(super) own: Option<Option<String>>,
    /// Who set the own priority, as stored.
    #[serde(default, deserialize_with = "typed")]
    pub(super) own_by: Option<String>,
    /// The base priority after the change and where it comes from.
    #[serde(default, deserialize_with = "typed")]
    pub(super) to: Option<String>,
    #[serde(default, deserialize_with = "typed")]
    pub(super) to_source: Option<String>,
}

/// `dependency_added` and `dependency_removed`.
#[derive(Debug, Default, Deserialize)]
pub(super) struct DependencyChanged {
    #[serde(default, deserialize_with = "typed")]
    pub(super) predecessor_id: Option<i64>,
}

/// `goal_dependency_added` and `goal_dependency_removed`.
#[derive(Debug, Default, Deserialize)]
pub(super) struct GoalDependencyChanged {
    #[serde(default, deserialize_with = "typed")]
    pub(super) goal_id: Option<i64>,
}

/// `draft_origin_recorded`.
#[derive(Debug, Default, Deserialize)]
pub(super) struct OriginRecorded {
    #[serde(default, deserialize_with = "typed")]
    pub(super) origin: Option<String>,
}

/// A goal as its JSON shows it (`goal_created`'s `goal`, `goal_updated`'s
/// `new`, a goal's `attributes_baseline`).
#[derive(Debug, Default, Deserialize)]
pub(super) struct GoalRecord {
    #[serde(default, deserialize_with = "typed")]
    pub(super) title: Option<String>,
    #[serde(default, deserialize_with = "typed")]
    pub(super) status: Option<String>,
    #[serde(default, deserialize_with = "typed")]
    pub(super) priority: Option<String>,
    #[serde(default, deserialize_with = "typed")]
    pub(super) priority_by: Option<String>,
    #[serde(default, deserialize_with = "typed")]
    pub(super) tags: Option<Vec<String>>,
    #[serde(default, deserialize_with = "typed")]
    pub(super) verdict: Option<String>,
    /// Set by the close; absent while the goal is open.
    #[serde(default, deserialize_with = "typed")]
    pub(super) closed_at: Option<String>,
}

/// `goal_created` and a goal's `attributes_baseline`.
#[derive(Debug, Default, Deserialize)]
pub(super) struct GoalCreated {
    #[serde(default, deserialize_with = "typed")]
    pub(super) goal: Option<GoalRecord>,
}

/// `goal_updated`. `new` keeps who set the priority before the edit; the
/// stored setter is `priority_by` beside it (absent from a binary before
/// the record).
#[derive(Debug, Default, Deserialize)]
pub(super) struct GoalUpdated {
    #[serde(default, deserialize_with = "typed")]
    pub(super) new: Option<GoalRecord>,
    #[serde(default, deserialize_with = "typed")]
    pub(super) priority_by: Option<String>,
}

/// `goal_closed`.
#[derive(Debug, Default, Deserialize)]
pub(super) struct GoalClosed {
    #[serde(default, deserialize_with = "typed")]
    pub(super) verdict: Option<String>,
}
