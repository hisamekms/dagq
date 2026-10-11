//! The window of records whose baselines come in groups: the records of
//! a baseline and, after them, the queue's mark that closes the group
//! (docs/design/attribute-records.md "基準と照合").

use super::super::RunEvent;
use super::super::stats::timestamp_millis;

/// Which of `events` (ascending id) a window that ends at `window_end`
/// (unix seconds; none for every event) takes: a baseline's record
/// (`is_record`) only with the mark that closes its group (`is_mark`),
/// whatever the record's time, so a window never holds part of a group (a
/// record with no mark after it stands alone); any other event when it is
/// within the window. None when the window ends before the first mark.
pub(super) fn taken(
    events: &[RunEvent],
    window_end: Option<i64>,
    is_mark: impl Fn(&RunEvent) -> bool,
    is_record: impl Fn(&RunEvent) -> bool,
) -> Option<Vec<bool>> {
    let end_ms = window_end.map(|end| end * 1000);
    let within = |event: &RunEvent| {
        end_ms.is_none_or(|end| timestamp_millis(&event.created_at).is_some_and(|at| at <= end))
    };
    if events
        .iter()
        .find(|event| is_mark(event))
        .is_some_and(|mark| !within(mark))
    {
        return None;
    }
    let mut taken = vec![false; events.len()];
    let mut closing: Option<bool> = None;
    for (index, event) in events.iter().enumerate().rev() {
        if is_mark(event) {
            closing = Some(within(event));
            taken[index] = within(event);
        } else if is_record(event) {
            taken[index] = closing.unwrap_or_else(|| within(event));
        } else {
            taken[index] = within(event);
        }
    }
    Some(taken)
}
