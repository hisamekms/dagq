use super::*;
use crate::domain::{
    GoalId,
    stats::{LiveSnapshot, StatsQuery, outlier_thresholds, stats},
};

/// A start of the records, in unix milliseconds.
const T0: i64 = 1_790_000_000_000;
const SEC: i64 = 1000;
const MIN: i64 = 60 * SEC;

/// The events of a queue as the tests write them, ascending id.
#[derive(Default)]
struct Log {
    events: Vec<RunEvent>,
}

impl Log {
    fn push(&mut self, at_ms: i64, kind: &str, payload: Value, run: Option<(&RunId, TaskId)>) {
        let id = EventId::new(self.events.len() as i64 + 1);
        self.events.push(RunEvent {
            id,
            task_id: run.map(|(_, task)| task),
            goal_id: None,
            run_id: run.map(|(run, _)| run.clone()),
            kind: kind.to_owned(),
            payload,
            created_at: crate::domain::marks::utc_text(at_ms),
            actor: None,
        });
    }

    fn write(&mut self, at_ms: i64, records: Vec<(EventKind, Value)>) -> Vec<&'static str> {
        let kinds = records.iter().map(|(kind, _)| kind.as_str()).collect();
        for (kind, payload) in records {
            self.push(at_ms, kind.as_str(), payload, None);
        }
        kinds
    }

    /// A run claimed with its session started at `at_ms`.
    fn claim(&mut self, at_ms: i64, run: &RunId, task: TaskId) {
        self.push(at_ms, "run_claimed", json!({}), Some((run, task)));
        self.push(at_ms, "agent_started", json!({}), Some((run, task)));
    }

    /// `recorder` observes `inputs` at `at_ms` for `supervisor`, judging
    /// them on the events so far as the supervisor's pass does; the kinds
    /// it wrote.
    fn observe(
        &mut self,
        recorder: &mut LiveRecorder,
        supervisor: &str,
        at_ms: i64,
        inputs: &LiveInputs,
    ) -> Vec<&'static str> {
        let alerts = judge(&self.events, at_ms / 1000, inputs).keys();
        let records = recorder.observe(
            supervisor,
            at_ms,
            Observation::Seen {
                inputs: Box::new(inputs.clone()),
                alerts,
            },
        );
        self.write(at_ms, records)
    }

    fn fail(
        &mut self,
        recorder: &mut LiveRecorder,
        supervisor: &str,
        at_ms: i64,
    ) -> Vec<&'static str> {
        let records = recorder.observe(
            supervisor,
            at_ms,
            Observation::Failed {
                reason: "the run directory could not be read".into(),
            },
        );
        self.write(at_ms, records)
    }

    fn payloads(&self, kind: EventKind) -> Vec<&Value> {
        self.events
            .iter()
            .filter(|event| event.kind == kind.as_str())
            .map(|event| &event.payload)
            .collect()
    }
}

fn run_id(n: u8) -> RunId {
    RunId::new(format!("{n}aa21145-c873-4cec-aee3-ee7f07f52e4a")).unwrap()
}

fn running(n: u8, idle: Option<i64>) -> LiveRun {
    LiveRun {
        run_id: run_id(n),
        task_id: TaskId::new(i64::from(n)),
        status: RunStatus::Running,
        workspace_id: None,
        idle: idle.map(|at| (at, Vec::new())),
        receipt: None,
        input: None,
        background_since: None,
        background_alive: None,
    }
}

fn config(idle_secs: i64) -> StallConfigReport {
    let mut config = StallConfig::default();
    config.set("idle_without_receipt_secs", idle_secs);
    config.set("background_alert_secs", 30 * 60);
    StallConfigReport {
        config,
        source: "supervisor",
    }
}

fn inputs(runs: Vec<LiveRun>) -> LiveInputs {
    LiveInputs {
        slots: SlotSnapshot {
            free_slots: 1,
            candidates: 0,
            ready: 0,
        },
        config: config(600),
        runs,
        outliers: HashMap::new(),
        positions: HashMap::new(),
    }
}

