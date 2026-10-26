//! What a job command a supervisor started as its own (`observe`,
//! `throughput-review`) reads through that supervisor's queue, opening
//! none: the reads of the whole queue as the service computes them
//! ([`JobRead`]) with what the job read with Git for them, the checkout the
//! queue is bound to, and the improvement proposals against its
//! `dagq.toml`'s limit.

use crate::application::queue_reads::{JobRead, QueueRead};
use crate::application::stats::{StatsClient, call_read};
use crate::application::supervisor_service::SupervisorQueue;
use crate::domain::stats::StatsQuery;
use anyhow::Result;
use serde_json::Value;
use std::path::{Path, PathBuf};

/// A job command's reads through the supervisor's queue, `git` the host's
/// Git the job reads `stats` and `kpi`'s Git part with (the service runs
/// none).
pub struct JobReads<'a> {
    pub git: &'a StatsClient<'a>,
}

impl JobReads<'_> {
    /// What the service answers the job's `read`, sent with what the job
    /// read with Git for it ([`call_read`]).
    fn read(&self, queue: &SupervisorQueue, read: JobRead) -> Result<Value> {
        call_read(&QueueRead::Job(read), self.git, |read| match read {
            QueueRead::Job(read) => queue.job_read(read),
            other => anyhow::bail!("a job reads no {other:?}"),
        })
    }

    /// The main checkout of the repository the queue is bound to; none for
    /// a queue bound to none or a binding that names no checkout.
    fn checkout(queue: &SupervisorQueue) -> Result<Option<PathBuf>> {
        Ok(
            crate::application::SupervisorReads::repository_binding(queue)?
                .map(|common_dir| super::adapters::main_checkout_of(Path::new(&common_dir)))
                .and_then(Result::ok),
        )
    }

    /// `[kpi]`'s `max_improvement_proposals` of the bound checkout's
    /// `dagq.toml`, as it sets it.
    fn improvement_limit(queue: &SupervisorQueue) -> Result<Option<usize>> {
        Ok(match Self::checkout(queue)? {
            Some(checkout) => super::run_env::load_kpi_settings(&checkout)?
                .and_then(|settings| settings.max_improvement_proposals),
            None => None,
        })
    }
}

impl crate::application::observer::ObserverSources<SupervisorQueue> for JobReads<'_> {
    fn stats(&self, queue: &SupervisorQueue, _: &Path, query: &StatsQuery) -> Result<Value> {
        self.read(
            queue,
            JobRead::Stats {
                query: query.clone(),
                git: None,
            },
        )
    }
    fn kpi(&self, queue: &SupervisorQueue, _: &Path) -> Result<Value> {
        self.read(queue, JobRead::ObserverKpi { git: None })
    }
    fn improvements(&self, queue: &SupervisorQueue) -> Result<Value> {
        crate::application::kpi::improvements_of_setting(queue, Self::improvement_limit(queue)?)
    }
    fn checkout(&self, queue: &SupervisorQueue) -> Result<Option<PathBuf>> {
        Self::checkout(queue)
    }
}

impl crate::application::throughput_review::ThroughputReviewSources<SupervisorQueue>
    for JobReads<'_>
{
    fn stats(&self, queue: &SupervisorQueue, _: &Path, query: &StatsQuery) -> Result<Value> {
        self.read(
            queue,
            JobRead::Stats {
                query: query.clone(),
                git: None,
            },
        )
    }
    fn kpi(
        &self,
        queue: &SupervisorQueue,
        _: &Path,
        query: &crate::domain::kpi::KpiQuery,
    ) -> Result<Value> {
        self.read(
            queue,
            JobRead::Kpi {
                query: query.clone(),
                git: None,
            },
        )
    }
    fn checkout(&self, queue: &SupervisorQueue) -> Result<Option<PathBuf>> {
        Self::checkout(queue)
    }
    fn record_finding(
        &self,
        queue: &mut SupervisorQueue,
        finding: crate::domain::NewFinding,
    ) -> Result<i64> {
        queue.record_finding(&finding)
    }
}
