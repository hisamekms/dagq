//! The ports of 観測と分析 (docs/design/architecture.md, section
//! "観測と分析"): the reads of the queue's records, the observer's log
//! and the marks, over the [`EventStore`] in [`super::shared`].

use super::execution::{CoordinationReads, RunLog};
use super::host::SupervisorReads;
use super::planning::{PlanningRecords, TaskReads};
use super::shared::{AskStore, EventStore};
use crate::domain::environment::{
    EnvironmentAttached, EnvironmentWait, Resources, RunProcesses, RunResource,
};
use crate::domain::host_metrics::{HostSample, ImportMark, NodeSample, SampleSource};
use crate::domain::ledger::{
    LedgerKind, LedgerRow,
    window::{LedgerWindow, WindowQuery},
};
use crate::domain::session_step::{
    ResourceSample, SeqRange, SessionOwner, SessionResource, SessionStep, Step, StepTarget,
    StepsRecorded,
};
use crate::domain::telemetry::{ReportRecorded, TelemetryReport};
use crate::domain::{AskId, EventId, EventKind, FindingView, RunEvent, RunId, TaskId};
use anyhow::Result;

/// What one role wrote in a window ([`ObserverLog::written_by`]): finding
/// ids it recorded, updated and closed (resolved or dismissed), its
/// asks' ids, and the findings it left without an ask.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct WrittenBy {
    pub recorded: Vec<i64>,
    pub updated: Vec<i64>,
    pub closed: Vec<i64>,
    pub asks: Vec<i64>,
    /// The findings it recorded or updated and did not close that have
    /// no `blocked` ask: none it opened in the window and none open now
    /// (ADR-t451-1 decision 2, the reading it kept to the finding).
    pub without_ask: Vec<i64>,
}

/// What the observer reads of the queue's record of its observations and
/// of what it wrote (ADR-0044).
pub trait ObserverLog {
    /// The id of the last event recorded before `unix` (seconds), 0 when
    /// there is none: a cursor that reads everything from that time on.
    fn event_id_before(&self, unix: i64) -> Result<EventId>;
    /// The newest ask's ID: the mark [`Self::written_by`] counts past.
    fn ask_high_water(&self) -> Result<AskId>;
    /// What `role` wrote after the marks: the findings it recorded,
    /// updated and closed after `event_id` and its asks after `ask_id`.
    fn written_by(&self, role: &str, event_id: EventId, ask_id: AskId) -> Result<WrittenBy>;
    /// The last observation of `mode` that ran its agent: the id and the
    /// payload of its `observe_finished` (a skipped one is not).
    fn last_observation(&self, mode: &str) -> Result<Option<(EventId, serde_json::Value)>>;
    /// How many events after `after` the observer did not write itself
    /// (ADR-0044), its own spans (`span_kind`) excluded.
    fn events_besides(&self, role: &str, span_kind: &str, after: EventId) -> Result<i64>;
    /// The newest `limit` observations, newest first: each
    /// `observe_finished` with the `observe_started` of the same directory
    /// when there is one (a skipped observation has none).
    fn observations(&self, limit: usize) -> Result<Vec<(RunEvent, Option<RunEvent>)>>;
}

/// What the mark commands read and record (ADR-0051 decision 12), with no
/// read of every event of the queue: the newest event id each command's
/// reads stop at, the events of named kinds in an id range a page at a
/// time (the marks' kinds, [`crate::domain::marks::READ_KINDS`]), the
/// events of any kind in a short id range (an event the command names, or
/// the events behind an event id cursor), and a new event of the queue
/// itself. Every [`EventStore`] is one.
pub trait MarkLog {
    /// The newest event id, 0 for an empty queue.
    fn newest_event_id(&self) -> Result<EventId>;
    /// The events of one of `kinds` with `after < id <= upto`, oldest
    /// first, at most `limit`.
    fn events_of_kinds(
        &self,
        kinds: &[&str],
        after: EventId,
        upto: EventId,
        limit: usize,
    ) -> Result<Vec<RunEvent>>;
    /// The events of any kind with `after < id <= upto`, oldest first, at
    /// most `limit`.
    fn events_in_range(&self, after: EventId, upto: EventId, limit: usize)
    -> Result<Vec<RunEvent>>;
    /// Record an event of the queue itself, on no task, goal or run.
    fn record_event(&self, kind: EventKind, payload: serde_json::Value) -> Result<EventId>;
}

impl<T: EventStore + ?Sized> MarkLog for T {
    fn newest_event_id(&self) -> Result<EventId> {
        self.latest_event_id()
    }

    fn events_of_kinds(
        &self,
        kinds: &[&str],
        after: EventId,
        upto: EventId,
        limit: usize,
    ) -> Result<Vec<RunEvent>> {
        self.events_of_between(kinds, after, upto, limit)
    }

    fn events_in_range(
        &self,
        after: EventId,
        upto: EventId,
        limit: usize,
    ) -> Result<Vec<RunEvent>> {
        self.events_between(after, upto, &crate::domain::EventFilter::default(), limit)
    }

    fn record_event(&self, kind: EventKind, payload: serde_json::Value) -> Result<EventId> {
        self.record_queue_event(kind, payload)
    }
}

