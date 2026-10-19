//! The review's judgment of an `already_satisfied` claim (ADR-t1818-1
//! decision 4): once the claim's verification passed on its snapshot, one
//! headless review job judges, on a checkout of that same head made for
//! it, whether main there meets the task's acceptance with no change. No
//! review subagent and no review program runs: there is no diff for them.
//! The verdict is read through the claim's own entry
//! ([`ClaimJudgment::parse`]), a job that gives none runs once more as a
//! run review's does, and a provider that cannot be used holds, moves or
//! waits as it does for a run review's job ([`Supervisor::review_job_failed`]).
//! Whatever it judged, the claim then goes to a person in the
//! `approve_close` ask with the review's reasons, recommendation and
//! confidence: the runtime closes nothing on the review yet. The verdict is
//! recorded and the ask opened at the job's request (`requested_by`); a job
//! that gave no judgment asks as a failed job (`recovery_failed`), at
//! nobody's.

use super::landing::{AfterUnusable, ReviewRoute, StartedReview, after_unusable};
use super::provider::ReviewJobFailure;
use super::*;
use crate::application::job_start_failure;
use crate::application::prompt::{REVIEW_ACCESS, claim_review_prompt};
use crate::domain::ActorContext;
use crate::domain::actor_model::ActorLaunch;
use crate::domain::headless_job::{JobKind, JobStop};
use crate::domain::review_jobs::{self, JobMiss};
use crate::domain::satisfied::{self, SnapshotVerification};
use crate::domain::satisfied_judgment::{self, ClaimJudgment};

/// A claim's judgment in the run's slot: its `attempt` (from 1, by the
/// run's `run_satisfied_review_started`), the verification that passed on
/// the head it judges, its prompt and its one job.
pub(super) struct ClaimReviewWatch {
    pub(super) attempt: usize,
    pub(super) verification: SnapshotVerification,
    prompt: String,
    /// Whether the job ran once more already, or starts as its run once
    /// more: kept across its waits, so a wait never gives it a second one.
    again: bool,
    state: ClaimJob,
    /// Whether a park withheld where it stands, which the park's later
    /// looks do not record again; a step after the fence went resets it.
    pub(super) withheld: bool,
}

/// Where the one job of a claim's judgment stands.
enum ClaimJob {
    /// It waits to start (for its provider, or as its run once more), why
    /// it waited last noted.
    Waiting(Option<String>),
    Running {
        job: Box<HeadlessJob>,
        switchable: bool,
    },
    /// It printed a judgment, not acted on yet.
    Judged(ClaimJudgment),
    /// It gave none and does not run again: why.
    Failed(String),
}

impl ClaimReviewWatch {
    /// How many of its jobs run: its job, or none.
    pub(super) const fn running(&self) -> usize {
        match self.state {
            ClaimJob::Running { .. } => 1,
            _ => 0,
        }
    }

    /// Set what it found aside once a park withheld it: its job waits to
    /// start afresh, so a judgment whose park did not happen after all is
    /// made again rather than the withheld one applied.
    pub(super) fn set_aside(&mut self) {
        self.withheld = true;
        self.again = false;
        self.state = ClaimJob::Waiting(None);
    }

    /// Stop its job, if it runs, recording its Execution.
    pub(super) fn abandon(&mut self) {
        if let ClaimJob::Running { job, .. } = &mut self.state {
            job.abandon();
        }
    }
}

impl replan_park::ParkedReviewJobs for ClaimReviewWatch {
    fn reap(&mut self, supervisor: &mut Supervisor<'_>, run: &TaskRun) -> Result<()> {
        supervisor.reap_claim_review(run, self)
    }

    fn running(&self) -> usize {
        Self::running(self)
    }

    fn waiting(&self) -> usize {
        usize::from(matches!(self.state, ClaimJob::Waiting(_)))
    }

    /// Never: a judgment that ended opens an ask (or runs again), which
    /// the park withholds.
    fn judged_once_ended(&self) -> bool {
        false
    }

