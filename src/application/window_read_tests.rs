//! The window a read folds as the rows come
//! ([`crate::domain::ledger::window::WindowRows`]) answers `stats`, `kpi` and
//! `report` as the form that keeps every row and a copy of the records does,
//! whatever the window's cursors and filters.

use std::collections::BTreeMap;
use std::sync::Arc;

use serde_json::{Value, json};

use super::kpi::{self, Host};
use super::report::{self, DiagramRenderer, ReportSetup};
use super::stats::{self, StatsSources};
use crate::domain::areas::AreaMap;
use crate::domain::host_metrics::{HOST_NODE, HostSample, NodeSample, SampleSource};
use crate::domain::kpi::config::KpiConfig;
use crate::domain::kpi::report::{DiagramSection, Keep};
use crate::domain::kpi::{KpiQuery, Period};
use crate::domain::ledger::findings::VALUES;
use crate::domain::ledger::testkit::{Log, T0, ms, run_id};
use crate::domain::ledger::window::{
    LedgerWindow, ON_ITS_WAY, WindowQuery, run_on_its_way, runs_on_their_way, select,
    select_holding_every_row,
};
use crate::domain::ledger::{LedgerKind, LedgerRow, build, node, steps};
use crate::domain::run_phase::Phase;
use crate::domain::session_step::{SessionStep, Step, StepKind, StepTarget};
use crate::domain::stats::{Cursor, StatsQuery};
use crate::domain::{GoalId, RunId, TaskChange, event_kind};

const BUILT: &str = "2030-01-01T00:00:00.000Z";
const END: i64 = 7200;

/// `task` of `goal` and `change` created at `secs`.
fn created(log: &mut Log, task: i64, goal: i64, change: &str, secs: i64) {
    log.add(
        Some(task),
        None,
        event_kind::TASK_CREATED,
        json!({"goal_id": goal, "status": "ready", "title": format!("t{task}"), "change": change,
               "execution_class": "implementation", "priority": null, "priority_by": null}),
        secs,
    );
}

/// A queue with the baselines, two landed runs of goals 3 and 4 (of changes
/// and areas of their own), a run of goal 3 on its way, a goal review whose
/// records name goal 3, a planner whose items name goal 4, a finding and
/// the queue's own events.
fn log() -> Log {
    let mut log = Log::default();
    log.add(
        None,
        None,
        event_kind::ATTRIBUTES_BASELINE,
        json!({"tasks": 0, "goals": 0}),
        0,
    )
    .add(
        None,
        None,
        event_kind::FINDING_BASELINE,
        json!({"findings": 0}),
        0,
    );
    created(&mut log, 1, 3, "runtime", 0);
    created(&mut log, 2, 4, "docs", 0);
    created(&mut log, 5, 3, "runtime", 0);
    log.landed(1, &run_id(1), 10);
    log.landed(2, &run_id(2), 2000);
    for event in &mut log.events {
        if event.kind.as_str() == "run_integrated" {
            let of_first = event.run_id.as_ref().map(RunId::as_str) == Some(run_id(1).as_str());
            let commit = if of_first { "c1" } else { "c2" };
            event.payload["commit"] = json!(commit);
            event.payload["result_commit"] = json!(commit);
        }
    }
    log.add(
        None,
        None,
        "main_commits_recorded",
        json!({"after": null, "commits": [
            {"sha": "c1", "at": T0 + 900, "reverts": null, "changes": [{"path": "src/a.rs"}]},
            {"sha": "c2", "at": T0 + 2900, "reverts": null, "changes": [{"path": "docs/a.md"}]},
        ]}),
        3000,
    )
    .add(
        None,
        None,
        "main_observed",
        json!({"head": "c2", "at": T0 + 3000, "since": T0 - 1, "reverts": true}),
        3000,
    );
    // The goal review's records name its goal, the planner's items theirs.
    log.add(
        None,
        None,
        "goal_review_started",
        json!({"goal_review_id": 21}),
        3100,
    );
    log.events.last_mut().unwrap().goal_id = Some(GoalId::new(3));
    log.add(
        None,
        None,
        "goal_review_finished",
        json!({"goal_review_id": 21, "verdict": "open"}),
        3200,
    );
    log.events.last_mut().unwrap().goal_id = Some(GoalId::new(3));
    log.add(
        None,
        None,
        "session_opened",
        json!({"kind": "runtime_planner", "planner_id": 9, "route": "headless",
               "session_id": "p", "goal_ids": [4]}),
        3300,
    );
    let opened = log.last();
    log.add(
        None,
        None,
        "session_closed",
        json!({"opened_event_id": opened, "session_id": "p", "reason": "exited"}),
        3400,
    );
    // The queue's own: an interval and a finding.
    let full = json!({"supervisor": "s", "class": "heavy", "running": 2, "slots": 2});
    log.add(None, None, "slots_full_started", full.clone(), 3500)
        .add(None, None, "slots_full_ended", full, 3600)
        .add(
            None,
            None,
            event_kind::FINDING_RECORDED,
            json!({"finding_id": 1, VALUES: {"id": 1, "target": "queue", "task_id": null,
                "run_id": null, "goal_id": null, "kind": "conflict_hotspot", "subject": "src/a.rs",
                "summary": "a", "impact": "normal", "status": "open", "occurrences": 1,
                "last_seen_at": T0}}),
            3700,
        );
    // The run on its way.
    let way = run_id(5);
    log.add(
        Some(5),
        Some(&way),
        "run_claimed",
        json!({"from": "ready", "to": "in_progress", "provider": "claude"}),
        4000,
    )
    .phase(5, &way, 4000, Phase::Worker, "x");
    log
}

