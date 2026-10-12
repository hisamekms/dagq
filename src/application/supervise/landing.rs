//! The review of an accepted run and its landing: the headless review,
//! its verdict, the conflict precheck, the `approve_landing` ask and the
//! landing itself (ADR-0023, ADR-0027).

use super::jobs::{ReviewAnswer, ReviewJob, ReviewJobInput, ReviewJobState};
use super::*;
use crate::application::agent_job::{self, AgentJob};
use crate::application::job_start_failure;
use crate::application::prompt::{PromptBytes, REVIEW_ACCESS};
use crate::application::review::{
    ReviewRange, SubagentSnapshot, review_range_at, snapshot_subagents,
};
use crate::application::review_programs::{ProgramSnapshot, snapshot_programs};
use crate::application::run_git;
use crate::domain::ActorContext;
use crate::domain::AskConfidence;
use crate::domain::EventKind;
use crate::domain::LandingAnswer;
use crate::domain::RecoveredLanding;
use crate::domain::actor_model::{ActorLaunch, ModelRole};
use crate::domain::concern::{
    self, ConcernDecision, ConcernReason, EscalatedBecause, LandingRecommendation,
};
use crate::domain::headless_job::{JobKind, JobSession, JobStop};
use crate::domain::review_jobs::{self, JobMiss, ReviewJobId};
use crate::domain::review_programs::{
    self as programs, AfterFailure, ProgramOutcome, ProgramsStep,
};
use crate::domain::review_reason;
use crate::domain::review_subagents::{Destination, VerdictRoute};
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
        let container = self.container.clone();
        let verification_tokens = self.verification_tokens.clone();
        let processes = self.processes.clone();
        let files = self.files.clone();
        let pid = self.layout.pid;
        let common_dir = path_text(&self.layout.common_dir)?;
        let push = RunHistory::from_events(&self.queue.run_events(run.id())?).landing_pushes();
        let generators = self.generators.clone();
        let load_average = self.host.load_average;
        let (disk_config, free_space) = (self.host.disk_config, self.host.free_space);
        let runs_dir = self.layout.runs_dir.clone();
        let db = self.layout.db.clone();
        // The supervisor asks; the Integrator checks and lands (ADR-t728-2).
        let request = IntegrationRequest {
            requester: self.layout.supervisor_actor(),
            run,
            previous,
            main,
            token: self.registration.token.clone(),
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
                    container: &*container,
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
                    verification_tokens: &*verification_tokens,
                },
                &request,
            )
        }))
    }
    /// Review an accepted run whose session stays open (ADR-0027 decision
    /// 1): first its program reviews ([`Self::start_review_programs`]),
    /// then, once they all pass, record `review_started` and start the
    /// headless review: write `review.md`, then run the reviewer's command
    /// with the task's acceptance and the verdict schema. A review that
    /// cannot even start is a failed one.
    pub(super) fn start_review(
        &mut self,
        run: &TaskRun,
        session: Option<SessionRef>,
    ) -> Result<Phase> {
        self.start_review_programs(run, session, false)
    }
    /// Start the program reviews of `run`'s next review (ADR-t1895-2):
    /// those the landing branch's commit configures whose paths the
    /// reviewed range changes, each script read at that commit, run one
    /// program job at a time in the configured order against the run's
    /// worktree. A review that needs none (no `[review.programs.*]`, or no
    /// program whose paths the range touches) goes to its agents at once
    /// and records nothing of them. `retried` is whether this is their one
    /// retry, after a program that could not be read, start or finish.
    fn start_review_programs(
        &mut self,
        run: &TaskRun,
        session: Option<SessionRef>,
        retried: bool,
    ) -> Result<Phase> {
        let events = self.queue.run_events(run.id())?;
        let history = RunHistory::from_events(&events);
        let attempt = history.review_attempts() + 1;
        let round = history.count(event_kind::REVIEW_PROGRAMS_STARTED) + 1;
        let snapshot = match self.review_programs(run) {
            Ok(Some(snapshot)) if !snapshot.programs.is_empty() => snapshot,
            Ok(_) => return self.resume_review(run, session, retried),
            Err(error) => {
                let error = format!("the review's programs could not be read: {error:#}");
                return self.review_programs_failed(run, (session, attempt, retried), None, error);
            }
        };
        let selected: Vec<Value> = snapshot
            .programs
            .iter()
            .map(|selected| {
                json!({
                    "name": selected.program.name,
                    "script": selected.program.script,
                    "matched": selected.matched,
                })
            })
            .collect();
        self.queue.record_runtime_event(
            run.id(),
            EventKind::ReviewProgramsStarted,
            json!({
                "attempt": attempt,
                "round": round,
                "retried": retried,
                "commit": snapshot.commit,
                "base": snapshot.range.base,
                "head": snapshot.range.head,
                "programs": selected,
            }),
        )?;
        info!(run_id = %run.id(), "run {}: {} program review(s) before review {attempt}", run.id(), snapshot.programs.len());
        self.go_on_with_programs(
            run,
            ReviewPrograms {
                session,
                attempt,
                round,
                retried,
                snapshot,
                outcomes: Vec::new(),
                detail: None,
            },
        )
    }
    /// The program reviews of `run`'s review from the landing branch's
    /// commit ([`snapshot_programs`]), selected by the receipt's range as
    /// the subagents are; `None` without `[review.programs.*]`.
    fn review_programs(&self, run: &TaskRun) -> Result<Option<ProgramSnapshot>> {
        snapshot_programs(
            &*self.repository,
            &|text| self.verifier.review_programs_in(text),
            &|main| self.receipt_range(run, main),
        )
    }
    /// The next step of `run`'s program reviews ([`programs::step`]): start
    /// the next program; once every one exited 0, go on to the agents'
    /// review; at the first that exited non-zero, send the worker back with
    /// its output, starting no other program and no agent (ADR-t1895-2
    /// decision 3); at the first that could not start or finish, fail as a
    /// review does (decision 4).
    fn go_on_with_programs(&mut self, run: &TaskRun, mut state: ReviewPrograms) -> Result<Phase> {
        loop {
            let step = programs::step(state.snapshot.programs.len(), &state.outcomes);
            let name = |index: usize| state.snapshot.programs[index].program.name.clone();
            match step {
                ProgramsStep::Run(index) => match self.start_run_program(run, &state, index) {
                    Ok(job) => {
                        return Ok(Phase::ReviewPrograms(ProgramsWatch {
                            programs: state,
                            job,
                        }));
                    }
                    Err(error) => {
                        let name = name(index);
                        let error = format!("the review program {name} could not start: {error:#}");
                        warn!(run_id = %run.id(), "run {}: {error}", run.id());
                        self.queue.record_runtime_event(
                            run.id(),
                            EventKind::ReviewProgramFinished,
                            json!({
                                "attempt": state.attempt,
                                "round": state.round,
                                "program": name,
                                "outcome": ProgramOutcome::StartFailed.as_str(),
                                "exit": null,
                                "duration_secs": 0,
                                "stdout_tail": null,
                                "stderr_tail": null,
                                "error": error,
                            }),
                        )?;
                        state.outcomes.push(ProgramOutcome::StartFailed);
                        state.detail = Some(error);
                    }
                },
                ProgramsStep::Passed => {
                    self.record_programs_finished(run, state.attempt, "passed", None, None, None)?;
                    info!(run_id = %run.id(), "run {}: its program reviews passed; its agents review it", run.id());
                    return self.resume_review(run, state.session, state.retried);
                }
                ProgramsStep::Rejected(index) => {
                    let name = name(index);
                    let reason = state.detail.take().unwrap_or_else(|| {
                        format!("the review program {name} rejected the change")
                    });
                    self.record_programs_finished(
                        run,
                        state.attempt,
                        "rejected",
                        Some(&name),
                        None,
                        Some(&reason),
                    )?;
                    info!(run_id = %run.id(), "run {}: the review program {name} rejected it; it goes back to its worker and no agent reviews it", run.id());
                    let verdict = ReviewVerdict {
                        verdict: ReviewDecision::Revise,
                        reasons: vec![reason],
                        reason_codes: Vec::new(),
                        summary: format!("the review program {name} rejected the change"),
                        recommendation: None,
                        confidence: None,
                        reason_category: None,
                        agents: Vec::new(),
                    };
                    let job = ActorContext::review_job(run.id(), state.attempt);
                    return self.send_revise(run, state.session, verdict, &job, None);
                }
                ProgramsStep::Failed(index, _) => {
                    let name = name(index);
                    let error = state
                        .detail
                        .take()
                        .unwrap_or_else(|| format!("the review program {name} failed"));
                    return self.review_programs_failed(
                        run,
                        (state.session, state.attempt, state.retried),
                        Some(&name),
                        error,
                    );
                }
            }
        }
    }
    /// Record `review_programs_finished` of the program reviews before
    /// review `attempt`, ended `outcome` at `program`: why they `failed`
    /// and whether they run again, or the `reason` the worker is sent back
    /// with.
    fn record_programs_finished(
        &mut self,
        run: &TaskRun,
        attempt: usize,
        outcome: &str,
        program: Option<&str>,
        failed: Option<(&str, bool)>,
        reason: Option<&str>,
    ) -> Result<()> {
        self.queue.record_runtime_event(
            run.id(),
            EventKind::ReviewProgramsFinished,
            json!({
                "attempt": attempt,
                "outcome": outcome,
                "program": program,
                "again": failed.is_some_and(|(_, again)| again),
                "error": failed.map(|(error, _)| error),
                "reason": reason,
            }),
        )?;
        Ok(())
    }
    /// The program reviews before review `attempt` failed with `error`, at
    /// `program` (none when they could not be read): they run once more
    /// from their first program unless this is their retry, and then the
    /// review fails to the person without an agent's job, as one that
    /// could not start (ADR-t1895-2 decision 4). The worker is not sent
    /// back.
    fn review_programs_failed(
        &mut self,
        run: &TaskRun,
        (session, attempt, retried): (Option<SessionRef>, usize, bool),
        program: Option<&str>,
        error: String,
    ) -> Result<Phase> {
        let again = programs::after_failure(retried) == AfterFailure::Again;
        self.record_programs_finished(
            run,
            attempt,
            "failed",
            program,
            Some((&error, again)),
            None,
        )?;
        if again {
            warn!(run_id = %run.id(), error = %error, "run {}: its program reviews failed: {error}; running them once more", run.id());
            return self.start_review_programs(run, session, true);
        }
        // No agent's job ran before this attempt, so none of its output
        // is named, whatever the programs' retry.
        self.unstarted_review_failed(run, session, attempt, error)
    }
    /// Go on from the program at `programs.outcomes.len()`, which ended
    /// as `end` after `duration_secs`, its time limit `timeout_secs`:
    /// record `review_program_finished` and take the next step.
    pub(super) fn review_program_ended(
        &mut self,
        run: &TaskRun,
        mut state: ReviewPrograms,
        end: &ProgramEnd,
        (duration_secs, timeout_secs): (u64, u64),
    ) -> Result<Phase> {
        let name = state.snapshot.programs[state.outcomes.len()]
            .program
            .name
            .clone();
        let outcome = ProgramOutcome::of_exit(end.exit.as_ref().map(|exit| exit.success));
        let exit = end.exit.as_ref().map(ToString::to_string);
        let error = (outcome == ProgramOutcome::TimedOut).then(|| {
            let output = self.program_output(run, state.round, &name);
            format!(
                "the review program {name} did not finish within {timeout_secs} seconds and was stopped; its output is in {output}.out and .err"
            )
        });
        self.queue.record_runtime_event(
            run.id(),
            EventKind::ReviewProgramFinished,
            json!({
                "attempt": state.attempt,
                "round": state.round,
                "program": name,
                "outcome": outcome.as_str(),
                "exit": exit,
                "duration_secs": duration_secs,
                "stdout_tail": end.stdout_tail,
                "stderr_tail": end.stderr_tail,
                "error": error,
            }),
        )?;
        state.detail = match outcome {
            ProgramOutcome::Rejected => Some(programs::rejection_reason(
                &name,
                exit.as_deref().unwrap_or("no exit status"),
                &end.stdout_tail,
                &end.stderr_tail,
            )),
            _ => error,
        };
        state.outcomes.push(outcome);
        self.go_on_with_programs(run, state)
    }
    /// Start the program at `index` of `state`'s programs as a program job
    /// against the run's worktree, its output in the run's directory and
    /// its script in the run's directory of `review-programs/`, which the
    /// worker cannot write.
    fn start_run_program(
        &self,
        run: &TaskRun,
        state: &ReviewPrograms,
        index: usize,
    ) -> Result<HeadlessJob> {
        let program = &state.snapshot.programs[index];
        let run_dir = PathBuf::from(run.run_dir().context("missing run directory")?);
        // A container run's programs work in its landing worktree, with
        // their git guarded (ADR-t2179-1 decisions 5 and 8).
        let worktree = run_git::review_tree(&*self.repository, run)?;
        anyhow::ensure!(
            self.files.is_dir(&worktree),
            "the run's worktree {} is gone",
            worktree.display()
        );
        let env = run_git::tree_env(&*self.repository, run, &worktree)?;
        let scratch = self.layout.review_programs_dir.join(run.id().as_str());
        let subject = JobSubject::review_program(run.id(), state.round, &program.program.name);
        let timeout = self.job_timeout(&subject);
        start_review_program(
            &self.job_ports(),
            self.spawner,
            self.programs,
            program,
            (&worktree, &env),
            (&run_dir, &scratch),
            (run.id(), state.round),
            timeout,
        )
    }
    /// Where the program `name` of the program reviews' `round` writes its
    /// output, without the `.out` / `.err` of its stdout and stderr.
    fn program_output(&self, run: &TaskRun, round: usize, name: &str) -> String {
        let stem = format!("review-program-{round}-{name}");
        run.run_dir().map_or(stem.clone(), |dir| {
            Path::new(dir).join(&stem).display().to_string()
        })
    }
    /// Start the review of `run`, or wait in [`Phase::ReviewHeld`] while
    /// none of its jobs can start ([`Self::begin_review`]), keeping
    /// `retried`: a review that waited starts again as the retry it was
    /// (one retry per review across the wait).
    pub(super) fn resume_review(
        &mut self,
        run: &TaskRun,
        session: Option<SessionRef>,
        retried: bool,
    ) -> Result<Phase> {
        self.begin_review(run, session, retried)
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
    /// Where the next start of a job of a review goes, `provider` the one
    /// its agent's `[review.subagents.<agent>]` names: such a job goes as a
    /// review whose `[roles.review]` names its provider (to it, else to the
    /// other provider with `[provider_fallback] jobs` on, else it waits for
    /// it, or, under `--no-claude`, moves off Claude), on the agent's own
    /// launch ([`crate::domain::actor_model::RoleModels::agent_launch`]);
    /// any other job goes by [`Self::review_route`].
    pub(super) fn review_job_route(&self, provider: Option<Provider>) -> ReviewRoute {
        let Some(provider) = provider else {
            return self.review_route();
        };
        let role = ModelRole::Review;
        provider::review_route(
            self.no_claude,
            (true, self.provider.fallback.jobs),
            self.role_models(role).agent_launch(role, provider),
            self.queue_hold,
            |provider| self.job_unusable(provider),
        )
    }
    /// Start the next review attempt of `run` (one more than its recorded
    /// reviews) where [`Self::review_route`] sends it, going to the person
    /// when no provider is left for its whole review. The agents its range
    /// selects are read from the landing branch before `review_started`,
    /// and a review whose agents or prompts cannot be read or made fails to
    /// the person without a job, never passing on fewer checks. It waits in
    /// [`Phase::ReviewHeld`] only while none of its jobs can start: neither
    /// its whole review's nor that of any agent that names its own provider
    /// ([`held_review`]); when one can, it starts and the jobs that cannot
    /// wait in it for their providers. Then its whole review's job and
    /// each selected agent's job start side by side, within the limit of
    /// agent jobs at once every run's review shares
    /// ([`Self::go_on_with_review`]). `retried` is whether this attempt is
    /// the retry of its program reviews, after which no job runs once more.
    pub(super) fn begin_review(
        &mut self,
        run: &TaskRun,
        session: Option<SessionRef>,
        retried: bool,
    ) -> Result<Phase> {
        let attempt =
            RunHistory::from_events(&self.queue.run_events(run.id())?).review_attempts() + 1;
        let live = match &session {
            Some(_) => session_alive(self, run.id())?,
            None => false,
        };
        let route = self.review_route();
        if let ReviewRoute::Manual(error) = route {
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
        // The selected agents, from the landing branch's commit
        // (ADR-t1453-1 decisions 3 and 4). When they cannot be known the
        // review cannot pass: it fails to the person with why.
        let subagents = match self.review_subagents(run) {
            Ok(subagents) => subagents,
            Err(snapshot) => {
                let error =
                    format!("the review's required subagents could not be read: {snapshot:#}");
                return self.unstarted_review_failed(run, session, attempt, error);
            }
        };
        let providers = agent_providers(subagents.as_ref());
        let session = match held_review(
            &route,
            &self.agent_routes(&providers),
            session,
            (retried, providers),
        ) {
            Ok((held, why)) => {
                info!(run_id = %run.id(), "run {}: its review waits: {why}", run.id());
                return Ok(held);
            }
            Err(session) => session,
        };
        // What the whole review starts with, or, while its route waits and
        // an agent's job starts, what its role gives it (its own start
        // records where it went).
        let launch = match route {
            ReviewRoute::Start(launch, _) => launch,
            _ => self
                .role_models(ModelRole::Review)
                .launch(ModelRole::Review),
        };
        // The Claude session id of the whole review's first job (ADR-0048
        // decision 4), which its review's span names: a whole review that
        // waits gets it too, and its first start on Claude takes it
        // ([`Self::start_review_job`]). Codex names its thread itself, which
        // the job's end records (ADR-t1063-1 decision 6).
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
        // What the whole review's prompt takes (task 1571, ADR-t1566-1
        // decision 6); each agent job's start records its own.
        let prepared = self.prepare_review(run, attempt, subagents.as_ref());
        if let Ok(prepared) = &prepared {
            started["prompt_bytes"] = json!(prepared.whole.1);
        }
        self.queue
            .record_runtime_event(run.id(), EventKind::ReviewStarted, started)?;
        let prepared = match prepared {
            Ok(prepared) => prepared,
            // Its own preparation failed: no job was tried.
            Err(error) => {
                let error = format!("the headless review could not start: {error:#}");
                warn!(run_id = %run.id(), error = %error, "run {}: {error}", run.id());
                self.close_review_session(run, None);
                return Ok(Phase::Exiting(ExitWatch::new(
                    session,
                    AfterExit::ReviewFailed {
                        attempt,
                        error,
                        duration_secs: 0,
                        // No job ran, so no output is named (task 426).
                        output: None,
                        session: None,
                    },
                )));
            }
        };
        let (prompt, prompt_bytes) = prepared.whole;
        let mut jobs = vec![ReviewJob {
            id: ReviewJobId::Whole,
            provider: None,
            input: ReviewJobInput::Whole {
                prompt,
                prompt_bytes,
                session_id,
            },
            again: false,
            state: ReviewJobState::Waiting(None),
            session: None,
        }];
        // An agent's provider as the landing branch's `dagq.toml` names it,
        // read with its paths (ADR-t1453-1 decision 4).
        let provider_of = |agent: &str| {
            subagents
                .iter()
                .flat_map(|snapshot| &snapshot.agents)
                .find(|snapshot| snapshot.name == agent)
                .and_then(|snapshot| snapshot.provider)
        };
        jobs.extend(prepared.agents.into_iter().map(|(agent, built)| ReviewJob {
            provider: provider_of(&agent),
            id: ReviewJobId::Agent(agent),
            input: ReviewJobInput::Agent(built),
            again: false,
            state: ReviewJobState::Waiting(None),
            session: None,
        }));
        info!(run_id = %run.id(), "run {} review {attempt}: {} job(s), the whole review and {} agent(s) (session {})", run.id(), jobs.len(), jobs.len() - 1, if live { "kept open" } else { "ended" });
        self.go_on_with_review(
            run,
            ReviewWatch {
                session,
                attempt,
                retried,
                started: self.generators.clock.monotonic(),
                jobs,
            },
        )
    }
    /// Fail review `attempt`, which did not start, to the person with
    /// `error` (`review_failed` and the `approve_landing` ask once the
    /// session exited): its required subagents could not be known or run.
    fn unstarted_review_failed(
        &mut self,
        run: &TaskRun,
        session: Option<SessionRef>,
        attempt: usize,
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
                output: None,
                session: None,
            },
        )))
    }
    /// Whether a review in [`Phase::ReviewHeld`] still waits: its whole
    /// review's route waits, and so does the job of each agent that names
    /// its own provider (`providers`, those the providers its selected
    /// agents name). A review that goes to the person does not wait.
    pub(super) fn review_held_waits(&self, providers: &[Provider]) -> bool {
        is_held(&self.review_route(), &self.agent_routes(providers))
    }
    /// Where the job of an agent that names each of `providers` would go
    /// now ([`Self::review_job_route`]).
    fn agent_routes(&self, providers: &[Provider]) -> Vec<ReviewRoute> {
        providers
            .iter()
            .map(|provider| self.review_job_route(Some(*provider)))
            .collect()
    }
    /// How many agent jobs of the reviews of the runs in the slots run now,
    /// the slot this pass stepped aside (it is out of the table while it
    /// steps): the limit of jobs at once counts them all
    /// ([`review_jobs::startable`]). The eval's jobs are not among them.
    pub(super) fn review_jobs_running(&self) -> usize {
        self.claim
            .slots
            .iter()
            .map(|slot| match &slot.phase {
                Phase::Review(watch) => watch.running(),
                _ => 0,
            })
            .sum()
    }
    /// Go on with the review of `run` in `watch` from where it stands
    /// ([`Self::tend_review`]): the next phase, or the review still.
    pub(super) fn go_on_with_review(
        &mut self,
        run: &TaskRun,
        mut watch: ReviewWatch,
    ) -> Result<Phase> {
        let elsewhere = self.review_jobs_running();
        Ok(match self.tend_review(run, &mut watch, elsewhere)? {
            Some(next) => next,
            None => Phase::Review(watch),
        })
    }
    /// One pass over the jobs of `run`'s review in `watch`, `elsewhere`
    /// the agent jobs of the other runs' reviews that run now: reap the
    /// jobs that ended ([`Self::review_job_ended`]), start those that
    /// wait while the limit of jobs at once leaves room and a provider can
    /// run them ([`Self::start_review_jobs`]), and once every job ended,
    /// judge the review ([`Self::judge_review`]) or, when one gave no
    /// answer, fail it to the person, never passing it on fewer judgments
    /// (ADR-t1895-1 decision 5). `None` while a job waits or runs.
    pub(super) fn tend_review(
        &mut self,
        run: &TaskRun,
        watch: &mut ReviewWatch,
        elsewhere: usize,
    ) -> Result<Option<Phase>> {
        let (attempt, retried) = (watch.attempt, watch.retried);
        for job in &mut watch.jobs {
            let ReviewJobState::Running { job: running, .. } = &mut job.state else {
                continue;
            };
            let agent = self.job_agent(running.provider).with_context(|| {
                format!("no {} runs on this supervisor", running.provider.as_str())
            })?;
            let Some(end) = running.poll_end(&*self.files, agent)? else {
                continue;
            };
            let ReviewJobState::Running {
                job: ended,
                switchable,
            } = std::mem::replace(&mut job.state, ReviewJobState::Waiting(None))
            else {
                unreachable!("the job runs");
            };
            self.review_job_ended(run, (attempt, retried), job, (ended, switchable), end)?;
        }
        self.start_review_jobs(run, watch, elsewhere)?;
        match watch.standing() {
            review_jobs::ReviewStanding::Open => Ok(None),
            review_jobs::ReviewStanding::Answered => self.judge_review(run, watch).map(Some),
            review_jobs::ReviewStanding::Missed(errors) => {
                let error = review_jobs_error(&errors);
                warn!(run_id = %run.id(), error = %error, "run {} review {attempt} failed: {error}; a person is asked", run.id());
                let session = whole_session(watch);
                self.close_review_session(run, session.as_ref());
                // The ask names the whole review's output when its job ran
                // and gave no answer; each missed job's error names its own.
                let whole_ran = watch.jobs.iter().any(|job| {
                    job.id == ReviewJobId::Whole
                        && matches!(job.state, ReviewJobState::Missed { ran: true, .. })
                });
                Ok(Some(Phase::Exiting(ExitWatch::new(
                    watch.session.clone(),
                    AfterExit::ReviewFailed {
                        attempt,
                        error,
                        duration_secs: watch.started.elapsed().as_secs(),
                        output: whole_ran.then_some(attempt),
                        session,
                    },
                ))))
            }
        }
    }
    /// Go on from `job` of review `attempt` of `run`, whose process
    /// `ended` (`switchable` as it started) and gave `end`: record
    /// `review_job_finished` with its Execution, and keep its answer, put
    /// it back to wait (for a provider it could not use, which is held, or
    /// as its one run once more) or give up on it. `retried` is whether the
    /// review is its program reviews' retry.
    fn review_job_ended(
        &mut self,
        run: &TaskRun,
        (attempt, retried): (usize, bool),
        job: &mut ReviewJob,
        (ended, switchable): (HeadlessJob, bool),
        end: std::result::Result<String, JobFailed>,
    ) -> Result<()> {
        let agent = self
            .job_agent(ended.provider)
            .with_context(|| format!("no {} runs on this supervisor", ended.provider.as_str()))?;
        let duration_secs = ended.started.elapsed().as_secs();
        let stdout = self.files.read_to_string(&ended.stdout).unwrap_or_default();
        // A provider that names its session itself (Codex) says its thread
        // and model in its output (ADR-t1063-1 decision 6); Claude's gives
        // the job's tokens only (ADR-t1486-1).
        let job_session = agent.job_session(&stdout, ended.started_at);
        let output = job.id.output(attempt, job.again);
        let mut finished = json!({
            "attempt": attempt,
            "agent": job.id.agent(),
            "again": job.again,
            "provider": ended.provider,
            "duration_secs": duration_secs,
            "output": output,
        });
        crate::domain::headless_job::JobSession::record_execution(
            job_session.as_ref(),
            &mut finished,
        );
        // The whole review's session, which the review's end and its span
        // name, whatever this run gave.
        job.session = job_session.map(identity_of);
        let (miss, error) = match end {
            Ok(reply) => match read_answer(&job.id, &reply) {
                Ok(answer) => {
                    finished["outcome"] = json!("answered");
                    finished["verdict"] = json!(answer.decision());
                    info!(run_id = %run.id(), "run {} review {attempt}: {} answered {}", run.id(), job.id.describe(), answer.decision().map_or("nothing", ReviewDecision::as_str));
                    job.state = ReviewJobState::Answered(answer);
                    return self.record_review_job_finished(run, finished);
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
        // A job stopped at a login that ran out or the usage limit is no
        // failure of the run: it joins the hold ask and waits with the
        // review's other jobs kept (task 438).
        let wall = self.job_wall(&ended);
        let hold = HoldJob::Review(run.id().clone());
        if let Some(wall) = wall
            && self.raise_job_wall(wall, &hold, &error)
        {
            info!(run_id = %run.id(), "run {} review {attempt}: {} stopped at the {} wall; it waits for the hold ask", run.id(), job.id.describe(), wall.as_str());
            finished["outcome"] = json!("held");
            finished["error"] = json!(error);
            return self.record_review_job_finished(run, finished);
        }
        // Another provider that cannot be used (Codex's login, usage limit
        // or start) is held, and a job whose role names its provider starts
        // again where the route sends it (ADR-t1063-1 decisions 4 and 5,
        // ADR-t1857-1).
        let mut unusable = wall.is_some();
        if ended.provider != Provider::Claude {
            let said = format!("{error}\n{stdout}");
            let failure = self.job_failure(&ended);
            if let Some((provider, reason)) = self.job_provider_failed(
                ended.provider,
                failure,
                (&error, &said),
                &hold,
                switchable,
            ) {
                match after_unusable(self.review_job_route(job.provider), provider) {
                    AfterUnusable::Fail => unusable = true,
                    AfterUnusable::Start(..) | AfterUnusable::Wait(_) => {
                        warn!(run_id = %run.id(), error = %error, "run {} review {attempt}: {} cannot be used ({}) for {}; it starts again where its route sends it", run.id(), provider.as_str(), reason.as_str(), job.id.describe());
                        finished["outcome"] = json!("held");
                        finished["error"] = json!(error);
                        finished["switch_reason"] = json!(reason.as_str());
                        return self.record_review_job_finished(run, finished);
                    }
                }
            }
        }
        finished["outcome"] = json!(miss.as_str());
        finished["error"] = json!(error);
        let again = self.missed_review_job(run, (attempt, retried), job, miss, &error, unusable);
        finished["runs_again"] = json!(again);
        self.record_review_job_finished(run, finished)
    }
    /// Put `job` of review `attempt`, which `missed` with `error`, back to
    /// wait as its one run once more when it runs again
    /// ([`review_jobs::runs_again`]), else give up on it with why and where
    /// its output is; whether it runs again. A job whose provider was
    /// found unusable and that did not move off it (`unusable`) is not run
    /// again: that failure is the provider's, and goes to the person.
    fn missed_review_job(
        &mut self,
        run: &TaskRun,
        (attempt, retried): (usize, bool),
        job: &mut ReviewJob,
        missed: JobMiss,
        error: &str,
        unusable: bool,
    ) -> bool {
        let manual = matches!(self.review_job_route(job.provider), ReviewRoute::Manual(_));
        let again = !unusable
            && review_jobs::runs_again(
                self.landing.retry_unreadable_review,
                job.again,
                retried,
                manual,
            );
        if again {
            warn!(run_id = %run.id(), error = %error, "run {} review {attempt}: {} {}: {error}; it runs once more", run.id(), job.id.describe(), missed.as_str());
            job.again = true;
            job.state = ReviewJobState::Waiting(None);
            return true;
        }
        let path = |again: bool| {
            let stem = job.id.output(attempt, again);
            run.run_dir().map_or(stem.clone(), |dir| {
                Path::new(dir).join(&stem).display().to_string()
            })
        };
        // Where the output of the run that missed is, or of the run before
        // when this one could not start.
        let (why, ran) = match (missed, job.again) {
            (JobMiss::StartFailed, false) => (format!("{}: {error}", job.id.describe()), false),
            (JobMiss::StartFailed, true) => (
                format!(
                    "{}: {error}; its earlier run's output is in {}.out and .err",
                    job.id.describe(),
                    path(false)
                ),
                true,
            ),
            (_, again) => (
                format!(
                    "{}: {error}; its output is in {}.out and .err",
                    job.id.describe(),
                    path(again)
                ),
                true,
            ),
        };
        job.state = ReviewJobState::Missed { why, ran };
        false
    }
    /// Record `review_job_finished` of a job of `run`'s review.
    fn record_review_job_finished(&mut self, run: &TaskRun, finished: Value) -> Result<()> {
        self.queue
            .record_runtime_event(run.id(), EventKind::ReviewJobFinished, finished)?;
        Ok(())
    }
    /// Start the jobs of `run`'s review that wait, in order, while the
    /// limit of agent jobs at once (`[review.jobs] concurrency`, which
    /// `elsewhere`, the other reviews' running jobs, shares) leaves room and
    /// [`Self::review_job_route`] sends them to a provider: each job goes
    /// where its route sends it when it starts, so one may run on a
    /// provider its review's other jobs could not use. While the route waits, the job
    /// waits; with no provider left (`--no-claude`), it gives no answer and
    /// the review goes to the person.
    fn start_review_jobs(
        &mut self,
        run: &TaskRun,
        watch: &mut ReviewWatch,
        elsewhere: usize,
    ) -> Result<()> {
        let limit = self.review_jobs.concurrency();
        let (attempt, retried) = (watch.attempt, watch.retried);
        for index in 0..watch.jobs.len() {
            if !matches!(watch.jobs[index].state, ReviewJobState::Waiting(_)) {
                continue;
            }
            if review_jobs::startable(limit, elsewhere + watch.running(), 1) == 0 {
                let why = format!("{limit} agent jobs of the reviews run at once");
                note_review_job_wait(run, attempt, &mut watch.jobs[index], why);
                continue;
            }
            match self.review_job_route(watch.jobs[index].provider) {
                ReviewRoute::Wait(why) => {
                    note_review_job_wait(run, attempt, &mut watch.jobs[index], why)
                }
                ReviewRoute::Manual(error) => {
                    warn!(run_id = %run.id(), "run {} review {attempt}: {} cannot run: {error}", run.id(), watch.jobs[index].id.describe());
                    watch.jobs[index].state = ReviewJobState::Missed {
                        why: error,
                        ran: false,
                    };
                }
                ReviewRoute::Start(launch, switchable) => {
                    let job = &mut watch.jobs[index];
                    self.start_review_job(run, (attempt, retried), job, launch, switchable)?;
                }
            }
        }
        Ok(())
    }
    /// Start `job` of review `attempt` of `run` on `launch` and record
    /// `review_job_started`. A start its provider failed holds that
    /// provider when `[roles.review]` or the agent's
    /// `[review.subagents.<agent>]` names it (`switchable`), and the job
    /// starts at once where the route sends it then, or waits for it
    /// (ADR-t1063-1 decisions 4 and 5, ADR-t1857-1); any other failure to
    /// start is a miss ([`Self::missed_review_job`]).
    fn start_review_job(
        &mut self,
        run: &TaskRun,
        (attempt, retried): (usize, bool),
        job: &mut ReviewJob,
        mut launch: ActorLaunch,
        mut switchable: bool,
    ) -> Result<()> {
        loop {
            let output = job.id.output(attempt, job.again);
            // The whole review's Claude session id `review_started` named,
            // or a new one for its later starts; an agent job is given none,
            // as the eval's is not.
            let session_id = match &mut job.input {
                ReviewJobInput::Whole { session_id, .. }
                    if launch.provider == crate::domain::Provider::Claude =>
                {
                    Some(
                        session_id
                            .take()
                            .unwrap_or_else(|| self.generators.ids.uuid()),
                    )
                }
                _ => None,
            };
            let spawned =
                self.spawn_review_job(run, attempt, job, &output, session_id.as_deref(), &launch);
            let error = match spawned {
                Ok(Ok((child, stdout, stderr))) => {
                    let started = self.headless_job(
                        "review",
                        child,
                        stdout,
                        stderr,
                        JobSubject {
                            provider: launch.provider,
                            label: job.id.agent().map(str::to_owned),
                            ..JobSubject::review(JobKind::Agent, run.id(), attempt)
                        },
                    );
                    let prompt_bytes = match &job.input {
                        ReviewJobInput::Whole { prompt_bytes, .. } => json!(prompt_bytes),
                        ReviewJobInput::Agent(built) => json!(built.prompt.bytes),
                    };
                    // Watched before its start is recorded, so a failed
                    // record leaves no job running unwatched.
                    job.state = ReviewJobState::Running {
                        job: started,
                        switchable,
                    };
                    self.queue.record_runtime_event(
                        run.id(),
                        EventKind::ReviewJobStarted,
                        json!({
                            "attempt": attempt,
                            "agent": job.id.agent(),
                            "again": job.again,
                            "launch": launch.to_value(),
                            "session_id": session_id,
                            "output": output,
                            "prompt_bytes": prompt_bytes,
                        }),
                    )?;
                    info!(run_id = %run.id(), "run {} review {attempt}: {} started on {}", run.id(), job.id.describe(), launch.provider.as_str());
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
                        let next = after_unusable(self.review_job_route(job.provider), provider);
                        if !matches!(next, AfterUnusable::Fail) {
                            warn!(run_id = %run.id(), error = %error, "run {} review {attempt}: {} cannot be used ({}) for {}", run.id(), provider.as_str(), reason.as_str(), job.id.describe());
                            self.record_review_job_finished(
                                run,
                                json!({
                                    "attempt": attempt,
                                    "agent": job.id.agent(),
                                    "again": job.again,
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
                                note_review_job_wait(run, attempt, job, why);
                                return Ok(());
                            }
                            AfterUnusable::Fail => {
                                self.start_missed(
                                    run,
                                    (attempt, retried),
                                    job,
                                    &output,
                                    &error,
                                    true,
                                )?;
                                return Ok(());
                            }
                        }
                    }
                    error
                }
                // Its own preparation failed: no provider was tried.
                Err(prepared) => format!("it could not start: {prepared:#}"),
            };
            return self.start_missed(run, (attempt, retried), job, &output, &error, false);
        }
    }
    /// Record that `job`, writing to `output`, could not start with
    /// `error`, and run it once more or give up on it
    /// ([`Self::missed_review_job`]).
    fn start_missed(
        &mut self,
        run: &TaskRun,
        (attempt, retried): (usize, bool),
        job: &mut ReviewJob,
        output: &str,
        error: &str,
        unusable: bool,
    ) -> Result<()> {
        warn!(run_id = %run.id(), error = %error, "run {} review {attempt}: {} {error}", run.id(), job.id.describe());
        let mut finished = json!({
            "attempt": attempt,
            "agent": job.id.agent(),
            "again": job.again,
            "outcome": JobMiss::StartFailed.as_str(),
            "error": error,
            "duration_secs": 0,
            "output": output,
        });
        let again = self.missed_review_job(
            run,
            (attempt, retried),
            job,
            JobMiss::StartFailed,
            error,
            unusable,
        );
        finished["runs_again"] = json!(again);
        self.record_review_job_finished(run, finished)
    }
    /// Judge `run`'s review once every job of `watch` answered: the whole
    /// review's verdict beside each agent's result
    /// ([`review_jobs::assemble`]), recorded as `review_finished` with where
    /// the heaviest of their judgments sends the run, and gone on with
    /// ([`Self::act_on_verdict`]).
    fn judge_review(&mut self, run: &TaskRun, watch: &ReviewWatch) -> Result<Phase> {
        let attempt = watch.attempt;
        let mut whole = None;
        let mut agents = Vec::new();
        for job in &watch.jobs {
            if let ReviewJobState::Answered(answer) = &job.state {
                match answer {
                    ReviewAnswer::Whole(verdict) => whole = Some(verdict.clone()),
                    ReviewAnswer::Agent(result) => agents.push(result.clone()),
                }
            }
        }
        let verdict = review_jobs::assemble(whole.context("the whole review's verdict")?, agents);
        let run = self.queue.run(run.id())?;
        let job = ActorContext::review_job(run.id(), attempt);
        let mut finished = json!({
            "verdict": verdict.verdict,
            "reasons": verdict.reasons,
            "reason_codes": verdict.recorded_codes(),
            "primary_code": verdict.primary_code(),
            "summary": verdict.summary,
            "duration_secs": watch.started.elapsed().as_secs(),
            "attempt": attempt,
            "recommendation": verdict.recommendation,
            "confidence": verdict.confidence,
            "reason_category": verdict.reason_category,
        });
        // Each agent's result, and where they and the whole review send the
        // run (ADR-t1453-1 decisions 5 and 7); a review with no agent
        // records as before.
        if !verdict.agents.is_empty() {
            let revise_left = self.revise_left(run.id())?;
            finished["agents"] = json!(verdict.agents);
            finished["route"] = verdict.route(revise_left).event_value();
        }
        // The whole review's session, which its span names; each job's
        // Execution is on its `review_job_finished`.
        if let Some(session) = whole_session(watch) {
            session.record(&mut finished);
        }
        let session = watch.session.clone();
        self.for_job(&job, |sv| {
            sv.queue
                .record_runtime_event(run.id(), EventKind::ReviewFinished, finished)?;
            info!(run_id = %run.id(), "run {} review {attempt}: {} ({})", run.id(), verdict.verdict.as_str(), verdict.summary);
            sv.act_on_verdict(&run, session, verdict, &job, attempt)
        })
    }
    /// The agents' review's range from the landing branch's commit
    /// ([`snapshot_subagents`]); the range is the receipt's, read only when
    /// agents are configured.
    pub(super) fn review_subagents(&self, run: &TaskRun) -> Result<Option<SubagentSnapshot>> {
        snapshot_subagents(
            &*self.repository,
            &|text| self.verifier.review_subagents_in(text),
            &|main| self.receipt_range(run, main),
        )
    }
    /// The range `run`'s review reads with the landing branch at `main`:
    /// from its receipt's commit ([`review_range_at`]).
    fn receipt_range(&self, run: &TaskRun, main: &CommitSha) -> Result<ReviewRange> {
        let receipt_path = Path::new(run.receipt_path().context("missing receipt path")?);
        let receipt = Receipt::parse(
            &self
                .files
                .read_to_string(receipt_path)
                .with_context(|| format!("read receipt {}", receipt_path.display()))?,
        )?;
        review_range_at(&*self.repository, run, &receipt, main)
    }
    /// Write the review's material and its jobs' prompts, held to their
    /// limits: the whole review's prompt with the known failures and what it takes (task 1571),
    /// which `review_started` records, and each selected agent's job built
    /// from the definition the landing branch's snapshot returned
    /// ([`agent_job::build`], the eval's own assembly), its prompt in its
    /// own directory of the run's.
    pub(super) fn prepare_review(
        &mut self,
        run: &TaskRun,
        attempt: usize,
        subagents: Option<&SubagentSnapshot>,
    ) -> Result<PreparedReview> {
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
        let language = self.verifier.language();
        // The whole review's job only: it decides whether to send the run
        // back (ADR-t1920-1 decision 5).
        let known_failures = self.known_failures(
            task.id(),
            &run_dir.join(format!("review-{attempt}-known-failures.json")),
        );
        let fitted = review_prompt(&task, run, &path, known_failures.as_ref())
            .with_language(language.as_ref());
        self.files.write(
            &run_dir.join(format!("review-prompt-{attempt}.txt")),
            fitted.text.as_bytes(),
        )?;
        let worktree = run_git::review_tree(&*self.repository, run)?;
        let mut agents = Vec::new();
        for (snapshot, agent) in subagents
            .iter()
            .flat_map(|snapshot| snapshot.agents.iter().map(move |agent| (snapshot, agent)))
        {
            let dir = run_dir
                .join(format!("review-{attempt}-agents"))
                .join(&agent.name);
            self.files
                .create_dir_all(&dir)
                .with_context(|| format!("create {}", dir.display()))?;
            let built = agent_job::build(&AgentJob {
                agent: &agent.name,
                definition: &agent.definition,
                tools: &agent.tools,
                base: &snapshot.range.base,
                head: &snapshot.range.head,
                material: Path::new(&path),
                cwd: &worktree,
                dir: &dir,
                language: language.as_ref(),
            })
            .with_context(|| format!("the job of the agent {}", agent.name))?;
            self.files
                .write(&dir.join("prompt.txt"), built.prompt.text.as_bytes())?;
            agents.push((agent.name.clone(), built));
        }
        Ok(PreparedReview {
            whole: (fitted.text, fitted.bytes),
            agents,
        })
    }
    /// Start `job` of review `attempt` writing to `output` (its stem in the
    /// run directory) on `launch`'s provider: the whole review's job
    /// through `AgentProvider::review_command`, an agent's through
    /// `AgentProvider::agent_job_command` as the eval starts it. The outer
    /// error is the job's own preparation, the inner one the start of its
    /// provider's process: only the latter says whether the provider can be
    /// used (ADR-t1063-1 decision 4).
    fn spawn_review_job(
        &mut self,
        run: &TaskRun,
        attempt: usize,
        job: &ReviewJob,
        output: &str,
        session_id: Option<&str>,
        launch: &ActorLaunch,
    ) -> Result<Result<StartedReview>> {
        let run_dir = PathBuf::from(run.run_dir().context("missing run directory")?);
        let stdout = run_dir.join(format!("{output}.out"));
        let stderr = run_dir.join(format!("{output}.err"));
        let worktree = run_git::review_tree(&*self.repository, run)?;
        // A worktree that is gone is the run's, not its provider's: its
        // spawn would read as a missing executable.
        anyhow::ensure!(
            self.files.is_dir(&worktree),
            "the run's worktree {} is gone",
            worktree.display()
        );
        // The repository's [run.env] reaches the review too (ADR-0023
        // decision 3), and the guard of a container run's landing worktree
        // reaches the git its agents run (ADR-t2179-1 decision 8).
        let mut env = self.verifier.run_env(&run_dir)?;
        env.extend(run_git::tree_env(&*self.repository, run, &worktree)?);
        // The review on either provider may not start the sccache server
        // (ADR-t2086-1).
        let look = self.sccache_look(crate::domain::sccache::CheckReason::BeforeReview, &run_dir);
        let named = crate::domain::sccache::SccacheTarget::of_pairs(&env).is_some();
        let without_env = look.apply(&mut env);
        if named {
            self.record_wrapper_removed(
                run.id(),
                &look,
                json!({"job": "review", "attempt": attempt, "agent": job.id.agent()}),
            );
        }
        let started = (|| {
            let agent = self.job_agent(launch.provider).with_context(|| {
                format!("no {} runs on this supervisor", launch.provider.as_str())
            })?;
            let program = match &job.input {
                // Like the observer's job: the CLI knows the review by its
                // role and allows it only reads of this queue.
                ReviewJobInput::Whole { prompt, .. } => HeadlessProgram::Review {
                    run,
                    tree: &worktree,
                    prompt,
                    access: REVIEW_ACCESS,
                },
                // One agent's own job, given no way to read the queue, as
                // the eval's is.
                ReviewJobInput::Agent(built) => HeadlessProgram::AgentJob(&built.launch),
            };
            let without_mcp = matches!(job.input, ReviewJobInput::Agent(_));
            self.actors_on(agent)
                .spawn(
                    ActorExecutionSpec::new(
                        ActorContext::review_job(run.id(), attempt),
                        WorkspaceAccess::Read(worktree.clone()),
                        ActorProgram::Headless {
                            program,
                            session_id,
                            launch: Some(launch),
                            without_mcp,
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
        // A clone reaches main only through its bundle (ADR-t2179-1
        // decision 6).
        if run.in_container() {
            crate::application::run_git::hand_main(
                &*self.repository,
                run,
                &self.repository.main_head()?,
            )?;
        }
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
        let conflicts = match crate::application::run_git::for_run(&*self.repository, run)
            .and_then(|repository| repository.merge_conflicts(main.as_str(), head.as_str()))
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
                    known_failures: self
                        .request_known_failures(run, &format!("conflict-{attempt}")),
                };
                crate::application::run_git::hand_main(&*self.repository, run, &main)?;
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
                close_workspace(
                    &mut *self.queue,
                    self.sessions,
                    &self.registration.token,
                    run,
                )
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
            let Some((action, cause)) = landing_answer_to_apply(&answer, run.status(), || {
                Ok(self.queue.run_lease(&run_id)?.is_some())
            })?
            else {
                continue;
            };
            match self.apply_landing_answer(&run, ask.id, &answer, action) {
                Ok(()) => self.record_rest(run.id(), None, cause),
                Err(error) => {
                    warn!(run_id = %run.id(), ask_id = %ask.id, error = %format_args!("{error:#}"), "run {}: the answer {answer:?} of ask {} could not be applied: {error:#}", run.id(), ask.id);
                }
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
        let held = self.landing.run_env_missing
            || self.verification_container_held()
            || self.observation.ci.held()
            || self.landing.unresolved
            || self.host.disk.landing_short;
        if approved_landing_waits(held, self.used_slots(), parallel, || {
            Ok(!self
                .queue
                .runs_with_status(RunStatus::Integrating)?
                .is_empty())
        })? {
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
        let Some(run) = first_to_land(queued) else {
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
            let leased = self
                .queue
                .lease_for_e2e(run.id(), &self.registration.token)?;
            drop(guard);
            if let Some(run) = leased {
                info!(run_id = %run.id(), "run {} runs its e2e before it lands as queued", run.id());
                self.claim.slots.admit_noting(
                    Slot::new(run, Phase::AwaitingSlot),
                    "e2e_leased",
                    &*self.queue,
                );
            }
            return Ok(());
        };
        let landing = self
            .queue
            .begin_integration(run.id(), &self.registration.token, &main)?;
        drop(guard);
        info!(run_id = %run.id(), "run {} lands onto main {main} as queued", run.id());
        let handle = self.spawn_landing(landing.clone(), RunStatus::AwaitingIntegration, main)?;
        self.claim.slots.admit_noting(
            Slot::new(landing, Phase::Landing(Some(handle))),
            EventKind::IntegrationStarted.as_str(),
            &*self.queue,
        );
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
            let leased = self
                .queue
                .lease_for_review(run.id(), &self.registration.token)?;
            drop(guard);
            let Some(run) = leased else {
                continue;
            };
            info!(run_id = %run.id(), task_id = %run.task_id(), "run {} of task {} is reviewed again: its landing was given up before it was approved or passed", run.id(), run.task_id());
            let session = self.session_of(&run)?;
            match self.start_review(&run, session) {
                Ok(phase) => self.claim.slots.admit_noting(
                    Slot::new(run, phase),
                    EventKind::ReviewStarted.as_str(),
                    &*self.queue,
                ),
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
                close_ended_landing_asks(&mut *self.queue, Some(run.task_id()));
                self.request_cleanup(Some(run.task_id()));
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

/// The answer of an `approve_landing` ask a pass applies now, with the
/// phase it moves the run to (queued to land, sent back for a resume, or
/// ended by the cancel): one [`LandingAnswer::parse`] reads, of a run
/// awaiting integration that nobody leases (`leased`, read only then).
/// The slots and the integration slot do not matter (task 949); any other
/// answer is left to the inbox.
pub(super) fn landing_answer_to_apply(
    answer: &str,
    status: RunStatus,
    leased: impl FnOnce() -> Result<bool>,
) -> Result<Option<(LandingAnswer, &'static str)>> {
    let Some(action) = LandingAnswer::parse(answer) else {
        return Ok(None);
    };
    if status != RunStatus::AwaitingIntegration || leased()? {
        return Ok(None);
    }
    let cause = match &action {
        LandingAnswer::Land => EventKind::IntegrationApproved.as_str(),
        LandingAnswer::SendBack(_) => "sent_back",
        LandingAnswer::Cancel => "canceled",
    };
    Ok(Some((action, cause)))
}

/// Whether a run queued to land waits on this pass: a hold of the landing
/// (`held`: a missing `[run.env]` program, the CI hold, an unresolved
/// landing branch or short disk space), no free slot (`used` of
/// `parallel`), or another run integrating (`integrating`, read only when
/// nothing before it waits).
pub(super) fn approved_landing_waits(
    held: bool,
    used: usize,
    parallel: usize,
    integrating: impl FnOnce() -> Result<bool>,
) -> Result<bool> {
    Ok(held || used >= parallel || integrating()?)
}

/// The run of the oldest of `queued`, each with the event that queued it
/// to land: approved runs land in the order they were approved.
pub(super) fn first_to_land<R>(mut queued: Vec<(EventId, R)>) -> Option<R> {
    queued.sort_by_key(|(approval, _)| *approval);
    queued.into_iter().next().map(|(_, run)| run)
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

/// Whether a review waits as a whole: its whole review's route (`whole`)
/// waits, and so does the route of every agent's job that names its own
/// provider (`agents`). When any of them can start, the review starts and
/// only the jobs that cannot wait in it, each for its provider.
pub(super) fn is_held(whole: &ReviewRoute, agents: &[ReviewRoute]) -> bool {
    matches!(whole, ReviewRoute::Wait(_))
        && agents
            .iter()
            .all(|route| matches!(route, ReviewRoute::Wait(_)))
}

/// The phase of a review that waits as a whole ([`is_held`]) on `whole`
/// and `agents`, with why: [`Phase::ReviewHeld`] keeping whether the
/// review is the retry (`retried`), so that the review that starts after
/// the wait is the retry it was and is not retried again (one retry per
/// review across the wait), and the providers its agents name
/// (`providers`), whose jobs end the wait once one of them can start.
/// `Err` gives the session back when the review does not wait.
pub(super) fn held_review<'a>(
    whole: &'a ReviewRoute,
    agents: &[ReviewRoute],
    session: Option<SessionRef>,
    (retried, providers): (bool, Vec<Provider>),
) -> std::result::Result<(Phase, &'a str), Option<SessionRef>> {
    match whole {
        ReviewRoute::Wait(why) if is_held(whole, agents) => Ok((
            Phase::ReviewHeld {
                session,
                retried,
                providers,
            },
            why,
        )),
        _ => Err(session),
    }
}

/// The providers the agents of `subagents` name, each once, in their
/// order: those whose jobs may start while the whole review's waits.
pub(super) fn agent_providers(subagents: Option<&SubagentSnapshot>) -> Vec<Provider> {
    let mut providers = Vec::new();
    for provider in subagents
        .iter()
        .flat_map(|snapshot| &snapshot.agents)
        .filter_map(|agent| agent.provider)
    {
        if !providers.contains(&provider) {
            providers.push(provider);
        }
    }
    providers
}

/// What a review prepared for its jobs ([`Supervisor::prepare_review`]):
/// the whole review's prompt and what it takes, and each selected agent's
/// job by the agent's name, in their configured order.
pub(super) struct PreparedReview {
    pub(super) whole: (String, PromptBytes),
    pub(super) agents: Vec<(String, agent_job::BuiltAgentJob)>,
}

/// Where a job of a review goes once the provider it `failed` on was found
/// unusable and held, as the review's route reads then
/// ([`provider::review_moves`]): it starts on the provider the route
/// sends it to (the other, with `switched_from` and `switch_reason`, when
/// `[provider_fallback] jobs` is on), or waits (for either, or with the
/// fallback off for the one that failed, once its hold ends); it gives no
/// answer when the route still sends it to the provider that failed (the
/// hold was not written) or to the person (`--no-claude`).
#[derive(Debug)]
pub(super) enum AfterUnusable {
    Start(ActorLaunch, bool),
    Wait(String),
    Fail,
}

/// [`AfterUnusable`] of a job on `failed` under `route`.
pub(super) fn after_unusable(route: ReviewRoute, failed: Provider) -> AfterUnusable {
    if !provider::review_moves(&route, failed) {
        return AfterUnusable::Fail;
    }
    match route {
        ReviewRoute::Start(launch, switchable) => AfterUnusable::Start(launch, switchable),
        ReviewRoute::Wait(why) => AfterUnusable::Wait(why),
        ReviewRoute::Manual(_) => AfterUnusable::Fail,
    }
}

/// What the job `id` of a review answered in its `reply`, as its provider
/// read it out: the whole review's verdict, which names no agent's result,
/// or the one completed result of the agent, read as the eval reads an
/// agent job's ([`agent_job::read_reply`]); else why it is unreadable.
fn read_answer(id: &ReviewJobId, reply: &str) -> std::result::Result<ReviewAnswer, String> {
    match id {
        ReviewJobId::Whole => ReviewVerdict::parse(reply).map(ReviewAnswer::Whole),
        ReviewJobId::Agent(agent) => {
            review_jobs::agent_result(agent, agent_job::read_reply(reply)).map(ReviewAnswer::Agent)
        }
    }
}

/// `session` without its tokens: what the review's end names of the whole
/// review's job (its span reads the session and model), whose Execution
/// its `review_job_finished` holds.
fn identity_of(session: JobSession) -> JobSession {
    JobSession {
        tokens: None,
        ..session
    }
}

/// The session of the last run of the whole review's job of `watch`.
fn whole_session(watch: &ReviewWatch) -> Option<JobSession> {
    watch
        .jobs
        .iter()
        .find(|job| job.id == ReviewJobId::Whole)
        .and_then(|job| job.session.clone())
}

/// Keep `job` of review `attempt` of `run` waiting for `why`, logged once
/// for each reason it waits for.
fn note_review_job_wait(run: &TaskRun, attempt: usize, job: &mut ReviewJob, why: String) {
    if matches!(&job.state, ReviewJobState::Waiting(Some(known)) if *known == why) {
        return;
    }
    info!(run_id = %run.id(), "run {} review {attempt}: {} waits: {why}", run.id(), job.id.describe());
    job.state = ReviewJobState::Waiting(Some(why));
}

/// The error of a review whose jobs `errors` gave no answer: each job's
/// why, or the `--no-claude` one alone (`review_failed` records it as
/// `provider_disabled`: no review agent ran).
fn review_jobs_error(errors: &[String]) -> String {
    if let Some(disabled) = errors
        .iter()
        .find(|error| error.starts_with(REVIEW_PROVIDER_DISABLED))
    {
        return disabled.clone();
    }
    format!(
        "the review's jobs did not all give an answer: {}",
        errors.join("; ")
    )
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

    /// A verdict as a review assembles and records it, its agents'
    /// results beside the whole review's.
    fn verdict(json: Value) -> ReviewVerdict {
        serde_json::from_value(json).unwrap()
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

    /// The answers a pass applies at once, whatever the slots (task 949):
    /// `land`, `send_back` (with the person's reason) and `cancel`, each
    /// with the phase it moves the run to, of a run awaiting integration
    /// that nobody leases. Any other answer is the inbox's; a run in
    /// another status or leased waits, its lease read only when the answer
    /// applies.
    #[test]
    fn a_landing_answer_is_applied_to_an_unleased_run_awaiting_integration() {
        let apply = |answer: &str, status: RunStatus, leased: bool| {
            landing_answer_to_apply(answer, status, || Ok(leased)).unwrap()
        };
        let waiting = RunStatus::AwaitingIntegration;
        assert_eq!(
            apply("land", waiting, false),
            Some((LandingAnswer::Land, "integration_approved"))
        );
        assert_eq!(
            apply("send_back: add a test", waiting, false),
            Some((
                LandingAnswer::SendBack(Some("add a test".to_owned())),
                "sent_back"
            ))
        );
        assert_eq!(
            apply("cancel", waiting, false),
            Some((LandingAnswer::Cancel, "canceled"))
        );
        assert_eq!(apply("land it", waiting, false), None);
        assert_eq!(apply("land", waiting, true), None);
        for status in [
            RunStatus::Running,
            RunStatus::Integrating,
            RunStatus::Failed,
        ] {
            assert_eq!(apply("land", status, false), None, "{status:?}");
        }
        let unread = || -> Result<bool> { panic!("the lease is read") };
        assert_eq!(
            landing_answer_to_apply("why?", waiting, unread).unwrap(),
            None
        );
        assert_eq!(
            landing_answer_to_apply("land", RunStatus::Running, unread).unwrap(),
            None
        );
    }

    /// A run a `land` answer queued waits while the only slot runs
    /// another task, while another run integrates, or under a hold of the
    /// landing; it starts once a slot is free and nothing integrates.
    #[test]
    fn an_approved_run_waits_for_a_slot_and_the_integration_slot() {
        let waits = |held: bool, used: usize, integrating: bool| {
            approved_landing_waits(held, used, 1, || Ok(integrating)).unwrap()
        };
        assert!(!waits(false, 0, false));
        assert!(waits(false, 1, false));
        assert!(waits(false, 0, true));
        assert!(waits(true, 0, false));
        let unread = || -> Result<bool> { panic!("the integrating runs are read") };
        assert!(approved_landing_waits(true, 0, 1, unread).unwrap());
        assert!(approved_landing_waits(false, 4, 4, unread).unwrap());
    }

    /// Approved runs land oldest approval first, whatever order the runs
    /// are read in.
    #[test]
    fn the_oldest_approval_lands_first() {
        let queued = vec![
            (EventId::new(9), "approved last"),
            (EventId::new(3), "approved first"),
            (EventId::new(5), "approved second"),
        ];
        assert_eq!(first_to_land(queued), Some("approved first"));
        assert_eq!(first_to_land::<&str>(Vec::new()), None);
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
    /// (task 426); one that names an earlier review's output says it is
    /// that review's.
    #[test]
    fn a_review_that_could_not_start_names_no_output_of_its_own() {
        let error =
            "the headless review could not start: the test reviewer cannot start this review";
        let first = failed_review_question(&run(), 1, None, error);
        assert_eq!(
            first,
            format!(
                "The supervisor's headless review of run r1 (task 1) failed and gave no verdict (review 1): {error}\
                 \nReview material: {DIR}/review.md{FAILED_TAIL}"
            )
        );
        assert!(!first.contains("Review output"), "{first}");
        let retry = failed_review_question(&run(), 2, Some(1), error);
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

    /// Each job's reply is read as its own answer: the whole review's
    /// verdict, which names no agent's result (decision 4), and an agent's
    /// one completed result, bare or out of a fence as the eval reads it;
    /// anything else is unreadable, which runs the job once more and is
    /// never acted on as a verdict.
    #[test]
    fn a_jobs_reply_is_read_as_its_own_answer() {
        let whole = read_answer(
            &ReviewJobId::Whole,
            r#"{"verdict": "pass", "reasons": [], "summary": "ok"}"#,
        )
        .unwrap();
        assert!(matches!(whole, ReviewAnswer::Whole(_)));
        assert_eq!(whole.decision(), Some(ReviewDecision::Pass));
        let Err(why) = read_answer(&ReviewJobId::Whole, "no verdict") else {
            panic!("a reply without a verdict was read");
        };
        assert!(
            why.starts_with("the review printed no verdict JSON"),
            "{why}"
        );
        let Err(why) = read_answer(
            &ReviewJobId::Whole,
            r#"{"verdict": "pass", "reasons": [], "summary": "ok",
                "agents": [{"agent": "design", "status": "completed", "verdict": "pass", "summary": "ok"}]}"#,
        ) else {
            panic!("the whole review answered for an agent");
        };
        assert!(why.contains("unknown field `agents`"), "{why}");
        let design = ReviewJobId::Agent("design".into());
        let fenced = "Done.\n```json\n{\"agent\": \"design\", \"status\": \"completed\", \"verdict\": \"revise\", \"reasons\": [{\"text\": \"stale map\", \"codes\": []}], \"summary\": \"s\"}\n```";
        let answer = read_answer(&design, fenced).unwrap();
        let ReviewAnswer::Agent(result) = &answer else {
            panic!("an agent's answer");
        };
        assert_eq!(result.reasons, ["stale map"]);
        assert_eq!(answer.decision(), Some(ReviewDecision::Revise));
        for reply in [
            "nothing",
            r#"{"agent": "tests", "status": "completed", "verdict": "pass"}"#,
            r#"{"agent": "design", "status": "failed"}"#,
        ] {
            assert!(read_answer(&design, reply).is_err(), "{reply}");
        }
    }

    /// The error of a review whose jobs did not all answer names each
    /// one's why; `--no-claude`'s stands alone, so that `review_failed`
    /// records it as `provider_disabled`.
    #[test]
    fn a_review_whose_jobs_did_not_all_answer_names_each() {
        assert_eq!(
            review_jobs_error(&["the agent design: timed out".to_owned()]),
            "the review's jobs did not all give an answer: the agent design: timed out"
        );
        let disabled = format!("{REVIEW_PROVIDER_DISABLED} and codex cannot be used (usage_limit)");
        assert_eq!(
            review_jobs_error(&["the whole review: x".to_owned(), disabled.clone()]),
            disabled
        );
    }

    /// A review that waits keeps whether it is the retry (task 2013): the
    /// held phase carries `retried`, so no job of the review that starts
    /// after the wait runs once more when it was its programs' retry, and
    /// each may once when it was not, and the providers its agents name. A
    /// review that does not wait gives the session back.
    #[test]
    fn a_held_review_keeps_whether_it_is_the_retry() {
        let wait = ReviewRoute::Wait("the hold ask holds Claude".into());
        let session = || {
            Some(SessionRef {
                workspace: "w1".into(),
                resume: None,
            })
        };
        for retried in [false, true] {
            let Ok((held, why)) = held_review(
                &wait,
                &[ReviewRoute::Wait("codex cannot be used".into())],
                session(),
                (retried, vec![Provider::Codex]),
            ) else {
                panic!("the review does not wait");
            };
            assert_eq!(why, "the hold ask holds Claude");
            let Phase::ReviewHeld {
                session: Some(session),
                retried: kept,
                providers,
            } = held
            else {
                panic!("not held with its session");
            };
            assert_eq!(session.workspace, "w1");
            assert_eq!(kept, retried);
            assert_eq!(providers, [Provider::Codex]);
            // A review that was its programs' retry runs no job once more.
            assert_eq!(review_jobs::runs_again(true, false, kept, false), !retried);
        }
        let manual = ReviewRoute::Manual("no provider".into());
        let Err(Some(session)) = held_review(&manual, &[], session(), (true, Vec::new())) else {
            panic!("a route that does not wait held the review");
        };
        assert_eq!(session.workspace, "w1");
    }

    /// A review waits as a whole only while none of its jobs can start:
    /// its whole review's route waits and so does the job of each agent
    /// that names its own provider. One such agent's job that can start
    /// starts the review, so that it runs while the whole review's job
    /// waits in it; a whole review that can start starts it whatever its
    /// agents' routes are.
    #[test]
    fn a_review_waits_as_a_whole_only_while_none_of_its_jobs_can_start() {
        let wait = || ReviewRoute::Wait("codex cannot be used (authentication)".into());
        let start = |provider: Provider| {
            ReviewRoute::Start(
                ActorLaunch {
                    provider,
                    ..ActorLaunch::default_of(ModelRole::Review)
                },
                true,
            )
        };
        assert!(is_held(&wait(), &[]));
        assert!(is_held(&wait(), &[wait(), wait()]));
        assert!(!is_held(&wait(), &[wait(), start(Provider::Claude)]));
        assert!(!is_held(&start(Provider::Codex), &[wait()]));
        assert!(!is_held(&ReviewRoute::Manual("none".into()), &[wait()]));
        let session = Some(SessionRef {
            workspace: "w1".into(),
            resume: None,
        });
        let Err(Some(_)) = held_review(
            &wait(),
            &[start(Provider::Claude)],
            session,
            (false, vec![Provider::Claude]),
        ) else {
            panic!("an agent's job can start, so the review starts");
        };
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
        assert_eq!(why, "the review still asks for changes after 3 revises");
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
                "The supervisor's review of run r1 (task 1) returned revise (the review still asks for changes after 3 revises): not yet\
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

    /// A send_back the runtime would apply past the round's three revises
    /// asks with the count of the round's revises; before the third one the
    /// review's revise still goes to the live session.
    #[test]
    fn a_send_back_past_the_revise_limit_asks_with_the_count_of_revises() {
        let mut kinds = vec![event_kind::REVIEW_STARTED];
        for _ in 0..2 {
            kinds.extend([
                event_kind::REVISE_REQUESTED,
                event_kind::REVISE_FINISHED,
                event_kind::REVIEW_STARTED,
            ]);
        }
        let two = events(&kinds);
        assert_eq!(
            decide_revise(&RunHistory::from_events(&two)),
            ReviseDecision::Request {
                attempt: 3,
                round: 3
            }
        );
        kinds.extend([
            event_kind::REVISE_REQUESTED,
            event_kind::REVISE_FINISHED,
            event_kind::REVIEW_STARTED,
        ]);
        let round = events(&kinds);
        let history = RunHistory::from_events(&round);
        assert_eq!(decide_revise(&history), ReviseDecision::Ask);
        let verdict = concern(Some("send_back"), Some("high"));
        let why = concern_escalation(&verdict, EscalatedBecause::ReviseLimit, &history);
        assert_eq!(
            why.as_deref(),
            Some("the review recommends send_back after 3 revises")
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
                "The supervisor's review of run r1 (task 1) returned concern (the review recommends send_back after 3 revises): judged"
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
    }

    /// Each job of a review goes by the review's route on its own
    /// (ADR-t1895-1 decision 7, ADR-t1857-1): a job whose provider failed
    /// moves to the other one with the fallback on, its launch saying from
    /// which and why; waits for that one with it off; and gives no answer
    /// when the route still sends it there (its hold was not written) or to
    /// the person. `--no-claude` comes first, with the fallback off too, and
    /// only the provider that failed is held: Codex's failure holds Codex,
    /// Claude's login or usage limit raises the hold ask and holds nothing
    /// of its own, and neither holds the other provider. No job is moved
    /// for what a provider can run.
    #[test]
    fn a_review_job_moves_waits_or_fails_as_its_route_and_fallback_say() {
        use crate::domain::actor_model::RoleModels;
        use crate::domain::headless_job::JobFailure;
        use crate::domain::provider_switch::SwitchReason;
        let mut models = RoleModels::default();
        models.entry(ModelRole::Review).provider = Some(Provider::Codex);
        let switchable = models.switchable(ModelRole::Review);
        let route = |no_claude: bool, fallback: bool, held: Option<Provider>| {
            super::provider::review_route(
                no_claude,
                (switchable, fallback),
                models.launch(ModelRole::Review),
                None,
                |provider| match (provider, no_claude) {
                    (Provider::Claude, true) => Some(SwitchReason::Disabled),
                    _ => (held == Some(provider)).then_some(SwitchReason::UsageLimit),
                },
            )
        };
        // Codex's usage limit holds Codex alone (ProviderHold) and no wall.
        assert_eq!(
            super::jobs::job_failure_route(Provider::Codex, JobFailure::UsageLimit, true),
            (None, Some((SwitchReason::UsageLimit, true)))
        );
        // Claude's raises the hold ask and writes no ProviderHold.
        let (wall, unusable) =
            super::jobs::job_failure_route(Provider::Claude, JobFailure::UsageLimit, true);
        assert!(wall.is_some());
        assert_eq!(unusable, Some((SwitchReason::UsageLimit, false)));
        // On: it moves to Claude, which is not held, the launch saying why.
        let AfterUnusable::Start(moved, true) =
            after_unusable(route(false, true, Some(Provider::Codex)), Provider::Codex)
        else {
            panic!("the job moves to Claude");
        };
        assert_eq!(moved.provider, Provider::Claude);
        assert_eq!(moved.to_value()["switched_from"], "codex");
        assert_eq!(moved.to_value()["switch_reason"], "usage_limit");
        // Off: it waits for Codex, saying so.
        let AfterUnusable::Wait(why) =
            after_unusable(route(false, false, Some(Provider::Codex)), Provider::Codex)
        else {
            panic!("the job waits for Codex");
        };
        assert!(why.contains("[provider_fallback] jobs is false"), "{why}");
        // Once Codex's hold ends, it starts on Codex again, not switched.
        let ReviewRoute::Start(again, true) = route(false, false, None) else {
            panic!("the job starts on Codex");
        };
        assert_eq!(
            (again.provider, again.switched_from),
            (Provider::Codex, None)
        );
        // The hold was not written: the route still sends it to Codex.
        assert!(matches!(
            after_unusable(route(false, true, None), Provider::Codex),
            AfterUnusable::Fail
        ));
        // `--no-claude`, on or off: no provider is left, the person reviews.
        for fallback in [true, false] {
            let ReviewRoute::Manual(error) = route(true, fallback, Some(Provider::Codex)) else {
                panic!("no provider is left");
            };
            assert!(error.starts_with(REVIEW_PROVIDER_DISABLED), "{error}");
            assert!(matches!(
                after_unusable(
                    route(true, fallback, Some(Provider::Codex)),
                    Provider::Codex
                ),
                AfterUnusable::Fail
            ));
        }
        // `--no-claude` with a usable Codex starts there, even off.
        let ReviewRoute::Start(codex, _) = route(true, false, None) else {
            panic!("Codex runs it");
        };
        assert_eq!(codex.provider, Provider::Codex);
        // A Codex review starts on Codex, whatever its agents: nothing
        // moves it for what a provider can run.
        assert!(codex.switch_reason.is_none());
    }

    /// An agent whose `[review.subagents.<agent>]` names its provider goes
    /// as a role that names its provider does, whatever `[roles.review]`
    /// says ([`Supervisor::review_job_route`]): it starts on its own
    /// provider, the launch's source saying the agent's table named it,
    /// while the review's other jobs start where `[roles.review]` sends
    /// them. When its provider cannot be used it moves alone to the other
    /// one with `[provider_fallback] jobs` on (`switched_from`,
    /// `switch_reason`), and with it off waits for its provider and starts
    /// there again once the hold ends. After it started, only the provider
    /// that failed is held, and the job moves or waits as before it
    /// started. `--no-claude` moves an agent that names Claude to Codex,
    /// even off, and with Codex unusable too the person reviews it.
    #[test]
    fn an_agent_that_names_its_provider_moves_or_waits_alone_as_the_fallback_says() {
        use crate::domain::actor_model::{LaunchSource, RoleModels};
        use crate::domain::headless_job::JobFailure;
        use crate::domain::provider_switch::SwitchReason;
        // `[roles.review]` names nothing: the whole review runs on Claude.
        let models = RoleModels::default();
        let role = ModelRole::Review;
        let unusable = |no_claude: bool, held: Option<Provider>| {
            move |provider: Provider| match (provider, no_claude) {
                (Provider::Claude, true) => Some(SwitchReason::Disabled),
                _ => (held == Some(provider)).then_some(SwitchReason::UsageLimit),
            }
        };
        let agent =
            |provider: Provider, no_claude: bool, fallback: bool, held: Option<Provider>| {
                super::provider::review_route(
                    no_claude,
                    (true, fallback),
                    models.agent_launch(role, provider),
                    None,
                    unusable(no_claude, held),
                )
            };
        let whole = super::provider::review_route(
            false,
            (models.switchable(role), true),
            models.launch(role),
            None,
            unusable(false, None),
        );
        let ReviewRoute::Start(whole, false) = whole else {
            panic!("the whole review starts on Claude");
        };
        assert_eq!(
            (whole.provider, whole.source),
            (Provider::Claude, LaunchSource::Default)
        );
        // Its own provider, named by its table.
        let ReviewRoute::Start(codex, true) = agent(Provider::Codex, false, false, None) else {
            panic!("the agent starts on Codex");
        };
        assert_eq!(codex.provider, Provider::Codex);
        assert_eq!(codex.to_value()["source"], "review.subagents");
        assert!(codex.switched_from.is_none());
        // Codex held, on: it alone moves to Claude, saying from which and why.
        let ReviewRoute::Start(moved, true) =
            agent(Provider::Codex, false, true, Some(Provider::Codex))
        else {
            panic!("the agent moves to Claude");
        };
        assert_eq!(moved.provider, Provider::Claude);
        assert_eq!(moved.to_value()["switched_from"], "codex");
        assert_eq!(moved.to_value()["switch_reason"], "usage_limit");
        assert_eq!(moved.to_value()["source"], "review.subagents");
        // Off: it waits for Codex, and starts on Codex once the hold ends.
        let ReviewRoute::Wait(why) = agent(Provider::Codex, false, false, Some(Provider::Codex))
        else {
            panic!("the agent waits for Codex");
        };
        assert!(why.contains("so it waits for codex"), "{why}");
        let ReviewRoute::Start(again, true) = agent(Provider::Codex, false, false, None) else {
            panic!("the agent starts on Codex again");
        };
        assert_eq!(
            (again.provider, again.switched_from),
            (Provider::Codex, None)
        );
        // After it started: Codex's failure holds Codex alone, and Claude's
        // login or usage limit raises the hold ask and holds nothing more.
        assert_eq!(
            super::jobs::job_failure_route(Provider::Codex, JobFailure::UsageLimit, true),
            (None, Some((SwitchReason::UsageLimit, true)))
        );
        assert_eq!(
            super::jobs::job_failure_route(Provider::Claude, JobFailure::Authentication, true).1,
            Some((SwitchReason::Authentication, false))
        );
        assert!(matches!(
            after_unusable(
                agent(Provider::Codex, false, true, Some(Provider::Codex)),
                Provider::Codex
            ),
            AfterUnusable::Start(ref next, true) if next.provider == Provider::Claude
        ));
        assert!(matches!(
            after_unusable(
                agent(Provider::Codex, false, false, Some(Provider::Codex)),
                Provider::Codex
            ),
            AfterUnusable::Wait(_)
        ));
        // An agent that names Claude, its hold ask holding Claude: on, it
        // moves to Codex; off, it waits for Claude.
        let ReviewRoute::Start(to_codex, true) =
            agent(Provider::Claude, false, true, Some(Provider::Claude))
        else {
            panic!("the agent moves to Codex");
        };
        assert_eq!(
            (to_codex.provider, to_codex.switched_from),
            (Provider::Codex, Some(Provider::Claude))
        );
        assert!(matches!(
            agent(Provider::Claude, false, false, Some(Provider::Claude)),
            ReviewRoute::Wait(_)
        ));
        // `--no-claude` comes first, off too; with Codex unusable as well
        // no provider is left.
        for fallback in [true, false] {
            let ReviewRoute::Start(disabled, true) = agent(Provider::Claude, true, fallback, None)
            else {
                panic!("the agent moves to Codex under --no-claude");
            };
            assert_eq!(disabled.provider, Provider::Codex);
            assert_eq!(disabled.switch_reason, Some(SwitchReason::Disabled));
            assert_eq!(disabled.source, LaunchSource::Subagent);
            assert!(matches!(
                agent(Provider::Claude, true, fallback, Some(Provider::Codex)),
                ReviewRoute::Manual(_)
            ));
        }
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
