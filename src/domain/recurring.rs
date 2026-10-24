//! Recurring drafts (ADR-0072): a proposal the repository declares in
//! `[[recurring]]` of `dagq.toml` with an interval in days. When the
//! interval since the last registration of its key has passed, the
//! supervisor registers one draft of origin `recurring`, and a planner of
//! the runtime's decides whether it is needed (adopts it with edit and
//! submit, or cancels it with a note). The setting is material for the
//! proposal, never an order to run or adopt it.
//!
//! The history of a key is the queue's `draft_origins` rows of origin
//! `recurring` whose material names the key: the interval counts from the
//! last registration (not from when its task ended), a key never
//! registered is due at once, and a supervisor that was stopped past the
//! due time registers one draft, not one per missed interval. While a
//! registered task of the key is neither `completed` nor `canceled`, no
//! next one is registered.

use serde::Serialize;
use serde_json::{Value, json};

use super::{EvidenceCheck, GoalId, NewTask, TaskId};

/// Seconds in one day of `interval_days` (a day is 24 hours).
pub const DAY_SECS: i64 = 86_400;

/// One `[[recurring]]` table of `dagq.toml`: a permanent `key` unique in
/// the file, the interval in whole days, and the material of the draft
/// (each required but `goal`, the goal its draft is proposed to belong to).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RecurringDraft {
    /// The permanent name of the proposal in the queue: its history and
    /// its overlap check go by it.
    pub key: String,
    /// Whole days (of 24 hours) from one registration to the next.
    pub interval_days: i64,
    /// The draft's title, description and acceptance.
    pub title: String,
    pub description: String,
    pub acceptance: String,
    /// The verification commands it proposes.
    pub verification: Vec<String>,
    /// The paths it may change, globs under a task's rules.
    pub paths: Vec<String>,
    /// The receipt checks it requires, as a task's.
    pub evidence: Vec<EvidenceCheck>,
    /// The goal it proposes the draft to belong to.
    pub goal: Option<GoalId>,
}

impl RecurringDraft {
    /// The interval in seconds.
    pub const fn interval_secs(&self) -> i64 {
        self.interval_days.saturating_mul(DAY_SECS)
    }

    /// The draft registered from it, in `goal` (the configured goal when
    /// it is open, else none).
    pub fn new_task(&self, goal: Option<GoalId>) -> NewTask {
        NewTask {
            title: self.title.clone(),
            description: self.description.clone(),
            acceptance: self.acceptance.clone(),
            verification_commands: self.verification.clone(),
            dependencies: Vec::new(),
            goal_dependencies: Vec::new(),
            goal_id: goal,
            context: format!(
                "A recurring proposal of dagq.toml [[recurring]] key {:?}, registered every {} days. Check whether the work is needed now before adopting it.",
                self.key, self.interval_days
            ),
            required_evidence: self.evidence.clone(),
            paths: self.paths.clone(),
            priority: None,
            change: None,
            provider: None,
            worker_mode: None,
            wait_for_build: false,
            execution_class: Default::default(),
        }
    }

    /// The setting as it was at a registration, kept in the draft's
    /// origin: a later change of the file takes effect from the next one.
    pub fn snapshot(&self) -> Value {
        json!(self)
    }
}

/// When the key whose last registration was at `last` (Unix seconds, none
/// when it was never registered) is due: none is due at once.
pub fn due_at(last: Option<i64>, interval_days: i64) -> Option<i64> {
    last.map(|at| at.saturating_add(interval_days.saturating_mul(DAY_SECS)))
}

/// What holds the draft of a key back at `now`, from what the queue
/// shows of the key: `open`, a task of it neither `completed` nor
/// `canceled`, and `last`, when it was last registered. `None` registers
/// it: a key never registered, or one whose interval since its last
/// registration passed however long ago (a stopped supervisor catches up
/// once, and the next interval counts from that registration).
pub fn held_back(
    now: i64,
    interval_days: i64,
    last: Option<i64>,
    open: Option<TaskId>,
) -> Option<RecurringOutcome> {
    if let Some(task_id) = open {
        return Some(RecurringOutcome::Open { task_id });
    }
    due_at(last, interval_days)
        .filter(|due| *due > now)
        .map(|due_at| RecurringOutcome::NotDue { due_at })
}

/// The goal the draft joins: the one its table names while that goal
/// `accepts` tasks (open, not a draft goal), else none, left to the
/// planner that adopts it.
pub fn joined_goal(named: Option<GoalId>, accepts: bool) -> Option<GoalId> {
    named.filter(|_| accepts)
}

/// The material of the draft's origin: its key, the previous registration
/// of the key (`last`: the task and when; nulls the first time) and the
/// setting as it is now.
pub fn origin_material(item: &RecurringDraft, last: Option<(TaskId, i64)>) -> Value {
    json!({
        "key": item.key,
        "previous_task_id": last.map(|(task, _)| task),
        "previous_registered_at": last.map(|(_, at)| at),
        "setting": item.snapshot(),
    })
}

/// What a supervisor's pass did with one `[[recurring]]` key.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum RecurringOutcome {
    /// A draft was registered.
    Registered { task_id: TaskId },
    /// A registered task of the key has not ended yet.
    Open { task_id: TaskId },
    /// The interval since the last registration has not passed.
    NotDue { due_at: i64 },
}

