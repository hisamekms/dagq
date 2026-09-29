//! What the observer reads of the KPIs (ADR-0051 decision 24): the days'
//! and the weeks' targets as `kpi` judged them, each breach with the
//! `kpi_breach_started` event that is its finding's evidence, the marks
//! that took effect since the breach started, and the periods' trend (the
//! KPIs that worsened against the period before, and the marks). The
//! observer copies these numbers into its `kpi` findings and makes none
//! of its own; a breach of one KPI and stratum is one finding whatever
//! its period (`subject` `<kpi>/<stratum>`, ADR-0044 decision 18).
//!
//! The forecast's scoring KPIs (`forecast.*`, ADR-0070 decisions 4 and 5)
//! are read the same way: their periods' values are listed under
//! `forecast`, and a breach of their targets (the bias that goes on) is a
//! finding of kind `forecast` whose subject drops the `forecast.` prefix
//! (`<metric>/<stratum>`), never a `kpi` finding.
use serde_json::{Value, json};

use super::{ALL, Kpi, PeriodKpis, TargetReport, push::breach_key};
use crate::domain::EventId;

/// The kind of the observer's findings of a breach (ADR-0051 decision 24).
pub const FINDING_KIND: &str = "kpi";

/// The kind of the observer's findings of a breach of the forecast's
/// scoring KPIs (ADR-0070 decision 5).
pub const FORECAST_FINDING_KIND: &str = "forecast";

/// The prefix of the forecast's scoring KPIs.
const FORECAST_PREFIX: &str = "forecast.";

/// The subject of the finding of `kpi`'s breach in `stratum`: of a
/// `forecast.*` KPI without the prefix.
pub fn subject(kpi: &str, stratum: &str) -> String {
    let metric = kpi.strip_prefix(FORECAST_PREFIX).unwrap_or(kpi);
    format!("{metric}/{stratum}")
}

/// The kind of the finding of `kpi`'s breach: `forecast` for the
/// forecast's scoring KPIs, `kpi` for the rest.
pub fn finding_kind(kpi: &str) -> &'static str {
    if kpi.starts_with(FORECAST_PREFIX) {
        FORECAST_FINDING_KIND
    } else {
        FINDING_KIND
    }
}

/// The observer's KPI input: `day` and `week` are `kpi` of the days and of
/// the weeks at the same moment; `open` the breaches recorded as started
/// and not resolved, each with its event.
pub fn observer_input(day: &Kpi, week: &Kpi, open: &[(EventId, Value)]) -> Value {
    let mut breaches = Vec::new();
    let mut targets = Vec::new();
    for (period, kpi) in [("day", day), ("week", week)] {
        for target in &kpi.targets {
            targets.push(target_entry(period, target));
            if target.state == "breach" {
                breaches.push(breach_entry(period, target, &kpi.periods, open));
            }
        }
    }
    json!({
        "config": {
            "min_samples": day.config.min_samples,
            "breach_periods": day.config.breach_periods,
            "breach_weeks": day.config.breach_weeks,
        },
        "breaches": breaches,
        "targets": targets,
        "trend": {
            "day": day.periods.iter().map(trend_entry).collect::<Vec<_>>(),
            "week": week.periods.iter().map(trend_entry).collect::<Vec<_>>(),
        },
        "forecast": {
            "day": day.periods.iter().filter_map(forecast_entry).collect::<Vec<_>>(),
            "week": week.periods.iter().filter_map(forecast_entry).collect::<Vec<_>>(),
        },
    })
}

/// A period's scoring of the forecast (ADR-0070 decision 4), for a period
/// with a sample or a row left out: the counts (`details.forecast`) and
/// each `forecast.*` KPI in the strata that have samples.
fn forecast_entry(period: &PeriodKpis) -> Option<Value> {
    let details = period.window.details.get("forecast")?;
    let excluded: u64 = details["excluded"]
        .as_object()
        .map(|reasons| reasons.values().filter_map(Value::as_u64).sum())
        .unwrap_or_default();
    if details["samples"].as_u64().unwrap_or_default() == 0 && excluded == 0 {
        return None;
    }
    let kpis: serde_json::Map<String, Value> = period
        .window
        .kpis
        .iter()
        .filter(|(kpi, _)| kpi.starts_with(FORECAST_PREFIX))
        .map(|(kpi, strata)| {
            let strata: serde_json::Map<String, Value> = strata
                .iter()
                .filter(|(_, measure)| measure.n > 0)
                .map(|(stratum, measure)| (stratum.clone(), json!(measure)))
                .collect();
            (kpi.clone(), Value::Object(strata))
        })
        .collect();
    Some(json!({
        "label": period.label,
        "partial": period.partial,
        "details": details,
        "kpis": kpis,
    }))
}