    fn withheld(&self) -> Value {
        let mut withheld = json!({
            "what": "satisfied_judgment",
            "attempt": self.attempt,
            "head": self.verification.head,
        });
        match &self.state {
            ClaimJob::Judged(judgment) => {
                withheld["result"] = json!({"verdict": judgment.verdict.as_str(),
                    "confidence": judgment.confidence.map(|c| c.as_str().to_owned())});
            }
            ClaimJob::Failed(why) => withheld["result"] = json!({"error": why}),
            ClaimJob::Waiting(_) | ClaimJob::Running { .. } => {}
        }
        withheld
    }
}

/// The actor of the job of judgment `attempt` of `run`'s claim, which its
/// verdict's events name as `requested_by`: a review job, apart from the
/// run review's attempts.
pub(super) fn claim_review_job(run: &RunId, attempt: usize) -> ActorContext {
    ActorContext::review_job(run, format_args!("satisfied-{attempt}"))
}

/// The stem in the run directory of the output of judgment `attempt`'s
/// job, or of its run once more (`again`).
fn claim_output(attempt: usize, again: bool) -> String {
    format!(
        "satisfied-review-{attempt}{}",
        if again { "-again" } else { "" }
    )
}

/// What a job of a claim's judgment that failed as `failure` leads to,
/// given whether it ran once more already (`again`) and whether it is run
/// again at all (`retry`, `[review]`'s retry of an unreadable review; a
/// person reviews a claim no provider can run, `manual`): Claude's login or
/// usage limit and another provider that cannot be used make it wait for
/// its route (no failure); a provider found unusable that nothing moves
/// fails it at once; any other failure runs it once more as a run review's
/// job ([`review_jobs::runs_again`]), then fails it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ClaimMiss {
    Wait,
    RunAgain,
    Fail,
}

pub(super) fn claim_miss(
    failure: ReviewJobFailure,
    (retry, again, manual): (bool, bool, bool),
) -> ClaimMiss {
    match failure {
        ReviewJobFailure::Walled(_) | ReviewJobFailure::Moved(..) => ClaimMiss::Wait,
        ReviewJobFailure::Unusable => ClaimMiss::Fail,
        ReviewJobFailure::Failed if review_jobs::runs_again(retry, again, false, manual) => {
            ClaimMiss::RunAgain
        }
        ReviewJobFailure::Failed => ClaimMiss::Fail,
    }
}

