//! How long an ask waited for the inbox's watcher to see it (task 1021,
//! the KPI `ask_seen_wait`): 0 seconds for an ask opened while the
//! watcher was alive, else the seconds to the watcher's return. The
//! watcher's state is the latest `inbox_watcher_absent` /
//! `inbox_watcher_returned` before the ask's `ask_opened`; an ask opened
//! before either was recorded is no sample, as the watchers' own records
//! do not last.
use super::{EventId, RunEvent, TaskId, timestamp_millis};
use crate::domain::event_kind::{ASK_OPENED, INBOX_WATCHER_ABSENT, INBOX_WATCHER_RETURNED};

/// The seconds each ask whose task `counts` accepts waited to be seen,
/// of those seen after `after` up to `upto`: an ask opened while the
/// watcher was alive is seen at its opening, one opened while it was
/// absent at the next `inbox_watcher_returned`. An ask still unseen at
/// `upto` is no sample yet. Sorted.
pub fn seen_waits(
    events: &[RunEvent],
    after: EventId,
    upto: EventId,
    counts: impl Fn(Option<TaskId>) -> bool,
) -> Vec<i64> {
    let mut alive: Option<bool> = None;
    // The openings (unix ms) of the asks waiting for the watcher's return.
    let mut unseen: Vec<i64> = Vec::new();
    let mut waits = Vec::new();
    for event in events.iter().filter(|event| event.id <= upto) {
        match event.kind.as_str() {
            INBOX_WATCHER_ABSENT => alive = Some(false),
            INBOX_WATCHER_RETURNED => {
                alive = Some(true);
                let returned = timestamp_millis(&event.created_at);
                for opened in unseen.drain(..) {
                    if event.id > after
                        && let Some(returned) = returned
                    {
                        waits.push(((returned - opened) / 1000).max(0));
                    }
                }
            }
            ASK_OPENED if counts(event.task_id) => match alive {
                None => {}
                Some(true) => {
                    if event.id > after {
                        waits.push(0);
                    }
                }
                Some(false) => unseen.extend(timestamp_millis(&event.created_at)),
            },
            _ => {}
        }
    }
    waits.sort_unstable();
    waits
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn at(id: i64, task: Option<i64>, kind: &str, secs: i64) -> RunEvent {
        RunEvent {
            id: EventId::new(id),
            task_id: task.map(TaskId::new),
            goal_id: None,
            run_id: None,
            kind: kind.to_owned(),
            payload: json!({}),
            created_at: crate::domain::marks::utc_text(secs * 1000),
            actor: None,
        }
    }

    fn all(_: Option<TaskId>) -> bool {
        true
    }

    #[test]
    fn an_ask_opened_while_the_watcher_is_alive_is_seen_at_once() {
        let events = [
            at(1, None, INBOX_WATCHER_RETURNED, 1_000),
            at(2, Some(7), ASK_OPENED, 1_100),
            at(3, None, ASK_OPENED, 1_200),
        ];
        assert_eq!(
            seen_waits(&events, EventId::new(0), EventId::new(3), all),
            [0, 0]
        );
    }

    #[test]
    fn an_ask_opened_while_absent_waits_for_the_return() {
        let events = [
            at(1, None, INBOX_WATCHER_RETURNED, 1_000),
            at(2, None, INBOX_WATCHER_ABSENT, 1_100),
            at(3, Some(7), ASK_OPENED, 1_200),
            at(4, Some(8), ASK_OPENED, 1_500),
            at(5, None, INBOX_WATCHER_RETURNED, 1_800),
            at(6, Some(9), ASK_OPENED, 1_900),
        ];
        let upto = EventId::new(6);
        assert_eq!(
            seen_waits(&events, EventId::new(0), upto, all),
            [0, 300, 600]
        );
        // Not returned by the window's end: no sample yet.
        assert!(seen_waits(&events, EventId::new(0), EventId::new(4), all).is_empty());
        // Put in the window of the return, not of the opening.
        assert_eq!(
            seen_waits(&events, EventId::new(4), upto, all),
            [0, 300, 600]
        );
        assert_eq!(seen_waits(&events, EventId::new(5), upto, all), [0]);
        // The task's filter.
        assert_eq!(
            seen_waits(&events, EventId::new(0), upto, |task| task
                == Some(TaskId::new(7))),
            [600]
        );
    }

    #[test]
    fn asks_before_any_record_of_the_watcher_are_no_samples() {
        let events = [
            at(1, Some(7), ASK_OPENED, 1_000),
            at(2, None, INBOX_WATCHER_ABSENT, 1_100),
            at(3, Some(8), ASK_OPENED, 1_200),
            at(4, None, INBOX_WATCHER_RETURNED, 1_250),
        ];
        assert_eq!(
            seen_waits(&events, EventId::new(0), EventId::new(4), all),
            [50]
        );
        // A queue that never recorded the watcher has no sample.
        assert!(seen_waits(&events[..1], EventId::new(0), EventId::new(1), all).is_empty());
    }
}
