//! The single, reconstructed landing order. Ownership and resource probes
//! belong to callers; selection here never takes a lease or changes a run.

use super::{EventId, EventKind, Priority, RunEvent, RunHistory, RunId, TaskId};
use serde::Serialize;
use std::cmp::Reverse;

/// A queued landing's durable provenance and current requirements.
/// `entered_at` is the authorizing event's timestamp, `judgment` its ID and
/// `basis` its kind. `priority` is the task's effective priority now.
/// `e2e` requires worker capacity; `disk` requires the landing disk budget.
/// Status adds a one-based `position` and `waiting_for` from the latest probe.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Entry {
    pub run_id: RunId,
    pub task_id: TaskId,
    pub entered_at: String,
    pub judgment: EventId,
    pub basis: String,
    pub priority: Priority,
    pub e2e: bool,
    pub disk: bool,
}

impl Entry {
    /// Fold the run's current authorization, including review pass, AI land,
    /// convergence, direct approval and recovery. Old queues need no migration.
    pub fn from_events(
        run_id: RunId,
        task_id: TaskId,
        events: &[RunEvent],
        priority: Priority,
        head: Option<&str>,
    ) -> Option<Self> {
        let e2e = head.is_some_and(|head| super::run_e2e::due(events, head).is_some());
        Self::with_requirements(run_id, task_id, events, priority, e2e)
    }

    fn with_requirements(
        run_id: RunId,
        task_id: TaskId,
        events: &[RunEvent],
        priority: Priority,
        e2e: bool,
    ) -> Option<Self> {
        let id = RunHistory::from_events(events).landing_judgment()?;
        let event = events.iter().find(|event| event.id == id)?;
        Some(Self {
            run_id,
            task_id,
            entered_at: event.created_at.clone(),
            judgment: id,
            basis: event.kind.as_str().to_owned(),
            priority,
            e2e,
            disk: true,
        })
    }
}

/// A probe's reason for withholding a candidate. Local holds may be skipped;
/// the queue-wide turn and active landing withhold every candidate.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Wait {
    Held,
    Integrating,
    Turn,
    Worker,
    Disk,
    Cleanup,
    Lease,
    Preparing,
    QueueOrder,
    Fence,
    Drain,
}

impl Wait {
    /// A human-readable predicate describing the reason a run cannot start.
    pub const fn describe(self) -> &'static str {
        match self {
            Self::Held => "is waiting for the landing hold to clear",
            Self::Integrating => "is waiting for the active landing to finish",
            Self::Turn => "is waiting for the shared landing turn",
            Self::Worker => "is waiting for a worker slot to run e2e",
            Self::Disk => "is waiting for enough disk space",
            Self::Cleanup => "is waiting for cleanup to release the worktree",
            Self::Lease => "is waiting for its current lease to be returned",
            Self::Preparing => "is waiting for landing preparation to finish",
            Self::QueueOrder => "is queued behind an earlier landing",
            Self::Fence => "is waiting for its task hold or replan to clear",
            Self::Drain => "is waiting for a supervisor that can start its landing",
        }
    }
}

/// Resource facts for one entry, read without mutating its ownership.
#[derive(Debug, Clone, Copy, Default)]
pub struct Resources {
    pub disk_short: bool,
    pub cleanup: bool,
    pub leased: bool,
    pub preparing: bool,
    pub fenced: bool,
    pub drain: bool,
}

impl Resources {
    /// Ready unleased entries backpressure worker admission even while
    /// waiting for a landing resource. A task hold or replan temporarily
    /// fences its work out of admission.
    pub const fn counts_toward_backpressure(self) -> bool {
        !self.leased && !self.preparing && !self.fenced
    }
}

/// Queue-wide facts. A held turn prevents even an entry with no commands
/// from bypassing work already reserving the shared landing resource.
#[derive(Debug, Clone, Copy, Default)]
pub struct Capacity {
    pub held: bool,
    pub integrating: bool,
    pub turn_busy: bool,
    pub worker_full: bool,
}

