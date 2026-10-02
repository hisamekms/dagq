//! Days and weeks, partial periods, null measures, targets and their
//! breaches, the host's settings, `--since`/`--until` windows and `--goal`.

use super::*;

/// Days start at local midnight and weeks on Monday; the labels are the
/// local date and the ISO week, across a year's end too.
#[test]
fn periods_start_at_local_midnight_and_monday() {
    let offset = JST * 1000;
    let noon = (MONDAY + 2 * DAY + 12 * HOUR) * 1000;
    let day = Period::Day.start(noon, offset);
    assert_eq!(day, (MONDAY + 2 * DAY) * 1000);
    assert_eq!(Period::Day.label(day, offset), "2026-09-23");
    // 08:59 in Tokyo is still the day before in UTC, but the local day's.
    let early = (MONDAY + 2 * DAY + 30 * 60) * 1000;
    assert_eq!(Period::Day.start(early, offset), day);
    let week = Period::Week.start(noon, offset);
    assert_eq!(week, MONDAY * 1000);
    assert_eq!(Period::Week.label(week, offset), "2026-W39");
    // 2027-01-01 is a Friday: its week is 2026's 53rd.
    let new_year = marks_ms("2027-01-01T12:00:00Z");
    assert_eq!(
        Period::Week.label(Period::Week.start(new_year, 0), 0),
        "2026-W53"
    );
    assert_eq!("week".parse::<Period>(), Ok(Period::Week));
    assert!("month".parse::<Period>().is_err());
    assert_eq!("load".parse::<Axis>(), Ok(Axis::Load));
    assert_eq!("provider".parse::<Axis>(), Ok(Axis::Provider));
    assert_eq!("route".parse::<Axis>(), Ok(Axis::Route));
    assert_eq!("codex".parse::<Axis>(), Ok(Axis::Codex));
    assert_eq!("group".parse::<Axis>(), Ok(Axis::Group));
    assert_eq!("nature".parse::<Axis>(), Ok(Axis::Nature));
    assert!("host".parse::<Axis>().unwrap_err().contains("toolchain"));
}

/// Spreads follow `stats`' rules, a rate over nothing is null, and a count
/// is judged whatever its size.
#[test]
fn measures_keep_null_apart_from_zero() {
    let secs = Measure::secs([40, 10, 30, 20]);
    assert_eq!(
        (secs.n, secs.median, secs.p90, secs.min, secs.max),
        (4, Some(25.0), Some(40.0), Some(10.0), Some(40.0))
    );
    assert_eq!(Measure::secs([]).median, None);
    let rate = Measure::ratio(1.0, 3);
    assert_eq!(rate.value, Some(0.333));
    assert_eq!(Measure::ratio(0.0, 0).value, None);
    assert_eq!(
        serde_json::to_value(Measure::ratio(0.0, 0)).unwrap(),
        json!({"n": 0, "value": null})
    );
    let spread = Measure::spread([1.5, 0.5, 2.25]);
    assert_eq!((spread.median, spread.p90), (Some(1.5), Some(2.25)));
    assert!(Measure::count(1).enough(5));
    assert!(!secs.enough(5));
    assert_eq!(secs.primary(), Some(25.0));
    assert_eq!(Measure::count(2).primary(), Some(2.0));
    assert_eq!(direction("landings"), Some(Direction::Higher));
    assert_eq!(direction("phase.work"), Some(Direction::Lower));
    assert_eq!(direction("session_open.inbox"), None);
    assert_eq!(direction("session_open.worker"), Some(Direction::Lower));
    assert_eq!(direction("auto_repairs"), None);
}

