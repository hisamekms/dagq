//! The replacement plan of a replanning operation (ADR-t1521-1 decisions 2
//! and 3, ADR-t1521-2 decision 4): what the runtime's planner of the
//! operation's request submits beside its proposal of child tasks, and the
//! one judgment ([`check_plan`]) that submit, `lint` and plan review apply
//! to it. A plan splits the parked task into children or redistributes its
//! scope among them; a continuation is no plan (the planner declines the
//! request to continue the run). The plan maps every item of the task's
//! acceptance as the operation fixed it to the children that hold it, or
//! deletes one only on a person's scope answer; names what each child
//! takes of the saved work (none, the snapshot's head or its diff, each to
//! one child only) and in what order; keeps every verification command and
//! evidence of the task; has each child wait for the task's predecessors
//! and none for the task or what waits for it; and says which children each
//! successor of the task waits for. It is checked against the versions of
//! the task's and its goal's conditions the operation fixed, so a plan made
//! on conditions that changed since is refused.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

use super::{
    AskId, DomainError, GoalId, ProposalId, ReplanId, RequestId, TaskId, TaskStatus,
    plan_request::RequestStatus,
    plan_review::PlanReviewAction,
    task_replan::{AcceptanceItem, ReplanOperation, ReplanStatus},
};

// How the plan replaces the task: several children that split its
// conditions, or a smaller unit with the rest left to children of the
// same goal.
string_enum!(PlanChoice {
    Split => "split",
    Redistribute => "redistribute",
});

// How one item of the task's acceptance goes to the replacement. `landed`
// is an item work already on main meets, which a child still checks;
// `deleted` holds no child and needs a person's scope answer.
string_enum!(ItemDisposition {
    Split => "split",
    Redistribute => "redistribute",
    Landed => "landed",
    Deleted => "deleted",
});

// What a child starts from: main as it is (`base`), the commits the parked
// run made (`snapshot_head`), or its saved uncommitted changes
// (`snapshot_diff`).
string_enum!(CarrySource {
    Base => "base",
    SnapshotHead => "snapshot_head",
    SnapshotDiff => "snapshot_diff",
});

/// One child of the plan: a task of the proposal, and what it takes of the
/// saved work. `order` places the children that take saved work: the
/// first takes it first.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlanChild {
    pub task_id: TaskId,
    pub carry: CarrySource,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub order: Option<u32>,
}

/// A child item that holds an item of the task's acceptance.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChildItem {
    pub task_id: TaskId,
    pub ordinal: usize,
}

/// How one item of the task's acceptance (its ordinal and hash as the
/// operation fixed them) goes to the children.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlanMapping {
    pub ordinal: usize,
    pub hash: String,
    pub disposition: ItemDisposition,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub children: Vec<ChildItem>,
    /// For `landed`: the completed task whose landing meets the item.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub landed_task_id: Option<TaskId>,
    /// For `deleted`: the answered ask that decided it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub authority_ask_id: Option<AskId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

/// Which children a successor of the task waits for in its place: all of
/// them unless `reason` says why fewer hold what it needs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlanSuccessor {
    pub task_id: TaskId,
    pub needs: Vec<TaskId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

/// The replacement plan a planner submits with `submit --replacement`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReplacementPlan {
    pub operation_id: ReplanId,
    /// The version of the task's conditions the plan was made on
    /// ([`super::task_replan::OriginalConditions::version`]).
    pub task_version: String,
    /// The goal's acceptance version the plan was made on; `None` for a
    /// task without a goal.
    #[serde(default)]
    pub goal_acceptance_version: Option<i64>,
    pub choice: PlanChoice,
    pub rationale: String,
    pub children: Vec<PlanChild>,
    pub mappings: Vec<PlanMapping>,
    #[serde(default)]
    pub successors: Vec<PlanSuccessor>,
}

impl ReplacementPlan {
    /// Parse a plan from its JSON.
    pub fn parse(text: &str) -> Result<Self, DomainError> {
        serde_json::from_str(text).map_err(|error| DomainError::InvalidReplacementPlan {
            reason: error.to_string(),
        })
    }

    /// The child tasks, in the plan's order.
    pub fn child_ids(&self) -> Vec<TaskId> {
        self.children.iter().map(|child| child.task_id).collect()
    }
}

// What a problem of a plan is about.
string_enum!(PlanProblemCode {
    OperationMismatch => "operation_mismatch",
    NotWithPlanner => "not_with_planner",
    VersionStale => "version_stale",
    BlankRationale => "blank_rationale",
    NoChildren => "no_children",
    ChildUnknown => "child_unknown",
    ChildNotMember => "child_not_member",
    ChildNotPending => "child_not_pending",
    MemberNotChild => "member_not_child",
    OriginalAsChild => "original_as_child",
    GoalMismatch => "goal_mismatch",
    ItemUnmapped => "item_unmapped",
    ItemUnknown => "item_unknown",
    ItemRepeated => "item_repeated",
    ChildItemMissing => "child_item_missing",
    DeletionUnauthorized => "deletion_unauthorized",
    LandedUnproven => "landed_unproven",
    ChildHoldsNothing => "child_holds_nothing",
    VerificationWeakened => "verification_weakened",
    EvidenceDropped => "evidence_dropped",
    PathsMissing => "paths_missing",
    CarryUnclear => "carry_unclear",
    PredecessorDropped => "predecessor_dropped",
    DependencyCycle => "dependency_cycle",
    SuccessorUnmapped => "successor_unmapped",
    SuccessorUnknown => "successor_unknown",
    SuccessorNeedsUnclear => "successor_needs_unclear",
    SuccessorStarted => "successor_started",
});

/// One problem of a plan: its code, the task it is about (a child, a
/// successor) when one, and why.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlanProblem {
    pub code: PlanProblemCode,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub task_id: Option<TaskId>,
    pub reason: String,
}

