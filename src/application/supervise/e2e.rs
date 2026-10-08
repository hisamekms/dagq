//! The runtime's e2e of a run after its review (ADR-t1233-2): a run whose
//! latest validation found that it needs the e2e ([`run_e2e::due`]) runs it
//! on the host, in its worktree at the reviewed commit, before it lands,
//! with the e2e gate of the automatic update ([`RunE2ePort`]): the failed
//! tests rerun once by name, the marks of `.config/e2e-quarantine.toml`
//! read from the landing branch's committed tree, and the host's one e2e
//! at a time. Its output goes to `e2e-<attempt>.log` (and
//! `e2e-<attempt>.rerun.log`) of the run directory.
//!
//! - It passes (flaky tests and tests under a mark included):
//!   `run_e2e_finished` (`outcome: passed`) and the run goes on to land.
//! - Tests still fail after their rerun under no mark: the run is parked
//!   for a resume (`run_e2e_failed`, code `e2e_failed`) that names the
//!   failed tests and the logs.
//! - It could not run or tell anything of the change (it could not start,
//!   an unreadable `[run.env]`, it or the rerun of its failed tests past
//!   the timeout, a rerun that could not start), which is not the change's
//!   fault (ADR-t1233-2 decision 3): `run_e2e_finished` (`outcome:
//!   unavailable`); the run keeps its slot and lease and tries again after
//!   [`RunE2ePort::retry`] ([`run_e2e::RETRY_SECS`]), and from the
//!   [`run_e2e::UNAVAILABLE_ATTENTION`]th in a row the inbox is told
//!   (`attention: true`, `check the e2e host`).
//! - When cmux does not answer, the e2e that need it are not run and the
//!   rest decide (ADR-t2105-1); the tests not run and why are on the `run_e2e_finished` (`skipped`).
//! - The repository has no e2e the runtime knows: `run_e2e_finished`
//!   (`outcome: not_configured`) and the run lands without it.
//!
//! One run of this supervisor runs its e2e at a time; another waits in its
//! slot (`run_e2e_waiting`, once per wait). Across processes (another
//! queue's, the automatic update's gate, `install`) the e2e's lock waits.

use super::*;
use crate::application::e2e_verdict;
use crate::application::install::{E2eOutcome, E2eSettings};
use crate::domain::{EventKind, e2e_quarantine, run_e2e};
use std::collections::HashSet;

/// Runs the e2e of a worktree with the settings.
pub type RunE2e = Arc<dyn Fn(&Path, &E2eSettings) -> Result<E2eOutcome> + Send + Sync>;

/// How the supervisor runs the e2e of a run.
#[derive(Clone)]
pub struct RunE2ePort {
    /// The e2e's settings: its command, timeout, cmux, `[run.env]`,
    /// scratch directory and the host's lock. The log is the run's
    /// and the time zone the supervisor's, set for each e2e.
    pub settings: E2eSettings,
    /// Run the e2e of the worktree with the settings (the gate's
    /// [`crate::application::install::Binaries::e2e`] on this machine).
    pub run: RunE2e,
    /// How long a run waits before its e2e that could not run is tried
    /// again ([`run_e2e::RETRY_SECS`]; tests shorten it).
    pub retry: Duration,
}

/// The e2e stage's state: how it runs the e2e, the runs waiting for it,
/// and when an e2e that could not run is tried again.
pub(super) struct E2eWaits {
    /// Runs the e2e of the runs after their review (ADR-t1233-2).
    port: Option<RunE2ePort>,
    /// The runs whose `run_e2e_waiting` was recorded for the wait now.
    waiting: HashSet<RunId>,
    /// When the e2e of a run that could not run is tried again.
    retry: HashMap<RunId, Instant>,
}

impl E2eWaits {
    pub(super) fn new(port: Option<RunE2ePort>) -> Self {
        Self {
            port,
            waiting: HashSet::new(),
            retry: HashMap::new(),
        }
    }
}

/// The e2e of a run in progress on a thread.
pub(super) struct E2eWatch {
    handle: Option<thread::JoinHandle<Result<E2eOutcome>>>,
    attempt: usize,
    commit: CommitSha,
    settings: E2eSettings,
    requirement: Value,
    started: Instant,
}

impl E2eWatch {
    /// Whether its thread ended (or it was taken already).
    pub(super) fn finished(&self) -> bool {
        self.handle
            .as_ref()
            .is_none_or(thread::JoinHandle::is_finished)
    }
}