/// Today is listed as partial, and no target is judged on it.
#[test]
fn the_period_not_over_yet_is_partial() {
    let mut queue = Queue::default();
    queue.run(&Run::new(1, None, MONDAY + HOUR, 300));
    let config = KpiConfig::merge(
        Some(&KpiSettings {
            targets: vec![Target {
                kpi: "landings".into(),
                change: None,
                area: None,
                stat: None,
                min: Some(1.0),
                max: None,
            }],
            ..KpiSettings::default()
        }),
        None,
    );
    let result = queue.kpi(MONDAY + 3 * HOUR, &config, &KpiQuery::default());
    let today = result.periods.last().unwrap();
    assert!(today.partial);
    assert_eq!(measure(today, "landings", ALL).value, Some(1.0));
    let target = &result.targets[0];
    assert_eq!(target.periods.last().unwrap().reason, Some("partial"));
    // Shown next to yesterday, but not judged.
    let landings = &today.comparison["landings"][ALL];
    assert_eq!(
        (landings.delta, landings.reason, landings.verdict),
        (Some(1.0), Some("partial"), None)
    );
    // The days before, with no landing, are judged and off target.
    assert_eq!(target.state, "breach");
    assert_eq!(target.periods[DEFAULT_LAST - 2].met, Some(false));
    assert_eq!(result.periods.len(), DEFAULT_LAST);
}

fn window_kpis(value: Option<f64>, n: usize) -> Kpis<Measure> {
    let mut measure = Measure::secs(std::iter::repeat_n(0, n));
    measure.median = value;
    let mut kpis = Kpis::new();
    kpis.entry("phase.work".into())
        .or_default()
        .insert("change=runtime".into(), measure);
    kpis
}

/// A target breaks after `breach_periods` judged periods off target in a
/// row; a period with too few samples is skipped without breaking the
/// streak; one period off target is only `missed`, and one on target ends
/// the breach.
#[test]
fn a_breach_needs_consecutive_judged_periods_off_target() {
    let config = KpiConfig::merge(
        Some(&KpiSettings {
            min_samples: Some(3),
            targets: vec![Target {
                kpi: "phase.work".into(),
                change: Some("runtime".into()),
                area: None,
                stat: Some(Stat::Median),
                min: None,
                max: Some(100.0),
            }],
            ..KpiSettings::default()
        }),
        None,
    );
    let days: Vec<(String, Kpis<Measure>)> = [
        (Some(50.0), 5),  // ok
        (Some(150.0), 5), // off
        (Some(150.0), 2), // too few: skipped
        (None, 0),        // nothing: skipped
        (Some(160.0), 4), // off
        (Some(170.0), 9), // off: the third in a row
    ]
    .into_iter()
    .enumerate()
    .map(|(day, (value, n))| (format!("d{day}"), window_kpis(value, n)))
    .collect();
    let periods = |upto: usize| -> Vec<JudgedPeriod<'_>> {
        days[..upto]
            .iter()
            .map(|(label, kpis)| JudgedPeriod {
                label,
                kpis,
                partial: false,
                listed: true,
            })
            .collect()
    };
    let breach = &judge(&config, &periods(6), 3)[0];
    assert_eq!(breach.state, "breach");
    assert_eq!(breach.streak, 3);
    assert_eq!(breach.breach_since.as_deref(), Some("d1"));
    assert_eq!(breach.stratum, "change=runtime");
    assert_eq!(breach.source, "repository");
    let reasons: Vec<Option<&str>> = breach.periods.iter().map(|p| p.reason).collect();
    assert_eq!(
        reasons,
        [
            None,
            None,
            Some("small_sample"),
            Some("no_value"),
            None,
            None
        ]
    );
    assert_eq!(breach.periods[1].met, Some(false));
    assert_eq!(breach.periods[2].met, None);
    let missed = &judge(&config, &periods(2), 3)[0];
    assert_eq!((missed.state, missed.streak), ("missed", 1));
    assert_eq!(judge(&config, &periods(1), 3)[0].state, "ok");
    assert_eq!(judge(&config, &periods(0), 3)[0].state, "not_judged");
    // Two weeks off target are a breach at `breach_weeks` 2.
    assert_eq!(judge(&config, &periods(6)[4..], 2)[0].state, "breach");
    // A judged period on target after the streak ends the breach.
    let mut recovered = days.clone();
    recovered.push(("d6".into(), window_kpis(Some(90.0), 5)));
    let periods: Vec<JudgedPeriod<'_>> = recovered
        .iter()
        .map(|(label, kpis)| JudgedPeriod {
            label,
            kpis,
            partial: false,
            listed: false,
        })
        .collect();
    let report = &judge(&config, &periods, 3)[0];
    assert_eq!((report.state, report.streak), ("ok", 0));
    assert!(report.periods.is_empty());
}