/// The ledger (ADR-t1662-1 decisions 10 to 13, ADR-t1662-2): one row per
/// measured object of each [`LedgerKind`], a view folded from the
/// [`EventStore`] by [`crate::domain::ledger`]. Readers go through this
/// port and [`crate::domain::ledger::read::read`] (one row) or
/// [`Self::ledger_window`] (a window of every kind) only, and no decision
/// of the runtime (claim, slot, resume, landing) reads it.
pub trait LedgerStore {
    /// Write `row` in place of the row of the same kind and key, unless
    /// that one is legacy: its values stay as first written and only its
    /// records and their range follow `row`; `false` then. Writing the
    /// same row again leaves one row.
    fn write_ledger_row(&self, row: &LedgerRow) -> Result<bool>;
    /// Write `row` as [`Self::write_ledger_row`] does only while the row of
    /// its kind and key the store holds is still `read` (`None`: none),
    /// the one its fold went on from. `None`, nothing written, when another
    /// writer (another supervisor's builder) wrote it since: the caller
    /// folds it again over the row held now, so a fold over an older read
    /// never takes back what that writer folded (the steps its retention
    /// removed since).
    fn write_ledger_row_over(
        &self,
        row: &LedgerRow,
        read: Option<&LedgerRow>,
    ) -> Result<Option<bool>>;
    /// Write every row of `rows`, each as [`Self::write_ledger_row_over`]
    /// writes it over the row it read (its second, `None`: none), and with
    /// `mark` the store's mark that its session rows record every session
    /// event up to its position
    /// ([`crate::domain::ledger::session::written::SessionsWritten`]), in one
    /// transaction: all of them or none. False, nothing written, when
    /// another writer wrote one of the rows since it was read; a failure
    /// part way writes nothing either. What builds the ledger writes the
    /// session rows a pass changed so, a bounded number at a time, the
    /// mark with the last of them.
    fn write_session_rows_over(
        &self,
        rows: &[(LedgerRow, Option<LedgerRow>)],
        mark: Option<&crate::domain::ledger::session::written::SessionsWritten>,
    ) -> Result<bool>;
    /// The store's mark of where its session rows record every session
    /// event up to, if it holds one: a store whose rows were written one by
    /// one holds none ([`crate::domain::ledger::session::written::written_through`]).
    fn sessions_written(
        &self,
    ) -> Result<Option<crate::domain::ledger::session::written::SessionsWritten>>;
    /// [`Self::write_session_rows_over`] with `marks`: the session rows'
    /// mark and, when given, the mark R that the run, task, queue and node
    /// rows record every input up to
    /// ([`crate::domain::ledger::session::written::RowsWritten`]), all in
    /// the one transaction. A store that keeps no R writes the session
    /// rows' mark alone, and gives no R ([`Self::ledger_marks`]).
    fn write_session_rows_marked(
        &self,
        rows: &[(LedgerRow, Option<LedgerRow>)],
        marks: Option<&crate::domain::ledger::session::written::Marks>,
    ) -> Result<bool> {
        self.write_session_rows_over(rows, marks.map(|marks| &marks.sessions))
    }
    /// Write `marks` alone in one transaction, as
    /// [`Self::write_session_rows_marked`] with no row: what builds the
    /// ledger writes so an R that moved with no session row to write and
    /// the session rows' mark as the store holds it.
    fn write_ledger_marks(
        &self,
        marks: &crate::domain::ledger::session::written::Marks,
    ) -> Result<()> {
        self.write_session_rows_marked(&[], Some(marks)).map(drop)
    }
    /// The store's marks, if it holds the session rows' one: with R when it
    /// holds that too ([`crate::domain::ledger::session::written::rows_written_through`]).
    fn ledger_marks(&self) -> Result<Option<crate::domain::ledger::session::written::Marks>> {
        Ok(self.sessions_written()?.map(|sessions| {
            crate::domain::ledger::session::written::Marks {
                sessions,
                rows: None,
            }
        }))
    }
    /// The row of `kind` and `key`, if there is one.
    fn ledger_row(&self, kind: LedgerKind, key: &str) -> Result<Option<LedgerRow>>;
    /// The rows of `keys` the store holds, each as [`Self::ledger_row`]
    /// reads it, in the order of `keys`; a key of no row gives none. What
    /// builds the ledger reads the rows of a share of its sweep so, in one
    /// read, rather than one read a row.
    fn ledger_rows_of(&self, keys: &[(LedgerKind, String)]) -> Result<Vec<LedgerRow>> {
        let mut rows = Vec::new();
        for (kind, key) in keys {
            rows.extend(self.ledger_row(*kind, key)?);
        }
        Ok(rows)
    }
    /// Every row of `kind`, by key.
    fn ledger_rows(&self, kind: LedgerKind) -> Result<Vec<LedgerRow>>;
    /// At most `limit` rows of `kind` after the key `after` (`None`: from
    /// the first), by key: a page of [`Self::ledger_rows`].
    fn ledger_rows_after(
        &self,
        kind: LedgerKind,
        after: Option<&str>,
        limit: usize,
    ) -> Result<Vec<LedgerRow>> {
        Ok(self
            .ledger_rows(kind)?
            .into_iter()
            .filter(|row| after.is_none_or(|after| row.key.as_str() > after))
            .take(limit)
            .collect())
    }
    /// The head of every row of `kind` ([`crate::domain::ledger::LedgerHead`]):
    /// its kind, key, range of inputs, mark of settled, version and the ids
    /// of its records, not the records, so that what builds the ledger
    /// makes its books again from the rows the store holds
    /// ([`crate::domain::ledger::session::SessionBook::from_heads`]) and
    /// reads no event of a row it does not fold.
    fn ledger_heads(&self, kind: LedgerKind) -> Result<Vec<crate::domain::ledger::LedgerHead>> {
        Ok(self
            .ledger_rows(kind)?
            .iter()
            .map(crate::domain::ledger::LedgerHead::of)
            .collect())
    }
    /// The queue rows' heads with what each says of itself besides
    /// ([`crate::domain::ledger::queue::QueueHead`]): its task, subject,
    /// times, outcome and detail, not its records, so that what builds the
    /// ledger makes the queue book again from the rows the store holds
    /// ([`crate::domain::ledger::queue::QueueBook::from_heads`]).
    fn ledger_queue_heads(&self) -> Result<Vec<crate::domain::ledger::queue::QueueHead>> {
        Ok(self
            .ledger_rows(LedgerKind::Queue)?
            .iter()
            .map(crate::domain::ledger::queue::QueueHead::of)
            .collect())
    }
    /// The heads ([`Self::ledger_queue_heads`]) of the queue rows what is
    /// made again at the position `at` for `since` (that position's,
    /// [`Self::ledger_tasks_open_since_at`]) reads
    /// ([`crate::domain::ledger::queue::QueueHead::forgotten_at`] leaves
    /// out the rest), so that what a new process holds of them at once
    /// does not grow with the rows settled that it forgets anyway.
    fn ledger_queue_heads_at(
        &self,
        at: EventId,
        since: crate::domain::pre_claim::rows::Since,
    ) -> Result<Vec<crate::domain::ledger::queue::QueueHead>> {
        self.ledger_queue_heads_at_after(at, since, None, usize::MAX)
    }
    /// At most `limit` of [`Self::ledger_queue_heads_at`] after the key
    /// `after` (`None`: from the first), by key: a page of them.
    fn ledger_queue_heads_at_after(
        &self,
        at: EventId,
        since: crate::domain::pre_claim::rows::Since,
        after: Option<&str>,
        limit: usize,
    ) -> Result<Vec<crate::domain::ledger::queue::QueueHead>> {
        let mut heads = self.ledger_queue_heads()?;
        heads.retain(|head| !head.forgotten_at(at, since));
        heads.sort_by(|a, b| a.head.key.cmp(&b.head.key));
        Ok(heads
            .into_iter()
            .filter(|head| after.is_none_or(|after| head.head.key.as_str() > after))
            .take(limit)
            .collect())
    }
    /// Of the session rows, the records a session book is made again by,
    /// each with its row's key ([`crate::domain::ledger::session::anchors`]):
    /// each row's first and every `session_closed`, not the rest.
    fn ledger_session_anchors(&self) -> Result<Vec<(String, RunEvent)>> {
        Ok(crate::domain::ledger::session::anchors(
            &self.ledger_rows(LedgerKind::Session)?,
        ))
    }
    /// From where the waits an event ended bear on a task row not settled
    /// ([`crate::domain::pre_claim::rows::Since`]): the earliest start and
    /// first event of those rows.
    fn ledger_tasks_open_since(&self) -> Result<crate::domain::pre_claim::rows::Since> {
        Ok(crate::domain::pre_claim::rows::Since::of(
            &self.ledger_rows(LedgerKind::Task)?,
        ))
    }
    /// A [`crate::domain::pre_claim::rows::Since`] for the position `at`
    /// (at or before the position the rows were written at) no later than
    /// the one the task rows folded up to `at` would give: that of the
    /// task rows whose inputs start by `at` and that are not settled or
    /// were folded from an input past `at`
    /// ([`crate::domain::ledger::InputsThrough::last`]). At the position
    /// the rows were written at it is [`Self::ledger_tasks_open_since`].
    fn ledger_tasks_open_since_at(
        &self,
        at: EventId,
    ) -> Result<crate::domain::pre_claim::rows::Since> {
        let rows: Vec<LedgerRow> = self
            .ledger_rows(LedgerKind::Task)?
            .into_iter()
            .filter(|row| row.inputs_from <= at)
            .map(|row| LedgerRow {
                is_final: row.is_final && row.inputs_through.last() <= at,
                ..row
            })
            .collect();
        Ok(crate::domain::pre_claim::rows::Since::of(&rows))
    }
    /// The queue rows the supervisors' lives and the waits before a claim
    /// are made again from ([`crate::domain::pre_claim::rows::holds`]: the
    /// waits open, those a supervisor's life ended or that ended from
    /// `since` on, the streams not replaced), without their records.
    fn ledger_wait_rows(
        &self,
        since: crate::domain::pre_claim::rows::Since,
    ) -> Result<Vec<LedgerRow>> {
        Ok(self
            .ledger_rows(LedgerKind::Queue)?
            .into_iter()
            .filter(|row| crate::domain::pre_claim::rows::holds(row, since))
            .map(|row| LedgerRow {
                records: Vec::new(),
                ..row
            })
            .collect())
    }
    /// The queue rows of the waits before a claim settled that a book made
    /// again from the rows held from `held` lacks and that bear on the
    /// time of a task from `from` up to `until_ms`
    /// ([`crate::domain::pre_claim::rows::lacks`]), without their records:
    /// what the fold of a task that starts before `held` reads besides the
    /// book, none of the waits settled over no time of it.
    fn ledger_settled_waits(
        &self,
        held: crate::domain::pre_claim::rows::Since,
        from: crate::domain::pre_claim::rows::Since,
        until_ms: i64,
    ) -> Result<Vec<LedgerRow>> {
        Ok(self
            .ledger_rows(LedgerKind::Queue)?
            .into_iter()
            .filter(|row| crate::domain::pre_claim::rows::lacks(row, held, from, until_ms))
            .map(|row| LedgerRow {
                records: Vec::new(),
                ..row
            })
            .collect())
    }
    /// Every `supervisor_stopped` the queue rows record, by id: the stops
    /// the supervisors' lives are made again by, not the other records.
    fn ledger_supervisor_stops(&self) -> Result<Vec<RunEvent>> {
        let mut stops: Vec<RunEvent> = self
            .ledger_rows(LedgerKind::Queue)?
            .into_iter()
            .flat_map(|row| row.records)
            .filter(|event| event.kind == EventKind::SupervisorStopped)
            .collect();
        stops.sort_by_key(|event| event.id);
        Ok(stops)
    }
    /// At most `limit` of the rows [`Self::ledger_wait_rows`] gives for
    /// `since` after the key `after` (`None`: from the first), by key: a
    /// page of them.
    fn ledger_wait_rows_after(
        &self,
        since: crate::domain::pre_claim::rows::Since,
        after: Option<&str>,
        limit: usize,
    ) -> Result<Vec<LedgerRow>> {
        Ok(self
            .ledger_wait_rows(since)?
            .into_iter()
            .filter(|row| after.is_none_or(|after| row.key.as_str() > after))
            .take(limit)
            .collect())
    }
    /// At most `limit` of the stops [`Self::ledger_supervisor_stops`]
    /// gives, each with where the queue rows record it (its row's key and
    /// its index among the row's records), by that place after the place
    /// `after` (`None`: from the first): a page of them, not yet in the
    /// order of their ids.
    fn ledger_supervisor_stops_after(
        &self,
        after: Option<(&str, i64)>,
        limit: usize,
    ) -> Result<Vec<((String, i64), RunEvent)>> {
        let mut rows = self.ledger_rows(LedgerKind::Queue)?;
        rows.sort_by(|a, b| a.key.cmp(&b.key));
        Ok(rows
            .into_iter()
            .flat_map(|row| {
                let key = row.key;
                row.records
                    .into_iter()
                    .zip(0_i64..)
                    .map(move |(event, index)| ((key.clone(), index), event))
            })
            .filter(|(_, event)| event.kind == EventKind::SupervisorStopped)
            .filter(|((key, index), _)| {
                after.is_none_or(|(after, at)| (key.as_str(), *index) > (after, at))
            })
            .take(limit)
            .collect())
    }
    /// Of the run rows, the records the summaries of the pushes are made
    /// again from ([`crate::domain::ledger::builder::pushes::PushesHeld::at`]),
    /// each cut to what they read of it
    /// ([`crate::domain::ledger::builder::pushes::cut`]), ascending id:
    /// every `push_finished`, and of the rows that record a `push_failed`
    /// (no other waits on a push) their `push_failed` and
    /// `run_phase_changed`.
    fn ledger_push_records(&self) -> Result<Vec<RunEvent>> {
        use crate::domain::ledger::builder::pushes::page::records_of_row;
        let mut records: Vec<RunEvent> = self
            .ledger_rows(LedgerKind::Run)?
            .iter()
            .flat_map(records_of_row)
            .collect();
        records.sort_by_key(|event| event.id);
        Ok(records)
    }
    /// A page of [`Self::ledger_push_records`] by the run rows' keys
    /// ([`crate::domain::ledger::builder::pushes::page::PushRecordsRead`]):
    /// the push records of the rows `read` names alone, the rows read with
    /// [`Self::ledger_rows_after`] and [`Self::ledger_row`], no other row.
    /// Gathered by
    /// [`crate::domain::ledger::builder::pushes::page::gather`], the pages
    /// are the records [`Self::ledger_push_records`] gives.
    fn ledger_push_records_page(
        &self,
        read: &crate::domain::ledger::builder::pushes::page::PushRecordsRead,
    ) -> Result<crate::domain::ledger::builder::pushes::page::PushRecordsPage> {
        use crate::domain::ledger::builder::pushes::page::{
            PushRecordsPage, PushRecordsRead, records_of_row,
        };
        Ok(match read {
            PushRecordsRead::Rows { after, rows } => PushRecordsPage::of_rows(
                &self.ledger_rows_after(LedgerKind::Run, after.as_deref(), *rows)?,
            ),
            PushRecordsRead::FirstOfRow { after, records } => {
                match self
                    .ledger_rows_after(LedgerKind::Run, after.as_deref(), 1)?
                    .first()
                {
                    Some(row) => {
                        PushRecordsPage::in_row(row.key.clone(), 1, records_of_row(row), *records)
                    }
                    None => PushRecordsPage::default(),
                }
            }
            PushRecordsRead::RestOfRow {
                key,
                after,
                records,
            } => {
                let row = self.ledger_row(LedgerKind::Run, key)?;
                let rest = row
                    .iter()
                    .flat_map(records_of_row)
                    .filter(|record| record.id > *after)
                    .collect();
                PushRecordsPage::in_row(key.clone(), usize::from(row.is_some()), rest, *records)
            }
        })
    }
    /// The heads of the run and task rows not settled at `through` and the
    /// cursor `resources` of the runs' windows measured from outside
    /// ([`crate::domain::ledger::builder::pushes::settled_at`], a legacy
    /// row whatever its mark): those whose folds read the pushes kept at
    /// that position on.
    fn ledger_heads_open_at(
        &self,
        through: EventId,
        resources: i64,
    ) -> Result<Vec<crate::domain::ledger::LedgerHead>> {
        use crate::domain::ledger::LedgerHead;
        use crate::domain::ledger::builder::pushes::settled_at;
        let mut rows = self.ledger_rows(LedgerKind::Run)?;
        rows.extend(self.ledger_rows(LedgerKind::Task)?);
        Ok(rows
            .iter()
            .filter(|row| !settled_at(&LedgerHead::of(row), row.is_legacy(), through, resources))
            .map(LedgerHead::of)
            .collect())
    }
    /// Whether the store holds a row [`Self::ledger_older_rows_after`] gives.
    /// No row is read.
    fn ledger_older_rows(&self) -> Result<bool> {
        Ok(!self.ledger_older_rows_after(None, 1)?.is_empty())
    }
    /// At most `limit` of the rows the store holds folded by an older
    /// [`crate::domain::ledger::LEDGER_V`], a legacy one aside (the store
    /// keeps the version a legacy row was first written at, and no fold
    /// moves it), after `after`, by the text of their kind and their key:
    /// what a builder gone on from R folds again by target
    /// ([`crate::domain::ledger::builder::Builder::older_rows_to_read`]).
    /// Their kinds and keys only: no row of this version is read.
    fn ledger_older_rows_after(
        &self,
        after: Option<&(LedgerKind, String)>,
        limit: usize,
    ) -> Result<Vec<(LedgerKind, String)>> {
        let mut older = Vec::new();
        for kind in LedgerKind::ALL {
            older.extend(
                self.ledger_rows(kind)?
                    .iter()
                    .filter(|row| {
                        !row.is_legacy() && row.ledger_v < crate::domain::ledger::LEDGER_V
                    })
                    .map(|row| (kind, row.key.clone())),
            );
        }
        let place = |(kind, key): &(LedgerKind, String)| (kind.as_str(), key.clone());
        older.sort_by_key(place);
        let after = after.map(place);
        Ok(older
            .into_iter()
            .filter(|row| after.as_ref().is_none_or(|after| place(row) > *after))
            .take(limit)
            .collect())
    }
    /// The keys of the run rows whose steps were folded and not classified
    /// by the rules of `version`
    /// ([`crate::domain::ledger::builder::Builder::mark_rules`]): those
    /// rules alone fold again. No row is read, as many keys however many
    /// runs those rules classified.
    fn ledger_runs_classified_otherwise(&self, version: &str) -> Result<Vec<String>> {
        use crate::domain::ledger::steps;
        Ok(self
            .ledger_rows(LedgerKind::Run)?
            .into_iter()
            .filter(|row| {
                row.detail.get(steps::DETAIL_KEY).is_some()
                    && steps::rules_of(&row.detail) != Some(version)
            })
            .map(|row| row.key)
            .collect())
    }
    /// The runs a task row names
    /// ([`crate::domain::ledger::builder::pushes::task_runs`], a legacy
    /// row's too) that have no run row of their own, each once, ascending:
    /// those on their way, as the builder writes a run's row only once it
    /// ended or waits on its push. No record is read.
    fn ledger_runs_without_row(&self) -> Result<Vec<RunId>> {
        let written: std::collections::BTreeSet<String> = self
            .ledger_heads(LedgerKind::Run)?
            .into_iter()
            .map(|head| head.key)
            .collect();
        let named: std::collections::BTreeSet<RunId> = self
            .ledger_rows(LedgerKind::Task)?
            .iter()
            .flat_map(crate::domain::ledger::builder::pushes::task_runs)
            .filter(|run| !written.contains(run))
            .filter_map(|run| RunId::new(&run).ok())
            .collect();
        Ok(named.into_iter().collect())
    }
    /// The runs a `run_claimed` of the task of a legacy task row names that
    /// have no run row of their own, each once, ascending: a legacy row
    /// keeps the runs it named as first written, so that a run claimed
    /// since is named by no row ([`Self::ledger_runs_without_row`]). Of the
    /// events it reads the claims of those tasks only.
    fn ledger_legacy_runs_without_row(&self) -> Result<Vec<RunId>>;
    /// The runs a `run_claimed` of `task` names, each once, ascending: those
    /// a fold of that task reads, which a legacy row of it, keeping the runs
    /// it named as first written, may not name. Of the events it reads the
    /// claims of that task only.
    fn ledger_runs_claimed(&self, task: crate::domain::TaskId) -> Result<Vec<RunId>>;
    /// The runs with a row of their own and an event up to `through` after
    /// the inputs of that row, each once, ascending: those that went on
    /// their way since their row was written, as the builder writes no row
    /// of a run on its way, or whose row is behind. It reads whether such
    /// an event is there, not the event.
    fn ledger_runs_past_their_rows(&self, through: EventId) -> Result<Vec<RunId>>;
    /// The pushes of no run the store's rows record, of the task rows and
    /// of the queue rows of a push alone (their subject a `push_finished`,
    /// [`crate::domain::ledger::owner::owner`]) only, each cut to what the
    /// summaries of the pushes read of it
    /// ([`crate::domain::ledger::builder::pushes::cut`]), ascending id. Of
    /// the queue rows it reads the heads ([`Self::ledger_queue_heads`]) and
    /// by key only those of a push alone, no other queue row.
    fn ledger_runless_pushes(&self) -> Result<Vec<RunEvent>> {
        use crate::domain::ledger::builder::pushes::cut;
        let mut rows = self.ledger_rows(LedgerKind::Task)?;
        for head in self.ledger_queue_heads()? {
            if head.subject_kind == EventKind::PushFinished.as_str() {
                rows.extend(self.ledger_row(LedgerKind::Queue, &head.head.key)?);
            }
        }
        let mut pushes: Vec<RunEvent> = rows
            .into_iter()
            .flat_map(|row| row.records)
            .filter(|event| event.kind == EventKind::PushFinished && event.run_id.is_none())
            .map(|event| cut(&event))
            .collect();
        pushes.sort_by_key(|event| event.id);
        pushes.dedup_by_key(|event| event.id);
        Ok(pushes)
    }
    /// The retention of the node rows, by the same local day as
    /// [`NodeSampleStore::remove_node_samples`]: drop from each the samples
    /// of a day before `before_day` and remove a row left with none
    /// ([`crate::domain::ledger::node::retained`]). Only the rows it
    /// changes are written, a bounded number a transaction: with none to
    /// change nothing is written, and one stopped part way is finished by
    /// the next call. Returns how many rows were removed.
    fn remove_node_rows(&self, before_day: i64) -> Result<usize>;
    /// The events of the run `run` the EventStore holds, in id order: the
    /// input [`Self::ledger_window`] folds the row of a run on its way
    /// from, as the builder writes none of it until it ends.
    fn ledger_run_events(&self, run: &RunId) -> Result<Vec<RunEvent>>;
    /// The last event up to `upto` (inclusive) whose payload names
    /// `supervisor` ([`crate::domain::supervisor_life::supervisor_of`]),
    /// whatever its kind; `None` with none. What builds the ledger cuts a
    /// row a supervisor's life reaches back to a position by it, one read
    /// a supervisor, however long that supervisor was silent since.
    fn last_event_naming_supervisor(
        &self,
        supervisor: &str,
        upto: EventId,
    ) -> Result<Option<RunEvent>>;
    /// About how many events the rows hold, for a read to make room for
    /// their records at once; a guess that is only a size (none: 0).
    fn ledger_events_expected(&self) -> Result<usize> {
        Ok(0)
    }
    /// [`Self::ledger_rows_after`] of the rows `from` does not leave out
    /// ([`RowsFrom::leaves_out`]): the condition is the store's, so what it
    /// leaves out is not read. A store without one gives them all.
    fn ledger_rows_from(
        &self,
        kind: LedgerKind,
        after: Option<&str>,
        limit: usize,
        from: &crate::domain::ledger::window::RowsFrom,
    ) -> Result<Vec<LedgerRow>> {
        let _ = from;
        self.ledger_rows_after(kind, after, limit)
    }
    /// What the rows `from` leaves out still give
    /// ([`crate::domain::ledger::window::EarlierRecords`]); a store that
    /// leaves none out has none.
    fn ledger_earlier(
        &self,
        from: &crate::domain::ledger::window::RowsFrom,
    ) -> Result<crate::domain::ledger::window::EarlierRecords> {
        let _ = from;
        Ok(crate::domain::ledger::window::EarlierRecords::default())
    }
    /// Make the reads of `read` of one state of the rows, whatever is written
    /// meanwhile. A store whose [`Self::ledger_rows_from`] leaves nothing out
    /// has nothing to hold together.
    /// The callback reads through the supplied port, which shares the open
    /// snapshot; calling the store's ordinary entries would begin nested reads.
    fn ledger_read_snapshot(
        &self,
        read: &mut dyn FnMut(&dyn LedgerWindowRead) -> Result<()>,
    ) -> Result<()> {
        read(&self)
    }
    /// The window `query` asks of the rows of every kind: the one entry
    /// the statistics read the ledger by. The rows are read by pages and
    /// folded one by one to what the window keeps of them
    /// ([`crate::domain::ledger::window::WindowRows`]), so that a read holds
    /// no row's records twice, no queue row and no run row's steps.
    /// [`WindowQuery::read_from_ms`] leaves rows out of that read
    /// (`domain::ledger::window`'s "A read from a time"); `since` does not.
    /// The rows of the runs on their way (of a task not ended, with no row
    /// of their own) are folded at `query`'s now from their events by the
    /// one fold ([`crate::domain::ledger::window::run_on_its_way`]) and
    /// taken the same way, so that a window holds what a run not ended
    /// recorded too.
    fn ledger_window(&self, query: &WindowQuery) -> Result<LedgerWindow> {
        use crate::domain::ledger::window::WindowRows;
        if query.read_from_ms.is_none() {
            let mut read = WindowRows::new(query, self.ledger_events_expected()?);
            for kind in LedgerKind::ALL {
                read.take_pages(|after, limit| self.ledger_rows_after(kind, after, limit))?;
            }
            return ledger_window_with_runs_on_their_way(&self, read, query);
        }
        // Which rows a read leaves out and what they still give must be of
        // one state of the rows, or a row moved between them is in neither.
        let mut window = None;
        self.ledger_read_snapshot(&mut |snapshot| {
            // A read from a time has no use for room for every event.
            let mut read = WindowRows::new(query, 0);
            // The queue's rows first: they tell which of the others to leave out.
            read.take_pages(|after, limit| snapshot.rows_after(LedgerKind::Queue, after, limit))?;
            let others = LedgerKind::ALL
                .into_iter()
                .filter(|kind| *kind != LedgerKind::Queue);
            if let Some(from) = read.rows_from() {
                for kind in others {
                    read.take_pages(|after, limit| snapshot.rows_from(kind, after, limit, &from))?;
                }
                read.take_earlier(snapshot.earlier(&from)?);
                loop {
                    let wanted = read.rows_wanted();
                    if wanted.is_empty() {
                        break;
                    }
                    for (kind, key) in wanted {
                        read.take_row(snapshot.row(kind, &key)?);
                    }
                }
            }
            window = Some(ledger_window_with_runs_on_their_way(snapshot, read, query)?);
            Ok(())
        })?;
        window.ok_or_else(|| anyhow::anyhow!("the ledger's snapshot read nothing"))
    }
}