/// What `stats` lists of the three judgments on `inputs` at `now`
/// (unix seconds): the slot alerts, the running alerts and the workspace
/// check.
fn stats_of(
    events: &[RunEvent],
    goals: &HashMap<TaskId, Option<GoalId>>,
    now: i64,
    inputs: &LiveInputs,
) -> (Vec<Alert>, Vec<RunningAlert>, WorkspaceCheck) {
    let live = LiveSnapshot {
        runs: inputs.runs.clone(),
        config: inputs.config.clone(),
        ..LiveSnapshot::default()
    };
    let result = stats(
        events,
        goals,
        now,
        inputs.slots,
        &StatsQuery::default(),
        &live,
    );
    let slots = result
        .alerts
        .into_iter()
        .filter(|alert| ["claim_held", "claim_deferred", "idle_slots"].contains(&alert.kind))
        .collect();
    (slots, result.running_alerts, result.workspace_check)
}

/// The fold at `at_ms` is observed and lists what `stats` lists at that
/// time on `inputs`, every field and in order.
fn assert_folds_as_stats(
    log: &Log,
    goals: &HashMap<TaskId, Option<GoalId>>,
    at_ms: i64,
    inputs: &LiveInputs,
) -> FoldedJudgments {
    let folded = fold(&log.events, at_ms / 1000);
    assert!(
        matches!(
            folded.running_alerts.state,
            ObservationState::Observed { .. }
        ),
        "{:?}",
        folded.running_alerts.state
    );
    // The queue as it was at the window's end.
    let events: Vec<RunEvent> = log
        .events
        .iter()
        .filter(|event| timestamp_millis(&event.created_at).is_some_and(|at| at <= at_ms))
        .cloned()
        .collect();
    let (slots, running, workspace) = stats_of(&events, goals, at_ms / 1000, inputs);
    assert_eq!(folded.slot_alerts.value.as_ref(), Some(&slots));
    assert_eq!(folded.running_alerts.value.as_ref(), Some(&running));
    assert_eq!(folded.workspace_check.value.as_ref(), Some(&workspace));
    folded
}

fn missing_reason(folded: &FoldedJudgments) -> Option<Missing> {
    for state in [
        &folded.slot_alerts.state,
        &folded.running_alerts.state,
        &folded.workspace_check.state,
    ] {
        assert_eq!(state, &folded.running_alerts.state);
    }
    match folded.running_alerts.state {
        ObservationState::Missing { reason, .. } => {
            assert_eq!(folded.running_alerts.value, None);
            Some(reason)
        }
        ObservationState::Observed { .. } => None,
    }
}