/// The host's settings win over the repository's, but not its
/// `max_improvement_proposals`; a target of the same KPI and stratum is
/// the host's, and no target is built in.
#[test]
fn the_host_settings_win_over_the_repository() {
    let target = |kpi: &str, change: Option<&str>, max: f64| Target {
        kpi: kpi.into(),
        change: change.map(Into::into),
        area: None,
        stat: None,
        min: None,
        max: Some(max),
    };
    let repository = KpiSettings {
        min_samples: Some(7),
        breach_periods: Some(4),
        max_improvement_proposals: Some(1),
        targets: vec![
            target("phase.work", Some("runtime"), 3600.0),
            target("revise_rate", None, 0.3),
        ],
        ..KpiSettings::default()
    };
    let host = KpiSettings {
        min_samples: Some(2),
        max_improvement_proposals: Some(9),
        targets: vec![target("phase.work", Some("runtime"), 1800.0)],
        ..KpiSettings::default()
    };
    let config = KpiConfig::merge(Some(&repository), Some(&host));
    assert_eq!(config.min_samples, 2);
    assert_eq!(config.breach_periods, 4);
    assert_eq!(config.breach_weeks, config::DEFAULT_BREACH_WEEKS);
    assert_eq!(config.max_improvement_proposals, 1);
    assert_eq!(config.sources["min_samples"], "host");
    assert_eq!(config.sources["breach_periods"], "repository");
    assert_eq!(config.sources["breach_weeks"], "default");
    assert_eq!(config.targets.len(), 2);
    assert_eq!(config.targets[0].target.max, Some(1800.0));
    assert_eq!(config.targets[0].source, "host");
    assert_eq!(config.targets[1].source, "repository");
    assert!(KpiConfig::default().targets.is_empty());
    let overlaid = repository.clone().overlay(host);
    assert_eq!(overlaid.min_samples, Some(2));
    assert_eq!(overlaid.targets.len(), 2);
}

/// `--since` / `--until` give one window next to the one of the same
/// length before it.
#[test]
fn one_window_of_any_length() {
    let mut queue = Queue::default();
    queue.run(&Run::new(1, None, MONDAY, 100));
    queue.run(&Run::new(2, None, MONDAY + 3 * HOUR, 100));
    let query = KpiQuery {
        since: Some(Cursor::Time((MONDAY + HOUR) * 1000)),
        until: Some(Cursor::Time((MONDAY + 5 * HOUR) * 1000)),
        ..KpiQuery::default()
    };
    let result = queue.kpi(MONDAY + DAY, &KpiConfig::default(), &query);
    assert_eq!(result.period, "window");
    assert_eq!(result.periods.len(), 1);
    let window = &result.periods[0];
    assert!(!window.partial);
    assert_eq!(measure(window, "landings", ALL).value, Some(1.0));
    assert_eq!(window.comparison["landings"][ALL].previous, Some(1.0));
    let error = kpi(
        &KpiInput {
            events: &queue.events,
            goals: &queue.goals,

            changes: &queue.changes,
            areas: None,
            heartbeats: &queue.heartbeats,
            draft_origins: &queue.draft_origins,
            now: MONDAY + DAY,
            utc_offset_secs: 0,
            cores: None,
            dagq_source: true,
            config: &KpiConfig::default(),
            host: None,
        },
        &KpiQuery {
            since: query.until,
            until: query.since,
            ..KpiQuery::default()
        },
    )
    .unwrap_err();
    assert!(error.contains("before --until"), "{error}");
}

/// With `--goal`, only that goal's runs count.
#[test]
fn a_goal_keeps_only_its_runs() {
    let mut queue = Queue::default();
    queue.run(&Run::new(1, None, MONDAY, 100));
    queue.run(&Run::new(2, None, MONDAY, 100));
    queue.goals.insert(TaskId::new(1), Some(GoalId::new(5)));
    let query = KpiQuery {
        goal_id: Some(GoalId::new(5)),
        last: 1,
        ..KpiQuery::default()
    };
    let result = queue.kpi(MONDAY + 12 * HOUR, &KpiConfig::default(), &query);
    assert_eq!(
        measure(&result.periods[0], "landings", ALL).value,
        Some(1.0)
    );
    assert_eq!(result.config.min_samples, config::DEFAULT_MIN_SAMPLES);
}
