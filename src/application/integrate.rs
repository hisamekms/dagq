//! Landing a validated run on `main` (ADR-0016, ADR-0019, ADR-0023): the
//! integration slot, the rebase onto the current `main`, the re-validation
//! and the verification commands, the squash commit, the push and the
//! follow-ups; and the receipt check the supervisor's validation shares.
//! The queue, Git, the verification commands, the push, time, IDs and
//! process liveness come in through the ports.
//!
//! The landing and the push are the [`Integrator`]'s alone (ADR-t728-2):
//! the supervisor and a person's `integrate` hand it an
//! [`IntegrationRequest`], and it checks the requester, the lease and the
//! approval (a review's pass or an `integrate`) itself before it lands,
//! writing the landing's events as itself at the requester's request. Only
//! it holds the [`PushGrant`] [`MainRemote::push_main`] takes.

use crate::domain::EventKind;
use crate::domain::LeaseToken;
use anyhow::{Context, Result, bail, ensure};
use serde_json::{Value, json};
use std::{
    path::{Path, PathBuf},
    sync::mpsc,
    thread,
    time::{Duration, Instant},
};
use tracing::{info, warn};

use super::{
    AskStore, Clock, IdGenerator, Landing, MainRemote, ProcessControl, Queue, Repository, RunFiles,
    RunLog, Verifier, path_text, reason_of_error, tail,
};
use crate::domain::{
    ActorContext, ActorRole, AuthorizationError, Authorizer, Capability, Resource, StaticPolicy,
};
use crate::domain::{
    CommitSha, DraftOrigin, EvidenceCheck, IntegrationOutcome, NewTask, PushReport, PushResult,
    Reason, ReasonCode, Receipt, ReceiptResult, RegisteredFollowUp, RunHistory, RunId, RunStatus,
    Task, TaskId, TaskRun,
    disk::{BUILD_OUTPUTS_REMOVED, DiskConfig, gib},
    event_kind, evidence_missing_reason, heartbeat_stale,
    landing_branch::{DEFAULT_REMOTE, LandingBranch, RemoteSource, missing_remote},
    measure::{LoadSummary, LoadWindow},
    required_of,
    scope::{out_of_scope, scope_violation_reason},
    validation::{self, CheckedOut, Fact, Judgement, ReceiptFacts, ReceiptFile},
    verify_failure,
};
use crate::migration_numbers;

/// Why validation did not accept a run's receipt, with what it could
/// verify on the way.
pub struct Rejection {
    pub reason: String,
    /// The code of `reason` (ADR-0034).
    pub code: ReasonCode,
    pub commit: Option<CommitSha>,
    pub receipt: Option<Receipt>,
    /// The task's required checks the receipt does not back, when that is
    /// all that is wrong: the run waits for a session instead of failing.
    pub evidence_missing: Vec<EvidenceCheck>,
    /// The changed paths outside the task's `paths` (ADR-0029), when the
    /// run is otherwise sound: it waits for a session to take them out.
    pub scope_violation: Vec<String>,
}

/// Cross-check the agent's receipt against Git: the receipt names the
/// clean head of the run branch, new work on top of the base commit, within
/// the task's paths and with the task's required evidence. The domain
/// judges ([`validation::judge`]) and says which fact it needs next; this
/// gathers it from the run files and Git. `Ok(Err(_))` is a verdict on the
/// run; `Err` a failure of the checks themselves.
pub fn check_receipt(
    repository: &dyn Repository,
    files: &dyn RunFiles,
    task: &Task,
    run: &TaskRun,
) -> Result<std::result::Result<(Receipt, CommitSha), Rejection>> {
    let receipt_path = run.receipt_path().context("missing receipt path")?;
    let required = required_of(task.required_evidence(), run.actual_provider());
    let mut facts = ReceiptFacts::new(
        run.id(),
        run.base_commit(),
        &required,
        task.paths(),
        receipt_path,
    );
    let worktree =
        || -> Result<&Path> { Ok(Path::new(run.worktree_path().context("missing worktree")?)) };
    loop {
        let fact = match validation::judge(&facts) {
            Judgement::Need(fact) => fact,
            Judgement::Accept(commit) => {
                let receipt = facts.into_receipt().context("accepted without a receipt")?;
                return Ok(Ok((receipt, commit)));
            }
            Judgement::Reject(rejection) => {
                return Ok(Err(Rejection {
                    reason: rejection.reason,
                    code: rejection.code,
                    commit: rejection.commit,
                    receipt: facts.into_receipt(),
                    evidence_missing: rejection.evidence_missing,
                    scope_violation: rejection.scope_violation,
                }));
            }
        };
        match fact {
            Fact::Receipt => {
                let text = match files.read_to_string(Path::new(receipt_path)) {
                    Ok(text) => Some(text),
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
                    Err(error) => return Err(error).context("read receipt"),
                };
                facts.receipt = Some(ReceiptFile::of(text.as_deref()));
            }
            Fact::CheckedOut => {
                let worktree = worktree()?;
                let branch = run.branch().context("missing branch")?;
                facts.checked_out = Some(CheckedOut {
                    branch: branch.to_owned(),
                    current: repository.current_branch(worktree)?,
                });
            }
            Fact::Head => facts.head = Some(repository.head(worktree()?)?),
            Fact::Descends => {
                let head = facts.head.as_ref().context("no head to check")?;
                facts.descends =
                    Some(repository.is_ancestor(run.base_commit().as_str(), head.as_str())?);
            }
            Fact::Status => facts.status = Some(repository.status(worktree()?)?),
            // The diff starts where the branch forked from the current main,
            // not at the base commit: a resumed session that rebased carries
            // what other tasks landed since, which is not its change.
            Fact::Changes => {
                let head = facts.head.as_ref().context("no head to diff")?;
                let fork = repository
                    .merge_base(repository.main_head()?.as_str(), head.as_str())?
                    .context("the run branch shares no history with main")?;
                facts.changes = Some(repository.changed_paths(fork.as_str(), head.as_str())?);
            }
        }
    }
}

/// Which run `integrate` lands.
#[derive(Debug, Clone, Copy)]
pub enum IntegrateTarget {
    /// The task's run that awaits integration or comes back from a session.
    Task(TaskId),
    /// The oldest run awaiting integration by validation time (FIFO).
    Next,
}

/// What `integrate` needs besides the run: the queue, the repository the
/// queue is bound to (`common_dir` is its Git common directory as text),
/// how the verification commands run, the remote `main` is pushed to
/// (`None` is `--no-push`), the run files (the worktree, the receipt and
/// the verification logs), the time and IDs, and process liveness for the
/// lease check.
pub struct Integration<'a> {
    pub queue: &'a mut dyn Queue,
    pub repository: &'a dyn Repository,
    pub verifier: &'a dyn Verifier,
    pub remote: Option<&'a dyn MainRemote>,
    pub files: &'a dyn RunFiles,
    pub common_dir: &'a str,
    pub clock: &'a dyn Clock,
    pub ids: &'a dyn IdGenerator,
    pub processes: &'a dyn ProcessControl,
    /// This process, recorded with the approval to land.
    pub pid: u32,
    /// The 1-minute load average, sampled while each verification command
    /// runs (task 197).
    pub load_average: fn() -> Option<f64>,
    /// The free disk space [`begin`] checks before it approves a landing
    /// (task 638); `None` checks nothing (the supervisor, which holds its
    /// landings for the disk itself).
    pub disk: Option<DiskRoom>,
    /// What the retry of a verification command that failed on a full disk
    /// checks first (task 639); `None` retries without a check.
    pub retry_disk: Option<RetryDisk<'a>>,
}

/// The free disk space integrate checks before it retries a verification
/// command that failed on a full disk (task 639): the `[disk]` of
/// `dagq.toml` and a reading of the free bytes of the queue's directory
/// now, `None` when it cannot be read.
#[derive(Clone, Copy)]
pub struct RetryDisk<'a> {
    pub config: DiskConfig,
    pub free: &'a dyn Fn() -> Option<u64>,
}

/// What a person's `integrate` checks the free disk space with (task
/// 638): the `[disk]` of `dagq.toml` and the free bytes of the queue's
/// directory, `None` when they could not be read.
#[derive(Debug, Clone, Copy)]
pub struct DiskRoom {
    pub config: DiskConfig,
    pub free: Option<u64>,
}

/// A request to the [`Integrator`] to land a run that holds the
/// integration slot under `token` (ADR-t728-2): `requester` is who asks
/// (the supervisor, or the person or the inbox behind `integrate`),
/// `previous` the status the run returns to when the landing stops before
/// `main` moves, and `main` the head it lands on.
#[derive(Debug, Clone)]
pub struct IntegrationRequest {
    pub requester: ActorContext,
    pub run: TaskRun,
    pub previous: RunStatus,
    pub main: CommitSha,
    pub token: LeaseToken,
}

/// Proof that a push of the landing branch is the [`Integrator`]'s
/// (ADR-t728-2): [`MainRemote::push_main`] takes one, and only the
/// Integrator, once the policy grants it [`Capability::Push`], makes one.
pub struct PushGrant(());

/// The trusted control plane's landing (ADR-t728-2 decision 1): the only
/// code that rebases, verifies, squashes, moves `main` and pushes. It acts
/// as an actor of role [`ActorRole::Integrator`], the only role the policy
/// grants [`Capability::Land`] and [`Capability::Push`]. On a host it runs
/// in the requester's process; the boundary is logical (decision 4).
#[derive(Debug, Clone)]
pub struct Integrator {
    actor: ActorContext,
}

impl Integrator {
    /// The integrator of the process `pid` (`integrator:<pid>`).
    pub fn of_process(pid: u32) -> Self {
        Self {
            actor: ActorContext::instance(ActorRole::Integrator, pid),
        }
    }

    /// An integrator acting as `actor`, which the policy must grant the
    /// landing and the push: any other actor is refused.
    pub fn acting_as(actor: ActorContext) -> std::result::Result<Self, AuthorizationError> {
        for capability in [Capability::Land, Capability::Push] {
            StaticPolicy.authorize(&actor, capability, &Resource::Queue)?;
        }
        Ok(Self { actor })
    }

    pub fn actor(&self) -> &ActorContext {
        &self.actor
    }

    /// The first half of a person's `integrate`, at `requester`'s request:
    /// pick the run of `target`, check the requester may ask to land it,
    /// record the call as the approval to land it (as the requester) and
    /// take the integration slot. `repo` is the checkout the caller named,
    /// for the error when it belongs to another repository than the
    /// queue's. `None` means no run awaits integration. The caller keeps
    /// the lease alive while [`Self::land`] lands the run.
    pub fn approve(
        &self,
        ctx: &mut Integration<'_>,
        requester: &ActorContext,
        target: IntegrateTarget,
        repo: &Path,
    ) -> Result<Option<IntegrationRequest>> {
        begin(ctx, requester, target, repo)
    }