fn target_entry(period: &str, target: &TargetReport) -> Value {
    json!({
        "period": period,
        "kpi": target.kpi,
        "stratum": target.stratum,
        "stat": target.stat,
        "min": target.min,
        "max": target.max,
        "state": target.state,
        "streak": target.streak,
        "breach_since": target.breach_since,
        "values": target.periods.iter().map(|p| json!({
            "period": p.period,
            "value": p.value,
            "n": p.n,
            "met": p.met,
            "reason": p.reason,
        })).collect::<Vec<_>>(),
    })
}

/// A breach as its finding is recorded: the kind, subject and evidence
/// (the open `kpi_breach_started` of the same period, KPI and stratum;
/// null before the supervisor recorded it), the latest judged value, and
/// the marks of the periods since the breach started.
fn breach_entry(
    period: &str,
    target: &TargetReport,
    periods: &[PeriodKpis],
    open: &[(EventId, Value)],
) -> Value {
    let key = (
        period.to_owned(),
        target.kpi.clone(),
        target.stratum.clone(),
    );
    let evidence = open
        .iter()
        .find(|(_, payload)| breach_key(payload) == key)
        .map(|(id, _)| *id);
    let latest = target.periods.iter().rev().find(|p| p.judged);
    let since = target.breach_since.as_deref().unwrap_or_default();
    let marks: Vec<Value> = periods
        .iter()
        .filter(|p| p.label.as_str() >= since)
        .flat_map(|p| p.marks.iter().map(move |mark| (p, mark)))
        .map(|(p, mark)| json!({"period": p.label, "label": mark.label, "kind": mark.kind, "at": mark.at}))
        .collect();
    json!({
        "finding_kind": finding_kind(&target.kpi),
        "subject": subject(&target.kpi, &target.stratum),
        "evidence_event_id": evidence,
        "period": period,
        "kpi": target.kpi,
        "stratum": target.stratum,
        "stat": target.stat,
        "min": target.min,
        "max": target.max,
        "value": latest.and_then(|p| p.value),
        "latest_period": latest.map(|p| &p.period),
        "streak": target.streak,
        "breach_since": target.breach_since,
        "marks": marks,
    })
}

