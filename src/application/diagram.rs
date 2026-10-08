//! The near-term dependency diagram (ADR-0077): which unfinished tasks to
//! draw ([`select`]), where each one and each goal frame goes ([`layout`]),
//! and the d2 source that fixes those coordinates ([`Diagram::to_d2`]).
//! Everything here is pure; drawing the SVG from the source is the host's
//! `d2 --layout=tala` (`infrastructure::d2`).

use std::collections::{BTreeMap, BTreeSet};

use serde::Serialize;

use super::{DependencyGraph, GraphNode, WaitFor};
use crate::domain::{GoalId, Priority, TaskId, TaskStatus};

/// Horizontal distance between the left edges of two adjacent columns.
pub const COLUMN_WIDTH: i64 = 300;
/// Width and height of a task's box.
pub const NODE_WIDTH: i64 = 240;
pub const NODE_HEIGHT: i64 = 64;
/// Vertical distance between the top edges of two rows of one band.
pub const ROW_HEIGHT: i64 = 88;
/// Room for a goal frame's heading above its first row.
pub const HEADER_HEIGHT: i64 = 36;
/// How far a goal frame reaches past the boxes it holds.
pub const FRAME_PADDING: i64 = 16;
/// Vertical gap between two lanes of bands.
pub const LANE_GAP: i64 = 40;
/// Distance of the whole drawing from the origin.
pub const MARGIN: i64 = 20;
/// Pixels one width unit of a label takes (an ASCII character is one
/// unit, a wide one two) at [`FONT_SIZE`].
const UNIT_PX: i64 = 8;
const FONT_SIZE: i64 = 14;

/// Why a task is drawn (ADR-0077 decision 1), in the order checked.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Reason {
    InProgress,
    Priority,
    Critical,
    /// A prerequisite, one level up, of a task drawn for another reason.
    Prerequisite,
}

/// The four colours of a box (ADR-0077 decision 3).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Tone {
    InProgress,
    /// `interrupt` or `urgent`.
    Pressing,
    High,
    /// `normal` or lower.
    Normal,
}

impl Tone {
    fn of(node: &GraphNode) -> Self {
        if node.status == TaskStatus::InProgress {
            Self::InProgress
        } else if node.effective_priority >= Priority::Urgent {
            Self::Pressing
        } else if node.effective_priority == Priority::High {
            Self::High
        } else {
            Self::Normal
        }
    }

    /// Fill and stroke of the box.
    pub const fn colors(self) -> (&'static str, &'static str) {
        match self {
            Self::InProgress => ("#dbeafe", "#1d4ed8"),
            Self::Pressing => ("#fee2e2", "#b91c1c"),
            Self::High => ("#fef3c7", "#b45309"),
            Self::Normal => ("#f3f4f6", "#6b7280"),
        }
    }

    const fn legend(self) -> &'static str {
        match self {
            Self::InProgress => "in_progress",
            Self::Pressing => "interrupt / urgent",
            Self::High => "high",
            Self::Normal => "normal or lower",
        }
    }
}

/// Stroke of an edge on the critical chain.
pub const CRITICAL_STROKE: &str = "#dc2626";
/// Stroke of the other edges and of a goal frame.
const EDGE_STROKE: &str = "#6b7280";
const FRAME_FILL: &str = "#fafafa";
const FRAME_STROKE: &str = "#9ca3af";
/// What a startable task's label starts with.
pub const STARTABLE_MARK: &str = "▶ ";

/// The tasks to draw, each with why: (i) in progress, (ii) an effective
/// priority of `high` or above, (iii) on the critical chain, and (iv) what
/// those still wait for, one level only: an unfinished predecessor, or the
/// unfinished tasks of a goal not closed as achieved.
pub fn select(graph: &DependencyGraph) -> BTreeMap<TaskId, Reason> {
    select_in(graph, None)
}