/// Sorted entries, reconstructed on every pass. No independent queue state.
#[derive(Debug, Default)]
pub struct LandingQueue {
    pub entries: Vec<Entry>,
}
impl LandingQueue {
    pub fn new(mut entries: Vec<Entry>) -> Self {
        entries.sort_by_key(|e| (Reverse(e.priority), e.entered_at.clone(), e.judgment));
        Self { entries }
    }
    pub fn waiting(entry: &Entry, capacity: Capacity, resources: Resources) -> Option<Wait> {
        if capacity.held {
            Some(Wait::Held)
        } else if capacity.integrating {
            Some(Wait::Integrating)
        } else if capacity.turn_busy {
            Some(Wait::Turn)
        } else if resources.leased {
            Some(Wait::Lease)
        } else if resources.preparing {
            Some(Wait::Preparing)
        } else if resources.fenced {
            Some(Wait::Fence)
        } else if resources.drain {
            Some(Wait::Drain)
        } else if resources.disk_short && entry.disk {
            Some(Wait::Disk)
        } else if resources.cleanup {
            Some(Wait::Cleanup)
        } else if entry.e2e && capacity.worker_full {
            Some(Wait::Worker)
        } else {
            None
        }
    }
    /// Explain a probe relative to the selected entry. `None` denotes the
    /// entry selected on this pass; otherwise a runnable successor waits
    /// for the earlier entry's use of the lane.
    pub fn explain(
        entry: &Entry,
        capacity: Capacity,
        resources: Resources,
        selected: Option<&RunId>,
    ) -> Option<Wait> {
        Self::waiting(entry, capacity, resources)
            .or_else(|| (selected != Some(&entry.run_id)).then_some(Wait::QueueOrder))
    }

    /// First independently runnable entry in priority/time order. Skipping
    /// never changes the timestamp or reserves resources from another run.
    pub fn select(
        &self,
        capacity: Capacity,
        mut resources: impl FnMut(&Entry) -> Resources,
    ) -> Option<&Entry> {
        self.entries
            .iter()
            .find(|entry| Self::waiting(entry, capacity, resources(entry)).is_none())
    }
}

/// `landing_queue_entered` carries the complete Entry as its payload;
/// `landing_queue_left` carries `judgment` and `reason` (start or invalidated).
/// `landing_queue_waiting` carries `judgment` and nullable `waiting_for`.
/// These are observations of the event-derived queue, never a second source
/// of authorization. Repeated probes record only changes in the wait reason.
pub fn recorded_entry(events: &[RunEvent], entry: &Entry) -> bool {
    events
        .iter()
        .rev()
        .find(|e| {
            matches!(
                e.kind.known(),
                Some(EventKind::LandingQueueEntered | EventKind::LandingQueueLeft)
            )
        })
        .is_some_and(|e| {
            e.kind == EventKind::LandingQueueEntered
                && e.payload["judgment"] == serde_json::json!(entry.judgment)
        })
}

