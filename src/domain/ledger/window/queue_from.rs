//! Which queue rows a read from a time ([`super::WindowQuery::read_from_ms`],
//! the time S) reads, kind by kind of their subject: promise 6 of
//! docs/plans/ledger-window-earlier-visibility-spike.md ("読みの約束").
//!
//! A kind is read whole ([`QueueRead::Whole`], every row by key, as a read
//! of every row reads it) or by its range ([`QueueRead::Range`]). Of a kind
//! read by its range the store gives the rows that have not ended by S,
//! ended at S or after it, or started at S or after it or at no time (a
//! start a clock ahead recorded) ([`reaches`]), the last that ended before S (each
//! row of that end), and, once those and the rows read whole are taken,
//! the rows ended before S whose inputs reach past `queue_before`, the
//! queue's last record of S or before it ([`past`]). A row left out so
//! holds its start and its end alone, both recorded before S, and every
//! record it holds is at or before `queue_before` by id: no
//! period from S holds one (its first event id is at or past
//! `queue_before`), and `queue_before` itself, which the rows taken give,
//! stays what every row gives. Neither follows the other's clock, so a
//! clock turned back or drifting leaves the answer that of every row.
//!
//! The kinds read by their range are those whose readers Spike 4678 found
//! to answer as every row so (docs/plans/ledger-window-earlier-visibility-spike.md,
//! "(ii) queue 行": the deferrals, the alerts and the landing slots). A
//! landing slot's records are its run's, which the run's row gives as well
//! by the bound of [`super::RowsFrom`] (each record before S in time), and
//! an attempt a start of a run left out ended is taken with that run's
//! landing records ([`super::Earlier::landings`], which `stats` and `kpi`
//! add to the events they read the attempts of). Each other
//! kind is read whole, its rows' records read by their readers from
//! before S (the waits and the supervisors' lives, the marks and their
//! withdrawals, the forecasts' snapshots and the baselines, the
//! `candidates_sampled`, the updates and failures, the asks and repairs of
//! no run, `stall_config_loaded`, main's records, the reverts and CI's
//! failures, the goals' attributes, the findings that link a proposal, the
//! inbox watcher's state) or a kind no reader was judged for. With the
//! store's views of the queue rows trusted, the kinds of [`BY_VIEWS`]
//! (main's records, the reverts and CI's failures, the goals' attributes,
//! the baselines' marks, the findings) are read by their range too, what
//! they hold of before S sought by what the range names
//! ([`super::queue_named`]). Waits, lives, marks, snapshots, samples,
//! failures, repairs, stall config and run-less asks also use range reads
//! with [`super::queue_lives`] when their views and visibility are usable;
//! otherwise they keep full pages.

use super::super::{LedgerKind, LedgerRow};
use crate::domain::EventId;

/// How a read from a time reads a subject kind before named views apply.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QueueRead {
    /// Every row of the kind, by key.
    Whole,
    /// The rows of the kind that reach S, the last ended before it, and
    /// those whose inputs reach past `queue_before` (module doc).
    Range,
}

/// The subject kinds whose queue rows a read from a time reads by their
/// range ([`QueueRead::Range`]); every other kind is read whole.
pub const BY_RANGE: &[&str] = &["claim_deferral", "landing_slot", "live_alert"];

/// The subject kinds whose queue rows a read from a time reads by their
/// range when the store's views of the queue rows can be trusted
/// ([`crate::application::LedgerWindowRead::queue_views_usable`]), the rows
/// of before the range they hold sought by what the range names
/// ([`super::queue_named`]): main's record and CI's (its summary up to S
/// and the commits and failures by their sha), the goals' records (each
/// goal named from its last record that replaces it), the baselines' marks
/// (the latest of each before S), and the findings (those that link a
/// proposal named). The kinds of main's record a summary holds no value of
/// (`main_rewritten`, `main_reverts_imported`) stay whole.
pub const BY_VIEWS: &[&str] = &[
    "main_commits_recorded",
    "main_paths_recorded",
    "main_observed",
    "main_read_failed",
    "main_read_recovered",
    "ci_checked",
    "ci_turned_red",
    "goal_created",
    "goal_updated",
    "goal_status_changed",
    "goal_closed",
    "goal_reopened",
    "attributes_baseline",
    "finding_baseline",
    "finding",
];

/// How a read from a time reads the queue rows of the subject kind `kind`.
pub fn read_of(kind: &str) -> QueueRead {
    read_of_with(kind, false)
}

/// How a read from a time reads the queue rows of the subject kind `kind`,
/// with the store's views of the queue rows trusted (`views`) or not.
pub fn read_of_with(kind: &str, views: bool) -> QueueRead {
    if BY_RANGE.contains(&kind) || (views && BY_VIEWS.contains(&kind)) {
        QueueRead::Range
    } else {
        QueueRead::Whole
    }
}

/// The subject kinds read by their range, with the views trusted or not.
pub fn by_range(views: bool) -> Vec<&'static str> {
    let mut kinds = BY_RANGE.to_vec();
    if views {
        kinds.extend_from_slice(BY_VIEWS);
    }
    kinds
}

/// The subject kind of the queue row `row` (none: empty, read whole).
pub fn kind_of(row: &LedgerRow) -> &str {
    row.subject
        .as_ref()
        .map_or("", |subject| subject.kind.as_str())
}

