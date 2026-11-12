//! `kpi` of the range [`plan`] gives against `kpi` of every event: the
//! answer (the periods' KPIs and marks, the targets, the forecast's
//! `forecast.*` and `details`, the comparison) is the same, and what is
//! read is the same with a small and a large settled history outside the
//! range.
//!
//! The read of a range is laid down as the ledger would give it: the rows
//! of the range ([`range_rows`]: a run's, task's or goal's every event
//! when it has one in the range, a queue event when it is in the range), the events
//! [`range_events`] and [`forecast_events`] select of the store, and for a
//! comparison the [`Cohort`]. A selected claim of a run whose row is not
//! read goes to [`Earlier::claims`], as a read from a time gives it (a
//! claim among the events would stand for a run). Two things are not this
//! module's reads and are given whole, uncounted: the supervisors' starts,
//! stops and heartbeats (a supervisor's life, which other reads name), and
//! the answers of the window's bounds that are not an event cursor's time
//! (the store's last event at or before a time, `Bound::LastAt`); an event
//! cursor's time is what the store's search ([`RangeReads::cursor_time`])
//! found. Which bounds are asked is declared of the ranged input with the
//! cohort ([`crate::domain::kpi::bounds_with`]), as the wiring will.
use super::forecast_and_cohort::chain;
use super::*;
use crate::domain::kpi::bounds_with;

/// The read's now: noon, 200 days after [`T0`].
const NOW_MS: i64 = T0 + 200 * DAY_MS + 12 * HOUR;
const NOW: i64 = NOW_MS / 1000;
/// Where the settled history outside every range ends.
const SETTLED_END: i64 = NOW_MS - 150 * DAY_MS;
const DAY: i64 = DAY_MS;

/// A history and the tasks' goals.
struct Case {
    /// The events the ledger's snapshot does not see: neither every
    /// event's answer nor the store's reads hold them.
    hidden: HashSet<EventId>,
    name: &'static str,
    log: Log,
    goals: HashMap<TaskId, Option<GoalId>>,
}

/// The rows of `range`: each run's, task's and goal's every event when
/// one of them is in it (a row is read whole when it has a record in the
/// range), every queue event in it.
fn range_rows(events: &[RunEvent], range: ReadRange) -> Vec<RunEvent> {
    #[derive(PartialEq, Eq, Hash)]
    enum Owner {
        Run(RunId),
        Task(TaskId),
        Goal(GoalId),
    }
    let inside = |e: &RunEvent| created_ms(e) > range.from_ms && created_ms(e) <= range.to_ms();
    let owner_of = |e: &RunEvent| match (&e.run_id, e.task_id, e.goal_id) {
        (Some(run), _, _) => Some(Owner::Run(run.clone())),
        (None, Some(task), _) => Some(Owner::Task(task)),
        (None, None, Some(goal)) => Some(Owner::Goal(goal)),
        (None, None, None) => None,
    };
    let read: HashSet<Owner> = events
        .iter()
        .filter(|e| inside(e))
        .filter_map(owner_of)
        .collect();
    events
        .iter()
        .filter(|e| owner_of(e).map_or_else(|| inside(e), |owner| read.contains(&owner)))
        .cloned()
        .collect()
}

/// The supervisors' lives, given whole (not this module's read).
fn lives(events: &[RunEvent]) -> Vec<RunEvent> {
    events
        .iter()
        .filter(|e| {
            matches!(
                e.kind.known(),
                Some(EventKind::SupervisorStarted | EventKind::SupervisorStopped)
            )
        })
        .cloned()
        .collect()
}

struct Fixtures {
    config: KpiConfig,
    changes: HashMap<TaskId, Option<crate::domain::TaskChange>>,
    heartbeats: HashMap<String, i64>,
    origins: HashMap<TaskId, crate::domain::DraftOrigin>,
}

impl Fixtures {
    fn new() -> Self {
        Self {
            config: KpiConfig::default(),
            changes: HashMap::new(),
            heartbeats: HashMap::new(),
            origins: HashMap::new(),
        }
    }

