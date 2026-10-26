//! The trusted runtime's atomic apply of a replacement plan plan review
//! found applicable (ADR-t1521-2 decision 5,
//! docs/plans/task-replanning.md, "trusted runtimeの原子的適用と依存"):
//! what it checks again right before the apply, all in the apply's own
//! transaction ([`apply_problems`]), and what the apply writes of the
//! dependencies ([`successor_edges`]) and of the saved work each child
//! takes ([`ChildCarry`]). Any problem refuses the whole apply: nothing of
//! it is written, and the operation is held `blocked` with the work kept,
//! then goes on as each problem says ([`ApplyRecovery`]).

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

use super::{
    GoalId, ProposalId, ProposalStatus, ReplanId, RunId, RunStatus, TaskId, TaskStatus,
    replan_plan::{CarrySource, PlanProblem, PlanProblemCode, PlanRevision, ReplacementPlan},
    task_replan::{AcceptanceItem, ConditionMapping, ReplanOperation, ReplanStatus},
};

/// Which children each task that waits for the replaced task waits for in
/// its place once the apply re-points its dependency: the children the
/// plan names for a successor it lists, and every child for one it does
/// not list (a task that came to wait for the replaced task after the
/// operation fixed its conditions), since the replaced task stood for all
/// of its conditions. `successors` is every task that waits for the
/// replaced task now; the result keeps its order.
pub fn successor_edges(
    plan: &ReplacementPlan,
    successors: &[TaskId],
) -> Vec<(TaskId, Vec<TaskId>)> {
    successors
        .iter()
        .map(|&successor| {
            let needs = plan
                .successors
                .iter()
                .find(|listed| listed.task_id == successor)
                .map_or_else(|| plan.child_ids(), |listed| listed.needs.clone());
            (successor, needs)
        })
        .collect()
}

/// A vertex of the wait graph (ADR-0038): a task waits for its
/// predecessors and for the goals it depends on, and a goal waits for its
/// tasks.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum WaitNode {
    Task(TaskId),
    Goal(GoalId),
}

/// The successor that would wait for itself once `edges` (a task and the
/// children it then waits for) join the wait graph `waits` (each vertex
/// and what it waits for directly, the edges onto the replaced task taken
/// out), through tasks and goals alike; `None` when no edge closes a loop.
pub fn closes_loop(
    waits: &BTreeMap<WaitNode, Vec<WaitNode>>,
    edges: &[(TaskId, Vec<TaskId>)],
) -> Option<TaskId> {
    let mut graph = waits.clone();
    for (task, needs) in edges {
        let entry = graph.entry(WaitNode::Task(*task)).or_default();
        for need in needs {
            if !entry.contains(&WaitNode::Task(*need)) {
                entry.push(WaitNode::Task(*need));
            }
        }
    }
    edges.iter().map(|(task, _)| *task).find(|&task| {
        let start = WaitNode::Task(task);
        let mut stack: Vec<WaitNode> = graph.get(&start).cloned().unwrap_or_default();
        let mut seen = BTreeSet::new();
        while let Some(next) = stack.pop() {
            if next == start {
                return true;
            }
            if seen.insert(next) {
                stack.extend(graph.get(&next).cloned().unwrap_or_default());
            }
        }
        false
    })
}

/// What the apply reads of the queue, in its own transaction.
#[derive(Debug, Clone)]
pub struct ApplyState<'a> {
    pub operation: &'a ReplanOperation,
    /// The generation the runtime read the operation under.
    pub generation: i64,
    /// The operation's latest plan.
    pub plan: Option<&'a PlanRevision>,
    /// The status of the proposal the latest plan went with.
    pub proposal: Option<ProposalStatus>,
    pub task: TaskStatus,
    /// The parked run's status and whether a process holds its lease.
    pub run: RunStatus,
    pub run_leased: bool,
    /// The children's statuses now.
    pub children: BTreeMap<TaskId, TaskStatus>,
    /// The version of each child's conditions now, and as plan review
    /// approved them with the latest plan (`None`: none recorded).
    pub child_versions: BTreeMap<TaskId, String>,
    pub approved_children: Option<BTreeMap<TaskId, String>>,
    /// Every task that waits for the replaced task now and has not ended,
    /// with its status: an ended one waits for nothing, and its edge is
    /// left as it is.
    pub successors: BTreeMap<TaskId, TaskStatus>,
    /// The problems the check of the latest plan finds now
    /// ([`super::replan_plan::check_plan`], and a loop through the whole
    /// wait graph once the successors wait for the children,
    /// [`closes_loop`]).
    pub plan_problems: Vec<PlanProblem>,
}

