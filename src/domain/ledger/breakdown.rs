//! The breakdown of a window's runs by their tags (ADR-t1662-1 decisions
//! 4 and 5, docs/design/measurement.md "タグで集計"): the run rows' sums
//! ([`super::Totals`]) added up in the two measures, the time and the
//! slot time, per blocker, per holds and per attempt kind, each in
//! seconds and as a share of its measure. Each axis adds up to 100% with
//! `unattributed` (time no record accounts for) and `none` (attributed
//! time the axis gives no tag); so the unattributed share is the same on
//! every axis. A legacy row has no intervals and is counted apart
//! (`legacy`), not in the shares. Reads the rows only.

use std::collections::BTreeMap;

use serde::Serialize;

use super::{LedgerRow, Measure, Totals};

/// The name of the bucket of time no record accounts for.
pub const UNATTRIBUTED: &str = "unattributed";
/// The name of the bucket of attributed time an axis gives no tag.
pub const NONE: &str = "none";

/// One bucket of a measure: its seconds and its share of the measure
/// (none for a measure of no time).
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct Share {
    pub secs: f64,
    pub ratio: Option<f64>,
}

/// One measure split three ways, each way adding up to the whole.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct Split {
    pub secs: f64,
    pub unattributed: Share,
    pub by_blocker: BTreeMap<String, Share>,
    pub by_holds: BTreeMap<String, Share>,
    /// By the attempt's kind (`first`, `revise`, `conflict`, `resume`), every
    /// attempt of a kind together.
    pub by_attempt: BTreeMap<String, Share>,
}

/// The breakdown of a set of run rows in both measures.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct Breakdown {
    /// The rows with intervals it sums.
    pub runs: usize,
    /// The legacy rows left out (no intervals to split).
    pub legacy: usize,
    pub duration: Split,
    pub slot: Split,
}

/// What a measure takes of a [`Measure`].
type Of = fn(&Measure) -> i64;

impl Breakdown {
    /// The breakdown of `rows` (run rows; others are passed over).
    pub fn of<'a>(rows: impl IntoIterator<Item = &'a LedgerRow>) -> Self {
        let mut breakdown = Self::default();
        let mut sum = Totals::default();
        for row in rows
            .into_iter()
            .filter(|row| row.kind == super::LedgerKind::Run)
        {
            if row.is_legacy() {
                breakdown.legacy += 1;
                continue;
            }
            breakdown.runs += 1;
            add(&mut sum, &row.totals);
        }
        breakdown.duration = split(&sum, |m| m.duration_ms, sum.unattributed_ms);
        // Unattributed time holds no slot (no record says it does).
        breakdown.slot = split(&sum, |m| m.slot_ms, 0);
        breakdown
    }

    /// The unattributed share of the time, none without any time.
    pub fn unattributed_ratio(&self) -> Option<f64> {
        self.duration.unattributed.ratio
    }
}

fn add(sum: &mut Totals, row: &Totals) {
    let plus = |into: &mut Measure, from: &Measure| {
        into.duration_ms += from.duration_ms;
        into.slot_ms += from.slot_ms;
    };
    plus(&mut sum.all, &row.all);
    sum.unattributed_ms += row.unattributed_ms;
    for (key, measure) in &row.by_blocker {
        plus(sum.by_blocker.entry(key.clone()).or_default(), measure);
    }
    for (key, measure) in &row.by_holds {
        plus(sum.by_holds.entry(key.clone()).or_default(), measure);
    }
    for (key, measure) in &row.by_attempt {
        // `<kind>.<n>`: every attempt of a kind together.
        let kind = key.split('.').next().unwrap_or(key).to_owned();
        plus(sum.by_attempt.entry(kind).or_default(), measure);
    }
}

