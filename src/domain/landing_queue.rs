//! The single, reconstructed landing order. Ownership and resource probes
//! belong to callers; selection here never takes a lease or changes a run.

use super::{Ask, AskKind, EventId, EventKind, Priority, RunEvent, RunHistory, RunId, TaskId};
use serde::Serialize;
use serde_json::{Value, json};
use std::cmp::Reverse;

/// A queued landing's durable provenance and current requirements.
/// `entry_id` and `entered_at` belong to one journaled stay, including
/// any wait for reapproval. `judgment` and `basis` describe its current
/// authorization. `priority` is the task's effective priority now.
/// `e2e` requires worker capacity; `disk` requires the landing disk budget.
/// Status adds a one-based `position` and `waiting_for` from the latest probe.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Entry {
    pub run_id: RunId,
    pub task_id: TaskId,
    pub entered_at: String,
    pub entry_id: EventId,
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

    /// An open entry waiting for a replacement judgment remains visible
    /// even while it cannot be selected.
    pub fn awaiting_reapproval(
        run_id: RunId,
        task_id: TaskId,
        events: &[RunEvent],
        priority: Priority,
        head: Option<&str>,
    ) -> Option<Self> {
        let open = EntryJournal::read(events).open?;
        let wait = open
            .wait
            .filter(|e| e.payload["waiting_for"] == "awaiting_reapproval")?;
        let id = EventId::new(wait.payload["judgment"].as_i64()?);
        let judgment = events.iter().find(|event| event.id == id)?;
        let e2e = head.is_some_and(|head| super::run_e2e::due(events, head).is_some());
        Some(Self::at(
            run_id,
            task_id,
            priority,
            e2e,
            judgment,
            open.id(),
            open.at(),
        ))
    }

    fn with_requirements(
        run_id: RunId,
        task_id: TaskId,
        events: &[RunEvent],
        priority: Priority,
        e2e: bool,
    ) -> Option<Self> {
        let id = RunHistory::from_events(events).landing_judgment()?;
        let judgment = events.iter().find(|event| event.id == id)?;
        let hold = events.iter().rev().find(|e| {
            e.kind
                .is_in(&[EventKind::TaskHeld, EventKind::TaskReleased])
        });
        if hold.is_some_and(|e| e.kind == EventKind::TaskHeld) {
            return None;
        }
        let journal = EntryJournal::read(events);
        if let Some(open) = journal.open {
            return Some(Self::at(
                run_id,
                task_id,
                priority,
                e2e,
                judgment,
                open.id(),
                open.at(),
            ));
        }
        // A ready run without an entry is a candidate for admission. This
        // also bootstraps journals created by older runtimes. After a
        // departure, a restored judgment cannot supply the next stay's time.
        let admitted = match journal.left {
            Some(left) if judgment.id <= left.id => events.last().unwrap_or(left),
            _ => judgment,
        };
        Some(Self::at(
            run_id,
            task_id,
            priority,
            e2e,
            judgment,
            admitted.id,
            &admitted.created_at,
        ))
    }

    fn at(
        run_id: RunId,
        task_id: TaskId,
        priority: Priority,
        e2e: bool,
        judgment: &RunEvent,
        entry_id: EventId,
        at: &str,
    ) -> Self {
        Self {
            run_id,
            task_id,
            entry_id,
            entered_at: at.to_owned(),
            judgment: judgment.id,
            basis: judgment.kind.as_str().to_owned(),
            priority,
            e2e,
            disk: true,
        }
    }

    pub fn entered_payload(&self) -> Value {
        let mut payload = json!(self);
        payload["at"] = json!(self.entered_at);
        payload
    }
}

/// One entry and its most recent wait. A persisted entry can outlive an
/// attempted landing and the judgment that enabled it.
#[derive(Debug, Clone, Copy)]
pub struct OpenEntry<'a> {
    pub entered: &'a RunEvent,
    wait: Option<&'a RunEvent>,
}

impl<'a> OpenEntry<'a> {
    pub fn id(self) -> EventId {
        journal_id(self.entered).unwrap_or(self.entered.id)
    }