/// What becomes of an operation held `blocked` by a refused apply
/// (ADR-t1521-2 decision 2: the saved work is kept and the operation goes
/// on to a fix, a continuation or a withdrawal), by what refused it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ApplyRecovery {
    /// The queue may still change (the parked run's lease, a write that
    /// failed, a move under another generation): the runtime checks again
    /// at each pass and applies once nothing refuses it.
    Retry,
    /// The approved plan no longer holds (its approval, a version, a
    /// child, a successor that started, a loop): the operation goes back
    /// to a planner with its saved work, the children released, on a new
    /// request, and the planner makes a new plan, continues the run or
    /// withdraws.
    Replan,
    /// The saved work is not as the park saved it: held for a person, since
    /// whether to go on without it is theirs to decide.
    Person,
}

/// One reason an apply cannot go now, and what it calls for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApplyProblem {
    pub recovery: ApplyRecovery,
    pub reason: String,
}

impl ApplyProblem {
    pub fn new(recovery: ApplyRecovery, reason: impl Into<String>) -> Self {
        Self {
            recovery,
            reason: reason.into(),
        }
    }
}

/// What a refusal for `problems` calls for: a person before a new plan,
/// and a new plan before another check.
pub fn recovery_of(problems: &[ApplyProblem]) -> Option<ApplyRecovery> {
    problems.iter().map(|problem| problem.recovery).max()
}

/// Why the apply of `state.operation` cannot go now; none means it
/// applies. The operation is the runtime's to apply only under the
/// generation it was read with, in `reviewing` or `applying` (or `blocked`
/// from `applying`, checked again), with a latest plan a pass approved
/// whose proposal is the operation's and accepted, and whose check finds
/// nothing now (the versions of the task's and the goal's conditions, the
/// deletions' authority, the children and the dependencies). The task is
/// still in progress with its parked run at rest and unleased, no child
/// has started or changed since plan review approved it (its conditions'
/// version), and no task that waits for the task has started.
pub fn apply_problems(state: &ApplyState<'_>) -> Vec<ApplyProblem> {
    use ApplyRecovery::{Replan, Retry};
    let op = state.operation;
    let mut problems = Vec::new();
    let mut push = |recovery, reason: String| problems.push(ApplyProblem::new(recovery, reason));
    if op.generation != state.generation {
        push(
            Retry,
            format!(
                "replan operation {} is at generation {}, read at {}",
                op.id, op.generation, state.generation
            ),
        );
    }
    let waiting = matches!(op.status, ReplanStatus::Reviewing | ReplanStatus::Applying)
        || (op.status == ReplanStatus::Blocked && op.return_phase == Some(ReplanStatus::Applying));
    if !waiting {
        push(
            Retry,
            format!(
                "replan operation {} is {}, not waiting for its apply",
                op.id,
                op.status.as_str()
            ),
        );
    }
    match state.plan {
        None => push(Replan, format!("replan operation {} has no plan", op.id)),
        Some(plan) => {
            if plan.approved_at.is_none() {
                push(
                    Replan,
                    format!(
                        "plan revision {} of replan operation {} was not approved by plan review",
                        plan.revision, op.id
                    ),
                );
            }
            if op.proposal_id != Some(plan.proposal_id) {
                push(
                    Replan,
                    format!(
                        "plan revision {} goes with proposal {}, the operation names {:?}",
                        plan.revision, plan.proposal_id, op.proposal_id
                    ),
                );
            }
            if state.proposal != Some(ProposalStatus::Accepted) {
                push(
                    Replan,
                    format!(
                        "proposal {} is {}, not accepted",
                        plan.proposal_id,
                        state.proposal.map_or("missing", ProposalStatus::as_str)
                    ),
                );
            }
        }
    }
    // The operation leaves its planner once the runtime applies it.
    for problem in state
        .plan_problems
        .iter()
        .filter(|problem| problem.code != PlanProblemCode::NotWithPlanner)
    {
        push(
            Replan,
            format!("{}: {}", problem.code.as_str(), problem.reason),
        );
    }
    if state.task != TaskStatus::InProgress {
        push(
            Retry,
            format!(
                "task {} is {}, not in progress",
                op.task_id,
                state.task.as_str()
            ),
        );
    }
    if state.run != RunStatus::NeedsSession || state.run_leased {
        push(
            Retry,
            format!(
                "run {} is {}{}, not parked at rest",
                op.run_id,
                state.run.as_str(),
                if state.run_leased { " and leased" } else { "" }
            ),
        );
    }
    for (child, status) in &state.children {
        if *status != TaskStatus::Submitted {
            push(
                Replan,
                format!("child task {child} is {}, not submitted", status.as_str()),
            );
        }
    }
    if state.plan.is_some_and(|plan| plan.approved_at.is_some()) {
        match &state.approved_children {
            None => push(
                Replan,
                "plan review's approval recorded no version of the children".to_owned(),
            ),
            Some(approved) => {
                for (child, now) in &state.child_versions {
                    if approved.get(child) != Some(now) {
                        push(
                            Replan,
                            format!(
                                "child task {child} changed since plan review approved the plan (its conditions were at version {}, they are at {now})",
                                approved.get(child).map_or("(none)", String::as_str)
                            ),
                        );
                    }
                }
            }
        }
    }
    for (successor, status) in &state.successors {
        if !matches!(
            status,
            TaskStatus::Draft | TaskStatus::Submitted | TaskStatus::Ready
        ) {
            push(
                Replan,
                format!(
                    "task {successor} waits for task {} and is {} already",
                    op.task_id,
                    status.as_str()
                ),
            );
        }
    }
    problems
}

