use std::collections::{HashMap, HashSet};

use serde_json::{Value, json};

use super::*;
use crate::domain::{
    EventId, FindingId, FindingStatus, GoalId, Impact, RunEvent, RunId, TaskId, TaskKind,
    kpi::{KpiConfig, KpiInput, KpiQuery, KpiSettings, Target, kpi},
    stats::Cursor,
};

/// 2026-09-21T00:00:00+09:00, a Monday, in unix seconds.
pub(crate) const MONDAY: i64 = 1_789_916_400;
pub(crate) const HOUR: i64 = 3600;
pub(crate) const DAY: i64 = 24 * HOUR;
const JST: i64 = 9 * HOUR;

/// What a page may not contain if it loads nothing from anywhere else.
pub(crate) const EXTERNAL: &[&str] = &[
    "<script",
    "<link",
    "<img",
    "<iframe",
    "<object",
    "<embed",
    "@import",
    "url(",
    "src=",
    "http://",
    "https://",
    "//cdn",
    "@font-face",
];

fn event(
    id: i64,
    task: Option<i64>,
    run: Option<&str>,
    kind: &str,
    payload: Value,
    secs: i64,
) -> RunEvent {
    RunEvent {
        id: EventId::new(id),
        task_id: task.map(TaskId::new),
        goal_id: None,
        run_id: run.map(|run| RunId::new(run).unwrap()),
        kind: kind.to_owned(),
        payload,
        created_at: marks::utc_text(secs * 1000),
    }
}

/// A queue's events with the kind and the goal of each task.
type Events = (
    Vec<RunEvent>,
    HashMap<TaskId, Option<TaskKind>>,
    HashMap<TaskId, Option<GoalId>>,
);

/// One landing a day on Monday to Thursday, and a mark on Tuesday whose
/// label needs escaping.
fn events() -> Events {
    let mut events = Vec::new();
    let mut kinds = HashMap::new();
    let mut goals = HashMap::new();
    for day in 0..4 {
        let task = day + 1;
        let claimed = MONDAY + day * DAY + 10 * HOUR;
        let run = format!("{task:08x}-0000-4000-8000-{claimed:012x}");
        kinds.insert(
            TaskId::new(task),
            Some("runtime".parse::<TaskKind>().unwrap()),
        );
        goals.insert(TaskId::new(task), None);
        for (offset, kind, payload) in [
            (
                -HOUR,
                "task_status_changed",
                json!({"from": "submitted", "to": "ready"}),
            ),
            (
                0,
                "run_claimed",
                json!({"parallel": 3, "slots": 1, "load_avg": 2.0, "dagq_version": "b1"}),
            ),
            (600, "receipt_observed", json!({})),
            (
                620,
                "validation_finished",
                json!({"status": "awaiting_integration"}),
            ),
            (640, "integration_started", json!({})),
            (700, "run_integrated", json!({"status": "integrated"})),
        ] {
            let id = i64::try_from(events.len()).unwrap() + 1;
            let run = (kind != "task_status_changed").then_some(run.as_str());
            events.push(event(id, Some(task), run, kind, payload, claimed + offset));
        }
    }
    let id = i64::try_from(events.len()).unwrap() + 1;
    events.push(event(
        id,
        None,
        None,
        "mark_recorded",
        json!({"label": "parallel <4> & 3", "at": marks::utc_text((MONDAY + DAY + 12 * HOUR) * 1000)}),
        MONDAY + DAY + 12 * HOUR,
    ));
    events.sort_by_key(|event| event.created_at.clone());
    for (index, event) in events.iter_mut().enumerate() {
        event.id = EventId::new(i64::try_from(index).unwrap() + 1);
    }
    (events, kinds, goals)
}

fn finding(id: i64, summary: &str) -> Finding {
    Finding {
        id: FindingId::new(id),
        kind: "threshold".into(),
        target: "queue".into(),
        task_id: None,
        run_id: None,
        goal_id: None,
        subject: "phase.work".into(),
        summary: summary.into(),
        detail: String::new(),
        impact: Impact::High,
        first_seen_at: MONDAY,
        last_seen_at: MONDAY + DAY,
        occurrences: 3,
        evidence: Vec::new(),
        status: FindingStatus::Open,
        status_reason: None,
        proposal_id: None,
        propose_reason: None,
        propose_requested_at: None,
        recorded_by: "observer".into(),
        updated_at: MONDAY,
    }
}

