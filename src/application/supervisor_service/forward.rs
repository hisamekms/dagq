//! [`SupervisorQueue`]'s operations: each goes to the service by its name,
//! which [`super::dispatch`] answers. Made from the ports' traits, one
//! method each, in their order; a method the traits add is a compile
//! error here until it is routed.

#![allow(clippy::wildcard_imports)]

use super::{SupervisorQueue, Wire, to_value};
use crate::application::*;
use crate::domain::*;
use anyhow::Result;
use std::collections::HashMap;
use std::path::Path;

use crate::application::Landing;
use crate::domain::cancel_reason::NewCancel;
use crate::domain::related::RelatedPage;
use crate::domain::search::{SearchPage, SearchQuery};

/// How many events [`EventStore::all_events`] asks for at a time, halved
/// for a page past the answer's limit.
const EVENTS_PAGE: usize = 2000;

/// How many runs [`RunReads::all_runs`] asks for at a time, halved for a
/// page past the answer's limit.
const RUNS_PAGE: usize = 500;

/// How many rows of the NodeSampleStore a read asks for at a time, halved
/// for a page past the answer's limit.
const SAMPLES_PAGE: usize = 10_000;

/// How many windows of the runs' resources a read asks for at a time,
/// halved for a page past the answer's limit.
const RESOURCES_PAGE: usize = 10_000;

/// How many bytes of samples one request of a file's import holds at most:
/// half the request's limit, which leaves the rest of the request room.
const SAMPLES_REQUEST_BYTES: usize = crate::domain::queue_service::MAX_REQUEST_BYTES / 2;

/// `items` in consecutive parts whose JSON is at most `budget` bytes each
/// (an item larger than it a part of its own), and one empty part for
/// none.
pub(super) fn parts_within<T: serde::Serialize>(items: &[T], budget: usize) -> Result<Vec<&[T]>> {
    let mut parts = Vec::new();
    let (mut start, mut size) = (0, 0);
    for (index, item) in items.iter().enumerate() {
        // The item and the comma before it.
        let bytes = serde_json::to_vec(item)?.len() + 1;
        if index > start && size + bytes > budget {
            parts.push(&items[start..index]);
            (start, size) = (index, 0);
        }
        size += bytes;
    }
    parts.push(&items[start..]);
    Ok(parts)
}

impl TaskReads for SupervisorQueue {
    fn list(&self, query: &TaskQuery) -> Result<TaskPage> {
        self.remote("list", vec![to_value(query)?])
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
        self.remote("show_goal", vec![to_value(goal_id)?])
    }
    fn show_proposal(&self, proposal_id: ProposalId) -> Result<Proposal> {
        self.remote("show_proposal", vec![to_value(proposal_id)?])
    }
    fn proposals(&self, all: bool) -> Result<Vec<Proposal>> {
        self.remote("proposals", vec![to_value(all)?])
    }
    fn lint_input(&self, tasks: &[TaskId]) -> Result<LintInput> {
        self.remote("lint_input", vec![to_value(tasks)?])
    }
    fn notes(&self, query: &NoteQuery) -> Result<NotePage> {
        self.remote("notes", vec![to_value(query)?])
    }
}

/// Every operation is sent, the planning a person, the inbox or a planner
/// does too: the service refuses what is none of the supervisor's
/// ([`super::needs`]).
impl TaskStore for SupervisorQueue {
    fn add(&mut self, task: NewTask) -> Result<Task> {
        self.remote("add", vec![to_value(task)?])
    }
    fn transition(&mut self, task_id: TaskId, action: TaskAction) -> Result<Task> {
        self.remote("transition", vec![to_value(task_id)?, to_value(action)?])
    }
    fn cancel(&mut self, task_id: TaskId, cancel: NewCancel) -> Result<Task> {
        self.remote("cancel", vec![to_value(task_id)?, to_value(cancel)?])
    }
    fn add_dependency(&mut self, task_id: TaskId, predecessor_id: TaskId) -> Result<()> {
        self.remote(
            "add_dependency",
            vec![to_value(task_id)?, to_value(predecessor_id)?],
        )
    }
    fn remove_dependency(&mut self, task_id: TaskId, predecessor_id: TaskId) -> Result<()> {
        self.remote(
            "remove_dependency",
            vec![to_value(task_id)?, to_value(predecessor_id)?],
        )
    }
    fn add_goal_dependency(&mut self, task_id: TaskId, goal_id: GoalId) -> Result<()> {
        self.remote(
            "add_goal_dependency",
            vec![to_value(task_id)?, to_value(goal_id)?],
        )
    }
    fn remove_goal_dependency(&mut self, task_id: TaskId, goal_id: GoalId) -> Result<()> {
        self.remote(
            "remove_goal_dependency",
            vec![to_value(task_id)?, to_value(goal_id)?],
        )
    }
    fn ready_at(&self, tasks: &[TaskId]) -> Result<HashMap<TaskId, i64>> {
        self.remote("ready_at", vec![to_value(tasks)?])
    }
    fn investigations(&mut self) -> Result<crate::domain::investigation::Investigations> {
        self.remote("investigations", vec![])
    }
    fn claim(&mut self, base_commit: &CommitSha) -> Result<ClaimOutcome> {
        self.remote("claim", vec![to_value(base_commit)?])
    }
    fn add_goal(&mut self, goal: NewGoal) -> Result<Goal> {
        self.remote("add_goal", vec![to_value(goal)?])
    }
    fn edit_goal(&mut self, goal_id: GoalId, edit: GoalEdit) -> Result<Goal> {
        self.remote("edit_goal", vec![to_value(goal_id)?, to_value(edit)?])
    }
    fn close_goal(&mut self, goal_id: GoalId, verdict: GoalVerdict) -> Result<Goal> {
        self.remote("close_goal", vec![to_value(goal_id)?, to_value(verdict)?])
    }
    fn set_goal(&mut self, task_id: TaskId, goal_id: Option<GoalId>) -> Result<Task> {
        self.remote("set_goal", vec![to_value(task_id)?, to_value(goal_id)?])
    }
    fn set_paths(&mut self, task_id: TaskId, paths: Vec<String>) -> Result<Task> {
        self.remote("set_paths", vec![to_value(task_id)?, to_value(paths)?])
    }
    fn edit_task(
        &mut self,
        task_id: TaskId,
        edit: TaskEdit,
        authorized: TaskStatus,
    ) -> Result<Task> {
        self.remote(
            "edit_task",
            vec![to_value(task_id)?, to_value(edit)?, to_value(authorized)?],
        )
    }
    fn set_priority(&mut self, task_id: TaskId, priority: Option<Priority>) -> Result<Task> {
        self.remote(
            "set_priority",
            vec![to_value(task_id)?, to_value(priority)?],
        )
    }
    fn submit(&mut self, submission: Submission) -> Result<Proposal> {
        self.remote("submit", vec![to_value(submission)?])
    }
    fn approve_proposal(&mut self, proposal_id: ProposalId) -> Result<Proposal> {
        self.remote("approve_proposal", vec![to_value(proposal_id)?])
    }
    fn send_back_proposal(&mut self, proposal_id: ProposalId) -> Result<Proposal> {
        self.remote("send_back_proposal", vec![to_value(proposal_id)?])
    }
    fn withdraw_proposal(&mut self, proposal_id: ProposalId) -> Result<Proposal> {
        self.remote("withdraw_proposal", vec![to_value(proposal_id)?])
    }
    fn ready_goal(&mut self, goal_id: GoalId) -> Result<Goal> {
        self.remote("ready_goal", vec![to_value(goal_id)?])
    }
    fn add_note(&mut self, note: NewNote) -> Result<RunEvent> {
        self.remote("add_note", vec![to_value(note)?])
    }
}

impl PlanningRecords for SupervisorQueue {
    fn related_landed_commits(&self, task: TaskId, limit: usize) -> Result<Vec<String>> {
        self.remote(
            "related_landed_commits",
            vec![to_value(task)?, to_value(limit)?],
        )
    }
    fn related_tasks(
        &self,
        task: TaskId,
        statuses: &[String],
        limit: usize,
    ) -> Result<RelatedPage> {
        self.remote(
            "related_tasks",
            vec![to_value(task)?, to_value(statuses)?, to_value(limit)?],
        )
    }
    fn search_documents(&self, query: &SearchQuery) -> Result<SearchPage> {
        self.remote("search_documents", vec![to_value(query)?])
    }
    fn task_goals(&self) -> Result<HashMap<TaskId, Option<GoalId>>> {
        self.remote("task_goals", vec![])
    }
    fn task_titles(&self) -> Result<HashMap<TaskId, String>> {
        self.remote("task_titles", vec![])
    }
    fn task_changes(&self) -> Result<HashMap<TaskId, Option<TaskChange>>> {
        self.remote("task_changes", vec![])
    }
    fn draft_origins(&self) -> Result<HashMap<TaskId, DraftOrigin>> {
        self.remote("draft_origins", vec![])
    }
}