/// [`select`] with (i) and (ii) narrowed to the tasks of `goal`; (iii)
/// is every task of `graph.critical` and (iv) comes from all of
/// `graph.tasks`, so a prerequisite or a critical step outside the goal is
/// still drawn. `None` narrows nothing.
pub fn select_in(graph: &DependencyGraph, goal: Option<GoalId>) -> BTreeMap<TaskId, Reason> {
    let critical: BTreeSet<TaskId> = graph.critical.iter().copied().collect();
    let in_goal = |node: &GraphNode| goal.is_none() || node.goal_id == goal;
    let mut chosen: BTreeMap<TaskId, Reason> = graph
        .tasks
        .iter()
        .filter_map(|node| {
            let reason = if in_goal(node) && node.status == TaskStatus::InProgress {
                Reason::InProgress
            } else if in_goal(node) && node.effective_priority >= Priority::High {
                Reason::Priority
            } else if critical.contains(&node.id) {
                Reason::Critical
            } else {
                return None;
            };
            Some((node.id, reason))
        })
        .collect();
    let open: BTreeSet<TaskId> = graph.tasks.iter().map(|node| node.id).collect();
    let mut prerequisites = BTreeSet::new();
    for node in graph
        .tasks
        .iter()
        .filter(|node| chosen.contains_key(&node.id))
    {
        for wait in &node.ready_after {
            match *wait {
                WaitFor::Task(id) if open.contains(&id) => {
                    prerequisites.insert(id);
                }
                WaitFor::Task(_) => {}
                WaitFor::Goal { goal } => prerequisites.extend(
                    graph
                        .tasks
                        .iter()
                        .filter(|member| member.goal_id == Some(goal))
                        .map(|member| member.id),
                ),
            }
        }
    }
    for id in prerequisites {
        chosen.entry(id).or_insert(Reason::Prerequisite);
    }
    chosen
}

/// A task's box: its column (dependency depth), and where it is drawn.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PlacedTask {
    pub id: TaskId,
    pub title: String,
    pub goal_id: Option<GoalId>,
    pub reason: Reason,
    pub tone: Tone,
    /// No dependency is left: the task is a claim candidate.
    pub startable: bool,
    pub column: usize,
    pub left: i64,
    pub top: i64,
}

/// A goal frame, the rectangle behind its tasks' boxes (not a container).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Frame {
    /// `None`: the tasks outside any goal.
    pub goal_id: Option<GoalId>,
    /// The heading, already cut to the frame's width.
    pub heading: String,
    pub left: i64,
    pub top: i64,
    pub width: i64,
    pub height: i64,
}

/// A dependency drawn from `from` (a task or a goal's frame) to the task
/// that waits for it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Edge {
    pub from: EdgeEnd,
    pub to: TaskId,
    /// Both ends are consecutive tasks of the critical chain.
    pub critical: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum EdgeEnd {
    Task(TaskId),
    Goal(GoalId),
}

/// The laid-out near-term diagram.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Diagram {
    /// In ID order.
    pub tasks: Vec<PlacedTask>,
    /// Top to bottom, then left to right.
    pub frames: Vec<Frame>,
    pub edges: Vec<Edge>,
    /// Where the legend starts: below every frame.
    pub legend_top: i64,
}

impl Diagram {
    /// The IDs of the tasks drawn, ascending.
    pub fn task_ids(&self) -> Vec<TaskId> {
        self.tasks.iter().map(|task| task.id).collect()
    }
}

/// Select the near-term tasks of `graph` and place them (ADR-0077
/// decision 2). `goal_titles` names the goals' frames.
pub fn near_term(graph: &DependencyGraph, goal_titles: &BTreeMap<GoalId, String>) -> Diagram {
    layout(graph, &select(graph), goal_titles)
}

/// The near-term diagram of `graph --goal`: `whole` is the graph over
/// every unfinished task and `narrowed` the one for the goal. The tasks
/// drawn for (i) and (ii) are the goal's, the critical chain is
/// `narrowed`'s (it starts in the goal and may leave it), and the
/// prerequisites (iv) come from `whole`, each in its own goal's frame.
pub fn near_term_in_goal(
    whole: &DependencyGraph,
    narrowed: &DependencyGraph,
    goal: GoalId,
    goal_titles: &BTreeMap<GoalId, String>,
) -> Diagram {
    let graph = DependencyGraph {
        tasks: whole.tasks.clone(),
        candidates: whole.candidates.clone(),
        critical: narrowed.critical.clone(),
    };
    layout(&graph, &select_in(&graph, Some(goal)), goal_titles)
}