impl Supervisor<'_> {
    /// Whether `run`'s e2e could not run and waits to be tried again: a
    /// supervisor that drains or hands off does not wait for it.
    pub(super) fn e2e_retry_pending(&self, run: &TaskRun) -> bool {
        self.e2e
            .retry
            .get(run.id())
            .is_some_and(|at| Instant::now() < *at)
    }

    /// The e2e `run` needs before it lands (ADR-t1233-2), at its head;
    /// `None` when it needs none or ran it for that commit.
    pub(super) fn e2e_due(&self, run: &TaskRun) -> Result<Option<run_e2e::Due>> {
        let Some(commit) = run.result_commit() else {
            return Ok(None);
        };
        let events = self.queue.run_events(run.id())?;
        Ok(run_e2e::due(&events, commit.as_str()))
    }

    /// For a passed run in its slot before it lands: `None` when it lands
    /// now (no e2e due), else the phase it goes on in: waiting in
    /// `AwaitingE2e` (another e2e of this supervisor runs, one that could
    /// not run waits to be tried again, or a handoff starts none) or
    /// running its e2e.
    pub(super) fn e2e_before_landing(&mut self, run: &TaskRun) -> Result<Option<Phase>> {
        let Some(due) = self.e2e_due(run)? else {
            self.e2e.waiting.remove(run.id());
            self.e2e.retry.remove(run.id());
            return Ok(None);
        };
        let commit = run
            .result_commit()
            .context("a run due its e2e has a result commit")?
            .clone();
        let Some(port) = self.e2e.port.clone() else {
            self.queue.record_runtime_event(
                run.id(),
                EventKind::RunE2eFinished,
                json!({
                    "attempt": due.attempt,
                    "commit": commit,
                    "outcome": run_e2e::NOT_CONFIGURED,
                    "requirement": due.requirement,
                }),
            )?;
            info!(run_id = %run.id(), "run {} needs the e2e, but the repository has no e2e the runtime knows; it lands without one", run.id());
            return Ok(None);
        };
        // A handoff starts no e2e: the next process runs it.
        if self.e2e_retry_pending(run) || self.handoff.is_some() {
            return Ok(Some(Phase::AwaitingE2e));
        }
        if self
            .claim
            .slots
            .iter()
            .any(|slot| matches!(slot.phase, Phase::E2e(_)))
        {
            if self.e2e.waiting.insert(run.id().clone()) {
                self.queue.record_runtime_event(
                    run.id(),
                    EventKind::RunE2eWaiting,
                    json!({"attempt": due.attempt, "commit": commit}),
                )?;
                info!(run_id = %run.id(), "run {} waits for the e2e of another run before its own", run.id());
            }
            return Ok(Some(Phase::AwaitingE2e));
        }
        self.e2e.waiting.remove(run.id());
        self.e2e.retry.remove(run.id());
        let worktree = PathBuf::from(run.worktree_path().context("missing worktree")?);
        let run_dir = PathBuf::from(run.run_dir().context("missing run directory")?);
        let now = self.generators.clock.now();
        let settings = E2eSettings {
            log: run_dir.join(format!("e2e-{}.log", due.attempt)),
            utc_offset_secs: (self.utc_offset)(now),
            ..port.settings.clone()
        };
        self.queue.record_runtime_event(
            run.id(),
            EventKind::RunE2eStarted,
            json!({
                "attempt": due.attempt,
                "commit": commit,
                "requirement": due.requirement,
                "log": settings.log,
                "worktree": worktree,
            }),
        )?;
        info!(run_id = %run.id(), "run {} runs its e2e at {commit} before it lands (attempt {})", run.id(), due.attempt);
        // The e2e may not start the sccache server (ADR-t2086-1): a missing
        // one is started here, and the gate looks again and guards it.
        self.ensure_sccache(crate::domain::sccache::CheckReason::BeforeE2e);
        let thread_settings = settings.clone();
        let handle = spawn_traced(move || (port.run)(&worktree, &thread_settings));
        Ok(Some(Phase::E2e(E2eWatch {
            handle: Some(handle),
            attempt: due.attempt,
            commit,
            settings,
            requirement: due.requirement,
            started: Instant::now(),
        })))
    }

    /// Record that the e2e of `run` could not run (`run_e2e_finished`,
    /// `outcome: unavailable`, with `payload` and why), and have the run
    /// wait in its slot to try again; from the
    /// [`run_e2e::UNAVAILABLE_ATTENTION`]th in a row the event carries
    /// `attention: true`.
    fn e2e_unavailable(
        &mut self,
        slot: &mut Slot,
        run: TaskRun,
        mut payload: Value,
        error: String,
    ) -> Result<Step> {
        let retry = self
            .e2e
            .port
            .as_ref()
            .map_or(Duration::from_secs(run_e2e::RETRY_SECS), |port| port.retry);
        let events = self.queue.run_events(run.id())?;
        let in_a_row = run_e2e::unavailable_in_a_row(&events) + 1;
        payload["outcome"] = json!(run_e2e::UNAVAILABLE);
        payload["error"] = json!(error);
        payload["in_a_row"] = json!(in_a_row);
        payload["retry_secs"] = json!(retry.as_secs());
        if in_a_row >= run_e2e::UNAVAILABLE_ATTENTION {
            payload["attention"] = json!(true);
        }
        self.queue
            .record_runtime_event(run.id(), EventKind::RunE2eFinished, payload)?;
        warn!(run_id = %run.id(), "run {}: its e2e could not run ({in_a_row} in a row); trying again in {}s: {error}", run.id(), retry.as_secs());
        self.e2e
            .retry
            .insert(run.id().clone(), Instant::now() + retry);
        slot.run = run;
        slot.transition(
            Phase::AwaitingSlot,
            EventKind::RunE2eFinished.as_str(),
            &*self.queue,
        );
        Ok(Step::Continue)
    }

    /// Act on the e2e `watch` ran for the run of `slot` (see the module).
    pub(super) fn finish_e2e(&mut self, slot: &mut Slot, mut watch: E2eWatch) -> Result<Step> {
        let outcome = match watch.handle.take().map(thread::JoinHandle::join) {
            Some(Ok(outcome)) => outcome,
            Some(Err(_)) => Err(anyhow!("the e2e's thread panicked")),
            None => Err(anyhow!("the e2e was not running")),
        };
        let run = self.queue.run(slot.run.id())?;
        let base = json!({
            "attempt": watch.attempt,
            "commit": watch.commit,
            "requirement": watch.requirement,
            "log": watch.settings.log,
            "secs": watch.started.elapsed().as_secs(),
        });
        let outcome = match outcome {
            Ok(outcome) => outcome,
            Err(error) => {
                return self.e2e_unavailable(slot, run, base, format!("{error:#}"));
            }
        };
        // The e2e ran without RUSTC_WRAPPER (ADR-t2086-1).
        if let Some((port, why)) = &outcome.sccache_wrapper_removed {
            let look = crate::domain::sccache::GuardLook::Unconfirmed {
                port: *port,
                why: why.clone(),
            };
            self.record_wrapper_removed(
                run.id(),
                &look,
                json!({"job": "e2e", "attempt": watch.attempt}),
            );
        }
        // Past its timeout, or a rerun past its timeout or that could not
        // start, the e2e told nothing of the change (ADR-t1233-2 decision
        // 3): it is tried again, not sent back to the worker.
        let rerun_cut = outcome.rerun.as_ref().and_then(|rerun| {
            if rerun.timed_out {
                Some(format!(
                    "the rerun by name of {} did not finish within {}s and was stopped; see {}",
                    rerun.tests.join(", "),
                    watch.settings.timeout.as_secs(),
                    watch.settings.rerun_log().display()
                ))
            } else {
                rerun.error.as_ref().map(|error| {
                    format!(
                        "the rerun by name of {} could not start: {error}",
                        rerun.tests.join(", ")
                    )
                })
            }
        });
        let cut = if outcome.timed_out {
            Some(outcome.failure(&watch.settings))
        } else {
            rerun_cut
        };
        if let Some(why) = cut {
            let mut payload = ran_payload(base, &outcome);
            if let Some(rerun) = &outcome.rerun {
                let mut value = json!({
                    "tests": rerun.tests,
                    "failed": rerun.failed,
                    "timed_out": rerun.timed_out,
                    "secs": rerun.secs,
                    "log": watch.settings.rerun_log(),
                    "cleanup": rerun.cleanup,
                });
                if let Some(error) = &rerun.error {
                    value["error"] = json!(error);
                }
                payload["rerun"] = value;
            }
            return self.e2e_unavailable(slot, run, payload, why);
        }
        // The marks of the landing branch's committed tree, not those of
        // the run's worktree the gate read (ADR-t1233-2 decision 5).
        // The run's own change, from where its branch forked from main.
        let changes = self
            .repository
            .main_head()
            .and_then(|main| {
                self.repository
                    .merge_base(main.as_str(), watch.commit.as_str())
            })
            .and_then(|fork| {
                let fork = fork.unwrap_or_else(|| run.base_commit().clone());
                self.repository
                    .changed_paths(fork.as_str(), watch.commit.as_str())
            });
        // A diff that cannot be read holds every mark back, as a file that
        // cannot be read does.
        let (quarantine, marks_left_out) = match changes {
            Ok(changes) => {
                run_e2e::marks_for_run(self.main_quarantine(&run), run.task_id().as_i64(), &changes)
            }
            Err(error) => {
                warn!(run_id = %run.id(), "run {}: its diff could not be read for the e2e marks, so none holds: {error:#}", run.id());
                (
                    e2e_quarantine::QuarantineFile::Unreadable(format!(
                        "the run's diff could not be read: {error:#}"
                    )),
                    Vec::new(),
                )
            }
        };
        let outcome = E2eOutcome {
            quarantine,
            ..outcome
        };
        let history = self.queue.e2e_gate_events(e2e_verdict::HISTORY)?;
        let verdict = e2e_verdict::judge(
            &outcome,
            &watch.settings,
            &history,
            self.generators.clock.now(),
        );
        let mut payload = ran_payload(base, &outcome);
        if let (Some(payload), Some(fields)) = (payload.as_object_mut(), verdict.fields.as_object())
        {
            payload.extend(fields.clone());
        }
        if !marks_left_out.is_empty() {
            payload["marks_left_out"] = json!(marks_left_out);
        }
        if verdict.passed {
            payload["outcome"] = json!(run_e2e::PASSED);
            self.queue
                .record_runtime_event(run.id(), EventKind::RunE2eFinished, payload)?;
            info!(run_id = %run.id(), "run {}: its e2e passed at {}; it lands next", run.id(), watch.commit);
            slot.run = run;
            slot.transition(
                Phase::AwaitingSlot,
                EventKind::RunE2eFinished.as_str(),
                &*self.queue,
            );
            return Ok(Step::Continue);
        }
        let reason = verdict
            .failure
            .unwrap_or_else(|| outcome.failure(&watch.settings));
        let parked = self
            .queue
            .park_e2e_failed(run.id(), &self.token, &reason, payload)?;
        warn!(run_id = %run.id(), "run {}: its e2e failed, so it waits for a resume instead of landing: {reason}", run.id());
        self.queue.release_lease(run.id(), &self.token)?;
        Ok(Step::Done(Box::new(parked)))
    }
}