/// A task the plan names as the check sees it now.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlanTask {
    pub status: TaskStatus,
    pub goal_id: Option<GoalId>,
    pub acceptance: Vec<AcceptanceItem>,
    pub verify: Vec<String>,
    pub paths: Vec<String>,
    pub evidence: Vec<serde_json::Value>,
    /// Direct predecessors.
    pub depends_on: Vec<TaskId>,
}

/// What an ask a deletion names is, as the check reads it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeletionAuthority {
    /// Whether it is about the operation (its request, its run or its
    /// task).
    pub about_operation: bool,
    /// Whether it asked for a person because of scope.
    pub scope: bool,
    /// Whether a person (or the inbox at a person's word) answered it.
    pub answered_by_person: bool,
    /// The answer, which approves a deletion only as the item's option
    /// chosen whole ([`approves_deletion`]).
    pub answer: String,
    /// The options the ask offered.
    pub options: Vec<String>,
}

/// The word a person's answer names acceptance item `item` with to approve
/// its deletion: `delete:<ordinal>:<the first 12 characters of its hash>`,
/// which binds the approval to the item as the operation fixed it. The
/// planner offers it as an option of its scope ask.
pub fn deletion_token(item: &AcceptanceItem) -> String {
    let hash: String = item.hash.chars().take(12).collect();
    format!("delete:{}:{hash}", item.ordinal)
}

/// Whether `authority` approves deleting `item`: the ask offered the
/// item's [`deletion_token`] as an option and the answer is that option,
/// whole. An answer that keeps the item, refuses or explains around the
/// token, approves another item's deletion or names an item since changed
/// approves none.
pub fn approves_deletion(authority: &DeletionAuthority, item: &AcceptanceItem) -> bool {
    let token = deletion_token(item);
    authority.answer.trim() == token && authority.options.contains(&token)
}

/// Everything [`check_plan`] reads of the queue, in one read.
#[derive(Debug, Clone)]
pub struct PlanContext<'a> {
    pub operation: &'a ReplanOperation,
    /// The version of the task's conditions now.
    pub task_version_now: String,
    /// The goal's acceptance version now.
    pub goal_acceptance_version_now: Option<i64>,
    /// The tasks of the proposal the plan goes with.
    pub members: Vec<TaskId>,
    /// Every task the plan names (children, successors, landed tasks) and
    /// every task the children wait for, as they are now.
    pub tasks: BTreeMap<TaskId, PlanTask>,
    /// The tasks that wait for the task, directly or not, now.
    pub downstream: BTreeSet<TaskId>,
    /// The asks the deletions name.
    pub authorities: BTreeMap<AskId, DeletionAuthority>,
}

impl PlanContext<'_> {
    /// The context as plan review's pass leaves it once its `actions`
    /// apply: a task it cancels as a duplicate is `canceled`, and a task it
    /// makes wait has that predecessor. The pass is judged on this, so it
    /// cannot accept a plan its own actions break, such as one that gives a
    /// child it cancels an item, saved work or a successor's wait. An
    /// action on a task outside the proposal changes nothing here: the pass
    /// refuses it ([`super::plan_review::check_action`]).
    pub fn after_actions(mut self, actions: &[PlanReviewAction]) -> Self {
        for action in actions {
            if !self.members.contains(&action.task_id()) {
                continue;
            }
            match action {
                PlanReviewAction::CancelDuplicate { task_id, .. } => {
                    if let Some(task) = self.tasks.get_mut(task_id) {
                        task.status = TaskStatus::Canceled;
                    }
                }
                PlanReviewAction::AddDependency {
                    task_id,
                    depends_on,
                } => {
                    if let Some(task) = self.tasks.get_mut(task_id)
                        && !task.depends_on.contains(depends_on)
                    {
                        task.depends_on.push(*depends_on);
                    }
                }
                PlanReviewAction::LowerPriority { .. } => {}
            }
        }
        self
    }
}

/// The problems of `plan` in `ctx`: none means the plan may go to plan
/// review, and plan review's pass may stand. Each rule reports what it
/// finds, in the order of the rules.
pub fn check_plan(plan: &ReplacementPlan, ctx: &PlanContext<'_>) -> Vec<PlanProblem> {
    let mut problems = Vec::new();
    let op = ctx.operation;
    let mut push = |code, task_id: Option<TaskId>, reason: String| {
        problems.push(PlanProblem {
            code,
            task_id,
            reason,
        });
    };
    if plan.operation_id != op.id {
        push(
            PlanProblemCode::OperationMismatch,
            None,
            format!(
                "the plan names replan operation {}, but the proposal replaces task {} of operation {}",
                plan.operation_id, op.task_id, op.id
            ),
        );
    }
    if !op.status.hands_to_planner() {
        push(
            PlanProblemCode::NotWithPlanner,
            None,
            format!(
                "replan operation {} is {}, not with its planner",
                op.id,
                op.status.as_str()
            ),
        );
    }
    if plan.task_version != op.task_version || ctx.task_version_now != op.task_version {
        push(
            PlanProblemCode::VersionStale,
            None,
            format!(
                "the plan was made on the conditions of task {} at version {}, the operation fixed {} and they are {} now",
                op.task_id, plan.task_version, op.task_version, ctx.task_version_now
            ),
        );
    }
    if plan.goal_acceptance_version != op.goal_acceptance_version
        || ctx.goal_acceptance_version_now != op.goal_acceptance_version
    {
        push(
            PlanProblemCode::VersionStale,
            None,
            format!(
                "the plan was made on goal acceptance version {:?}, the operation fixed {:?} and it is {:?} now: the goal's acceptance is not the planner's to change",
                plan.goal_acceptance_version,
                op.goal_acceptance_version,
                ctx.goal_acceptance_version_now
            ),
        );
    }
    if plan.rationale.trim().is_empty() {
        push(
            PlanProblemCode::BlankRationale,
            None,
            "the plan gives no rationale for its choice".into(),
        );
    }
    let children: BTreeSet<TaskId> = plan.child_ids().into_iter().collect();
    if children.is_empty() {
        push(
            PlanProblemCode::NoChildren,
            None,
            "the plan names no child task; to continue the run, decline the request with --continue-run".into(),
        );
    }
    for &child in &children {
        if child == op.task_id {
            push(
                PlanProblemCode::OriginalAsChild,
                Some(child),
                format!("task {child} is the task being replaced; its children are new tasks"),
            );
        }
        if !ctx.members.contains(&child) {
            push(
                PlanProblemCode::ChildNotMember,
                Some(child),
                format!("child task {child} is not a task of the proposal"),
            );
        }
        match ctx.tasks.get(&child) {
            None => push(
                PlanProblemCode::ChildUnknown,
                Some(child),
                format!("child task {child} does not exist"),
            ),
            Some(task) if task.goal_id != op.goal_id => push(
                PlanProblemCode::GoalMismatch,
                Some(child),
                format!(
                    "child task {child} belongs to goal {:?}, the task being replaced to goal {:?}",
                    task.goal_id, op.goal_id
                ),
            ),
            Some(_) => {}
        }
        // What the plan gives a child (items, saved work, successors'
        // waits) holds only while the child can still take it.
        if let Some(task) = ctx.tasks.get(&child)
            && !matches!(task.status, TaskStatus::Draft | TaskStatus::Submitted)
        {
            push(
                PlanProblemCode::ChildNotPending,
                Some(child),
                format!(
                    "child task {child} is {}; a child is a task the replacement has not started (draft or submitted), so map what it held to the others",
                    task.status.as_str()
                ),
            );
        }
    }
    for &member in &ctx.members {
        if !children.contains(&member) {
            push(
                PlanProblemCode::MemberNotChild,
                Some(member),
                format!(
                    "task {member} is in the proposal but not a child of the plan; a replacement proposal holds its children only"
                ),
            );
        }
    }
    check_items(plan, ctx, &children, &mut push);
    check_checks(ctx, &children, &mut push);
    check_carry(plan, &mut push);
    check_dependencies(ctx, &children, &mut push);
    check_successors(plan, ctx, &children, &mut push);
    problems
}