/// Place `chosen` tasks of `graph`: the column is the longest chain of
/// drawn prerequisites before the task; each goal is a band of rows
/// spanning its tasks' columns; bands of goals joined by a drawn
/// dependency come first, in order of their first column, and goals joined
/// to no other come last; a band shares the previous band's lane when
/// their columns do not overlap.
pub fn layout(
    graph: &DependencyGraph,
    chosen: &BTreeMap<TaskId, Reason>,
    goal_titles: &BTreeMap<GoalId, String>,
) -> Diagram {
    let nodes: BTreeMap<TaskId, &GraphNode> = graph
        .tasks
        .iter()
        .filter(|node| chosen.contains_key(&node.id))
        .map(|node| (node.id, node))
        .collect();
    let mut members: BTreeMap<GoalId, Vec<TaskId>> = BTreeMap::new();
    for node in nodes.values() {
        if let Some(goal) = node.goal_id {
            members.entry(goal).or_default().push(node.id);
        }
    }
    // The drawn prerequisites of each drawn task, and the edges.
    let mut prerequisites: BTreeMap<TaskId, BTreeSet<TaskId>> = BTreeMap::new();
    let mut edges = Vec::new();
    let critical_steps: BTreeSet<(TaskId, TaskId)> = graph
        .critical
        .windows(2)
        .map(|pair| (pair[0], pair[1]))
        .collect();
    for node in nodes.values() {
        let before = prerequisites.entry(node.id).or_default();
        for wait in &node.ready_after {
            match *wait {
                WaitFor::Task(id) if nodes.contains_key(&id) => {
                    before.insert(id);
                    edges.push(Edge {
                        from: EdgeEnd::Task(id),
                        to: node.id,
                        critical: critical_steps.contains(&(id, node.id)),
                    });
                }
                WaitFor::Task(_) => {}
                WaitFor::Goal { goal } => {
                    let Some(ids) = members.get(&goal) else {
                        continue;
                    };
                    before.extend(ids.iter().copied().filter(|id| *id != node.id));
                    edges.push(Edge {
                        from: EdgeEnd::Goal(goal),
                        to: node.id,
                        critical: ids
                            .iter()
                            .any(|id| critical_steps.contains(&(*id, node.id))),
                    });
                }
            }
        }
    }
    let columns = depths(&prerequisites);
    // One band per goal, the tasks outside any goal as a band of their own.
    let mut bands: BTreeMap<Option<GoalId>, Vec<TaskId>> = BTreeMap::new();
    for node in nodes.values() {
        bands.entry(node.goal_id).or_default().push(node.id);
    }
    let order = band_order(&bands, &prerequisites, &nodes, &columns);
    let candidates: BTreeSet<TaskId> = graph.candidates.iter().copied().collect();
    let mut placed = BTreeMap::new();
    let mut frames = Vec::new();
    // Lanes top to bottom: each band either joins the last lane, right of
    // everything there, or opens a new lane below.
    let mut lane_top = MARGIN;
    let mut lane_height = 0;
    let mut lane_last_column: Option<usize> = None;
    for key in order {
        let ids = &bands[&key];
        let first = ids.iter().map(|id| columns[id]).min().unwrap_or(0);
        let last = ids.iter().map(|id| columns[id]).max().unwrap_or(0);
        let mut rows: BTreeMap<usize, i64> = BTreeMap::new();
        let mut row_of = BTreeMap::new();
        for id in ids {
            let row = rows.entry(columns[id]).or_default();
            row_of.insert(*id, *row);
            *row += 1;
        }
        let row_count = rows.values().copied().max().unwrap_or(1);
        let height = HEADER_HEIGHT + (row_count - 1) * ROW_HEIGHT + NODE_HEIGHT + FRAME_PADDING;
        if lane_last_column.is_some_and(|column| first <= column) {
            lane_top += lane_height + LANE_GAP;
            lane_height = 0;
        }
        lane_height = lane_height.max(height);
        lane_last_column = Some(last);
        let column_left = |column: usize| MARGIN + FRAME_PADDING + column as i64 * COLUMN_WIDTH;
        let width = (last - first) as i64 * COLUMN_WIDTH + NODE_WIDTH + 2 * FRAME_PADDING;
        let heading = match key {
            Some(goal) => format!(
                "goal {goal}: {}",
                goal_titles.get(&goal).map(String::as_str).unwrap_or("")
            ),
            None => "no goal".to_owned(),
        };
        frames.push(Frame {
            goal_id: key,
            heading: fit(heading.trim_end_matches(": "), width - 2 * FRAME_PADDING),
            left: column_left(first) - FRAME_PADDING,
            top: lane_top,
            width,
            height,
        });
        for id in ids {
            let node = nodes[id];
            placed.insert(
                *id,
                PlacedTask {
                    id: *id,
                    title: node.title.clone(),
                    goal_id: node.goal_id,
                    reason: chosen[id],
                    tone: Tone::of(node),
                    startable: candidates.contains(id),
                    column: columns[id],
                    left: column_left(columns[id]),
                    top: lane_top + HEADER_HEIGHT + row_of[id] * ROW_HEIGHT,
                },
            );
        }
    }
    Diagram {
        tasks: placed.into_values().collect(),
        frames,
        edges,
        legend_top: lane_top + lane_height + LANE_GAP,
    }
}

