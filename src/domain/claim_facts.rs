//! What `run_claimed` records of the claim beyond the run itself
//! (ADR-t1662-1 decisions 6 and 21 to 26, docs/design/measurement.md
//! "claimの前" and "runの比較の軸"): where the claimed task stood among the
//! candidates and the priority it was claimed at ([`CandidateFacts`], which
//! the supervisor passes for every candidate under [`BY_TASK`] and the
//! claim keeps the claimed task's of, [`settle`]), when the task became
//! claimable ([`claimable_since`]), and the claim's attributes as an open set of
//! keys and values ([`attributes`]). The claim also records the claimed
//! task's `execution_class` and, in the facts, whether the aging across
//! the execution classes moved it ahead within its effective priority
//! (`aged`, ADR-t1489-1).

use std::collections::{BTreeMap, BTreeSet};

use serde::Serialize;
use serde_json::{Map, Value};

use super::{Priority, PrioritySource, RunEvent, TaskId, stats::timestamp_millis};

/// The key of the claim's attributes holding each candidate's
/// [`CandidateFacts`], by task ID, until the claim knows its task
/// ([`settle`]); `run_claimed` never keeps it.
pub const BY_TASK: &str = "candidates_by_task";

/// What the claim knew of one candidate when it lined them up: its place
/// in the line (`candidate_rank`, from 1), its priority as the claim
/// order compared it (`effective_priority`), from its base (`priority`,
/// [`super::base_priority`]) and where that came from, and whether the
/// aging across the execution classes moved it ahead within its effective
/// priority (`aged`,
/// ADR-t1487-2 decision 4).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct CandidateFacts {
    pub candidate_rank: usize,
    pub priority: Priority,
    pub priority_source: PrioritySource,
    pub effective_priority: Priority,
    pub aged: bool,
}

/// The facts of each task of `order` (the candidates of a claim, in the
/// order it takes them, `aged` those aging moved ahead), with their
/// priorities from `priorities` (base, source, effective); a task it has
/// none of is left out.
pub fn candidate_facts(
    order: &[TaskId],
    aged: &[TaskId],
    priorities: impl Fn(TaskId) -> Option<(Priority, PrioritySource, Priority)>,
) -> BTreeMap<String, CandidateFacts> {
    order
        .iter()
        .enumerate()
        .filter_map(|(index, task)| {
            let (priority, priority_source, effective_priority) = priorities(*task)?;
            Some((
                task.to_string(),
                CandidateFacts {
                    candidate_rank: index + 1,
                    priority,
                    priority_source,
                    effective_priority,
                    aged: aged.contains(task),
                },
            ))
        })
        .collect()
}

/// Put the facts of `task` from [`BY_TASK`] at the top of a claim's
/// attributes and drop the others; a task without an entry (claimed from
/// outside the line) gets none.
pub fn settle(attributes: &mut Map<String, Value>, task: TaskId) {
    let Some(mut by_task) = attributes.remove(BY_TASK) else {
        return;
    };
    if let Some(Value::Object(facts)) = by_task.get_mut(task.to_string()).map(Value::take) {
        attributes.extend(facts);
    }
}