type Push<'a> = dyn FnMut(PlanProblemCode, Option<TaskId>, String) + 'a;

/// Every item of the task's acceptance is mapped once, to child items that
/// exist, or deleted on a person's scope answer; `landed` names a
/// completed task and still a child that checks it; every child holds an
/// item.
fn check_items(
    plan: &ReplacementPlan,
    ctx: &PlanContext<'_>,
    children: &BTreeSet<TaskId>,
    push: &mut Push<'_>,
) {
    let op = ctx.operation;
    let items: BTreeMap<usize, &AcceptanceItem> = op
        .conditions
        .acceptance
        .iter()
        .map(|item| (item.ordinal, item))
        .collect();
    let mut seen = BTreeSet::new();
    let mut holding = BTreeSet::new();
    for mapping in &plan.mappings {
        let ordinal = mapping.ordinal;
        let Some(item) = items.get(&ordinal) else {
            push(
                PlanProblemCode::ItemUnknown,
                None,
                format!(
                    "task {} has no acceptance item {ordinal} as the operation fixed it",
                    op.task_id
                ),
            );
            continue;
        };
        if item.hash != mapping.hash {
            push(
                PlanProblemCode::VersionStale,
                None,
                format!(
                    "acceptance item {ordinal} of task {} has hash {}, the plan names {}",
                    op.task_id, item.hash, mapping.hash
                ),
            );
        }
        if !seen.insert(ordinal) {
            push(
                PlanProblemCode::ItemRepeated,
                None,
                format!(
                    "acceptance item {ordinal} is mapped more than once; list its children in one mapping"
                ),
            );
            continue;
        }
        for held in &mapping.children {
            holding.insert(held.task_id);
            let exists = children.contains(&held.task_id)
                && ctx.tasks.get(&held.task_id).is_some_and(|task| {
                    task.acceptance
                        .iter()
                        .any(|child_item| child_item.ordinal == held.ordinal)
                });
            if !exists {
                push(
                    PlanProblemCode::ChildItemMissing,
                    Some(held.task_id),
                    format!(
                        "acceptance item {ordinal} goes to item {} of task {}, which is not an item of a child of the plan",
                        held.ordinal, held.task_id
                    ),
                );
            }
        }
        match mapping.disposition {
            ItemDisposition::Deleted => {
                if !mapping.children.is_empty() {
                    push(
                        PlanProblemCode::DeletionUnauthorized,
                        None,
                        format!(
                            "acceptance item {ordinal} is deleted and held by children at once"
                        ),
                    );
                }
                let authorized = mapping
                    .authority_ask_id
                    .and_then(|ask| ctx.authorities.get(&ask))
                    .is_some_and(|authority| {
                        authority.about_operation
                            && authority.scope
                            && authority.answered_by_person
                            && approves_deletion(authority, item)
                    });
                if !authorized {
                    push(
                        PlanProblemCode::DeletionUnauthorized,
                        None,
                        format!(
                            "acceptance item {ordinal} is deleted without a person's answer `{}` to a scope ask about the operation (authority_ask_id); ask one with --because scope offering that option, or map it to a child",
                            deletion_token(item)
                        ),
                    );
                }
            }
            disposition => {
                if mapping.children.is_empty() {
                    push(
                        PlanProblemCode::ItemUnmapped,
                        None,
                        format!(
                            "acceptance item {ordinal} ({}) names no child item that holds it",
                            disposition.as_str()
                        ),
                    );
                }
                if disposition == ItemDisposition::Landed {
                    let landed = mapping.landed_task_id.filter(|task| *task != op.task_id);
                    let proven = landed
                        .and_then(|task| ctx.tasks.get(&task))
                        .is_some_and(|task| task.status == TaskStatus::Completed);
                    if !proven {
                        push(
                            PlanProblemCode::LandedUnproven,
                            None,
                            format!(
                                "acceptance item {ordinal} is called landed without a completed task that landed it (landed_task_id); the parked run's saved work has not landed"
                            ),
                        );
                    }
                }
            }
        }
    }
    for ordinal in items.keys() {
        if !seen.contains(ordinal) {
            push(
                PlanProblemCode::ItemUnmapped,
                None,
                format!(
                    "acceptance item {ordinal} of task {} is mapped to no child and not deleted",
                    op.task_id
                ),
            );
        }
    }
    for &child in children {
        if !holding.contains(&child) {
            push(
                PlanProblemCode::ChildHoldsNothing,
                Some(child),
                format!(
                    "child task {child} holds no acceptance item of task {}",
                    op.task_id
                ),
            );
        }
    }
}

