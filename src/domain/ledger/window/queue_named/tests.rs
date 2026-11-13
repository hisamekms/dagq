//! What a read from a time names of the queue rows read by the views, and
//! which of the events a store seeks by those names it keeps.

use std::collections::BTreeSet;

use serde_json::json;

use super::*;
use crate::domain::ledger::Subject;
use crate::domain::marks::utc_text;
use crate::domain::{EventId, GoalId, RunId, TaskId};

const T0_MS: i64 = 1_900_000_000_000;

fn event(id: i64, kind: &str, payload: serde_json::Value, ms: i64) -> RunEvent {
    RunEvent {
        id: EventId::new(id),
        task_id: None,
        goal_id: None,
        run_id: None,
        kind: kind.into(),
        payload,
        created_at: utc_text(T0_MS + ms),
        actor: None,
    }
}

fn of_goal(mut event: RunEvent, goal: i64) -> RunEvent {
    event.goal_id = Some(GoalId::new(goal));
    event
}

fn row(kind: LedgerKind, records: Vec<RunEvent>) -> LedgerRow {
    let mut row = LedgerRow::new(kind, "k".to_owned(), None, "");
    row.subject = Some(Subject::default());
    row.records = records;
    row
}

#[test]
fn the_rows_name_their_goals_landed_commits_reverts_and_heads() {
    let mut names = QueueNames::default();
    let mut task = event(1, "task_created", json!({"goal_id": 7}), 0);
    task.task_id = Some(TaskId::new(3));
    let mut moved = event(2, "task_goal_changed", json!({"from": 8, "to": 9}), 0);
    moved.task_id = Some(TaskId::new(3));
    names.note_row(&row(LedgerKind::Task, vec![task, moved]));
    let mut landed = event(3, "run_integrated", json!({"result_commit": "c1"}), 0);
    landed.run_id = Some(RunId::new("00000001-1111-4111-8111-111111111111").unwrap());
    names.note_row(&row(LedgerKind::Run, vec![landed]));
    names.note_queue_row(&row(
        LedgerKind::Queue,
        vec![
            event(
                4,
                "main_commits_recorded",
                json!({"commits": [{"sha": "r1", "reverts": "c0"}, {"sha": "x", "reverts": null}]}),
                0,
            ),
            event(5, "main_paths_recorded", json!({"head": "h1"}), 0),
            event(
                8,
                "main_reverts_imported",
                json!({"reverts": [{"sha": "r2", "reverts": "human"}]}),
                0,
            ),
            of_goal(event(6, "goal_status_changed", json!({}), 0), 11),
        ],
    ));
    names.note_goal(12);
    // A node row and a queue row taken as a run's name nothing.
    names.note_row(&row(
        LedgerKind::Queue,
        vec![of_goal(event(7, "goal_updated", json!({}), 0), 99)],
    ));
    let wants = names.wants(5, &BTreeSet::from([40]), None);
    assert_eq!(wants.goals, BTreeSet::from([7, 8, 9, 11, 12]));
    assert_eq!(
        wants.shas,
        BTreeSet::from(["c0".to_owned(), "c1".to_owned(), "human".to_owned()])
    );
    assert_eq!(wants.landed, BTreeSet::from(["c1".to_owned()]));
    assert_eq!(wants.proposals, BTreeSet::from([40]));
    assert!(wants.marks && !wants.is_empty());
    // Asked once: nothing is left, and a new name is all that is.
    assert!(wants.less(&wants).is_empty());
    names.note_goal(13);
    let more = names.wants(5, &BTreeSet::from([40]), None).less(&wants);
    assert_eq!(more.goals, BTreeSet::from([13]));
    assert!(more.shas.is_empty() && !more.marks);
    let mut asked = wants.clone();
    asked.extend(&more);
    assert!(
        names
            .wants(5, &BTreeSet::from([40]), None)
            .less(&asked)
            .is_empty()
    );
}

#[test]
fn a_summary_adds_the_commits_past_a_head_and_of_a_window_s_times() {
    use crate::domain::ledger::queue_views::{MainRecord, MainSummary};
    let summary = MainSummary {
        through: Some(EventId::new(9)),
        commits: vec!["a".into(), "h1".into(), "b".into(), "c".into()],
        record: Some(MainRecord {
            commits_at: vec![1, 2, 3, 2_000],
            ..MainRecord::default()
        }),
        ..MainSummary::default()
    };
    let mut names = QueueNames::default();
    names.note_queue_record(&event(10, "main_paths_recorded", json!({"head": "h1"}), 0));
    let wants = names.wants(1_999_500, &BTreeSet::new(), Some(&summary));
    assert_eq!(wants.shas, BTreeSet::from(["b".into(), "c".into()]));
}