/// The first observation is a baseline with the inputs of the run that
/// is not an alert yet (its idle marker under the threshold) and no alert
/// open; an observation where only the time went on records nothing, and
/// the reach comes once its interval passed. With no new record, the fold
/// within the grace after that reach is `警告なし`, and once the window's
/// end alone takes the idle past its threshold it is the
/// `idle_without_receipt` `stats` lists, every field.
#[test]
fn a_baseline_without_an_alert_is_judged_again_at_the_windows_end() {
    let mut log = Log::default();
    let run = run_id(1);
    log.claim(T0, &run, TaskId::new(1));
    let live = inputs(vec![running(1, Some(T0 + MIN))]);
    let mut recorder = LiveRecorder::default();
    assert_eq!(
        log.observe(&mut recorder, "s", T0 + 2 * MIN, &live),
        ["live_alert_baseline"]
    );
    let baseline = log.payloads(EventKind::LiveAlertBaseline)[0];
    assert_eq!(baseline["version"], 1);
    assert_eq!(baseline["open"], json!([]));
    assert_eq!(baseline["runs"][0]["idle"][0], T0 + MIN);
    assert!(
        log.observe(&mut recorder, "s", T0 + 3 * MIN, &live)
            .is_empty()
    );
    assert_eq!(
        log.observe(&mut recorder, "s", T0 + 7 * MIN, &live),
        ["live_alert_reached"]
    );
    assert_eq!(
        log.payloads(EventKind::LiveAlertReached)[0],
        &json!({"supervisor": "s", "ok": true, "version": 1})
    );
    let goals = HashMap::new();
    let quiet = assert_folds_as_stats(&log, &goals, T0 + 8 * MIN, &live);
    assert_eq!(quiet.running_alerts.value, Some(Vec::new()));
    assert_eq!(missing_reason(&quiet), None);
    // 11 minutes idle, past the 10 minutes, with nothing recorded since.
    let alert = assert_folds_as_stats(&log, &goals, T0 + 12 * MIN, &live);
    let alerts = alert.running_alerts.value.unwrap();
    assert_eq!(alerts.len(), 1);
    assert_eq!(alerts[0].kind, "idle_without_receipt");
    assert_eq!(alerts[0].value, Some(11 * 60));
    assert_eq!(
        alert.running_alerts.state,
        ObservationState::Observed {
            supervisor: "s".into(),
            version: 1,
            last_ok_ms: T0 + 7 * MIN,
        }
    );
}

/// `long_background` and `running_outlier` come from the window's end too:
/// the background task's start and the run's outlier threshold are
/// inputs, the time it has run is not.
#[test]
fn the_background_and_the_outlier_alerts_come_from_the_windows_end_too() {
    let mut log = Log::default();
    let goal = Some(GoalId::new(7));
    let mut goals = HashMap::new();
    // Two finished runs of the goal worked 10 and 20 minutes: the median is
    // 15, and a run over 30 minutes is an outlier.
    for (n, work) in [(2u8, 10), (3, 20)] {
        let run = run_id(n);
        let task = TaskId::new(i64::from(n));
        goals.insert(task, goal);
        log.claim(T0 - HOUR, &run, task);
        log.push(
            T0 - HOUR + work * MIN,
            "receipt_observed",
            json!({}),
            Some((&run, task)),
        );
        log.push(
            T0 - HOUR + work * MIN + MIN,
            "run_integrated",
            json!({}),
            Some((&run, task)),
        );
    }
    let run = run_id(1);
    goals.insert(TaskId::new(1), goal);
    log.claim(T0, &run, TaskId::new(1));
    let mut busy = running(1, Some(T0 + 5 * MIN));
    let task = BackgroundTask {
        id: "b1".into(),
        description: "tests".into(),
        command: "cargo test".into(),
    };
    busy.idle = Some((T0 + 5 * MIN, vec![task]));
    busy.background_since = Some(T0 + MIN);
    busy.input = Some(T0 + 6 * MIN);
    let mut live = inputs(vec![busy]);
    live.outliers = outlier_thresholds(&log.events, &goals, &live.runs);
    assert_eq!(live.outliers.get(&run), Some(&(30 * 60)));
    let mut recorder = LiveRecorder::default();
    log.observe(&mut recorder, "s", T0 + 10 * MIN, &live);
    log.observe(&mut recorder, "s", T0 + 15 * MIN, &live);
    let quiet = assert_folds_as_stats(&log, &goals, T0 + 20 * MIN, &live);
    assert_eq!(quiet.running_alerts.value, Some(Vec::new()));
    for minute in [20, 25, 30] {
        log.observe(&mut recorder, "s", T0 + minute * MIN, &live);
    }
    assert!(log.payloads(EventKind::LiveAlertStarted).is_empty());
    // A second past 31 minutes since the claim, and past 30 minutes since
    // the background task started.
    let late = T0 + 31 * MIN;
    let folded = assert_folds_as_stats(&log, &goals, late + SEC, &live);
    let kinds: Vec<&str> = folded
        .running_alerts
        .value
        .unwrap()
        .iter()
        .map(|alert| alert.kind)
        .collect();
    assert_eq!(kinds, ["long_background", "running_outlier"]);
}