/// The reads needed to fold a ledger window, borrowed from a store or its
/// open snapshot. It exposes domain values, so snapshot callbacks share the
/// caller's transaction without knowing the SQL execution port.
pub trait LedgerWindowRead {
    fn rows_after(
        &self,
        kind: LedgerKind,
        after: Option<&str>,
        limit: usize,
    ) -> Result<Vec<LedgerRow>>;
    fn rows_from(
        &self,
        kind: LedgerKind,
        after: Option<&str>,
        limit: usize,
        from: &crate::domain::ledger::window::RowsFrom,
    ) -> Result<Vec<LedgerRow>>;
    fn earlier(
        &self,
        from: &crate::domain::ledger::window::RowsFrom,
    ) -> Result<crate::domain::ledger::window::EarlierRecords>;
    fn row(&self, kind: LedgerKind, key: &str) -> Result<Option<LedgerRow>>;
    fn run_events(&self, run: &RunId) -> Result<Vec<RunEvent>>;
}

impl<T: LedgerStore + ?Sized> LedgerWindowRead for &T {
    fn rows_after(
        &self,
        kind: LedgerKind,
        after: Option<&str>,
        limit: usize,
    ) -> Result<Vec<LedgerRow>> {
        self.ledger_rows_after(kind, after, limit)
    }
    fn rows_from(
        &self,
        kind: LedgerKind,
        after: Option<&str>,
        limit: usize,
        from: &crate::domain::ledger::window::RowsFrom,
    ) -> Result<Vec<LedgerRow>> {
        self.ledger_rows_from(kind, after, limit, from)
    }
    fn earlier(
        &self,
        from: &crate::domain::ledger::window::RowsFrom,
    ) -> Result<crate::domain::ledger::window::EarlierRecords> {
        self.ledger_earlier(from)
    }
    fn row(&self, kind: LedgerKind, key: &str) -> Result<Option<LedgerRow>> {
        self.ledger_row(kind, key)
    }
    fn run_events(&self, run: &RunId) -> Result<Vec<RunEvent>> {
        self.ledger_run_events(run)
    }
}