#[test]
fn a_goal_s_walk_stops_at_what_replaces_it_before_the_time() {
    let goal =
        |kind: &str, payload: serde_json::Value, ms: i64| of_goal(event(1, kind, payload, ms), 5);
    let record = json!({"title": "g", "status": "open", "priority": "high"});
    let from = T0_MS + 100;
    let updated = goal("goal_updated", json!({"new": record}), 50);
    assert!(goal_record(&updated));
    assert!(stops_goal_walk(&updated, None, from));
    // After the time, or of no new values: not.
    assert!(!stops_goal_walk(
        &goal("goal_updated", json!({"new": record}), 150),
        None,
        from
    ));
    assert!(!stops_goal_walk(
        &goal("goal_updated", json!({}), 50),
        None,
        from
    ));
    assert!(!stops_goal_walk(
        &goal("goal_status_changed", json!({"to": "draft"}), 50),
        None,
        from
    ));
    assert!(stops_goal_walk(
        &goal("goal_created", json!({"goal": record}), 50),
        None,
        from
    ));
    // A baseline by its group's mark.
    let baseline = goal("attributes_baseline", json!({"goal": record}), 50);
    let mark = |ms: i64| event(2, "attributes_baseline", json!({}), ms);
    assert!(is_mark(EventKind::AttributesBaseline, &mark(0)));
    assert!(!is_mark(EventKind::AttributesBaseline, &baseline));
    assert!(stops_goal_walk(&baseline, Some(&mark(60)), from));
    assert!(!stops_goal_walk(&baseline, Some(&mark(160)), from));
    assert!(!stops_goal_walk(&baseline, None, from));
    // A task's record of the goal is its row's.
    let mut task = goal("goal_updated", json!({"new": record}), 50);
    task.task_id = Some(TaskId::new(1));
    assert!(!goal_record(&task));
}

#[test]
fn the_seeks_keep_what_links_or_records_what_is_named() {
    let proposals = BTreeSet::from([4]);
    let linked = event(
        1,
        "finding_status_changed",
        json!({"finding_id": 9, "to": "proposed", "proposal_id": 4}),
        0,
    );
    assert_eq!(linked_finding(&linked, &proposals), Some(9));
    let other = event(
        1,
        "finding_status_changed",
        json!({"finding_id": 9, "to": "open", "proposal_id": 4}),
        0,
    );
    assert_eq!(linked_finding(&other, &proposals), None);
    assert_eq!(linked_finding(&linked, &BTreeSet::from([5])), None);
    let submitted = of_goal(event(1, "goal_submitted", json!({"proposal_id": 6}), 0), 3);
    assert_eq!(submitted_proposals(&[submitted]), BTreeSet::from([6]));
    let commits = event(
        1,
        "main_commits_recorded",
        json!({"commits": [{"sha": "a"}, {"sha": "r", "reverts": "l"}]}),
        0,
    );
    let none = BTreeSet::new();
    assert!(records_commit(
        &commits,
        &BTreeSet::from(["a".to_owned()]),
        &none
    ));
    assert!(records_commit(
        &commits,
        &none,
        &BTreeSet::from(["l".to_owned()])
    ));
    assert!(!records_commit(
        &commits,
        &BTreeSet::from(["l".to_owned()]),
        &none
    ));
    assert!(before(&event(1, "x", json!({}), 5), T0_MS + 6));
    assert!(!before(&event(1, "x", json!({}), 6), T0_MS + 6));
}

#[test]
fn a_task_s_baseline_names_the_goals_it_waits_for_and_a_snapshot_its_goals_closes() {
    let mut names = QueueNames::default();
    let mut baseline = event(
        1,
        "attributes_baseline",
        json!({"goal_id": null, "goal_dependencies": [21, 22]}),
        0,
    );
    baseline.task_id = Some(TaskId::new(3));
    names.note_row(&row(LedgerKind::Task, vec![baseline]));
    names.note_snapshot(&event(
        8,
        "forecast_recorded",
        json!({"goals": [{"id": 31}, {"id": 32}], "tasks": [{"id": 3}]}),
        0,
    ));
    // Another kind's goals are not a snapshot's.
    names.note_snapshot(&event(9, "goal_updated", json!({"goals": [{"id": 33}]}), 0));
    let wants = names.wants(5, &BTreeSet::new(), None);
    assert_eq!(wants.goals, BTreeSet::from([21, 22]));
    assert_eq!(wants.closes, BTreeSet::from([(31, 8), (32, 8)]));
    assert!(wants.less(&wants).closes.is_empty());
}
