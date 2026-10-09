//! The record of `stats`' judgments of now (docs/design/measurement.md
//! "今の判定の記録"): what a supervisor's pass writes, and what the fold of
//! the events alone gives back against `stats` on a real queue.
use crate::common;
use crate::runtime_support;

use dagq::application::stats::{LiveSources, StatsSources, live_inputs, stats};
use dagq::domain::{
    EventKind, LeaseToken,
    live_alerts::{LiveRecorder, Observation, ObservationState, fold, judge},
    stall::StallConfig,
};
use dagq::infrastructure::adapters::{ClaudeCode, SystemProcesses};
use runtime_support::headless::*;
use runtime_support::*;

/// The queue events of `kind`.
fn queue_payloads(db: &Path, kind: &str) -> Vec<Value> {
    SqliteQueue::open(db)
        .unwrap()
        .all_events()
        .unwrap()
        .into_iter()
        .filter(|event| event.kind == kind)
        .map(|event| event.payload)
        .collect()
}

/// The supervisor's pass writes the record: one baseline after it
/// starts, and for the headless run idle without a receipt while its
/// `stalled` ask waits, the alert's start and, once the answer's turn
/// writes the receipt, its end.
#[test]
fn a_supervisor_records_its_baseline_and_an_idle_alert_from_start_to_end() {
    let (dir, repo, db, backend) = headless_fixture(&[]);
    set_turns(
        dir.path(),
        &format!(
            r#"case "$PROMPT" in
"answer to ask "*) {FINISH} ;;
*) denied; say refused ;;
esac"#
        ),
    );
    let backend = Arc::new(backend);
    let mut stall = StallConfig::default();
    stall.set("idle_without_receipt_secs", 1);
    let reviewer = Arc::new(TestReviewer::new(&[verdict("pass", &[], "fine")]));
    let options = SuperviseOptions {
        stall: Some(stall),
        live_alert_interval: Duration::ZERO,
        ..supervise_options(4, true)
    };
    let supervisor = {
        let (db, repo, backend, reviewer) = (
            db.to_owned(),
            repo.to_owned(),
            backend.clone(),
            reviewer.clone(),
        );
        thread::spawn(move || {
            runtime::supervise_with_reviewer(
                &db,
                &repo,
                &*backend,
                &claude_stub(&db),
                &*reviewer,
                Path::new(env!("CARGO_BIN_EXE_dagq")),
                &options,
            )
        })
    };
    wait_until(&db, common::STEP_LIMIT, |queue| {
        !stalled_asks(queue).is_empty()
    });
    wait_until(&db, common::STEP_LIMIT, |_| {
        !queue_payloads(&db, EventKind::LiveAlertStarted.as_str()).is_empty()
    });
    let ask = stalled_asks(&SqliteQueue::open(&db).unwrap()).remove(0);
    SqliteQueue::open(&db)
        .unwrap()
        .answer(ask.id, "go on and finish")
        .unwrap();
    let outcome = joined(supervisor, "the supervisor thread to return").unwrap();
    backend.join();
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    let run = detail(&db).runs[0].id().to_string();
    let baselines = queue_payloads(&db, EventKind::LiveAlertBaseline.as_str());
    assert_eq!(baselines.len(), 1, "{baselines:?}");
    assert_eq!(baselines[0]["version"], 1);
    let of_run = |kind: EventKind| -> Vec<Value> {
        queue_payloads(&db, kind.as_str())
            .into_iter()
            .filter(|payload| payload["run_id"] == json!(run))
            .collect()
    };
    let started = of_run(EventKind::LiveAlertStarted);
    let ended = of_run(EventKind::LiveAlertEnded);
    assert_eq!(started.len(), 1, "{started:?}");
    assert_eq!(ended, started, "{ended:?}");
    assert_eq!(started[0]["kind"], "idle_without_receipt");
    assert_eq!(started[0]["supervisor"], baselines[0]["supervisor"]);
}