impl PlanRequestStore for SupervisorQueue {
    fn planner_requests(&self) -> Result<Vec<crate::domain::plan_request::PlanRequest>> {
        self.remote("planner_requests", vec![])
    }
    fn open_request_planner(
        &mut self,
        request: crate::domain::RequestId,
        answer: Option<AskId>,
    ) -> Result<RequestPlannerStart> {
        self.remote(
            "open_request_planner",
            vec![to_value(request)?, to_value(answer)?],
        )
    }
    fn plan_request(
        &self,
        request: crate::domain::RequestId,
    ) -> Result<crate::domain::plan_request::PlanRequest> {
        self.remote("plan_request", vec![to_value(request)?])
    }
    fn request_asks(&self, request: crate::domain::RequestId) -> Result<Vec<Ask>> {
        self.remote("request_asks", vec![to_value(request)?])
    }
    fn event_by_id(&self, id: EventId) -> Result<Option<RunEvent>> {
        self.remote("event_by_id", vec![to_value(id)?])
    }
    fn replan_brief(
        &self,
        id: crate::domain::ReplanId,
    ) -> Result<crate::domain::replan_plan::ReplanBrief> {
        self.remote("replan_brief", vec![to_value(id)?])
    }
    fn replacement_brief(
        &self,
        proposal: crate::domain::ProposalId,
    ) -> Result<Option<crate::domain::replan_plan::ReplanBrief>> {
        self.remote("replacement_brief", vec![to_value(proposal)?])
    }
    fn settle_replan_requests(
        &mut self,
    ) -> Result<Vec<crate::domain::task_replan::ReplanOperation>> {
        self.remote("settle_replan_requests", vec![])
    }
    fn replacements_to_apply(&self) -> Result<Vec<crate::domain::task_replan::ReplanOperation>> {
        self.remote("replacements_to_apply", vec![])
    }
    fn begin_replan_apply(
        &mut self,
        id: crate::domain::ReplanId,
        generation: i64,
    ) -> Result<crate::domain::task_replan::ReplanOperation> {
        self.remote(
            "begin_replan_apply",
            vec![to_value(id)?, to_value(generation)?],
        )
    }
    fn apply_replacement(
        &mut self,
        id: crate::domain::ReplanId,
        generation: i64,
        snapshot_problem: Option<&str>,
    ) -> Result<crate::domain::replan_apply::ApplyOutcome> {
        self.remote(
            "apply_replacement",
            vec![
                to_value(id)?,
                to_value(generation)?,
                to_value(snapshot_problem)?,
            ],
        )
    }
    fn child_carry(&self, task: TaskId) -> Result<Option<crate::domain::replan_apply::ChildCarry>> {
        self.remote("child_carry", vec![to_value(task)?])
    }
}

impl DraftPlannerReads for SupervisorQueue {
    fn planner_findings(&self) -> Result<Vec<Finding>> {
        self.remote("planner_findings", vec![])
    }
    fn improvements(&self, limit: usize) -> Result<crate::domain::ImprovementLimit> {
        self.remote("improvements", vec![to_value(limit)?])
    }
}

impl DraftPlannerStore for SupervisorQueue {
    fn register_follow_ups(
        &mut self,
        run: &RunId,
        entries: Vec<FollowUpRegistration>,
        depth: i64,
    ) -> Result<Vec<crate::domain::RegisteredFollowUp>> {
        self.remote(
            "register_follow_ups",
            vec![to_value(run)?, to_value(entries)?, to_value(depth)?],
        )
    }
    fn record_draft_origin(
        &mut self,
        task: TaskId,
        origin: DraftOrigin,
        material: &serde_json::Value,
    ) -> Result<()> {
        self.remote(
            "record_draft_origin",
            vec![to_value(task)?, to_value(origin)?, to_value(material)?],
        )
    }
    fn draft_origin(&self, task: TaskId) -> Result<Option<(DraftOrigin, serde_json::Value)>> {
        self.remote("draft_origin", vec![to_value(task)?])
    }
    fn register_recurring_draft(
        &mut self,
        item: &crate::domain::recurring::RecurringDraft,
    ) -> Result<crate::domain::recurring::RecurringOutcome> {
        self.remote("register_recurring_draft", vec![to_value(item)?])
    }
    fn planner_drafts(&self) -> Result<Vec<DraftTarget>> {
        self.remote("planner_drafts", vec![])
    }
    fn open_draft_planner(
        &mut self,
        drafts: &[TaskId],
        answer: Option<AskId>,
    ) -> Result<DraftPlannerStart> {
        self.remote(
            "open_draft_planner",
            vec![to_value(drafts)?, to_value(answer)?],
        )
    }
    fn planner_draft_tasks(&self, planner: PlannerId) -> Result<Vec<TaskId>> {
        self.remote("planner_draft_tasks", vec![to_value(planner)?])
    }
    fn draft_bundle(&self, planner: PlannerId) -> Result<Option<crate::domain::DraftBundleView>> {
        self.remote("draft_bundle", vec![to_value(planner)?])
    }
    fn planner_answers(&self) -> Result<Vec<Ask>> {
        self.remote("planner_answers", vec![])
    }
    fn planner_answer_route(&self, ask: &Ask) -> Result<PlannerAnswerRoute> {
        self.remote("planner_answer_route", vec![to_value(ask)?])
    }
    fn proposal_questions(&self, proposal: ProposalId) -> Result<Vec<Ask>> {
        self.remote("proposal_questions", vec![to_value(proposal)?])
    }
    fn replaced_tasks(&self, proposal: ProposalId) -> Result<Vec<TaskId>> {
        self.remote("replaced_tasks", vec![to_value(proposal)?])
    }
    fn answer_request(&self, ask: &Ask) -> Result<Option<crate::domain::RequestId>> {
        self.remote("answer_request", vec![to_value(ask)?])
    }
    fn claim_planner_answer(
        &mut self,
        ask: AskId,
        planner: PlannerId,
        workspace: &str,
    ) -> Result<bool> {
        self.remote(
            "claim_planner_answer",
            vec![to_value(ask)?, to_value(planner)?, to_value(workspace)?],
        )
    }
    fn close_planner_answer(&mut self, ask: AskId, why: &str) -> Result<()> {
        self.remote("close_planner_answer", vec![to_value(ask)?, to_value(why)?])
    }
    fn record_task_event(
        &mut self,
        task: TaskId,
        kind: EventKind,
        payload: serde_json::Value,
    ) -> Result<()> {
        self.remote(
            "record_task_event",
            vec![to_value(task)?, to_value(kind)?, to_value(payload)?],
        )
    }
    fn exhausted_drafts(&self) -> Result<Vec<Task>> {
        self.remote("exhausted_drafts", vec![])
    }
    fn follow_up_depth(&self, task: TaskId) -> Result<i64> {
        self.remote("follow_up_depth", vec![to_value(task)?])
    }
    fn set_follow_up_depth(&mut self, task: TaskId, depth: i64) -> Result<()> {
        self.remote(
            "set_follow_up_depth",
            vec![to_value(task)?, to_value(depth)?],
        )
    }
    fn open_finding_planner(
        &mut self,
        finding: FindingId,
        answer: Option<AskId>,
        limit: usize,
    ) -> Result<FindingPlannerStart> {
        self.remote(
            "open_finding_planner",
            vec![to_value(finding)?, to_value(answer)?, to_value(limit)?],
        )
    }
    fn finding_view(&self, finding: FindingId) -> Result<FindingView> {
        self.remote("finding_view", vec![to_value(finding)?])
    }
    fn finding_asks(&self, finding: FindingId) -> Result<Vec<Ask>> {
        self.remote("finding_asks", vec![to_value(finding)?])
    }
    fn settle_findings(&mut self) -> Result<Vec<(FindingId, FindingStatus)>> {
        self.remote("settle_findings", vec![])
    }
    fn exhausted_findings(&self) -> Result<Vec<Finding>> {
        self.remote("exhausted_findings", vec![])
    }
    fn record_finding_event(
        &mut self,
        finding: FindingId,
        kind: EventKind,
        payload: serde_json::Value,
    ) -> Result<()> {
        self.remote(
            "record_finding_event",
            vec![to_value(finding)?, to_value(kind)?, to_value(payload)?],
        )
    }
    fn ask_delivered_to(&self, ask: AskId, workspace: &str) -> Result<bool> {
        self.remote(
            "ask_delivered_to",
            vec![to_value(ask)?, to_value(workspace)?],
        )
    }
    fn answer_claimed_at(&self, ask: AskId, workspace: &str) -> Result<Option<i64>> {
        self.remote(
            "answer_claimed_at",
            vec![to_value(ask)?, to_value(workspace)?],
        )
    }
    fn ask_delivery_failed(&self, ask: AskId, planner: PlannerId) -> Result<bool> {
        self.remote(
            "ask_delivery_failed",
            vec![to_value(ask)?, to_value(planner)?],
        )
    }
}