/// When `task` last became claimable (ADR-t1487-2 decision 2), replayed
/// from the events that move it in and out of the candidates, in the
/// order of their IDs: its own creation, status changes, claims (a claim
/// moves it to `in_progress` with no status change of its own),
/// dependencies and goal dependencies added and removed, and goal
/// changes; the status
/// changes of the tasks it depends on; and the creation, opening from
/// draft, closes and reopenings of the goals it depends on or belongs to.
/// It is a candidate while it is `ready`, every task it depends on is
/// `completed`, every goal it depends on is closed as achieved, and its
/// own goal is not a draft (`READY_QUERY`). The `created_at` of the event
/// that last made it one, whatever came after; `None` when none did. A
/// dependency met before it was added, or removed after it was met, moves
/// nothing; one removed while unmet returns the task at the removal.
pub fn claimable_since(task: TaskId, events: &[RunEvent]) -> Option<String> {
    use super::event_kind::EventKind as K;
    use super::{GoalId, GoalStatus, GoalVerdict, TaskStatus};
    use crate::domain::run::payload::restore;
    let mut sorted: Vec<&RunEvent> = events.iter().collect();
    sorted.sort_by_key(|event| event.id);
    let mut ready = false;
    let mut goal: Option<GoalId> = None;
    let mut dependencies: BTreeSet<TaskId> = BTreeSet::new();
    let mut completed: BTreeSet<TaskId> = BTreeSet::new();
    let mut goal_dependencies: BTreeSet<GoalId> = BTreeSet::new();
    let mut achieved: BTreeSet<GoalId> = BTreeSet::new();
    let mut drafts: BTreeSet<GoalId> = BTreeSet::new();
    let mut candidate = false;
    let mut since = None;
    for event in sorted {
        let kind = &event.kind;
        let payload = &event.payload;
        if event.task_id == Some(task) {
            if *kind == K::TaskCreated {
                let created: restored::TaskCreated = restore(payload);
                ready = created.status == Some(TaskStatus::Ready);
                goal = created.goal_id;
            } else if *kind == K::TaskStatusChanged {
                let changed: restored::TaskStatusChanged = restore(payload);
                ready = changed.to == Some(TaskStatus::Ready);
            } else if *kind == K::RunClaimed {
                ready = false;
            } else if *kind == K::DependencyAdded {
                let added: restored::DependencyChanged = restore(payload);
                dependencies.extend(added.predecessor_id);
            } else if *kind == K::DependencyRemoved {
                let removed: restored::DependencyChanged = restore(payload);
                if let Some(id) = removed.predecessor_id {
                    dependencies.remove(&id);
                }
            } else if *kind == K::GoalDependencyAdded {
                let added: restored::GoalDependencyChanged = restore(payload);
                goal_dependencies.extend(added.goal_id);
            } else if *kind == K::GoalDependencyRemoved {
                let removed: restored::GoalDependencyChanged = restore(payload);
                if let Some(id) = removed.goal_id {
                    goal_dependencies.remove(&id);
                }
            } else if *kind == K::TaskGoalChanged {
                let moved: restored::TaskGoalChanged = restore(payload);
                goal = moved.to;
            }
        } else if let Some(other) = event.task_id {
            if *kind == K::TaskStatusChanged {
                let changed: restored::TaskStatusChanged = restore(payload);
                if changed.to == Some(TaskStatus::Completed) {
                    completed.insert(other);
                } else {
                    completed.remove(&other);
                }
            }
        } else if let Some(goal_id) = event.goal_id {
            if *kind == K::GoalCreated {
                let created: restored::GoalCreated = restore(payload);
                if created.goal.and_then(|goal| goal.status) == Some(GoalStatus::Draft) {
                    drafts.insert(goal_id);
                }
            } else if *kind == K::GoalStatusChanged {
                let changed: restored::GoalStatusChanged = restore(payload);
                if changed.to == Some(GoalStatus::Draft) {
                    drafts.insert(goal_id);
                } else {
                    drafts.remove(&goal_id);
                }
            } else if *kind == K::GoalClosed {
                let closed: restored::GoalClosed = restore(payload);
                if closed.verdict == Some(GoalVerdict::Achieved) {
                    achieved.insert(goal_id);
                } else {
                    achieved.remove(&goal_id);
                }
            } else if *kind == K::GoalReopened {
                // Reopened open, whatever it was before it closed.
                achieved.remove(&goal_id);
                drafts.remove(&goal_id);
            }
        }
        let now = ready
            && dependencies.is_subset(&completed)
            && goal_dependencies.is_subset(&achieved)
            && goal.is_none_or(|goal| !drafts.contains(&goal));
        if now && !candidate && timestamp_millis(&event.created_at).is_some() {
            since = Some(event.created_at.clone());
        }
        candidate = now;
    }
    since
}

/// The payloads [`claimable_since`] reads, restored as typed values with
/// `run::payload::restore`: each type names the keys it reads once, and a
/// missing key, `null` or a value of another type (an older or broken
/// writer) reads as absent, so no recorded event is refused.
mod restored {
    use serde::Deserialize;

    use crate::domain::run::payload::typed;
    use crate::domain::{GoalId, GoalStatus, GoalVerdict, TaskId, TaskStatus};

    /// `task_created`.
    #[derive(Debug, Default, Deserialize)]
    pub(super) struct TaskCreated {
        #[serde(default, deserialize_with = "typed")]
        pub(super) status: Option<TaskStatus>,
        #[serde(default, deserialize_with = "typed")]
        pub(super) goal_id: Option<GoalId>,
    }