/// Latest wait observation for this entry since its last entry or exit.
/// A restored authorization keeps its judgment, so observations from a
/// previous stay in the queue cannot explain its current wait.
pub fn recorded_wait(events: &[RunEvent], judgment: EventId) -> Option<&RunEvent> {
    events
        .iter()
        .rev()
        .find(|e| {
            matches!(
                e.kind.known(),
                Some(
                    EventKind::LandingQueueEntered
                        | EventKind::LandingQueueLeft
                        | EventKind::LandingQueueWaiting
                )
            )
        })
        .filter(|e| {
            e.kind == EventKind::LandingQueueWaiting
                && e.payload["judgment"] == serde_json::json!(judgment)
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{Value, json};

    fn event(id: i64, kind: EventKind, payload: Value) -> RunEvent {
        RunEvent {
            id: EventId::new(id),
            task_id: Some(TaskId::new(id)),
            goal_id: None,
            run_id: None,
            kind: kind.as_str().into(),
            payload,
            created_at: format!("2026-11-18T{:02}:00:00Z", id),
            actor: None,
        }
    }
    fn entry(id: i64, priority: Priority, e2e: bool) -> Entry {
        Entry::with_requirements(
            RunId::new(format!("run-{id}")).unwrap(),
            TaskId::new(id),
            &[event(
                id,
                EventKind::IntegrationApproved,
                json!({"ask_id": id}),
            )],
            priority,
            e2e,
        )
        .unwrap()
    }

    #[test]
    fn a_restored_entry_only_uses_wait_observations_from_its_current_stay() {
        let judgment = EventId::new(1);
        let wait = |id, reason| {
            event(
                id,
                EventKind::LandingQueueWaiting,
                json!({"judgment": judgment, "waiting_for": reason}),
            )
        };
        let mut events = vec![
            event(
                2,
                EventKind::LandingQueueEntered,
                json!({"judgment": judgment}),
            ),
            wait(3, "disk"),
        ];
        assert_eq!(
            recorded_wait(&events, judgment).unwrap().payload["waiting_for"],
            "disk"
        );
        events.push(event(
            4,
            EventKind::LandingQueueLeft,
            json!({"judgment": judgment, "reason": "start"}),
        ));
        assert!(recorded_wait(&events, judgment).is_none());
        events.push(event(
            5,
            EventKind::LandingQueueEntered,
            json!({"judgment": judgment}),
        ));
        assert!(recorded_wait(&events, judgment).is_none());
        events.push(wait(6, "turn"));
        assert_eq!(
            recorded_wait(&events, judgment).unwrap().payload["waiting_for"],
            "turn"
        );
    }

    /// The reported overtaking: answered runs 5036/4331/5062 precede
    /// later passes 4075/5061/5111, irrespective of enumeration order.
    #[test]
    fn equal_priority_keeps_answers_before_later_review_passes() {
        let mut entries = Vec::new();
        for (id, task, kind, payload) in [
            (
                1,
                5036,
                EventKind::IntegrationApproved,
                json!({"ask_id": 1}),
            ),
            (
                2,
                4331,
                EventKind::IntegrationApproved,
                json!({"ask_id": 2}),
            ),
            (
                3,
                5062,
                EventKind::IntegrationApproved,
                json!({"ask_id": 3}),
            ),
            (
                4,
                4075,
                EventKind::ReviewFinished,
                json!({"verdict": "pass"}),
            ),
            (
                5,
                5061,
                EventKind::ReviewFinished,
                json!({"verdict": "pass"}),
            ),
            (
                6,
                5111,
                EventKind::ReviewFinished,
                json!({"verdict": "pass"}),
            ),
        ] {
            entries.push(
                Entry::with_requirements(
                    RunId::new(format!("r-{task}")).unwrap(),
                    TaskId::new(task),
                    &[event(id, kind, payload)],
                    Priority::Normal,
                    false,
                )
                .unwrap(),
            );
        }
        entries.reverse();
        let queue = LandingQueue::new(entries);
        assert_eq!(
            queue
                .entries
                .iter()
                .map(|e| e.task_id.as_i64())
                .collect::<Vec<_>>(),
            [5036, 4331, 5062, 4075, 5061, 5111]
        );
        assert_eq!(
            queue
                .select(Capacity::default(), |_| Resources::default())
                .unwrap()
                .task_id
                .as_i64(),
            5036
        );
    }

    #[test]
    fn effective_priority_precedes_time_and_ties_use_event_id() {
        let mut urgent = entry(3, Priority::Urgent, false);
        urgent.entered_at = entry(1, Priority::Low, false).entered_at;
        let queue = LandingQueue::new(vec![
            entry(2, Priority::High, false),
            urgent,
            entry(1, Priority::Low, false),
        ]);
        assert_eq!(
            queue
                .entries
                .iter()
                .map(|e| e.judgment.as_i64())
                .collect::<Vec<_>>(),
            [3, 2, 1]
        );
        let mut a = entry(1, Priority::Normal, false);
        a.entered_at = entry(2, Priority::Normal, false).entered_at;
        assert_eq!(
            LandingQueue::new(vec![entry(2, Priority::Normal, false), a]).entries[0]
                .judgment
                .as_i64(),
            1
        );
    }

    #[test]
    fn only_independent_resources_allow_skipping_and_waits_keep_their_place() {
        let queue = LandingQueue::new(vec![
            entry(1, Priority::Normal, true),
            entry(2, Priority::Normal, false),
        ]);
        assert_eq!(
            LandingQueue::explain(
                &queue.entries[0],
                Capacity::default(),
                Resources::default(),
                Some(&queue.entries[0].run_id)
            ),
            None
        );
        assert_eq!(
            LandingQueue::explain(
                &queue.entries[1],
                Capacity::default(),
                Resources::default(),
                Some(&queue.entries[0].run_id)
            ),
            Some(Wait::QueueOrder)
        );
        for blocked in [
            Resources {
                disk_short: true,
                ..Default::default()
            },
            Resources {
                cleanup: true,
                ..Default::default()
            },
            Resources {
                drain: true,
                ..Default::default()
            },
        ] {
            assert_eq!(
                queue
                    .select(Capacity::default(), |e| if e.judgment.as_i64() == 1 {
                        blocked
                    } else {
                        Resources::default()
                    })
                    .unwrap()
                    .judgment
                    .as_i64(),
                2
            );
            assert!(queue.select(Capacity::default(), |_| blocked).is_none());
        }
        assert_eq!(
            queue
                .select(
                    Capacity {
                        worker_full: true,
                        ..Default::default()
                    },
                    |_| Resources::default()
                )
                .unwrap()
                .judgment
                .as_i64(),
            2
        );
        for (capacity, reason) in [
            (
                Capacity {
                    integrating: true,
                    ..Default::default()
                },
                Wait::Integrating,
            ),
            (
                Capacity {
                    turn_busy: true,
                    ..Default::default()
                },
                Wait::Turn,
            ),
            (
                Capacity {
                    held: true,
                    ..Default::default()
                },
                Wait::Held,
            ),
        ] {
            assert!(queue.select(capacity, |_| Resources::default()).is_none());
            assert_eq!(
                LandingQueue::explain(&queue.entries[0], capacity, Resources::default(), None),
                Some(reason)
            );
        }
        assert_eq!(
            queue
                .select(Capacity::default(), |_| Resources::default())
                .unwrap()
                .judgment
                .as_i64(),
            1
        );
    }

    #[test]
    fn only_ready_unleased_and_unfenced_entries_backpressure_workers() {
        assert!(
            Resources {
                disk_short: true,
                cleanup: true,
                ..Default::default()
            }
            .counts_toward_backpressure()
        );
        for facts in [
            Resources {
                leased: true,
                ..Default::default()
            },
            Resources {
                preparing: true,
                ..Default::default()
            },
            Resources {
                fenced: true,
                ..Default::default()
            },
        ] {
            assert!(!facts.counts_toward_backpressure());
        }
    }

    #[test]
    fn an_abandoned_run_or_failed_e2e_leaves_until_authorized_again() {
        for (kind, payload) in [
            (EventKind::RuntimeError, json!({"lease_released": true})),
            (EventKind::IntegrationHeld, json!({})),
            (EventKind::IntegrationFailed, json!({})),
            (EventKind::RunE2eFailed, json!({"reason": "failed"})),
            (
                EventKind::LandingRecheckFailed,
                json!({"action": "resumed"}),
            ),
        ] {
            let mut events = vec![
                event(1, EventKind::ReviewFinished, json!({"verdict": "pass"})),
                event(2, EventKind::LandingQueued, json!({"via": "exit"})),
            ];
            let build = |events: &[RunEvent]| {
                Entry::from_events(
                    RunId::new("r").unwrap(),
                    TaskId::new(1),
                    events,
                    Priority::Normal,
                    None,
                )
            };
            assert!(build(&events).is_some());
            events.push(event(3, kind, payload));
            assert!(build(&events).is_none(), "{kind:?}");
            events.push(event(
                4,
                EventKind::IntegrationApproved,
                json!({"ask_id": 1}),
            ));
            assert_eq!(build(&events).unwrap().judgment, EventId::new(4));
        }
    }

    #[test]
    fn a_trusted_error_take_up_restores_the_original_place() {
        let mut events = vec![
            event(1, EventKind::ReviewFinished, json!({"verdict": "pass"})),
            event(2, EventKind::LandingQueued, json!({"via": "exit"})),
            event(3, EventKind::IntegrationStarted, json!({})),
            event(
                4,
                EventKind::IntegrationError,
                json!({"code": "other", "build": "old"}),
            ),
        ];
        let build = |events: &[RunEvent]| {
            Entry::from_events(
                RunId::new("r").unwrap(),
                TaskId::new(1),
                events,
                Priority::Normal,
                None,
            )
        };
        assert!(build(&events).is_none());
        events.push(event(
            5,
            EventKind::LeaseAcquired,
            json!({"reason": "stranded_landing", "build_retry": {"error": 4, "build": "new"}}),
        ));
        assert_eq!(build(&events).unwrap().judgment, EventId::new(1));
        assert!(!RunHistory::from_events(&events).landing_ready());
        let mut abandoned = events.clone();
        abandoned.push(event(6, EventKind::LeaseReleased, json!({})));
        assert!(build(&abandoned).is_none());
        events.push(event(6, EventKind::LandingQueued, json!({"via": "exit"})));
        events.push(event(7, EventKind::LeaseReleased, json!({})));
        assert!(RunHistory::from_events(&events).landing_ready());
        assert_eq!(build(&events).unwrap().judgment, EventId::new(1));
        events.push(event(8, EventKind::IntegrationStarted, json!({})));
        assert!(build(&events).is_none());
    }

    #[test]
    fn requirements_are_rebuilt_from_validation_and_the_e2e_of_the_current_head() {
        let mut events = vec![event(
            1,
            EventKind::IntegrationApproved,
            json!({"ask_id": 1}),
        )];
        let build = |events: &[RunEvent], head| {
            Entry::from_events(
                RunId::new("r").unwrap(),
                TaskId::new(1),
                events,
                Priority::High,
                Some(head),
            )
            .unwrap()
        };
        assert!(!build(&events, "head").e2e);
        events.push(event(
            2,
            EventKind::ValidationFinished,
            json!({"e2e_requirement": {"required": true}}),
        ));
        let required = build(&events, "head");
        assert!(required.e2e && required.disk);
        assert_eq!(required.priority, Priority::High);
        events.push(event(
            3,
            EventKind::RunE2eFinished,
            json!({"commit": "head", "outcome": "passed"}),
        ));
        assert!(!build(&events, "head").e2e);
        assert!(build(&events, "new-head").e2e);
    }

    #[test]
    fn a_pass_waits_for_its_owner_to_finish_preparation() {
        let mut events = vec![event(
            1,
            EventKind::ReviewFinished,
            json!({"verdict": "pass"}),
        )];
        let history = RunHistory::from_events(&events);
        assert_eq!(history.landing_judgment(), Some(EventId::new(1)));
        assert!(!history.landing_ready());
        let ready = entry(1, Priority::Normal, false);
        assert_eq!(
            LandingQueue::waiting(
                &ready,
                Capacity::default(),
                Resources {
                    preparing: true,
                    ..Default::default()
                }
            ),
            Some(Wait::Preparing)
        );
        events.push(event(2, EventKind::LandingQueued, json!({"via": "exit"})));
        assert!(RunHistory::from_events(&events).landing_ready());
        events.push(event(
            3,
            EventKind::AskOpened,
            json!({"kind": "approve_landing"}),
        ));
        assert!(!RunHistory::from_events(&events).landing_ready());
    }

    #[test]
    fn authorization_sources_recovery_and_handoff_rebuild_without_reordering() {
        for (kind, payload) in [
            (EventKind::ReviewFinished, json!({"verdict": "pass"})),
            (
                EventKind::ReviewFinished,
                json!({"verdict": "concern", "recommendation": "land", "confidence": "high"}),
            ),
            (EventKind::IntegrationApproved, json!({"ask_id": 4})),
            (EventKind::IntegrationApproved, json!({"pid": 1})),
        ] {
            let mut events = vec![event(1, kind, payload)];
            let build = |events: &[RunEvent]| {
                Entry::with_requirements(
                    RunId::new("r").unwrap(),
                    TaskId::new(1),
                    events,
                    Priority::High,
                    true,
                )
            };
            let original = build(&events).unwrap();
            assert_eq!(original.basis, kind.as_str());
            assert!(original.e2e && original.disk);
            events.push(event(2, EventKind::IntegrationStarted, json!({})));
            assert!(build(&events).is_none());
            let mut recovered = events.clone();
            recovered.push(event(
                3,
                EventKind::RunRecovered,
                json!({"previous_status": "integrating"}),
            ));
            assert_eq!(build(&recovered), Some(original.clone()));
            events.push(event(3, EventKind::IntegrationHandedOver, json!({})));
            assert_eq!(build(&events), Some(original));
            events.push(event(
                5,
                EventKind::AskOpened,
                json!({"kind": "approve_landing"}),
            ));
            assert!(build(&events).is_none());
        }
        let events = [
            event(1, EventKind::ReviewFinished, json!({"verdict": "revise"})),
            event(2, EventKind::ReviseLimitJudged, json!({"converged": true})),
        ];
        let converged = Entry::with_requirements(
            RunId::new("r").unwrap(),
            TaskId::new(1),
            &events,
            Priority::Normal,
            false,
        )
        .unwrap();
        assert_eq!(converged.judgment.as_i64(), 2);
    }
}
