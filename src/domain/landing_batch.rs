//! Selection and serial isolation for a landing batch.
//! A failed verification splits the stage in two. The first half is
//! settled before the second is checked on top of the commits that landed.

use std::{num::NonZeroUsize, ops::Range};

use serde::{Deserialize, Serialize};

use super::{
    RunId,
    landing_queue::{Capacity, Entry, LandingQueue, Resources},
    landing_verification::Planned,
};

/// Share commands across members while keeping each plan in order. A
/// command needed again after a setup step gets another invocation.
/// Replacement gates share their covered commands, and retain their
/// separate execution mode from commands run as registered.
pub fn verification_commands(plans: impl IntoIterator<Item = Vec<Planned>>) -> Vec<Planned> {
    let mut combined: Vec<Planned> = Vec::new();
    for plan in plans {
        let mut next = 0;
        for command in plan {
            if let Some(offset) = combined[next..].iter().position(|prior| {
                prior.command == command.command && prior.replaced() == command.replaced()
            }) {
                next += offset;
                let previous = &mut combined[next];
                for replaced in command.replaces {
                    if !previous.replaces.contains(&replaced) {
                        previous.replaces.push(replaced);
                    }
                }
            } else {
                combined.insert(next, command);
            }
            next += 1;
        }
    }
    combined
}

/// When the turn opens, choose from the head of the current queue up to
/// the limit and start with the available members. Independently
/// blocked entries yield as they do for a single landing. An eligible e2e
/// entry ends the prefix, or occupies the turn by itself.
pub fn select(
    queue: &LandingQueue,
    capacity: Capacity,
    limit: NonZeroUsize,
    mut solo: impl FnMut(&Entry) -> bool,
    mut resources: impl FnMut(&Entry) -> Resources,
) -> Vec<&Entry> {
    let mut selected = Vec::new();
    for entry in &queue.entries {
        let facts = resources(entry);
        // If the e2e's worker slot is its only obstacle, stop an already
        // selected prefix there. With no prefix, ordinary admission may
        // still pick an independent successor.
        let solo = solo(entry);
        if solo
            && !selected.is_empty()
            && LandingQueue::waiting(
                entry,
                Capacity {
                    worker_full: false,
                    ..capacity
                },
                facts,
            )
            .is_none()
        {
            break;
        }
        if LandingQueue::waiting(entry, capacity, facts).is_some() {
            continue;
        }
        selected.push(entry);
        if solo || selected.len() == limit.get() {
            break;
        }
    }
    selected
}

/// A single verification stage of the current batch. `runs` is in
/// application order, and `depth` is zero for the initial stage.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Stage {
    pub runs: Vec<RunId>,
    pub depth: usize,
}

/// Payload of `landing_batch_progress` and the value of status's
/// `landing_batch` for the most recent batch. The ID distinguishes
/// repeated attempts with the same members. `runs` is the original
/// selection, `stage` is the current verification (null during
/// preparation/publication), and `removed` lists members sent back so
/// far. `finished` closes the observation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Progress {
    pub id: String,
    pub runs: Vec<RunId>,
    pub stage: Option<Stage>,
    pub removed: Vec<RunId>,
    pub finished: bool,
}

/// A contiguous portion of the batch. Stages are consumed depth-first,
/// with each later stage tested on top of the successful earlier stages.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Segment {
    pub range: Range<usize>,
    pub depth: usize,
}

pub enum Prepared<T> {
    Verify(Segment),
    ResolveFirst(T),
}

pub enum Verification<T> {
    Passed,
    RunFailed(T),
    Interrupted(T),
}

pub enum Verified<T> {
    Publish,
    Split,
    SendBack(T),
    Stop(T),
}

/// Pending stages in depth-first order. The caller must settle a stage
/// before asking for the next one, using the accepted prefix as its base.
pub struct Stages(Vec<Segment>);

impl Stages {
    pub fn new(len: usize) -> Self {
        Self(vec![Segment {
            range: 0..len,
            depth: 0,
        }])
    }

