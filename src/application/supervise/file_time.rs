//! A file's time compared with a time the supervisor recorded (task 1050).
//!
//! A file's mtime (an idle or input marker, a receipt) keeps nanoseconds,
//! while an event's `created_at` keeps milliseconds, and so does every time
//! an adopting supervisor rebuilds from events (`at_event`,
//! [`recorded_at`], a payload's `*_ms`). Compared as they are, a file
//! written in the same millisecond as an event but before it looks newer
//! than the event. The file's time is therefore cut to the millisecond
//! first: a file written in the same millisecond as the time it is
//! compared with is taken as not after it. A marker that answers an input
//! comes a turn after it, never within its millisecond, so one in the same
//! millisecond is the one from before it; taking it for new would end a
//! wait early (a nudge answered, a recovery job's detection ended, a
//! receipt counted as rewritten), while the other error only waits for the
//! next marker. The design is in `docs/design/supervisor-lifecycle/
//! idle-without-receipt.md`.

use super::*;

/// `at` cut to the millisecond, the precision an event's `created_at`
/// keeps.
pub(super) fn to_millis(at: SystemTime) -> SystemTime {
    let since = at.duration_since(UNIX_EPOCH).unwrap_or_default();
    UNIX_EPOCH + Duration::from_millis(u64::try_from(since.as_millis()).unwrap_or(u64::MAX))
}

/// Whether a file written at `modified` was written after `at`, compared
/// to the millisecond: a file of the same millisecond is not.
pub(super) fn written_after(modified: SystemTime, at: SystemTime) -> bool {
    to_millis(modified) > to_millis(at)
}

/// When `event` was recorded, on the files' wall clock, to the millisecond
/// its `created_at` keeps; `None` when it cannot be read.
pub(super) fn event_time(event: &RunEvent) -> Option<SystemTime> {
    crate::domain::stats::timestamp_millis(&event.created_at)
        .map(|ms| UNIX_EPOCH + Duration::from_millis(u64::try_from(ms).unwrap_or(0)))
}

/// When `event` was recorded, as [`event_time`]; the epoch when it cannot
/// be read.
pub(super) fn recorded_at(event: &RunEvent) -> SystemTime {
    event_time(event).unwrap_or(UNIX_EPOCH)
}

/// `at` as the `sent_at` of a revise or conflict request (task 1197): unix
/// seconds with the millisecond below the point, the precision of
/// [`to_millis`], so that an adopter compares the files with it as the
/// supervisor that sent it did. Whole seconds would make a receipt or a
/// marker written earlier in the same second look written after it.
pub(super) fn request_sent_at(at: SystemTime) -> Value {
    let ms = to_millis(at)
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    json!(ms as f64 / 1000.0)
}

/// The `sent_at` of a recorded revise or conflict request, read back to
/// the millisecond it was written with (whole seconds before task 1197);
/// the epoch when not recorded.
pub(super) fn request_sent_at_of(payload: &Value) -> SystemTime {
    let ms = crate::domain::request_sent_at_millis(payload).unwrap_or_default();
    UNIX_EPOCH + Duration::from_millis(u64::try_from(ms).unwrap_or(0))
}

/// A time of `ms` milliseconds and `ns` nanoseconds past a fixed second,
/// for the tests of the millisecond's edge: no real clock. A time an adopter
/// reads from an event is one with `ns` 0; a file's time keeps `ns`.
#[cfg(test)]
pub(super) fn at_ns(ms: u64, ns: u64) -> SystemTime {
    UNIX_EPOCH
        + Duration::from_secs(1_000_000)
        + Duration::from_millis(ms)
        + Duration::from_nanos(ns)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(ms: u64, ns: u64) -> SystemTime {
        at_ns(ms, ns)
    }

    #[test]
    fn to_millis_cuts_below_the_millisecond() {
        assert_eq!(to_millis(at(250, 999_999)), at(250, 0));
        assert_eq!(to_millis(at(250, 0)), at(250, 0));
        assert_eq!(to_millis(UNIX_EPOCH), UNIX_EPOCH);
    }

    #[test]
    fn a_file_of_the_same_millisecond_is_not_written_after() {
        // An event recorded at 250.600 ms keeps 250 ms: a marker written
        // at 250.100 ms, before the event, is not newer than it.
        let event = to_millis(at(250, 600_000));
        assert!(!written_after(at(250, 100_000), event));
        assert!(!written_after(at(250, 999_999), event));
        // Nor is one of the same millisecond as an in-memory time.
        assert!(!written_after(at(250, 900_000), at(250, 100_000)));
        // The next millisecond is after, the one before is not.
        assert!(written_after(at(251, 0), event));
        assert!(!written_after(at(249, 999_999), event));
    }

    #[test]
    fn a_request_sent_at_is_read_back_to_its_millisecond() {
        // Sent at 250.600 ms past the second: recorded and read as 250 ms.
        let sent = at(250, 600_000);
        let recorded = json!({"sent_at": request_sent_at(sent)});
        let adopted = request_sent_at_of(&recorded);
        assert_eq!(adopted, at(250, 0));
        // A receipt or a marker written before it, in the same millisecond
        // or the same second, is not after it; one in a later millisecond
        // is, as it is for the supervisor that sent it.
        for before in [at(250, 100_000), at(0, 0), at(100, 0)] {
            assert!(!written_after(before, adopted));
            assert!(!written_after(before, sent));
        }
        assert!(written_after(at(251, 0), adopted));
        assert!(written_after(at(251, 0), sent));
        // Every millisecond of the second reads back exactly.
        for ms in 0..1000 {
            let recorded = json!({"sent_at": request_sent_at(at(ms, 999_999))});
            assert_eq!(request_sent_at_of(&recorded), at(ms, 0));
        }
        // A request recorded in whole seconds before task 1197 reads as
        // that second; one without a time as the epoch.
        assert_eq!(request_sent_at_of(&json!({"sent_at": 1_000_000})), at(0, 0));
        assert_eq!(request_sent_at_of(&json!({})), UNIX_EPOCH);
    }

    #[test]
    fn event_times_keep_milliseconds() {
        let event = RunEvent {
            id: EventId::new(1),
            task_id: None,
            goal_id: None,
            run_id: None,
            kind: "stall_nudged".into(),
            payload: json!({}),
            created_at: "2026-09-30T00:00:00.250Z".into(),
            actor: None,
        };
        let at = event_time(&event).unwrap();
        assert_eq!(at, to_millis(at));
        assert_eq!(recorded_at(&event), at);
        assert!(!written_after(at + Duration::from_micros(500), at));
        assert!(written_after(at + Duration::from_millis(1), at));
        let unreadable = RunEvent {
            created_at: "never".into(),
            ..event
        };
        assert_eq!(event_time(&unreadable), None);
        assert_eq!(recorded_at(&unreadable), UNIX_EPOCH);
    }
}