    /// Land the run of `request` (ADR-t728-2 decisions 2 and 3), after
    /// checking itself that the requester may ask it, that the run holds
    /// the integration slot under the request's token, and that the run
    /// was approved (an `integrate` or an `approve_landing` answer of
    /// `land`) or passed its latest review. A refused request gives the
    /// slot back and fails; nothing reaches `main`. The landing's events
    /// are written as the Integrator at the requester's request.
    pub fn land(
        &self,
        ctx: &mut Integration<'_>,
        request: &IntegrationRequest,
    ) -> Result<IntegrationOutcome> {
        let grant = self.push_grant()?;
        let previous = ctx.queue.act_as(self.actor.clone());
        let requested_by = ctx.queue.request_as(Some(&request.requester));
        let landed = self
            .check(&mut *ctx.queue, request)
            .and_then(|()| land_integrating(ctx, &grant, request));
        ctx.queue.restore_request(requested_by);
        if let Some(previous) = previous {
            ctx.queue.act_as(previous);
        }
        landed
    }

    fn push_grant(&self) -> Result<PushGrant> {
        StaticPolicy.authorize(&self.actor, Capability::Land, &Resource::Queue)?;
        StaticPolicy.authorize(&self.actor, Capability::Push, &Resource::Queue)?;
        Ok(PushGrant(()))
    }

    /// Refuse `request` unless [`landing_refusal`] finds nothing wrong,
    /// giving its slot back (when the request still holds it).
    fn check(&self, queue: &mut dyn Queue, request: &IntegrationRequest) -> Result<()> {
        let run = queue.run(request.run.id())?;
        let holds = queue.holds_lease(run.id(), &request.token)?;
        let events = queue.run_events(run.id())?;
        let Some(refusal) = landing_refusal(&request.requester, &run, holds, &events) else {
            return Ok(());
        };
        let message = format!(
            "the integrator refused to land run {} at the request of {}: {refusal}",
            run.id(),
            request.requester.actor_id()
        );
        warn!(op = "integrate", run_id = %run.id(), "{message}");
        if holds
            && let Err(record) = queue.abort_integration(
                run.id(),
                &request.token,
                request.previous.as_str(),
                &message,
                &Reason::new(ReasonCode::Other),
            )
        {
            warn!(op = "integrate", run_id = %run.id(), error = %format_args!("{record:#}"), "run {}: could not give the slot back: {record:#}", run.id());
        }
        bail!("{message}")
    }
}

/// Why the [`Integrator`] refuses to land `run` at `requester`'s request,
/// `None` when it lands it: the policy does not grant the requester
/// [`Capability::IntegrationRequest`] on the run, the run is not
/// `integrating` under the request's lease (`holds`), or `events` record
/// neither an approval to land (`integration_approved`) nor a pass as the
/// latest review verdict. A review's pass is data the request carries;
/// the landing's own checks still decide (ADR-t728-2 decision 3).
pub fn landing_refusal(
    requester: &ActorContext,
    run: &TaskRun,
    holds: bool,
    events: &[crate::domain::RunEvent],
) -> Option<String> {
    let resource = Resource::Run {
        id: run.id().clone(),
        task: Some(run.task_id()),
    };
    if let Err(error) = StaticPolicy.authorize(requester, Capability::IntegrationRequest, &resource)
    {
        return Some(error.to_string());
    }
    if run.status() != RunStatus::Integrating || !holds {
        return Some(format!(
            "the run is {} and the request does not hold its integration slot",
            run.status().as_str()
        ));
    }
    let history = RunHistory::from_events(events);
    let passed = history
        .last(event_kind::REVIEW_FINISHED)
        .is_some_and(|review| review.payload["verdict"] == "pass");
    if !history.approved() && !passed {
        return Some("the run was neither approved to land nor passed its latest review".into());
    }
    None
}

/// The first half of `integrate` ([`Integrator::approve`]). The requester
/// is refused before anything is recorded when the policy does not grant
/// it [`Capability::IntegrationRequest`] on the run.
fn begin(
    ctx: &mut Integration<'_>,
    requester: &ActorContext,
    target: IntegrateTarget,
    repo: &Path,
) -> Result<Option<IntegrationRequest>> {
    let queue = &mut *ctx.queue;
    let common_dir = ctx.common_dir;
    let bound = queue
        .repository_binding()?
        .context("queue is not bound to a repository; no run was supervised")?;
    ensure!(
        bound == common_dir,
        "{} belongs to {common_dir}, but the queue is bound to {bound}",
        repo.display()
    );
    let run = match target {
        IntegrateTarget::Task(task_id) => {
            let detail = queue.show(task_id)?;
            if let Some(busy) = detail
                .runs
                .iter()
                .find(|r| r.status() == RunStatus::Integrating)
            {
                bail!(
                    "run {} of task {task_id} is already integrating (see doctor if it is stuck)",
                    busy.id()
                );
            }
            detail
                .runs
                .iter()
                .find(|r| {
                    matches!(
                        r.status(),
                        RunStatus::AwaitingIntegration | RunStatus::NeedsSession
                    )
                })
                .cloned()
                .with_context(|| {
                    format!(
                        "task {task_id} ({}) has no run awaiting integration or a session",
                        detail.task.status().as_str()
                    )
                })?
        }
        IntegrateTarget::Next => match queue.next_awaiting_integration()? {
            Some(run) => run,
            None => return Ok(None),
        },
    };
    // A run the supervisor holds (its review, ADR-0027, or its resume) is
    // not approved by a call that cannot land it.
    if let Some(lease) = queue.run_lease(run.id())?
        && !heartbeat_stale(
            ctx.processes.alive(lease.pid),
            ctx.clock.now() - lease.heartbeat_at,
        )
    {
        bail!(
            "run {} is held by the supervisor (its review or resume is in progress); see show for its review_finished / resume_finished events",
            run.id()
        );
    }
    StaticPolicy.authorize(
        requester,
        Capability::IntegrationRequest,
        &Resource::Run {
            id: run.id().clone(),
            task: Some(run.task_id()),
        },
    )?;
    if let Some(room) = ctx.disk {
        check_disk_room(&*queue, &room, run.id())?;
    }
    // The call is the approval to land (ADR-0016 decision 5): a run it
    // parks as `needs_session` is landed by the supervisor once a resumed
    // session resolved it (ADR-0019 decision 1).
    if !queue.has_run_event(run.id(), event_kind::INTEGRATION_APPROVED)? {
        queue.record_runtime_event(
            run.id(),
            EventKind::IntegrationApproved,
            json!({"status": run.status().as_str(), "pid": ctx.pid, "push": ctx.remote.is_some()}),
        )?;
    }
    let previous = run.status();
    let token = ctx.ids.lease_token();
    let main = ctx.repository.main_head()?;
    let run = queue.begin_integration(run.id(), &token, &main)?;
    Ok(Some(IntegrationRequest {
        requester: requester.clone(),
        run,
        previous,
        main,
        token,
    }))
}

/// Refuse to land `run` while the free disk space is below what a
/// landing's verification needs (task 638, ADR-0047 decision 44): the same
/// threshold the supervisor holds its landings at. A person is at hand, so
/// the call fails instead of waiting; nothing is recorded. No threshold (no
/// build measured and no `min_free_bytes`) or unread free space checks
/// nothing, as in the supervisor.
fn check_disk_room(queue: &dyn Queue, room: &DiskRoom, run: &RunId) -> Result<()> {
    if let Some(DiskShort {
        free,
        need,
        largest_build,
    }) = disk_short(queue, &room.config, room.free)?
    {
        let largest =
            largest_build.map_or_else(|| "none measured".into(), |bytes| gib(bytes as f64));
        bail!(
            "not enough free disk space to land run {run}: {} free in the queue's directory, below the {} a landing's verification needs (the largest build of the recent runs, {largest}, times [disk] integrate_factor of dagq.toml, at least min_free_bytes); the run was not approved and is unchanged. Free disk space (dagq doctor lists the runs and their worktrees; the worktrees of ended runs nobody looks at any more, or other files on that disk) and run integrate again",
            gib(free as f64),
            gib(need as f64),
        );
    }
    Ok(())
}

/// The free disk space below a landing's threshold (task 377): what is
/// free, what is needed, and the largest recent build it follows.
#[derive(Debug, Clone, Copy)]
struct DiskShort {
    free: u64,
    need: u64,
    largest_build: Option<u64>,
}

impl DiskShort {
    fn to_json(self) -> Value {
        json!({
            "free_bytes": self.free,
            "needed_bytes": self.need,
            "largest_build_bytes": self.largest_build,
        })
    }
}

/// Whether `free` is short of the landing threshold `config` sets over the
/// latest `build_outputs_removed`: `None` with room, no threshold or no
/// reading.
fn disk_short(
    queue: &dyn Queue,
    config: &DiskConfig,
    free: Option<u64>,
) -> Result<Option<DiskShort>> {
    let limit = usize::try_from(config.sample_runs).unwrap_or(0);
    let builds: Vec<u64> = queue
        .latest_events_of(BUILD_OUTPUTS_REMOVED, limit)?
        .iter()
        .filter_map(|event| event.payload.get("bytes").and_then(Value::as_u64))
        .collect();
    let needs = config.needs(&builds);
    Ok(match (free, needs.landing) {
        (Some(free), Some(need)) if free < need => Some(DiskShort {
            free,
            need,
            largest_build: needs.largest_build,
        }),
        _ => None,
    })
}

/// Land a run that holds the integration slot under `token` and record
/// the outcome: the [`Integrator`]'s landing, whichever requester asked
/// for it. An error before `main` moved gives the slot back and returns
/// the run to `previous`.
fn land_integrating(
    ctx: &mut Integration<'_>,
    grant: &PushGrant,
    request: &IntegrationRequest,
) -> Result<IntegrationOutcome> {
    let IntegrationRequest {
        run,
        previous,
        main,
        token,
        ..
    } = request;
    let previous = *previous;
    let queue = &mut *ctx.queue;
    let repository = ctx.repository;
    let task = queue.show(run.task_id())?.task;
    // Every event of the landing, the push and the follow-ups carries the run.
    let _span =
        tracing::info_span!("integrate", run_id = %run.id(), task_id = %run.task_id()).entered();
    info!(
        op = "integrate",
        main = %main,
        "run {} integrating task {} onto main {main}",
        run.id(),
        run.task_id()
    );
    // The landing branch is resolved once: the landed commit may rewrite
    // `[repository] branch` of the main checkout's dagq.toml, and the
    // fast-forward and the push still go to the branch this landing began on.
    let landed = repository.landing_branch().and_then(|onto| {
        let verdict = land(
            queue,
            repository,
            ctx.verifier,
            ctx.load_average,
            ctx.files,
            ctx.retry_disk.as_ref(),
            &task,
            run,
            &onto,
            main,
        )?;
        Ok((onto, verdict))
    });
    let (onto, verdict) = match landed {
        Ok(landed) => landed,
        Err(error) => {
            // Nothing reached main: give the slot back and keep the run where it was.
            let message = format!("integration stopped before main moved: {error:#}");
            if let Err(record) = queue.abort_integration(
                run.id(),
                token,
                previous.as_str(),
                &message,
                &reason_of_error(&error, ReasonCode::Other),
            ) {
                warn!(
                    op = "integrate",
                    error = %format_args!("{record:#}"),
                    "run {}: could not record the error: {record:#}",
                    run.id()
                );
            }
            return Err(error.context(format!(
                "run {} returned to {}",
                run.id(),
                previous.as_str()
            )));
        }
    };
    Ok(match verdict {
        Verdict::Landed(landing, proposed) => {
            let verification_skipped = landing.verification_skipped;
            let (task, run) = queue
                .finish_integration(run.id(), token, &landing, ctx.common_dir)
                .with_context(|| {
                    format!(
                        "main advanced to {} but run {} could not be completed; inspect show and doctor",
                        landing.commit, run.id()
                    )
                })?;
            info!(
                op = "integrate",
                commit = %landing.commit,
                "task {} landed as {} on main; run {} integrated",
                task.id(),
                landing.commit,
                run.id()
            );
            close_landing_asks(queue, &run);
            remove_landed_worktree(queue, repository, &run);
            let push = push_main(queue, grant, &onto, ctx.remote, run.id(), &landing.commit);
            let follow_ups = register_follow_ups(queue, &task, run.id(), proposed.as_ref());
            IntegrationOutcome::Integrated {
                task: Box::new(task),
                run: Box::new(run),
                verification_skipped,
                push: Box::new(push),
                follow_ups,
            }
        }
        Verdict::Deferred { reason, mut detail } => {
            warn!(
                op = "integrate",
                reason = %reason,
                "run {} needs a session: {reason}",
                run.id()
            );
            // How many more times the supervisor resumes it (ADR-0019); the
            // event is a person's only once none are left (as an ask).
            detail["resumes_left"] = json!(resumes_left(queue, run.id()));
            let run = queue.defer_integration(run.id(), token, &reason, detail)?;
            IntegrationOutcome::NeedsSession {
                run: Box::new(run),
                main: main.clone(),
                reason,
            }
        }
        Verdict::Held { reason, detail } => {
            warn!(
                op = "integrate",
                reason = %reason,
                "run {} waits for a person: {reason}",
                run.id()
            );
            let run = queue.hold_integration(run.id(), token, &reason, detail)?;
            IntegrationOutcome::Held {
                run: Box::new(run),
                main: main.clone(),
                reason,
            }
        }
        Verdict::ReceiptFailed { reason, receipt } => {
            warn!(
                op = "integrate",
                reason = %reason,
                "run {} failed: {reason}",
                run.id()
            );
            let run = queue.fail_integration(run.id(), token, &reason, receipt)?;
            IntegrationOutcome::Failed {
                run: Box::new(run),
                reason,
            }
        }
    })
}