const HOUR: i64 = 60 * MIN;

/// The keys of an alert: it starts, lasts with no record, changes its
/// reason (an end and a start) and ends; only the set of keys changing
/// is written, with no input changed.
#[test]
fn an_alert_is_written_as_it_starts_changes_its_key_and_ends() {
    let live = inputs(Vec::new());
    let key = |reason: &str| AlertKey {
        judgment: "running".into(),
        kind: "workspace_mismatch".into(),
        run_id: Some(run_id(1).as_str().to_owned()),
        reason: Some(reason.into()),
    };
    let mut log = Log::default();
    let mut recorder = LiveRecorder::default();
    let mut see = |log: &mut Log, at: i64, keys: &[AlertKey]| {
        let records = recorder.observe(
            "s",
            at,
            Observation::Seen {
                inputs: Box::new(live.clone()),
                alerts: keys.iter().cloned().collect(),
            },
        );
        log.write(at, records)
    };
    assert_eq!(see(&mut log, T0, &[]), ["live_alert_baseline"]);
    assert_eq!(
        see(&mut log, T0 + MIN, &[key("run_without_wrapper")]),
        ["live_alert_started"]
    );
    assert!(see(&mut log, T0 + 2 * MIN, &[key("run_without_wrapper")]).is_empty());
    assert_eq!(
        see(&mut log, T0 + 3 * MIN, &[key("run_without_workspace")]),
        ["live_alert_ended", "live_alert_started"]
    );
    assert_eq!(see(&mut log, T0 + 4 * MIN, &[]), ["live_alert_ended"]);
    let ended = log.payloads(EventKind::LiveAlertEnded);
    assert_eq!(
        ended[0],
        &json!({"supervisor": "s", "judgment": "running", "kind": "workspace_mismatch",
                "run_id": run_id(1).as_str(), "reason": "run_without_wrapper"})
    );
    assert_eq!(ended[1]["reason"], "run_without_workspace");
}

/// The same idle alert while its idle marker, its background tasks and its
/// threshold change: each change is an input's record with the next
/// version and no start or end, and the fold after each is what `stats`
/// lists, `value`, `threshold` and `background_tasks` too.
#[test]
fn the_same_alert_with_new_inputs_writes_only_the_inputs() {
    let mut log = Log::default();
    let run = run_id(1);
    log.claim(T0, &run, TaskId::new(1));
    log.push(
        T0 + 22 * MIN + 15 * SEC,
        "stall_nudged",
        json!({}),
        Some((&run, TaskId::new(1))),
    );
    let mut live = inputs(vec![running(1, Some(T0 + MIN))]);
    let goals = HashMap::new();
    let mut recorder = LiveRecorder::default();
    assert_eq!(
        log.observe(&mut recorder, "s", T0 + 20 * MIN, &live),
        ["live_alert_baseline"]
    );
    let opened = log.payloads(EventKind::LiveAlertBaseline)[0]["open"].clone();
    assert_eq!(opened[0]["kind"], "idle_without_receipt");
    let folded = assert_folds_as_stats(&log, &goals, T0 + 20 * MIN + 30 * SEC, &live);
    assert_eq!(folded.running_alerts.value.unwrap()[0].nudged, Some(false));

    // A later idle marker, still idle past the threshold.
    live.runs[0].idle = Some((T0 + 3 * MIN, Vec::new()));
    assert_eq!(
        log.observe(&mut recorder, "s", T0 + 21 * MIN, &live),
        ["live_alert_input_changed"]
    );
    assert_folds_as_stats(&log, &goals, T0 + 21 * MIN + 30 * SEC, &live);
    // Its background tasks.
    live.runs[0].idle = Some((
        T0 + 3 * MIN,
        vec![BackgroundTask {
            id: "b1".into(),
            description: "build".into(),
            command: "cargo build".into(),
        }],
    ));
    assert_eq!(
        log.observe(&mut recorder, "s", T0 + 22 * MIN, &live),
        ["live_alert_input_changed"]
    );
    assert_folds_as_stats(&log, &goals, T0 + 22 * MIN + 30 * SEC, &live);
    // And its threshold.
    live.config = config(900);
    assert_eq!(
        log.observe(&mut recorder, "s", T0 + 23 * MIN, &live),
        ["live_alert_input_changed"]
    );
    let folded = assert_folds_as_stats(&log, &goals, T0 + 28 * MIN, &live);
    let alert = &folded.running_alerts.value.unwrap()[0];
    assert_eq!(
        (alert.value, alert.threshold, alert.nudged),
        (Some(25 * 60), Some(900), Some(true))
    );
    assert_eq!(alert.background_tasks.len(), 1);
    let versions: Vec<&Value> = log
        .payloads(EventKind::LiveAlertInputChanged)
        .iter()
        .map(|payload| &payload["version"])
        .collect();
    assert_eq!(versions, [&json!(2), &json!(3), &json!(4)]);
    assert!(log.payloads(EventKind::LiveAlertStarted).is_empty());
    assert!(log.payloads(EventKind::LiveAlertEnded).is_empty());
}