/// The window of the rows `read` took, with the rows of the runs on their
/// way folded at `query`'s now.
fn ledger_window_with_runs_on_their_way(
    store: &(impl LedgerWindowRead + ?Sized),
    mut read: crate::domain::ledger::window::WindowRows,
    query: &WindowQuery,
) -> Result<LedgerWindow> {
    for run in read.runs_on_their_way() {
        let events = store.run_events(&run)?;
        if let Some(row) =
            crate::domain::ledger::window::run_on_its_way(&run, &events, query.now_ms)
        {
            read.take(row);
        }
    }
    Ok(read.into_window(query))
}

/// The NodeSampleStore (ADR-t1662-2 decisions 1 and 5): the permanent
/// record of each node's load, one row per node and unix second, the
/// input the measurement reads in place of the host's CSV files. Only
/// host運用's record of the host's load writes it
/// ([`crate::domain::host_metrics`]); the measurement reads it.
pub trait NodeSampleStore {
    /// Record `sample` of `node` on its local day `day` from `source`, with
    /// the same sample in the common units in the same row, unless a row of
    /// `node` is at its second, or on the same `day` less than `apart_secs`
    /// from it ([`crate::domain::host_metrics::apart_secs`]: another
    /// supervisor of the queue sampled); whether it was written.
    /// `resources` makes the common units from the second of the node's
    /// row before the sample (`None` for its first), read in the same
    /// write, so the time the sample stands for is the time since that row
    /// ([`crate::domain::environment::NodeRate::over`]); a sample
    /// written after a later row of `node` gets its own second, so it
    /// stands for no time the later row counted.
    fn record_node_sample(
        &self,
        node: &str,
        day: i64,
        sample: &HostSample,
        resources: &crate::domain::environment::NodeRate,
        source: SampleSource,
        apart_secs: i64,
    ) -> Result<bool>;
    /// Add the samples of `node` read from the CSV `file` of the local day
    /// `day` (`csv_import`) whose second has no row of `node` yet, however
    /// near another row they are (a supervisor kept them), and mark the
    /// file imported up to the `bytes` read. The rows are committed a
    /// bounded number a transaction and the mark with the last of them, so
    /// an import stopped part way leaves the file's mark as it was (none,
    /// or that of a shorter read before the file grew) and the next import
    /// of it adds the rest, none twice. Returns how many rows were
    /// added.
    fn import_node_samples(
        &self,
        node: &str,
        file: &str,
        day: i64,
        samples: &[HostSample],
        bytes: u64,
    ) -> Result<usize> {
        let mark = ImportMark {
            bytes,
            rows: samples.len(),
        };
        self.import_node_sample_part(node, file, day, samples, Some(mark))
    }
    /// [`Self::import_node_samples`] of a part of the samples of `file`,
    /// marking the file with `mark` (the bytes read and the file's rows)
    /// only when given: an import sent in parts gives it with its last, so
    /// one stopped part way leaves the file's mark as it was, and the next
    /// import reads the file again whole and adds the rest, none twice.
    fn import_node_sample_part(
        &self,
        node: &str,
        file: &str,
        day: i64,
        samples: &[HostSample],
        mark: Option<ImportMark>,
    ) -> Result<usize>;
    /// The CSV files of `node` marked imported, each with the bytes read.
    fn imported_files(&self, node: &str) -> Result<std::collections::BTreeMap<String, u64>>;
    /// Remove the rows and the marks of the files of a local day before
    /// `before_day`: the retention, by the day as the files go. Returns how
    /// many rows were removed.
    fn remove_node_samples(&self, before_day: i64) -> Result<usize>;
    /// The rows of `node` taken in `[from, until]` (unix seconds), in the
    /// order they were taken.
    fn node_samples(&self, node: &str, from: i64, until: i64) -> Result<Vec<NodeSample>>;
    /// Every node's rows that came in after the row `after` (their id),
    /// in the order they came in: the cursor of what builds the ledger.
    fn node_samples_after(&self, after: i64) -> Result<Vec<NodeSample>>;
}

