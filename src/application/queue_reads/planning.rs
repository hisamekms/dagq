//! The planning context's reads of the queue (計画管理 in
//! docs/design/architecture.md): `list`, `candidates`, `graph`,
//! `goal list`, `goal show`, `lint`, `search` and `related`.
//!
//! Each read takes the planning context's [`TaskStore`] and only the reads
//! the other contexts open to every context: the execution context's
//! [`RunLog`] (the open claim deferrals of `candidates` and `graph`) and
//! its own [`PlanningRecords`] (the search index and the related tasks).

use anyhow::Result;
use serde_json::{Value, json};

use super::{
    CandidatesRead, GoalListRead, GoalShowRead, GraphRead, LintRead, ListRead, RelatedRead,
    SearchRead,
};
use crate::application::claim_view::{
    AgingInputs, ClaimAging, ClaimExclusions, Deferrals, claim_view, class_aging_secs,
    open_deferrals,
};
use crate::application::{
    Clock, DependencyGraph, PlanningRecords, RunLog, StatusFilter, TaskQuery, TaskStore,
    claim_candidates, dependency_graph,
};
use crate::domain::claim_order;
use crate::domain::search::{self, SearchQuery};
use crate::domain::{
    ChangeSet, GoalDetail, GoalId, Proposal, ProposalId, TagSet, TaskId, TaskStatus, TitlePrefixes,
};

/// What the planning reads take beyond the queue's records that the
/// composition root assembles: the repository's sets of changes and goal
/// tags, the claims' holds, the SVG the host's d2 draws and the command
/// line's views.
pub trait PlanningSources<Q: ?Sized> {
    /// The repository's set of changes (ADR-t980-1), none without a
    /// checkout.
    fn changes(&self, queue: &Q) -> Result<Option<ChangeSet>>;
    /// The repository's prefixes of a task's title, none without a
    /// checkout.
    fn title_prefixes(&self, queue: &Q) -> Result<Option<TitlePrefixes>>;
    /// The repository's set of goal tags (ADR-t1639-1 decision 6), none
    /// without a checkout.
    fn goal_tags(&self, queue: &Q) -> Result<Option<TagSet>>;
    /// What holds the claims now as the supervisors recorded it
    /// (`candidates`' `held`, ADR-t1992-1).
    fn claim_holds(&self, queue: &Q) -> Result<Vec<Value>>;
    /// The tokens of the supervisors alive now, with a fresh heartbeat:
    /// those whose claims the shown order is the order of.
    fn live_supervisors(&self, queue: &Q) -> Result<Vec<String>>;
    /// What the threshold of the aging across the execution classes is
    /// chosen from (`[supervisor] class_aging`, ADR-t1487-2).
    fn class_aging(&self, queue: &Q) -> Result<AgingInputs>;
    /// The clock the waits of the aging are measured by.
    fn clock<'q>(&self, queue: &'q Q) -> &'q dyn Clock;
    /// The SVG the host's d2 draws from `source`.
    fn render_svg(&self, source: &str) -> Result<String>;
    /// What the command line prints as is (`graph --format d2|svg`).
    fn raw_stdout(&self, text: String) -> Value;
    /// `goal show`'s compact form, without `--full`.
    fn goal_view(&self, detail: &GoalDetail) -> Value;
}

/// `list`.
pub fn list(queue: &(impl TaskStore + ?Sized), read: &ListRead) -> Result<Value> {
    Ok(serde_json::to_value(queue.list(&TaskQuery {
        status: list_status(read)?,
        goal_id: read.goal.map(GoalId::new),
        limit: usize::try_from(read.limit)?,
        before: read.before.map(TaskId::new),
        full: read.full,
    })?)?)
}

/// `candidates`: the order a claim takes them with the deferred tasks
/// apart (the rule's order with them kept, `ignore_deferrals`), what
/// holds the claims and the ready tasks out of the order by why
/// (`excluded`).
pub fn candidates<Q: TaskStore + RunLog + ?Sized>(
    queue: &Q,
    sources: &(impl PlanningSources<Q> + ?Sized),
    read: &CandidatesRead,
) -> Result<Value> {
    let input = queue.graph_input()?;
    let excluded = ClaimExclusions::of(&input);
    let graph = dependency_graph(input, None);
    let deferrals = if read.ignore_deferrals {
        Deferrals::Ignored
    } else {
        Deferrals::Excluded
    };
    // The rule's order shows no aging: nothing to read for it.
    let aging = match deferrals {
        Deferrals::Excluded => claim_aging(queue, sources, &graph)?,
        Deferrals::Ignored => ClaimAging::default(),
    };
    let view = claim_view(
        claim_candidates(queue.candidates()?, &graph),
        |candidate| candidate.task.id(),
        open_deferrals(queue, || sources.live_supervisors(queue))?,
        deferrals,
        &aging,
    );
    let mut value = json!({
        "candidates": view.candidates,
        "deferred": view.deferred,
        "held": sources.claim_holds(queue)?,
        "excluded": excluded,
    });
    if !view.aged.is_empty() {
        value["aged"] = serde_json::to_value(view.aged)?;
    }
    Ok(value)
}

