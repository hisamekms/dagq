//! The session steps in queue.db (`session_steps`, [`SessionStepStore`]),
//! their resource samples (`session_resources`, [`SessionResourceStore`])
//! and their owners (`session_owners`, [`SessionOwners`]).
//! Its boundary on a real queue.db is `tests/it/session_steps.rs`.

use super::*;
use crate::application::{SessionOwners, SessionResourceStore, SessionStepStore};
use crate::domain::session_step::{
    self, ResourceSample, SeqRange, SessionOwner, SessionResource, SessionStep, Step, StepError,
    StepKind, StepTarget, StepsRecorded, TargetKind, gaps, ranges,
};

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

/// The seqs of `session_id` the retention removed.
fn removed(conn: &Connection, session_id: &str) -> rusqlite::Result<Vec<SeqRange>> {
    let mut statement = conn.prepare_cached(
        "SELECT from_seq, to_seq FROM session_step_removed WHERE session_id=?1 ORDER BY from_seq",
    )?;
    statement
        .query_map([session_id], |row| {
            Ok(SeqRange {
                from: row.get(0)?,
                to: row.get(1)?,
            })
        })?
        .collect()
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

    fn remove_steps(&self, before: i64) -> Result<usize> {
        // With none due no write lock is taken.
        let due: bool = self.conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM session_steps WHERE received_at < ?1)
                 OR EXISTS(SELECT 1 FROM session_resources WHERE received_at < ?1)
                 OR EXISTS(SELECT 1 FROM session_owners WHERE registered_at < ?1)",
            [before],
            |row| row.get(0),
        )?;
        if !due {
            return Ok(0);
        }
        let tx = self.write_tx("session_steps_retention", TransactionBehavior::Immediate)?;
        // The removed seqs of the sessions that keep a step: those of a
        // session removed whole are not kept.
        let kept: Vec<(String, i64)> = {
            let mut statement = tx.prepare(
                "SELECT old.session_id, old.seq FROM session_steps old
                 WHERE old.received_at < ?1 AND EXISTS(
                     SELECT 1 FROM session_steps new
                     WHERE new.session_id=old.session_id AND new.received_at >= ?1)
                 ORDER BY old.session_id, old.seq",
            )?;
            statement
                .query_map([before], |row| Ok((row.get(0)?, row.get(1)?)))?
                .collect::<rusqlite::Result<_>>()?
        };
        for session in kept.chunk_by(|a, b| a.0 == b.0) {
            for range in ranges(session.iter().map(|(_, seq)| *seq)) {
                tx.execute(
                    "INSERT INTO session_step_removed(session_id, from_seq, to_seq)
                     VALUES (?1, ?2, ?3)",
                    params![session[0].0, range.from, range.to],
                )?;
            }
        }
        let removed = tx.execute("DELETE FROM session_steps WHERE received_at < ?1", [before])?;
        tx.execute(
            "DELETE FROM session_resources WHERE received_at < ?1",
            [before],
        )?;
        tx.execute(
            "DELETE FROM session_owners WHERE registered_at < ?1
                 AND NOT EXISTS(SELECT 1 FROM session_steps s
                                WHERE s.session_id=session_owners.session_id)
                 AND NOT EXISTS(SELECT 1 FROM session_resources r
                                WHERE r.session_id=session_owners.session_id)",
            [before],
        )?;
        tx.execute(
            "DELETE FROM session_step_removed WHERE NOT EXISTS(
                 SELECT 1 FROM session_steps
                 WHERE session_steps.session_id=session_step_removed.session_id)",
            [],
        )?;
        tx.commit()?;
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
