//! [`SupervisorQueue`]'s operations: those [`super::dispatch`] answers go
//! to the service by their names, every other one to the connection of
//! its own ([`SupervisorQueue::local`]). Made from the ports' traits, one
//! method each, in their order; a method the traits add is a compile
//! error here until it is routed.

#![allow(clippy::wildcard_imports)]

use super::{SupervisorQueue, to_value};
use crate::application::*;
use crate::domain::*;
use anyhow::Result;
use std::collections::HashMap;
use std::path::Path;

use crate::application::Landing;
use crate::domain::cancel_reason::NewCancel;
use crate::domain::related::RelatedPage;
use crate::domain::search::{SearchPage, SearchQuery};

impl TaskReads for SupervisorQueue {
    fn list(&self, query: &TaskQuery) -> Result<TaskPage> {
        let local = self.local()?;
        TaskReads::list(&**local, query)
    }
    fn show(&mut self, task_id: TaskId) -> Result<TaskDetail> {
        self.remote("show", vec![to_value(task_id)?])
    }
    fn candidates(&self) -> Result<Vec<Task>> {
        self.remote("candidates", vec![])
    }
    fn build_waits(
        &self,
    ) -> Result<std::collections::HashMap<TaskId, Vec<crate::domain::Landing>>> {
        self.remote("build_waits", vec![])
    }
    fn graph_input(&self) -> Result<GraphInput> {
        self.remote("graph_input", vec![])
    }
    fn predecessors(&self, task_id: TaskId) -> Result<Vec<Predecessor>> {
        self.remote("predecessors", vec![to_value(task_id)?])
    }
    fn goal_predecessors(&self, task_id: TaskId) -> Result<Vec<GoalPredecessor>> {
        self.remote("goal_predecessors", vec![to_value(task_id)?])
    }
    fn tasks_in_progress(&self) -> Result<Vec<Task>> {
        self.remote("tasks_in_progress", vec![])
    }
    fn task_fenced(&self, task: TaskId) -> Result<bool> {
        self.remote("task_fenced", vec![to_value(task)?])
    }
    fn list_goals(&self) -> Result<Vec<GoalSummary>> {
        self.remote("list_goals", vec![])
    }
    fn show_goal(&mut self, goal_id: GoalId) -> Result<GoalDetail> {
        let mut local = self.local()?;
        TaskReads::show_goal(&mut **local, goal_id)
    }
    fn show_proposal(&self, proposal_id: ProposalId) -> Result<Proposal> {
        let local = self.local()?;
        TaskReads::show_proposal(&**local, proposal_id)
    }
    fn proposals(&self, all: bool) -> Result<Vec<Proposal>> {
        let local = self.local()?;
        TaskReads::proposals(&**local, all)
    }
    fn lint_input(&self, tasks: &[TaskId]) -> Result<LintInput> {
        let local = self.local()?;
        TaskReads::lint_input(&**local, tasks)
    }
    fn notes(&self, query: &NoteQuery) -> Result<NotePage> {
        let local = self.local()?;
        TaskReads::notes(&**local, query)
    }
}

impl TaskStore for SupervisorQueue {
    fn add(&mut self, task: NewTask) -> Result<Task> {
        let mut local = self.local()?;
        TaskStore::add(&mut **local, task)
    }
    fn transition(&mut self, task_id: TaskId, action: TaskAction) -> Result<Task> {
        let mut local = self.local()?;
        TaskStore::transition(&mut **local, task_id, action)
    }
    fn cancel(&mut self, task_id: TaskId, cancel: NewCancel) -> Result<Task> {
        let mut local = self.local()?;
        TaskStore::cancel(&mut **local, task_id, cancel)
    }
    fn add_dependency(&mut self, task_id: TaskId, predecessor_id: TaskId) -> Result<()> {
        let mut local = self.local()?;
        TaskStore::add_dependency(&mut **local, task_id, predecessor_id)
    }
    fn remove_dependency(&mut self, task_id: TaskId, predecessor_id: TaskId) -> Result<()> {
        let mut local = self.local()?;
        TaskStore::remove_dependency(&mut **local, task_id, predecessor_id)
    }
    fn add_goal_dependency(&mut self, task_id: TaskId, goal_id: GoalId) -> Result<()> {
        let mut local = self.local()?;
        TaskStore::add_goal_dependency(&mut **local, task_id, goal_id)
    }
    fn remove_goal_dependency(&mut self, task_id: TaskId, goal_id: GoalId) -> Result<()> {
        let mut local = self.local()?;
        TaskStore::remove_goal_dependency(&mut **local, task_id, goal_id)
    }
    fn ready_at(&self, tasks: &[TaskId]) -> Result<HashMap<TaskId, i64>> {
        self.remote("ready_at", vec![to_value(tasks)?])
    }
    fn investigations(&mut self) -> Result<crate::domain::investigation::Investigations> {
        self.remote("investigations", vec![])
    }
    fn claim(&mut self, base_commit: &CommitSha) -> Result<ClaimOutcome> {
        let mut local = self.local()?;
        TaskStore::claim(&mut **local, base_commit)
    }
    fn add_goal(&mut self, goal: NewGoal) -> Result<Goal> {
        let mut local = self.local()?;
        TaskStore::add_goal(&mut **local, goal)
    }
    fn edit_goal(&mut self, goal_id: GoalId, edit: GoalEdit) -> Result<Goal> {
        let mut local = self.local()?;
        TaskStore::edit_goal(&mut **local, goal_id, edit)
    }
    fn close_goal(&mut self, goal_id: GoalId, verdict: GoalVerdict) -> Result<Goal> {
        let mut local = self.local()?;
        TaskStore::close_goal(&mut **local, goal_id, verdict)
    }
    fn set_goal(&mut self, task_id: TaskId, goal_id: Option<GoalId>) -> Result<Task> {
        let mut local = self.local()?;
        TaskStore::set_goal(&mut **local, task_id, goal_id)
    }
    fn set_paths(&mut self, task_id: TaskId, paths: Vec<String>) -> Result<Task> {
        let mut local = self.local()?;
        TaskStore::set_paths(&mut **local, task_id, paths)
    }
    fn edit_task(
        &mut self,
        task_id: TaskId,
        edit: TaskEdit,
        authorized: TaskStatus,
    ) -> Result<Task> {
        let mut local = self.local()?;
        TaskStore::edit_task(&mut **local, task_id, edit, authorized)
    }
    fn set_priority(&mut self, task_id: TaskId, priority: Option<Priority>) -> Result<Task> {
        let mut local = self.local()?;
        TaskStore::set_priority(&mut **local, task_id, priority)
    }
    fn submit(&mut self, submission: Submission) -> Result<Proposal> {
        let mut local = self.local()?;
        TaskStore::submit(&mut **local, submission)
    }
    fn approve_proposal(&mut self, proposal_id: ProposalId) -> Result<Proposal> {
        let mut local = self.local()?;
        TaskStore::approve_proposal(&mut **local, proposal_id)
    }
    fn send_back_proposal(&mut self, proposal_id: ProposalId) -> Result<Proposal> {
        let mut local = self.local()?;
        TaskStore::send_back_proposal(&mut **local, proposal_id)
    }
    fn withdraw_proposal(&mut self, proposal_id: ProposalId) -> Result<Proposal> {
        let mut local = self.local()?;
        TaskStore::withdraw_proposal(&mut **local, proposal_id)
    }
    fn ready_goal(&mut self, goal_id: GoalId) -> Result<Goal> {
        let mut local = self.local()?;
        TaskStore::ready_goal(&mut **local, goal_id)
    }
    fn add_note(&mut self, note: NewNote) -> Result<RunEvent> {
        let mut local = self.local()?;
        TaskStore::add_note(&mut **local, note)
    }
}

impl PlanningRecords for SupervisorQueue {
    fn related_landed_commits(&self, task: TaskId, limit: usize) -> Result<Vec<String>> {
        let local = self.local()?;
        PlanningRecords::related_landed_commits(&**local, task, limit)
    }
    fn related_tasks(
        &self,
        task: TaskId,
        statuses: &[String],
        limit: usize,
    ) -> Result<RelatedPage> {
        let local = self.local()?;
        PlanningRecords::related_tasks(&**local, task, statuses, limit)
    }
    fn search_documents(&self, query: &SearchQuery) -> Result<SearchPage> {
        let local = self.local()?;
        PlanningRecords::search_documents(&**local, query)
    }
    fn task_goals(&self) -> Result<HashMap<TaskId, Option<GoalId>>> {
        let local = self.local()?;
        PlanningRecords::task_goals(&**local)
    }
    fn task_titles(&self) -> Result<HashMap<TaskId, String>> {
        let local = self.local()?;
        PlanningRecords::task_titles(&**local)
    }
    fn task_changes(&self) -> Result<HashMap<TaskId, Option<TaskChange>>> {
        let local = self.local()?;
        PlanningRecords::task_changes(&**local)
    }
    fn draft_origins(&self) -> Result<HashMap<TaskId, DraftOrigin>> {
        let local = self.local()?;
        PlanningRecords::draft_origins(&**local)
    }
}

impl PlanRequestStore for SupervisorQueue {
    fn planner_requests(&self) -> Result<Vec<crate::domain::plan_request::PlanRequest>> {
        let local = self.local()?;
        PlanRequestStore::planner_requests(&**local)
    }
    fn open_request_planner(
        &mut self,
        request: crate::domain::RequestId,
        answer: Option<AskId>,
    ) -> Result<RequestPlannerStart> {
        let mut local = self.local()?;
        PlanRequestStore::open_request_planner(&mut **local, request, answer)
    }
    fn plan_request(
        &self,
        request: crate::domain::RequestId,
    ) -> Result<crate::domain::plan_request::PlanRequest> {
        let local = self.local()?;
        PlanRequestStore::plan_request(&**local, request)
    }
    fn request_asks(&self, request: crate::domain::RequestId) -> Result<Vec<Ask>> {
        let local = self.local()?;
        PlanRequestStore::request_asks(&**local, request)
    }
    fn event_by_id(&self, id: EventId) -> Result<Option<RunEvent>> {
        let local = self.local()?;
        PlanRequestStore::event_by_id(&**local, id)
    }
    fn replan_brief(
        &self,
        id: crate::domain::ReplanId,
    ) -> Result<crate::domain::replan_plan::ReplanBrief> {
        let local = self.local()?;
        PlanRequestStore::replan_brief(&**local, id)
    }
    fn replacement_brief(
        &self,
        proposal: crate::domain::ProposalId,
    ) -> Result<Option<crate::domain::replan_plan::ReplanBrief>> {
        let local = self.local()?;
        PlanRequestStore::replacement_brief(&**local, proposal)
    }
    fn settle_replan_requests(
        &mut self,
    ) -> Result<Vec<crate::domain::task_replan::ReplanOperation>> {
        let mut local = self.local()?;
        PlanRequestStore::settle_replan_requests(&mut **local)
    }
}