/// The SessionStepStore (ADR-t1662-2 decisions 1 and 4, ADR-t1662-3
/// decision 12): the steps of the sessions, one row per
/// `(session_id, seq)`, each with the one target its session is of. A
/// run's steps are for its ledger row; a target of no run's are kept here
/// only, read by their target. A step is kept
/// [`crate::domain::session_step::RETENTION_DAYS`] after it came in.
pub trait SessionStepStore {
    /// Record `steps` of `session_id` of `target` as received at
    /// `received_at` (unix ms), in one transaction: a step whose
    /// `(session_id, seq)` is held already, or was removed by the
    /// retention from a session that keeps a step, adds nothing, whatever
    /// it says now; steps that do not hold together
    /// ([`crate::domain::session_step::check`]) and a session whose steps
    /// came in under another target refuse the whole call, recording none
    /// ([`crate::domain::session_step::StepError`]).
    fn record_steps(
        &self,
        target: &StepTarget,
        session_id: &str,
        steps: &[Step],
        received_at: i64,
    ) -> Result<StepsRecorded>;
    /// The gaps of the seqs of `session_id`
    /// ([`crate::domain::session_step::gaps`]): a seq the retention
    /// removed is no gap, a seq that never came in is one.
    fn session_gaps(&self, session_id: &str) -> Result<Vec<SeqRange>>;
    /// At most `limit` steps that came in after the step `after` (their
    /// id), in the order they came in: the cursor of what builds the
    /// ledger.
    fn steps_after(&self, after: i64, limit: usize) -> Result<Vec<SessionStep>>;
    /// The steps of `target`, in the order they came in.
    fn target_steps(&self, target: &StepTarget) -> Result<Vec<SessionStep>>;
    /// Remove every step that came in before `before` (unix ms); a session
    /// keeping other steps keeps the seqs removed, so they open no gap and
    /// are not added again. The resource samples that came in before it go
    /// too, and the owners registered before it whose session keeps no
    /// step and no sample ([`SessionOwners`]). With none to remove nothing
    /// is written. Returns how many steps were removed.
    fn remove_steps(&self, before: i64) -> Result<usize>;
}