    pub fn take(&mut self) -> Option<Segment> {
        self.0.pop()
    }

    /// Verify the clean prefix first and then reconsider the blocked
    /// member on the new base. A blocker at the front must take its own
    /// landing path. Only continue with the queued suffix when that path
    /// removes the blocker from the landing queue.
    pub fn prepared<T>(&mut self, mut stage: Segment, blocked: Option<(usize, T)>) -> Prepared<T> {
        match blocked {
            Some((index, gate)) if index == stage.range.start => {
                self.push(index + 1..stage.range.end, stage.depth);
                Prepared::ResolveFirst(gate)
            }
            Some((index, _)) => {
                self.push(index..stage.range.end, stage.depth);
                stage.range.end = index;
                Prepared::Verify(stage)
            }
            None => Prepared::Verify(stage),
        }
    }

    /// Isolate a verification failure in FIFO order, sending back a
    /// singleton that fails on the accepted prefix. An interruption such
    /// as a host failure ends the batch without attributing blame.
    pub fn verified<T>(&mut self, stage: &Segment, result: Verification<T>) -> Verified<T> {
        match result {
            Verification::Passed => Verified::Publish,
            Verification::Interrupted(reason) => Verified::Stop(reason),
            Verification::RunFailed(reason) if stage.range.len() == 1 => Verified::SendBack(reason),
            Verification::RunFailed(_) => {
                let mid = stage.range.start + stage.range.len() / 2;
                let depth = stage.depth + 1;
                self.push(mid..stage.range.end, depth);
                self.push(stage.range.start..mid, depth);
                Verified::Split
            }
        }
    }

