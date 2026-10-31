//! `dagq mark` and `dagq marks`: record, retract and list a person's or a
//! planner's change marks and the other marks (ADR-0051 decisions 10–13)
//! through the [`MarkLog`] port, the time `--at` is checked against read
//! from the injected [`Clock`] (ADR-0013 policy 7).
//!
//! None of them reads every event of the queue. `marks` reads every event
//! of [`marks::READ_KINDS`] a page at a time, and for an event id cursor
//! the few events at or before it that give its time; `mark --at ID` reads
//! that one event; `mark --retract` reads the mark it retracts and the
//! retractions after it that name it; and the mark each prints is read
//! from the event just recorded and those retractions.

use anyhow::{Context, Result, ensure};
use serde_json::Value;
use std::time::UNIX_EPOCH;

use super::{Clock, MarkLog};
use crate::domain::marks::{self, Mark};
use crate::domain::stats::{Cursor, timestamp_millis};
use crate::domain::{EventId, EventKind, RunEvent};

/// The most events one read of the marks' kinds takes.
pub const PAGE: usize = 1000;
/// How many ids back from an event id cursor one read for its time looks;
/// it reads further back, a span at a time, only past ids no event has.
pub const CURSOR_SPAN: i64 = 64;

/// `dagq marks`: the recorded and the derived marks whose time is after
/// `since` and at or before `until`, as [`marks::marks`] of every event of
/// the queue lists them. An event id cursor stands for the latest time of
/// the events at or before it within the nearest [`CURSOR_SPAN`] that has
/// one, which is the latest time at or before it as the queue writes the
/// times of its events in the order of their ids.
pub fn list_marks<L: MarkLog + ?Sized>(
    log: &L,
    since: Option<Cursor>,
    until: Option<Cursor>,
) -> Result<Vec<Mark>> {
    let newest = log.newest_event_id()?;
    let since = since
        .map(|cursor| cursor_millis(log, cursor, newest))
        .transpose()?;
    let until = until
        .map(|cursor| cursor_millis(log, cursor, newest))
        .transpose()?;
    let kinds = marks::READ_KINDS.map(EventKind::as_str);
    let events = events_of_kinds(log, &kinds, EventId::new(0), newest)?;
    Ok(marks::marks(
        &events,
        since.map(Cursor::Time),
        until.map(Cursor::Time),
    ))
}

/// Every event of `kinds` with `after < id <= upto`, read [`PAGE`] at a
/// time, oldest first.
fn events_of_kinds<L: MarkLog + ?Sized>(
    log: &L,
    kinds: &[&str],
    mut after: EventId,
    upto: EventId,
) -> Result<Vec<RunEvent>> {
    let mut events = Vec::new();
    loop {
        let page = log.events_of_kinds(kinds, after, upto, PAGE)?;
        let full = page.len() == PAGE;
        match page.last() {
            Some(last) => after = last.id,
            None => break,
        }
        events.extend(page);
        if !full {
            break;
        }
    }
    Ok(events)
}

/// The time `cursor` stands for ([`marks::cursor_millis`]), an event id
/// read off the events of the nearest [`CURSOR_SPAN`] at or before it (and
/// at or before `newest`) that has one.
fn cursor_millis<L: MarkLog + ?Sized>(log: &L, cursor: Cursor, newest: EventId) -> Result<i64> {
    let Cursor::Event(id) = cursor else {
        return Ok(marks::cursor_millis(cursor, &[]));
    };
    let mut upto = id.as_i64().min(newest.as_i64());
    while upto > 0 {
        let after = (upto - CURSOR_SPAN).max(0);
        let near = log.events_in_range(
            EventId::new(after),
            EventId::new(upto),
            CURSOR_SPAN as usize,
        )?;
        if near
            .iter()
            .any(|event| timestamp_millis(&event.created_at).is_some())
        {
            return Ok(marks::cursor_millis(cursor, &near));
        }
        upto = after;
    }
    Ok(i64::MIN)
}

