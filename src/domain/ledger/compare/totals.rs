//! The sums of the run and task rows the reconciliation takes values of
//! ([`totals`]): the rows' own [`super::super::Totals`], not a fold again,
//! so what the stage's gate reads follows the fold as it changes.

use std::collections::BTreeMap;

use serde::Serialize;

use super::super::{LedgerKind, LedgerRow, Measure};
use super::Reconciliation;

/// The blockers and holds every sum lists, a zero one too
/// ([`crate::domain::run_phase::Blocker`], [`crate::domain::run_phase::Holds`]);
/// a tag a row has besides them is listed as well.
const BLOCKERS: [&str; 7] = [
    "queue", "ai", "compute", "human", "runtime", "external", "infra",
];
const HOLDS: [&str; 3] = ["worker_slot", "landing_slot", "none"];

/// One measure in seconds, of milliseconds summed: a millisecond is kept.
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize)]
pub struct Seconds {
    pub duration_secs: f64,
    pub slot_secs: f64,
}

#[allow(clippy::cast_precision_loss)]
fn secs(ms: i64) -> f64 {
    ms as f64 / 1000.0
}

impl From<&Measure> for Seconds {
    fn from(measure: &Measure) -> Self {
        Self {
            duration_secs: secs(measure.duration_ms),
            slot_secs: secs(measure.slot_ms),
        }
    }
}

/// The sums of one kind's rows. The unattributed time is in none of
/// `by_blocker` and `by_holds` and holds no slot: each of those sums plus
/// `unattributed_secs` is `duration_secs`.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct KindTotals {
    /// The rows the values are of.
    pub rows: usize,
    /// Legacy rows otherwise taken, left out: they have no intervals.
    pub legacy: usize,
    pub duration_secs: f64,
    pub slot_secs: f64,
    pub unattributed_secs: f64,
    /// `unattributed_secs / duration_secs`, none with no duration.
    pub unattributed_ratio: Option<f64>,
    pub by_blocker: BTreeMap<String, Seconds>,
    pub by_holds: BTreeMap<String, Seconds>,
}

/// The sums of the run rows and of the task rows.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct LedgerTotals {
    pub run: KindTotals,
    pub task: KindTotals,
}

#[derive(Default)]
struct Sum {
    rows: usize,
    legacy: usize,
    all: Measure,
    unattributed_ms: i64,
    by_blocker: BTreeMap<String, Measure>,
    by_holds: BTreeMap<String, Measure>,
}

fn add_all(into: &mut BTreeMap<String, Measure>, from: &BTreeMap<String, Measure>) {
    for (key, measure) in from {
        let sum = into.entry(key.clone()).or_default();
        sum.duration_ms += measure.duration_ms;
        sum.slot_ms += measure.slot_ms;
    }
}

impl Sum {
    fn add(&mut self, row: &LedgerRow) {
        if row.is_legacy() {
            self.legacy += 1;
            return;
        }
        self.rows += 1;
        self.all.duration_ms += row.totals.all.duration_ms;
        self.all.slot_ms += row.totals.all.slot_ms;
        self.unattributed_ms += row.totals.unattributed_ms;
        add_all(&mut self.by_blocker, &row.totals.by_blocker);
        add_all(&mut self.by_holds, &row.totals.by_holds);
    }

    fn finish(self) -> KindTotals {
        let listed = |names: &[&str], sums: &BTreeMap<String, Measure>| {
            let mut out: BTreeMap<String, Seconds> = names
                .iter()
                .map(|name| ((*name).to_owned(), Seconds::default()))
                .collect();
            out.extend(sums.iter().map(|(key, sum)| (key.clone(), sum.into())));
            out
        };
        #[allow(clippy::cast_precision_loss)]
        let ratio = (self.all.duration_ms > 0)
            .then(|| self.unattributed_ms as f64 / self.all.duration_ms as f64);
        KindTotals {
            rows: self.rows,
            legacy: self.legacy,
            duration_secs: secs(self.all.duration_ms),
            slot_secs: secs(self.all.slot_ms),
            unattributed_secs: secs(self.unattributed_ms),
            unattributed_ratio: ratio,
            by_blocker: listed(&BLOCKERS, &self.by_blocker),
            by_holds: listed(&HOLDS, &self.by_holds),
        }
    }
}

/// The sums of the `stored` run and task rows `reconciled` finds ended in
/// its window, settled and up to date (not dirty, not pending on a push):
/// the rows the values of the parallel run are taken from. A legacy row
/// is counted apart and not summed.
pub fn totals(reconciled: &Reconciliation, stored: &[LedgerRow]) -> LedgerTotals {
    let by_key: BTreeMap<(LedgerKind, &str), &LedgerRow> = stored
        .iter()
        .map(|row| ((row.kind, row.key.as_str()), row))
        .collect();
    let (mut run, mut task) = (Sum::default(), Sum::default());
    for state in &reconciled.rows {
        let sum = match state.kind {
            LedgerKind::Run => &mut run,
            LedgerKind::Task => &mut task,
            _ => continue,
        };
        if !(state.row && state.is_final && state.up_to_date) || state.push_pending {
            continue;
        }
        if let Some(row) = by_key.get(&(state.kind, state.key.as_str())) {
            sum.add(row);
        }
    }
    LedgerTotals {
        run: run.finish(),
        task: task.finish(),
    }
}

#[cfg(test)]
mod tests;
