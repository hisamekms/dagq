use std::path::Path;

use serde_json::{Value, json};

use super::*;
use crate::domain::kpi::{
    Period, TargetReport,
    config::TargetPeriod,
    report::tests::{DAY, HOUR, MONDAY, report},
};

fn target(state: &'static str, periods: &[(&str, Option<f64>, bool)]) -> TargetReport {
    TargetReport {
        kpi: "phase.work".into(),
        stratum: "change=fix".into(),
        stat: "median",
        min: None,
        max: Some(3600.0),
        source: "host",
        state,
        streak: 3,
        breach_since: (state == "breach").then(|| "2026-09-22".to_owned()),
        periods: periods
            .iter()
            .map(|(period, value, judged)| TargetPeriod {
                period: (*period).to_owned(),
                value: *value,
                n: 5,
                judged: *judged,
                reason: (!judged).then_some("partial"),
                met: judged.then_some(false),
            })
            .collect(),
    }
}

#[test]
fn a_breach_starts_with_its_latest_judged_value_and_ends_when_met() {
    let breach = target(
        "breach",
        &[
            ("2026-09-24", Some(4000.0), true),
            ("2026-09-25", None, false),
        ],
    );
    let started = breach_started("day", &breach).unwrap();
    assert_eq!(
        started,
        json!({
            "period": "day", "kpi": "phase.work", "stratum": "change=fix",
            "stat": "median", "min": null, "max": 3600.0, "source": "host",
            "since": "2026-09-22", "streak": 3, "label": "2026-09-24", "value": 4000.0,
        })
    );
    assert_eq!(
        breach_key(&started),
        ("day".into(), "phase.work".into(), "change=fix".into())
    );
    assert_eq!(breach_started("day", &target("missed", &[])), None);

    // Still in breach, or not judged: it goes on.
    assert_eq!(
        breach_resolved(&started, std::slice::from_ref(&breach)),
        None
    );
    assert_eq!(
        breach_resolved(&started, &[target("not_judged", &[])]),
        None
    );
    let met = target("ok", &[("2026-09-25", Some(100.0), true)]);
    assert_eq!(
        breach_resolved(&started, &[met]).unwrap(),
        json!({"period": "day", "kpi": "phase.work", "stratum": "change=fix",
               "label": "2026-09-25", "reason": "met"})
    );
    assert_eq!(
        breach_resolved(&started, &[])
            .unwrap()
            .get("reason")
            .and_then(Value::as_str),
        Some("target_removed")
    );
}

#[test]
fn a_breach_message_carries_its_kpi_value_and_target() {
    let started = breach_started(
        "day",
        &target("breach", &[("2026-09-24", Some(4000.0), true)]),
    )
    .unwrap();
    let message = breach_message("dagq", "/q/queue.db", &started);
    assert_eq!(message.kind, PushKind::Breach);
    assert_eq!(message.period, "2026-09-24");
    assert_eq!(message.report_html, None);
    let body = &message.body;
    assert_eq!(body["kind"], "breach");
    assert_eq!(body["queue"], "/q/queue.db");
    assert_eq!(body["title"], "dagq target breach: phase.work (change=fix)");
    assert_eq!(
        body["text"],
        "phase.work (change=fix): 1h 06m (median, target ≤ 1h 00m), missed 3 period(s) in a row (since 2026-09-22)"
    );
    assert_eq!(body["breaches"][0]["periods"], 3);
    assert_eq!(body["breaches"][0]["value"], 4000.0);
    let stdin = message.stdin();
    assert_eq!(stdin.last(), Some(&b'\n'));
    let parsed: Value = serde_json::from_slice(&stdin).unwrap();
    assert_eq!(&parsed, body);
}

#[test]
fn a_summary_carries_the_kpis_breaches_misses_resolved_and_asks() {
    let thursday = MONDAY + 3 * DAY + 12 * HOUR;
    let mut report = report(MONDAY + 4 * DAY + 12 * HOUR, thursday, Period::Day);
    let mut missed = report.kpi.targets[0].clone();
    missed.kpi = "revise_rate".into();
    missed.state = "missed";
    missed.min = None;
    missed.max = Some(0.2);
    report.kpi.targets.push(missed);
    let resolved =
        [json!({"period": "day", "kpi": "first_pass_rate", "stratum": "all", "reason": "met"})];
    let message = summary_message(
        "dagq",
        "/q/queue.db",
        &report,
        &resolved,
        2,
        (Path::new("/r/d.html"), Path::new("/r/d.json")),
    );
    assert_eq!(message.kind, PushKind::Daily);
    assert_eq!(message.period, "2026-09-24");
    assert_eq!(message.report_html.as_deref(), Some("/r/d.html"));
    let body = &message.body;
    assert_eq!(body["title"], "dagq 2026-09-24: landings 1, breaches 1");
    assert_eq!(body["breaches"][0]["kpi"], "landings");
    assert_eq!(body["breaches"][0]["min"], 2.0);
    assert_eq!(body["missed"][0]["kpi"], "revise_rate");
    assert_eq!(body["resolved"][0]["kpi"], "first_pass_rate");
    assert_eq!(body["open_asks"], 2);
    assert_eq!(body["report_json"], "/r/d.json");
    let text = body["text"].as_str().unwrap();
    for line in [
        "landings 1",
        "lead_time ",
        "Breaches:\n- landings (all): 1 (value, target ≥ 2)",
        "Missed (1 period):\n- revise_rate (all)",
        "Resolved:\n- first_pass_rate (all)",
        "open asks: 2",
        "report: /r/d.html",
    ] {
        assert!(text.contains(line), "{line} in {text}");
    }

    let week =
        super::super::report::tests::report(MONDAY + 7 * DAY + 12 * HOUR, thursday, Period::Week);
    let message = summary_message("dagq", "q", &week, &[], 0, (Path::new("h"), Path::new("j")));
    assert_eq!(message.kind, PushKind::Weekly);
    assert_eq!(message.body["kind"], "weekly");
    assert_eq!(message.period, "2026-W39");
}

#[test]
fn a_message_is_tried_three_times_in_all() {
    assert_eq!(retry_after(1), Some(60));
    assert_eq!(retry_after(2), Some(300));
    assert_eq!(retry_after(3), None);
    assert_eq!(retry_after(0), None);
    let config = PushConfig::new(vec!["x".into()]);
    assert_eq!(
        (
            config.timeout_secs,
            config.daily,
            config.breach,
            config.max_breach_per_day
        ),
        (30, true, true, 3)
    );
}

#[test]
fn the_stderr_kept_is_a_tail_without_the_arguments() {
    let command = [
        "/bin/push".to_owned(),
        "https://hooks.example/T0/B0/secret".to_owned(),
        "-v".to_owned(),
    ];
    let tail = stderr_tail(
        "posting to https://hooks.example/T0/B0/secret -v failed\n",
        &command,
    );
    assert_eq!(tail, "posting to [argument] -v failed");
    let long = "é".repeat(STDERR_TAIL_BYTES);
    let tail = stderr_tail(&long, &command);
    assert!(tail.starts_with('…'));
    assert!(tail.len() <= STDERR_TAIL_BYTES + '…'.len_utf8());
}