impl DraftPlannerReads for SupervisorQueue {
    fn planner_findings(&self) -> Result<Vec<Finding>> {
        let local = self.local()?;
        DraftPlannerReads::planner_findings(&**local)
    }
    fn improvements(&self, limit: usize) -> Result<crate::domain::ImprovementLimit> {
        let local = self.local()?;
        DraftPlannerReads::improvements(&**local, limit)
    }
}

impl DraftPlannerStore for SupervisorQueue {
    fn register_follow_ups(
        &mut self,
        run: &RunId,
        entries: Vec<FollowUpRegistration>,
        depth: i64,
    ) -> Result<Vec<crate::domain::RegisteredFollowUp>> {
        let mut local = self.local()?;
        DraftPlannerStore::register_follow_ups(&mut **local, run, entries, depth)
    }
    fn record_draft_origin(
        &mut self,
        task: TaskId,
        origin: DraftOrigin,
        material: &serde_json::Value,
    ) -> Result<()> {
        let mut local = self.local()?;
        DraftPlannerStore::record_draft_origin(&mut **local, task, origin, material)
    }
    fn draft_origin(&self, task: TaskId) -> Result<Option<(DraftOrigin, serde_json::Value)>> {
        let local = self.local()?;
        DraftPlannerStore::draft_origin(&**local, task)
    }
    fn register_recurring_draft(
        &mut self,
        item: &crate::domain::recurring::RecurringDraft,
    ) -> Result<crate::domain::recurring::RecurringOutcome> {
        let mut local = self.local()?;
        DraftPlannerStore::register_recurring_draft(&mut **local, item)
    }
    fn planner_drafts(&self) -> Result<Vec<DraftTarget>> {
        let local = self.local()?;
        DraftPlannerStore::planner_drafts(&**local)
    }
    fn open_draft_planner(
        &mut self,
        drafts: &[TaskId],
        answer: Option<AskId>,
    ) -> Result<DraftPlannerStart> {
        let mut local = self.local()?;
        DraftPlannerStore::open_draft_planner(&mut **local, drafts, answer)
    }
    fn planner_draft_tasks(&self, planner: PlannerId) -> Result<Vec<TaskId>> {
        let local = self.local()?;
        DraftPlannerStore::planner_draft_tasks(&**local, planner)
    }
    fn draft_bundle(&self, planner: PlannerId) -> Result<Option<crate::domain::DraftBundleView>> {
        let local = self.local()?;
        DraftPlannerStore::draft_bundle(&**local, planner)
    }
    fn planner_answers(&self) -> Result<Vec<Ask>> {
        let local = self.local()?;
        DraftPlannerStore::planner_answers(&**local)
    }
    fn planner_answer_route(&self, ask: &Ask) -> Result<PlannerAnswerRoute> {
        let local = self.local()?;
        DraftPlannerStore::planner_answer_route(&**local, ask)
    }
    fn proposal_questions(&self, proposal: ProposalId) -> Result<Vec<Ask>> {
        let local = self.local()?;
        DraftPlannerStore::proposal_questions(&**local, proposal)
    }
    fn replaced_tasks(&self, proposal: ProposalId) -> Result<Vec<TaskId>> {
        let local = self.local()?;
        DraftPlannerStore::replaced_tasks(&**local, proposal)
    }
    fn answer_request(&self, ask: &Ask) -> Result<Option<crate::domain::RequestId>> {
        let local = self.local()?;
        DraftPlannerStore::answer_request(&**local, ask)
    }
    fn claim_planner_answer(
        &mut self,
        ask: AskId,
        planner: PlannerId,
        workspace: &str,
    ) -> Result<bool> {
        let mut local = self.local()?;
        DraftPlannerStore::claim_planner_answer(&mut **local, ask, planner, workspace)
    }
    fn close_planner_answer(&mut self, ask: AskId, why: &str) -> Result<()> {
        let mut local = self.local()?;
        DraftPlannerStore::close_planner_answer(&mut **local, ask, why)
    }
    fn record_task_event(
        &mut self,
        task: TaskId,
        kind: EventKind,
        payload: serde_json::Value,
    ) -> Result<()> {
        let mut local = self.local()?;
        DraftPlannerStore::record_task_event(&mut **local, task, kind, payload)
    }
    fn exhausted_drafts(&self) -> Result<Vec<Task>> {
        let local = self.local()?;
        DraftPlannerStore::exhausted_drafts(&**local)
    }
    fn follow_up_depth(&self, task: TaskId) -> Result<i64> {
        let local = self.local()?;
        DraftPlannerStore::follow_up_depth(&**local, task)
    }
    fn set_follow_up_depth(&mut self, task: TaskId, depth: i64) -> Result<()> {
        let mut local = self.local()?;
        DraftPlannerStore::set_follow_up_depth(&mut **local, task, depth)
    }
    fn open_finding_planner(
        &mut self,
        finding: FindingId,
        answer: Option<AskId>,
        limit: usize,
    ) -> Result<FindingPlannerStart> {
        let mut local = self.local()?;
        DraftPlannerStore::open_finding_planner(&mut **local, finding, answer, limit)
    }
    fn finding_view(&self, finding: FindingId) -> Result<FindingView> {
        let local = self.local()?;
        DraftPlannerStore::finding_view(&**local, finding)
    }
    fn finding_asks(&self, finding: FindingId) -> Result<Vec<Ask>> {
        let local = self.local()?;
        DraftPlannerStore::finding_asks(&**local, finding)
    }
    fn settle_findings(&mut self) -> Result<Vec<(FindingId, FindingStatus)>> {
        let mut local = self.local()?;
        DraftPlannerStore::settle_findings(&mut **local)
    }
    fn exhausted_findings(&self) -> Result<Vec<Finding>> {
        let local = self.local()?;
        DraftPlannerStore::exhausted_findings(&**local)
    }
    fn record_finding_event(
        &mut self,
        finding: FindingId,
        kind: EventKind,
        payload: serde_json::Value,
    ) -> Result<()> {
        let mut local = self.local()?;
        DraftPlannerStore::record_finding_event(&mut **local, finding, kind, payload)
    }
    fn ask_delivered_to(&self, ask: AskId, workspace: &str) -> Result<bool> {
        let local = self.local()?;
        DraftPlannerStore::ask_delivered_to(&**local, ask, workspace)
    }
    fn answer_claimed_at(&self, ask: AskId, workspace: &str) -> Result<Option<i64>> {
        let local = self.local()?;
        DraftPlannerStore::answer_claimed_at(&**local, ask, workspace)
    }
    fn ask_delivery_failed(&self, ask: AskId, planner: PlannerId) -> Result<bool> {
        let local = self.local()?;
        DraftPlannerStore::ask_delivery_failed(&**local, ask, planner)
    }
}

impl PlanReviewStore for SupervisorQueue {
    fn interrupt_plan_reviews_for_handoff(&mut self, token: &LeaseToken) -> Result<()> {
        let mut local = self.local()?;
        PlanReviewStore::interrupt_plan_reviews_for_handoff(&mut **local, token)
    }
    fn plan_review_candidates(&self) -> Result<Vec<PlanReviewCandidate>> {
        let local = self.local()?;
        PlanReviewStore::plan_review_candidates(&**local)
    }
    #[allow(clippy::too_many_arguments)]
    fn begin_plan_review(
        &mut self,
        proposal: ProposalId,
        token: &LeaseToken,
        plan_reviews_dir: &Path,
        cwd: &Path,
        launch: &crate::domain::actor_model::ActorLaunch,
        supervisor_alive: &dyn Fn(u32) -> bool,
    ) -> Result<Option<PlanReviewJob>> {
        let mut local = self.local()?;
        PlanReviewStore::begin_plan_review(
            &mut **local,
            proposal,
            token,
            plan_reviews_dir,
            cwd,
            launch,
            supervisor_alive,
        )
    }
    fn finish_plan_review(
        &mut self,
        job: &PlanReviewJob,
        token: &LeaseToken,
        apply: &PlanReviewApply,
    ) -> Result<PlanReviewApplied> {
        let mut local = self.local()?;
        PlanReviewStore::finish_plan_review(&mut **local, job, token, apply)
    }
    fn fail_plan_review(
        &mut self,
        job: &PlanReviewJob,
        token: &LeaseToken,
        failure: &PlanReviewFailure,
    ) -> Result<()> {
        let mut local = self.local()?;
        PlanReviewStore::fail_plan_review(&mut **local, job, token, failure)
    }
    fn plan_reviews_finished_before(&self, before: i64) -> Result<Vec<i64>> {
        let local = self.local()?;
        PlanReviewStore::plan_reviews_finished_before(&**local, before)
    }
    fn revising_proposals(&self) -> Result<Vec<RevisingProposal>> {
        let local = self.local()?;
        PlanReviewStore::revising_proposals(&**local)
    }
    fn claim_revise(&mut self, proposal: ProposalId) -> Result<bool> {
        let mut local = self.local()?;
        PlanReviewStore::claim_revise(&mut **local, proposal)
    }
    fn revise_goal_wait(&self, proposal: ProposalId) -> Result<ReviseGoalWait> {
        let local = self.local()?;
        PlanReviewStore::revise_goal_wait(&**local, proposal)
    }
    #[allow(clippy::too_many_arguments)]
    fn revise_sent(
        &mut self,
        proposal: ProposalId,
        planner: PlannerId,
        workspace: &str,
        opened: Option<&crate::domain::actor_model::ActorLaunch>,
        answers: &[AskId],
    ) -> Result<()> {
        let mut local = self.local()?;
        PlanReviewStore::revise_sent(&mut **local, proposal, planner, workspace, opened, answers)
    }
    fn revise_lost(
        &mut self,
        proposal: ProposalId,
        planner: Option<PlannerId>,
        why: &str,
    ) -> Result<()> {
        let mut local = self.local()?;
        PlanReviewStore::revise_lost(&mut **local, proposal, planner, why)
    }
    fn planner_unresponsive(
        &mut self,
        proposal: ProposalId,
        planner: Option<PlannerId>,
        waited_secs: i64,
        holders: &[PlannerHold],
    ) -> Result<()> {
        let mut local = self.local()?;
        PlanReviewStore::planner_unresponsive(&mut **local, proposal, planner, waited_secs, holders)
    }
    fn settle_proposals(&mut self) -> Result<Vec<(ProposalId, crate::domain::ProposalStatus)>> {
        let mut local = self.local()?;
        PlanReviewStore::settle_proposals(&mut **local)
    }
    fn plan_answers(&self) -> Result<Vec<Ask>> {
        let local = self.local()?;
        PlanReviewStore::plan_answers(&**local)
    }
    fn decide_plan(&mut self, ask: AskId) -> Result<Option<PlanDecided>> {
        let mut local = self.local()?;
        PlanReviewStore::decide_plan(&mut **local, ask)
    }
    fn applies_plan_answer(&self, ask: &Ask) -> Result<bool> {
        let local = self.local()?;
        PlanReviewStore::applies_plan_answer(&**local, ask)
    }
    fn plan_review_holds(&self) -> Result<Vec<PlanReviewHold>> {
        let local = self.local()?;
        PlanReviewStore::plan_review_holds(&**local)
    }
    fn stranded_dependencies(&self) -> Result<Vec<StrandedDependency>> {
        let local = self.local()?;
        PlanReviewStore::stranded_dependencies(&**local)
    }
    fn answered_asks(&self, limit: usize) -> Result<Vec<Ask>> {
        let local = self.local()?;
        PlanReviewStore::answered_asks(&**local, limit)
    }
}

