//! [`forecast_events`] on the cases of Spike 4670's (vii) and Spike 4756's
//! (ii-a), and [`cohort`] on those of Spike 4135's (v) and Spike 4670's
//! (viii).
use super::*;
use crate::domain::stats;

const TASK: i64 = 900;
const OTHER: i64 = 901;
const GOAL: i64 = 50;
const AHEAD: i64 = 902;
const CANCELED: i64 = 903;

/// Task 900 finished once and reopened, snapshots of it before and after
/// that finish, of goal 50 and of task 901 (which does not finish in the
/// range); between the snapshots and the range a new build in a claim, a
/// recorded mark retracted after the range, a change of task 900's priority
/// and `between` settled units; in the range task 900 and goal 50 finish,
/// task 902 finishes again (its first finish written by a clock ahead past
/// the range) and task 903 is canceled; after it a mark backdated before
/// the snapshots (after them by id). `before` settled units precede it all.
pub(super) fn forecast_history(before: i64, between: i64) -> (Log, i64, i64, ()) {
    let mut log = Log::default();
    let t = log.settled(before, T0);
    log.push(
        None,
        Some(GOAL),
        None,
        EventKind::GoalCreated,
        json!({"title": "g"}),
        t,
    );
    for task in [TASK, AHEAD, CANCELED] {
        log.created(task, t);
    }
    // Written before the snapshots by a clock ahead: after the one without
    // `triggers_through` by time alone.
    log.priority(TASK, t + 7 * MIN);
    log.mark("clock ahead", None, t + 7 * MIN);
    log.snapshot(&[TASK], &[], t);
    log.task_status(TASK, "completed", t + MIN);
    log.task_status(TASK, "ready", t + 2 * MIN);
    log.snapshot(&[TASK, OTHER, AHEAD, CANCELED], &[GOAL], t + 3 * MIN);
    log.snapshot_by_time(&[TASK], t + 3 * MIN + MIN / 2);
    log.run(None, attributes("v2", 4, None), t + 4 * MIN);
    let mark = log.mark("between", None, t + 5 * MIN);
    log.priority(TASK, t + 6 * MIN);
    let t = log.settled(between, t + 10 * MIN);
    let from = t + 10 * MIN;
    let to = from + 60 * MIN;
    log.task_status(AHEAD, "completed", to + 60 * MIN);
    log.task_status(AHEAD, "ready", from + MIN);
    log.task_status(AHEAD, "completed", from + 30 * MIN);
    log.task_status(CANCELED, "canceled", from + 40 * MIN);
    log.task_status(TASK, "completed", from + 10 * MIN);
    log.goal_closed(GOAL, from + 20 * MIN);
    log.retract(mark, to + 10 * MIN);
    log.mark("backdated", Some(T0 - 60 * MIN), to + 20 * MIN);
    (log, from, to, ())
}

fn whole_scoring(store: &Store, from: i64, to: i64) -> Scoring {
    let marks = marks::marks(&store.events, None, None);
    let scoring = score::score(&store.events, &marks);
    let within = |ms: i64| ms > from && ms <= to;
    Scoring {
        samples: scoring
            .samples
            .into_iter()
            .filter(|sample| within(sample.finished_ms))
            .collect(),
        excluded: scoring
            .excluded
            .into_iter()
            .filter(|(_, ms, _)| within(*ms))
            .collect(),
    }
}

fn check_forecast(store: &Store, from: i64, to: i64) {
    let whole = whole_scoring(store, from, to);
    let targets: Vec<&str> = whole.samples.iter().map(|s| s.target.as_str()).collect();
    assert_eq!(
        targets,
        ["task", "goal", "task"],
        "task 900 after reopening (by id and by time), goal 50; not task 902"
    );
    assert_eq!(whole.excluded.len(), 1, "task 903 canceled");
    assert!(whole.samples.iter().all(|sample| sample.marks > 0));
    assert_eq!(forecast_scoring(store, from, to).unwrap(), whole);
}

/// The settled history before the finishing targets were created is not
/// read, however long.
#[test]
fn the_history_before_the_targets_open_is_not_read() {
    let (small, large) = reads_with(
        |units| forecast_history(units, 30),
        |s, f, t, ()| {
            check_forecast(s, f, t);
        },
    );
    assert_eq!(small, large);
}