/// A failure is written at once, not again while it lasts within the
/// interval, and the return to success at once too. Failing past the
/// grace while the supervisor's evidence of life goes on is `観測の失敗`;
/// observing as before past the grace, with no alert or with the same
/// one, is not missing.
#[test]
fn a_failure_and_its_return_are_written_at_once() {
    let mut log = Log::default();
    let run = run_id(1);
    log.claim(T0, &run, TaskId::new(1));
    let live = inputs(vec![running(1, Some(T0 + MIN))]);
    let mut recorder = LiveRecorder::default();
    log.observe(&mut recorder, "s", T0 + 2 * MIN, &live);
    assert_eq!(
        log.fail(&mut recorder, "s", T0 + 3 * MIN),
        ["live_alert_reached"]
    );
    assert_eq!(
        log.payloads(EventKind::LiveAlertReached)[0]["ok"],
        json!(false)
    );
    assert!(log.fail(&mut recorder, "s", T0 + 4 * MIN).is_empty());
    assert_eq!(
        log.observe(&mut recorder, "s", T0 + 5 * MIN, &live),
        ["live_alert_reached"]
    );
    assert_eq!(
        log.payloads(EventKind::LiveAlertReached)[1],
        &json!({"supervisor": "s", "ok": true, "version": 1})
    );

    // Observing on for an hour: no alert, then the same alert, never missing.
    let goals = HashMap::new();
    for minute in (10..=60).step_by(5) {
        log.observe(&mut recorder, "s", T0 + minute * MIN, &live);
        log.push(
            T0 + minute * MIN,
            "supervisor_alive",
            json!({"supervisor": "s"}),
            None,
        );
    }
    let open = assert_folds_as_stats(&log, &goals, T0 + 61 * MIN, &live);
    assert_eq!(open.running_alerts.value.unwrap().len(), 1);
    assert_eq!(log.payloads(EventKind::LiveAlertStarted).len(), 1);
    let mut quiet = live.clone();
    quiet.runs[0].receipt = Some(T0 + 62 * MIN);
    for minute in (62..=100).step_by(5) {
        log.observe(&mut recorder, "s", T0 + minute * MIN, &quiet);
    }
    let folded = assert_folds_as_stats(&log, &goals, T0 + 101 * MIN, &quiet);
    assert_eq!(folded.running_alerts.value, Some(Vec::new()));

    // Failing for 20 minutes while alive.
    for minute in (102..=122).step_by(5) {
        log.fail(&mut recorder, "s", T0 + minute * MIN);
        log.push(
            T0 + minute * MIN,
            "supervisor_alive",
            json!({"supervisor": "s"}),
            None,
        );
    }
    let failed = fold(&log.events, (T0 + 122 * MIN) / 1000);
    assert_eq!(missing_reason(&failed), Some(Missing::ObservationFailed));
}