/// Each child runs every verification command of the task, the children
/// keep its evidence between them, and each declares paths when the task
/// did.
fn check_checks(ctx: &PlanContext<'_>, children: &BTreeSet<TaskId>, push: &mut Push<'_>) {
    let conditions = &ctx.operation.conditions;
    let known: Vec<&PlanTask> = children
        .iter()
        .filter_map(|child| ctx.tasks.get(child))
        .collect();
    let evidence = conditions.evidence.as_array().cloned().unwrap_or_default();
    for check in &evidence {
        if !known.iter().any(|task| task.evidence.contains(check)) {
            push(
                PlanProblemCode::EvidenceDropped,
                None,
                format!("no child requires the evidence {check} of the task being replaced"),
            );
        }
    }
    for &child in children {
        let Some(task) = ctx.tasks.get(&child) else {
            continue;
        };
        // Each child changes what the task did: it runs every verification
        // of the task, and may add its own.
        for command in &conditions.verify {
            if !task.verify.contains(command) {
                push(
                    PlanProblemCode::VerificationWeakened,
                    Some(child),
                    format!(
                        "child task {child} does not run the verification `{command}` of the task being replaced"
                    ),
                );
            }
        }
        if task.verify.is_empty() {
            push(
                PlanProblemCode::VerificationWeakened,
                Some(child),
                format!("child task {child} has no verification command"),
            );
        }
        if !conditions.paths.is_empty() && task.paths.is_empty() {
            push(
                PlanProblemCode::PathsMissing,
                Some(child),
                format!(
                    "child task {child} declares no paths, while the task being replaced declared them"
                ),
            );
        }
    }
}

/// Each saved part of the work goes to one child only, the children that
/// take saved work say in which order, and no two share a place.
fn check_carry(plan: &ReplacementPlan, push: &mut Push<'_>) {
    let mut takers: BTreeMap<&'static str, Vec<TaskId>> = BTreeMap::new();
    let mut orders: BTreeMap<u32, TaskId> = BTreeMap::new();
    for child in &plan.children {
        if child.carry == CarrySource::Base {
            continue;
        }
        takers
            .entry(child.carry.as_str())
            .or_default()
            .push(child.task_id);
        match child.order {
            None => push(
                PlanProblemCode::CarryUnclear,
                Some(child.task_id),
                format!(
                    "child task {} takes {} without the order it takes it in",
                    child.task_id,
                    child.carry.as_str()
                ),
            ),
            Some(order) => {
                if let Some(other) = orders.insert(order, child.task_id) {
                    push(
                        PlanProblemCode::CarryUnclear,
                        Some(child.task_id),
                        format!(
                            "children {other} and {} both take saved work in order {order}",
                            child.task_id
                        ),
                    );
                }
            }
        }
    }
    for (source, tasks) in takers {
        if tasks.len() > 1 {
            push(
                PlanProblemCode::CarryUnclear,
                Some(tasks[1]),
                format!(
                    "{source} goes to children {tasks:?}: each part of the saved work goes to one child, which the others wait for"
                ),
            );
        }
    }
    let mut named = BTreeSet::new();
    for child in &plan.children {
        if !named.insert(child.task_id) {
            push(
                PlanProblemCode::CarryUnclear,
                Some(child.task_id),
                format!("child task {} is named twice", child.task_id),
            );
        }
    }
}

/// Each child waits for every predecessor of the task that has not ended,
/// for neither the task nor anything that waits for it, and the children
/// wait for one another without a loop.
fn check_dependencies(ctx: &PlanContext<'_>, children: &BTreeSet<TaskId>, push: &mut Push<'_>) {
    let op = ctx.operation;
    for &child in children {
        let Some(task) = ctx.tasks.get(&child) else {
            continue;
        };
        for predecessor in &op.conditions.predecessors {
            let ended = ctx.tasks.get(predecessor).is_some_and(|task| {
                matches!(task.status, TaskStatus::Completed | TaskStatus::Canceled)
            });
            if !ended && !task.depends_on.contains(predecessor) {
                push(
                    PlanProblemCode::PredecessorDropped,
                    Some(child),
                    format!(
                        "child task {child} does not wait for task {predecessor}, which the task being replaced waits for"
                    ),
                );
            }
        }
        for on in &task.depends_on {
            if *on == op.task_id || ctx.downstream.contains(on) {
                push(
                    PlanProblemCode::DependencyCycle,
                    Some(child),
                    format!(
                        "child task {child} waits for task {on}, which is the task being replaced or waits for it: the replacement would close a loop"
                    ),
                );
            }
        }
    }
    // A loop among the children themselves.
    for &start in children {
        let mut stack: Vec<TaskId> = ctx
            .tasks
            .get(&start)
            .map(|task| task.depends_on.clone())
            .unwrap_or_default();
        let mut seen = BTreeSet::new();
        while let Some(next) = stack.pop() {
            if next == start {
                push(
                    PlanProblemCode::DependencyCycle,
                    Some(start),
                    format!("child task {start} waits for itself through the other children"),
                );
                break;
            }
            if children.contains(&next) && seen.insert(next) {
                stack.extend(
                    ctx.tasks
                        .get(&next)
                        .map(|task| task.depends_on.clone())
                        .unwrap_or_default(),
                );
            }
        }
    }
}

