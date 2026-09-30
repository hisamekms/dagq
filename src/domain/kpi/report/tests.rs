use std::collections::{HashMap, HashSet};

use serde_json::{Value, json};

use super::*;
use crate::domain::{
    EventId, FindingId, FindingStatus, GoalId, Impact, RunEvent, RunId, TaskId,
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
        actor: None,
    }
}

/// A queue's events with the goal of each task.
type Events = (Vec<RunEvent>, HashMap<TaskId, Option<GoalId>>);

/// One landing a day on Monday to Thursday, and a mark on Tuesday whose
/// label needs escaping.
fn events() -> Events {
    let mut events = Vec::new();
    let mut goals = HashMap::new();
    for day in 0..4 {
        let task = day + 1;
        let claimed = MONDAY + day * DAY + 10 * HOUR;
        let run = format!("{task:08x}-0000-4000-8000-{claimed:012x}");
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
    // Wednesday noon's forecast snapshot gives task 4 (Thursday's landing)
    // 20 hours; it completes 2 hours later than that.
    let snapshot = MONDAY + 2 * DAY + 12 * HOUR;
    events.push(event(
        0,
        None,
        None,
        "forecast_recorded",
        json!({"at_secs": snapshot, "method": 1, "tasks": [{"id": 4, "p50_secs": 20 * HOUR, "p90_secs": 30 * HOUR}], "goals": []}),
        snapshot,
    ));
    events.push(event(
        0,
        Some(4),
        None,
        "task_status_changed",
        json!({"from": "in_progress", "to": "completed"}),
        MONDAY + 3 * DAY + 10 * HOUR + 700,
    ));
    events.sort_by_key(|event| event.created_at.clone());
    for (index, event) in events.iter_mut().enumerate() {
        event.id = EventId::new(i64::try_from(index).unwrap() + 1);
    }
    (events, goals)
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
    report_with_host(now, at, period, None)
}

/// [`report`] with the host's load read by `host`.
fn report_with_host(
    now: i64,
    at: i64,
    period: Period,
    host: Option<crate::domain::kpi::HostReader<'_>>,
) -> Report {
    let (events, goals) = events();
    let settings = KpiSettings {
        targets: vec![
            Target {
                kpi: "landings".into(),
                change: None,
                area: None,
                stat: None,
                min: Some(2.0),
                max: None,
            },
            Target {
                kpi: "forecast.p90_hit_rate".into(),
                change: None,
                area: None,
                stat: None,
                min: Some(0.75),
                max: None,
            },
        ],
        ..KpiSettings::default()
    };
    let config = KpiConfig::merge(Some(&settings), None);
    let kpi = kpi(
        &KpiInput {
            events: &events,
            goals: &goals,
            changes: &(1..=4)
                .map(|task| (TaskId::new(task), Some("fix".parse().unwrap())))
                .collect(),
            areas: None,
            heartbeats: &HashMap::new(),
            draft_origins: &HashMap::new(),
            now,
            utc_offset_secs: JST,
            cores: Some(4),
            dagq_source: true,
            config: &config,
            host,
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
    Report::new(
        kpi,
        period,
        now * 1000,
        "0.1.0-dev+abc",
        &findings,
        DiagramSection::not_drawn(Vec::new(), false, "no near-term task to draw"),
    )
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
    // The day's one attempt held the integration slot for a minute.
    let kpis = &json["periods"][6]["kpis"];
    assert_eq!(kpis["landing_utilization"]["all"]["value"], 0.001);
    assert_eq!(kpis["landing_utilization.peak"]["all"]["value"], 0.017);
    assert_eq!(kpis["landing_attempt"]["all"]["median"], 60.0);
    let landings = json["targets"]
        .as_array()
        .unwrap()
        .iter()
        .find(|target| target["kpi"] == "landings")
        .unwrap();
    assert_eq!(landings["state"], "breach");
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
    assert!(html.contains("<code>landing_utilization</code>"));
    assert!(html.contains(
        "1m 00s of 1d 00h (0.1%), 1 attempt(s), 1 landed; the busiest hour from 2026-09-24T01:00:00.000Z at 1.7%"
    ));
    assert!(
        html.contains("12 open or proposed when the report was generated; the first 10 by impact")
    );
    let targets = html.find("<h2>Targets</h2>").unwrap();
    assert!(targets < html.find("<h2>Trend</h2>").unwrap());
}

/// The diagram d2 drew goes inline with nothing that loads from outside;
/// without one the section says why, and the rest of the page is the same.
/// The JSON names the tasks and whether the d2 source was made, never the
/// SVG.
#[test]
fn a_report_carries_the_dependency_diagram_or_why_not() {
    let thursday = MONDAY + 3 * DAY + 12 * HOUR;
    let mut report = report(MONDAY + 4 * DAY + 12 * HOUR, thursday, Period::Day);
    let json = serde_json::to_value(&report).unwrap();
    assert_eq!(
        json["diagram"],
        json!({"tasks": [], "d2_source": false, "reason": "no near-term task to draw"})
    );
    let without = render_html(&report);
    assert!(without.contains("<h2>Near-term dependencies</h2>"));
    assert!(without.contains("Not drawn: no near-term task to draw"));

    let d2 = concat!(
        "<?xml version=\"1.0\"?><svg xmlns=\"http://www.w3.org/2000/svg\" viewBox=\"0 0 9 9\">",
        "<style><![CDATA[@font-face{src:url(\"data:font/woff;base64,AA==\")} @import url(https://x/y.css);]]></style>",
        "<a href=\"https://example.com\"><rect fill=\"url(#g)\"/></a><image href=\"http://x/i.png\"/>",
        "<script>alert(1)</script><text>#7 on top</text></svg>"
    );
    report.diagram = DiagramSection::drawn(vec![7, 9], d2);
    let json = serde_json::to_value(&report).unwrap();
    assert_eq!(json["diagram"], json!({"tasks": [7, 9], "d2_source": true}));
    let html = render_html(&report);
    assert_eq!(external_references(&html), Vec::<String>::new());
    let start = html.find("<div class=\"scroll diagram\"><svg").unwrap();
    let end = html[start..].find("</svg></div>").unwrap() + start;
    assert!(html[start..end].contains("#7 on top"));
    // Outside the SVG, the page is as it was without the diagram.
    let outside = format!("{}{}", &html[..start], &html[end..]);
    for external in EXTERNAL {
        assert!(!outside.contains(external), "{external} in the page");
    }
    assert!(html.contains("2 task(s) as the queue stood at 2026-09-25T03:00:00.000Z"));
    assert!(!html.contains("Not drawn"));

    // What d2 could not do is shown, escaped.
    report.diagram = DiagramSection::not_drawn(
        vec![7],
        true,
        "cannot draw the dependency diagram: d2plugin-tala not found <PATH>",
    );
    let html = render_html(&report);
    assert!(html.contains(
        "Not drawn: cannot draw the dependency diagram: d2plugin-tala not found &lt;PATH&gt;"
    ));
    assert!(!html.contains("<svg data"));
    assert_eq!(
        serde_json::to_value(&report).unwrap()["diagram"]["reason"],
        "cannot draw the dependency diagram: d2plugin-tala not found <PATH>"
    );
    // An SVG that cannot go inline is a reason too.
    let refused = DiagramSection::drawn(vec![7], "not an svg");
    assert!(refused.svg.is_none() && refused.d2_source);
    assert!(refused.reason.unwrap().contains("no <svg>"));
}

/// The page shows the period's forecast errors per stratum, which way the
/// p50 leaned, and the state of a target on them.
#[test]
fn a_report_shows_the_forecast_error() {
    let thursday = MONDAY + 3 * DAY + 12 * HOUR;
    let report = report(MONDAY + 4 * DAY + 12 * HOUR, thursday, Period::Day);
    let json = serde_json::to_value(&report).unwrap();
    let thursday = &json["periods"][6];
    assert_eq!(
        thursday["kpis"]["forecast.p90_hit_rate"]["all"]["value"],
        1.0
    );
    assert_eq!(thursday["details"]["forecast"]["samples"], 1);
    let html = render_html(&report);
    let section = &html[html.find("<h2>Forecast error</h2>").unwrap()..];
    let section = &section[..section.find("</table>").unwrap()];
    assert!(section.contains("1 sample(s)"), "{section}");
    assert!(section.contains("<td>change=fix</td>"), "{section}");
    // The strata table has the change's too (ADR-t980-1).
    let strata = &html[html.find("By stratum (change, area").unwrap()..];
    assert!(strata.contains("<td>change=fix</td>"), "{strata}");
    assert!(section.contains("<td>marks=0</td>"), "{section}");
    // 2 hours 11 minutes late, a tenth of the 20 hours given.
    assert!(section.contains("+2h 11m"), "{section}");
    assert!(section.contains("+11.0%"), "{section}");
    assert!(section.contains("<td>late</td>"), "{section}");
    // One sample is too few to judge the target on it.
    assert!(
        section.contains("<td class=\"state-not_judged\">not_judged</td>"),
        "{section}"
    );
    assert!(section.contains("<tr><td>target=task</td>"), "{section}");
    // Without a sample, the section says so and has no table.
    let monday = MONDAY + 12 * HOUR;
    let html = render_html(&super::tests::report(
        MONDAY + DAY + 12 * HOUR,
        monday,
        Period::Day,
    ));
    let section = &html[html.find("<h2>Forecast error</h2>").unwrap()..];
    assert!(section.starts_with("<h2>Forecast error</h2><p class=\"meta\">0 sample(s)"));
    assert!(!section[..section.find("<h2>KPIs of").unwrap()].contains("<table>"));
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

/// The page shows the host's CPU per landing, per kind of process, and its
/// load over the cores (goal 72) when the host's records were read, and
/// says so when the period has none.
#[test]
fn a_report_shows_the_host_cpu_per_landing() {
    use crate::domain::host_metrics::{HostSample, summarize};
    let thursday = MONDAY + 3 * DAY;
    let samples: Vec<HostSample> = (1..=10)
        .map(|index| {
            let mut sample = HostSample::new(thursday + 30 * index);
            sample.set("cpu_total", Some(100.0));
            sample.set("cpu_rustc", Some(100.0));
            sample.set("load1", Some(8.0));
            sample
        })
        .collect();
    let read = |from, until| summarize(&samples, from, until);
    let now = MONDAY + 4 * DAY + 12 * HOUR;
    let report = report_with_host(
        now,
        thursday + 12 * HOUR,
        Period::Day,
        Some(crate::domain::kpi::HostReader(&read)),
    );
    let json = serde_json::to_value(&report).unwrap();
    let kpis = &json["periods"][6]["kpis"];
    assert_eq!(kpis["cpu_per_landing"]["all"]["value"], 300.0);
    assert_eq!(kpis["cpu_per_landing.rustc"]["all"]["value"], 300.0);
    assert_eq!(kpis["load_per_core"]["all"]["median"], 2.0);
    let html = render_html(&report);
    assert!(html.contains("<h2>Host CPU</h2>"), "{html}");
    assert!(html.contains(
        "The processes spent 5m 00s of CPU over 1 landing(s): 5m 00s per landing (cargo 0s, rustc 5m 00s, claude 0s, dagq 0s, other 0s). The 1-minute load over 4 core(s): median 2, p90 2, max 2."
    ), "{html}");
    assert!(html.contains("<code>cpu_per_landing</code>"));
    // Wednesday had no record.
    let wednesday = report_with_host(
        now,
        thursday - 12 * HOUR,
        Period::Day,
        Some(crate::domain::kpi::HostReader(&read)),
    );
    let html = render_html(&wednesday);
    assert!(html.contains("No CPU was recorded in the period."));
    assert!(!html.contains("1-minute load"));
    // Without the host's records, no such section.
    assert!(
        !render_html(&super::tests::report(
            now,
            thursday + 12 * HOUR,
            Period::Day
        ))
        .contains("Host CPU")
    );
}

/// The headless jobs of the period (goal 73) per kind and per provider
/// (and the throughput review's per mode): how many, the share failed, the
/// median time and the share of each verdict; no section when no job ran.
#[test]
fn a_report_shows_the_headless_jobs_per_provider() {
    use crate::domain::kpi::Measure;
    let thursday = MONDAY + 3 * DAY + 12 * HOUR;
    let now = MONDAY + 4 * DAY + 12 * HOUR;
    let mut report = report(now, thursday, Period::Day);
    assert!(!render_html(&report).contains("<h2>Headless jobs</h2>"));
    let kpis = &mut report.kpi.periods.last_mut().unwrap().window.kpis;
    for (stratum, count, failed, secs, achieved) in [
        ("all", 3, 1.0, vec![60, 120], 1.0),
        ("provider=claude", 1, 0.0, vec![120], 1.0),
        ("provider=codex", 2, 1.0, vec![60], 1.0),
        ("model=gpt-5.5", 2, 1.0, vec![60], 1.0),
    ] {
        let mut put = |name: &str, measure: Measure| {
            kpis.entry(name.to_owned())
                .or_default()
                .insert(stratum.to_owned(), measure);
        };
        put("job.count.goal_review", Measure::count(count));
        put("job.failed_rate.goal_review", Measure::ratio(failed, count));
        put("job.secs.goal_review", Measure::secs(secs));
        put(
            "job.verdict.goal_review.achieved",
            Measure::ratio(achieved, count - usize::from(failed > 0.0)),
        );
    }
    for (stratum, count, failed, secs) in [
        ("all", 3, 1.0, vec![40, 50, 900]),
        ("mode=hourly", 2, 1.0, vec![40, 50]),
        ("mode=daily", 1, 0.0, vec![900]),
        ("mode=weekly", 0, 0.0, vec![]),
    ] {
        let mut put = |name: &str, measure: Measure| {
            kpis.entry(name.to_owned())
                .or_default()
                .insert(stratum.to_owned(), measure);
        };
        put("job.count.throughput_review", Measure::count(count));
        put(
            "job.failed_rate.throughput_review",
            Measure::ratio(failed, count),
        );
        put("job.secs.throughput_review", Measure::secs(secs));
    }
    let html = render_html(&report);
    assert!(html.contains("<h2>Headless jobs</h2>"), "{html}");
    // The throughput review per mode, a mode without a job left out.
    assert!(html.contains(
        "<tr><td><code>throughput_review</code></td><td>mode=hourly</td><td class=\"num\">2</td><td class=\"num\">50.0%</td>"
    ), "{html}");
    assert!(html.contains("<td>mode=daily</td><td class=\"num\">1</td>"));
    assert!(html.contains(
        "<tr><td><code>goal_review</code></td><td>codex</td><td class=\"num\">2</td><td class=\"num\">50.0%</td><td class=\"num\">1m 00s</td><td>achieved 100.0%</td></tr>"
    ), "{html}");
    assert!(html.contains("<td>claude</td><td class=\"num\">1</td><td class=\"num\">0.0%</td>"));
    assert!(html.contains("<td>all</td><td class=\"num\">3</td>"));
    // The model's strata are in the table by stratum, not in this section.
    let jobs = html.find("<h2>Headless jobs</h2>").unwrap();
    let kpis = html.find("<h2>KPIs of").unwrap();
    assert!(jobs < kpis);
    assert!(!html[jobs..kpis].contains("model="));
    assert!(!html[jobs..kpis].contains("mode=weekly"));
}
