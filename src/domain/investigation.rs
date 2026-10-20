//! Investigations (ADR-t1487-1 decisions 7 and 8): a goal becomes an
//! investigation when one of its Spikes is claimed, and stays one while a
//! Spike of it is unfinished (a draft is not) or a replanning request of
//! one of its Spikes is `open`; a Spike without a goal is one of its own
//! from its claim to its request's end. Each stretch is recorded in
//! `spike_cycles`: opened by the claim, closed by the supervisor once it
//! ended, or once its goal was abandoned.
//!
//! `[supervisor] max_investigations` limits how many investigations hold
//! their place at once: at the limit, a Spike that would open a new one is
//! deferred ([`INVESTIGATION_LIMIT`]) until one ends. A Spike of an
//! investigation already open, and one an unfinished Spike of an open
//! investigation waits for (transitively, over task dependencies and over
//! goal dependencies, which wait for the goal's unfinished tasks), is never
//! held, so the limit may be passed for a while. An investigation all of whose remaining work
//! only waits for a person's answer (a Spike's run on its question, its
//! proposal on plan review's question, its request's `planner_question`)
//! stops counting once the wait is `[conflicts] waiting_owner_grace_secs`
//! old, as a run's does for the hotspots (ADR-t1484-1); the answer counts
//! it again, past the limit if need be. Nothing is refused at submit, and
//! no planner waits: the ready Spike does.
//!
//! The order with the Spike limit (ADR-t1487-2): the Spike limit holds
//! every Spike first, with its own reason; only when it lets Spikes go
//! does this limit hold those that would open a new investigation. Both
//! leave the slot to the implementations.
//!
//! `[goals] max_spike_cycles` limits a goal's cycles (decision 8 as
//! ADR-t2414-1 amends it): a cycle is a round of Spikes and the replanning
//! of their results. The claim that opens an investigation starts the
//! first; a planner of the runtime's submit of a further Spike of the goal
//! while its results are replanned (a request of its Spikes `open`) starts
//! the next, the investigation kept open as decision 7 says. Spikes run
//! side by side are one round. A
//! planner of the runtime's cannot submit a Spike of a goal that reached
//! the limit ([`spike_submit`]), which a person decides instead.

use std::collections::{BTreeMap, HashSet};

use super::{DomainError, ExecutionClass, GoalId, TaskId};

/// The `claim_deferred` reason of a Spike held because as many
/// investigations as `[supervisor] max_investigations` are open.
pub const INVESTIGATION_LIMIT: &str = "investigation_limit";

/// What an investigation is of: a goal, or a Spike without one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum InvestigationKey {
    Goal(GoalId),
    Spike(TaskId),
}

impl InvestigationKey {
    /// The investigation a Spike of `goal` belongs to.
    pub fn of(task: TaskId, goal: Option<GoalId>) -> Self {
        goal.map_or(Self::Spike(task), Self::Goal)
    }
}

/// An open investigation as the limit counts it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Investigation {
    pub key: InvestigationKey,
    /// For each of its remaining items (an unfinished Spike, an `open`
    /// request of its Spikes), since when (unix seconds) it only waits for
    /// a person's answer, `None` while it moves.
    pub waits: Vec<Option<i64>>,
}

impl Investigation {
    /// Since when every remaining item only waits for a person: the latest
    /// of their waits; `None` when one moves or nothing remains.
    pub fn person_waiting_since(&self) -> Option<i64> {
        if self.waits.is_empty() {
            return None;
        }
        self.waits
            .iter()
            .try_fold(i64::MIN, |latest, wait| wait.map(|since| latest.max(since)))
    }

    /// Whether it counts to the limit at `now`: not once it only waited
    /// for a person for `grace_secs` or longer.
    pub fn counts(&self, now: i64, grace_secs: i64) -> bool {
        self.person_waiting_since()
            .is_none_or(|since| now - since < grace_secs)
    }
}

/// The limit in force for a claim: `max` (`None` limits nothing) and the
/// grace of a wait for a person.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct InvestigationLimit {
    pub max: Option<usize>,
    pub grace_secs: i64,
}

/// The open investigations, and the Spikes their unfinished Spikes wait
/// for over task and goal dependencies, transitively ([`awaited_spikes`]).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Investigations {
    pub open: Vec<Investigation>,
    pub awaited: HashSet<TaskId>,
}

