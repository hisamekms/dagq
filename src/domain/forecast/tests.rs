use std::collections::HashMap;

use serde_json::{Value, json};

use super::*;
use crate::domain::{EventId, Priority, RunEvent, RunId};

const NOW: i64 = 1_790_000_000;

fn sample(work: i64, validate: i64, wait_to_land: i64) -> Sample {
    Sample {
        work,
        validate,
        wait_to_land,
    }
}

/// Every run lands in `secs`: 60% work, 10% validate, 30% wait.
fn history(change: Option<TaskChange>, secs: &[i64]) -> History {
    History {
        runs: secs
            .iter()
            .map(|&s| {
                (
                    change.clone(),
                    sample(s * 6 / 10, s / 10, s - s * 6 / 10 - s / 10),
                )
            })
            .collect(),
        ..History::default()
    }
}

fn task(id: i64, priority: Priority, depends_on: &[i64]) -> ForecastTask {
    ForecastTask {
        id: TaskId::new(id),
        goal_id: None,
        change: None,
        rank: ClaimRank::new(priority, 0, TaskId::new(id)),
        depends_on: depends_on.iter().copied().map(TaskId::new).collect(),
        goal_dependencies: Vec::new(),
        running: None,
    }
}

fn run(tasks: &[ForecastTask], goals: &[ForecastGoal], parallel: usize, h: &History) -> Forecast {
    forecast(&ForecastInput {
        now: NOW,
        tasks,
        goals,
        parallel,
        history: h,
        min_samples: 1,
        trials: 200,
        seed: 7,
    })
}

fn p50(forecast: &Forecast, id: i64) -> Option<i64> {
    forecast
        .tasks
        .iter()
        .find(|task| task.id == TaskId::new(id))
        .and_then(|task| task.at.p50_secs)
}

#[test]
fn a_chain_of_dependencies_finishes_one_after_another() {
    let h = history(None, &[1000]);
    let tasks = [
        task(1, Priority::Normal, &[]),
        task(2, Priority::Normal, &[1]),
        task(3, Priority::Normal, &[2]),
    ];
    let forecast = run(&tasks, &[], 4, &h);
    assert_eq!(p50(&forecast, 1), Some(1000));
    assert_eq!(p50(&forecast, 2), Some(2000));
    assert_eq!(p50(&forecast, 3), Some(3000));
    let first = &forecast.tasks[0];
    assert_eq!(first.at.p50.as_deref(), Some("2026-09-21T14:30:00.000Z"));
    assert_eq!(first.at.p90_secs, Some(1000));
    assert_eq!(first.at.reason, None);
    assert_eq!(forecast.method, METHOD);
    assert_eq!(forecast.at, "2026-09-21T14:13:20.000Z");
}

#[test]
fn independent_tasks_wait_for_a_free_slot() {
    let h = history(None, &[600]);
    let tasks: Vec<_> = (1..=5).map(|id| task(id, Priority::Normal, &[])).collect();
    let two = run(&tasks, &[], 2, &h);
    let ends: Vec<_> = (1..=5).map(|id| p50(&two, id)).collect();
    assert_eq!(
        ends,
        [Some(600), Some(600), Some(1200), Some(1200), Some(1800)]
    );
    let five = run(&tasks, &[], 5, &h);
    assert!((1..=5).all(|id| p50(&five, id) == Some(600)));
    assert_eq!(five.assumptions.parallel, 5);
}

#[test]
fn the_higher_effective_priority_is_claimed_first() {
    let h = history(None, &[600]);
    let tasks = [
        task(1, Priority::Low, &[]),
        task(2, Priority::Normal, &[]),
        task(3, Priority::Urgent, &[]),
    ];
    let forecast = run(&tasks, &[], 1, &h);
    assert_eq!(p50(&forecast, 3), Some(600));
    assert_eq!(p50(&forecast, 2), Some(1200));
    assert_eq!(p50(&forecast, 1), Some(1800));
    // With the same priority, the task that releases more goes first, then
    // the lower ID.
    let mut tasks = [
        task(1, Priority::Normal, &[]),
        task(2, Priority::Normal, &[]),
    ];
    tasks[1].rank = ClaimRank::new(Priority::Normal, 3, TaskId::new(2));
    let forecast = run(&tasks, &[], 1, &h);
    assert_eq!(p50(&forecast, 2), Some(600));
    assert_eq!(p50(&forecast, 1), Some(1200));
}

