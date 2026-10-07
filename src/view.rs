//! Compact views of `show` and `goal show` for the sessions (ADR-0016):
//! a size bounded by the number of runs or tasks, not by their history.
//! Keys keep the names of the full output; a view only omits keys or
//! truncates strings. An object with a truncated string carries
//! `truncated: true`, and `--full` prints the stored records unchanged.

use serde_json::{Map, Value, json};

use crate::domain::{
    GoalDetail, OBSERVATION_KIND, RunEvent, TaskDetail,
    background_wrapper::current_background_session, broker_usage, claim_defer, reason,
};

/// The one key of a command's value that is printed as is instead of as
/// JSON: `graph --format d2|svg` without `--out`, and `run log` /
/// `planner log`, which print a session's log themselves and leave it
/// empty.
pub const RAW_STDOUT: &str = "__dagq_raw_stdout";
/// Characters a long text field keeps before `…`.
pub const TEXT_LIMIT: usize = 300;
/// Latest events a compact view keeps unless told otherwise.
pub const DEFAULT_EVENTS: usize = 10;
/// Latest asks `show` lists without `--full`.
pub const DEFAULT_ASKS: usize = 10;
/// Latest notes (`observation` events) a compact view lists in full.
pub const DEFAULT_OBSERVATIONS: usize = 5;
/// Payload keys a compact event keeps: what happened, not where. `code`
/// is the reason code (ADR-0034).
const EVENT_GIST: [&str; 6] = ["status", "reason", "last_error", "from", "to", "code"];

pub use crate::application::health::truncate;

/// Truncate the string fields `keys` of `object` in place and mark it
/// `truncated` when any of them was cut.
fn truncate_fields(object: &mut Map<String, Value>, keys: &[&str]) {
    let mut truncated = false;
    for key in keys {
        if let Some(Value::String(text)) = object.get_mut(*key)
            && let Some(cut) = truncate(text, TEXT_LIMIT)
        {
            *text = cut;
            truncated = true;
        }
    }
    if truncated {
        object.insert("truncated".into(), Value::Bool(true));
    }
}

fn object(value: impl serde::Serialize) -> Map<String, Value> {
    match serde_json::to_value(value) {
        Ok(Value::Object(map)) => map,
        _ => Map::new(),
    }
}

fn pick(source: &Map<String, Value>, keys: &[&str]) -> Map<String, Value> {
    keys.iter()
        .filter_map(|key| source.get(*key).map(|v| ((*key).to_owned(), v.clone())))
        .collect()
}

fn latest<T>(items: &[T], count: usize) -> &[T] {
    &items[items.len().saturating_sub(count)..]
}

/// `show` without `--full`: the task with long texts truncated, the latest
/// run's identity and outcome, its processes, the latest `events`
/// events with their run and the gist of their payload (no paths), and the
/// latest [`DEFAULT_ASKS`] asks about the task or its runs.
pub fn task_detail(detail: &TaskDetail, events: usize) -> Value {
    let mut task = object(&detail.task);
    truncate_fields(&mut task, &["description", "acceptance", "context"]);
    let latest_run = detail.runs.last();
    let runs: Vec<Value> = latest_run
        .map(|latest| {
            let mut run = pick(
                &object(latest),
                &[
                    "id",
                    "status",
                    "branch",
                    "result_commit",
                    "last_error",
                    "worktree_path",
                    "workspace_id",
                ],
            );
            truncate_fields(&mut run, &["last_error"]);
            let events = run_events(detail, latest);
            if let Some(code) = reason::run_error_code(latest, &events) {
                run.insert("last_error_code".into(), json!(code));
            }
            // A session in the background: its wrapper's pid and its log
            // (`run log`, ADR-t1404-1 decision 6).
            if let Some(session) = current_background_session(&events) {
                run.insert("background".into(), json!(session));
            }
            // Its calls through the resource broker and around it, when
            // its end counted them (`broker_tool_use`).
            if let Some(usage) = broker_usage::latest_tool_use(&events, latest.id()) {
                run.insert("broker_tool_use".into(), usage);
            }
            Value::Object(run)
        })
        .into_iter()
        .collect();
    let processes: Vec<_> = detail
        .processes
        .iter()
        .filter(|p| latest_run.is_some_and(|run| *run.id() == p.run_id))
        .collect();
    let events: Vec<Value> = latest(&detail.events, events)
        .iter()
        .map(event_gist)
        .collect();
    // The claim deferred now, whatever the reason (a hotspot, the worker,
    // the build, ADR-t1632-1): from the task's latest deferral event.
    let deferral = detail
        .events
        .iter()
        .rev()
        .find(|event| claim_defer::DEFERRAL_KINDS.contains(&event.kind.as_str()))
        .and_then(claim_defer::OpenDeferral::of);
    let mut shown = json!({
        "task": task,
        "dependencies": detail.dependencies,
        "goal_dependencies": detail.goal_dependencies,
        "duplicate_of": detail.duplicate_of,
        "duplicates": detail.duplicates,
        "origin": detail.origin,
        "follow_up_drafts": detail.follow_up_drafts,
        "revisit": detail.revisit,
        "membership_judgements": detail.membership_judgements,
        "runs": runs,
        "runs_total": detail.runs.len(),
        "events": events,
        "events_total": detail.events.len(),
        "observations": observations(&detail.events),
        "processes": processes,
        "asks": asks(detail),
        "asks_total": detail.asks.len(),
    });
    if let Some(deferral) = deferral {
        shown["claim_deferral"] = json!(deferral);
    }
    shown
}

