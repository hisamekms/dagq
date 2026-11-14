use super::*;
use crate::domain::ledger::testkit::run_row;
use crate::domain::{EventActor, EventId, GoalId, RunId, TaskId};
use serde_json::json;

fn event(id: i64, task: Option<i64>, run: Option<&str>, kind: &str, payload: Value) -> RunEvent {
    RunEvent {
        id: EventId::new(id),
        task_id: task.map(TaskId::new),
        goal_id: None,
        run_id: run.map(|run| RunId::new(run).unwrap()),
        kind: kind.into(),
        payload,
        created_at: crate::domain::marks::utc_text(1_000 * id),
        actor: None,
    }
}

const RUN: &str = "00000001-1111-4111-8111-111111111111";

fn named() -> Named {
    let mut row = run_row(RUN, Some(TaskId::new(5)), Vec::new());
    row.records = vec![
        event(1, Some(5), Some(RUN), "run_claimed", json!({})),
        event(
            2,
            None,
            None,
            "plan_review_finished",
            json!({"plan_review_id": 9}),
        ),
        event(
            3,
            None,
            None,
            "session_opened",
            json!({"planner_id": 4, "proposal_id": 8}),
        ),
    ];
    let mut named = Named::default();
    named.note_row(&row);
    named
}

#[test]
fn only_a_task_row_read_is_held_and_a_run_row_names_its_task_alone() {
    let mut named = named();
    assert!(named.names_task(5) && !named.holds_task(5));
    let mut row = LedgerRow::new(LedgerKind::Task, "6".into(), Some(TaskId::new(6)), "");
    row.records = vec![event(4, Some(6), None, "task_priority_changed", json!({}))];
    named.note_row(&row);
    assert!(named.holds_task(6) && !named.holds_task(5));
}

#[test]
fn the_rows_read_name_their_tasks_reviews_planners_and_proposals() {
    let named = named();
    assert_eq!(named.tasks(), &BTreeSet::from([5]));
    assert_eq!(named.reviews(), &BTreeSet::from([9]));
    assert_eq!(named.proposals(), &BTreeSet::from([8]));
    assert_eq!(
        named.session_key_ranges(),
        vec![
            (
                "runtime_planner/4/".to_owned(),
                "runtime_planner/40".to_owned()
            ),
            ("plan_review/9/".to_owned(), "plan_review/90".to_owned()),
        ]
    );
}

#[test]
fn the_queue_names_a_planner_by_an_ask_s_actor_of_the_time_on_and_links_proposals() {
    let mut ask = event(
        10,
        None,
        None,
        "ask_opened",
        json!({"kind": "planner_question"}),
    );
    ask.actor = Some(EventActor {
        role: "planner".into(),
        id: "planner:70".into(),
        requested_by: None,
    });
    let mut old = ask.clone();
    old.id = EventId::new(1);
    old.created_at = crate::domain::marks::utc_text(0);
    old.actor.as_mut().unwrap().id = "planner:71".into();
    let linked = event(
        11,
        None,
        None,
        "finding_status_changed",
        json!({"to": "proposed", "proposal_id": 12}),
    );
    let forecast = event(
        12,
        None,
        None,
        "forecast_recorded",
        json!({"tasks": [{"id": 5}, {"id": 6}]}),
    );
    let mut row = run_row(RUN, None, Vec::new());
    row.kind = LedgerKind::Queue;
    row.records = vec![old, ask, linked, forecast];
    let mut named = named();
    named.note_queue_row(&row, 5_000);
    let ranges = named.session_key_ranges();
    assert!(ranges.contains(&("runtime_planner/70/".into(), "runtime_planner/700".into())));
    assert!(
        !ranges
            .iter()
            .any(|(low, _)| low.starts_with("runtime_planner/71/"))
    );
    assert_eq!(named.goal_proposals(), BTreeSet::from([8, 12]));
    assert_eq!(named.snapshot_tasks(), BTreeSet::from([5, 6]));
    // A plan's change of a snapshot's task is named, its other records not.
    assert!(named.names(&event(
        13,
        Some(6),
        None,
        "task_priority_changed",
        json!({})
    )));
    assert!(!named.names(&event(14, Some(6), None, "task_status_changed", json!({}))));
}

/// An ask of no actor (an older binary's) names its planner by `asked_by`;
/// an actor, when there is one, is the asker whatever `asked_by` says.
#[test]
fn an_ask_of_no_actor_names_its_planner_by_asked_by() {
    let old = event(
        10,
        None,
        None,
        "ask_opened",
        json!({"kind": "planner_question", "asked_by": "planner:72"}),
    );
    let mut acted = event(
        11,
        None,
        None,
        "ask_opened",
        json!({"kind": "planner_question", "asked_by": "planner:73"}),
    );
    acted.actor = Some(EventActor {
        role: "planner".into(),
        id: "planner:74".into(),
        requested_by: None,
    });
    let mut row = run_row(RUN, None, Vec::new());
    row.kind = LedgerKind::Queue;
    row.records = vec![old, acted];
    let mut named = Named::default();
    named.note_queue_row(&row, 0);
    let lows: Vec<String> = named
        .session_key_ranges()
        .into_iter()
        .map(|(low, _)| low)
        .collect();
    assert_eq!(lows, ["runtime_planner/72/", "runtime_planner/74/"]);
}