/// What an apply of an operation did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ApplyOutcome {
    /// The replacement is in place: the operation as it is now.
    Applied(Box<ReplanOperation>),
    /// It was applied before: nothing was written.
    AlreadyApplied,
    /// Nothing was applied, for `problems`; the operation is held
    /// `blocked` when it still was where the apply read it, and goes on as
    /// `recovery` says. `recorded` is whether the refusal wrote anything:
    /// a check again that finds what was held before writes nothing.
    Refused {
        problems: Vec<String>,
        recovery: ApplyRecovery,
        recorded: bool,
    },
}

/// What a child takes of the saved work of the run its operation replaced,
/// as the applied plan said: the snapshot's head (`snapshot_head`) or its
/// uncommitted changes (`snapshot_diff`), in `order` among the children
/// that take saved work. Its claim hands only that over, from the
/// operation's snapshot, to the child's own run, which goes through every
/// gate of a run as any other (receipt, validation, review, e2e, landing).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChildCarry {
    pub operation: ReplanId,
    pub from_task: TaskId,
    pub from_run: RunId,
    pub proposal: Option<ProposalId>,
    pub snapshot_id: String,
    pub carry: CarrySource,
    pub order: Option<u32>,
}

impl ChildCarry {
    /// The operation whose saved work `event` says a child's run was
    /// handed, or failed to be handed, as the execution context records it
    /// at the claim: `run_inherited` naming a replacement's operation, or
    /// `replan_carry_failed`. `None` for another kind, an inheritance of an
    /// earlier run's branch (no operation, as events written before the
    /// operation was recorded restore too), or a payload that does not
    /// restore.
    pub fn handover_of(event: &super::RunEvent) -> Option<ReplanId> {
        use super::EventKind;
        #[derive(Deserialize)]
        struct Inherited {
            #[serde(default)]
            replan_operation_id: Option<ReplanId>,
        }
        #[derive(Deserialize)]
        struct Failed {
            operation_id: ReplanId,
        }
        let payload = event.payload.clone();
        match event.kind.known()? {
            EventKind::RunInherited => {
                serde_json::from_value::<Inherited>(payload)
                    .ok()?
                    .replan_operation_id
            }
            EventKind::ReplanCarryFailed => serde_json::from_value::<Failed>(payload)
                .ok()
                .map(|failed| failed.operation_id),
            _ => None,
        }
    }
}

