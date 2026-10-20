//! How long an ask waited for the inbox's watcher to see it (task 1021,
//! the KPI `ask_seen_wait`): 0 seconds for an ask opened while the
//! watcher was alive, else the seconds to the watcher's return or to the
//! ask's end, whichever came first (ADR-t1332-1). The ask's end is its
//! first `ask_answered`, the runtime's own (`runtime_closed`) included, or
//! an `ask_closed` before any answer: an ask that ended before the watcher
//! came back waited no longer to be seen. The watcher's state is the
//! latest `inbox_watcher_absent` / `inbox_watcher_returned` before the
//! ask's `ask_opened`; an ask opened before either was recorded is no
//! sample, as the watchers' own records do not last. The supervisor
//! records the watcher's changes once a pass: an ask opened after the
//! watcher left but before its absence was recorded counts 0 seconds, and
//! a change while no supervisor runs is recorded at its next pass. Only
//! `kpi` reads it: the watcher is the inbox's, whatever the ask's kind.
use super::{
    EventId, RunEvent, TaskId,
    asks::{ask_ends, ask_key},
    timestamp_millis,
};
use crate::domain::EventKind;

/// The seconds each ask whose task `counts` accepts waited to be seen,
/// of those seen after `after` up to `upto`: an ask opened while the
/// watcher was alive is seen at its opening, one opened while it was
/// absent at the next `inbox_watcher_returned` or at its end, whichever
/// came first, and is put in the window of that event. An ask neither
/// seen nor ended at `upto` is no sample yet. Sorted.
pub fn seen_waits(
    events: &[RunEvent],
    after: EventId,
    upto: EventId,
    counts: impl Fn(Option<TaskId>) -> bool,
) -> Vec<i64> {
    let ends = ask_ends(events, upto);
    let mut alive: Option<bool> = None;
    // The asks waiting for the watcher's return: their opening (unix ms)
    // and their end's event, if they ended by `upto`.
    let mut unseen: Vec<(i64, Option<EventId>)> = Vec::new();
    let mut waits = Vec::new();
    let seen = |opened: i64, event: &RunEvent, waits: &mut Vec<i64>| {
        if event.id > after
            && let Some(at) = timestamp_millis(&event.created_at)
        {
            waits.push(((at - opened) / 1000).max(0));
        }
    };
    for event in events.iter().filter(|event| event.id <= upto) {
        unseen.retain(|&(opened, end)| {
            let ended = end == Some(event.id);
            if ended {
                seen(opened, event, &mut waits);
            }
            !ended
        });
        match event.kind.known() {
            Some(EventKind::InboxWatcherAbsent) => alive = Some(false),
            Some(EventKind::InboxWatcherReturned) => {
                alive = Some(true);
                for (opened, _) in unseen.drain(..) {
                    seen(opened, event, &mut waits);
                }
            }
            Some(EventKind::AskOpened) if counts(event.task_id) => match alive {
                None => {}
                Some(true) => {
                    if event.id > after {
                        waits.push(0);
                    }
                }
                Some(false) => {
                    if let Some(opened) = timestamp_millis(&event.created_at) {
                        let end = ends.get(&ask_key(event)).map(|(id, _)| *id);
                        unseen.push((opened, end));
                    }
                }
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
    use crate::domain::event_kind::ASK_OPENED;
    use crate::domain::event_kind::INBOX_WATCHER_ABSENT;
    use crate::domain::event_kind::INBOX_WATCHER_RETURNED;
    use serde_json::json;

    fn at(id: i64, task: Option<i64>, kind: &str, secs: i64) -> RunEvent {
        RunEvent {
            id: EventId::new(id),
            task_id: task.map(TaskId::new),
            goal_id: None,
            run_id: None,
            kind: kind.into(),
            payload: json!({}),
            created_at: crate::domain::marks::utc_text(secs * 1000),
            actor: None,
        }
    }

    fn ask(id: i64, kind: &str, payload: serde_json::Value, secs: i64) -> RunEvent {
        RunEvent {
            payload,
            ..at(id, Some(7), kind, secs)
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

    #[test]
    fn an_ask_that_ends_before_the_return_waits_up_to_its_end() {
        use crate::domain::event_kind::{ASK_ANSWERED, ASK_CLOSED};
        let events = [
            at(1, None, INBOX_WATCHER_ABSENT, 1_000),
            // A person answers from another terminal.
            ask(2, ASK_OPENED, json!({"ask_id": 1}), 1_100),
            // The runtime closes one (stuck_exit).
            ask(3, ASK_OPENED, json!({"ask_id": 2}), 1_200),
            // One that ends after the return.
            ask(4, ASK_OPENED, json!({"ask_id": 3}), 1_300),
            // One closed with no answer.
            ask(5, ASK_OPENED, json!({"ask_id": 4}), 1_400),
            ask(6, ASK_ANSWERED, json!({"ask_id": 1}), 1_150),
            ask(
                7,
                ASK_ANSWERED,
                json!({"ask_id": 2, "kind": "stuck_exit", "runtime_closed": true}),
                1_500,
            ),
            ask(8, ASK_CLOSED, json!({"ask_id": 4}), 1_600),
            // The person's answer is closed later: the answer is the end.
            ask(9, ASK_CLOSED, json!({"ask_id": 1}), 1_700),
            at(10, None, INBOX_WATCHER_RETURNED, 2_300),
            ask(11, ASK_ANSWERED, json!({"ask_id": 3}), 2_500),
        ];
        let upto = EventId::new(11);
        assert_eq!(
            seen_waits(&events, EventId::new(0), upto, all),
            [50, 200, 300, 1_000]
        );
        // Each in the window of its end, or of the return.
        assert_eq!(
            seen_waits(&events, EventId::new(0), EventId::new(6), all),
            [50]
        );
        assert_eq!(
            seen_waits(&events, EventId::new(6), EventId::new(9), all),
            [200, 300]
        );
        assert_eq!(seen_waits(&events, EventId::new(9), upto, all), [1_000]);
        // Neither back nor ended by the window's end: no sample yet.
        assert!(seen_waits(&events, EventId::new(0), EventId::new(5), all).is_empty());
    }

    #[test]
    fn an_ask_that_ends_while_the_watcher_stays_away_counts_at_its_end() {
        use crate::domain::event_kind::ASK_ANSWERED;
        let events = [
            at(1, None, INBOX_WATCHER_ABSENT, 1_000),
            ask(2, ASK_OPENED, json!({"ask_id": 1}), 1_100),
            ask(3, ASK_OPENED, json!({"ask_id": 2}), 1_200),
            ask(
                4,
                ASK_ANSWERED,
                json!({"ask_id": 1, "runtime_closed": true}),
                1_400,
            ),
        ];
        // The watcher never came back: the ended ask counts in its end's
        // window, the open one is no sample.
        assert_eq!(
            seen_waits(&events, EventId::new(3), EventId::new(4), all),
            [300]
        );
        assert!(seen_waits(&events, EventId::new(4), EventId::new(4), all).is_empty());
    }

    #[test]
    fn an_ask_opened_while_alive_counts_zero_whatever_its_end() {
        use crate::domain::event_kind::ASK_ANSWERED;
        let events = [
            at(1, None, INBOX_WATCHER_RETURNED, 1_000),
            ask(2, ASK_OPENED, json!({"ask_id": 1}), 1_100),
            at(3, None, INBOX_WATCHER_ABSENT, 1_200),
            ask(
                4,
                ASK_ANSWERED,
                json!({"ask_id": 1, "runtime_closed": true}),
                1_400,
            ),
            at(5, None, INBOX_WATCHER_RETURNED, 2_000),
        ];
        assert_eq!(
            seen_waits(&events, EventId::new(0), EventId::new(5), all),
            [0]
        );
    }
}