impl GoalReviewStore for SupervisorQueue {
    fn interrupt_goal_reviews_for_handoff(&mut self, token: &LeaseToken) -> Result<()> {
        let mut local = self.local()?;
        GoalReviewStore::interrupt_goal_reviews_for_handoff(&mut **local, token)
    }
    fn goal_review_candidates(&self) -> Result<Vec<GoalId>> {
        let local = self.local()?;
        GoalReviewStore::goal_review_candidates(&**local)
    }
    #[allow(clippy::too_many_arguments)]
    fn begin_goal_review(
        &mut self,
        goal: GoalId,
        token: &LeaseToken,
        goal_reviews_dir: &Path,
        cwd: &Path,
        launch: &crate::domain::actor_model::ActorLaunch,
        supervisor_alive: &dyn Fn(u32) -> bool,
    ) -> Result<Option<GoalReviewJob>> {
        let mut local = self.local()?;
        GoalReviewStore::begin_goal_review(
            &mut **local,
            goal,
            token,
            goal_reviews_dir,
            cwd,
            launch,
            supervisor_alive,
        )
    }
    fn goal_reviews(&self, goal: GoalId) -> Result<Vec<GoalReviewRecord>> {
        let local = self.local()?;
        GoalReviewStore::goal_reviews(&**local, goal)
    }
    fn finish_goal_review(
        &mut self,
        job: &GoalReviewJob,
        token: &LeaseToken,
        apply: &GoalReviewApply,
    ) -> Result<GoalReviewApplied> {
        let mut local = self.local()?;
        GoalReviewStore::finish_goal_review(&mut **local, job, token, apply)
    }
    fn fail_goal_review(
        &mut self,
        job: &GoalReviewJob,
        token: &LeaseToken,
        failure: &GoalReviewFailure,
    ) -> Result<()> {
        let mut local = self.local()?;
        GoalReviewStore::fail_goal_review(&mut **local, job, token, failure)
    }
    fn goal_answers(&self) -> Result<Vec<Ask>> {
        let local = self.local()?;
        GoalReviewStore::goal_answers(&**local)
    }
    fn decide_goal(&mut self, ask: AskId) -> Result<Option<GoalDecided>> {
        let mut local = self.local()?;
        GoalReviewStore::decide_goal(&mut **local, ask)
    }
    fn correction_answers(&self) -> Result<Vec<Ask>> {
        let local = self.local()?;
        GoalReviewStore::correction_answers(&**local)
    }
    fn decide_correction(&mut self, ask: AskId) -> Result<Option<serde_json::Value>> {
        let mut local = self.local()?;
        GoalReviewStore::decide_correction(&mut **local, ask)
    }
    fn goal_review_holds(&self) -> Result<Vec<GoalReviewHold>> {
        let local = self.local()?;
        GoalReviewStore::goal_review_holds(&**local)
    }
    fn goal_follow_ups(&self) -> Result<Vec<crate::domain::follow_up::GoalFollowUps>> {
        let local = self.local()?;
        GoalReviewStore::goal_follow_ups(&**local)
    }
    fn rearm_goal_review(&mut self, goal: GoalId) -> Result<serde_json::Value> {
        let mut local = self.local()?;
        GoalReviewStore::rearm_goal_review(&mut **local, goal)
    }
}

impl RunReads for SupervisorQueue {
    fn active_runs(&self) -> Result<Vec<TaskRun>> {
        self.remote("active_runs", vec![])
    }
    fn all_runs(&self) -> Result<Vec<TaskRun>> {
        self.remote("all_runs", vec![])
    }
    fn run(&self, id: &RunId) -> Result<TaskRun> {
        self.remote("run", vec![to_value(id)?])
    }
    fn runs_with_status(&self, status: RunStatus) -> Result<Vec<TaskRun>> {
        self.remote("runs_with_status", vec![to_value(status)?])
    }
    fn next_awaiting_integration(&self) -> Result<Option<TaskRun>> {
        self.remote("next_awaiting_integration", vec![])
    }
    fn ended_run_workspaces(&self) -> Result<Vec<EndedRunWorkspace>> {
        self.remote("ended_run_workspaces", vec![])
    }
    fn ended_run_worktrees(&self) -> Result<Vec<EndedRunWorktree>> {
        self.remote("ended_run_worktrees", vec![])
    }
    fn ended_run_worktree(&self, id: &RunId) -> Result<Option<EndedRunWorktree>> {
        self.remote("ended_run_worktree", vec![to_value(id)?])
    }
    fn latest_runs_in_progress(&self) -> Result<Vec<TaskRun>> {
        self.remote("latest_runs_in_progress", vec![])
    }
    fn runs_with_pending_push(&self) -> Result<Vec<TaskRun>> {
        self.remote("runs_with_pending_push", vec![])
    }
    fn run_in_workspace(&self, workspace_id: &str) -> Result<Option<RunId>> {
        self.remote("run_in_workspace", vec![to_value(workspace_id)?])
    }
}

