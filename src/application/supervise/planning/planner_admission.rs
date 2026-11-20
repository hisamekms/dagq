//! Admit new runtime planners through the shared slots (ADR-t5583-1).
use super::*;
use crate::{
    application::{GraphInput, RevisingProposal, dependency_graph, effective_priority},
    domain::{
        Ask, DraftTarget, Finding, FindingId, GoalId, Priority, ProposalId, RequestId, TaskId,
        event_kind::PlannerStartBlocked,
        follow_up::bundles,
        planner_admission::{Candidate, Kind, order},
        planner_turns,
        stats::timestamp_millis,
    },
};
use std::collections::BTreeMap;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum AnswerTarget {
    Request(RequestId),
    Finding(FindingId),
    Draft(TaskId),
}

enum Work {
    Revise(Box<RevisingProposal>),
    Answer(Box<Ask>, AnswerTarget),
    Request(RequestId),
    Draft(Vec<TaskId>),
    Finding(FindingId),
}

impl PlanningState {
    /// Try new work in effective priority, kind and wait-age order
    /// (ADR-t5583-1 decision 6). A start that takes no slot after checking
    /// eligibility leaves room for the next candidate in the same pass.
    /// Keep the existing proposal-ID order for the overdue revise recovery.
    pub(super) fn admit_planners(
        &mut self,
        env: &mut PlanningEnv<'_>,
        runtime_open: usize,
        revises: Vec<(RevisingProposal, Option<i64>)>,
        answers: Vec<(Ask, AnswerTarget)>,
        overdue_before: i64,
        blocked: &mut PlannerStartBlocked,
    ) -> Result<(bool, Option<ProposalId>)> {
        let mut overdue: BTreeSet<_> = revises
            .iter()
            .filter(|(_, since)| since.is_some_and(|since| since < overdue_before))
            .map(|(revise, _)| revise.proposal.id())
            .collect();
        let mut free = env.runtime_planners.saturating_sub(runtime_open);
        if free == 0 {
            return Ok((false, overdue.first().copied()));
        }
        let candidates = planner_candidates(env, revises, answers)?;
        let mut opened_revise = false;
        let mut improvement_limit = None;
        for work in order(free, candidates) {
            let opened = match work {
                Work::Revise(revise) => {
                    overdue.remove(&revise.proposal.id());
                    let opened = self.start_unowned_revise(env, &revise, blocked)?;
                    opened_revise |= opened;
                    opened
                }
                Work::Answer(ask, target) => {
                    self.start_answer_planner(env, &ask, target, blocked)?
                }
                Work::Request(id) => self
                    .start_request_planner(env, id, None, blocked)?
                    .is_some(),
                Work::Draft(tasks) => self.start_draft_planner(env, &tasks, None)?.is_some(),
                Work::Finding(id) => {
                    let limit =
                        *improvement_limit.get_or_insert_with(|| self.max_improvement_proposals());
                    self.start_marked_finding(env, id, limit)?
                }
            };
            if opened {
                free -= 1;
                if free == 0 {
                    break;
                }
            }
        }
        let starved = overdue.first().copied().filter(|_| free == 0);
        Ok((opened_revise, starved))
    }

    fn start_answer_planner(
        &mut self,
        env: &mut PlanningEnv<'_>,
        ask: &Ask,
        target: AnswerTarget,
        blocked: &mut PlannerStartBlocked,
    ) -> Result<bool> {
        let workspace = match target {
            AnswerTarget::Request(request) => {
                self.start_request_planner(env, request, Some(ask), blocked)?
            }
            AnswerTarget::Finding(finding) => {
                self.start_finding_planner(env, finding, Some(ask), 0)?
            }
            AnswerTarget::Draft(task) => self.start_draft_answer(env, task, ask)?,
        };
        if let Some(workspace) = workspace {
            env.queue.ask_delivered(ask.id, &workspace)?;
            return Ok(true);
        }
        Ok(false)
    }
}