/// Each task's column: 0 without a drawn prerequisite, else one more than
/// its deepest prerequisite's. The queue keeps the dependencies acyclic.
pub fn depths(prerequisites: &BTreeMap<TaskId, BTreeSet<TaskId>>) -> BTreeMap<TaskId, usize> {
    fn depth(
        id: TaskId,
        prerequisites: &BTreeMap<TaskId, BTreeSet<TaskId>>,
        memo: &mut BTreeMap<TaskId, usize>,
        visiting: &mut BTreeSet<TaskId>,
    ) -> usize {
        if let Some(depth) = memo.get(&id) {
            return *depth;
        }
        // A cycle would never end; it cannot come from the queue, and
        // counting it as depth 0 keeps the layout total.
        if !visiting.insert(id) {
            return 0;
        }
        let value = prerequisites
            .get(&id)
            .into_iter()
            .flatten()
            .map(|before| depth(*before, prerequisites, memo, visiting) + 1)
            .max()
            .unwrap_or(0);
        visiting.remove(&id);
        memo.insert(id, value);
        value
    }
    let mut memo = BTreeMap::new();
    for id in prerequisites.keys() {
        depth(*id, prerequisites, &mut memo, &mut BTreeSet::new());
    }
    memo
}

/// The bands top to bottom: the groups of bands joined by a drawn
/// dependency (each group in order of its first column, then key) first,
/// in order of their first band, then the bands joined to no other.
fn band_order(
    bands: &BTreeMap<Option<GoalId>, Vec<TaskId>>,
    prerequisites: &BTreeMap<TaskId, BTreeSet<TaskId>>,
    nodes: &BTreeMap<TaskId, &GraphNode>,
    columns: &BTreeMap<TaskId, usize>,
) -> Vec<Option<GoalId>> {
    let mut links: BTreeMap<Option<GoalId>, BTreeSet<Option<GoalId>>> = BTreeMap::new();
    for (id, before) in prerequisites {
        for other in before {
            let (a, b) = (nodes[id].goal_id, nodes[other].goal_id);
            if a != b {
                links.entry(a).or_default().insert(b);
                links.entry(b).or_default().insert(a);
            }
        }
    }
    let first_column =
        |key: &Option<GoalId>| bands[key].iter().map(|id| columns[id]).min().unwrap_or(0);
    let mut seen = BTreeSet::new();
    let mut groups: Vec<Vec<Option<GoalId>>> = Vec::new();
    for key in bands.keys() {
        if !seen.insert(*key) {
            continue;
        }
        let mut group = vec![*key];
        let mut pending = vec![*key];
        while let Some(next) = pending.pop() {
            for linked in links.get(&next).into_iter().flatten() {
                if seen.insert(*linked) {
                    group.push(*linked);
                    pending.push(*linked);
                }
            }
        }
        group.sort_by_key(|key| (first_column(key), *key));
        groups.push(group);
    }
    let (joined, alone): (Vec<_>, Vec<_>) = groups.into_iter().partition(|g| g.len() > 1);
    joined.into_iter().chain(alone).flatten().collect()
}

/// Width units of `text`: one per character, two for a wide one.
fn units(c: char) -> i64 {
    if (c as u32) >= 0x1100 { 2 } else { 1 }
}

/// `text` cut to fit `width` pixels, ending in `…` when cut.
pub fn fit(text: &str, width: i64) -> String {
    let limit = (width / UNIT_PX).max(1);
    if text.chars().map(units).sum::<i64>() <= limit {
        return text.to_owned();
    }
    let mut used = 1;
    let mut out = String::new();
    for c in text.chars() {
        if used + units(c) > limit {
            break;
        }
        used += units(c);
        out.push(c);
    }
    out.push('…');
    out
}

/// `text` as a d2 double-quoted string.
fn quoted(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + 2);
    out.push('"');
    for c in text.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '$' => out.push_str("\\$"),
            '\n' => out.push_str("\\n"),
            c if c.is_control() => {}
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

fn task_key(id: TaskId) -> String {
    format!("t{id}")
}

fn goal_key(goal: Option<GoalId>) -> String {
    goal.map_or_else(|| "goal_none".to_owned(), |goal| format!("goal_{goal}"))
}

