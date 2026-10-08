//! The review of an accepted run and its landing: the headless review,
//! its verdict, the conflict precheck, the `approve_landing` ask and the
//! landing itself (ADR-0023, ADR-0027).

use super::*;
use crate::application::job_start_failure;
use crate::application::prompt::{PromptBytes, REVIEW_ACCESS};
use crate::application::review::{
    SubagentSnapshot, review_range_at, review_subagents_prompt, snapshot_subagents,
};
use crate::domain::ActorContext;
use crate::domain::AskConfidence;
use crate::domain::EventKind;
use crate::domain::LandingAnswer;
use crate::domain::RecoveredLanding;
use crate::domain::actor_model::{ActorLaunch, ModelRole};
use crate::domain::concern::{
    self, ConcernDecision, ConcernReason, EscalatedBecause, LandingRecommendation,
};
use crate::domain::headless_job::JobSession;
use crate::domain::provider_switch::SwitchReason;
use crate::domain::review_reason;
use crate::domain::review_subagents::{AgentDefinition, Destination, VerdictRoute};
use crate::domain::worker_model::{self, Escalation};

/// The answer the supervisor closes an earlier, unclosed `approve_landing`
/// ask of a run with when a later review of the run asks again: it failed
/// (task 328) or did not pass (task 425).
const STALE_LANDING_ASK_CLOSED: &str =
    "a later review of the run asks again; closed by the runtime";

/// How the error of a review no agent ran begins when `--no-claude`
/// leaves no provider for it: `review_failed` records it as
/// `provider_disabled` and its ask says no review agent ran.
pub(super) const REVIEW_PROVIDER_DISABLED: &str =
    "provider_disabled: Claude is disabled by --no-claude";

/// How the error of a review begins when no provider that can be used can
/// run its required subagents (ADR-t1453-1 decision 8).
pub(super) const SUBAGENTS_UNSUPPORTED: &str = "subagents_unsupported";

/// Why a review that requires the subagents `required` did not start on
/// `provider`, which cannot run them, nor on another provider.
pub(super) fn subagents_unsupported(provider: Provider, required: &[String]) -> String {
    format!(
        "{SUBAGENTS_UNSUPPORTED}: the review requires the subagents {}, which {} cannot run, and no other provider that can run them can be used",
        required.join(", "),
        provider.as_str()
    )
}

/// The launch of a review that requires the subagents `required`
/// (ADR-t1453-1 decision 8), given whether a provider's agent runs review
/// subagents (`runs`) and whether it can be used now (`usable`): `launch`
/// when its provider runs them; else the other provider's when that one
/// runs the review role, can be used and runs them, with why it was
/// switched (no hold: the provider itself can be used); else why neither
/// can, for the person.
pub(super) fn subagent_launch_of(
    launch: ActorLaunch,
    required: &[String],
    runs: impl Fn(Provider) -> bool,
    usable: impl Fn(Provider) -> bool,
) -> std::result::Result<ActorLaunch, String> {
    if runs(launch.provider) {
        return Ok(launch);
    }
    let other = launch.provider.other();
    if crate::domain::actor_model::runs_on(ModelRole::Review, other) && usable(other) && runs(other)
    {
        return Ok(launch.switched(other, SwitchReason::SubagentsUnsupported));
    }
    Err(subagents_unsupported(launch.provider, required))
}

/// A review job that started: its process and its stdout and stderr.
type StartedReview = (Box<dyn Spawned>, PathBuf, PathBuf);

/// Where the next review of a run goes (ADR-t1063-1 decisions 4 and 5,
/// ADR-t1207-1).
pub(super) enum ReviewRoute {
    /// Start on this launch; `true` when `[roles.review]` names its
    /// provider, so a provider that cannot be used moves it to the other.
    Start(ActorLaunch, bool),
    /// Wait with the session open until a provider can be used; why.
    Wait(String),
    /// Under `--no-claude` no provider can review it: the person does,
    /// told why (`review_failed` and the `approve_landing` ask).
    Manual(String),
}