fn split(sum: &Totals, of: Of, unattributed_ms: i64) -> Split {
    let whole = of(&sum.all);
    let share = |ms: i64| Share {
        secs: ms as f64 / 1000.0,
        ratio: (whole > 0).then(|| ms as f64 / whole as f64),
    };
    let axis = |by: &BTreeMap<String, Measure>| {
        let mut ms: BTreeMap<String, i64> = BTreeMap::new();
        for (key, measure) in by {
            *ms.entry(key.clone()).or_default() += of(measure);
        }
        let rest = whole - unattributed_ms - ms.values().sum::<i64>();
        if rest > 0 {
            *ms.entry(NONE.to_owned()).or_default() += rest;
        }
        ms.into_iter()
            .map(|(key, ms)| (key, share(ms)))
            .collect::<BTreeMap<String, Share>>()
    };
    Split {
        secs: whole as f64 / 1000.0,
        unattributed: share(unattributed_ms),
        by_blocker: axis(&sum.by_blocker),
        by_holds: axis(&sum.by_holds),
        by_attempt: axis(&sum.by_attempt),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::ledger::fold::Interval;
    use crate::domain::run_phase::{Attempt, AttemptKind, Blocker, Holds};
    use serde_json::json;

    fn interval(
        blocker: Option<Blocker>,
        holds: Option<Holds>,
        attempt: Option<Attempt>,
        start: i64,
        end: i64,
    ) -> Interval {
        Interval {
            phase: if blocker.is_some() {
                "work".into()
            } else {
                crate::domain::ledger::fold::UNATTRIBUTED.into()
            },
            blocker,
            holds,
            attempt,
            start,
            end,
            cause: None,
            blocked_by: None,
            run_id: None,
        }
    }

    fn row(intervals: &[Interval], legacy: bool) -> LedgerRow {
        let mut row = crate::domain::ledger::LedgerRow::new(
            crate::domain::ledger::LedgerKind::Run,
            "r".into(),
            None,
            "t",
        );
        row.totals = Totals::of(intervals);
        row.intervals = intervals.to_vec();
        if legacy {
            row.legacy = Some(json!({}));
        }
        row
    }

    /// Each axis of each measure adds up to the whole with `unattributed`
    /// and `none`; the attempts of a kind are summed; a legacy row is
    /// counted apart and in no share.
    #[test]
    fn each_axis_adds_up_to_the_whole_with_unattributed() {
        let work = |n| Attempt {
            kind: AttemptKind::First,
            n,
        };
        let a = row(
            &[
                interval(
                    Some(Blocker::Ai),
                    Some(Holds::WorkerSlot),
                    Some(work(1)),
                    0,
                    6000,
                ),
                interval(None, None, None, 6000, 7000),
                interval(Some(Blocker::Human), Some(Holds::None), None, 7000, 10_000),
            ],
            false,
        );
        let b = row(
            &[interval(
                Some(Blocker::Queue),
                Some(Holds::LandingSlot),
                Some(work(2)),
                0,
                10_000,
            )],
            false,
        );
        let legacy = row(&[], true);
        let breakdown = Breakdown::of([&a, &b, &legacy]);
        assert_eq!((breakdown.runs, breakdown.legacy), (2, 1));
        let duration = &breakdown.duration;
        assert_eq!(duration.secs, 20.0);
        assert_eq!(duration.unattributed.ratio, Some(0.05));
        for axis in [
            &duration.by_blocker,
            &duration.by_holds,
            &duration.by_attempt,
        ] {
            let total: f64 = axis.values().filter_map(|share| share.ratio).sum::<f64>()
                + duration.unattributed.ratio.unwrap();
            assert!((total - 1.0).abs() < 1e-9, "{axis:?}");
        }
        assert_eq!(duration.by_blocker["ai"].secs, 6.0);
        assert_eq!(duration.by_attempt["first"].secs, 16.0);
        assert_eq!(duration.by_attempt[NONE].secs, 3.0);
        // The slot time: the worker's 6 s and the landing's 10 s.
        assert_eq!(breakdown.slot.secs, 16.0);
        assert_eq!(breakdown.slot.unattributed.secs, 0.0);
        let slot_total: f64 = breakdown
            .slot
            .by_holds
            .values()
            .filter_map(|s| s.ratio)
            .sum();
        assert!((slot_total - 1.0).abs() < 1e-9);
        assert_eq!(breakdown.unattributed_ratio(), Some(0.05));
        assert_eq!(Breakdown::of([&legacy]).unattributed_ratio(), None);
    }
}
