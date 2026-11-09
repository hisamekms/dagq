//! A prototype of a window's read that holds its records' payloads as their
//! JSON text instead of [`serde_json::Value`]'s tree, to be decoded where a
//! fold reads one (docs/plans/ledger-window-payload-text-spike.md). No read
//! of `stats`, `kpi` or `report` makes it: it is measured, not wired.
//!
//! The entry is
//! [`crate::application::ledger_window_payload_text`]: the read of
//! [`crate::application::LedgerStore::ledger_window`] page by page, the same
//! rows and the same order, with each row's records turned to
//! [`TextEvent`]s as the row is taken ([`WindowRows::payload_text`]): a read
//! holds the trees of one page of rows at most (the store gives a page whole,
//! up to [`super::ROWS_PER_PAGE`] rows, before they are taken one by one). It
//! gives a [`TextWindow`]: what the window holds before a fold reads it.
//! [`TextWindow::into_window`] decodes every record back into the
//! [`LedgerWindow`] today's folds take, so that a measure can fold it and
//! compare the answer with the runtime's read; the decoded window is held
//! whole while it is folded (`examples/kpi_read_memory.rs`, `whole <dir>
//! <read> text`, counts it in the process's peak).
//! [`TextWindow::into_window_of_kinds`] decodes by kind: only the payloads of
//! the kinds the folds read, the others' records kept without one
//! (`whole <dir> <read> text-kinds`, with the kinds the example names); the
//! trees it makes are held while the window is folded too. A read whose
//! readers decode a payload where they read it, and drop it after, has no
//! prototype here: its readers take today's [`RunEvent`]s.
//!
//! A later Spike that reads fewer of the queue rows' columns takes the same
//! entry over its own store: the records it gives are turned to text alike.

use super::{
    Earlier, LedgerRow, LedgerWindow, RecordGoals, WindowQuery, WindowRows, of_records, settled,
};
use crate::domain::{EventActor, EventId, GoalId, RecordedKind, RunEvent, RunId, TaskId};

/// A [`RunEvent`] whose payload is its JSON text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TextEvent {
    pub id: EventId,
    pub task_id: Option<TaskId>,
    pub goal_id: Option<GoalId>,
    pub run_id: Option<RunId>,
    pub kind: RecordedKind,
    pub payload: Box<str>,
    pub created_at: String,
    pub actor: Option<EventActor>,
}

impl TextEvent {
    /// `event` with its payload's tree dropped for its text.
    pub fn of(event: RunEvent) -> Self {
        Self {
            payload: event.payload.to_string().into_boxed_str(),
            id: event.id,
            task_id: event.task_id,
            goal_id: event.goal_id,
            run_id: event.run_id,
            kind: event.kind,
            created_at: event.created_at,
            actor: event.actor,
        }
    }

    /// The event with its payload decoded: the [`RunEvent`] [`Self::of`]
    /// was given (a payload text is read back as the value it was written of).
    pub fn decode(self) -> RunEvent {
        RunEvent {
            payload: serde_json::from_str(&self.payload)
                .expect("a payload's text is the JSON of a value"),
            id: self.id,
            task_id: self.task_id,
            goal_id: self.goal_id,
            run_id: self.run_id,
            kind: self.kind,
            created_at: self.created_at,
            actor: self.actor,
        }
    }

    /// The event with its payload decoded when `reads` its kind, and with
    /// none ([`serde_json::Value::Null`]) when not: the tree of a payload no
    /// fold reads is never made.
    pub fn decode_if(self, reads: impl Fn(&RecordedKind) -> bool) -> RunEvent {
        if reads(&self.kind) {
            return self.decode();
        }
        RunEvent {
            payload: serde_json::Value::Null,
            id: self.id,
            task_id: self.task_id,
            goal_id: self.goal_id,
            run_id: self.run_id,
            kind: self.kind,
            created_at: self.created_at,
            actor: self.actor,
        }
    }
}

/// The records of a read as text: the rows' and those
/// [`WindowRows::take_earlier`] puts among them, apart as [`WindowRows`]
/// keeps them.
#[derive(Debug, Default)]
pub(super) struct TextRecords {
    records: Vec<TextEvent>,
    earlier: Vec<TextEvent>,
}

impl TextRecords {
    /// Turn the records a row just gave to text.
    pub(super) fn take(&mut self, records: &mut Vec<RunEvent>) {
        self.records.extend(records.drain(..).map(TextEvent::of));
    }