/// Every successor of the task is listed once with the children it waits
/// for in its place: all of them, or fewer with why; none has started.
fn check_successors(
    plan: &ReplacementPlan,
    ctx: &PlanContext<'_>,
    children: &BTreeSet<TaskId>,
    push: &mut Push<'_>,
) {
    let op = ctx.operation;
    let original: BTreeSet<TaskId> = op.conditions.successors.iter().copied().collect();
    let mut listed = BTreeSet::new();
    for successor in &plan.successors {
        let id = successor.task_id;
        if !original.contains(&id) || !listed.insert(id) {
            push(
                PlanProblemCode::SuccessorUnknown,
                Some(id),
                format!(
                    "task {id} is not a successor of task {} as the operation fixed it, or is listed twice",
                    op.task_id
                ),
            );
            continue;
        }
        let needs: BTreeSet<TaskId> = successor.needs.iter().copied().collect();
        if needs.is_empty() || !needs.is_subset(children) {
            push(
                PlanProblemCode::SuccessorNeedsUnclear,
                Some(id),
                format!(
                    "successor {id} must wait for one or more children of the plan, it names {:?}",
                    successor.needs
                ),
            );
        } else if needs.len() < children.len()
            && successor
                .reason
                .as_deref()
                .is_none_or(|reason| reason.trim().is_empty())
        {
            push(
                PlanProblemCode::SuccessorNeedsUnclear,
                Some(id),
                format!(
                    "successor {id} waits for only some of the children without a reason why the others hold nothing it needs"
                ),
            );
        }
        // An ended successor waits for nothing any more: its edge is left.
        if let Some(task) = ctx.tasks.get(&id)
            && !matches!(
                task.status,
                TaskStatus::Draft | TaskStatus::Submitted | TaskStatus::Ready
            )
            && !task.status.is_terminal()
        {
            push(
                PlanProblemCode::SuccessorStarted,
                Some(id),
                format!(
                    "successor {id} is {} already: its run is not the replacement's to change",
                    task.status.as_str()
                ),
            );
        }
    }
    for id in original.difference(&listed) {
        push(
            PlanProblemCode::SuccessorUnmapped,
            Some(*id),
            format!(
                "successor {id} of task {} is not given the children it waits for in its place",
                op.task_id
            ),
        );
    }
}

/// A plan as the queue keeps it: its revision for the operation (1 and
/// up, one per submit), the proposal it went with, its SHA-256, and when a
/// pass found it applicable.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlanRevision {
    pub revision: i64,
    pub proposal_id: ProposalId,
    pub plan: ReplacementPlan,
    pub hash: String,
    pub approved_at: Option<i64>,
}

/// A review that sent the parked run back, for its planner.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReviewFinding {
    /// The verdicts of the judgments that sent it back, `/`-joined.
    pub verdict: String,
    /// The findings it sent the run back with, a deciding agent's under
    /// the agent's name.
    pub reasons: Vec<String>,
}

/// A task that waits for the task being replaced, for its planner.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SuccessorBrief {
    pub task_id: TaskId,
    pub status: TaskStatus,
    pub title: String,
}

/// What the planner of an operation's request reads of the operation
/// beside its snapshot: the turns of the parked run by what each was for,
/// its reviews that did not pass, the successors of its task now, and the
/// latest plan submitted with its problems now.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReplanBrief {
    pub operation: ReplanOperation,
    pub turns: Vec<(String, usize)>,
    pub reviews: Vec<ReviewFinding>,
    pub successors: Vec<SuccessorBrief>,
    pub plan: Option<PlanRevision>,
    pub problems: Vec<PlanProblem>,
}

/// What a turn was for, by the first word of its `what`: the count the
/// planner reads per cause.
pub fn turn_cause(what: &str) -> String {
    what.split_whitespace().next().map_or_else(
        || "unlabeled".to_owned(),
        |word| {
            word.trim_end_matches(|c: char| !c.is_alphanumeric())
                .to_owned()
        },
    )
}

/// Where an operation with its planner goes as the trusted runtime settles
/// its request `request`, ended in `status` (`declined` with the latest
/// `request_declined`'s choice to continue and its reason, `exhausted`, or
/// `proposed` with its replacement proposal withdrawn and no planner
/// left), and why: only the planner's own choice to continue continues
/// the run; every other end leaves the operation `blocked` for a person,
/// with the work kept.
pub fn planning_end(
    request: RequestId,
    status: RequestStatus,
    declined: Option<(bool, String)>,
) -> (ReplanStatus, String) {
    match (status, declined) {
        (RequestStatus::Declined, Some((true, why))) => (
            ReplanStatus::ContinuingAfterPark,
            format!("the planner of request {request} chose to continue the run: {why}"),
        ),
        (RequestStatus::Declined, declined) => (
            ReplanStatus::Blocked,
            format!(
                "the planner of request {request} declined it without a replacement: {}",
                declined.map(|(_, why)| why).unwrap_or_default()
            ),
        ),
        (RequestStatus::Exhausted, _) => (
            ReplanStatus::Blocked,
            format!(
                "request {request} ran out of planners without a replacement; the inbox decides"
            ),
        ),
        _ => (
            ReplanStatus::Blocked,
            format!(
                "the replacement proposal of request {request} was withdrawn and no planner works on it; the inbox decides"
            ),
        ),
    }
}