/// The latest [`DEFAULT_ASKS`] asks of the task, oldest first, with the
/// keys of `asks` (`recommendation` and `confidence` null without them,
/// `closed_at` null while one is open) and their question and answer cut
/// to [`TEXT_LIMIT`].
fn asks(detail: &TaskDetail) -> Vec<Value> {
    latest(&detail.asks, DEFAULT_ASKS)
        .iter()
        .map(|ask| {
            let mut ask = object(ask);
            truncate_fields(&mut ask, &["question", "answer"]);
            Value::Object(ask)
        })
        .collect()
}

/// The events of `run` among the task's.
fn run_events(detail: &TaskDetail, run: &crate::domain::TaskRun) -> Vec<RunEvent> {
    detail
        .events
        .iter()
        .filter(|event| event.run_id.as_ref() == Some(run.id()))
        .cloned()
        .collect()
}

/// The latest [`DEFAULT_OBSERVATIONS`] notes among `events`, oldest first,
/// with their text cut to [`TEXT_LIMIT`].
fn observations(events: &[RunEvent]) -> Vec<Value> {
    let notes: Vec<&RunEvent> = events
        .iter()
        .filter(|event| event.kind == OBSERVATION_KIND)
        .collect();
    latest(&notes, DEFAULT_OBSERVATIONS)
        .iter()
        .map(|event| {
            let mut note = pick(&object(event), &["id", "created_at"]);
            if let Some(run_id) = &event.run_id {
                note.insert("run_id".into(), json!(run_id));
            }
            if let Value::Object(payload) = &event.payload {
                note.extend(pick(payload, &["text", "kind", "by"]));
            }
            truncate_fields(&mut note, &["text"]);
            Value::Object(note)
        })
        .collect()
}

fn event_gist(event: &RunEvent) -> Value {
    let mut compact = pick(&object(event), &["id", "kind", "created_at"]);
    if let Some(run_id) = &event.run_id {
        compact.insert("run_id".into(), json!(run_id));
    }
    // Who wrote it (ADR-t728-1 decision 4); none on an older row.
    if let Some(actor) = &event.actor {
        compact.insert("actor".into(), json!(actor));
    }
    if let Value::Object(payload) = &event.payload {
        let mut gist = pick(payload, &EVENT_GIST);
        if !gist.is_empty() {
            truncate_fields(&mut gist, &EVENT_GIST);
            // `task_edited` keeps the old and new texts of a task under
            // `from` / `to`; they are cut like the task's own texts.
            for key in ["from", "to"] {
                if let Some(Value::Object(fields)) = gist.get_mut(key) {
                    let keys: Vec<String> = fields.keys().cloned().collect();
                    let keys: Vec<&str> = keys.iter().map(String::as_str).collect();
                    truncate_fields(fields, &keys);
                }
            }
            compact.insert("payload".into(), Value::Object(gist));
        }
    }
    Value::Object(compact)
}

