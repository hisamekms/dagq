//! `dagq mark`: record and retract a person's or a planner's change mark
//! (ADR-0051 decision 12) through the [`MarkLog`] port, the time `--at` is
//! checked against read from the injected [`Clock`] (ADR-0013 policy 7).

use anyhow::{Context, Result, ensure};
use serde_json::Value;
use std::time::UNIX_EPOCH;

use super::{Clock, MarkLog};
use crate::domain::marks;
use crate::domain::stats::{Cursor, timestamp_millis};
use crate::domain::{EventId, EventKind};

/// `dagq mark <label>`: record a person's or a planner's change mark,
/// `at` the time the change took effect when it is marked afterwards.
/// Returns the mark as `marks` lists it.
pub fn record_mark(
    log: &dyn MarkLog,
    clock: &dyn Clock,
    label: &str,
    note: Option<&str>,
    at: Option<Cursor>,
    by: &str,
) -> Result<Value> {
    let events = log.events()?;
    let at = at
        .map(|cursor| {
            marks::cursor_time(cursor, &events)
                .context("--at names an event id this queue does not have")
        })
        .transpose()?;
    if let Some(at) = &at {
        let now = clock.system_time().duration_since(UNIX_EPOCH)?.as_millis();
        ensure!(
            timestamp_millis(at).is_some_and(|at| i128::from(at) <= now as i128),
            "--at {at} is in the future; a mark stands for a change already made"
        );
    }
    let payload = marks::mark_payload(label, note, at, by).map_err(anyhow::Error::msg)?;
    let id = log.record_event(EventKind::MarkRecorded, payload)?;
    recorded_mark(log, id)
}

/// `dagq mark --retract <id>`: record that the mark `target` was no change;
/// the mark stays, retracted.
pub fn retract_mark(log: &dyn MarkLog, target: EventId, by: &str) -> Result<Value> {
    let payload =
        marks::retraction_payload(&log.events()?, target, by).map_err(anyhow::Error::msg)?;
    let id = log.record_event(EventKind::MarkRetracted, payload)?;
    recorded_mark(log, id)
}

fn recorded_mark(log: &dyn MarkLog, id: EventId) -> Result<Value> {
    let mark = marks::marks(&log.events()?, None, None)
        .into_iter()
        .find(|mark| mark.id == Some(id))
        .context("the recorded mark is not listed")?;
    Ok(serde_json::to_value(mark)?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::RunEvent;
    use crate::domain::marks::utc_text;
    use serde_json::json;
    use std::cell::RefCell;
    use std::time::{Duration, SystemTime};

    /// 2026-10-04T00:00:00.000Z.
    const NOW: i64 = 1_791_072_000_000;

    struct At(SystemTime);

    impl Clock for At {
        fn system_time(&self) -> SystemTime {
            self.0
        }

        fn monotonic(&self) -> std::time::Instant {
            std::time::Instant::now()
        }
    }

    fn fixed() -> At {
        At(UNIX_EPOCH + Duration::from_millis(NOW as u64))
    }

    #[derive(Default)]
    struct Log(RefCell<Vec<RunEvent>>);

    impl MarkLog for Log {
        fn events(&self) -> Result<Vec<RunEvent>> {
            Ok(self.0.borrow().clone())
        }

        fn record_event(&self, kind: EventKind, payload: Value) -> Result<EventId> {
            let mut events = self.0.borrow_mut();
            let id = EventId::new(events.len() as i64 + 1);
            events.push(RunEvent {
                id,
                task_id: None,
                goal_id: None,
                run_id: None,
                kind: kind.as_str().to_owned(),
                payload,
                created_at: utc_text(NOW),
                actor: None,
            });
            Ok(id)
        }
    }

    #[test]
    fn a_mark_at_the_clocks_time_is_recorded_with_that_time() {
        let log = Log::default();
        let mark = record_mark(
            &log,
            &fixed(),
            "parallel 4",
            Some(" more slots "),
            Some(Cursor::Time(NOW)),
            "person",
        )
        .unwrap();
        assert_eq!(mark["id"], json!(1));
        assert_eq!(mark["at"], json!(utc_text(NOW)));
        let recorded = &log.0.borrow()[0];
        assert_eq!(recorded.kind, "mark_recorded");
        assert_eq!(
            recorded.payload,
            json!({"label": "parallel 4", "note": "more slots", "at": utc_text(NOW), "by": "person"})
        );
    }

    #[test]
    fn a_mark_one_millisecond_after_the_clock_is_refused_and_not_recorded() {
        let log = Log::default();
        let error = record_mark(
            &log,
            &fixed(),
            "parallel 4",
            None,
            Some(Cursor::Time(NOW + 1)),
            "person",
        )
        .unwrap_err();
        assert_eq!(
            error.to_string(),
            format!(
                "--at {} is in the future; a mark stands for a change already made",
                utc_text(NOW + 1)
            )
        );
        assert!(log.0.borrow().is_empty());
    }

    #[test]
    fn a_mark_without_at_does_not_read_the_future_check() {
        // A clock before the epoch fails `duration_since`; no `--at`, no read.
        let log = Log::default();
        let before_epoch = At(UNIX_EPOCH - Duration::from_secs(1));
        let mark = record_mark(&log, &before_epoch, "x", None, None, "person").unwrap();
        assert_eq!(mark["at"], json!(utc_text(NOW)));
        assert_eq!(log.0.borrow()[0].payload["at"], Value::Null);
    }

    #[test]
    fn an_at_naming_a_missing_event_is_refused() {
        let error = record_mark(
            &Log::default(),
            &fixed(),
            "x",
            None,
            Some(Cursor::Event(EventId::new(9))),
            "person",
        )
        .unwrap_err();
        assert_eq!(
            error.to_string(),
            "--at names an event id this queue does not have"
        );
    }

    #[test]
    fn a_retraction_is_recorded_once_and_lists_the_retraction() {
        let log = Log::default();
        record_mark(&log, &fixed(), "x", None, None, "person").unwrap();
        let retraction = retract_mark(&log, EventId::new(1), "planner").unwrap();
        assert_eq!(retraction["id"], json!(2));
        assert_eq!(retraction["kind"], json!("mark_retracted"));
        let again = retract_mark(&log, EventId::new(1), "planner").unwrap_err();
        assert_eq!(again.to_string(), "mark 1 was already retracted by 2");
        assert_eq!(log.0.borrow().len(), 2);
    }
}