    fn input<'a>(
        &'a self,
        events: &'a [RunEvent],
        goals: &'a HashMap<TaskId, Option<GoalId>>,
        earlier: Option<&'a Earlier>,
        answers: Option<&'a Answers>,
    ) -> KpiInput<'a> {
        KpiInput {
            events,
            goals,
            changes: &self.changes,
            areas: None,
            heartbeats: &self.heartbeats,
            draft_origins: &self.origins,
            now: NOW,
            utc_offset_secs: 0,
            cores: Some(4),
            rust_toolchain: true,
            config: &self.config,
            host: None,
            ledger: None,
            rows: &[],
            post_landing: None,
            executions: None,
            earlier,
            answers,
        }
    }
}

/// Compares the answer of `query` read by its range with every event's,
/// and returns what the range's read read: its rows and the small reads
/// (a comparison's cohort, whole by design, apart).
fn check(case: &Case, query: &KpiQuery) -> usize {
    let fixtures = Fixtures::new();
    let visible: Vec<RunEvent> = case
        .log
        .events
        .iter()
        .filter(|e| !case.hidden.contains(&e.id))
        .cloned()
        .collect();
    let events = &visible;
    let whole = kpi(&fixtures.input(events, &case.goals, None, None), query)
        .map(|kpi| serde_json::to_value(kpi).unwrap());
    let store = Store::hiding(case.log.events.clone(), case.hidden.clone());
    let plan = plan(query, &fixtures.config, NOW, 0, &store, &case.goals)
        .unwrap_or_else(|undecided| panic!("{}: {query:?}: {undecided:?}", case.name));
    if plan.cohort.is_some() {
        store.read.set(0);
    }
    let (from, to) = (plan.range.from_ms, plan.range.to_ms());
    let rows = range_rows(events, plan.range);
    let mut selected = range_events(&store, from, to).unwrap();
    selected.extend(forecast_events(&store, from, to).unwrap());
    let read = rows.len() + store.read.get();
    let runs: HashSet<RunId> = rows.iter().filter_map(|e| e.run_id.clone()).collect();
    let (claims, rest): (Vec<RunEvent>, Vec<RunEvent>) = selected.into_iter().partition(|e| {
        e.kind == EventKind::RunClaimed && e.run_id.as_ref().is_some_and(|run| !runs.contains(run))
    });
    let merged = super::super::dedup(rows.into_iter().chain(rest).chain(lives(events)));
    let earlier = Earlier {
        claims: super::super::dedup(claims),
        ..Earlier::default()
    };
    let mut answers = Answers::default();
    let search = Store::hiding(case.log.events.clone(), case.hidden.clone());
    for _ in 0..2 {
        let probe = fixtures.input(&merged, &case.goals, Some(&earlier), Some(&answers));
        for bound in bounds_with(&probe, query, plan.cohort.as_ref()) {
            match bound {
                Bound::TimeThrough(id) => answers.set(bound, id, search.cursor_time(id).unwrap()),
                Bound::LastAt(ms) => answers.set(bound, Cursor::Time(ms).event_id(events), None),
                Bound::FirstIn(..) => panic!("a window from the first event has no range"),
            }
        }
    }
    let ranged = kpi_with(
        &fixtures.input(&merged, &case.goals, Some(&earlier), Some(&answers)),
        query,
        plan.cohort.as_ref(),
    )
    .map(|kpi| serde_json::to_value(kpi).unwrap());
    assert_eq!(ranged, whole, "{}: {query:?}", case.name);
    read
}

/// The case `build` makes with `units` settled units, checked for each of
/// `questions` with a small and a large settled history: the same answers,
/// and the same reads.
/// `build` gives the questions of its own history (an event cursor is an
/// id of it).
fn check_all(build: impl Fn(i64) -> (Case, Vec<KpiQuery>)) {
    let ((small, of_small), (large, of_large)) = (build(30), build(200));
    assert!(large.log.events.len() > small.log.events.len() + 400);
    assert_eq!(of_small.len(), of_large.len());
    for (a, b) in of_small.iter().zip(&of_large) {
        let (x, y) = (check(&small, a), check(&large, b));
        assert_eq!(x, y, "{}: {a:?}", small.name);
    }
}

fn day_two() -> KpiQuery {
    KpiQuery {
        period: Period::Day,
        last: 2,
        ..KpiQuery::default()
    }
}