/// Each supervisor writes its own stream of the whole queue; the fold takes
/// the one with the latest successful observation and does not add them,
/// so an alert is listed once. A lease that moves changes no input and no
/// key: nothing is written. Once the latest supervisor stopped past the
/// grace, the other's stream is taken; once both did, `supervisor の停止`.
#[test]
fn the_latest_stream_is_taken_and_streams_are_not_added() {
    let mut log = Log::default();
    let goals = HashMap::new();
    for n in 1..=2 {
        log.claim(T0, &run_id(n), TaskId::new(i64::from(n)));
    }
    // Run 1 idle past the threshold, run 2 without a marker.
    let live = LiveInputs {
        slots: SlotSnapshot {
            free_slots: 2,
            candidates: 0,
            ready: 3,
        },
        ..inputs(vec![running(1, Some(T0 + MIN)), running(2, None)])
    };
    let (mut a, mut b) = (LiveRecorder::default(), LiveRecorder::default());
    log.observe(&mut a, "a", T0 + 20 * MIN, &live);
    log.observe(&mut b, "b", T0 + 21 * MIN, &live);
    let folded = assert_folds_as_stats(&log, &goals, T0 + 22 * MIN, &live);
    assert_eq!(folded.running_alerts.value.as_ref().unwrap().len(), 1);
    assert_eq!(
        folded.slot_alerts.value.as_ref().unwrap()[0].kind,
        "idle_slots"
    );
    assert!(matches!(
        &folded.running_alerts.state,
        ObservationState::Observed { supervisor, .. } if supervisor == "b"
    ));
    // Run 2's lease moves from a to b: the same inputs, no record.
    assert!(log.observe(&mut a, "a", T0 + 23 * MIN, &live).is_empty());
    assert!(log.observe(&mut b, "b", T0 + 23 * MIN, &live).is_empty());

    // a reaches on; b stopped at 30.
    log.push(
        T0 + 30 * MIN,
        "supervisor_stopped",
        json!({"supervisor": "b"}),
        None,
    );
    for minute in [25, 30, 35, 40, 45, 50, 55] {
        log.observe(&mut a, "a", T0 + minute * MIN, &live);
    }
    let folded = assert_folds_as_stats(&log, &goals, T0 + 56 * MIN, &live);
    assert!(matches!(
        &folded.running_alerts.state,
        ObservationState::Observed { supervisor, .. } if supervisor == "a"
    ));
    log.push(
        T0 + 57 * MIN,
        "supervisor_stopped",
        json!({"supervisor": "a"}),
        None,
    );
    let stopped = fold(&log.events, (T0 + 80 * MIN) / 1000);
    assert_eq!(missing_reason(&stopped), Some(Missing::SupervisorStopped));
}

/// A reach that confirmed a version its stream's records do not reach is
/// `記録の欠け`, even within the grace, and is not made up for with the
/// older version; another stream's observation is taken in its place.
#[test]
fn a_reach_past_the_records_is_a_gap() {
    let mut log = Log::default();
    let run = run_id(1);
    log.claim(T0, &run, TaskId::new(1));
    let mut live = inputs(vec![running(1, Some(T0 + MIN))]);
    let mut recorder = LiveRecorder::default();
    log.observe(&mut recorder, "s", T0 + 2 * MIN, &live);
    // An input changed but its record was lost.
    live.runs[0].idle = Some((T0 + 3 * MIN, Vec::new()));
    let records = recorder.observe(
        "s",
        T0 + 4 * MIN,
        Observation::Seen {
            inputs: Box::new(live.clone()),
            alerts: BTreeSet::new(),
        },
    );
    assert_eq!(records.len(), 1);
    log.observe(&mut recorder, "s", T0 + 8 * MIN, &live);
    assert_eq!(log.payloads(EventKind::LiveAlertReached)[0]["version"], 2);
    let gap = fold(&log.events, (T0 + 9 * MIN) / 1000);
    assert_eq!(missing_reason(&gap), Some(Missing::RecordGap));

    let mut other = LiveRecorder::default();
    log.observe(&mut other, "t", T0 + 9 * MIN, &live);
    let goals = HashMap::new();
    assert_folds_as_stats(&log, &goals, T0 + 10 * MIN, &live);
}

