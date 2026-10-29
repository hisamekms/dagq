//! The session steps in queue.db (`session_steps`, [`SessionStepStore`]),
//! their resource samples (`session_resources`, [`SessionResourceStore`])
//! and their owners (`session_owners`, [`SessionOwners`]).
//! Its boundary on a real queue.db is `tests/it/session_steps.rs`.

use super::*;
use crate::application::{SessionOwners, SessionResourceStore, SessionStepStore};
use crate::domain::session_step::{
    self, ResourceSample, SeqRange, SessionOwner, SessionResource, SessionStep, Step, StepError,
    StepKind, StepTarget, StepsRecorded, TargetKind, gaps, ranges, removed_within,
};
use crate::domain::telemetry::{Beat, TargetFacts};

/// The columns of a stored step, in the order [`step_row`] reads them.
const COLUMNS: &str = "id,target_kind,target_id,session_id,seq,kind,offset_ms,duration_ms,\
    model,tool,command_shape,exit_code,background,output_bytes,received_at";

fn unknown(index: usize, what: &str, value: &str) -> rusqlite::Error {
    rusqlite::Error::FromSqlConversionFailure(
        index,
        rusqlite::types::Type::Text,
        format!("unknown {what} {value:?}").into(),
    )
}

/// A row of `session_steps` read with [`COLUMNS`].
fn step_row(row: &Row<'_>) -> rusqlite::Result<SessionStep> {
    let target_kind: String = row.get(1)?;
    let kind: String = row.get(5)?;
    let shape: Option<String> = row.get(10)?;
    let command_shape = shape
        .map(|text| serde_json::from_str(&text))
        .transpose()
        .map_err(|error| {
            rusqlite::Error::FromSqlConversionFailure(
                10,
                rusqlite::types::Type::Text,
                Box::new(error),
            )
        })?;
    Ok(SessionStep {
        id: row.get(0)?,
        target: StepTarget {
            kind: TargetKind::parse(&target_kind)
                .ok_or_else(|| unknown(1, "target kind", &target_kind))?,
            id: row.get(2)?,
        },
        session_id: row.get(3)?,
        received_at: row.get(14)?,
        step: Step {
            seq: row.get(4)?,
            kind: StepKind::parse(&kind).ok_or_else(|| unknown(5, "step kind", &kind))?,
            offset_ms: row.get(6)?,
            duration_ms: row.get(7)?,
            model: row.get(8)?,
            tool: row.get(9)?,
            command_shape,
            exit_code: row.get(11)?,
            background: row.get(12)?,
            output_bytes: row.get(13)?,
        },
    })
}

/// The columns of a stored resource sample, in the order
/// [`resource_row`] reads them.
const RESOURCE_COLUMNS: &str = "id,target_kind,target_id,session_id,seq,offset_ms,duration_ms,\
    cpu_seconds,cpu_limit,throttled_seconds,memory_max_bytes,memory_limit_bytes,read_bytes,\
    write_bytes,cpu_pressure,load_average,node_id,received_at";

fn target_of(kind: &str, id: String, index: usize) -> rusqlite::Result<StepTarget> {
    Ok(StepTarget {
        kind: TargetKind::parse(kind).ok_or_else(|| unknown(index, "target kind", kind))?,
        id,
    })
}

/// A row of `session_resources` read with [`RESOURCE_COLUMNS`].
fn resource_row(row: &Row<'_>) -> rusqlite::Result<SessionResource> {
    let kind: String = row.get(1)?;
    Ok(SessionResource {
        id: row.get(0)?,
        target: target_of(&kind, row.get(2)?, 1)?,
        session_id: row.get(3)?,
        received_at: row.get(17)?,
        sample: ResourceSample {
            seq: row.get(4)?,
            offset_ms: row.get(5)?,
            duration_ms: row.get(6)?,
            cpu_seconds: row.get(7)?,
            cpu_limit: row.get(8)?,
            throttled_seconds: row.get(9)?,
            memory_max_bytes: row.get(10)?,
            memory_limit_bytes: row.get(11)?,
            read_bytes: row.get(12)?,
            write_bytes: row.get(13)?,
            cpu_pressure: row.get(14)?,
            load_average: row.get(15)?,
            node_id: row.get(16)?,
        },
    })
}

/// Whether `session_id` is of another target than `target`: its owner,
/// its steps or its samples name another.
fn of_another_target(
    conn: &Connection,
    session_id: &str,
    target: &StepTarget,
) -> rusqlite::Result<bool> {
    conn.query_row(
        "SELECT EXISTS(
             SELECT 1 FROM session_owners
             WHERE session_id=?1 AND (target_kind<>?2 OR target_id<>?3)
             UNION ALL
             SELECT 1 FROM session_steps
             WHERE session_id=?1 AND (target_kind<>?2 OR target_id<>?3)
             UNION ALL
             SELECT 1 FROM session_resources
             WHERE session_id=?1 AND (target_kind<>?2 OR target_id<>?3))",
        params![session_id, target.kind.as_str(), target.id],
        |row| row.get(0),
    )
}

/// The `from_seq` of the row of `session_step_removed` that voids the other
/// rows of its session. The retention writes it with the last steps of a
/// session that keeps none, so its ranges go at once for this binary's
/// readers however many transactions their rows take to go, and a step
/// that comes in meanwhile starts the session anew. Below
/// [`session_step::FIRST_SEQ`] and with `to_seq` below it, it holds no
/// seq, so a binary that does not know it reads it as nothing; such a
/// binary reads the ranges left of a removal stopped part way as still
/// removed seqs until this binary's retention removes them (its own
/// removes them only while the session has no step, and only when a row
/// is due).
const VOID_FROM: i64 = session_step::FIRST_SEQ - 1;