impl Supervisor<'_> {
    /// Begin the review's judgment of `run`'s claim, whose `verification`
    /// just passed on its snapshot: its prompt (written to the run
    /// directory beside its output), `run_satisfied_review_started`, and
    /// its job waiting to start in the slot's next step. The judgment's
    /// tree is checked out at the job's start.
    pub(super) fn begin_claim_review(
        &mut self,
        run: &TaskRun,
        verification: SnapshotVerification,
    ) -> Result<Phase> {
        let attempt = satisfied::judgments(&self.queue.run_events(run.id())?) + 1;
        let run_dir = PathBuf::from(run.run_dir().context("missing run directory")?);
        let task = self.queue.show(run.task_id())?.task;
        let receipt_path = run.receipt_path().unwrap_or_default();
        let receipt = self
            .files
            .read_to_string(Path::new(receipt_path))
            .ok()
            .and_then(|text| Receipt::parse(&text).ok());
        let tree = integration::satisfied_scratch(&self.layout.satisfied_dir, run.id());
        let fitted = claim_review_prompt(
            &task,
            run,
            (receipt.as_ref(), receipt_path),
            &verification,
            &tree,
        )
        .with_language(self.verifier.language().as_ref());
        self.files.write(
            &run_dir.join(format!("satisfied-review-prompt-{attempt}.txt")),
            fitted.text.as_bytes(),
        )?;
        self.queue.record_runtime_event(
            run.id(),
            EventKind::RunSatisfiedReviewStarted,
            json!({"attempt": attempt, "head": verification.head, "prompt_bytes": fitted.bytes}),
        )?;
        info!(run_id = %run.id(), "run {}: its already_satisfied claim passed on main {}; the review judges it there (judgment {attempt})", run.id(), verification.head);
        Ok(Phase::SatisfiedReview(ClaimReviewWatch {
            attempt,
            verification,
            prompt: fitted.text,
            again: false,
            state: ClaimJob::Waiting(None),
            withheld: false,
        }))
    }

    /// One step of `run`'s claim judgment in `watch`: reap its job, start
    /// it while it waits and a provider can run it, and once it judged or
    /// failed, ask the person ([`Self::claim_judged`],
    /// [`Self::claim_unjudged`]): the run then rests, given back.
    pub(super) fn tend_claim_review(
        &mut self,
        run: &TaskRun,
        watch: &mut ClaimReviewWatch,
    ) -> Result<Option<TaskRun>> {
        watch.withheld = false;
        self.reap_claim_review(run, watch)?;
        if matches!(watch.state, ClaimJob::Waiting(_)) {
            self.start_claim_review(run, watch)?;
        }
        match std::mem::replace(&mut watch.state, ClaimJob::Waiting(None)) {
            ClaimJob::Judged(judgment) => self.claim_judged(run, watch, &judgment).map(Some),
            ClaimJob::Failed(why) => self.claim_unjudged(run, watch, &why).map(Some),
            state => {
                watch.state = state;
                Ok(None)
            }
        }
    }

    /// Reap the job of `run`'s claim judgment in `watch` once its process
    /// ended, recording `run_satisfied_review_job_finished`: its judgment is
    /// kept, or it waits again (for its provider, or as its run once more),
    /// or it failed. Nothing starts and nothing is acted on: what the
    /// park of a replanning or a hold does before it withholds the result.
    pub(super) fn reap_claim_review(
        &mut self,
        run: &TaskRun,
        watch: &mut ClaimReviewWatch,
    ) -> Result<()> {
        let ClaimJob::Running { job: running, .. } = &mut watch.state else {
            return Ok(());
        };
        let agent = self.job_agent(running.provider()).with_context(|| {
            format!("no {} runs on this supervisor", running.provider().as_str())
        })?;
        let Some(end) = running.poll_end(&*self.files, agent)? else {
            return Ok(());
        };
        let ClaimJob::Running {
            job: ended,
            switchable,
        } = std::mem::replace(&mut watch.state, ClaimJob::Waiting(None))
        else {
            unreachable!("the job runs");
        };
        let duration_secs = ended.elapsed().as_secs();
        let stdout = self
            .files
            .read_to_string(ended.stdout())
            .unwrap_or_default();
        let output = claim_output(watch.attempt, watch.again);
        let mut finished = json!({
            "attempt": watch.attempt,
            "again": watch.again,
            "provider": ended.provider(),
            "duration_secs": duration_secs,
            "output": &output,
        });
        crate::domain::headless_job::JobSession::record_execution(
            agent.job_session(&stdout, ended.started_at()).as_ref(),
            &mut finished,
        );
        let (miss, error) = match end {
            Ok(reply) => match ClaimJudgment::parse(&reply) {
                Ok(judgment) => {
                    finished["outcome"] = json!("answered");
                    finished["verdict"] = json!(judgment.verdict.as_str());
                    info!(run_id = %run.id(), "run {} judgment {}: the review judged {}", run.id(), watch.attempt, judgment.verdict.as_str());
                    watch.state = ClaimJob::Judged(judgment);
                    return self.record_claim_job_finished(run, finished);
                }
                Err(error) => (JobMiss::Unreadable, error),
            },
            Err(failed) => (
                match failed.stop() {
                    JobStop::Exited => JobMiss::Exited,
                    JobStop::TimedOut => JobMiss::TimedOut,
                },
                failed.into_error(),
            ),
        };
        let failure = self.review_job_failed(
            &ended,
            (&error, &stdout),
            &HoldJob::Review(run.id().clone()),
            (None, switchable),
        );
        finished["error"] = json!(error);
        match failure {
            ReviewJobFailure::Walled(wall) => {
                info!(run_id = %run.id(), "run {} judgment {}: its job stopped at the {} wall; it waits for the hold ask", run.id(), watch.attempt, wall.as_str());
                finished["outcome"] = json!("held");
            }
            ReviewJobFailure::Moved(provider, reason) => {
                warn!(run_id = %run.id(), error = %error, "run {} judgment {}: {} cannot be used ({}); its job starts again where its route sends it", run.id(), watch.attempt, provider.as_str(), reason.as_str());
                finished["outcome"] = json!("held");
                finished["switch_reason"] = json!(reason.as_str());
            }
            ReviewJobFailure::Unusable | ReviewJobFailure::Failed => {
                finished["outcome"] = json!(miss.as_str());
                let again = self.claim_job_missed(run, watch, failure, &error, Some(output));
                finished["runs_again"] = json!(again);
            }
        }
        self.record_claim_job_finished(run, finished)
    }

    /// Put the job of `watch`, which missed as `failure` with `error`, back
    /// to wait as its run once more, or fail it with why and where the
    /// output of its run named `output` is, if any; whether it runs again
    /// ([`claim_miss`]).
    fn claim_job_missed(
        &mut self,
        run: &TaskRun,
        watch: &mut ClaimReviewWatch,
        failure: ReviewJobFailure,
        error: &str,
        output: Option<String>,
    ) -> bool {
        let manual = matches!(self.review_route(), ReviewRoute::Manual(_));
        let retry = self.landing.retry_unreadable_review;
        match claim_miss(failure, (retry, watch.again, manual)) {
            ClaimMiss::Wait => {
                watch.state = ClaimJob::Waiting(None);
                false
            }
            ClaimMiss::RunAgain => {
                warn!(run_id = %run.id(), error = %error, "run {} judgment {}: its job gave no judgment: {error}; it runs once more", run.id(), watch.attempt);
                watch.again = true;
                watch.state = ClaimJob::Waiting(None);
                true
            }
            ClaimMiss::Fail => {
                let why = match (run.run_dir(), output) {
                    (Some(dir), Some(output)) => format!(
                        "{error}; its output is in {}.out and .err",
                        Path::new(dir).join(output).display()
                    ),
                    _ => error.to_owned(),
                };
                watch.state = ClaimJob::Failed(why);
                false
            }
        }
    }

    fn record_claim_job_finished(&mut self, run: &TaskRun, finished: Value) -> Result<()> {
        self.queue.record_runtime_event(
            run.id(),
            EventKind::RunSatisfiedReviewJobFinished,
            finished,
        )?;
        Ok(())
    }

    /// Start the waiting job of `run`'s claim judgment where
    /// [`Self::review_route`] sends a run review, within the limit of
    /// review jobs at once the run reviews share: it waits while the route
    /// waits, and fails when no provider is left (`--no-claude`). A start
    /// its provider failed holds, moves or waits as a run review's job
    /// does (ADR-t1063-1 decisions 4 and 5, ADR-t1857-1); any other failure
    /// to start is a miss ([`Self::claim_job_missed`]).
    fn start_claim_review(&mut self, run: &TaskRun, watch: &mut ClaimReviewWatch) -> Result<()> {
        let limit = self.review_jobs.concurrency();
        if review_jobs::startable(limit, self.review_jobs_running(), 1) == 0 {
            note_claim_wait(
                run,
                watch,
                format!("{limit} agent jobs of the reviews run at once"),
            );
            return Ok(());
        }
        let (mut launch, mut switchable) = match self.review_route() {
            ReviewRoute::Wait(why) => {
                note_claim_wait(run, watch, why);
                return Ok(());
            }
            ReviewRoute::Manual(error) => {
                warn!(run_id = %run.id(), "run {} judgment {}: it cannot run: {error}", run.id(), watch.attempt);
                watch.state = ClaimJob::Failed(error);
                return Ok(());
            }
            ReviewRoute::Start(launch, switchable) => (launch, switchable),
        };
        loop {
            let output = claim_output(watch.attempt, watch.again);
            let session_id =
                (launch.provider == Provider::Claude).then(|| self.generators.ids.uuid());
            let error = match self.spawn_claim_review(
                run,
                watch,
                &output,
                session_id.as_deref(),
                &launch,
            ) {
                Ok(Ok((child, stdout, stderr))) => {
                    let started = self.headless_job(
                        "review",
                        child,
                        stdout,
                        stderr,
                        JobSubject {
                            provider: launch.provider,
                            label: Some(
                                crate::domain::headless_job::SATISFIED_REVIEW_LABEL.to_owned(),
                            ),
                            ..JobSubject::review(JobKind::Agent, run.id(), watch.attempt)
                        },
                    );
                    // Watched before its start is recorded, so a failed
                    // record leaves no job running unwatched.
                    watch.state = ClaimJob::Running {
                        job: Box::new(started),
                        switchable,
                    };
                    self.queue.record_runtime_event(
                        run.id(),
                        EventKind::RunSatisfiedReviewJobStarted,
                        json!({
                            "attempt": watch.attempt,
                            "again": watch.again,
                            "launch": launch.to_value(),
                            "session_id": session_id,
                            "output": output,
                        }),
                    )?;
                    info!(run_id = %run.id(), "run {} judgment {}: its job started on {}", run.id(), watch.attempt, launch.provider.as_str());
                    return Ok(());
                }
                Ok(Err(started)) => {
                    let error = format!("it could not start: {started:#}");
                    if let Some((provider, reason)) = self.job_provider_failed(
                        launch.provider,
                        job_start_failure(&started),
                        (&error, &error),
                        &HoldJob::Review(run.id().clone()),
                        switchable,
                    ) {
                        let next = after_unusable(self.review_route(), provider);
                        if !matches!(next, AfterUnusable::Fail) {
                            warn!(run_id = %run.id(), error = %error, "run {} judgment {}: {} cannot be used ({})", run.id(), watch.attempt, provider.as_str(), reason.as_str());
                            self.record_claim_job_finished(
                                run,
                                json!({
                                    "attempt": watch.attempt,
                                    "again": watch.again,
                                    "provider": provider,
                                    "outcome": "held",
                                    "error": error,
                                    "switch_reason": reason.as_str(),
                                    "duration_secs": 0,
                                    "output": output,
                                }),
                            )?;
                        }
                        match next {
                            AfterUnusable::Start(next, next_switchable) => {
                                launch = next;
                                switchable = next_switchable;
                                continue;
                            }
                            AfterUnusable::Wait(why) => {
                                note_claim_wait(run, watch, why);
                                return Ok(());
                            }
                            AfterUnusable::Fail => {
                                return self.claim_start_missed(
                                    run,
                                    watch,
                                    ReviewJobFailure::Unusable,
                                    &error,
                                );
                            }
                        }
                    }
                    error
                }
                // Its own preparation failed: no provider was tried.
                Err(prepared) => format!("it could not start: {prepared:#}"),
            };
            return self.claim_start_missed(run, watch, ReviewJobFailure::Failed, &error);
        }
    }

    /// Record that the job of `watch` could not start with `error`, and run
    /// it once more or fail it ([`Self::claim_job_missed`]).
    fn claim_start_missed(
        &mut self,
        run: &TaskRun,
        watch: &mut ClaimReviewWatch,
        failure: ReviewJobFailure,
        error: &str,
    ) -> Result<()> {
        warn!(run_id = %run.id(), error = %error, "run {} judgment {}: its job {error}", run.id(), watch.attempt);
        let mut finished = json!({
            "attempt": watch.attempt,
            "again": watch.again,
            "outcome": JobMiss::StartFailed.as_str(),
            "error": error,
            "duration_secs": 0,
            "output": claim_output(watch.attempt, watch.again),
        });
        // The output named on a failure is its earlier run's, if any.
        let earlier = watch.again.then(|| claim_output(watch.attempt, false));
        let again = self.claim_job_missed(run, watch, failure, error, earlier);
        finished["runs_again"] = json!(again);
        self.record_claim_job_finished(run, finished)
    }

    /// Start the job of `run`'s claim judgment writing to `output` (its
    /// stem in the run directory) on `launch`'s provider: the review's
    /// job, read-only, in the judgment's tree, the run's scratch worktree
    /// of the queue's `satisfied/` made again at the snapshot's head
    /// without trusting what is there ([`integration::fresh_satisfied_scratch`]),
    /// never the run's own worktree. The outer error is the job's own
    /// preparation, the inner one the start of its provider's process.
    fn spawn_claim_review(
        &mut self,
        run: &TaskRun,
        watch: &ClaimReviewWatch,
        output: &str,
        session_id: Option<&str>,
        launch: &ActorLaunch,
    ) -> Result<Result<StartedReview>> {
        let run_dir = PathBuf::from(run.run_dir().context("missing run directory")?);
        let stdout = run_dir.join(format!("{output}.out"));
        let stderr = run_dir.join(format!("{output}.err"));
        let tree = integration::fresh_satisfied_scratch(
            &*self.repository,
            &self.layout.satisfied_dir,
            run.id(),
            &watch.verification.head,
        )?;
        // The repository's [run.env] reaches it as it reaches a run's
        // review (ADR-0023 decision 3), and it may not start the sccache
        // server either (ADR-t2086-1).
        let mut env = self.verifier.run_env(&run_dir)?;
        let look = self.sccache_look(crate::domain::sccache::CheckReason::BeforeReview, &run_dir);
        let named = crate::domain::sccache::SccacheTarget::of_pairs(&env).is_some();
        let without_env = look.apply(&mut env);
        if named {
            self.record_wrapper_removed(
                run.id(),
                &look,
                json!({"job": "satisfied_review", "attempt": watch.attempt}),
            );
        }
        let started = (|| {
            let agent = self.job_agent(launch.provider).with_context(|| {
                format!("no {} runs on this supervisor", launch.provider.as_str())
            })?;
            self.actors_on(agent)
                .spawn(
                    ActorExecutionSpec::new(
                        claim_review_job(run.id(), watch.attempt),
                        WorkspaceAccess::Read(tree.clone()),
                        ActorProgram::Headless {
                            program: HeadlessProgram::Review {
                                run,
                                tree: &tree,
                                prompt: &watch.prompt,
                                access: REVIEW_ACCESS,
                            },
                            session_id,
                            launch: Some(launch),
                            without_mcp: false,
                            env,
                            without_env,
                            streams: Streams::Files {
                                stdout: &stdout,
                                stderr: &stderr,
                            },
                        },
                    )
                    .with_timeout(self.review_job_timeout(JobKind::Agent, launch.provider)),
                )
                .context("start the claim's review")?
                .process()
        })();
        Ok(started.map(|child| (child, stdout, stderr)))
    }

    /// The review judged `run`'s claim as `judgment`: the `approve_close`
    /// ask opened (reason `scope`) and the judgment recorded as
    /// `run_satisfied_judged`, with its reasons, recommendation and confidence, both at
    /// the job's request ([`Self::for_job`]); then the judgment's tree goes
    /// and the run rests, given back. A `high` pass is asked too.
    fn claim_judged(
        &mut self,
        run: &TaskRun,
        watch: &ClaimReviewWatch,
        judgment: &ClaimJudgment,
    ) -> Result<TaskRun> {
        let job = claim_review_job(run.id(), watch.attempt);
        let (task, summary) = self.claim_of(run)?;
        let verification = &watch.verification;
        // The ask first: the claim rests from its judgment's record on, so
        // it never rests without one.
        let ask = self.for_job(&job, |sv| {
            let ask = sv.open_close_ask(
                run,
                (
                    satisfied_judgment::judged_question(&task, &summary, verification, judgment),
                    &satisfied::CLOSE_OPTIONS,
                ),
                (
                    AskReason::Scope,
                    judgment.recommendation.map(|r| r.as_str().to_owned()),
                    judgment.confidence,
                ),
            )?;
            sv.queue.record_runtime_event(
                run.id(),
                EventKind::RunSatisfiedJudged,
                satisfied_judgment::judged_payload(watch.attempt, &verification.head, Ok(judgment)),
            )?;
            Ok(ask)
        })?;
        info!(run_id = %run.id(), "run {}: the review judged its already_satisfied claim on main {} {}; it waits for a person in ask {}", run.id(), verification.head, judgment.verdict.as_str(), ask["id"]);
        self.rest_after_judgment(run)
    }

    /// The job of `run`'s claim judgment gave no judgment (`why`): recorded
    /// as `run_satisfied_judged` with the error and the `approve_close` ask
    /// opened as a failed job's (`recovery_failed`), at nobody's request;
    /// then the run rests as after a judgment.
    fn claim_unjudged(
        &mut self,
        run: &TaskRun,
        watch: &ClaimReviewWatch,
        why: &str,
    ) -> Result<TaskRun> {
        let (task, summary) = self.claim_of(run)?;
        let verification = &watch.verification;
        let ask = self.open_close_ask(
            run,
            (
                satisfied_judgment::unjudged_question(&task, &summary, verification, why),
                &satisfied::CLOSE_OPTIONS,
            ),
            (AskReason::RecoveryFailed, None, None),
        )?;
        self.queue.record_runtime_event(
            run.id(),
            EventKind::RunSatisfiedJudged,
            satisfied_judgment::judged_payload(watch.attempt, &verification.head, Err(why)),
        )?;
        warn!(run_id = %run.id(), error = %why, "run {}: the review gave no judgment of its already_satisfied claim: {why}; it waits for a person in ask {}", run.id(), ask["id"]);
        self.rest_after_judgment(run)
    }

    /// The task of `run`'s claim and its receipt's summary, for its ask.
    fn claim_of(&mut self, run: &TaskRun) -> Result<(String, String)> {
        let task = self.queue.show(run.task_id())?.task;
        Ok((task.id().to_string(), self.receipt_summary(run)))
    }

    /// Remove the judgment's tree, give the lease of `run` back and read
    /// it: it rests on its ask.
    fn rest_after_judgment(&mut self, run: &TaskRun) -> Result<TaskRun> {
        self.remove_claim_tree(run);
        self.queue
            .release_lease(run.id(), self.registration.token())?;
        self.queue.run(run.id())
    }

    /// Remove `run`'s judgment tree, the scratch worktree its verification
    /// used too; one that cannot be removed is only noted, as the
    /// verification's: the next rebuild or the cleanup of the ended run
    /// removes it.
    pub(super) fn remove_claim_tree(&self, run: &TaskRun) {
        let tree = integration::satisfied_scratch(&self.layout.satisfied_dir, run.id());
        if let Err(error) = self.repository.remove_scratch(&tree) {
            warn!(run_id = %run.id(), "run {}: the judgment's tree {} could not be removed: {error:#}", run.id(), tree.display());
        }
    }
}