/// The second step reads the targets' open periods: the history between
/// the snapshots and the range grows what it reads, as the doc comment of
/// [`forecast_events`] bounds it.
#[test]
fn the_open_periods_of_the_targets_are_read() {
    let (small, large) = reads_with(
        |units| forecast_history(30, units),
        |s, f, t, ()| {
            check_forecast(s, f, t);
        },
    );
    assert!(large > small * 10, "{small} {large}");
}

/// A range with snapshots and marks and no finish reads nothing past the
/// finishes, whatever the history.
#[test]
fn a_range_where_no_target_finishes_reads_no_snapshot() {
    let build = |units| {
        let (log, from, _, ()) = forecast_history(units, units);
        (log, from - 200 * MIN, from, ())
    };
    let (small, large) = reads_with(build, |store, from, to, ()| {
        let whole = whole_scoring(store, from, to);
        assert!(whole.samples.is_empty() && whole.excluded.is_empty());
        assert_eq!(forecast_scoring(store, from, to).unwrap(), whole);
    });
    assert_eq!(small, large);
}

/// A snapshot that forecast the task long before the range, a mark
/// recorded in each settled unit between it and the range, and the task
/// finishing in the range: the sample's `marks_between` counts every one
/// of those marks, so the second step reads them (Spike 4756's (ii-a)
/// `does_not_hold`, which planner decision A takes as the second step's
/// bound). What it reads grows with the task's open period (the units
/// between the snapshot and the range, here 30 against 3000, as the
/// answer's 6 against 66 marks of the Spike did), not with the history
/// before the task was created.
pub(super) fn far_snapshot(before: i64, between: i64) -> (Log, i64, i64, ()) {
    let mut log = Log::default();
    let t = log.settled(before, T0);
    log.created(TASK, t);
    log.snapshot(&[TASK], &[], t + MIN);
    let mut at = t + 2 * MIN;
    for n in 0..between {
        log.mark(&format!("unit {n}"), None, at);
        at = log.settled(1, at + MIN);
    }
    let from = at + 10 * MIN;
    let to = from + 60 * MIN;
    log.task_status(TASK, "completed", from + 10 * MIN);
    (log, from, to, ())
}

#[test]
fn a_far_snapshot_reads_the_marks_of_the_open_period() {
    for between in [6, 66] {
        let (log, from, to, ()) = far_snapshot(30, between);
        let store = log.store();
        let whole = whole_scoring(&store, from, to);
        assert_eq!(whole.samples.len(), 1);
        assert!(
            whole.samples[0].marks >= between as usize,
            "{:?}",
            whole.samples
        );
        assert_eq!(forecast_scoring(&store, from, to).unwrap(), whole);
    }
    // Not with the history before the task was created.
    let (small, large) = reads_with(
        |units| far_snapshot(units, 6),
        |s, f, t, ()| {
            assert_eq!(forecast_scoring(s, f, t).unwrap(), whole_scoring(s, f, t));
        },
    );
    assert_eq!(small, large);
    // With the open period.
    let (small, large) = reads_with(
        |units| far_snapshot(30, units / 30),
        |s, f, t, ()| {
            assert_eq!(forecast_scoring(s, f, t).unwrap(), whole_scoring(s, f, t));
        },
    );
    assert!(large > small * 10, "{small} {large}");
}

/// The finish times of every event's runs (`stats`' `finished_event_id`),
/// of the tasks `counts` keeps.
fn whole_finishes(events: &[RunEvent], counts: impl Fn(TaskId) -> bool) -> Vec<i64> {
    let times: HashMap<EventId, i64> = events.iter().map(|e| (e.id, created_ms(e))).collect();
    let mut finishes: Vec<i64> = stats::run_stats(events)
        .iter()
        .filter(|run| counts(run.task_id))
        .filter_map(|run| times.get(&run.finished_event_id?).copied())
        .collect();
    finishes.sort_unstable();
    finishes
}