/// Push the landing branch to the remote `[repository]` of `dagq.toml`
/// names (`origin` by default, ADR-t615-1) and record the outcome as
/// `push_finished`, `push_skipped` or `push_failed` on the landed run.
/// `onto` is the landing branch resolved when the landing began, not
/// resolved again from the dagq.toml the landing may have rewritten.
/// `push = false` and a missing default remote skip the push; a configured
/// remote that is missing fails it. A failure to record is only reported:
/// the landing stands either way.
fn push_main(
    queue: &dyn Queue,
    grant: &PushGrant,
    onto: &LandingBranch,
    remote: Option<&dyn MainRemote>,
    run_id: &RunId,
    commit: &CommitSha,
) -> PushReport {
    let branch = Some(onto.name.clone());
    let report = |outcome, remote: &str, error: Option<String>, reason: Option<&str>| PushReport {
        outcome,
        remote: remote.to_owned(),
        branch: branch.clone(),
        error,
        reason: reason.map(str::to_owned),
    };
    let failed = |remote: &str, error: &anyhow::Error| {
        report(PushResult::Failed, remote, Some(format!("{error:#}")), None)
    };
    let report = match remote {
        None => report(PushResult::Skipped, DEFAULT_REMOTE, None, Some("--no-push")),
        Some(main_remote) => match main_remote.push_config() {
            Err(error) => failed(DEFAULT_REMOTE, &error),
            Ok(config) if !config.push() => report(
                PushResult::Skipped,
                config.remote(),
                None,
                Some("push = false in dagq.toml"),
            ),
            Ok(config) => {
                let name = config.remote();
                match main_remote.has_remote(name) {
                    Ok(false) if config.remote_source() == RemoteSource::Default => report(
                        PushResult::Skipped,
                        name,
                        None,
                        Some(&format!("the repository has no remote {name}")),
                    ),
                    Ok(false) => report(PushResult::Failed, name, Some(missing_remote(name)), None),
                    Ok(true) => match main_remote.push_main(grant, name, onto) {
                        Ok(()) => report(PushResult::Pushed, name, None, None),
                        Err(error) => failed(name, &error),
                    },
                    Err(error) => failed(name, &error),
                }
            }
        },
    };
    let (kind, payload) = match report.outcome {
        PushResult::Pushed => (
            EventKind::PushFinished,
            json!({"remote": report.remote, "branch": report.branch, "commit": commit}),
        ),
        PushResult::Skipped => (
            EventKind::PushSkipped,
            json!({"remote": report.remote, "branch": report.branch, "commit": commit, "reason": report.reason}),
        ),
        PushResult::Failed => (
            EventKind::PushFailed,
            json!({"code": ReasonCode::PushFailed, "remote": report.remote, "branch": report.branch, "commit": commit, "error": report.error}),
        ),
    };
    let remote = report.remote.as_str();
    match &report.error {
        Some(error) => warn!(
            op = "push",
            run_id = %run_id,
            remote,
            error = %error,
            "run {run_id}: push of the landing branch failed: {error}"
        ),
        None => info!(
            op = "push",
            run_id = %run_id,
            remote,
            outcome = %kind,
            "run {run_id}: {kind} ({remote})"
        ),
    }
    if let Err(error) = queue.record_runtime_event(run_id, kind, payload) {
        warn!(
            op = "push",
            run_id = %run_id,
            error = %format_args!("{error:#}"),
            "run {run_id}: could not record {kind}: {error:#}"
        );
    }
    report
}

/// Register the landed receipt's `follow_ups` of `task`'s run `run_id` as
/// draft tasks of the task's goal (ADR-0019 decision 4), one follow-up
/// deeper than the task (`follow_up_depth`, ADR-0037 decision 6), with their
/// origin (`follow_up`, the task and run) for the planner the runtime opens
/// for each (ADR-0041 decision 16): the title and
/// description as proposed, no acceptance, verification commands or
/// dependencies, and a context naming where they came from. A closed goal
/// takes no task, so the follow-up is registered without a goal and its
/// `follow_up_registered` says `goal_closed: true`. An entry whose `title`
/// is not a non-blank string or whose `description` is not a string is not
/// registered: its `follow_up_registered` has `task_id: null`, the `skipped`
/// reason and the entry itself as `follow_up`. Every event carries the
/// entry's `index` and its `category` (ADR-t947-3: as the worker wrote it,
/// `unlabeled` without one), which the draft's origin material keeps too,
/// and an entry already recorded is not looked at again, so
/// a second call for the same run adds nothing (the task and its event are
/// written one after the other, so only a failure to record between them
/// could let a later call register it twice). A registration that fails is
/// only reported: the landing stands either way. Returns what this call
/// registered.
pub fn register_follow_ups<Q: Queue + ?Sized>(
    queue: &mut Q,
    task: &Task,
    run_id: &RunId,
    follow_ups: Option<&Value>,
) -> Vec<RegisteredFollowUp> {
    let Some(entries) = follow_ups.and_then(Value::as_array) else {
        return Vec::new();
    };
    let registered: Vec<u64> = match queue.run_events(run_id) {
        Ok(events) => RunHistory::from_events(&events).registered_follow_ups(),
        Err(error) => {
            warn!(
                op = "follow_up",
                run_id = %run_id,
                error = %format_args!("{error:#}"),
                "run {run_id}: follow_ups not registered: {error:#}"
            );
            return Vec::new();
        }
    };
    let goal_closed = match task.goal_id() {
        Some(goal_id) => match queue.show_goal(goal_id) {
            Ok(detail) => detail.closed,
            Err(error) => {
                warn!(
                    op = "follow_up",
                    run_id = %run_id,
                    error = %format_args!("{error:#}"),
                    "run {run_id}: follow_ups not registered: {error:#}"
                );
                return Vec::new();
            }
        },
        None => false,
    };
    // A draft is one follow-up further from a person's judgement than the
    // task that proposed it (ADR-0037 decision 6).
    // An unreadable depth counts as the deepest that still asks, so the
    // registration goes on and no draft is adopted without a person.
    let depth = match queue.follow_up_depth(task.id()) {
        Ok(depth) => depth + 1,
        Err(error) => {
            warn!(
                op = "follow_up",
                run_id = %run_id,
                task_id = %task.id(),
                error = %format_args!("{error:#}"),
                "run {run_id}: the follow_up_depth of task {} could not be read: {error:#}",
                task.id()
            );
            crate::domain::follow_up::FOLLOW_UP_ASK_DEPTH
        }
    };
    let mut added = Vec::new();
    for (index, entry) in entries.iter().enumerate() {
        if registered.contains(&(index as u64)) {
            continue;
        }
        let title = entry["title"].as_str().map(str::trim).unwrap_or_default();
        let description = entry["description"].as_str();
        // The worker's category of it (ADR-t947-3), `unlabeled` without
        // one; an unknown one is kept as it is.
        let category = crate::domain::follow_up_category(entry);
        let skipped = if title.is_empty() {
            Some("title is not a non-blank string")
        } else if description.is_none() {
            Some("description is not a string")
        } else {
            None
        };
        if let Some(reason) = skipped {
            warn!(
                op = "follow_up",
                run_id = %run_id,
                reason,
                "run {run_id}: follow_up {index} was not registered: {reason}"
            );
            let payload = json!({
                "task_id": null,
                "title": entry["title"],
                "index": index,
                "skipped": reason,
                "category": category,
                "follow_up": entry,
            });
            if let Err(error) =
                queue.record_runtime_event(run_id, EventKind::FollowUpRegistered, payload)
            {
                warn!(
                    op = "follow_up",
                    run_id = %run_id,
                    error = %format_args!("{error:#}"),
                    "run {run_id}: could not record follow_up_registered: {error:#}"
                );
            }
            continue;
        }
        let new = NewTask {
            title: title.to_owned(),
            description: description.unwrap_or_default().to_owned(),
            acceptance: String::new(),
            verification_commands: Vec::new(),
            required_evidence: Vec::new(),
            paths: Vec::new(),
            priority: Default::default(),
            kind: None,
            change: None,
            dependencies: Vec::new(),
            goal_dependencies: Vec::new(),
            goal_id: task.goal_id().filter(|_| !goal_closed),
            context: format!(
                "follow_up proposed by the receipt of run {run_id} of task {} ({})",
                task.id(),
                task.title()
            ),
            provider: None,
            worker_mode: None,
        };
        let created = match queue.add(new) {
            Ok(created) => created,
            Err(error) => {
                warn!(
                    op = "follow_up",
                    run_id = %run_id,
                    error = %format_args!("{error:#}"),
                    "run {run_id}: follow_up {title:?} was not registered: {error:#}"
                );
                continue;
            }
        };
        if let Err(error) = queue.set_follow_up_depth(created.id(), depth) {
            warn!(
                op = "follow_up",
                run_id = %run_id,
                task_id = %created.id(),
                error = %format_args!("{error:#}"),
                "run {run_id}: could not record the follow_up_depth of task {}: {error:#}",
                created.id()
            );
        }
        // The draft waits for a planner of the runtime's (ADR-0041 decision
        // 16), which is shown where it came from.
        let material = json!({
            "source_task_id": task.id(),
            "source_run_id": run_id,
            "index": index,
            "category": category,
        });
        if let Err(error) =
            queue.record_draft_origin(created.id(), DraftOrigin::FollowUp, &material)
        {
            warn!(
                op = "follow_up",
                run_id = %run_id,
                task_id = %created.id(),
                error = %format_args!("{error:#}"),
                "run {run_id}: could not record where draft task {} came from: {error:#}",
                created.id()
            );
        }
        let mut payload = json!({"task_id": created.id(), "title": created.title(), "index": index, "category": category});
        if goal_closed {
            payload["goal_closed"] = json!(true);
        }
        if let Err(error) =
            queue.record_runtime_event(run_id, EventKind::FollowUpRegistered, payload)
        {
            warn!(
                op = "follow_up",
                run_id = %run_id,
                error = %format_args!("{error:#}"),
                "run {run_id}: could not record follow_up_registered: {error:#}"
            );
        }
        info!(
            op = "follow_up",
            run_id = %run_id,
            follow_up_task_id = %created.id(),
            "run {run_id}: follow_up {:?} registered as draft task {}",
            created.title(),
            created.id()
        );
        added.push(RegisteredFollowUp {
            task_id: created.id(),
            title: created.title().to_owned(),
        });
    }
    added
}