/// The seqs of `session_id` the retention removed: none once they are void
/// ([`VOID_FROM`]).
fn removed(conn: &Connection, session_id: &str) -> rusqlite::Result<Vec<SeqRange>> {
    let mut statement = conn.prepare_cached(
        "SELECT from_seq, to_seq FROM session_step_removed WHERE session_id=?1 ORDER BY from_seq",
    )?;
    let ranges = statement
        .query_map([session_id], |row| {
            Ok(SeqRange {
                from: row.get(0)?,
                to: row.get(1)?,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    if ranges.first().is_some_and(|range| range.from == VOID_FROM) {
        return Ok(Vec::new());
    }
    Ok(ranges)
}

/// The next rows of a session (`?1`) past seq `?2`, at most `?4`, in the
/// order of its index `(session_id, seq)`, each with whether it was received
/// before `?3`: the retention walks a session by these with no lock, each
/// read going past no more rows than its limit.
const WALK_STEPS: &str = "SELECT seq, received_at < ?3 FROM session_steps
     WHERE session_id=?1 AND seq > ?2 ORDER BY seq LIMIT ?4";

/// The next samples of a session as [`WALK_STEPS`] walks its steps, each
/// with its `id`.
const WALK_RESOURCES: &str = "SELECT seq, id, received_at < ?3 FROM session_resources
     WHERE session_id=?1 AND seq > ?2 ORDER BY seq LIMIT ?4";

/// Whether the step of a session (`?1`) at seq `?2` is there and was
/// received before `?3`, found by its key: a candidate read again under
/// the lock.
const STEP_DUE: &str = "SELECT 1 FROM session_steps
     WHERE session_id=?1 AND seq=?2 AND received_at < ?3";

/// Remove the step of a session (`?1`) at seq `?2`, found by its key.
const REMOVE_STEP: &str = "DELETE FROM session_steps WHERE session_id=?1 AND seq=?2";

/// The first steps of a session (`?1`), at most `?2`, to count whether it
/// has more than a batch's candidates: the read stops at its limit.
const STEPS_UP_TO: &str = "SELECT seq FROM session_steps WHERE session_id=?1 LIMIT ?2";

/// Remove every step of a session (`?1`), run only once [`STEPS_UP_TO`]
/// found no more than the batch's candidates.
const REMOVE_SESSION_STEPS: &str = "DELETE FROM session_steps WHERE session_id=?1";

/// Remove the sample `?1` if it was received before `?2`, found by its
/// primary key.
const REMOVE_DUE_RESOURCE: &str = "DELETE FROM session_resources WHERE id=?1 AND received_at < ?2";

/// Join the removed seqs `?3..=?4` of a session (`?1`) to its range that
/// ends at `?2` (`?3 - 1`), found by the primary key as the last one that
/// starts below.
const EXTEND_REMOVED: &str = "UPDATE session_step_removed SET to_seq=?4
     WHERE session_id=?1 AND to_seq=?2 AND from_seq=(
         SELECT max(from_seq) FROM session_step_removed WHERE session_id=?1 AND from_seq < ?3)";

/// Remove the first void ranges of a session (`?1`, those past the row at
/// `?2` that voids them, at most `?3`), in the order of its primary key.
const REMOVE_VOID_RANGES: &str = "DELETE FROM session_step_removed WHERE rowid IN (
     SELECT rowid FROM session_step_removed WHERE session_id=?1 AND from_seq > ?2
     ORDER BY from_seq LIMIT ?3)";

/// The statements a batch of the retention reads a session's rows by under
/// its lock, each with the search the migrations' indexes give it. Each
/// finds a row by its key, stops at its limit, or goes past only rows it
/// takes, so a batch reads about as many rows as it removes, not the rows
/// a session keeps (its boundary on a real queue.db is
/// `tests/it/session_steps.rs`).
pub const RETENTION_BATCH_READS: [(&str, &str); 7] = [
    (
        STEP_DUE,
        "sqlite_autoindex_session_steps_1 (session_id=? AND seq=?)",
    ),
    (
        REMOVE_STEP,
        "sqlite_autoindex_session_steps_1 (session_id=? AND seq=?)",
    ),
    (
        STEPS_UP_TO,
        "sqlite_autoindex_session_steps_1 (session_id=?)",
    ),
    (
        REMOVE_SESSION_STEPS,
        "sqlite_autoindex_session_steps_1 (session_id=?)",
    ),
    (REMOVE_DUE_RESOURCE, "INTEGER PRIMARY KEY (rowid=?)"),
    (
        EXTEND_REMOVED,
        "sqlite_autoindex_session_step_removed_1 (session_id=? AND from_seq<?)",
    ),
    (
        REMOVE_VOID_RANGES,
        "sqlite_autoindex_session_step_removed_1 (session_id=? AND from_seq>?)",
    ),
];

/// The statements the retention walks a session's rows by with no lock,
/// each with the search it makes in the session's index.
pub const RETENTION_CANDIDATE_READS: [(&str, &str); 2] = [
    (
        WALK_STEPS,
        "sqlite_autoindex_session_steps_1 (session_id=? AND seq>?)",
    ),
    (
        WALK_RESOURCES,
        "sqlite_autoindex_session_resources_1 (session_id=? AND seq>?)",
    ),
];

/// Record `range` of `session_id` as removed, joined to the range that ends
/// just below it if there is one, so that a run of consecutive seqs removed
/// over batches and days stays one row: one row written either way.
fn record_removed(
    tx: &crate::infrastructure::sqlite::WriteTx<'_>,
    session_id: &str,
    range: SeqRange,
) -> Result<usize> {
    let joined = tx.execute(
        EXTEND_REMOVED,
        params![session_id, range.from - 1, range.from, range.to],
    )?;
    if joined > 0 {
        return Ok(joined);
    }
    Ok(tx.execute(
        "INSERT INTO session_step_removed(session_id, from_seq, to_seq) VALUES (?1, ?2, ?3)",
        params![session_id, range.from, range.to],
    )?)
}

/// The rows of a session the retention may remove, read with no lock: the
/// steps and samples received before its time found so far, in the order
/// of their seqs, and how far the walk of each went. A batch reads each
/// again under its lock by its key and takes it off; a row another writer
/// adds meanwhile is received after that time, so the candidates of a
/// walk that went to the end are every row of the session due.
#[derive(Default)]
struct Candidates {
    /// The seqs of the steps due not yet taken.
    steps: Vec<i64>,
    /// The last seq of a step walked.
    steps_from: Option<i64>,
    /// Whether the walk of the steps went to the end.
    steps_walked: bool,
    /// The ids of the samples due not yet taken.
    samples: Vec<i64>,
    /// The last seq of a sample walked.
    samples_from: Option<i64>,
    /// Whether the walk of the samples went to the end.
    samples_walked: bool,
}

/// Walk `table`'s rows of `session_id` by `walk` ([`WALK_STEPS`] or
/// [`WALK_RESOURCES`]) past `from`, a read of at most
/// [`ROWS_PER_TRANSACTION`] rows at a time with no lock, until `due` holds
/// as many or the walk goes to the end: whether it did.
fn walk_due(
    conn: &Connection,
    walk: &str,
    session_id: &str,
    before: i64,
    from: &mut Option<i64>,
    due: &mut Vec<i64>,
) -> Result<bool> {
    let limit = i64::try_from(ROWS_PER_TRANSACTION)?;
    let mut statement = conn.prepare_cached(walk)?;
    // The row's seq, its key (its seq again for a step, its id for a
    // sample) and whether it is due.
    let due_at = statement.column_count() - 1;
    while due.len() < ROWS_PER_TRANSACTION {
        let rows: Vec<(i64, i64, bool)> = statement
            .query_map(
                params![session_id, from.unwrap_or(i64::MIN), before, limit],
                |row| Ok((row.get(0)?, row.get(due_at - 1)?, row.get(due_at)?)),
            )?
            .collect::<rusqlite::Result<_>>()?;
        if let Some(&(seq, ..)) = rows.last() {
            *from = Some(seq);
        }
        due.extend(rows.iter().filter(|row| row.2).map(|row| row.1));
        if rows.len() < ROWS_PER_TRANSACTION {
            return Ok(true);
        }
    }
    Ok(false)
}

impl Candidates {
    /// Walk on, with no lock, until each kind holds as many candidates as a
    /// transaction removes or its walk goes to the end.
    fn fill(&mut self, conn: &Connection, session_id: &str, before: i64) -> Result<()> {
        if !self.steps_walked {
            self.steps_walked = walk_due(
                conn,
                WALK_STEPS,
                session_id,
                before,
                &mut self.steps_from,
                &mut self.steps,
            )?;
        }
        if !self.samples_walked {
            self.samples_walked = walk_due(
                conn,
                WALK_RESOURCES,
                session_id,
                before,
                &mut self.samples_from,
                &mut self.samples,
            )?;
        }
        Ok(())
    }

    /// About how many rows of a transaction they take: a step and its range
    /// row, a sample, and the owner.
    fn rows(&self) -> usize {
        self.steps.len() * 2 + self.samples.len() + 1
    }
}

/// What [`remove_session_rows`] did to a session in one transaction.
struct SessionRemoved {
    /// The steps it removed.
    steps: usize,
    /// The rows it wrote or removed, of every table.
    rows: usize,
    /// Whether the session has no row left to remove; when not, it goes on
    /// in the next transaction.
    done: bool,
}

/// Remove at most `budget` of the void ranges of `session_id` (those the
/// row at [`VOID_FROM`] voids), then that row once none is left and the
/// budget has room: whether it is gone. `rows` counts what it removed.
fn remove_void(
    tx: &crate::infrastructure::sqlite::WriteTx<'_>,
    session_id: &str,
    budget: usize,
    rows: &mut usize,
) -> Result<bool> {
    let limit = budget.saturating_sub(*rows);
    if limit > 0 {
        *rows += tx.execute(
            REMOVE_VOID_RANGES,
            params![session_id, VOID_FROM, i64::try_from(limit)?],
        )?;
    }
    let left: bool = tx.query_row(
        "SELECT EXISTS(SELECT 1 FROM session_step_removed WHERE session_id=?1 AND from_seq > ?2)",
        params![session_id, VOID_FROM],
        |row| row.get(0),
    )?;
    if left || *rows >= budget {
        return Ok(false);
    }
    *rows += tx.execute(
        "DELETE FROM session_step_removed WHERE session_id=?1 AND from_seq=?2",
        params![session_id, VOID_FROM],
    )?;
    Ok(true)
}

/// Write or remove at most `budget` rows of `session_id` received before
/// `before`, of those `candidates` found with no lock, each read again
/// under the caller's lock by its key: the ranges a transaction before
/// voided ([`remove_void`]), the steps in the order of their seqs, then
/// the samples, then the owner registered before `before` of a session
/// left with none. What it reads under the lock goes past only the rows it
/// takes and those it stops at ([`RETENTION_BATCH_READS`]), not the rows
/// the session keeps.
///
/// Every state it commits reads, to this binary, as the session did before
/// the retention or as removed whole (its gaps, the seqs a step sent again
/// duplicates, its target). A removed seq of a session that keeps a step
/// goes to `session_step_removed` with it (a range row counts in `budget`,
/// [`removed_within`]), so a retention stopped part way leaves it no gap
/// and none counted twice, and a step that comes in meanwhile finds the
/// session as it was. A session that keeps no step loses its last steps
/// in the transaction that voids its ranges ([`VOID_FROM`]), which then go
/// a bounded number a transaction; a step that comes in meanwhile starts
/// the session anew.
fn remove_session_rows(
    tx: &crate::infrastructure::sqlite::WriteTx<'_>,
    session_id: &str,
    before: i64,
    budget: usize,
    candidates: &mut Candidates,
) -> Result<SessionRemoved> {
    let left = |rows: usize| budget.saturating_sub(rows);
    let mut rows = 0;
    let mut steps = 0;
    let not_done = |steps, rows| SessionRemoved {
        steps,
        rows,
        done: false,
    };
    let voided: bool = tx.query_row(
        "SELECT EXISTS(SELECT 1 FROM session_step_removed WHERE session_id=?1 AND from_seq=?2)",
        params![session_id, VOID_FROM],
        |row| row.get(0),
    )?;
    // The ranges voided before go first: a range written now is not void.
    if voided && !remove_void(tx, session_id, budget, &mut rows)? {
        return Ok(not_done(steps, rows));
    }
    // The first candidates the transaction has room for, those still due.
    let taken = candidates.steps.len().min(left(rows));
    let read: Vec<i64> = {
        let mut statement = tx.prepare_cached(STEP_DUE)?;
        let mut read = Vec::with_capacity(taken);
        for seq in candidates.steps.drain(..taken) {
            if statement.exists(params![session_id, seq, before])? {
                read.push(seq);
            }
        }
        read
    };
    // A read that is not full and leaves no candidate of a walk that went
    // to the end holds every step due, so the count for a step kept goes
    // past no more than those.
    let all_due = candidates.steps_walked && candidates.steps.is_empty() && read.len() < left(rows);
    let whole: Option<bool> = if all_due {
        let held = tx
            .prepare_cached(STEPS_UP_TO)?
            .query_map(params![session_id, i64::try_from(read.len() + 1)?], |_| {
                Ok(())
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?
            .len();
        if held == read.len() {
            Some(tx.query_row(
                "SELECT EXISTS(SELECT 1 FROM session_step_removed WHERE session_id=?1)",
                [session_id],
                |row| row.get(0),
            )?)
        } else {
            None
        }
    } else {
        None
    };
    if let Some(ranged) = whole {
        // Removed whole: its last steps go with the row that voids its
        // ranges, which fits as the read was not full.
        steps = tx.execute(REMOVE_SESSION_STEPS, [session_id])?;
        rows += steps;
        if ranged {
            rows += tx.execute(
                "INSERT INTO session_step_removed(session_id, from_seq, to_seq)
                 VALUES (?1, ?2, ?3)",
                params![session_id, VOID_FROM, VOID_FROM - 1],
            )?;
            if !remove_void(tx, session_id, budget, &mut rows)? {
                return Ok(not_done(steps, rows));
            }
        }
    } else {
        let fits = removed_within(&read, left(rows));
        let seqs = &read[..fits];
        for range in ranges(seqs.iter().copied()) {
            rows += record_removed(tx, session_id, range)?;
        }
        for &seq in seqs {
            steps += tx.execute(REMOVE_STEP, params![session_id, seq])?;
        }
        rows += steps;
        // The steps due it had no room for are the next transaction's
        // first candidates.
        candidates.steps.splice(..0, read[fits..].iter().copied());
        if !all_due || fits < read.len() {
            return Ok(not_done(steps, rows));
        }
    }
    let limit = left(rows);
    if limit == 0 {
        return Ok(not_done(steps, rows));
    }
    let taken = candidates.samples.len().min(limit);
    let mut samples = 0;
    for id in candidates.samples.drain(..taken) {
        samples += tx.execute(REMOVE_DUE_RESOURCE, params![id, before])?;
    }
    rows += samples;
    // A full take may leave samples due.
    if taken == limit || !candidates.samples_walked {
        return Ok(not_done(steps, rows));
    }
    // The samples left room for one more row.
    rows += tx.execute(
        "DELETE FROM session_owners WHERE session_id=?1 AND registered_at < ?2
             AND NOT EXISTS(SELECT 1 FROM session_steps WHERE session_id=?1)
             AND NOT EXISTS(SELECT 1 FROM session_resources WHERE session_id=?1)",
        params![session_id, before],
    )?;
    Ok(SessionRemoved {
        steps,
        rows,
        done: true,
    })
}

impl SqliteQueue {
    /// The steps `filter` (a `WHERE` clause and its order) keeps.
    fn read_steps(&self, filter: &str, params: impl rusqlite::Params) -> Result<Vec<SessionStep>> {
        let mut statement = self.conn.prepare(&format!(
            "SELECT {COLUMNS} FROM session_steps WHERE {filter}"
        ))?;
        let steps = statement
            .query_map(params, step_row)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(steps)
    }
}

impl SessionStepStore for SqliteQueue {
    fn record_steps(
        &self,
        target: &StepTarget,
        session_id: &str,
        steps: &[Step],
        received_at: i64,
    ) -> Result<StepsRecorded> {
        session_step::check(steps)?;
        let tx = self.write_tx("session_steps_record", TransactionBehavior::Immediate)?;
        if of_another_target(&tx, session_id, target)? {
            return Err(StepError::SessionOfAnotherTarget.into());
        }
        let removed = removed(&tx, session_id)?;
        let mut recorded = StepsRecorded::default();
        {
            let mut insert = tx.prepare_cached(
                "INSERT INTO session_steps(target_kind,target_id,session_id,seq,kind,
                     offset_ms,duration_ms,model,tool,command_shape,exit_code,background,
                     output_bytes,received_at)
                 VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14)
                 ON CONFLICT(session_id, seq) DO NOTHING",
            )?;
            for step in steps {
                // A step the retention removed came in already.
                if removed.iter().any(|range| range.contains(step.seq)) {
                    recorded.duplicates += 1;
                    continue;
                }
                let shape = step
                    .command_shape
                    .as_ref()
                    .map(serde_json::to_string)
                    .transpose()?;
                let rows = insert.execute(params![
                    target.kind.as_str(),
                    target.id,
                    session_id,
                    step.seq,
                    step.kind.as_str(),
                    step.offset_ms,
                    step.duration_ms,
                    step.model,
                    step.tool,
                    shape,
                    step.exit_code,
                    step.background,
                    step.output_bytes,
                    received_at,
                ])?;
                tx.wrote(rows);
                if rows == 1 {
                    recorded.added += 1;
                } else {
                    recorded.duplicates += 1;
                }
            }
        }
        tx.commit()?;
        Ok(recorded)
    }

    fn session_gaps(&self, session_id: &str) -> Result<Vec<SeqRange>> {
        let mut statement = self
            .conn
            .prepare_cached("SELECT seq FROM session_steps WHERE session_id=?1 ORDER BY seq")?;
        let seqs = statement
            .query_map([session_id], |row| row.get(0))?
            .collect::<rusqlite::Result<Vec<i64>>>()?;
        let mut covered = ranges(seqs);
        covered.extend(removed(&self.conn, session_id)?);
        Ok(gaps(covered))
    }

    fn steps_after(&self, after: i64, limit: usize) -> Result<Vec<SessionStep>> {
        self.read_steps(
            "id > ?1 ORDER BY id LIMIT ?2",
            params![after, i64::try_from(limit)?],
        )
    }

    fn target_steps(&self, target: &StepTarget) -> Result<Vec<SessionStep>> {
        self.read_steps(
            "target_kind=?1 AND target_id=?2 ORDER BY id",
            params![target.kind.as_str(), target.id],
        )
    }

    /// Read with no lock which sessions have a row due, then remove their
    /// rows by transactions of at most [`ROWS_PER_TRANSACTION`] rows and
    /// sessions, [`PAUSE_BETWEEN_TRANSACTIONS`] apart with no lock held: with
    /// none due no write lock is taken. Before each transaction, with no
    /// lock, it walks the sessions the transaction may reach for the rows
    /// due ([`Candidates`]), so the walk past the rows a session keeps is
    /// not under the lock. Each transaction reads those rows again under its
    /// lock by their keys ([`remove_session_rows`]), so a step another
    /// writer added between two is kept and not counted as removed, and a
    /// retention stopped between two is finished by the next.
    fn remove_steps(&self, before: i64) -> Result<usize> {
        let sessions: Vec<String> = {
            let mut statement = self.conn.prepare(
                "SELECT session_id FROM session_steps WHERE received_at < ?1
                 UNION SELECT session_id FROM session_resources WHERE received_at < ?1
                 UNION SELECT session_id FROM session_owners o WHERE registered_at < ?1
                     AND NOT EXISTS(SELECT 1 FROM session_steps s
                                    WHERE s.session_id=o.session_id AND s.received_at >= ?1)
                     AND NOT EXISTS(SELECT 1 FROM session_resources r
                                    WHERE r.session_id=o.session_id AND r.received_at >= ?1)
                 UNION SELECT session_id FROM session_step_removed d WHERE from_seq=?2
                     OR NOT EXISTS(SELECT 1 FROM session_steps s WHERE s.session_id=d.session_id)
                 ORDER BY 1",
            )?;
            statement
                .query_map(params![before, VOID_FROM], |row| row.get(0))?
                .collect::<rusqlite::Result<_>>()?
        };
        let mut candidates: Vec<Option<Candidates>> = Vec::new();
        candidates.resize_with(sessions.len(), || None);
        let (mut next, mut removed, mut first) = (0, 0, true);
        while next < sessions.len() {
            if !first {
                std::thread::sleep(PAUSE_BETWEEN_TRANSACTIONS);
            }
            first = false;
            // The sessions the transaction may reach, walked with no lock.
            let mut reach = next;
            let mut planned = 0;
            while reach < sessions.len() && planned < ROWS_PER_TRANSACTION {
                let session = candidates[reach].get_or_insert_with(Candidates::default);
                session.fill(&self.conn, &sessions[reach], before)?;
                planned += session.rows();
                reach += 1;
            }
            let tx = self.write_tx("session_steps_retention", TransactionBehavior::Immediate)?;
            let mut budget = ROWS_PER_TRANSACTION;
            let mut steps = 0;
            while budget > 0 && next < reach {
                let walked = candidates[next].get_or_insert_with(Candidates::default);
                let session = remove_session_rows(&tx, &sessions[next], before, budget, walked)?;
                steps += session.steps;
                // A session that is not done goes on in the next
                // transaction; one that removes nothing still takes a
                // place, so a transaction goes through at most so many
                // sessions too.
                if !session.done {
                    break;
                }
                budget = budget.saturating_sub(session.rows.max(1));
                candidates[next] = None;
                next += 1;
            }
            tx.commit()?;
            removed += steps;
        }
        Ok(removed)
    }
}

impl SessionResourceStore for SqliteQueue {
    fn record_resources(
        &self,
        target: &StepTarget,
        session_id: &str,
        samples: &[ResourceSample],
        received_at: i64,
    ) -> Result<StepsRecorded> {
        session_step::check_resources(samples)?;
        let tx = self.write_tx("session_resources_record", TransactionBehavior::Immediate)?;
        if of_another_target(&tx, session_id, target)? {
            return Err(StepError::SessionOfAnotherTarget.into());
        }
        let mut recorded = StepsRecorded::default();
        {
            let mut insert = tx.prepare_cached(
                "INSERT INTO session_resources(target_kind,target_id,session_id,seq,offset_ms,
                     duration_ms,cpu_seconds,cpu_limit,throttled_seconds,memory_max_bytes,
                     memory_limit_bytes,read_bytes,write_bytes,cpu_pressure,load_average,node_id,
                     received_at)
                 VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17)
                 ON CONFLICT(session_id, seq) DO NOTHING",
            )?;
            for sample in samples {
                let rows = insert.execute(params![
                    target.kind.as_str(),
                    target.id,
                    session_id,
                    sample.seq,
                    sample.offset_ms,
                    sample.duration_ms,
                    sample.cpu_seconds,
                    sample.cpu_limit,
                    sample.throttled_seconds,
                    sample.memory_max_bytes,
                    sample.memory_limit_bytes,
                    sample.read_bytes,
                    sample.write_bytes,
                    sample.cpu_pressure,
                    sample.load_average,
                    sample.node_id,
                    received_at,
                ])?;
                tx.wrote(rows);
                if rows == 1 {
                    recorded.added += 1;
                } else {
                    recorded.duplicates += 1;
                }
            }
        }
        tx.commit()?;
        Ok(recorded)
    }

    fn target_resources(&self, target: &StepTarget) -> Result<Vec<SessionResource>> {
        let mut statement = self.conn.prepare(&format!(
            "SELECT {RESOURCE_COLUMNS} FROM session_resources
             WHERE target_kind=?1 AND target_id=?2 ORDER BY id"
        ))?;
        let samples = statement
            .query_map(params![target.kind.as_str(), target.id], resource_row)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(samples)
    }

    fn resources_after(&self, after: i64, limit: usize) -> Result<Vec<SessionResource>> {
        let mut statement = self.conn.prepare(&format!(
            "SELECT {RESOURCE_COLUMNS} FROM session_resources WHERE id > ?1 ORDER BY id LIMIT ?2"
        ))?;
        let samples = statement
            .query_map(params![after, i64::try_from(limit)?], resource_row)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(samples)
    }
}

impl SessionOwners for SqliteQueue {
    fn register_session(
        &self,
        target: &StepTarget,
        session_id: &str,
        registered_at: i64,
    ) -> Result<()> {
        let tx = self.write_tx("session_owner_register", TransactionBehavior::Immediate)?;
        if of_another_target(&tx, session_id, target)? {
            return Err(StepError::SessionOfAnotherTarget.into());
        }
        let rows = tx.execute(
            "INSERT INTO session_owners(session_id, target_kind, target_id, registered_at)
             VALUES (?1, ?2, ?3, ?4) ON CONFLICT(session_id) DO NOTHING",
            params![session_id, target.kind.as_str(), target.id, registered_at],
        )?;
        tx.wrote(rows);
        tx.commit()?;
        Ok(())
    }

    fn session_owner(&self, session_id: &str) -> Result<Option<SessionOwner>> {
        let owner = self
            .conn
            .query_row(
                "SELECT target_kind, target_id, registered_at FROM session_owners
                 WHERE session_id=?1",
                [session_id],
                |row| {
                    let kind: String = row.get(0)?;
                    Ok(SessionOwner {
                        session_id: session_id.to_owned(),
                        target: target_of(&kind, row.get(1)?, 0)?,
                        registered_at: row.get(2)?,
                    })
                },
            )
            .optional()?;
        Ok(owner)
    }
}

/// A supervised job's row as a target of no run's facts are read: `Ended`
/// once it finished (`ended`), else the registration of the supervisor
/// that runs it (its `pid` and its heartbeat's `age`), none once it is gone.
fn supervised_job(ended: Option<i64>, pid: Option<i64>, age: Option<i64>) -> TargetFacts {
    match ended {
        Some(_) => TargetFacts::Ended,
        None => TargetFacts::SupervisedJob {
            supervisor: pid.zip(age).map(|(pid, age_secs)| Beat {
                pid: u32::try_from(pid).unwrap_or_default(),
                age_secs,
            }),
        },
    }
}

impl SqliteQueue {
    /// What the queue holds of `target`, a target of no run, for whether its
    /// telemetry token still holds ([`crate::domain::telemetry::TargetFacts`]):
    /// a planner's `closed_at`; a plan review's, a goal review's or an eval
    /// job's row (`finished_at`, `ended_at`) and the registration of the
    /// supervisor that runs it, its heartbeat's age by the DB's clock; a
    /// command's start (`observe_started`, `throughput_review_started`) and
    /// a finish of its directory after it, the time it ends by from its
    /// start, its `timeout_secs` and the margin, and its `pid`. A target
    /// the queue does not have has ended, and so does a run's.
    pub fn telemetry_target(&self, target: &StepTarget) -> Result<TargetFacts> {
        let Ok(id) = target.id.parse::<i64>() else {
            return Ok(TargetFacts::Ended);
        };
        let job = |table: &str, ended: &str, only: &str| -> Result<TargetFacts> {
            let row = self
                .conn
                .query_row(
                    &format!(
                        "SELECT r.{ended}, s.pid, unixepoch() - s.heartbeat_at FROM {table} r
                         LEFT JOIN supervisors s ON s.token = r.supervisor_token
                         WHERE r.id = ?1{only}"
                    ),
                    [id],
                    |row| {
                        Ok((
                            row.get::<_, Option<i64>>(0)?,
                            row.get::<_, Option<i64>>(1)?,
                            row.get::<_, Option<i64>>(2)?,
                        ))
                    },
                )
                .optional()?;
            Ok(row.map_or(TargetFacts::Ended, |(ended, pid, age)| {
                supervised_job(ended, pid, age)
            }))
        };
        let command = |started: EventKind, finished: EventKind| -> Result<TargetFacts> {
            let row = self
                .conn
                .query_row(
                    "SELECT CAST((julianday(s.created_at) - 2440587.5) * 86400000 AS INTEGER),
                            json_extract(s.payload, '$.timeout_secs'),
                            json_extract(s.payload, '$.pid'),
                            EXISTS (SELECT 1 FROM run_events f
                                    WHERE f.kind = ?3 AND f.id > s.id
                                      AND json_extract(f.payload, '$.dir')
                                          = json_extract(s.payload, '$.dir'))
                     FROM run_events s WHERE s.id = ?1 AND s.kind = ?2",
                    params![id, started.as_str(), finished.as_str()],
                    |row| {
                        Ok((
                            row.get::<_, i64>(0)?,
                            row.get::<_, Option<i64>>(1)?,
                            row.get::<_, Option<i64>>(2)?,
                            row.get::<_, bool>(3)?,
                        ))
                    },
                )
                .optional()?;
            Ok(match row {
                Some((started_ms, timeout_secs, pid, false)) => TargetFacts::StartedCommand {
                    ends_by_ms: started_ms.saturating_add(timeout_secs.map_or(
                        crate::domain::throughput_review::RUNNING_MS,
                        |secs| {
                            secs.saturating_mul(1000)
                                .saturating_add(crate::domain::telemetry::COMMAND_MARGIN_MS)
                        },
                    )),
                    pid: pid.and_then(|pid| u32::try_from(pid).ok()),
                },
                _ => TargetFacts::Ended,
            })
        };
        match target.kind {
            TargetKind::Run => Ok(TargetFacts::Ended),
            TargetKind::Planner => {
                let closed = self
                    .conn
                    .query_row(
                        "SELECT closed_at IS NOT NULL FROM planners WHERE id = ?1",
                        [id],
                        |row| row.get::<_, bool>(0),
                    )
                    .optional()?;
                Ok(if closed == Some(false) {
                    TargetFacts::OpenPlanner
                } else {
                    TargetFacts::Ended
                })
            }
            TargetKind::PlanReview => job("plan_reviews", "finished_at", ""),
            TargetKind::GoalReview => job("goal_reviews", "finished_at", ""),
            TargetKind::EvalJob => job(
                "headless_jobs",
                "ended_at",
                &format!(
                    " AND r.run_id IS NULL AND r.kind = '{}'",
                    crate::domain::headless_job::AGENT_EVAL
                ),
            ),
            TargetKind::ThroughputReview => command(
                EventKind::ThroughputReviewStarted,
                EventKind::ThroughputReviewFinished,
            ),
            TargetKind::Observer => command(EventKind::ObserveStarted, EventKind::ObserveFinished),
        }
    }

    /// Who started `target`, a target of no run
    /// ([`crate::domain::queue_service::TargetStart`]): a command's start
    /// event's actor and the `pid` of its payload, a supervised job's row's
    /// `supervisor_token`. `None` for a target with no such record: a run's,
    /// a planner's, and one the queue does not have.
    pub fn target_start(
        &self,
        target: &StepTarget,
    ) -> Result<Option<crate::domain::queue_service::TargetStart>> {
        use crate::domain::queue_service::TargetStart;
        let Ok(id) = target.id.parse::<i64>() else {
            return Ok(None);
        };
        let command = |started: EventKind| -> Result<Option<TargetStart>> {
            let row = self
                .conn
                .query_row(
                    "SELECT actor_role, actor_id, json_extract(payload, '$.pid')
                     FROM run_events WHERE id = ?1 AND kind = ?2",
                    params![id, started.as_str()],
                    |row| {
                        Ok((
                            row.get::<_, Option<String>>(0)?,
                            row.get::<_, Option<String>>(1)?,
                            row.get::<_, Option<i64>>(2)?,
                        ))
                    },
                )
                .optional()?;
            Ok(row.map(|(role, actor_id, pid)| TargetStart::Command {
                actor: role
                    .and_then(|role| role.parse::<crate::domain::ActorRole>().ok())
                    .zip(actor_id),
                pid: pid.and_then(|pid| u32::try_from(pid).ok()),
            }))
        };
        let row = |table: &str, only: &str| -> Result<Option<TargetStart>> {
            let supervisor = self
                .conn
                .query_row(
                    &format!("SELECT supervisor_token FROM {table} r WHERE r.id = ?1{only}"),
                    [id],
                    |row| row.get::<_, Option<String>>(0),
                )
                .optional()?;
            Ok(supervisor.map(|supervisor| TargetStart::Supervised {
                supervisor: supervisor.map(LeaseToken::new),
            }))
        };
        match target.kind {
            TargetKind::Run | TargetKind::Planner => Ok(None),
            TargetKind::PlanReview => row("plan_reviews", ""),
            TargetKind::GoalReview => row("goal_reviews", ""),
            TargetKind::EvalJob => row(
                "headless_jobs",
                &format!(
                    " AND r.run_id IS NULL AND r.kind = '{}'",
                    crate::domain::headless_job::AGENT_EVAL
                ),
            ),
            TargetKind::ThroughputReview => command(EventKind::ThroughputReviewStarted),
            TargetKind::Observer => command(EventKind::ObserveStarted),
        }
    }

    /// What the queue holds of each record of a job's
    /// ([`crate::domain::queue_service::JobRecords`]), read as
    /// [`Self::telemetry_target`] reads a target of no run, with whose it
    /// is: a plan or goal review attempt's rows and a run's job's
    /// `headless_jobs` rows (its finish and its supervisor's registration),
    /// and the command starts of a mode's period or of an observation
    /// (with the process they name). Empty for a job the queue has no
    /// record of.
    pub fn job_facts(
        &self,
        records: &crate::domain::queue_service::JobRecords,
    ) -> Result<Vec<crate::domain::queue_service::JobRecord>> {
        use crate::domain::queue_service::{JobRecord, JobRecords, RowLabel};
        // Each record's id and whose it is: a row's registration, a
        // start's process.
        type Whose = (i64, Option<String>, Option<i64>);
        let rows = |sql: &str, values: &[&dyn rusqlite::ToSql]| -> Result<Vec<Whose>> {
            let mut statement = self.conn.prepare(sql)?;
            let rows = statement
                .query_map(values, |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(rows)
        };
        let of = |kind: TargetKind, rows: Vec<Whose>| -> Result<Vec<JobRecord>> {
            rows.into_iter()
                .map(|(id, supervisor, pid)| {
                    Ok(JobRecord {
                        facts: self.telemetry_target(&StepTarget::of(kind, id))?,
                        supervisor: supervisor.map(LeaseToken::new),
                        pid: pid.and_then(|pid| u32::try_from(pid).ok()),
                    })
                })
                .collect()
        };
        let started = "SELECT id, NULL, json_extract(payload, '$.pid') FROM run_events
                       WHERE kind = ?1";
        match records {
            JobRecords::PlanReview { proposal, attempt } => of(
                TargetKind::PlanReview,
                rows(
                    "SELECT id, supervisor_token, NULL FROM plan_reviews
                     WHERE proposal_id = ?1 AND attempt = ?2",
                    params![proposal, attempt],
                )?,
            ),
            JobRecords::GoalReview { goal, attempt } => of(
                TargetKind::GoalReview,
                rows(
                    "SELECT id, supervisor_token, NULL FROM goal_reviews
                     WHERE goal_id = ?1 AND attempt = ?2",
                    params![goal, attempt],
                )?,
            ),
            JobRecords::ThroughputReview { mode, period } => of(
                TargetKind::ThroughputReview,
                rows(
                    &format!(
                        "{started} AND json_extract(payload, '$.mode') = ?2
                           AND json_extract(payload, '$.period') = ?3"
                    ),
                    params![EventKind::ThroughputReviewStarted.as_str(), mode, period],
                )?,
            ),
            JobRecords::Observation { session_id } => of(
                TargetKind::Observer,
                rows(
                    &format!("{started} AND json_extract(payload, '$.session_id') = ?2"),
                    params![EventKind::ObserveStarted.as_str(), session_id],
                )?,
            ),
            JobRecords::RunJob {
                kinds,
                run,
                label,
                attempt,
            } => {
                let (operator, label) = match label {
                    RowLabel::Is(label) => ("IS", label.as_str()),
                    RowLabel::Not(label) => ("IS NOT", *label),
                };
                let mut records = Vec::new();
                for kind in *kinds {
                    let mut statement = self.conn.prepare(&format!(
                        "SELECT r.ended_at, s.pid, unixepoch() - s.heartbeat_at, r.supervisor_token
                         FROM headless_jobs r
                         LEFT JOIN supervisors s ON s.token = r.supervisor_token
                         WHERE r.kind = ?1 AND r.run_id = ?2 AND r.attempt = ?3
                           AND r.label {operator} ?4"
                    ))?;
                    let rows = statement
                        .query_map(params![kind, run, attempt, label], |row| {
                            Ok(JobRecord {
                                facts: supervised_job(row.get(0)?, row.get(1)?, row.get(2)?),
                                supervisor: Some(LeaseToken::new(row.get::<_, String>(3)?)),
                                pid: None,
                            })
                        })?
                        .collect::<rusqlite::Result<Vec<_>>>()?;
                    records.extend(rows);
                }
                Ok(records)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::infrastructure::sqlite::WriteTx;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU64, Ordering};

    const BEFORE: i64 = 1_000;

    /// The VM steps the transactions of the retention of one session take
    /// under their lock, on the tables of the migrations: `kept` steps and
    /// samples received after [`BEFORE`] at its first seqs, then 300 steps
    /// and 50 samples due. With the steps they remove.
    fn steps_under_the_lock(kept: i64) -> (u64, usize) {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(include_str!("../../../migrations/0082_session_steps.sql"))
            .unwrap();
        conn.execute_batch(include_str!(
            "../../../migrations/0083_session_owners_and_resources.sql"
        ))
        .unwrap();
        conn.execute(
            "WITH RECURSIVE n(seq) AS (SELECT 1 UNION ALL SELECT seq + 1 FROM n WHERE seq < ?1 + 300)
             INSERT INTO session_steps(target_kind, target_id, session_id, seq, kind,
                 offset_ms, duration_ms, received_at)
             SELECT 'run', 'r', 's', seq, 'tool', 0, 0, CASE WHEN seq > ?1 THEN 1 ELSE 2000 END
             FROM n",
            [kept],
        )
        .unwrap();
        conn.execute(
            "WITH RECURSIVE n(seq) AS (SELECT 1 UNION ALL SELECT seq + 1 FROM n WHERE seq < ?1 + 50)
             INSERT INTO session_resources(target_kind, target_id, session_id, seq,
                 offset_ms, duration_ms, received_at)
             SELECT 'run', 'r', 's', seq, 0, 0, CASE WHEN seq > ?1 THEN 1 ELSE 2000 END
             FROM n",
            [kept],
        )
        .unwrap();
        let counted = Arc::new(AtomicU64::new(0));
        let mut candidates = Candidates::default();
        let (mut removed, mut done) = (0, false);
        while !done {
            candidates.fill(&conn, "s", BEFORE).unwrap();
            let count = counted.clone();
            conn.progress_handler(
                1,
                Some(move || {
                    count.fetch_add(1, Ordering::Relaxed);
                    false
                }),
            )
            .unwrap();
            let tx = WriteTx::begin(
                &conn,
                "t",
                TransactionBehavior::Immediate,
                std::time::Duration::MAX,
            )
            .unwrap();
            let session =
                remove_session_rows(&tx, "s", BEFORE, ROWS_PER_TRANSACTION, &mut candidates)
                    .unwrap();
            tx.commit().unwrap();
            conn.progress_handler(1, None::<fn() -> bool>).unwrap();
            removed += session.steps;
            done = session.done;
        }
        (counted.load(Ordering::Relaxed), removed)
    }

    /// A batch reads under its lock the rows it removes, found by their
    /// keys, not the rows the session keeps before them: a session that
    /// keeps a thousand times more rows at its first seqs takes as many VM
    /// steps under the lock.
    #[test]
    fn a_retention_batch_reads_under_its_lock_no_row_its_session_keeps() {
        let (few, removed) = steps_under_the_lock(10);
        assert_eq!(removed, 300);
        let (many, removed) = steps_under_the_lock(10_000);
        assert_eq!(removed, 300);
        assert!(many <= few + few / 10, "{many} VM steps against {few}");
    }
}