fn planner_candidates(
    env: &mut PlanningEnv<'_>,
    revises: Vec<(RevisingProposal, Option<i64>)>,
    answers: Vec<(Ask, AnswerTarget)>,
) -> Result<Vec<Candidate<Work>>> {
    let drafts = env.queue.planner_drafts()?;
    let requests = env.queue.planner_requests()?;
    let findings = planner_turns(env.queue.planner_findings()?);
    let needs_graph = !revises.is_empty()
        || !answers.is_empty()
        || !drafts.is_empty()
        || !findings.before_drafts.is_empty()
        || !findings.improvements.is_empty();
    let priorities = Priorities::new(if needs_graph {
        env.queue.graph_input()?
    } else {
        GraphInput::default()
    });
    let mut candidates = Vec::new();
    for (revise, since) in revises {
        candidates.push(Candidate {
            kind: Kind::Revise,
            priority: priorities.proposal(revise.proposal.task_ids(), revise.proposal.goal_ids()),
            waiting_since: since.map_or_else(
                || timestamp_millis(revise.proposal.updated_at()).unwrap_or_default(),
                milliseconds,
            ),
            work: Work::Revise(Box::new(revise)),
        });
    }
    let mut finding_priorities = FindingPriorities::new(&priorities);
    for (ask, target) in answers {
        let priority = match answer_priority_target(&ask, target) {
            AnswerTarget::Finding(id) => {
                let finding = env.queue.finding_view(id)?.finding;
                finding_priorities.priority_of(
                    env,
                    &finding,
                    finding_kind(&finding.kind, &finding.recorded_by),
                )?
            }
            AnswerTarget::Request(id) => request_priority(env.queue.plan_request(id)?.priority),
            AnswerTarget::Draft(task) => priorities.task(task),
        };
        candidates.push(Candidate {
            kind: Kind::Answer,
            priority,
            waiting_since: milliseconds(ask.answered_at.unwrap_or(ask.created_at)),
            work: Work::Answer(Box::new(ask), target),
        });
    }
    for request in requests {
        candidates.push(Candidate {
            kind: Kind::Request,
            priority: request_priority(request.priority),
            waiting_since: milliseconds(request.created_at),
            work: Work::Request(request.id),
        });
    }
    for bundle in bundles(drafts) {
        let work: Vec<_> = bundle.iter().map(|target| target.task.id()).collect();
        candidates.push(Candidate {
            kind: Kind::Draft,
            priority: priorities.tasks(work.iter().copied()),
            waiting_since: draft_bundle_wait_start(env, &bundle)?,
            work: Work::Draft(work),
        });
    }
    for (finding, kind) in findings
        .before_drafts
        .into_iter()
        .map(|finding| {
            let kind = exempt_finding_kind(&finding.kind);
            (finding, kind)
        })
        .chain(
            findings
                .improvements
                .into_iter()
                .map(|finding| (finding, Kind::Improvement)),
        )
    {
        candidates.push(Candidate {
            kind,
            priority: finding_priorities.priority_of(env, &finding, kind)?,
            waiting_since: milliseconds(
                finding
                    .propose_requested_at
                    .unwrap_or(finding.first_seen_at),
            ),
            work: Work::Finding(finding.id),
        });
    }

    Ok(candidates)
}

fn milliseconds(seconds: i64) -> i64 {
    seconds.saturating_mul(1000)
}

fn draft_bundle_wait_start(env: &PlanningEnv<'_>, bundle: &[DraftTarget]) -> Result<i64> {
    let mut oldest = i64::MAX;
    for target in bundle {
        let since = if let Some(revisit) = &target.revisit {
            milliseconds(revisit.revisit_at)
        } else if target.origin == crate::domain::DraftOrigin::Reopened
            && let Some(proposal) = target.material["proposal_id"].as_i64()
        {
            // Withdrawal makes the reopened draft eligible. Later edits
            // to the task must not reset its wait.
            let withdrawn = env.queue.show_proposal(ProposalId::new(proposal))?;
            timestamp_millis(withdrawn.updated_at()).unwrap_or_default()
        } else {
            timestamp_millis(target.task.created_at()).unwrap_or_default()
        };
        oldest = oldest.min(since);
    }
    Ok(oldest)
}

fn answer_priority_target(ask: &Ask, route: AnswerTarget) -> AnswerTarget {
    // A request planner can carry a draft's answer. The draft is still
    // the subject of that ask.
    match (route, ask.request_id, ask.task_id) {
        (AnswerTarget::Request(_), None, Some(task)) => AnswerTarget::Draft(task),
        _ => route,
    }
}

fn request_priority(priority: Option<Priority>) -> Priority {
    priority.unwrap_or_default()
}

fn exempt_finding_kind(kind: &str) -> Kind {
    if kind == crate::domain::ci_watch::FINDING_KIND {
        Kind::CiRepair
    } else {
        Kind::AgentWatch
    }
}

fn finding_kind(kind: &str, recorded_by: &str) -> Kind {
    if crate::domain::counts_as_improvement(kind, recorded_by) {
        Kind::Improvement
    } else {
        exempt_finding_kind(kind)
    }
}

struct Priorities {
    input: GraphInput,
    nodes: BTreeMap<TaskId, crate::application::GraphNode>,
}

impl Priorities {
    fn new(input: GraphInput) -> Self {
        let nodes = dependency_graph(input.clone(), None)
            .tasks
            .into_iter()
            .map(|node| (node.id, node))
            .collect();
        Self { input, nodes }
    }