#[test]
fn the_same_seed_gives_the_same_forecast_and_percentiles_spread() {
    let h = history(None, &[100, 200, 300, 400, 500, 600, 700, 800, 900, 1000]);
    let tasks = [
        task(1, Priority::Normal, &[]),
        task(2, Priority::Normal, &[1]),
    ];
    let first = run(&tasks, &[], 1, &h);
    assert_eq!(first, run(&tasks, &[], 1, &h));
    let one = &first.tasks[0].at;
    assert!(one.p50_secs < one.p90_secs, "{one:?}");
    assert!((100..=1000).contains(&one.p50_secs.unwrap()));
    assert!(p50(&first, 2) > p50(&first, 1));
    let other = forecast(&ForecastInput {
        seed: 8,
        ..ForecastInput {
            now: NOW,
            tasks: &tasks,
            goals: &[],
            parallel: 1,
            history: &h,
            min_samples: 1,
            trials: 200,
            seed: 7,
        }
    });
    assert_eq!(other.seed, 8);
    assert_eq!(seed(NOW, 42), seed(NOW, 42));
    assert_ne!(seed(NOW, 42), seed(NOW, 43));
}

#[test]
fn a_change_with_too_few_samples_draws_from_the_whole_distribution() {
    let mut h = history(
        Some("docs".parse::<TaskChange>().unwrap()),
        &[100, 100, 100],
    );
    h.runs.push((
        Some("runtime".parse::<TaskChange>().unwrap()),
        sample(5000, 0, 0),
    ));
    let mut docs = task(1, Priority::Normal, &[]);
    docs.change = Some("docs".parse::<TaskChange>().unwrap());
    let mut runtime = task(2, Priority::Normal, &[]);
    runtime.change = Some("runtime".parse::<TaskChange>().unwrap());
    let forecast = forecast(&ForecastInput {
        now: NOW,
        tasks: &[docs, runtime],
        goals: &[],
        parallel: 2,
        history: &h,
        min_samples: 2,
        trials: 50,
        seed: 1,
    });
    assert_eq!(p50(&forecast, 1), Some(100));
    assert_eq!(forecast.tasks[0].distribution, "docs");
    assert_eq!(forecast.tasks[1].distribution, ALL);
    assert_eq!(forecast.assumptions.substituted, ["runtime"]);
    let samples = &forecast.assumptions.samples;
    assert_eq!(samples.all, 4);
    assert_eq!(samples.changes["runtime"].runs, 1);
    assert_eq!(samples.changes["docs"].distribution, "docs");
    assert_eq!(forecast.assumptions.min_samples, 2);
}

#[test]
fn a_run_in_flight_draws_the_rest_of_its_phase_from_longer_runs() {
    let h = History {
        runs: vec![(None, sample(100, 10, 20)), (None, sample(1000, 10, 20))],
        ..History::default()
    };
    let mut working = task(1, Priority::Normal, &[]);
    working.running = Some(Running {
        phase: Phase::Work,
        elapsed: 400,
        waiting: None,
    });
    let mut landing = task(2, Priority::Normal, &[]);
    landing.running = Some(Running {
        phase: Phase::WaitToLand,
        elapsed: 5,
        waiting: None,
    });
    let forecast = run(&[working, landing], &[], 2, &h);
    // Only the 1000-second work lasted past 400 seconds.
    assert_eq!(p50(&forecast, 1), Some(600 + 30));
    assert_eq!(p50(&forecast, 2), Some(15));
    assert_eq!(forecast.tasks[0].phase, Some(Phase::Work));
    // Past every sample, the phase is drawn again whole.
    let mut late = task(3, Priority::Normal, &[]);
    late.running = Some(Running {
        phase: Phase::Validate,
        elapsed: 50,
        waiting: None,
    });
    let forecast = run(&[late], &[], 1, &h);
    assert_eq!(p50(&forecast, 3), Some(30));
}

#[test]
fn a_run_waiting_for_a_person_holds_no_slot() {
    let h = History {
        runs: vec![(None, sample(100, 0, 0))],
        ask_waits: vec![50, 300],
        ..History::default()
    };
    let mut waiting = task(1, Priority::Normal, &[]);
    waiting.running = Some(Running {
        phase: Phase::Work,
        elapsed: 10,
        waiting: Some(100),
    });
    let forecast = run(&[waiting, task(2, Priority::Normal, &[])], &[], 1, &h);
    // Only the 300-second wait lasted past 100 seconds: 200 more, then the
    // 90 seconds of work left.
    assert_eq!(p50(&forecast, 1), Some(290));
    assert!(forecast.tasks[0].waiting);
    assert_eq!(p50(&forecast, 2), Some(100));
    assert_eq!(forecast.assumptions.samples.ask_wait, 2);
}