/// What the aging of the shown order compares ([`ClaimAging`]): the
/// candidates of `graph`, the queue's whole graph, with their class,
/// effective priority and the time each became claimable.
fn claim_aging<Q: TaskStore + ?Sized>(
    queue: &Q,
    sources: &(impl PlanningSources<Q> + ?Sized),
    graph: &DependencyGraph,
) -> Result<ClaimAging> {
    let ready_at = if graph.candidates.is_empty() {
        Default::default()
    } else {
        queue.ready_at(&graph.candidates)?
    };
    let inputs = sources.class_aging(queue)?;
    let secs = class_aging_secs(inputs.file.as_ref(), &inputs.registered);
    let now_ms = crate::application::unix_millis(sources.clock(queue).system_time());
    Ok(ClaimAging {
        candidates: graph
            .candidates
            .iter()
            .filter_map(|id| {
                let node = graph.tasks.iter().find(|node| node.id == *id)?;
                Some(claim_order::Candidate {
                    id: *id,
                    class: node.execution_class,
                    priority: node.effective_priority,
                    ready_at_ms: ready_at.get(id).copied(),
                })
            })
            .collect(),
        secs,
        now_ms,
    })
}

/// `graph`: the dependency graph as JSON, or its near-term diagram as d2
/// source or the SVG the host draws from it.
pub fn graph<Q: TaskStore + RunLog + ?Sized>(
    queue: &Q,
    sources: &(impl PlanningSources<Q> + ?Sized),
    read: &GraphRead,
) -> Result<Value> {
    let goal = read.goal.map(GoalId::new);
    Ok(match read.format.as_str() {
        "json" => {
            let input = queue.graph_input()?;
            // The aging compares the queue's candidates, whatever the goal.
            let aging = claim_aging(queue, sources, &dependency_graph(input.clone(), None))?;
            let mut graph = dependency_graph(input, goal);
            let view = claim_view(
                std::mem::take(&mut graph.candidates),
                |id| *id,
                open_deferrals(queue, || sources.live_supervisors(queue))?,
                Deferrals::Excluded,
                &aging,
            );
            let mut value = serde_json::to_value(graph)?;
            value["candidates"] = serde_json::to_value(view.candidates)?;
            value["deferred"] = serde_json::to_value(view.deferred)?;
            if !view.aged.is_empty() {
                value["aged"] = serde_json::to_value(view.aged)?;
            }
            value
        }
        format => {
            let (source, _) = graph_diagram(queue, goal)?;
            let text = if format == "svg" {
                sources.render_svg(&source)?
            } else {
                source
            };
            sources.raw_stdout(text)
        }
    })
}

/// The near-term dependency diagram of `graph --format d2|svg` (ADR-0077)
/// as d2 source, and the tasks it shows. It is drawn from the graph before
/// the supervisor's deferred tasks are split from `candidates`, so a
/// deferred task is still marked startable.
pub fn graph_diagram(
    queue: &(impl TaskStore + ?Sized),
    goal_id: Option<GoalId>,
) -> Result<(String, Vec<TaskId>)> {
    let input = queue.graph_input()?;
    let graph = dependency_graph(input.clone(), goal_id);
    let titles = queue
        .list_goals()?
        .into_iter()
        .map(|goal| (goal.id, goal.title))
        .collect();
    let diagram = match goal_id {
        // The goal's prerequisites and critical steps outside it are
        // drawn too (ADR-0077 decision 1).
        Some(goal) => crate::application::diagram::near_term_in_goal(
            &dependency_graph(input, None),
            &graph,
            goal,
            &titles,
        ),
        None => crate::application::diagram::near_term(&graph, &titles),
    };
    Ok((diagram.to_d2(), diagram.task_ids()))
}

/// `goal list`.
pub fn goal_list(queue: &(impl TaskStore + ?Sized), read: &GoalListRead) -> Result<Value> {
    Ok(serde_json::to_value(crate::domain::goal::list(
        queue.list_goals()?,
        &read.tags()?,
    ))?)
}