/// The questions of the periods and windows, the event cursors among
/// `cursors` (ids in the range, one of them across a clock turned back).
fn questions(cursors: &[EventId]) -> Vec<KpiQuery> {
    let mut questions = vec![
        day_two(),
        KpiQuery {
            period: Period::Week,
            last: 2,
            ..KpiQuery::default()
        },
        KpiQuery {
            last: 4,
            by: vec!["parallel".parse().unwrap(), "dagq_version".parse().unwrap()],
            ..day_two()
        },
        KpiQuery {
            goal_id: Some(GoalId::new(7)),
            ..day_two()
        },
        KpiQuery {
            at: Some(Cursor::Time(NOW_MS - 3 * DAY)),
            ..day_two()
        },
        KpiQuery {
            period: Period::Week,
            last: 1,
            at: Some(Cursor::Time(NOW_MS - 3 * DAY)),
            ..KpiQuery::default()
        },
        KpiQuery {
            since: Some(Cursor::Time(NOW_MS - 4 * DAY)),
            until: Some(Cursor::Time(NOW_MS - DAY)),
            ..KpiQuery::default()
        },
        KpiQuery {
            since: Some(Cursor::Time(NOW_MS - 2 * DAY)),
            by: vec!["parallel".parse().unwrap()],
            goal_id: Some(GoalId::new(7)),
            ..KpiQuery::default()
        },
    ];
    for &cursor in cursors {
        questions.push(KpiQuery {
            at: Some(Cursor::Event(cursor)),
            ..day_two()
        });
        questions.push(KpiQuery {
            since: Some(Cursor::Event(cursor)),
            ..KpiQuery::default()
        });
        questions.push(KpiQuery {
            since: Some(Cursor::Time(NOW_MS - 6 * DAY)),
            until: Some(Cursor::Event(cursor)),
            ..KpiQuery::default()
        });
    }
    questions
}

/// Runs of goal 7's task 70 and of tasks of their own in the days before
/// now, a new build and a new Codex whose previous claims are far back,
/// starts announcing builds, marks (one written in the range but backdated
/// far before it, one written later and backdated into it, one by a CLI's
/// clock ahead of the DB's, a pruned stop recorded after its heartbeat),
/// retractions after the range and by a clock turned back, a claim and a
/// mark written by a clock turned back, and the settled history long
/// before. Returns the event cursors: a mark, and a mark with an earlier
/// id written later in time than it (the clock turned back).
fn marks_case(units: i64) -> (Case, Vec<EventId>) {
    let mut log = Log::default();
    log.run(
        None,
        attributes("v0", 4, Some("0.45.0")),
        SETTLED_END - 60 * DAY,
    );
    log.settled_until(units, SETTLED_END);
    let mut cursors = Vec::new();
    for day in (1..=9).rev() {
        let t = NOW_MS - day * DAY + 2 * HOUR;
        if day == 6 {
            log.start("v2", 3, t);
        }
        let version = if day <= 6 { "v2" } else { "v1" };
        let codex = (day == 5).then_some("0.46.0");
        log.run_for(Some(70), attributes(version, 3, codex), t + MIN, 10 * MIN);
        log.run_for(None, attributes(version, 3, None), t + 2 * HOUR, 30 * MIN);
        let mark = log.mark(&format!("day {day}"), None, t + 3 * HOUR);
        if day == 5 {
            log.mark(
                "backdated far out",
                Some(SETTLED_END - 10 * DAY),
                t + 4 * HOUR,
            );
            log.mark("cli ahead", Some(t + 5 * HOUR), t + 4 * HOUR + MIN);
            // Written back in time: an earlier id with a later time.
            log.mark("ahead", None, t + 9 * HOUR);
            cursors.push(log.mark("behind", None, t + 6 * HOUR));
            log.run(None, attributes("v2", 3, None), t - 2 * DAY);
            log.retract(mark, t - 3 * DAY);
        }
        if day == 2 {
            log.retract(EventId::new(mark.as_i64()), NOW_MS - HOUR);
            cursors.push(mark);
        }
    }
    log.mark(
        "backdated into the range",
        Some(NOW_MS - 4 * DAY),
        NOW_MS - 2 * HOUR,
    );
    log.queue(
        EventKind::SupervisorStopped,
        json!({"supervisor": "s", "outcome": "pruned", "dagq_version": "v2",
               "last_heartbeat_at": (NOW_MS - 3 * DAY) / 1000}),
        NOW_MS - HOUR,
    );
    let goals = [(TaskId::new(70), Some(GoalId::new(7)))].into();
    (
        Case {
            hidden: HashSet::new(),
            name: "marks",
            log,
            goals,
        },
        cursors,
    )
}