#[test]
fn a_goal_closes_after_its_tasks_and_releases_the_tasks_waiting_for_it() {
    let h = History {
        runs: vec![(None, sample(100, 0, 0))],
        close_delays: vec![1000],
        ..History::default()
    };
    let goal = |id: i64| ForecastGoal {
        id: GoalId::new(id),
        since_last_landing: None,
        unplanned_tasks: 0,
        empty: false,
    };
    let mut member = task(1, Priority::Normal, &[]);
    member.goal_id = Some(GoalId::new(10));
    let mut after = task(2, Priority::Normal, &[]);
    after.goal_dependencies = vec![GoalId::new(10)];
    let mut idle = goal(11);
    idle.since_last_landing = Some(400);
    let forecast = run(&[member, after], &[goal(10), idle], 2, &h);
    assert_eq!(p50(&forecast, 1), Some(100));
    assert_eq!(forecast.goals[0].at.p50_secs, Some(1100));
    assert_eq!(forecast.goals[0].open_tasks, 1);
    assert_eq!(p50(&forecast, 2), Some(1200));
    // A goal with no open task left only waits to be closed.
    assert_eq!(forecast.goals[1].at.p50_secs, Some(600));
    assert_eq!(forecast.goals[1].open_tasks, 0);
    assert_eq!(forecast.assumptions.samples.close_delay, 1);
    // A goal with unplanned tasks, or with none at all, never closes here,
    // and holds back the tasks waiting for it; the rest still finish.
    let mut unplanned = goal(10);
    unplanned.unplanned_tasks = 1;
    let mut empty = goal(11);
    empty.empty = true;
    let mut on_empty = task(3, Priority::Normal, &[]);
    on_empty.goal_dependencies = vec![GoalId::new(11)];
    let mut member = task(1, Priority::Normal, &[]);
    member.goal_id = Some(GoalId::new(10));
    let mut after = task(2, Priority::Normal, &[]);
    after.goal_dependencies = vec![GoalId::new(10)];
    let forecast = run(&[member, after, on_empty], &[unplanned, empty], 2, &h);
    assert_eq!(p50(&forecast, 1), Some(100));
    assert_eq!(forecast.goals[0].at.reason, Some("blocked"));
    assert_eq!(forecast.goals[1].at.reason, Some("blocked"));
    assert_eq!(forecast.tasks[1].at.reason, Some("blocked"));
    assert_eq!(forecast.tasks[2].at.reason, Some("blocked"));
}

#[test]
fn a_zero_delay_is_drawn_like_any_other() {
    let h = History {
        runs: vec![(None, sample(100, 0, 0))],
        close_delays: vec![0],
        ..History::default()
    };
    let mut member = task(1, Priority::Normal, &[]);
    member.goal_id = Some(GoalId::new(10));
    let goal = ForecastGoal {
        id: GoalId::new(10),
        since_last_landing: None,
        unplanned_tasks: 0,
        empty: false,
    };
    let forecast = run(&[member], &[goal], 1, &h);
    assert_eq!(forecast.goals[0].at.p50_secs, Some(100));
}

#[test]
fn what_never_finishes_says_why() {
    let h = history(None, &[100]);
    let mut on_draft_goal = task(2, Priority::Normal, &[]);
    on_draft_goal.goal_dependencies = vec![GoalId::new(99)];
    let tasks = [
        task(1, Priority::Normal, &[50]),
        on_draft_goal,
        task(3, Priority::Normal, &[]),
    ];
    let forecast = run(&tasks, &[], 1, &h);
    assert_eq!(forecast.tasks[0].at.reason, Some("blocked"));
    assert_eq!(forecast.tasks[0].at.p50, None);
    assert_eq!(forecast.tasks[1].at.reason, Some("blocked"));
    assert_eq!(p50(&forecast, 3), Some(100));
    // Without a slot, what waits for a left-out task is still blocked.
    let no_slots = run(&tasks, &[], 0, &h);
    assert_eq!(no_slots.tasks[0].at.reason, Some("blocked"));
    assert_eq!(no_slots.tasks[2].at.reason, Some("no_slots"));
    let no_samples = run(&tasks[2..], &[], 1, &History::default());
    assert_eq!(no_samples.tasks[0].at.reason, Some("no_samples"));
    let json = serde_json::to_value(&no_samples).unwrap();
    assert_eq!(json["tasks"][0]["p50"], Value::Null);
    assert_eq!(json["tasks"][0]["reason"], "no_samples");
    assert_eq!(json["tasks"][0]["distribution"], ALL);
    assert!(json["assumptions"]["left_out"].as_array().unwrap().len() > 1);
}