impl Supervisor<'_> {
    /// Record `landing_queued` as `run` starts to wait for the integration
    /// slot (`Phase::AwaitingSlot`): `stats` counts the wait from there to
    /// `integration_started` as its `landing_queue` phase (goal 36). `via`
    /// says what sent it: `exit` (its session exited after a passed review
    /// or with the run approved) or `resume` (an approved resolved resume);
    /// a `land` answer records its own with `via: approve`
    /// ([`Self::apply_landing_answer`]).
    /// Only `stats` reads it, so a failure to record it is only reported:
    /// the step it follows has already changed the run.
    pub(super) fn queue_landing(&mut self, run: &TaskRun, via: &str) {
        if let Err(error) =
            self.queue
                .record_runtime_event(run.id(), EventKind::LandingQueued, json!({"via": via}))
        {
            warn!(run_id = %run.id(), error = %format_args!("{error:#}"), "run {}: could not record landing_queued: {error:#}", run.id());
        }
    }
    /// Land `run`, which holds the integration slot under this token, on a
    /// thread (`previous` is where an error before `main` moved returns it).
    /// It pushes unless an approving `integrate --no-push` recorded
    /// `push: false`; a run landed on a passed review always pushes.
    /// Its verification commands may not start the sccache server
    /// (ADR-t2086-1): the supervisor looks at the server first and starts
    /// a missing one, and the landing looks again and guards them.
    pub(super) fn spawn_landing(
        &mut self,
        run: TaskRun,
        previous: RunStatus,
        main: CommitSha,
    ) -> Result<thread::JoinHandle<Result<IntegrationOutcome>>> {
        self.ensure_sccache(crate::domain::sccache::CheckReason::BeforeIntegrate);
        let sccache = self.host.sccache_port.clone();
        let queues = self.queues.clone();
        let repository = self.repository.clone();
        let remote = self.remote.clone();
        let verifier = self.verifier.clone();
        let processes = self.processes.clone();
        let files = self.files.clone();
        let pid = self.layout.pid;
        let common_dir = path_text(&self.layout.common_dir)?;
        let push = RunHistory::from_events(&self.queue.run_events(run.id())?).landing_pushes();
        let generators = self.generators.clone();
        let load_average = self.load_average;
        let (disk_config, free_space) = (self.host.disk_config, self.host.free_space);
        let runs_dir = self.layout.runs_dir.clone();
        let db = self.layout.db.clone();
        // The supervisor asks; the Integrator checks and lands (ADR-t728-2).
        let request = IntegrationRequest {
            requester: self.layout.supervisor_actor(),
            run,
            previous,
            main,
            token: self.token.clone(),
        };
        let integrator = Integrator::of_process(pid);
        Ok(spawn_traced(move || {
            let mut queue = queues.open()?;
            // As `check_disk` reads it, for the retry of a command that
            // failed on a full disk (task 639).
            let free = || free_space(&runs_dir).or_else(|| db.parent().and_then(free_space));
            integrator.land(
                &mut Integration {
                    queue: &mut *queue,
                    repository: &*repository,
                    verifier: &*verifier,
                    remote: push.then_some(&*remote as &dyn MainRemote),
                    files: &*files,
                    common_dir: &common_dir,
                    clock: &*generators.clock,
                    ids: &*generators.ids,
                    processes: &*processes,
                    pid,
                    load_average,
                    disk: None,
                    retry_disk: Some(integration::RetryDisk {
                        config: disk_config,
                        free: &free,
                    }),
                    sccache: sccache.as_ref().map(|SccachePort(server)| {
                        &**server as &dyn crate::application::SccacheServer
                    }),
                },
                &request,
            )
        }))
    }
    /// Record `review_started` and start the headless review of an accepted
    /// run whose session stays open (ADR-0027 decision 1): write
    /// `review.md`, then run the reviewer's command with the task's
    /// acceptance and the verdict schema. A review that cannot even start
    /// is a failed one.
    pub(super) fn start_review(
        &mut self,
        run: &TaskRun,
        session: Option<SessionRef>,
    ) -> Result<Phase> {
        self.resume_review(run, session, false)
    }
    /// Start the review of `run`, or wait in [`Phase::ReviewHeld`] while
    /// its route waits, keeping `retried`: a review that waited starts
    /// again as the retry it was (one retry per review across the wait).
    pub(super) fn resume_review(
        &mut self,
        run: &TaskRun,
        session: Option<SessionRef>,
        retried: bool,
    ) -> Result<Phase> {
        match held_review(&self.review_route(), session, retried) {
            Ok((held, why)) => {
                info!(run_id = %run.id(), "run {}: its review waits: {why}", run.id());
                Ok(held)
            }
            Err(session) => self.begin_review(run, session, retried),
        }
    }
    /// Where the next review goes (ADR-t1207-1). A `[roles.review]` that
    /// names no provider reviews on Claude as before: under `--no-claude`
    /// the person reviews it, and it waits while the queue's hold ask
    /// holds Claude (task 437). One that names its provider goes like the
    /// goal review (ADR-t1063-1 decisions 4 and 5): to its provider when
    /// it can be used, else to the other provider when that one can (and
    /// `[provider_fallback] jobs` is on, ADR-t1857-1), else it waits, or,
    /// under `--no-claude`, goes to the person.
    pub(super) fn review_route(&self) -> ReviewRoute {
        let role = ModelRole::Review;
        let models = self.role_models(role);
        provider::review_route(
            self.no_claude,
            (models.switchable(role), self.provider.fallback.jobs),
            models.launch(role),
            self.queue_hold,
            |provider| self.job_unusable(provider),
        )
    }
    /// Move review `attempt` off `unusable`, the provider that could not
    /// run it and is held now (ADR-t1063-1 decisions 4 and 5): record
    /// `review_retried` with the provider and why, then start the review
    /// on the other provider, or wait with the session open while neither
    /// can be used. `Err` gives the session back when the review does not
    /// move: under `--no-claude` with no provider left, or when the route
    /// still sends it to the same provider (its hold was not written), so
    /// that it fails to the person instead of starting there again.
    pub(super) fn move_review(
        &mut self,
        run: &TaskRun,
        session: Option<SessionRef>,
        attempt: usize,
        // Why the attempt failed, and what its job said of its session.
        (error, job_session): (&str, Option<&JobSession>),
        (provider, reason): (Provider, SwitchReason),
        retried: bool,
    ) -> Result<std::result::Result<Phase, Option<SessionRef>>> {
        let route = self.review_route();
        if !provider::review_moves(&route, provider) {
            return Ok(Err(session));
        }
        let mut retried_event = json!({
            "attempt": attempt,
            "error": error,
            "provider": provider.as_str(),
            "switch_reason": reason.as_str(),
        });
        // The job that ran on Codex names its thread and model (task 1339).
        if let Some(job_session) = job_session {
            job_session.record(&mut retried_event);
        }
        self.queue
            .record_runtime_event(run.id(), EventKind::ReviewRetried, retried_event)?;
        Ok(Ok(match held_review(&route, session, retried) {
            Ok((held, why)) => {
                warn!(run_id = %run.id(), error = %error, "run {} review {attempt}: {} cannot be used ({}); the review waits: {why}", run.id(), provider.as_str(), reason.as_str());
                held
            }
            Err(session) => {
                warn!(run_id = %run.id(), error = %error, "run {} review {attempt}: {} cannot be used ({}); reviewing it on the other provider", run.id(), provider.as_str(), reason.as_str());
                self.begin_review(run, session, retried)?
            }
        }))
    }
    /// Review the run once more with the same input after review `attempt`
    /// printed no readable verdict (task 328) or its job exited non-zero
    /// (task 1984), as `cause` says: record `review_retried` with why, then
    /// start the next review, whose own failure is not retried again. While
    /// the review's route waits, the next review waits in
    /// [`Phase::ReviewHeld`] as the retry, `review_retried` already
    /// recorded, and starts as the retry once the wait ends.
    ///
    /// The event's `cause` says which retry it is: `"unreadable"` after a
    /// verdict that could not be read (no verdict JSON, or one lacking the
    /// results of its required subagents), `"job_failed"` after a job that
    /// exited non-zero. The `review_retried` of a review moved off a
    /// provider that cannot be used ([`Self::move_review`]) has no `cause`:
    /// its `switch_reason` says why instead.
    pub(super) fn retry_review(
        &mut self,
        run: &TaskRun,
        session: Option<SessionRef>,
        attempt: usize,
        (error, cause): (&str, RetryCause),
        job_session: Option<&JobSession>,
    ) -> Result<Phase> {
        let mut retried_event = json!({
            "attempt": attempt,
            "error": error,
            "cause": cause.as_str(),
        });
        // The job that ran on Codex names its thread and model (task 1339).
        if let Some(job_session) = job_session {
            job_session.record(&mut retried_event);
        }
        self.queue
            .record_runtime_event(run.id(), EventKind::ReviewRetried, retried_event)?;
        let what = match cause {
            RetryCause::Unreadable => "printed no readable verdict",
            RetryCause::JobFailed => "failed",
        };
        warn!(run_id = %run.id(), error = %error, "run {} review {attempt} {what}: {error}; reviewing it once more", run.id());
        self.resume_review(run, session, true)
    }
    /// Start the next review attempt of `run` (one more than its recorded
    /// reviews) where [`Self::review_route`] sends it: it waits in
    /// [`Phase::ReviewHeld`] while no provider can be used, and goes to
    /// the person when none is left. The required subagents are read from
    /// the landing branch before `review_started`, and a review whose
    /// subagents cannot be read or run fails to the person without a job,
    /// never passing on fewer checks. `retried` is whether this attempt is
    /// the one retry of the review.
    pub(super) fn begin_review(
        &mut self,
        run: &TaskRun,
        mut session: Option<SessionRef>,
        retried: bool,
    ) -> Result<Phase> {
        let attempt =
            RunHistory::from_events(&self.queue.run_events(run.id())?).review_attempts() + 1;
        let live = match &session {
            Some(_) => session_alive(self, run.id())?,
            None => false,
        };
        let (launch, switchable) = match self.review_route() {
            ReviewRoute::Start(launch, switchable) => (launch, switchable),
            ReviewRoute::Wait(why) => {
                info!(run_id = %run.id(), "run {}: its review waits: {why}", run.id());
                return Ok(Phase::ReviewHeld { session, retried });
            }
            ReviewRoute::Manual(error) => {
                (self.review_material)(run.task_id(), None)?;
                return Ok(Phase::Exiting(ExitWatch::new(
                    session,
                    AfterExit::ReviewFailed {
                        attempt,
                        error,
                        duration_secs: 0,
                        output: None,
                        session: None,
                    },
                )));
            }
        };
        // The required subagents, from the landing branch's commit
        // (ADR-t1453-1 decisions 3 and 4). When they cannot be known the
        // review cannot pass: it fails to the person with why.
        let subagents = match self.review_subagents(run) {
            Ok(subagents) => subagents,
            Err(snapshot) => {
                let error =
                    format!("the review's required subagents could not be read: {snapshot:#}");
                return self.unstarted_review_failed(run, session, attempt, retried, error);
            }
        };
        let required: Vec<String> = subagents
            .iter()
            .flat_map(|s| s.agents.iter().map(|a| a.name.clone()))
            .collect();
        // A provider that cannot run the required subagents does not start
        // the review: the other one does, or a person reviews it
        // (ADR-t1453-1 decision 8). Nothing is skipped silently.
        let launch = if required.is_empty() {
            launch
        } else {
            match self.subagent_launch(launch, &required) {
                Ok(launch) => launch,
                Err(error) => {
                    return self.unstarted_review_failed(run, session, attempt, retried, error);
                }
            }
        };
        // The job's Claude session id (ADR-0048 decision 4); Codex names
        // its thread itself, which the review's end records (ADR-t1063-1
        // decision 6), as a goal review's does.
        let session_id = (launch.provider == crate::domain::Provider::Claude)
            .then(|| self.generators.ids.uuid());
        let mut started = json!({
            "attempt": attempt,
            "workspace_id": session.as_ref().map(|s| s.workspace.clone()),
            "session_live": live,
            "session_id": session_id,
            "launch": launch.to_value(),
        });
        if let Some(subagents) = &subagents {
            started["subagents"] = subagents.event_value();
        }
        // What its prompt takes (task 1571, ADR-t1566-1 decision 6).
        let prepared = self.prepare_review(run, attempt, subagents.as_ref());
        if let Ok((_, prompt_bytes)) = &prepared {
            started["prompt_bytes"] = json!(prompt_bytes);
        }
        self.queue
            .record_runtime_event(run.id(), EventKind::ReviewStarted, started)?;
        let spawned = match prepared {
            Ok((prompt, _)) => self.spawn_review(
                run,
                (attempt, &prompt),
                session_id.as_deref(),
                &launch,
                subagents.as_ref(),
            ),
            Err(error) => Err(error),
        };
        let error = match spawned {
            Ok(Ok((child, stdout, stderr))) => {
                info!(run_id = %run.id(), "run {} review {attempt} started on {} (session {})", run.id(), launch.provider.as_str(), if live { "kept open" } else { "ended" });
                let mut job = self.headless_job(
                    "review",
                    child,
                    stdout,
                    stderr,
                    JobSubject {
                        provider: launch.provider,
                        ..JobSubject::run(headless_job::REVIEW, run.id(), attempt)
                    },
                );
                // The provider that runs the review bounds it.
                if let Some(agent) = self.job_agent(launch.provider) {
                    job.timeout = agent.review_timeout();
                }
                return Ok(Phase::Review(ReviewWatch {
                    session,
                    attempt,
                    retried,
                    switchable,
                    required,
                    job,
                }));
            }
            // Its provider did not start: one that `[roles.review]` names
            // is held, and the review starts again where the route sends it
            // (ADR-t1063-1 decisions 4 and 5); under `--no-claude` with no
            // provider left, it fails to the person below.
            Ok(Err(started)) => {
                let error = format!("the headless review could not start: {started:#}");
                if let Some(unusable) = self.job_provider_failed(
                    launch.provider,
                    job_start_failure(&started),
                    (&error, &error),
                    &HoldJob::Review(run.id().clone()),
                    switchable,
                ) {
                    match self.move_review(
                        run,
                        session,
                        attempt,
                        (&error, None),
                        unusable,
                        retried,
                    )? {
                        Ok(phase) => return Ok(phase),
                        Err(back) => session = back,
                    }
                }
                error
            }
            // Its own preparation failed: no provider was tried.
            Err(prepared) => format!("the headless review could not start: {prepared:#}"),
        };
        warn!(run_id = %run.id(), error = %error, "run {}: {error}", run.id());
        self.close_review_session(run, None);
        Ok(Phase::Exiting(ExitWatch::new(
            session,
            AfterExit::ReviewFailed {
                attempt,
                error,
                duration_secs: 0,
                // No job ran, so this attempt wrote no output; a retry
                // follows a job that ran and printed an unreadable verdict
                // (task 426).
                output: unstarted_review_output(retried, attempt),
                session: None,
            },
        )))
    }
    /// Fail review `attempt`, which did not start, to the person with
    /// `error` (`review_failed` and the `approve_landing` ask once the
    /// session exited): its required subagents could not be known or run.
    fn unstarted_review_failed(
        &mut self,
        run: &TaskRun,
        session: Option<SessionRef>,
        attempt: usize,
        retried: bool,
        error: String,
    ) -> Result<Phase> {
        warn!(run_id = %run.id(), error = %error, "run {}: {error}", run.id());
        // For the person; a material that cannot be written (a receipt
        // that cannot be read fails both) still fails the review rather
        // than the supervisor's pass.
        if let Err(material) = (self.review_material)(run.task_id(), None) {
            warn!(run_id = %run.id(), "run {}: its review material could not be written: {material:#}", run.id());
        }
        Ok(Phase::Exiting(ExitWatch::new(
            session,
            AfterExit::ReviewFailed {
                attempt,
                error,
                duration_secs: 0,
                output: unstarted_review_output(retried, attempt),
                session: None,
            },
        )))
    }
    /// The launch of a review that requires the subagents `required`
    /// (ADR-t1453-1 decision 8): `launch` when its provider can run them,
    /// else the other provider's when that one can run them and be used,
    /// with why it was switched; else why neither can, for the person.
    fn subagent_launch(
        &self,
        launch: ActorLaunch,
        required: &[String],
    ) -> std::result::Result<ActorLaunch, String> {
        subagent_launch_of(
            launch,
            required,
            |provider| {
                self.job_agent(provider)
                    .is_some_and(|agent| agent.runs_review_subagents())
            },
            |provider| self.job_unusable(provider).is_none(),
        )
    }
    /// The review's required subagents from the landing branch's commit
    /// ([`snapshot_subagents`]); the range is the receipt's, read only when
    /// agents are configured.
    pub(super) fn review_subagents(&self, run: &TaskRun) -> Result<Option<SubagentSnapshot>> {
        let range = |main: &CommitSha| {
            let receipt_path = Path::new(run.receipt_path().context("missing receipt path")?);
            let receipt = Receipt::parse(
                &self
                    .files
                    .read_to_string(receipt_path)
                    .with_context(|| format!("read receipt {}", receipt_path.display()))?,
            )?;
            review_range_at(&*self.repository, run, &receipt, main)
        };
        snapshot_subagents(
            &*self.repository,
            &|text| self.verifier.review_subagents_in(text),
            &range,
        )
    }
    /// Write the review's material and prompt, held to its limits (task
    /// 1571): the prompt and what it takes, which `review_started` records.
    pub(super) fn prepare_review(
        &mut self,
        run: &TaskRun,
        attempt: usize,
        subagents: Option<&SubagentSnapshot>,
    ) -> Result<(String, PromptBytes)> {
        let run_dir = PathBuf::from(run.run_dir().context("missing run directory")?);
        // The attempt's range as its agents were selected from it, so that
        // review.md shows the diff that selected them even when main moved
        // since (none without agents configured: review.md finds its own).
        let material = (self.review_material)(run.task_id(), subagents.map(|s| &s.range))?;
        let path = material["path"]
            .as_str()
            .context("review wrote no path")?
            .to_owned();
        let task = self.queue.show(run.task_id())?.task;
        // The selected agents and their definitions go to the job beside
        // review.md; a review that needs none reads as before.
        let mut required = None;
        if let Some(snapshot) = subagents.filter(|s| !s.agents.is_empty()) {
            let input = run_dir.join(format!("review-subagents-{attempt}.json"));
            self.files.write(
                &input,
                serde_json::to_string_pretty(&snapshot.job_input())?.as_bytes(),
            )?;
            required = Some(review_subagents_prompt(snapshot, &input));
        }
        let fitted = review_prompt(&task, run, &path, required.as_deref())
            .with_language(self.verifier.language().as_ref());
        self.files.write(
            &run_dir.join(format!("review-prompt-{attempt}.txt")),
            fitted.text.as_bytes(),
        )?;
        Ok((fitted.text, fitted.bytes))
    }
    /// Start the job of review `attempt` with `prompt` on `launch`'s
    /// provider. The outer error is the review's own preparation, the
    /// inner one the start of its provider's process: only the latter says
    /// whether the provider can be used (ADR-t1063-1 decision 4).
    pub(super) fn spawn_review(
        &mut self,
        run: &TaskRun,
        (attempt, prompt): (usize, &str),
        session_id: Option<&str>,
        launch: &ActorLaunch,
        subagents: Option<&SubagentSnapshot>,
    ) -> Result<Result<StartedReview>> {
        let run_dir = PathBuf::from(run.run_dir().context("missing run directory")?);
        let stdout = run_dir.join(format!("review-{attempt}.out"));
        let stderr = run_dir.join(format!("review-{attempt}.err"));
        let worktree = PathBuf::from(run.worktree_path().context("missing worktree")?);
        // A worktree that is gone is the run's, not its provider's: its
        // spawn would read as a missing executable.
        anyhow::ensure!(
            self.files.is_dir(&worktree),
            "the run's worktree {} is gone",
            worktree.display()
        );
        // The repository's [run.env] reaches the review too (ADR-0023
        // decision 3).
        let mut env = self.verifier.run_env(&run_dir)?;
        // The review on either provider may not start the sccache server
        // (ADR-t2086-1).
        let look = self.sccache_look(crate::domain::sccache::CheckReason::BeforeReview, &run_dir);
        let named = crate::domain::sccache::SccacheTarget::of_pairs(&env).is_some();
        let without_env = look.apply(&mut env);
        if named {
            self.record_wrapper_removed(
                run.id(),
                &look,
                json!({"job": "review", "attempt": attempt}),
            );
        }
        // Each required agent's definition as committed on the landing
        // branch, for the provider to hand its job.
        let definitions: Vec<AgentDefinition> = subagents
            .iter()
            .flat_map(|s| &s.agents)
            .map(|agent| AgentDefinition::read(&agent.name, &agent.definition))
            .collect();
        let started = (|| {
            let agent = self.job_agent(launch.provider).with_context(|| {
                format!("no {} runs on this supervisor", launch.provider.as_str())
            })?;
            // Like the observer's job: the CLI knows the review by its role
            // and allows it only reads of this queue.
            self.actors_on(agent)
                .spawn(
                    ActorExecutionSpec::new(
                        ActorContext::review_job(run.id(), attempt),
                        WorkspaceAccess::Read(worktree),
                        ActorProgram::Headless {
                            program: HeadlessProgram::Review {
                                run,
                                prompt,
                                access: REVIEW_ACCESS,
                                subagents: &definitions,
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
                    .with_timeout(agent.review_timeout()),
                )
                .context("start the review")?
                .process()
        })();
        Ok(started.map(|child| (child, stdout, stderr)))
    }
    /// Move on from a verdict: `pass` exits the session and lands; `revise`
    /// goes to the live session while revises are left (ADR-0027 decision
    /// 2); a `concern` lands or goes back on the job's recommendation when
    /// it may be applied (ADR-t451-1 decision 3, [`Self::act_on_concern`]);
    /// anything else exits the session and asks a person. The ask
    /// records `job`, the review job that returned `verdict` as review
    /// `attempt`, as its `requested_by` (task 798).
    pub(super) fn act_on_verdict(
        &mut self,
        run: &TaskRun,
        session: Option<SessionRef>,
        verdict: ReviewVerdict,
        job: &ActorContext,
        attempt: usize,
    ) -> Result<Phase> {
        // With its agents' results, the review goes where the heaviest of
        // its judgments sends it (ADR-t1453-1 decision 7). When no agent's
        // judgment goes there, the verdict's own decides, as before.
        if !verdict.agents.is_empty() {
            let route = verdict.route(self.revise_left(run.id())?);
            if !route.deciding().is_empty() {
                if route.parent_lighter() {
                    warn!(run_id = %run.id(), "run {} review {attempt}: the verdict {} is lighter than its subagents' ({}); the run goes to {}", run.id(), verdict.verdict.as_str(), route.deciding().iter().map(|a| a.agent.as_str()).collect::<Vec<_>>().join(", "), route.destination.as_str());
                }
                return self.act_on_agents(run, session, verdict, &route, job, attempt);
            }
        }
        match verdict.verdict {
            ReviewDecision::Pass => self.precheck(run, session, verdict, Some(job.clone())),
            ReviewDecision::Concern => self.act_on_concern(run, session, verdict, job, attempt),
            ReviewDecision::Revise => self.send_revise(run, session, verdict, job, None),
        }
    }
    /// Whether the round of `run` has a revise left.
    pub(super) fn revise_left(&self, run: &RunId) -> Result<bool> {
        let events = self.queue.run_events(run)?;
        Ok(decide_revise(&RunHistory::from_events(&events)) != ReviseDecision::Ask)
    }
    /// Go where `route` sends a verdict one of whose agents' judgments
    /// decides it (ADR-t1453-1 decision 7): back to the session with the
    /// reasons of every judgment that sends it back (one revise of the
    /// round, a person past the limit), or to a person with every judgment
    /// that asks for one.
    fn act_on_agents(
        &mut self,
        run: &TaskRun,
        session: Option<SessionRef>,
        verdict: ReviewVerdict,
        route: &VerdictRoute,
        job: &ActorContext,
        attempt: usize,
    ) -> Result<Phase> {
        let parent_decides = route.parent == route.destination;
        let parent_concern = parent_decides && verdict.verdict == ReviewDecision::Concern;
        let combined = combined_verdict(&verdict, route, parent_decides);
        match route.destination {
            Destination::SendBack => {
                let concern = parent_concern
                    .then(|| SentBackConcern::of(&verdict, attempt))
                    .flatten();
                let phase = self.send_revise(run, session, combined, job, concern)?;
                if parent_concern {
                    // As `act_on_concern` records a send_back it could not
                    // apply.
                    let unsent =
                        (!matches!(phase, Phase::Revise(_))).then_some(EscalatedBecause::Unsent);
                    self.record_concern_decided(run, attempt, &verdict, unsent)?;
                }
                Ok(phase)
            }
            Destination::Ask | Destination::Land => {
                if parent_concern {
                    let escalated = match verdict.concern_decision(true) {
                        ConcernDecision::Ask(why) => why,
                        // Past the revise limit only.
                        _ => EscalatedBecause::ReviseLimit,
                    };
                    self.record_concern_decided(run, attempt, &verdict, Some(escalated))?;
                }
                let why = agents_escalation(&verdict, route, parent_decides);
                Ok(landing_ask(Some(why), combined, session, job, true))
            }
        }
    }
    /// A `concern` (ADR-t451-1 decision 3): with a `high` confidence and
    /// no reason a person is needed, `land` goes the way of a `pass` (the
    /// conflict precheck, the session's `/exit` right before the landing,
    /// the e2e when the run needs it) and `send_back` that of a `revise`
    /// (counted toward the revise limit); anything else exits the session
    /// and asks a person with the job's recommendation. `concern_decided`
    /// records which.
    fn act_on_concern(
        &mut self,
        run: &TaskRun,
        session: Option<SessionRef>,
        verdict: ReviewVerdict,
        job: &ActorContext,
        attempt: usize,
    ) -> Result<Phase> {
        let events = self.queue.run_events(run.id())?;
        let history = RunHistory::from_events(&events);
        let revise_left = decide_revise(&history) != ReviseDecision::Ask;
        let decision = verdict.concern_decision(revise_left);
        let escalated = match decision {
            ConcernDecision::Land => {
                self.record_concern_decided(run, attempt, &verdict, None)?;
                info!(run_id = %run.id(), "run {} review {attempt}: the concern lands on the review's recommendation", run.id());
                return self.precheck(run, session, verdict, Some(job.clone()));
            }
            ConcernDecision::SendBack => {
                let concern = SentBackConcern::of(&verdict, attempt);
                let phase = self.send_revise(run, session, verdict.clone(), job, concern)?;
                let unsent =
                    (!matches!(phase, Phase::Revise(_))).then_some(EscalatedBecause::Unsent);
                self.record_concern_decided(run, attempt, &verdict, unsent)?;
                return Ok(phase);
            }
            ConcernDecision::Ask(why) => why,
        };
        self.record_concern_decided(run, attempt, &verdict, Some(escalated))?;
        let why = concern_escalation(&verdict, escalated, &history);
        Ok(landing_ask(why, verdict, session, job, true))
    }
    /// Record `concern_decided` for review `attempt`'s concern.
    pub(super) fn record_concern_decided(
        &mut self,
        run: &TaskRun,
        attempt: usize,
        verdict: &ReviewVerdict,
        escalated: Option<EscalatedBecause>,
    ) -> Result<()> {
        self.queue.record_runtime_event(
            run.id(),
            EventKind::ConcernDecided,
            concern::decided_payload(
                attempt,
                verdict.recommendation,
                verdict.confidence,
                verdict.reason_category,
                escalated,
            ),
        )?;
        Ok(())
    }
    /// Send `verdict`'s reasons to the live session as the next revise (a
    /// `revise`, or a `concern` the job recommends sending back), the
    /// round's limit already checked; a person is asked when the session
    /// ended or the request could not be sent. `concern` is the concern
    /// whose `send_back` this applies, which the ask names when the session
    /// does not fix it (task 1392).
    fn send_revise(
        &mut self,
        run: &TaskRun,
        session: Option<SessionRef>,
        verdict: ReviewVerdict,
        job: &ActorContext,
        concern: Option<SentBackConcern>,
    ) -> Result<Phase> {
        let carries = verdict.verdict == ReviewDecision::Concern;
        let ask = |why: String, verdict: ReviewVerdict, session| {
            landing_ask(Some(why), verdict, session, job, carries)
        };
        let events = self.queue.run_events(run.id())?;
        let history = RunHistory::from_events(&events);
        let ReviseDecision::Request { attempt, round } = decide_revise(&history) else {
            let why = revise_limit_why(history.round_revise_attempts());
            return Ok(ask(why, verdict, session));
        };
        let Some(live) = session
            .clone()
            .filter(|_| session_alive(self, run.id()).unwrap_or(false))
        else {
            let why = "the session had ended, so nobody could revise the run".to_owned();
            return Ok(ask(why, verdict, session));
        };
        let task = self.queue.show(run.task_id())?.task;
        let run_dir = Path::new(run.run_dir().context("missing run directory")?);
        let findings_file = run_dir.join(format!("revise-{attempt}-findings.txt"));
        let message = revise_request(
            &task,
            run,
            round,
            &verdict.reasons,
            Some(&findings_file.to_string_lossy()),
        )?
        .with_language(self.verifier.language().as_ref());
        // The request names the file as where the cut findings are whole
        // (ADR-t2072-1).
        if message.bytes.omitted.contains_key("findings") {
            self.files
                .write(&findings_file, revise_findings(&verdict.reasons).as_bytes())?;
        }
        self.files.write(
            &run_dir.join(format!("revise-{attempt}.txt")),
            message.text.as_bytes(),
        )?;
        // A revise is rework the task caused: the live session is
        // switched one step up before the request (ADR-0079 decision
        // 5). One that cannot be switched goes on as it is, and why
        // is recorded.
        let current = WorkerSession::current(&events);
        let (worker, raise) = match current.raised() {
            None => (current, None),
            Some(raised) => (
                raised,
                Some(Escalation {
                    from: current,
                    reason: worker_model::REVISE.to_owned(),
                }),
            ),
        };
        let sent_at = self.files.now();
        // Recorded before it is written to `turns/`: a supervisor that
        // stops in between leaves an adopter that writes it once when
        // the attempt's request is not there (`adopted_start`), and
        // never a second time when it is.
        let mut requested = json!({"attempt": attempt, "reasons": verdict.reasons, "sent_at": super::file_time::request_sent_at(sent_at)});
        if let Some(requested) = requested.as_object_mut() {
            let provider = run.actual_provider();
            requested.extend(worker.fields_raised(provider, raise.as_ref()));
        }
        self.queue
            .record_runtime_event(run.id(), EventKind::ReviseRequested, requested)?;
        let _submission = match submit(
            self,
            run,
            &live.workspace,
            Input::from(&message),
            "revise request",
        ) {
            Ok(submission) => submission,
            Err(error) => {
                let why = format!("the revise request could not be sent: {error:#}");
                warn!(run_id = %run.id(), "run {}: {why}", run.id());
                self.queue.record_runtime_event(
                    run.id(),
                    EventKind::ReviseUnsent,
                    json!({"attempt": attempt, "error": why}),
                )?;
                return Ok(ask(why, verdict, session));
            }
        };
        info!(run_id = %run.id(), "revise {round} of {MAX_REVISE_ATTEMPTS} (revise-{attempt}) sent to run {} in workspace {}", run.id(), live.workspace);
        Ok(Phase::Revise(ReviseWatch::new(
            run,
            live,
            attempt,
            Fix::Revise {
                reasons: verdict.reasons,
                concern,
            },
            sent_at,
            Some(sent_at),
            self.generators.clock.monotonic(),
        )?))
    }

    /// Before a passed run's session is asked to exit, judge with `git
    /// merge-tree` whether its head conflicts with the current main,
    /// without touching the worktree (ADR-0027 decision 4). A clean merge
    /// exits the session and lands. A conflict records `conflict_precheck`
    /// and sends the live session the resolution request of a resume; the
    /// session's rewritten receipt is validated and reviewed again. The
    /// requests are conflict-only attempts after a passed review (ADR-0047
    /// decision 24): with the run's conflict-only resumes they stop at
    /// `[resume] conflict_only_limit` (`ResumeConfig`), and they are not counted toward
    /// `MAX_RESUME_ATTEMPTS`. Past the conflict-only limit, a run that can
    /// be retried with its branch carried over goes on to land without a
    /// person: a conflicting landing parks it with its resumes used up, and
    /// the resume pass retries it (`exhaust_resumes`). Otherwise, and when
    /// its counted resumes are used up, the session exits and a person is
    /// asked. Without a live session to ask (or when Git cannot judge), the
    /// run lands as before, and a conflicting landing parks it for a resume.
    /// `job` is the review job that passed the run, recorded as the
    /// `requested_by` of the ask (task 798).
    pub(super) fn precheck(
        &mut self,
        run: &TaskRun,
        session: Option<SessionRef>,
        verdict: ReviewVerdict,
        job: Option<ActorContext>,
    ) -> Result<Phase> {
        let land = |session| Phase::Exiting(ExitWatch::new(session, AfterExit::Land));
        let head = run
            .result_commit()
            .cloned()
            .context("accepted run has no result commit")?;
        // A landing branch that does not resolve cannot be judged: the run
        // goes on to land, and its landing waits until it resolves.
        let main = match self.repository.main_head() {
            Ok(main) => main,
            Err(error) => {
                warn!(run_id = %run.id(), error = %format_args!("{error:#}"), "run {}: the conflict precheck could not read the landing branch: {error:#}; landing", run.id());
                return Ok(land(session));
            }
        };
        let conflicts = match self
            .repository
            .merge_conflicts(main.as_str(), head.as_str())
        {
            Ok(conflicts) => conflicts,
            Err(error) => {
                warn!(run_id = %run.id(), error = %format_args!("{error:#}"), "run {}: the conflict precheck against main {main} failed: {error:#}; landing", run.id());
                return Ok(land(session));
            }
        };
        if conflicts.is_empty() {
            return Ok(land(session));
        }
        let events = self.queue.run_events(run.id())?;
        let history = RunHistory::from_events(&events);
        let requested = history.conflict_requests();
        // Resumes of the run parked only by a conflict after its review
        // passed are not counted (ADR-0047 decision 24), nor are the
        // requests: both are conflict-only attempts.
        let resumes = history.resumes();
        let attempt = requested + 1;
        let mut payload = json!({
            "code": ReasonCode::RebaseConflict,
            "main": main,
            "head": head,
            // Recorded for the reader; a failure to find it does not stop the request.
            "merge_base": self.repository.merge_base(main.as_str(), head.as_str()).ok().flatten(),
            "conflicts": conflicts,
            "attempt": attempt,
            "requested": false,
        });
        let why = format!(
            "git merge-tree finds that main {main} conflicts with the run in {}",
            conflicts.join(", ")
        );
        // What the retry that carries the branch over needs besides the
        // conflict after a passed review: no run of the task was retried
        // that way, and the head holds commits on top of the run's base.
        let inherited = self
            .queue
            .show(run.task_id())?
            .events
            .iter()
            .any(uses_automatic_inherit);
        let own_commits = head != *run.base_commit();
        match decide_conflict(&history, !inherited && own_commits, self.resume.config) {
            ConflictDecision::RequestRebase => {}
            ConflictDecision::Inherit => {
                // Recorded for the reader and an adopter, which goes on
                // to land as well.
                payload["exhausted"] = json!(true);
                self.queue
                    .record_runtime_event(run.id(), EventKind::ConflictPrecheck, payload)?;
                info!(run_id = %run.id(), "run {}: {why}, after {requested} conflict requests and {} conflict-only resumes (at most {} in all); landing, and a conflicting landing retries the task with the run's branch carried over", run.id(), resumes.conflict_only, self.resume.config.conflict_only_limit);
                return Ok(land(session));
            }
            ConflictDecision::Ask => {
                let why = if resumes.counted >= MAX_RESUME_ATTEMPTS {
                    format!(
                        "{why}, after {requested} conflict requests and {} counted resumes (at most {MAX_RESUME_ATTEMPTS})",
                        resumes.counted
                    )
                } else {
                    let cannot = if inherited {
                        "a run of its task was already retried with its branch carried over"
                    } else {
                        "its branch holds no commit to carry over"
                    };
                    format!(
                        "{why}, after {requested} conflict requests and {} conflict-only resumes (at most {} in all), and {cannot}",
                        resumes.conflict_only, self.resume.config.conflict_only_limit
                    )
                };
                // What an adopter asks, if it takes the run over before the ask.
                payload["asked"] = json!(why);
                self.queue
                    .record_runtime_event(run.id(), EventKind::ConflictPrecheck, payload)?;
                info!(run_id = %run.id(), "run {}: {why}; asking a person", run.id());
                return Ok(Phase::Exiting(ExitWatch::new(
                    session,
                    Fix::Conflict(verdict).ask(String::new(), why, job),
                )));
            }
        }
        let live = session
            .clone()
            .filter(|_| session_alive(self, run.id()).unwrap_or(false));
        let sent = match &live {
            Some(live) => {
                let task = self.queue.show(run.task_id())?.task;
                let landed = landed_since(&mut *self.queue, &*self.repository, run, &main)?;
                let request = ResumeRequest {
                    main: main.clone(),
                    branch: self.repository.landing_branch()?.name,
                    reason: why.clone(),
                    kind: ResumeKind::Precheck,
                    reason_file: super::resume::reason_file(run, &format!("conflict-{attempt}")),
                };
                let message = resume_request(&task, run, &request, &landed)?
                    .with_language(self.verifier.language().as_ref());
                self.write_reason_file(&request, &message)?;
                let run_dir = Path::new(run.run_dir().context("missing run directory")?);
                self.files.write(
                    &run_dir.join(format!("conflict-{attempt}.txt")),
                    message.text.as_bytes(),
                )?;
                let sent_at = self.files.now();
                // Recorded before it is written, like a revise request (an
                // adopter writes it once when it is not in `turns/`); a
                // request that could not be sent is withdrawn below.
                let mut sending = payload.clone();
                sending["requested"] = json!(true);
                sending["sent_at"] = super::file_time::request_sent_at(sent_at);
                self.queue
                    .record_runtime_event(run.id(), EventKind::ConflictPrecheck, sending)?;
                submit(
                    self,
                    run,
                    &live.workspace,
                    Input::from(&message),
                    "conflict request",
                )
                .map(|_submission| (sent_at, sent_at))
                .map_err(|error| format!("the request could not be sent: {error:#}"))
            }
            None => Err("the session had ended".to_owned()),
        };
        let (Some(live), Ok((sent_at, start))) = (live.clone(), sent.clone()) else {
            let error = sent.err().unwrap_or_default();
            payload["error"] = json!(error);
            if live.is_some() {
                // Withdraws the request recorded before the send; its
                // conflicts were counted with it.
                payload["unsent"] = json!(true);
                if let Some(payload) = payload.as_object_mut() {
                    payload.remove("conflicts");
                }
            }
            self.queue
                .record_runtime_event(run.id(), EventKind::ConflictPrecheck, payload)?;
            warn!(run_id = %run.id(), error = %error, "run {}: {why}, and {error}; landing, whose rebase parks it for a resume", run.id());
            return Ok(land(session));
        };
        info!(run_id = %run.id(), "run {}: {why}; asked its live session in workspace {} to rebase (request {attempt})", run.id(), live.workspace);
        Ok(Phase::Revise(ReviseWatch::new(
            run,
            live,
            attempt,
            Fix::Conflict(verdict),
            sent_at,
            Some(start),
            self.generators.clock.monotonic(),
        )?))
    }
    /// Stop the session's wrapper after it exited ([`stop_run_session`]):
    /// the worker's own through [`close_workspace`], a resume's by
    /// recording `workspace_closed` with its attempt.
    pub(super) fn close_session(&mut self, run: &TaskRun, session: &SessionRef) -> Result<TaskRun> {
        match session.resume {
            None if run.workspace_closed_at().is_none() && run.workspace_id().is_some() => {
                close_workspace(&mut *self.queue, self.sessions, &self.token, run)
            }
            None => Ok(run.clone()),
            Some(attempt) => {
                match stop_run_session(self.sessions, &session.workspace, StopRoute::AfterReview) {
                    Ok(()) => self.queue.record_runtime_event(
                        run.id(),
                        EventKind::WorkspaceClosed,
                        json!({"workspace_id": session.workspace, "resume_attempt": attempt}),
                    )?,
                    Err(error) => {
                        let message = format!(
                            "the resume's session {} could not be stopped: {error:#}",
                            session.workspace
                        );
                        warn!(run_id = %run.id(), "run {}: {message}", run.id());
                        self.queue.record_cleanup_failure(
                            run.id(),
                            &message,
                            &reason_of_error(&error, ReasonCode::BackendFailed),
                        )?;
                    }
                }
                self.queue.run(run.id())
            }
        }
    }
    /// Open the `approve_landing` ask of a run whose review did not pass
    /// (ADR-0027, ADR-0022 decision 3), which the inbox's watch notifies;
    /// returns its ID.
    pub(super) fn open_landing_ask(
        &mut self,
        run: &TaskRun,
        decision: ReviewDecision,
        reasons: &[String],
        summary: &str,
        why: Option<&str>,
        (recommendation, confidence, reason_category): (
            Option<LandingRecommendation>,
            Option<AskConfidence>,
            Option<ConcernReason>,
        ),
    ) -> Result<AskId> {
        let question = landing_question(
            run,
            decision,
            reasons,
            summary,
            why,
            (recommendation, confidence),
        );
        self.ask_approve_landing(
            run,
            question,
            recommendation.map(|r| r.as_str().to_owned()),
            confidence,
            landing_ask_reason(reason_category),
        )
    }
    /// Open the `approve_landing` ask of a run whose headless review failed
    /// (task 328), with why and where the review's material and output
    /// are, so that the failure reaches the inbox in the step that records
    /// `review_failed`; returns its ID. `output` is the attempt whose job
    /// ran and wrote its output: a review that could not start names no
    /// output of its own (task 426).
    pub(super) fn open_failed_review_ask(
        &mut self,
        run: &TaskRun,
        attempt: usize,
        output: Option<usize>,
        error: &str,
    ) -> Result<AskId> {
        let question = failed_review_question(run, attempt, output, error);
        self.ask_approve_landing(run, question, None, None, AskReason::Scope)
    }
    /// Open an `approve_landing` ask of `run` carrying the review job's
    /// `recommendation` and `confidence` when it gave them (ADR-t451-1
    /// decision 3).
    fn ask_approve_landing(
        &mut self,
        run: &TaskRun,
        question: String,
        recommendation: Option<String>,
        confidence: Option<AskConfidence>,
        reason_category: AskReason,
    ) -> Result<AskId> {
        // An earlier ask of the run is about an earlier review (the run was
        // sent back since): it would hold the new one back as a repeat, and
        // its answer no longer fits (task 328, task 425).
        for stale in self
            .queue
            .close_approve_landing_asks(run.id(), STALE_LANDING_ASK_CLOSED)?
        {
            info!(run_id = %run.id(), ask_id = %stale.id, "run {}: closed its earlier approve_landing ask {}", run.id(), stale.id);
        }
        // Through `ask`, like the CLI: the inbox's watch notifies it.
        let outcome = ask::ask(
            &mut *self.queue,
            NewAsk {
                recommendation,
                confidence,
                kind: AskKind::ApproveLanding,
                task_id: None,
                run_id: Some(run.id().clone()),
                question,
                options: LANDING_OPTIONS.iter().map(|o| (*o).to_owned()).collect(),
                asked_by: "supervisor".to_owned(),
                reason_category,
                topics: Vec::new(),
                finding_id: None,
                request_id: None,
            },
        )?;
        outcome["id"]
            .as_i64()
            .map(AskId::new)
            .context("ask returned no id")
    }
    /// Apply the answered `approve_landing` asks of runs awaiting
    /// integration that nobody leases (ADR-0027), on every pass whatever the
    /// slots and the integration slot (task 949): `land` records the
    /// approval and queues the run to land ([`Self::start_approved_landings`]
    /// lands it), `send_back` makes it `needs_session` for a resume that
    /// names the review's reasons (after the person's, `send_back:
    /// <reason>`, task 1424), and `cancel` fails the run and cancels its
    /// task ([`LandingAnswer`]). The ask is closed once applied; any other
    /// answer is left to the inbox. An error is noted and the ask is tried again on a
    /// later pass.
    pub(super) fn apply_landing_answers(&mut self) -> Result<()> {
        for ask in self.queue.landing_answers()? {
            let Some(run_id) = ask.run_id.clone() else {
                continue;
            };
            let answer = ask.answer.as_deref().unwrap_or_default().trim().to_owned();
            let run = self.queue.run(&run_id)?;
            let Some(action) = LandingAnswer::parse(&answer) else {
                continue;
            };
            if run.status() != RunStatus::AwaitingIntegration
                || self.queue.run_lease(&run_id)?.is_some()
            {
                continue;
            }
            if let Err(error) = self.apply_landing_answer(&run, ask.id, &answer, action) {
                warn!(run_id = %run.id(), ask_id = %ask.id, error = %format_args!("{error:#}"), "run {}: the answer {answer:?} of ask {} could not be applied: {error:#}", run.id(), ask.id);
            }
        }
        Ok(())
    }
    /// Start the landing of the oldest run queued by a `land` answer
    /// ([`RunHistory::queued_approval`]) or by the recovery of a landing
    /// it may land again (task 1118, [`RunHistory::recovered_landing`]):
    /// awaiting integration, nobody
    /// leasing it, once a slot is free, no run integrates, and the landing
    /// would not fail on a missing program (ADR-0049 decision 9), an
    /// unresolved landing branch (ADR-t615-1) or short free disk space
    /// (task 377). Called before new claims, so a queued run lands first;
    /// the queue is read from the events, so a run approved under another
    /// supervisor is landed once, with its ask left closed (task 949).
    /// A landing that fails to start is only noted by the caller, which goes on,
    /// and the run stays queued for a later pass.
    pub(super) fn start_approved_landings(&mut self, parallel: usize) -> Result<()> {
        if self.landing.run_env_missing
            || self.observation.ci.held()
            || self.landing.unresolved
            || self.host.disk.landing_short
            || self.used_slots() >= parallel
            || !self
                .queue
                .runs_with_status(RunStatus::Integrating)?
                .is_empty()
        {
            return Ok(());
        }
        let mut queued = Vec::new();
        for run in self
            .queue
            .runs_with_status(RunStatus::AwaitingIntegration)?
        {
            if self.queue.run_lease(run.id())?.is_some() {
                continue;
            }
            if let Some(approval) =
                RunHistory::from_events(&self.queue.run_events(run.id())?).queued_to_land()
            {
                queued.push((approval, run));
            }
        }
        queued.sort_by_key(|(approval, _)| *approval);
        let Some((_, run)) = queued.into_iter().next() else {
            return Ok(());
        };
        let e2e = self.e2e_due(&run)?.is_some();
        let main = if e2e {
            None
        } else {
            Some(self.repository.main_head()?)
        };
        // Not while the cleanup job clears the run's build outputs (task
        // 1289): the landing builds them again once it has passed.
        let cleaning = self.host.cleanup.cleaning();
        let mut guard = cleanup::lock_cleaning(&cleaning);
        if !guard.may_lease(run.id()) {
            self.host.cleanup.defer();
            return Ok(());
        }
        // A run that still needs its e2e (ADR-t1233-2) runs it in a slot
        // of its own first, and lands from there.
        let Some(main) = main else {
            let leased = self.queue.lease_for_e2e(run.id(), &self.token)?;
            drop(guard);
            if let Some(run) = leased {
                info!(run_id = %run.id(), "run {} runs its e2e before it lands as queued", run.id());
                self.claim.slots.admit(Slot::new(run, Phase::AwaitingSlot));
            }
            return Ok(());
        };
        let landing = self.queue.begin_integration(run.id(), &self.token, &main)?;
        drop(guard);
        info!(run_id = %run.id(), "run {} lands onto main {main} as queued", run.id());
        let handle = self.spawn_landing(landing.clone(), RunStatus::AwaitingIntegration, main)?;
        self.claim
            .slots
            .admit(Slot::new(landing, Phase::Landing(Some(handle))));
        Ok(())
    }
    /// Review the runs recovered from a landing they may not land again
    /// ([`RunHistory::recovered_landing`]'s `Review`, task 1118) as a run
    /// just validated: awaiting integration, nobody leasing it, no
    /// `approve_landing` ask of it open, while slots are free. The run is
    /// leased to this supervisor and its review started; what follows is
    /// that of any review (ADR-0027).
    pub(super) fn review_recovered_runs(&mut self, parallel: usize) -> Result<()> {
        for run in self
            .queue
            .runs_with_status(RunStatus::AwaitingIntegration)?
        {
            if self.used_slots() >= parallel {
                break;
            }
            if self.queue.run_lease(run.id())?.is_some()
                || !matches!(
                    RunHistory::from_events(&self.queue.run_events(run.id())?).recovered_landing(),
                    Some(RecoveredLanding::Review(_))
                )
                || self
                    .queue
                    .has_unclosed_ask(run.id(), AskKind::ApproveLanding)?
            {
                continue;
            }
            // Not while the cleanup job clears the run's build outputs
            // (task 1289).
            let cleaning = self.host.cleanup.cleaning();
            let mut guard = cleanup::lock_cleaning(&cleaning);
            if !guard.may_lease(run.id()) {
                self.host.cleanup.defer();
                continue;
            }
            let leased = self.queue.lease_for_review(run.id(), &self.token)?;
            drop(guard);
            let Some(run) = leased else {
                continue;
            };
            info!(run_id = %run.id(), task_id = %run.task_id(), "run {} of task {} is reviewed again: its landing was given up before it was approved or passed", run.id(), run.task_id());
            let session = self.session_of(&run)?;
            match self.start_review(&run, session) {
                Ok(phase) => self.claim.slots.admit(Slot::new(run, phase)),
                Err(error) => {
                    let message = format!("run {} could not be reviewed: {error:#}", run.id());
                    warn!(run_id = %run.id(), error = %format_args!("{error:#}"), "{}", message);
                    self.abandon(&run, message, &reason_of_error(&error, ReasonCode::Other));
                }
            }
        }
        Ok(())
    }
    /// Apply `answer`, an answer to the `approve_landing` ask `ask_id` of
    /// `run` read as `action` ([`LandingAnswer::parse`]). The
    /// `landing_decided` of a `send_back` records the answer as
    /// `send_back` and the person's reason, when given, as
    /// `person_reason`.
    pub(super) fn apply_landing_answer(
        &mut self,
        run: &TaskRun,
        ask_id: AskId,
        answer: &str,
        action: LandingAnswer,
    ) -> Result<()> {
        self.record_review_outcome(run, ask_id, answer)?;
        let payload = json!({"ask_id": ask_id, "answer": answer});
        match action {
            LandingAnswer::Land => {
                // Each step is recorded once for the ask, so an answer
                // applied in part is finished on a later pass.
                let events = self.queue.run_events(run.id())?;
                if !RunHistory::from_events(&events).approved_by_ask(ask_id) {
                    self.queue.record_runtime_event(
                        run.id(),
                        EventKind::IntegrationApproved,
                        json!({"status": run.status().as_str(), "pid": self.layout.pid, "push": true, "ask_id": ask_id}),
                    )?;
                    self.queue.record_runtime_event(
                        run.id(),
                        EventKind::LandingQueued,
                        json!({"via": "approve", "ask_id": ask_id}),
                    )?;
                }
                self.queue.close_ask(ask_id)?;
                info!(run_id = %run.id(), "run {} is approved by ask {ask_id} and waits to land", run.id());
            }
            LandingAnswer::SendBack(person_reason) => {
                let reasons = latest_review_reasons(&*self.queue, run.id())?;
                let reason = sent_back_reason(ask_id, person_reason.as_deref(), &reasons);
                let mut payload = json!({"ask_id": ask_id, "answer": "send_back"});
                if let Some(person_reason) = person_reason {
                    payload["person_reason"] = json!(person_reason);
                }
                self.queue.decide_landing(
                    run.id(),
                    RunStatus::NeedsSession,
                    &reason,
                    Reason::new(ReasonCode::SentBack).on(payload),
                )?;
                self.queue.close_ask(ask_id)?;
                info!(run_id = %run.id(), "run {} was sent back by ask {ask_id}; it waits for a resume", run.id());
            }
            LandingAnswer::Cancel => {
                let reason = canceled_reason(ask_id);
                self.queue.decide_landing(
                    run.id(),
                    RunStatus::Failed,
                    &reason,
                    Reason::new(ReasonCode::Cancelled).on(payload),
                )?;
                self.queue.transition(run.task_id(), TaskAction::Cancel)?;
                self.queue.close_ask(ask_id)?;
                info!(run_id = %run.id(), task_id = %run.task_id(), "run {} failed and task {} was canceled by ask {ask_id}", run.id(), run.task_id());
                self.close_ended_landing_asks(Some(run.task_id()));
                self.clean_task_worktrees(run.task_id());
            }
        }
        Ok(())
    }
}

impl Supervisor<'_> {
    /// Record what a person's answer to an `approve_landing` ask says of
    /// the review's findings (ADR-t947-1 decision 4): a `review_outcome`
    /// on the run's latest review, once per ask, when that review gave a
    /// verdict that sent the run back. The ask of a failed review, or of a
    /// pass (or a concern the runtime landed, ADR-t451-1 decision 3) whose
    /// conflict a person decides, records none.
    fn record_review_outcome(&self, run: &TaskRun, ask_id: AskId, answer: &str) -> Result<()> {
        let Some(outcome) = review_reason::answer_outcome(answer) else {
            return Ok(());
        };
        let events = self.queue.run_events(run.id())?;
        let recorded = events
            .iter()
            .any(|e| e.kind == event_kind::REVIEW_OUTCOME && e.payload["ask_id"] == json!(ask_id));
        let review = events.iter().rev().find(|e| {
            matches!(
                e.kind.as_str(),
                event_kind::REVIEW_FINISHED | event_kind::REVIEW_FAILED
            )
        });
        let Some(review) = review.filter(|review| {
            !recorded
                && review.kind == event_kind::REVIEW_FINISHED
                && !concern::lets_land(&review.payload)
        }) else {
            return Ok(());
        };
        // A verdict recorded before the codes has each reason unlabeled.
        let codes: Vec<Vec<String>> =
            serde_json::from_value(review.payload["reason_codes"].clone()).unwrap_or_else(|_| {
                let reasons = review.payload["reasons"].as_array().map_or(0, Vec::len);
                review_reason::recorded(&[], reasons.max(1))
            });
        self.queue.record_runtime_event(
            run.id(),
            EventKind::ReviewOutcome,
            json!({
                "attempt": review.payload["attempt"],
                "ask_id": ask_id,
                "outcome": outcome,
                "reason_codes": codes,
                "primary_code": review_reason::primary(&codes),
            }),
        )?;
        Ok(())
    }
}

/// The `approve_landing` ask `verdict` leads to once the session exited,
/// `why` saying why a person decides; `requested_by` is `job`, the review
/// job that returned it (task 798). `carries` puts the job's
/// recommendation and confidence on the ask (a `concern`'s, ADR-t451-1
/// decision 3).
fn landing_ask(
    why: Option<String>,
    verdict: ReviewVerdict,
    session: Option<SessionRef>,
    job: &ActorContext,
    carries: bool,
) -> Phase {
    Phase::Exiting(ExitWatch::new(
        session,
        AfterExit::Ask {
            decision: verdict.verdict,
            recommendation: verdict.recommendation.filter(|_| carries),
            confidence: verdict.confidence.filter(|_| carries),
            reason_category: verdict.reason_category.filter(|_| carries),
            reasons: verdict.reasons,
            summary: verdict.summary,
            why,
            requested_by: Some(job.clone()),
            sent_back: None,
        },
    ))
}

/// The reason a `send_back` answer to ask `ask_id` parks the run with,
/// which its resume names: the person's reason first when the answer gave
/// one (`send_back: <reason>`, task 1424), then the latest review's
/// `reasons`.
fn sent_back_reason(ask_id: AskId, person_reason: Option<&str>, reasons: &[String]) -> String {
    let findings = if reasons.is_empty() {
        "(no reasons recorded)".to_owned()
    } else {
        reasons.join("; ")
    };
    match person_reason {
        Some(person_reason) => format!(
            "a person sent the run back in ask {ask_id}: {person_reason}; the review's findings: {findings}"
        ),
        None => format!("the review's findings were sent back by ask {ask_id}: {findings}"),
    }
}

/// The reason a `cancel` answer to ask `ask_id` fails the run with.
fn canceled_reason(ask_id: AskId) -> String {
    format!("canceled by ask {ask_id}")
}

/// Why a verdict that would send the run back asks a person instead: the
/// round's `revises` were already sent (`MAX_REVISE_ATTEMPTS`).
fn revise_limit_why(revises: usize) -> String {
    format!("the review still asks for changes after {revises} revises")
}

/// Why a review is reviewed once more with the same input.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum RetryCause {
    /// Its stdout held no readable verdict (task 328).
    Unreadable,
    /// Its job exited non-zero (task 1984).
    JobFailed,
}

