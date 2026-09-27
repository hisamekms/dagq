//! The `headless_jobs` rows (task 443): the process of each headless job a
//! supervisor started, so that another supervisor can stop it once the one
//! that started it is gone.
use anyhow::Result;
use rusqlite::{TransactionBehavior, params};

use super::sqlite::SqliteQueue;
use crate::{
    application::{HeadlessJobRecord, HeadlessJobStore, NewHeadlessJob},
    domain::HEARTBEAT_TIMEOUT_SECS,
};

impl HeadlessJobStore for SqliteQueue {
    fn record_headless_job(&self, job: &NewHeadlessJob) -> Result<i64> {
        let now = self.generators.clock.now();
        let tx = rusqlite::Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        tx.execute(
            "INSERT INTO headless_jobs(kind, label, run_id, proposal_id, goal_id, attempt, pid,
                                       process_start, supervisor_token, started_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
            params![
                job.kind,
                job.label,
                job.run_id,
                job.proposal_id,
                job.goal_id,
                job.attempt as i64,
                job.pid,
                job.process_start,
                job.supervisor_token,
                now
            ],
        )?;
        let id = tx.last_insert_rowid();
        tx.commit()?;
        Ok(id)
    }

    fn end_headless_job(&self, id: i64, outcome: &str) -> Result<bool> {
        let now = self.generators.clock.now();
        let tx = rusqlite::Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        let ended = tx.execute(
            "UPDATE headless_jobs SET ended_at=?2, outcome=?3 WHERE id=?1 AND ended_at IS NULL",
            params![id, now, outcome],
        )?;
        tx.commit()?;
        Ok(ended == 1)
    }

    fn orphaned_headless_jobs(&self, token: &str, own: bool) -> Result<Vec<HeadlessJobRecord>> {
        let now = self.generators.clock.now();
        Ok(self
            .conn
            .prepare(
                "SELECT j.id, j.kind, j.label, j.run_id, j.proposal_id, j.goal_id, j.attempt,
                        j.pid, j.process_start, j.supervisor_token, j.started_at, s.pid
                 FROM headless_jobs j
                 LEFT JOIN supervisors s ON s.token = j.supervisor_token
                 WHERE j.ended_at IS NULL
                   AND CASE WHEN j.supervisor_token = ?1 THEN ?2
                            ELSE s.token IS NULL OR ?3 - s.heartbeat_at > ?4 END
                 ORDER BY j.id",
            )?
            .query_map(params![token, own, now, HEARTBEAT_TIMEOUT_SECS], |r| {
                Ok(HeadlessJobRecord {
                    id: r.get(0)?,
                    kind: r.get(1)?,
                    label: r.get(2)?,
                    run_id: r.get(3)?,
                    proposal_id: r.get(4)?,
                    goal_id: r.get(5)?,
                    attempt: r.get::<_, i64>(6)? as usize,
                    pid: r.get(7)?,
                    process_start: r.get(8)?,
                    supervisor_token: r.get(9)?,
                    started_at: r.get(10)?,
                    supervisor_pid: r.get(11)?,
                })
            })?
            .collect::<rusqlite::Result<_>>()?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{RunId, headless_job};

    fn job(token: &str, pid: u32) -> NewHeadlessJob {
        NewHeadlessJob {
            kind: headless_job::REVIEW,
            label: None,
            run_id: Some(RunId::new("run-1").unwrap()),
            proposal_id: None,
            goal_id: None,
            attempt: 2,
            pid,
            process_start: Some("Sun Sep 27 10:00:00 2026".into()),
            supervisor_token: token.into(),
        }
    }

    #[test]
    fn only_the_unfinished_jobs_of_gone_supervisors_are_orphaned() {
        let dir = tempfile::tempdir().unwrap();
        let queue = SqliteQueue::init(dir.path().join("q.db")).unwrap();
        queue
            .conn
            .execute(
                "INSERT INTO supervisors(token, pid, parallel, heartbeat_at)
                 VALUES ('live', 1, 1, unixepoch()), ('stale', 1, 1, unixepoch() - 600)",
                [],
            )
            .unwrap();
        let live = queue.record_headless_job(&job("live", 10)).unwrap();
        let stale = queue.record_headless_job(&job("stale", 11)).unwrap();
        let gone = queue.record_headless_job(&job("gone", 12)).unwrap();
        let ended = queue.record_headless_job(&job("gone", 13)).unwrap();
        let mine = queue.record_headless_job(&job("me", 14)).unwrap();
        assert!(queue.end_headless_job(ended, headless_job::ENDED).unwrap());
        // A second end keeps the first outcome.
        assert!(
            !queue
                .end_headless_job(ended, headless_job::STOPPED)
                .unwrap()
        );
        let ids = |own| {
            queue
                .orphaned_headless_jobs("me", own)
                .unwrap()
                .into_iter()
                .map(|j| j.id)
                .collect::<Vec<_>>()
        };
        assert_eq!(ids(false), [stale, gone]);
        assert_eq!(ids(true), [stale, gone, mine]);
        let _ = live;
        let record = &queue.orphaned_headless_jobs("me", false).unwrap()[0];
        assert_eq!(record.kind, "review");
        assert_eq!(record.run_id.as_ref().unwrap().as_str(), "run-1");
        assert_eq!(record.attempt, 2);
        assert_eq!(record.pid, 11);
        assert_eq!(record.supervisor_token, "stale");
        assert_eq!(record.supervisor_pid, Some(1));
        let unregistered = &queue.orphaned_headless_jobs("me", false).unwrap()[1];
        assert_eq!(unregistered.supervisor_pid, None);
        let outcome: String = queue
            .conn
            .query_row(
                "SELECT outcome FROM headless_jobs WHERE id=?1",
                [ended],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(outcome, "ended");
    }
}