    fn at(self) -> &'a str {
        self.entered.payload["at"]
            .as_str()
            .or_else(|| self.entered.payload["entered_at"].as_str())
            .unwrap_or(&self.entered.created_at)
    }

    pub fn awaiting_reapproval(self) -> bool {
        self.wait
            .is_some_and(|e| e.payload["waiting_for"] == "awaiting_reapproval")
    }

    fn observe_wait(&mut self, event: &'a RunEvent) {
        // A resource probe can have read the entry before the comparison
        // spent its judgment. Only a newer judgment can resume probing it.
        if self.awaiting_reapproval()
            && event.payload["waiting_for"] != "awaiting_reapproval"
            && event.payload["judgment"].as_i64()
                <= self.wait.and_then(|wait| wait.payload["judgment"].as_i64())
        {
            return;
        }
        self.wait = Some(event);
    }

    pub fn reapproval_payload(self, comparison: &Value) -> Value {
        let judgment = comparison["judgment"]
            .as_i64()
            .or_else(|| self.entered.payload["judgment"].as_i64());
        json!({"entry_id": self.id(), "judgment": judgment, "waiting_for": Wait::AwaitingReapproval})
    }
}

fn journal_id(event: &RunEvent) -> Option<EventId> {
    event.payload["entry_id"]
        .as_i64()
        .or_else(|| event.payload["judgment"].as_i64())
        .map(EventId::new)
}

/// The lifetime is reconstructed solely from entry, departure, and wait
/// records. An old journal may identify observations by judgment instead
/// of entry ID. A late write for a closed entry cannot affect its successor.
#[derive(Default)]
pub struct EntryJournal<'a> {
    pub open: Option<OpenEntry<'a>>,
    left: Option<&'a RunEvent>,
}

impl<'a> EntryJournal<'a> {
    pub fn read(events: &'a [RunEvent]) -> Self {
        let mut journal = Self::default();
        for event in events {
            match event.kind.known() {
                Some(EventKind::LandingQueueEntered) => {
                    journal.open = Some(OpenEntry {
                        entered: event,
                        wait: None,
                    });
                }
                Some(EventKind::LandingQueueLeft)
                    if journal
                        .open
                        .is_none_or(|open| journal_id(event) == Some(open.id())) =>
                {
                    journal.open = None;
                    journal.left = Some(event);
                }
                Some(EventKind::LandingQueueWaiting)
                    if let Some(open) = &mut journal.open
                        && journal_id(event) == Some(open.id()) =>
                {
                    open.observe_wait(event);
                }
                _ => {}
            }
        }
        journal
    }
}

/// The queue record a transition must write in its own transaction when
/// the run has an open entry. A landing start keeps that entry open until
/// its outcome. A mismatch alone temporarily withdraws eligibility.
#[derive(Clone, Copy)]
pub enum EntryChange<'a> {
    Left(&'a str),
    AwaitingReapproval,
    StartReview,
}

impl<'a> EntryChange<'a> {
    pub fn departure(self, open: OpenEntry<'_>) -> Option<&'a str> {
        match self {
            Self::Left(reason) => Some(reason),
            Self::StartReview if !open.awaiting_reapproval() => Some("review"),
            _ => None,
        }
    }
}

pub fn entry_change(kind: EventKind, payload: &Value) -> Option<EntryChange<'_>> {
    use EventKind::*;
    let reason = match kind {
        IntegrationCompared if payload["matched"] == false => {
            return Some(EntryChange::AwaitingReapproval);
        }
        IntegrationError
            if payload["code"] == crate::domain::ReasonCode::LandingChanged.as_str() =>
        {
            return None;
        }
        RunIntegrated => "landed",
        IntegrationDeferred | IntegrationError | IntegrationFailed | IntegrationHeld => {
            payload["code"].as_str().unwrap_or(kind.as_str())
        }
        LandingDecided => payload["answer"].as_str().unwrap_or(kind.as_str()),
        ResumeStarted => "resume",
        ReviewStarted => return Some(EntryChange::StartReview),
        ReviseRequested => "revise",
        TaskHeld | HoldParked => "hold",
        LandingRecheckFailed if payload["action"] == crate::domain::recheck::RESUMED => {
            kind.as_str()
        }
        ReviewFailed | RunE2eFailed | ReplanParked | RecoveryParked | SessionGoneParked
        | EvidenceMissing | ScopeViolation | RunSatisfiedClosed | RuntimeError => kind.as_str(),
        RunRecovered
            if matches!(
                payload["status"].as_str(),
                Some("interrupted" | "failed" | "replaced")
            ) =>
        {
            kind.as_str()
        }
        RunAgentEvalFinished if payload["outcome"] != "passed" => kind.as_str(),
        _ => return None,
    };
    Some(EntryChange::Left(reason))
}

/// A probe's reason for withholding a candidate. Local holds may be skipped;
/// the queue-wide turn and active landing withhold every candidate.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Wait {
    AwaitingReapproval,
    Held,
    Answer,
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
            Self::AwaitingReapproval => "is waiting for its replacement landing approval",
            Self::Held => "is waiting for the landing hold to clear",
            Self::Answer => "is waiting for an answer",
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