/// The problems as the lines a revise or a refusal names.
pub fn describe(problems: &[PlanProblem]) -> Vec<String> {
    problems
        .iter()
        .map(|problem| format!("{}: {}", problem.code.as_str(), problem.reason))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::RunId;
    use crate::domain::task_replan::{
        Diagnosis, OriginalConditions, ReplanOrigin, ReplanStatus, acceptance_items,
    };

    fn op() -> ReplanOperation {
        let conditions = OriginalConditions {
            title: "big".into(),
            description: "d".into(),
            acceptance: acceptance_items("A1 the API\nA2 the docs"),
            paths: vec!["src/**".into()],
            verify: vec!["cargo test".into()],
            evidence: serde_json::json!([]),
            predecessors: vec![TaskId::new(5)],
            successors: vec![TaskId::new(20)],
            goal: None,
        };
        ReplanOperation {
            id: ReplanId::new(1),
            task_id: TaskId::new(10),
            run_id: RunId::new("run").unwrap(),
            goal_id: Some(GoalId::new(3)),
            origin: ReplanOrigin::Manual,
            status: ReplanStatus::Planning,
            status_reason: None,
            generation: 2,
            return_phase: None,
            reason: "too big".into(),
            task_version: conditions.version(),
            conditions,
            goal_acceptance_version: Some(4),
            diagnosis: Diagnosis::default(),
            snapshot_id: Some("s".into()),
            request_id: None,
            request_status: None,
            proposal_id: None,
            requested_by: "inbox".into(),
            requested_by_id: "inbox".into(),
            mappings: Vec::new(),
            replaced_by: Vec::new(),
            created_at: 0,
            updated_at: 0,
            closed_at: None,
        }
    }

    fn child(depends_on: &[i64]) -> PlanTask {
        PlanTask {
            status: TaskStatus::Submitted,
            goal_id: Some(GoalId::new(3)),
            acceptance: acceptance_items("C1 it holds"),
            verify: vec!["cargo test".into()],
            paths: vec!["src/a.rs".into()],
            evidence: Vec::new(),
            depends_on: depends_on.iter().copied().map(TaskId::new).collect(),
        }
    }

    fn plan(op: &ReplanOperation) -> ReplacementPlan {
        let item = |ordinal: usize, task: i64| PlanMapping {
            ordinal,
            hash: op.conditions.acceptance[ordinal - 1].hash.clone(),
            disposition: ItemDisposition::Split,
            children: vec![ChildItem {
                task_id: TaskId::new(task),
                ordinal: 1,
            }],
            landed_task_id: None,
            authority_ask_id: None,
            reason: None,
        };
        ReplacementPlan {
            operation_id: op.id,
            task_version: op.task_version.clone(),
            goal_acceptance_version: Some(4),
            choice: PlanChoice::Split,
            rationale: "two independent parts".into(),
            children: vec![
                PlanChild {
                    task_id: TaskId::new(11),
                    carry: CarrySource::SnapshotHead,
                    order: Some(1),
                },
                PlanChild {
                    task_id: TaskId::new(12),
                    carry: CarrySource::Base,
                    order: None,
                },
            ],
            mappings: vec![item(1, 11), item(2, 12)],
            successors: vec![PlanSuccessor {
                task_id: TaskId::new(20),
                needs: vec![TaskId::new(11), TaskId::new(12)],
                reason: None,
            }],
        }
    }

    fn context(op: &ReplanOperation) -> PlanContext<'_> {
        let mut tasks = BTreeMap::new();
        tasks.insert(TaskId::new(11), child(&[5]));
        tasks.insert(TaskId::new(12), child(&[5, 11]));
        tasks.insert(
            TaskId::new(5),
            PlanTask {
                status: TaskStatus::Ready,
                ..child(&[])
            },
        );
        tasks.insert(
            TaskId::new(20),
            PlanTask {
                status: TaskStatus::Draft,
                ..child(&[10])
            },
        );
        PlanContext {
            operation: op,
            task_version_now: op.task_version.clone(),
            goal_acceptance_version_now: Some(4),
            members: vec![TaskId::new(11), TaskId::new(12)],
            tasks,
            downstream: [TaskId::new(20)].into_iter().collect(),
            authorities: BTreeMap::new(),
        }
    }

    fn codes(plan: &ReplacementPlan, ctx: &PlanContext<'_>) -> Vec<PlanProblemCode> {
        check_plan(plan, ctx).into_iter().map(|p| p.code).collect()
    }

    #[test]
    fn a_plan_that_keeps_every_condition_check_and_dependency_has_no_problem() {
        let op = op();
        let ctx = context(&op);
        assert_eq!(check_plan(&plan(&op), &ctx), Vec::new());
    }

    #[test]
    fn an_unmapped_item_and_a_child_holding_nothing_are_refused() {
        let op = op();
        let ctx = context(&op);
        let mut plan = plan(&op);
        plan.mappings.pop();
        assert_eq!(
            codes(&plan, &ctx),
            vec![
                PlanProblemCode::ItemUnmapped,
                PlanProblemCode::ChildHoldsNothing
            ]
        );
    }

    #[test]
    fn a_deletion_needs_a_persons_scope_answer_about_the_operation() {
        let op = op();
        let mut ctx = context(&op);
        let mut plan = plan(&op);
        plan.mappings[1] = PlanMapping {
            disposition: ItemDisposition::Deleted,
            children: Vec::new(),
            authority_ask_id: Some(AskId::new(7)),
            ..plan.mappings[1].clone()
        };
        plan.mappings[0].children.push(ChildItem {
            task_id: TaskId::new(12),
            ordinal: 1,
        });
        assert_eq!(
            codes(&plan, &ctx),
            vec![PlanProblemCode::DeletionUnauthorized]
        );
        ctx.authorities.insert(
            AskId::new(7),
            DeletionAuthority {
                about_operation: true,
                scope: true,
                answered_by_person: false,
                answer: deletion_token(&op.conditions.acceptance[1]),
                options: vec![deletion_token(&op.conditions.acceptance[1]), "keep".into()],
            },
        );
        assert_eq!(
            codes(&plan, &ctx),
            vec![PlanProblemCode::DeletionUnauthorized]
        );
        // A person's answer that keeps the item, refuses or explains
        // around its token, or approves another item's deletion, approves
        // none of this one.
        let token = deletion_token(&op.conditions.acceptance[1]);
        let authority = ctx.authorities.get_mut(&AskId::new(7)).unwrap();
        authority.answered_by_person = true;
        for refused in [
            "keep".to_owned(),
            "do not delete anything".to_owned(),
            format!("keep; do not {token}"),
            format!("{token}, as asked"),
            deletion_token(&op.conditions.acceptance[0]),
            format!("delete:2:{}", "0".repeat(12)),
        ] {
            ctx.authorities.get_mut(&AskId::new(7)).unwrap().answer = refused.clone();
            assert_eq!(
                codes(&plan, &ctx),
                vec![PlanProblemCode::DeletionUnauthorized],
                "{refused}"
            );
        }
        ctx.authorities.get_mut(&AskId::new(7)).unwrap().answer = format!(" {token}\n");
        assert_eq!(codes(&plan, &ctx), Vec::new());
        // The token approves only as an option the ask offered.
        ctx.authorities.get_mut(&AskId::new(7)).unwrap().options = vec!["keep".into()];
        assert_eq!(
            codes(&plan, &ctx),
            vec![PlanProblemCode::DeletionUnauthorized]
        );
        ctx.authorities.get_mut(&AskId::new(7)).unwrap().options =
            vec![token.clone(), "keep".into()];
        // Asked of something else than the operation, or not for scope,
        // it approves nothing either.
        ctx.authorities.get_mut(&AskId::new(7)).unwrap().scope = false;
        assert_eq!(
            codes(&plan, &ctx),
            vec![PlanProblemCode::DeletionUnauthorized]
        );
    }

    #[test]
    fn a_deletion_token_names_the_item_by_its_ordinal_and_hash() {
        let items = acceptance_items("A1\nA2");
        let token = deletion_token(&items[1]);
        assert_eq!(token, format!("delete:2:{}", &items[1].hash[..12]));
        let chosen = DeletionAuthority {
            about_operation: true,
            scope: true,
            answered_by_person: true,
            answer: token.clone(),
            options: vec![deletion_token(&items[0]), token.clone(), "keep".into()],
        };
        assert!(approves_deletion(&chosen, &items[1]));
        assert!(!approves_deletion(&chosen, &items[0]));
        let around = DeletionAuthority {
            answer: format!("not {token}"),
            ..chosen
        };
        assert!(!approves_deletion(&around, &items[1]));
    }

    #[test]
    fn landed_needs_a_completed_task_not_the_parked_work() {
        let op = op();
        let mut ctx = context(&op);
        let mut plan = plan(&op);
        plan.mappings[0].disposition = ItemDisposition::Landed;
        plan.mappings[0].landed_task_id = Some(op.task_id);
        assert_eq!(codes(&plan, &ctx), vec![PlanProblemCode::LandedUnproven]);
        plan.mappings[0].landed_task_id = Some(TaskId::new(5));
        assert_eq!(codes(&plan, &ctx), vec![PlanProblemCode::LandedUnproven]);
        ctx.tasks.get_mut(&TaskId::new(5)).unwrap().status = TaskStatus::Completed;
        assert_eq!(codes(&plan, &ctx), Vec::new());
    }

    #[test]
    fn a_dropped_verification_or_evidence_and_missing_paths_are_refused() {
        let mut op = op();
        op.conditions.evidence = serde_json::json!([{"kind": "screenshot"}]);
        let mut ctx = context(&op);
        for task in [11, 12] {
            let child = ctx.tasks.get_mut(&TaskId::new(task)).unwrap();
            child.verify = vec!["true".into()];
        }
        ctx.tasks.get_mut(&TaskId::new(12)).unwrap().paths.clear();
        assert_eq!(
            codes(&plan(&op), &ctx),
            vec![
                PlanProblemCode::EvidenceDropped,
                PlanProblemCode::VerificationWeakened,
                PlanProblemCode::VerificationWeakened,
                PlanProblemCode::PathsMissing,
            ]
        );
    }

    #[test]
    fn saved_work_goes_to_one_child_in_a_given_order() {
        let op = op();
        let ctx = context(&op);
        let mut plan = plan(&op);
        plan.children[1].carry = CarrySource::SnapshotHead;
        plan.children[1].order = Some(1);
        assert_eq!(
            codes(&plan, &ctx),
            vec![PlanProblemCode::CarryUnclear, PlanProblemCode::CarryUnclear]
        );
        plan.children[1].carry = CarrySource::SnapshotDiff;
        plan.children[1].order = None;
        assert_eq!(codes(&plan, &ctx), vec![PlanProblemCode::CarryUnclear]);
        plan.children[1].order = Some(2);
        assert_eq!(codes(&plan, &ctx), Vec::new());
    }

    #[test]
    fn a_child_waiting_for_the_task_or_its_successor_or_in_a_loop_is_refused() {
        let op = op();
        let mut ctx = context(&op);
        ctx.tasks
            .get_mut(&TaskId::new(11))
            .unwrap()
            .depends_on
            .extend([TaskId::new(20), TaskId::new(12)]);
        let found = codes(&plan(&op), &ctx);
        assert!(
            found
                .iter()
                .all(|code| *code == PlanProblemCode::DependencyCycle),
            "{found:?}"
        );
        assert_eq!(found.len(), 3, "{found:?}");
        let mut ctx = context(&op);
        ctx.tasks
            .get_mut(&TaskId::new(11))
            .unwrap()
            .depends_on
            .clear();
        assert_eq!(
            codes(&plan(&op), &ctx),
            vec![PlanProblemCode::PredecessorDropped]
        );
    }

    #[test]
    fn every_successor_waits_for_named_children_and_fewer_need_a_reason() {
        let op = op();
        let mut ctx = context(&op);
        let mut plan = plan(&op);
        plan.successors[0].needs = vec![TaskId::new(11)];
        assert_eq!(
            codes(&plan, &ctx),
            vec![PlanProblemCode::SuccessorNeedsUnclear]
        );
        plan.successors[0].reason = Some("it uses only the API".into());
        assert_eq!(codes(&plan, &ctx), Vec::new());
        // A canceled successor waits for nothing: it has not started.
        ctx.tasks.get_mut(&TaskId::new(20)).unwrap().status = TaskStatus::Canceled;
        assert_eq!(codes(&plan, &ctx), Vec::new());
        ctx.tasks.get_mut(&TaskId::new(20)).unwrap().status = TaskStatus::InProgress;
        assert_eq!(codes(&plan, &ctx), vec![PlanProblemCode::SuccessorStarted]);
        plan.successors.clear();
        assert_eq!(codes(&plan, &ctx), vec![PlanProblemCode::SuccessorUnmapped]);
    }

    #[test]
    fn conditions_or_a_goal_acceptance_changed_since_make_the_plan_stale() {
        let op = op();
        let mut ctx = context(&op);
        ctx.task_version_now = "changed".into();
        assert_eq!(codes(&plan(&op), &ctx), vec![PlanProblemCode::VersionStale]);
        let mut ctx = context(&op);
        ctx.goal_acceptance_version_now = Some(5);
        assert_eq!(codes(&plan(&op), &ctx), vec![PlanProblemCode::VersionStale]);
        let ctx = context(&op);
        let mut stale = plan(&op);
        stale.mappings[0].hash = "other".into();
        assert_eq!(codes(&stale, &ctx), vec![PlanProblemCode::VersionStale]);
    }

    #[test]
    fn members_and_children_are_one_set_of_the_same_goal() {
        let op = op();
        let mut ctx = context(&op);
        ctx.members.push(TaskId::new(13));
        ctx.tasks.get_mut(&TaskId::new(12)).unwrap().goal_id = None;
        assert_eq!(
            codes(&plan(&op), &ctx),
            vec![
                PlanProblemCode::GoalMismatch,
                PlanProblemCode::MemberNotChild
            ]
        );
    }

    #[test]
    fn a_child_canceled_or_started_is_refused() {
        let op = op();
        let mut ctx = context(&op);
        assert_eq!(codes(&plan(&op), &ctx), Vec::new());
        ctx.tasks.get_mut(&TaskId::new(11)).unwrap().status = TaskStatus::Draft;
        assert_eq!(codes(&plan(&op), &ctx), Vec::new());
        for status in [
            TaskStatus::Canceled,
            TaskStatus::Ready,
            TaskStatus::InProgress,
            TaskStatus::Completed,
        ] {
            ctx.tasks.get_mut(&TaskId::new(12)).unwrap().status = status;
            let problems = check_plan(&plan(&op), &ctx);
            assert_eq!(
                problems
                    .iter()
                    .map(|p| (p.code, p.task_id))
                    .collect::<Vec<_>>(),
                vec![(PlanProblemCode::ChildNotPending, Some(TaskId::new(12)))],
                "{status:?}"
            );
        }
    }

    #[test]
    fn a_pass_is_judged_on_the_plan_its_actions_leave() {
        let op = op();
        let plan = plan(&op);
        let cancel = PlanReviewAction::CancelDuplicate {
            task_id: TaskId::new(12),
            duplicate_of: TaskId::new(30),
        };
        let wait = |on: i64| PlanReviewAction::AddDependency {
            task_id: TaskId::new(11),
            depends_on: TaskId::new(on),
        };
        let lower = PlanReviewAction::LowerPriority {
            task_id: TaskId::new(11),
            priority: crate::domain::Priority::Low,
        };
        let after =
            |actions: &[PlanReviewAction]| codes(&plan, &context(&op).after_actions(actions));
        assert_eq!(after(&[lower, wait(5)]), Vec::new());
        assert_eq!(after(&[cancel]), vec![PlanProblemCode::ChildNotPending]);
        // Actions on the successor or the predecessor are not the pass's
        // to apply: they leave the plan as it is.
        let outside = |task: i64| PlanReviewAction::CancelDuplicate {
            task_id: TaskId::new(task),
            duplicate_of: TaskId::new(30),
        };
        assert_eq!(after(&[outside(20), outside(5)]), Vec::new());
        let mut dropped = context(&op);
        dropped
            .tasks
            .get_mut(&TaskId::new(11))
            .unwrap()
            .depends_on
            .clear();
        assert_eq!(
            codes(&plan, &dropped.after_actions(&[outside(5)])),
            vec![PlanProblemCode::PredecessorDropped]
        );
        assert_eq!(after(&[wait(20)]), vec![PlanProblemCode::DependencyCycle]);
        // Task 12 already waits for 11: both now wait for themselves.
        assert_eq!(
            after(&[wait(12)]),
            vec![PlanProblemCode::DependencyCycle; 2]
        );
    }

    #[test]
    fn only_the_planners_choice_to_continue_continues_the_run() {
        let request = RequestId::new(4);
        let end = |status, declined| planning_end(request, status, declined).0;
        assert_eq!(
            end(
                RequestStatus::Declined,
                Some((true, "a local defect".into()))
            ),
            ReplanStatus::ContinuingAfterPark
        );
        for (status, declined) in [
            (
                RequestStatus::Declined,
                Some((false, "nothing to plan".into())),
            ),
            (RequestStatus::Declined, None),
            (RequestStatus::Exhausted, None),
            (RequestStatus::Proposed, None),
        ] {
            assert_eq!(end(status, declined), ReplanStatus::Blocked, "{status:?}");
        }
        let (_, why) = planning_end(
            request,
            RequestStatus::Declined,
            Some((true, "a local defect".into())),
        );
        assert!(
            why.contains("request 4") && why.contains("a local defect"),
            "{why}"
        );
    }

    #[test]
    fn a_turn_is_counted_by_the_first_word_of_what_it_was_for() {
        assert_eq!(turn_cause("revise: fix the test"), "revise");
        assert_eq!(turn_cause("resume after the park"), "resume");
        assert_eq!(turn_cause(""), "unlabeled");
    }

    #[test]
    fn a_plan_parses_from_its_json() {
        let op = op();
        let text = serde_json::to_string(&plan(&op)).unwrap();
        assert_eq!(ReplacementPlan::parse(&text).unwrap(), plan(&op));
        assert!(ReplacementPlan::parse("{}").is_err());
    }
}