impl PlanReviewStore for SupervisorQueue {
    fn interrupt_plan_reviews_for_handoff(
        &mut self,
        token: &LeaseToken,
        keep: &[i64],
    ) -> Result<()> {
        self.remote(
            "interrupt_plan_reviews_for_handoff",
            vec![to_value(token)?, to_value(keep)?],
        )
    }
    fn plan_review_candidates(&self) -> Result<Vec<PlanReviewCandidate>> {
        self.remote("plan_review_candidates", vec![])
    }
    #[allow(clippy::too_many_arguments)]
    fn begin_plan_review(
        &mut self,
        proposal: ProposalId,
        token: &LeaseToken,
        plan_reviews_dir: &Path,
        cwd: &Path,
        launch: &crate::domain::actor_model::ActorLaunch,
        _supervisor_alive: &dyn Fn(u32) -> bool,
    ) -> Result<Option<PlanReviewJob>> {
        self.remote(
            "begin_plan_review",
            vec![
                to_value(proposal)?,
                to_value(token)?,
                to_value(plan_reviews_dir)?,
                to_value(cwd)?,
                to_value(launch)?,
            ],
        )
    }
    fn take_over_plan_review(
        &mut self,
        job: &PlanReviewJob,
        token: &LeaseToken,
        _alive: &dyn Fn(u32) -> bool,
    ) -> Result<crate::domain::headless_job::ReviewTakeover> {
        self.remote(
            "take_over_plan_review",
            vec![to_value(job)?, to_value(token)?],
        )
    }
    fn finish_plan_review(
        &mut self,
        job: &PlanReviewJob,
        token: &LeaseToken,
        apply: &PlanReviewApply,
    ) -> Result<PlanReviewApplied> {
        self.remote(
            "finish_plan_review",
            vec![to_value(job)?, to_value(token)?, to_value(apply)?],
        )
    }
    fn fail_plan_review(
        &mut self,
        job: &PlanReviewJob,
        token: &LeaseToken,
        failure: &PlanReviewFailure,
    ) -> Result<()> {
        self.remote(
            "fail_plan_review",
            vec![to_value(job)?, to_value(token)?, to_value(failure)?],
        )
    }
    fn plan_reviews_finished_before(&self, before: i64) -> Result<Vec<i64>> {
        self.remote("plan_reviews_finished_before", vec![to_value(before)?])
    }
    fn revising_proposals(&self) -> Result<Vec<RevisingProposal>> {
        self.remote("revising_proposals", vec![])
    }
    fn claim_revise(&mut self, proposal: ProposalId) -> Result<bool> {
        self.remote("claim_revise", vec![to_value(proposal)?])
    }
    fn revise_goal_wait(&self, proposal: ProposalId) -> Result<ReviseGoalWait> {
        self.remote("revise_goal_wait", vec![to_value(proposal)?])
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
        self.remote(
            "revise_sent",
            vec![
                to_value(proposal)?,
                to_value(planner)?,
                to_value(workspace)?,
                to_value(opened)?,
                to_value(answers)?,
            ],
        )
    }
    fn revise_lost(
        &mut self,
        proposal: ProposalId,
        planner: Option<PlannerId>,
        why: &str,
    ) -> Result<()> {
        self.remote(
            "revise_lost",
            vec![to_value(proposal)?, to_value(planner)?, to_value(why)?],
        )
    }
    fn planner_unresponsive(
        &mut self,
        proposal: ProposalId,
        planner: Option<PlannerId>,
        waited_secs: i64,
        holders: &[PlannerHold],
    ) -> Result<()> {
        self.remote(
            "planner_unresponsive",
            vec![
                to_value(proposal)?,
                to_value(planner)?,
                to_value(waited_secs)?,
                to_value(holders)?,
            ],
        )
    }
    fn settle_proposals(&mut self) -> Result<Vec<(ProposalId, crate::domain::ProposalStatus)>> {
        self.remote("settle_proposals", vec![])
    }
    fn plan_answers(&self) -> Result<Vec<Ask>> {
        self.remote("plan_answers", vec![])
    }
    fn decide_plan(&mut self, ask: AskId) -> Result<Option<PlanDecided>> {
        self.remote("decide_plan", vec![to_value(ask)?])
    }
    fn applies_plan_answer(&self, ask: &Ask) -> Result<bool> {
        self.remote("applies_plan_answer", vec![to_value(ask)?])
    }
    fn plan_review_holds(&self) -> Result<Vec<PlanReviewHold>> {
        self.remote("plan_review_holds", vec![])
    }
    fn stranded_dependencies(&self) -> Result<Vec<StrandedDependency>> {
        self.remote("stranded_dependencies", vec![])
    }
    fn answered_asks(&self, limit: usize) -> Result<Vec<Ask>> {
        self.remote("answered_asks", vec![to_value(limit)?])
    }
}

impl GoalReviewStore for SupervisorQueue {
    fn interrupt_goal_reviews_for_handoff(
        &mut self,
        token: &LeaseToken,
        keep: &[i64],
    ) -> Result<()> {
        self.remote(
            "interrupt_goal_reviews_for_handoff",
            vec![to_value(token)?, to_value(keep)?],
        )
    }
    fn goal_review_candidates(&self) -> Result<Vec<GoalId>> {
        self.remote("goal_review_candidates", vec![])
    }
    fn goal_replacements(
        &self,
        goal: GoalId,
    ) -> Result<crate::domain::replan_apply::GoalReplacements> {
        self.remote("goal_replacements", vec![to_value(goal)?])
    }
    fn unresolved_replacements(
        &self,
    ) -> Result<
        Vec<(
            Option<GoalId>,
            crate::domain::replan_apply::UnresolvedReplacement,
        )>,
    > {
        self.remote("unresolved_replacements", vec![])
    }
    #[allow(clippy::too_many_arguments)]
    fn begin_goal_review(
        &mut self,
        goal: GoalId,
        token: &LeaseToken,
        goal_reviews_dir: &Path,
        cwd: &Path,
        launch: &crate::domain::actor_model::ActorLaunch,
        _supervisor_alive: &dyn Fn(u32) -> bool,
    ) -> Result<Option<GoalReviewJob>> {
        self.remote(
            "begin_goal_review",
            vec![
                to_value(goal)?,
                to_value(token)?,
                to_value(goal_reviews_dir)?,
                to_value(cwd)?,
                to_value(launch)?,
            ],
        )
    }
    fn goal_reviews(&self, goal: GoalId) -> Result<Vec<GoalReviewRecord>> {
        self.remote("goal_reviews", vec![to_value(goal)?])
    }
    fn take_over_goal_review(
        &mut self,
        job: &GoalReviewJob,
        token: &LeaseToken,
        _alive: &dyn Fn(u32) -> bool,
    ) -> Result<crate::domain::headless_job::ReviewTakeover> {
        self.remote(
            "take_over_goal_review",
            vec![to_value(job)?, to_value(token)?],
        )
    }
    fn finish_goal_review(
        &mut self,
        job: &GoalReviewJob,
        token: &LeaseToken,
        apply: &GoalReviewApply,
    ) -> Result<GoalReviewApplied> {
        self.remote(
            "finish_goal_review",
            vec![to_value(job)?, to_value(token)?, to_value(apply)?],
        )
    }
    fn fail_goal_review(
        &mut self,
        job: &GoalReviewJob,
        token: &LeaseToken,
        failure: &GoalReviewFailure,
    ) -> Result<()> {
        self.remote(
            "fail_goal_review",
            vec![to_value(job)?, to_value(token)?, to_value(failure)?],
        )
    }
    fn goal_answers(&self) -> Result<Vec<Ask>> {
        self.remote("goal_answers", vec![])
    }
    fn decide_goal(&mut self, ask: AskId) -> Result<Option<GoalDecided>> {
        self.remote("decide_goal", vec![to_value(ask)?])
    }
    fn correction_answers(&self) -> Result<Vec<Ask>> {
        self.remote("correction_answers", vec![])
    }
    fn decide_correction(&mut self, ask: AskId) -> Result<Option<serde_json::Value>> {
        self.remote("decide_correction", vec![to_value(ask)?])
    }
    fn goal_review_holds(&self) -> Result<Vec<GoalReviewHold>> {
        self.remote("goal_review_holds", vec![])
    }
    fn goal_follow_ups(&self) -> Result<Vec<crate::domain::follow_up::GoalFollowUps>> {
        self.remote("goal_follow_ups", vec![])
    }
    fn rearm_goal_review(&mut self, goal: GoalId) -> Result<serde_json::Value> {
        self.remote("rearm_goal_review", vec![to_value(goal)?])
    }
}

