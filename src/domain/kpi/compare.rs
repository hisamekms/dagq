//! The comparison across a change (ADR-0051 decisions 14–16): a window
//! before the mark and one after it, every other mark in and between them
//! listed next to the numbers (`confounders`), both split by the change,
//! the area, `parallel`, the load band, the build and the axes of `--by`,
//! and marks too close to split
//! (fewer finished runs between them than `min_samples`) taken as one
//! overlapping change. A routine mark, one that only records a
//! supervisor's build, is no change of its own (ADR-t1381-1): it is listed
//! among the confounders and the build is read in the `build=` strata.
//! Nothing is removed automatically: a person narrows the windows with
//! `--compare A..B,C..D`.
use std::collections::BTreeMap;

use serde::Serialize;

use super::{
    COMPARE_AXES, Change, CompareSpec, DAY_MS, KpiQuery, Kpis, Measure, cursor_ms, window::Context,
};
use crate::domain::{
    host_metrics::HostSummary,
    marks::{self, DERIVED_PREFIX, MARK_RETRACTED, Mark, SUPERVISOR_STARTED, SUPERVISOR_STOPPED},
    stats::{Cursor, executions::Coverage, timestamp_millis},
};

/// One side of a comparison.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct WindowSpan {
    pub start: String,
    pub end: String,
    /// The window reaches past now.
    pub partial: bool,
    /// The runs that finished in it.
    pub runs: usize,
    /// The host's load in it, as a period's `host` (task 872).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub host: Option<HostSummary>,
    /// How much of it the records of the tokens cover (`full`, `partial`,
    /// `none`): a side before the records has no `tokens` to compare.
    pub token_coverage: Coverage,
}

/// The change split at, when `--compare` names a mark or a time.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Split {
    /// Where the window before ends and the one after starts.
    pub start: String,
    pub end: String,
    /// The marks of the change: one, or the overlapping ones taken
    /// together; none for a time no mark took effect at.
    pub marks: Vec<Mark>,
    /// False when the change is overlapping marks: this comparison cannot
    /// tell them apart.
    pub separable: bool,
}

/// A mark in or between the windows other than the change compared.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Confounder {
    /// `before`, `between` or `after`.
    pub position: &'static str,
    #[serde(flatten)]
    pub mark: Mark,
}

/// One KPI of one stratum on both sides.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Side {
    pub before: Measure,
    pub after: Measure,
    #[serde(flatten)]
    pub change: Change,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Comparison {
    pub split: Option<Split>,
    pub before: WindowSpan,
    pub after: WindowSpan,
    pub confounders: Vec<Confounder>,
    /// The groups of overlapping marks in the range, each taken as one change.
    pub overlapping: Vec<Vec<Mark>>,
    /// Per KPI, per stratum (`all`, `change=`, `area=`, `parallel=`,
    /// `load=`, `build=`).
    pub strata: Kpis<Side>,
    /// The times of the work (`lead_time`, `phase.*`, `land_phase.*`) for
    /// each change the summary is made for (ADR-t980-1), by change; not
    /// listed when no run of the comparison has a stratum of one.
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub change_summary: BTreeMap<String, BTreeMap<String, Side>>,
    /// The same for each area (ADR-t980-1), by area; not listed without
    /// `[areas]`.
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub area_summary: BTreeMap<String, BTreeMap<String, Side>>,
}

/// The marks that split KPIs, by when they took effect: neither a
/// retracted mark nor a retraction.
fn splitting(marks: &[Mark]) -> Vec<&Mark> {
    marks
        .iter()
        .filter(|mark| mark.retracted_by.is_none() && mark.kind != MARK_RETRACTED)
        .collect()
}

fn mark_ms(mark: &Mark) -> i64 {
    timestamp_millis(&mark.at).unwrap_or(i64::MIN)
}

/// Whether each of `marks` (time order) is routine (ADR-t1381-1): a
/// supervisor's start with the same `parallel` as the start before it, a
/// supervisor's stop, or a derived build. Such a mark only records the
/// build, which the `build=` strata read, so it groups with no other.
fn routine(marks: &[&Mark]) -> Vec<bool> {
    let mut parallel: Option<&serde_json::Value> = None;
    marks
        .iter()
        .map(|mark| match mark.kind.as_str() {
            SUPERVISOR_STARTED => {
                let this = mark.detail.get("parallel");
                let same = matches!((parallel, this), (Some(before), Some(now)) if before == now);
                parallel = this;
                same
            }
            SUPERVISOR_STOPPED => true,
            kind => kind.strip_prefix(DERIVED_PREFIX) == Some("dagq_version"),
        })
        .collect()
}