/// The host's rows of a sample every 1000 s.
fn node_rows() -> Vec<LedgerRow> {
    let samples: Vec<NodeSample> = (0..4)
        .map(|step| {
            let unix = T0 + 1000 * step;
            let mut sample = HostSample::new(unix);
            sample.set("load1", Some(1.5));
            sample.set("cpu_total", Some(100.0));
            NodeSample {
                resources: Default::default(),
                id: step + 1,
                node_id: HOST_NODE.to_owned(),
                local_day: unix.div_euclid(86_400),
                source: SampleSource::Sampled,
                sample: sample.as_written(),
            }
        })
        .collect();
    node::node_rows(&samples, BUILT)
}

/// The rows a store holds of [`log`], as the runtime reads them: a run's row
/// with its steps, node rows, none of the run on its way but the one folded
/// at the read.
fn rows() -> Vec<LedgerRow> {
    let log = log();
    let now_ms = ms(END);
    let way = RunId::new(run_id(5)).unwrap();
    let mut rows: Vec<LedgerRow> = build(&log.events, now_ms, BUILT)
        .into_iter()
        .filter(|row| !(row.kind == LedgerKind::Run && row.key == way.as_str()))
        .collect();
    let landed = RunId::new(run_id(1)).unwrap();
    let step = SessionStep {
        id: 1,
        target: StepTarget::run(&landed),
        session_id: "s".to_owned(),
        received_at: now_ms,
        step: Step::new(1, StepKind::ToolCall, 0, 1000),
    };
    let starts = BTreeMap::from([("s".to_owned(), ms(10))]);
    let run = rows
        .iter_mut()
        .find(|row| row.kind == LedgerKind::Run && row.key == landed.as_str())
        .unwrap();
    run.detail[steps::DETAIL_KEY] = steps::fold_steps(&starts, &[step]).detail(&run.intervals);
    rows.extend(node_rows());
    assert_eq!(runs_on_their_way(&rows), std::slice::from_ref(&way));
    let events: Vec<_> = log
        .events
        .iter()
        .filter(|event| event.run_id.as_ref() == Some(&way))
        .cloned()
        .collect();
    let on_its_way = run_on_its_way(&way, &events, now_ms).unwrap();
    assert_eq!(on_its_way.built_at, ON_ITS_WAY);
    rows.push(on_its_way);
    rows
}

fn areas() -> AreaMap {
    AreaMap::new(vec![
        ("runtime".to_owned(), vec!["src/**".to_owned()]),
        ("docs".to_owned(), vec!["docs/**".to_owned()]),
    ])
    .unwrap()
}

fn host() -> Host {
    Host {
        utc_offset_secs: 0,
        cores: Some(8),
        rust_toolchain: true,
    }
}