impl Diagram {
    /// The d2 source: the goal frames first (so they are drawn behind),
    /// then the boxes, the edges and the legend, every shape at a fixed
    /// `top` / `left` for TALA.
    pub fn to_d2(&self) -> String {
        let mut out = String::from("# dagq graph: near-term dependencies\n");
        for frame in &self.frames {
            out.push_str(&format!(
                "{key}: {{\n  label: {label}\n  label.near: top-left\n  shape: rectangle\n  top: {top}\n  left: {left}\n  width: {width}\n  height: {height}\n  style.fill: \"{FRAME_FILL}\"\n  style.stroke: \"{FRAME_STROKE}\"\n  style.stroke-dash: 3\n  style.font-size: {FONT_SIZE}\n}}\n",
                key = goal_key(frame.goal_id),
                label = quoted(&frame.heading),
                top = frame.top,
                left = frame.left,
                width = frame.width,
                height = frame.height,
            ));
        }
        for task in &self.tasks {
            let (fill, stroke) = task.tone.colors();
            let mark = if task.startable { STARTABLE_MARK } else { "" };
            let title = fit(&task.title, NODE_WIDTH - 24);
            out.push_str(&format!(
                "{key}: {{\n  label: {label}\n  shape: rectangle\n  top: {top}\n  left: {left}\n  width: {NODE_WIDTH}\n  height: {NODE_HEIGHT}\n  style.fill: \"{fill}\"\n  style.stroke: \"{stroke}\"\n  style.font-size: {FONT_SIZE}\n{bold}}}\n",
                key = task_key(task.id),
                label = quoted(&format!("{mark}#{}\n{title}", task.id)),
                top = task.top,
                left = task.left,
                bold = if task.startable {
                    "  style.double-border: true\n"
                } else {
                    ""
                },
            ));
        }
        for edge in &self.edges {
            let from = match edge.from {
                EdgeEnd::Task(id) => task_key(id),
                EdgeEnd::Goal(goal) => goal_key(Some(goal)),
            };
            let (stroke, width) = if edge.critical {
                (CRITICAL_STROKE, 4)
            } else {
                (EDGE_STROKE, 1)
            };
            out.push_str(&format!(
                "{from} -> {to}: {{\n  style.stroke: \"{stroke}\"\n  style.stroke-width: {width}\n}}\n",
                to = task_key(edge.to),
            ));
        }
        out.push_str(&self.legend());
        out
    }

