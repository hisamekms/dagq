//! The queue the planning commands work on
//! ([`crate::application::commands::planning`]): the ports of the task
//! store, and what the authorization reads before them.

use crate::domain::EventKind;
use anyhow::Result;
use serde_json::Value;

use super::sqlite::{SqliteQueue, read_task};
use crate::application::commands::planning::{Dependency, PlanningStore};
use crate::application::{GoalReviewStore, RunLog, TaskStore};
use crate::domain::{
    FindingId, Goal, GoalEdit, GoalId, GoalVerdict, NewGoal, NewTask, Priority, Proposal,
    ProposalId, Submission, Task, TaskAction, TaskDetail, TaskEdit, TaskId, TaskStatus,
};

impl PlanningStore for SqliteQueue {
    fn task_status(&self, task: TaskId) -> Result<TaskStatus> {
        Ok(read_task(&self.conn, task)?.status())
    }

    fn proposal_owner(&self, proposal: ProposalId) -> Result<Option<String>> {
        super::proposals::owner_actor(&self.conn, proposal)
    }

    fn record_denial(&self, payload: Value) -> Result<()> {
        RunLog::record_queue_event(self, EventKind::AuthorizationDenied, payload).map(drop)
    }

    fn add(&mut self, task: NewTask) -> Result<Task> {
        TaskStore::add(self, task)
    }

    fn edit_task(&mut self, task: TaskId, edit: TaskEdit, authorized: TaskStatus) -> Result<Task> {
        TaskStore::edit_task(self, task, edit, authorized)
    }

    fn set_goal(&mut self, task: TaskId, goal: Option<GoalId>) -> Result<Task> {
        TaskStore::set_goal(self, task, goal)
    }

    fn set_paths(&mut self, task: TaskId, paths: Vec<String>) -> Result<Task> {
        TaskStore::set_paths(self, task, paths)
    }

    fn set_priority(&mut self, task: TaskId, priority: Priority) -> Result<Task> {
        TaskStore::set_priority(self, task, priority)
    }

    fn transition(&mut self, task: TaskId, action: TaskAction) -> Result<Task> {
        TaskStore::transition(self, task, action)
    }

    fn cancel_duplicate(&mut self, task: TaskId, duplicate_of: TaskId) -> Result<Task> {
        TaskStore::cancel_duplicate(self, task, duplicate_of)
    }

    fn add_dependency(&mut self, task: TaskId, on: Dependency) -> Result<()> {
        match on {
            Dependency::Task(predecessor) => TaskStore::add_dependency(self, task, predecessor),
            Dependency::Goal(goal) => TaskStore::add_goal_dependency(self, task, goal),
        }
    }

    fn remove_dependency(&mut self, task: TaskId, on: Dependency) -> Result<()> {
        match on {
            Dependency::Task(predecessor) => TaskStore::remove_dependency(self, task, predecessor),
            Dependency::Goal(goal) => TaskStore::remove_goal_dependency(self, task, goal),
        }
    }

    fn show(&mut self, task: TaskId) -> Result<TaskDetail> {
        TaskStore::show(self, task)
    }

    fn submit(&mut self, submission: Submission, findings: &[FindingId]) -> Result<Proposal> {
        self.submit_linking(submission, findings)
    }

    fn withdraw_proposal(&mut self, proposal: ProposalId) -> Result<Proposal> {
        TaskStore::withdraw_proposal(self, proposal)
    }

    fn add_goal(&mut self, goal: NewGoal) -> Result<Goal> {
        TaskStore::add_goal(self, goal)
    }

    fn edit_goal(&mut self, goal: GoalId, edit: GoalEdit) -> Result<Goal> {
        TaskStore::edit_goal(self, goal, edit)
    }

    fn ready_goal(&mut self, goal: GoalId) -> Result<Goal> {
        TaskStore::ready_goal(self, goal)
    }

    fn close_goal(&mut self, goal: GoalId, verdict: GoalVerdict) -> Result<Goal> {
        TaskStore::close_goal(self, goal, verdict)
    }

    fn rearm_goal_review(&mut self, goal: GoalId) -> Result<Value> {
        GoalReviewStore::rearm_goal_review(self, goal)
    }
}