/// Marks `gap` finishes apart from before a range to after it, with a
/// routine start, a retracted mark and a derived build between two of
/// them, runs that failed a verification before they landed (their first
/// end is the failure, one written by a clock turned back), and runs of
/// goal 7's task 70 among the others. Returns the range of the middle
/// marks and the chain's marks.
pub(super) fn chain(units: i64, gap: usize) -> (Log, i64, i64, Vec<EventId>) {
    let mut log = Log::default();
    log.run(None, attributes("v0", 4, Some("0.45.0")), T0);
    log.start("v1", 4, T0 + MIN);
    let mut t = log.settled(units, T0 + 2 * MIN);
    let mut chain = Vec::new();
    for n in 0..6 {
        chain.push(log.mark(&format!("chain {n}"), None, t));
        if n == 2 {
            log.start("v1", 4, t + MIN / 2);
            let retracted = log.mark("retracted", None, t + MIN / 2 + 1);
            log.retract(retracted, t + MIN / 2 + 2);
            // A claim of a new build not finished: a derived mark and no
            // finish between the marks.
            log.claim(None, attributes("v2", 4, None), t + MIN / 2 + 3);
        }
        for k in 0..gap as i64 {
            t += 2 * MIN;
            if k == 0 {
                let (task, run, _) = log.claim(Some(70), attributes("v2", 4, None), t);
                log.end(
                    (task, &run),
                    EventKind::ValidationFinished,
                    json!({"status": "failed"}),
                    t - MIN / 2 * 3,
                );
                log.end(
                    (task, &run),
                    EventKind::RunIntegrated,
                    json!({}),
                    t + MIN / 2,
                );
            } else {
                log.run(Some(70 + k % 2), attributes("v2", 4, None), t);
            }
        }
        t += 3 * MIN;
    }
    let ms = |id: EventId| created_ms(&log.events[id.as_i64() as usize - 1]);
    let (from, to) = (ms(chain[2]) - 1, ms(chain[3]));
    log.settled(units, t + 10 * MIN);
    (log, from, to, chain)
}

/// The cohort is every mark and every run's first end: of every event's,
/// with `--goal` of its tasks, and the groups and the split it cuts are
/// every event's, with `min_samples` − 1 and `min_samples` finishes between
/// the marks. What it reads is the whole history of its kinds (the reason
/// is [`cohort`]'s doc comment).
#[test]
fn the_cohort_is_every_mark_and_every_runs_first_end() {
    let config = KpiConfig::default();
    let min = config.min_samples;
    let goals: HashMap<TaskId, Option<GoalId>> = [(TaskId::new(70), Some(GoalId::new(7)))].into();
    for gap in [min - 1, min] {
        let (log, from, to, chain) = chain(30, gap);
        let store = log.store();
        let whole_marks = marks::marks(&store.events, None, None);
        for goal in [None, Some(GoalId::new(7))] {
            let got = cohort(&store, goal, &goals).unwrap();
            let counts = |task: TaskId| {
                goal.is_none_or(|goal| goals.get(&task).copied().flatten() == Some(goal))
            };
            assert_eq!(got.marks, whole_marks);
            assert_eq!(got.finishes, whole_finishes(&store.events, counts));
        }
        let got = cohort(&store, None, &goals).unwrap();
        let whole_finishes = whole_finishes(&store.events, |_| true);
        let touching = |groups: Vec<Vec<Mark>>| -> Vec<usize> {
            groups
                .into_iter()
                .filter(|g| g.iter().any(|m| mark_ms(m) > from && mark_ms(m) <= to))
                .map(|g| g.len())
                .collect()
        };
        let whole = touching(compare::overlapping_groups(
            &whole_marks,
            &whole_finishes,
            min,
        ));
        assert_eq!(
            touching(compare::overlapping_groups(&got.marks, &got.finishes, min)),
            whole
        );
        if gap < min {
            assert_eq!(whole, [6], "one group past both ends");
        } else {
            assert_eq!(whole, [1, 1], "each its own");
        }
        let query = KpiQuery {
            compare: Some(CompareSpec::At(Cursor::Event(chain[2]))),
            ..KpiQuery::default()
        };
        let cursor = |c: Cursor| match c {
            Cursor::Time(ms) => Some(ms),
            Cursor::Event(id) => (id.as_i64() > 0).then(|| marks::cursor_millis(c, &store.events)),
        };
        let spec = query.compare.unwrap();
        let of = |marks: &[Mark], finishes: &[i64]| {
            compare::split_windows(marks, finishes, min, spec, &query, cursor).unwrap()
        };
        assert_eq!(
            of(&got.marks, &got.finishes),
            of(&whole_marks, &whole_finishes)
        );
    }
}