    /// `task_status_changed`.
    #[derive(Debug, Default, Deserialize)]
    pub(super) struct TaskStatusChanged {
        #[serde(default, deserialize_with = "typed")]
        pub(super) to: Option<TaskStatus>,
    }

    /// `dependency_added` and `dependency_removed`.
    #[derive(Debug, Default, Deserialize)]
    pub(super) struct DependencyChanged {
        #[serde(default, deserialize_with = "typed")]
        pub(super) predecessor_id: Option<TaskId>,
    }

    /// `goal_dependency_added` and `goal_dependency_removed`.
    #[derive(Debug, Default, Deserialize)]
    pub(super) struct GoalDependencyChanged {
        #[serde(default, deserialize_with = "typed")]
        pub(super) goal_id: Option<GoalId>,
    }

    /// `task_goal_changed`: the goal it moved to, absent out of any goal.
    #[derive(Debug, Default, Deserialize)]
    pub(super) struct TaskGoalChanged {
        #[serde(default, deserialize_with = "typed")]
        pub(super) to: Option<GoalId>,
    }

    /// `goal_created`: the goal as its JSON shows it.
    #[derive(Debug, Default, Deserialize)]
    pub(super) struct GoalCreated {
        #[serde(default, deserialize_with = "typed")]
        pub(super) goal: Option<GoalRecord>,
    }

    #[derive(Debug, Default, Deserialize)]
    pub(super) struct GoalRecord {
        #[serde(default, deserialize_with = "typed")]
        pub(super) status: Option<GoalStatus>,
    }

    /// `goal_status_changed`.
    #[derive(Debug, Default, Deserialize)]
    pub(super) struct GoalStatusChanged {
        #[serde(default, deserialize_with = "typed")]
        pub(super) to: Option<GoalStatus>,
    }

    /// `goal_closed`.
    #[derive(Debug, Default, Deserialize)]
    pub(super) struct GoalClosed {
        #[serde(default, deserialize_with = "typed")]
        pub(super) verdict: Option<GoalVerdict>,
    }
}

/// The key of `run_claimed` that holds [`attributes`].
pub const ATTRIBUTES: &str = "attributes";

/// The names the kpi's derived axes use (`axis_value`): a claim's
/// attribute never takes one, so that reading a layer by its key reads
/// the derived value for these and the attribute for any other.
pub const RESERVED: [&str; 10] = [
    "provider",
    "claude",
    "model",
    "group",
    "slot",
    "load",
    "toolchain",
    "change",
    "area",
    "nature",
];

/// Each attribute and the field of `run_claimed` it is the value of,
/// tried in order (the first one present): the one place a new attribute
/// is added. The fields are those the claim wrote from
/// [`super::measure::ClaimAttributes`], the run and its session.
pub const KEYS: [(&str, &[&str]); 19] = [
    ("build", &["dagq_version"]),
    ("claude_version", &["claude_version"]),
    ("codex", &["codex_version"]),
    ("rustc_release", &["rustc_release"]),
    ("rustc_host", &["rustc_host"]),
    ("parallel", &["parallel"]),
    ("slots", &["slots"]),
    ("load_avg", &["load_avg"]),
    ("requested_provider", &["requested_provider", "provider"]),
    ("claim_provider", &["provider"]),
    ("route", &["worker_mode"]),
    ("claim_model", &["model"]),
    ("effort", &["effort"]),
    ("trial_group", &["group"]),
    ("instructions_prompt", &["instructions_prompt"]),
    ("instructions_plugin", &["instructions_plugin"]),
    ("instructions_repo", &["instructions_repo"]),
    ("priority", &["priority"]),
    ("priority_source", &["priority_source"]),
];

