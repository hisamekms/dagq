//! The remote pass does HTTP off the write line. Each queue call independently
//! rejoins it and opens a fenced entry, so slow GitHub replies cannot block
//! lease renewal or human writes for the duration of the whole pass.
use super::*;
use crate::application::{RunnerLeaseStore, RunnerTakeoverStore};
use crate::domain::{run_refs::End, runner_lease::*, runner_takeover::Takeover};

pub(super) struct RefQueue(pub Arc<SqliteServiceBackend>);
impl RefQueue {
    fn call<T: Send + 'static>(
        &self,
        f: impl FnOnce(&mut SqliteQueue) -> Result<T> + Send + 'static,
    ) -> Result<T> {
        let on = self.0.clone();
        let (send, receive) = std::sync::mpsc::sync_channel(1);
        on_line(self.0.write_line.as_ref(), move || {
            let result = (|| {
                let mut queue = write_queue(&on.storage, on.write_line.as_ref(), &on.db)?
                    .with_generators(on.generators.clone())
                    .with_actor(SqliteServiceBackend::<SqliteStorage>::own_actor());
                f(&mut queue)
            })();
            let _ = send.send(result);
            Ok(())
        })?;
        receive.recv().context("runner ref queue reply missing")?
    }
}
impl RunnerLeaseStore for RefQueue {
    fn grant_runner_lease(&self, run: &str, runner: &str, now: i64) -> Result<Grant> {
        let (run, runner) = (run.to_owned(), runner.to_owned());
        self.call(move |q| q.grant_runner_lease(&run, &runner, now))
    }
    fn report_runner_lease(
        &self,
        run: &str,
        report: Report<'_>,
        head: Option<&str>,
        now: i64,
    ) -> Result<Verdict> {
        let (run, runner, generation, head) = (
            run.to_owned(),
            report.runner_id.to_owned(),
            report.generation,
            head.map(str::to_owned),
        );
        self.call(move |q| {
            q.report_runner_lease(
                &run,
                Report {
                    runner_id: &runner,
                    generation,
                },
                head.as_deref(),
                now,
            )
        })
    }
    fn release_runner_lease(&self, run: &str, report: Report<'_>, now: i64) -> Result<Verdict> {
        let (run, runner, generation) = (
            run.to_owned(),
            report.runner_id.to_owned(),
            report.generation,
        );
        self.call(move |q| {
            q.release_runner_lease(
                &run,
                Report {
                    runner_id: &runner,
                    generation,
                },
                now,
            )
        })
    }
    fn lose_runner_lease(&self, run: &str, generation: i64) -> Result<Verdict> {
        let run = run.to_owned();
        self.call(move |q| q.lose_runner_lease(&run, generation))
    }
    fn verify_runner_ref(&self, run: &str, generation: i64, head: &str, now: i64) -> Result<()> {
        let (run, head) = (run.to_owned(), head.to_owned());
        self.call(move |q| q.verify_runner_ref(&run, generation, &head, now))
    }
    fn runner_lease(&self, run: &str) -> Result<Option<RunnerLease>> {
        let run = run.to_owned();
        self.call(move |q| q.runner_lease(&run))
    }
    fn accepted_head(&self, run: &str) -> Result<Option<AcceptedHead>> {
        let run = run.to_owned();
        self.call(move |q| q.accepted_head(&run))
    }
    fn verified_ref(&self, run: &str) -> Result<Option<VerifiedRef>> {
        let run = run.to_owned();
        self.call(move |q| q.verified_ref(&run))
    }
}
impl RunnerTakeoverStore for RefQueue {
    fn runner_ref_ends(&self) -> Result<Vec<End>> {
        self.call(|q| q.runner_ref_ends())
    }
    fn runner_takeovers(&self) -> Result<Vec<Takeover>> {
        self.call(|q| q.runner_takeovers())
    }
    fn runner_registration(
        &self,
        id: &str,
    ) -> Result<Option<crate::domain::runner_registration::Runner>> {
        let id = id.to_owned();
        self.call(move |q| q.runner_registration(&id))
    }
    fn runner_owner(
        &self,
        runner: &crate::domain::runner_registration::Runner,
    ) -> Result<Option<crate::domain::queue_service::Member>> {
        let runner = runner.clone();
        self.call(move |q| q.runner_owner(&runner))
    }
    fn lose_runner_if_due(
        &self,
        run: &str,
        generation: i64,
        now: i64,
    ) -> Result<Option<LossReason>> {
        let run = run.to_owned();
        self.call(move |q| q.lose_runner_if_due(&run, generation, now))
    }
    fn recover_runner(&mut self, lease: &RunnerLease) -> Result<()> {
        let lease = lease.clone();
        self.call(move |q| q.recover_runner(&lease))
    }
}