    fn push(&mut self, range: Range<usize>, depth: usize) {
        if !range.is_empty() {
            self.0.push(Segment { range, depth });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::domain::{EventId, Priority, TaskId};

    fn entry(id: i64, e2e: bool) -> Entry {
        Entry {
            run_id: RunId::new(format!("run-{id}")).unwrap(),
            task_id: TaskId::new(id),
            entry_id: EventId::new(id),
            judgment: EventId::new(id),
            entered_at: format!("2026-11-19T00:00:0{id}Z"),
            basis: "review_finished".into(),
            priority: Priority::Normal,
            e2e,
            disk: true,
        }
    }

    #[test]
    fn uses_the_available_prefix_immediately_and_leaves_e2e_alone() {
        let first = entry(1, false);
        let second = entry(2, false);
        let e2e = entry(3, true);
        let limit = NonZeroUsize::new(4).unwrap();
        let capacity = Capacity::default();
        let one = LandingQueue::new(vec![first.clone()]);
        assert_eq!(
            select(&one, capacity, limit, |e| e.e2e, |_| Resources::default()),
            [&first]
        );
        let queue = LandingQueue::new(vec![second.clone(), e2e.clone(), first.clone()]);
        assert_eq!(
            select(
                &queue,
                capacity,
                NonZeroUsize::MIN,
                |e| e.e2e,
                |_| Resources::default()
            ),
            [&first]
        );
        assert_eq!(
            select(&queue, capacity, limit, |e| e.e2e, |_| Resources::default()),
            [&first, &second]
        );
        let blocked = |entry: &Entry| Resources {
            disk_short: !entry.e2e,
            ..Resources::default()
        };
        assert_eq!(select(&queue, capacity, limit, |e| e.e2e, blocked), [&e2e]);
        assert!(
            select(
                &queue,
                Capacity {
                    turn_busy: true,
                    ..capacity
                },
                limit,
                |e| e.e2e,
                |_| Resources::default()
            )
            .is_empty()
        );
    }

    #[test]
    fn skips_an_independently_held_entry_and_stops_under_a_queue_hold() {
        let [first, held, last] = [entry(1, false), entry(2, false), entry(3, false)];
        let queue = LandingQueue::new(vec![held.clone(), first.clone(), last.clone()]);
        let limit = NonZeroUsize::new(3).unwrap();
        let resources = |entry: &Entry| Resources {
            unanswered: entry.run_id == held.run_id,
            ..Resources::default()
        };
        let capacity = Capacity::default();
        assert_eq!(
            select(&queue, capacity, limit, |e| e.e2e, resources),
            [&first, &last]
        );
        assert!(
            select(
                &queue,
                Capacity {
                    held: true,
                    ..capacity
                },
                limit,
                |e| e.e2e,
                resources,
            )
            .is_empty()
        );
    }

    #[test]
    fn verifies_the_prefix_before_revisiting_a_blocked_member_at_the_same_depth() {
        let mut pending = Stages::new(3);
        let stage = pending.take().unwrap();
        let Prepared::Verify(prefix) = pending.prepared(stage, Some((1, "conflict"))) else {
            panic!("verify the clean prefix first");
        };
        assert_eq!(
            prefix,
            Segment {
                range: 0..1,
                depth: 0
            }
        );
        assert!(matches!(
            pending.verified::<()>(&prefix, Verification::Passed),
            Verified::Publish
        ));
        let retry = pending.take().unwrap();
        assert_eq!(
            retry,
            Segment {
                range: 1..3,
                depth: 0
            }
        );
        assert!(matches!(
            pending.prepared(retry, Some((1, "conflict"))),
            Prepared::ResolveFirst("conflict")
        ));
        assert_eq!(
            pending.take(),
            Some(Segment {
                range: 2..3,
                depth: 0
            })
        );
    }

    #[test]
    fn an_interruption_stops_without_splitting_or_sending_back_a_member() {
        let mut pending = Stages::new(3);
        let stage = pending.take().unwrap();
        assert!(matches!(
            pending.verified(&stage, Verification::Interrupted("host failure")),
            Verified::Stop("host failure")
        ));
        assert!(pending.take().is_none());
    }

    fn command(command: &str, replaces: &[&str]) -> Planned {
        Planned {
            command: command.to_owned(),
            replaces: replaces.iter().map(|name| (*name).to_owned()).collect(),
        }
    }

    #[test]
    fn shares_commands_and_retains_each_members_order_after_setup() {
        let lint = command("lint", &[]);
        let setup = command("setup", &[]);
        let test = command("test", &[]);
        assert_eq!(
            verification_commands([
                vec![lint.clone(), setup.clone(), test.clone()],
                vec![setup.clone(), lint.clone(), test.clone()],
            ]),
            [lint.clone(), setup, lint, test]
        );
    }

    #[test]
    fn shares_replacement_gates_and_unions_their_coverage() {
        let ordinary = command("verify", &[]);
        assert_eq!(
            verification_commands([
                vec![ordinary.clone(), command("verify", &["first", "shared"])],
                vec![ordinary.clone(), command("verify", &["shared", "second"])],
            ]),
            [ordinary, command("verify", &["first", "shared", "second"])]
        );
    }

    #[test]
    fn isolates_an_interaction_on_top_of_the_passing_prefix() {
        let mut pending = Stages::new(3);
        let mut accepted = Vec::new();
        let mut removed = Vec::new();
        let mut checked = Vec::new();
        while let Some(stage) = pending.take() {
            let candidate: Vec<_> = accepted
                .iter()
                .copied()
                .chain(stage.range.clone())
                .collect();
            checked.push((stage.depth, candidate.clone()));
            let result = if candidate.contains(&0) && candidate.contains(&1) {
                Verification::RunFailed(())
            } else {
                Verification::Passed
            };
            match pending.verified(&stage, result) {
                Verified::Publish => accepted.extend(stage.range),
                Verified::SendBack(()) => removed.push(stage.range.start),
                Verified::Split => {}
                Verified::Stop(()) => panic!("no host interruption"),
            }
        }
        assert_eq!(
            checked,
            [
                (0, vec![0, 1, 2]),
                (1, vec![0]),
                (1, vec![0, 1, 2]),
                (2, vec![0, 1]),
                (2, vec![0, 2]),
            ]
        );
        assert_eq!(accepted, [0, 2]);
        assert_eq!(removed, [1]);
    }
}