impl RetryCause {
    pub(super) fn as_str(self) -> &'static str {
        match self {
            Self::Unreadable => "unreadable",
            Self::JobFailed => "job_failed",
        }
    }
}

/// Why a review that ended as `outcome` is reviewed once more with the
/// same input, if it is: stdout without a readable verdict (task 328) or a
/// job that exited non-zero (task 1984), while the retry is on (`retry`),
/// when the review was not itself a retry (`retried`, one retry per review
/// for either cause) and a provider is left to review it (`manual`: the
/// `--no-claude` route that gives it to a person). A job stopped at the
/// timeout is not reviewed again: another review would spend the timeout
/// once more on a job that did not end. Nor is a non-zero exit of a job
/// whose provider was found unusable and that did not move off it
/// (`unusable`, after `move_review`): that failure is the provider's, not
/// a passing one, and goes to the person as before.
pub(super) fn retries_review(
    outcome: &ReviewEnd,
    retry: bool,
    retried: bool,
    manual: bool,
    unusable: bool,
) -> Option<RetryCause> {
    let cause = match outcome {
        ReviewEnd::Unreadable(_) => RetryCause::Unreadable,
        ReviewEnd::Failed(JobFailed::Exited(_)) if !unusable => RetryCause::JobFailed,
        ReviewEnd::Failed(_) | ReviewEnd::Verdict(_) => return None,
    };
    (retry && !retried && !manual).then_some(cause)
}