impl Investigations {
    /// How many open investigations count at `now`.
    pub fn counted(&self, now: i64, grace_secs: i64) -> usize {
        self.open
            .iter()
            .filter(|investigation| investigation.counts(now, grace_secs))
            .count()
    }

    /// Why a task `id` of `class` and `goal` may not be claimed now
    /// ([`INVESTIGATION_LIMIT`]), `None` when it may: only a Spike that
    /// would open an investigation at the limit, and not one an open
    /// investigation waits for.
    pub fn limit(
        &self,
        id: TaskId,
        class: ExecutionClass,
        goal: Option<GoalId>,
        limit: InvestigationLimit,
        now: i64,
    ) -> Option<&'static str> {
        let max = limit.max?;
        if !class.is_spike() || self.awaited.contains(&id) {
            return None;
        }
        let key = InvestigationKey::of(id, goal);
        if self
            .open
            .iter()
            .any(|investigation| investigation.key == key)
        {
            return None;
        }
        (self.counted(now, limit.grace_secs) >= max).then_some(INVESTIGATION_LIMIT)
    }
}

/// What a task waits for: its direct predecessors over task dependencies
/// (`tasks`), the goals it waits to close achieved over goal dependencies
/// (`goals`), and each such goal's unfinished tasks (`goal_tasks`), which
/// the goal waits for before it can close.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct WaitEdges {
    pub tasks: BTreeMap<TaskId, Vec<TaskId>>,
    pub goals: BTreeMap<TaskId, Vec<GoalId>>,
    pub goal_tasks: BTreeMap<GoalId, Vec<TaskId>>,
}

/// The Spikes the unfinished Spikes `from` wait for, transitively over
/// `edges` (a goal dependency waits for the goal's unfinished tasks), of
/// those in `spikes`.
pub fn awaited_spikes(
    from: &[TaskId],
    edges: &WaitEdges,
    spikes: &HashSet<TaskId>,
) -> HashSet<TaskId> {
    let mut seen: HashSet<TaskId> = HashSet::new();
    let mut goals_seen: HashSet<GoalId> = HashSet::new();
    let mut stack: Vec<TaskId> = from.to_vec();
    while let Some(task) = stack.pop() {
        let via_goals = edges
            .goals
            .get(&task)
            .into_iter()
            .flatten()
            .filter(|goal| goals_seen.insert(**goal))
            .flat_map(|goal| edges.goal_tasks.get(goal).into_iter().flatten());
        let waited: Vec<TaskId> = edges
            .tasks
            .get(&task)
            .into_iter()
            .flatten()
            .chain(via_goals)
            .copied()
            .collect();
        for predecessor in waited {
            if seen.insert(predecessor) {
                stack.push(predecessor);
            }
        }
    }
    seen.retain(|task| spikes.contains(task));
    seen
}

/// The `claim_deferred` payload's message of a Spike the investigation
/// limit holds.
pub fn investigation_message(counted: usize, max: usize) -> String {
    format!(
        "{counted} investigations are open, the limit of [supervisor] max_investigations ({max}): a Spike that opens another is not claimed until one ends"
    )
}

/// What a submit of Spikes of a goal finds: whether a planner of the
/// runtime's submits them, whether they already count in a cycle (the
/// resubmission of a proposal plan review sent back, one of whose Spikes
/// started a cycle), whether the goal's results are being replanned (its
/// investigation is open and a replanning request of its Spikes is
/// `open`), and how many cycles the goal has had.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SpikeSubmit {
    pub by_runtime: bool,
    pub counted: bool,
    pub replanning: bool,
    pub cycles: usize,
}