/// The event `id`, if the queue has it.
fn event<L: MarkLog + ?Sized>(log: &L, id: EventId) -> Result<Option<RunEvent>> {
    Ok(log
        .events_in_range(EventId::new(id.as_i64().saturating_sub(1)), id, 1)?
        .into_iter()
        .find(|event| event.id == id))
}

/// The event `id` and the retractions recorded after it that name it.
fn with_retractions<L: MarkLog + ?Sized>(log: &L, id: EventId) -> Result<Vec<RunEvent>> {
    let Some(event) = event(log, id)? else {
        return Ok(Vec::new());
    };
    let retractions = events_of_kinds(
        log,
        &[EventKind::MarkRetracted.as_str()],
        id,
        log.newest_event_id()?,
    )?;
    Ok(std::iter::once(event)
        .chain(
            retractions
                .into_iter()
                .filter(|retraction| retraction.payload["mark"].as_i64() == Some(id.as_i64())),
        )
        .collect())
}

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
    let at = at
        .map(|cursor| {
            let named = match cursor {
                Cursor::Event(id) => event(log, id)?.into_iter().collect(),
                Cursor::Time(_) => Vec::new(),
            };
            marks::cursor_time(cursor, &named)
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
    let payload = marks::retraction_payload(&with_retractions(log, target)?, target, by)
        .map_err(anyhow::Error::msg)?;
    let id = log.record_event(EventKind::MarkRetracted, payload)?;
    recorded_mark(log, id)
}

fn recorded_mark(log: &dyn MarkLog, id: EventId) -> Result<Value> {
    let mark = marks::marks(&with_retractions(log, id)?, None, None)
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

    /// A log that answers only the narrow reads, and fails a test that
    /// reads what no mark needs: another kind than the marks' through the
    /// kinds' read, more than a [`PAGE`] at once, or a range of any kind
    /// wider than a [`CURSOR_SPAN`] (a read of every event is one).
    #[derive(Default)]
    struct Log {
        events: RefCell<Vec<RunEvent>>,
        /// The ids every read returned.
        read: RefCell<Vec<EventId>>,
    }

    impl Log {
        fn of(events: Vec<RunEvent>) -> Self {
            Self {
                events: RefCell::new(events),
                read: RefCell::default(),
            }
        }

        fn answer(&self, keep: impl Fn(&RunEvent) -> bool, limit: usize) -> Vec<RunEvent> {
            let events: Vec<RunEvent> = self
                .events
                .borrow()
                .iter()
                .filter(|event| keep(event))
                .take(limit)
                .cloned()
                .collect();
            self.read
                .borrow_mut()
                .extend(events.iter().map(|event| event.id));
            events
        }
    }

    impl MarkLog for Log {
        fn newest_event_id(&self) -> Result<EventId> {
            Ok(self
                .events
                .borrow()
                .last()
                .map_or(EventId::new(0), |event| event.id))
        }

        fn events_of_kinds(
            &self,
            kinds: &[&str],
            after: EventId,
            upto: EventId,
            limit: usize,
        ) -> Result<Vec<RunEvent>> {
            assert!(limit <= PAGE, "a read of {limit} events at once");
            let read_kinds = marks::READ_KINDS.map(EventKind::as_str);
            assert!(
                kinds.iter().all(|kind| read_kinds.contains(kind)),
                "a read of {kinds:?}, not only the marks' kinds"
            );
            Ok(self.answer(
                |event| {
                    event.id > after && event.id <= upto && kinds.contains(&event.kind.as_str())
                },
                limit,
            ))
        }

        fn events_in_range(
            &self,
            after: EventId,
            upto: EventId,
            limit: usize,
        ) -> Result<Vec<RunEvent>> {
            assert!(
                upto.as_i64() - after.as_i64() <= CURSOR_SPAN && limit <= CURSOR_SPAN as usize,
                "a read of every kind from {after} to {upto}"
            );
            Ok(self.answer(|event| event.id > after && event.id <= upto, limit))
        }

        fn record_event(&self, kind: EventKind, payload: Value) -> Result<EventId> {
            let mut events = self.events.borrow_mut();
            let id = EventId::new(events.last().map_or(0, |event| event.id.as_i64()) + 1);
            events.push(RunEvent {
                id,
                task_id: None,
                goal_id: None,
                run_id: None,
                kind: kind.as_str().into(),
                payload,
                created_at: utc_text(NOW),
                actor: None,
            });
            Ok(id)
        }
    }

    fn queue_event(id: i64, kind: EventKind, payload: Value, at: i64) -> RunEvent {
        RunEvent {
            id: EventId::new(id),
            task_id: None,
            goal_id: None,
            run_id: None,
            kind: kind.as_str().into(),
            payload,
            created_at: utc_text(at),
            actor: None,
        }
    }

    fn claim(id: i64, version: &str, at: i64) -> RunEvent {
        RunEvent {
            task_id: Some(crate::domain::TaskId::new(id)),
            run_id: Some(
                crate::domain::RunId::new(format!("00000000-0000-4000-8000-{id:012}")).unwrap(),
            ),
            ..queue_event(
                id,
                EventKind::RunClaimed,
                json!({"dagq_version": version, "parallel": 4}),
                at,
            )
        }
    }

    /// A queue of every sort of mark among many events no mark is read
    /// off, with a gap in its ids: a start, a claim of each version (the
    /// third derived), a person's mark of an earlier time written late, a
    /// retracted `[run.env]` change, and a pruned stop.
    fn busy_queue() -> Vec<RunEvent> {
        let minute = 60_000;
        let time = |id: i64| NOW - 10_000 * minute + id * minute;
        let mut events = Vec::new();
        let mut id = 0;
        for step in 0..3000 {
            if step == 2000 {
                // Ids no event has.
                id += 200;
                continue;
            }
            id += 1;
            let at = time(id);
            events.push(match step {
                10 => queue_event(
                    id,
                    EventKind::SupervisorStarted,
                    json!({"dagq_version": "v1", "parallel": 4}),
                    at,
                ),
                100 => claim(id, "v1", at),
                1500 => claim(id, "v2", at),
                1800 => queue_event(
                    id,
                    EventKind::RunEnvChanged,
                    json!({"hash": "h", "changed": ["A"]}),
                    at,
                ),
                2400 => claim(id, "v3", at),
                2500 => queue_event(
                    id,
                    EventKind::MarkRetracted,
                    json!({"mark": 1801, "label": ""}),
                    at,
                ),
                2600 => queue_event(
                    id,
                    EventKind::MarkRecorded,
                    json!({"label": "late", "at": utc_text(at - 2000 * minute)}),
                    at,
                ),
                2700 => queue_event(
                    id,
                    EventKind::SupervisorStopped,
                    json!({"outcome": "pruned", "last_heartbeat_at": (at - 100 * minute) / 1000}),
                    at,
                ),
                _ => queue_event(id, EventKind::ObserveStarted, json!({}), at),
            });
        }
        events
    }

    #[test]
    fn marks_read_only_their_kinds_and_list_what_every_event_lists() {
        let events = busy_queue();
        let time = |id: i64| {
            timestamp_millis(
                &events
                    .iter()
                    .find(|e| e.id.as_i64() == id)
                    .unwrap()
                    .created_at,
            )
            .unwrap()
        };
        let newest = events.last().unwrap().id.as_i64();
        let cursors = [
            None,
            Some(Cursor::Event(EventId::new(1))),
            // At a mark (its own time is outside `since` and inside `until`).
            Some(Cursor::Event(EventId::new(1501))),
            Some(Cursor::Event(EventId::new(1500))),
            // Within the ids no event has, and past the newest.
            Some(Cursor::Event(EventId::new(2050))),
            Some(Cursor::Event(EventId::new(newest + 500))),
            Some(Cursor::Time(time(1801))),
            // The time a person's late mark took effect.
            Some(Cursor::Time(time(800))),
            Some(Cursor::Time(time(2600) - 1)),
            Some(Cursor::Time(time(2600))),
        ];
        let mut listed = 0;
        for since in cursors {
            for until in cursors {
                let log = Log::of(events.clone());
                let read = list_marks(&log, since, until).unwrap();
                assert_eq!(
                    read,
                    marks::marks(&events, since, until),
                    "{since:?}..{until:?}"
                );
                listed += read.len();
                let others = log
                    .read
                    .borrow()
                    .iter()
                    .filter(|id| {
                        let event = events.iter().find(|e| e.id == **id).unwrap();
                        !event.kind.is_in(&marks::READ_KINDS)
                    })
                    .count();
                assert!(
                    others <= 2 * CURSOR_SPAN as usize,
                    "{since:?}..{until:?}: {others}"
                );
            }
        }
        assert!(listed > 0);
        let all = marks::marks(&events, None, None);
        let kinds: Vec<&str> = all.iter().map(|mark| mark.kind.as_str()).collect();
        assert_eq!(
            kinds,
            [
                "supervisor_started",
                "mark_recorded",
                "derived:dagq_version",
                "run_env_changed",
                "derived:dagq_version",
                "mark_retracted",
                "supervisor_stopped",
            ]
        );
        assert_eq!(all[3].retracted_by, Some(EventId::new(2700)));
    }

    #[test]
    fn marks_read_their_kinds_a_page_at_a_time() {
        let events: Vec<RunEvent> = (1..=2 * PAGE as i64 + 1)
            .map(|id| claim(id, if id % 2 == 0 { "a" } else { "b" }, NOW + id))
            .collect();
        let log = Log::of(events.clone());
        let read = list_marks(&log, None, None).unwrap();
        assert_eq!(read.len(), 2 * PAGE);
        assert_eq!(read, marks::marks(&events, None, None));
        assert_eq!(log.read.borrow().len(), events.len());
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
        let recorded = &log.events.borrow()[0];
        assert_eq!(recorded.kind, EventKind::MarkRecorded);
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
        assert!(log.events.borrow().is_empty());
    }

    #[test]
    fn a_mark_without_at_does_not_read_the_future_check() {
        // A clock before the epoch fails `duration_since`; no `--at`, no read.
        let log = Log::default();
        let before_epoch = At(UNIX_EPOCH - Duration::from_secs(1));
        let mark = record_mark(&log, &before_epoch, "x", None, None, "person").unwrap();
        assert_eq!(mark["at"], json!(utc_text(NOW)));
        assert_eq!(log.events.borrow()[0].payload["at"], Value::Null);
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
        assert_eq!(log.events.borrow().len(), 2);
    }

    #[test]
    fn mark_reads_only_the_event_it_names_and_the_retractions_of_its_mark() {
        let log = Log::of(busy_queue());
        let at = record_mark(
            &log,
            &fixed(),
            "x",
            None,
            Some(Cursor::Event(EventId::new(1501))),
            "person",
        )
        .unwrap();
        let named = log.events.borrow()[1500].clone();
        assert_eq!(named.id, EventId::new(1501));
        assert_eq!(at["at"], json!(named.created_at));
        let id = at["id"].as_i64().unwrap();
        // The named event, then the recorded one: no retraction is after it.
        assert_eq!(log.read.take(), [EventId::new(1501), EventId::new(id)]);
        let again = retract_mark(&log, EventId::new(1801), "planner").unwrap_err();
        assert_eq!(again.to_string(), "mark 1801 was already retracted by 2700");
        assert_eq!(log.read.take(), [EventId::new(1801), EventId::new(2700)]);
        let retraction = retract_mark(&log, EventId::new(id), "planner").unwrap();
        assert_eq!(retraction["kind"], json!("mark_retracted"));
        assert_eq!(retraction["detail"]["mark"], json!(id));
        let retraction = retraction["id"].as_i64().unwrap();
        // The mark, then the retraction just recorded.
        assert_eq!(
            log.read.take(),
            [EventId::new(id), EventId::new(retraction)]
        );
    }
}