/// The splitting marks, each with whether it is routine.
fn classified(marks: &[Mark]) -> Vec<(&Mark, bool)> {
    let splitting = splitting(marks);
    let routine = routine(&splitting);
    splitting.into_iter().zip(routine).collect()
}

/// The marks grouped into changes (decision 16): a mark joins the group of
/// the one before it when fewer than `min_samples` runs finished after that
/// one up to it. `finishes` are the runs' finish times, ascending. Routine
/// marks are in no group and are skipped when counting (ADR-t1381-1).
pub fn overlapping_groups(marks: &[Mark], finishes: &[i64], min_samples: usize) -> Vec<Vec<Mark>> {
    let mut groups: Vec<Vec<Mark>> = Vec::new();
    for (mark, _) in classified(marks)
        .into_iter()
        .filter(|(_, routine)| !routine)
    {
        let at = mark_ms(mark);
        match groups.last_mut() {
            Some(group)
                if {
                    let previous = mark_ms(group.last().expect("groups are not empty"));
                    let between = finishes.partition_point(|&t| t <= at)
                        - finishes.partition_point(|&t| t <= previous);
                    between < min_samples
                } =>
            {
                group.push(mark.clone());
            }
            _ => groups.push(vec![mark.clone()]),
        }
    }
    groups
}

/// The change `cursor` names: the group of a mark that is not routine, or
/// a routine mark alone. A claim names its derived marks; when they are
/// both routine and not, the one that is not (ADR-t1381-1 (i)).
fn named_change(marks: &[Mark], groups: &[Vec<Mark>], cursor: Cursor) -> Option<Vec<Mark>> {
    let names = |mark: &Mark| match cursor {
        // A derived mark is named by the claim it was read off.
        Cursor::Event(id) => mark.id == Some(id) || mark.detail["claim_event"] == id.as_i64(),
        Cursor::Time(ms) => mark_ms(mark) == ms,
    };
    groups
        .iter()
        .find(|group| group.iter().any(names))
        .cloned()
        .or_else(|| {
            classified(marks)
                .into_iter()
                .find(|(mark, routine)| *routine && names(mark))
                .map(|(mark, _)| vec![mark.clone()])
        })
}

