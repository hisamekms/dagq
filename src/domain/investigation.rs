//! Investigations (ADR-t1487-1 decisions 7 and 8): a goal becomes an
//! investigation when one of its Spikes is claimed, and stays one while a
//! Spike of it is unfinished (a draft is not) or a replanning request of
//! one of its Spikes is `open`; a Spike without a goal is one of its own
//! from its claim to its request's end. Each stretch is a cycle, recorded
//! in `spike_cycles`: opened by the claim, closed by the supervisor once it
//! ended, or once its goal was abandoned.
//!
//! `[supervisor] max_investigations` limits how many investigations hold
//! their place at once: at the limit, a Spike that would open a new one is
//! deferred ([`INVESTIGATION_LIMIT`]) until one ends. A Spike of an
//! investigation already open, and one an unfinished Spike of an open
//! investigation waits for (transitively), is never held, so the limit
//! may be passed for a while. An investigation all of whose remaining work
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
//! `[goals] max_spike_cycles` limits a goal's cycles (decision 8): a
//! planner of the runtime's cannot submit a Spike of a goal that reached
//! it ([`check_spike_cycles`]), which a person decides instead.

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
/// for over task dependencies, transitively.
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

/// The Spikes the unfinished Spikes `from` wait for, transitively over
/// `predecessors` (each task's direct ones), of those in `spikes`.
pub fn awaited_spikes(
    from: &[TaskId],
    predecessors: &BTreeMap<TaskId, Vec<TaskId>>,
    spikes: &HashSet<TaskId>,
) -> HashSet<TaskId> {
    let mut seen: HashSet<TaskId> = HashSet::new();
    let mut stack: Vec<TaskId> = from.to_vec();
    while let Some(task) = stack.pop() {
        for predecessor in predecessors.get(&task).into_iter().flatten() {
            if seen.insert(*predecessor) {
                stack.push(*predecessor);
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

/// Refuse a planner of the runtime's submit of the Spike `task` of `goal`
/// when the goal has had `cycles` investigations, as many as `max`
/// (`[goals] max_spike_cycles`, decision 8): another Spike changes the
/// scope, which a person decides (a `planner_question`, or their own
/// submit). `None` limits nothing.
pub fn check_spike_cycles(
    task: TaskId,
    goal: GoalId,
    cycles: usize,
    max: Option<usize>,
) -> Result<(), DomainError> {
    match max {
        Some(max) if cycles >= max => Err(DomainError::SpikeCycleLimit {
            task_id: task,
            goal_id: goal,
            cycles,
            max,
        }),
        _ => Ok(()),
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
        let predecessors: BTreeMap<TaskId, Vec<TaskId>> = [
            (TaskId::new(1), vec![TaskId::new(2)]),
            (TaskId::new(2), vec![TaskId::new(3), TaskId::new(4)]),
            (TaskId::new(4), vec![TaskId::new(1)]),
        ]
        .into();
        let spikes: HashSet<TaskId> = [TaskId::new(3), TaskId::new(4), TaskId::new(9)].into();
        assert_eq!(
            awaited_spikes(&[TaskId::new(1)], &predecessors, &spikes),
            [TaskId::new(3), TaskId::new(4)].into()
        );
        assert!(awaited_spikes(&[TaskId::new(9)], &predecessors, &spikes).is_empty());
    }

    #[test]
    fn a_goal_at_its_cycle_limit_refuses_another_spike() {
        let (task, goal) = (TaskId::new(4), GoalId::new(2));
        assert!(check_spike_cycles(task, goal, 1, Some(2)).is_ok());
        assert!(check_spike_cycles(task, goal, 9, None).is_ok());
        let refused = check_spike_cycles(task, goal, 2, Some(2)).unwrap_err();
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
    }
}