/// The resource samples of the sessions (ADR-t1662-3 decisions 1 and 4):
/// what an executor measured from inside where the supervisor cannot, one
/// row per `(session_id, seq)`, each with its session's target, kept and
/// removed with the steps ([`SessionStepStore::remove_steps`]).
pub trait SessionResourceStore {
    /// Record `samples` of `session_id` of `target` as received at
    /// `received_at` (unix ms), in one transaction: a sample whose
    /// `(session_id, seq)` is held already adds nothing; samples that do
    /// not hold together ([`crate::domain::session_step::check_resources`])
    /// and a session whose steps or samples came in under another target
    /// refuse the whole call, recording none.
    fn record_resources(
        &self,
        target: &StepTarget,
        session_id: &str,
        samples: &[ResourceSample],
        received_at: i64,
    ) -> Result<StepsRecorded>;
    /// The samples of `target`, in the order they came in.
    fn target_resources(&self, target: &StepTarget) -> Result<Vec<SessionResource>>;
    /// At most `limit` samples that came in after the sample `after` (their
    /// id), in the order they came in: the cursor of what builds the
    /// ledger.
    fn resources_after(&self, after: i64, limit: usize) -> Result<Vec<SessionResource>>;
}

/// The resources of the runs the supervisor measured from outside
/// (ADR-t1662-1 decision 8 and (ii), ADR-t1662-3 decisions 1 and 4,
/// docs/design/measurement-environment.md "runの資源"): one row per window, in the
/// order they were recorded, each under the `environment_attached` of the
/// run it was measured in. A run's ledger row folds them into its
/// intervals.
pub trait RunResourceStore {
    /// Record `row` (its `id` is the store's to give); its id.
    fn record_run_resource(&self, row: &RunResource) -> Result<i64>;
    /// The windows of `run`, in the order they were recorded.
    fn run_resources(&self, run: &RunId) -> Result<Vec<RunResource>>;
    /// At most `limit` windows recorded after the window `after` (their id),
    /// in that order: the cursor of what builds the ledger.
    fn run_resources_after(&self, after: i64, limit: usize) -> Result<Vec<RunResource>>;
    /// The runs whose last `environment_attached` has no `last` window yet,
    /// each with that event's id.
    fn open_attachments(&self) -> Result<Vec<(RunId, i64)>>;
}