/// The phase of a review that waits on `route`, with why, when the route
/// waits: [`Phase::ReviewHeld`] keeping whether the review is the retry
/// (`retried`), so that the review that starts after the wait is the retry
/// it was and is not retried again (one retry per review across the wait).
/// `Err` gives the session back when the route does not wait.
pub(super) fn held_review(
    route: &ReviewRoute,
    session: Option<SessionRef>,
    retried: bool,
) -> std::result::Result<(Phase, &str), Option<SessionRef>> {
    match route {
        ReviewRoute::Wait(why) => Ok((Phase::ReviewHeld { session, retried }, why)),
        ReviewRoute::Start(..) | ReviewRoute::Manual(_) => Err(session),
    }
}

/// The attempt whose output the ask of review `attempt` names when that
/// review could not start, so wrote none (task 426): the review before,
/// when this one was its retry (after an unreadable verdict or a non-zero
/// exit); else none.
fn unstarted_review_output(retried: bool, attempt: usize) -> Option<usize> {
    retried.then(|| attempt - 1)
}

/// The question of the `approve_landing` ask of `run` whose review returned
/// `decision` that did not pass: why a person decides, the reasons, where
/// the review material is, and the job's recommendation when it carries one
/// (ADR-t451-1 decision 3).
fn landing_question(
    run: &TaskRun,
    decision: ReviewDecision,
    reasons: &[String],
    summary: &str,
    why: Option<&str>,
    (recommendation, confidence): (Option<LandingRecommendation>, Option<AskConfidence>),
) -> String {
    let mut question = format!(
        "The supervisor's review of run {} (task {}) returned {}{}: {summary}",
        run.id(),
        run.task_id(),
        decision.as_str(),
        why.map(|why| format!(" ({why})")).unwrap_or_default()
    );
    for reason in reasons {
        question.push_str(&format!("\n- {reason}"));
    }
    if let Some(run_dir) = &run.run_dir() {
        question.push_str(&format!("\nReview material: {run_dir}/review.md"));
    }
    if let Some(recommendation) = recommendation {
        question.push_str(&format!(
            "\nThe review recommends {} ({} confidence).",
            recommendation.as_str(),
            confidence.map_or("no", AskConfidence::as_str)
        ));
    }
    question.push_str(
        "\nland: land it as it is. send_back: resume the session with these reasons (answer `send_back: <your reason>` to add yours). cancel: fail the run and cancel the task.",
    );
    question
}

