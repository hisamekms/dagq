//! The queue the planning commands work on
//! ([`crate::application::commands::planning`]): the ports of the task
//! store, and what the authorization reads before them.

use crate::domain::EventKind;
use anyhow::Result;
use serde_json::Value;

use super::sqlite::{SqliteQueue, read_task};
use crate::application::commands::planning::{Dependency, PlanningStore};
use crate::application::{EventStore, GoalReviewStore, TaskStore};
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
        EventStore::record_queue_event(self, EventKind::AuthorizationDenied, payload).map(drop)
    }

    fn add(&mut self, task: NewTask) -> Result<Task> {
        TaskStore::add(self, task)
    }

    fn edit_task(&mut self, task: TaskId, edit: TaskEdit, authorized: TaskStatus) -> Result<Task> {
        TaskStore::edit_task(self, task, edit, authorized)
    }

    fn judge_follow_up(
        &mut self,
        task: TaskId,
        judgement: crate::domain::follow_up::MembershipJudgement,
        role: &str,
    ) -> Result<Value> {
        SqliteQueue::judge_follow_up(self, task, judgement, role)
    }

    fn set_goal(
        &mut self,
        task: TaskId,
        goal: Option<GoalId>,
        authorized: TaskStatus,
    ) -> Result<Task> {
        self.set_goal_authorized(task, goal, Some(authorized))
    }

    fn set_paths(
        &mut self,
        task: TaskId,
        paths: Vec<String>,
        authorized: TaskStatus,
    ) -> Result<Task> {
        self.set_paths_authorized(task, paths, Some(authorized))
    }

    fn set_priority(
        &mut self,
        task: TaskId,
        priority: Option<Priority>,
        authorized: TaskStatus,
    ) -> Result<Task> {
        self.set_priority_authorized(task, priority, Some(authorized))
    }

    fn revisit_draft(
        &mut self,
        task: TaskId,
        change: crate::domain::follow_up::RevisitChange,
        role: &str,
        actor: &str,
    ) -> Result<Option<crate::domain::DraftRevisit>> {
        SqliteQueue::revisit_draft(self, task, change, role, actor)
    }

    fn transition(
        &mut self,
        task: TaskId,
        action: TaskAction,
        authorized: TaskStatus,
    ) -> Result<Task> {
        self.transition_authorized(task, action, Some(authorized))
    }

    fn cancel_duplicate(
        &mut self,
        task: TaskId,
        duplicate_of: TaskId,
        authorized: TaskStatus,
    ) -> Result<Task> {
        self.cancel_duplicate_authorized(task, duplicate_of, Some(authorized))
    }

    fn add_dependency(
        &mut self,
        task: TaskId,
        on: Dependency,
        authorized: TaskStatus,
    ) -> Result<()> {
        let authorized = Some(authorized);
        match on {
            Dependency::Task(predecessor) => {
                self.add_dependency_authorized(task, predecessor, authorized)
            }
            Dependency::Goal(goal) => self.add_goal_dependency_authorized(task, goal, authorized),
        }
    }

    fn remove_dependency(
        &mut self,
        task: TaskId,
        on: Dependency,
        authorized: TaskStatus,
    ) -> Result<()> {
        let authorized = Some(authorized);
        match on {
            Dependency::Task(predecessor) => {
                self.remove_dependency_authorized(task, predecessor, authorized)
            }
            Dependency::Goal(goal) => {
                self.remove_goal_dependency_authorized(task, goal, authorized)
            }
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