/// `goal show` without `--full`: the goal with long texts truncated, every
/// task's id, status and title, the unfinished tasks waiting for the goal
/// (`dependents`), and the kind and time of the latest events.
pub fn goal_detail(detail: &GoalDetail) -> Value {
    let mut goal = object(&detail.goal);
    truncate_fields(
        &mut goal,
        &["title", "description", "acceptance", "constraints"],
    );
    let events: Vec<Value> = latest(&detail.events, DEFAULT_EVENTS)
        .iter()
        .map(|event| Value::Object(pick(&object(event), &["kind", "created_at"])))
        .collect();
    json!({
        "goal": goal,
        "closed": detail.closed,
        "acceptance_version": detail.acceptance_version,
        "follow_up_memberships": detail.follow_up_memberships,
        "tasks": detail.tasks,
        "dependents": detail.dependents,
        "events": events,
        "events_total": detail.events.len(),
        "observations": observations(&detail.events),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{
        CommitSha, EventId, Goal, GoalId, GoalTask, Provider, RunId, RunProcess, RunStatus, Task,
        TaskId, TaskRun, TaskStatus,
    };

    fn task(description: &str) -> Task {
        Task::restore(crate::domain::TaskRecord {
            goal_priority: None,
            id: TaskId::new(1),
            title: "t".into(),
            description: description.into(),
            acceptance: "short".into(),
            verification_commands: vec!["true".into()],
            required_evidence: Vec::new(),
            paths: Vec::new(),
            priority: Default::default(),
            change: None,
            status: TaskStatus::InProgress,
            goal_id: None,
            context: String::new(),
            created_at: "c".into(),
            updated_at: "u".into(),
            worker: crate::domain::worker::Worker::CLAUDE_INTERACTIVE,
            named_mode: None,
            wait_for_build: false,
        })
        .unwrap()
    }

    fn run(id: &str) -> TaskRun {
        TaskRun::restore(crate::domain::RunRecord {
            id: RunId::new(id).unwrap(),
            task_id: TaskId::new(1),
            status: RunStatus::Failed,
            requested_provider: Provider::Claude,
            actual_provider: Provider::Claude,
            worker_mode: crate::domain::worker::WorkerMode::Interactive,
            base_commit: CommitSha::try_from("b".repeat(40)).unwrap(),
            branch: Some(format!("dagq/{id}")),
            worktree_path: Some("/w".into()),
            workspace_id: Some("ws".into()),
            receipt_path: Some("/r".into()),
            log_path: Some("/l".into()),
            result_commit: None,
            repo_path: Some("/repo".into()),
            run_dir: Some("/run".into()),
            last_error: Some("e".repeat(TEXT_LIMIT + 1)),
            workspace_closed_at: None,
            created_at: "c".into(),
        })
        .unwrap()
    }

    fn event(id: i64, payload: Value) -> RunEvent {
        RunEvent {
            id: EventId::new(id),
            task_id: Some(TaskId::new(1)),
            goal_id: None,
            run_id: Some(RunId::new("b").unwrap()),
            kind: format!("kind{id}"),
            payload,
            created_at: format!("t{id}"),
            actor: None,
        }
    }

    fn process(run_id: &str) -> RunProcess {
        RunProcess {
            run_id: RunId::new(run_id).unwrap(),
            role: "wrapper".into(),
            pid: 1,
            heartbeat_at: 0,
            exited_at: None,
            exit_code: None,
        }
    }

    #[test]
    fn truncate_counts_characters_and_marks_the_cut() {
        assert_eq!(truncate("abc", 3), None);
        assert_eq!(truncate("abcd", 3).as_deref(), Some("abc…"));
        assert_eq!(truncate("あいうえ", 2).as_deref(), Some("あい…"));
    }

    #[test]
    fn task_detail_keeps_the_latest_run_and_events_without_paths() {
        let detail = TaskDetail {
            membership_judgements: Vec::new(),
            task: task(&"d".repeat(TEXT_LIMIT + 5)),
            dependencies: vec![TaskId::new(3)],
            goal_dependencies: vec![GoalId::new(4)],
            duplicate_of: None,
            duplicates: Vec::new(),
            origin: None,
            follow_up_drafts: Vec::new(),
            revisit: None,
            asks: Vec::new(),
            runs: vec![run("a"), run("b")],
            events: (1..=12)
                .map(|id| {
                    event(
                        id,
                        json!({"path": "/run/receipt.json", "status": "failed", "reason": "x"}),
                    )
                })
                .chain([RunEvent {
                    run_id: None,
                    actor: Some(crate::domain::EventActor {
                        role: "supervisor".into(),
                        id: "supervisor:9".into(),
                        requested_by: Some("review-job:b:1".into()),
                    }),
                    ..event(13, json!({"path": "/p"}))
                }])
                .collect(),
            processes: vec![process("a"), process("b")],
        };
        let view = task_detail(&detail, 3);
        let description = view["task"]["description"].as_str().unwrap();
        assert!(description.ends_with('…'));
        assert_eq!(description.chars().count(), TEXT_LIMIT + 1);
        assert_eq!(view["task"]["truncated"], true);
        assert_eq!(view["task"]["acceptance"], "short");
        assert_eq!(view["task"]["verification_commands"], json!(["true"]));
        assert_eq!(view["runs_total"], 2);
        assert_eq!(view["goal_dependencies"], json!([4]));
        let runs = view["runs"].as_array().unwrap();
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0]["id"], "b");
        assert_eq!(runs[0]["truncated"], true);
        assert!(runs[0].get("run_dir").is_none());
        assert!(runs[0]["result_commit"].is_null());
        assert!(runs[0].get("broker_tool_use").is_none());
        assert_eq!(view["processes"].as_array().unwrap().len(), 1);
        assert_eq!(view["processes"][0]["run_id"], "b");
        assert_eq!(view["events_total"], 13);
        let events = view["events"].as_array().unwrap();
        assert_eq!(
            events.iter().map(|e| e["id"].clone()).collect::<Vec<_>>(),
            vec![json!(11), json!(12), json!(13)]
        );
        assert_eq!(
            events[0],
            json!({"id": 11, "kind": "kind11", "created_at": "t11", "run_id": "b",
                   "payload": {"status": "failed", "reason": "x"}})
        );
        assert_eq!(
            events[2],
            json!({"id": 13, "kind": "kind13", "created_at": "t13",
                   "actor": {"role": "supervisor", "id": "supervisor:9",
                             "requested_by": "review-job:b:1"}})
        );
    }

    /// The latest run carries its latest `broker_tool_use` whole: the
    /// brokered and the direct counts side by side.
    #[test]
    fn task_detail_shows_the_latest_runs_brokered_and_direct_counts() {
        let usage = |direct: u64| {
            json!({"brokered": 2, "brokered_by_op": {"fs.read": 2},
                   "direct": direct, "direct_by_tool": {"Bash": direct}})
        };
        let tool_use = |id: i64, run_id: &str, direct: u64| RunEvent {
            run_id: Some(RunId::new(run_id).unwrap()),
            kind: crate::domain::event_kind::BROKER_TOOL_USE.into(),
            ..event(id, usage(direct))
        };
        let detail = TaskDetail {
            membership_judgements: Vec::new(),
            task: task("short"),
            dependencies: vec![],
            goal_dependencies: vec![],
            duplicate_of: None,
            duplicates: Vec::new(),
            origin: None,
            follow_up_drafts: Vec::new(),
            revisit: None,
            asks: Vec::new(),
            runs: vec![run("a"), run("b")],
            events: vec![
                tool_use(1, "b", 1),
                tool_use(2, "b", 3),
                tool_use(3, "a", 9),
            ],
            processes: vec![],
        };
        let view = task_detail(&detail, 10);
        assert_eq!(view["runs"][0]["id"], "b");
        assert_eq!(view["runs"][0]["broker_tool_use"], usage(3));
    }

    #[test]
    fn task_detail_without_runs_is_empty() {
        let detail = TaskDetail {
            membership_judgements: Vec::new(),
            task: task("short"),
            dependencies: vec![],
            goal_dependencies: vec![],
            duplicate_of: None,
            duplicates: Vec::new(),
            origin: None,
            follow_up_drafts: Vec::new(),
            revisit: None,
            asks: Vec::new(),
            runs: vec![],
            events: vec![],
            processes: vec![],
        };
        let view = task_detail(&detail, DEFAULT_EVENTS);
        assert!(view["task"].get("truncated").is_none());
        assert_eq!(view["runs"], json!([]));
        assert_eq!(view["events"], json!([]));
        assert_eq!(view["asks"], json!([]));
        assert_eq!(view["asks_total"], 0);
    }

    fn ask(id: i64, run: Option<&str>, question: &str) -> crate::domain::Ask {
        crate::domain::Ask {
            id: crate::domain::AskId::new(id),
            kind: crate::domain::AskKind::Decide,
            task_id: run.is_none().then(|| TaskId::new(1)),
            run_id: run.map(|run| RunId::new(run).unwrap()),
            question: question.into(),
            options: vec!["retry".into(), "cancel".into()],
            answer: None,
            asked_by: "supervisor".into(),
            reason_category: crate::domain::AskReason::Scope,
            topics: Vec::new(),
            recommendation: None,
            confidence: None,
            subject: None,
            affected: Vec::new(),
            created_at: id,
            answered_at: None,
            closed_at: None,
            finding_id: None,
            request_id: None,
            answered_by: None,
            option_index: None,
            answer_authority: None,
            answer_approval: None,
        }
    }

    /// `show` lists the asks of the task and its runs (ADR-t451-1 decision
    /// 1): the latest [`DEFAULT_ASKS`] oldest first, with the keys of
    /// `asks`, a null recommendation and confidence without them, and the
    /// question and answer cut.
    #[test]
    fn task_detail_lists_the_latest_asks_with_their_recommendation() {
        let mut asks: Vec<crate::domain::Ask> = (1..=DEFAULT_ASKS as i64 + 2)
            .map(|id| ask(id, None, "which?"))
            .collect();
        let recommended = asks.last_mut().unwrap();
        recommended.run_id = Some(RunId::new("b").unwrap());
        recommended.task_id = None;
        recommended.recommendation = Some("retry".into());
        recommended.confidence = Some(crate::domain::AskConfidence::Low);
        recommended.question = "q".repeat(TEXT_LIMIT + 1);
        recommended.answer = Some("a".repeat(TEXT_LIMIT + 1));
        recommended.answered_at = Some(20);
        recommended.closed_at = Some(21);
        let detail = TaskDetail {
            membership_judgements: Vec::new(),
            task: task("short"),
            dependencies: vec![],
            goal_dependencies: vec![],
            duplicate_of: None,
            duplicates: Vec::new(),
            origin: None,
            follow_up_drafts: Vec::new(),
            revisit: None,
            runs: vec![run("b")],
            events: vec![],
            processes: vec![],
            asks,
        };
        let view = task_detail(&detail, DEFAULT_EVENTS);
        assert_eq!(view["asks_total"], DEFAULT_ASKS + 2);
        let listed = view["asks"].as_array().unwrap();
        assert_eq!(listed.len(), DEFAULT_ASKS);
        assert_eq!(listed[0]["id"], 3);
        assert_eq!(listed[0]["task_id"], 1);
        assert_eq!(listed[0]["kind"], "decide");
        assert_eq!(listed[0]["question"], "which?");
        assert_eq!(listed[0]["options"], json!(["retry", "cancel"]));
        assert_eq!(listed[0]["reason_category"], "scope");
        assert_eq!(listed[0]["recommendation"], Value::Null);
        assert_eq!(listed[0]["confidence"], Value::Null);
        assert_eq!(listed[0]["closed_at"], Value::Null);
        assert!(listed[0].get("truncated").is_none());
        let last = &listed[DEFAULT_ASKS - 1];
        assert_eq!(last["id"], DEFAULT_ASKS + 2);
        assert_eq!(last["run_id"], "b");
        assert_eq!(last["task_id"], Value::Null);
        assert_eq!(last["recommendation"], "retry");
        assert_eq!(last["confidence"], "low");
        assert_eq!(last["closed_at"], 21);
        assert_eq!(last["truncated"], true);
        for key in ["question", "answer"] {
            let text = last[key].as_str().unwrap();
            assert!(text.ends_with('…'), "{key}");
            assert_eq!(text.chars().count(), TEXT_LIMIT + 1, "{key}");
        }
        // `--full` prints the stored records: every ask, whole.
        let full = serde_json::to_value(&detail).unwrap();
        assert_eq!(full["asks"].as_array().unwrap().len(), DEFAULT_ASKS + 2);
        assert_eq!(
            full["asks"][DEFAULT_ASKS + 1]["question"]
                .as_str()
                .unwrap()
                .chars()
                .count(),
            TEXT_LIMIT + 1
        );
    }

    #[test]
    fn goal_detail_truncates_texts_and_keeps_kinds_of_latest_events() {
        let detail = GoalDetail {
            acceptance_version: 1,
            follow_up_memberships: Vec::new(),
            goal: Goal::restore(crate::domain::GoalRecord {
                priority: Default::default(),
                tags: Vec::new(),
                id: GoalId::new(1),
                title: "g".into(),
                description: "x".repeat(TEXT_LIMIT * 2),
                acceptance: "a".into(),
                constraints: "c".repeat(TEXT_LIMIT + 1),
                doc: None,
                status: crate::domain::GoalStatus::Draft,
                closed_at: None,
                verdict: None,
                created_at: "c".into(),
                updated_at: "u".into(),
            })
            .unwrap(),
            closed: false,
            tasks: vec![GoalTask {
                id: TaskId::new(2),
                title: "t".into(),
                status: TaskStatus::Ready,
                priority: crate::domain::Priority::High,
                priority_source: crate::domain::PrioritySource::Goal,
                priority_by: crate::domain::plan_request::PriorityBy::Human,
            }],
            dependents: vec![GoalTask {
                id: TaskId::new(5),
                title: "waits".into(),
                status: TaskStatus::Draft,
                priority: crate::domain::Priority::Low,
                priority_source: crate::domain::PrioritySource::Task,
                priority_by: crate::domain::plan_request::PriorityBy::Ai,
            }],
            events: (1..=11).map(|id| event(id, json!({"goal": {}}))).collect(),
        };
        let view = goal_detail(&detail);
        assert_eq!(view["goal"]["truncated"], true);
        assert!(view["goal"]["constraints"].as_str().unwrap().ends_with('…'));
        assert_eq!(view["goal"]["acceptance"], "a");
        assert_eq!(
            view["tasks"],
            json!([{"id": 2, "title": "t", "status": "ready", "priority": "high",
                "priority_source": "goal", "priority_by": "human"}])
        );
        assert_eq!(
            view["dependents"],
            json!([{"id": 5, "title": "waits", "status": "draft", "priority": "low",
                "priority_source": "task", "priority_by": "ai"}])
        );
        assert_eq!(view["events_total"], 11);
        let events = view["events"].as_array().unwrap();
        assert_eq!(events.len(), DEFAULT_EVENTS);
        assert_eq!(events[0], json!({"kind": "kind2", "created_at": "t2"}));
    }

    #[test]
    fn views_list_the_latest_notes_with_their_text() {
        let note = |id: i64, text: &str| RunEvent {
            kind: OBSERVATION_KIND.into(),
            ..event(id, json!({"text": text, "kind": "stall", "by": "observer"}))
        };
        let mut events: Vec<RunEvent> = (1..=7).map(|id| note(id, "slow")).collect();
        events.push(event(8, json!({})));
        events.push(RunEvent {
            run_id: None,
            ..note(9, &"n".repeat(TEXT_LIMIT + 1))
        });
        let detail = TaskDetail {
            membership_judgements: Vec::new(),
            task: task("short"),
            dependencies: vec![],
            goal_dependencies: vec![],
            duplicate_of: None,
            duplicates: Vec::new(),
            origin: None,
            follow_up_drafts: Vec::new(),
            revisit: None,
            asks: Vec::new(),
            runs: vec![],
            events,
            processes: vec![],
        };
        let view = task_detail(&detail, 1);
        let notes = view["observations"].as_array().unwrap();
        assert_eq!(notes.len(), DEFAULT_OBSERVATIONS);
        assert_eq!(
            notes[0],
            json!({"id": 4, "created_at": "t4", "run_id": "b",
                   "text": "slow", "kind": "stall", "by": "observer"})
        );
        assert_eq!(notes[4]["truncated"], true);
        assert!(notes[4].get("run_id").is_none());
        assert!(notes[4]["text"].as_str().unwrap().ends_with('…'));
    }
}