#[test]
fn a_seek_takes_a_task_s_records_and_a_run_s_past_its_end_but_not_its_claims_or_sessions() {
    let named = named();
    assert!(named.takes(&event(20, Some(5), None, "task_status_changed", json!({}))));
    assert!(named.takes(&event(
        21,
        Some(5),
        Some(RUN),
        "task_priority_changed",
        json!({})
    )));
    assert!(named.takes(&event(
        22,
        Some(1),
        Some(RUN),
        "follow_up_registered",
        json!({"task_id": 5})
    )));
    for kind in [
        "run_claimed",
        "ask_opened",
        "session_opened",
        "run_integrated",
        "turn_finished",
    ] {
        assert!(
            !named.takes(&event(23, Some(5), Some(RUN), kind, json!({}))),
            "{kind}"
        );
    }
    // A session's and the queue's come with their rows.
    assert!(!named.takes(&event(
        24,
        None,
        None,
        "plan_review_finished",
        json!({"plan_review_id": 9})
    )));
    assert!(!named.takes(&event(25, Some(6), None, "task_status_changed", json!({}))));
}

#[test]
fn a_proposal_s_members_and_its_goals_tasks_are_found_by_what_names_them() {
    let named = named();
    let submitted = event(
        30,
        Some(66),
        None,
        "task_submitted",
        json!({"proposal_id": 8}),
    );
    assert_eq!(named.member_of(&submitted), Some(66));
    let own = event(
        31,
        Some(5),
        None,
        "task_submitted",
        json!({"proposal_id": 8}),
    );
    assert_eq!(named.member_of(&own), None, "named whole already");
    let mut goal = event(32, None, None, "goal_submitted", json!({"proposal_id": 8}));
    goal.goal_id = Some(GoalId::new(80));
    assert_eq!(named.goal_of(&goal), Some(80));
    let goals = BTreeSet::from([80]);
    for (kind, payload) in [
        ("task_created", json!({"goal_id": 80})),
        ("attributes_baseline", json!({"goal_id": 80})),
        ("task_goal_changed", json!({"to": 80})),
    ] {
        assert_eq!(
            goal_task(&event(33, Some(82), None, kind, payload), &goals),
            Some(82),
            "{kind}"
        );
    }
    let created = event(34, Some(83), None, "task_created", json!({"goal_id": 81}));
    assert_eq!(goal_task(&created, &goals), None);
}

#[test]
fn a_linked_proposal_holding_a_goal_is_sought_as_a_named_one() {
    let mut queue = run_row(RUN, None, Vec::new());
    queue.kind = LedgerKind::Queue;
    queue.records = [40, 44]
        .into_iter()
        .map(|proposal| {
            event(
                proposal,
                None,
                None,
                "finding_status_changed",
                json!({"to": "proposed", "proposal_id": proposal}),
            )
        })
        .collect();
    let mut named = named();
    named.note_queue_row(&queue, 0);
    let goal = |id: i64, proposal: i64| {
        let mut event = event(
            id,
            None,
            None,
            "goal_submitted",
            json!({"proposal_id": proposal}),
        );
        event.goal_id = Some(GoalId::new(88));
        event
    };
    // 40 holds a goal; 44 none; 41 is linked by no finding.
    let with = named.with_goal_proposals_of(&[goal(50, 40), goal(51, 41)]);
    assert_eq!(with.proposals(), &BTreeSet::from([8, 40]));
    let canceled = event(
        52,
        Some(401),
        None,
        "plan_decided",
        json!({"proposal_id": 40, "status": "canceled"}),
    );
    assert!(!named.names(&canceled));
    assert!(with.names(&canceled));
    let submitted = event(
        53,
        Some(401),
        None,
        "task_submitted",
        json!({"proposal_id": 40}),
    );
    assert_eq!(with.member_of(&submitted), Some(401));
}

#[test]
fn a_member_s_ends_are_its_creation_baseline_last_changes_and_the_reopenings_after() {
    let records = vec![
        event(1, Some(66), None, "task_created", json!({})),
        event(2, Some(66), None, "attributes_baseline", json!({})),
        event(3, Some(66), None, "task_reopened", json!({})),
        event(4, Some(66), None, "task_status_changed", json!({})),
        event(5, Some(66), None, "task_goal_changed", json!({})),
        event(6, Some(66), None, "task_status_changed", json!({})),
        event(7, Some(66), None, "task_reopened", json!({})),
        event(8, Some(66), None, "task_goal_changed", json!({})),
        event(9, Some(66), None, "task_priority_changed", json!({})),
    ];
    let ends: Vec<i64> = records
        .iter()
        .filter(|event| member_end(event, &records))
        .map(|event| event.id.as_i64())
        .collect();
    assert_eq!(ends, vec![1, 2, 6, 7, 8]);
}

#[test]
fn a_session_s_record_a_seek_gives_names_its_planner_when_it_names_what_is_named() {
    let named = named();
    let revised = event(
        40,
        Some(5),
        None,
        "plan_revise_sent",
        json!({"planner_id": 77, "proposal_id": 8}),
    );
    let other = event(
        41,
        Some(6),
        None,
        "plan_revise_sent",
        json!({"planner_id": 78, "proposal_id": 9}),
    );
    let ranges = named
        .with_sessions_of(&[revised, other])
        .session_key_ranges();
    assert!(ranges.contains(&("runtime_planner/77/".into(), "runtime_planner/770".into())));
    assert!(
        !ranges
            .iter()
            .any(|(low, _)| low.starts_with("runtime_planner/78/"))
    );
}