/// `goal show`: the whole detail with `full`, else the command line's
/// compact view.
pub fn goal_show<Q: TaskStore + ?Sized>(
    queue: &mut Q,
    sources: &(impl PlanningSources<Q> + ?Sized),
    read: &GoalShowRead,
) -> Result<Value> {
    let detail = queue.show_goal(GoalId::new(read.id))?;
    Ok(if read.full {
        serde_json::to_value(detail)?
    } else {
        sources.goal_view(&detail)
    })
}

/// `lint`: the named tasks and the proposals' tasks against the
/// repository's sets of changes and goal tags.
pub fn lint<Q: TaskStore + ?Sized>(
    queue: &Q,
    sources: &(impl PlanningSources<Q> + ?Sized),
    read: &LintRead,
) -> Result<Value> {
    let mut proposals = Vec::with_capacity(read.proposals.len());
    for id in &read.proposals {
        proposals.push(queue.show_proposal(ProposalId::new(*id))?);
    }
    let targets = lint_targets(&read.tasks, &proposals);
    // The repository's set of changes holds the tasks lint checks
    // (ADR-t980-1).
    let mut input = queue.lint_input(&targets)?;
    input.changes = sources.changes(queue)?;
    input.title_prefixes = sources.title_prefixes(queue)?;
    input.goal_tags = sources.goal_tags(queue)?;
    Ok(json!({"tasks": targets, "violations": crate::domain::lint::lint(&input)}))
}

/// `search`.
pub fn search(queue: &(impl PlanningRecords + ?Sized), read: &SearchRead) -> Result<Value> {
    Ok(serde_json::to_value(
        queue.search_documents(&SearchQuery {
            terms: read.query.clone(),
            kinds: read
                .kinds
                .iter()
                .map(|kind| kind.parse())
                .collect::<Result<_, _>>()?,
            statuses: read
                .status
                .iter()
                .map(|value| search::parse_status(value))
                .collect::<Result<_, _>>()?,
            goal_id: read.goal.map(GoalId::new),
            limit: usize::try_from(read.limit)?,
            full: read.full,
        })?,
    )?)
}

/// `related`: the statuses as the store names them, whatever spacing they
/// were given with.
pub fn related(queue: &(impl PlanningRecords + ?Sized), read: &RelatedRead) -> Result<Value> {
    let statuses = read
        .status
        .iter()
        .map(|status| Ok(status.trim().parse::<TaskStatus>()?.as_str().to_owned()))
        .collect::<Result<Vec<_>>>()?;
    Ok(serde_json::to_value(queue.related_tasks(
        TaskId::new(read.task),
        &statuses,
        usize::try_from(read.limit)?,
    )?)?)
}

/// `list`'s statuses: every one with `--all`, the open ones without
/// `--status`, else those named.
pub(super) fn list_status(read: &ListRead) -> Result<StatusFilter> {
    Ok(if read.all {
        StatusFilter::Any
    } else if read.status.is_empty() {
        StatusFilter::Open
    } else {
        StatusFilter::Only(
            read.status
                .iter()
                .map(|value| value.trim().parse::<TaskStatus>())
                .collect::<Result<_, _>>()?,
        )
    })
}

/// The tasks `lint` checks: those named, then the proposals' tasks, each
/// once in the order first named.
pub(super) fn lint_targets(tasks: &[i64], proposals: &[Proposal]) -> Vec<TaskId> {
    let mut targets: Vec<TaskId> = tasks.iter().copied().map(TaskId::new).collect();
    for proposal in proposals {
        targets.extend_from_slice(proposal.task_ids());
    }
    let mut seen = std::collections::HashSet::new();
    targets.retain(|id| seen.insert(*id));
    targets
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::application::queue_reads::fakes::Records;

    fn read(status: &[&str], limit: u32) -> RelatedRead {
        RelatedRead {
            task: 4,
            status: status.iter().map(|status| (*status).to_owned()).collect(),
            limit,
        }
    }

    #[test]
    fn related_asks_the_records_for_the_named_statuses_trimmed_and_the_limit() {
        let records = Records::default();
        let page = related(&records, &read(&[" ready ", "completed"], 3)).unwrap();
        assert_eq!(page, json!({"task_id": 4, "related": [], "total": 0}));
        assert_eq!(
            *records.related.borrow(),
            [(
                TaskId::new(4),
                vec!["ready".to_owned(), "completed".to_owned()],
                3
            )]
        );
    }

    #[test]
    fn related_refuses_an_unknown_status_before_reading_the_records() {
        let records = Records::default();
        assert!(related(&records, &read(&["gone"], 3)).is_err());
        assert!(records.related.borrow().is_empty());
    }
}