/// The comparison `spec` asks for.
pub(super) fn compare(
    context: &Context<'_>,
    spec: CompareSpec,
    query: &KpiQuery,
    now_ms: i64,
) -> Result<Comparison, String> {
    let events = context.events;
    let min_samples = context.min_samples;
    let groups = overlapping_groups(&context.marks, &context.finishes, min_samples);
    let at = |cursor: Cursor| cursor_ms(cursor, events).ok_or("--compare names no event");
    let (split, before, after) = match spec {
        CompareSpec::At(cursor) => {
            let time = match cursor {
                // Split at the mark's effective time: a person's --at, or a
                // pruned supervisor stop's last heartbeat, may precede recording.
                Cursor::Event(id) => context
                    .marks
                    .iter()
                    .find(|mark| mark.id == Some(id))
                    .map_or_else(|| at(cursor), |mark| Ok(mark_ms(mark)))?,
                Cursor::Time(ms) => ms,
            };
            let group = named_change(&context.marks, &groups, cursor);
            let (first, last) = group.as_ref().map_or((time, time), |group| {
                (mark_ms(&group[0]), mark_ms(&group[group.len() - 1]))
            });
            let window = query.window_days.max(1) * DAY_MS;
            (
                Some(Split {
                    start: marks::utc_text(first),
                    end: marks::utc_text(last),
                    separable: group.as_ref().is_none_or(|group| group.len() == 1),
                    marks: group.unwrap_or_default(),
                }),
                (first - window, first),
                (last, last + window),
            )
        }
        CompareSpec::Windows([(a, b), (c, d)]) => {
            let (before, after) = ((at(a)?, at(b)?), (at(c)?, at(d)?));
            if before.0 >= before.1 || after.0 >= after.1 {
                return Err("each --compare window must start before it ends".into());
            }
            if before.1 > after.0 {
                return Err("the first --compare window must end before the second starts".into());
            }
            (None, before, after)
        }
    };
    let in_split = |mark: &Mark| {
        split
            .as_ref()
            .is_some_and(|split| split.marks.iter().any(|kept| kept == mark))
    };
    // A routine mark read off the claim of the change takes effect with it,
    // on the window after (ADR-t1381-1 (i)).
    let claim_of_split = |mark: &Mark| {
        let claim = &mark.detail["claim_event"];
        !claim.is_null()
            && split.as_ref().is_some_and(|split| {
                split
                    .marks
                    .iter()
                    .any(|kept| &kept.detail["claim_event"] == claim)
            })
    };
    let confounders = splitting(&context.marks)
        .into_iter()
        .filter(|mark| {
            let at = mark_ms(mark);
            at > before.0 && at <= after.1 && !in_split(mark)
        })
        .map(|mark| {
            let at = mark_ms(mark);
            Confounder {
                position: if claim_of_split(mark) {
                    "after"
                } else if at <= before.1 {
                    "before"
                } else if at > after.0 {
                    "after"
                } else {
                    "between"
                },
                mark: mark.clone(),
            }
        })
        .collect();
    let overlapping = groups
        .iter()
        .filter(|group| {
            group.len() > 1
                && group.iter().any(|mark| {
                    let at = mark_ms(mark);
                    at > before.0 && at <= after.1
                })
        })
        .cloned()
        .collect();
    let mut axes = COMPARE_AXES.to_vec();
    for axis in &query.by {
        if !axes.contains(axis) {
            axes.push(*axis);
        }
    }
    axes.sort_unstable();
    let windows = [before, after].map(|(start, end)| context.window(start, end, &axes));
    let span = |(start, end): (i64, i64), window: &super::WindowKpis| WindowSpan {
        start: marks::utc_text(start),
        end: marks::utc_text(end),
        partial: end > now_ms,
        runs: window.runs,
        host: context.host.map(|host| host.between(start, end)),
        token_coverage: window.token_coverage,
    };
    let empty = Measure::default();
    let after_partial = after.1 > now_ms;
    let mut strata: Kpis<Side> = BTreeMap::new();
    for (name, after_strata) in &windows[1].kpis {
        let before_strata = windows[0].kpis.get(name);
        let keys = after_strata
            .keys()
            .chain(before_strata.into_iter().flat_map(|s| s.keys()));
        for stratum in keys {
            let before = before_strata.and_then(|s| s.get(stratum));
            let after = after_strata.get(stratum).unwrap_or(&empty);
            strata.entry(name.clone()).or_default().insert(
                stratum.clone(),
                Side {
                    before: before.cloned().unwrap_or_default(),
                    after: after.clone(),
                    change: {
                        let mut change = Change::between(name, before, after, min_samples);
                        if after_partial {
                            change.not_over();
                        }
                        change
                    },
                },
            );
        }
    }
    // Work differs by an order of magnitude between changes (decision 15),
    // so the summary is made per change and per area: the ones asked for,
    // or every value the comparison saw (ADR-t980-1 decision 6(b): no value
    // is built in).
    let change_summary = summarize(&strata, "change", &query.changes);
    let area_summary = summarize(&strata, "area", &query.areas);
    Ok(Comparison {
        split,
        before: span(before, &windows[0]),
        after: span(after, &windows[1]),
        confounders,
        overlapping,
        strata,
        change_summary,
        area_summary,
    })
}

/// The times of the work for each value of `axis`: those in `wanted`, or
/// every value the strata have.
fn summarize(
    strata: &Kpis<Side>,
    axis: &str,
    wanted: &[String],
) -> BTreeMap<String, BTreeMap<String, Side>> {
    let prefix = format!("{axis}=");
    let values: Vec<String> = if wanted.is_empty() {
        strata
            .values()
            .flat_map(|sides| sides.keys())
            .filter_map(|stratum| stratum.strip_prefix(&prefix))
            .map(str::to_owned)
            .collect::<std::collections::BTreeSet<_>>()
            .into_iter()
            .collect()
    } else {
        wanted.to_vec()
    };
    values
        .into_iter()
        .map(|value| {
            let stratum = format!("{prefix}{value}");
            let kpis = strata
                .iter()
                .filter(|(name, _)| {
                    *name == "lead_time"
                        || name.starts_with("phase.")
                        || name.starts_with("land_phase.")
                })
                .filter_map(|(name, sides)| Some((name.clone(), sides.get(&stratum)?.clone())))
                .collect();
            (value, kpis)
        })
        .collect()
}