    fn task(&self, id: TaskId) -> Priority {
        self.nodes
            .get(&id)
            .map_or(Priority::Normal, |node| node.effective_priority)
    }

    fn tasks(&self, ids: impl IntoIterator<Item = TaskId>) -> Priority {
        ids.into_iter()
            .filter_map(|id| self.nodes.get(&id))
            .map(|node| node.effective_priority)
            .max()
            .unwrap_or_default()
    }

    fn proposal(&self, tasks: &[TaskId], goals: &[GoalId]) -> Priority {
        // A proposal's work includes the current members of its goals.
        let members = self
            .input
            .tasks
            .iter()
            .filter(|task| task.goal_id.is_some_and(|goal| goals.contains(&goal)));
        self.tasks(tasks.iter().copied().chain(members.map(|task| task.id)))
    }

    fn goal(&self, id: GoalId, own: Priority) -> Priority {
        // Goal waiters include tasks depending on the goal itself and the
        // same transitive waiters used by worker admission.
        let mut reached = BTreeSet::new();
        let mut pending: Vec<_> = self
            .input
            .tasks
            .iter()
            .filter(|task| {
                task.goal_dependencies
                    .iter()
                    .any(|dependency| dependency.goal_id == id && dependency.verdict.is_none())
            })
            .map(|task| task.id)
            .collect();
        while let Some(id) = pending.pop() {
            if reached.insert(id)
                && let Some(node) = self.nodes.get(&id)
            {
                pending.extend_from_slice(&node.blocks);
            }
        }
        effective_priority(
            own,
            self.input
                .tasks
                .iter()
                .filter(|task| reached.contains(&task.id)),
        )
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FindingPrioritySource {
    LandingQueue,
    Goal(GoalId),
    TaskGoal(TaskId),
    Default,
}

fn finding_priority_source(
    kind: Kind,
    goal_id: Option<GoalId>,
    task_id: Option<TaskId>,
) -> FindingPrioritySource {
    match (kind, goal_id, task_id) {
        (Kind::CiRepair, _, _) => FindingPrioritySource::LandingQueue,
        (_, Some(goal), _) => FindingPrioritySource::Goal(goal),
        (_, None, Some(task)) => FindingPrioritySource::TaskGoal(task),
        _ => FindingPrioritySource::Default,
    }
}

struct FindingPriorities<'a> {
    priorities: &'a Priorities,
    landing: Option<Priority>,
    goals: BTreeMap<GoalId, Priority>,
}

impl<'a> FindingPriorities<'a> {
    fn new(priorities: &'a Priorities) -> Self {
        Self {
            priorities,
            landing: None,
            goals: BTreeMap::new(),
        }
    }

    /// A CI repair uses the highest effective priority of the runs
    /// waiting to land. A watch step or improvement uses its linked goal's
    /// effective priority. Without such a run or goal, use normal priority.
    fn priority_of(
        &mut self,
        env: &mut PlanningEnv<'_>,
        finding: &Finding,
        kind: Kind,
    ) -> Result<Priority> {
        let goal = match finding_priority_source(kind, finding.goal_id, finding.task_id) {
            FindingPrioritySource::LandingQueue => {
                if let Some(priority) = self.landing {
                    return Ok(priority);
                }
                let waiting = crate::application::integrate::landing_candidates(&*env.queue)?;
                let priority = self
                    .priorities
                    .tasks(waiting.iter().map(|(run, _)| run.task_id()));
                self.landing = Some(priority);
                return Ok(priority);
            }
            FindingPrioritySource::Goal(goal) => Some(goal),
            FindingPrioritySource::TaskGoal(task) => env.queue.show(task)?.task.goal_id(),
            FindingPrioritySource::Default => None,
        };
        let Some(goal) = goal else {
            return Ok(Priority::Normal);
        };
        if let Some(&priority) = self.goals.get(&goal) {
            return Ok(priority);
        }
        let own = env.queue.show_goal(goal)?.goal.priority();
        let priority = self.priorities.goal(goal, own);
        self.goals.insert(goal, priority);
        Ok(priority)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        application::{GraphGoalDependency, GraphTask},
        domain::{GoalStatus, PrioritySource, TaskStatus, plan_request::PriorityBy},
    };