    /// Turn the records [`WindowRows::take_earlier`] kept to text.
    pub(super) fn take_earlier(&mut self, records: &mut Vec<RunEvent>) {
        self.earlier.extend(records.drain(..).map(TextEvent::of));
    }
}

/// What a window read with its payloads as text holds before it is folded.
#[derive(Debug)]
pub struct TextWindow {
    /// Every record read, ascending id, each once.
    pub records: Vec<TextEvent>,
    rows: Vec<LedgerRow>,
    goals: RecordGoals,
    earlier: Earlier,
}

impl TextWindow {
    /// The window `query` asks of what was read, every payload decoded: the
    /// [`LedgerWindow`] [`WindowRows::into_window`] gives of the same rows.
    pub fn into_window(self, query: &WindowQuery) -> LedgerWindow {
        self.into_window_of_kinds(query, |_| true)
    }

    /// [`Self::into_window`] with the payloads decoded of the kinds `reads`
    /// only ([`TextEvent::decode_if`]), all of them before the window is
    /// folded, the others' records kept without one. Each text is dropped as
    /// its record is made, so a decoded record and its text are not held
    /// together past it.
    pub fn into_window_of_kinds(
        self,
        query: &WindowQuery,
        reads: impl Fn(&RecordedKind) -> bool,
    ) -> LedgerWindow {
        let records = self
            .records
            .into_iter()
            .map(|event| event.decode_if(&reads))
            .collect();
        let mut window = of_records(records, self.rows, &self.goals, query);
        window.earlier = self.earlier;
        window
    }
}

impl WindowRows {
    /// [`Self::new`] of a read that holds its records as text, with room for
    /// `events` of them.
    pub fn payload_text(query: &WindowQuery, events: usize) -> Self {
        let mut read = Self::new(query, 0);
        read.text = Some(TextRecords {
            records: Vec::with_capacity(events.saturating_add(super::EVENTS_DURING_A_READ)),
            earlier: Vec::new(),
        });
        read
    }

    /// [`Self::into_window`] before its records are decoded, of a read made
    /// by [`Self::payload_text`].
    ///
    /// # Panics
    ///
    /// When the read holds its records as values ([`Self::new`]).
    pub fn into_text_window(mut self) -> TextWindow {
        let mut text = self.text.take().expect("a read of the payloads as text");
        self.rows.shrink_to_fit();
        text.records.append(&mut text.earlier);
        text.records.shrink_to_fit();
        text.records.sort_by_key(|event| event.id);
        text.records.dedup_by_key(|event| event.id);
        let records = text.records;
        let earlier = settled(self.earlier, |id| {
            records.binary_search_by_key(&id, |read| read.id).is_ok()
        });
        TextWindow {
            records,
            rows: self.rows,
            goals: self.goals,
            earlier,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_payload_reads_back_as_the_value_it_was_written_of() {
        let event = RunEvent {
            id: EventId::new(7),
            task_id: Some(TaskId::new(3)),
            goal_id: None,
            run_id: None,
            kind: RecordedKind::from_name("supervisor_alive"),
            payload: serde_json::json!({"b": [1, 2.5, null], "a": {"x": "y"}, "f": 0.1}),
            created_at: "2030-01-01T00:00:00.000Z".to_owned(),
            actor: None,
        };
        let text = TextEvent::of(event.clone());
        assert_eq!(
            &*text.payload,
            r#"{"a":{"x":"y"},"b":[1,2.5,null],"f":0.1}"#
        );
        assert_eq!(text.decode(), event);
    }

    #[test]
    fn a_kind_not_read_keeps_its_record_without_a_payload() {
        let event = RunEvent {
            id: EventId::new(1),
            task_id: None,
            goal_id: None,
            run_id: None,
            kind: RecordedKind::from_name("supervisor_alive"),
            payload: serde_json::json!({"supervisor": "s"}),
            created_at: "2030-01-01T00:00:00.000Z".to_owned(),
            actor: None,
        };
        let read = TextEvent::of(event.clone()).decode_if(|_| true);
        assert_eq!(read, event);
        let unread =
            TextEvent::of(event.clone()).decode_if(|kind| kind.as_str() != "supervisor_alive");
        assert_eq!(unread.payload, serde_json::Value::Null);
        assert_eq!(
            (unread.id, unread.kind, unread.created_at),
            (event.id, event.kind, event.created_at)
        );
    }

    #[test]
    #[should_panic(expected = "a read of the payloads as text")]
    fn a_read_of_values_has_no_text_window() {
        let _ = WindowRows::new(&WindowQuery::default(), 0).into_text_window();
    }
}