/// A period of the trend: its runs, its marks, and the KPIs of the whole
/// queue and of each change of task judged worse than the period before.
fn trend_entry(period: &PeriodKpis) -> Value {
    let worsened: Vec<Value> = period
        .comparison
        .iter()
        .flat_map(|(kpi, strata)| strata.iter().map(move |(stratum, c)| (kpi, stratum, c)))
        .filter(|(_, stratum, _)| stratum.as_str() == ALL || stratum.starts_with("change="))
        .filter(|(_, _, change)| change.verdict == Some("worsened"))
        .map(|(kpi, stratum, change)| {
            json!({
                "kpi": kpi,
                "stratum": stratum,
                "previous": change.previous,
                "delta": change.delta,
                "ratio": change.ratio,
            })
        })
        .collect();
    json!({
        "label": period.label,
        "partial": period.partial,
        "runs": period.window.runs,
        "marks": period.marks.iter().map(|m| json!({"label": m.label, "kind": m.kind, "at": m.at})).collect::<Vec<_>>(),
        "worsened": worsened,
    })
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;
    use crate::domain::{
        kpi::{Change, ConfigReport, WindowKpis, config::TargetPeriod},
        marks::Mark,
    };

    fn target(kpi: &str, stratum: &str, state: &'static str, since: Option<&str>) -> TargetReport {
        TargetReport {
            kpi: kpi.into(),
            stratum: stratum.into(),
            stat: "median",
            min: None,
            max: Some(3600.0),
            source: "repository",
            state,
            streak: if state == "breach" { 3 } else { 0 },
            breach_since: since.map(Into::into),
            periods: vec![
                TargetPeriod {
                    period: "2026-09-25".into(),
                    value: Some(4000.0),
                    n: 9,
                    judged: true,
                    reason: None,
                    met: Some(false),
                },
                TargetPeriod {
                    period: "2026-09-26".into(),
                    value: Some(4100.0),
                    n: 5,
                    judged: false,
                    reason: Some("partial"),
                    met: None,
                },
            ],
        }
    }

    fn period(label: &str, marks: &[&str], worsened: Option<(&str, &str)>) -> PeriodKpis {
        let mut comparison = BTreeMap::new();
        if let Some((kpi, stratum)) = worsened {
            let change = |verdict| Change {
                previous: Some(100.0),
                delta: Some(50.0),
                ratio: Some(1.5),
                judged: true,
                verdict: Some(verdict),
                ..Change::default()
            };
            comparison.insert(
                kpi.to_owned(),
                BTreeMap::from([
                    (stratum.to_owned(), change("worsened")),
                    // Other axes than the changes are left out, and so is
                    // what did not worsen.
                    ("parallel=3".to_owned(), change("worsened")),
                    ("change=docs".to_owned(), change("improved")),
                ]),
            );
        }
        PeriodKpis {
            label: label.into(),
            start: String::new(),
            end: String::new(),
            partial: false,
            window: WindowKpis {
                runs: 7,
                kpis: BTreeMap::new(),
                details: BTreeMap::new(),
                unavailable: BTreeMap::new(),
            },
            marks: marks
                .iter()
                .map(|label| Mark {
                    id: None,
                    kind: "mark".into(),
                    at: format!("{label}-at"),
                    recorded_at: String::new(),
                    label: (*label).into(),
                    retracted_by: None,
                    detail: Value::Null,
                })
                .collect(),
            comparison,
            host: None,
        }
    }

    fn kpi(period: &'static str, targets: Vec<TargetReport>, periods: Vec<PeriodKpis>) -> Kpi {
        Kpi {
            period,
            utc_offset_secs: 0,
            cores: None,
            config: ConfigReport {
                min_samples: 5,
                breach_periods: 3,
                breach_weeks: 2,
                max_improvement_proposals: 2,
                sources: BTreeMap::new(),
            },
            periods,
            targets,
            compare: None,
        }
    }

    #[test]
    fn a_breach_carries_its_finding_subject_evidence_and_the_marks_since_it_started() {
        let day = kpi(
            "day",
            vec![
                target("phase.work", "change=runtime", "breach", Some("2026-09-24")),
                target("lead_time", "all", "missed", None),
            ],
            vec![
                period("2026-09-23", &["before the breach"], None),
                period("2026-09-24", &["parallel 4→3"], None),
                period("2026-09-25", &[], Some(("phase.work", "change=runtime"))),
            ],
        );
        let week = kpi(
            "week",
            vec![target(
                "phase.work",
                "change=runtime",
                "breach",
                Some("2026-W38"),
            )],
            vec![period("2026-W39", &[], None)],
        );
        let open = vec![
            (
                EventId::new(41),
                json!({"period": "week", "kpi": "phase.work", "stratum": "change=runtime"}),
            ),
            (
                EventId::new(40),
                json!({"period": "day", "kpi": "phase.work", "stratum": "change=runtime"}),
            ),
        ];
        let input = observer_input(&day, &week, &open);

        let breaches = input["breaches"].as_array().unwrap();
        assert_eq!(breaches.len(), 2, "{breaches:?}");
        // One KPI and stratum is one subject in the days and the weeks.
        for breach in breaches {
            assert_eq!(breach["finding_kind"], "kpi");
            assert_eq!(breach["subject"], "phase.work/change=runtime");
        }
        assert_eq!(breaches[0]["period"], "day");
        assert_eq!(breaches[0]["evidence_event_id"], 40);
        assert_eq!(breaches[0]["value"], 4000.0);
        assert_eq!(breaches[0]["latest_period"], "2026-09-25");
        assert_eq!(breaches[0]["streak"], 3);
        assert_eq!(
            breaches[0]["marks"],
            json!([{"period": "2026-09-24", "label": "parallel 4→3", "kind": "mark", "at": "parallel 4→3-at"}])
        );
        assert_eq!(breaches[1]["period"], "week");
        assert_eq!(breaches[1]["evidence_event_id"], 41);

        // A missed target is listed but is no breach.
        let targets = input["targets"].as_array().unwrap();
        assert_eq!(targets.len(), 3);
        assert_eq!(targets[1]["state"], "missed");
        assert_eq!(targets[1]["values"][1]["reason"], "partial");

        let trend = &input["trend"]["day"];
        assert_eq!(trend.as_array().unwrap().len(), 3);
        assert_eq!(trend[2]["runs"], 7);
        assert_eq!(
            trend[2]["worsened"],
            json!([{"kpi": "phase.work", "stratum": "change=runtime", "previous": 100.0, "delta": 50.0, "ratio": 1.5}])
        );
        assert_eq!(trend[0]["marks"][0]["label"], "before the breach");
        assert_eq!(input["config"]["breach_periods"], 3);
    }

    #[test]
    fn a_breach_not_recorded_yet_has_no_evidence() {
        let day = kpi(
            "day",
            vec![target("lead_time", "all", "breach", Some("2026-09-25"))],
            Vec::new(),
        );
        let week = kpi("week", Vec::new(), Vec::new());
        let input = observer_input(
            &day,
            &week,
            &[(
                EventId::new(3),
                json!({"period": "week", "kpi": "lead_time", "stratum": "all"}),
            )],
        );
        assert_eq!(input["breaches"][0]["evidence_event_id"], Value::Null);
        assert_eq!(input["breaches"][0]["subject"], "lead_time/all");
        assert_eq!(input["breaches"][0]["marks"], json!([]));
    }
    #[test]
    fn a_forecast_breach_is_a_forecast_finding_and_its_scoring_is_listed() {
        let mut scored = period("2026-09-25", &[], None);
        let mut ratio = BTreeMap::new();
        ratio.insert(
            "all".to_owned(),
            crate::domain::kpi::Measure::spread([0.5, 0.75]),
        );
        ratio.insert(
            "change=docs".to_owned(),
            crate::domain::kpi::Measure::spread(std::iter::empty()),
        );
        scored
            .window
            .kpis
            .insert("forecast.p50_error_ratio".into(), ratio);
        scored
            .window
            .kpis
            .insert("lead_time".into(), BTreeMap::new());
        scored.window.details.insert(
            "forecast",
            json!({"samples": 2, "excluded": {"canceled": 0}, "with_marks": 0}),
        );
        let mut empty = period("2026-09-24", &[], None);
        empty.window.details.insert(
            "forecast",
            json!({"samples": 0, "excluded": {"canceled": 0, "abandoned": 0}}),
        );
        let day = kpi(
            "day",
            vec![target(
                "forecast.p50_error_ratio",
                "change=runtime",
                "breach",
                Some("2026-09-25"),
            )],
            vec![empty, scored],
        );
        let week = kpi("week", Vec::new(), vec![period("2026-W39", &[], None)]);
        let input = observer_input(&day, &week, &[]);

        let breach = &input["breaches"][0];
        assert_eq!(breach["finding_kind"], "forecast");
        assert_eq!(breach["subject"], "p50_error_ratio/change=runtime");
        assert_eq!(breach["kpi"], "forecast.p50_error_ratio");

        // Only the period with samples, only the forecast's KPIs, and only
        // the strata with samples.
        let forecast = input["forecast"]["day"].as_array().unwrap();
        assert_eq!(forecast.len(), 1, "{forecast:?}");
        assert_eq!(forecast[0]["label"], "2026-09-25");
        assert_eq!(forecast[0]["details"]["samples"], 2);
        let kpis = forecast[0]["kpis"].as_object().unwrap();
        assert_eq!(
            kpis.keys().collect::<Vec<_>>(),
            ["forecast.p50_error_ratio"]
        );
        let strata = kpis["forecast.p50_error_ratio"].as_object().unwrap();
        assert_eq!(strata.keys().collect::<Vec<_>>(), ["all"]);
        assert_eq!(strata["all"]["n"], 2);
        assert_eq!(input["forecast"]["week"], json!([]));

        assert_eq!(finding_kind("lead_time"), "kpi");
        assert_eq!(subject("lead_time", "all"), "lead_time/all");
    }
}
