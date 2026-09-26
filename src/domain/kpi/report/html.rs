//! The report as one self-contained HTML page (ADR-0051 decision 20): the
//! styles and the charts (inline SVG) are in the page, and nothing is
//! loaded from anywhere else — no script, stylesheet, font, image or CDN.
//! Readable without JavaScript, which it does not use.
use std::fmt::Write;

use super::{Report, ReportFile};
use crate::domain::kpi::{ALL, Change, Measure, PeriodKpis, TargetReport};

/// The KPIs the trend lists first, in this order; the others follow by name.
const LEADING: &[&str] = &[
    "landings",
    "lead_time",
    "phase.startup",
    "phase.work",
    "phase.validate",
    "phase.wait_to_land",
    "first_pass_rate",
    "revise_rate",
    "conflict_rate",
    "verification_failed_rate",
    "failed_rate",
    "resumes_per_run",
    "slot_usage",
    "asks_per_landing",
    "ask_wait",
    "max_load_avg",
    "findings_open",
];

const STYLE: &str = r#"
:root{--bg:#fbfbfa;--fg:#1f2328;--muted:#656d76;--line:#d8dee4;--panel:#ffffff;--accent:#3b6fb6;--accent-soft:#9db8dd;--bad:#c0392b;--bad-bg:#fbe9e7;--warn:#b7791f;--warn-bg:#fdf3e1;--good:#2e7d4f}
@media (prefers-color-scheme: dark){:root:not([data-theme="light"]){--bg:#15181c;--fg:#e6e8eb;--muted:#9aa4ae;--line:#2d333b;--panel:#1c2127;--accent:#79a6e0;--accent-soft:#3d5675;--bad:#f28b82;--bad-bg:#3a2020;--warn:#f0b35a;--warn-bg:#3a2f1c;--good:#7fd09a}}
:root[data-theme="dark"]{--bg:#15181c;--fg:#e6e8eb;--muted:#9aa4ae;--line:#2d333b;--panel:#1c2127;--accent:#79a6e0;--accent-soft:#3d5675;--bad:#f28b82;--bad-bg:#3a2020;--warn:#f0b35a;--warn-bg:#3a2f1c;--good:#7fd09a}
*{box-sizing:border-box}
body{margin:0;background:var(--bg);color:var(--fg);font:14px/1.5 -apple-system,BlinkMacSystemFont,"Segoe UI",system-ui,sans-serif}
main{max-width:1100px;margin:0 auto;padding:24px 16px 48px}
h1{font-size:22px;margin:0 0 4px}
h2{font-size:16px;margin:28px 0 8px}
.meta{color:var(--muted);margin:0}
.badge{display:inline-block;padding:1px 8px;border-radius:10px;font-size:12px;background:var(--warn-bg);color:var(--warn);margin-left:6px}
.scroll{overflow-x:auto}
table{border-collapse:collapse;width:100%;background:var(--panel)}
th,td{border-bottom:1px solid var(--line);padding:4px 8px;text-align:left;vertical-align:middle;white-space:nowrap}
td.num,th.num{text-align:right;font-variant-numeric:tabular-nums}
td.text{white-space:normal}
th{color:var(--muted);font-weight:600;font-size:12px}
tr.breach td{background:var(--bad-bg)}
tr.missed td{background:var(--warn-bg)}
.state-breach,.worsened{color:var(--bad);font-weight:600}
.state-missed{color:var(--warn);font-weight:600}
.state-ok,.improved{color:var(--good)}
.muted{color:var(--muted)}
svg.spark{display:block}
svg.spark .bar{fill:var(--accent)}
svg.spark .bar.partial{fill:var(--accent-soft)}
svg.spark .base{stroke:var(--line)}
svg.spark .mark{fill:var(--warn)}
details{margin-top:12px}
summary{cursor:pointer;color:var(--accent)}
ul{padding-left:20px}
a{color:var(--accent)}
"#;

/// The page of `report`.
pub fn render_html(report: &Report) -> String {
    let header = &report.report;
    let mut page = String::new();
    let title = format!("dagq KPI {}", header.label);
    open_page(&mut page, &title);
    let latest = report.kpi.periods.last();
    let _ = write!(
        page,
        "<header><h1>KPI report · {}{}</h1><p class=\"meta\">{} · {} – {} · {} run(s) finished · generated {} · {}</p></header>",
        esc(&header.label),
        if header.partial {
            "<span class=\"badge\">partial</span>"
        } else {
            ""
        },
        esc(header.period),
        esc(latest.map_or("", |p| &p.start)),
        esc(latest.map_or("", |p| &p.end)),
        latest.map_or(0, |p| p.window.runs),
        esc(&header.generated_at),
        esc(&header.build),
    );
    targets(&mut page, &report.kpi.targets);
    trend(&mut page, &report.kpi.periods);
    marks(&mut page, &report.kpi.periods);
    if let Some(latest) = latest {
        kpis(&mut page, latest);
    }
    findings(&mut page, report);
    if let Some(latest) = latest.filter(|p| !p.window.unavailable.is_empty()) {
        page.push_str("<h2>Not recorded</h2><ul>");
        for (kpi, reason) in &latest.window.unavailable {
            let _ = write!(page, "<li><code>{}</code>: {}</li>", esc(kpi), esc(reason));
        }
        page.push_str("</ul>");
    }
    let config = &report.kpi.config;
    let _ = write!(
        page,
        "<p class=\"meta\">min_samples {} · breach after {} day(s) / {} week(s) · UTC offset {}s · the values are derived from run_events again by <code>dagq kpi</code></p>",
        config.min_samples, config.breach_periods, config.breach_weeks, report.kpi.utc_offset_secs
    );
    page.push_str("</main></body></html>\n");
    page
}

/// The page that lists the reports, newest first: `daily` and `weekly`.
pub fn index_html(daily: &[ReportFile], weekly: &[ReportFile], generated_at: &str) -> String {
    let mut page = String::new();
    open_page(&mut page, "dagq KPI reports");
    let _ = write!(
        page,
        "<header><h1>KPI reports</h1><p class=\"meta\">updated {}</p></header>",
        esc(generated_at)
    );
    for (heading, files) in [("Daily", daily), ("Weekly", weekly)] {
        let _ = write!(page, "<h2>{heading}</h2>");
        if files.is_empty() {
            page.push_str("<p class=\"muted\">none yet</p>");
            continue;
        }
        let mut files: Vec<&ReportFile> = files.iter().collect();
        files.sort_by(|a, b| b.label.cmp(&a.label).then(a.partial.cmp(&b.partial)));
        page.push_str("<ul>");
        for file in files {
            let _ = write!(
                page,
                "<li><a href=\"{}\">{}</a>{} <a class=\"muted\" href=\"{}\">json</a></li>",
                esc(&file.path("html")),
                esc(&file.label),
                if file.partial {
                    "<span class=\"badge\">partial</span>"
                } else {
                    ""
                },
                esc(&file.path("json")),
            );
        }
        page.push_str("</ul>");
    }
    page.push_str("</main></body></html>\n");
    page
}

fn open_page(page: &mut String, title: &str) {
    let _ = write!(
        page,
        "<!doctype html>\n<html lang=\"en\"><head><meta charset=\"utf-8\"><meta name=\"viewport\" content=\"width=device-width, initial-scale=1\"><title>{}</title><style>{STYLE}</style></head><body><main>",
        esc(title)
    );
}

fn targets(page: &mut String, targets: &[TargetReport]) {
    page.push_str("<h2>Targets</h2>");
    if targets.is_empty() {
        page.push_str(
            "<p class=\"muted\">No target is set ([kpi.targets] of dagq.toml or host.toml).</p>",
        );
        return;
    }
    let rank = |state: &str| match state {
        "breach" => 0,
        "missed" => 1,
        "not_judged" => 2,
        _ => 3,
    };
    let mut sorted: Vec<&TargetReport> = targets.iter().collect();
    sorted.sort_by_key(|target| rank(target.state));
    page.push_str("<div class=\"scroll\"><table><tr><th>KPI</th><th>stratum</th><th>stat</th><th>target</th><th class=\"num\">latest</th><th>state</th><th class=\"num\">streak</th><th>since</th><th>source</th></tr>");
    for target in sorted {
        let bound = [
            target
                .min
                .map(|min| format!("≥ {}", value(&target.kpi, Some(min)))),
            target
                .max
                .map(|max| format!("≤ {}", value(&target.kpi, Some(max)))),
        ]
        .into_iter()
        .flatten()
        .collect::<Vec<_>>()
        .join(", ");
        let latest = target.periods.iter().rev().find_map(|period| period.value);
        let _ = write!(
            page,
            "<tr class=\"{state}\"><td><code>{}</code></td><td>{}</td><td>{}</td><td>{}</td><td class=\"num\">{}</td><td class=\"state-{state}\">{state}</td><td class=\"num\">{}</td><td>{}</td><td>{}</td></tr>",
            esc(&target.kpi),
            esc(&target.stratum),
            esc(target.stat),
            esc(&bound),
            value(&target.kpi, latest),
            target.streak,
            esc(target.breach_since.as_deref().unwrap_or("")),
            esc(target.source),
            state = esc(target.state),
        );
    }
    page.push_str("</table></div>");
}

/// The KPIs of the `all` stratum in the order the trend lists them.
fn ordered(periods: &[PeriodKpis]) -> Vec<&str> {
    let mut names: Vec<&str> = periods
        .iter()
        .flat_map(|period| period.window.kpis.iter())
        .filter(|(_, strata)| strata.get(ALL).is_some_and(|m| m.primary().is_some()))
        .map(|(name, _)| name.as_str())
        .collect();
    names.sort_unstable_by_key(|name| {
        (
            LEADING
                .iter()
                .position(|lead| lead == name)
                .unwrap_or(LEADING.len()),
            *name,
        )
    });
    names.dedup();
    names
}

fn trend(page: &mut String, periods: &[PeriodKpis]) {
    let (Some(first), Some(last)) = (periods.first(), periods.last()) else {
        return;
    };
    let _ = write!(
        page,
        "<h2>Trend</h2><p class=\"meta\">{} to {}, one bar per {}; a triangle marks a period with a change mark (listed below), a pale bar a period not over yet.</p>",
        esc(&first.label),
        esc(&last.label),
        if last.label.contains("-W") {
            "week"
        } else {
            "day"
        },
    );
    page.push_str("<div class=\"scroll\"><table><tr><th>KPI</th><th>trend</th><th class=\"num\">latest</th><th class=\"num\">previous</th><th class=\"num\">change</th><th>verdict</th></tr>");
    for name in ordered(periods) {
        let values: Vec<Option<f64>> = periods
            .iter()
            .map(|period| period.window.kpis.get(name)?.get(ALL)?.primary())
            .collect();
        let change = last.comparison.get(name).and_then(|s| s.get(ALL));
        let _ = write!(
            page,
            "<tr><td><code>{}</code></td><td>{}</td><td class=\"num\">{}</td><td class=\"num\">{}</td><td class=\"num\">{}</td>{}</tr>",
            esc(name),
            spark(name, periods, &values),
            value(name, values.last().copied().flatten()),
            value(name, change.and_then(|c| c.previous)),
            delta(name, change.and_then(|c| c.delta)),
            verdict(change),
        );
    }
    page.push_str("</table></div>");
}

/// A bar per period, scaled to the largest value, with a triangle above
/// each period that has a mark.
fn spark(name: &str, periods: &[PeriodKpis], values: &[Option<f64>]) -> String {
    const STEP: usize = 14;
    const BAR: usize = 10;
    const HEIGHT: f64 = 32.0;
    const TOP: f64 = 8.0;
    let width = STEP * periods.len().max(1);
    let largest = values.iter().flatten().fold(0.0_f64, |a, b| a.max(b.abs()));
    let mut svg = format!(
        "<svg class=\"spark\" viewBox=\"0 0 {width} {HEIGHT}\" width=\"{width}\" height=\"{HEIGHT}\" role=\"img\" aria-label=\"{} per period\"><line class=\"base\" x1=\"0\" y1=\"{y}\" x2=\"{width}\" y2=\"{y}\"/>",
        esc(name),
        y = HEIGHT - 0.5,
    );
    for (index, (period, value)) in periods.iter().zip(values).enumerate() {
        let x = index * STEP;
        if let Some(v) = value {
            let height = if largest > 0.0 {
                (v.abs() / largest * (HEIGHT - TOP)).max(1.0)
            } else {
                1.0
            };
            let _ = write!(
                svg,
                "<rect class=\"bar{}\" x=\"{x}\" y=\"{:.1}\" width=\"{BAR}\" height=\"{height:.1}\"><title>{}: {}</title></rect>",
                if period.partial { " partial" } else { "" },
                HEIGHT - height,
                esc(&period.label),
                self::value(name, Some(*v)),
            );
        }
        if !period.marks.is_empty() {
            let center = x + BAR / 2;
            let labels: Vec<&str> = period.marks.iter().map(|m| m.label.as_str()).collect();
            let _ = write!(
                svg,
                "<path class=\"mark\" d=\"M{} 0 L{} 0 L{center} 5 Z\"><title>{}: {}</title></path>",
                center - 3,
                center + 3,
                esc(&period.label),
                esc(&labels.join("; ")),
            );
        }
    }
    svg.push_str("</svg>");
    svg
}

fn marks(page: &mut String, periods: &[PeriodKpis]) {
    page.push_str("<h2>Change marks</h2>");
    let listed: Vec<_> = periods
        .iter()
        .flat_map(|period| period.marks.iter().map(move |mark| (period, mark)))
        .collect();
    if listed.is_empty() {
        page.push_str("<p class=\"muted\">No change took effect in these periods.</p>");
        return;
    }
    page.push_str("<div class=\"scroll\"><table><tr><th>period</th><th>at</th><th>kind</th><th>change</th></tr>");
    for (period, mark) in listed {
        let _ = write!(
            page,
            "<tr><td>{}</td><td>{}</td><td><code>{}</code></td><td class=\"text\">{}{}</td></tr>",
            esc(&period.label),
            esc(&mark.at),
            esc(&mark.kind),
            esc(&mark.label),
            if mark.retracted_by.is_some() {
                " <span class=\"muted\">(retracted)</span>"
            } else {
                ""
            },
        );
    }
    page.push_str("</table></div>");
}

fn kpis(page: &mut String, period: &PeriodKpis) {
    let _ = write!(page, "<h2>KPIs of {}</h2>", esc(&period.label));
    let row = |page: &mut String, name: &str, stratum: Option<&str>, measure: &Measure| {
        let change = period
            .comparison
            .get(name)
            .and_then(|s| s.get(stratum.unwrap_or(ALL)));
        let _ = write!(page, "<tr><td><code>{}</code></td>", esc(name));
        if let Some(stratum) = stratum {
            let _ = write!(page, "<td>{}</td>", esc(stratum));
        }
        let _ = write!(
            page,
            "<td class=\"num\">{}</td><td class=\"num\">{}</td><td class=\"num\">{}</td><td class=\"num\">{}</td><td class=\"num\">{}</td><td class=\"num\">{}</td><td class=\"num\">{}</td>{}</tr>",
            measure.n,
            value(name, measure.value),
            value(name, measure.median),
            value(name, measure.p90),
            value(name, change.and_then(|c| c.previous)),
            delta(name, change.and_then(|c| c.delta)),
            value(name, change.and_then(|c| c.baseline_7d)),
            verdict(change),
        );
    };
    let columns = "<th class=\"num\">n</th><th class=\"num\">value</th><th class=\"num\">median</th><th class=\"num\">p90</th><th class=\"num\">previous</th><th class=\"num\">change</th><th class=\"num\">7-day</th><th>verdict</th></tr>";
    let _ = write!(
        page,
        "<div class=\"scroll\"><table><tr><th>KPI</th>{columns}"
    );
    for (name, strata) in &period.window.kpis {
        if let Some(measure) = strata.get(ALL) {
            row(page, name, None, measure);
        }
    }
    page.push_str("</table></div>");
    let _ = write!(
        page,
        "<details><summary>By stratum (kind and the claim's attributes)</summary><div class=\"scroll\"><table><tr><th>KPI</th><th>stratum</th>{columns}"
    );
    for (name, strata) in &period.window.kpis {
        for (stratum, measure) in strata.iter().filter(|(s, _)| s.as_str() != ALL) {
            row(page, name, Some(stratum), measure);
        }
    }
    page.push_str("</table></div></details>");
}

fn findings(page: &mut String, report: &Report) {
    let _ = write!(
        page,
        "<h2>Open findings</h2><p class=\"meta\">{} open or proposed when the report was generated{}</p>",
        report.findings_open,
        if report.findings_open > report.findings.len() {
            format!("; the first {} by impact", report.findings.len())
        } else {
            String::new()
        },
    );
    if report.findings.is_empty() {
        return;
    }
    page.push_str("<div class=\"scroll\"><table><tr><th class=\"num\">id</th><th>impact</th><th>kind</th><th>target</th><th>summary</th><th class=\"num\">seen</th><th>last seen</th><th>status</th></tr>");
    for finding in &report.findings {
        let _ = write!(
            page,
            "<tr><td class=\"num\">{}</td><td>{}</td><td><code>{}</code></td><td>{}{}</td><td class=\"text\">{}</td><td class=\"num\">{}</td><td>{}</td><td>{}</td></tr>",
            finding.id,
            esc(finding.impact),
            esc(&finding.kind),
            esc(&finding.target),
            if finding.subject.is_empty() {
                String::new()
            } else {
                format!(" <code>{}</code>", esc(&finding.subject))
            },
            esc(&finding.summary),
            finding.occurrences,
            esc(&finding.last_seen_at),
            esc(finding.status),
        );
    }
    page.push_str("</table></div>");
}

fn verdict(change: Option<&Change>) -> String {
    match change {
        Some(Change {
            verdict: Some(verdict),
            ..
        }) => format!("<td class=\"{verdict}\">{verdict}</td>"),
        Some(Change {
            reason: Some(reason),
            ..
        }) => format!("<td class=\"muted\">{}</td>", reason.replace('_', " ")),
        _ => "<td></td>".to_owned(),
    }
}

/// How a KPI's values read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Unit {
    Secs,
    Ratio,
    Number,
}

fn unit(kpi: &str) -> Unit {
    let durations = ["phase.", "land_phase.", "verify_command.", "session_open."];
    if kpi.ends_with("_rate") || kpi == "slot_usage" || kpi.starts_with("session_active_ratio.") {
        Unit::Ratio
    } else if durations.iter().any(|prefix| kpi.starts_with(prefix))
        || kpi.starts_with("session_active.")
        || matches!(
            kpi,
            "lead_time" | "ask_wait" | "ask_apply_wait" | "finding_resolve_time"
        )
    {
        Unit::Secs
    } else {
        Unit::Number
    }
}

fn secs(value: f64) -> String {
    let sign = if value < 0.0 { "-" } else { "" };
    let total = value.abs().round() as i64;
    let (d, h, m, s) = (
        total / 86_400,
        total / 3600 % 24,
        total / 60 % 60,
        total % 60,
    );
    if total < 60 {
        format!("{sign}{s}s")
    } else if total < 3600 {
        format!("{sign}{m}m {s:02}s")
    } else if total < 86_400 {
        format!("{sign}{h}h {m:02}m")
    } else {
        format!("{sign}{d}d {h:02}h")
    }
}

fn number(value: f64) -> String {
    if value.fract() == 0.0 && value.abs() < 1e15 {
        format!("{value:.0}")
    } else {
        format!("{value:.2}")
    }
}

/// A KPI's value in its unit; `—` for none.
fn value(kpi: &str, value: Option<f64>) -> String {
    let Some(value) = value else {
        return "—".to_owned();
    };
    match unit(kpi) {
        Unit::Secs => secs(value),
        Unit::Ratio => format!("{:.1}%", value * 100.0),
        Unit::Number => number(value),
    }
}

/// A change of a KPI, signed: seconds, percentage points or a number.
fn delta(kpi: &str, delta: Option<f64>) -> String {
    let Some(delta) = delta else {
        return "—".to_owned();
    };
    let sign = if delta > 0.0 { "+" } else { "" };
    match unit(kpi) {
        Unit::Secs => format!("{sign}{}", secs(delta)),
        Unit::Ratio => format!("{sign}{:.1} pt", delta * 100.0),
        Unit::Number => format!("{sign}{}", number(delta)),
    }
}

/// `text` safe inside an element and inside a quoted attribute.
fn esc(text: &str) -> String {
    let mut escaped = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '&' => escaped.push_str("&amp;"),
            '<' => escaped.push_str("&lt;"),
            '>' => escaped.push_str("&gt;"),
            '"' => escaped.push_str("&quot;"),
            '\'' => escaped.push_str("&#39;"),
            _ => escaped.push(c),
        }
    }
    escaped
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn values_read_in_their_unit() {
        assert_eq!(value("phase.work", Some(3725.0)), "1h 02m");
        assert_eq!(value("lead_time", Some(90_000.0)), "1d 01h");
        assert_eq!(value("ask_wait", Some(42.4)), "42s");
        assert_eq!(value("verify_command.cargo test", Some(125.0)), "2m 05s");
        assert_eq!(value("first_pass_rate", Some(0.625)), "62.5%");
        assert_eq!(value("session_active_ratio.planner", Some(0.5)), "50.0%");
        assert_eq!(value("slot_usage", Some(1.0)), "100.0%");
        assert_eq!(value("landings", Some(12.0)), "12");
        assert_eq!(value("max_load_avg", Some(3.456)), "3.46");
        assert_eq!(value("landings", None), "—");
        assert_eq!(delta("phase.work", Some(-65.0)), "-1m 05s");
        assert_eq!(delta("phase.work", Some(65.0)), "+1m 05s");
        assert_eq!(delta("revise_rate", Some(0.05)), "+5.0 pt");
        assert_eq!(delta("landings", Some(-2.0)), "-2");
        assert_eq!(delta("landings", None), "—");
    }

    #[test]
    fn text_is_escaped() {
        assert_eq!(
            esc("verify_command.a<b & \"c\" 'd'>"),
            "verify_command.a&lt;b &amp; &quot;c&quot; &#39;d&#39;&gt;"
        );
    }
}