    fn legend(&self) -> String {
        let mut out = String::new();
        let mut left = MARGIN;
        let top = self.legend_top;
        for (index, tone) in [Tone::InProgress, Tone::Pressing, Tone::High, Tone::Normal]
            .into_iter()
            .enumerate()
        {
            let (fill, stroke) = tone.colors();
            out.push_str(&format!(
                "legend_{index}: {{\n  label: {label}\n  shape: rectangle\n  top: {top}\n  left: {left}\n  width: 180\n  height: 36\n  style.fill: \"{fill}\"\n  style.stroke: \"{stroke}\"\n  style.font-size: 12\n}}\n",
                label = quoted(tone.legend()),
            ));
            left += 200;
        }
        let notes = [
            format!("{STARTABLE_MARK}double border: startable now"),
            "thick red line: critical chain".to_owned(),
        ];
        for (index, note) in notes.iter().enumerate() {
            out.push_str(&format!(
                "legend_note_{index}: {{\n  label: {label}\n  shape: text\n  top: {top}\n  left: {left}\n  style.font-size: 12\n}}\n",
                label = quoted(note),
                top = top + index as i64 * 20,
            ));
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn node(id: i64, goal: Option<i64>, ready_after: Vec<WaitFor>) -> GraphNode {
        GraphNode {
            id: TaskId::new(id),
            status: TaskStatus::Ready,
            priority: Priority::Normal,
            priority_source: crate::domain::PrioritySource::Default,
            priority_by: crate::domain::plan_request::PriorityBy::Ai,
            effective_priority: Priority::Normal,
            title: format!("task {id}"),
            goal_id: goal.map(GoalId::new),
            goal_status: None,
            depends_on: Vec::new(),
            goal_dependencies: Vec::new(),
            blocks: Vec::new(),
            unblocks: 0,
            ready_after,
        }
    }

    fn after(ids: &[i64]) -> Vec<WaitFor> {
        ids.iter()
            .map(|id| WaitFor::Task(TaskId::new(*id)))
            .collect()
    }

    fn ids(values: &[i64]) -> Vec<TaskId> {
        values.iter().copied().map(TaskId::new).collect()
    }

    /// 1 → 2 → 3 in goal 10 (3 is high), 4 in progress in goal 20 after 9
    /// (normal, not drawn beyond one level), 5 waits for goal 10, 6 alone
    /// and normal, 7 on the critical chain with no goal, 8 before 7.
    fn graph() -> DependencyGraph {
        let mut three = node(3, Some(10), after(&[2]));
        three.effective_priority = Priority::High;
        let mut four = node(4, Some(20), after(&[9]));
        four.status = TaskStatus::InProgress;
        let mut five = node(
            5,
            Some(30),
            vec![WaitFor::Goal {
                goal: GoalId::new(10),
            }],
        );
        five.effective_priority = Priority::Urgent;
        DependencyGraph {
            tasks: vec![
                node(1, Some(10), vec![]),
                node(2, Some(10), after(&[1])),
                three,
                four,
                five,
                node(6, None, vec![]),
                node(7, None, after(&[8])),
                node(8, None, vec![]),
                node(9, Some(20), after(&[1])),
            ],
            candidates: ids(&[1, 8]),
            critical: ids(&[8, 7]),
        }
    }

    #[test]
    fn selects_in_progress_high_critical_and_one_level_of_prerequisites() {
        let chosen = select(&graph());
        let expected: BTreeMap<TaskId, Reason> = [
            (1, Reason::Prerequisite),
            (2, Reason::Prerequisite),
            (3, Reason::Priority),
            (4, Reason::InProgress),
            (5, Reason::Priority),
            (7, Reason::Critical),
            (8, Reason::Critical),
            (9, Reason::Prerequisite),
        ]
        .into_iter()
        .map(|(id, reason)| (TaskId::new(id), reason))
        .collect();
        // 6 is normal and on nothing's path; 9's own prerequisite 1 is in
        // only because goal 10 is a prerequisite of 5.
        assert_eq!(chosen, expected);
    }

    #[test]
    fn prerequisites_stop_after_one_level() {
        let mut graph = graph();
        graph
            .tasks
            .retain(|node| [1, 2, 3].contains(&node.id.as_i64()));
        graph.critical.clear();
        let chosen = select(&graph);
        assert_eq!(chosen.keys().copied().collect::<Vec<_>>(), ids(&[2, 3]));
    }

    #[test]
    fn a_goal_draws_its_prerequisites_and_critical_steps_outside_it() {
        let titles = BTreeMap::new();
        // Goal 20: 4 is in progress, 9 before it waits for 1 of goal 10;
        // the high 3, the urgent 5 and the chain 8 → 7 are other goals'.
        let narrowed = DependencyGraph {
            critical: ids(&[9, 4]),
            ..graph()
        };
        let chosen = select_in(&narrowed, Some(GoalId::new(20)));
        let expected: BTreeMap<TaskId, Reason> = [
            (1, Reason::Prerequisite),
            (4, Reason::InProgress),
            (9, Reason::Critical),
        ]
        .into_iter()
        .map(|(id, reason)| (TaskId::new(id), reason))
        .collect();
        assert_eq!(chosen, expected);
        let diagram = near_term_in_goal(&graph(), &narrowed, GoalId::new(20), &titles);
        assert_eq!(diagram.task_ids(), ids(&[1, 4, 9]));
        let goals: Vec<Option<i64>> = diagram
            .frames
            .iter()
            .map(|frame| frame.goal_id.map(GoalId::as_i64))
            .collect();
        assert_eq!(goals, [Some(10), Some(20)]);
        assert!(diagram.edges.iter().any(|edge| {
            edge.from == EdgeEnd::Task(TaskId::new(1)) && edge.to == TaskId::new(9)
        }));

        // Goal 10's chain 1 → 9 → 4 leaves it: every step is drawn, with
        // one level of prerequisites; 4 is drawn for the chain, not for
        // being in progress outside the goal.
        let narrowed = DependencyGraph {
            critical: ids(&[1, 9, 4]),
            ..graph()
        };
        let diagram = near_term_in_goal(&graph(), &narrowed, GoalId::new(10), &titles);
        let reasons: Vec<(i64, Reason)> = diagram
            .tasks
            .iter()
            .map(|task| (task.id.as_i64(), task.reason))
            .collect();
        assert_eq!(
            reasons,
            [
                (1, Reason::Critical),
                (2, Reason::Prerequisite),
                (3, Reason::Priority),
                (4, Reason::Critical),
                (9, Reason::Critical),
            ]
        );
        let critical: Vec<(EdgeEnd, TaskId)> = diagram
            .edges
            .iter()
            .filter(|edge| edge.critical)
            .map(|edge| (edge.from, edge.to))
            .collect();
        assert_eq!(
            critical,
            [
                (EdgeEnd::Task(TaskId::new(9)), TaskId::new(4)),
                (EdgeEnd::Task(TaskId::new(1)), TaskId::new(9)),
            ]
        );
        assert_eq!(
            diagram.to_d2(),
            near_term_in_goal(&graph(), &narrowed, GoalId::new(10), &titles).to_d2()
        );
        // Without a goal nothing is narrowed.
        assert_eq!(select_in(&graph(), None), select(&graph()));
    }

    #[test]
    fn columns_follow_the_depth_and_bands_share_a_lane_when_they_can() {
        let titles = BTreeMap::from([(GoalId::new(10), "first goal".to_owned())]);
        let diagram = near_term(&graph(), &titles);
        let column = |id: i64| {
            diagram
                .tasks
                .iter()
                .find(|task| task.id.as_i64() == id)
                .unwrap()
                .column
        };
        assert_eq!(
            [1, 2, 3, 4, 5, 7, 8, 9].map(column),
            [0, 1, 2, 2, 3, 1, 0, 1]
        );
        assert_eq!(diagram.task_ids(), ids(&[1, 2, 3, 4, 5, 7, 8, 9]));
        // Goals 10, 20 and 30 are joined; the no-goal band is alone and last.
        let order: Vec<Option<i64>> = diagram
            .frames
            .iter()
            .map(|frame| frame.goal_id.map(GoalId::as_i64))
            .collect();
        assert_eq!(order, [Some(10), Some(20), Some(30), None]);
        let frame = |goal: Option<i64>| {
            diagram
                .frames
                .iter()
                .find(|frame| frame.goal_id.map(GoalId::as_i64) == goal)
                .unwrap()
        };
        assert_eq!(frame(Some(10)).heading, "goal 10: first goal");
        assert_eq!(frame(Some(20)).heading, "goal 20");
        assert_eq!(frame(Some(10)).left, MARGIN);
        assert_eq!(
            frame(Some(10)).width,
            2 * COLUMN_WIDTH + NODE_WIDTH + 2 * FRAME_PADDING
        );
        // Goal 20 spans columns 1..2, overlapping goal 10: a new lane.
        let second_lane = MARGIN + frame(Some(10)).height + LANE_GAP;
        assert_eq!(frame(Some(20)).top, second_lane);
        // Goal 30 starts at column 3, right of goal 20: the same lane.
        assert_eq!(frame(Some(30)).top, second_lane);
        // The no-goal band (columns 0..1) overlaps: a third lane.
        assert!(frame(None).top > second_lane);
        let task = |id: i64| diagram.tasks.iter().find(|t| t.id.as_i64() == id).unwrap();
        assert_eq!(task(1).left, MARGIN + FRAME_PADDING);
        assert_eq!(task(1).top, MARGIN + HEADER_HEIGHT);
        assert_eq!(task(3).left, MARGIN + FRAME_PADDING + 2 * COLUMN_WIDTH);
        assert_eq!(task(8).top, task(7).top);
        assert!(task(1).startable && task(8).startable && !task(2).startable);
        assert_eq!(task(4).tone, Tone::InProgress);
        assert_eq!(task(5).tone, Tone::Pressing);
        assert_eq!(task(3).tone, Tone::High);
        assert_eq!(task(1).tone, Tone::Normal);
        assert!(diagram.legend_top > frame(None).top + frame(None).height);
        let critical: Vec<_> = diagram.edges.iter().filter(|edge| edge.critical).collect();
        assert_eq!(critical.len(), 1);
        assert_eq!(critical[0].from, EdgeEnd::Task(TaskId::new(8)));
        assert!(
            diagram.edges.iter().any(
                |edge| edge.from == EdgeEnd::Goal(GoalId::new(10)) && edge.to == TaskId::new(5)
            )
        );
    }

    #[test]
    fn two_tasks_of_one_column_stack_in_rows() {
        let graph = DependencyGraph {
            tasks: vec![
                {
                    let mut a = node(1, Some(1), vec![]);
                    a.effective_priority = Priority::High;
                    a
                },
                {
                    let mut b = node(2, Some(1), vec![]);
                    b.effective_priority = Priority::Interrupt;
                    b
                },
            ],
            candidates: Vec::new(),
            critical: Vec::new(),
        };
        let diagram = near_term(&graph, &BTreeMap::new());
        assert_eq!(diagram.tasks[1].top - diagram.tasks[0].top, ROW_HEIGHT);
        assert_eq!(
            diagram.frames[0].height,
            HEADER_HEIGHT + ROW_HEIGHT + NODE_HEIGHT + FRAME_PADDING
        );
    }

    #[test]
    fn an_empty_graph_draws_only_the_legend() {
        let diagram = near_term(
            &DependencyGraph {
                tasks: Vec::new(),
                candidates: Vec::new(),
                critical: Vec::new(),
            },
            &BTreeMap::new(),
        );
        assert!(diagram.tasks.is_empty() && diagram.frames.is_empty());
        assert_eq!(diagram.legend_top, MARGIN + LANE_GAP);
        let d2 = diagram.to_d2();
        assert!(d2.contains("legend_0:") && !d2.contains("->"));
    }

    #[test]
    fn a_cycle_does_not_hang_the_depths() {
        let prerequisites = BTreeMap::from([
            (TaskId::new(1), BTreeSet::from([TaskId::new(2)])),
            (TaskId::new(2), BTreeSet::from([TaskId::new(1)])),
        ]);
        assert_eq!(depths(&prerequisites).len(), 2);
    }

    #[test]
    fn headings_and_labels_are_cut_to_the_width() {
        assert_eq!(fit("short", 200), "short");
        assert_eq!(fit("abcdefghij", 48), "abcde…");
        assert_eq!(fit("あいうえおかきくけこ", 48), "あい…");
        assert_eq!(fit("xy", 0), "…");
    }

    #[test]
    fn d2_fixes_coordinates_escapes_labels_and_is_stable() {
        let mut graph = graph();
        graph.tasks[2].title = "say \"hi\" \\ ${x}".to_owned();
        let titles = BTreeMap::from([(GoalId::new(10), "first goal".to_owned())]);
        let d2 = near_term(&graph, &titles).to_d2();
        assert_eq!(d2, near_term(&graph, &titles).to_d2());
        assert!(d2.contains(
            "goal_10: {\n  label: \"goal 10: first goal\"\n  label.near: top-left\n  shape: rectangle\n  top: 20\n  left: 20\n"
        ));
        assert!(d2.contains(r##"label: "#3\nsay \"hi\" \\ \${x}""##), "{d2}");
        assert!(d2.contains(&format!("label: \"{STARTABLE_MARK}#1\\ntask 1\"")));
        assert!(d2.contains("style.double-border: true"));
        assert!(d2.contains(&format!(
            "t8 -> t7: {{\n  style.stroke: \"{CRITICAL_STROKE}\"\n  style.stroke-width: 4\n}}"
        )));
        assert!(d2.contains("goal_10 -> t5:"));
        assert!(d2.contains("goal_none: {"));
        // Frames come before the boxes, so they are drawn behind them.
        assert!(d2.find("goal_none: {").unwrap() < d2.find("t1: {").unwrap());
        assert_eq!(quoted("a\u{7}b\nc"), "\"ab\\nc\"");
    }

    /// Whether `text` has a Hiragana, Katakana or CJK ideograph character.
    fn has_japanese(text: &str) -> bool {
        text.chars().any(|c| {
            matches!(c,
                '\u{3040}'..='\u{309f}'
                | '\u{30a0}'..='\u{30ff}'
                | '\u{31f0}'..='\u{31ff}'
                | '\u{ff66}'..='\u{ff9f}'
                | '\u{3400}'..='\u{4dbf}'
                | '\u{4e00}'..='\u{9fff}'
                | '\u{f900}'..='\u{faff}')
        })
    }

    #[test]
    fn fixed_labels_are_english() {
        let titles = BTreeMap::from([
            (GoalId::new(10), "first goal".to_owned()),
            (GoalId::new(20), "second goal".to_owned()),
            (GoalId::new(30), "third goal".to_owned()),
        ]);
        // Every tone, a startable task, the critical chain and a band
        // without a goal are drawn, so every fixed label is in the source.
        let diagram = near_term(&graph(), &titles);
        let d2 = diagram.to_d2();
        assert!(!has_japanese(&d2), "{d2}");
        assert!(d2.contains("label: \"normal or lower\""));
        assert!(d2.contains("label: \"no goal\""));
        assert!(d2.contains(&format!(
            "label: \"{STARTABLE_MARK}double border: startable now\""
        )));
        assert!(d2.contains("label: \"thick red line: critical chain\""));
        let in_goal = near_term_in_goal(&graph(), &graph(), GoalId::new(10), &titles).to_d2();
        assert!(!has_japanese(&in_goal), "{in_goal}");
        assert!(has_japanese("あア漢"));
    }

    #[test]
    fn user_titles_are_drawn_unchanged() {
        let mut graph = graph();
        graph.tasks[0].title = "日本語の題".to_owned();
        let titles = BTreeMap::from([(GoalId::new(10), "ゴールの題".to_owned())]);
        let d2 = near_term(&graph, &titles).to_d2();
        assert!(d2.contains("#1\\n日本語の題"), "{d2}");
        assert!(d2.contains("label: \"goal 10: ゴールの題\""), "{d2}");
    }
}