/// Why a person is needed for the `approve_landing` ask of a review whose
/// job gave `reason_category`: `discard` for a discard, `scope` otherwise.
fn landing_ask_reason(reason_category: Option<ConcernReason>) -> AskReason {
    match reason_category {
        Some(ConcernReason::Discard) => AskReason::Discard,
        _ => AskReason::Scope,
    }
}

/// The question of the `approve_landing` ask of `run` whose headless review
/// `attempt` failed with `error` (task 328): why, and where the review's
/// material and output are. `output` is the attempt whose job ran and wrote
/// its output: a review that could not start names no output of its own
/// (task 426). A review no agent ran under `--no-claude` says so.
fn failed_review_question(
    run: &TaskRun,
    attempt: usize,
    output: Option<usize>,
    error: &str,
) -> String {
    let mut question = format!(
        "The supervisor's headless review of run {} (task {}) failed and gave no verdict (review {attempt}): {error}",
        run.id(),
        run.task_id(),
    );
    if error.starts_with(REVIEW_PROVIDER_DISABLED) {
        question = format!(
            "{error}. No review agent ran for run {} (task {}); review it manually.",
            run.id(),
            run.task_id()
        );
    }
    if let Some(run_dir) = &run.run_dir() {
        question.push_str(&format!("\nReview material: {run_dir}/review.md"));
        match output {
            Some(ran) if ran == attempt => question.push_str(&format!(
                "\nReview output: {run_dir}/review-{ran}.out, {run_dir}/review-{ran}.err"
            )),
            Some(ran) => question.push_str(&format!(
                "\nReview output of the earlier review {ran}: {run_dir}/review-{ran}.out, {run_dir}/review-{ran}.err"
            )),
            None => {}
        }
    }
    question.push_str(
        "\nReview the material by hand, then answer. land: land it as it is. send_back: resume the session with this failure as the reason (answer `send_back: <your reason>` to add yours). cancel: fail the run and cancel the task.",
    );
    question
}

