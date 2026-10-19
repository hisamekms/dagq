//! The measurement's fold of the attributes (`domain::ledger::attributes`,
//! docs/design/attribute-records.md) set against what the store reads now,
//! for the queue tests of the records and of the paths that write them.

use std::collections::BTreeMap;

use dagq::application::{GraphInput, TaskReads, dependency_graph, diagram};
use dagq::domain::GoalId;
use dagq::domain::ledger::attributes::{Attributes, Folded, fold};
use dagq::infrastructure::sqlite::SqliteQueue;

/// The attributes the queue's records fold to now.
pub fn folded(queue: &SqliteQueue) -> Attributes {
    match fold(&queue.all_events().unwrap(), None) {
        Folded::Attributes(attributes) => attributes,
        Folded::BeforeBaseline => panic!("before the baseline"),
    }
}

/// The near-term diagram `report` draws from a graph and the goals' titles.
fn near_term(graph: GraphInput, goals: &[dagq::domain::GoalSummary]) -> diagram::Diagram {
    let titles: BTreeMap<GoalId, String> = goals
        .iter()
        .map(|goal| (goal.id, goal.title.clone()))
        .collect();
    diagram::near_term(&dependency_graph(graph, None), &titles)
}

/// What the fold gives equals what the store reads now: the lookups of
/// `stats` and `kpi`, the goals as `list_goals` gives them, every task's
/// status, the graph `report` reads (its tasks and candidates) and the
/// near-term diagram drawn from it.
pub fn assert_folded_matches_state(queue: &mut SqliteQueue) {
    let folded = folded(queue);
    assert_eq!(folded.task_goals(), queue.task_goals().unwrap());
    assert_eq!(folded.task_titles(), queue.task_titles().unwrap());
    assert_eq!(folded.task_changes(), queue.task_changes().unwrap());
    assert_eq!(folded.draft_origins(), queue.draft_origins().unwrap());
    let goals = queue.list_goals().unwrap();
    assert_eq!(folded.goal_summaries(), goals);
    for (&id, task) in &folded.tasks {
        assert_eq!(
            task.status,
            queue.show(id).unwrap().task.status(),
            "task {id}"
        );
    }
    let graph = queue.graph_input().unwrap();
    let from_records = GraphInput::folded(&folded);
    assert_eq!(from_records, graph);
    assert_eq!(
        near_term(from_records, &folded.goal_summaries()),
        near_term(graph, &goals)
    );
}