impl RunTransitions for SupervisorQueue {
    fn claim_for_supervisor(
        &mut self,
        base_commit: &CommitSha,
        token: &LeaseToken,
    ) -> Result<ClaimOutcome> {
        self.remote(
            "claim_for_supervisor",
            vec![to_value(base_commit)?, to_value(token)?],
        )
    }
    fn abandon_run(
        &mut self,
        id: &RunId,
        token: &LeaseToken,
        message: &str,
        reason: &Reason,
        session: Option<&serde_json::Value>,
    ) -> Result<TaskRun> {
        self.remote(
            "abandon_run",
            vec![
                to_value(id)?,
                to_value(token)?,
                to_value(message)?,
                to_value(reason)?,
                to_value(session)?,
            ],
        )
    }
    fn begin_integration(
        &mut self,
        id: &RunId,
        token: &LeaseToken,
        main: &CommitSha,
    ) -> Result<TaskRun> {
        let mut local = self.local()?;
        RunTransitions::begin_integration(&mut **local, id, token, main)
    }
    fn defer_integration(
        &mut self,
        id: &RunId,
        token: &LeaseToken,
        reason: &str,
        detail: serde_json::Value,
    ) -> Result<TaskRun> {
        let mut local = self.local()?;
        RunTransitions::defer_integration(&mut **local, id, token, reason, detail)
    }
    fn hold_integration(
        &mut self,
        id: &RunId,
        token: &LeaseToken,
        reason: &str,
        detail: serde_json::Value,
    ) -> Result<TaskRun> {
        let mut local = self.local()?;
        RunTransitions::hold_integration(&mut **local, id, token, reason, detail)
    }
    fn fail_integration(
        &mut self,
        id: &RunId,
        token: &LeaseToken,
        reason: &str,
        receipt: serde_json::Value,
    ) -> Result<TaskRun> {
        let mut local = self.local()?;
        RunTransitions::fail_integration(&mut **local, id, token, reason, receipt)
    }
    fn abort_integration(
        &mut self,
        id: &RunId,
        token: &LeaseToken,
        revert_to: &str,
        message: &str,
        reason: &Reason,
    ) -> Result<TaskRun> {
        let mut local = self.local()?;
        RunTransitions::abort_integration(&mut **local, id, token, revert_to, message, reason)
    }
    fn finish_integration(
        &mut self,
        id: &RunId,
        token: &LeaseToken,
        landing: &Landing,
        common_dir: &str,
    ) -> Result<(Task, TaskRun)> {
        let mut local = self.local()?;
        RunTransitions::finish_integration(&mut **local, id, token, landing, common_dir)
    }
    fn record_cleanup_failure(&mut self, id: &RunId, message: &str, reason: &Reason) -> Result<()> {
        self.remote(
            "record_cleanup_failure",
            vec![to_value(id)?, to_value(message)?, to_value(reason)?],
        )
    }
    fn replan_guard(
        &self,
        task: crate::domain::TaskId,
    ) -> Result<Option<crate::domain::task_replan::ReplanGuard>> {
        self.remote("replan_guard", vec![to_value(task)?])
    }
    fn turn_fence(
        &self,
        task: crate::domain::TaskId,
    ) -> Result<Option<crate::domain::task_replan::Fence>> {
        self.remote("turn_fence", vec![to_value(task)?])
    }
    fn settling_holds(&self) -> Result<Vec<crate::domain::task_hold::TaskHold>> {
        self.remote("settling_holds", vec![])
    }
    fn remind_task_holds(&mut self) -> Result<usize> {
        self.remote("remind_task_holds", vec![])
    }
    fn open_hold(
        &self,
        task: crate::domain::TaskId,
    ) -> Result<Option<crate::domain::task_hold::TaskHold>> {
        self.remote("open_hold", vec![to_value(task)?])
    }
    fn latest_run_of(&self, task: crate::domain::TaskId) -> Result<Option<TaskRun>> {
        self.remote("latest_run_of", vec![to_value(task)?])
    }
    #[allow(clippy::too_many_arguments)]
    fn park_for_hold(
        &mut self,
        hold: crate::domain::HoldId,
        id: &RunId,
        token: &LeaseToken,
        reason: &str,
        payload: serde_json::Value,
    ) -> Result<Option<TaskRun>> {
        self.remote(
            "park_for_hold",
            vec![
                to_value(hold)?,
                to_value(id)?,
                to_value(token)?,
                to_value(reason)?,
                to_value(payload)?,
            ],
        )
    }
    fn settle_hold_at_rest(
        &mut self,
        hold: crate::domain::HoldId,
        id: &RunId,
        status: RunStatus,
    ) -> Result<bool> {
        self.remote(
            "settle_hold_at_rest",
            vec![to_value(hold)?, to_value(id)?, to_value(status)?],
        )
    }
    fn release_for_hold(
        &mut self,
        hold: crate::domain::HoldId,
        id: &RunId,
        token: &LeaseToken,
    ) -> Result<bool> {
        self.remote(
            "release_for_hold",
            vec![to_value(hold)?, to_value(id)?, to_value(token)?],
        )
    }
    fn parking_replans(&self) -> Result<Vec<crate::domain::task_replan::ReplanOperation>> {
        self.remote("parking_replans", vec![])
    }
    fn replans_of_run(
        &self,
        run: &RunId,
    ) -> Result<Vec<crate::domain::task_replan::ReplanOperation>> {
        self.remote("replans_of_run", vec![to_value(run)?])
    }
    #[allow(clippy::too_many_arguments)]
    fn move_replan(
        &mut self,
        id: crate::domain::ReplanId,
        generation: i64,
        step: &crate::domain::task_replan::ReplanMove,
    ) -> Result<crate::domain::task_replan::ReplanOperation> {
        self.remote(
            "move_replan",
            vec![to_value(id)?, to_value(generation)?, to_value(step)?],
        )
    }
    #[allow(clippy::too_many_arguments)]
    fn settle_replan(
        &mut self,
        op: crate::domain::ReplanId,
        generation: i64,
        step: &crate::domain::task_replan::ReplanMove,
        token: Option<&LeaseToken>,
        payload: serde_json::Value,
    ) -> Result<(crate::domain::task_replan::ReplanOperation, TaskRun)> {
        self.remote(
            "settle_replan",
            vec![
                to_value(op)?,
                to_value(generation)?,
                to_value(step)?,
                to_value(token)?,
                to_value(payload)?,
            ],
        )
    }
    fn rest_fenced(
        &mut self,
        id: &RunId,
        token: &LeaseToken,
        reason: &str,
        payload: serde_json::Value,
    ) -> Result<Option<TaskRun>> {
        self.remote(
            "rest_fenced",
            vec![
                to_value(id)?,
                to_value(token)?,
                to_value(reason)?,
                to_value(payload)?,
            ],
        )
    }
    fn lease_for_park(
        &mut self,
        id: &RunId,
        token: &LeaseToken,
        op: crate::domain::ReplanId,
        generation: i64,
    ) -> Result<Option<TaskRun>> {
        self.remote(
            "lease_for_park",
            vec![
                to_value(id)?,
                to_value(token)?,
                to_value(op)?,
                to_value(generation)?,
            ],
        )
    }
    fn lease_for_hold_park(
        &mut self,
        id: &RunId,
        token: &LeaseToken,
        hold: crate::domain::HoldId,
    ) -> Result<Option<TaskRun>> {
        self.remote(
            "lease_for_hold_park",
            vec![to_value(id)?, to_value(token)?, to_value(hold)?],
        )
    }
    #[allow(clippy::too_many_arguments)]
    fn begin_restore(
        &mut self,
        id: &RunId,
        token: &LeaseToken,
        op: crate::domain::ReplanId,
        generation: i64,
    ) -> Result<crate::domain::replan_snapshot::RestoreStart> {
        self.remote(
            "begin_restore",
            vec![
                to_value(id)?,
                to_value(token)?,
                to_value(op)?,
                to_value(generation)?,
            ],
        )
    }
    #[allow(clippy::too_many_arguments)]
    fn finish_restore(
        &mut self,
        id: &RunId,
        token: &LeaseToken,
        op: crate::domain::ReplanId,
        generation: i64,
        restored: std::result::Result<serde_json::Value, String>,
    ) -> Result<bool> {
        self.remote(
            "finish_restore",
            vec![
                to_value(id)?,
                to_value(token)?,
                to_value(op)?,
                to_value(generation)?,
                to_value(restored)?,
            ],
        )
    }
    #[allow(clippy::too_many_arguments)]
    fn record_clone_restore(
        &mut self,
        id: &RunId,
        token: &LeaseToken,
        op: crate::domain::ReplanId,
        generation: i64,
        restored: std::result::Result<serde_json::Value, String>,
    ) -> Result<bool> {
        self.remote(
            "record_clone_restore",
            vec![
                to_value(id)?,
                to_value(token)?,
                to_value(op)?,
                to_value(generation)?,
                to_value(restored)?,
            ],
        )
    }
    fn workspace_closed(&mut self, id: &RunId, token: &LeaseToken) -> Result<TaskRun> {
        self.remote("workspace_closed", vec![to_value(id)?, to_value(token)?])
    }
    fn cleanup_failed(
        &mut self,
        id: &RunId,
        token: &LeaseToken,
        message: &str,
        reason: &Reason,
    ) -> Result<TaskRun> {
        self.remote(
            "cleanup_failed",
            vec![
                to_value(id)?,
                to_value(token)?,
                to_value(message)?,
                to_value(reason)?,
            ],
        )
    }
    #[allow(clippy::too_many_arguments)]
    fn claim_for_supervisor_holding(
        &mut self,
        base_commit: &CommitSha,
        token: &LeaseToken,
        order: &[TaskId],
        held: &crate::domain::claim_order::ClaimHolds,
        attributes: Option<&serde_json::Value>,
        trial: &crate::domain::worker_model::WorkerTrial,
        routes: &[crate::domain::provider_switch::WorkerRoute],
    ) -> Result<ClaimOutcome> {
        self.remote(
            "claim_for_supervisor_holding",
            vec![
                to_value(base_commit)?,
                to_value(token)?,
                to_value(order)?,
                to_value(held)?,
                to_value(attributes)?,
                to_value(trial)?,
                to_value(routes)?,
            ],
        )
    }
    #[allow(clippy::too_many_arguments)]
    fn switch_provider(
        &mut self,
        id: &RunId,
        token: &LeaseToken,
        worker: crate::domain::worker::Worker,
        payload: serde_json::Value,
    ) -> Result<TaskRun> {
        self.remote(
            "switch_provider",
            vec![
                to_value(id)?,
                to_value(token)?,
                to_value(worker)?,
                to_value(payload)?,
            ],
        )
    }
    fn record_runtime_error(&mut self, id: &RunId, message: &str, reason: &Reason) -> Result<()> {
        self.remote(
            "record_runtime_error",
            vec![to_value(id)?, to_value(message)?, to_value(reason)?],
        )
    }
    fn plan_run(&mut self, id: &RunId, token: &LeaseToken, plan: &RunPlan) -> Result<()> {
        self.remote(
            "plan_run",
            vec![to_value(id)?, to_value(token)?, to_value(plan)?],
        )
    }
    fn workspace_created(&mut self, id: &RunId, token: &LeaseToken, workspace: &str) -> Result<()> {
        self.remote(
            "workspace_created",
            vec![to_value(id)?, to_value(token)?, to_value(workspace)?],
        )
    }
    fn finish_supervision(
        &mut self,
        id: &RunId,
        token: &LeaseToken,
        receipt: bool,
    ) -> Result<TaskRun> {
        self.remote(
            "finish_supervision",
            vec![to_value(id)?, to_value(token)?, to_value(receipt)?],
        )
    }
    fn finish_lost_session(
        &mut self,
        id: &RunId,
        token: &LeaseToken,
        exit_code: i32,
        receipt: bool,
    ) -> Result<TaskRun> {
        self.remote(
            "finish_lost_session",
            vec![
                to_value(id)?,
                to_value(token)?,
                to_value(exit_code)?,
                to_value(receipt)?,
            ],
        )
    }
    fn finish_supervision_live(&mut self, id: &RunId, token: &LeaseToken) -> Result<TaskRun> {
        self.remote(
            "finish_supervision_live",
            vec![to_value(id)?, to_value(token)?],
        )
    }
    fn finish_validation(
        &mut self,
        id: &RunId,
        token: &LeaseToken,
        validation: &Validation,
    ) -> Result<TaskRun> {
        self.remote(
            "finish_validation",
            vec![to_value(id)?, to_value(token)?, to_value(validation)?],
        )
    }
    fn restart_validation(&mut self, id: &RunId, token: &LeaseToken) -> Result<TaskRun> {
        self.remote("restart_validation", vec![to_value(id)?, to_value(token)?])
    }
    fn decide_landing(
        &mut self,
        id: &RunId,
        status: RunStatus,
        reason: &str,
        payload: serde_json::Value,
    ) -> Result<TaskRun> {
        let mut local = self.local()?;
        RunTransitions::decide_landing(&mut **local, id, status, reason, payload)
    }
    fn close_satisfied(
        &mut self,
        id: &RunId,
        token: Option<&LeaseToken>,
        payload: serde_json::Value,
    ) -> Result<(Task, TaskRun)> {
        let mut local = self.local()?;
        RunTransitions::close_satisfied(&mut **local, id, token, payload)
    }
    #[allow(clippy::too_many_arguments)]
    fn decide_satisfied(
        &mut self,
        id: &RunId,
        reason: &str,
        payload: serde_json::Value,
        task: &crate::domain::satisfied::TaskMove,
    ) -> Result<TaskRun> {
        let mut local = self.local()?;
        RunTransitions::decide_satisfied(&mut **local, id, reason, payload, task)
    }
    fn park_live(
        &mut self,
        id: &RunId,
        token: &LeaseToken,
        reason: &str,
        payload: serde_json::Value,
    ) -> Result<TaskRun> {
        self.remote(
            "park_live",
            vec![
                to_value(id)?,
                to_value(token)?,
                to_value(reason)?,
                to_value(payload)?,
            ],
        )
    }
    fn park_e2e_failed(
        &mut self,
        id: &RunId,
        token: &LeaseToken,
        reason: &str,
        payload: serde_json::Value,
    ) -> Result<TaskRun> {
        self.remote(
            "park_e2e_failed",
            vec![
                to_value(id)?,
                to_value(token)?,
                to_value(reason)?,
                to_value(payload)?,
            ],
        )
    }
    fn park_rechecked(
        &mut self,
        id: &RunId,
        token: Option<&LeaseToken>,
        reason: &str,
        payload: serde_json::Value,
    ) -> Result<Option<TaskRun>> {
        self.remote(
            "park_rechecked",
            vec![
                to_value(id)?,
                to_value(token)?,
                to_value(reason)?,
                to_value(payload)?,
            ],
        )
    }
    fn record_workspace_closed(
        &mut self,
        id: &RunId,
        workspace_id: &str,
        payload: serde_json::Value,
    ) -> Result<()> {
        self.remote(
            "record_workspace_closed",
            vec![to_value(id)?, to_value(workspace_id)?, to_value(payload)?],
        )
    }
}