/// `stats`, `kpi` and `report` of `window`, as what they answer.
fn answers(window: &LedgerWindow, query: &WindowQuery) -> [Value; 3] {
    let now = ms(END) / 1000;
    let areas = areas();
    let no_config = || Ok(None);
    let no_conflicts = || Ok(None);
    let stats = stats::of_window(
        window,
        now,
        &StatsQuery {
            since: query.since,
            until: query.until,
            goal_id: query.goal_id,
            full: true,
        },
        &StatsSources {
            config_file: &no_config,
            conflicts_file: &no_conflicts,
            areas: Some(&areas),
            utc_offset_secs: 0,
            counts_checks: true,
            rust_toolchain: true,
        },
    )
    .unwrap();
    let kpi = kpi::of_window(
        window,
        now,
        host(),
        &KpiConfig::default(),
        Some(&areas),
        &KpiQuery::default(),
    )
    .unwrap();
    let setup = ReportSetup {
        root: "/reports".into(),
        host: host(),
        config: KpiConfig::default(),
        keep: Keep::default(),
        build: "b".to_owned(),
        diagram: DiagramRenderer(Arc::new(|source: &str| Ok(source.to_owned()))),
        areas: Some(areas.clone()),
    };
    let report = report::make_with(
        window,
        &setup,
        now,
        Period::Week,
        None,
        DiagramSection::not_drawn(Vec::new(), false, "not drawn"),
    )
    .unwrap();
    [
        serde_json::to_value(stats).unwrap(),
        serde_json::to_value(kpi).unwrap(),
        serde_json::to_value(report).unwrap(),
    ]
}

/// The keys of the window's rows of `kind`.
fn keys(window: &LedgerWindow, kind: LedgerKind) -> Vec<&str> {
    window.of(kind).map(|row| row.key.as_str()).collect()
}

/// Whatever the window, the folded one answers as the one that holds every
/// row, and chooses the same rows but the queue's; and each filter chooses
/// what it is there to, so that the windows do tell apart.
#[test]
fn the_folded_window_answers_stats_kpi_and_report_as_the_one_holding_every_row() {
    let rows = rows();
    let now_ms = ms(END);
    let base = WindowQuery {
        areas: Some(areas()),
        now_ms,
        ..WindowQuery::default()
    };
    let cases = [
        ("none", base.clone()),
        (
            "goal",
            WindowQuery {
                goal_id: Some(GoalId::new(3)),
                ..base.clone()
            },
        ),
        (
            "goal of a planner's items",
            WindowQuery {
                goal_id: Some(GoalId::new(4)),
                ..base.clone()
            },
        ),
        (
            "change",
            WindowQuery {
                change: Some(TaskChange::try_from("docs".to_owned()).unwrap()),
                ..base.clone()
            },
        ),
        (
            "area",
            WindowQuery {
                area: Some("docs".to_owned()),
                ..base.clone()
            },
        ),
        (
            "since and until",
            WindowQuery {
                since: Some(Cursor::Time(ms(1000))),
                until: Some(Cursor::Time(ms(3500))),
                ..base.clone()
            },
        ),
    ];
    let mut chosen = BTreeMap::new();
    for (name, query) in &cases {
        let held = select_holding_every_row(rows.clone(), query);
        let folded = select(rows.clone(), query);
        assert_eq!(answers(&folded, query), answers(&held, query), "{name}");
        assert_eq!(folded.events(), held.events(), "{name}");
        assert_eq!(folded.executions(), held.executions(), "{name}");
        assert_eq!(folded.execution_tokens(), held.execution_tokens(), "{name}");
        for kind in [
            LedgerKind::Run,
            LedgerKind::Task,
            LedgerKind::Session,
            LedgerKind::Node,
        ] {
            assert_eq!(keys(&folded, kind), keys(&held, kind), "{name} {kind:?}");
        }
        chosen.insert(*name, keys(&folded, LedgerKind::Session).join(","));
        assert!(folded.of(LedgerKind::Node).next().is_some(), "{name}");
        let (first, second, way) = (run_id(1), run_id(2), run_id(5));
        let expected: Vec<&str> = match *name {
            "none" => vec![&first, &second, &way],
            "goal" => vec![&first, &way],
            "goal of a planner's items" | "change" | "area" => vec![&second],
            "since and until" => vec![&second],
            other => panic!("{other}"),
        }
        .into_iter()
        .map(String::as_str)
        .collect();
        let mut runs = keys(&folded, LedgerKind::Run);
        runs.sort_unstable();
        assert_eq!(runs, expected, "{name}");
    }
    assert!(chosen["goal"].contains("goal_review/21/job"));
    assert!(!chosen["goal"].contains("runtime_planner/9"));
    assert!(chosen["goal of a planner's items"].contains("runtime_planner/9"));
    assert!(!chosen["goal of a planner's items"].contains("goal_review/21/job"));
    // The answers are of something: the landed runs, the finding.
    let [stats, kpi, report] = answers(&select(rows, &base), &base);
    assert_eq!(stats["runs"].as_array().unwrap().len(), 2);
    assert_ne!(kpi["periods"], json!([]));
    assert_eq!(report["findings_open"], json!(1));
}