/// The environment the runs execute in (ADR-t1662-3 decisions 3 to 6,
/// docs/design/measurement-environment.md "環境の口"): it names itself as a run
/// is attached to it, and measures from outside, in the common units, the
/// runs' resources and the node's. The adapter now is the host's
/// (`infrastructure::environment::HostEnvironment`); a container's or a
/// cluster's goes in its place once it passes the same tests. An
/// environment that cannot measure a run from outside says so
/// ([`Self::measures_from_outside`]), and the run's executor measures it
/// from inside and sends it through the [`TelemetrySink`] instead.
pub trait ExecutionEnvironment: Send + Sync {
    /// What it is, as a run is attached to it (`environment_attached`).
    fn attached(&self) -> EnvironmentAttached;
    /// Whether it measures a run's resources from outside.
    fn measures_from_outside(&self) -> bool;
    /// The waits it adds to a run (none on the host), each a phase of
    /// `blocker: infra` ([`crate::domain::run_phase::Phase::Environment`]).
    fn waits(&self) -> &'static [EnvironmentWait];
    /// Measure from outside what each run of `runs` (its id, the pids it
    /// registered and its window) spent in its window: one entry per run
    /// it measured, none for a run it cannot.
    fn measure_runs(&self, runs: &[RunProcesses]) -> Result<Vec<(RunId, Resources)>>;
    /// Forget what it kept of `run` between its measures.
    fn forget_run(&self, run: &RunId);
    /// The node's resources over `interval` from one `sample` of the
    /// host's load pass, in the common units: the time spent (the CPU and
    /// the throttled seconds) in proportion to `interval` and the rest not
    /// depending on it, which the store's measure of one second relies on
    /// ([`crate::domain::environment::NodeRate`]).
    fn node_resources(&self, sample: &HostSample, interval: std::time::Duration) -> Resources;
}

/// What the supervisor's measure of the runs' resources reads and writes:
/// the runs' events (their attachment) and leases (who holds them), the
/// supervisors (whether that one lives), and the windows it records.
pub trait ResourceQueue:
    EventStore + RunResourceStore + CoordinationReads + SupervisorReads
{
}

impl<T: EventStore + RunResourceStore + CoordinationReads + SupervisorReads + ?Sized> ResourceQueue
    for T
{
}

/// The owners of the sessions (ADR-t1662-3 decision 10): the control side
/// registers the target a session is of before its executor reports, and
/// the queue service records a report only for the telemetry principal of
/// the session's owner, so no token takes another target's session first.
/// Only the control side registers; no use case of the service does.
pub trait SessionOwners {
    /// Register `session_id` as `target`'s at `registered_at` (unix ms).
    /// Registering it again to the same target changes nothing; a session
    /// registered to another target, or whose steps or samples came in
    /// under another, is refused
    /// ([`crate::domain::session_step::StepError::SessionOfAnotherTarget`]).
    fn register_session(
        &self,
        target: &StepTarget,
        session_id: &str,
        registered_at: i64,
    ) -> Result<()>;
    /// The owner `session_id` was registered to; `None` for one never
    /// registered (or removed by the retention).
    fn session_owner(&self, session_id: &str) -> Result<Option<SessionOwner>>;
}

/// Where an executor sends what its sessions did (ADR-t1662-3 decisions 1
/// and 2, docs/design/measurement.md "送る口"): a report of one session's
/// steps, and of the resources it measured from inside where the
/// supervisor cannot, of the one target its telemetry token was issued
/// for, every
/// [`crate::domain::telemetry::REPORT_INTERVAL_SECS`]. The adapter now is
/// the queue service's `telemetry_report` through the client mode, on the
/// host too; the agent the executor runs sends nothing. A report that does
/// not arrive changes no run's, job's or session's outcome: the executor
/// sends its steps and samples again, and the stores keep one of each
/// `(session_id, seq)`.
pub trait TelemetrySink {
    /// Send `report`: what the SessionStepStore added of its steps and
    /// samples and held already, or why it was refused. A report past its
    /// bounds ([`crate::domain::telemetry::check`]) is refused before it
    /// is sent.
    fn report(&self, report: &TelemetryReport) -> Result<ReportRecorded>;
}

/// Where an executor finds the [`TelemetrySink`] of its target (ADR-t1662-3
/// decision 11): the sink of the queue at `db` with the telemetry token the
/// control side issued for `target`, read from its file each time it is
/// asked, so a token issued again is the one sent with. `None` while the
/// target holds no token: the executor sends nothing then and keeps its
/// steps. The executor reads the token's file itself; it is never in an
/// agent's environment or command line.
pub trait TelemetrySinks: Send + Sync {
    fn sink(&self, db: &std::path::Path, target: &StepTarget) -> Option<Box<dyn TelemetrySink>>;

    /// The secret key of the queue at `db` the executor digests its
    /// commands' words with (ADR-t1673-1 decision 10), made once by the
    /// first that asks; `None` when it can be neither read nor made, and
    /// the executor's steps then keep no command's shape. The executor
    /// reads it itself: it is never sent, recorded or given to an agent.
    fn digest_key(&self, db: &std::path::Path) -> Option<crate::domain::command_shape::DigestKey>;