enum Verdict {
    /// Landed with the receipt's `follow_ups`.
    Landed(Landing, Option<Value>),
    /// Re-validation did not pass; the worktree is left for a session.
    Deferred { reason: String, detail: Value },
    /// The session's rewritten receipt reports `failed`; `receipt` is its
    /// JSON, kept with the `integration_failed` event.
    ReceiptFailed { reason: String, receipt: Value },
    /// A verification command failed on the host again after its retry, or
    /// on a full disk with no room to retry it (task 639): the run waits
    /// for a person, awaiting integration, without a resume.
    Held { reason: String, detail: Value },
}

/// Rebase, re-validate and land one run. `Ok(Deferred)` and
/// `Ok(ReceiptFailed)` are verdicts on the run; `Err` is a failure of the
/// landing itself (Git, files) before `main` moved.
#[allow(clippy::too_many_arguments)]
fn land(
    queue: &mut dyn Queue,
    repository: &dyn Repository,
    verifier: &dyn Verifier,
    load_average: fn() -> Option<f64>,
    files: &dyn RunFiles,
    retry_disk: Option<&RetryDisk<'_>>,
    task: &Task,
    run: &TaskRun,
    landing_branch: &LandingBranch,
    main: &CommitSha,
) -> Result<Verdict> {
    let defer = |code: Reason, reason: String, detail: Value| {
        Ok(Verdict::Deferred {
            reason,
            detail: code.on(detail),
        })
    };
    let worktree = Path::new(run.worktree_path().context("missing worktree")?);
    ensure!(
        files.is_dir(worktree),
        "worktree {} is missing",
        worktree.display()
    );
    // A worktree whose queue directory moved is still found through its own
    // `.git` file, but the repository's record of it points at the old path
    // until repaired, and removing it after landing would fail (ADR-0017).
    repository.repair_worktree(worktree)?;
    let branch = run.branch().context("missing branch")?;
    // The landing branch's name, for the reasons a resumed session reads.
    let onto = &landing_branch.name;
    let run_dir = Path::new(run.run_dir().context("missing run directory")?);
    // A rebase left behind by a crashed landing or an unfinished session is undone first.
    if repository.rebase_in_progress(worktree)? {
        repository.rebase_abort(worktree)?;
        queue.record_runtime_event(
            run.id(),
            EventKind::IntegrationRebaseAborted,
            json!({"code": ReasonCode::RebaseInProgress, "reason": "a rebase was left in progress"}),
        )?;
    }
    let expected_ref = format!("refs/heads/{branch}");
    match repository.current_branch(worktree)? {
        Some(current) if current == expected_ref => (),
        current => {
            return defer(
                ReasonCode::CommitMismatch.into(),
                format!(
                    "worktree is on {} instead of {expected_ref}",
                    current.as_deref().unwrap_or("a detached HEAD")
                ),
                json!({}),
            );
        }
    }
    let head = repository.head(worktree)?;
    // The receipt must describe this head: the validated one for a fresh run,
    // the one the session rewrote after resolving otherwise. A stale receipt
    // means the session is not done.
    let receipt_path = Path::new(run.receipt_path().context("missing receipt path")?);
    let receipt = match files.read_to_string(receipt_path) {
        Ok(text) => match Receipt::parse(&text) {
            Ok(receipt) => receipt,
            Err(error) => {
                return defer(
                    ReasonCode::of_receipt_error(&error).into(),
                    format!("{error:#}"),
                    json!({}),
                );
            }
        },
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return defer(
                ReasonCode::ReceiptMissing.into(),
                format!("receipt is missing at {}", receipt_path.display()),
                json!({}),
            );
        }
        Err(error) => return Err(error).context("read receipt"),
    };
    if receipt.result() == ReceiptResult::Failed {
        return Ok(Verdict::ReceiptFailed {
            reason: format!("session reported the run as failed: {}", receipt.summary()),
            receipt: serde_json::to_value(&receipt)?,
        });
    }
    let required = required_of(task.required_evidence(), run.actual_provider());
    if let Err(error) = receipt.check_requiring(run.id(), &required) {
        return defer(
            ReasonCode::of_receipt_error(&error).into(),
            format!("{error:#}"),
            json!({}),
        );
    }
    // A resumed session may have come back without the evidence it was
    // asked for; `checks` tells the next resume to ask for it again.
    let missing = receipt.missing_evidence(&required);
    if !missing.is_empty() {
        return defer(
            ReasonCode::EvidenceMissing.into(),
            evidence_missing_reason(&missing),
            json!({"checks": missing}),
        );
    }
    // The receipt read here is the one that lands (or the one a session
    // rewrote after resolving), so it is recorded whatever happens next: the
    // DB otherwise keeps only the receipt seen at validation time.
    queue.record_runtime_event(
        run.id(),
        EventKind::IntegrationReceipt,
        json!({
            "main": main,
            "commit": receipt.commit(),
            "receipt": serde_json::to_value(&receipt)?,
        }),
    )?;
    if !receipt.names_commit(head.as_str()) {
        return defer(
            ReasonCode::CommitMismatch.into(),
            format!(
                "receipt commit {} is not the head of {branch} ({head}); rerun your checks and rewrite the receipt for the current head",
                receipt.commit()
            ),
            json!({"head": head}),
        );
    }
    let status = repository.status(worktree)?;
    if !status.trim().is_empty() {
        return defer(
            ReasonCode::WorktreeDirty.into(),
            format!("worktree is not clean:\n{}", status.trim_end()),
            json!({"head": head}),
        );
    }
    // Onto the current main. A no-op when the run already sits on it.
    if let Err(output) = repository.rebase(worktree, main.as_str())? {
        let conflicts = repository.conflicted_files(worktree).unwrap_or_default();
        if repository.rebase_in_progress(worktree)? {
            repository.rebase_abort(worktree)?;
        }
        return defer(
            ReasonCode::RebaseConflict.into(),
            format!(
                "rebase onto {onto} {main} conflicted in {}; resolve it in the worktree (git rebase {main}), rerun your checks, and rewrite the receipt with the new head",
                if conflicts.is_empty() {
                    "the run branch".to_owned()
                } else {
                    conflicts.join(", ")
                }
            ),
            json!({
                "main": main,
                "head": head,
                "conflicts": conflicts,
                "output_tail": tail(&output, 2000),
                "aborted": true,
            }),
        );
    }
    let rebased = repository.head(worktree)?;
    queue.record_runtime_event(
        run.id(),
        EventKind::IntegrationRebased,
        json!({"main": main, "head_before": head, "head_after": rebased}),
    )?;
    if rebased == *main {
        return defer(
            ReasonCode::RebaseEmpty.into(),
            format!(
                "no commit remains on top of {onto} {main} after the rebase; if the change is no longer needed, write a failed receipt with the reason"
            ),
            json!({"main": main, "head": rebased}),
        );
    }
    ensure!(
        repository.is_ancestor(main.as_str(), rebased.as_str())?,
        "rebased head {rebased} does not descend from main {main}"
    );
    let status = repository.status(worktree)?;
    if !status.trim().is_empty() {
        return defer(
            ReasonCode::WorktreeDirty.into(),
            format!(
                "worktree is not clean after the rebase:\n{}",
                status.trim_end()
            ),
            json!({"main": main, "head": rebased}),
        );
    }
    // A migration the run adds under a number main took meanwhile would
    // fail the build with two files of one number (ADR-0067 decision 3);
    // dagq's source repository only (ADR-t614-1).
    let rebased = match renumber_migration(queue, repository, run, worktree, main, &rebased)? {
        Renumbering::Unchanged => rebased,
        Renumbering::Renumbered(head) => head,
        Renumbering::Blocked { reason, detail } => {
            return defer(ReasonCode::MigrationNumberTaken.into(), reason, detail);
        }
    };
    // What lands is the squash of main..rebased, so that is the diff held to
    // the task's paths (ADR-0029): the rebase may have changed it since
    // validation, and a resumed session may have committed more.
    let outside = out_of_scope(
        task.paths(),
        &repository.changed_paths(main.as_str(), rebased.as_str())?,
    );
    if !outside.is_empty() {
        return defer(
            ReasonCode::ScopeViolation.into(),
            format!(
                "{} after the rebase onto {onto} {main}; take them out of the run branch (or ask for the task's --paths to be widened), commit, and rewrite the receipt with the new head",
                scope_violation_reason(&outside)
            ),
            json!({"main": main, "head": rebased, "scope_violation": outside, "allowed": task.paths()}),
        );
    }
    // The task's verification commands run here, once per commit, on the
    // rebased tree: validation only checks the receipt (ADR-0023 decision 1).
    let commands = task.verification_commands();
    let run_env = if commands.is_empty() {
        Vec::new()
    } else {
        // A program [run.env] names that cannot be executed would fail
        // every command: stop before them, like an unreadable dagq.toml,
        // so the run goes back without using a resume (ADR-0049 decision 9).
        if let Some(message) = verifier.run_env_programs(Some(run_dir))?.missing_message() {
            bail!("{message}");
        }
        verifier.run_env(run_dir)?
    };
    // The landing's verification, done once more when only flaky tests
    // failed (task 768, ADR-t768-1): a new attempt of every command on the
    // same rebased head, in the slot it holds (main does not move meanwhile).
    'verify: loop {
        // Each attempt keeps its own logs, so a second integrate of the run does
        // not overwrite why the first one failed.
        let attempt = next_integrate_attempt(files, run_dir);
        let step = VerifyStep {
            verifier,
            load_average,
            files,
            run,
            worktree,
            run_env: &run_env,
            attempt,
        };
        for (index, command) in commands.iter().enumerate() {
            let index = index + 1;
            let first = step.run(
                queue,
                index,
                command,
                &integrate_verify_log(run_dir, attempt, index),
                None,
            )?;
            let Some(first_failure) = first.failure.clone() else {
                continue;
            };
            let mut last = first;
            // A failure the host caused (a full disk, a kill, a timeout) is
            // retried once in this attempt instead of resuming the worker, and
            // is a person's when it fails so again (task 639, ADR-t639-1).
            if first_failure.class.is_environmental() {
                let short = match retry_disk {
                    Some(room) if first_failure.class == verify_failure::FailureClass::DiskFull => {
                        disk_short(&*queue, &room.config, (room.free)())?
                    }
                    _ => None,
                };
                if let Some(short) = short {
                    return Ok(held(
                        run,
                        main,
                        &rebased,
                        command,
                        index,
                        &last,
                        None,
                        Some(short),
                    ));
                }
                let retried = step.run(
                    queue,
                    index,
                    command,
                    &integrate_retry_log(run_dir, attempt, index),
                    Some(&last),
                )?;
                match &retried.failure {
                    None => continue,
                    Some(failure) if failure.class.is_environmental() => {
                        return Ok(held(
                            run,
                            main,
                            &rebased,
                            command,
                            index,
                            &retried,
                            Some(&last),
                            None,
                        ));
                    }
                    // The code's own failure on the retry: the worker's, as
                    // any other.
                    Some(_) => last = retried,
                }
            }
            let failure = last.failure.as_ref().unwrap_or(&first_failure);
            // Every failed test passed on nextest's retry: the landing is done
            // once more instead of a resume, once per run (task 768).
            if failure.class == verify_failure::FailureClass::Flaky
                && !queue.has_run_event(run.id(), event_kind::INTEGRATION_RETRIED)?
            {
                let reason = format!(
                    "verification command {command:?} failed after the rebase onto {main} only on tests that passed when run again ({}); see {}. Verifying it once more instead of resuming the session",
                    last.flaky_tests.join(", "),
                    last.log.display()
                );
                warn!(op = "integrate", reason = %reason, "run {}: {reason}", run.id());
                queue.record_runtime_event(
                    run.id(),
                    EventKind::IntegrationRetried,
                    Reason::new(ReasonCode::VerificationFlaky)
                        .with("index", index)
                        .on(json!({
                            "main": main,
                            "head": rebased,
                            "attempt": attempt,
                            "command": command,
                            "exit_code": last.exit_code,
                            "failure": failure.to_json(),
                            "failed_tests": last.tests_json(),
                            "flaky_tests": last.flaky_tests,
                            "log_path": last.log.to_str(),
                            "reason": reason,
                        })),
                )?;
                continue 'verify;
            }
            return defer(
                Reason::new(ReasonCode::VerificationFailed).with("index", index),
                format!(
                    "verification command {command:?} exited with {} after the rebase onto {main} ({}: {}); see {}",
                    last.exit_code,
                    failure.class.as_str(),
                    failure.evidence,
                    last.log.display()
                ),
                json!({
                    "main": main,
                    "head": rebased,
                    "command": command,
                    "exit_code": last.exit_code,
                    "signal": last.signal,
                    "failure": failure.to_json(),
                    "failed_tests": last.tests_json(),
                    "failed_tests_omitted": last.omitted_json(),
                    "flaky_tests": last.flaky_tests,
                }),
            );
        }
        break;
    }
    // One commit on main with the rebased tree; the run's own history stays
    // reachable under refs/dagq/runs/<run-id>.
    let paragraphs = commit_message(task, run, &receipt);
    let tree = repository.tree_of(rebased.as_str())?;
    let commit = repository.commit_tree(&tree, main.as_str(), &paragraphs)?;
    let history_ref = format!("refs/dagq/runs/{}", run.id());
    repository.update_ref(&history_ref, rebased.as_str())?;
    repository.advance_main(landing_branch, main.as_str(), commit.as_str())?;
    Ok(Verdict::Landed(
        Landing {
            commit,
            source_commit: rebased,
            main_before: main.clone(),
            history_ref,
            message: paragraphs.join("\n\n"),
            verification_skipped: false,
        },
        receipt.into_follow_ups(),
    ))
}