impl RunRecovery for SupervisorQueue {
    fn runs_leased_by_others(&self, token: &LeaseToken) -> Result<Vec<LeasedRun>> {
        self.remote("runs_leased_by_others", vec![to_value(token)?])
    }
    fn runs_leased_by(&self, token: &LeaseToken) -> Result<Vec<TaskRun>> {
        self.remote("runs_leased_by", vec![to_value(token)?])
    }
    fn unclosed_run_asks(&self, run_id: &RunId) -> Result<Vec<crate::domain::Ask>> {
        self.remote("unclosed_run_asks", vec![to_value(run_id)?])
    }
    fn adopt_run(
        &mut self,
        id: &RunId,
        previous_token: &LeaseToken,
        token: &LeaseToken,
        pid: u32,
        wrapper: serde_json::Value,
    ) -> Result<Option<TaskRun>> {
        self.remote(
            "adopt_run",
            vec![
                to_value(id)?,
                to_value(previous_token)?,
                to_value(token)?,
                to_value(pid)?,
                to_value(wrapper)?,
            ],
        )
    }
    fn lease_for_review(&mut self, id: &RunId, token: &LeaseToken) -> Result<Option<TaskRun>> {
        self.remote("lease_for_review", vec![to_value(id)?, to_value(token)?])
    }
    fn lease_for_e2e(&mut self, id: &RunId, token: &LeaseToken) -> Result<Option<TaskRun>> {
        self.remote("lease_for_e2e", vec![to_value(id)?, to_value(token)?])
    }
    fn lease_stranded_landing(
        &mut self,
        id: &RunId,
        token: &LeaseToken,
    ) -> Result<Option<TaskRun>> {
        self.remote(
            "lease_stranded_landing",
            vec![to_value(id)?, to_value(token)?],
        )
    }
    fn lease_satisfied(&mut self, id: &RunId, token: &LeaseToken) -> Result<Option<TaskRun>> {
        self.remote("lease_satisfied", vec![to_value(id)?, to_value(token)?])
    }
    fn recover_run(
        &mut self,
        id: &RunId,
        checked_processes: usize,
        report: serde_json::Value,
    ) -> Result<TaskRun> {
        self.remote(
            "recover_run",
            vec![
                to_value(id)?,
                to_value(checked_processes)?,
                to_value(report)?,
            ],
        )
    }
    fn runs_to_triage(&self) -> Result<Vec<TaskRun>> {
        let local = self.local()?;
        RunRecovery::runs_to_triage(&**local)
    }
    #[allow(clippy::too_many_arguments)]
    fn begin_triage(
        &mut self,
        id: &RunId,
        token: &LeaseToken,
        request: Option<serde_json::Value>,
        launch: &crate::domain::actor_model::ActorLaunch,
    ) -> Result<Option<(TaskRun, usize)>> {
        let mut local = self.local()?;
        RunRecovery::begin_triage(&mut **local, id, token, request, launch)
    }
    #[allow(clippy::too_many_arguments)]
    fn finish_triage(
        &mut self,
        id: &RunId,
        token: &LeaseToken,
        action: &TriageAction,
        payload: serde_json::Value,
        also: Vec<(EventKind, serde_json::Value)>,
    ) -> Result<TaskRun> {
        let mut local = self.local()?;
        RunRecovery::finish_triage(&mut **local, id, token, action, payload, also)
    }
    fn decide_triage(
        &mut self,
        id: &RunId,
        ask_id: AskId,
        answer: &str,
        reason: &str,
    ) -> Result<TaskRun> {
        let mut local = self.local()?;
        RunRecovery::decide_triage(&mut **local, id, ask_id, answer, reason)
    }
    fn runs_needing_session(&self) -> Result<Vec<ResumeCandidate>> {
        self.remote("runs_needing_session", vec![])
    }
    #[allow(clippy::too_many_arguments)]
    fn begin_resume(
        &mut self,
        id: &RunId,
        token: &LeaseToken,
        main: &CommitSha,
        reason: Option<&str>,
        config: crate::domain::resume::ResumeConfig,
    ) -> Result<Option<(TaskRun, usize)>> {
        self.remote(
            "begin_resume",
            vec![
                to_value(id)?,
                to_value(token)?,
                to_value(main)?,
                to_value(reason)?,
                to_value(config)?,
            ],
        )
    }
    #[allow(clippy::too_many_arguments)]
    fn finish_resume(
        &mut self,
        id: &RunId,
        token: &LeaseToken,
        status: Option<RunStatus>,
        reason: Option<&str>,
        keep_lease: bool,
        payload: serde_json::Value,
    ) -> Result<TaskRun> {
        self.remote(
            "finish_resume",
            vec![
                to_value(id)?,
                to_value(token)?,
                to_value(status)?,
                to_value(reason)?,
                to_value(keep_lease)?,
                to_value(payload)?,
            ],
        )
    }
    fn finish_approved_resume(
        &mut self,
        id: &RunId,
        token: &LeaseToken,
        head: &CommitSha,
        payload: serde_json::Value,
    ) -> Result<TaskRun> {
        self.remote(
            "finish_approved_resume",
            vec![
                to_value(id)?,
                to_value(token)?,
                to_value(head)?,
                to_value(payload)?,
            ],
        )
    }
    fn skip_resume(
        &mut self,
        id: &RunId,
        token: &LeaseToken,
        head: &CommitSha,
        main: &CommitSha,
        approved: bool,
    ) -> Result<Option<TaskRun>> {
        self.remote(
            "skip_resume",
            vec![
                to_value(id)?,
                to_value(token)?,
                to_value(head)?,
                to_value(main)?,
                to_value(approved)?,
            ],
        )
    }
    #[allow(clippy::too_many_arguments)]
    fn exhaust_resumes(
        &mut self,
        id: &RunId,
        exhaustion: &Exhaustion,
        reason: &str,
        config: crate::domain::resume::ResumeConfig,
    ) -> Result<Option<TaskRun>> {
        self.remote(
            "exhaust_resumes",
            vec![
                to_value(id)?,
                to_value(exhaustion)?,
                to_value(reason)?,
                to_value(config)?,
            ],
        )
    }
    fn hold_guard(&self, task: TaskId) -> Result<Option<crate::domain::task_hold::HoldGuard>> {
        self.remote("hold_guard", vec![to_value(task)?])
    }
    fn held_claims(&self) -> Result<Vec<crate::domain::task_hold::HoldGuard>> {
        self.remote("held_claims", vec![])
    }
    #[allow(clippy::too_many_arguments)]
    fn note_hold_deferred(
        &mut self,
        hold: crate::domain::task_hold::HoldGuard,
        run: Option<&RunId>,
        step: crate::domain::task_replan::GuardedStep,
        supervisor: &LeaseToken,
    ) -> Result<bool> {
        self.remote(
            "note_hold_deferred",
            vec![
                to_value(hold)?,
                to_value(run)?,
                to_value(step)?,
                to_value(supervisor)?,
            ],
        )
    }
    fn keep_triage_answer(
        &mut self,
        id: &RunId,
        ask_id: AskId,
        answer: &str,
        reason: &str,
    ) -> Result<bool> {
        let mut local = self.local()?;
        RunRecovery::keep_triage_answer(&mut **local, id, ask_id, answer, reason)
    }
    fn keep_live_verdict(&mut self, id: &RunId, verdict: serde_json::Value) -> Result<bool> {
        let mut local = self.local()?;
        RunRecovery::keep_live_verdict(&mut **local, id, verdict)
    }
    fn end_holds_of_ended_tasks(&mut self) -> Result<Vec<crate::domain::TaskId>> {
        self.remote("end_holds_of_ended_tasks", vec![])
    }
    fn released_kept_results(&self) -> Result<Vec<EventId>> {
        let local = self.local()?;
        RunRecovery::released_kept_results(&**local)
    }
    fn settle_kept_result(&mut self, kept: EventId) -> Result<KeptOutcome> {
        let mut local = self.local()?;
        RunRecovery::settle_kept_result(&mut **local, kept)
    }
}

impl CoordinationReads for SupervisorQueue {
    fn holds_lease(&self, id: &RunId, token: &LeaseToken) -> Result<bool> {
        self.remote("holds_lease", vec![to_value(id)?, to_value(token)?])
    }
    fn run_leases(&self) -> Result<Vec<RunLease>> {
        self.remote("run_leases", vec![])
    }
    fn run_lease(&self, id: &RunId) -> Result<Option<RunLease>> {
        self.remote("run_lease", vec![to_value(id)?])
    }
    fn processes(&self, id: &RunId) -> Result<Vec<RunProcess>> {
        self.remote("processes", vec![to_value(id)?])
    }
    fn backend_slots(&self, token: Option<&LeaseToken>) -> Result<(i64, Option<i64>)> {
        self.remote("backend_slots", vec![to_value(token)?])
    }
}