/// What replaced a task of a goal, as goal review reads it (ADR-t1521-2
/// decision 5): the applied operation, the task's acceptance items as the
/// operation fixed them, the children, and how each item went to a child or
/// was deleted with its authority, with the versions the review's
/// fingerprint compares. The replaced task's own run never landed: its
/// items are met only through its children's landings.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ReplacementLineage {
    pub operation: ReplanId,
    pub replaced_task: TaskId,
    /// The version of the task's conditions the operation fixed, and of
    /// its goal's acceptance.
    pub task_version: String,
    pub goal_acceptance_version: Option<i64>,
    /// The applied plan's revision and hash.
    pub plan_revision: i64,
    pub plan_hash: String,
    pub conditions: Vec<AcceptanceItem>,
    pub children: Vec<TaskId>,
    pub mappings: Vec<ConditionMapping>,
}

impl ReplacementLineage {
    /// What a goal review's fingerprint keeps of it: another plan, other
    /// children or a change of a mapping makes a review that saw the old
    /// ones stale.
    pub fn fingerprint(&self) -> String {
        let mappings = super::replan_snapshot::sha256(
            serde_json::to_string(&self.mappings)
                .unwrap_or_default()
                .as_bytes(),
        );
        let children: Vec<String> = self.children.iter().map(ToString::to_string).collect();
        format!(
            "{}>{}:{}:{}:{}:{}",
            self.replaced_task,
            children.join("+"),
            self.operation,
            self.task_version,
            self.plan_hash,
            mappings
        )
    }
}

/// The replacements of a goal's replaced tasks, through the replacements
/// of their replaced children too ([`ReplacementLineage`]), and the tasks
/// they reach outside the goal, which its review reads with its own.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct GoalReplacements {
    pub lineage: Vec<ReplacementLineage>,
    pub outside: Vec<TaskId>,
}

/// What `status` (the inbox) shows, on the replaced task, while
/// [`unresolved_replacements`] finds its record unresolved.
pub const REPLACEMENT_UNRESOLVED: &str = "replacement_unresolved";

/// Why the applied replacements of a replaced task do not say what
/// replaced it (ADR-t1521-2 decision 5, docs/plans/task-replanning.md,
/// "goal reviewとgoal close"). The apply always writes at least one child
/// and never a loop, so only a record broken by hand or by a later defect
/// has one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(tag = "fault", rename_all = "snake_case")]
pub enum ReplacementFault {
    /// No applied replacement names a task in its place.
    Empty,
    /// An applied replacement names task `to`, which is not there.
    Missing { to: TaskId },
    /// Following the replacements from it comes back to task `to`, which
    /// the same walk is still following.
    Cycle { to: TaskId },
}

impl std::fmt::Display for ReplacementFault {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Empty => f.write_str("its replacement names no task"),
            Self::Missing { to } => {
                write!(f, "its replacement names task {to}, which is not there")
            }
            Self::Cycle { to } => write!(f, "its replacements loop back to task {to}"),
        }
    }
}

/// A replaced task whose record [`ReplacementFault`] says is broken: it is
/// never resolved, so the goal of the replaced task it was reached from
/// neither starts a goal review nor closes as achieved (a person closes it
/// otherwise), and a recurring key it holds stays held, since nothing
/// fixes the record.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
pub struct UnresolvedReplacement {
    pub task: TaskId,
    #[serde(flatten)]
    pub fault: ReplacementFault,
}