/// The verdict a review whose agents decide where it goes (`route`)
/// applies: the reasons of every judgment that goes there, an agent's
/// prefixed with its name (the verdict's own when `parent_decides`), as a
/// `revise` when it goes back and a `concern` when it asks. Its concern
/// fields are those that make a person needed: a `discard` when any
/// judgment that asks gives one, else a `scope`; no recommendation, which
/// each judgment's line of the ask carries.
pub(super) fn combined_verdict(
    verdict: &ReviewVerdict,
    route: &VerdictRoute,
    parent_decides: bool,
) -> ReviewVerdict {
    let mut reasons = Vec::new();
    let mut reason_codes = Vec::new();
    if parent_decides {
        reasons.extend(verdict.reasons.iter().cloned());
        reason_codes.extend(verdict.recorded_codes());
    }
    let deciding = route.deciding();
    let results = verdict
        .agents
        .iter()
        .filter(|r| deciding.iter().any(|a| a.agent == r.agent));
    let mut categories = Vec::new();
    if parent_decides {
        categories.extend(verdict.reason_category);
    }
    for result in results {
        categories.extend(result.reason_category);
        let texts = if result.reasons.is_empty() {
            vec![result.summary.clone()]
        } else {
            result.reasons.clone()
        };
        for text in texts {
            reasons.push(format!("{}: {text}", result.agent));
            reason_codes.push(vec![review_reason::UNLABELED.to_owned()]);
        }
    }
    let asks = route.destination == Destination::Ask;
    let reason_category = asks.then(|| {
        if categories.contains(&ConcernReason::Discard) {
            ConcernReason::Discard
        } else {
            ConcernReason::Scope
        }
    });
    ReviewVerdict {
        verdict: if asks {
            ReviewDecision::Concern
        } else {
            ReviewDecision::Revise
        },
        reasons,
        reason_codes,
        summary: verdict.summary.clone(),
        recommendation: None,
        confidence: None,
        reason_category,
        agents: verdict.agents.clone(),
    }
}

/// Why a review whose agents decide where it goes asks a person: each
/// judgment that asks for one, by whom, with its decision, recommendation,
/// confidence and the reason a person is needed; and the verdict's own
/// decision when it was lighter.
pub(super) fn agents_escalation(
    verdict: &ReviewVerdict,
    route: &VerdictRoute,
    parent_decides: bool,
) -> String {
    let describe = |who: &str,
                    decision: Option<ReviewDecision>,
                    recommendation: Option<LandingRecommendation>,
                    confidence: Option<AskConfidence>,
                    category: Option<ConcernReason>| {
        let mut said = format!(
            "{who} returned {}",
            decision.map_or("nothing", ReviewDecision::as_str)
        );
        if decision == Some(ReviewDecision::Concern) {
            said.push_str(&format!(
                " recommending {} ({} confidence{})",
                recommendation.map_or("nothing", LandingRecommendation::as_str),
                confidence.map_or("no", AskConfidence::as_str),
                category
                    .map(|c| format!(", {}", c.as_str()))
                    .unwrap_or_default()
            ));
        }
        said
    };
    let mut parts = Vec::new();
    if parent_decides {
        parts.push(describe(
            "the review",
            Some(verdict.verdict),
            verdict.recommendation,
            verdict.confidence,
            verdict.reason_category,
        ));
    }
    for agent in route.deciding() {
        if let Some(result) = verdict.agents.iter().find(|r| r.agent == agent.agent) {
            parts.push(describe(
                &format!("the subagent {}", result.agent),
                result.verdict,
                result.recommendation,
                result.confidence,
                result.reason_category,
            ));
        }
    }
    let mut why = format!("a person decides: {}", parts.join("; "));
    if route.parent_lighter() {
        why.push_str(&format!(
            "; the review's own verdict {} was lighter",
            verdict.verdict.as_str()
        ));
    }
    why
}

