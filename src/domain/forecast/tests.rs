use std::collections::HashMap;

use serde_json::{Value, json};

use super::*;
use crate::domain::{EventId, Priority, RunEvent, RunId};

const NOW: i64 = 1_790_000_000;

fn sample_of(work: i64, validate: i64, wait_to_land: i64) -> Sample {
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
                    sample_of(s * 6 / 10, s / 10, s - s * 6 / 10 - s / 10),
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
        sample_of(5000, 0, 0),
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
fn a_run_in_flight_draws_the_rest_of_its_stage_from_longer_runs() {
    let h = History {
        runs: vec![
            (None, sample_of(100, 10, 20)),
            (None, sample_of(1000, 10, 20)),
        ],
        ..History::default()
    };
    let mut working = task(1, Priority::Normal, &[]);
    working.running = Some(Running {
        phase: Some("worker".into()),
        stage: Stage::Work,
        elapsed: 400,
        waiting: None,
    });
    let mut landing = task(2, Priority::Normal, &[]);
    landing.running = Some(Running {
        phase: Some("landing".into()),
        stage: Stage::WaitToLand,
        elapsed: 5,
        waiting: None,
    });
    let forecast = run(&[working, landing], &[], 2, &h);
    // Only the 1000-second work lasted past 400 seconds.
    assert_eq!(p50(&forecast, 1), Some(600 + 30));
    assert_eq!(p50(&forecast, 2), Some(15));
    assert_eq!(forecast.tasks[0].phase.as_deref(), Some("worker"));
    // Past every sample, the stage is drawn again whole.
    let mut late = task(3, Priority::Normal, &[]);
    late.running = Some(Running {
        phase: None,
        stage: Stage::Validate,
        elapsed: 50,
        waiting: None,
    });
    let forecast = run(&[late], &[], 1, &h);
    assert_eq!(p50(&forecast, 3), Some(30));
}

#[test]
fn a_run_waiting_for_a_person_holds_no_slot() {
    let h = History {
        runs: vec![(None, sample_of(100, 0, 0))],
        ask_waits: vec![50, 300],
        ..History::default()
    };
    let mut waiting = task(1, Priority::Normal, &[]);
    waiting.running = Some(Running {
        phase: Some("waiting".into()),
        stage: Stage::Work,
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
        runs: vec![(None, sample_of(100, 0, 0))],
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
        runs: vec![(None, sample_of(100, 0, 0))],
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
        kind: kind.into(),
        payload,
        created_at: at.to_owned(),
        actor: None,
    }
}

/// The history takes a landed run's sample from the ledger's one read of
/// its row, and the close delays and answers from those events alone.
/// A legacy row (a run from before `run_phase_changed`) draws the sample
/// `stats` gave the run, the value the forecast drew before the ledger.
#[test]
fn history_holds_landed_runs_close_delays_and_answers() {
    use crate::domain::ledger::{read::read, run_row};
    use crate::domain::stats::{LiveSnapshot, SlotSnapshot, StatsQuery};
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
    let step = |id: i64, kind: &str, minute: &str, payload: Value| {
        event(
            id,
            r,
            5,
            kind,
            &format!("2026-09-20T00:{minute}:00.000Z"),
            payload,
        )
    };
    let run_events = [
        step(1, "run_claimed", "00", json!({})),
        step(2, "receipt_observed", "10", json!({})),
        step(3, "validation_finished", "11", json!({})),
        step(4, "run_integrated", "20", json!({"status": "integrated"})),
    ];
    let opened = event(
        5,
        "",
        5,
        "ask_opened",
        "2026-09-20T00:30:00.000Z",
        json!({"ask_id": 1}),
    );
    let answered = event(
        7,
        "",
        5,
        "ask_answered",
        "2026-09-20T00:35:00.000Z",
        json!({"ask_id": 1}),
    );
    let goals = HashMap::from([(TaskId::new(5), Some(GoalId::new(3)))]);
    let docs = || Some("docs".parse::<TaskChange>().unwrap());
    let row = run_row(r, &run_events, &[], 0, "built");
    assert!(row.is_legacy());
    let drawn = sample(&read(&row)).unwrap();
    // What `stats` gave the run before the ledger.
    let all: Vec<RunEvent> = run_events
        .iter()
        .cloned()
        .chain([opened.clone(), closed.clone(), answered.clone()])
        .collect();
    let stats = crate::domain::stats::stats(
        &all,
        &goals,
        NOW,
        SlotSnapshot::default(),
        &StatsQuery {
            full: true,
            ..StatsQuery::default()
        },
        &LiveSnapshot::default(),
    );
    let before = &stats.runs[0];
    assert_eq!(
        (drawn.work, drawn.validate, drawn.wait_to_land),
        (
            before.work.unwrap(),
            before.validate.unwrap(),
            before.wait_to_land.unwrap()
        )
    );
    assert_eq!(
        drawn,
        Sample {
            work: 600,
            validate: 60,
            wait_to_land: 540
        }
    );
    let h = super::history(
        vec![(docs(), drawn)],
        &[run_events[3].clone(), closed],
        &[opened, answered],
        &goals,
    );
    assert_eq!(
        h.runs,
        [(
            docs(),
            Sample {
                work: 600,
                validate: 60,
                wait_to_land: 540
            }
        )]
    );
    assert_eq!(h.close_delays, [3000]);
    assert_eq!(h.ask_waits, [300]);
    assert!(h.last_landings.contains_key(&GoalId::new(3)));
    assert_eq!(
        super::history(Vec::new(), &[], &[], &goals),
        History::default()
    );
    // A row of a run that did not land, or lacks a value, draws nothing.
    let unlanded = run_row(r, &run_events[..3], &[], 0, "built");
    assert_eq!(sample(&read(&unlanded)), None);
}

/// A new row's sample is its intervals under today's names, as the
/// ledger's one read gives them.
#[test]
fn a_new_rows_sample_is_its_intervals_under_todays_names() {
    use crate::domain::ledger::testkit::{Log, run_id};
    use crate::domain::ledger::{read::read, run_row};
    let r = run_id(1);
    let mut log = Log::default();
    log.landed(5, &r, 0);
    let drawn = sample(&read(&run_row(&r, &log.events, &[], 0, "built"))).unwrap();
    // Worked to the `validating` at 600 s, validated to 660 s, landed at
    // the push from 900 s.
    assert_eq!(
        drawn,
        Sample {
            work: 600,
            validate: 60,
            wait_to_land: 240
        }
    );
}

/// A run in flight is placed by its fold: its stage by the first
/// `validating`, the phase it goes on in, and the time in a phase a
/// person's answer decides. One with no `run_phase_changed` has its stage
/// drawn whole.
#[test]
fn a_run_in_flight_is_placed_by_its_fold() {
    use crate::domain::ledger::testkit::{Log, ms, run_id};
    use crate::domain::run_phase::Phase as RunPhase;
    let r = run_id(2);
    let mut log = Log::default();
    log.add(Some(5), Some(&r), "run_claimed", json!({}), 0)
        .phase(5, &r, 0, RunPhase::Provisioning, "x")
        .phase(5, &r, 10, RunPhase::Worker, "x");
    let work = running(&log.events, ms(1200));
    assert_eq!(
        (
            work.phase.as_deref(),
            work.stage,
            work.elapsed,
            work.waiting
        ),
        (Some("worker"), Stage::Work, 1200, None)
    );
    log.phase(5, &r, 600, RunPhase::Validating, "x");
    let validate = running(&log.events, ms(1200));
    assert_eq!((validate.stage, validate.elapsed), (Stage::Validate, 600));
    log.phase(5, &r, 660, RunPhase::Review, "x");
    let landing = running(&log.events, ms(1200));
    assert_eq!(
        (landing.phase.as_deref(), landing.stage, landing.elapsed),
        (Some("review"), Stage::WaitToLand, 540)
    );
    log.phase(5, &r, 900, RunPhase::LandingAnswer, "approve_landing");
    let asked = running(&log.events, ms(1200));
    assert_eq!(
        (asked.phase.as_deref(), asked.stage, asked.waiting),
        (Some("landing_answer"), Stage::WaitToLand, Some(300))
    );
    // No record: the stage is drawn whole.
    let legacy = [event(
        1,
        &r,
        5,
        "run_claimed",
        "2026-09-20T00:00:00.000Z",
        json!({}),
    )];
    let whole = running(&legacy, ms(1200));
    assert_eq!(
        (whole.phase, whole.stage, whole.elapsed, whole.waiting),
        (None, Stage::Work, 0, None)
    );
    assert_eq!(running(&[], ms(0)).elapsed, 0);
}

#[test]
fn a_sample_measures_its_stages() {
    let s = sample_of(10, 20, 30);
    assert_eq!(s.total(), 60);
    assert_eq!(s.stage(Stage::Validate), 20);
    assert_eq!(s.from(Stage::Validate), 50);
    assert_eq!(s.from(Stage::WaitToLand), 30);
    assert_eq!(s.stage(Stage::WaitToLand), 30);
    assert_eq!(s.stage(Stage::Work), 10);
}

#[test]
fn the_latest_run_is_in_flight_through_its_review_revise_and_resume() {
    use crate::domain::RunStatus;
    for status in [
        RunStatus::Claimed,
        RunStatus::Starting,
        RunStatus::Running,
        RunStatus::Validating,
        RunStatus::AwaitingIntegration,
        RunStatus::Integrating,
        RunStatus::NeedsSession,
    ] {
        assert!(in_flight(status), "{status:?}");
    }
    // A retry waits for a slot and starts over; a landed run is done.
    for status in [
        RunStatus::Failed,
        RunStatus::Interrupted,
        RunStatus::Integrated,
        RunStatus::Succeeded,
    ] {
        assert!(!in_flight(status), "{status:?}");
    }
}

/// A run sent back by its review (revise) or resumed after `needs_session`
/// stays in `wait_to_land` from the end of its first `validating`: the
/// samples' `wait_to_land` holds the review, the resumes and the landing.
/// Its wait for a person's answer (a phase whose blocker is `human`) holds
/// no slot; once it returns it holds one again.
#[test]
fn a_run_under_review_revise_or_resume_stays_waiting_to_land() {
    use crate::domain::ledger::testkit::{Log, ms, run_id};
    use crate::domain::run_phase::{Attempt, AttemptKind, Phase as RunPhase};
    let r = run_id(3);
    let mut log = Log::default();
    let resume = Attempt::of(AttemptKind::Resume, 1);
    log.add(Some(5), Some(&r), "run_claimed", json!({}), 0)
        .phase(5, &r, 0, RunPhase::Worker, "x")
        .phase(5, &r, 600, RunPhase::Validating, "x")
        .phase(5, &r, 660, RunPhase::Review, "x")
        .phase(5, &r, 840, RunPhase::NeedsSession, "x")
        .phase_in(5, &r, 900, RunPhase::Resume, resume, "x")
        .phase_in(5, &r, 1500, RunPhase::Validating, resume, "x")
        .phase_in(5, &r, 1560, RunPhase::E2e, resume, "x");
    let now = ms(31 * 60);
    let revised = running(&log.events, now);
    assert_eq!(
        (revised.stage, revised.elapsed, revised.waiting),
        (Stage::WaitToLand, 20 * 60, None)
    );
    log.phase_in(
        5,
        &r,
        28 * 60,
        RunPhase::Waiting { in_slot: false },
        resume,
        "ask",
    );
    let asked = running(&log.events, now);
    assert_eq!(
        (asked.stage, asked.elapsed, asked.waiting),
        (Stage::WaitToLand, 20 * 60, Some(3 * 60))
    );
    log.phase_in(5, &r, 30 * 60, RunPhase::Returning, resume, "answered");
    let returning = running(&log.events, now);
    assert_eq!(
        (returning.stage, returning.waiting),
        (Stage::WaitToLand, None)
    );

    // In the forecast: the rest of `wait_to_land` past 20 minutes from the
    // longer sample only. The ready docs task (100 seconds) takes the one
    // slot while the run waits for a person, and after it otherwise.
    let docs = || "docs".parse::<TaskChange>().unwrap();
    let h = History {
        runs: vec![
            (None, sample_of(600, 60, 600)),
            (None, sample_of(600, 60, 3000)),
            (Some(docs()), sample_of(100, 0, 0)),
        ],
        ask_waits: vec![60, 600],
        ..History::default()
    };
    let with = |running: Running| {
        let mut in_flight = task(1, Priority::High, &[]);
        in_flight.running = Some(running);
        let mut ready = task(2, Priority::Normal, &[]);
        ready.change = Some(docs());
        run(&[in_flight, ready], &[], 1, &h)
    };
    let forecast = with(revised);
    assert_eq!(forecast.tasks[0].phase.as_deref(), Some("e2e"));
    assert!(!forecast.tasks[0].waiting);
    assert_eq!(p50(&forecast, 1), Some(3000 - 1200));
    assert_eq!(p50(&forecast, 2), Some(1800 + 100));
    let forecast = with(asked);
    assert_eq!(forecast.tasks[0].phase.as_deref(), Some("waiting"));
    assert!(forecast.tasks[0].waiting);
    // The 600-second answer outlasted 180 seconds: 420 more, then the
    // rest of `wait_to_land`.
    assert_eq!(p50(&forecast, 1), Some(420 + 1800));
    assert_eq!(p50(&forecast, 2), Some(100));
    let forecast = with(returning);
    assert!(!forecast.tasks[0].waiting);
    assert_eq!(p50(&forecast, 2), Some(1800 + 100));
}
