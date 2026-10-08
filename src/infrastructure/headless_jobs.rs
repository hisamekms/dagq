//! The `headless_jobs` rows (task 443): the process of each headless job a
//! supervisor started, so that another supervisor can stop it once the one
//! that started it is gone.
use anyhow::Result;
use rusqlite::{TransactionBehavior, params};

use super::sqlite::SqliteQueue;
use crate::domain::write_rules::check_non_blank;
use crate::domain::{LeaseToken, Provider, headless_job};
use crate::{
    application::{HeadlessJobRecord, HeadlessJobStore, NewHeadlessJob},
    domain::HEARTBEAT_TIMEOUT_SECS,
};

impl HeadlessJobStore for SqliteQueue {
    fn record_headless_job(&self, job: &NewHeadlessJob) -> Result<i64> {
        check_non_blank("headless job kind", job.kind)?;
        let now = self.generators.clock.now();
        let tx = rusqlite::Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        tx.execute(
            "INSERT INTO headless_jobs(kind, label, run_id, proposal_id, goal_id, attempt, pid,
                                       process_start, supervisor_token, started_at, provider)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
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
                now,
                job.provider
                    .map_or(headless_job::NO_PROVIDER, Provider::as_str)
            ],
        )?;
        let id = tx.last_insert_rowid();
        tx.commit()?;
        Ok(id)
    }

    fn end_headless_job(&self, id: i64, outcome: &str) -> Result<bool> {
        check_non_blank("headless job outcome", outcome)?;
        let now = self.generators.clock.now();
        let tx = rusqlite::Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        let ended = tx.execute(
            "UPDATE headless_jobs SET ended_at=?2, outcome=?3 WHERE id=?1 AND ended_at IS NULL",
            params![id, now, outcome],
        )?;
        tx.commit()?;
        Ok(ended == 1)
    }

    fn orphaned_headless_jobs(
        &self,
        token: &LeaseToken,
        own: bool,
    ) -> Result<Vec<HeadlessJobRecord>> {
        let now = self.generators.clock.now();
        Ok(self
            .conn
            .prepare(
                "SELECT j.id, j.kind, j.label, j.run_id, j.proposal_id, j.goal_id, j.attempt,
                        j.pid, j.process_start, j.supervisor_token, j.started_at, s.pid,
                        j.provider
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
                    provider: r.get(12)?,
                })
            })?
            .collect::<rusqlite::Result<_>>()?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::RunId;

    fn job(token: &LeaseToken, pid: u32) -> NewHeadlessJob {
        NewHeadlessJob {
            kind: headless_job::REVIEW,
            label: None,
            run_id: Some(RunId::new("run-1").unwrap()),
            proposal_id: None,
            goal_id: None,
            attempt: 2,
            provider: Some(crate::domain::Provider::Claude),
            pid,
            process_start: Some("Sun Sep 27 10:00:00 2026".into()),
            supervisor_token: token.clone(),
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
        let live = queue
            .record_headless_job(&job(&LeaseToken::new("live"), 10))
            .unwrap();
        let stale = queue
            .record_headless_job(&job(&LeaseToken::new("stale"), 11))
            .unwrap();
        let gone = queue
            .record_headless_job(&job(&LeaseToken::new("gone"), 12))
            .unwrap();
        let ended = queue
            .record_headless_job(&job(&LeaseToken::new("gone"), 13))
            .unwrap();
        let mine = queue
            .record_headless_job(&job(&LeaseToken::new("me"), 14))
            .unwrap();
        assert!(queue.end_headless_job(ended, headless_job::ENDED).unwrap());
        // A second end keeps the first outcome.
        assert!(
            !queue
                .end_headless_job(ended, headless_job::STOPPED)
                .unwrap()
        );
        let ids = |own| {
            queue
                .orphaned_headless_jobs(&LeaseToken::new("me"), own)
                .unwrap()
                .into_iter()
                .map(|j| j.id)
                .collect::<Vec<_>>()
        };
        assert_eq!(ids(false), [stale, gone]);
        assert_eq!(ids(true), [stale, gone, mine]);
        let _ = live;
        let record = &queue
            .orphaned_headless_jobs(&LeaseToken::new("me"), false)
            .unwrap()[0];
        assert_eq!(record.kind, "review");
        assert_eq!(record.run_id.as_ref().unwrap().as_str(), "run-1");
        assert_eq!(record.attempt, 2);
        assert_eq!(record.pid, 11);
        assert_eq!(record.supervisor_token, "stale");
        assert_eq!(record.supervisor_pid, Some(1));
        let unregistered = &queue
            .orphaned_headless_jobs(&LeaseToken::new("me"), false)
            .unwrap()[1];
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

    /// A blank kind or outcome is refused before the write (ADR-t876-1: the
    /// rule the `headless_jobs` CHECK held), and nothing is recorded.
    #[test]
    fn a_blank_kind_or_outcome_is_not_written() {
        let dir = tempfile::tempdir().unwrap();
        let queue = SqliteQueue::init(dir.path().join("q.db")).unwrap();
        let token = LeaseToken::new("me");
        let blank = NewHeadlessJob {
            kind: " ",
            ..job(&token, 10)
        };
        let error = queue.record_headless_job(&blank).unwrap_err();
        assert_eq!(error.to_string(), "headless job kind must not be blank");
        let rows: i64 = queue
            .conn
            .query_row("SELECT count(*) FROM headless_jobs", [], |r| r.get(0))
            .unwrap();
        assert_eq!(rows, 0);
        let id = queue.record_headless_job(&job(&token, 10)).unwrap();
        let error = queue.end_headless_job(id, "").unwrap_err();
        assert_eq!(error.to_string(), "headless job outcome must not be blank");
        assert_eq!(
            queue.orphaned_headless_jobs(&token, true).unwrap()[0].id,
            id
        );
    }

    /// A job records its provider, a program job none, and a row written
    /// without one (by an older binary, or before migration 0055) reads as
    /// `claude`; each reads back with its kind.
    #[test]
    fn a_job_records_its_provider_and_one_without_reads_as_claude() {
        let dir = tempfile::tempdir().unwrap();
        let queue = SqliteQueue::init(dir.path().join("q.db")).unwrap();
        let token = LeaseToken::new("me");
        let codex = queue
            .record_headless_job(&NewHeadlessJob {
                provider: Some(crate::domain::Provider::Codex),
                ..job(&token, 10)
            })
            .unwrap();
        let program = queue
            .record_headless_job(&NewHeadlessJob {
                kind: headless_job::REVIEW_PROGRAM,
                label: Some("check-docs".into()),
                provider: None,
                ..job(&token, 12)
            })
            .unwrap();
        queue
            .conn
            .execute(
                "INSERT INTO headless_jobs(kind, attempt, pid, supervisor_token, started_at)
                 VALUES ('goal_review', 1, 11, 'me', 0)",
                [],
            )
            .unwrap();
        let providers: Vec<(i64, String, String, Option<String>)> = queue
            .orphaned_headless_jobs(&token, true)
            .unwrap()
            .into_iter()
            .map(|j| (j.id, j.kind, j.provider, j.label))
            .collect();
        let row = |id: i64, kind: &str, provider: &str, label: Option<&str>| {
            (
                id,
                kind.to_owned(),
                provider.to_owned(),
                label.map(str::to_owned),
            )
        };
        assert_eq!(
            providers,
            vec![
                row(codex, "review", "codex", None),
                row(program, "review_program", "none", Some("check-docs")),
                row(program + 1, "goal_review", "claude", None),
            ]
        );
    }
}