impl RunCoordination for SupervisorQueue {
    fn heartbeat_leases(&self, token: &LeaseToken) -> Result<usize> {
        self.remote("heartbeat_leases", vec![to_value(token)?])
    }
    fn release_lease(&mut self, id: &RunId, token: &LeaseToken) -> Result<()> {
        self.remote("release_lease", vec![to_value(id)?, to_value(token)?])
    }
    fn heartbeat(&mut self, token: &LeaseToken) -> Result<HeartbeatWrite> {
        self.remote("heartbeat", vec![to_value(token)?])
    }
    fn register_wrapper(&mut self, id: &RunId, token: &LeaseToken, pid: u32) -> Result<()> {
        self.remote(
            "register_wrapper",
            vec![to_value(id)?, to_value(token)?, to_value(pid)?],
        )
    }
    fn register_resume_wrapper(&mut self, id: &RunId, token: &LeaseToken, pid: u32) -> Result<()> {
        self.remote(
            "register_resume_wrapper",
            vec![to_value(id)?, to_value(token)?, to_value(pid)?],
        )
    }
    fn clear_lost_session(
        &mut self,
        id: &RunId,
        token: &LeaseToken,
        lost: Option<u32>,
    ) -> Result<()> {
        self.remote(
            "clear_lost_session",
            vec![to_value(id)?, to_value(token)?, to_value(lost)?],
        )
    }
    fn session_reopened(
        &mut self,
        id: &RunId,
        token: &LeaseToken,
        workspace: &str,
        attempt: u64,
        repaired: serde_json::Value,
    ) -> Result<()> {
        self.remote(
            "session_reopened",
            vec![
                to_value(id)?,
                to_value(token)?,
                to_value(workspace)?,
                to_value(attempt)?,
                to_value(repaired)?,
            ],
        )
    }
    fn register_agent(&mut self, id: &RunId, wrapper_pid: u32, agent_pid: u32) -> Result<()> {
        self.remote(
            "register_agent",
            vec![to_value(id)?, to_value(wrapper_pid)?, to_value(agent_pid)?],
        )
    }
    fn register_resume_agent(
        &mut self,
        id: &RunId,
        wrapper_pid: u32,
        agent_pid: u32,
    ) -> Result<()> {
        self.remote(
            "register_resume_agent",
            vec![to_value(id)?, to_value(wrapper_pid)?, to_value(agent_pid)?],
        )
    }
    fn register_turn_agent(&mut self, id: &RunId, wrapper_pid: u32, agent_pid: u32) -> Result<()> {
        self.remote(
            "register_turn_agent",
            vec![to_value(id)?, to_value(wrapper_pid)?, to_value(agent_pid)?],
        )
    }
    fn heartbeat_wrapper(&self, id: &RunId, pid: u32) -> Result<()> {
        self.remote("heartbeat_wrapper", vec![to_value(id)?, to_value(pid)?])
    }
    fn wrapper_exited(&mut self, id: &RunId, pid: u32, exit_code: i32) -> Result<()> {
        self.remote(
            "wrapper_exited",
            vec![to_value(id)?, to_value(pid)?, to_value(exit_code)?],
        )
    }
}

impl SessionReads for SupervisorQueue {
    fn silent_planners(&self) -> Result<Vec<(PlannerSession, RunEvent)>> {
        let local = self.local()?;
        SessionReads::silent_planners(&**local)
    }
}

impl SessionRegistry for SupervisorQueue {
    fn remove_session_workspace(&self, role: SessionRole) -> Result<bool> {
        let local = self.local()?;
        SessionRegistry::remove_session_workspace(&**local, role)
    }
    fn forget_retired_session_workspaces(&self) -> Result<usize> {
        let local = self.local()?;
        SessionRegistry::forget_retired_session_workspaces(&**local)
    }
    fn open_planner(
        &self,
        origin: PlannerOrigin,
        proposal: Option<ProposalId>,
    ) -> Result<PlannerSession> {
        let local = self.local()?;
        SessionRegistry::open_planner(&**local, origin, proposal)
    }
    fn planner_workspace_created(&self, id: PlannerId, workspace_id: &str) -> Result<()> {
        let local = self.local()?;
        SessionRegistry::planner_workspace_created(&**local, id, workspace_id)
    }
    fn close_planner(&self, id: PlannerId, error: Option<&str>) -> Result<PlannerSession> {
        let local = self.local()?;
        SessionRegistry::close_planner(&**local, id, error)
    }
    fn end_planner(&self, id: PlannerId, payload: &serde_json::Value) -> Result<bool> {
        let local = self.local()?;
        SessionRegistry::end_planner(&**local, id, payload)
    }
    fn planner(&self, id: PlannerId) -> Result<PlannerSession> {
        let local = self.local()?;
        SessionRegistry::planner(&**local, id)
    }
    fn planners(&self, all: bool) -> Result<Vec<PlannerSession>> {
        let local = self.local()?;
        SessionRegistry::planners(&**local, all)
    }
    fn set_planner_route(&self, id: PlannerId, route: crate::domain::PlannerRoute) -> Result<()> {
        let local = self.local()?;
        SessionRegistry::set_planner_route(&**local, id, route)
    }
    fn planner_turn_events(&self, id: PlannerId) -> Result<Vec<RunEvent>> {
        let local = self.local()?;
        SessionRegistry::planner_turn_events(&**local, id)
    }
    fn planner_answer_wait(
        &self,
        id: PlannerId,
        asks: &[AskId],
        payload: &serde_json::Value,
    ) -> Result<bool> {
        let local = self.local()?;
        SessionRegistry::planner_answer_wait(&**local, id, asks, payload)
    }
    fn planner_answer_undelivered(
        &self,
        id: PlannerId,
        ask: AskId,
        payload: &serde_json::Value,
    ) -> Result<bool> {
        let local = self.local()?;
        SessionRegistry::planner_answer_undelivered(&**local, id, ask, payload)
    }
    fn planner_handover(&self, ask: AskId) -> Result<Option<crate::domain::PlannerHandover>> {
        let local = self.local()?;
        SessionRegistry::planner_handover(&**local, ask)
    }
    fn register_planner_wrapper(&self, id: PlannerId, pid: u32) -> Result<()> {
        let local = self.local()?;
        SessionRegistry::register_planner_wrapper(&**local, id, pid)
    }
    fn register_planner_agent(&self, id: PlannerId, wrapper_pid: u32, agent: u32) -> Result<()> {
        let local = self.local()?;
        SessionRegistry::register_planner_agent(&**local, id, wrapper_pid, agent)
    }
    fn heartbeat_planner(&self, id: PlannerId, wrapper_pid: u32) -> Result<()> {
        let local = self.local()?;
        SessionRegistry::heartbeat_planner(&**local, id, wrapper_pid)
    }
    fn planner_exited(&self, id: PlannerId, wrapper_pid: u32, exit_code: i32) -> Result<()> {
        let local = self.local()?;
        SessionRegistry::planner_exited(&**local, id, wrapper_pid, exit_code)
    }
    fn planner_silent(&self, id: PlannerId, payload: serde_json::Value) -> Result<bool> {
        let local = self.local()?;
        SessionRegistry::planner_silent(&**local, id, payload)
    }
    fn record_session_turns(&self) -> Result<usize> {
        let local = self.local()?;
        SessionRegistry::record_session_turns(&**local)
    }
    fn record_session_tokens(&self) -> Result<usize> {
        let local = self.local()?;
        SessionRegistry::record_session_tokens(&**local)
    }
    fn record_session_hook(
        &self,
        hook: &crate::domain::sessions::SessionHook,
    ) -> Result<serde_json::Value> {
        let local = self.local()?;
        SessionRegistry::record_session_hook(&**local, hook)
    }
    fn open_hook_session_spans(&self) -> Result<Vec<crate::domain::sessions::OpenSpan>> {
        let local = self.local()?;
        SessionRegistry::open_hook_session_spans(&**local)
    }
    fn close_inferred_sessions(&self, ended: &[EventId]) -> Result<usize> {
        let local = self.local()?;
        SessionRegistry::close_inferred_sessions(&**local, ended)
    }
    fn close_review_session(
        &self,
        id: &RunId,
        session: Option<&crate::domain::headless_job::JobSession>,
    ) -> Result<usize> {
        let local = self.local()?;
        SessionRegistry::close_review_session(&**local, id, session)
    }
}

impl SupervisorReads for SupervisorQueue {
    fn supervisors(&self) -> Result<Vec<SupervisorRegistration>> {
        self.remote("supervisors", vec![])
    }
    fn handoff_request(&self, token: &LeaseToken) -> Result<Option<String>> {
        self.remote("handoff_request", vec![to_value(token)?])
    }
    fn repository_binding(&self) -> Result<Option<String>> {
        self.remote("repository_binding", vec![])
    }
}

