//! The session steps in queue.db (`session_steps`, [`SessionStepStore`]).
//! Its boundary on a real queue.db is `tests/it/session_steps.rs`.

use super::*;
use crate::application::SessionStepStore;
use crate::domain::session_step::{
    self, SeqRange, SessionStep, Step, StepError, StepKind, StepTarget, StepsRecorded, TargetKind,
    gaps, ranges,
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
        let held: Option<(String, String)> = tx
            .query_row(
                "SELECT target_kind, target_id FROM session_steps WHERE session_id=?1 LIMIT 1",
                [session_id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;
        if held.is_some_and(|(kind, id)| kind != target.kind.as_str() || id != target.id) {
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
            "SELECT EXISTS(SELECT 1 FROM session_steps WHERE received_at < ?1)",
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
            "DELETE FROM session_step_removed WHERE NOT EXISTS(
                 SELECT 1 FROM session_steps
                 WHERE session_steps.session_id=session_step_removed.session_id)",
            [],
        )?;
        tx.commit()?;
        Ok(removed)
    }
}
