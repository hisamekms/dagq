//! What `run_claimed` records of the claim beyond the run itself
//! (ADR-t1662-1 decisions 6 and 21 to 26, docs/design/measurement.md
//! "claimの前" and "runの比較の軸"): where the claimed task stood among the
//! candidates and the priority it was claimed at ([`CandidateFacts`], which
//! the supervisor passes for every candidate under [`BY_TASK`] and the
//! claim keeps the claimed task's of, [`settle`]), when the task became
//! claimable ([`ready_at`]), and the claim's attributes as an open set of
//! keys and values ([`attributes`]).

use std::collections::BTreeMap;

use serde::Serialize;
use serde_json::{Map, Value};

use super::{Priority, PrioritySource, TaskId, stats::timestamp_millis};

/// The key of the claim's attributes holding each candidate's
/// [`CandidateFacts`], by task ID, until the claim knows its task
/// ([`settle`]); `run_claimed` never keeps it.
pub const BY_TASK: &str = "candidates_by_task";

/// What the claim knew of one candidate when it lined them up: its place
/// in the line (`candidate_rank`, from 1) and its priority as the claim
/// order compared it (`effective_priority`), from its base (`priority`,
/// [`super::base_priority`]) and where that came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct CandidateFacts {
    pub candidate_rank: usize,
    pub priority: Priority,
    pub priority_source: PrioritySource,
    pub effective_priority: Priority,
}

/// The facts of each task of `order` (the candidates of a claim, in the
/// order it takes them), with their priorities from `priorities` (base,
/// source, effective); a task it has none of is left out.
pub fn candidate_facts(
    order: &[TaskId],
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

/// When a task became claimable: the latest of its own move to `ready`
/// (`own`), the completions of the tasks it depends on and the closes of
/// the goals it depends on (`prerequisites`). Times are the events'
/// `created_at`; one that cannot be read is skipped. The submit of a
/// draft goal and the end of the task's earlier run are not among them, so
/// a task that waited on one of those reads as ready earlier.
pub fn ready_at<'t>(
    own: Option<&'t str>,
    prerequisites: impl IntoIterator<Item = &'t str>,
) -> Option<&'t str> {
    own.into_iter()
        .chain(prerequisites)
        .filter_map(|text| Some((timestamp_millis(text)?, text)))
        .max_by_key(|(at, _)| *at)
        .map(|(_, text)| text)
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
        let facts = candidate_facts(&order, |task| {
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
            })
        );
        // A task claimed from outside the line gets no facts.
        settle(&mut attributes, TaskId::new(1));
        assert_eq!(Value::Object(attributes), json!({"candidates": 3}));
    }

    #[test]
    fn a_task_is_ready_at_the_latest_of_its_own_and_its_prerequisites() {
        let own = "2026-10-01T00:05:00.000Z";
        let done = "2026-10-01T00:09:00.000Z";
        assert_eq!(ready_at(Some(own), [done, "nonsense"]), Some(done));
        assert_eq!(ready_at(Some(done), [own]), Some(done));
        assert_eq!(ready_at(None, []), None);
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