/// The report of Thursday, judged at Friday noon with a target on the
/// landings that three days in a row missed.
pub(crate) fn report(now: i64, at: i64, period: Period) -> Report {
    let (events, kinds, goals) = events();
    let settings = KpiSettings {
        targets: vec![Target {
            kpi: "landings".into(),
            kind: None,
            stat: None,
            min: Some(2.0),
            max: None,
        }],
        ..KpiSettings::default()
    };
    let config = KpiConfig::merge(Some(&settings), None);
    let kpi = kpi(
        &KpiInput {
            events: &events,
            goals: &goals,
            kinds: &kinds,
            heartbeats: &HashMap::new(),
            draft_origins: &HashMap::new(),
            now,
            utc_offset_secs: JST,
            cores: Some(4),
            config: &config,
        },
        &KpiQuery {
            period,
            at: Some(Cursor::Time(at * 1000)),
            ..KpiQuery::default()
        },
    )
    .unwrap();
    let findings: Vec<Finding> = (1..=12)
        .map(|id| finding(id, &format!("work <slow> {id}")))
        .collect();
    Report::new(kpi, period, now * 1000, "0.1.0-dev+abc", &findings)
}

/// The JSON is `kpi`'s with the header and the top findings; the page shows
/// the breach first, the mark over the trend and the findings, escaped.
#[test]
fn a_report_carries_the_kpis_the_header_and_the_top_findings() {
    let thursday = MONDAY + 3 * DAY + 12 * HOUR;
    let report = report(MONDAY + 4 * DAY + 12 * HOUR, thursday, Period::Day);
    assert_eq!(report.report.label, "2026-09-24");
    assert!(!report.report.partial);
    assert_eq!(report.findings_open, 12);
    assert_eq!(report.findings.len(), FINDINGS_LISTED);
    let json = serde_json::to_value(&report).unwrap();
    assert_eq!(json["report"]["period"], "day");
    assert_eq!(json["report"]["build"], "0.1.0-dev+abc");
    assert_eq!(json["report"]["generated_at"], "2026-09-25T03:00:00.000Z");
    assert_eq!(json["period"], "day");
    assert_eq!(json["periods"].as_array().unwrap().len(), 7);
    assert_eq!(json["periods"][6]["kpis"]["landings"]["all"]["value"], 1.0);
    assert_eq!(json["targets"][0]["state"], "breach");
    assert_eq!(json["findings"][0]["impact"], "high");
    assert_eq!(
        report.file_name(Period::Day).path("html"),
        "daily/2026-09-24.html"
    );

    let html = render_html(&report);
    for external in EXTERNAL {
        assert!(!html.contains(external), "{external} in the page");
    }
    assert!(html.starts_with("<!doctype html>"));
    assert!(html.contains("<svg class=\"spark\""));
    assert!(html.contains("parallel &lt;4&gt; &amp; 3"));
    assert!(!html.contains("parallel <4>"));
    assert!(html.contains("work &lt;slow&gt; 1"));
    assert!(html.contains("<tr class=\"breach\"><td><code>landings</code>"));
    assert!(html.contains("class=\"mark\""));
    assert!(
        html.contains("12 open or proposed when the report was generated; the first 10 by impact")
    );
    let targets = html.find("<h2>Targets</h2>").unwrap();
    assert!(targets < html.find("<h2>Trend</h2>").unwrap());
}

/// Today's report is partial, and its files say so.
#[test]
fn a_partial_report_is_named_apart() {
    let now = MONDAY + 3 * DAY + 12 * HOUR;
    let report = report(now, now, Period::Day);
    assert!(report.report.partial);
    let file = report.file_name(Period::Day);
    assert_eq!(file.path("json"), "daily/2026-09-24.partial.json");
    assert!(render_html(&report).contains("<span class=\"badge\">partial</span>"));
    let week = self::report(now, now, Period::Week);
    assert_eq!(week.report.label, "2026-W39");
    assert_eq!(
        week.file_name(Period::Week).path("html"),
        "weekly/2026-W39.partial.html"
    );
}