#[test]
fn the_periods_and_windows_answer_as_every_event_whatever_the_history() {
    check_all(|units| {
        let (case, cursors) = marks_case(units);
        (case, questions(&cursors))
    });
}

/// Task 900 reopened: finished long before the range, ready again, then
/// finished in it; a snapshot of it far before the range (still in the
/// task's open period) and marks between, one recorded after the range
/// and backdated before the snapshot; goal 50 closed in the range; task
/// 903 canceled in it; the settled history long before.
fn forecast_case(units: i64) -> Case {
    const TASK: i64 = 900;
    const GOAL: i64 = 50;
    const CANCELED: i64 = 903;
    let mut log = Log::default();
    log.settled_until(units, SETTLED_END);
    let opened = NOW_MS - 40 * DAY;
    log.push(
        None,
        Some(GOAL),
        None,
        EventKind::GoalCreated,
        json!({"title": "g"}),
        opened,
    );
    log.created(TASK, opened);
    log.created(CANCELED, opened);
    log.snapshot(&[TASK], &[], opened + MIN);
    log.task_status(TASK, "completed", opened + 2 * MIN);
    log.task_status(TASK, "ready", opened + 3 * MIN);
    log.snapshot(&[TASK, CANCELED], &[GOAL], opened + 4 * MIN);
    log.snapshot_by_time(&[TASK], opened + 5 * MIN);
    for day in 0..30 {
        let t = opened + (day + 1) * DAY;
        log.mark(&format!("between {day}"), None, t);
        log.run(
            None,
            attributes(if day < 20 { "v1" } else { "v2" }, 4, None),
            t + HOUR,
        );
        if day == 25 {
            log.priority(TASK, t + 2 * HOUR);
        }
    }
    log.task_status(TASK, "completed", NOW_MS - 3 * DAY);
    log.goal_closed(GOAL, NOW_MS - 2 * DAY);
    log.task_status(CANCELED, "canceled", NOW_MS - DAY);
    log.mark("backdated", Some(opened - DAY), NOW_MS - HOUR);
    let goals = [
        (TaskId::new(TASK), Some(GoalId::new(7))),
        (TaskId::new(CANCELED), Some(GoalId::new(7))),
    ]
    .into();
    Case {
        hidden: HashSet::new(),
        name: "forecast",
        log,
        goals,
    }
}

/// The forecast's KPIs (the `forecast.*` layers and `details` with
/// `with_marks` and `marks_between`) are every event's; what the range
/// reads does not grow with the history before the targets opened (the
/// second step grows with their open periods instead, the doc comment of
/// [`forecast_events`]).
#[test]
fn the_forecast_answers_as_every_event_whatever_the_history() {
    let case = forecast_case(30);
    let whole = kpi(
        &Fixtures::new().input(&case.log.events, &case.goals, None, None),
        &KpiQuery {
            last: 4,
            ..day_two()
        },
    )
    .unwrap();
    let details = serde_json::to_value(&whole.periods).unwrap();
    let text = details.to_string();
    assert!(text.contains("\"with_marks\":1"), "{text}");
    check_all(|units| (forecast_case(units), questions(&[])));
}

/// Marks a few finishes apart before, in and after a comparison's
/// windows ([`chain`]) at a fixed distance before now.
fn compare_case(units: i64, gap: usize) -> (Case, Vec<EventId>) {
    let (mut log, _, _, marks) = chain(0, gap);
    // Move the chain to just before now, the settled history before it.
    let first = created_ms(&log.events[0]);
    let shift = NOW_MS - 12 * DAY - first;
    for event in &mut log.events {
        event.created_at = marks::utc_text(created_ms(event) + shift);
    }
    let mut history = Log::default();
    history.settled_until(units, SETTLED_END);
    // Enough finishes of goal 7's task after the settled history's first
    // start (a mark that is not routine) that no group of the chain reaches
    // back to it, with `--goal` too.
    for n in 0..KpiConfig::default().min_samples as i64 {
        history.run(Some(70), attributes("v1", 4, None), SETTLED_END + n * HOUR);
    }
    let offset = history.events.len() as i64;
    for mut event in log.events {
        event.id = EventId::new(event.id.as_i64() + offset);
        if event.kind == EventKind::MarkRetracted {
            event.payload["mark"] = json!(event.payload["mark"].as_i64().unwrap() + offset);
        }
        history.events.push(event);
    }
    let goals = [(TaskId::new(70), Some(GoalId::new(7)))].into();
    let marks = marks
        .into_iter()
        .map(|id| EventId::new(id.as_i64() + offset))
        .collect();
    (
        Case {
            hidden: HashSet::new(),
            name: "compare",
            log: history,
            goals,
        },
        marks,
    )
}