/// One key as `status` and `stats` show it: the last registration, the
/// task of the key not ended yet (which holds the next back), and when the
/// next one is due (`next_at`, Unix seconds; none for a key never
/// registered, which is registered on the next pass).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RecurringSchedule {
    pub key: String,
    pub interval_days: i64,
    pub last_task_id: Option<TaskId>,
    pub last_registered_at: Option<i64>,
    pub open_task_id: Option<TaskId>,
    pub next_at: Option<i64>,
    /// `next_at` in RFC 3339 (UTC).
    pub next_at_utc: Option<String>,
}

impl RecurringSchedule {
    /// The schedule of `key` every `interval_days` from its last
    /// registration (`last`: the task and when) and the task of the key not
    /// ended yet.
    pub fn of(
        key: &str,
        interval_days: i64,
        last: Option<(TaskId, i64)>,
        open: Option<TaskId>,
    ) -> Self {
        let next_at = due_at(last.map(|(_, at)| at), interval_days);
        Self {
            key: key.to_owned(),
            interval_days,
            last_task_id: last.map(|(task, _)| task),
            last_registered_at: last.map(|(_, at)| at),
            open_task_id: open,
            next_at,
            next_at_utc: next_at.map(|at| super::marks::utc_text(at.saturating_mul(1000))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rust() -> RecurringDraft {
        RecurringDraft {
            key: "rust-toolchain".into(),
            interval_days: 42,
            title: "chore: raise Rust".into(),
            description: "d".into(),
            acceptance: "a".into(),
            verification: vec!["cargo test".into()],
            paths: vec!["rust-toolchain.toml".into()],
            evidence: vec![EvidenceCheck::Tests],
            goal: Some(GoalId::new(46)),
        }
    }

    /// A key never registered is due at once; one registered is due its
    /// interval after that registration.
    #[test]
    fn a_key_is_due_its_interval_after_its_last_registration() {
        assert_eq!(due_at(None, 42), None);
        assert_eq!(due_at(Some(1_000), 42), Some(1_000 + 42 * DAY_SECS));
        assert_eq!(rust().interval_secs(), 42 * DAY_SECS);
    }

    /// The draft carries the material, the goal it is given and no
    /// dependency; the snapshot keeps every field.
    #[test]
    fn the_draft_carries_the_material_of_its_table() {
        let task = rust().new_task(None);
        assert_eq!(task.title, "chore: raise Rust");
        assert_eq!(task.verification_commands, vec!["cargo test"]);
        assert_eq!(task.paths, vec!["rust-toolchain.toml"]);
        assert_eq!(task.required_evidence, vec![EvidenceCheck::Tests]);
        assert_eq!(task.goal_id, None);
        assert!(task.context.contains("rust-toolchain"));
        task.validate().unwrap();
        let snapshot = rust().snapshot();
        assert_eq!(snapshot["key"], "rust-toolchain");
        assert_eq!(snapshot["interval_days"], 42);
        assert_eq!(snapshot["evidence"], json!(["tests"]));
        assert_eq!(snapshot["goal"], 46);
    }

    /// A key never registered is registered at once; one registered is
    /// held back until its interval passed, by the second, and registered
    /// once however many intervals passed; a task of the key not ended
    /// holds it back even past its interval.
    #[test]
    fn a_key_is_held_back_by_its_open_task_and_its_interval() {
        let at = 1_000;
        let next = at + 42 * DAY_SECS;
        assert_eq!(held_back(at, 42, None, None), None);
        assert_eq!(
            held_back(next - 1, 42, Some(at), None),
            Some(RecurringOutcome::NotDue { due_at: next })
        );
        assert_eq!(held_back(next, 42, Some(at), None), None);
        assert_eq!(held_back(at + 200 * DAY_SECS, 42, Some(at), None), None);
        let open = TaskId::new(7);
        for now in [at, next, at + 200 * DAY_SECS] {
            assert_eq!(
                held_back(now, 42, Some(at), Some(open)),
                Some(RecurringOutcome::Open { task_id: open })
            );
        }
        assert_eq!(
            held_back(at, 42, None, Some(open)),
            Some(RecurringOutcome::Open { task_id: open })
        );
    }

    /// The draft joins its named goal only while that goal accepts tasks.
    #[test]
    fn the_draft_joins_its_goal_only_while_it_accepts_tasks() {
        let goal = Some(GoalId::new(46));
        assert_eq!(joined_goal(goal, true), goal);
        assert_eq!(joined_goal(goal, false), None);
        assert_eq!(joined_goal(None, true), None);
    }

    /// The origin's material names the previous registration, and nulls
    /// the first time.
    #[test]
    fn the_origin_material_names_the_previous_registration() {
        let first = origin_material(&rust(), None);
        assert_eq!(first["key"], "rust-toolchain");
        assert_eq!(first["previous_task_id"], Value::Null);
        assert_eq!(first["previous_registered_at"], Value::Null);
        assert_eq!(first["setting"]["interval_days"], 42);
        let next = origin_material(&rust(), Some((TaskId::new(4), 1_000)));
        assert_eq!(next["previous_task_id"], 4);
        assert_eq!(next["previous_registered_at"], 1_000);
    }

    /// The schedule shows when the next registration is due, and none for
    /// a key never registered.
    #[test]
    fn the_schedule_shows_the_next_registration() {
        let never = RecurringSchedule::of("k", 1, None, None);
        assert_eq!(never.next_at, None);
        assert_eq!(never.next_at_utc, None);
        let last = RecurringSchedule::of("k", 1, Some((TaskId::new(7), 0)), Some(TaskId::new(7)));
        assert_eq!(last.next_at, Some(DAY_SECS));
        assert_eq!(
            last.next_at_utc.as_deref(),
            Some("1970-01-02T00:00:00.000Z")
        );
        assert_eq!(last.open_task_id, Some(TaskId::new(7)));
    }
}