/// Before any baseline, and with none at all, nothing is observed: the
/// `観測が無い` of `NoBaseline`, apart from `警告なし`. A stream silent
/// past the grace with its supervisor alive is `Stale`.
#[test]
fn before_the_baseline_and_past_the_grace_nothing_is_observed() {
    assert_eq!(
        missing_reason(&fold(&[], T0 / 1000)),
        Some(Missing::NoBaseline)
    );
    let mut log = Log::default();
    let live = inputs(Vec::new());
    let mut recorder = LiveRecorder::default();
    log.fail(&mut recorder, "s", T0);
    log.observe(&mut recorder, "s", T0 + 10 * MIN, &live);
    let before = fold(&log.events, (T0 + 5 * MIN) / 1000);
    assert_eq!(missing_reason(&before), Some(Missing::NoBaseline));
    let after = fold(&log.events, (T0 + 11 * MIN) / 1000);
    assert_eq!(missing_reason(&after), None);
    assert_eq!(after.running_alerts.value, Some(Vec::new()));
    for minute in [20, 25, 30, 35] {
        log.push(
            T0 + minute * MIN,
            "supervisor_alive",
            json!({"supervisor": "s"}),
            None,
        );
    }
    let stale = fold(&log.events, (T0 + 10 * MIN + GRACE_SECS * SEC + SEC) / 1000);
    assert_eq!(missing_reason(&stale), Some(Missing::Stale));
    let within = fold(&log.events, (T0 + 10 * MIN + GRACE_SECS * SEC) / 1000);
    assert_eq!(missing_reason(&within), None);
}

/// A new process of the same supervisor writes a baseline again, and the
/// fold starts the stream over from it: a run gone since is not listed.
#[test]
fn a_restart_writes_a_new_baseline_that_replaces_the_stream() {
    let mut log = Log::default();
    for n in 1..=2 {
        log.claim(T0, &run_id(n), TaskId::new(i64::from(n)));
    }
    let both = inputs(vec![running(1, Some(T0 + MIN)), running(2, Some(T0 + MIN))]);
    let mut recorder = LiveRecorder::default();
    log.observe(&mut recorder, "s", T0 + 20 * MIN, &both);
    let one = inputs(vec![running(2, Some(T0 + MIN))]);
    let mut again = LiveRecorder::default();
    assert_eq!(
        log.observe(&mut again, "s", T0 + 21 * MIN, &one),
        ["live_alert_baseline"]
    );
    let goals = HashMap::new();
    let folded = assert_folds_as_stats(&log, &goals, T0 + 22 * MIN, &one);
    assert_eq!(folded.running_alerts.value.unwrap().len(), 1);
    // A run gone is an input's record of null, and one that comes is
    // appended in the queue's order.
    let three = inputs(vec![running(2, Some(T0 + MIN)), running(3, None)]);
    log.claim(T0 + 22 * MIN, &run_id(3), TaskId::new(3));
    log.observe(&mut again, "s", T0 + 23 * MIN, &three);
    assert_folds_as_stats(&log, &goals, T0 + 24 * MIN, &three);
    let none = inputs(vec![running(3, None)]);
    log.observe(&mut again, "s", T0 + 25 * MIN, &none);
    let changed = log.payloads(EventKind::LiveAlertInputChanged);
    assert_eq!(changed.last().unwrap()["value"], Value::Null);
    assert_folds_as_stats(&log, &goals, T0 + 26 * MIN, &none);
}