    #[test]
    fn revisions_draft_bundles_and_repairs_use_the_maximum_effective_task_priority() {
        let low = TaskId::new(1);
        let inherited = TaskId::new(2);
        let waiter = TaskId::new(3);
        let goal = GoalId::new(1);
        let task = |id: TaskId, status, priority, goal_id: Option<GoalId>, depends_on| GraphTask {
            id,
            status,
            priority,
            goal_id,
            depends_on,
            priority_source: PrioritySource::Default,
            priority_by: PriorityBy::Ai,
            execution_class: Default::default(),
            title: id.to_string(),
            goal_status: goal_id.map(|_| GoalStatus::Open),
            goal_dependencies: vec![],
        };
        let priorities = Priorities::new(GraphInput {
            tasks: vec![
                task(low, TaskStatus::Draft, Priority::Low, None, vec![]),
                task(
                    inherited,
                    TaskStatus::Draft,
                    Priority::Normal,
                    Some(goal),
                    vec![],
                ),
                task(
                    waiter,
                    TaskStatus::Ready,
                    Priority::Interrupt,
                    None,
                    vec![inherited],
                ),
            ],
            ..GraphInput::default()
        });
        // The revise includes current goal members beyond its explicit tasks.
        assert_eq!(priorities.proposal(&[low], &[goal]), Priority::Interrupt);
        assert_eq!(priorities.proposal(&[low], &[]), Priority::Low);
        // A draft bundle and the runs waiting for CI take the maximum over
        // their respective task IDs. An empty landing queue falls back.
        assert_eq!(priorities.tasks([low, inherited]), Priority::Interrupt);
        assert_eq!(priorities.tasks([low]), Priority::Low);
        assert_eq!(priorities.tasks([]), Priority::Normal);
    }

    #[test]
    fn answers_take_the_priority_source_of_the_ask_and_requests_default_to_normal() {
        let request = RequestId::new(1);
        let draft = TaskId::new(2);
        let finding = FindingId::new(3);
        let ask = |task, request, finding| {
            serde_json::from_value::<Ask>(json!({
                "id": 1, "kind": "planner_question", "task_id": task, "run_id": null,
                "question": "Keep the draft?", "options": [], "answer": "yes", "asked_by": "planner",
                "reason_category": "scope", "created_at": 1, "answered_at": 2, "closed_at": null,
                "request_id": request, "finding_id": finding
            })).unwrap()
        };
        assert_eq!(
            answer_priority_target(
                &ask(Some(draft), None, None),
                AnswerTarget::Request(request)
            ),
            AnswerTarget::Draft(draft)
        );
        assert_eq!(
            answer_priority_target(
                &ask(None, Some(request), None),
                AnswerTarget::Request(request)
            ),
            AnswerTarget::Request(request)
        );
        assert_eq!(
            answer_priority_target(
                &ask(None, None, Some(finding)),
                AnswerTarget::Finding(finding)
            ),
            AnswerTarget::Finding(finding)
        );
        assert_eq!(request_priority(Some(Priority::Urgent)), Priority::Urgent);
        assert_eq!(request_priority(None), Priority::Normal);
    }

    #[test]
    fn finding_priorities_follow_landing_runs_or_the_linked_goal_and_its_ready_waiters() {
        let goal = GoalId::new(1);
        let task = TaskId::new(1);
        let ci = crate::domain::ci_watch::FINDING_KIND;
        let watch = crate::domain::agent_eval::watch::WATCH_FINDING_KIND;
        let source =
            |kind, by, goal, task| finding_priority_source(finding_kind(kind, by), goal, task);
        assert_eq!(
            source(ci, "supervisor", Some(goal), Some(task)),
            FindingPrioritySource::LandingQueue
        );
        assert_eq!(
            source(ci, "observer", Some(goal), None),
            FindingPrioritySource::Goal(goal)
        );
        assert_eq!(
            source(watch, "supervisor", Some(goal), None),
            FindingPrioritySource::Goal(goal)
        );
        assert_eq!(
            source("stall", "observer", None, Some(task)),
            FindingPrioritySource::TaskGoal(task)
        );
        assert_eq!(
            source(watch, "supervisor", None, None),
            FindingPrioritySource::Default
        );

        let direct = GraphTask {
            id: task,
            status: TaskStatus::Draft,
            priority: Priority::Interrupt,
            priority_source: PrioritySource::Default,
            priority_by: PriorityBy::Ai,
            execution_class: Default::default(),
            title: "A draft that waits for the goal".into(),
            goal_id: None,
            goal_status: None,
            depends_on: vec![],
            goal_dependencies: vec![GraphGoalDependency {
                goal_id: goal,
                verdict: None,
            }],
        };
        let downstream = GraphTask {
            id: TaskId::new(2),
            status: TaskStatus::Ready,
            priority: Priority::Urgent,
            depends_on: vec![direct.id],
            goal_dependencies: vec![],
            ..direct.clone()
        };
        let priorities = Priorities::new(GraphInput {
            tasks: vec![direct, downstream],
            ..GraphInput::default()
        });
        assert_eq!(priorities.goal(goal, Priority::High), Priority::Urgent);
        assert_eq!(
            priorities.goal(GoalId::new(2), Priority::Low),
            Priority::Low
        );
    }
}