/// Whether a read from a time reads the queue row `row` whole: one of a
/// kind read whole.
pub fn read_whole(row: &LedgerRow) -> bool {
    read_whole_with(row, false)
}

/// [`read_whole`] with the store's views of the queue rows trusted
/// (`views`) or not.
pub fn read_whole_with(row: &LedgerRow, views: bool) -> bool {
    row.kind == LedgerKind::Queue && read_of_with(kind_of(row), views) == QueueRead::Whole
}

/// Whether the queue row `row` of a kind read by its range reaches the
/// time `from_ms`: not ended, ended at it or after it, or started at it or
/// after it or at no time (a row of these kinds holds its start and its
/// end alone, so with its end before the time only a clock ahead and back
/// records its start after it).
pub fn reaches(row: &LedgerRow, from_ms: i64) -> bool {
    row.ended_at.is_none_or(|end| end >= from_ms)
        || row.started_at.is_none_or(|start| start >= from_ms)
}

/// Whether the queue row `row`, ended before the time, reads inputs past
/// `queue_before`, so that a read takes it whatever its end.
pub fn past(row: &LedgerRow, queue_before: EventId) -> bool {
    row.inputs_through.events > queue_before
}

/// Of `rows`, those a read from `from_ms` takes of the kind `kind` read by
/// its range before `queue_before` is known: those that reach the time and
/// each row of the last end before it, by key. What a store that holds no
/// condition of its own gives.
pub fn reaching(rows: &[LedgerRow], kind: &str, from_ms: i64) -> Vec<LedgerRow> {
    let of_kind = || {
        rows.iter()
            .filter(move |row| row.kind == LedgerKind::Queue && kind_of(row) == kind)
    };
    let last = of_kind()
        .filter_map(|row| row.ended_at.filter(|end| *end < from_ms))
        .max();
    let mut kept: Vec<LedgerRow> = of_kind()
        .filter(|row| reaches(row, from_ms) || (last.is_some() && row.ended_at == last))
        .cloned()
        .collect();
    kept.sort_by(|a, b| a.key.cmp(&b.key));
    kept
}

/// Of `rows`, those of the kind `kind` ended before `from_ms` whose inputs
/// reach past `queue_before`, by key.
pub fn past_of(
    rows: &[LedgerRow],
    kind: &str,
    from_ms: i64,
    queue_before: EventId,
) -> Vec<LedgerRow> {
    let mut kept: Vec<LedgerRow> = rows
        .iter()
        .filter(|row| row.kind == LedgerKind::Queue && kind_of(row) == kind)
        .filter(|row| !reaches(row, from_ms) && past(row, queue_before))
        .cloned()
        .collect();
    kept.sort_by(|a, b| a.key.cmp(&b.key));
    kept
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::ledger::Subject;

    fn row(key: &str, kind: &str, ended_at: Option<i64>, through: i64) -> LedgerRow {
        let mut row = LedgerRow::new(LedgerKind::Queue, key.to_owned(), None, "");
        row.subject = Some(Subject {
            kind: kind.to_owned(),
            ..Subject::default()
        });
        row.started_at = Some(0);
        row.ended_at = ended_at;
        row.inputs_through.events = EventId::new(through);
        row
    }

    #[test]
    fn the_kinds_of_ii_are_read_whole_and_the_judged_ones_by_their_range() {
        for kind in BY_RANGE {
            assert_eq!(read_of(kind), QueueRead::Range);
        }
        for kind in [
            "slots_full",
            "claim_hold",
            "landing_hold",
            "live_alert_stream",
            "finding",
            "supervisor_started",
            "mark_recorded",
            "forecast_recorded",
            "inbox_watcher_absent",
            "",
            "a_kind_of_tomorrow",
        ] {
            assert_eq!(read_of(kind), QueueRead::Whole, "{kind}");
        }
    }

    #[test]
    fn a_range_keeps_what_reaches_the_time_and_the_last_end_before_it() {
        let rows = vec![
            row("1", "live_alert", Some(10), 1),
            row("2", "live_alert", Some(20), 2),
            row("3", "live_alert", Some(20), 3),
            row("4", "live_alert", Some(30), 4),
            row("5", "live_alert", None, 5),
            row("6", "landing_slot", Some(5), 6),
            row("7", "finding", Some(1), 7),
        ];
        let mut skewed = row("8", "live_alert", Some(15), 8);
        skewed.started_at = Some(35);
        let mut untimed = row("9", "live_alert", Some(15), 9);
        untimed.started_at = None;
        let mut rows = rows;
        rows.extend([skewed, untimed]);
        let keys = |rows: Vec<LedgerRow>| rows.into_iter().map(|row| row.key).collect::<Vec<_>>();
        assert_eq!(
            keys(reaching(&rows, "live_alert", 30)),
            ["2", "3", "4", "5", "8", "9"]
        );
        assert_eq!(keys(reaching(&rows, "landing_slot", 30)), ["6"]);
        assert!(!read_whole(&rows[5]));
        assert_eq!(
            keys(past_of(&rows, "live_alert", 30, EventId::new(1))),
            ["2", "3"]
        );
        assert!(past_of(&rows, "live_alert", 30, EventId::new(3)).is_empty());
        assert!(read_whole(&rows[6]));
        assert!(!read_whole(&rows[0]));
    }
}