impl SupervisorRegistry for SupervisorQueue {
    #[allow(clippy::too_many_arguments)]
    fn register_supervisor(
        &mut self,
        token: &LeaseToken,
        pid: u32,
        limits: crate::domain::slot_limits::SlotLimits,
        providers: &[crate::domain::worker::ProviderCheck],
        binary_version: &str,
    ) -> Result<SupervisorRegistration> {
        self.remote(
            "register_supervisor",
            vec![
                to_value(token)?,
                to_value(pid)?,
                to_value(limits)?,
                to_value(providers)?,
                to_value(binary_version)?,
            ],
        )
    }
    fn prune_supervisor(
        &self,
        token: &LeaseToken,
        kind: EventKind,
        stopped: &dyn Fn(&SupervisorRegistration) -> serde_json::Value,
    ) -> Result<bool> {
        // The payload of the registration as read now: the service writes
        // it with the prune, in its transaction.
        let Some(registration) = SupervisorReads::supervisors(self)?
            .into_iter()
            .find(|registration| registration.token == *token)
        else {
            return Ok(false);
        };
        self.remote(
            "prune_supervisor",
            vec![to_value(token)?, to_value(kind)?, stopped(&registration)],
        )
    }
    fn accept_handoff(&self, token: &LeaseToken) -> Result<()> {
        self.remote("accept_handoff", vec![to_value(token)?])
    }
    fn request_handoff(&self, token: &LeaseToken, binary: &str) -> Result<bool> {
        self.remote("request_handoff", vec![to_value(token)?, to_value(binary)?])
    }
    fn take_handoff(&self, token: &LeaseToken, binary: &str) -> Result<bool> {
        self.remote("take_handoff", vec![to_value(token)?, to_value(binary)?])
    }
    fn cancel_handoff(&self, token: &LeaseToken, binary: &str) -> Result<bool> {
        self.remote("cancel_handoff", vec![to_value(token)?, to_value(binary)?])
    }
    #[allow(clippy::too_many_arguments)]
    fn resume_registration(
        &mut self,
        token: &LeaseToken,
        pid: u32,
        limits: crate::domain::slot_limits::SlotLimits,
        providers: &[crate::domain::worker::ProviderCheck],
        binary_version: &str,
    ) -> Result<SupervisorRegistration> {
        self.remote(
            "resume_registration",
            vec![
                to_value(token)?,
                to_value(pid)?,
                to_value(limits)?,
                to_value(providers)?,
                to_value(binary_version)?,
            ],
        )
    }
    fn set_auto_update(&self, token: &LeaseToken, enabled: bool) -> Result<()> {
        self.remote(
            "set_auto_update",
            vec![to_value(token)?, to_value(enabled)?],
        )
    }
    fn set_slot_limits(
        &self,
        token: &LeaseToken,
        limits: crate::domain::slot_limits::SlotLimits,
    ) -> Result<()> {
        self.remote("set_slot_limits", vec![to_value(token)?, to_value(limits)?])
    }
    fn set_max_load(
        &self,
        token: &LeaseToken,
        max_load: Option<f64>,
        source: Option<crate::domain::slot_limits::SettingSource>,
    ) -> Result<()> {
        let value = vec![to_value(token)?, to_value(max_load)?];
        let Some(source) = source else {
            return self.remote("set_max_load", value);
        };
        let mut sourced = value.clone();
        sourced.push(to_value(source)?);
        // The service of the binary before a handoff, until this
        // supervisor replaces it, refuses a source as arguments it does not
        // read: the value goes alone. Any other failure is the call's own.
        match self.remote("set_max_load", sourced) {
            Err(error) if super::Refused::bad_request(&error) => self.remote("set_max_load", value),
            result => result,
        }
    }
    fn rebind_repository(&mut self, common_dir: &str) -> Result<Option<String>> {
        self.remote("rebind_repository", vec![to_value(common_dir)?])
    }
    fn bind_repository(&mut self, common_dir: &str) -> Result<()> {
        self.remote("bind_repository", vec![to_value(common_dir)?])
    }
    fn assert_repository(&self, common_dir: &str) -> Result<()> {
        self.remote("assert_repository", vec![to_value(common_dir)?])
    }
    fn set_supervisor_mode(
        &self,
        token: &LeaseToken,
        mode: SupervisorMode,
        workspace_id: Option<&str>,
    ) -> Result<()> {
        self.remote(
            "set_supervisor_mode",
            vec![to_value(token)?, to_value(mode)?, to_value(workspace_id)?],
        )
    }
}

impl HeadlessJobStore for SupervisorQueue {
    fn record_headless_job(&self, job: &NewHeadlessJob) -> Result<i64> {
        let local = self.local()?;
        HeadlessJobStore::record_headless_job(&**local, job)
    }
    fn end_headless_job(&self, id: i64, outcome: &str) -> Result<bool> {
        let local = self.local()?;
        HeadlessJobStore::end_headless_job(&**local, id, outcome)
    }
    fn orphaned_headless_jobs(
        &self,
        token: &LeaseToken,
        own: bool,
    ) -> Result<Vec<HeadlessJobRecord>> {
        let local = self.local()?;
        HeadlessJobStore::orphaned_headless_jobs(&**local, token, own)
    }
}

impl AskStore for SupervisorQueue {
    #[allow(clippy::too_many_arguments)]
    fn open_update_ask(
        &mut self,
        kind: crate::domain::AskKind,
        question: &str,
        options: &[&str],
        asked_by: &str,
        subject: Option<&str>,
        details: serde_json::Value,
    ) -> Result<crate::domain::Ask> {
        let mut local = self.local()?;
        AskStore::open_update_ask(
            &mut **local,
            kind,
            question,
            options,
            asked_by,
            subject,
            details,
        )
    }
    fn ask_opened_payload(&self, id: crate::domain::AskId) -> Result<serde_json::Value> {
        let local = self.local()?;
        AskStore::ask_opened_payload(&**local, id)
    }
    #[allow(clippy::too_many_arguments)]
    fn close_installed_update_ask(
        &mut self,
        id: crate::domain::AskId,
        answer: &str,
        installed: &str,
    ) -> Result<Option<crate::domain::Ask>> {
        let mut local = self.local()?;
        AskStore::close_installed_update_ask(&mut **local, id, answer, installed)
    }
    fn release_updates_on(&self) -> bool {
        // Read off the queue's connection as the queue reads it: one that
        // does not open looks for no release.
        match self.local() {
            Ok(local) => AskStore::release_updates_on(&**local),
            Err(error) => {
                tracing::warn!("the queue could not be opened to read [update]: {error:#}");
                false
            }
        }
    }
    fn update_answers(&self, kind: &crate::domain::AskKind) -> Result<Vec<crate::domain::Ask>> {
        let local = self.local()?;
        AskStore::update_answers(&**local, kind)
    }
    fn asks(&self, query: AskQuery) -> Result<Vec<Ask>> {
        let local = self.local()?;
        AskStore::asks(&**local, query)
    }
    fn has_unclosed_ask(&self, run_id: &RunId, kind: AskKind) -> Result<bool> {
        let local = self.local()?;
        AskStore::has_unclosed_ask(&**local, run_id, kind)
    }
    fn ask(&mut self, ask: NewAsk) -> Result<AskOutcome> {
        let mut local = self.local()?;
        AskStore::ask(&mut **local, ask)
    }
    fn hold(&mut self, hold: crate::domain::NewHold) -> Result<crate::domain::HoldOutcome> {
        let mut local = self.local()?;
        AskStore::hold(&mut **local, hold)
    }
    fn hold_of(&self, run_id: &RunId) -> Result<Option<Ask>> {
        let local = self.local()?;
        AskStore::hold_of(&**local, run_id)
    }
    fn hold_unclosed(&self, run_id: &RunId) -> Result<bool> {
        let local = self.local()?;
        AskStore::hold_unclosed(&**local, run_id)
    }
    fn close_hold_asks(
        &mut self,
        reason: crate::domain::AskReason,
        subject: Option<&str>,
        answer: &str,
    ) -> Result<Vec<Ask>> {
        let mut local = self.local()?;
        AskStore::close_hold_asks(&mut **local, reason, subject, answer)
    }
    fn read_ask(&self, id: AskId) -> Result<Ask> {
        let local = self.local()?;
        AskStore::read_ask(&**local, id)
    }
    fn answer_as(
        &mut self,
        id: AskId,
        text: &str,
        answerer: crate::domain::Answerer,
    ) -> Result<Ask> {
        let mut local = self.local()?;
        AskStore::answer_as(&mut **local, id, text, answerer)
    }
    fn close_ask(&mut self, id: AskId) -> Result<Ask> {
        let mut local = self.local()?;
        AskStore::close_ask(&mut **local, id)
    }
    fn landing_answers(&self) -> Result<Vec<Ask>> {
        let local = self.local()?;
        AskStore::landing_answers(&**local)
    }
    fn triage_answers(&self) -> Result<Vec<Ask>> {
        let local = self.local()?;
        AskStore::triage_answers(&**local)
    }
    fn undelivered_answers(&self, run_id: &RunId) -> Result<Vec<Ask>> {
        let local = self.local()?;
        AskStore::undelivered_answers(&**local, run_id)
    }
    fn ask_delivered(&mut self, id: AskId, workspace_id: &str) -> Result<Ask> {
        let mut local = self.local()?;
        AskStore::ask_delivered(&mut **local, id, workspace_id)
    }
    fn has_unclosed_worker_question(&self, run_id: &RunId) -> Result<bool> {
        let local = self.local()?;
        AskStore::has_unclosed_worker_question(&**local, run_id)
    }
    fn has_unclosed_worker_question_since(
        &self,
        run_id: &RunId,
        created_from: i64,
    ) -> Result<bool> {
        let local = self.local()?;
        AskStore::has_unclosed_worker_question_since(&**local, run_id, created_from)
    }
    fn last_worker_question_closed(&self, run_id: &RunId) -> Result<Option<i64>> {
        let local = self.local()?;
        AskStore::last_worker_question_closed(&**local, run_id)
    }
    fn close_stuck_exit_asks(&mut self, run_id: &RunId, answer: &str) -> Result<Vec<Ask>> {
        let mut local = self.local()?;
        AskStore::close_stuck_exit_asks(&mut **local, run_id, answer)
    }
    fn close_answer_prompt_asks(&mut self, run_id: &RunId, answer: &str) -> Result<Vec<Ask>> {
        let mut local = self.local()?;
        AskStore::close_answer_prompt_asks(&mut **local, run_id, answer)
    }
    fn unclosed_stalled_ask(&self, run_id: &RunId) -> Result<Option<Ask>> {
        let local = self.local()?;
        AskStore::unclosed_stalled_ask(&**local, run_id)
    }
    fn close_stalled_asks(&mut self, run_id: &RunId, answer: &str) -> Result<Vec<Ask>> {
        let mut local = self.local()?;
        AskStore::close_stalled_asks(&mut **local, run_id, answer)
    }
    fn end_stalled_detections(&mut self, run_id: &RunId, answer: &str) -> Result<Vec<Ask>> {
        let mut local = self.local()?;
        AskStore::end_stalled_detections(&mut **local, run_id, answer)
    }
    fn note_on_asks(&mut self, run_id: &RunId, note: &str, why: &str) -> Result<Vec<Ask>> {
        let mut local = self.local()?;
        AskStore::note_on_asks(&mut **local, run_id, note, why)
    }
    fn close_approve_landing_asks(&mut self, run_id: &RunId, answer: &str) -> Result<Vec<Ask>> {
        let mut local = self.local()?;
        AskStore::close_approve_landing_asks(&mut **local, run_id, answer)
    }
    fn close_run_asks(&mut self, run_id: &RunId, answer: &str) -> Result<Vec<Ask>> {
        let mut local = self.local()?;
        AskStore::close_run_asks(&mut **local, run_id, answer)
    }
    fn close_ended_landing_asks(&mut self, task: Option<TaskId>) -> Result<Vec<Ask>> {
        let mut local = self.local()?;
        AskStore::close_ended_landing_asks(&mut **local, task)
    }
    fn close_blocked_asks(
        &mut self,
        run_id: &RunId,
        task_id: TaskId,
        answer: &str,
    ) -> Result<Vec<Ask>> {
        let mut local = self.local()?;
        AskStore::close_blocked_asks(&mut **local, run_id, task_id, answer)
    }
}