/// An open approval ask can outlive a newer judgment. A landing is
/// allowed to finish and close that ask; it does not await an answer to it.
pub fn waiting_for_answer(asks: &[Ask], history: &RunHistory<'_>) -> bool {
    asks.iter().any(|ask| {
        ask.is_open()
            && (ask.kind != AskKind::ApproveLanding || !history.supersedes_landing_ask(ask.id))
    })
}

/// Resource facts for one entry, read without mutating its ownership.
#[derive(Debug, Clone, Copy, Default)]
pub struct Resources {
    pub unanswered: bool,
    pub disk_short: bool,
    pub cleanup: bool,
    pub leased: bool,
    pub preparing: bool,
    pub fenced: bool,
    pub drain: bool,
}

impl Resources {
    /// Whether the entry has returned its lease, is prepared and is
    /// unfenced. Worker backpressure also requires the run to be able
    /// to progress by Admission's predicate.
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
        // The journal ID preserves arrival order when two entries have the
        // same recorded time. A replacement judgment cannot change that tie.
        entries.sort_by(|a, b| {
            (Reverse(a.priority), a.entered_at.as_str(), a.entry_id).cmp(&(
                Reverse(b.priority),
                b.entered_at.as_str(),
                b.entry_id,
            ))
        });
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
        } else if resources.unanswered {
            Some(Wait::Answer)
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

    /// First independently runnable entry in priority/time order. A
    /// skipped entry retains its original queue position and timestamp.
    /// For a batch, [`super::landing_batch::select`] takes up to the limit
    /// from the runnable prefix when the turn opens.
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

/// Whether the candidate has an open entry. A replacement judgment does
/// not cause a second entry event during the same stay.
pub fn recorded_entry(events: &[RunEvent], entry: &Entry) -> bool {
    EntryJournal::read(events)
        .open
        .is_some_and(|open| open.id() == entry.entry_id)
}

/// The wait written for the current entry and judgment, if both agree.
pub fn recorded_wait(events: &[RunEvent], judgment: EventId) -> Option<&RunEvent> {
    EntryJournal::read(events)
        .open?
        .wait
        .filter(|event| event.payload["judgment"].as_i64() == Some(judgment.as_i64()))
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
    fn a_new_landing_judgment_ends_the_wait_for_its_superseded_ask() {
        let mut ask: Ask = serde_json::from_value(json!({
            "id": 7, "kind": "approve_landing", "question": "Land?",
            "options": [], "asked_by": "runtime", "reason_category": "scope",
            "created_at": 0,
        }))
        .unwrap();
        let events = [
            event(
                1,
                EventKind::AskOpened,
                json!({"ask_id": 7, "kind": "approve_landing"}),
            ),
            event(2, EventKind::IntegrationApproved, json!({})),
        ];
        assert!(waiting_for_answer(
            std::slice::from_ref(&ask),
            &RunHistory::from_events(&events[..1])
        ));
        assert!(!waiting_for_answer(
            std::slice::from_ref(&ask),
            &RunHistory::from_events(&events)
        ));
        ask.kind = AskKind::WorkerQuestion;
        assert!(waiting_for_answer(
            std::slice::from_ref(&ask),
            &RunHistory::from_events(&events)
        ));
        ask.answered_at = Some(1);
        assert!(!waiting_for_answer(
            &[ask],
            &RunHistory::from_events(&events)
        ));
    }

    #[test]
    fn an_answer_wait_preserves_position_and_yields_to_the_next_entry() {
        let queue = LandingQueue::new(vec![
            entry(1, Priority::Normal, false),
            entry(2, Priority::High, false),
        ]);
        let capacity = Capacity::default();
        let head = &queue.entries[0];
        assert_eq!(head.task_id, TaskId::new(2));
        let blocked = Resources {
            unanswered: true,
            ..Resources::default()
        };
        let next = queue
            .select(capacity, |entry| {
                if entry.run_id == head.run_id {
                    blocked
                } else {
                    Resources::default()
                }
            })
            .unwrap();
        assert_eq!(next.task_id, TaskId::new(1));
        assert_eq!(
            LandingQueue::explain(head, capacity, blocked, Some(&next.run_id)),
            Some(Wait::Answer)
        );
        assert_eq!(
            queue
                .select(capacity, |_| Resources::default())
                .unwrap()
                .task_id,
            TaskId::new(2)
        );
    }

    fn reapproval_events() -> Vec<RunEvent> {
        vec![
            event(1, EventKind::ReviewFinished, json!({"verdict": "pass"})),
            event(
                2,
                EventKind::LandingQueueEntered,
                json!({"entry_id": 1, "at": "2026-11-18T01:00:00Z", "judgment": 1}),
            ),
            event(3, EventKind::IntegrationStarted, json!({})),
            event(
                4,
                EventKind::IntegrationCompared,
                json!({"matched": false, "judgment": 1, "reapproval": "review"}),
            ),
            event(
                5,
                EventKind::LandingQueueWaiting,
                json!({"entry_id": 1, "judgment": 1, "waiting_for": "awaiting_reapproval"}),
            ),
            event(
                6,
                EventKind::IntegrationError,
                json!({"code": "landing_changed"}),
            ),
            event(7, EventKind::ReviewStarted, json!({})),
        ]
    }

    fn reapproved_run(events: &[RunEvent]) -> Option<Entry> {
        Entry::from_events(
            RunId::new("reapproved").unwrap(),
            TaskId::new(1),
            events,
            Priority::Normal,
            None,
        )
    }

    #[test]
    fn reapproval_restores_the_first_entry_time_and_keeps_the_current_judgment() {
        let mut events = reapproval_events();
        events.push(event(
            8,
            EventKind::LandingQueueWaiting,
            json!({"entry_id": 1, "judgment": 1, "waiting_for": "integrating"}),
        ));
        assert!(reapproved_run(&events).is_none());
        let open = EntryJournal::read(&events).open.unwrap();
        assert!(
            entry_change(EventKind::ReviewStarted, &Value::Null)
                .unwrap()
                .departure(open)
                .is_none()
        );
        let waiting = Entry::awaiting_reapproval(
            RunId::new("reapproved").unwrap(),
            TaskId::new(1),
            &events,
            Priority::Normal,
            None,
        )
        .unwrap();
        assert_eq!(waiting.entry_id, EventId::new(1));
        assert_eq!(waiting.entered_at, "2026-11-18T01:00:00Z");

        events.push(event(
            9,
            EventKind::ReviewFinished,
            json!({"verdict": "pass"}),
        ));
        let restored = reapproved_run(&events).unwrap();
        assert_eq!(restored.judgment, EventId::new(9));
        assert_eq!(restored.entry_id, EventId::new(1));
        assert_eq!(restored.entered_at, "2026-11-18T01:00:00Z");
        assert!(recorded_wait(&events, restored.judgment).is_none());
        assert_eq!(
            LandingQueue::new(vec![entry(4, Priority::Normal, false), restored]).entries[0]
                .entry_id,
            EventId::new(1)
        );
    }

    #[test]
    fn verification_failure_revise_and_resume_end_the_entry_before_a_later_pass() {
        for (kind, payload, expected) in [
            (
                EventKind::IntegrationDeferred,
                json!({"code": "verification_failed"}),
                "verification_failed",
            ),
            (EventKind::ReviseRequested, json!({}), "revise"),
            (EventKind::ResumeStarted, json!({}), "resume"),
        ] {
            let Some(EntryChange::Left(reason)) = entry_change(kind, &payload) else {
                panic!("{kind:?} must end an open entry");
            };
            assert_eq!(reason, expected);
            let mut events = reapproval_events();
            match kind {
                EventKind::IntegrationDeferred => {
                    events.push(event(
                        8,
                        EventKind::ReviewFinished,
                        json!({"verdict": "pass"}),
                    ));
                    events.push(event(9, EventKind::IntegrationStarted, json!({})));
                }
                EventKind::ReviseRequested => {
                    events.push(event(
                        8,
                        EventKind::ReviewFinished,
                        json!({"verdict": "revise"}),
                    ));
                }
                _ => {}
            }
            events.extend([
                event(
                    10,
                    EventKind::LandingQueueLeft,
                    json!({"entry_id": 1, "reason": expected}),
                ),
                event(11, kind, payload),
                event(12, EventKind::ReviewStarted, json!({})),
                event(13, EventKind::ReviewFinished, json!({"verdict": "pass"})),
            ]);
            let later = reapproved_run(&events).unwrap();
            assert_eq!(later.entry_id, EventId::new(13));
            assert_eq!(later.entered_at, "2026-11-18T13:00:00Z");
            assert!(!recorded_entry(&events, &later));
            assert_eq!(
                LandingQueue::new(vec![later, entry(4, Priority::Normal, false)]).entries[0]
                    .entry_id,
                EventId::new(4)
            );
        }
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
    fn effective_priority_precedes_time_and_exact_ties_keep_entry_order() {
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
        a.judgment = EventId::new(3);
        assert_eq!(
            LandingQueue::new(vec![entry(2, Priority::Normal, false), a]).entries[0]
                .entry_id
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