    /// The file of the queue at `db` a session wrapper keeps `target`'s
    /// step numbering and unsent steps in, its directory made: beside the
    /// tokens' files, under
    /// [`STEPS_DIR`](crate::domain::queue_service::STEPS_DIR), never in
    /// a worktree, a run's, a planner's or a job's directory, which an
    /// agent can write. The wrapper reads and writes it itself; its path is
    /// never in an agent's environment or command line. `None` keeps the
    /// steps in the wrapper alone.
    fn steps_file(&self, db: &std::path::Path, target: &StepTarget) -> Option<std::path::PathBuf>;
}

/// What builds the ledger reads and writes (docs/design/measurement.md
/// "台帳を作る係"): the EventStore, the NodeSampleStore, the
/// SessionStepStore (with the owners, whose registration places a
/// session's steps and samples on the supervisor's clock, and the samples
/// an executor measured) and the runs' resources, its inputs, and the
/// LedgerStore. Every store of the seven is one.
pub trait LedgerQueue:
    EventStore
    + LedgerStore
    + NodeSampleStore
    + SessionStepStore
    + SessionOwners
    + SessionResourceStore
    + RunResourceStore
{
}

impl<
    T: EventStore
        + LedgerStore
        + NodeSampleStore
        + SessionStepStore
        + SessionOwners
        + SessionResourceStore
        + RunResourceStore
        + ?Sized,
> LedgerQueue for T
{
}

/// What reports read and record of the queue as a whole: the findings,
/// the written reports, KPI breaches, forecasts and the CI watch.
///
/// Owned by the observation and analysis context: its reads are open to
/// every context and its `record_*` writes are its own
/// (src/context_boundaries/ownership.rs).
pub trait QueueRecords {
    /// The findings `query` lists, larger impact first (`findings`), for
    /// the KPI report's open findings.
    fn findings(&self, query: &crate::domain::FindingQuery) -> Result<Vec<FindingView>>;
    /// The reports recorded as written (`report_written`, ADR-0051
    /// decision 20): each (period, label).
    fn reports_written(&self) -> Result<std::collections::HashSet<(String, String)>>;
    /// Record `report_written` with `payload` (its `period` and `label`)
    /// unless the same report is recorded; `false` when it is.
    fn record_report_written(&self, payload: serde_json::Value) -> Result<bool>;
    /// The KPI breaches started and not resolved (ADR-0051 decision 18),
    /// each its `kpi_breach_started` payload.
    fn kpi_breaches_open(&self) -> Result<Vec<serde_json::Value>>;
    /// Record a breach's start or end unless it already stands so; a
    /// start is marked `pushed` while `push_day` (the local day, the
    /// day's limit) allows. Returns the payload recorded.
    fn record_kpi_breach(
        &self,
        kind: EventKind,
        payload: serde_json::Value,
        push_day: Option<(i64, usize)>,
    ) -> Result<Option<serde_json::Value>>;
    /// Record `kpi_push_abandoned` unless one stands since the latest
    /// push that succeeded (ADR-0051 decision 23); `false` when it does.
    fn record_kpi_push_abandoned(&self, payload: serde_json::Value) -> Result<bool>;
    /// Record a forecast snapshot (`forecast_recorded`, ADR-0070 decision
    /// 3) unless another was recorded after `previous`; its ID, or `None`.
    fn record_forecast(
        &self,
        payload: serde_json::Value,
        previous: Option<EventId>,
    ) -> Result<Option<EventId>>;
    /// Every event of the CI watch (ADR-t1920-1), oldest first.
    fn ci_watch_events(&self) -> Result<Vec<RunEvent>>;
    /// Record one settled CI run in one write transaction unless another
    /// supervisor recorded a run since the event `record.previous`; `None`
    /// then.
    fn record_ci_check(
        &self,
        record: crate::domain::ci_watch::CiCheckRecord,
    ) -> Result<Option<crate::domain::ci_watch::CiCheckRecorded>>;
    /// Record `finding` unless one of the same kind, target and subject
    /// is there in any status, in one write transaction: its id, or
    /// `None` when one was there (the watch of an adopted definition,
    /// ADR-t1728-1 (g)).
    fn record_finding_once(
        &self,
        finding: crate::domain::NewFinding,
    ) -> Result<Option<crate::domain::FindingId>>;
    /// Every `ci_scheduled_checked` event (the runs of the scheduled
    /// workflows `[ci_watch]` declares, ADR-t3807-1), oldest first.
    fn ci_scheduled_events(&self) -> Result<Vec<RunEvent>>;
    /// Record one run of a scheduled workflow in one write transaction
    /// unless another supervisor recorded one since `record.previous`;
    /// `None` then.
    fn record_ci_scheduled_check(
        &self,
        record: crate::domain::ci_watch::scheduled::ScheduledCheckRecord,
    ) -> Result<Option<crate::domain::ci_watch::CiCheckRecorded>>;
    /// Every record of how old the declared workflows' latest successes
    /// are (`domain::ci_watch::freshness::FRESHNESS_KINDS`), oldest first.
    fn ci_freshness_events(&self) -> Result<Vec<RunEvent>>;
    /// Write one record of a workflow's freshness in one write
    /// transaction: a stale record unless one of the same workflow and
    /// latest success is there, a fresh record only while the workflow's
    /// latest record is the stale one it closes. Its event ID, or `None`
    /// when it was not written.
    fn record_ci_freshness(
        &self,
        record: crate::domain::ci_watch::freshness::FreshnessRecord,
    ) -> Result<Option<EventId>>;
    /// The `ci_failure` findings `task` fixes (linked to its proposal, or
    /// dismissed as covered by it).
    fn ci_failure_findings_of(&self, task: TaskId) -> Result<Vec<crate::domain::FindingId>>;
    /// The tasks that fix each `ci_failure` finding, by its ID: those of
    /// the proposal linked to it and the one it was dismissed as covered
    /// by, canceled ones aside, in ID order.
    fn ci_failure_fix_tasks(&self) -> Result<std::collections::BTreeMap<i64, Vec<TaskId>>>;
}

/// The ports of the queue 観測と分析's passes of the supervisor and their
/// jobs reach: the tasks and the records of the planning tables, the runs,
/// their leases and the supervisors read, its own records and findings,
/// and the asks a finding holds by. [`super::shared::Queue`] has it as a
/// supertrait, so the loop's connection upcasts to it.
pub trait ObservationQueue:
    TaskReads + PlanningRecords + SupervisorReads + CoordinationReads + RunLog + QueueRecords + AskStore
{
}

impl<
    T: TaskReads
        + PlanningRecords
        + SupervisorReads
        + CoordinationReads
        + RunLog
        + QueueRecords
        + AskStore
        + ?Sized,
> ObservationQueue for T
{
}