/// Note why the job of `watch` waits, once for each reason.
fn note_claim_wait(run: &TaskRun, watch: &mut ClaimReviewWatch, why: String) {
    if let ClaimJob::Waiting(noted) = &watch.state
        && noted.as_deref() == Some(why.as_str())
    {
        return;
    }
    info!(run_id = %run.id(), "run {} judgment {}: its job waits: {why}", run.id(), watch.attempt);
    watch.state = ClaimJob::Waiting(Some(why));
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::provider_switch::SwitchReason;
    use crate::domain::queue_hold::Wall;

    /// A provider that cannot be used is no failure of the claim's
    /// judgment: Claude's login or usage limit, and another provider that
    /// is held and moved, make the job wait for its route, never running
    /// it once more nor failing it to the person. A provider found unusable
    /// that nothing moves fails it at once; any other failure runs it once
    /// more, as a run review's job, then fails it, and not at all when the
    /// retry is off or no provider is left.
    #[test]
    fn a_claims_job_waits_for_its_provider_runs_once_more_or_fails() {
        for failure in [
            ReviewJobFailure::Walled(Wall::Authentication),
            ReviewJobFailure::Walled(Wall::UsageLimit),
            ReviewJobFailure::Moved(Provider::Codex, SwitchReason::UsageLimit),
        ] {
            for again in [false, true] {
                assert_eq!(
                    claim_miss(failure, (true, again, false)),
                    ClaimMiss::Wait,
                    "{failure:?}"
                );
            }
        }
        assert_eq!(
            claim_miss(ReviewJobFailure::Unusable, (true, false, false)),
            ClaimMiss::Fail
        );
        assert_eq!(
            claim_miss(ReviewJobFailure::Failed, (true, false, false)),
            ClaimMiss::RunAgain
        );
        for (retry, again, manual) in [
            (true, true, false),
            (false, false, false),
            (true, false, true),
        ] {
            assert_eq!(
                claim_miss(ReviewJobFailure::Failed, (retry, again, manual)),
                ClaimMiss::Fail,
                "{retry} {again} {manual}"
            );
        }
    }

    fn watch(state: ClaimJob) -> ClaimReviewWatch {
        ClaimReviewWatch {
            attempt: 2,
            verification: SnapshotVerification {
                head: CommitSha::parse("a".repeat(40), "test").unwrap(),
                commands: 2,
                failure: None,
            },
            prompt: String::new(),
            again: false,
            state,
            withheld: false,
        }
    }

    /// The park of a replanning or a hold takes a claim's judgment through
    /// the review jobs' shared part: a job that waits to start is withheld
    /// and never started, and a judgment that ended, judged or not, is
    /// withheld rather than acted on (no ask), with what it found; the
    /// park's own part (here a hold's) goes in front of it.
    #[test]
    fn a_claims_judgment_at_the_park_is_withheld_whatever_it_found() {
        use replan_park::{JobsPark, ParkedReviewJobs, park_review_jobs};
        let head = "a".repeat(40);
        let waiting = watch(ClaimJob::Waiting(Some("the hold ask".into())));
        assert_eq!(
            (waiting.running(), ParkedReviewJobs::waiting(&waiting)),
            (0, 1)
        );
        assert_eq!(
            park_review_jobs(&waiting),
            JobsPark::Withheld(json!({"what": "satisfied_judgment", "attempt": 2,
                "head": head, "waiting": 1}))
        );
        let judged = watch(ClaimJob::Judged(
            ClaimJudgment::parse(
                r#"{"verdict":"pass","reasons":[],"summary":"s","confidence":"high"}"#,
            )
            .unwrap(),
        ));
        assert_eq!(
            park_review_jobs(&judged),
            JobsPark::Withheld(json!({"what": "satisfied_judgment", "attempt": 2,
                "head": head, "result": {"verdict": "pass", "confidence": "high"}, "waiting": 0}))
        );
        let failed = watch(ClaimJob::Failed("no JSON".into()));
        assert_eq!(
            park_review_jobs(&failed),
            JobsPark::Withheld(json!({"what": "satisfied_judgment", "attempt": 2,
                "head": head, "result": {"error": "no JSON"}, "waiting": 0}))
        );
        // A hold's park records it as its own, with the hold in front.
        let hold = replan_park::ParkFor::Hold(crate::domain::task_hold::TaskHold {
            id: crate::domain::HoldId::new(4),
            task_id: TaskId::new(1),
            run_id: None,
            reason: "wait".into(),
            until: None,
            by_role: "user".into(),
            by_id: "user".into(),
            created_at: 0,
            settled_at: None,
        });
        let (kind, base) = hold.withheld();
        assert_eq!(kind, EventKind::HoldResultWithheld);
        let JobsPark::Withheld(payload) = park_review_jobs(&failed) else {
            panic!("withheld");
        };
        assert_eq!(
            replan_park::withheld_payload(&base, payload),
            json!({"hold_id": 4, "what": "satisfied_judgment", "attempt": 2,
                "head": head, "result": {"error": "no JSON"}, "waiting": 0})
        );
        // Set aside, it waits to start afresh: a fence that lifts before
        // the park judges it again.
        let mut aside = watch(ClaimJob::Failed("no JSON".into()));
        aside.again = true;
        aside.set_aside();
        assert!(aside.withheld && !aside.again);
        assert!(matches!(aside.state, ClaimJob::Waiting(None)));
        assert_eq!(
            claim_review_job(&RunId::new("run-1").unwrap(), 2).actor_id(),
            "review-job:run-1:satisfied-2"
        );
    }
}