impl RunReads for SupervisorQueue {
    fn active_runs(&self) -> Result<Vec<TaskRun>> {
        self.remote("active_runs", vec![])
    }
    // A page at a time from where the page before ended: the runs only
    // grow, in the order they were claimed, and none is removed, so the
    // pages are the runs the one read would have given, however many the
    // queue's history holds.
    fn all_runs(&self) -> Result<Vec<TaskRun>> {
        let mut offset = 0;
        pages(RUNS_PAGE, usize::MAX, |page| {
            let read: Vec<TaskRun> =
                self.remote("runs_page", vec![to_value(offset)?, to_value(page)?])?;
            offset += read.len();
            Ok(read)
        })
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
    fn approve_integration(
        &mut self,
        id: &RunId,
        payload: serde_json::Value,
        _held: &dyn Fn(&crate::domain::RunLease) -> bool,
    ) -> Result<bool> {
        self.remote("approve_integration", vec![to_value(id)?, payload])
    }
    fn begin_integration(
        &mut self,
        id: &RunId,
        token: &LeaseToken,
        main: &CommitSha,
    ) -> Result<TaskRun> {
        self.begin_landing(id, token, main, false)
    }
    fn begin_integration_over_handoff(
        &mut self,
        id: &RunId,
        token: &LeaseToken,
        main: &CommitSha,
    ) -> Result<TaskRun> {
        self.begin_landing(id, token, main, true)
    }
    fn begin_e2e(
        &mut self,
        id: &RunId,
        token: &LeaseToken,
        payload: serde_json::Value,
    ) -> Result<()> {
        self.remote(
            "begin_e2e",
            vec![to_value(id)?, to_value(token)?, payload, to_value(false)?],
        )
    }
    fn begin_e2e_over_handoff(
        &mut self,
        id: &RunId,
        token: &LeaseToken,
        payload: serde_json::Value,
    ) -> Result<()> {
        self.remote(
            "begin_e2e",
            vec![to_value(id)?, to_value(token)?, payload, to_value(true)?],
        )
    }
    fn defer_integration(
        &mut self,
        id: &RunId,
        token: &LeaseToken,
        reason: &str,
        detail: serde_json::Value,
    ) -> Result<TaskRun> {
        self.remote(
            "defer_integration",
            vec![
                to_value(id)?,
                to_value(token)?,
                to_value(reason)?,
                to_value(detail)?,
            ],
        )
    }
    fn hold_integration(
        &mut self,
        id: &RunId,
        token: &LeaseToken,
        reason: &str,
        detail: serde_json::Value,
    ) -> Result<TaskRun> {
        self.remote(
            "hold_integration",
            vec![
                to_value(id)?,
                to_value(token)?,
                to_value(reason)?,
                to_value(detail)?,
            ],
        )
    }
    fn fail_integration(
        &mut self,
        id: &RunId,
        token: &LeaseToken,
        reason: &str,
        receipt: serde_json::Value,
    ) -> Result<TaskRun> {
        self.remote(
            "fail_integration",
            vec![
                to_value(id)?,
                to_value(token)?,
                to_value(reason)?,
                to_value(receipt)?,
            ],
        )
    }
    fn abort_integration(
        &mut self,
        id: &RunId,
        token: &LeaseToken,
        revert_to: &str,
        message: &str,
        reason: &Reason,
    ) -> Result<TaskRun> {
        self.remote(
            "abort_integration",
            vec![
                to_value(id)?,
                to_value(token)?,
                to_value(revert_to)?,
                to_value(message)?,
                to_value(reason)?,
            ],
        )
    }
    fn hand_over_integration(
        &mut self,
        id: &RunId,
        token: &LeaseToken,
        revert_to: &str,
        reason: &str,
        detail: serde_json::Value,
    ) -> Result<TaskRun> {
        self.remote(
            "hand_over_integration",
            vec![
                to_value(id)?,
                to_value(token)?,
                to_value(revert_to)?,
                to_value(reason)?,
                to_value(detail)?,
            ],
        )
    }
    fn finish_integration(
        &mut self,
        id: &RunId,
        token: &LeaseToken,
        landing: &Landing,
        common_dir: &str,
    ) -> Result<(Task, TaskRun)> {
        self.remote(
            "finish_integration",
            vec![
                to_value(id)?,
                to_value(token)?,
                to_value(landing)?,
                to_value(common_dir)?,
            ],
        )
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
    fn open_replan_operations(&self) -> Result<Vec<crate::domain::task_replan::ReplanOperation>> {
        self.remote("open_replan_operations", vec![])
    }
    fn replan_lineage(
        &self,
        task: crate::domain::TaskId,
    ) -> Result<Vec<crate::domain::task_replan::ReplanOperation>> {
        self.remote("replan_lineage", vec![to_value(task)?])
    }
    fn detect_replan(
        &mut self,
        new: &crate::domain::task_replan::NewReplan,
        by_id: &str,
    ) -> Result<crate::domain::task_replan::ReplanOperation> {
        self.remote("detect_replan", vec![to_value(new)?, to_value(by_id)?])
    }
    fn ask_replan_diagnosis(
        &mut self,
        ask: &crate::domain::NewAsk,
        op: (crate::domain::ReplanId, i64),
        reason: &str,
        run: &RunId,
        (payload, premise): (
            serde_json::Value,
            Option<&crate::domain::task_replan::DiagnosisPremise>,
        ),
    ) -> Result<(
        crate::domain::AskOutcome,
        crate::domain::task_replan::ReplanOperation,
    )> {
        self.remote(
            "ask_replan_diagnosis",
            vec![
                to_value(ask)?,
                to_value(op)?,
                to_value(reason)?,
                to_value(run)?,
                payload,
                to_value(premise)?,
            ],
        )
    }
    fn running_diagnosis_owners(&self) -> Result<Vec<crate::domain::replan_detect::RunningOwner>> {
        // The service of the binary before a handoff, until this
        // supervisor replaces it, answers the owners as pids alone, which
        // cannot tell a dead owner whose pid another process took from a
        // live one, nor another pid namespace's supervisor from this one:
        // no owner is read, so no diagnosis job starts until then.
        match self.remote("diagnosis_owners", vec![]) {
            Err(error) if super::Refused::bad_request(&error) => Err(error.context(
                "the queue's service of another binary does not tell the owners of the diagnosis jobs by their registration",
            )),
            result => result,
        }
    }
    fn start_replan_diagnosis(
        &mut self,
        op: (crate::domain::ReplanId, i64),
        reason: &str,
        run: &RunId,
        (payload, owner, gone): (
            serde_json::Value,
            &crate::domain::replan_detect::JobOwner,
            &[crate::domain::replan_detect::JobOwner],
        ),
    ) -> Result<crate::domain::task_replan::ReplanOperation> {
        self.remote(
            "start_replan_diagnosis",
            vec![
                to_value(op)?,
                to_value(reason)?,
                to_value(run)?,
                payload,
                to_value(owner)?,
                to_value(gone)?,
            ],
        )
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
    fn fail_continuation_start(
        &mut self,
        id: &RunId,
        token: &LeaseToken,
        op: crate::domain::ReplanId,
        generation: i64,
        why: &str,
    ) -> Result<bool> {
        self.remote(
            "fail_continuation_start",
            vec![
                to_value(id)?,
                to_value(token)?,
                to_value(op)?,
                to_value(generation)?,
                to_value(why)?,
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
        self.remote(
            "decide_landing",
            vec![
                to_value(id)?,
                to_value(status)?,
                to_value(reason)?,
                to_value(payload)?,
            ],
        )
    }
    fn close_satisfied(
        &mut self,
        id: &RunId,
        token: Option<&LeaseToken>,
        payload: serde_json::Value,
    ) -> Result<(Task, TaskRun)> {
        self.remote(
            "close_satisfied",
            vec![to_value(id)?, to_value(token)?, to_value(payload)?],
        )
    }
    #[allow(clippy::too_many_arguments)]
    fn decide_satisfied(
        &mut self,
        id: &RunId,
        reason: &str,
        payload: serde_json::Value,
        task: &crate::domain::satisfied::TaskMove,
    ) -> Result<TaskRun> {
        self.remote(
            "decide_satisfied",
            vec![
                to_value(id)?,
                to_value(reason)?,
                to_value(payload)?,
                to_value(task)?,
            ],
        )
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
        self.remote("runs_to_triage", vec![])
    }
    #[allow(clippy::too_many_arguments)]
    fn begin_triage(
        &mut self,
        id: &RunId,
        token: &LeaseToken,
        request: Option<serde_json::Value>,
        launch: &crate::domain::actor_model::ActorLaunch,
    ) -> Result<Option<(TaskRun, usize)>> {
        self.remote(
            "begin_triage",
            vec![
                to_value(id)?,
                to_value(token)?,
                to_value(request)?,
                to_value(launch)?,
            ],
        )
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
        self.remote(
            "finish_triage",
            vec![
                to_value(id)?,
                to_value(token)?,
                to_value(action)?,
                to_value(payload)?,
                to_value(also)?,
            ],
        )
    }
    fn decide_triage(
        &mut self,
        id: &RunId,
        ask_id: AskId,
        answer: &str,
        reason: &str,
    ) -> Result<TaskRun> {
        self.remote(
            "decide_triage",
            vec![
                to_value(id)?,
                to_value(ask_id)?,
                to_value(answer)?,
                to_value(reason)?,
            ],
        )
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
        self.remote(
            "keep_triage_answer",
            vec![
                to_value(id)?,
                to_value(ask_id)?,
                to_value(answer)?,
                to_value(reason)?,
            ],
        )
    }
    fn keep_live_verdict(&mut self, id: &RunId, verdict: serde_json::Value) -> Result<bool> {
        self.remote("keep_live_verdict", vec![to_value(id)?, to_value(verdict)?])
    }
    fn end_holds_of_ended_tasks(&mut self) -> Result<Vec<crate::domain::TaskId>> {
        self.remote("end_holds_of_ended_tasks", vec![])
    }
    fn released_kept_results(&self) -> Result<Vec<EventId>> {
        self.remote("released_kept_results", vec![])
    }
    fn settle_kept_result(&mut self, kept: EventId) -> Result<KeptOutcome> {
        self.remote("settle_kept_result", vec![to_value(kept)?])
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
        self.remote("silent_planners", vec![])
    }
}

impl SessionRegistry for SupervisorQueue {
    fn remove_session_workspace(&self, role: SessionRole) -> Result<bool> {
        self.remote("remove_session_workspace", vec![to_value(role)?])
    }
    fn forget_retired_session_workspaces(&self) -> Result<usize> {
        self.remote("forget_retired_session_workspaces", vec![])
    }
    fn open_planner(
        &self,
        origin: PlannerOrigin,
        proposal: Option<ProposalId>,
    ) -> Result<PlannerSession> {
        self.remote("open_planner", vec![to_value(origin)?, to_value(proposal)?])
    }
    fn planner_workspace_created(&self, id: PlannerId, workspace_id: &str) -> Result<()> {
        self.remote(
            "planner_workspace_created",
            vec![to_value(id)?, to_value(workspace_id)?],
        )
    }
    fn close_planner(&self, id: PlannerId, error: Option<&str>) -> Result<PlannerSession> {
        self.remote("close_planner", vec![to_value(id)?, to_value(error)?])
    }
    fn end_planner(&self, id: PlannerId, payload: &serde_json::Value) -> Result<bool> {
        self.remote("end_planner", vec![to_value(id)?, to_value(payload)?])
    }
    fn planner(&self, id: PlannerId) -> Result<PlannerSession> {
        self.remote("planner", vec![to_value(id)?])
    }
    fn planners(&self, all: bool) -> Result<Vec<PlannerSession>> {
        self.remote("planners", vec![to_value(all)?])
    }
    fn set_planner_route(&self, id: PlannerId, route: crate::domain::PlannerRoute) -> Result<()> {
        self.remote("set_planner_route", vec![to_value(id)?, to_value(route)?])
    }
    fn planner_turn_events(&self, id: PlannerId) -> Result<Vec<RunEvent>> {
        self.remote("planner_turn_events", vec![to_value(id)?])
    }
    fn planner_answer_wait(
        &self,
        id: PlannerId,
        asks: &[AskId],
        payload: &serde_json::Value,
    ) -> Result<bool> {
        self.remote(
            "planner_answer_wait",
            vec![to_value(id)?, to_value(asks)?, to_value(payload)?],
        )
    }
    fn planner_answer_undelivered(
        &self,
        id: PlannerId,
        ask: AskId,
        payload: &serde_json::Value,
    ) -> Result<bool> {
        self.remote(
            "planner_answer_undelivered",
            vec![to_value(id)?, to_value(ask)?, to_value(payload)?],
        )
    }
    fn planner_handover(&self, ask: AskId) -> Result<Option<crate::domain::PlannerHandover>> {
        self.remote("planner_handover", vec![to_value(ask)?])
    }
    fn register_planner_wrapper(&self, id: PlannerId, pid: u32) -> Result<()> {
        self.remote(
            "register_planner_wrapper",
            vec![to_value(id)?, to_value(pid)?],
        )
    }
    fn register_planner_agent(&self, id: PlannerId, wrapper_pid: u32, agent: u32) -> Result<()> {
        self.remote(
            "register_planner_agent",
            vec![to_value(id)?, to_value(wrapper_pid)?, to_value(agent)?],
        )
    }
    fn heartbeat_planner(&self, id: PlannerId, wrapper_pid: u32) -> Result<()> {
        self.remote(
            "heartbeat_planner",
            vec![to_value(id)?, to_value(wrapper_pid)?],
        )
    }
    fn planner_exited(&self, id: PlannerId, wrapper_pid: u32, exit_code: i32) -> Result<()> {
        self.remote(
            "planner_exited",
            vec![to_value(id)?, to_value(wrapper_pid)?, to_value(exit_code)?],
        )
    }
    fn planner_silent(&self, id: PlannerId, payload: serde_json::Value) -> Result<bool> {
        self.remote("planner_silent", vec![to_value(id)?, to_value(payload)?])
    }
    fn record_session_turns(&self) -> Result<usize> {
        self.remote("record_session_turns", vec![])
    }
    fn record_session_tokens(&self) -> Result<usize> {
        self.remote("record_session_tokens", vec![])
    }
    fn record_session_hook(
        &self,
        hook: &crate::domain::sessions::SessionHook,
    ) -> Result<serde_json::Value> {
        self.remote("record_session_hook", vec![to_value(hook)?])
    }
    fn open_hook_session_spans(&self) -> Result<Vec<crate::domain::sessions::OpenSpan>> {
        self.remote("open_hook_session_spans", vec![])
    }
    fn close_inferred_sessions(&self, ended: &[EventId]) -> Result<usize> {
        self.remote("close_inferred_sessions", vec![to_value(ended)?])
    }
    fn close_review_session(
        &self,
        id: &RunId,
        session: Option<&crate::domain::headless_job::JobSession>,
    ) -> Result<usize> {
        self.remote(
            "close_review_session",
            vec![to_value(id)?, to_value(session)?],
        )
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
        self.remote("record_headless_job", vec![to_value(job)?])
    }
    fn end_headless_job(&self, id: i64, outcome: &str) -> Result<bool> {
        self.remote("end_headless_job", vec![to_value(id)?, to_value(outcome)?])
    }
    fn orphaned_headless_jobs(
        &self,
        token: &LeaseToken,
        own: bool,
    ) -> Result<Vec<HeadlessJobRecord>> {
        self.remote(
            "orphaned_headless_jobs",
            vec![to_value(token)?, to_value(own)?],
        )
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
        self.remote(
            "open_update_ask",
            vec![
                to_value(kind)?,
                to_value(question)?,
                to_value(options)?,
                to_value(asked_by)?,
                to_value(subject)?,
                to_value(details)?,
            ],
        )
    }
    fn ask_opened_payload(&self, id: crate::domain::AskId) -> Result<serde_json::Value> {
        self.remote("ask_opened_payload", vec![to_value(id)?])
    }
    #[allow(clippy::too_many_arguments)]
    fn close_installed_update_ask(
        &mut self,
        id: crate::domain::AskId,
        answer: &str,
        installed: &str,
    ) -> Result<Option<crate::domain::Ask>> {
        self.remote(
            "close_installed_update_ask",
            vec![to_value(id)?, to_value(answer)?, to_value(installed)?],
        )
    }
    fn release_updates_on(&self) -> bool {
        self.remote("release_updates_on", vec![])
            .unwrap_or_else(|error| {
                tracing::warn!("release_updates_on through the queue service: {error:#}");
                bool::default()
            })
    }
    fn update_answers(&self, kind: &crate::domain::AskKind) -> Result<Vec<crate::domain::Ask>> {
        self.remote("update_answers", vec![to_value(kind)?])
    }
    fn asks(&self, query: AskQuery) -> Result<Vec<Ask>> {
        self.remote("asks", vec![to_value(query)?])
    }
    fn has_unclosed_ask(&self, run_id: &RunId, kind: AskKind) -> Result<bool> {
        self.remote("has_unclosed_ask", vec![to_value(run_id)?, to_value(kind)?])
    }
    fn ask(&mut self, ask: NewAsk) -> Result<AskOutcome> {
        self.remote("ask", vec![to_value(ask)?])
    }
    fn hold(&mut self, hold: crate::domain::NewHold) -> Result<crate::domain::HoldOutcome> {
        self.remote("hold", vec![to_value(hold)?])
    }
    fn hold_of(&self, run_id: &RunId) -> Result<Option<Ask>> {
        self.remote("hold_of", vec![to_value(run_id)?])
    }
    fn hold_unclosed(&self, run_id: &RunId) -> Result<bool> {
        self.remote("hold_unclosed", vec![to_value(run_id)?])
    }
    fn close_hold_asks(
        &mut self,
        reason: crate::domain::AskReason,
        subject: Option<&str>,
        answer: &str,
    ) -> Result<Vec<Ask>> {
        self.remote(
            "close_hold_asks",
            vec![to_value(reason)?, to_value(subject)?, to_value(answer)?],
        )
    }
    fn read_ask(&self, id: AskId) -> Result<Ask> {
        self.remote("read_ask", vec![to_value(id)?])
    }
    fn answer_as(
        &mut self,
        id: AskId,
        text: &str,
        answerer: crate::domain::Answerer,
    ) -> Result<Ask> {
        self.remote(
            "answer_as",
            vec![to_value(id)?, to_value(text)?, to_value(answerer)?],
        )
    }
    fn close_ask(&mut self, id: AskId) -> Result<Ask> {
        self.remote("close_ask", vec![to_value(id)?])
    }
    fn landing_answers(&self) -> Result<Vec<Ask>> {
        self.remote("landing_answers", vec![])
    }
    fn triage_answers(&self) -> Result<Vec<Ask>> {
        self.remote("triage_answers", vec![])
    }
    fn undelivered_answers(&self, run_id: &RunId) -> Result<Vec<Ask>> {
        self.remote("undelivered_answers", vec![to_value(run_id)?])
    }
    fn ask_delivered(&mut self, id: AskId, workspace_id: &str) -> Result<Ask> {
        self.remote(
            "ask_delivered",
            vec![to_value(id)?, to_value(workspace_id)?],
        )
    }
    fn has_unclosed_worker_question(&self, run_id: &RunId) -> Result<bool> {
        self.remote("has_unclosed_worker_question", vec![to_value(run_id)?])
    }
    fn has_unclosed_worker_question_since(
        &self,
        run_id: &RunId,
        created_from: i64,
    ) -> Result<bool> {
        self.remote(
            "has_unclosed_worker_question_since",
            vec![to_value(run_id)?, to_value(created_from)?],
        )
    }
    fn last_worker_question_closed(&self, run_id: &RunId) -> Result<Option<i64>> {
        self.remote("last_worker_question_closed", vec![to_value(run_id)?])
    }
    fn close_stuck_exit_asks(&mut self, run_id: &RunId, answer: &str) -> Result<Vec<Ask>> {
        self.remote(
            "close_stuck_exit_asks",
            vec![to_value(run_id)?, to_value(answer)?],
        )
    }
    fn close_answer_prompt_asks(&mut self, run_id: &RunId, answer: &str) -> Result<Vec<Ask>> {
        self.remote(
            "close_answer_prompt_asks",
            vec![to_value(run_id)?, to_value(answer)?],
        )
    }
    fn unclosed_stalled_ask(&self, run_id: &RunId) -> Result<Option<Ask>> {
        self.remote("unclosed_stalled_ask", vec![to_value(run_id)?])
    }
    fn close_stalled_asks(&mut self, run_id: &RunId, answer: &str) -> Result<Vec<Ask>> {
        self.remote(
            "close_stalled_asks",
            vec![to_value(run_id)?, to_value(answer)?],
        )
    }
    fn end_stalled_detections(&mut self, run_id: &RunId, answer: &str) -> Result<Vec<Ask>> {
        self.remote(
            "end_stalled_detections",
            vec![to_value(run_id)?, to_value(answer)?],
        )
    }
    fn note_on_asks(&mut self, run_id: &RunId, note: &str, why: &str) -> Result<Vec<Ask>> {
        self.remote(
            "note_on_asks",
            vec![to_value(run_id)?, to_value(note)?, to_value(why)?],
        )
    }
    fn close_approve_landing_asks(&mut self, run_id: &RunId, answer: &str) -> Result<Vec<Ask>> {
        self.remote(
            "close_approve_landing_asks",
            vec![to_value(run_id)?, to_value(answer)?],
        )
    }
    fn close_run_asks(&mut self, run_id: &RunId, answer: &str) -> Result<Vec<Ask>> {
        self.remote("close_run_asks", vec![to_value(run_id)?, to_value(answer)?])
    }
    fn close_ended_landing_asks(&mut self, task: Option<TaskId>) -> Result<Vec<Ask>> {
        self.remote("close_ended_landing_asks", vec![to_value(task)?])
    }
    fn close_blocked_asks(
        &mut self,
        run_id: &RunId,
        task_id: TaskId,
        answer: &str,
    ) -> Result<Vec<Ask>> {
        self.remote(
            "close_blocked_asks",
            vec![to_value(run_id)?, to_value(task_id)?, to_value(answer)?],
        )
    }
}

impl EventStore for SupervisorQueue {
    fn request_as(&self, requester: Option<&ActorContext>) -> Option<String> {
        let requested = requester.map(|actor| actor.actor_id().to_owned());
        self.requested_by.replace(requested)
    }
    fn restore_request(&self, previous: Option<String>) {
        self.requested_by.replace(previous);
    }
    fn act_as(&self, actor: ActorContext) -> Option<ActorContext> {
        self.write_as(actor)
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
    fn stats_window(&self) -> Result<crate::domain::stats::StatsWindow> {
        self.remote("stats_window", vec![])
    }
    // A page at a time, each within the answer's limit, up to the newest
    // event when the read began: the log only grows, so the pages are the
    // events the one read would have given.
    fn all_events(&self) -> Result<Vec<RunEvent>> {
        let upto = self.latest_event_id()?;
        let filter = EventFilter::default();
        let (mut events, mut after, mut page) = (Vec::new(), EventId::new(0), EVENTS_PAGE);
        loop {
            let read: Result<Vec<RunEvent>> = self.remote(
                "events_between",
                vec![
                    to_value(after)?,
                    to_value(upto)?,
                    to_value(&filter)?,
                    to_value(page)?,
                ],
            );
            let read = match read {
                Err(error) if page > 1 && super::too_large(&error) => {
                    page /= 2;
                    continue;
                }
                read => read?,
            };
            let Some(last) = read.last() else {
                return Ok(events);
            };
            after = last.id;
            let full = read.len() == page;
            events.extend(read);
            if !full {
                return Ok(events);
            }
        }
    }
    fn latest_task_events(&self, kinds: &[&str]) -> Result<Vec<RunEvent>> {
        self.remote("latest_task_events", vec![to_value(kinds)?])
    }
    // A history past one answer whole is read in pages, each after the
    // last event of the page before.
    fn run_events(&self, id: &RunId) -> Result<Vec<RunEvent>> {
        match self.remote("run_events", vec![to_value(id)?]) {
            Err(error) if super::too_large(&error) => {
                self.events_in_pages("run_events_after", to_value(id)?)
            }
            read => read,
        }
    }
    fn task_events(&self, task: TaskId) -> Result<Vec<RunEvent>> {
        match self.remote("task_events", vec![to_value(task)?]) {
            Err(error) if super::too_large(&error) => {
                self.events_in_pages("task_events_after", to_value(task)?)
            }
            read => read,
        }
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
        // Its answer is nothing: a result's record kept to be sent is
        // recorded as far as its caller goes.
        match self.remote(
            "record_runtime_event",
            vec![to_value(id)?, to_value(kind)?, to_value(payload)?],
        ) {
            Err(error) if super::resend::kept(&error) => {
                tracing::warn!("{error:#}");
                Ok(())
            }
            answered => answered,
        }
    }
    fn record_runtime_event_now(
        &self,
        id: &RunId,
        kind: EventKind,
        payload: serde_json::Value,
    ) -> Result<()> {
        self.remote_now(
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
    fn earliest_event_at(&self) -> Result<Option<i64>> {
        self.remote("earliest_event_at", vec![])
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
    fn record_queue_events(
        &self,
        events: Vec<(EventKind, serde_json::Value)>,
    ) -> Result<Vec<EventId>> {
        self.remote("record_queue_events", vec![to_value(events)?])
    }
    fn record_attributes_baseline(&self) -> Result<Option<(usize, usize)>> {
        self.remote("record_attributes_baseline", vec![])
    }
    fn record_findings_baseline(&self) -> Result<Option<usize>> {
        self.remote("record_findings_baseline", vec![])
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
    // In pages, each after the last event of the page before, a page past
    // the answer's limit asked again at half the size: the ledger's builder
    // reads thousands at a time.
    fn events_between(
        &self,
        after: EventId,
        upto: EventId,
        filter: &EventFilter,
        limit: usize,
    ) -> Result<Vec<RunEvent>> {
        let mut after = after;
        pages(limit, limit, |page| {
            let read: Vec<RunEvent> = self.remote(
                "events_between",
                vec![
                    to_value(after)?,
                    to_value(upto)?,
                    to_value(filter)?,
                    to_value(page)?,
                ],
            )?;
            if let Some(last) = read.last() {
                after = last.id;
            }
            Ok(read)
        })
    }
}

impl QueueRecords for SupervisorQueue {
    fn findings(&self, query: &crate::domain::FindingQuery) -> Result<Vec<FindingView>> {
        self.remote("findings", vec![to_value(query)?])
    }
    fn reports_written(&self) -> Result<std::collections::HashSet<(String, String)>> {
        self.remote("reports_written", vec![])
    }
    fn record_report_written(&self, payload: serde_json::Value) -> Result<bool> {
        self.remote("record_report_written", vec![to_value(payload)?])
    }
    fn kpi_breaches_open(&self) -> Result<Vec<serde_json::Value>> {
        self.remote("kpi_breaches_open", vec![])
    }
    fn record_kpi_breach(
        &self,
        kind: EventKind,
        payload: serde_json::Value,
        push_day: Option<(i64, usize)>,
    ) -> Result<Option<serde_json::Value>> {
        self.remote(
            "record_kpi_breach",
            vec![to_value(kind)?, to_value(payload)?, to_value(push_day)?],
        )
    }
    fn record_kpi_push_abandoned(&self, payload: serde_json::Value) -> Result<bool> {
        self.remote("record_kpi_push_abandoned", vec![to_value(payload)?])
    }
    fn record_forecast(
        &self,
        payload: serde_json::Value,
        previous: Option<EventId>,
    ) -> Result<Option<EventId>> {
        self.remote(
            "record_forecast",
            vec![to_value(payload)?, to_value(previous)?],
        )
    }
    fn ci_watch_events(&self) -> Result<Vec<RunEvent>> {
        self.remote("ci_watch_events", vec![])
    }
    #[allow(clippy::too_many_arguments)]
    fn record_ci_check(
        &self,
        record: crate::domain::ci_watch::CiCheckRecord,
    ) -> Result<Option<crate::domain::ci_watch::CiCheckRecorded>> {
        self.remote("record_ci_check", vec![to_value(record)?])
    }
    fn record_finding_once(
        &self,
        finding: crate::domain::NewFinding,
    ) -> Result<Option<crate::domain::FindingId>> {
        self.remote("record_finding_once", vec![to_value(finding)?])
    }
    fn ci_failure_findings_of(&self, task: TaskId) -> Result<Vec<crate::domain::FindingId>> {
        self.remote("ci_failure_findings_of", vec![to_value(task)?])
    }
    fn ci_failure_fix_tasks(&self) -> Result<std::collections::BTreeMap<i64, Vec<TaskId>>> {
        self.remote("ci_failure_fix_tasks", vec![])
    }
}

impl InboxWatchLog for SupervisorQueue {
    fn claim_inbox_nudge(&self, payload: serde_json::Value) -> Result<bool> {
        self.remote("claim_inbox_nudge", vec![to_value(payload)?])
    }
    fn record_inbox_watcher_change(
        &self,
        kind: EventKind,
        payload: serde_json::Value,
    ) -> Result<bool> {
        self.remote(
            "record_inbox_watcher_change",
            vec![to_value(kind)?, to_value(payload)?],
        )
    }
}

impl EvalRounds for SupervisorQueue {
    fn settle_eval_round(
        &self,
        eval_id: i64,
        kind: EventKind,
        payload: serde_json::Value,
    ) -> Result<bool> {
        self.remote(
            "settle_eval_round",
            vec![to_value(eval_id)?, to_value(kind)?, to_value(payload)?],
        )
    }
    // Whether an owner is alive is the service's to tell, on the same
    // host: a process other than the principal's that runs (`answer`).
    fn take_up_eval_round(
        &self,
        eval_id: i64,
        token: &str,
        _alive: &dyn Fn(u32) -> bool,
    ) -> Result<bool> {
        self.remote(
            "take_up_eval_round",
            vec![to_value(eval_id)?, to_value(token)?],
        )
    }
    fn take_case_set(
        &self,
        set_id: i64,
        token: &str,
        _alive: &dyn Fn(u32) -> bool,
    ) -> Result<bool> {
        self.remote("take_case_set", vec![to_value(set_id)?, to_value(token)?])
    }
}

impl ObserverLog for SupervisorQueue {
    fn event_id_before(&self, unix: i64) -> Result<EventId> {
        self.remote("event_id_before", vec![to_value(unix)?])
    }
    fn ask_high_water(&self) -> Result<AskId> {
        self.remote("ask_high_water", vec![])
    }
    fn written_by(&self, role: &str, event_id: EventId, ask_id: AskId) -> Result<WrittenBy> {
        self.remote(
            "written_by",
            vec![to_value(role)?, to_value(event_id)?, to_value(ask_id)?],
        )
    }
    fn last_observation(&self, mode: &str) -> Result<Option<(EventId, serde_json::Value)>> {
        self.remote("last_observation", vec![to_value(mode)?])
    }
    fn events_besides(&self, role: &str, span_kind: &str, after: EventId) -> Result<i64> {
        self.remote(
            "events_besides",
            vec![to_value(role)?, to_value(span_kind)?, to_value(after)?],
        )
    }
    fn observations(&self, limit: usize) -> Result<Vec<(RunEvent, Option<RunEvent>)>> {
        self.remote("observations", vec![to_value(limit)?])
    }
}

// The samples cross in their compact form (`super::wire::HostSampleForm`),
// and the reads in pages: each page is the store's read from where the
// page before ended, so rows that came in since are read too, as by a
// later read.
impl NodeSampleStore for SupervisorQueue {
    // The common units are made on the service's side, in the write that
    // reads the node's row before the sample.
    fn record_node_sample(
        &self,
        node: &str,
        day: i64,
        sample: &crate::domain::host_metrics::HostSample,
        resources: &crate::domain::environment::NodeRate,
        source: crate::domain::host_metrics::SampleSource,
        apart_secs: i64,
    ) -> Result<bool> {
        self.remote(
            "record_node_sample",
            vec![
                to_value(node)?,
                to_value(day)?,
                to_value(sample.clone().into_form())?,
                to_value(resources)?,
                to_value(source)?,
                to_value(apart_secs)?,
            ],
        )
    }
    // A file's samples go in parts that each fit in a request, the mark
    // with the last only.
    fn import_node_samples(
        &self,
        node: &str,
        file: &str,
        day: i64,
        samples: &[crate::domain::host_metrics::HostSample],
        bytes: u64,
    ) -> Result<usize> {
        let forms: Vec<super::wire::HostSampleForm> =
            samples.iter().cloned().map(Wire::into_form).collect();
        let parts = parts_within(&forms, SAMPLES_REQUEST_BYTES)?;
        let mark = crate::domain::host_metrics::ImportMark {
            bytes,
            rows: samples.len(),
        };
        let mut added = 0;
        for (index, part) in parts.iter().enumerate() {
            let last = index + 1 == parts.len();
            added += self.import_part(node, file, day, part, last.then_some(mark))?;
        }
        Ok(added)
    }
    fn import_node_sample_part(
        &self,
        node: &str,
        file: &str,
        day: i64,
        samples: &[crate::domain::host_metrics::HostSample],
        mark: Option<crate::domain::host_metrics::ImportMark>,
    ) -> Result<usize> {
        let forms: Vec<super::wire::HostSampleForm> =
            samples.iter().cloned().map(Wire::into_form).collect();
        self.import_part(node, file, day, &forms, mark)
    }
    fn imported_files(&self, node: &str) -> Result<std::collections::BTreeMap<String, u64>> {
        self.remote("imported_files", vec![to_value(node)?])
    }
    fn remove_node_samples(&self, before_day: i64) -> Result<usize> {
        self.remote("remove_node_samples", vec![to_value(before_day)?])
    }
    fn node_samples(
        &self,
        node: &str,
        from: i64,
        until: i64,
    ) -> Result<Vec<crate::domain::host_metrics::NodeSample>> {
        // One row per node and second, in the order they were taken: the
        // next page begins at the second after the last row's.
        let mut from = from;
        pages(SAMPLES_PAGE, usize::MAX, |page| {
            let read: Vec<crate::domain::host_metrics::NodeSample> = self.remote(
                "node_samples",
                vec![
                    to_value(node)?,
                    to_value(from)?,
                    to_value(until)?,
                    to_value(page)?,
                ],
            )?;
            if let Some(last) = read.last() {
                from = last.sample.unix.saturating_add(1);
            }
            Ok(read)
        })
    }
    fn node_samples_after(
        &self,
        after: i64,
    ) -> Result<Vec<crate::domain::host_metrics::NodeSample>> {
        let mut after = after;
        pages(SAMPLES_PAGE, usize::MAX, |page| {
            let read: Vec<crate::domain::host_metrics::NodeSample> = self.remote(
                "node_samples_after",
                vec![to_value(after)?, to_value(page)?],
            )?;
            if let Some(last) = read.last() {
                after = last.id;
            }
            Ok(read)
        })
    }
}

impl SupervisorQueue {
    /// One request of [`NodeSampleStore::import_node_sample_part`].
    fn import_part(
        &self,
        node: &str,
        file: &str,
        day: i64,
        samples: &[super::wire::HostSampleForm],
        mark: Option<crate::domain::host_metrics::ImportMark>,
    ) -> Result<usize> {
        self.remote(
            "import_node_sample_part",
            vec![
                to_value(node)?,
                to_value(file)?,
                to_value(day)?,
                to_value(samples)?,
                to_value(mark)?,
            ],
        )
    }
}

/// The rows `read` gives a page of at most the size it is given at a time
/// (`first` at first), each from where the one before ended, until a page
/// is not full or `limit` rows came: a page past the answer's limit is
/// asked again at half the size.
fn pages<T>(
    first: usize,
    limit: usize,
    mut read: impl FnMut(usize) -> Result<Vec<T>>,
) -> Result<Vec<T>> {
    let (mut rows, mut page) = (Vec::new(), first);
    while rows.len() < limit {
        let size = page.min(limit - rows.len());
        let got = match read(size) {
            Err(error) if size > 1 && super::too_large(&error) => {
                page = size / 2;
                continue;
            }
            got => got?,
        };
        let full = got.len() == size;
        rows.extend(got);
        if !full {
            break;
        }
    }
    Ok(rows)
}

// The reads in pages, each after the last window of the page before (the
// windows' ids grow in the order they were recorded): a run's history and
// the ledger's read may be longer than one answer holds.
impl RunResourceStore for SupervisorQueue {
    fn record_run_resource(&self, row: &crate::domain::environment::RunResource) -> Result<i64> {
        self.remote("record_run_resource", vec![to_value(row)?])
    }
    fn run_resources(&self, run: &RunId) -> Result<Vec<crate::domain::environment::RunResource>> {
        let mut after = 0;
        pages(RESOURCES_PAGE, usize::MAX, |page| {
            let read: Vec<crate::domain::environment::RunResource> = self.remote(
                "run_resources",
                vec![to_value(run)?, to_value(after)?, to_value(page)?],
            )?;
            if let Some(last) = read.last() {
                after = last.id;
            }
            Ok(read)
        })
    }
    fn run_resources_after(
        &self,
        after: i64,
        limit: usize,
    ) -> Result<Vec<crate::domain::environment::RunResource>> {
        let mut after = after;
        pages(RESOURCES_PAGE, limit, |page| {
            let read: Vec<crate::domain::environment::RunResource> = self.remote(
                "run_resources_after",
                vec![to_value(after)?, to_value(page)?],
            )?;
            if let Some(last) = read.last() {
                after = last.id;
            }
            Ok(read)
        })
    }
    fn open_attachments(&self) -> Result<Vec<(RunId, i64)>> {
        self.remote("open_attachments", vec![])
    }
}

impl SupervisorQueue {
    /// The events `op` reads of `of` (a run or a task), a page at a time.
    fn events_in_pages(&self, op: &str, of: serde_json::Value) -> Result<Vec<RunEvent>> {
        let mut after = EventId::new(0);
        pages(EVENTS_PAGE, usize::MAX, |page| {
            let read: Vec<RunEvent> =
                self.remote(op, vec![of.clone(), to_value(after)?, to_value(page)?])?;
            if let Some(last) = read.last() {
                after = last.id;
            }
            Ok(read)
        })
    }
}

/// How many ledger rows a read asks for at a time, halved for a page past
/// the answer's limit: a run's row holds its events.
const LEDGER_ROWS_PAGE: usize = 100;

/// How many bytes of a ledger row one write sends whole at most, half the
/// request's limit; a longer one crosses in parts.
const LEDGER_ROW_REQUEST_BYTES: usize = crate::domain::queue_service::MAX_REQUEST_BYTES / 2;

/// How many session steps or samples a read asks for at a time, halved for
/// a page past the answer's limit.
const STEPS_PAGE: usize = 10_000;

/// Why the supervisor's side does not send `op`: no one of the supervisor
/// sends it (what builds the ledger only reads the steps, the samples and
/// the owners, and writes each row over the one it read), so the service
/// has no such operation of the supervisor's, and it is refused here, not
/// done on the DB (fail closed).
fn not_sent(op: &str) -> anyhow::Error {
    anyhow::anyhow!(
        "the supervisor does not {op} through the queue service: it is no operation of its use case"
    )
}

// What builds the ledger reads and writes: the reads in pages, each after
// the last of the page before, and a write over a row read naming that row
// by its fingerprint, so a request holds one row.
impl LedgerStore for SupervisorQueue {
    fn write_ledger_row(&self, _: &crate::domain::ledger::LedgerRow) -> Result<bool> {
        Err(not_sent("write_ledger_row"))
    }
    // A row too long for a request whole crosses in parts
    // ([`super::large`]).
    fn write_ledger_row_over(
        &self,
        row: &crate::domain::ledger::LedgerRow,
        read: Option<&crate::domain::ledger::LedgerRow>,
    ) -> Result<Option<bool>> {
        let read = to_value(read.map(crate::domain::ledger::LedgerRow::fingerprint))?;
        let text = serde_json::to_string(row)?;
        if text.len() <= LEDGER_ROW_REQUEST_BYTES {
            return self.remote("write_ledger_row_over", vec![to_value(row)?, read]);
        }
        let digest = super::large::digest(&text);
        let parts = super::large::split(&text, super::large::WRITE_PART_BYTES);
        for (index, part) in parts.iter().enumerate() {
            let () = self.remote(
                "stage_ledger_row",
                vec![
                    to_value(&digest)?,
                    to_value(index)?,
                    to_value(parts.len())?,
                    to_value(part)?,
                ],
            )?;
        }
        self.remote(
            "write_staged_ledger_row_over",
            vec![to_value(&digest)?, read],
        )
    }
    // A row past one answer whole is read in parts.
    fn ledger_row(
        &self,
        kind: crate::domain::ledger::LedgerKind,
        key: &str,
    ) -> Result<Option<crate::domain::ledger::LedgerRow>> {
        match self.remote("ledger_row", vec![to_value(kind)?, to_value(key)?]) {
            Err(error) if super::too_large(&error) => self.ledger_row_in_parts(kind, key),
            read => read,
        }
    }
    // In pages after the last key of the page before, a page past the
    // answer's limit asked again at a quarter of its size, and a row past
    // it alone read in parts (the pages after it of the first size again).
    fn ledger_rows(
        &self,
        kind: crate::domain::ledger::LedgerKind,
    ) -> Result<Vec<crate::domain::ledger::LedgerRow>> {
        let (mut rows, mut after, mut page) = (Vec::new(), None::<String>, LEDGER_ROWS_PAGE);
        loop {
            let read: Vec<crate::domain::ledger::LedgerRow> = match self.remote(
                "ledger_rows",
                vec![to_value(kind)?, to_value(&after)?, to_value(page)?],
            ) {
                Err(error) if super::too_large(&error) && page > 1 => {
                    page = (page / 4).max(1);
                    continue;
                }
                Err(error) if super::too_large(&error) => {
                    let keys: Vec<String> = self.remote(
                        "ledger_row_keys",
                        vec![to_value(kind)?, to_value(&after)?, to_value(1)?],
                    )?;
                    let Some(key) = keys.into_iter().next() else {
                        return Ok(rows);
                    };
                    rows.extend(self.ledger_row_in_parts(kind, &key)?);
                    after = Some(key);
                    page = LEDGER_ROWS_PAGE;
                    continue;
                }
                read => read?,
            };
            let full = read.len() == page;
            if let Some(last) = read.last() {
                after = Some(last.key.clone());
            }
            rows.extend(read);
            if !full {
                return Ok(rows);
            }
        }
    }
    fn remove_node_rows(&self, before_day: i64) -> Result<usize> {
        self.remote("remove_node_rows", vec![to_value(before_day)?])
    }
}

/// How many times a read of a row in parts starts again while another
/// writer writes the row between its parts.
const ROW_PART_TRIES: usize = 3;

impl SupervisorQueue {
    /// The row of `kind` and `key` read a part of its text at a time
    /// ([`super::large`]), all of the same row (another writer's row
    /// between them starts the read again); `None` for no row.
    fn ledger_row_in_parts(
        &self,
        kind: crate::domain::ledger::LedgerKind,
        key: &str,
    ) -> Result<Option<crate::domain::ledger::LedgerRow>> {
        'read: for _ in 0..ROW_PART_TRIES {
            let (mut text, mut fingerprint, mut offset) = (String::new(), None::<String>, 0);
            loop {
                let part: Option<super::large::RowPart> = self.remote(
                    "ledger_row_part",
                    vec![
                        to_value(kind)?,
                        to_value(key)?,
                        to_value(offset)?,
                        to_value(super::large::READ_PART_BYTES)?,
                    ],
                )?;
                let Some(part) = part else {
                    return Ok(None);
                };
                if fingerprint
                    .as_ref()
                    .is_some_and(|read| *read != part.fingerprint)
                {
                    continue 'read;
                }
                anyhow::ensure!(
                    part.next > offset || part.next == part.total,
                    "the queue service's part of the row {key} does not go on"
                );
                text.push_str(&part.part);
                offset = part.next;
                let done = offset >= part.total;
                fingerprint = Some(part.fingerprint);
                if done {
                    break;
                }
            }
            anyhow::ensure!(
                fingerprint.as_deref() == Some(super::large::digest(&text).as_str()),
                "the parts of the row {key} are not of its fingerprint"
            );
            return Ok(Some(serde_json::from_str(&text)?));
        }
        anyhow::bail!("the row {key} was written again while it was read, {ROW_PART_TRIES} times")
    }
}

impl SessionStepStore for SupervisorQueue {
    fn record_steps(
        &self,
        _: &crate::domain::session_step::StepTarget,
        _: &str,
        _: &[crate::domain::session_step::Step],
        _: i64,
    ) -> Result<crate::domain::session_step::StepsRecorded> {
        Err(not_sent("record_steps"))
    }
    fn session_gaps(&self, _: &str) -> Result<Vec<crate::domain::session_step::SeqRange>> {
        Err(not_sent("session_gaps"))
    }
    fn steps_after(
        &self,
        after: i64,
        limit: usize,
    ) -> Result<Vec<crate::domain::session_step::SessionStep>> {
        let mut after = after;
        pages(STEPS_PAGE, limit, |page| {
            let read: Vec<crate::domain::session_step::SessionStep> =
                self.remote("steps_after", vec![to_value(after)?, to_value(page)?])?;
            if let Some(last) = read.last() {
                after = last.id;
            }
            Ok(read)
        })
    }
    fn target_steps(
        &self,
        target: &crate::domain::session_step::StepTarget,
    ) -> Result<Vec<crate::domain::session_step::SessionStep>> {
        let mut after = 0;
        pages(STEPS_PAGE, usize::MAX, |page| {
            let read: Vec<crate::domain::session_step::SessionStep> = self.remote(
                "target_steps",
                vec![to_value(target)?, to_value(after)?, to_value(page)?],
            )?;
            if let Some(last) = read.last() {
                after = last.id;
            }
            Ok(read)
        })
    }
    fn remove_steps(&self, before: i64) -> Result<usize> {
        self.remote("remove_steps", vec![to_value(before)?])
    }
}

impl SessionResourceStore for SupervisorQueue {
    fn record_resources(
        &self,
        _: &crate::domain::session_step::StepTarget,
        _: &str,
        _: &[crate::domain::session_step::ResourceSample],
        _: i64,
    ) -> Result<crate::domain::session_step::StepsRecorded> {
        Err(not_sent("record_resources"))
    }
    fn target_resources(
        &self,
        target: &crate::domain::session_step::StepTarget,
    ) -> Result<Vec<crate::domain::session_step::SessionResource>> {
        let mut after = 0;
        pages(STEPS_PAGE, usize::MAX, |page| {
            let read: Vec<crate::domain::session_step::SessionResource> = self.remote(
                "target_resources",
                vec![to_value(target)?, to_value(after)?, to_value(page)?],
            )?;
            if let Some(last) = read.last() {
                after = last.id;
            }
            Ok(read)
        })
    }
    fn resources_after(
        &self,
        after: i64,
        limit: usize,
    ) -> Result<Vec<crate::domain::session_step::SessionResource>> {
        let mut after = after;
        pages(STEPS_PAGE, limit, |page| {
            let read: Vec<crate::domain::session_step::SessionResource> =
                self.remote("resources_after", vec![to_value(after)?, to_value(page)?])?;
            if let Some(last) = read.last() {
                after = last.id;
            }
            Ok(read)
        })
    }
}

impl SessionOwners for SupervisorQueue {
    fn register_session(
        &self,
        _: &crate::domain::session_step::StepTarget,
        _: &str,
        _: i64,
    ) -> Result<()> {
        Err(not_sent("register_session"))
    }
    fn session_owner(
        &self,
        session_id: &str,
    ) -> Result<Option<crate::domain::session_step::SessionOwner>> {
        self.remote("session_owner", vec![to_value(session_id)?])
    }
}