/// How integrate runs one verification command of an attempt and records
/// it as `verification_command`.
struct VerifyStep<'a> {
    verifier: &'a dyn Verifier,
    load_average: fn() -> Option<f64>,
    files: &'a dyn RunFiles,
    run: &'a TaskRun,
    worktree: &'a Path,
    run_env: &'a [(String, String)],
    attempt: u32,
}

/// One run of a verification command: its exit, why it failed (none when
/// it passed), the tests it names as failed, and its log.
#[derive(Clone)]
struct Checked {
    exit_code: i32,
    signal: Option<i32>,
    failure: Option<verify_failure::VerifyFailure>,
    failed_tests: Option<verify_failure::FailedTests>,
    /// Those of the failed tests nextest ran again and saw pass (task 768).
    flaky_tests: Vec<String>,
    log: PathBuf,
}

impl Checked {
    fn tests_json(&self) -> Value {
        json!(self.failed_tests.as_ref().map(|tests| &tests.names))
    }

    fn omitted_json(&self) -> Value {
        json!(self.failed_tests.as_ref().map(|tests| tests.omitted))
    }
}

impl VerifyStep<'_> {
    /// Run `command`, the `index`th, with its output in `log`, and record
    /// it; `retry_of` is the run it retries (task 639), whose failure and
    /// log the event names. A command the verifier killed at its limit for
    /// the whole command is a `timeout` failure, not an error.
    fn run(
        &self,
        queue: &mut dyn Queue,
        index: usize,
        command: &str,
        log: &Path,
        retry_of: Option<&Checked>,
    ) -> Result<Checked> {
        let started = Instant::now();
        let (status, load) = sampled(self.load_average, LOAD_SAMPLE_INTERVAL, || {
            self.verifier
                .run_to_log(command, self.worktree, self.run_env, log)
        });
        let duration_secs = (started.elapsed().as_secs_f64() * 1000.0).round() / 1000.0;
        let (code, signal, timed_out) = match status {
            Ok(status) => (status.code, status.signal, None),
            Err(error) => match error.downcast_ref::<verify_failure::CommandTimedOut>() {
                // Killed at the limit (SIGKILL).
                Some(timed_out) => (None, Some(9), Some(*timed_out)),
                None => return Err(error),
            },
        };
        let exit_code = code.unwrap_or(128);
        // Lossy: a log cut off by a kill or a full disk may end mid-character,
        // and its marks still count.
        let output = self
            .files
            .read(log)
            .map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
            .unwrap_or_default();
        // Why it failed, from its exit and its log (task 467), or its limit.
        let failure = match timed_out {
            Some(timed_out) => Some(timed_out.failure()),
            None => {
                (exit_code != 0).then(|| verify_failure::classify(command, code, signal, &output))
            }
        };
        // The tests it names as failed, when tests failed or ran out of
        // time (task 515).
        let failed_tests = failure
            .as_ref()
            .filter(|failure| {
                matches!(
                    failure.class,
                    verify_failure::FailureClass::TestFailure
                        | verify_failure::FailureClass::Timeout
                        | verify_failure::FailureClass::Flaky
                )
            })
            .map(|_| verify_failure::failed_tests(&output));
        // The failed tests that passed on nextest's retry: the mark `stats`
        // counts them flaky by (task 768).
        let flaky_tests = match &failed_tests {
            Some(failed) => verify_failure::flaky_tests(&output)
                .into_iter()
                .filter(|name| failed.names.contains(name))
                .collect(),
            None => Vec::new(),
        };
        let checked = Checked {
            exit_code,
            signal,
            failure,
            failed_tests,
            flaky_tests,
            log: log.to_path_buf(),
        };
        let mut payload = json!({
            "phase": "integration",
            "attempt": self.attempt,
            "index": index,
            "command": command,
            "exit_code": exit_code,
            "signal": signal,
            "failure": checked.failure.as_ref().map(|failure| failure.to_json()),
            "failed_tests": checked.tests_json(),
            "failed_tests_omitted": checked.omitted_json(),
            "flaky_tests": checked.flaky_tests,
            "duration_secs": duration_secs,
            "load_avg_mean": load.load_avg_mean,
            "load_avg_max": load.load_avg_max,
            "log_path": path_text(log)?,
            "output_tail": tail(&output, 2000),
        });
        if let Some(first) = retry_of {
            payload["retry"] = json!(true);
            payload["retry_of"] = json!({
                "failure": first.failure.as_ref().map(|failure| failure.to_json()),
                "log_path": path_text(&first.log)?,
            });
        }
        queue.record_runtime_event(self.run.id(), EventKind::VerificationCommand, payload)?;
        Ok(checked)
    }
}

/// The verdict on a verification command that failed on the host: `last`
/// failed again when it retried `first`, or, with `first` `None`, failed on
/// a full disk that is `short` of room to retry it (task 639). The run
/// waits for a person, who lands it again once the host is fixed.
#[allow(clippy::too_many_arguments)]
fn held(
    run: &TaskRun,
    main: &CommitSha,
    head: &CommitSha,
    command: &str,
    index: usize,
    last: &Checked,
    first: Option<&Checked>,
    short: Option<DiskShort>,
) -> Verdict {
    let failure = last
        .failure
        .clone()
        .unwrap_or(verify_failure::VerifyFailure {
            class: verify_failure::FailureClass::Unknown,
            evidence: String::new(),
        });
    let logs = first
        .map(|first| format!("{} and {}", first.log.display(), last.log.display()))
        .unwrap_or_else(|| last.log.display().to_string());
    let why = match (first, short) {
        (_, Some(short)) => format!(
            "it was not retried: {} free, below the {} a landing's verification needs; free disk space (dagq doctor lists the runs and their worktrees)",
            gib(short.free as f64),
            gib(short.need as f64)
        ),
        _ => "it failed so again when retried once; fix the host (free disk space, a lighter load)"
            .to_owned(),
    };
    Verdict::Held {
        reason: format!(
            "verification command {command:?} failed on the host after the rebase onto {main} ({}: {}); see {logs}. {why}, then land it with dagq integrate {}; no session is resumed",
            failure.class.as_str(),
            failure.evidence,
            run.task_id()
        ),
        detail: Reason::new(ReasonCode::VerificationEnvironment)
            .with("index", index)
            .on(json!({
                "main": main,
                "head": head,
                "command": command,
                "exit_code": last.exit_code,
                "signal": last.signal,
                "failure": failure.to_json(),
                "log_path": last.log.to_str(),
                "retried": first.is_some(),
                "first_failure": first.and_then(|first| first.failure.as_ref().map(|failure| failure.to_json())),
                "first_log_path": first.and_then(|first| first.log.to_str()),
                "disk": short.map(DiskShort::to_json),
            })),
    }
}

/// What [`renumber_migration`] did to a rebased run.
enum Renumbering {
    /// No migration the run adds has a number `main` has.
    Unchanged,
    /// The run's one migration moved to the next free number; its new head.
    Renumbered(CommitSha),
    /// A number is taken but cannot be moved mechanically; the reason for
    /// the session, with the numbers.
    Blocked { reason: String, detail: Value },
}