/// The replaced tasks reached from `from` (replaced tasks), through the
/// replacements of a replaced child too, whose record is broken, in task
/// order. `replacements` is each task's applied replacements (the tasks in
/// its place) and `statuses` each task that is there: a task with status
/// `replaced` or in `from` needs one replacement, each replacement a task
/// that is there and that the walk is not still following. The walk visits
/// a task once, so a loop ends it.
pub fn unresolved_replacements(
    from: &[TaskId],
    replacements: &BTreeMap<TaskId, Vec<TaskId>>,
    statuses: &BTreeMap<TaskId, TaskStatus>,
) -> Vec<UnresolvedReplacement> {
    fn walk(
        task: TaskId,
        root: bool,
        replacements: &BTreeMap<TaskId, Vec<TaskId>>,
        statuses: &BTreeMap<TaskId, TaskStatus>,
        path: &mut Vec<TaskId>,
        visited: &mut BTreeSet<TaskId>,
        found: &mut BTreeSet<UnresolvedReplacement>,
    ) {
        visited.insert(task);
        path.push(task);
        let children = replacements.get(&task).map(Vec::as_slice).unwrap_or(&[]);
        if children.is_empty() && (root || statuses.get(&task) == Some(&TaskStatus::Replaced)) {
            found.insert(UnresolvedReplacement {
                task,
                fault: ReplacementFault::Empty,
            });
        }
        for &child in children {
            let fault = if !statuses.contains_key(&child) {
                Some(ReplacementFault::Missing { to: child })
            } else if path.contains(&child) {
                Some(ReplacementFault::Cycle { to: child })
            } else {
                None
            };
            if let Some(fault) = fault {
                found.insert(UnresolvedReplacement { task, fault });
            } else if !visited.contains(&child) {
                walk(child, false, replacements, statuses, path, visited, found);
            }
        }
        path.pop();
    }
    let mut visited = BTreeSet::new();
    let mut found = BTreeSet::new();
    for &task in from {
        if !visited.contains(&task) {
            walk(
                task,
                true,
                replacements,
                statuses,
                &mut Vec::new(),
                &mut visited,
                &mut found,
            );
        }
    }
    found.into_iter().collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::replan_plan::{PlanChild, PlanChoice, PlanSuccessor};
    use crate::domain::task_replan::{
        Diagnosis, OriginalConditions, ReplanOrigin, acceptance_items,
    };

    fn id(n: i64) -> TaskId {
        TaskId::new(n)
    }

    fn plan(successors: Vec<PlanSuccessor>) -> ReplacementPlan {
        ReplacementPlan {
            operation_id: ReplanId::new(1),
            task_version: "v".into(),
            goal_acceptance_version: None,
            choice: PlanChoice::Split,
            rationale: "r".into(),
            children: vec![
                PlanChild {
                    task_id: id(10),
                    carry: CarrySource::SnapshotHead,
                    order: Some(1),
                },
                PlanChild {
                    task_id: id(11),
                    carry: CarrySource::Base,
                    order: None,
                },
            ],
            mappings: Vec::new(),
            successors,
        }
    }

    fn op(status: ReplanStatus) -> ReplanOperation {
        ReplanOperation {
            id: ReplanId::new(1),
            task_id: id(7),
            run_id: RunId::new("run-1").unwrap(),
            goal_id: None,
            origin: ReplanOrigin::Manual,
            status,
            status_reason: None,
            generation: 2,
            return_phase: None,
            reason: "too long".into(),
            conditions: OriginalConditions {
                title: "t".into(),
                description: "d".into(),
                acceptance: acceptance_items("a"),
                paths: Vec::new(),
                verify: Vec::new(),
                evidence: serde_json::Value::Null,
                predecessors: Vec::new(),
                successors: Vec::new(),
                goal: None,
            },
            task_version: "v".into(),
            goal_acceptance_version: None,
            diagnosis: Diagnosis::default(),
            snapshot_id: Some("snap".into()),
            request_id: None,
            request_status: None,
            proposal_id: Some(ProposalId::new(5)),
            requested_by: "user".into(),
            requested_by_id: "user".into(),
            mappings: Vec::new(),
            replaced_by: Vec::new(),
            created_at: 0,
            updated_at: 0,
            closed_at: None,
        }
    }

    fn revision(approved: bool) -> PlanRevision {
        PlanRevision {
            revision: 1,
            proposal_id: ProposalId::new(5),
            plan: plan(Vec::new()),
            hash: "h".into(),
            approved_at: approved.then_some(1),
        }
    }

    fn state<'a>(op: &'a ReplanOperation, plan: &'a PlanRevision) -> ApplyState<'a> {
        ApplyState {
            operation: op,
            generation: 2,
            plan: Some(plan),
            proposal: Some(ProposalStatus::Accepted),
            task: TaskStatus::InProgress,
            run: RunStatus::NeedsSession,
            run_leased: false,
            children: [
                (id(10), TaskStatus::Submitted),
                (id(11), TaskStatus::Submitted),
            ]
            .into_iter()
            .collect(),
            child_versions: [(id(10), "a".to_owned()), (id(11), "b".to_owned())]
                .into_iter()
                .collect(),
            approved_children: Some(
                [(id(10), "a".to_owned()), (id(11), "b".to_owned())]
                    .into_iter()
                    .collect(),
            ),
            successors: [(id(20), TaskStatus::Ready)].into_iter().collect(),
            plan_problems: Vec::new(),
        }
    }

    #[test]
    fn a_listed_successor_waits_for_its_children_and_one_added_since_for_all() {
        let plan = plan(vec![PlanSuccessor {
            task_id: id(20),
            needs: vec![id(11)],
            reason: Some("only the docs".into()),
        }]);
        assert_eq!(
            successor_edges(&plan, &[id(20), id(21)]),
            vec![(id(20), vec![id(11)]), (id(21), vec![id(10), id(11)])]
        );
        assert!(successor_edges(&plan, &[]).is_empty());
    }

    fn task(n: i64) -> WaitNode {
        WaitNode::Task(id(n))
    }

    #[test]
    fn a_successor_that_a_child_waits_for_closes_a_loop() {
        let waits: BTreeMap<WaitNode, Vec<WaitNode>> =
            [(task(11), vec![task(10)]), (task(10), vec![task(20)])]
                .into_iter()
                .collect();
        assert_eq!(closes_loop(&waits, &[(id(20), vec![id(11)])]), Some(id(20)));
        let waits: BTreeMap<WaitNode, Vec<WaitNode>> =
            [(task(11), vec![task(10)])].into_iter().collect();
        assert_eq!(closes_loop(&waits, &[(id(20), vec![id(10), id(11)])]), None);
    }

    /// A child that waits for a goal whose task is a successor closes a
    /// loop through the goal: the goal waits for its tasks.
    #[test]
    fn a_loop_through_a_goal_is_found() {
        let goal = WaitNode::Goal(GoalId::new(3));
        let waits: BTreeMap<WaitNode, Vec<WaitNode>> =
            [(task(10), vec![goal]), (goal, vec![task(20), task(21)])]
                .into_iter()
                .collect();
        assert_eq!(closes_loop(&waits, &[(id(20), vec![id(10)])]), Some(id(20)));
        // A successor outside the goal closes none.
        assert_eq!(closes_loop(&waits, &[(id(22), vec![id(10)])]), None);
    }

    /// A lineage's fingerprint changes with a mapping, the plan or the
    /// children, and with nothing else it reads.
    #[test]
    fn a_lineage_fingerprint_follows_its_mappings_plan_and_children() {
        let lineage = ReplacementLineage {
            operation: ReplanId::new(1),
            replaced_task: id(7),
            task_version: "v".into(),
            goal_acceptance_version: Some(1),
            plan_revision: 1,
            plan_hash: "h".into(),
            conditions: acceptance_items("a"),
            children: vec![id(10), id(11)],
            mappings: vec![ConditionMapping {
                ordinal: 1,
                hash: "x".into(),
                child_task_id: Some(id(10)),
                child_ordinal: Some(1),
                disposition: "split".into(),
                authority: None,
            }],
        };
        let seen = lineage.fingerprint();
        assert_eq!(seen, lineage.clone().fingerprint());
        let mut moved = lineage.clone();
        moved.mappings[0].child_task_id = Some(id(11));
        let mut replanned = lineage.clone();
        replanned.plan_hash = "other".into();
        let mut fewer = lineage.clone();
        fewer.children.pop();
        for changed in [moved, replanned, fewer] {
            assert_ne!(changed.fingerprint(), seen, "{changed:?}");
        }
    }

    #[test]
    fn a_refusal_calls_for_a_person_before_a_new_plan_and_a_plan_before_a_retry() {
        let problem = ApplyProblem::new;
        assert_eq!(recovery_of(&[]), None);
        assert_eq!(
            recovery_of(&[problem(ApplyRecovery::Retry, "leased")]),
            Some(ApplyRecovery::Retry)
        );
        assert_eq!(
            recovery_of(&[
                problem(ApplyRecovery::Retry, "leased"),
                problem(ApplyRecovery::Replan, "stale"),
            ]),
            Some(ApplyRecovery::Replan)
        );
        assert_eq!(
            recovery_of(&[
                problem(ApplyRecovery::Person, "lost"),
                problem(ApplyRecovery::Replan, "stale"),
            ]),
            Some(ApplyRecovery::Person)
        );
    }

    #[test]
    fn an_approved_plan_of_a_parked_task_applies() {
        let op = op(ReplanStatus::Reviewing);
        let plan = revision(true);
        assert_eq!(apply_problems(&state(&op, &plan)), Vec::new());
        let applying = self::op(ReplanStatus::Applying);
        assert!(apply_problems(&state(&applying, &plan)).is_empty());
        // One held blocked from its apply is checked again.
        let mut blocked = self::op(ReplanStatus::Blocked);
        blocked.return_phase = Some(ReplanStatus::Applying);
        assert!(apply_problems(&state(&blocked, &plan)).is_empty());
        blocked.return_phase = Some(ReplanStatus::Snapshotting);
        assert!(!apply_problems(&state(&blocked, &plan)).is_empty());
        // Leaving the planner is the apply itself.
        let mut with_planner = state(&op, &plan);
        with_planner.plan_problems.push(PlanProblem {
            code: PlanProblemCode::NotWithPlanner,
            task_id: None,
            reason: "applying".into(),
        });
        assert!(apply_problems(&with_planner).is_empty());
    }

    type Change = Box<dyn Fn(&mut ApplyState<'_>)>;

    #[test]
    fn every_stale_or_started_part_refuses_the_whole_apply() {
        let op = op(ReplanStatus::Reviewing);
        let approved = revision(true);
        let unapproved = revision(false);
        use ApplyRecovery::{Replan, Retry};
        let cases: Vec<(&str, ApplyRecovery, Change)> = vec![
            ("generation", Retry, Box::new(|s| s.generation = 1)),
            ("no plan", Replan, Box::new(|s| s.plan = None)),
            (
                "not accepted",
                Replan,
                Box::new(|s| s.proposal = Some(ProposalStatus::Revising)),
            ),
            (
                "version_stale",
                Replan,
                Box::new(|s| {
                    s.plan_problems.push(PlanProblem {
                        code: PlanProblemCode::VersionStale,
                        task_id: None,
                        reason: "changed".into(),
                    });
                }),
            ),
            (
                "not in progress",
                Retry,
                Box::new(|s| s.task = TaskStatus::Completed),
            ),
            (
                "not parked",
                Retry,
                Box::new(|s| s.run = RunStatus::Running),
            ),
            ("not parked", Retry, Box::new(|s| s.run_leased = true)),
            (
                "not submitted",
                Replan,
                Box::new(|s| {
                    s.children.insert(id(10), TaskStatus::Ready);
                }),
            ),
            (
                "changed since plan review approved",
                Replan,
                Box::new(|s| {
                    s.child_versions.insert(id(11), "edited".to_owned());
                }),
            ),
            (
                "recorded no version",
                Replan,
                Box::new(|s| s.approved_children = None),
            ),
            (
                "already",
                Replan,
                Box::new(|s| {
                    s.successors.insert(id(21), TaskStatus::InProgress);
                }),
            ),
            (
                "dependency_cycle",
                Replan,
                Box::new(|s| {
                    s.plan_problems.push(PlanProblem {
                        code: PlanProblemCode::DependencyCycle,
                        task_id: Some(id(20)),
                        reason: "a loop".into(),
                    });
                }),
            ),
        ];
        let reasons = |problems: &[ApplyProblem]| -> Vec<String> {
            problems.iter().map(|p| p.reason.clone()).collect()
        };
        for (expected, recovery, change) in cases {
            let mut s = state(&op, &approved);
            change(&mut s);
            let problems = apply_problems(&s);
            assert!(
                problems.iter().any(
                    |problem| problem.reason.contains(expected) && problem.recovery == recovery
                ),
                "{expected}: {problems:?}"
            );
        }
        let problems = reasons(&apply_problems(&state(&op, &unapproved)));
        assert!(
            problems.iter().any(|p| p.contains("not approved")),
            "{problems:?}"
        );
        let mut other = state(&op, &approved);
        let mut moved = self::op(ReplanStatus::Planning);
        moved.proposal_id = Some(ProposalId::new(6));
        other.operation = &moved;
        let problems = reasons(&apply_problems(&other));
        assert!(
            problems.iter().any(|p| p.contains("not waiting")),
            "{problems:?}"
        );
        assert!(
            problems.iter().any(|p| p.contains("goes with proposal")),
            "{problems:?}"
        );
    }

    fn replaced_by(edges: &[(i64, i64)]) -> BTreeMap<TaskId, Vec<TaskId>> {
        let mut map: BTreeMap<TaskId, Vec<TaskId>> = BTreeMap::new();
        for &(from, to) in edges {
            map.entry(id(from)).or_default().push(id(to));
        }
        map
    }

    fn there(tasks: &[(i64, TaskStatus)]) -> BTreeMap<TaskId, TaskStatus> {
        tasks.iter().map(|&(n, status)| (id(n), status)).collect()
    }

    #[test]
    fn a_replacement_whose_tasks_are_all_there_is_not_unresolved() {
        let statuses = there(&[
            (1, TaskStatus::Replaced),
            (2, TaskStatus::Completed),
            (3, TaskStatus::Replaced),
            (4, TaskStatus::Ready),
        ]);
        let edges = replaced_by(&[(1, 2), (1, 3), (3, 4)]);
        assert!(unresolved_replacements(&[id(1)], &edges, &statuses).is_empty());
        assert!(unresolved_replacements(&[], &edges, &statuses).is_empty());
    }

    #[test]
    fn an_empty_record_is_unresolved_on_the_replaced_task_and_on_a_replaced_child() {
        let statuses = there(&[
            (1, TaskStatus::Replaced),
            (2, TaskStatus::Replaced),
            (5, TaskStatus::Replaced),
        ]);
        let edges = replaced_by(&[(1, 2)]);
        let empty = |n| UnresolvedReplacement {
            task: id(n),
            fault: ReplacementFault::Empty,
        };
        assert_eq!(
            unresolved_replacements(&[id(1), id(5)], &edges, &statuses),
            vec![empty(2), empty(5)]
        );
    }

    #[test]
    fn a_replacement_naming_a_task_not_there_is_unresolved() {
        let statuses = there(&[(1, TaskStatus::Replaced), (2, TaskStatus::Completed)]);
        let edges = replaced_by(&[(1, 2), (1, 9)]);
        assert_eq!(
            unresolved_replacements(&[id(1)], &edges, &statuses),
            vec![UnresolvedReplacement {
                task: id(1),
                fault: ReplacementFault::Missing { to: id(9) },
            }]
        );
    }

    #[test]
    fn a_loop_ends_the_walk_and_is_unresolved_where_it_closes() {
        let statuses = there(&[
            (1, TaskStatus::Replaced),
            (2, TaskStatus::Replaced),
            (3, TaskStatus::Replaced),
        ]);
        let edges = replaced_by(&[(1, 2), (2, 3), (3, 2)]);
        let cycle = UnresolvedReplacement {
            task: id(3),
            fault: ReplacementFault::Cycle { to: id(2) },
        };
        assert_eq!(
            unresolved_replacements(&[id(1)], &edges, &statuses),
            vec![cycle]
        );
        // Starting inside the loop finds it too; a task reached twice
        // without a loop (another root) is no loop.
        let self_loop = replaced_by(&[(1, 1)]);
        assert_eq!(
            unresolved_replacements(&[id(1)], &self_loop, &statuses),
            vec![UnresolvedReplacement {
                task: id(1),
                fault: ReplacementFault::Cycle { to: id(1) },
            }]
        );
        let shared = replaced_by(&[(1, 3), (2, 3), (3, 4)]);
        let statuses = there(&[
            (1, TaskStatus::Replaced),
            (2, TaskStatus::Replaced),
            (3, TaskStatus::Replaced),
            (4, TaskStatus::Completed),
        ]);
        assert!(unresolved_replacements(&[id(1), id(2)], &shared, &statuses).is_empty());
    }

    #[test]
    fn a_fault_says_what_is_broken() {
        assert_eq!(
            ReplacementFault::Missing { to: id(9) }.to_string(),
            "its replacement names task 9, which is not there"
        );
        assert_eq!(
            serde_json::to_value(UnresolvedReplacement {
                task: id(3),
                fault: ReplacementFault::Cycle { to: id(2) },
            })
            .unwrap(),
            serde_json::json!({"task": 3, "fault": "cycle", "to": 2})
        );
    }
}