fn event(id: i64, run: &str, task: i64, kind: &str, at: &str, payload: Value) -> RunEvent {
    RunEvent {
        id: EventId::new(id),
        task_id: (task > 0).then_some(TaskId::new(task)),
        goal_id: None,
        run_id: (!run.is_empty()).then(|| RunId::new(run).unwrap()),
        kind: kind.to_owned(),
        payload,
        created_at: at.to_owned(),
        actor: None,
    }
}

#[test]
fn history_holds_landed_runs_close_delays_and_answers() {
    let r = "11111111-1111-1111-1111-111111111111";
    let mut closed = event(
        6,
        "",
        0,
        "goal_closed",
        "2026-09-20T01:10:00.000Z",
        json!({"verdict": "achieved"}),
    );
    closed.goal_id = Some(GoalId::new(3));
    let events = [
        event(
            1,
            r,
            5,
            "run_claimed",
            "2026-09-20T00:00:00.000Z",
            json!({}),
        ),
        event(
            2,
            r,
            5,
            "receipt_observed",
            "2026-09-20T00:10:00.000Z",
            json!({}),
        ),
        event(
            3,
            r,
            5,
            "validation_finished",
            "2026-09-20T00:11:00.000Z",
            json!({}),
        ),
        event(
            4,
            r,
            5,
            "run_integrated",
            "2026-09-20T00:20:00.000Z",
            json!({"status": "integrated"}),
        ),
        event(
            5,
            "",
            5,
            "ask_opened",
            "2026-09-20T00:30:00.000Z",
            json!({"ask_id": 1}),
        ),
        closed,
        event(
            7,
            "",
            5,
            "ask_answered",
            "2026-09-20T00:35:00.000Z",
            json!({"ask_id": 1}),
        ),
    ];
    let goals = HashMap::from([(TaskId::new(5), Some(GoalId::new(3)))]);
    let changes = HashMap::from([(TaskId::new(5), Some("docs".parse::<TaskChange>().unwrap()))]);
    let h = super::history(&events, &goals, &changes, NOW);
    assert_eq!(
        h.runs,
        [(
            Some("docs".parse::<TaskChange>().unwrap()),
            sample(600, 60, 540)
        )]
    );
    assert_eq!(h.close_delays, [3000]);
    assert_eq!(h.ask_waits, [300]);
    assert!(h.last_landings.contains_key(&GoalId::new(3)));
    assert_eq!(
        super::history(&[], &goals, &changes, NOW),
        History::default()
    );
}

#[test]
fn a_run_in_flight_is_in_the_phase_of_its_latest_step() {
    let r = "22222222-2222-2222-2222-222222222222";
    let ms = |text: &str| crate::domain::stats::timestamp_millis(text).unwrap();
    let claimed = event(
        1,
        r,
        5,
        "run_claimed",
        "2026-09-20T00:00:00.000Z",
        json!({}),
    );
    let receipt = event(
        2,
        r,
        5,
        "receipt_observed",
        "2026-09-20T00:10:00.000Z",
        json!({}),
    );
    let validated = event(
        3,
        r,
        5,
        "validation_finished",
        "2026-09-20T00:11:00.000Z",
        json!({}),
    );
    let now = ms("2026-09-20T00:20:00.000Z");
    let work = running(std::slice::from_ref(&claimed), now);
    assert_eq!(
        (work.phase, work.elapsed, work.waiting),
        (Phase::Work, 1200, None)
    );
    let validate = running(&[claimed.clone(), receipt.clone()], now);
    assert_eq!((validate.phase, validate.elapsed), (Phase::Validate, 600));
    let landing = running(&[claimed, receipt, validated], now);
    assert_eq!((landing.phase, landing.elapsed), (Phase::WaitToLand, 540));
    let waits = [
        event(
            1,
            r,
            5,
            "run_claimed",
            "2026-09-20T00:00:00.000Z",
            json!({}),
        ),
        event(
            2,
            r,
            5,
            crate::domain::waiting::RUN_WAITING_STARTED,
            "2026-09-20T00:15:00.000Z",
            json!({"phase": "work"}),
        ),
    ];
    assert_eq!(running(&waits, now).waiting, Some(300));
    assert_eq!(running(&[], now).elapsed, 0);
}

#[test]
fn a_sample_measures_its_phases() {
    let s = sample(10, 20, 30);
    assert_eq!(s.total(), 60);
    assert_eq!(s.phase(Phase::Validate), 20);
    assert_eq!(s.from(Phase::Validate), 50);
    assert_eq!(s.from(Phase::WaitToLand), 30);
    assert_eq!(s.phase(Phase::WaitToLand), 30);
    assert_eq!(s.phase(Phase::Work), 10);
}