/// The claim's attributes, from the fields of its `run_claimed` (`payload`)
/// by [`KEYS`]: each value as text, and no key for a value that is absent
/// or null. A key in [`RESERVED`] is never set.
pub fn attributes(payload: &Map<String, Value>) -> Map<String, Value> {
    let mut set = Map::new();
    for (key, fields) in KEYS {
        if RESERVED.contains(&key) {
            continue;
        }
        let value = fields.iter().find_map(|field| match payload.get(*field) {
            None | Some(Value::Null) => None,
            Some(Value::String(text)) => Some(text.clone()),
            Some(other) => Some(other.to_string()),
        });
        if let Some(value) = value {
            set.insert(key.to_owned(), Value::String(value));
        }
    }
    set
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::instructions::{self, InstructionVersions};
    use crate::domain::measure::{ClaimAttributes, HostVersions};
    use serde_json::json;

    #[test]
    fn the_candidates_carry_their_rank_and_priorities_and_the_claim_keeps_its_own() {
        let order = [TaskId::new(7), TaskId::new(3), TaskId::new(9)];
        let facts = candidate_facts(&order, &[TaskId::new(3)], |task| {
            (task != TaskId::new(9)).then_some((
                Priority::Normal,
                if task == TaskId::new(3) {
                    PrioritySource::Goal
                } else {
                    PrioritySource::Task
                },
                Priority::High,
            ))
        });
        assert_eq!(facts.len(), 2);
        assert_eq!(facts["3"].candidate_rank, 2);
        let mut attributes = Map::new();
        attributes.insert(BY_TASK.to_owned(), serde_json::to_value(&facts).unwrap());
        attributes.insert("candidates".to_owned(), json!(3));
        let mut claimed = attributes.clone();
        settle(&mut claimed, TaskId::new(3));
        assert_eq!(
            Value::Object(claimed),
            json!({
                "candidates": 3,
                "candidate_rank": 2,
                "priority": "normal",
                "priority_source": "goal",
                "effective_priority": "high",
                "aged": true,
            })
        );
        // A task claimed from outside the line gets no facts.
        settle(&mut attributes, TaskId::new(1));
        assert_eq!(Value::Object(attributes), json!({"candidates": 3}));
    }

    /// Events for [`claimable_since`]: each with its ID, on `task` (or a
    /// goal when `task` is 0), at minute `minute`.
    fn at(
        id: i64,
        task: i64,
        goal: Option<i64>,
        kind: &str,
        payload: Value,
        minute: i64,
    ) -> RunEvent {
        RunEvent {
            id: crate::domain::EventId::new(id),
            task_id: (task != 0).then(|| TaskId::new(task)),
            goal_id: goal.map(crate::domain::GoalId::new),
            run_id: None,
            kind: kind.into(),
            payload,
            created_at: format!("2026-10-01T10:{minute:02}:00.000Z"),
            actor: None,
        }
    }

    const T: i64 = 7;

    fn created(id: i64, status: &str, goal: Option<i64>, minute: i64) -> RunEvent {
        at(
            id,
            T,
            None,
            "task_created",
            json!({"status": status, "goal_id": goal}),
            minute,
        )
    }

    fn status(id: i64, task: i64, to: &str, minute: i64) -> RunEvent {
        at(
            id,
            task,
            None,
            "task_status_changed",
            json!({"to": to}),
            minute,
        )
    }

    fn own(id: i64, kind: &str, payload: Value, minute: i64) -> RunEvent {
        at(id, T, None, kind, payload, minute)
    }

    fn goal(id: i64, goal: i64, kind: &str, payload: Value, minute: i64) -> RunEvent {
        at(id, 0, Some(goal), kind, payload, minute)
    }

    fn since(events: &[RunEvent]) -> Option<String> {
        claimable_since(TaskId::new(T), events)
    }

    fn minute(minute: i64) -> Option<String> {
        Some(format!("2026-10-01T10:{minute:02}:00.000Z"))
    }

    /// What an older or broken writer left reads as absent, never as a
    /// refusal: an unknown status, a value of another type, a payload that
    /// is no object.
    #[test]
    fn an_unreadable_payload_reads_as_absent() {
        assert_eq!(since(&[created(1, "someday", None, 0)]), None);
        let events = [
            created(1, "draft", None, 0),
            at(2, T, None, "task_status_changed", json!({"to": 7}), 5),
            at(3, T, None, "task_status_changed", json!("ready"), 6),
        ];
        assert_eq!(since(&events), None);
        // A predecessor named by a string is no dependency to wait for.
        let events = [
            created(1, "ready", None, 0),
            own(2, "dependency_added", json!({"predecessor_id": "8"}), 1),
        ];
        assert_eq!(since(&events), minute(0));
    }

    #[test]
    fn a_task_is_claimable_from_its_move_to_ready_or_its_creation_ready() {
        assert_eq!(since(&[created(1, "draft", None, 0)]), None);
        assert_eq!(
            since(&[created(1, "draft", None, 0), status(2, T, "ready", 5)]),
            minute(5)
        );
        assert_eq!(since(&[created(1, "ready", None, 3)]), minute(3));
        // Claimed (the claim records no status change of its own), then
        // back to ready after a failed run: claimable again from then.
        let events = [
            created(1, "draft", None, 0),
            status(2, T, "ready", 5),
            own(
                3,
                "run_claimed",
                json!({"from": "ready", "to": "in_progress"}),
                6,
            ),
            status(4, T, "ready", 9),
        ];
        assert_eq!(since(&events), minute(9));
        // Claimed now: the last time it became one stands.
        assert_eq!(since(&events[..3]), minute(5));
    }

    #[test]
    fn a_task_dependency_holds_it_until_completed_and_its_edits_count_only_when_they_move_it() {
        let base = [
            created(1, "draft", None, 0),
            own(2, "dependency_added", json!({"predecessor_id": 8}), 0),
            status(3, T, "ready", 1),
        ];
        assert_eq!(since(&base), None);
        let mut met = base.to_vec();
        met.push(status(4, 8, "completed", 20));
        assert_eq!(since(&met), minute(20));
        // Removed after it was met: the completion stands.
        let mut removed = met.clone();
        removed.push(own(
            5,
            "dependency_removed",
            json!({"predecessor_id": 8}),
            30,
        ));
        assert_eq!(since(&removed), minute(20));
        // Removed while unmet: claimable from the removal.
        let mut unmet = base.to_vec();
        unmet.push(own(
            4,
            "dependency_removed",
            json!({"predecessor_id": 8}),
            25,
        ));
        assert_eq!(since(&unmet), minute(25));
        // Added after it was met, kept or removed: nothing moves.
        let late = [
            created(1, "draft", None, 0),
            status(2, T, "ready", 0),
            status(3, 8, "completed", 10),
            own(4, "dependency_added", json!({"predecessor_id": 8}), 20),
        ];
        assert_eq!(since(&late), minute(0));
        let mut late_removed = late.to_vec();
        late_removed.push(own(
            5,
            "dependency_removed",
            json!({"predecessor_id": 8}),
            30,
        ));
        assert_eq!(since(&late_removed), minute(0));
        // A predecessor reopened takes it out; completed again, back.
        let mut reopened = met.clone();
        reopened.push(status(5, 8, "ready", 40));
        reopened.push(status(6, 8, "completed", 50));
        assert_eq!(since(&reopened), minute(50));
    }

    #[test]
    fn a_goal_dependency_holds_it_until_closed_as_achieved() {
        let base = [
            created(1, "draft", None, 0),
            own(2, "goal_dependency_added", json!({"goal_id": 4}), 0),
            status(3, T, "ready", 1),
        ];
        assert_eq!(since(&base), None);
        let mut achieved = base.to_vec();
        achieved.push(goal(
            4,
            4,
            "goal_closed",
            json!({"verdict": "achieved"}),
            20,
        ));
        assert_eq!(since(&achieved), minute(20));
        let mut removed = achieved.clone();
        removed.push(own(5, "goal_dependency_removed", json!({"goal_id": 4}), 30));
        assert_eq!(since(&removed), minute(20));
        // Abandoned never releases it; removed then, claimable from then.
        let mut abandoned = base.to_vec();
        abandoned.push(goal(
            4,
            4,
            "goal_closed",
            json!({"verdict": "abandoned"}),
            20,
        ));
        assert_eq!(since(&abandoned), None);
        abandoned.push(own(5, "goal_dependency_removed", json!({"goal_id": 4}), 30));
        assert_eq!(since(&abandoned), minute(30));
        // Reopened takes it out; achieved again, back.
        achieved.push(goal(5, 4, "goal_reopened", json!({}), 40));
        assert_eq!(since(&achieved), minute(20));
        achieved.push(goal(
            6,
            4,
            "goal_closed",
            json!({"verdict": "achieved"}),
            50,
        ));
        assert_eq!(since(&achieved), minute(50));
        // Added after it was achieved: nothing moves.
        let late = [
            created(1, "ready", None, 0),
            goal(2, 4, "goal_closed", json!({"verdict": "achieved"}), 10),
            own(3, "goal_dependency_added", json!({"goal_id": 4}), 20),
            own(4, "goal_dependency_removed", json!({"goal_id": 4}), 30),
        ];
        assert_eq!(since(&late), minute(0));
    }

    #[test]
    fn a_draft_goal_holds_its_tasks_until_it_opens_or_they_move_out() {
        let draft = goal(
            1,
            3,
            "goal_created",
            json!({"goal": {"status": "draft"}}),
            0,
        );
        let open_goal = goal(2, 5, "goal_created", json!({"goal": {"status": "open"}}), 0);
        let base = [
            draft.clone(),
            open_goal.clone(),
            created(3, "draft", Some(3), 0),
            status(4, T, "ready", 1),
        ];
        assert_eq!(since(&base), None);
        let mut opened = base.to_vec();
        opened.push(goal(
            5,
            3,
            "goal_status_changed",
            json!({"from": "draft", "to": "open"}),
            20,
        ));
        assert_eq!(since(&opened), minute(20));
        // Moved on to another open goal later: the opening stands.
        opened.push(own(6, "task_goal_changed", json!({"from": 3, "to": 5}), 30));
        assert_eq!(since(&opened), minute(20));
        // Moved out of the draft goal: claimable from the move.
        let mut moved = base.to_vec();
        moved.push(own(5, "task_goal_changed", json!({"from": 3, "to": 5}), 25));
        assert_eq!(since(&moved), minute(25));
        // A candidate moved into a goal opened before: nothing moves.
        let into_opened = [
            draft,
            open_goal,
            created(3, "ready", None, 0),
            goal(
                4,
                3,
                "goal_status_changed",
                json!({"from": "draft", "to": "open"}),
                10,
            ),
            own(5, "task_goal_changed", json!({"from": null, "to": 3}), 20),
        ];
        assert_eq!(since(&into_opened), minute(0));
    }

    /// Every attribute takes the value of its field of `run_claimed`, as
    /// text; a run whose provider the claim moved keeps the provider asked
    /// for and the one claimed apart; absent and null values have no key.
    #[test]
    fn the_attributes_are_the_claims_fields_by_their_keys() {
        let claim = ClaimAttributes {
            dagq_version: "0.9.0+abc".to_owned(),
            host: HostVersions {
                claude_version: Some("2.0.1".to_owned()),
                codex_version: Some("0.46.0".to_owned()),
                rustc_release: Some("1.90.0".to_owned()),
                rustc_host: None,
            },
            parallel: 4,
            slots: 2,
            load_avg: Some(3.5),
            spacing: None,
            light_room: None,
            instructions: BTreeMap::from([("codex".to_owned(), InstructionVersions::unknown())]),
            candidates: Some(5),
            by_task: BTreeMap::new(),
        };
        let Value::Object(mut payload) = serde_json::to_value(&claim).unwrap() else {
            unreachable!()
        };
        instructions::settle(&mut payload, "codex");
        payload.extend(
            json!({
                "provider": "codex",
                "requested_provider": "claude",
                "worker_mode": "headless",
                "model": null,
                "effort": "high",
                "group": null,
                "priority": "urgent",
                "priority_source": "goal",
            })
            .as_object()
            .unwrap()
            .clone(),
        );
        assert_eq!(
            Value::Object(attributes(&payload)),
            json!({
                "build": "0.9.0+abc",
                "claude_version": "2.0.1",
                "codex": "0.46.0",
                "rustc_release": "1.90.0",
                "parallel": "4",
                "slots": "2",
                "load_avg": "3.5",
                "requested_provider": "claude",
                "claim_provider": "codex",
                "route": "headless",
                "effort": "high",
                "instructions_prompt": instructions::UNKNOWN,
                "instructions_plugin": instructions::UNKNOWN,
                "instructions_repo": instructions::UNKNOWN,
                "priority": "urgent",
                "priority_source": "goal",
            })
        );
        // Without a requested provider of its own, the provider stands for it.
        let mut direct = Map::new();
        direct.insert("provider".to_owned(), json!("claude"));
        direct.insert("model".to_owned(), json!("opus"));
        direct.insert("group".to_owned(), json!("b"));
        assert_eq!(
            Value::Object(attributes(&direct)),
            json!({
                "requested_provider": "claude",
                "claim_provider": "claude",
                "claim_model": "opus",
                "trial_group": "b",
            })
        );
    }

    /// No attribute takes a reserved name, whatever the fields hold.
    #[test]
    fn no_attribute_takes_a_reserved_name() {
        assert!(KEYS.iter().all(|(key, _)| !RESERVED.contains(key)));
        let payload: Map<String, Value> = RESERVED
            .iter()
            .map(|name| ((*name).to_owned(), json!("x")))
            .collect();
        let set = attributes(&payload);
        assert!(set.keys().all(|key| !RESERVED.contains(&key.as_str())));
    }
}