/// A comparison at a mark (its group reaching before and after the window
/// with `min_samples` − 1 finishes between marks, alone with
/// `min_samples`), at a time, and of two windows, with and without
/// `--goal`, is every event's.
#[test]
fn a_comparison_answers_as_every_event() {
    let min = KpiConfig::default().min_samples;
    for gap in [min - 1, min] {
        check_all(|units| {
            let (case, marks) = compare_case(units, gap);
            let questions: Vec<KpiQuery> = [
                CompareSpec::At(Cursor::Event(marks[2])),
                CompareSpec::At(Cursor::Time(NOW_MS - 11 * DAY)),
                CompareSpec::Windows([
                    (Cursor::Time(NOW_MS - 13 * DAY), Cursor::Event(marks[2])),
                    (Cursor::Event(marks[3]), Cursor::Time(NOW_MS - 10 * DAY)),
                ]),
            ]
            .into_iter()
            .flat_map(|spec| {
                [None, Some(GoalId::new(7))].map(|goal_id| KpiQuery {
                    compare: Some(spec),
                    goal_id,
                    window_days: 1,
                    ..day_two()
                })
            })
            .collect();
            (case, questions)
        });
    }
}

/// The ids of the events of `log` that `keep` picks, and every event of the
/// runs whose claim it picks (a run on its way the snapshot does not see).
fn hide(log: &Log, keep: impl Fn(&RunEvent) -> bool) -> HashSet<EventId> {
    let runs: HashSet<&RunId> = log
        .events
        .iter()
        .filter(|e| keep(e) && e.kind == EventKind::RunClaimed)
        .filter_map(|e| e.run_id.as_ref())
        .collect();
    log.events
        .iter()
        .filter(|e| keep(e) || e.run_id.as_ref().is_some_and(|run| runs.contains(run)))
        .map(|e| e.id)
        .collect()
}

/// Events of the store the ledger's snapshot does not see: a run of goal
/// 7's task in the range (its claim and finish), a mark of the range, the
/// event later in time before a cursor (the cursor's aggregate time is an
/// invisible event's, so its walk takes the visible one), a target's
/// finish in the range and a snapshot. The small reads return the visible
/// ones alone, and every answer is that of the visible events, whatever
/// the settled history.
#[test]
fn invisible_events_are_neither_read_nor_answered() {
    check_all(|units| {
        let (mut case, cursors) = marks_case(units);
        let claim_of_day_3 = NOW_MS - 3 * DAY + 2 * HOUR + MIN;
        case.hidden = hide(&case.log, |e| {
            (e.kind == EventKind::RunClaimed && created_ms(e) == claim_of_day_3)
                || e.payload["label"] == "day 4"
                || e.payload["label"] == "ahead"
        });
        assert!(case.hidden.len() >= 4, "a claim, its end, two marks");
        case.name = "marks, some invisible";
        (case, questions(&cursors))
    });
    check_all(|units| {
        let mut case = forecast_case(units);
        let snapshot = case
            .log
            .events
            .iter()
            .filter(|e| {
                e.kind == EventKind::ForecastRecorded
                    && e.payload["tasks"].as_array().is_some_and(|t| !t.is_empty())
            })
            .map(|e| e.id)
            .next_back();
        case.hidden = hide(&case.log, |e| {
            e.kind == EventKind::GoalClosed || Some(e.id) == snapshot
        });
        assert_eq!(case.hidden.len(), 2);
        case.name = "forecast, some invisible";
        (case, questions(&[]))
    });
}