#[test]
fn labels_and_file_names_are_parsed_back() {
    assert_eq!(
        label_days(Period::Day, "2026-09-21"),
        Some(MONDAY.div_euclid(DAY) + 1)
    );
    assert_eq!(label_days(Period::Day, "2026-02-31"), None);
    assert_eq!(label_days(Period::Day, "junk"), None);
    let monday = label_days(Period::Week, "2026-W39").unwrap();
    assert_eq!(date(monday), "2026-09-21");
    assert_eq!(
        date(label_days(Period::Week, "2026-W53").unwrap()),
        "2026-12-28"
    );
    assert_eq!(
        date(label_days(Period::Week, "2027-W01").unwrap()),
        "2027-01-04"
    );
    assert_eq!(label_days(Period::Week, "2027-W53"), None);
    assert_eq!(label_days(Period::Week, "2026-W00"), None);
    assert_eq!(label_days(Period::Week, "2026-39"), None);
    let (file, extension) = ReportFile::parse(Period::Day, "2026-09-21.partial.html").unwrap();
    assert!(file.partial);
    assert_eq!((file.label.as_str(), extension), ("2026-09-21", "html"));
    assert_eq!(
        ReportFile::parse(Period::Day, "2026-09-21.json")
            .unwrap()
            .0
            .stem(),
        "2026-09-21"
    );
    assert!(ReportFile::parse(Period::Day, "2026-09-21.txt").is_none());
    assert!(ReportFile::parse(Period::Day, "2026-09-21.html.tmp").is_none());
    assert!(ReportFile::parse(Period::Week, "2026-09-21.html").is_none());
    assert!(ReportFile::parse(Period::Day, "notes").is_none());
}

/// Seven days back and the last week, less what was written.
#[test]
fn the_reports_due_are_the_days_and_the_week_not_written() {
    let now = (MONDAY + 2 * DAY + 30 * 60) * 1000;
    let offset = JST * 1000;
    let due = due(now, offset, &HashSet::new());
    let labels: Vec<&str> = due.iter().map(|(_, label, _)| label.as_str()).collect();
    assert_eq!(
        labels,
        [
            "2026-09-16",
            "2026-09-17",
            "2026-09-18",
            "2026-09-19",
            "2026-09-20",
            "2026-09-21",
            "2026-09-22",
            "2026-W38"
        ]
    );
    assert_eq!(due[6].2, (MONDAY + DAY) * 1000);
    assert_eq!(due[7].0, Period::Week);
    let written: HashSet<(String, String)> = [
        ("day", "2026-09-22"),
        ("week", "2026-W38"),
        ("day", "2026-09-16"),
    ]
    .into_iter()
    .map(|(p, l)| (p.to_owned(), l.to_owned()))
    .collect();
    let due = super::due(now, offset, &written);
    assert_eq!(due.len(), 5);
    assert!(due.iter().all(|(period, _, _)| *period == Period::Day));
}

#[test]
fn the_retention_removes_old_and_superseded_reports() {
    let now = (MONDAY + 12 * HOUR) * 1000;
    let offset = JST * 1000;
    let keep = Keep {
        daily_days: 3,
        weekly_weeks: 2,
    };
    let file = |period, label: &str, partial| ReportFile {
        period,
        label: label.to_owned(),
        partial,
    };
    let none = HashSet::new();
    assert!(!expired(
        &file(Period::Day, "2026-09-18", false),
        now,
        offset,
        keep,
        &none
    ));
    assert!(expired(
        &file(Period::Day, "2026-09-17", false),
        now,
        offset,
        keep,
        &none
    ));
    assert!(!expired(
        &file(Period::Week, "2026-W38", false),
        now,
        offset,
        keep,
        &none
    ));
    assert!(!expired(
        &file(Period::Week, "2026-W37", false),
        now,
        offset,
        keep,
        &none
    ));
    assert!(expired(
        &file(Period::Week, "2026-W36", false),
        now,
        offset,
        keep,
        &none
    ));
    assert!(!expired(
        &file(Period::Day, "junk", false),
        now,
        offset,
        keep,
        &none
    ));
    let complete: HashSet<(Period, String)> = [(Period::Day, "2026-09-20".to_owned())].into();
    assert!(expired(
        &file(Period::Day, "2026-09-20", true),
        now,
        offset,
        keep,
        &complete
    ));
    assert!(!expired(
        &file(Period::Day, "2026-09-21", true),
        now,
        offset,
        keep,
        &complete
    ));
    assert_eq!(Keep::default().daily_days, 90);
    assert_eq!(Keep::default().weekly_weeks, 104);
}

#[test]
fn the_index_lists_the_reports_newest_first() {
    let file = |period, label: &str, partial| ReportFile {
        period,
        label: label.to_owned(),
        partial,
    };
    let html = index_html(
        &[
            file(Period::Day, "2026-09-20", false),
            file(Period::Day, "2026-09-22", true),
            file(Period::Day, "2026-09-21", false),
        ],
        &[],
        "2026-09-22T00:00:00.000Z",
    );
    for external in EXTERNAL {
        assert!(!html.contains(external), "{external} in the index");
    }
    let at = |needle: &str| html.find(needle).unwrap();
    assert!(at("daily/2026-09-22.partial.html") < at("daily/2026-09-21.html"));
    assert!(at("daily/2026-09-21.html") < at("daily/2026-09-20.html"));
    assert!(html.contains("<p class=\"muted\">none yet</p>"));
}