/// On a queue of two live supervisors of different `parallel` and one
/// stopped, a run under no lease, one landing under a token no supervisor
/// registered, one waiting for a person and one whose lease moves to the
/// other live supervisor, the streams of two supervisors folded at a
/// window's end give what `stats` lists at that time: the slot alerts, the
/// running alerts (each run once) and the workspace check, every field
/// and in order. The lease that moves writes nothing.
#[test]
fn the_folded_streams_give_what_stats_lists_on_the_queue() {
    let (_dir, repo, db) = fixture();
    let mut queue = SqliteQueue::open(&db).unwrap();
    for title in ["second", "third", "fourth"] {
        add_ready_task(&mut queue, title, &[]);
    }
    let live = std::process::id();
    queue
        .register_supervisor(&LeaseToken::new("a"), live, 2, "0.0.1")
        .unwrap();
    queue
        .register_supervisor(&LeaseToken::new("c"), live, 1, "0.0.1")
        .unwrap();
    queue
        .register_supervisor(&LeaseToken::new("b"), dead_pid(), 3, "0.0.1")
        .unwrap();
    let moving = provision_under(&repo, &db, "a");
    let unleased = provision_under(&repo, &db, "a");
    let landing = provision_under(&repo, &db, "a");
    let waiting = provision_under(&repo, &db, "a");
    // Ready, but blocked by the first task: the free slot is idle.
    add_ready_task(&mut queue, "after the first", &[TaskId::new(1)]);
    let sql = Connection::open(&db).unwrap();
    sql.execute(
        "DELETE FROM run_leases WHERE run_id=?1",
        [unleased.id().as_str()],
    )
    .unwrap();
    sql.execute(
        "UPDATE run_leases SET token='by-hand' WHERE run_id=?1",
        [landing.id().as_str()],
    )
    .unwrap();
    for (run, status) in [
        (&moving, "running"),
        (&unleased, "running"),
        (&landing, "integrating"),
        (&waiting, "running"),
    ] {
        sql.execute(
            "UPDATE task_runs SET status=?2 WHERE id=?1",
            [run.id().as_str(), status],
        )
        .unwrap();
    }
    for run in [&moving, &unleased, &waiting] {
        queue
            .record_runtime_event(run.id(), EventKind::AgentStarted, json!({}))
            .unwrap();
    }
    // Idle after the sessions started, without a receipt.
    await_second_after(unix_second_now());
    for run in [&moving, &unleased, &waiting] {
        fs::write(
            Path::new(run.run_dir().unwrap()).join("idle.json"),
            r#"{"background_tasks":[]}"#,
        )
        .unwrap();
    }
    queue
        .record_runtime_event(
            waiting.id(),
            EventKind::RunWaitingStarted,
            json!({"phase": "session", "status": "running"}),
        )
        .unwrap();
    queue
        .record_queue_event(
            EventKind::StallConfigLoaded,
            json!({"idle_without_receipt_secs": 1}),
        )
        .unwrap();

    let signals = ClaudeCode {
        executable: PathBuf::from("claude"),
    };
    let no_file = || Ok(None);
    let sources = LiveSources {
        files: &LocalRunFiles,
        signals: &signals,
        config_file: &no_file,
    };
    let observe = |queue: &SqliteQueue, recorder: &mut LiveRecorder, token: &str| {
        let now = unix_second_now();
        let events = queue.all_events().unwrap();
        let inputs = live_inputs(queue, &SystemProcesses, now, &events, &sources).unwrap();
        let alerts = judge(&events, now, &inputs).keys();
        let records = recorder.observe(
            token,
            now * 1000,
            Observation::Seen {
                inputs: Box::new(inputs),
                alerts,
            },
        );
        let kinds: Vec<&str> = records.iter().map(|(kind, _)| kind.as_str()).collect();
        for (kind, payload) in records {
            queue.record_queue_event(kind, payload).unwrap();
        }
        kinds
    };
    let (mut b, mut a) = (LiveRecorder::default(), LiveRecorder::default());
    assert_eq!(observe(&queue, &mut b, "b"), ["live_alert_baseline"]);
    await_second_after(unix_second_now());
    assert_eq!(observe(&queue, &mut a, "a"), ["live_alert_baseline"]);
    sql.execute(
        "UPDATE run_leases SET token='c' WHERE run_id=?1",
        [moving.id().as_str()],
    )
    .unwrap();
    assert!(observe(&queue, &mut a, "a").is_empty());
    assert!(observe(&queue, &mut b, "b").is_empty());

    // Past the idle threshold, with nothing recorded since.
    let end = unix_second_now() + 5;
    let folded = fold(&queue.all_events().unwrap(), end);
    assert!(
        matches!(&folded.running_alerts.state, ObservationState::Observed { supervisor, .. } if supervisor == "a"),
        "{:?}",
        folded.running_alerts.state
    );
    let config_file = || Ok(None);
    let conflicts_file = || Ok(None);
    let history = |_| anyhow::bail!("no history");
    let areas = dagq::application::areas::AreaReader::none();
    let listed = stats(
        &queue,
        &SystemProcesses,
        end,
        &Default::default(),
        &StatsSources {
            files: &LocalRunFiles,
            signals: &signals,
            config_file: &config_file,
            conflicts_file: &conflicts_file,
            history: &history,
            host_metrics: None,
            areas: &areas,
            utc_offset_secs: 0,
            dagq_source: false,
        },
    )
    .unwrap();
    let slot_alerts: Vec<Value> = listed
        .alerts
        .iter()
        .filter(|alert| ["claim_held", "claim_deferred", "idle_slots"].contains(&alert.kind))
        .map(|alert| json!(alert))
        .collect();
    // a's 2 and c's 1 slots, less the two runs executing: the run waiting
    // for a person and the one landing under `by-hand` hold none.
    assert_eq!(
        slot_alerts,
        [
            json!({"kind": "idle_slots", "task_id": null, "run_id": null, "value": 1, "threshold": 0})
        ]
    );
    assert_eq!(json!(folded.slot_alerts.value.unwrap()), json!(slot_alerts));
    let running = folded.running_alerts.value.unwrap();
    assert_eq!(json!(running), json!(listed.running_alerts));
    assert_eq!(
        json!(folded.workspace_check.value.unwrap()),
        json!(listed.workspace_check)
    );
    // The idle runs, each once.
    let mut idle: Vec<String> = running
        .iter()
        .filter(|alert| alert.kind == "idle_without_receipt")
        .map(|alert| alert.run_id.as_ref().unwrap().to_string())
        .collect();
    idle.sort();
    let mut expected = vec![
        moving.id().to_string(),
        unleased.id().to_string(),
        waiting.id().to_string(),
    ];
    expected.sort();
    assert_eq!(idle, expected);
}