/// The `run_e2e_finished` of an e2e that ran: `base` with how it went, and
/// the tests it did not run and why (`skipped`), so a run does not land
/// past them silently (ADR-t1162-1, ADR-t2105-1).
fn ran_payload(base: Value, outcome: &E2eOutcome) -> Value {
    let mut payload = base;
    payload["secs"] = json!(outcome.secs);
    payload["lock_wait_secs"] = json!(outcome.lock_wait_secs);
    payload["timed_out"] = json!(outcome.timed_out);
    payload["failed_tests"] = json!(outcome.failed_tests);
    payload["cleanup"] = outcome.cleanup.clone();
    if let Some(skipped) = &outcome.skipped {
        payload["skipped"] = skipped.to_json();
    }
    payload
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::application::install::E2eSkip;

    /// The tests an e2e did not run for want of cmux are named with why on
    /// the run's `run_e2e_finished`; one that ran them all names none.
    #[test]
    fn the_e2e_event_names_the_tests_not_run() {
        let skipped = E2eSkip::cmux("`cmux ping` failed: Access denied");
        let outcome = E2eOutcome {
            passed: true,
            secs: 90,
            cleanup: json!({"removed": false}),
            skipped: Some(skipped.clone()),
            ..Default::default()
        };
        let payload = ran_payload(json!({"attempt": 1}), &outcome);
        assert_eq!(payload["attempt"], 1);
        assert_eq!(payload["secs"], 90);
        assert_eq!(payload["cleanup"], json!({"removed": false}));
        assert_eq!(payload["skipped"], skipped.to_json());
        let all = E2eOutcome {
            skipped: None,
            ..outcome
        };
        assert!(ran_payload(json!({}), &all).get("skipped").is_none());
    }
}