/// Judge the submit `submit` of the Spikes of `goal` (the first `task`)
/// against `max` (`[goals] max_spike_cycles`; `None` limits nothing),
/// decision 8 as ADR-t2414-1 amends it: whether it starts the goal's next
/// cycle. A planner of the runtime's further Spikes while the goal's
/// results are replanned start one, all of a submit together; a Spike
/// submitted while the round still runs (no request open yet) joins it,
/// and the first of an investigation not open yet starts none (its claim
/// does). When the goal has had `max` cycles, a planner of the runtime's
/// Spikes are refused instead, since another Spike changes the scope,
/// which a person decides (a `planner_question`, or their own submit,
/// which neither is refused nor starts one). Spikes already counted start
/// none and are not refused. A cycle counts once submitted, whatever plan
/// review and the Spikes' runs do later.
pub fn spike_submit(
    task: TaskId,
    goal: GoalId,
    submit: SpikeSubmit,
    max: Option<usize>,
) -> Result<bool, DomainError> {
    if !submit.by_runtime || submit.counted {
        return Ok(false);
    }
    match max {
        Some(max) if submit.cycles >= max => Err(DomainError::SpikeCycleLimit {
            task_id: task,
            goal_id: goal,
            cycles: submit.cycles,
            max,
        }),
        _ => Ok(submit.replanning),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW: i64 = 10_000;
    const GRACE: i64 = 600;

    fn goal(id: i64, waits: Vec<Option<i64>>) -> Investigation {
        Investigation {
            key: InvestigationKey::Goal(GoalId::new(id)),
            waits,
        }
    }

    fn limit(max: Option<usize>) -> InvestigationLimit {
        InvestigationLimit {
            max,
            grace_secs: GRACE,
        }
    }

    #[test]
    fn a_spike_opening_an_investigation_at_the_limit_is_held() {
        let open = Investigations {
            open: vec![goal(1, vec![None])],
            awaited: HashSet::new(),
        };
        let spike = ExecutionClass::Spike;
        // Another goal's Spike opens a second: held at 1.
        assert_eq!(
            open.limit(
                TaskId::new(5),
                spike,
                Some(GoalId::new(2)),
                limit(Some(1)),
                NOW
            ),
            Some(INVESTIGATION_LIMIT)
        );
        // A goalless Spike opens its own: held as well.
        assert_eq!(
            open.limit(TaskId::new(6), spike, None, limit(Some(1)), NOW),
            Some(INVESTIGATION_LIMIT)
        );
        // Below the limit, and without one, it goes.
        assert_eq!(
            open.limit(
                TaskId::new(5),
                spike,
                Some(GoalId::new(2)),
                limit(Some(2)),
                NOW
            ),
            None
        );
        assert_eq!(
            open.limit(
                TaskId::new(5),
                spike,
                Some(GoalId::new(2)),
                limit(None),
                NOW
            ),
            None
        );
        // The open goal's own Spike, and an implementation, go.
        assert_eq!(
            open.limit(
                TaskId::new(7),
                spike,
                Some(GoalId::new(1)),
                limit(Some(1)),
                NOW
            ),
            None
        );
        assert_eq!(
            open.limit(
                TaskId::new(8),
                ExecutionClass::Implementation,
                Some(GoalId::new(2)),
                limit(Some(1)),
                NOW
            ),
            None
        );
    }

    #[test]
    fn a_spike_an_open_investigation_waits_for_goes_past_the_limit() {
        let open = Investigations {
            open: vec![goal(1, vec![None])],
            awaited: [TaskId::new(5)].into(),
        };
        assert_eq!(
            open.limit(
                TaskId::new(5),
                ExecutionClass::Spike,
                Some(GoalId::new(2)),
                limit(Some(1)),
                NOW
            ),
            None
        );
    }

    #[test]
    fn an_investigation_waiting_only_for_a_person_past_the_grace_stops_counting() {
        // Every item waits for a person, the latest since 9,500: within the
        // grace it counts, past it it does not.
        let waiting = goal(1, vec![Some(9_000), Some(9_500)]);
        assert_eq!(waiting.person_waiting_since(), Some(9_500));
        assert!(waiting.counts(NOW, GRACE));
        assert!(!waiting.counts(9_500 + GRACE, GRACE));
        // One item that moves keeps it counted.
        let moving = goal(2, vec![Some(1_000), None]);
        assert_eq!(moving.person_waiting_since(), None);
        assert!(moving.counts(NOW, GRACE));
        assert_eq!(goal(3, Vec::new()).person_waiting_since(), None);
        let open = Investigations {
            open: vec![goal(1, vec![Some(1_000)]), moving],
            awaited: HashSet::new(),
        };
        assert_eq!(open.counted(NOW, GRACE), 1);
        assert_eq!(
            open.limit(
                TaskId::new(5),
                ExecutionClass::Spike,
                Some(GoalId::new(4)),
                limit(Some(2)),
                NOW
            ),
            None
        );
    }

    #[test]
    fn the_awaited_spikes_follow_the_dependencies_transitively() {
        let edges = WaitEdges {
            tasks: [
                (TaskId::new(1), vec![TaskId::new(2)]),
                (TaskId::new(2), vec![TaskId::new(3), TaskId::new(4)]),
                (TaskId::new(4), vec![TaskId::new(1)]),
            ]
            .into(),
            ..WaitEdges::default()
        };
        let spikes: HashSet<TaskId> = [TaskId::new(3), TaskId::new(4), TaskId::new(9)].into();
        assert_eq!(
            awaited_spikes(&[TaskId::new(1)], &edges, &spikes),
            [TaskId::new(3), TaskId::new(4)].into()
        );
        assert!(awaited_spikes(&[TaskId::new(9)], &edges, &spikes).is_empty());
    }

    /// A goal dependency waits for the goal's unfinished tasks, and the
    /// closure goes on from them over task and goal dependencies alike; a
    /// goal no waiting task depends on is not reached, and a cycle through
    /// goals ends.
    #[test]
    fn the_awaited_spikes_follow_goal_dependencies_transitively() {
        let (b, c, d) = (GoalId::new(2), GoalId::new(3), GoalId::new(4));
        let edges = WaitEdges {
            tasks: [(TaskId::new(20), vec![TaskId::new(21)])].into(),
            goals: [
                (TaskId::new(1), vec![b]),
                (TaskId::new(21), vec![d]),
                (TaskId::new(40), vec![b]),
            ]
            .into(),
            goal_tasks: [
                (b, vec![TaskId::new(20)]),
                (c, vec![TaskId::new(30)]),
                (d, vec![TaskId::new(40)]),
            ]
            .into(),
        };
        let spikes: HashSet<TaskId> = [20, 21, 30, 40].map(TaskId::new).into();
        assert_eq!(
            awaited_spikes(&[TaskId::new(1)], &edges, &spikes),
            [20, 21, 40].map(TaskId::new).into()
        );
        assert!(awaited_spikes(&[TaskId::new(30)], &edges, &spikes).is_empty());
    }

    fn submit(by_runtime: bool, counted: bool, replanning: bool, cycles: usize) -> SpikeSubmit {
        SpikeSubmit {
            by_runtime,
            counted,
            replanning,
            cycles,
        }
    }

    #[test]
    fn a_goal_at_its_cycle_limit_refuses_another_spike() {
        let (task, goal) = (TaskId::new(4), GoalId::new(2));
        assert!(spike_submit(task, goal, submit(true, false, true, 9), None).is_ok());
        let refused = spike_submit(task, goal, submit(true, false, true, 2), Some(2)).unwrap_err();
        assert!(matches!(
            refused,
            DomainError::SpikeCycleLimit {
                cycles: 2,
                max: 2,
                ..
            }
        ));
        assert!(
            refused.to_string().contains("planner_question"),
            "{refused}"
        );
        // A Spike joining the running round, or the first of the goal's next
        // investigation, is refused at the limit as well.
        assert!(spike_submit(task, goal, submit(true, false, false, 1), Some(1)).is_err());
        // A person's submit goes, and starts no cycle.
        assert_eq!(
            spike_submit(task, goal, submit(false, false, true, 5), Some(2)),
            Ok(false)
        );
    }

    #[test]
    fn a_further_spike_inside_an_open_investigation_starts_the_next_cycle() {
        let (task, goal) = (TaskId::new(4), GoalId::new(2));
        // The first round's replanning submits a further Spike: below the
        // limit it goes and starts the second.
        assert_eq!(
            spike_submit(task, goal, submit(true, false, true, 1), Some(2)),
            Ok(true)
        );
        assert_eq!(
            spike_submit(task, goal, submit(true, false, true, 1), None),
            Ok(true)
        );
        // The second round's replanning submits another: the investigation
        // is still open, but the goal has had two cycles.
        assert!(spike_submit(task, goal, submit(true, false, true, 2), Some(2)).is_err());
        // A Spike submitted while the round runs, and the first of an
        // investigation not open yet, start none.
        assert_eq!(
            spike_submit(task, goal, submit(true, false, false, 0), Some(2)),
            Ok(false)
        );
        // Spikes already counted, submitted again after a send back, start
        // none and are not refused.
        assert_eq!(
            spike_submit(task, goal, submit(true, true, true, 2), Some(2)),
            Ok(false)
        );
    }
}