/// Why a `concern` goes to a person (`escalated`), for its ask; `None` for
/// a verdict with no recommendation, which asks as it always did.
pub(super) fn concern_escalation(
    verdict: &ReviewVerdict,
    escalated: EscalatedBecause,
    history: &RunHistory<'_>,
) -> Option<String> {
    let recommends = verdict
        .recommendation
        .map_or("nothing", |recommendation| recommendation.as_str());
    match escalated {
        EscalatedBecause::NoRecommendation => None,
        EscalatedBecause::LowConfidence => Some(format!(
            "the review recommends {recommends} without high confidence"
        )),
        EscalatedBecause::Scope => Some(format!(
            "the review recommends {recommends}, but landing it would accept a departure from the acceptance, an ADR or the goal (scope)"
        )),
        EscalatedBecause::Discard => Some(format!(
            "the review recommends {recommends}, but the judgement is whether to throw the work away (discard)"
        )),
        EscalatedBecause::ReviseLimit => Some(format!(
            "the review recommends send_back after {} revises",
            history.round_revise_attempts()
        )),
        EscalatedBecause::Unsent => Some(format!(
            "the review recommends {recommends}, but it could not be applied"
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{EventId, RunRecord};

    const RUN: &str = "r1";
    const DIR: &str = "/runs/r1";

    fn run() -> TaskRun {
        TaskRun::restore(RunRecord {
            id: RunId::new(RUN).unwrap(),
            task_id: TaskId::new(1),
            status: RunStatus::AwaitingIntegration,
            requested_provider: Provider::Claude,
            actual_provider: Provider::Claude,
            worker_mode: crate::domain::worker::Worker::default_mode(Provider::Claude),
            base_commit: CommitSha::parse("a".repeat(40), "commit").unwrap(),
            branch: Some(format!("dagq/{RUN}")),
            worktree_path: Some(format!("{DIR}/worktree")),
            workspace_id: None,
            receipt_path: Some(format!("{DIR}/receipt.json")),
            log_path: None,
            result_commit: None,
            repo_path: None,
            run_dir: Some(DIR.to_owned()),
            last_error: None,
            workspace_closed_at: None,
            created_at: String::new(),
        })
        .unwrap()
    }

    fn verdict(json: Value) -> ReviewVerdict {
        ReviewVerdict::parse(&json.to_string()).unwrap()
    }

    fn concern(recommendation: Option<&str>, confidence: Option<&str>) -> ReviewVerdict {
        verdict(
            json!({"verdict": "concern", "reasons": ["a finding"], "summary": "judged",
                       "recommendation": recommendation, "confidence": confidence,
                       "reason_category": null}),
        )
    }

    fn events(kinds: &[&str]) -> Vec<RunEvent> {
        kinds
            .iter()
            .enumerate()
            .map(|(i, kind)| RunEvent {
                id: EventId::new(i as i64 + 1),
                task_id: None,
                goal_id: None,
                run_id: None,
                kind: (*kind).into(),
                payload: json!({"attempt": i + 1}),
                created_at: String::new(),
                actor: None,
            })
            .collect()
    }

    /// The tail every failed review's ask ends with.
    const FAILED_TAIL: &str = "\nReview the material by hand, then answer. land: land it as it is. send_back: resume the session with this failure as the reason (answer `send_back: <your reason>` to add yours). cancel: fail the run and cancel the task.";

    /// The ask of a review whose job ran and failed names this review's
    /// output (a non-zero exit, the timeout, an unreadable verdict twice;
    /// task 328).
    #[test]
    fn a_failed_review_that_ran_names_its_own_output() {
        let error = "the headless review exited with exit status: 3: broken";
        assert_eq!(
            failed_review_question(&run(), 1, Some(1), error),
            format!(
                "The supervisor's headless review of run r1 (task 1) failed and gave no verdict (review 1): {error}\
                 \nReview material: {DIR}/review.md\
                 \nReview output: {DIR}/review-1.out, {DIR}/review-1.err{FAILED_TAIL}"
            )
        );
    }

    /// A review that could not start wrote no output, so its ask names none
    /// of its own (task 426): none at all for a first review, the earlier
    /// review's when it was the retry of an unreadable verdict.
    #[test]
    fn a_review_that_could_not_start_names_no_output_of_its_own() {
        let error =
            "the headless review could not start: the test reviewer cannot start this review";
        assert_eq!(unstarted_review_output(false, 1), None);
        assert_eq!(unstarted_review_output(true, 2), Some(1));
        let first = failed_review_question(&run(), 1, unstarted_review_output(false, 1), error);
        assert_eq!(
            first,
            format!(
                "The supervisor's headless review of run r1 (task 1) failed and gave no verdict (review 1): {error}\
                 \nReview material: {DIR}/review.md{FAILED_TAIL}"
            )
        );
        assert!(!first.contains("Review output"), "{first}");
        let retry = failed_review_question(&run(), 2, unstarted_review_output(true, 2), error);
        assert_eq!(
            retry,
            format!(
                "The supervisor's headless review of run r1 (task 1) failed and gave no verdict (review 2): {error}\
                 \nReview material: {DIR}/review.md\
                 \nReview output of the earlier review 1: {DIR}/review-1.out, {DIR}/review-1.err{FAILED_TAIL}"
            )
        );
        assert!(!retry.contains("review-2."), "{retry}");
    }

    /// A review no agent ran under `--no-claude` says so instead.
    #[test]
    fn a_review_no_agent_ran_says_to_review_it_manually() {
        let error = format!("{REVIEW_PROVIDER_DISABLED}; no provider is left");
        assert_eq!(
            failed_review_question(&run(), 1, None, &error),
            format!(
                "{error}. No review agent ran for run r1 (task 1); review it manually.\
                 \nReview material: {DIR}/review.md{FAILED_TAIL}"
            )
        );
    }

    /// A verdict that cannot be read and a job that exited non-zero are
    /// reviewed once more, each with its cause: not a job stopped at the
    /// timeout, not a retry itself (one retry per review for either cause),
    /// not with the retry off, and not when no provider is left to review
    /// it (`--no-claude`). A verdict is acted on, retried or not.
    #[test]
    fn an_unreadable_verdict_or_a_non_zero_exit_is_reviewed_again_once() {
        let unreadable = || ReviewEnd::Unreadable("the review printed no verdict JSON".into());
        let exited = || {
            ReviewEnd::Failed(JobFailed::Exited(
                "the headless review exited with exit status: 1: (none)".into(),
            ))
        };
        let timed_out = || {
            ReviewEnd::Failed(JobFailed::TimedOut(
                "the headless review did not finish within 1 seconds".into(),
            ))
        };
        let pass = || {
            ReviewEnd::Verdict(verdict(
                json!({"verdict": "pass", "reasons": [], "summary": "ok"}),
            ))
        };
        for (end, cause) in [
            (unreadable as fn() -> ReviewEnd, RetryCause::Unreadable),
            (exited, RetryCause::JobFailed),
        ] {
            assert_eq!(
                retries_review(&end(), true, false, false, false),
                Some(cause)
            );
            for (retry, retried, manual) in [
                (true, true, false),
                (false, false, false),
                (true, false, true),
                (false, true, true),
            ] {
                assert_eq!(retries_review(&end(), retry, retried, manual, false), None);
            }
        }
        // A provider found unusable that the review did not move off: its
        // non-zero exit goes to the person; an unreadable verdict is retried
        // as before.
        assert_eq!(retries_review(&exited(), true, false, false, true), None);
        assert_eq!(
            retries_review(&unreadable(), true, false, false, true),
            Some(RetryCause::Unreadable)
        );
        for retry in [false, true] {
            for retried in [false, true] {
                for manual in [false, true] {
                    for unusable in [false, true] {
                        let decide =
                            |end: ReviewEnd| retries_review(&end, retry, retried, manual, unusable);
                        assert_eq!(decide(timed_out()), None);
                        assert_eq!(decide(pass()), None);
                    }
                }
            }
        }
        assert_eq!(RetryCause::Unreadable.as_str(), "unreadable");
        assert_eq!(RetryCause::JobFailed.as_str(), "job_failed");
    }

    /// A review that waits keeps whether it is the retry (task 2013): the
    /// held phase carries `retried`, so the review that starts after the
    /// wait is not retried again when it was the retry, and is retried once
    /// when it was not. A route that does not wait gives the session back.
    #[test]
    fn a_held_review_keeps_whether_it_is_the_retry() {
        let exited = ReviewEnd::Failed(JobFailed::Exited(
            "the headless review exited with exit status: 1: (none)".into(),
        ));
        let wait = ReviewRoute::Wait("the hold ask holds Claude".into());
        for retried in [false, true] {
            let session = Some(SessionRef {
                workspace: "w1".into(),
                resume: None,
            });
            let Ok((held, why)) = held_review(&wait, session, retried) else {
                panic!("the review does not wait");
            };
            assert_eq!(why, "the hold ask holds Claude");
            let Phase::ReviewHeld {
                session: Some(session),
                retried: kept,
            } = held
            else {
                panic!("not held with its session");
            };
            assert_eq!(session.workspace, "w1");
            assert_eq!(kept, retried);
            let again = retries_review(&exited, true, kept, false, false);
            assert_eq!(again, (!retried).then_some(RetryCause::JobFailed));
        }
        let manual = ReviewRoute::Manual("no provider".into());
        let Err(Some(session)) = held_review(
            &manual,
            Some(SessionRef {
                workspace: "w1".into(),
                resume: None,
            }),
            true,
        ) else {
            panic!("a route that does not wait held the review");
        };
        assert_eq!(session.workspace, "w1");
    }

    /// The reasons a `send_back` (with and without the person's reason)
    /// and a `cancel` record; which answers apply is
    /// `LandingAnswer::parse`'s.
    #[test]
    fn a_send_back_and_a_cancel_record_their_reasons() {
        assert_eq!(
            sent_back_reason(AskId::new(7), None, &["a".into(), "b".into()]),
            "the review's findings were sent back by ask 7: a; b"
        );
        assert_eq!(
            sent_back_reason(AskId::new(7), None, &[]),
            "the review's findings were sent back by ask 7: (no reasons recorded)"
        );
        assert_eq!(
            sent_back_reason(
                AskId::new(7),
                Some("keep the flag"),
                &["a".into(), "b".into()]
            ),
            "a person sent the run back in ask 7: keep the flag; the review's findings: a; b"
        );
        assert_eq!(canceled_reason(AskId::new(7)), "canceled by ask 7");
    }

    /// The tail every landing ask of a review that did not pass ends with.
    const LANDING_TAIL: &str = "\nland: land it as it is. send_back: resume the session with these reasons (answer `send_back: <your reason>` to add yours). cancel: fail the run and cancel the task.";

    /// A `revise` past the round's limit asks a person why, with the
    /// findings and the material (ADR-0027 decision 2).
    #[test]
    fn a_revise_past_its_limit_asks_with_the_count_of_revises() {
        let why = revise_limit_why(MAX_REVISE_ATTEMPTS);
        assert_eq!(why, "the review still asks for changes after 2 revises");
        assert_eq!(
            landing_question(
                &run(),
                ReviewDecision::Revise,
                &["still short".into()],
                "not yet",
                Some(&why),
                (None, None),
            ),
            format!(
                "The supervisor's review of run r1 (task 1) returned revise (the review still asks for changes after 2 revises): not yet\
                 \n- still short\nReview material: {DIR}/review.md{LANDING_TAIL}"
            )
        );
    }

    /// A concern the runtime did not apply asks with the job's
    /// recommendation and confidence, and why (ADR-t451-1 decision 3); a
    /// discard is asked for that reason, anything else for `scope`.
    #[test]
    fn a_concern_asks_with_its_recommendation_and_why() {
        let history_events = events(&[]);
        let history = RunHistory::from_events(&history_events);
        for (recommendation, confidence, escalated, why) in [
            (
                LandingRecommendation::Land,
                AskConfidence::Low,
                EscalatedBecause::LowConfidence,
                "the review recommends land without high confidence",
            ),
            (
                LandingRecommendation::Land,
                AskConfidence::High,
                EscalatedBecause::Scope,
                "the review recommends land, but landing it would accept a departure from the acceptance, an ADR or the goal (scope)",
            ),
            (
                LandingRecommendation::SendBack,
                AskConfidence::High,
                EscalatedBecause::Discard,
                "the review recommends send_back, but the judgement is whether to throw the work away (discard)",
            ),
            (
                LandingRecommendation::SendBack,
                AskConfidence::High,
                EscalatedBecause::Unsent,
                "the review recommends send_back, but it could not be applied",
            ),
        ] {
            let verdict = concern(Some(recommendation.as_str()), Some(confidence.as_str()));
            let got = concern_escalation(&verdict, escalated, &history);
            assert_eq!(got.as_deref(), Some(why));
            assert_eq!(
                landing_question(
                    &run(),
                    ReviewDecision::Concern,
                    &verdict.reasons,
                    &verdict.summary,
                    got.as_deref(),
                    (verdict.recommendation, verdict.confidence),
                ),
                format!(
                    "The supervisor's review of run r1 (task 1) returned concern ({why}): judged\
                     \n- a finding\nReview material: {DIR}/review.md\
                     \nThe review recommends {} ({} confidence).{LANDING_TAIL}",
                    recommendation.as_str(),
                    confidence.as_str()
                )
            );
        }
        assert_eq!(
            landing_ask_reason(Some(ConcernReason::Discard)),
            AskReason::Discard
        );
        assert_eq!(
            landing_ask_reason(Some(ConcernReason::Scope)),
            AskReason::Scope
        );
        assert_eq!(landing_ask_reason(None), AskReason::Scope);
    }

    /// A send_back the runtime would apply past the round's two revises asks
    /// with the count of the round's revises.
    #[test]
    fn a_send_back_past_the_revise_limit_asks_with_the_count_of_revises() {
        let round = events(&[
            event_kind::REVIEW_STARTED,
            event_kind::REVISE_REQUESTED,
            event_kind::REVISE_FINISHED,
            event_kind::REVIEW_STARTED,
            event_kind::REVISE_REQUESTED,
            event_kind::REVISE_FINISHED,
            event_kind::REVIEW_STARTED,
        ]);
        let history = RunHistory::from_events(&round);
        assert_eq!(decide_revise(&history), ReviseDecision::Ask);
        let verdict = concern(Some("send_back"), Some("high"));
        let why = concern_escalation(&verdict, EscalatedBecause::ReviseLimit, &history);
        assert_eq!(
            why.as_deref(),
            Some("the review recommends send_back after 2 revises")
        );
        let question = landing_question(
            &run(),
            ReviewDecision::Concern,
            &verdict.reasons,
            &verdict.summary,
            why.as_deref(),
            (verdict.recommendation, verdict.confidence),
        );
        assert!(
            question.starts_with(
                "The supervisor's review of run r1 (task 1) returned concern (the review recommends send_back after 2 revises): judged"
            ),
            "{question}"
        );
    }

    /// A concern without a recommendation (the form before ADR-t451-1)
    /// asks as it always did: no why, no recommendation.
    #[test]
    fn a_concern_without_a_recommendation_asks_as_before() {
        let verdict = concern(None, None);
        let none = events(&[]);
        let why = concern_escalation(
            &verdict,
            EscalatedBecause::NoRecommendation,
            &RunHistory::from_events(&none),
        );
        assert_eq!(why, None);
        let question = landing_question(
            &run(),
            ReviewDecision::Concern,
            &verdict.reasons,
            &verdict.summary,
            why.as_deref(),
            (verdict.recommendation, verdict.confidence),
        );
        assert_eq!(
            question,
            format!(
                "The supervisor's review of run r1 (task 1) returned concern: judged\
                 \n- a finding\nReview material: {DIR}/review.md{LANDING_TAIL}"
            )
        );
        assert!(!question.contains("recommends"), "{question}");
    }

    /// A verdict without the completed result of each required agent is
    /// never acted on as a verdict: it is the unreadable one, retried once
    /// and then failed to a person (ADR-t1453-1 decision 6); one of a
    /// review that requires none and names none reads as before.
    #[test]
    fn a_verdict_lacking_its_agents_results_is_unreadable() {
        use super::super::jobs::review_end;
        let required = vec!["design".to_owned()];
        let pass = || verdict(json!({"verdict": "pass", "reasons": [], "summary": "ok"}));
        let end = review_end(pass(), &required);
        let ReviewEnd::Unreadable(why) = &end else {
            panic!("a pass without its agent's result was acted on");
        };
        assert!(why.ends_with("no result of design"), "{why}");
        assert_eq!(
            retries_review(&end, true, false, false, false),
            Some(RetryCause::Unreadable)
        );
        assert_eq!(retries_review(&end, true, true, false, false), None);
        assert!(matches!(review_end(pass(), &[]), ReviewEnd::Verdict(_)));
        let failed = verdict(json!({"verdict": "pass", "reasons": [], "summary": "ok",
            "agents": [{"agent": "design", "status": "failed"}]}));
        assert!(matches!(
            review_end(failed.clone(), &required),
            ReviewEnd::Unreadable(_)
        ));
        // A repository without agents does not take a result it never asked for.
        assert!(matches!(review_end(failed, &[]), ReviewEnd::Unreadable(_)));
        let whole = verdict(json!({"verdict": "pass", "reasons": [], "summary": "ok",
            "agents": [{"agent": "design", "status": "completed", "verdict": "pass", "summary": "ok"}]}));
        assert!(matches!(
            review_end(whole, &required),
            ReviewEnd::Verdict(_)
        ));
    }

    /// What a review whose agents decide where it goes applies and asks
    /// with: the reasons of the judgments that go there, each agent's
    /// named, a discard over a scope, and each judgment that asks in why.
    #[test]
    fn the_agents_that_decide_give_their_reasons_and_why() {
        let lighter = verdict(json!({
            "verdict": "pass", "reasons": ["fine"], "summary": "ok",
            "agents": [
                {"agent": "design", "status": "completed", "verdict": "revise", "reasons": ["docs drift"], "summary": "s"},
                {"agent": "lint", "status": "completed", "verdict": "pass", "summary": "ok"},
                {"agent": "terse", "status": "completed", "verdict": "revise", "summary": "add a test"},
            ],
        }));
        let route = lighter.route(true);
        let combined = combined_verdict(&lighter, &route, false);
        assert_eq!(combined.verdict, ReviewDecision::Revise);
        assert_eq!(
            combined.reasons,
            ["design: docs drift", "terse: add a test"]
        );
        assert_eq!(combined.reason_codes.len(), 2);
        assert_eq!(combined.reason_category, None);
        let asking = verdict(json!({
            "verdict": "concern", "reasons": ["mine"], "summary": "look",
            "recommendation": "land", "confidence": "low",
            "agents": [
                {"agent": "design", "status": "completed", "verdict": "concern", "reasons": ["drop it"],
                 "summary": "s", "recommendation": "send_back", "confidence": "high", "reason_category": "discard"},
            ],
        }));
        let route = asking.route(true);
        assert_eq!(route.destination, Destination::Ask);
        let combined = combined_verdict(&asking, &route, true);
        assert_eq!(combined.verdict, ReviewDecision::Concern);
        assert_eq!(combined.reasons, ["mine", "design: drop it"]);
        assert_eq!(combined.reason_category, Some(ConcernReason::Discard));
        assert_eq!(combined.recommendation, None);
        assert_eq!(
            agents_escalation(&asking, &route, true),
            "a person decides: the review returned concern recommending land (low confidence); the subagent design returned concern recommending send_back (high confidence, discard)"
        );
        let scope = verdict(json!({
            "verdict": "pass", "reasons": [], "summary": "ok",
            "agents": [{"agent": "design", "status": "completed", "verdict": "concern", "reasons": [],
                        "summary": "s", "reason_category": "scope"}],
        }));
        let route = scope.route(true);
        assert_eq!(
            combined_verdict(&scope, &route, false).reason_category,
            Some(ConcernReason::Scope)
        );
        assert_eq!(
            agents_escalation(&scope, &route, false),
            "a person decides: the subagent design returned concern recommending nothing (no confidence, scope); the review's own verdict pass was lighter"
        );
        assert_eq!(
            subagents_unsupported(Provider::Codex, &["design".to_owned(), "lint".to_owned()]),
            "subagents_unsupported: the review requires the subagents design, lint, which codex cannot run, and no other provider that can run them can be used"
        );
    }

    /// A review that requires subagents its provider cannot run starts on
    /// the other provider when that one runs them and can be used, its
    /// launch saying from which and why, and holds nothing; with neither,
    /// it fails to the person with why (ADR-t1453-1 decision 8).
    #[test]
    fn a_review_whose_provider_cannot_run_its_agents_moves_or_fails() {
        use crate::domain::actor_model::RoleModels;
        let required = ["design".to_owned(), "tests".to_owned()];
        let mut models = RoleModels::default();
        models.entry(ModelRole::Review).provider = Some(Provider::Codex);
        let codex = models.launch(ModelRole::Review);
        let claude_runs = |provider: Provider| provider == Provider::Claude;
        // `[roles.review]` names Codex, which runs no review subagents: it
        // starts on Claude, its launch saying why.
        let moved = subagent_launch_of(codex.clone(), &required, claude_runs, |_| true).unwrap();
        assert_eq!(moved.provider, Provider::Claude);
        assert_eq!(moved.to_value()["switched_from"], "codex");
        assert_eq!(moved.to_value()["switch_reason"], SUBAGENTS_UNSUPPORTED);
        // On a provider that runs them it starts as it is.
        let claude = ActorLaunch::default_of(ModelRole::Review);
        assert_eq!(
            subagent_launch_of(claude.clone(), &required, claude_runs, |_| true),
            Ok(claude.clone())
        );
        // Claude cannot be used: the Codex review fails to the person.
        assert_eq!(
            subagent_launch_of(codex, &required, claude_runs, |provider| {
                provider != Provider::Claude
            }),
            Err(subagents_unsupported(Provider::Codex, &required))
        );
        // No provider runs them.
        assert_eq!(
            subagent_launch_of(claude, &required, |_| false, |_| true).unwrap_err(),
            "subagents_unsupported: the review requires the subagents design, tests, which claude cannot run, and no other provider that can run them can be used"
        );
    }

    /// With `[provider_fallback] jobs` off a review that requires
    /// subagents its provider cannot run still starts on the other
    /// provider: a choice by ability, not a provider that cannot be used
    /// (ADR-t1453-1 decision 8, ADR-t1857-1).
    #[test]
    fn with_the_fallback_off_a_review_still_moves_for_its_agents() {
        use crate::domain::actor_model::RoleModels;
        let required = ["design".to_owned()];
        let mut models = RoleModels::default();
        models.entry(ModelRole::Review).provider = Some(Provider::Codex);
        let route = super::provider::review_route(
            false,
            (models.switchable(ModelRole::Review), false),
            models.launch(ModelRole::Review),
            None,
            |_| None,
        );
        let ReviewRoute::Start(codex, true) = route else {
            panic!("the review starts on Codex");
        };
        assert_eq!(codex.provider, Provider::Codex);
        let moved = subagent_launch_of(
            codex,
            &required,
            |provider| provider == Provider::Claude,
            |_| true,
        )
        .unwrap();
        assert_eq!(moved.provider, Provider::Claude);
        assert_eq!(
            moved.switch_reason,
            Some(SwitchReason::SubagentsUnsupported)
        );
    }

    /// The verdict is lighter than an agent's judgment (ADR-t1453-1
    /// decision 7): a concern a person must decide by the rule of a concern
    /// (ADR-t451-1 decision 3) asks, the ask naming the agent, what it
    /// returned, that the verdict was lighter and why a person is needed;
    /// a `send_back` on high confidence or a `revise` goes back with the
    /// agent's reasons; and an agent's `scope` concern under a concern that
    /// would land on high confidence still asks.
    #[test]
    fn an_agents_heavier_judgment_under_a_lighter_verdict_decides_where_the_run_goes() {
        let tests_pass = json!({"agent": "tests", "status": "completed", "verdict": "pass", "reasons": [], "summary": "ok"});
        let under_pass = |design: Value| {
            verdict(
                json!({"verdict": "pass", "reasons": [], "summary": "meets the acceptance",
                "agents": [design, tests_pass.clone()]}),
            )
        };
        let design_concern = |fields: Value| {
            let mut concern = json!({"agent": "design", "status": "completed", "verdict": "concern",
                "reasons": ["departs"], "summary": "s"});
            for (key, value) in fields.as_object().unwrap() {
                concern[key] = value.clone();
            }
            concern
        };
        let question = |combined: &ReviewVerdict, why: &str| {
            landing_question(
                &run(),
                combined.verdict,
                &combined.reasons,
                &combined.summary,
                Some(why),
                (combined.recommendation, combined.confidence),
            )
        };
        for (fields, said, reason) in [
            (
                json!({"recommendation": "land", "confidence": "high", "reason_category": "scope"}),
                "the subagent design returned concern recommending land (high confidence, scope)",
                AskReason::Scope,
            ),
            (
                json!({"recommendation": "land", "confidence": "high", "reason_category": "discard"}),
                "the subagent design returned concern recommending land (high confidence, discard)",
                AskReason::Discard,
            ),
            (
                json!({"recommendation": "send_back", "confidence": "low"}),
                "the subagent design returned concern recommending send_back (low confidence)",
                AskReason::Scope,
            ),
            (
                json!({}),
                "the subagent design returned concern recommending nothing (no confidence)",
                AskReason::Scope,
            ),
        ] {
            let asking = under_pass(design_concern(fields));
            let route = asking.route(true);
            assert_eq!(
                (route.destination, route.parent, route.parent_lighter()),
                (Destination::Ask, Destination::Land, true),
                "{said}"
            );
            let combined = combined_verdict(&asking, &route, false);
            assert_eq!(combined.verdict, ReviewDecision::Concern, "{said}");
            assert_eq!(combined.reasons, ["design: departs"], "{said}");
            assert_eq!(
                landing_ask_reason(combined.reason_category),
                reason,
                "{said}"
            );
            let why = agents_escalation(&asking, &route, false);
            let asked = question(&combined, &why);
            for part in [
                said,
                "the review's own verdict pass was lighter",
                "- design: departs",
            ] {
                assert!(asked.contains(part), "{part} in {asked}");
            }
        }
        // A send_back on high confidence and a revise go back with the
        // agent's reasons, the verdict lighter.
        for (design, reasons) in [
            (
                design_concern(json!({"recommendation": "send_back", "confidence": "high"})),
                ["design: departs"],
            ),
            (
                json!({"agent": "design", "status": "completed", "verdict": "revise",
                    "reasons": ["add a test"], "summary": "s"}),
                ["design: add a test"],
            ),
        ] {
            let back = under_pass(design);
            let route = back.route(true);
            assert_eq!(route.destination, Destination::SendBack, "{reasons:?}");
            assert!(route.parent_lighter(), "{reasons:?}");
            assert_eq!(route.event_value()["parent_lighter"], true);
            let combined = combined_verdict(&back, &route, false);
            assert_eq!(combined.verdict, ReviewDecision::Revise);
            assert_eq!(combined.reasons, reasons);
        }
        // An agent's scope concern under a concern that would land.
        let landing = verdict(
            json!({"verdict": "concern", "reasons": [], "summary": "lands",
            "recommendation": "land", "confidence": "high",
            "agents": [design_concern(json!({"recommendation": "land", "confidence": "high",
                "reason_category": "scope"})), tests_pass]}),
        );
        let route = landing.route(true);
        assert_eq!(
            (route.destination, route.parent),
            (Destination::Ask, Destination::Land)
        );
        let parent_decides = route.parent == route.destination;
        let combined = combined_verdict(&landing, &route, parent_decides);
        let asked = question(
            &combined,
            &agents_escalation(&landing, &route, parent_decides),
        );
        for part in [
            "the subagent design returned concern recommending land (high confidence, scope)",
            "the review's own verdict concern was lighter",
            "- design: departs",
        ] {
            assert!(asked.contains(part), "{part} in {asked}");
        }
    }
}