/// A run that comes back unfinished (a triage's resume of an older run)
/// is folded at its position in the queue's list of runs, as `stats`
/// lists it, not after the runs recorded before it.
#[test]
fn a_run_that_comes_back_is_folded_in_the_queues_order() {
    let mut log = Log::default();
    for n in 1..=3 {
        log.claim(T0, &run_id(n), TaskId::new(i64::from(n)));
    }
    let with_positions = |runs: Vec<LiveRun>| {
        let mut live = inputs(runs);
        live.positions = live
            .runs
            .iter()
            .map(|run| (run.run_id.clone(), run.task_id.as_i64() as usize))
            .collect();
        live
    };
    let later = with_positions(vec![running(2, Some(T0 + MIN)), running(3, Some(T0 + MIN))]);
    let mut recorder = LiveRecorder::default();
    log.observe(&mut recorder, "s", T0 + 20 * MIN, &later);
    let back = with_positions(vec![
        running(1, Some(T0 + MIN)),
        running(2, Some(T0 + MIN)),
        running(3, Some(T0 + MIN)),
    ]);
    assert_eq!(
        log.observe(&mut recorder, "s", T0 + 21 * MIN, &back),
        ["live_alert_input_changed", "live_alert_started"]
    );
    let goals = HashMap::new();
    let folded = assert_folds_as_stats(&log, &goals, T0 + 22 * MIN, &back);
    let order: Vec<RunId> = folded
        .running_alerts
        .value
        .unwrap()
        .into_iter()
        .filter_map(|alert| alert.run_id)
        .collect();
    assert_eq!(order, [run_id(1), run_id(2), run_id(3)]);
}

/// A supervisor that just goes silent, with no `supervisor_stopped`: its
/// records and its `supervisor_alive` stop. Within the grace its stream is
/// still taken; past it, once `supervisor_life_end` reads it as silent, the
/// fold is `supervisor の停止`. The same stream with `supervisor_alive`
/// going on is `Stale`, and with its observation failing meanwhile
/// `ObservationFailed`: the three are told apart.
#[test]
fn a_silent_supervisor_is_stopped_apart_from_a_stale_or_failing_one() {
    let observed = || {
        let mut log = Log::default();
        log.claim(T0, &run_id(1), TaskId::new(1));
        let live = inputs(vec![running(1, Some(T0 + MIN))]);
        let mut recorder = LiveRecorder::default();
        for minute in (5..=30).step_by(5) {
            log.observe(&mut recorder, "s", T0 + minute * MIN, &live);
            log.push(
                T0 + minute * MIN,
                "supervisor_alive",
                json!({"supervisor": "s"}),
                None,
            );
        }
        (log, recorder)
    };
    let past_grace = (T0 + 30 * MIN + GRACE_SECS * SEC + SEC) / 1000;

    let (silent, _) = observed();
    assert!(!silent.events.iter().any(|e| e.kind == "supervisor_stopped"));
    assert_eq!(
        missing_reason(&fold(&silent.events, (T0 + 40 * MIN) / 1000)),
        None
    );
    assert_eq!(
        missing_reason(&fold(&silent.events, past_grace)),
        Some(Missing::SupervisorStopped)
    );

    let (mut alive, _) = observed();
    for minute in (35..=50).step_by(5) {
        alive.push(
            T0 + minute * MIN,
            "supervisor_alive",
            json!({"supervisor": "s"}),
            None,
        );
    }
    assert_eq!(
        missing_reason(&fold(&alive.events, past_grace)),
        Some(Missing::Stale)
    );

    let (mut failing, mut recorder) = observed();
    for minute in (35..=50).step_by(5) {
        failing.fail(&mut recorder, "s", T0 + minute * MIN);
        failing.push(
            T0 + minute * MIN,
            "supervisor_alive",
            json!({"supervisor": "s"}),
            None,
        );
    }
    assert_eq!(
        missing_reason(&fold(&failing.events, past_grace)),
        Some(Missing::ObservationFailed)
    );
}