impl EventStore for SupervisorQueue {
    fn request_as(&self, requester: Option<&ActorContext>) -> Option<String> {
        let requested = requester.map(|actor| actor.actor_id().to_owned());
        if let Some(local) = self.local.borrow().as_ref() {
            local.restore_request(requested.clone());
        }
        self.requested_by.replace(requested)
    }
    fn restore_request(&self, previous: Option<String>) {
        if let Some(local) = self.local.borrow().as_ref() {
            local.restore_request(previous.clone());
        }
        self.requested_by.replace(previous);
    }
    fn act_as(&self, actor: ActorContext) -> Option<ActorContext> {
        if let Some(local) = self.local.borrow().as_ref() {
            local.act_as(actor.clone());
        }
        // The principal's own actor is the one written before any other.
        let previous = self.acting.replace(Some(actor));
        Some(previous.unwrap_or_else(|| self.service_actor()))
    }
    fn update_events(&self, limit: usize) -> Result<Vec<RunEvent>> {
        self.remote("update_events", vec![to_value(limit)?])
    }
    fn installs_after(&self, after: EventId, limit: usize) -> Result<(Vec<RunEvent>, usize)> {
        self.remote("installs_after", vec![to_value(after)?, to_value(limit)?])
    }
    fn e2e_gate_events(&self, limit: usize) -> Result<Vec<RunEvent>> {
        self.remote("e2e_gate_events", vec![to_value(limit)?])
    }
    fn all_events(&self) -> Result<Vec<RunEvent>> {
        let local = self.local()?;
        EventStore::all_events(&**local)
    }
    fn latest_task_events(&self, kinds: &[&str]) -> Result<Vec<RunEvent>> {
        self.remote("latest_task_events", vec![to_value(kinds)?])
    }
    fn run_events(&self, id: &RunId) -> Result<Vec<RunEvent>> {
        self.remote("run_events", vec![to_value(id)?])
    }
    fn task_events(&self, task: TaskId) -> Result<Vec<RunEvent>> {
        self.remote("task_events", vec![to_value(task)?])
    }
    fn has_run_event(&self, id: &RunId, kind: &str) -> Result<bool> {
        self.remote("has_run_event", vec![to_value(id)?, to_value(kind)?])
    }
    fn record_runtime_event(
        &self,
        id: &RunId,
        kind: EventKind,
        payload: serde_json::Value,
    ) -> Result<()> {
        self.remote(
            "record_runtime_event",
            vec![to_value(id)?, to_value(kind)?, to_value(payload)?],
        )
    }
    fn last_observe(&self, mode: &str) -> Result<Option<i64>> {
        self.remote("last_observe", vec![to_value(mode)?])
    }
    fn latest_event_id(&self) -> Result<EventId> {
        self.remote("latest_event_id", vec![])
    }
    fn record_backend_failure(
        &self,
        run: Option<&RunId>,
        payload: serde_json::Value,
    ) -> Result<()> {
        self.remote(
            "record_backend_failure",
            vec![to_value(run)?, to_value(payload)?],
        )
    }
    fn record_queue_event(&self, kind: EventKind, payload: serde_json::Value) -> Result<EventId> {
        self.remote(
            "record_queue_event",
            vec![to_value(kind)?, to_value(payload)?],
        )
    }
    fn record_attributes_baseline(&self) -> Result<Option<(usize, usize)>> {
        let local = self.local()?;
        EventStore::record_attributes_baseline(&**local)
    }
    fn record_findings_baseline(&self) -> Result<Option<usize>> {
        let local = self.local()?;
        EventStore::record_findings_baseline(&**local)
    }
    fn latest_event_of(&self, kind: &str) -> Result<Option<RunEvent>> {
        self.remote("latest_event_of", vec![to_value(kind)?])
    }
    fn latest_events_of(&self, kind: &str, limit: usize) -> Result<Vec<RunEvent>> {
        self.remote("latest_events_of", vec![to_value(kind)?, to_value(limit)?])
    }
    fn latest_queue_event(&self, kinds: &[&str]) -> Result<Option<RunEvent>> {
        self.remote("latest_queue_event", vec![to_value(kinds)?])
    }
    fn latest_events_by_supervisor(
        &self,
        kinds: &[&str],
        supervisors: &[&str],
    ) -> Result<Vec<RunEvent>> {
        self.remote(
            "latest_events_by_supervisor",
            vec![to_value(kinds)?, to_value(supervisors)?],
        )
    }
    fn events_of_between(
        &self,
        kinds: &[&str],
        after: EventId,
        upto: EventId,
        limit: usize,
    ) -> Result<Vec<RunEvent>> {
        self.remote(
            "events_of_between",
            vec![
                to_value(kinds)?,
                to_value(after)?,
                to_value(upto)?,
                to_value(limit)?,
            ],
        )
    }
    fn events_between(
        &self,
        after: EventId,
        upto: EventId,
        filter: &EventFilter,
        limit: usize,
    ) -> Result<Vec<RunEvent>> {
        self.remote(
            "events_between",
            vec![
                to_value(after)?,
                to_value(upto)?,
                to_value(filter)?,
                to_value(limit)?,
            ],
        )
    }
}

impl QueueRecords for SupervisorQueue {
    fn findings(&self, query: &crate::domain::FindingQuery) -> Result<Vec<FindingView>> {
        let local = self.local()?;
        QueueRecords::findings(&**local, query)
    }
    fn reports_written(&self) -> Result<std::collections::HashSet<(String, String)>> {
        let local = self.local()?;
        QueueRecords::reports_written(&**local)
    }
    fn record_report_written(&self, payload: serde_json::Value) -> Result<bool> {
        let local = self.local()?;
        QueueRecords::record_report_written(&**local, payload)
    }
    fn kpi_breaches_open(&self) -> Result<Vec<serde_json::Value>> {
        let local = self.local()?;
        QueueRecords::kpi_breaches_open(&**local)
    }
    fn record_kpi_breach(
        &self,
        kind: EventKind,
        payload: serde_json::Value,
        push_day: Option<(i64, usize)>,
    ) -> Result<Option<serde_json::Value>> {
        let local = self.local()?;
        QueueRecords::record_kpi_breach(&**local, kind, payload, push_day)
    }
    fn record_kpi_push_abandoned(&self, payload: serde_json::Value) -> Result<bool> {
        let local = self.local()?;
        QueueRecords::record_kpi_push_abandoned(&**local, payload)
    }
    fn record_forecast(
        &self,
        payload: serde_json::Value,
        previous: Option<EventId>,
    ) -> Result<Option<EventId>> {
        let local = self.local()?;
        QueueRecords::record_forecast(&**local, payload, previous)
    }
    fn ci_watch_events(&self) -> Result<Vec<RunEvent>> {
        let local = self.local()?;
        QueueRecords::ci_watch_events(&**local)
    }
    #[allow(clippy::too_many_arguments)]
    fn record_ci_check(
        &self,
        record: crate::domain::ci_watch::CiCheckRecord,
    ) -> Result<Option<crate::domain::ci_watch::CiCheckRecorded>> {
        let local = self.local()?;
        QueueRecords::record_ci_check(&**local, record)
    }
    fn record_finding_once(
        &self,
        finding: crate::domain::NewFinding,
    ) -> Result<Option<crate::domain::FindingId>> {
        let local = self.local()?;
        QueueRecords::record_finding_once(&**local, finding)
    }
    fn ci_failure_findings_of(&self, task: TaskId) -> Result<Vec<crate::domain::FindingId>> {
        let local = self.local()?;
        QueueRecords::ci_failure_findings_of(&**local, task)
    }
    fn ci_failure_fix_tasks(&self) -> Result<std::collections::BTreeMap<i64, Vec<TaskId>>> {
        let local = self.local()?;
        QueueRecords::ci_failure_fix_tasks(&**local)
    }
}

impl InboxWatchLog for SupervisorQueue {
    fn claim_inbox_nudge(&self, payload: serde_json::Value) -> Result<bool> {
        let local = self.local()?;
        InboxWatchLog::claim_inbox_nudge(&**local, payload)
    }
    fn record_inbox_watcher_change(
        &self,
        kind: EventKind,
        payload: serde_json::Value,
    ) -> Result<bool> {
        let local = self.local()?;
        InboxWatchLog::record_inbox_watcher_change(&**local, kind, payload)
    }
}

impl EvalRounds for SupervisorQueue {
    fn settle_eval_round(
        &self,
        eval_id: i64,
        kind: EventKind,
        payload: serde_json::Value,
    ) -> Result<bool> {
        let local = self.local()?;
        EvalRounds::settle_eval_round(&**local, eval_id, kind, payload)
    }
    fn take_up_eval_round(
        &self,
        eval_id: i64,
        token: &str,
        alive: &dyn Fn(u32) -> bool,
    ) -> Result<bool> {
        let local = self.local()?;
        EvalRounds::take_up_eval_round(&**local, eval_id, token, alive)
    }
    fn take_case_set(&self, set_id: i64, token: &str, alive: &dyn Fn(u32) -> bool) -> Result<bool> {
        let local = self.local()?;
        EvalRounds::take_case_set(&**local, set_id, token, alive)
    }
}