/// Renumber the migration the rebased run adds when `main` already has its
/// number (ADR-0067 decision 3), that is when another file of the rebased
/// tree, one the run did not add, has it: moved with `git mv` to the next number
/// free on `main` and committed on the run branch, recorded as
/// `migration_renumbered`. Only a run that adds exactly one migration, whose
/// number none of the run's other changed files mentions, is moved; any
/// other collision is left to a session with the next free number.
///
/// Only in dagq's source repository (ADR-t614-1): the numbers are those of
/// dagq's own queue schema, and another repository's `migrations/` is its
/// own, left as any other file.
fn renumber_migration(
    queue: &mut dyn Queue,
    repository: &dyn Repository,
    run: &TaskRun,
    worktree: &Path,
    main: &CommitSha,
    rebased: &CommitSha,
) -> Result<Renumbering> {
    if !repository.is_dagq_source() {
        return Ok(Renumbering::Unchanged);
    }
    let in_directory = |path: &str| {
        path.strip_prefix(migration_numbers::DIRECTORY)
            .and_then(|rest| rest.strip_prefix('/'))
            .filter(|name| migration_numbers::number(name).is_some())
            .map(str::to_owned)
    };
    let added: Vec<String> = repository
        .added_paths(main.as_str(), rebased.as_str())?
        .iter()
        .filter_map(|path| in_directory(path))
        .collect();
    if added.is_empty() {
        return Ok(Renumbering::Unchanged);
    }
    // Judged on the rebased tree, not on main's: a migration the run renames
    // or replaces keeps its number without a collision. What else the
    // rebased tree has came from main.
    let kept: Vec<u32> = repository
        .paths_in(rebased.as_str(), migration_numbers::DIRECTORY)?
        .iter()
        .filter_map(|path| in_directory(path))
        .filter(|name| !added.contains(name))
        .filter_map(|name| migration_numbers::number(&name))
        .collect();
    let taken: Vec<&String> = added
        .iter()
        .filter(|name| kept.contains(&migration_numbers::number(name).unwrap_or(0)))
        .collect();
    if taken.is_empty() {
        return Ok(Renumbering::Unchanged);
    }
    let next = kept.iter().copied().max().unwrap_or(0) + 1;
    let next_digits = migration_numbers::digits(next);
    let path = |name: &str| format!("{}/{name}", migration_numbers::DIRECTORY);
    let blocked = |why: String, extra: Value| {
        let mut detail = json!({
            "main": main,
            "head": rebased,
            "migrations": added.iter().map(|name| path(name)).collect::<Vec<_>>(),
            "taken": taken.iter().map(|name| path(name)).collect::<Vec<_>>(),
            "next_number": next_digits,
        });
        if let (Some(detail), Value::Object(extra)) = (detail.as_object_mut(), extra) {
            detail.extend(extra);
        }
        Ok(Renumbering::Blocked {
            reason: format!(
                "{why}; main {main} already has the number of {}, and the next free number is {next_digits}: renumber the run's migrations from {next_digits} (git mv), update what refers to their numbers, rerun the verification commands, and rewrite the receipt with the new head",
                taken
                    .iter()
                    .map(|name| path(name))
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
            detail,
        })
    };
    if added.len() > 1 {
        return blocked(
            format!(
                "the run adds {} migrations, which are not renumbered mechanically",
                added.len()
            ),
            json!({}),
        );
    }
    let name = &added[0];
    let old = path(name);
    let old_digits = migration_numbers::digits(migration_numbers::number(name).unwrap_or(0));
    let others: Vec<String> = repository
        .changed_paths(main.as_str(), rebased.as_str())?
        .into_iter()
        .filter(|changed| *changed != old)
        .collect();
    let referring = repository.paths_containing(rebased.as_str(), &old_digits, &others)?;
    if !referring.is_empty() {
        return blocked(
            format!(
                "the run's other changes mention {old_digits} ({})",
                referring.join(", ")
            ),
            json!({"referring": referring}),
        );
    }
    let new = path(&migration_numbers::renumbered(name, next));
    let head = match repository.rename_and_commit(
        worktree,
        &old,
        &new,
        &[
            format!("fix: renumber migration {old_digits} to {next_digits}"),
            format!(
                "main {main} took number {old_digits} while the run was open, so dagq integrate moved {old} to {new}."
            ),
        ],
    )? {
        Ok(head) => head,
        // The rename is undone and the worktree is back at the rebased head,
        // so the session can move the migration itself.
        Err(refused) => {
            let gist = commit_error_gist(&refused);
            return blocked(
                format!("git refused the commit moving {old} to {new} ({gist})"),
                json!({"commit_error": gist}),
            );
        }
    };
    queue.record_runtime_event(
        run.id(),
        EventKind::MigrationRenumbered,
        json!({
            "main": main,
            "from": old,
            "to": new,
            "old_number": old_digits,
            "new_number": next_digits,
            "head_before": rebased,
            "head_after": head,
        }),
    )?;
    info!(
        op = "integrate",
        run_id = %run.id(),
        "run {}: renumbered migration {old} to {new}",
        run.id()
    );
    Ok(Renumbering::Renumbered(head))
}

/// The lines of what Git said when it refused a commit, joined and cut to
/// 500 characters: enough for the session to see why.
fn commit_error_gist(refused: &str) -> String {
    let joined = refused
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .collect::<Vec<_>>()
        .join("; ");
    if joined.is_empty() {
        return "git commit failed without output".to_owned();
    }
    match joined.char_indices().nth(500) {
        Some((cut, _)) => format!("{}…", &joined[..cut]),
        None => joined,
    }
}

/// How often [`sampled`] reads the load average while a verification
/// command runs.
const LOAD_SAMPLE_INTERVAL: Duration = Duration::from_secs(5);

/// Run `work`, sampling `load_average` when it starts, every `every` while
/// it runs, and when it ends: the load over one verification command.
fn sampled<T>(
    load_average: fn() -> Option<f64>,
    every: Duration,
    work: impl FnOnce() -> T,
) -> (T, LoadSummary) {
    let (stop, stopped) = mpsc::channel::<()>();
    thread::scope(|scope| {
        let sampler = scope.spawn(move || {
            let mut window = LoadWindow::default();
            loop {
                window.add(load_average());
                match stopped.recv_timeout(every) {
                    Err(mpsc::RecvTimeoutError::Timeout) => (),
                    _ => break,
                }
            }
            window.add(load_average());
            window.summary()
        });
        let value = work();
        drop(stop);
        let load = sampler.join().unwrap_or_default();
        (value, load)
    })
}

/// Where integrate's attempt `attempt` writes the log of its `index`th
/// verification command (both from 1): `integrate-<attempt>-verify-<index>.log`.
pub fn integrate_verify_log(run_dir: &Path, attempt: u32, index: usize) -> PathBuf {
    run_dir.join(format!("integrate-{attempt}-verify-{index}.log"))
}

/// Where integrate's attempt `attempt` writes the log of the retry of its
/// `index`th verification command (task 639), next to the first run's:
/// `integrate-<attempt>-verify-<index>-retry.log`.
pub fn integrate_retry_log(run_dir: &Path, attempt: u32, index: usize) -> PathBuf {
    run_dir.join(format!("integrate-{attempt}-verify-{index}-retry.log"))
}

/// The attempt, command index and whether it is the retry's (task 639) of
/// an integrate verification log's file name. The name used before
/// attempts were counted, `integrate-verify-<index>.log`, is attempt 0: it
/// came before any numbered one.
fn integrate_log_key(name: &str) -> Option<(u32, usize, bool)> {
    let stem = name.strip_prefix("integrate-")?.strip_suffix(".log")?;
    let (stem, retry) = match stem.strip_suffix("-retry") {
        Some(stem) => (stem, true),
        None => (stem, false),
    };
    if let Some(index) = stem.strip_prefix("verify-") {
        return Some((0, index.parse().ok()?, retry));
    }
    let (attempt, index) = stem.split_once("-verify-")?;
    Some((attempt.parse().ok()?, index.parse().ok()?, retry))
}

/// The files of `dir` with their names; none when it cannot be read.
pub fn log_names(files: &dyn RunFiles, dir: &Path) -> Vec<(String, PathBuf)> {
    files
        .read_dir(dir)
        .map(|entries| {
            entries
                .into_iter()
                .filter_map(|path| {
                    let name = path.file_name()?.to_str()?.to_owned();
                    Some((name, path))
                })
                .collect()
        })
        .unwrap_or_default()
}

/// The attempt integrate's next run of the verification commands writes
/// its logs under: one past the highest attempt in `run_dir` (1 when none).
pub fn next_integrate_attempt(files: &dyn RunFiles, run_dir: &Path) -> u32 {
    log_names(files, run_dir)
        .iter()
        .filter_map(|(name, _)| integrate_log_key(name))
        .map(|(attempt, _, _)| attempt)
        .max()
        .map_or(1, |attempt| attempt + 1)
}

/// Integrate's verification logs in `run_dir`: those of the latest attempt
/// in command order, and those of the earlier attempts, oldest first.
pub fn integrate_logs(files: &dyn RunFiles, run_dir: &Path) -> (Vec<PathBuf>, Vec<PathBuf>) {
    let mut keyed: Vec<((u32, usize, bool), PathBuf)> = log_names(files, run_dir)
        .into_iter()
        .filter_map(|(name, path)| Some((integrate_log_key(&name)?, path)))
        .collect();
    keyed.sort();
    let Some(&((latest, _, _), _)) = keyed.last() else {
        return (Vec::new(), Vec::new());
    };
    let (current, earlier): (Vec<_>, Vec<_>) = keyed
        .into_iter()
        .partition(|((attempt, _, _), _)| *attempt == latest);
    (
        current.into_iter().map(|(_, path)| path).collect(),
        earlier.into_iter().map(|(_, path)| path).collect(),
    )
}

/// Title, the receipt's summary, and the trailers that tie the commit to
/// the queue, as paragraphs.
fn commit_message(task: &Task, run: &TaskRun, receipt: &Receipt) -> Vec<String> {
    let mut paragraphs = vec![task.title().trim().to_owned()];
    let summary = receipt.summary().trim();
    if !summary.is_empty() {
        paragraphs.push(summary.to_owned());
    }
    paragraphs.push(format!("Dagq-Task: {}\nDagq-Run: {}", task.id(), run.id()));
    paragraphs
}

/// The answer an `approve_landing` ask of a run nobody closed is closed
/// with once the run is integrated (task 425).
const LANDED_LANDING_ASK_CLOSED: &str = "the run was integrated; closed by the runtime";

/// Close the `approve_landing` asks of the landed `run` nobody closed
/// (task 425), and the `blocked` asks of the run or its task (task 329):
/// whether it was landed by hand or by the supervisor, nobody needs to
/// answer them any more. `main` already moved, so a failure is only
/// reported.
fn close_landing_asks(queue: &mut dyn AskStore, run: &TaskRun) {
    let closes = [
        (
            "approve_landing",
            queue.close_approve_landing_asks(run.id(), LANDED_LANDING_ASK_CLOSED),
        ),
        (
            "blocked",
            queue.close_blocked_asks(run.id(), run.task_id(), LANDED_LANDING_ASK_CLOSED),
        ),
    ];
    for (kind, result) in closes {
        match result {
            Ok(closed) => {
                for ask in closed {
                    info!(op = "integrate", ask_id = %ask.id, "run {}: closed its {kind} ask {} as it was integrated", run.id(), ask.id);
                }
            }
            Err(error) => warn!(
                op = "integrate",
                error = %format_args!("{error:#}"),
                "run {}: could not close its {kind} asks: {error:#}",
                run.id()
            ),
        }
    }
}

/// Drop the landed run's worktree and branch. The result is already on
/// `main` and under the history ref, so a failure here is only recorded.
fn remove_landed_worktree(queue: &mut dyn Queue, repository: &dyn Repository, run: &TaskRun) {
    let (Some(worktree), Some(branch)) = (&run.worktree_path(), &run.branch()) else {
        return;
    };
    let recorded = match repository.remove_worktree_and_branch(Path::new(worktree), branch) {
        Ok(()) => queue.record_runtime_event(
            run.id(),
            EventKind::WorktreeRemoved,
            json!({"path": worktree, "branch": branch}),
        ),
        Err(error) => {
            let message = format!("landed worktree {worktree} could not be removed: {error:#}");
            warn!(op = "cleanup", run_id = %run.id(), "run {}: {message}", run.id());
            queue.record_cleanup_failure(run.id(), &message, &ReasonCode::GitFailed.into())
        }
    };
    if let Err(error) = recorded {
        warn!(
            op = "cleanup",
            run_id = %run.id(),
            error = %format_args!("{error:#}"),
            "run {}: could not record the cleanup: {error:#}",
            run.id()
        );
    }
}

/// How many more counted resumes the run has (ADR-0047 decision 24: a
/// resume of a run parked only by a conflict after its review passed is
/// not counted); unreadable events leave none.
fn resumes_left<Q: RunLog + ?Sized>(queue: &Q, id: &RunId) -> usize {
    queue.run_events(id).map_or(0, |events| {
        RunHistory::from_events(&events).resumes().left()
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::application::memory_files::MemoryFiles;
    use crate::application::{EndedRunWorkspace, EndedRunWorktree};
    use crate::domain::{EventId, Provider, RunEvent, RunRecord, TaskRecord, TaskStatus};
    use std::cell::RefCell;

    const BASE: &str = "1111111111111111111111111111111111111111";
    const HEAD: &str = "2222222222222222222222222222222222222222";
    const RUN: &str = "00000000-0000-4000-8000-000000000001";

    /// A repository whose worktree is on `branch` at `head`, with `status`.
    struct FakeRepository {
        branch: Option<String>,
        head: CommitSha,
        status: String,
        calls: RefCell<Vec<String>>,
    }

    impl FakeRepository {
        fn sound() -> Self {
            Self {
                branch: Some(format!("refs/heads/dagq/{RUN}")),
                head: sha(HEAD),
                status: String::new(),
                calls: RefCell::default(),
            }
        }
    }

    impl Repository for FakeRepository {
        fn is_dagq_source(&self) -> bool {
            true
        }
        fn main_head(&self) -> Result<CommitSha> {
            Ok(sha(BASE))
        }
        fn current_branch(&self, _: &Path) -> Result<Option<String>> {
            Ok(self.branch.clone())
        }
        fn head(&self, _: &Path) -> Result<CommitSha> {
            Ok(self.head.clone())
        }
        fn is_ancestor(&self, ancestor: &str, descendant: &str) -> Result<bool> {
            self.calls
                .borrow_mut()
                .push(format!("is_ancestor {ancestor} {descendant}"));
            Ok(ancestor == BASE)
        }
        fn merge_base(&self, _: &str, _: &str) -> Result<Option<CommitSha>> {
            Ok(Some(sha(BASE)))
        }
        fn status(&self, _: &Path) -> Result<String> {
            Ok(self.status.clone())
        }
        fn rebase_in_progress(&self, _: &Path) -> Result<bool> {
            unimplemented!()
        }
        fn rebase_abort(&self, _: &Path) -> Result<()> {
            unimplemented!()
        }
        fn rebase(&self, _: &Path, _: &str) -> Result<std::result::Result<(), String>> {
            unimplemented!()
        }
        fn conflicted_files(&self, _: &Path) -> Result<Vec<String>> {
            unimplemented!()
        }
        fn changed_paths(&self, _: &str, _: &str) -> Result<Vec<String>> {
            Ok(vec!["src/lib.rs".to_owned()])
        }
        fn added_paths(&self, _: &str, _: &str) -> Result<Vec<String>> {
            unimplemented!()
        }
        fn paths_in(&self, _: &str, _: &str) -> Result<Vec<String>> {
            unimplemented!()
        }
        fn paths_containing(&self, _: &str, _: &str, _: &[String]) -> Result<Vec<String>> {
            unimplemented!()
        }
        fn rename_and_commit(
            &self,
            _: &Path,
            _: &str,
            _: &str,
            _: &[String],
        ) -> Result<std::result::Result<CommitSha, String>> {
            unimplemented!()
        }
        fn tree_of(&self, _: &str) -> Result<String> {
            unimplemented!()
        }
        fn commit_tree(&self, _: &str, _: &str, _: &[String]) -> Result<CommitSha> {
            unimplemented!()
        }
        fn update_ref(&self, _: &str, _: &str) -> Result<()> {
            unimplemented!()
        }
        fn advance_main(&self, _: &LandingBranch, _: &str, _: &str) -> Result<()> {
            unimplemented!()
        }
        fn repair_worktree(&self, _: &Path) -> Result<()> {
            unimplemented!()
        }
        fn tracks(&self, _: &Path, _: &str) -> Result<bool> {
            unimplemented!()
        }
        fn remove_worktree_and_branch(&self, _: &Path, _: &str) -> Result<()> {
            unimplemented!()
        }
        fn branches(&self) -> Result<Vec<String>> {
            unimplemented!()
        }
        fn delete_branch(&self, _: &str) -> Result<()> {
            unimplemented!()
        }
        fn main_checkout(&self) -> Result<Option<PathBuf>> {
            unimplemented!()
        }
        fn create_worktree(&self, _: &TaskRun) -> Result<String> {
            unimplemented!()
        }
        fn merge_conflicts(&self, _: &str, _: &str) -> Result<Vec<String>> {
            unimplemented!()
        }
        fn landed_task_ids(&self, _: &str, _: &str) -> Result<Vec<crate::domain::TaskId>> {
            unimplemented!()
        }
        fn log_oneline(&self, _: &str, _: &str) -> Result<String> {
            unimplemented!()
        }
        fn diff_stat(&self, _: &str, _: &str) -> Result<String> {
            unimplemented!()
        }
        fn diff_numbers(&self, _: &str, _: &str) -> Result<crate::application::DiffNumbers> {
            unimplemented!()
        }
        fn diff_to_file(&self, _: &str, _: &str, _: &Path) -> Result<()> {
            unimplemented!()
        }
    }

    fn sha(text: &str) -> CommitSha {
        CommitSha::parse(text, "commit").unwrap()
    }

    fn task(paths: &[&str]) -> Task {
        task_requiring(paths, &[EvidenceCheck::Tests])
    }

    fn task_requiring(paths: &[&str], required: &[EvidenceCheck]) -> Task {
        Task::restore(TaskRecord {
            id: TaskId::new(7),
            title: "  land the change  ".to_owned(),
            description: String::new(),
            acceptance: String::new(),
            verification_commands: Vec::new(),
            required_evidence: required.to_vec(),
            paths: paths.iter().map(|p| (*p).to_owned()).collect(),
            priority: Default::default(),
            kind: None,
            change: None,
            status: TaskStatus::InProgress,
            goal_id: None,
            context: String::new(),
            created_at: String::new(),
            updated_at: String::new(),
            worker: crate::domain::worker::Worker::DEFAULT,
        })
        .unwrap()
    }

    fn run(dir: &Path) -> TaskRun {
        run_in(dir, RunStatus::Validating)
    }

    fn run_in(dir: &Path, status: RunStatus) -> TaskRun {
        run_on(dir, status, Provider::Claude)
    }

    fn run_on(dir: &Path, status: RunStatus, provider: Provider) -> TaskRun {
        TaskRun::restore(RunRecord {
            id: RunId::new(RUN).unwrap(),
            task_id: TaskId::new(7),
            status,
            requested_provider: provider,
            actual_provider: provider,
            worker_mode: crate::domain::worker::Worker::default_mode(provider),
            base_commit: sha(BASE),
            branch: Some(format!("dagq/{RUN}")),
            worktree_path: Some(path_text(dir).unwrap()),
            workspace_id: None,
            receipt_path: Some(path_text(&dir.join("receipt.json")).unwrap()),
            log_path: None,
            result_commit: None,
            repo_path: None,
            run_dir: Some(path_text(dir).unwrap()),
            last_error: None,
            workspace_closed_at: None,
            created_at: String::new(),
        })
        .unwrap()
    }

    /// Where the run's files are in [`MemoryFiles`].
    const DIR: &str = "/runs/run";

    fn write_receipt(files: &MemoryFiles, dir: &Path, tests: &str) {
        let receipt = json!({
            "run_id": RUN,
            "result": "succeeded",
            "commit": HEAD,
            "tests": {"status": tests, "evidence_or_reason": "cargo test"},
            "e2e": {"status": "not_applicable", "evidence_or_reason": "no runtime change"},
            "subagent_review": {"status": "not_applicable", "evidence_or_reason": "small"},
            "summary": "  squash it  ",
        });
        files
            .write(&dir.join("receipt.json"), receipt.to_string().as_bytes())
            .unwrap();
    }

    #[test]
    fn a_sound_receipt_is_accepted_through_the_repository_port() {
        let (files, dir) = (MemoryFiles::default(), Path::new(DIR));
        write_receipt(&files, dir, "passed");
        let repository = FakeRepository::sound();
        let (receipt, commit) = check_receipt(&repository, &files, &task(&[]), &run(dir))
            .unwrap()
            .unwrap_or_else(|rejection| panic!("{}", rejection.reason));
        assert_eq!(commit, sha(HEAD));
        assert_eq!(
            repository.calls.borrow().as_slice(),
            [format!("is_ancestor {BASE} {HEAD}")]
        );
        assert_eq!(
            commit_message(&task(&[]), &run(dir), &receipt),
            [
                "land the change".to_owned(),
                "squash it".to_owned(),
                format!("Dagq-Task: 7\nDagq-Run: {RUN}"),
            ]
        );
    }

    #[test]
    fn check_receipt_rejects_what_git_does_not_back() {
        let (files, dir) = (MemoryFiles::default(), Path::new(DIR));
        let reason = |repository: &FakeRepository, task: &Task| match check_receipt(
            repository,
            &files,
            task,
            &run(dir),
        )
        .unwrap()
        {
            Ok(_) => panic!("accepted"),
            Err(rejection) => rejection,
        };
        let missing = reason(&FakeRepository::sound(), &task(&[]));
        assert!(missing.reason.starts_with("receipt was not submitted at "));
        assert!(missing.receipt.is_none());

        write_receipt(&files, dir, "passed");
        let detached = FakeRepository {
            branch: None,
            ..FakeRepository::sound()
        };
        assert_eq!(
            reason(&detached, &task(&[])).reason,
            format!("worktree is on a detached HEAD instead of refs/heads/dagq/{RUN}")
        );
        let dirty = FakeRepository {
            status: " M src/lib.rs\n".to_owned(),
            ..FakeRepository::sound()
        };
        let rejection = reason(&dirty, &task(&[]));
        assert_eq!(rejection.reason, "worktree is not clean:\n M src/lib.rs");
        assert_eq!(rejection.commit, Some(sha(HEAD)));
        let outside = reason(&FakeRepository::sound(), &task(&["docs/**"]));
        assert_eq!(outside.scope_violation, ["src/lib.rs"]);

        write_receipt(&files, dir, "not_applicable");
        let evidence = reason(&FakeRepository::sound(), &task(&[]));
        assert_eq!(evidence.evidence_missing, [EvidenceCheck::Tests]);
    }

    /// A Codex worker has no subagent: its receipt reports subagent_review
    /// not_applicable with a reason, and passes validation for a task that
    /// requires the evidence; the same receipt of a Claude run waits for it.
    #[test]
    fn a_codex_receipt_passes_without_a_subagent_review_the_task_requires() {
        let (files, dir) = (MemoryFiles::default(), Path::new(DIR));
        write_receipt(&files, dir, "passed");
        let task = task_requiring(&[], &[EvidenceCheck::Tests, EvidenceCheck::SubagentReview]);
        let codex = run_on(dir, RunStatus::Validating, Provider::Codex);
        let (receipt, commit) = check_receipt(&FakeRepository::sound(), &files, &task, &codex)
            .unwrap()
            .unwrap_or_else(|rejection| panic!("{}", rejection.reason));
        assert_eq!(commit, sha(HEAD));
        assert_eq!(
            receipt.subagent_review().status(),
            crate::domain::CheckStatus::NotApplicable
        );
        let claude = run_on(dir, RunStatus::Validating, Provider::Claude);
        let rejection = check_receipt(&FakeRepository::sound(), &files, &task, &claude)
            .unwrap()
            .expect_err("a Claude run owes the subagent review");
        assert_eq!(rejection.evidence_missing, [EvidenceCheck::SubagentReview]);
    }

    #[test]
    fn integrate_logs_are_read_through_the_run_files_port() {
        let (files, dir) = (MemoryFiles::default(), Path::new(DIR));
        assert_eq!(next_integrate_attempt(&files, dir), 1);
        for name in [
            "integrate-verify-1.log",
            "integrate-2-verify-2.log",
            "integrate-2-verify-1.log",
            "verify-1.log",
        ] {
            files.write(&dir.join(name), b"").unwrap();
        }
        files
            .write(Path::new("/elsewhere/integrate-9-verify-1.log"), b"")
            .unwrap();
        assert_eq!(next_integrate_attempt(&files, dir), 3);
        assert_eq!(
            integrate_logs(&files, dir),
            (
                vec![
                    dir.join("integrate-2-verify-1.log"),
                    dir.join("integrate-2-verify-2.log")
                ],
                vec![dir.join("integrate-verify-1.log")]
            )
        );
    }

    #[test]
    fn tail_keeps_the_last_bytes_on_a_character_boundary() {
        assert_eq!(tail("abcdef", 3), "def");
        assert_eq!(tail("ab", 3), "ab");
        // A multi-byte character is not split.
        assert_eq!(tail("aé", 2), "é");
        assert_eq!(tail("aé", 1), "");
    }

    #[test]
    fn commit_error_gist_joins_the_lines_and_cuts_long_output() {
        assert_eq!(
            commit_error_gist(
                "error: gpg failed to sign the data\n\nfatal: failed to write commit object\n"
            ),
            "error: gpg failed to sign the data; fatal: failed to write commit object"
        );
        assert_eq!(commit_error_gist(" \n"), "git commit failed without output");
        let long = "é".repeat(600);
        assert_eq!(commit_error_gist(&long), format!("{}…", "é".repeat(500)));
    }

    /// The run log alone, for a use case that reads only it: a test double
    /// implements this one port, not the whole queue.
    struct EventsOnly {
        events: Option<Vec<RunEvent>>,
    }

    #[allow(unused_variables)]
    impl RunLog for EventsOnly {
        fn update_events(&self, limit: usize) -> Result<Vec<RunEvent>> {
            unreachable!("resumes_left reads only the run's events")
        }
        fn active_runs(&self) -> Result<Vec<TaskRun>> {
            unreachable!("resumes_left reads only the run's events")
        }
        fn all_runs(&self) -> Result<Vec<TaskRun>> {
            unreachable!("resumes_left reads only the run's events")
        }
        fn all_events(&self) -> Result<Vec<RunEvent>> {
            unreachable!("resumes_left reads only the run's events")
        }
        fn latest_task_events(&self, kinds: &[&str]) -> Result<Vec<RunEvent>> {
            unreachable!("resumes_left reads only the run's events")
        }
        fn run(&self, id: &RunId) -> Result<TaskRun> {
            unreachable!("resumes_left reads only the run's events")
        }
        fn runs_with_status(&self, status: RunStatus) -> Result<Vec<TaskRun>> {
            unreachable!("resumes_left reads only the run's events")
        }
        fn next_awaiting_integration(&self) -> Result<Option<TaskRun>> {
            unreachable!("resumes_left reads only the run's events")
        }
        fn run_events(&self, _id: &RunId) -> Result<Vec<RunEvent>> {
            self.events
                .clone()
                .ok_or_else(|| anyhow::anyhow!("unreadable"))
        }
        fn has_run_event(&self, id: &RunId, kind: &str) -> Result<bool> {
            unreachable!("resumes_left reads only the run's events")
        }
        fn record_runtime_event(
            &self,
            id: &RunId,
            kind: EventKind,
            payload: serde_json::Value,
        ) -> Result<()> {
            unreachable!("resumes_left reads only the run's events")
        }
        fn ended_run_workspaces(&self) -> Result<Vec<EndedRunWorkspace>> {
            unreachable!("resumes_left reads only the run's events")
        }
        fn ended_run_worktrees(&self) -> Result<Vec<EndedRunWorktree>> {
            unreachable!("resumes_left reads only the run's events")
        }
        fn last_observe(&self, mode: &str) -> Result<Option<i64>> {
            unreachable!("resumes_left reads only the run's events")
        }
        fn latest_event_id(&self) -> Result<EventId> {
            unreachable!("resumes_left reads only the run's events")
        }
        fn latest_runs_in_progress(&self) -> Result<Vec<TaskRun>> {
            unreachable!("resumes_left reads only the run's events")
        }
        fn runs_with_pending_push(&self) -> Result<Vec<TaskRun>> {
            unreachable!("resumes_left reads only the run's events")
        }
        fn run_in_workspace(&self, workspace_id: &str) -> Result<Option<RunId>> {
            unreachable!("resumes_left reads only the run's events")
        }
        fn record_backend_failure(
            &self,
            run: Option<&RunId>,
            payload: serde_json::Value,
        ) -> Result<()> {
            unreachable!("resumes_left reads only the run's events")
        }
        fn record_queue_event(
            &self,
            kind: EventKind,
            payload: serde_json::Value,
        ) -> Result<EventId> {
            unreachable!("resumes_left reads only the run's events")
        }
        fn latest_event_of(&self, kind: &str) -> Result<Option<RunEvent>> {
            unreachable!("resumes_left reads only the run's events")
        }
        fn latest_events_of(&self, kind: &str, limit: usize) -> Result<Vec<RunEvent>> {
            unreachable!("resumes_left reads only the run's events")
        }
        fn claim_inbox_nudge(&self, payload: serde_json::Value) -> Result<bool> {
            unreachable!("resumes_left reads only the run's events")
        }
        fn latest_queue_event(&self, kinds: &[&str]) -> Result<Option<RunEvent>> {
            unreachable!("resumes_left reads only the run's events")
        }
        fn events_of_between(
            &self,
            kinds: &[&str],
            after: EventId,
            upto: EventId,
            limit: usize,
        ) -> Result<Vec<RunEvent>> {
            unreachable!("resumes_left reads only the run's events")
        }
    }

    #[test]
    fn resumes_left_reads_the_run_log_alone() {
        let run = RunId::new(RUN).unwrap();
        let fresh = EventsOnly {
            events: Some(Vec::new()),
        };
        assert_eq!(
            resumes_left(&fresh, &run),
            RunHistory::from_events(&[]).resumes().left()
        );
        assert_eq!(resumes_left(&EventsOnly { events: None }, &run), 0);
    }

    fn integrating() -> TaskRun {
        run_in(Path::new("/tmp/run"), RunStatus::Integrating)
    }

    fn landing_event(kind: &str, payload: Value) -> crate::domain::RunEvent {
        crate::domain::RunEvent {
            id: crate::domain::EventId::new(1),
            task_id: None,
            goal_id: None,
            run_id: None,
            kind: kind.to_owned(),
            payload,
            created_at: String::new(),
            actor: None,
        }
    }

    #[test]
    fn only_the_integrator_role_may_act_as_the_integrator() {
        for role in ActorRole::ALL {
            let acting = Integrator::acting_as(ActorContext::instance(role, 1));
            if role == ActorRole::Integrator {
                assert_eq!(acting.unwrap().actor().actor_id(), "integrator:1");
            } else {
                let error = acting.unwrap_err();
                assert_eq!(error.role, role);
                assert_eq!(error.capability, Capability::Land, "{role:?}");
            }
        }
        assert_eq!(Integrator::of_process(9).actor().actor_id(), "integrator:9");
        assert!(Integrator::of_process(9).push_grant().is_ok());
    }

    #[test]
    fn the_integrator_lands_at_the_request_of_the_user_the_inbox_and_the_supervisor_only() {
        let run = integrating();
        let approved = [landing_event(event_kind::INTEGRATION_APPROVED, json!({}))];
        for role in ActorRole::ALL {
            let requester = ActorContext::instance(role, 1);
            let refusal = landing_refusal(&requester, &run, true, &approved);
            match role {
                ActorRole::User | ActorRole::Inbox | ActorRole::Supervisor => {
                    assert_eq!(refusal, None, "{role:?}");
                }
                _ => {
                    let refusal = refusal.unwrap_or_else(|| panic!("{role:?} may request"));
                    assert!(refusal.contains("may not landing.request"), "{refusal}");
                }
            }
        }
        // A worker is refused even for its own run.
        let worker = ActorContext::worker(run.id(), run.task_id());
        assert!(landing_refusal(&worker, &run, true, &approved).is_some());
    }

    #[test]
    fn the_integrator_lands_only_a_leased_approved_or_passed_run() {
        let supervisor = ActorContext::instance(ActorRole::Supervisor, 1);
        let landing = integrating();
        let pass = landing_event(event_kind::REVIEW_FINISHED, json!({"verdict": "pass"}));
        let concern = landing_event(event_kind::REVIEW_FINISHED, json!({"verdict": "concern"}));
        let approved = landing_event(event_kind::INTEGRATION_APPROVED, json!({}));
        assert_eq!(
            landing_refusal(&supervisor, &landing, true, std::slice::from_ref(&pass)),
            None
        );
        assert_eq!(
            landing_refusal(&supervisor, &landing, true, &[concern.clone(), approved]),
            None
        );
        // Neither approved nor passed, or passed and then not.
        for events in [vec![], vec![concern.clone()], vec![pass.clone(), concern]] {
            let refusal = landing_refusal(&supervisor, &landing, true, &events).unwrap();
            assert!(refusal.contains("neither approved"), "{refusal}");
        }
        // The slot is not the request's, or the run does not hold it.
        let refusal =
            landing_refusal(&supervisor, &landing, false, std::slice::from_ref(&pass)).unwrap();
        assert!(refusal.contains("integration slot"), "{refusal}");
        let awaiting = run(Path::new("/tmp/run"));
        assert!(landing_refusal(&supervisor, &awaiting, true, &[pass]).is_some());
    }
}
