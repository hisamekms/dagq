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
use crate::domain::run_phase;
use anyhow::{Context, Result, bail, ensure};
use serde_json::{Value, json};
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::mpsc,
    thread,
    time::{Duration, Instant},
};
use tracing::{info, warn};

use super::{
    AskStore, Clock, CloneFacts, EventStore, FollowUpRegistration, IdGenerator, Landing,
    MainRemote, ProcessControl, Queue, QueueRecords, Repository, RunFiles, RunLog, RunTransitions,
    Verifier, path_text, reason_of_error, tail,
};
use crate::domain::{
    ActorContext, ActorRole, AuthorizationError, Authorizer, Capability, Resource, StaticPolicy,
};
use crate::domain::{
    CommitSha, EvidenceCheck, IntegrationOutcome, NewTask, PushReport, PushResult, Reason,
    ReasonCode, Receipt, ReceiptResult, RegisteredFollowUp, RunEvent, RunHistory, RunId, RunStatus,
    Task, TaskId, TaskRun,
    disk::{DiskConfig, DiskRun, DiskStage, DiskState, gib},
    execution_class::missing_spike_result,
    heartbeat_stale,
    landing_branch::{
        DEFAULT_REMOTE, LandingBranch, RemoteSource, RepositoryConfig, missing_remote,
    },
    landing_queue::{Capacity, Entry, LandingQueue, Resources, recorded_entry},
    landing_verification,
    measure::{LoadSummary, LoadWindow},
    required_of,
    satisfied::{SnapshotFailure, SnapshotVerification},
    scope::{out_of_scope, scope_violation_reason},
    toolchain::Toolchains,
    validation::{self, CheckedOut, E2eRequirement, Fact, Judgement, ReceiptFacts, ReceiptFile},
    verify_failure,
};
use crate::migration_numbers;

pub(super) mod batch;
pub mod binding;
mod programs;
pub use programs::LandingPrograms;

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
    /// The fields of a Spike's result the receipt lacks (ADR-t1487-1
    /// decision 3), likewise.
    pub spike_result_missing: Vec<String>,
    /// The changed paths outside the task's `paths` (ADR-0029), when the
    /// run is otherwise sound: it waits for a session to take them out.
    pub scope_violation: Vec<String>,
    /// Whether `e2e` was required and why, when validation got that far
    /// (ADR-t963-1 decision 2).
    pub e2e_requirement: Option<E2eRequirement>,
}

/// A receipt validation accepted: the receipt, the commit it names and
/// whether `e2e` was required of it (ADR-t963-1 decision 2). An
/// `already_satisfied` claim (ADR-t1818-1) names the base as its commit
/// and lands nothing.
#[derive(Debug)]
pub struct Accepted {
    pub receipt: Receipt,
    pub commit: CommitSha,
    pub e2e_requirement: E2eRequirement,
    pub already_satisfied: bool,
}

/// Cross-check the agent's receipt against Git: the receipt names the
/// clean head of the run branch, new work on top of the base commit, within
/// the task's paths and with the task's required evidence but `e2e`, which
/// the runtime runs itself after the review (ADR-t1233-2); whether the run
/// needs it is read from the task and from the diff against `e2e_paths`,
/// the repository's `[e2e] paths` (ADR-t963-1 decision 2). The domain
/// judges ([`validation::judge`]) and says which fact it needs next; this
/// gathers it from the run files and Git. `Ok(Err(_))` is a verdict on the
/// run; `Err` a failure of the checks themselves.
pub fn check_receipt(
    repository: &dyn Repository,
    files: &dyn RunFiles,
    task: &Task,
    run: &TaskRun,
    e2e_paths: &[String],
) -> Result<std::result::Result<Accepted, Rejection>> {
    let receipt_path = run.receipt_path().context("missing receipt path")?;
    let required = required_of(task.required_evidence(), run.actual_provider());
    let mut facts = ReceiptFacts::new(
        run.id(),
        run.base_commit(),
        &required,
        task.paths(),
        receipt_path,
    )
    .with_e2e_paths(e2e_paths)
    .with_task_e2e(task.required_evidence().contains(&EvidenceCheck::E2e))
    .with_spike(task.execution_class().is_spike());
    let worktree =
        || -> Result<&Path> { Ok(Path::new(run.worktree_path().context("missing worktree")?)) };
    // A run in a container is read from its bundle and state file, never
    // with git in its clone (ADR-t2179-1 decisions 1 to 4), once per
    // validation; a landing worktree an earlier validation's commit left
    // goes first, as the landing is of the commit accepted now. Without a
    // guard nothing of the run is handled, and the run fails for a person
    // to see to the git configuration (ADR-t2179-1 decision 8).
    let mut clone: Option<CloneFacts> = None;
    if run.in_container() {
        match super::run_git::for_run(repository, run) {
            Err(error) if super::run_git::guard_does_not_hold(&error) => {
                return Ok(Err(unguarded_rejection(facts, run, &error)));
            }
            // A git that could not read the configuration is the checks'
            // failure, which the next validation tries again.
            Err(error) => return Err(error),
            Ok(_) => (),
        }
        repository.remove_landing_worktree(run)?;
    }
    loop {
        let fact = match validation::judge(&facts) {
            Judgement::Need(fact) => fact,
            Judgement::Accept(commit) => {
                let e2e_requirement = facts
                    .e2e_requirement()
                    .context("accepted without knowing whether e2e is required")?;
                let receipt = facts.into_receipt().context("accepted without a receipt")?;
                return Ok(Ok(Accepted {
                    receipt,
                    commit,
                    e2e_requirement,
                    already_satisfied: false,
                }));
            }
            // No diff: only the task can require the e2e.
            Judgement::AlreadySatisfied(commit) => {
                let e2e_requirement = E2eRequirement::of(facts.task_e2e, &[], &[]);
                let receipt = facts.into_receipt().context("accepted without a receipt")?;
                return Ok(Ok(Accepted {
                    receipt,
                    commit,
                    e2e_requirement,
                    already_satisfied: true,
                }));
            }
            Judgement::Reject(rejection) => {
                let e2e_requirement = facts.e2e_requirement();
                return Ok(Err(Rejection {
                    e2e_requirement,
                    reason: rejection.reason,
                    code: rejection.code,
                    commit: rejection.commit,
                    receipt: facts.into_receipt(),
                    evidence_missing: rejection.evidence_missing,
                    spike_result_missing: rejection.spike_result_missing,
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
                let branch = run.branch().context("missing branch")?;
                let current = if run.in_container() {
                    match clone_of(repository, run, &mut clone)? {
                        Ok(clone) => clone.state.branch.clone(),
                        Err(why) => return Ok(Err(back_to_session(facts, why))),
                    }
                } else {
                    // Git finds a worktree's admin dir by name alone: one
                    // pruned does not read, and one another worktree took
                    // reads that one's branch.
                    let worktree = worktree()?;
                    let expected = format!("refs/heads/{branch}");
                    match repository.current_branch(worktree) {
                        Ok(current)
                            if current.as_deref() != Some(expected.as_str())
                                && super::run_git::registration_broken(repository, worktree) =>
                        {
                            let error = anyhow::anyhow!(
                                "its git reads {}",
                                current.as_deref().unwrap_or("a detached HEAD")
                            );
                            return Ok(Err(unregistered_rejection(facts, run, &error)));
                        }
                        Ok(current) => current,
                        Err(error) if super::run_git::registration_broken(repository, worktree) => {
                            return Ok(Err(unregistered_rejection(facts, run, &error)));
                        }
                        // A git that failed otherwise is the checks'
                        // failure, which the next validation tries again.
                        Err(error) => return Err(error),
                    }
                };
                facts.checked_out = Some(CheckedOut {
                    branch: branch.to_owned(),
                    current,
                });
            }
            Fact::Head => {
                facts.head = Some(match &clone {
                    Some(clone) => clone.head.clone(),
                    None => repository.head(worktree()?)?,
                });
            }
            Fact::Descends => {
                let head = facts.head.as_ref().context("no head to check")?;
                facts.descends =
                    Some(repository.is_ancestor(run.base_commit().as_str(), head.as_str())?);
            }
            Fact::Status => {
                facts.status = Some(match &clone {
                    Some(clone) => clone.state.status.clone(),
                    None => repository.status(worktree()?)?,
                });
            }
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

/// The facts of `run`, a run in a container, read once into `clone`: the
/// head imported from its bundle and its state file. `Ok(Err(why))` is a
/// bundle or state file that is missing or cannot be read or imported, or
/// a state whose `HEAD` is not the bundle's head: the session's to put
/// right, never read with git in the clone instead.
/// Branch and status come from the state file; ancestry, diff and scope
/// use the imported branch in the control side's objects. Missing, broken
/// or symlinked files, a bundle with foreign refs or unavailable prerequisite
/// commits, and mismatched heads return `commit_mismatch` to the session.
/// Before validation the previous landing worktree is removed; landing
/// later uses only the accepted commit. A failed control-side guard reads
/// nothing and instead fails with `git_failed` for recovery/triage.
fn clone_of<'a>(
    repository: &dyn Repository,
    run: &TaskRun,
    clone: &'a mut Option<CloneFacts>,
) -> Result<std::result::Result<&'a CloneFacts, String>> {
    if clone.is_none() {
        let facts = match repository.clone_facts(run)? {
            Ok(facts) => facts,
            Err(why) => {
                return Ok(Err(format!(
                    "{why}; the container writes the run branch's bundle and the run's state at the end of each turn, so end the turn with the work committed on the run branch"
                )));
            }
        };
        if facts.state.head() != facts.head {
            return Ok(Err(format!(
                "the run's state names {} as HEAD, but the run branch's bundle holds {}; commit on the run branch and end the turn so that both are written again",
                facts.state.head(),
                facts.head
            )));
        }
        *clone = Some(facts);
    }
    Ok(Ok(clone.as_ref().expect("read above")))
}

/// The rejection that sends a container run whose bundle or state cannot
/// be used back to its session, like a receipt that does not name the
/// head (ADR-t2179-1 decision 4).
fn back_to_session(facts: ReceiptFacts<'_>, reason: String) -> Rejection {
    Rejection {
        e2e_requirement: facts.e2e_requirement(),
        reason,
        code: ReasonCode::CommitMismatch,
        commit: None,
        receipt: facts.into_receipt(),
        evidence_missing: Vec::new(),
        spike_result_missing: Vec::new(),
        scope_violation: Vec::new(),
    }
}

/// The rejection of a container run whose commits the control side's git
/// cannot be kept from running the hooks and drivers of (ADR-t2179-1
/// decision 8): the run fails with `git_failed`, since nothing its session
/// does would make the guard hold.
fn unguarded_rejection(facts: ReceiptFacts<'_>, run: &TaskRun, error: &anyhow::Error) -> Rejection {
    Rejection {
        e2e_requirement: facts.e2e_requirement(),
        reason: format!(
            "run {} was not validated: {error:#}; see to the git configuration; no session is resumed",
            run.id()
        ),
        code: ReasonCode::GitFailed,
        commit: None,
        receipt: facts.into_receipt(),
        evidence_missing: Vec::new(),
        spike_result_missing: Vec::new(),
        scope_violation: Vec::new(),
    }
}

/// The rejection of a run on the host whose worktree's git cannot be read
/// because its registration is broken
/// ([`registration_broken`](super::run_git::registration_broken)): the run
/// fails with `git_failed` and goes to its recovery job, never back to a
/// session in that worktree nor kept for the next pass, which would read
/// it no better.
fn unregistered_rejection(
    facts: ReceiptFacts<'_>,
    run: &TaskRun,
    error: &anyhow::Error,
) -> Rejection {
    Rejection {
        e2e_requirement: facts.e2e_requirement(),
        reason: format!(
            "run {} was not validated: its worktree is not registered on its branch, so its git cannot be read ({error:#}); no session is resumed",
            run.id()
        ),
        code: ReasonCode::GitFailed,
        commit: None,
        receipt: facts.into_receipt(),
        evidence_missing: Vec::new(),
        spike_result_missing: Vec::new(),
        scope_violation: Vec::new(),
    }
}

/// Run the commands of `plan` on `head`, the snapshot of main an
/// `already_satisfied` claim is verified on, with the run's `[run.env]`:
/// the task's verification commands with `[landing_verification]`'s in
/// place of those it names, as the landing runs them (ADR-t2740-1), the
/// replacing one with the known failures of the landing's own command and
/// `head` as its base. Nothing is rebased, committed or pushed, and the
/// run's own worktree keeps the base its receipt names. On the host they
/// run in the run's scratch worktree of `scratches`, the queue's
/// `satisfied/`, made again with `head` checked out detached
/// ([`fresh_satisfied_scratch`]), with the run's short temporary files'
/// directory outside the queue's `runs/` as their `TMPDIR`
/// ([`host_verification_tmpdir`]), as the landing's verification on the
/// host. Set to the container (ADR-t2113-3
/// decision 1), they run as the landing's verification does: each in a
/// container of its own on a clone of `head` made by `containers`, with a
/// token of the snapshot's own and without the sccache wrapper, never on
/// the host instead. The first command that fails ends it, its log kept
/// in the run's directory. `Err` is a verification that could not run at
/// all.
pub fn verify_snapshot(
    repository: &dyn Repository,
    verifier: &dyn Verifier,
    files: &dyn RunFiles,
    plan: &SnapshotPlan,
    run: &TaskRun,
    (head, scratches): (&CommitSha, &Path),
    containers: SnapshotContainers<'_>,
) -> Result<SnapshotVerification> {
    let commands = &plan.commands;
    let mut verification = SnapshotVerification {
        head: head.clone(),
        commands: commands.len(),
        failure: None,
    };
    if commands.is_empty() {
        return Ok(verification);
    }
    let run_dir = Path::new(run.run_dir().context("missing run directory")?);
    if verifier.runtimes()?.verification == crate::domain::container_runtime::Runtime::Container {
        // No program of the host runs the commands, and no cache of the
        // host's or the workers' is theirs (ADR-t2114-4 decision 4).
        let mut env = verifier.run_env(run_dir)?;
        crate::domain::run_container::without_sccache_wrapper(&mut env);
        // Declared before the container, so revoked after what the
        // container left is removed.
        let token = super::queue_broker::VerificationToken::issue(
            containers.tokens,
            run.id(),
            super::queue_broker::TokenFor::Snapshot,
        )?;
        let container = ContainerVerification::start(
            containers.containers,
            verifier,
            run,
            head,
            &token.file,
            &mut |ask, note| {
                if note != crate::domain::machine_memory::MemoryNote::Still {
                    tracing::info!(run_id = %run.id(), "run {}: its snapshot's verification and the dagq machine's memory: {note:?}", run.id());
                }
                let Some((kind, payload)) = super::run_container::memory_note_event(
                    ask,
                    note,
                    json!({"snapshot": head.as_str()}),
                ) else {
                    return;
                };
                if let Some(events) = containers.events
                    && let Err(error) = events.record_runtime_event(run.id(), kind, payload)
                {
                    warn!(run_id = %run.id(), "run {}: the wait of its snapshot's verification for the dagq machine's memory could not be recorded: {error:#}", run.id());
                }
            },
        )?;
        // Staged with the preparation's materials, as the landing's.
        let landing_env = plan.landing_env(
            files,
            &container.prepared.staged,
            &container.prepared.materials,
            head,
        )?;
        containers
            .containers
            .publish_materials(&container.prepared)
            .context("copy the verification's materials into the run directory")?;
        verification.failure = snapshot_commands(
            files,
            run_dir,
            head,
            commands,
            &mut verification.commands,
            |planned, log| {
                let since = container.containers.oom_since();
                let status = container.run_to_log(
                    verifier,
                    &planned.command,
                    &with_landing_env(&env, planned, &landing_env),
                    log,
                );
                let timed_out = status
                    .as_ref()
                    .err()
                    .and_then(|e| e.downcast_ref::<verify_failure::CommandTimedOut>());
                let read = status.as_ref().is_ok_and(|exit| {
                    crate::domain::run_container::oom::should_read(exit.success, exit.code)
                }) || timed_out.is_some();
                if read {
                    let output = files.read_to_string(log).unwrap_or_default();
                    let failure = match (&status, timed_out) {
                        (_, Some(timeout)) => timeout.failure(),
                        (Ok(exit), _) => verify_failure::classify(
                            &verifier.toolchains().unwrap_or_else(|_| Toolchains::none()),
                            &planned.command,
                            exit.code,
                            exit.signal,
                            &output,
                        ),
                        _ => unreachable!("only a command's failure is diagnosed"),
                    };
                    let observation = container.read_oom(since);
                    if let Some(events) = containers.events {
                        let annotation = super::run_container::record_verification_oom(
                            events,
                            run.id(),
                            &observation,
                            &planned.command,
                            &failure,
                        );
                        if let Some(text) = annotation {
                            let _ = files.write(log, format!("{output}\n{text}\n").as_bytes());
                        }
                    }
                }
                status
            },
        )?;
        return Ok(verification);
    }
    if let Some(message) = verifier.run_env_programs(Some(run_dir))?.missing_message() {
        bail!("{message}");
    }
    let mut env = verifier.run_env(run_dir)?;
    let _tmp = host_verification_tmpdir(files, run.id(), &mut env)?;
    let landing_env = plan.landing_env(files, run_dir, run_dir, head)?;
    // The scratch worktree goes whatever the commands did.
    let ran = fresh_satisfied_scratch(repository, scratches, run.id(), head).and_then(|scratch| {
        snapshot_commands(
            files,
            run_dir,
            head,
            commands,
            &mut verification.commands,
            |planned, log| {
                verifier.run_to_log_for(
                    run.id(),
                    &planned.command,
                    &scratch,
                    &with_landing_env(&env, planned, &landing_env),
                    log,
                )
            },
        )
    });
    let scratch = satisfied_scratch(scratches, run.id());
    if let Err(error) = repository.remove_scratch(&scratch) {
        warn!(run_id = %run.id(), "run {}: the scratch worktree {} could not be removed: {error:#}", run.id(), scratch.display());
    }
    verification.failure = ran?;
    Ok(verification)
}

/// What [`verify_snapshot`] runs for a task: its verification commands
/// planned as the landing plans them, and, when `[landing_verification]`
/// replaces some, the known failures its command is told, read from the
/// queue before the verification leaves the loop.
#[derive(Debug, Clone)]
pub struct SnapshotPlan {
    pub commands: Vec<landing_verification::Planned>,
    pub known_failures: Option<KnownFailures>,
}

impl SnapshotPlan {
    /// Plan `task`'s commands with the `dagq.toml` `verifier` reads.
    pub fn of<Q: RunLog + QueueRecords + ?Sized>(
        queue: &Q,
        verifier: &dyn Verifier,
        task: &Task,
    ) -> Result<Self> {
        let commands = landing_plan(verifier, task)?;
        let known_failures = if commands.iter().any(landing_verification::Planned::replaced) {
            Some(KnownFailures::read(queue, verifier, task)?)
        } else {
            None
        };
        Ok(Self {
            commands,
            known_failures,
        })
    }

    /// The variables of the replacing command on `head`, its known
    /// failures written as [`KnownFailures::env`] does; none when nothing
    /// is replaced.
    fn landing_env(
        &self,
        files: &dyn RunFiles,
        written: &Path,
        named: &Path,
        head: &CommitSha,
    ) -> Result<Vec<(String, String)>> {
        match &self.known_failures {
            Some(known) => known.env(files, written, named, head),
            None => Ok(Vec::new()),
        }
    }
}

/// `env` with `landing_env` on top when `planned` replaces a task's
/// command, as the landing hands its own command.
fn with_landing_env(
    env: &[(String, String)],
    planned: &landing_verification::Planned,
    landing_env: &[(String, String)],
) -> Vec<(String, String)> {
    let extra: &[(String, String)] = if planned.replaced() { landing_env } else { &[] };
    env.iter().chain(extra).cloned().collect()
}

/// What [`verify_snapshot`] runs a snapshot's commands with when
/// `dagq.toml` sets the verification to the container: the containers of
/// the verifications and the tokens they are handed.
#[derive(Clone, Copy)]
pub struct SnapshotContainers<'a> {
    pub containers: &'a dyn super::run_container::VerificationContainers,
    pub tokens: &'a dyn super::queue_broker::VerificationTokens,
    /// Where the wait of its container for the dagq machine's memory is
    /// recorded, as the landing's is; without, it is only logged.
    pub events: Option<&'a dyn EventStore>,
}

/// Run `commands` of the snapshot `head` in order through `run_to_log`,
/// each with its log in `run_dir`, until one fails: that one's failure,
/// with `ran` set to how many ran; none when every one passed.
fn snapshot_commands(
    files: &dyn RunFiles,
    run_dir: &Path,
    head: &CommitSha,
    commands: &[landing_verification::Planned],
    ran: &mut usize,
    mut run_to_log: impl FnMut(&landing_verification::Planned, &Path) -> Result<super::Exit>,
) -> Result<Option<SnapshotFailure>> {
    for (index, planned) in commands.iter().enumerate() {
        let log = run_dir.join(format!(
            "satisfied-{}-{}.log",
            &head.as_str()[..12],
            index + 1
        ));
        let exit = run_to_log(planned, &log)?;
        if !exit.success {
            // The commands after it do not run.
            *ran = index + 1;
            let output = files.read_to_string(&log).unwrap_or_default();
            return Ok(Some(SnapshotFailure {
                command: planned.command.clone(),
                exit_code: exit.code.unwrap_or(128),
                log_path: path_text(&log)?,
                output_tail: tail(&output, 2000).to_owned(),
            }));
        }
    }
    Ok(None)
}

/// Where `run`'s snapshot of main is checked out when its
/// `already_satisfied` claim is verified on the host: its directory of
/// `dir`, the queue's `satisfied/`, outside what the run's worker can
/// write (its run directory and worktree), so no `.git` or link the worker
/// left points the host's Git at a directory of the worker's choosing.
/// What an earlier verification left working in it is stopped before it
/// is made again.
pub fn satisfied_scratch(dir: &Path, run: &RunId) -> PathBuf {
    dir.join(run.as_str())
}

/// Make `run`'s [`satisfied_scratch`] of `dir` again from nothing with
/// `head` checked out detached, and give its path: what was there is not
/// trusted as a worktree but removed (a link without being followed), so
/// every use starts from `head` alone. What checks a snapshot of a claim
/// out on the host checks it out with this.
pub fn fresh_satisfied_scratch(
    repository: &dyn Repository,
    dir: &Path,
    run: &RunId,
    head: &CommitSha,
) -> Result<PathBuf> {
    let scratch = satisfied_scratch(dir, run);
    repository
        .fresh_scratch(&scratch, head.as_str())
        .with_context(|| format!("make the scratch worktree {}", scratch.display()))?;
    Ok(scratch)
}

/// What the receipt `integrate` reads before landing lacks: the code, the
/// reason and the payload of the park, with the missing checks in `checks`
/// and, for a Spike, the fields of its result in `spike_result`
/// (ADR-t1487-1 decision 3), so that the next resume asks for them; none
/// when it lacks nothing.
fn landing_gaps(
    receipt: &Receipt,
    required: &[EvidenceCheck],
    task: &Task,
) -> Option<(ReasonCode, String, Value)> {
    let missing = receipt.missing_evidence(required);
    let spike_missing = if task.execution_class().is_spike() {
        missing_spike_result(receipt.spike_result())
    } else {
        Vec::new()
    };
    if missing.is_empty() && spike_missing.is_empty() {
        return None;
    }
    let (code, reason) = validation::missing_reason(&missing, &spike_missing);
    let mut payload = json!({"checks": missing});
    if !spike_missing.is_empty() {
        payload["spike_result"] = json!(spike_missing);
    }
    Some((code, reason, payload))
}

/// Why the receipt `integrate` reads before landing fails the run instead
/// of landing it, `None` for a `succeeded` one: the session reported the
/// run `failed`, or `already_satisfied` (ADR-t1818-1), which has no commit
/// to land. Validation keeps a claim of the first session `validating` and
/// off this path; one a resumed session wrote here goes to the run's
/// recovery like a failed run.
fn receipt_ends_landing(receipt: &Receipt) -> Option<String> {
    match receipt.result() {
        ReceiptResult::Succeeded => None,
        ReceiptResult::Failed => Some(format!(
            "session reported the run as failed: {}",
            receipt.summary()
        )),
        ReceiptResult::AlreadySatisfied => Some(format!(
            "session reported already_satisfied, which has no commit to land: {}",
            receipt.summary()
        )),
    }
}

/// Which run `integrate` lands.
#[derive(Debug, Clone, Copy)]
pub enum IntegrateTarget {
    /// The task's run that awaits integration or comes back from a session.
    Task(TaskId),
    /// The first ready entry by effective priority and entry time. When
    /// none is ready, approve an awaiting run by validation order.
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
    /// Whether the container the verification may be set to can be used
    /// (goal 38).
    pub container: &'a dyn super::ContainerRuntime,
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
    /// What the verification commands' sccache server is looked at and
    /// their guard made with (ADR-t2086-1): given `[run.env]` naming
    /// sccache, they are refused the server's start, and compile through
    /// the guard made in the run's directory when the server listens just
    /// before, else without `RUSTC_WRAPPER`, recorded as
    /// `sccache_wrapper_removed` (`by` `integrate`). `None` looks at
    /// nothing and gives `[run.env]` as it is.
    pub sccache: Option<&'a dyn super::SccacheServer>,
    /// The tokens of the verifications (ADR-t2114-1 decision 2): one is
    /// issued for each verification of a run's commands and revoked at its
    /// end, its principal the run's verification, reaching through the
    /// queue broker only the registries `dagq.toml` names.
    pub verification_tokens: &'a dyn super::queue_broker::VerificationTokens,
    /// The containers of the verifications `dagq.toml` sets to the
    /// container (ADR-t2113-3 decision 1): each verification's commands run
    /// in a container of its own on a clone of the commit that lands, never
    /// on the host.
    pub verification_containers: &'a dyn super::run_container::VerificationContainers,
    /// The queue's turns of the host's heavy jobs (ADR-t2719-1): the
    /// verification waits for the automatic update's build and e2e gate and
    /// holds a landing check's turn while it runs; `None` takes none.
    pub heavy_turn: Option<super::heavy_turn::HeavyTurn>,
    /// The steps of the verification's commands, a session of the run's
    /// own (ADR-t1662-3 decision 1): each command a tool's call, sent to
    /// the run's sink every interval while they run and once more after
    /// the verification, before the run lands; what that could not send is
    /// sent again by the supervisor's sweeps. `None` sends none.
    pub steps: Option<super::step_report::LandingSteps>,
    /// Whether the supervisor that asked for the landing drains for a
    /// handoff now (ADR-t3344-1): when it does, a verification that would be done
    /// again (a host's failure or only flaky tests) is not, and the
    /// landing is handed over to wait for the slot again. `None` (a
    /// person's `integrate`) never hands over.
    pub handing_off: Option<HandoffDrain<'a>>,
    /// What runs the review programs the diff that lands requires, after
    /// the rebase and before the verification commands (ADR-t3749-1);
    /// `None` runs none.
    pub programs: Option<LandingPrograms<'a>>,
    /// The build identifier of the binary that lands (the supervisor's
    /// `binary_version`, or this binary's at a person's `integrate`),
    /// recorded as the `build` of an `integration_error`: a supervisor of
    /// another build takes the run up once (ADR-t3524-1).
    pub build: &'a str,
}

/// What a landing reads, before it verifies again, to know whether the
/// supervisor that asked for it drains for a handoff (ADR-t3344-1): its
/// handoff's flag, set on each pass, and its stop, read as it is set, since
/// a stop wins over a handoff (decision 3) without waiting for the pass.
#[derive(Clone, Copy)]
pub struct HandoffDrain<'a> {
    pub handing_off: &'a std::sync::atomic::AtomicBool,
    pub stop: &'a std::sync::atomic::AtomicBool,
}

impl HandoffDrain<'_> {
    /// Whether the landing is handed over instead of verified again: the
    /// handoff's flag set and no stop asked.
    fn hands_over(self) -> bool {
        use std::sync::atomic::Ordering::SeqCst;
        self.handing_off.load(SeqCst) && !self.stop.load(SeqCst)
    }
}

/// The free disk space integrate checks before it retries a verification
/// command that failed on a full disk (task 639), against what a
/// landing's verification needs as the supervisor judges it
/// ([`crate::domain::disk::DiskState::needs`]): the `[disk]` of
/// `dagq.toml`, a reading of the free bytes of the queue's directory now
/// and the size of its volume, `None` when they cannot be read, and the
/// runs moving besides the landing, read at the retry (`None` counts the
/// queue's runs in [`MOVING_STATUSES`]).
#[derive(Clone, Copy)]
pub struct RetryDisk<'a> {
    pub config: DiskConfig,
    pub free: &'a dyn Fn() -> Option<u64>,
    pub total: Option<u64>,
    pub moving: Option<&'a dyn Fn() -> usize>,
    /// Supervisor retries consume its immutable latest pass result.
    pub decision: Option<&'a dyn Fn() -> Option<crate::domain::disk::DiskDecision>>,
}

/// What a person's `integrate` checks the free disk space with (task
/// 638): the `[disk]` of `dagq.toml`, the free bytes of the queue's
/// directory and the size of its volume, `None` when they could not be
/// read; the runs moving are the queue's runs in [`MOVING_STATUSES`].
#[derive(Debug, Clone, Copy)]
pub struct DiskRoom {
    pub config: DiskConfig,
    pub free: Option<u64>,
    pub total: Option<u64>,
}

/// A request to the [`Integrator`] to land a run that holds the
/// integration slot under `token` (ADR-t728-2): `requester` is who asks
/// (the supervisor, or the person or the inbox behind `integrate`),
/// `previous` the status the run returns to when the landing stops before
/// `main` moves, and `main` the head it lands on.
///
/// The approval behind it, `integration_approved` when there is one (a
/// person's `integrate`, or an `approve_landing` answered `land`), is
/// recorded as the requester; a supervisor that lands a run takes the slot itself, so the
/// `integration_started` of its landing is the supervisor's.
/// `Integrator::approve` records `integration_approved` as the requester
/// only after authorization and takes the slot before returning this request.
/// For a supervisor landing, the service's `begin_landing` takes the slot
/// and issues the integrator token in that same transaction.
/// During `Integrator::land`, events are written as the integrator and
/// `requested_by` is the requester's actor id.
#[derive(Debug, Clone)]
pub struct IntegrationRequest {
    pub requester: ActorContext,
    pub run: TaskRun,
    pub previous: RunStatus,
    pub main: CommitSha,
    pub token: LeaseToken,
    /// Keeps the admitted turn through verification and publishing. The
    /// request can be cloned by the batch runner.
    pub(crate) held_turn: Option<std::sync::Arc<std::sync::Mutex<super::heavy_turn::Turn>>>,
}

/// Proof that a push of the landing branch is the [`Integrator`]'s
/// (ADR-t728-2): [`MainRemote::push_main`] takes one, and only the
/// Integrator, once the policy grants it [`Capability::Push`], makes one.
pub struct PushGrant(());

/// What a person's `integrate` sends its verification's steps through
/// ([`Integrator::land_reporting`]): the control side's registration of
/// its session, the sinks of the run's token, the queue's database, and
/// the clock its sends look at.
pub struct LandingTelemetry<'a> {
    pub tokens: &'a dyn super::queue_service::TelemetryTokens,
    pub sinks: std::sync::Arc<dyn super::TelemetrySinks>,
    pub db: &'a Path,
    pub clock: std::sync::Arc<dyn super::Clock>,
}

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
    /// Host verification checks run.env programs expanded with this run_dir
    /// before commands. Missing programs abort landing like an unreadable config,
    /// restore the prior state (no needs_session or spent resume), and put
    /// variables, values and PATH in last_error. No commands means no check.
    /// Container verification removes the host wrapper instead.
    /// This checks only `required_of` receipt evidence and runs no e2e
    /// (ADR-t963-1 decision 4). A person landing a run directly bypasses the
    /// runtime's e2e: before landing one requiring it, inspect the run's
    /// `e2e-*.log` and `run_e2e_finished` for the commit.
    pub fn land(
        &self,
        ctx: &mut Integration<'_>,
        request: &IntegrationRequest,
    ) -> Result<IntegrationOutcome> {
        let grant = self.push_grant()?;
        let previous = ctx.queue.act_as(self.actor.clone());
        let requested_by = ctx.queue.request_as(Some(&request.requester));
        let landed = self
            .check(&mut *ctx.queue, request, ctx.build)
            .and_then(|()| land_integrating(ctx, &grant, request, batch::Gate::Ordinary));
        ctx.queue.restore_request(requested_by);
        if let Some(previous) = previous {
            ctx.queue.act_as(previous);
        }
        landed
    }

    /// [`Self::land`] at a person's `integrate`, its verification the
    /// run's executor (ADR-t1662-3 decision 1): its commands' steps go to
    /// the sink of the run's token as a session of its own, which
    /// `telemetry` registers to the run first. A person's integrate has no
    /// sweep, so what the verification's end could not send is tried once
    /// more as it returns. Nothing of the steps changes the outcome.
    pub fn land_reporting(
        &self,
        ctx: &mut Integration<'_>,
        request: &IntegrationRequest,
        telemetry: LandingTelemetry<'_>,
    ) -> Result<IntegrationOutcome> {
        let unsent = std::sync::Arc::new(super::step_report::UnsentSteps::default());
        ctx.steps = Some(super::step_report::LandingSteps {
            shared: super::step_report::SharedVerification::new(
                super::step_report::VerificationSteps::of_run(
                    telemetry.tokens,
                    telemetry.sinks,
                    telemetry.db,
                    request.run.id(),
                    ctx.ids.uuid(),
                ),
                telemetry.clock.monotonic(),
            ),
            unsent: unsent.clone(),
            clock: telemetry.clock,
        });
        let outcome = self.land(ctx, request);
        ctx.steps = None;
        unsent.resend();
        outcome
    }

    fn push_grant(&self) -> Result<PushGrant> {
        StaticPolicy.authorize(&self.actor, Capability::Land, &Resource::Queue)?;
        StaticPolicy.authorize(&self.actor, Capability::Push, &Resource::Queue)?;
        Ok(PushGrant(()))
    }

    /// Refuse `request` unless [`landing_refusal`] finds nothing wrong,
    /// giving its slot back (when the request still holds it).
    fn check(
        &self,
        queue: &mut dyn Queue,
        request: &IntegrationRequest,
        build: &str,
    ) -> Result<()> {
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
        if holds {
            match queue.abort_integration(
                run.id(),
                &request.token,
                request.previous.as_str(),
                &message,
                &Reason::new(ReasonCode::Other).with("build", build),
            ) {
                Ok(run) => record_landing_left(
                    &*queue,
                    &request.requester,
                    &run,
                    EventKind::IntegrationError,
                ),
                Err(record) => {
                    warn!(op = "integrate", run_id = %run.id(), error = %format_args!("{record:#}"), "run {}: could not give the slot back: {record:#}", run.id());
                }
            }
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
/// A converged `revise_limit_judged` after the latest review also counts as
/// landable (`RunHistory::landable`). When refused, `Integrator::land`
/// returns any held slot through `abort_integration`, recording
/// `integration_error`, and returns an error without landing. A pass does
/// not bypass receipt, rebase, verification or scope checks.
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
    if !RunHistory::from_events(events).landable() {
        return Some("the run was neither approved to land nor passed its latest review".into());
    }
    None
}

/// Runs from which both landing paths reconstruct the common queue.
pub(super) fn landing_candidates(
    queue: &(impl RunLog + ?Sized),
) -> Result<Vec<(TaskRun, Vec<RunEvent>)>> {
    let mut runs = queue.runs_with_status(RunStatus::AwaitingIntegration)?;
    runs.extend(queue.runs_with_status(RunStatus::NeedsSession)?);
    let mut candidates = Vec::new();
    for run in runs {
        let events = queue.run_events(run.id())?;
        if RunHistory::from_events(&events).can_queue_landing(run.status()) {
            candidates.push((run, events));
        }
    }
    Ok(candidates)
}

/// Select by the domain order. After approval, record the observed
/// entries and also consider the explicitly approved target if it still
/// needs a session, as a named invocation may attempt that landing.
fn first_direct_landing(
    queue: &mut dyn Queue,
    capacity: Capacity,
    request: IntegrateTarget,
    approved_target: Option<&TaskRun>,
) -> Result<Option<TaskRun>> {
    let graph = super::dependency_graph(queue.graph_input()?, None);
    let priorities: HashMap<_, _> = graph
        .tasks
        .iter()
        .map(|task| (task.id, task.effective_priority))
        .collect();
    let mut candidates = landing_candidates(queue)?;
    if let Some(run) = approved_target
        && !candidates
            .iter()
            .any(|(candidate, _)| candidate.id() == run.id())
    {
        candidates.push((run.clone(), queue.run_events(run.id())?));
    }
    let mut entries = Vec::new();
    let mut runnable = HashMap::new();
    for (run, events) in candidates {
        let priority = match priorities.get(&run.task_id()) {
            Some(&priority) => priority,
            None => queue.show(run.task_id())?.task.priority(),
        };
        let Some(entry) = Entry::from_events(
            run.id().clone(),
            run.task_id(),
            &events,
            priority,
            run.result_commit().map(|commit| commit.as_str()),
        ) else {
            continue;
        };
        let history = RunHistory::from_events(&events);
        let approved = approved_target.is_some_and(|target| target.id() == run.id());
        // Naming this task approves its landing even with answers still
        // pending. Those asks close only after the landing succeeds.
        let named_approval = approved && matches!(request, IntegrateTarget::Task(_));
        let resources = Resources {
            unanswered: !named_approval
                && crate::domain::landing_queue::waiting_for_answer(
                    &queue.unclosed_run_asks(run.id())?,
                    &history,
                ),
            // Approval already refused a live lease on the named run.
            // The start transaction still requires the exact token for
            // any lease left there, including a stale one.
            leased: !approved && queue.run_lease(run.id())?.is_some(),
            preparing: !history.landing_ready(),
            fenced: queue.turn_fence(run.task_id())?.is_some(),
            ..Default::default()
        };
        let waiting_in_queue = history.can_queue_landing(run.status());
        let entered = recorded_entry(&events, &entry);
        runnable.insert(
            run.id().clone(),
            (run, resources, waiting_in_queue, entered),
        );
        entries.push(entry);
    }
    let order = LandingQueue::new(entries);
    // A person's integrate does not use the supervisor's worker slots or
    // run e2e, but its observations keep the run's actual e2e requirement.
    let selected = order
        .select(capacity, |entry| runnable[&entry.run_id].1)
        .map(|entry| &entry.run_id);
    if let Some(target) = approved_target {
        for entry in &order.entries {
            let (_, _, waiting_in_queue, entered) = &runnable[&entry.run_id];
            // A named attempt on a run that still needs a session joins
            // the queue only if it can start now. Otherwise the approval
            // remains for its worker resume.
            if !entered && (*waiting_in_queue || selected == Some(&entry.run_id)) {
                queue.record_runtime_event(
                    &entry.run_id,
                    EventKind::LandingQueueEntered,
                    entry.entered_payload(),
                )?;
            }
        }
        let entry = order
            .entries
            .iter()
            .find(|entry| &entry.run_id == target.id())
            .with_context(|| {
                format!(
                    "run {} is not ready to land; its approval is recorded",
                    target.id()
                )
            })?;
        if let Some(wait) =
            LandingQueue::explain(entry, capacity, runnable[target.id()].1, selected)
        {
            if wait == crate::domain::landing_queue::Wait::Integrating
                && let Some(active) = queue.runs_with_status(RunStatus::Integrating)?.first()
            {
                bail!(
                    "run {} is integrating; one run lands at a time (see doctor if it is stuck)",
                    active.id()
                );
            }
            bail!(
                "run {} {}; its approval is recorded",
                target.id(),
                wait.describe()
            );
        }
    }
    if let (Some(id), Some(_)) = (selected, approved_target)
        && let Some(entry) = order.entries.iter().find(|entry| &entry.run_id == id)
    {
        ensure!(
            recorded_entry(&queue.run_events(id)?, entry),
            "run {id} left the landing queue while its order was being checked"
        );
    }
    Ok(selected
        .and_then(|id| runnable.remove(id))
        .map(|(run, _, _, _)| run))
}

/// The first half of `integrate` ([`Integrator::approve`]). The requester
/// is refused before anything is recorded when the policy does not grant
/// it [`Capability::IntegrationRequest`] on the run. `repo`'s Git common
/// directory (`common_dir`) must be the one the queue is bound to.
/// `--next` first takes a ready entry from the common landing queue,
/// including an approved run whose resume has resolved. If none is ready,
/// it can approve a run awaiting integration by validation order.
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
        IntegrateTarget::Next => {
            match first_direct_landing(queue, Capacity::default(), target, None)? {
                Some(run) => run,
                None => match queue.next_awaiting_integration()? {
                    Some(run) => run,
                    None => return Ok(None),
                },
            }
        }
    };
    StaticPolicy.authorize(
        requester,
        Capability::IntegrationRequest,
        &Resource::Run {
            id: run.id().clone(),
            task: Some(run.task_id()),
        },
    )?;
    if let Some(room) = ctx.disk {
        let now = crate::application::unix_millis(ctx.clock.system_time());
        check_disk_room(&*queue, ctx.files, &room, &run, now)?;
    }
    // The call is the approval to land (ADR-0016 decision 5): a run it
    // parks as `needs_session` is landed by the supervisor once a resumed
    // session resolved it (ADR-0019 decision 1). Its prerequisites are
    // checked again in the approval's own write ([`approvable`]).
    // The approval is bound to the run's diff as it is now (ADR-t4840-2
    // decision 8): what lands is compared with it.
    let mut approval =
        json!({"status": run.status().as_str(), "pid": ctx.pid, "push": ctx.remote.is_some()});
    // A run in a container is read only by the git kept from running its
    // commits' hooks and drivers: one that cannot be made stops here,
    // before anything is read or approved (ADR-t2179-1 decision 8).
    let guarded = super::run_git::for_run(ctx.repository, &run)?;
    let repository: &dyn Repository = &*guarded;
    binding::put(
        &mut approval,
        &binding::of_run(repository, &run, &queue.run_events(run.id())?)?,
    );
    let (processes, clock) = (ctx.processes, ctx.clock);
    queue.approve_integration(run.id(), approval, &|lease| {
        !heartbeat_stale(processes.alive(lease.pid), clock.now() - lease.heartbeat_at)
    })?;
    // Direct approval joins the same order; naming a task cannot jump
    // over an earlier independently runnable authorization.
    let capacity = Capacity {
        integrating: !queue.runs_with_status(RunStatus::Integrating)?.is_empty(),
        ..Default::default()
    };
    first_direct_landing(queue, capacity, target, Some(&run))?;
    // Leave the run queued while the update owns the common turn. Read
    // the order again after waiting, while holding the turn for the start.
    let turn = match ctx.heavy_turn.as_ref() {
        Some(turn) => match turn.try_check(ctx.files, ctx.processes)? {
            Ok(guard) => Some(guard),
            Err(_) => {
                let guard = turn.check(
                    ctx.files,
                    ctx.processes,
                    super::heavy_turn::CHECK_POLL,
                    |what| {
                        info!(run_id = %run.id(), "the landing waits for {}", what.describe());
                    },
                )?;
                let capacity = Capacity {
                    integrating: !queue.runs_with_status(RunStatus::Integrating)?.is_empty(),
                    ..capacity
                };
                first_direct_landing(queue, capacity, target, Some(&run))?;
                Some(guard)
            }
        },
        None => None,
    };
    let previous = run.status();
    let token = ctx.ids.lease_token();
    let main = ctx.repository.main_head()?;
    let run = queue.begin_integration(run.id(), &token, &main)?;
    // It lands from here, holding the integration slot, until its push or
    // until the landing gives the slot back ([`record_landing_left`]).
    record_phase(&*queue, run.id(), EventKind::IntegrationStarted, |_| {
        Some(run_phase::Phase::Landing)
    });
    Ok(Some(IntegrationRequest {
        requester: requester.clone(),
        run,
        previous,
        main,
        token,
        held_turn: turn.map(|guard| std::sync::Arc::new(std::sync::Mutex::new(guard))),
    }))
}

/// Whether the approval to land run `id` of `payload` is recorded now,
/// from the run, lease and events read in the approval's own write
/// ([`RunTransitions::approve_integration`]): `false` for a run approved
/// already in the landing queue, bound to the same diff
/// ([`approved_as_bound`]). A fresh direct request can approve a run again
/// after a previous landing withdrew it from the queue. Refused is a
/// run whose status is not the one `payload` names (it changed since the
/// caller read it), one that awaits neither
/// integration nor a session, and one a lease `held` says is held (the
/// supervisor's review, ADR-0027, or its resume): a call that cannot land
/// it does not approve it.
pub fn approvable(
    run: &TaskRun,
    lease: Option<&crate::domain::RunLease>,
    events: &[crate::domain::RunEvent],
    payload: &Value,
    held: &dyn Fn(&crate::domain::RunLease) -> bool,
) -> Result<bool> {
    let leased = lease.is_some_and(held);
    let approved = RunHistory::from_events(events).landing_judgment().is_some()
        && approved_as_bound(events, payload);
    approval_due(
        run.id(),
        run.status(),
        payload["status"].as_str(),
        leased,
        approved,
    )
}

/// Whether run's `events` hold an approval to land that stands
/// ([`RunHistory::approved`]) and that the landing compares with, bound to
/// the diff `payload` is bound to (ADR-t4840-2 decision 8): the run's last
/// judgment ([`RunHistory::last_judgment`]) is an approval bound to the
/// same set. A person's `integrate` of a diff other than the one the last
/// judgment was bound to approves the diff as it is now, even when an
/// earlier approval was bound to it. A payload bound to nothing takes any
/// approval that stands.
fn approved_as_bound(events: &[crate::domain::RunEvent], payload: &Value) -> bool {
    use crate::domain::landing_binding::binding_of;
    let history = RunHistory::from_events(events);
    if !history.approved() {
        return false;
    }
    let Some(binding) = binding_of(payload) else {
        return true;
    };
    history.last_judgment().is_some_and(|judgment| {
        judgment.event.kind == EventKind::IntegrationApproved
            && judgment
                .binding
                .is_some_and(|bound| bound.approved_blobs == binding.approved_blobs)
    })
}

/// [`approvable`]'s decision on what it read of run `id`: its `status`,
/// the status the caller read it in (`read_in`), whether a live lease
/// holds it (`held`) and whether it was `approved` already.
fn approval_due(
    id: &RunId,
    status: RunStatus,
    read_in: Option<&str>,
    held: bool,
    approved: bool,
) -> Result<bool> {
    ensure!(
        matches!(
            status,
            RunStatus::AwaitingIntegration | RunStatus::NeedsSession
        ) && read_in == Some(status.as_str()),
        "run {id} is {}: only a run awaiting integration or a session, as it was read, is approved to land",
        status.as_str()
    );
    if held {
        bail!(
            "run {id} is held by the supervisor (its review or resume is in progress); see show for its review_finished / resume_finished events"
        );
    }
    Ok(!approved)
}

/// Measure the landing candidate at its boundary (the supervisor uses its
/// off-loop cache). Missing worktree or any unreadable part is unknown.
fn landing_current(files: &dyn RunFiles, run: &TaskRun) -> Option<u64> {
    if run.in_container() {
        return None;
    }
    let mut bytes = files.tree_size(Path::new(run.worktree_path()?)).ok()??;
    for path in [
        Path::new(run.run_dir()?).join(super::RUN_TMP_DIR),
        super::host_verification_tmp_path(run.id()),
    ] {
        bytes = bytes.saturating_add(files.tree_size(&path).ok()?.unwrap_or(0));
    }
    Some(bytes)
}

/// Refuse to land `run` while the free disk space is below what a
/// landing's verification needs (task 638, ADR-t4473-1): the same need the
/// supervisor holds its landings at, with the queue's running runs as the
/// runs moving. A person is at hand, so the call fails instead of
/// waiting; nothing is recorded. Unread free space checks nothing, as in
/// the supervisor.
fn check_disk_room(
    queue: &dyn Queue,
    files: &dyn RunFiles,
    room: &DiskRoom,
    run: &TaskRun,
    now_millis: i64,
) -> Result<()> {
    if let Some(short) = disk_short(
        queue,
        &room.config,
        room.free,
        room.total,
        None,
        now_millis,
        (Some(run.id()), landing_current(files, run)),
    )? {
        bail!(disk_refusal(run.id(), short));
    }
    Ok(())
}

/// Why a person's `integrate` refuses to land `run` while the disk is
/// `short`: what is free, what is needed and what it is made of, and
/// where to free room, in the words of the inbox's disk ask.
fn disk_refusal(run: &RunId, short: DiskShort) -> String {
    let expected = short
        .expected
        .map_or_else(|| "none yet".into(), |bytes| gib(bytes as f64));
    let relaxed = short
        .relaxed
        .map_or_else(String::new, |relaxed| format!("; {}", relaxed.message()));
    format!(
        "not enough free disk space to land run {run}: {} free in the queue's directory, below the {} a landing's verification needs (the reserve of {}, [disk] reserve of dagq.toml as a share of the volume and at least {}, plus unused INTEGRATOR reservation from {expected}{relaxed}); the run was not approved and is unchanged. Free disk space (dagq doctor lists the runs and their worktrees; the worktrees of ended runs nobody looks at any more, or other files on that disk) and run integrate again",
        gib(short.free as f64),
        gib(short.need as f64),
        gib(short.reserve as f64),
        gib(crate::domain::disk::MIN_RESERVE_BYTES as f64),
    ) + &format!("; {}", short.needs.message())
}

/// The free disk space below what a landing's verification needs: what
/// is free, what is needed, and the reserve and the expectation it is
/// made of.
#[derive(Debug, Clone, Copy)]
struct DiskShort {
    free: u64,
    need: u64,
    reserve: u64,
    expected: Option<u64>,
    /// The relaxation of the needs it was reckoned under (ADR-t4727-1).
    relaxed: Option<crate::domain::disk::DiskRelaxed>,
    needs: crate::domain::disk::DiskNeeds,
}

impl DiskShort {
    fn to_json(self) -> Value {
        json!({
            "free_bytes": self.free,
            "needed_bytes": self.need,
            "reserve_bytes": self.reserve,
            "expected_bytes": self.expected,
            "relaxed": self.relaxed,
        })
    }
}

/// Whether `free` is short of what a landing's verification needs, with
/// `total` the size of the volume, `moving` the runs moving besides it
/// (`None` counts the queue's runs in [`MOVING_STATUSES`]) and the expectation from the
/// recent runs' samples ([`crate::application::recent_run_samples`]),
/// under the relaxation of the needs in effect at `now_millis`, read from
/// its newest record as the supervisor reads it (ADR-t4727-1; its end is
/// the supervisor's to record): `None` with room or no reading.
fn disk_short<L: super::RunLog + ?Sized>(
    queue: &L,
    config: &DiskConfig,
    free: Option<u64>,
    total: Option<u64>,
    moving: Option<Vec<DiskRun>>,
    now_millis: i64,
    candidate: (Option<&RunId>, Option<u64>),
) -> Result<Option<DiskShort>> {
    if free.is_none() {
        return Ok(None);
    }
    let (candidate_id, current) = candidate;
    let samples = crate::application::recent_run_samples(queue)?;
    let budgets = crate::domain::disk::stage_budgets(
        config,
        &samples,
        &match config.estimate {
            crate::domain::disk::DiskEstimate::Recent => {
                crate::application::recent_stage_samples(queue)?
            }
            crate::domain::disk::DiskEstimate::Fixed => Default::default(),
        },
        now_millis,
    );
    let kinds = crate::domain::disk::RELAX_KINDS.map(EventKind::as_str);
    let relaxed =
        crate::domain::disk::relax_state(queue.latest_queue_event(&kinds)?.as_ref(), now_millis)
            .in_effect();
    let mut activity = moving.as_ref().map_or(0, Vec::len);
    let runs = match moving {
        Some(runs) => runs,
        None => {
            let mut runs = Vec::new();
            for status in MOVING_STATUSES {
                for run in queue.runs_with_status(status)? {
                    if candidate_id == Some(run.id()) {
                        continue;
                    }
                    let events = queue.run_events(run.id())?;
                    let phase = crate::domain::run_phase::last_recorded(&events);
                    let phase = phase
                        .and_then(|p| crate::domain::run_phase::Phase::from_name(&p.phase))
                        .unwrap_or_else(|| {
                            crate::domain::run_phase::Phase::for_disk_status(status)
                        });
                    let usage = crate::domain::disk::disk_usage(phase, !run.in_container(), true);
                    activity += usize::from(usage.active);
                    let stage = usage.stage;
                    runs.push(DiskRun {
                        stage,
                        current: None,
                    });
                }
            }
            runs
        }
    };
    let mut short = landing_short(
        &DiskState {
            free,
            total: total.unwrap_or(0),
            expected: budgets.worker,
            integrator_expected: budgets.integrator,
            moving: activity,
            runs,
            landing_current: current,
            landing_waiting: false,
            relaxed,
        },
        config.reserve,
    );
    if let Some(short) = short.as_mut() {
        short.needs.estimate = config.estimate;
        short.needs.sample_count = budgets.sample_count;
    }
    Ok(short)
}

/// Statuses that can have active work. Recorded phases separate worker,
/// waiting and integrator reservations, and remove parked human/provider
/// waits from the activity count; absent records conservatively count work.
const MOVING_STATUSES: [RunStatus; 5] = [
    RunStatus::Claimed,
    RunStatus::Starting,
    RunStatus::Running,
    RunStatus::Validating,
    RunStatus::Integrating,
];

/// Whether the free bytes of `state` are short of what a landing's
/// verification needs with the reserve `share`
/// ([`DiskState::needs`]): `None` with room or no reading.
fn landing_short(state: &DiskState, share: f64) -> Option<DiskShort> {
    let id = RunId::new("landing-candidate").expect("valid static id");
    let result = state.decide(
        share,
        &[crate::domain::disk::LandingCandidate {
            run: id.clone(),
            current: state.landing_current,
            integrating: false,
            waiting: true,
        }],
        crate::domain::disk::DiskEstimate::Recent,
        0,
    );
    landing_short_decision(&result, &id)
}

fn landing_short_decision(
    result: &crate::domain::disk::DiskDecision,
    run: &RunId,
) -> Option<DiskShort> {
    let landing = result.landing(run)?;
    if landing.allowed {
        return None;
    }
    let mut needs = result.needs;
    needs.landing = landing.needed;
    Some(DiskShort {
        free: result.free?,
        need: landing.needed,
        reserve: needs.reserve,
        expected: needs.integrator_expected,
        relaxed: needs.relaxed,
        needs,
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
    gate: batch::Gate,
) -> Result<IntegrationOutcome> {
    let IntegrationRequest {
        run,
        previous,
        main,
        token,
        ..
    } = request;
    let previous = *previous;
    // The git that handles a container run's commits runs none of their
    // hooks and drivers (ADR-t2179-1 decision 8); one that cannot be kept
    // from it lands nothing.
    let guarded = super::run_git::for_run(ctx.repository, run);
    let repository: &dyn Repository = match &guarded {
        Ok(repository) => &**repository,
        Err(_) => ctx.repository,
    };
    let task = ctx.queue.show(run.task_id())?.task;
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
        if let Err(error) = &guarded {
            return Ok((onto, unguarded(run, error)));
        }
        // Ordinarily the start holds the turn through the landing
        // (ADR-t5283-4). If the caller did not supply a guard, acquire it
        // before any verification commands.
        let _turn = match &ctx.heavy_turn {
            Some(turns) if request.held_turn.is_none() && !task.verification_commands().is_empty() => {
                Some(
                    turns.check(ctx.files, ctx.processes, super::heavy_turn::CHECK_POLL, |what| {
                        info!(
                            op = "integrate",
                            "run {}'s landing waits for {} to end before its verification",
                            run.id(),
                            what.describe()
                        );
                    })?,
                )
            }
            _ => None,
        };
        // The steps of the commands are sent every interval while they
        // run.
        let ticking = ctx
            .steps
            .as_ref()
            .map(|steps| super::step_report::Ticking::start(steps.shared.clone(), steps.clock.clone()));
        let verdict = match gate {
            batch::Gate::Stop(verdict) => Ok(verdict),
            batch::Gate::Abort(error) => Err(error),
            gate => land(ctx, repository, &task, run, &onto, main, gate),
        };
        // What the verification ran is sent before the run lands, while
        // its token holds; a failure is only logged.
        drop(ticking);
        if let Some(steps) = ctx.steps.as_ref()
            && !steps.shared.finish(ctx.clock.monotonic(), &steps.unsent)
        {
            warn!(
                op = "integrate",
                "run {}: the steps of its verification could not all be sent; the sweeps send them again",
                run.id()
            );
        }
        Ok((onto, verdict?))
    });
    let queue = &mut *ctx.queue;
    let (onto, verdict) = match landed {
        Ok(landed) => landed,
        Err(error) => {
            // Nothing reached main: give the slot back and keep the run where it was.
            let message = format!("integration stopped before main moved: {error:#}");
            match queue.abort_integration(
                run.id(),
                token,
                previous.as_str(),
                &message,
                &reason_of_error(&error, ReasonCode::Other).with("build", ctx.build),
            ) {
                Ok(run) => record_landing_left(
                    &*queue,
                    &request.requester,
                    &run,
                    EventKind::IntegrationError,
                ),
                Err(record) => warn!(
                    op = "integrate",
                    error = %format_args!("{record:#}"),
                    "run {}: could not record the error: {record:#}",
                    run.id()
                ),
            }
            return Err(error.context(format!(
                "run {} returned to {}",
                run.id(),
                previous.as_str()
            )));
        }
    };
    finish_landing(ctx, repository, grant, request, &onto, verdict)
}

fn finish_landing(
    ctx: &mut Integration<'_>,
    repository: &dyn Repository,
    grant: &PushGrant,
    request: &IntegrationRequest,
    onto: &LandingBranch,
    verdict: Verdict,
) -> Result<IntegrationOutcome> {
    let IntegrationRequest {
        run,
        main,
        token,
        previous,
        ..
    } = request;
    let queue = &mut *ctx.queue;
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
            remove_landed_worktree(
                queue,
                repository,
                ctx.files,
                ctx.processes,
                ctx.common_dir,
                &run,
            );
            let push = push_main(
                queue,
                repository,
                grant,
                onto,
                ctx.remote,
                run.id(),
                &landing.commit,
            );
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
            record_landing_left(
                &*queue,
                &request.requester,
                &run,
                EventKind::IntegrationDeferred,
            );
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
            record_landing_left(
                &*queue,
                &request.requester,
                &run,
                EventKind::IntegrationHeld,
            );
            IntegrationOutcome::Held {
                run: Box::new(run),
                main: main.clone(),
                reason,
            }
        }
        Verdict::HandedOver { reason, detail } => {
            info!(
                op = "integrate",
                reason = %reason,
                "run {} is handed over: {reason}",
                run.id()
            );
            let run =
                queue.hand_over_integration(run.id(), token, previous.as_str(), &reason, detail)?;
            IntegrationOutcome::HandedOver {
                run: Box::new(run),
                main: main.clone(),
                reason,
            }
        }
        Verdict::Changed {
            reason,
            reapproval,
            differing,
        } => {
            warn!(
                op = "integrate",
                reason = %reason,
                "run {} did not land: {reason}",
                run.id()
            );
            // The slot goes back and the run awaits integration for its
            // re-approval, its session's work done (a run that was waiting
            // for a session was queued after it resolved); the comparison's
            // record spends the judgment
            // ([`crate::domain::landing_binding::unspent`]).
            let run = queue.abort_integration(
                run.id(),
                token,
                RunStatus::AwaitingIntegration.as_str(),
                &reason,
                &Reason::new(ReasonCode::LandingChanged)
                    .with("reapproval", reapproval.as_str())
                    .with("differing", json!(differing))
                    .with("build", ctx.build),
            )?;
            record_landing_left(
                &*queue,
                &request.requester,
                &run,
                EventKind::IntegrationError,
            );
            IntegrationOutcome::Changed {
                run: Box::new(run),
                main: main.clone(),
                reason,
                reapproval: reapproval.as_str().to_owned(),
                differing,
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
            record_landing_left(
                &*queue,
                &request.requester,
                &run,
                EventKind::IntegrationFailed,
            );
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
/// What is pushed and recorded is [`decide_push`]'s; a failure to record is
/// only reported: the landing stands either way.
fn push_main(
    queue: &dyn Queue,
    repository: &dyn Repository,
    grant: &PushGrant,
    onto: &LandingBranch,
    remote: Option<&dyn MainRemote>,
    run_id: &RunId,
    commit: &CommitSha,
) -> PushReport {
    let PushDecision {
        report,
        kind,
        payload,
        check_error,
    } = decide_push(
        remote,
        || repository.repository_config().unwrap_or_default(),
        grant,
        onto,
        commit,
    );
    if let Some(check_error) = check_error {
        warn!(op = "push", run_id = %run_id, error = %check_error, "could not check remote after failed push");
    }
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
    } else if let Err(error) = record_push_phase(queue, run_id, kind) {
        warn!(
            op = "push",
            run_id = %run_id,
            error = %format_args!("{error:#}"),
            "run {run_id}: could not record the phase after {kind}: {error:#}"
        );
    }
    report
}

/// Record the phase the push's outcome `kind` ends the landed run in
/// ([`run_phase::Phase::after_push`]), in the run's attempt.
fn record_push_phase<Q: EventStore + ?Sized>(
    queue: &Q,
    run_id: &RunId,
    kind: EventKind,
) -> Result<()> {
    let (phase, cause) = run_phase::Phase::after_push(kind);
    let attempt = run_phase::last_recorded(&queue.run_events(run_id)?)
        .map_or(run_phase::Attempt::FIRST, |recorded| recorded.attempt);
    queue.record_runtime_event_now(
        run_id,
        EventKind::RunPhaseChanged,
        run_phase::PhaseChange::new(phase, attempt, cause).payload(),
    )
}

/// Record the phase `run` is in once its landing gave the integration
/// slot back before main moved, by the status it left the run in
/// ([`run_phase::Phase::after_landing`]): `cause` is the event that gave
/// the slot back. This closes the landing's phase. A landing the
/// supervisor asked for records nothing here: the supervisor records the
/// run's phases from its slot, which took the slot and lets it go.
fn record_landing_left<Q: EventStore + ?Sized>(
    queue: &Q,
    requester: &ActorContext,
    run: &TaskRun,
    cause: EventKind,
) {
    if requester.role() == ActorRole::Supervisor {
        return;
    }
    // A person's landing: no supervisor's build takes it up again.
    record_phase(queue, run.id(), cause, |events| {
        run_phase::Phase::after_landing(run.status(), events, None)
    });
}

/// Record the phase `phase` decides from the run's events, which `cause`
/// moved it to, in the attempt last recorded, unless it repeats the last
/// record. A failure is only warned of: the landing goes on either way.
fn record_phase<Q: EventStore + ?Sized>(
    queue: &Q,
    run_id: &RunId,
    cause: EventKind,
    phase: impl FnOnce(&[crate::domain::RunEvent]) -> Option<run_phase::Phase>,
) {
    let recorded = queue.run_events(run_id).and_then(|events| {
        match phase(&events)
            .and_then(|phase| run_phase::next_change(&events, phase, cause.as_str()))
        {
            Some(change) => {
                queue.record_runtime_event_now(run_id, EventKind::RunPhaseChanged, change.payload())
            }
            None => Ok(()),
        }
    });
    if let Err(error) = recorded {
        warn!(
            op = "integrate",
            run_id = %run_id,
            error = %format_args!("{error:#}"),
            "run {run_id}: could not record its phase after {cause}: {error:#}"
        );
    }
}

/// What [`decide_push`] did: the report, the event recording it, and why
/// the remote could not be checked after a failed push.
#[derive(Debug)]
struct PushDecision {
    report: PushReport,
    kind: EventKind,
    payload: Value,
    check_error: Option<String>,
}

/// Push `onto` through `remote` as `[repository]` says and tell what became
/// of it. `push = false` and a missing default remote skip the push; a
/// configured remote that is missing fails it. `--no-push` (no `remote`)
/// skips it and records the remote `[repository]` names (`no_push_config`),
/// `origin` when it cannot be read, without looking at the remote. A failed
/// push whose landed `commit` the remote branch already contains is
/// `pushed`, `already_delivered`.
fn decide_push(
    remote: Option<&dyn MainRemote>,
    no_push_config: impl FnOnce() -> RepositoryConfig,
    grant: &PushGrant,
    onto: &LandingBranch,
    commit: &CommitSha,
) -> PushDecision {
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
    let mut already_delivered = false;
    let mut check_error = None;
    let report = match remote {
        None => {
            let config = no_push_config();
            report(
                PushResult::Skipped,
                config.remote(),
                None,
                Some("--no-push"),
            )
        }
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
                        Err(error) => {
                            match main_remote.contains_landed_commit(name, onto, commit) {
                                Ok(true) => {
                                    already_delivered = true;
                                    report(PushResult::Pushed, name, None, None)
                                }
                                Ok(false) => failed(name, &error),
                                Err(error_of_check) => {
                                    check_error = Some(format!("{error_of_check:#}"));
                                    failed(name, &error)
                                }
                            }
                        }
                    },
                    Err(error) => failed(name, &error),
                }
            }
        },
    };
    let (kind, payload) = match report.outcome {
        PushResult::Pushed => (
            EventKind::PushFinished,
            json!({"remote": report.remote, "branch": report.branch, "commit": commit, "already_delivered": already_delivered}),
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
    PushDecision {
        report,
        kind,
        payload,
        check_error,
    }
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
/// and so does the entry's `membership_proposal` (ADR-t1504-2 decision 11:
/// as written, null without one; a proposal for the planner, not a
/// judgement), and an entry already recorded is not looked at again, so
/// a second call for the same run adds nothing. All unregistered entries of
/// one receipt, including skipped events, are committed in one transaction.
/// A registration that fails is only reported: the landing stands either way.
/// Returns what this call registered.
pub fn register_follow_ups<Q: Queue + ?Sized>(
    queue: &mut Q,
    task: &Task,
    run_id: &RunId,
    follow_ups: Option<&Value>,
) -> Vec<RegisteredFollowUp> {
    let Some(entries) = follow_ups.and_then(Value::as_array) else {
        return Vec::new();
    };
    // The store reads the source task and goal again inside its registration
    // transaction. Draft membership and immutable origin facts come from that
    // snapshot, including a close that won the race with registration.
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
    let prepared = entries
        .iter()
        .enumerate()
        .map(|(index, entry)| receipt_follow_up_registration(task, run_id, index, entry))
        .collect();
    match queue.register_follow_ups(run_id, prepared, depth) {
        Ok(added) => {
            for item in &added {
                info!(op = "follow_up", run_id = %run_id, follow_up_task_id = %item.task_id,
                    "run {run_id}: follow_up {:?} registered as draft task {}", item.title, item.task_id);
            }
            added
        }
        Err(error) => {
            warn!(op = "follow_up", run_id = %run_id, error = %format_args!("{error:#}"),
                "run {run_id}: follow_ups not registered: {error:#}");
            Vec::new()
        }
    }
}

/// Entry `index` of the `follow_ups` of `task`'s run `run_id`'s receipt as
/// [`register_follow_ups`] hands it to the store: a draft of the task's goal
/// with the entry's trimmed title and its description, no acceptance,
/// verification commands or dependencies, and a context naming the run and
/// the task; or, when the title is not a non-blank string or the
/// description not a string, no draft and the `skipped` reason. The
/// category and the membership proposal are the entry's as written.
fn receipt_follow_up_registration(
    task: &Task,
    run_id: &RunId,
    index: usize,
    entry: &Value,
) -> FollowUpRegistration {
    let title = entry["title"].as_str().map(str::trim).unwrap_or_default();
    let description = entry["description"].as_str();
    let skipped = if title.is_empty() {
        Some("title is not a non-blank string")
    } else if description.is_none() {
        Some("description is not a string")
    } else {
        None
    };
    let draft = description
        .filter(|_| skipped.is_none())
        .map(|description| NewTask {
            title: title.to_owned(),
            description: description.to_owned(),
            acceptance: String::new(),
            verification_commands: Vec::new(),
            required_evidence: Vec::new(),
            paths: Vec::new(),
            priority: Default::default(),
            change: None,
            dependencies: Vec::new(),
            goal_dependencies: Vec::new(),
            goal_id: task.goal_id(),
            context: format!(
                "follow_up proposed by the receipt of run {run_id} of task {} ({})",
                task.id(),
                task.title()
            ),
            provider: None,
            worker_mode: None,
            wait_for_build: false,
            execution_class: Default::default(),
        });
    FollowUpRegistration {
        key: crate::domain::FollowUpKey::Receipt { index },
        entry: entry.clone(),
        category: crate::domain::follow_up_category(entry),
        membership_proposal: crate::domain::follow_up_membership_proposal(entry),
        draft,
        skipped: skipped.map(str::to_owned),
    }
}

/// Register the `follow_ups` of review `attempt` of `task`'s run `run_id`
/// (`verdict`, the whole review's and each agent job's) as `docs_drift`
/// follow_up drafts of the task's goal (ADR-t2352-1), the way
/// [`register_follow_ups`] registers a receipt's: one follow-up deeper than
/// the task, origin `follow_up` with the source run and goal, the planner
/// the runtime opens for the run's drafts. A document's path and section
/// make one draft for the run ([`crate::domain::new_review_follow_ups`]):
/// the same passage found by two jobs, by a later review of the run, or by
/// this verdict judged again after a restart adds nothing. The verdict's
/// decision and where it sends the run do not read them, and a
/// registration that fails is only reported. Returns what this call
/// registered.
pub fn register_review_follow_ups<Q: EventStore + super::DraftPlannerStore + ?Sized>(
    queue: &mut Q,
    task: &Task,
    run_id: &RunId,
    attempt: usize,
    verdict: &crate::domain::ReviewVerdict,
) -> Vec<RegisteredFollowUp> {
    register_found_follow_ups(
        queue,
        task,
        run_id,
        attempt,
        verdict.found_follow_ups(),
        review_follow_up_registration,
    )
}

/// The reasons review `attempt` of `task`'s run `run_id` sent the run back
/// with past its revise limit, left when the run landed with no code
/// finding among them (ADR-t2599-1): registered as follow_up drafts the way
/// [`register_review_follow_ups`] registers a review's follow_ups, each
/// keyed by its text, of the category its codes map to
/// ([`crate::domain::review_reason::follow_up_category`]).
pub fn register_converged_follow_ups<Q: EventStore + super::DraftPlannerStore + ?Sized>(
    queue: &mut Q,
    task: &Task,
    run_id: &RunId,
    attempt: usize,
    findings: &[crate::domain::review_reason::Finding],
) -> Vec<RegisteredFollowUp> {
    let found = findings
        .iter()
        .map(|finding| crate::domain::FoundReviewFollowUp {
            agent: None,
            follow_up: crate::domain::ReviewFollowUp {
                text: finding.text.clone(),
                path: String::new(),
                section: finding.text.clone(),
                codes: finding.codes.clone(),
            },
        })
        .collect();
    register_found_follow_ups(
        queue,
        task,
        run_id,
        attempt,
        found,
        converged_follow_up_registration,
    )
}

/// Register as drafts those of `found` the run has none of yet, each as
/// `registration` makes it.
fn register_found_follow_ups<Q: EventStore + super::DraftPlannerStore + ?Sized>(
    queue: &mut Q,
    task: &Task,
    run_id: &RunId,
    attempt: usize,
    found: Vec<crate::domain::FoundReviewFollowUp>,
    registration: fn(
        &Task,
        &RunId,
        usize,
        crate::domain::FoundReviewFollowUp,
    ) -> FollowUpRegistration,
) -> Vec<RegisteredFollowUp> {
    if found.is_empty() {
        return Vec::new();
    }
    let registered = match queue.run_events(run_id) {
        Ok(events) => {
            crate::domain::RunHistory::from_events(&events).registered_review_follow_ups()
        }
        Err(error) => {
            warn!(op = "follow_up", run_id = %run_id, error = %format_args!("{error:#}"),
                "run {run_id}: review {attempt}'s follow_ups not registered: {error:#}");
            return Vec::new();
        }
    };
    let new = crate::domain::new_review_follow_ups(found, &registered);
    if new.is_empty() {
        return Vec::new();
    }
    let depth = match queue.follow_up_depth(task.id()) {
        Ok(depth) => depth + 1,
        Err(error) => {
            warn!(op = "follow_up", run_id = %run_id, task_id = %task.id(),
                error = %format_args!("{error:#}"),
                "run {run_id}: the follow_up_depth of task {} could not be read: {error:#}",
                task.id());
            crate::domain::follow_up::FOLLOW_UP_ASK_DEPTH
        }
    };
    let prepared = new
        .into_iter()
        .map(|found| registration(task, run_id, attempt, found))
        .collect();
    match queue.register_follow_ups(run_id, prepared, depth) {
        Ok(added) => {
            for item in &added {
                info!(op = "follow_up", run_id = %run_id, follow_up_task_id = %item.task_id,
                    "run {run_id}: review {attempt}'s follow_up {:?} registered as draft task {}", item.title, item.task_id);
            }
            added
        }
        Err(error) => {
            warn!(op = "follow_up", run_id = %run_id, error = %format_args!("{error:#}"),
                "run {run_id}: review {attempt}'s follow_ups not registered: {error:#}");
            Vec::new()
        }
    }
}

/// The registration of one follow_up `found` by review `attempt` of
/// `task`'s run `run_id` (ADR-t2352-1): a `docs_drift` draft of the task's
/// goal, keyed by its document, whose title names the document and whose
/// description is the job's text and which job of which review found it.
pub fn review_follow_up_registration(
    task: &Task,
    run_id: &RunId,
    attempt: usize,
    found: crate::domain::FoundReviewFollowUp,
) -> FollowUpRegistration {
    let who = found.agent.as_deref().map_or_else(
        || "the whole review".to_owned(),
        |agent| format!("the agent {agent}"),
    );
    let follow_up = &found.follow_up;
    let draft = NewTask {
        title: follow_up.draft_title(),
        description: format!(
            "{}\n\nFound by {who} of review {attempt} of run {run_id} in a document its diff did not touch, so the review did not send the run back for it.",
            follow_up.text
        ),
        acceptance: String::new(),
        verification_commands: Vec::new(),
        required_evidence: Vec::new(),
        paths: Vec::new(),
        priority: Default::default(),
        change: None,
        dependencies: Vec::new(),
        goal_dependencies: Vec::new(),
        goal_id: task.goal_id(),
        context: format!(
            "follow_up found by review {attempt} of run {run_id} of task {} ({})",
            task.id(),
            task.title()
        ),
        provider: None,
        worker_mode: None,
        wait_for_build: false,
        execution_class: Default::default(),
    };
    FollowUpRegistration {
        key: crate::domain::FollowUpKey::Review {
            attempt,
            agent: found.agent.clone(),
            key: follow_up.key(),
        },
        entry: json!(follow_up),
        category: crate::domain::REVIEW_FOLLOW_UP_CATEGORY.to_owned(),
        membership_proposal: Value::Null,
        draft: Some(draft),
        skipped: None,
    }
}

/// The registration of one reason `found` that review `attempt` of
/// `task`'s run `run_id` left when the run landed past its revise limit
/// (ADR-t2599-1): a draft of the task's goal, of the category of the
/// reason's codes, keyed by its text, whose description is the text and
/// which review left it.
fn converged_follow_up_registration(
    task: &Task,
    run_id: &RunId,
    attempt: usize,
    found: crate::domain::FoundReviewFollowUp,
) -> FollowUpRegistration {
    let follow_up = &found.follow_up;
    let category = crate::domain::review_reason::follow_up_category(&follow_up.codes);
    let head: String = follow_up.text.chars().take(80).collect();
    let title = if head.len() < follow_up.text.len() {
        format!("{category}: {head}…")
    } else {
        format!("{category}: {head}")
    };
    let draft = NewTask {
        title,
        description: format!(
            "{}\n\nLeft by review {attempt} of run {run_id} (codes: {}), which sent the run back after its revises were used up with no code finding, so the run landed without it.",
            follow_up.text,
            follow_up.codes.join(", ")
        ),
        acceptance: String::new(),
        verification_commands: Vec::new(),
        required_evidence: Vec::new(),
        paths: Vec::new(),
        priority: Default::default(),
        change: None,
        dependencies: Vec::new(),
        goal_dependencies: Vec::new(),
        goal_id: task.goal_id(),
        context: format!(
            "follow_up left by review {attempt} of run {run_id} of task {} ({})",
            task.id(),
            task.title()
        ),
        provider: None,
        worker_mode: None,
        wait_for_build: false,
        execution_class: Default::default(),
    };
    FollowUpRegistration {
        key: crate::domain::FollowUpKey::Review {
            attempt,
            agent: None,
            key: follow_up.key(),
        },
        entry: json!(follow_up),
        category: category.to_owned(),
        membership_proposal: Value::Null,
        draft: Some(draft),
        skipped: None,
    }
}

enum Verdict {
    /// Landed with the receipt's `follow_ups`.
    Landed(Landing, Option<Value>),
    /// Re-validation did not pass; the worktree is left for a session.
    /// `detail` becomes the `integration_deferred` payload: the reason's
    /// code and what the case has of the conflicting files, the tail of
    /// Git's output and the head after the rebase, to which `integrate`
    /// adds `resumes_left`, how many counted resumes the supervisor still
    /// makes.
    Deferred { reason: String, detail: Value },
    /// The session's rewritten receipt reports `failed`; `receipt` is its
    /// JSON, kept with the `integration_failed` event.
    ReceiptFailed { reason: String, receipt: Value },
    /// A verification command failed on the host again after its retry, or
    /// on a full disk with no room to retry it (task 639): the run waits
    /// for a person, awaiting integration, without a resume.
    Held { reason: String, detail: Value },
    /// A verification that would be done again while the supervisor
    /// drains for a handoff (ADR-t3344-1): the run waits for the landing
    /// slot again, its lease kept, and the next process lands it anew.
    HandedOver { reason: String, detail: Value },
    /// The landing could not carry over the run's last
    /// judgment that lets it land (ADR-t5283-2): nothing lands
    /// and the run goes back to that judgment's path to be approved again.
    Changed {
        reason: String,
        reapproval: crate::domain::landing_binding::Reapproval,
        differing: Vec<String>,
    },
}

/// Rebase, re-validate and land one run. `Ok(Deferred)` and
/// `Ok(ReceiptFailed)` are verdicts on the run; `Err` is a failure of the
/// landing itself (Git, files) before `main` moved.
#[allow(clippy::too_many_arguments)]
fn land(
    ctx: &mut Integration<'_>,
    repository: &dyn Repository,
    task: &Task,
    run: &TaskRun,
    landing_branch: &LandingBranch,
    main: &CommitSha,
    gate: batch::Gate,
) -> Result<Verdict> {
    let queue = &mut *ctx.queue;
    let files = ctx.files;
    let defer = |code: Reason, reason: String, detail: Value| {
        Ok(Verdict::Deferred {
            reason,
            detail: code.on(detail),
        })
    };
    // A landing that moved main before whoever landed it stopped (task
    // 1118) is not landed a second time: its commit is recorded as landed.
    if let Some((commit, parent)) = repository.landed_run_commit(
        run.base_commit().as_str(),
        main.as_str(),
        run.id().as_str(),
    )? {
        return landed_before(queue, repository, files, task, run, main, commit, parent);
    }
    // A container run lands in the control-side worktree at its validated
    // commit or the runtime rewrite its re-approval saw.
    let landing_tree;
    let worktree = if run.in_container() {
        let events = queue.run_events(run.id())?;
        let head = CommitSha::parse(
            binding::landing_head(repository, run, &events)?,
            "landing head",
        )?;
        landing_tree = match repository.landing_worktree(run, &head)? {
            Ok(tree) => tree,
            Err(why) => return Ok(no_landing_worktree(run, &why)),
        };
        landing_tree.as_path()
    } else {
        Path::new(run.worktree_path().context("missing worktree")?)
    };
    ensure!(
        files.is_dir(worktree),
        "worktree {} is missing",
        worktree.display()
    );
    // A worktree whose queue directory moved is still found through its own
    // `.git` file, but the repository's record of it points at the old path
    // until repaired, and removing it after landing would fail (ADR-0017).
    if !run.in_container() {
        repository.repair_worktree(worktree)?;
    }
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
    // A landing worktree holds the accepted commit detached; the clone's
    // branch was the validation's to check.
    match repository.current_branch(worktree)? {
        _ if run.in_container() => (),
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
    if let Some(reason) = receipt_ends_landing(&receipt) {
        return Ok(Verdict::ReceiptFailed {
            reason,
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
    if let Some((code, reason, payload)) = landing_gaps(&receipt, &required, task) {
        return defer(code.into(), reason, payload);
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
    // A landing that stopped after its rebase left the rebased head: the
    // receipt still describes it (task 1118).
    if !receipt.names_commit(head.as_str())
        && !RunHistory::from_events(&queue.run_events(run.id())?)
            .landing_rewrote(receipt.commit(), head.as_str())
    {
        // A commit the receipt does not name, after the run's judgment
        // that lets it land, changes what it would land: the comparison is
        // recorded and spends the judgment before the session is asked for
        // a receipt of its head (ADR-t4840-2 decision 7).
        binding::spend_on_unnamed_commit(&*queue, repository, run, main, &head)?;
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
        // A run in a container rebases onto the ref main's bundle reaches
        // its clone as (ADR-t2179-1 decision 6).
        let target = super::run_git::rebase_target(run, main);
        let target = if run.in_container() {
            format!("{target}, which holds {main}")
        } else {
            target
        };
        return defer(
            ReasonCode::RebaseConflict.into(),
            format!(
                "rebase onto {onto} {main} conflicted in {}; resolve it in the worktree (git rebase {target}), rerun your checks, and rewrite the receipt with the new head",
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
        binding::rewrite_record(
            repository,
            json!({"main": main, "head_before": head, "head_after": rebased}),
            (main, &head, &rebased),
        )?,
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
    let moved_by_rebase = rebased != head;
    let mut renumbered = false;
    let rebased = match renumber_migration(queue, repository, run, worktree, main, &rebased)? {
        Renumbering::Unchanged => rebased,
        Renumbering::Renumbered(head) => {
            renumbered = true;
            head
        }
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
    // Prove the approval covers the run's change in the final tree before
    // running verification (ADR-t5283-2 and ADR-t4840-2 decision 8).
    let merged = match binding::compare_landing(
        &*queue,
        repository,
        run,
        binding::Rewrites {
            main,
            head: &head,
            rebased: &rebased,
            moved_by_rebase,
            renumbered,
        },
        // No committed protected-path list is supplied by this runtime.
        &[],
    )? {
        binding::Compared::Matched { merged } => merged,
        binding::Compared::Changed {
            reason,
            reapproval,
            differing,
        } => {
            // A run in a container is rebased in its landing worktree,
            // made again at its validated commit by the next landing or
            // review: the rebased head its re-approval reads is kept.
            if run.in_container()
                && let Err(error) =
                    repository.update_ref(&binding::rewritten_ref(run), rebased.as_str())
            {
                warn!(op = "integrate", error = %format_args!("{error:#}"), "run {}: the rebased head {rebased} is not kept under {}: {error:#}", run.id(), binding::rewritten_ref(run));
            }
            return Ok(Verdict::Changed {
                reason,
                reapproval,
                differing,
            });
        }
    };
    let tree = repository.tree_of(rebased.as_str())?;
    let batched = matches!(gate, batch::Gate::Proof { .. });
    let verified = match gate {
        batch::Gate::Ordinary => false,
        batch::Gate::Proof { branch, .. } if branch != *landing_branch => {
            return Ok(batch_base_moved(main));
        }
        batch::Gate::Proof {
            tree: expected,
            rejection,
            ..
        } if expected == tree => {
            if let Some(verdict) = rejection {
                return Ok(verdict);
            }
            true
        }
        batch::Gate::Proof { .. } => false,
        batch::Gate::Stop(_) | batch::Gate::Abort(_) => {
            unreachable!("resolved before the landing gate")
        }
    };
    // The review programs the diff that lands requires, the cheap gate
    // before the commands (ADR-t3749-1): one that fails sends the run back
    // before any command runs.
    if !verified
        && let Some(programs) = ctx.programs.as_ref()
        && let Some(verdict) = programs::landing_programs(
            queue,
            repository,
            ctx.verifier,
            (files, ctx.clock),
            programs,
            ctx.handing_off,
            &programs::ProgramsAt {
                run,
                worktree,
                run_dir,
                onto,
                main,
                rebased: &rebased,
            },
        )?
    {
        return Ok(verdict);
    }
    if !verified {
        let commands = landing_plan(ctx.verifier, task)?;
        if let Some(verdict) = verify_landing(
            ctx,
            run,
            worktree,
            (main, &rebased, main),
            &commands,
            &|| KnownFailures::read(&*ctx.queue, ctx.verifier, task),
        )? {
            return Ok(verdict);
        }
    }
    // One commit on main with the rebased tree; the run's own history stays
    // reachable under refs/dagq/runs/<run-id>.
    let paragraphs = commit_message(task, run, &receipt);
    let commit = repository.commit_tree(&tree, main.as_str(), &paragraphs)?;
    let history_ref = format!("refs/dagq/runs/{}", run.id());
    repository.update_ref(&history_ref, rebased.as_str())?;
    if batched && repository.landing_branch()? != *landing_branch {
        return Ok(batch_base_moved(main));
    }
    if let Err(error) = repository.advance_main(landing_branch, main.as_str(), commit.as_str()) {
        if batched && repository.main_head().is_ok_and(|head| head != *main) {
            return Ok(batch_base_moved(main));
        }
        return Err(error);
    }
    let merged = binding::merged_record(merged, &commit);
    Ok(Verdict::Landed(
        Landing {
            commit,
            source_commit: rebased,
            main_before: main.clone(),
            history_ref,
            message: paragraphs.join("\n\n"),
            verification_skipped: false,
            merged: Some(merged),
        },
        receipt.into_follow_ups(),
    ))
}

fn batch_base_moved(main: &CommitSha) -> Verdict {
    Verdict::HandedOver {
        reason: "the landing branch moved during the batch".to_owned(),
        detail: json!({"main": main}),
    }
}

/// Run one landing gate against a rebased tree. Batch stages use the same
/// classification, retry, container and handoff rules as a single run.
/// The selection base stays at the original main across splits so a
/// path-selected gate still covers all accepted members in the tree.
fn verify_landing(
    ctx: &Integration<'_>,
    run: &TaskRun,
    worktree: &Path,
    (main, rebased, selection_base): (&CommitSha, &CommitSha, &CommitSha),
    commands: &[landing_verification::Planned],
    known: &dyn Fn() -> Result<KnownFailures>,
) -> Result<Option<Verdict>> {
    let queue = &*ctx.queue;
    let verifier = ctx.verifier;
    let files = ctx.files;
    let clock = ctx.clock;
    let container = ctx.container;
    let load_average = ctx.load_average;
    let retry_disk = ctx.retry_disk.as_ref();
    let sccache = ctx.sccache;
    let verification_tokens = ctx.verification_tokens;
    let verification_containers = ctx.verification_containers;
    let steps = ctx.steps.as_ref().map(|steps| &*steps.shared);
    let handing_off = ctx.handing_off;
    let run_dir = Path::new(run.run_dir().context("missing run directory")?);
    // The profiles that read the commands' output (ADR-t2268-1), read like
    // `[landing_verification]`: a declaration that cannot be resolved stops
    // before the commands, as an unreadable dagq.toml does.
    let toolchains = if commands.is_empty() {
        Toolchains::none()
    } else {
        verifier.toolchains()?
    };
    // Set to a container that cannot be used, the commands do not run on
    // the host instead (goal 38, ADR-t2113-1 decision 4): stop before them,
    // so the run goes back without using a resume.
    // On the host the landing's command reads its known failures from the
    // run directory, written before anything else runs.
    let replaced = commands.iter().any(landing_verification::Planned::replaced);
    let host_landing_env = if replaced && !verification_in_container(verifier, commands)? {
        known()?.env(files, run_dir, run_dir, selection_base)?
    } else {
        Vec::new()
    };
    let in_container = if commands.is_empty() {
        false
    } else {
        match verification_runtime(queue, verifier, container)? {
            Ok(runtime) => runtime == crate::domain::container_runtime::Runtime::Container,
            Err(message) => bail!("{message}"),
        }
    };
    // The host's verification's `TMPDIR`, until the landing ends.
    let _verification_tmp;
    let mut run_env = if commands.is_empty() {
        Vec::new()
    } else if in_container {
        // No program of the host runs the commands, and no cache of the
        // host's or the workers' is theirs (ADR-t2114-4 decision 4).
        let mut env = verifier.run_env(run_dir)?;
        without_wrapper(queue, clock, run, &mut env);
        env
    } else {
        // A program [run.env] names that cannot be executed would fail
        // every command: stop before them, like an unreadable dagq.toml,
        // so the run goes back without using a resume (ADR-0049 decision 9).
        if let Some(message) = verifier.run_env_programs(Some(run_dir))?.missing_message() {
            bail!("{message}");
        }
        let mut env = verifier.run_env(run_dir)?;
        _verification_tmp = host_verification_tmpdir(files, run.id(), &mut env)?;
        if let Some((port, why)) = guard_verification(sccache, run_dir, &mut env) {
            super::sccache::record_wrapper_removed(
                queue,
                run.id(),
                clock.now(),
                "integrate",
                port,
                &why,
                json!({"job": "verification"}),
            );
        }
        env
    };
    let non_incremental = if commands.is_empty() {
        Vec::new()
    } else {
        landing_verification::non_incremental(
            verifier.landing_verification()?.as_ref(),
            &toolchains,
        )
        .map_err(anyhow::Error::msg)?
    };
    for build in &non_incremental {
        run_env.extend(build.env.clone());
    }
    // The verification's token, revoked when it goes at the verification's
    // end, however it ends (ADR-t2114-1 decision 2). A run on the host does
    // not use it: the container's verification is handed its file.
    let verification_token = if commands.is_empty() {
        None
    } else {
        Some(super::queue_broker::VerificationToken::issue(
            verification_tokens,
            run.id(),
            super::queue_broker::TokenFor::Landing,
        )?)
    };
    // The container of the verification, on a clone of the rebased commit,
    // removed with what it left when it goes, however the verification
    // ends.
    let container_verification = match (&verification_token, in_container) {
        (Some(token), true) => {
            // The wait for the machine's memory is the run's record, between
            // the landing's start and its first command.
            let records = queue;
            Some(ContainerVerification::start(
                verification_containers,
                verifier,
                run,
                rebased,
                &token.file,
                &mut |ask, note| {
                    let Some((kind, payload)) =
                        super::run_container::memory_note_event(ask, note, json!({}))
                    else {
                        return;
                    };
                    if let Err(error) = records.record_runtime_event(run.id(), kind, payload) {
                        warn!(run_id = %run.id(), "run {}: the wait of its verification for the dagq machine's memory could not be recorded: {error:#}", run.id());
                    }
                },
            )?)
        }
        (None, true) => bail!("the verification in the container has no token"),
        (_, false) => None,
    };
    // In a container, the landing's own command reads its known failures
    // from the materials it mounts: staged with the preparation command's,
    // then copied into the run directory with them.
    let landing_env = match &container_verification {
        Some(verification) if replaced => known()?.env(
            files,
            &verification.prepared.staged,
            &verification.prepared.materials,
            selection_base,
        )?,
        _ => host_landing_env,
    };
    if let Some(verification) = &container_verification {
        verification
            .containers
            .publish_materials(&verification.prepared)
            .context("copy the verification's materials into the run directory")?;
    }
    if !commands.is_empty() && !non_incremental.is_empty() {
        let outputs = super::landing_build_cleanup::remove(files, worktree, &non_incremental);
        queue.record_runtime_event(
            run.id(),
            EventKind::LandingIncrementalRemoved,
            json!({"run_id": run.id(), "outputs": outputs}),
        )?;
    }
    // The landing's verification, done once more when only flaky tests
    // failed (task 768, ADR-t768-1): a new attempt of every command on the
    // same rebased head, in the slot it holds (main does not move meanwhile).
    // The receipt check and the rebase are not done again: the rebase moved
    // the run branch past the receipt's `commit`, so a second check would
    // find a `commit_mismatch`. The once is per landing: earlier
    // `integration_retried` events do not count, so a landing after a
    // resume or a recovery may retry once again.
    let mut flaky_retried = false;
    'verify: loop {
        // Each attempt keeps its own logs, so a second integrate of the run does
        // not overwrite why the first one failed.
        let attempt = next_integrate_attempt(files, run_dir);
        for (index, planned) in commands.iter().enumerate() {
            let index = index + 1;
            let command = &planned.command;
            let step = VerifyStep {
                verifier,
                container: container_verification.as_ref(),
                load_average,
                files,
                run,
                worktree,
                run_env: &run_env,
                toolchains: &toolchains,
                planned,
                extra_env: if planned.replaced() {
                    &landing_env
                } else {
                    &[]
                },
                attempt,
                steps,
            };
            let first = step.run(
                queue,
                index,
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
                        if let Some(read) = room.decision {
                            let decision = read().context("no disk pass available for landing")?;
                            landing_short_decision(&decision, run.id())
                        } else {
                            disk_short(
                                queue,
                                &room.config,
                                (room.free)(),
                                room.total,
                                room.moving.map(|moving| {
                                    vec![
                                        DiskRun {
                                            stage: DiskStage::Worker,
                                            current: None
                                        };
                                        moving()
                                    ]
                                }),
                                crate::application::unix_millis(clock.system_time()),
                                (Some(run.id()), landing_current(files, run)),
                            )?
                        }
                    }
                    _ => None,
                };
                if let Some(short) = short {
                    return Ok(Some(held(
                        run,
                        main,
                        rebased,
                        command,
                        index,
                        &last,
                        None,
                        Some(short),
                    )));
                }
                // Not retried while the supervisor drains for a handoff
                // (ADR-t3344-1): the next process lands it anew.
                if drains_for_handoff(handing_off) {
                    return Ok(Some(handed_over(
                        main,
                        rebased,
                        command,
                        (index, attempt),
                        &last,
                        &first_failure,
                    )));
                }
                let retried = step.run(
                    queue,
                    index,
                    &integrate_retry_log(run_dir, attempt, index),
                    Some(&last),
                )?;
                match &retried.failure {
                    None => continue,
                    Some(failure) if failure.class.is_environmental() => {
                        return Ok(Some(held(
                            run,
                            main,
                            rebased,
                            command,
                            index,
                            &retried,
                            Some(&last),
                            None,
                        )));
                    }
                    // The code's own failure on the retry: the worker's, as
                    // any other.
                    Some(_) => last = retried,
                }
            }
            let failure = last.failure.as_ref().unwrap_or(&first_failure);
            // Every failed test passed on the tool's retry: the landing is
            // done once more instead of a resume, once per landing (task
            // 1039), with the environment of the profile that read it flaky
            // (ADR-t2268-1 decision 6).
            if failure.class == verify_failure::FailureClass::Flaky && !flaky_retried {
                // Not done again while the supervisor drains for a handoff
                // (ADR-t3344-1): the next process lands it anew, its once
                // counted from that landing.
                if drains_for_handoff(handing_off) {
                    return Ok(Some(handed_over(
                        main,
                        rebased,
                        command,
                        (index, attempt),
                        &last,
                        failure,
                    )));
                }
                let retry_env = toolchains.flaky_retry_env(failure.profile.unwrap_or_default());
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
                            "profile": failure.profile,
                            "retry_env": retry_env
                                .iter()
                                .map(|(key, value)| (key.clone(), Value::from(value.as_str())))
                                .collect::<serde_json::Map<String, Value>>(),
                            "command": command,
                            "exit_code": last.exit_code,
                            "failure": failure.to_json(),
                            "failed_tests": last.tests_json(),
                            "flaky_tests": last.flaky_tests,
                            "log_path": last.log.to_str(),
                            "reason": reason,
                        })),
                )?;
                flaky_retried = true;
                // Over `[run.env]`'s value of the same variable.
                run_env.retain(|(key, _)| retry_env.iter().all(|(added, _)| added != key));
                run_env.extend(retry_env);
                continue 'verify;
            }
            return Ok(Some(verification_failed(
                main, rebased, command, index, &last, failure,
            )));
        }
        break;
    }
    drop(container_verification);
    drop(verification_token);
    Ok(None)
}

/// Whether the supervisor that asked for the landing drains for a
/// handoff now ([`Integration::handing_off`]).
fn drains_for_handoff(handing_off: Option<HandoffDrain<'_>>) -> bool {
    handing_off.is_some_and(HandoffDrain::hands_over)
}

/// The verdict of a landing handed over instead of verified again
/// (ADR-t3344-1): command `index` of verification `attempt` failed as
/// `failure` on `rebased`, which a handoff's drain does not verify again.
/// The payload of `integration_handed_over` says what failed and where its
/// log is.
fn handed_over(
    main: &CommitSha,
    rebased: &CommitSha,
    command: &str,
    (index, attempt): (usize, u32),
    last: &Checked,
    failure: &verify_failure::VerifyFailure,
) -> Verdict {
    let reason = format!(
        "verification command {command:?} failed after the rebase onto {main} ({}); see {}. It would be verified once more, but the supervisor drains for a handoff: the landing is handed over, its lease kept, and the next process lands it again",
        failure.class.as_str(),
        last.log.display()
    );
    Verdict::HandedOver {
        reason: reason.clone(),
        detail: json!({
            "main": main,
            "head": rebased,
            "attempt": attempt,
            "index": index,
            "command": command,
            "exit_code": last.exit_code,
            "failure": failure.to_json(),
            "failed_tests": last.tests_json(),
            "flaky_tests": last.flaky_tests,
            "log_path": last.log.to_str(),
        }),
    }
}

/// The landing of `run` a landing before this one put on `main` as
/// `commit` (on `parent`), and stopped before it recorded it (task 1118):
/// recorded as `auto_repaired` (`repair: landing_found_on_main`) and
/// returned as landed without a rebase, a verification or a new commit,
/// so the run is integrated, `main` pushed and the receipt's follow-ups
/// registered as after any landing.
#[allow(clippy::too_many_arguments)]
fn landed_before(
    queue: &mut dyn Queue,
    repository: &dyn Repository,
    files: &dyn RunFiles,
    task: &Task,
    run: &TaskRun,
    main: &CommitSha,
    commit: CommitSha,
    parent: CommitSha,
) -> Result<Verdict> {
    warn!(op = "integrate", commit = %commit, "run {} already landed on main as {commit}; it is recorded as landed, not landed again", run.id());
    queue.record_runtime_event(
        run.id(),
        EventKind::AutoRepaired,
        json!({
            "layer": "runtime",
            "repair": "landing_found_on_main",
            "conditions": {"commit": commit, "main": main},
            "detail": {"main_before": parent},
        }),
    )?;
    let receipt = run
        .receipt_path()
        .and_then(|path| files.read_to_string(Path::new(path)).ok())
        .and_then(|text| Receipt::parse(&text).ok());
    let source_commit = run
        .worktree_path()
        .map(Path::new)
        .filter(|worktree| files.is_dir(worktree))
        .and_then(|_| super::run_git::head(repository, run).ok())
        .unwrap_or_else(|| commit.clone());
    // The merge the stopped landing made, from its comparison that matched
    // and the landed commit, audited with the run's landing as any merge.
    let landed = binding::bind(repository, parent.as_str(), commit.as_str()).ok();
    let merged = binding::recovered_merged(
        &queue.run_events(run.id())?,
        landed.as_ref(),
        &parent,
        &commit,
    );
    Ok(Verdict::Landed(
        Landing {
            message: receipt
                .as_ref()
                .map(|receipt| commit_message(task, run, receipt).join("\n\n"))
                .unwrap_or_default(),
            commit,
            source_commit,
            main_before: parent,
            history_ref: format!("refs/dagq/runs/{}", run.id()),
            verification_skipped: false,
            merged,
        },
        receipt.and_then(Receipt::into_follow_ups),
    ))
}

/// The verdict on a container run whose landing worktree cannot be made
/// (ADR-t2179-1 decision 5): it does not land and waits for a person, as
/// nothing the session does would make it.
fn no_landing_worktree(run: &TaskRun, why: &str) -> Verdict {
    Verdict::Held {
        reason: format!(
            "run {} was not landed: {why}; see to the queue's landing directory, then land it with dagq integrate {}; no session is resumed",
            run.id(),
            run.task_id()
        ),
        detail: Reason::new(ReasonCode::GitFailed).on(json!({"landing_worktree": why})),
    }
}

/// The verdict on a container run whose commits the control side's git
/// cannot be kept from running the hooks and drivers of (ADR-t2179-1
/// decision 8): it does not land and waits for a person.
fn unguarded(run: &TaskRun, error: &anyhow::Error) -> Verdict {
    Verdict::Held {
        reason: format!(
            "run {} was not landed: {error:#}; see to the git configuration, then land it with dagq integrate {}; no session is resumed",
            run.id(),
            run.task_id()
        ),
        detail: Reason::new(ReasonCode::GitFailed).on(json!({"guard": format!("{error:#}")})),
    }
}

/// The commands the landing runs for `task`'s verification commands:
/// those `[landing_verification]` of `dagq.toml` names replaced by its
/// command, the others as registered (ADR-t1925-1 decision 4), and every
/// one as registered without it. Read like `[run.env]`, only for a task
/// that has commands to run. The snapshot of an `already_satisfied` claim
/// runs them so too (ADR-t2740-1).
fn landing_plan(
    verifier: &dyn Verifier,
    task: &Task,
) -> Result<Vec<landing_verification::Planned>> {
    let config = if task.verification_commands().is_empty() {
        None
    } else {
        verifier.landing_verification()?
    };
    Ok(landing_verification::plan(
        config.as_ref(),
        task.verification_commands(),
    ))
}

/// What the landing's own command is told of CI (ADR-t1925-1 decision 4):
/// the tests that fail already on the watched branch, as `dagq ci failures
/// --task` shows them for the task (the items of the CI failures it fixes
/// kept apart), and whether the task fixes one.
#[derive(Debug, Clone)]
pub struct KnownFailures {
    failures: Value,
    fix_run: bool,
}

impl KnownFailures {
    /// Read `task`'s from the queue.
    pub fn read<Q: RunLog + QueueRecords + ?Sized>(
        queue: &Q,
        verifier: &dyn Verifier,
        task: &Task,
    ) -> Result<Self> {
        let watch = verifier.ci_watch_config()?;
        let branch = watch.as_ref().and_then(|watch| watch.branch.clone());
        let failures = super::ci_watch::known_failures(
            queue,
            watch.as_ref(),
            branch.as_deref(),
            Some(task.id()),
        )?;
        let fix_run = !queue.ci_failure_findings_of(task.id())?.is_empty();
        Ok(Self { failures, fix_run })
    }

    /// A shared gate may omit only failures every member could omit.
    fn for_batch(policies: &[Self]) -> Result<Self> {
        let first = policies
            .first()
            .context("missing batch verification policy")?;
        let mut failures = first.failures.clone();
        let fix_run = policies.iter().any(|policy| policy.fix_run);
        if let Some(skipped) = failures["failures"].as_array_mut() {
            skipped.retain(|item| {
                policies.iter().all(|policy| {
                    policy.failures["failures"]
                        .as_array()
                        .is_some_and(|items| items.contains(item))
                })
            });
        }
        let mut kept = Vec::new();
        for policy in policies {
            if let Some(items) = policy.failures["kept_for_task"].as_array() {
                for item in items {
                    if !kept.contains(item) {
                        kept.push(item.clone());
                    }
                }
            }
        }
        failures["kept_for_task"] = json!(kept);
        Ok(Self { failures, fix_run })
    }

    /// The variables the landing's own command gets on `base`, the main
    /// it runs on: the failures written in `written` and named as they
    /// will be in `named` (its copy).
    fn env(
        &self,
        files: &dyn RunFiles,
        written: &Path,
        named: &Path,
        base: &CommitSha,
    ) -> Result<Vec<(String, String)>> {
        let path = written.join(landing_verification::KNOWN_FAILURES_FILE);
        files
            .write(
                &path,
                serde_json::to_string_pretty(&self.failures)?.as_bytes(),
            )
            .with_context(|| format!("write {}", path.display()))?;
        Ok(landing_verification::env(
            base.as_str(),
            &path_text(&named.join(landing_verification::KNOWN_FAILURES_FILE))?,
            self.fix_run,
        ))
    }
}

/// Where the verification commands run: the runtime `dagq.toml` sets the
/// verification to, or why they do not start (`Err`), when it sets the
/// container and the container cannot be used. A look at the container
/// records `container_runtime_unavailable` (or `_available`) as the
/// supervisor does, where the queue's latest one does not say it already,
/// so the inbox is told without a supervisor too.
fn verification_runtime(
    queue: &dyn EventStore,
    verifier: &dyn Verifier,
    container: &dyn super::ContainerRuntime,
) -> Result<std::result::Result<crate::domain::container_runtime::Runtime, String>> {
    use crate::domain::container_runtime::{
        CONTAINER_RUNTIME_KINDS, ContainerCheck, Runtime, RuntimeTarget,
    };
    let runtimes = verifier.runtimes()?;
    if runtimes.verification == Runtime::Host {
        return Ok(Ok(Runtime::Host));
    }
    let check = ContainerCheck::of(&runtimes, |target| container.availability(target));
    let last = queue.latest_queue_event(&CONTAINER_RUNTIME_KINDS)?;
    if let Some((kind, mut payload)) =
        check.transition(last.as_ref().map(|e| (&e.kind, &e.payload)))
    {
        payload["by"] = json!("integrate");
        queue.record_queue_event(kind, payload)?;
    }
    Ok(match check.why(RuntimeTarget::Verification) {
        None => Ok(Runtime::Container),
        Some(why) => Err(format!(
            "dagq.toml sets the verification's runtime to the container, which cannot run it: {why}; \
no verification command runs on the host instead"
        )),
    })
}

/// Whether `dagq.toml` sets the verification of `commands` to the
/// container (none to run is never in a container).
fn verification_in_container(
    verifier: &dyn Verifier,
    commands: &[landing_verification::Planned],
) -> Result<bool> {
    Ok(!commands.is_empty()
        && verifier.runtimes()?.verification
            == crate::domain::container_runtime::Runtime::Container)
}

/// Take the sccache wrapper out of `env`, the `[run.env]` of a
/// verification in the container
/// ([`crate::domain::run_container::without_sccache_wrapper`]), and record
/// it as `sccache_wrapper_removed` (`by` `integrate`).
fn without_wrapper(
    queue: &dyn Queue,
    clock: &dyn Clock,
    run: &TaskRun,
    env: &mut Vec<(String, String)>,
) {
    if let Some(target) = crate::domain::run_container::without_sccache_wrapper(env) {
        super::sccache::record_wrapper_removed(
            queue,
            run.id(),
            clock.now(),
            "integrate",
            target.port,
            "the verification runs in a container, which has no protected cache to compile with",
            json!({"job": "verification", "runtime": "container"}),
        );
    }
}

/// The log of the preparation command of a verification in the container,
/// in the run directory.
pub const PREPARE_LOG: &str = "verification-prepare.log";

/// A verification of a run in its container (ADR-t2113-3 decision 1): its
/// parts made ready and its materials prepared by [`Self::start`], each
/// command run in a container of its own by [`Self::run_to_log`], and what
/// it left removed when it goes, however the verification ends.
/// Preparation uses RunMachine for the machine, image and relay, with a new
/// build-output volume for this verification, never the worker's volume.
/// The directory and clone's safety checks are GitRepository::create_verification_clone's;
/// it also holds the 0600 token copy and command home. Known failures are
/// written with the prepared materials and published through descriptors,
/// replacing worker symlinks. Cleanup removes the final container as well as
/// those left between commands, however verification ends.
struct ContainerVerification<'a> {
    containers: &'a dyn super::run_container::VerificationContainers,
    prepared: super::run_container::PreparedVerification,
    run: RunId,
    run_dir: PathBuf,
}

impl<'a> ContainerVerification<'a> {
    /// Make the verification of `run`'s commit `commit` ready with the
    /// token whose file is `token`, then run the preparation command
    /// `dagq.toml` declares, if any, on the host: one that fails runs no
    /// verification command. Then reserve the dagq machine's memory for its
    /// commands, waiting in the host-wide order until it fits, `notes` told
    /// of the wait (ADR-t3163-1 decision 4): held to the verification's end,
    /// never released between its commands, and no command runs before it
    /// is reserved, on the host or anywhere.
    fn start(
        containers: &'a dyn super::run_container::VerificationContainers,
        verifier: &dyn Verifier,
        run: &TaskRun,
        commit: &CommitSha,
        token: &Path,
        notes: &mut dyn FnMut(
            &crate::domain::machine_memory::MemoryAsk,
            crate::domain::machine_memory::MemoryNote<'_>,
        ),
    ) -> Result<Self> {
        let run_dir = PathBuf::from(run.run_dir().context("missing run directory")?);
        let prepared = containers
            .prepare(run, commit, token)
            .context("make the verification's container ready")?;
        let verification = Self {
            containers,
            prepared,
            run: run.id().clone(),
            run_dir,
        };
        let log = verification.run_dir.join(PREPARE_LOG);
        if let Some(exit) = verifier.prepare_verification(&verification.prepared.staged, &log)? {
            ensure!(
                exit.success,
                "the preparation command [container.verification] of dagq.toml declares exited with {}; see {}; no verification command ran",
                exit.code
                    .map_or_else(|| "a signal".to_owned(), |code| code.to_string()),
                log.display()
            );
        }
        containers
            .reserve_memory(run, &verification.prepared, notes)
            .context("reserve the dagq machine's memory for the verification's container")?;
        Ok(verification)
    }

    fn read_oom(&self, since: i64) -> crate::domain::run_container::oom::OomObservation {
        use crate::domain::run_container::oom::{OomObservation, OomState};
        match self
            .prepared
            .queue
            .verification_container(&self.prepared.id)
        {
            Ok(name) => self
                .containers
                .read_oom(&name, since, self.prepared.limits.memory_mib),
            Err(why) => {
                let mut observation =
                    OomObservation::unreadable(&self.prepared.id, self.prepared.limits.memory_mib);
                observation.state = OomState::Unreadable(why);
                observation
            }
        }
    }

    /// Run `command` with `env` in the verification's container, its
    /// output in `log`: through `podman` on the host, whose limit for the
    /// whole command is the verifier's. A container an earlier command
    /// left (one stopped at its limit) is removed first. When podman made
    /// or started no container ([`podman_failed`]), the command did not
    /// run: that is an error, as a container [`Self::start`] could not make
    /// ready, not the command's failure.
    ///
    /// [`podman_failed`]: crate::domain::run_container::podman_failed
    fn run_to_log(
        &self,
        verifier: &dyn Verifier,
        command: &str,
        env: &[(String, String)],
        log: &Path,
    ) -> Result<super::Exit> {
        if let Err(error) = self.containers.remove_container(&self.prepared) {
            warn!(run_id = %self.run, "run {}: the verification's container could not be removed before its command: {error:#}", self.run);
        }
        let line = super::run_container::verification_line(
            self.containers,
            &self.prepared,
            &self.run,
            command,
            env,
        )?;
        let exit = verifier.run_to_log_for(&self.run, &line, &self.run_dir, &[], log)?;
        ensure!(
            !crate::domain::run_container::podman_failed(exit.code),
            "podman did not start the verification's container for {command:?} ({exit}); see {}; nothing was judged, and it did not run on the host instead",
            log.display()
        );
        Ok(exit)
    }
}

impl Drop for ContainerVerification<'_> {
    fn drop(&mut self) {
        if let Err(error) = self.containers.finish(&self.prepared) {
            warn!(run_id = %self.run, "run {}: what the verification {} left could not all be removed: {error:#}", self.run, self.prepared.id);
        }
    }
}

/// Set in `env`, the `[run.env]` of the landing's verification commands on
/// the host (its flaky retry included) or of an `already_satisfied`
/// snapshot's on the host ([`verify_snapshot`]), `TMPDIR` to the run's
/// [`host_verification_tmp_path`](super::host_verification_tmp_path), short
/// and outside the queue's `runs/`, over a `TMPDIR` the `[run.env]` names:
/// what their tests make there (a queue's fixtures, a whole e2e workspace)
/// goes with the run's leftovers once its task is over, and counts in the
/// size of a recent run, instead of staying in the host's `$TMPDIR`; and
/// the sockets they make there fit in a socket path's length, and a test
/// that looks above its temporary directory for a `runs/` does not find
/// the run's, as the run directory's
/// [`RUN_TMP_DIR`](super::RUN_TMP_DIR) gave them neither. A container's
/// verification is not given it: the container takes what it left when it
/// goes. Neither is the landing recheck: it runs in a scratch worktree of
/// the queue's that the runs it checks and main's own check share, so what
/// it leaves is no one run's to remove.
///
/// The directory goes as soon as the verification ends when nothing is
/// left in it ([`VerificationTmp`]), so a verification that wrote nothing
/// there leaves nothing in `/tmp` for the run's cleanup.
fn host_verification_tmpdir<'a>(
    files: &'a dyn RunFiles,
    run: &RunId,
    env: &mut Vec<(String, String)>,
) -> Result<VerificationTmp<'a>> {
    let tmp = super::host_verification_tmp_dir(files, run)?;
    let text = tmp
        .to_str()
        .context("the temporary files' directory is not UTF-8")?
        .to_owned();
    env.retain(|(key, _)| key != "TMPDIR");
    env.push(("TMPDIR".to_owned(), text));
    Ok(VerificationTmp { files, dir: tmp })
}

/// The `TMPDIR` of a verification on the host ([`host_verification_tmpdir`]),
/// removed when this drops, at the verification's end however it ends, if
/// it is empty: what is in it waits for the run's cleanup, which counts it.
struct VerificationTmp<'a> {
    files: &'a dyn RunFiles,
    dir: PathBuf,
}

impl Drop for VerificationTmp<'_> {
    fn drop(&mut self) {
        if let Err(error) = self.files.remove_empty_dir(&self.dir) {
            warn!(
                "the verification's temporary files' directory {} could not be removed: {error:#}",
                self.dir.display()
            );
        }
    }
}

/// Put into `env`, the `[run.env]` of the verification commands, what the
/// look at its sccache server just before them found (ADR-t2086-1): the
/// guard made in `run_dir`, or no `RUSTC_WRAPPER`, refused the server's
/// start either way. Returns the port and why when they run without
/// `RUSTC_WRAPPER`, for `sccache_wrapper_removed`.
fn guard_verification(
    sccache: Option<&dyn super::SccacheServer>,
    run_dir: &Path,
    env: &mut Vec<(String, String)>,
) -> Option<(u16, String)> {
    let look = match (
        sccache,
        crate::domain::sccache::SccacheTarget::of_pairs(env),
    ) {
        (Some(server), Some(target)) => super::sccache::look(server, &target, run_dir),
        _ => return None,
    };
    look.apply(env);
    look.removed().map(|(port, why)| (port, why.to_owned()))
}

/// How integrate runs one verification command of an attempt and records
/// it as `verification_command`.
struct VerifyStep<'a> {
    verifier: &'a dyn Verifier,
    /// The container the commands run in; `None` runs them on the host.
    container: Option<&'a ContainerVerification<'a>>,
    load_average: fn() -> Option<f64>,
    files: &'a dyn RunFiles,
    run: &'a TaskRun,
    worktree: &'a Path,
    run_env: &'a [(String, String)],
    /// The profiles that read the command's output.
    toolchains: &'a Toolchains,
    /// The command to run, and the task's it runs in place of.
    planned: &'a landing_verification::Planned,
    /// What the command gets on top of `run_env`: [`KnownFailures::env`] for the
    /// landing's own command, nothing for a task's.
    extra_env: &'a [(String, String)],
    attempt: u32,
    /// Where the command's step goes ([`Integration::steps`]).
    steps: Option<&'a super::step_report::SharedVerification>,
}

/// One run of a verification command: its exit, why it failed (none when
/// it passed), the tests it names as failed, and its log.
#[derive(Clone)]
struct Checked {
    exit_code: i32,
    signal: Option<i32>,
    failure: Option<verify_failure::VerifyFailure>,
    failed_tests: Option<verify_failure::FailedTests>,
    /// Those of the failed tests the tool ran again and saw pass (task 768).
    flaky_tests: Vec<String>,
    /// The limit for the whole command, in seconds, when the verifier
    /// killed it there (`limit_secs` of `verification_command`); its
    /// failure is still the code's when tests failed in time before the
    /// kill (ADR-t4950-1).
    limit_secs: Option<u64>,
    log: PathBuf,
    oom_annotation: Option<String>,
}

impl Checked {
    fn tests_json(&self) -> Value {
        json!(self.failed_tests.as_ref().map(|tests| &tests.names))
    }

    /// `failed_tests_omitted` of `verification_command` and of
    /// `verification_failed`'s `integration_deferred`: how many failed
    /// tests were left out of `failed_tests` past
    /// [`MAX_FAILED_TESTS`](verify_failure::MAX_FAILED_TESTS), null when
    /// none were read.
    fn omitted_json(&self) -> Value {
        json!(self.failed_tests.as_ref().map(|tests| tests.omitted))
    }
}

impl VerifyStep<'_> {
    /// Run the planned command, the `index`th, with its output in `log`,
    /// and record it; `retry_of` is the run it retries (task 639), whose
    /// failure and log the event names. A command the verifier killed at
    /// its limit for the whole command is a `timeout` failure, not an
    /// error. A command run in place of the task's names them in the event
    /// (`replaces`) and atop its log.
    #[expect(
        clippy::disallowed_methods,
        reason = "L5: times the verification command it runs for the event; no decision reads it"
    )]
    fn run(
        &self,
        queue: &dyn super::EventStore,
        index: usize,
        log: &Path,
        retry_of: Option<&Checked>,
    ) -> Result<Checked> {
        let command = self.planned.command.as_str();
        let env: Vec<(String, String)> =
            self.run_env.iter().chain(self.extra_env).cloned().collect();
        let oom_since = self.container.map(|c| c.containers.oom_since());
        let started = Instant::now();
        let (status, load) = sampled(self.load_average, LOAD_SAMPLE_INTERVAL, || {
            match self.container {
                Some(container) => container.run_to_log(self.verifier, command, &env, log),
                None => {
                    self.verifier
                        .run_to_log_for(self.run.id(), command, self.worktree, &env, log)
                }
            }
        });
        let ended = Instant::now();
        let duration_secs = (ended.duration_since(started).as_secs_f64() * 1000.0).round() / 1000.0;
        let (code, signal, timed_out) = match status {
            Ok(status) => (status.code, status.signal, None),
            Err(error) => match error.downcast_ref::<verify_failure::CommandTimedOut>() {
                // Killed at the limit (SIGKILL).
                Some(timed_out) => (None, Some(9), Some(*timed_out)),
                None => return Err(error),
            },
        };
        // Lossy: a log cut off by a kill or a full disk may end mid-character,
        // and its marks still count.
        let read = self.files.read(log);
        let output = read
            .as_ref()
            .map(|bytes| String::from_utf8_lossy(bytes).into_owned())
            .unwrap_or_default();
        // The header is best effort: the disk that failed the command may
        // not take it either, and the failure is still to be judged.
        if let (Some(header), Ok(bytes)) = (self.planned.log_header(), &read) {
            let mut text = header.into_bytes();
            text.extend_from_slice(bytes);
            if let Err(error) = self.files.write(log, &text) {
                warn!(
                    op = "integrate",
                    "run {}: could not put the replaced commands atop {}: {error}",
                    self.run.id(),
                    log.display()
                );
            }
        }
        let mut checked = judge_command(
            self.toolchains,
            command,
            code,
            signal,
            timed_out,
            &output,
            log,
        );
        if crate::domain::run_container::oom::should_read(checked.failure.is_none(), code)
            && let (Some(container), Some(since), Some(failure)) =
                (self.container, oom_since, &checked.failure)
        {
            let observation = container.read_oom(since);
            checked.oom_annotation = super::run_container::record_verification_oom(
                queue,
                self.run.id(),
                &observation,
                command,
                failure,
            );
            if let Some(text) = &checked.oom_annotation {
                // Append only after classification; the annotation cannot
                // supply a classifier mark on this command or its retry.
                let mut annotated = self.files.read(log).unwrap_or_default();
                annotated.extend_from_slice(format!("\n{text}\n").as_bytes());
                if let Err(error) = self.files.write(log, &annotated) {
                    warn!("could not append container OOM diagnostic: {error:#}");
                }
            }
        }
        let exit_code = checked.exit_code;
        if let Some(steps) = self.steps {
            steps.command(
                (
                    command,
                    &crate::domain::command_shape::Subcommands::of(self.toolchains),
                ),
                (started, ended),
                code.map(i64::from),
                read.as_ref().ok().map(|bytes| bytes.len() as i64),
            );
        }
        let mut payload = json!({
            "phase": "integration",
            "attempt": self.attempt,
            "index": index,
            "command": command,
            "replaces": self.planned.replaces_json(),
            "exit_code": exit_code,
            "signal": signal,
            "failure": checked.failure.as_ref().map(|failure| failure.to_json()),
            "failed_tests": checked.tests_json(),
            "failed_tests_omitted": checked.omitted_json(),
            "flaky_tests": checked.flaky_tests,
            "limit_secs": checked.limit_secs,
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

/// What a run of `command` that ended with `code` or `signal` (or was
/// killed at its limit, `timed_out`) and wrote `output` to `log` says: its
/// exit (128 without a code), why it failed from its exit and its log (task
/// 467) or its limit, the tests it names as failed when tests failed or ran
/// out of time (task 515), and those the tool ran again and saw pass (task
/// 768), each read by `toolchains`' profiles. A command that passed still
/// names its flaky tests.
fn judge_command(
    toolchains: &Toolchains,
    command: &str,
    code: Option<i32>,
    signal: Option<i32>,
    timed_out: Option<verify_failure::CommandTimedOut>,
    output: &str,
    log: &Path,
) -> Checked {
    let exit_code = code.unwrap_or(128);
    // Killed at its limit: tests that had failed in time before the kill
    // are the code's, not the host's (ADR-t4950-1).
    let (failure, mut failed_tests) = match timed_out {
        Some(timed_out) => {
            let (failure, tests) = timed_out.judge(toolchains, output);
            (Some(failure), Some(tests))
        }
        None => {
            let failure = (exit_code != 0)
                .then(|| verify_failure::classify(toolchains, command, code, signal, output));
            let tests = failure
                .as_ref()
                .filter(|failure| {
                    matches!(
                        failure.class,
                        verify_failure::FailureClass::TestFailure
                            | verify_failure::FailureClass::Timeout
                            | verify_failure::FailureClass::Flaky
                    )
                })
                .map(|_| verify_failure::failed_tests(toolchains, output));
            (failure, tests)
        }
    };
    // A successful retry still records the flaky tests, including those of
    // a summary alone. Keep the same name limit as failed commands.
    if failure.is_none() {
        let mut names = verify_failure::flaky_tests(toolchains, output);
        if !names.is_empty() {
            let omitted = names.len().saturating_sub(verify_failure::MAX_FAILED_TESTS);
            names.truncate(verify_failure::MAX_FAILED_TESTS);
            failed_tests = Some(verify_failure::FailedTests { names, omitted });
        }
    }
    // The failed tests that passed on the tool's retry: the mark `stats`
    // counts them flaky by (task 768).
    let flaky_tests = match &failed_tests {
        Some(failed) => verify_failure::flaky_tests(toolchains, output)
            .into_iter()
            .filter(|name| failed.names.contains(name))
            .collect(),
        None => Vec::new(),
    };
    Checked {
        exit_code,
        signal,
        failure,
        failed_tests,
        flaky_tests,
        limit_secs: timed_out.map(|timed_out| timed_out.limit_secs),
        log: log.to_path_buf(),
        oom_annotation: None,
    }
}

/// Preserve the host's existing payload shape; only a positive container
/// observation adds the diagnostic to the notice's detail.
fn oom_detail(mut payload: Value, checked: &Checked) -> Value {
    if let Some(text) = &checked.oom_annotation {
        payload["container_oom"] = json!(text);
    }
    payload
}

/// The verdict on the `index`th verification command `command`, which
/// failed as `last` (why: `failure`) on `head`, rebased onto `main`: the
/// worker's to fix, so the run waits for a session.
fn verification_failed(
    main: &CommitSha,
    head: &CommitSha,
    command: &str,
    index: usize,
    last: &Checked,
    failure: &verify_failure::VerifyFailure,
) -> Verdict {
    Verdict::Deferred {
        reason: format!(
            "verification command {command:?} exited with {} after the rebase onto {main} ({}: {}); see {}",
            last.exit_code,
            failure.class.as_str(),
            failure.evidence,
            last.log.display()
        ) + &killed_after_failed_tests(last)
            + &last
                .oom_annotation
                .as_ref()
                .map(|s| format!(". {s}"))
                .unwrap_or_default(),
        detail: Reason::new(ReasonCode::VerificationFailed)
            .with("index", index)
            .on(oom_detail(
                json!({
                    "main": main,
                    "head": head,
                    "command": command,
                    "exit_code": last.exit_code,
                    "signal": last.signal,
                    "failure": failure.to_json(),
                    "failed_tests": last.tests_json(),
                    "failed_tests_omitted": last.omitted_json(),
                    "flaky_tests": last.flaky_tests,
                    "limit_secs": last.limit_secs,
                }),
                last,
            )),
    }
}

/// For the worker's notice of a command killed at its limit after tests
/// had failed in time (ADR-t4950-1): the limit and the tests' names, so
/// the session sees which to fix without the log; empty otherwise.
fn killed_after_failed_tests(last: &Checked) -> String {
    let (Some(limit), Some(tests)) = (last.limit_secs, &last.failed_tests) else {
        return String::new();
    };
    if tests.names.is_empty() {
        return String::new();
    }
    let more = match tests.omitted {
        0 => String::new(),
        n => format!(" and {n} more"),
    };
    format!(
        ". It ran past its {limit} s limit and was killed, after these tests had failed (not out of time): {}{more}",
        tests.names.join(", ")
    )
}

/// The verdict on a verification command that failed on the host: `last`
/// failed again when it retried `first`, or, with `first` `None`, failed on
/// a full disk that is `short` of room to retry it (task 639). The run
/// waits for a person, who lands it again once the host is fixed. The
/// `integration_held` payload names the retry's failure and log
/// (`failure`, `log_path`), whether it retried (`retried`), the first
/// failure and its log it retried (`first_failure`, `first_log_path`; null
/// when it did not retry), and the free and needed room when the disk was
/// short (`disk`).
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
            profile: None,
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
        ) + &last.oom_annotation.as_ref().map(|s| format!(". {s}")).unwrap_or_default(),
        detail: Reason::new(ReasonCode::VerificationEnvironment)
            .with("index", index)
            .on(oom_detail(json!({
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
            }), last)),
    }
}

/// What [`renumber_migration`] did to a rebased run.
#[derive(Debug)]
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
/// number (ADR-0067 decision 3), as [`plan_renumber`] decides: moved with
/// `git mv` to the next number free on `main` and committed on the run
/// branch, recorded as `migration_renumbered`.
fn renumber_migration(
    queue: &mut dyn Queue,
    repository: &dyn Repository,
    run: &TaskRun,
    worktree: &Path,
    main: &CommitSha,
    rebased: &CommitSha,
) -> Result<Renumbering> {
    let (collision, old, new, old_digits) = match plan_renumber(repository, main, rebased)? {
        RenumberPlan::Unchanged => return Ok(Renumbering::Unchanged),
        RenumberPlan::Blocked(blocked) => return Ok(blocked),
        RenumberPlan::Move {
            collision,
            old,
            new,
            old_digits,
        } => (collision, old, new, old_digits),
    };
    let next_digits = &collision.next_digits;
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
        Err(refused) => return Ok(collision.refused(&old, &new, &refused)),
    };
    queue.record_runtime_event(
        run.id(),
        EventKind::MigrationRenumbered,
        binding::rewrite_record(
            repository,
            json!({
                "main": main,
                "from": old,
                "to": new,
                "old_number": old_digits,
                "new_number": next_digits,
                "head_before": rebased,
                "head_after": head,
            }),
            (main, rebased, &head),
        )?,
    )?;
    info!(
        op = "integrate",
        run_id = %run.id(),
        "run {}: renumbered migration {old} to {new}",
        run.id()
    );
    Ok(Renumbering::Renumbered(head))
}
/// What [`plan_renumber`] decided for a rebased run.
#[derive(Debug)]
enum RenumberPlan {
    /// No migration the run adds has a number `main` has.
    Unchanged,
    /// A number is taken but cannot be moved mechanically:
    /// [`Renumbering::Blocked`].
    Blocked(Renumbering),
    /// Move the run's one migration `old` to `new`, from number
    /// `old_digits` to the collision's next free number.
    Move {
        collision: Collision,
        old: String,
        new: String,
        old_digits: String,
    },
}

/// A migration the run adds whose number `main` already has: the paths and
/// numbers the reason for a session names.
#[derive(Debug)]
struct Collision {
    main: CommitSha,
    head: CommitSha,
    /// The migrations the run adds, as paths.
    migrations: Vec<String>,
    /// Those of them whose number the rebased tree has otherwise.
    taken: Vec<String>,
    next_digits: String,
}

impl Collision {
    /// Left to a session because of `why`, with `extra` in its detail.
    fn blocked(&self, why: String, extra: Value) -> Renumbering {
        let Self {
            main,
            head,
            migrations,
            taken,
            next_digits,
        } = self;
        let mut detail = json!({
            "main": main,
            "head": head,
            "migrations": migrations,
            "taken": taken,
            "next_number": next_digits,
        });
        if let (Some(detail), Value::Object(extra)) = (detail.as_object_mut(), extra) {
            detail.extend(extra);
        }
        Renumbering::Blocked {
            reason: format!(
                "{why}; main {main} already has the number of {}, and the next free number is {next_digits}: renumber the run's migrations from {next_digits} (git mv), update what refers to their numbers, rerun the verification commands, and rewrite the receipt with the new head",
                taken.join(", ")
            ),
            detail,
        }
    }

    /// Left to a session because Git refused the commit moving `old` to
    /// `new` with `refused`.
    fn refused(&self, old: &str, new: &str, refused: &str) -> Renumbering {
        let gist = commit_error_gist(refused);
        self.blocked(
            format!("git refused the commit moving {old} to {new} ({gist})"),
            json!({"commit_error": gist}),
        )
    }
}

/// Whether and how to renumber the migration the rebased run adds, read
/// through `repository` alone: a collision is a migration the run added
/// whose number another file of the rebased tree, one the run did not add,
/// has. Only a run that adds exactly one migration, whose number none of
/// the run's other changed files mentions, is moved, to the next number
/// free on `main`; any other collision is left to a session with the next
/// free number.
///
/// Only in dagq's source repository (ADR-t614-1): the numbers are those of
/// dagq's own queue schema, and another repository's `migrations/` is its
/// own, left as any other file.
fn plan_renumber(
    repository: &dyn Repository,
    main: &CommitSha,
    rebased: &CommitSha,
) -> Result<RenumberPlan> {
    if !repository.is_dagq_source() {
        return Ok(RenumberPlan::Unchanged);
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
        return Ok(RenumberPlan::Unchanged);
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
        return Ok(RenumberPlan::Unchanged);
    }
    let next = kept.iter().copied().max().unwrap_or(0) + 1;
    let path = |name: &str| format!("{}/{name}", migration_numbers::DIRECTORY);
    let collision = Collision {
        main: main.clone(),
        head: rebased.clone(),
        migrations: added.iter().map(|name| path(name)).collect(),
        taken: taken.iter().map(|name| path(name)).collect(),
        next_digits: migration_numbers::digits(next),
    };
    if added.len() > 1 {
        return Ok(RenumberPlan::Blocked(collision.blocked(
            format!(
                "the run adds {} migrations, which are not renumbered mechanically",
                added.len()
            ),
            json!({}),
        )));
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
        return Ok(RenumberPlan::Blocked(collision.blocked(
            format!(
                "the run's other changes mention {old_digits} ({})",
                referring.join(", ")
            ),
            json!({"referring": referring}),
        )));
    }
    Ok(RenumberPlan::Move {
        new: path(&migration_numbers::renumbered(name, next)),
        old,
        old_digits,
        collision,
    })
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
/// `integrate-<attempt>-verify-<index>-retry.log`. [`integrate_logs`] and
/// [`next_integrate_attempt`] read it as a log of the same attempt.
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

/// The answer an ask of a run nobody closed is closed with once the run
/// is integrated.
const LANDED_LANDING_ASK_CLOSED: &str = "the run was integrated; closed by the runtime";

/// Close the asks nobody needs to answer once `run` landed, whether by
/// hand or by the supervisor: every ask of the run, whatever its kind
/// (its stalled detections ended first, so their ends are recorded), the
/// `approve_landing` asks of the task's other runs, as the task is
/// completed, and the `blocked` asks of the task naming no run (task 329).
/// An ask left open would stay in `dagq asks` and the inbox after the
/// landing. Each close is recorded on the run, or the task alone, its
/// `ask_opened` was, and leaves the ask's finding as it is. `main` already
/// moved, so a failure is only reported.
fn close_landing_asks(queue: &mut dyn AskStore, run: &TaskRun) {
    let closes = [
        (
            "stalled",
            queue.end_stalled_detections(run.id(), LANDED_LANDING_ASK_CLOSED),
        ),
        (
            "unclosed",
            queue.close_run_asks(run.id(), LANDED_LANDING_ASK_CLOSED),
        ),
        (
            "earlier runs' approve_landing",
            queue.close_ended_landing_asks(Some(run.task_id())),
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
/// What runs from under a worktree on the host is stopped first, as the
/// supervisor's cleanup does, so none writes into it while Git removes it.
/// When Git fails anyway (it may have removed the worktree's admin dir and
/// left the directory, `Directory not empty`), [`clear_left_worktree`]
/// takes the directory, or at least its `.git`, and the failure is still
/// recorded.
fn remove_landed_worktree<Q: EventStore + RunTransitions + ?Sized>(
    queue: &mut Q,
    repository: &dyn Repository,
    files: &dyn RunFiles,
    processes: &dyn ProcessControl,
    common_dir: &str,
    run: &TaskRun,
) {
    let (Some(worktree), Some(branch)) = (&run.worktree_path(), &run.branch()) else {
        return;
    };
    let path = Path::new(worktree);
    // The executables' paths are read resolved: so is the worktree's, and
    // the run directory's to tell that it resolves inside it.
    let resolved = run.run_dir().and_then(|dir| {
        Some((
            files.canonicalize(path).ok()?,
            files.canonicalize(Path::new(dir)).ok()?,
        ))
    });
    let own = if run.in_container() {
        None
    } else {
        own_worktree(run.run_dir(), path, common_dir, resolved)
    };
    let mut stopped = Vec::new();
    if let Some(resolved) = &own {
        match super::worktree_processes::stop_worktree_processes(
            processes,
            resolved,
            super::worktree_processes::WORKTREE_STOP_POLLS,
        ) {
            Ok(listed) => stopped = listed,
            Err(error) => warn!(
                op = "cleanup",
                run_id = %run.id(),
                error = %format_args!("{error:#}"),
                "run {}: the processes running from its worktree could not be listed: {error:#}",
                run.id()
            ),
        }
    }
    let removed = measured_removal(
        || files.tree_size(path),
        path,
        || super::run_git::remove(repository, run, branch),
    );
    let recorded = match removed {
        Ok(size) => queue.record_runtime_event(
            run.id(),
            EventKind::WorktreeRemoved,
            removed_worktree_payload(worktree, branch, &stopped, size),
        ),
        Err(error) => {
            let mut message = format!("landed worktree {worktree} could not be removed: {error:#}");
            if own.is_some() {
                message.push_str(&clear_left_worktree(files, path));
            }
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

/// Measure the landed worktree at `path` (`measure`), then `remove` it
/// whatever the measure gave: the size, or why it could not be measured,
/// once the removal succeeded. It runs on the landing's own thread, never
/// the supervise loop's, after the worktree's processes stopped.
fn measured_removal(
    measure: impl FnOnce() -> std::io::Result<Option<u64>>,
    path: &Path,
    remove: impl FnOnce() -> Result<()>,
) -> Result<std::result::Result<Option<u64>, String>> {
    let size = measure().map_err(|error| format!("measure {}: {error}", path.display()));
    remove().map(|()| size)
}

/// The `worktree_removed` of a landed run's worktree, with the bytes it
/// took as `size` measured it before its removal, in the form the
/// cleanup's removals take: a worktree that could not be measured has
/// `bytes` 0 and why in `unmeasured`, so its record reads apart from a
/// measured 0; none there (gone, or a link) is `worktree_missing`.
fn removed_worktree_payload(
    worktree: &str,
    branch: &str,
    stopped: &[Value],
    size: std::result::Result<Option<u64>, String>,
) -> Value {
    let mut payload = json!({"path": worktree, "branch": branch});
    match size {
        Ok(Some(bytes)) => payload["bytes"] = json!(bytes),
        // As the cleanup's: a worktree gone before (or a link) is no
        // run's size.
        Ok(None) => {
            payload["bytes"] = json!(0);
            payload["worktree_missing"] = json!(true);
        }
        Err(why) => {
            payload["bytes"] = json!(0);
            payload["unmeasured"] = json!([why]);
        }
    }
    if !stopped.is_empty() {
        payload["stopped_processes"] = json!(stopped);
    }
    payload
}

/// The resolved path of `worktree` when it is the run's own worktree under
/// its run directory ([`crate::domain::is_run_worktree`]) holding no part
/// of the repository (`common_dir`), both as
/// recorded and as `resolved` (the worktree's and the run directory's paths
/// with their links resolved): the only worktree the landing stops
/// processes under and removes by itself. A worktree a link replaced, to
/// another worktree or the checkout, resolves outside the run directory and
/// is none; so is one that could not be resolved.
fn own_worktree(
    run_dir: Option<&str>,
    worktree: &Path,
    common_dir: &str,
    resolved: Option<(PathBuf, PathBuf)>,
) -> Option<PathBuf> {
    let common = Path::new(common_dir);
    let (resolved, resolved_dir) = resolved?;
    let name = worktree.file_name()?;
    (run_dir.is_some_and(|dir| crate::domain::is_run_worktree(Path::new(dir), worktree))
        && !common.starts_with(worktree)
        && resolved == resolved_dir.join(name)
        && !common.starts_with(&resolved))
    .then_some(resolved)
}

/// What is left of a landed run's own worktree ([`own_worktree`]) once `git
/// worktree remove` failed goes: its `.git` first, then the directory. A
/// `.git` left would name an admin dir by a name the next worktree may
/// take, and the supervisor's cleanup would find that one by it. What was
/// done, for the failure's message.
fn clear_left_worktree(files: &dyn RunFiles, worktree: &Path) -> String {
    let gone = |result: std::io::Result<()>| match result {
        Err(error) if error.kind() != std::io::ErrorKind::NotFound => Err(error),
        _ => Ok(()),
    };
    let dotgit = gone(files.remove_file(&worktree.join(".git")));
    match (dotgit, gone(files.remove_dir_all(worktree))) {
        (_, Ok(())) => "; its directory was removed".to_owned(),
        (Ok(()), Err(dir)) => {
            format!("; its .git was removed, and removing its directory failed: {dir}")
        }
        (Err(dotgit), Err(dir)) => {
            format!("; and removing its .git ({dotgit}) and its directory ({dir}) failed")
        }
    }
}

/// How many more counted resumes the run has (ADR-0047 decision 24: a
/// resume of a run parked only by a conflict after its review passed is
/// not counted); unreadable events leave none.
fn resumes_left<Q: EventStore + ?Sized>(queue: &Q, id: &RunId) -> usize {
    queue.run_events(id).map_or(0, |events| {
        RunHistory::from_events(&events).resumes().left()
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::application::WorktreeAdmin;
    use crate::application::memory_files::MemoryFiles;
    use crate::domain::event_kind;
    use crate::domain::{
        EventFilter, EventId, Provider, RunEvent, RunRecord, TaskRecord, TaskStatus,
    };
    use std::cell::RefCell;

    const BASE: &str = "1111111111111111111111111111111111111111";
    const HEAD: &str = "2222222222222222222222222222222222222222";
    const RUN: &str = "00000000-0000-4000-8000-000000000001";

    /// The approval to land is recorded once, of a run awaiting
    /// integration or a session in the status its caller read it in and
    /// held by no live lease; any other is refused.
    #[test]
    fn a_run_is_approved_to_land_only_as_it_was_read_and_held_by_none() {
        let id = RunId::new(RUN).unwrap();
        let due = |status: RunStatus, read_in: Option<&str>, held, approved| {
            approval_due(&id, status, read_in, held, approved)
        };
        for status in [RunStatus::AwaitingIntegration, RunStatus::NeedsSession] {
            let read_in = Some(status.as_str());
            assert!(due(status, read_in, false, false).unwrap(), "{status:?}");
            // Approved already: nothing more is recorded.
            assert!(!due(status, read_in, false, true).unwrap(), "{status:?}");
            let held = due(status, read_in, true, false).unwrap_err();
            assert!(
                held.to_string().contains("is held by the supervisor"),
                "{held:#}"
            );
        }
        // Changed since it was read, or read in no status.
        for read_in in [Some("awaiting_integration"), None] {
            let changed = due(RunStatus::NeedsSession, read_in, false, false).unwrap_err();
            assert!(
                changed.to_string().contains("as it was read"),
                "{changed:#}"
            );
        }
        for status in [
            RunStatus::Integrating,
            RunStatus::Integrated,
            RunStatus::Running,
        ] {
            let refused = due(status, Some(status.as_str()), false, false).unwrap_err();
            assert!(
                refused
                    .to_string()
                    .contains("only a run awaiting integration"),
                "{status:?}: {refused:#}"
            );
        }
    }

    /// A person's `integrate` is recorded again unless the last judgment
    /// the landing compares with is an approval bound to the same set: one
    /// of diff A, then one of diff B whose landing stopped for another
    /// reason, then the diff back at A approves A again (the earlier
    /// approval of A is not what the landing compares with).
    #[test]
    fn a_direct_integrate_is_recorded_again_unless_the_last_judgment_is_its_own_set() {
        let bound = |blob: &str| json!({"binding": {"approved_blobs": {"f": {"blob": blob}}}});
        let approval = |id: i64, blob: &str| RunEvent {
            id: EventId::new(id),
            task_id: None,
            goal_id: None,
            run_id: None,
            kind: EventKind::IntegrationApproved.as_str().into(),
            payload: bound(blob),
            created_at: String::new(),
            actor: Some(crate::domain::EventActor {
                role: "user".into(),
                id: "u".into(),
                requested_by: None,
            }),
        };
        let a_then_b = [approval(1, "A"), approval(2, "B")];
        assert!(!approved_as_bound(&a_then_b, &bound("A")));
        assert!(approved_as_bound(&a_then_b, &bound("B")));
        assert!(approved_as_bound(&a_then_b[..1], &bound("A")));
        assert!(!approved_as_bound(&[], &bound("A")));
        // Then A again: the last judgment is A's approval.
        let a_b_a = [approval(1, "A"), approval(2, "B"), approval(3, "A")];
        assert!(approved_as_bound(&a_b_a, &bound("A")));
        assert!(!approved_as_bound(&a_b_a, &bound("B")));
    }

    /// A landed run's `worktree_removed` holds the bytes measured before
    /// the removal; one that could not be measured has `bytes` 0 and why
    /// in `unmeasured`, which a measured 0 has not; none there is
    /// `worktree_missing`.
    #[test]
    fn a_landed_worktree_is_recorded_with_its_size_or_why_it_was_unmeasured() {
        let payload = |size| removed_worktree_payload("/q/r/worktree", "dagq/r", &[], size);
        let measured = payload(Ok(Some(9_000)));
        assert_eq!(measured["bytes"], 9_000);
        assert_eq!(measured["path"], "/q/r/worktree");
        assert_eq!(measured["branch"], "dagq/r");
        assert!(measured.get("unmeasured").is_none(), "{measured}");
        assert!(measured.get("stopped_processes").is_none(), "{measured}");
        let zero = payload(Ok(Some(0)));
        assert_eq!(zero["bytes"], 0);
        assert!(zero.get("unmeasured").is_none(), "{zero}");
        assert!(zero.get("worktree_missing").is_none(), "{zero}");
        let missing = payload(Ok(None));
        assert_eq!(missing["bytes"], 0);
        assert_eq!(missing["worktree_missing"], true);
        assert!(missing.get("unmeasured").is_none(), "{missing}");
        let why = "measure /q/r/worktree: Too many open files".to_owned();
        let unmeasured = payload(Err(why.clone()));
        assert_eq!(unmeasured["bytes"], 0);
        assert_eq!(unmeasured["unmeasured"], json!([why]));
        let stopped = [json!({"pid": 7})];
        let with_stopped =
            removed_worktree_payload("/q/r/worktree", "dagq/r", &stopped, Ok(Some(1)));
        assert_eq!(with_stopped["stopped_processes"], json!(stopped));
    }

    /// The landed worktree is measured before its removal and removed even
    /// when it cannot be measured; a failed removal gives its error.
    #[test]
    fn a_landed_worktree_is_measured_first_and_removed_even_unmeasured() {
        let path = Path::new("/q/r/worktree");
        let calls = RefCell::new(Vec::new());
        let measured = measured_removal(
            || {
                calls.borrow_mut().push("measure");
                Ok(Some(9))
            },
            path,
            || {
                calls.borrow_mut().push("remove");
                Ok(())
            },
        );
        assert_eq!(measured.unwrap(), Ok(Some(9)));
        assert_eq!(*calls.borrow(), ["measure", "remove"]);
        calls.borrow_mut().clear();
        let unmeasured = measured_removal(
            || Err(std::io::Error::from_raw_os_error(libc::EMFILE)),
            path,
            || {
                calls.borrow_mut().push("remove");
                Ok(())
            },
        );
        let why = unmeasured.unwrap().unwrap_err();
        assert!(why.starts_with("measure /q/r/worktree: "), "{why}");
        assert_eq!(*calls.borrow(), ["remove"]);
        let failed = measured_removal(|| Ok(Some(9)), path, || anyhow::bail!("git failed"));
        assert!(failed.unwrap_err().to_string().contains("git failed"));
    }

    /// The landing stops processes under and removes only the run's own
    /// `<run dir>/worktree-<run id>` (or `<run dir>/worktree` of a run made
    /// before), as recorded and as resolved: not one with no
    /// run directory, elsewhere, holding the repository, replaced by a link
    /// that resolves outside the run directory, or not resolved at all.
    #[test]
    fn only_the_runs_own_worktree_resolved_inside_its_run_dir_is_the_landings_to_clear() {
        let dir = Some("/q/runs/r1");
        let worktree = Path::new("/q/runs/r1/worktree");
        let common = "/repo/.git";
        let resolved =
            |worktree: &str, dir: &str| Some((PathBuf::from(worktree), PathBuf::from(dir)));
        // Its ancestors may resolve elsewhere (`/var` to `/private/var`).
        assert_eq!(
            own_worktree(
                dir,
                worktree,
                common,
                resolved("/private/q/runs/r1/worktree", "/private/q/runs/r1")
            ),
            Some(PathBuf::from("/private/q/runs/r1/worktree"))
        );
        assert_eq!(
            own_worktree(
                None,
                worktree,
                common,
                resolved("/q/runs/r1/worktree", "/q/runs/r1")
            ),
            None
        );
        for elsewhere in [
            "/q/runs/r2/worktree",
            "/q/runs/r1",
            "/q/runs/r1/worktree/sub",
        ] {
            assert_eq!(
                own_worktree(
                    dir,
                    Path::new(elsewhere),
                    common,
                    resolved(elsewhere, "/q/runs/r1")
                ),
                None,
                "{elsewhere}"
            );
        }
        assert_eq!(
            own_worktree(
                dir,
                worktree,
                "/q/runs/r1/worktree/.git",
                resolved("/q/runs/r1/worktree", "/q/runs/r1")
            ),
            None
        );
        // A link in its place, to the checkout or another run's worktree.
        assert_eq!(
            own_worktree(dir, worktree, common, resolved("/repo", "/q/runs/r1")),
            None
        );
        assert_eq!(
            own_worktree(
                dir,
                worktree,
                common,
                resolved("/q/runs/r2/worktree", "/q/runs/r1")
            ),
            None
        );
        assert_eq!(own_worktree(dir, worktree, common, None), None);
        // A run's worktree of its own name, and none of another run's.
        let named = Path::new("/q/runs/r1/worktree-r1");
        assert_eq!(
            own_worktree(
                dir,
                named,
                common,
                resolved("/private/q/runs/r1/worktree-r1", "/private/q/runs/r1")
            ),
            Some(PathBuf::from("/private/q/runs/r1/worktree-r1"))
        );
        let other = Path::new("/q/runs/r1/worktree-r2");
        assert_eq!(
            own_worktree(
                dir,
                other,
                common,
                resolved("/q/runs/r1/worktree-r2", "/q/runs/r1")
            ),
            None
        );
    }

    #[test]
    fn a_command_timeout_can_be_identified_through_verification_context() {
        let timed_out = verify_failure::CommandTimedOut { limit_secs: 1800 };
        let error = anyhow::Error::new(timed_out).context("verification command \"x\"");
        assert_eq!(
            error.downcast_ref::<verify_failure::CommandTimedOut>(),
            Some(&timed_out)
        );
    }

    /// A repository whose worktree is on `branch` at `head`, with `status`.
    /// For the migration renumbering: whether it is dagq's source, the paths
    /// the run added and changed, the rebased tree's paths, and the contents
    /// [`Repository::paths_containing`] searches.
    struct FakeRepository {
        branch: Option<String>,
        head: CommitSha,
        status: String,
        calls: RefCell<Vec<String>>,
        dagq_source: bool,
        added: Vec<String>,
        changed: Vec<String>,
        tree: Vec<String>,
        contents: Vec<(String, String)>,
        merged_head: Option<CommitSha>,
        /// Whether reading the configuration for the guard fails, rather
        /// than the guard not holding.
        guard_unread: bool,
        /// Whether the worktree's git fails to read its branch.
        branch_unread: bool,
        /// The admin dir the worktree's `.git` names.
        admin: WorktreeAdmin,
    }

    impl FakeRepository {
        fn sound() -> Self {
            Self {
                branch: Some(format!("refs/heads/dagq/{RUN}")),
                head: sha(HEAD),
                status: String::new(),
                calls: RefCell::default(),
                dagq_source: true,
                added: Vec::new(),
                changed: vec!["src/lib.rs".to_owned()],
                tree: Vec::new(),
                contents: Vec::new(),
                merged_head: None,
                guard_unread: false,
                branch_unread: false,
                admin: WorktreeAdmin::Unreadable,
            }
        }
    }

    impl Repository for FakeRepository {
        fn guarded(&self) -> Result<Box<dyn Repository + Send + Sync>> {
            if self.guard_unread {
                anyhow::bail!("git config timed out");
            }
            Err(anyhow::Error::new(
                crate::domain::run_clone::GuardDoesNotHold("core.hooksPath is unset".to_owned()),
            ))
        }
        fn is_dagq_source(&self) -> bool {
            self.dagq_source
        }
        fn fresh_scratch(&self, path: &Path, commit: &str) -> Result<()> {
            self.calls
                .borrow_mut()
                .push(format!("fresh_scratch {} {commit}", path.display()));
            Ok(())
        }
        fn remove_scratch(&self, path: &Path) -> Result<()> {
            self.calls
                .borrow_mut()
                .push(format!("remove_scratch {}", path.display()));
            Ok(())
        }
        fn current_branch(&self, _: &Path) -> Result<Option<String>> {
            if self.branch_unread {
                anyhow::bail!("git symbolic-ref failed: fatal: not a git repository");
            }
            Ok(self.branch.clone())
        }
        fn worktree_admin(&self, _: &Path) -> Result<WorktreeAdmin> {
            Ok(self.admin.clone())
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
            Ok(self.changed.clone())
        }
        fn added_paths(&self, from: &str, to: &str) -> Result<Vec<String>> {
            self.calls
                .borrow_mut()
                .push(format!("added_paths {from} {to}"));
            Ok(self.added.clone())
        }
        fn paths_in(&self, _: &str, dir: &str) -> Result<Vec<String>> {
            Ok(self
                .tree
                .iter()
                .filter(|path| path.starts_with(&format!("{dir}/")))
                .cloned()
                .collect())
        }
        fn paths_containing(&self, _: &str, needle: &str, paths: &[String]) -> Result<Vec<String>> {
            Ok(self
                .contents
                .iter()
                .filter(|(path, text)| paths.contains(path) && text.contains(needle))
                .map(|(path, _)| path.clone())
                .collect())
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
        fn blobs_in(&self, _: &str, paths: &[&str]) -> Result<Vec<(String, String)>> {
            Ok(self
                .tree
                .iter()
                .filter(|path| paths.contains(&path.as_str()))
                .map(|path| (path.clone(), format!("blob:{path}")))
                .collect())
        }
        fn diff_text(&self, from: &str, to: &str) -> Result<Vec<u8>> {
            Ok(format!("{from}..{to}").into_bytes())
        }
        fn tree_of(&self, commit: &str) -> Result<String> {
            Ok(format!("tree:{commit}"))
        }
        fn merged_tree(
            &self,
            _: &str,
            _: &str,
        ) -> Result<std::result::Result<String, Vec<String>>> {
            let head = self
                .merged_head
                .as_ref()
                .context("no merge in the fixture")?;
            Ok(Ok(self.tree_of(head.as_str())?))
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

    impl crate::application::RepositoryReads for FakeRepository {
        fn main_head(&self) -> Result<CommitSha> {
            Ok(sha(BASE))
        }
        fn merge_base(&self, _: &str, _: &str) -> Result<Option<CommitSha>> {
            Ok(Some(sha(BASE)))
        }
    }

    /// A review's follow_up becomes a `docs_drift` draft of the task's
    /// goal keyed by its document (ADR-t2352-1): the title names the path
    /// and section, the description the job's text and which job of which
    /// review found it, and nothing else of the draft is filled in for the
    /// planner.
    #[test]
    fn a_reviews_follow_up_registers_a_docs_drift_draft_of_the_tasks_goal() {
        let mut record = task_record(&[], &[]);
        record.goal_id = Some(crate::domain::GoalId::new(194));
        let task = Task::restore(record).unwrap();
        let run = RunId::new(RUN).unwrap();
        let found = crate::domain::FoundReviewFollowUp {
            agent: Some("design-consistency".to_owned()),
            follow_up: crate::domain::ReviewFollowUp {
                text: "it still says the review sends the run back".to_owned(),
                path: "./docs/design/a.md".to_owned(),
                section: "#verdict".to_owned(),
                codes: vec!["docs_drift".to_owned()],
            },
        };
        let registration = review_follow_up_registration(&task, &run, 2, found);
        assert_eq!(registration.category, "docs_drift");
        assert_eq!(registration.membership_proposal, Value::Null);
        assert_eq!(registration.skipped, None);
        assert_eq!(
            registration.key,
            crate::domain::FollowUpKey::Review {
                attempt: 2,
                agent: Some("design-consistency".to_owned()),
                key: crate::domain::ReviewFollowUpKey::new("docs/design/a.md", "verdict"),
            }
        );
        let draft = registration.draft.unwrap();
        assert_eq!(draft.title, "docs_drift: docs/design/a.md (verdict)");
        assert_eq!(draft.goal_id, Some(crate::domain::GoalId::new(194)));
        assert!(
            draft
                .description
                .starts_with("it still says the review sends the run back\n\nFound by the agent design-consistency of review 2 of run "),
            "{}",
            draft.description
        );
        assert!(
            draft
                .context
                .contains(&format!("review 2 of run {RUN} of task 7"))
        );
        assert!(draft.acceptance.is_empty() && draft.verification_commands.is_empty());
        assert!(draft.dependencies.is_empty() && draft.paths.is_empty());
    }

    /// A receipt's follow_up becomes a draft of the task's goal with its
    /// trimmed title and description and a context naming the run and the
    /// task, and nothing else of the draft filled in (ADR-0041 decision 16);
    /// an entry without a non-blank title or a string description is
    /// skipped with the reason. The category is the worker's or
    /// `unlabeled` (ADR-t947-3), the membership proposal as written or
    /// null (ADR-t1504-2 decision 11), and the key is the entry's index.
    #[test]
    fn a_receipts_follow_up_is_a_draft_of_the_goal_or_skipped_with_the_reason() {
        let mut record = task_record(&[], &[]);
        record.goal_id = Some(crate::domain::GoalId::new(3));
        let task = Task::restore(record).unwrap();
        let run = RunId::new(RUN).unwrap();
        let proposal = json!({"classification": "out_of_scope", "acceptance_items": ["(1) done"], "reason": "r"});
        let entry = json!({"title": " later work ", "description": "outside the task", "category": "defect", "membership_proposal": proposal});
        let registration = receipt_follow_up_registration(&task, &run, 0, &entry);
        assert_eq!(
            registration.key,
            crate::domain::FollowUpKey::Receipt { index: 0 }
        );
        assert_eq!(registration.entry, entry);
        assert_eq!(registration.category, "defect");
        assert_eq!(registration.membership_proposal, proposal);
        assert_eq!(registration.skipped, None);
        let draft = registration.draft.unwrap();
        assert_eq!(draft.title, "later work");
        assert_eq!(draft.description, "outside the task");
        assert_eq!(draft.goal_id, Some(crate::domain::GoalId::new(3)));
        assert_eq!(
            draft.context,
            format!(
                "follow_up proposed by the receipt of run {RUN} of task 7 (  land the change  )"
            )
        );
        assert!(draft.acceptance.is_empty() && draft.verification_commands.is_empty());
        assert!(draft.dependencies.is_empty() && draft.goal_dependencies.is_empty());
        assert!(draft.paths.is_empty() && draft.required_evidence.is_empty());
        assert_eq!(draft.worker_mode, None);

        // An empty description is a description; no category and no
        // proposal are `unlabeled` and null.
        let plain = receipt_follow_up_registration(
            &task,
            &run,
            2,
            &json!({"title": "more work", "description": ""}),
        );
        assert_eq!(plain.key, crate::domain::FollowUpKey::Receipt { index: 2 });
        assert_eq!(plain.category, "unlabeled");
        assert_eq!(plain.membership_proposal, Value::Null);
        assert_eq!(plain.draft.unwrap().description, "");

        for (entry, reason) in [
            (
                json!({"title": "  ", "description": "d", "category": "ops", "membership_proposal": {"classification": "maybe"}}),
                "title is not a non-blank string",
            ),
            (
                json!({"description": "d"}),
                "title is not a non-blank string",
            ),
            (
                json!({"title": 5, "description": "d"}),
                "title is not a non-blank string",
            ),
            (
                json!({"title": "no description"}),
                "description is not a string",
            ),
            (
                json!({"title": "t", "description": 1}),
                "description is not a string",
            ),
        ] {
            let skipped = receipt_follow_up_registration(&task, &run, 1, &entry);
            assert_eq!(skipped.skipped.as_deref(), Some(reason), "{entry}");
            assert!(skipped.draft.is_none(), "{entry}");
            assert_eq!(skipped.entry, entry);
            assert_eq!(
                skipped.key,
                crate::domain::FollowUpKey::Receipt { index: 1 }
            );
        }
        // A skipped entry keeps the worker's category and proposal as written.
        let skipped = receipt_follow_up_registration(
            &task,
            &run,
            1,
            &json!({"title": "  ", "description": "d", "category": "ops", "membership_proposal": {"classification": "maybe"}}),
        );
        assert_eq!(skipped.category, "ops");
        assert_eq!(
            skipped.membership_proposal,
            json!({"classification": "maybe"})
        );

        // A task without a goal makes drafts without one.
        let lone = Task::restore(task_record(&[], &[])).unwrap();
        let draft = receipt_follow_up_registration(&lone, &run, 0, &entry)
            .draft
            .unwrap();
        assert_eq!(draft.goal_id, None);
    }

    fn sha(text: &str) -> CommitSha {
        CommitSha::parse(text, "commit").unwrap()
    }

    fn task(paths: &[&str]) -> Task {
        task_requiring(paths, &[EvidenceCheck::Tests])
    }

    fn task_requiring(paths: &[&str], required: &[EvidenceCheck]) -> Task {
        Task::restore(task_record(paths, required)).unwrap()
    }

    fn task_record(paths: &[&str], required: &[EvidenceCheck]) -> TaskRecord {
        TaskRecord {
            goal_priority: None,
            id: TaskId::new(7),
            title: "  land the change  ".to_owned(),
            description: String::new(),
            acceptance: String::new(),
            verification_commands: Vec::new(),
            required_evidence: required.to_vec(),
            paths: paths.iter().map(|p| (*p).to_owned()).collect(),
            priority: Default::default(),
            change: None,
            status: TaskStatus::InProgress,
            goal_id: None,
            context: String::new(),
            created_at: String::new(),
            updated_at: String::new(),
            worker: crate::domain::worker::Worker::CLAUDE_INTERACTIVE,
            named_mode: None,
            wait_for_build: false,
            execution_class: Default::default(),
        }
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
    /// The queue's `satisfied/` of a unit test.
    const SCRATCHES: &str = "/q/satisfied";

    /// A container run whose commits the repository's git cannot be kept
    /// from running the hooks and drivers of, or whose landing worktree
    /// cannot be made, lands nothing and waits for a person: held, not sent
    /// to its session (ADR-t2179-1 decisions 5 and 8). A run on the host
    /// takes the repository as it is.
    #[test]
    fn a_container_run_that_cannot_be_guarded_or_given_a_landing_worktree_is_held() {
        let container = run_on(
            Path::new("/q/clones/run"),
            RunStatus::Integrating,
            Provider::Claude,
        );
        assert!(container.in_container());
        let repository = FakeRepository::sound();
        let error = match super::super::run_git::for_run(&repository, &container) {
            Ok(_) => panic!("a repository that cannot guard its git guarded it"),
            Err(error) => error,
        };
        for verdict in [
            unguarded(&container, &error),
            no_landing_worktree(&container, "/q/landing/run is there already"),
        ] {
            let Verdict::Held { reason, detail } = verdict else {
                panic!("not held")
            };
            assert!(reason.contains("was not landed"), "{reason}");
            assert!(reason.contains("no session is resumed"), "{reason}");
            assert_eq!(detail["code"], "git_failed");
        }
        let host = run(Path::new(DIR));
        assert!(super::super::run_git::for_run(&repository, &host).is_ok());
    }

    /// A container run whose git cannot be guarded fails its validation
    /// with `git_failed` before its landing worktree is removed or anything
    /// of it is read: no error of the checks, and no session resumed.
    #[test]
    fn a_container_run_that_cannot_be_guarded_fails_its_validation() {
        let files = MemoryFiles::default();
        write_receipt(&files, Path::new("/q/clones/run"), "passed");
        let container = run_on(
            Path::new("/q/clones/run"),
            RunStatus::Validating,
            Provider::Claude,
        );
        let rejection = check_receipt(
            &FakeRepository::sound(),
            &files,
            &task(&[]),
            &container,
            &[],
        )
        .unwrap()
        .expect_err("an unguarded container run was accepted");
        assert_eq!(rejection.code, ReasonCode::GitFailed);
        assert!(
            rejection.reason.contains("no session is resumed"),
            "{}",
            rejection.reason
        );
        assert!(rejection.evidence_missing.is_empty() && rejection.scope_violation.is_empty());
        // A git that could not read the configuration is no verdict.
        let unread = FakeRepository {
            guard_unread: true,
            ..FakeRepository::sound()
        };
        assert!(check_receipt(&unread, &files, &task(&[]), &container, &[]).is_err());
    }

    /// A run on the host whose worktree's registration is broken (its
    /// admin dir pruned, so its git does not read, or taken by another
    /// worktree, so its git reads another branch) fails its validation with
    /// `git_failed` for its recovery job, never an error of the checks that
    /// every pass meets again. A git that fails on a worktree whose
    /// registration holds is the checks' error, and a worktree on another
    /// branch in its own admin dir goes back to its session as before.
    #[test]
    fn a_host_run_whose_worktree_is_not_registered_fails_its_validation() {
        let files = MemoryFiles::default();
        write_receipt(&files, Path::new(DIR), "passed");
        let host = run(Path::new(DIR));
        let check = |repository: &FakeRepository| {
            check_receipt(repository, &files, &task(&[]), &host, &[]).map(|checked| checked.err())
        };
        let taken = WorktreeAdmin::Found {
            points_back: false,
            head: "ref: refs/heads/other".to_owned(),
        };
        for (branch_unread, branch, admin) in [
            (true, None, WorktreeAdmin::Missing),
            (true, None, taken.clone()),
            (false, Some("refs/heads/other".to_owned()), taken.clone()),
            (false, None, taken),
        ] {
            let broken = FakeRepository {
                branch_unread,
                branch,
                admin,
                ..FakeRepository::sound()
            };
            let rejection = check(&broken)
                .unwrap()
                .expect("a broken worktree was accepted");
            assert_eq!(rejection.code, ReasonCode::GitFailed);
            for said in ["not registered on its branch", "no session is resumed"] {
                assert!(
                    rejection.reason.contains(said),
                    "{said}: {}",
                    rejection.reason
                );
            }
        }
        let unread = FakeRepository {
            branch_unread: true,
            admin: WorktreeAdmin::Found {
                points_back: true,
                head: String::new(),
            },
            ..FakeRepository::sound()
        };
        assert!(check(&unread).is_err());
        for admin in [
            WorktreeAdmin::Unreadable,
            WorktreeAdmin::Found {
                points_back: true,
                head: "ref: refs/heads/other".to_owned(),
            },
        ] {
            let elsewhere = FakeRepository {
                branch: Some("refs/heads/other".to_owned()),
                admin,
                ..FakeRepository::sound()
            };
            let rejection = check(&elsewhere)
                .unwrap()
                .expect("a worktree on another branch was accepted");
            assert_eq!(rejection.code, ReasonCode::CommitMismatch);
        }
        let on_its_branch = FakeRepository {
            admin: WorktreeAdmin::Missing,
            ..FakeRepository::sound()
        };
        assert!(check(&on_its_branch).unwrap().is_none());
        assert!(check(&FakeRepository::sound()).unwrap().is_none());
    }

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

    /// A verifier whose commands exit as `passes` says, one per command in
    /// order, and records each command it ran.
    struct ScriptedVerifier {
        passes: Vec<bool>,
        ran: RefCell<Vec<String>>,
        /// The environment of each command it ran, on top of `[run.env]`.
        envs: RefCell<Vec<Vec<(String, String)>>>,
        /// `[roles.verification]` sets the container.
        container: bool,
        /// `[landing_verification]`.
        landing: Option<landing_verification::LandingVerification>,
        /// `[run.env]`.
        run_env: Vec<(String, String)>,
        /// Kill every command at this limit for the whole command, in
        /// seconds, as the verifier does past `command_timeout_secs`.
        limit_secs: Option<u64>,
    }

    impl ScriptedVerifier {
        fn new(passes: Vec<bool>, container: bool) -> Self {
            Self {
                passes,
                ran: RefCell::new(Vec::new()),
                envs: RefCell::new(Vec::new()),
                container,
                landing: None,
                run_env: Vec::new(),
                limit_secs: None,
            }
        }
    }

    impl Verifier for ScriptedVerifier {
        fn landing_verification(
            &self,
        ) -> Result<Option<landing_verification::LandingVerification>> {
            Ok(self.landing.clone())
        }
        fn runtimes(&self) -> Result<crate::domain::container_runtime::RuntimeConfig> {
            use crate::domain::container_runtime::Runtime;
            Ok(crate::domain::container_runtime::RuntimeConfig {
                worker: Runtime::Host,
                verification: if self.container {
                    Runtime::Container
                } else {
                    Runtime::Host
                },
            })
        }
        fn run_env(&self, _: &Path) -> Result<Vec<(String, String)>> {
            Ok(self.run_env.clone())
        }
        fn run_env_programs(
            &self,
            _: Option<&Path>,
        ) -> Result<crate::domain::run_env::RunEnvCheck> {
            Ok(Default::default())
        }
        fn run_env_table(&self) -> Result<Option<Vec<(String, String)>>> {
            unreachable!("the snapshot's verification reads the run's environment only")
        }
        fn run_env_salt(&self) -> Result<String> {
            unreachable!("the snapshot's verification reads the run's environment only")
        }
        fn run_to_log(
            &self,
            command: &str,
            _: &Path,
            env: &[(String, String)],
            _: &Path,
        ) -> Result<super::super::Exit> {
            if let Some(limit_secs) = self.limit_secs {
                self.ran.borrow_mut().push(command.to_owned());
                return Err(verify_failure::CommandTimedOut { limit_secs }.into());
            }
            let mut ran = self.ran.borrow_mut();
            let success = self.passes[ran.len()];
            ran.push(command.to_owned());
            self.envs.borrow_mut().push(env.to_vec());
            Ok(super::super::Exit {
                success,
                code: Some(if success { 0 } else { 1 }),
                signal: None,
                description: String::new(),
            })
        }
    }

    /// The snapshot's verification runs the task's commands in order in the
    /// scratch worktree of the head and removes it after; the first that
    /// fails ends it, and `commands` counts those that ran, not those
    /// registered.
    #[test]
    fn a_snapshot_verification_stops_at_its_first_failure_and_counts_what_ran() {
        let mut record = task_record(&[], &[]);
        record.verification_commands = vec!["first".into(), "second".into(), "third".into()];
        let task = Task::restore(record).unwrap();
        let (files, dir) = (MemoryFiles::default(), Path::new(DIR));
        let run = run(dir);
        let verify = |passes: Vec<bool>| {
            let repository = FakeRepository::sound();
            let verifier = ScriptedVerifier::new(passes, false);
            let verification = verify_snapshot(
                &repository,
                &verifier,
                &files,
                &as_registered(&task),
                &run,
                (&sha(HEAD), Path::new(SCRATCHES)),
                NoContainers::parts(&NoContainers::default()),
            )
            .unwrap();
            (
                verification,
                verifier.ran.into_inner(),
                repository.calls.into_inner(),
            )
        };
        let (failed, ran, calls) = verify(vec![false, true, true]);
        assert_eq!(failed.commands, 1);
        assert_eq!(ran, ["first"]);
        let failure = failed.failure.unwrap();
        assert_eq!((failure.command.as_str(), failure.exit_code), ("first", 1));
        // Made again in the queue's `satisfied/`, outside the run's
        // directory, for every verification.
        let scratch = format!("{SCRATCHES}/{RUN}");
        assert!(!scratch.starts_with(DIR), "{scratch}");
        assert_eq!(
            calls,
            [
                format!("fresh_scratch {scratch} {HEAD}"),
                format!("remove_scratch {scratch}")
            ]
        );
        let (second, ran, _) = verify(vec![true, false, true]);
        assert_eq!((second.commands, ran.len()), (2, 2));
        let (passed, ran, _) = verify(vec![true, true, true]);
        assert_eq!((passed.commands, passed.failure, ran.len()), (3, None, 3));
    }

    /// On the host the snapshot's commands have the run's short temporary
    /// files' directory outside `runs/`, made for them, as their `TMPDIR`,
    /// over the one `[run.env]` names, so what their tests make there goes
    /// with the run's leftovers; the rest of the `[run.env]` stays.
    #[test]
    fn a_snapshot_verification_on_the_host_keeps_its_temporary_files_in_the_runs_short_directory() {
        let mut record = task_record(&[], &[]);
        record.verification_commands = vec!["first".into(), "second".into()];
        let task = Task::restore(record).unwrap();
        let (files, dir) = (MemoryFiles::default(), Path::new(DIR));
        let mut verifier = ScriptedVerifier::new(vec![true, true], false);
        verifier.run_env = vec![
            ("TMPDIR".to_owned(), "/tmp/person".to_owned()),
            ("CARGO_BUILD_JOBS".to_owned(), "4".to_owned()),
        ];
        let verification = verify_snapshot(
            &FakeRepository::sound(),
            &verifier,
            &files,
            &as_registered(&task),
            &run(dir),
            (&sha(HEAD), Path::new(SCRATCHES)),
            NoContainers::parts(&NoContainers::default()),
        )
        .unwrap();
        assert_eq!((verification.commands, verification.failure), (2, None));
        let tmp = super::super::host_verification_tmp_path(&RunId::new(RUN).unwrap());
        let expected = vec![
            ("CARGO_BUILD_JOBS".to_owned(), "4".to_owned()),
            ("TMPDIR".to_owned(), tmp.to_str().unwrap().to_owned()),
        ];
        assert_eq!(verifier.envs.into_inner(), [expected.clone(), expected]);
        // Nothing was written there: it went at the verification's end.
        assert!(!files.is_dir(&tmp));
    }

    /// The containers and the tokens of a snapshot's verification in a
    /// unit test: what each was asked is recorded, and a container's
    /// command line is the command alone, which the scripted verifier runs.
    #[derive(Default)]
    struct NoContainers {
        calls: std::sync::Mutex<Vec<String>>,
        /// The arguments of each container's command line, joined.
        lines: std::sync::Mutex<Vec<String>>,
    }

    impl NoContainers {
        fn parts(&self) -> SnapshotContainers<'_> {
            SnapshotContainers {
                containers: self,
                tokens: self,
                events: None,
            }
        }
        fn note(&self, call: String) {
            self.calls.lock().unwrap().push(call);
        }
    }

    impl super::super::queue_broker::VerificationTokens for NoContainers {
        fn issue(&self, run: &RunId, of: super::super::queue_broker::TokenFor) -> Result<PathBuf> {
            self.note(format!("issue {run} {of:?}"));
            Ok(PathBuf::from("/q/token"))
        }
        fn revoke(&self, run: &RunId, of: super::super::queue_broker::TokenFor) -> Result<bool> {
            self.note(format!("revoke {run} {of:?}"));
            Ok(true)
        }
        fn issued(
            &self,
            _: &RunId,
            _: super::super::queue_broker::TokenFor,
        ) -> Result<Option<super::super::queue_broker::TokenIssue>> {
            Ok(None)
        }
        fn revoke_issued(
            &self,
            _: &RunId,
            _: super::super::queue_broker::TokenFor,
            _: &super::super::queue_broker::TokenIssue,
        ) -> Result<bool> {
            Ok(false)
        }
        fn sweep(
            &self,
            _: &mut dyn FnMut(&RunId, super::super::queue_broker::TokenFor) -> Result<bool>,
        ) -> Result<super::super::queue_broker::TokenSweep> {
            Ok(Default::default())
        }
    }

    impl super::super::run_container::VerificationContainers for NoContainers {
        fn prepare(
            &self,
            run: &TaskRun,
            commit: &CommitSha,
            token: &Path,
        ) -> Result<super::super::run_container::PreparedVerification> {
            self.note(format!("prepare {} {commit} {}", run.id(), token.display()));
            Ok(super::super::run_container::PreparedVerification {
                id: "v".to_owned(),
                queue: crate::domain::podman_machine::QueueKey::new("q").unwrap(),
                image: "image".to_owned(),
                dir: PathBuf::from("/q/verification/v"),
                staged: PathBuf::from("/q/verification/v/staged"),
                materials: PathBuf::from("/q/run/materials"),
                build_output: None,
                limits: Default::default(),
            })
        }
        fn publish_materials(
            &self,
            _: &super::super::run_container::PreparedVerification,
        ) -> Result<()> {
            self.note("publish_materials".to_owned());
            Ok(())
        }
        fn command_line(&self, args: &[String]) -> String {
            self.lines.lock().unwrap().push(args.join(" "));
            "in container".to_owned()
        }
        fn remove_container(
            &self,
            _: &super::super::run_container::PreparedVerification,
        ) -> Result<()> {
            Ok(())
        }
        fn reserve_memory(
            &self,
            _: &TaskRun,
            verification: &super::super::run_container::PreparedVerification,
            _: &mut dyn FnMut(
                &crate::domain::machine_memory::MemoryAsk,
                crate::domain::machine_memory::MemoryNote<'_>,
            ),
        ) -> Result<()> {
            self.note(format!("reserve_memory {}", verification.id));
            Ok(())
        }
        fn finish(&self, _: &super::super::run_container::PreparedVerification) -> Result<()> {
            self.note("finish".to_owned());
            Ok(())
        }
        fn remove_left(&self, _: &RunId) -> Result<Vec<String>> {
            unreachable!("the supervisor removes what an earlier verification left")
        }
    }

    /// With the verification set to the container, the snapshot's
    /// commands run through the verification's container on a clone of the
    /// head with a token of the snapshot's own, never on the host: no
    /// worktree is checked out, and what the container left and the token
    /// go at the end, the container's first; the first that fails ends it
    /// (ADR-t2113-3 decision 1, ADR-t2113-1 decision 4).
    #[test]
    fn a_snapshot_verification_set_to_the_container_runs_its_commands_in_containers() {
        let mut record = task_record(&[], &[]);
        record.verification_commands = vec!["first".into(), "second".into()];
        let task = Task::restore(record).unwrap();
        let (files, dir) = (MemoryFiles::default(), Path::new(DIR));
        let verify = |passes: Vec<bool>| {
            let repository = FakeRepository::sound();
            let verifier = ScriptedVerifier::new(passes, true);
            let containers = NoContainers::default();
            let verification = verify_snapshot(
                &repository,
                &verifier,
                &files,
                &as_registered(&task),
                &run(dir),
                (&sha(HEAD), Path::new(SCRATCHES)),
                containers.parts(),
            )
            .unwrap();
            assert!(repository.calls.into_inner().is_empty());
            (
                verification,
                verifier.ran.into_inner(),
                containers.calls.into_inner().unwrap(),
            )
        };
        let (passed, ran, calls) = verify(vec![true, true]);
        assert_eq!((passed.commands, passed.failure), (2, None));
        assert_eq!(ran, ["in container", "in container"]);
        assert_eq!(
            calls,
            [
                format!("issue {RUN} Snapshot"),
                format!("prepare {RUN} {HEAD} /q/token"),
                // The machine's memory before the first command, held to
                // the end.
                "reserve_memory v".to_owned(),
                "publish_materials".to_owned(),
                "finish".to_owned(),
                format!("revoke {RUN} Snapshot"),
            ]
        );
        let (failed, ran, calls) = verify(vec![false, true]);
        assert_eq!(failed.commands, 1);
        assert_eq!(ran.len(), 1);
        assert_eq!(failed.failure.unwrap().exit_code, 1);
        assert_eq!(
            calls[calls.len() - 2..],
            ["finish".to_owned(), format!("revoke {RUN} Snapshot")]
        );
    }

    /// `task`'s verification commands planned without
    /// `[landing_verification]`: every one as registered.
    fn as_registered(task: &Task) -> SnapshotPlan {
        SnapshotPlan {
            commands: landing_verification::plan(None, task.verification_commands()),
            known_failures: None,
        }
    }

    /// The commands of a task with two coverage gates among checks no
    /// landing replaces, and the `[landing_verification]` that replaces
    /// the gates.
    fn gated() -> (Task, landing_verification::LandingVerification) {
        let mut record = task_record(&[], &[]);
        record.verification_commands = vec![
            "cargo fmt --all --check".into(),
            "cargo llvm-cov nextest --locked --workspace --fail-under-lines 80".into(),
            "sh scripts/check-doc-links.sh".into(),
            "cargo llvm-cov --locked --fail-under-lines 80".into(),
        ];
        let config = landing_verification::LandingVerification {
            non_incremental: false,
            replaces: vec![
                "cargo llvm-cov nextest".into(),
                "cargo llvm-cov --locked --fail-under-lines 80".into(),
            ],
            command: "sh scripts/landing-it.sh".into(),
        };
        (Task::restore(record).unwrap(), config)
    }

    /// The snapshot's commands are planned as the landing's: with
    /// `[landing_verification]` its command runs once in place of every
    /// coverage gate, the other checks as registered and in order; without
    /// it every one as registered (ADR-t2740-1).
    #[test]
    fn a_snapshot_plans_its_commands_as_the_landing_does() {
        let (task, config) = gated();
        let mut verifier = ScriptedVerifier::new(Vec::new(), false);
        let planned = |verifier: &ScriptedVerifier| -> Vec<(String, Vec<String>)> {
            landing_plan(verifier, &task)
                .unwrap()
                .into_iter()
                .map(|planned| (planned.command, planned.replaces))
                .collect()
        };
        let registered: Vec<(String, Vec<String>)> = task
            .verification_commands()
            .iter()
            .map(|command| (command.clone(), Vec::new()))
            .collect();
        assert_eq!(planned(&verifier), registered);
        verifier.landing = Some(config);
        let commands = task.verification_commands();
        assert_eq!(
            planned(&verifier),
            [
                (commands[0].clone(), Vec::new()),
                (
                    "sh scripts/landing-it.sh".to_owned(),
                    vec![commands[1].clone(), commands[3].clone()]
                ),
                (commands[2].clone(), Vec::new()),
            ]
        );
    }

    /// The known failures of a snapshot's replacing command.
    fn known_failures() -> KnownFailures {
        KnownFailures {
            failures: json!({"failures": [{"kind": "test", "name": "dagq::it m::broken"}], "kept_for_task": []}),
            fix_run: false,
        }
    }

    #[test]
    fn a_batch_omits_only_failures_all_its_members_can_omit() {
        let failure = |name| json!({"kind": "test", "name": name});
        let shared = failure("dagq::it m::already_failing");
        let until_fixed = failure("dagq::it m::fixed_by_one_member");
        let partial = failure("dagq::it m::skipped_by_two_members");
        let other_fix = failure("dagq::it m::also_fixed");
        let policy = |failures, kept_for_task, fix_run| KnownFailures {
            failures: json!({
                "enabled": true,
                "branch": "main",
                "failures": failures,
                "kept_for_task": kept_for_task,
            }),
            fix_run,
        };
        let batch = KnownFailures::for_batch(&[
            policy(
                vec![shared.clone(), until_fixed.clone(), partial.clone()],
                vec![],
                false,
            ),
            policy(
                vec![shared.clone(), partial],
                vec![until_fixed.clone()],
                true,
            ),
            policy(
                vec![shared.clone()],
                vec![until_fixed.clone(), other_fix.clone()],
                true,
            ),
        ])
        .unwrap();
        assert_eq!(
            batch.failures,
            json!({
                "enabled": true,
                "branch": "main",
                "failures": [shared],
                "kept_for_task": [until_fixed, other_fix],
            })
        );
        assert!(batch.fix_run);
    }

    /// On the host the replacing command of a snapshot gets what the
    /// landing's own command gets, with the snapshot's head as its base
    /// and its known failures written to the run's directory; the checks
    /// it does not replace run without them, and no coverage gate runs.
    #[test]
    fn a_snapshot_runs_the_landing_command_in_place_of_its_coverage_gates_with_the_landing_env() {
        let (task, config) = gated();
        let (files, dir) = (MemoryFiles::default(), Path::new(DIR));
        let plan = SnapshotPlan {
            commands: landing_verification::plan(Some(&config), task.verification_commands()),
            known_failures: Some(known_failures()),
        };
        let verifier = ScriptedVerifier::new(vec![true; 3], false);
        let verification = verify_snapshot(
            &FakeRepository::sound(),
            &verifier,
            &files,
            &plan,
            &run(dir),
            (&sha(HEAD), Path::new(SCRATCHES)),
            NoContainers::parts(&NoContainers::default()),
        )
        .unwrap();
        assert_eq!((verification.commands, verification.failure), (3, None));
        assert_eq!(
            verifier.ran.into_inner(),
            [
                "cargo fmt --all --check",
                "sh scripts/landing-it.sh",
                "sh scripts/check-doc-links.sh"
            ]
        );
        let list = dir.join(landing_verification::KNOWN_FAILURES_FILE);
        let tmpdir = vec![(
            "TMPDIR".to_owned(),
            super::super::host_verification_tmp_path(&RunId::new(RUN).unwrap())
                .to_str()
                .unwrap()
                .to_owned(),
        )];
        let mut replacing = tmpdir.clone();
        replacing.extend(landing_verification::env(
            HEAD,
            list.to_str().unwrap(),
            false,
        ));
        assert_eq!(
            verifier.envs.into_inner(),
            [tmpdir.clone(), replacing, tmpdir]
        );
        let written: Value = serde_json::from_slice(&files.read(&list).unwrap()).unwrap();
        assert_eq!(written, known_failures().failures);
    }

    /// In the container the replacing command of a snapshot gets its
    /// known failures as the landing's container does: written with the
    /// verification's staged materials and named in the copy it mounts.
    #[test]
    fn a_snapshot_in_the_container_hands_its_replacing_command_the_landing_env() {
        let (task, config) = gated();
        let files = MemoryFiles::default();
        let plan = SnapshotPlan {
            commands: landing_verification::plan(Some(&config), task.verification_commands()),
            known_failures: Some(known_failures()),
        };
        let verifier = ScriptedVerifier::new(vec![true; 3], true);
        let containers = NoContainers::default();
        let verification = verify_snapshot(
            &FakeRepository::sound(),
            &verifier,
            &files,
            &plan,
            &run(Path::new(DIR)),
            (&sha(HEAD), Path::new(SCRATCHES)),
            containers.parts(),
        )
        .unwrap();
        assert_eq!((verification.commands, verification.failure), (3, None));
        let lines = containers.lines.into_inner().unwrap();
        assert_eq!(lines.len(), 3);
        let named = format!(
            "--env {}=/q/run/materials/{}",
            landing_verification::KNOWN_FAILURES_ENV,
            landing_verification::KNOWN_FAILURES_FILE
        );
        let base = format!("--env {}={HEAD}", landing_verification::BASE_ENV);
        assert!(
            lines[1].contains(&named) && lines[1].contains(&base),
            "{}",
            lines[1]
        );
        assert!(lines[1].contains("landing-it.sh"), "{}", lines[1]);
        for line in [&lines[0], &lines[2]] {
            assert!(!line.contains("DAGQ_LANDING_BASE"), "{line}");
            assert!(!line.contains("DAGQ_CI_KNOWN_FAILURES"), "{line}");
        }
        let staged =
            Path::new("/q/verification/v/staged").join(landing_verification::KNOWN_FAILURES_FILE);
        let written: Value = serde_json::from_slice(&files.read(&staged).unwrap()).unwrap();
        assert_eq!(written, known_failures().failures);
    }

    /// A receipt `integrate` reads lands only when it is `succeeded`: a
    /// `failed` or an `already_satisfied` one fails the run, saying which.
    #[test]
    fn only_a_succeeded_receipt_reaches_the_landing() {
        let of = |result: &str| {
            Receipt::parse(
                &json!({
                    "run_id": "r",
                    "result": result,
                    "commit": "0".repeat(40),
                    "tests": {"status": "passed", "evidence_or_reason": "ran"},
                    "e2e": {"status": "not_applicable", "evidence_or_reason": "none"},
                    "subagent_review": {"status": "passed", "evidence_or_reason": "ok"},
                    "summary": "s",
                })
                .to_string(),
            )
            .unwrap()
        };
        assert_eq!(receipt_ends_landing(&of("succeeded")), None);
        assert_eq!(
            receipt_ends_landing(&of("failed")).as_deref(),
            Some("session reported the run as failed: s")
        );
        assert_eq!(
            receipt_ends_landing(&of("already_satisfied")).as_deref(),
            Some("session reported already_satisfied, which has no commit to land: s")
        );
    }

    #[test]
    fn a_sound_receipt_is_accepted_through_the_repository_port() {
        let (files, dir) = (MemoryFiles::default(), Path::new(DIR));
        write_receipt(&files, dir, "passed");
        let repository = FakeRepository::sound();
        let Accepted {
            receipt,
            commit,
            e2e_requirement,
            already_satisfied,
        } = check_receipt(&repository, &files, &task(&[]), &run(dir), &[])
            .unwrap()
            .unwrap_or_else(|rejection| panic!("{}", rejection.reason));
        assert!(!e2e_requirement.required);
        assert!(!already_satisfied);
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
            &[],
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

    /// The repository's `[e2e] paths` (ADR-t963-1 decision 2): a diff that
    /// touches them needs the e2e, which the runtime runs after the review
    /// (ADR-t1233-2), so the receipt backs none on any provider; a task
    /// that asks for the e2e needs it whatever the diff.
    #[test]
    fn check_receipt_records_the_e2e_a_diff_touching_the_e2e_paths_needs() {
        let (files, dir) = (MemoryFiles::default(), Path::new(DIR));
        write_receipt(&files, dir, "passed");
        let repository = FakeRepository::sound();
        let touching = ["src/**".to_owned()];
        for provider in [Provider::Claude, Provider::Codex] {
            let run = run_on(dir, RunStatus::Validating, provider);
            let Ok(accepted) =
                check_receipt(&repository, &files, &task(&[]), &run, &touching).unwrap()
            else {
                panic!("the receipt backs no e2e");
            };
            assert!(accepted.e2e_requirement.required);
            assert_eq!(accepted.e2e_requirement.paths, ["src/lib.rs"]);
        }
        let run = run(dir);
        let outside = ["tests/e2e.rs".to_owned()];
        let Ok(accepted) = check_receipt(&repository, &files, &task(&[]), &run, &outside).unwrap()
        else {
            panic!("the diff stays outside the e2e paths");
        };
        assert!(!accepted.e2e_requirement.required);
        let asks = task_requiring(&[], &[EvidenceCheck::Tests, EvidenceCheck::E2e]);
        let Ok(accepted) = check_receipt(&repository, &files, &asks, &run, &outside).unwrap()
        else {
            panic!("the receipt backs no e2e the task asks for");
        };
        assert!(accepted.e2e_requirement.required);
    }

    /// The receipt read before landing: a Spike's without its result waits
    /// for a session that writes it, with the fields under `spike_result`;
    /// an implementation's is not asked for one (ADR-t1487-1 decision 3).
    #[test]
    fn a_landing_spike_receipt_without_its_result_is_left_to_a_session() {
        let receipt = |result: Option<Value>| {
            let mut text = json!({
                "run_id": "r", "result": "succeeded", "commit": HEAD,
                "tests": {"status": "passed", "evidence_or_reason": "ran"},
                "e2e": {"status": "not_applicable", "evidence_or_reason": "runtime"},
                "subagent_review": {"status": "passed", "evidence_or_reason": "ok"},
            });
            if let Some(result) = result {
                text["spike_result"] = result;
            }
            Receipt::parse(&text.to_string()).unwrap()
        };
        let spike = Task::restore(TaskRecord {
            execution_class: crate::domain::ExecutionClass::Spike,
            ..task_record(&[], &[])
        })
        .unwrap();
        let implementation = task_requiring(&[], &[]);
        assert!(landing_gaps(&receipt(None), &[], &implementation).is_none());
        let (code, reason, payload) = landing_gaps(&receipt(None), &[], &spike).unwrap();
        assert_eq!(code, ReasonCode::SpikeResultMissing);
        assert_eq!(reason, "spike result missing: spike_result");
        assert_eq!(
            payload,
            json!({"checks": [], "spike_result": ["spike_result"]})
        );
        let complete = json!({
            "verdict": "does_not_hold", "grounds": "g", "evidence": "e",
            "conditions": {"commit": HEAD, "tools": "t", "provider": "p", "environment": "e"},
        });
        assert!(landing_gaps(&receipt(Some(complete.clone())), &[], &spike).is_none());
        let (code, _, payload) =
            landing_gaps(&receipt(Some(complete)), &[EvidenceCheck::E2e], &spike).unwrap();
        assert_eq!(code, ReasonCode::EvidenceMissing);
        // `checks` is what the next resume reads to ask again.
        assert_eq!(payload, json!({"checks": ["e2e"]}));
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
        let Accepted {
            receipt, commit, ..
        } = check_receipt(&FakeRepository::sound(), &files, &task, &codex, &[])
            .unwrap()
            .unwrap_or_else(|rejection| panic!("{}", rejection.reason));
        assert_eq!(commit, sha(HEAD));
        assert_eq!(
            receipt.subagent_review().status(),
            crate::domain::CheckStatus::NotApplicable
        );
        let claude = run_on(dir, RunStatus::Validating, Provider::Claude);
        let rejection = check_receipt(&FakeRepository::sound(), &files, &task, &claude, &[])
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

    /// The event store alone, for a use case that reads only it: a test double
    /// implements this one port, not the whole queue. It keeps the
    /// `run_phase_changed` payloads recorded.
    struct EventsOnly {
        events: Option<Vec<RunEvent>>,
        phases: std::cell::RefCell<Vec<Value>>,
    }

    impl EventsOnly {
        fn of(events: Option<Vec<RunEvent>>) -> Self {
            Self {
                events,
                phases: Default::default(),
            }
        }
    }

    #[allow(unused_variables)]
    impl EventStore for EventsOnly {
        fn update_events(&self, limit: usize) -> Result<Vec<RunEvent>> {
            unreachable!("resumes_left reads only the run's events")
        }
        fn all_events(&self) -> Result<Vec<RunEvent>> {
            unreachable!("resumes_left reads only the run's events")
        }
        fn latest_task_events(&self, kinds: &[&str]) -> Result<Vec<RunEvent>> {
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
            assert_eq!(kind, EventKind::RunPhaseChanged);
            self.phases.borrow_mut().push(payload);
            Ok(())
        }
        fn last_observe(&self, mode: &str) -> Result<Option<i64>> {
            unreachable!("resumes_left reads only the run's events")
        }
        fn latest_event_id(&self) -> Result<EventId> {
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
        fn latest_events_by_supervisor(
            &self,
            kinds: &[&str],
            supervisors: &[&str],
        ) -> Result<Vec<RunEvent>> {
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
        fn events_between(
            &self,
            after: EventId,
            upto: EventId,
            filter: &EventFilter,
            limit: usize,
        ) -> Result<Vec<RunEvent>> {
            unreachable!("events_between is not what the test reaches")
        }
    }

    #[test]
    fn resumes_left_reads_the_run_log_alone() {
        let run = RunId::new(RUN).unwrap();
        let fresh = EventsOnly::of(Some(Vec::new()));
        assert_eq!(
            resumes_left(&fresh, &run),
            RunHistory::from_events(&[]).resumes().left()
        );
        assert_eq!(resumes_left(&EventsOnly::of(None), &run), 0);
    }

    fn phase_event(phase: run_phase::Phase, attempt: run_phase::Attempt) -> RunEvent {
        landing_event(
            "run_phase_changed",
            run_phase::PhaseChange::new(phase, attempt, "x").payload(),
        )
    }

    /// A person's `integrate` records the landing, holding the landing
    /// slot, once it took the integration slot, in the run's attempt.
    #[test]
    fn a_persons_landing_is_recorded_as_it_takes_the_slot() {
        let attempt = run_phase::Attempt::of(run_phase::AttemptKind::Resume, 1);
        let log = EventsOnly::of(Some(vec![phase_event(
            run_phase::Phase::LandingAnswer,
            attempt,
        )]));
        record_phase(
            &log,
            &RunId::new(RUN).unwrap(),
            EventKind::IntegrationStarted,
            |_| Some(run_phase::Phase::Landing),
        );
        assert_eq!(
            *log.phases.borrow(),
            [json!({
                "phase": "landing",
                "blocker": "compute",
                "holds": "landing_slot",
                "attempt": {"kind": "resume", "n": 1},
                "cause": "integration_started",
                "v": run_phase::RULES_VERSION,
            })]
        );
    }

    /// A person's landing that gives the slot back (a refused check, an
    /// error before main moved, a deferral, a hold or a failure) closes its
    /// phase with the phase of the status the run is left in, in the run's
    /// attempt; it records nothing that repeats the last record or that it
    /// cannot read, nor for the supervisor's landing.
    #[test]
    fn a_landing_given_back_closes_its_phase_by_the_status_it_leaves() {
        let person = ActorContext::instance(ActorRole::User, 1);
        let attempt = run_phase::Attempt::of(run_phase::AttemptKind::Revise, 2);
        let landing = || Some(vec![phase_event(run_phase::Phase::Landing, attempt)]);
        for (status, cause, phase) in [
            (
                RunStatus::AwaitingIntegration,
                EventKind::IntegrationError,
                "landing_answer",
            ),
            (
                RunStatus::NeedsSession,
                EventKind::IntegrationError,
                "needs_session",
            ),
            (
                RunStatus::NeedsSession,
                EventKind::IntegrationDeferred,
                "needs_session",
            ),
            (
                RunStatus::AwaitingIntegration,
                EventKind::IntegrationHeld,
                "landing_answer",
            ),
            (RunStatus::Failed, EventKind::IntegrationFailed, "ended"),
        ] {
            let log = EventsOnly::of(landing());
            record_landing_left(&log, &person, &run_in(Path::new("/tmp/run"), status), cause);
            let phases = log.phases.borrow();
            assert_eq!(phases.len(), 1, "{status:?}: {phases:?}");
            assert_eq!(phases[0]["phase"], phase);
            assert_eq!(phases[0]["cause"], cause.as_str());
            assert_eq!(phases[0]["attempt"], json!({"kind": "revise", "n": 2}));
        }
        let rested = EventsOnly::of(Some(vec![phase_event(
            run_phase::Phase::LandingAnswer,
            attempt,
        )]));
        let awaiting = run_in(Path::new("/tmp/run"), RunStatus::AwaitingIntegration);
        record_landing_left(&rested, &person, &awaiting, EventKind::IntegrationError);
        assert!(rested.phases.borrow().is_empty());
        let unreadable = EventsOnly::of(None);
        record_landing_left(&unreadable, &person, &awaiting, EventKind::IntegrationError);
        assert!(unreadable.phases.borrow().is_empty());
        // The supervisor's landing is recorded by its slot.
        let supervised = EventsOnly::of(landing());
        let supervisor = ActorContext::instance(ActorRole::Supervisor, 1);
        record_landing_left(
            &supervised,
            &supervisor,
            &awaiting,
            EventKind::IntegrationError,
        );
        assert!(supervised.phases.borrow().is_empty());
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
            kind: kind.into(),
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

    /// A Git remote double: `[repository]` is `config` (or fails to read
    /// with its error), the repository has the `remotes`, a push fails with
    /// `push_error` when set, and the check after a failed push answers
    /// `contains`. Every push and check is counted.
    struct FakeRemote {
        config: std::result::Result<RepositoryConfig, String>,
        remotes: Vec<&'static str>,
        remote_error: Option<String>,
        push_error: Option<String>,
        contains: std::result::Result<bool, String>,
        pushes: RefCell<Vec<String>>,
        checks: RefCell<Vec<String>>,
    }

    impl Default for FakeRemote {
        fn default() -> Self {
            Self {
                config: Ok(RepositoryConfig::default()),
                remotes: vec!["origin"],
                remote_error: None,
                push_error: None,
                contains: Ok(false),
                pushes: RefCell::default(),
                checks: RefCell::default(),
            }
        }
    }

    impl MainRemote for FakeRemote {
        fn push_config(&self) -> Result<RepositoryConfig> {
            self.config.clone().map_err(|error| anyhow::anyhow!(error))
        }
        fn has_remote(&self, remote: &str) -> Result<bool> {
            if let Some(error) = &self.remote_error {
                bail!("{error}");
            }
            Ok(self.remotes.contains(&remote))
        }
        fn push_main(&self, _: &PushGrant, remote: &str, branch: &LandingBranch) -> Result<()> {
            self.pushes
                .borrow_mut()
                .push(format!("{remote} {}", branch.name));
            match &self.push_error {
                Some(error) => bail!("{error}"),
                None => Ok(()),
            }
        }
        fn contains_landed_commit(
            &self,
            remote: &str,
            branch: &LandingBranch,
            commit: &CommitSha,
        ) -> Result<bool> {
            self.checks
                .borrow_mut()
                .push(format!("{remote} {} {commit}", branch.name));
            self.contains
                .clone()
                .map_err(|error| anyhow::anyhow!(error))
        }
    }

    fn configured(remote: Option<&str>, push: Option<bool>) -> RepositoryConfig {
        RepositoryConfig {
            branch: None,
            remote: remote.map(str::to_owned),
            push,
        }
    }

    /// What [`decide_push`] makes of `remote` onto `main`, `--no-push`
    /// reading the default `[repository]`.
    fn pushed_through(remote: Option<&dyn MainRemote>) -> PushDecision {
        decide_push(
            remote,
            RepositoryConfig::default,
            &PushGrant(()),
            &LandingBranch::main(),
            &sha(HEAD),
        )
    }

    fn report(
        outcome: PushResult,
        remote: &str,
        error: Option<&str>,
        reason: Option<&str>,
    ) -> PushReport {
        PushReport {
            outcome,
            remote: remote.to_owned(),
            branch: Some("main".to_owned()),
            error: error.map(str::to_owned),
            reason: reason.map(str::to_owned),
        }
    }

    #[test]
    fn the_landing_is_pushed_to_origin_and_recorded_as_push_finished() {
        let remote = FakeRemote::default();
        let decision = pushed_through(Some(&remote));
        assert_eq!(
            decision.report,
            report(PushResult::Pushed, "origin", None, None)
        );
        assert_eq!(decision.kind, EventKind::PushFinished);
        assert_eq!(
            decision.payload,
            json!({"remote": "origin", "branch": "main", "commit": HEAD, "already_delivered": false})
        );
        assert_eq!(decision.check_error, None);
        assert_eq!(*remote.pushes.borrow(), ["origin main"]);
        assert!(remote.checks.borrow().is_empty());
        // The report as integrate's outcome shows it.
        assert_eq!(
            serde_json::to_value(&decision.report).unwrap(),
            json!({"outcome": "pushed", "remote": "origin", "branch": "main", "error": null})
        );
    }

    #[test]
    fn a_rejected_push_the_remote_already_has_is_pushed_and_already_delivered() {
        let remote = FakeRemote {
            push_error: Some("cannot lock ref: is at newer but expected older".into()),
            contains: Ok(true),
            ..FakeRemote::default()
        };
        let decision = pushed_through(Some(&remote));
        assert_eq!(
            decision.report,
            report(PushResult::Pushed, "origin", None, None)
        );
        assert_eq!(decision.kind, EventKind::PushFinished);
        assert_eq!(
            decision.payload,
            json!({"remote": "origin", "branch": "main", "commit": HEAD, "already_delivered": true})
        );
        assert_eq!(*remote.checks.borrow(), [format!("origin main {HEAD}")]);
    }

    #[test]
    fn a_rejected_push_the_remote_does_not_have_fails_with_gits_message() {
        let remote = FakeRemote {
            push_error: Some("rejected: fetch first".into()),
            ..FakeRemote::default()
        };
        let decision = pushed_through(Some(&remote));
        assert_eq!(
            decision.report,
            report(
                PushResult::Failed,
                "origin",
                Some("rejected: fetch first"),
                None
            )
        );
        assert_eq!(decision.kind, EventKind::PushFailed);
        assert_eq!(
            decision.payload,
            json!({"code": "push_failed", "remote": "origin", "branch": "main", "commit": HEAD, "error": "rejected: fetch first"})
        );
        assert_eq!(decision.check_error, None);
    }

    #[test]
    fn a_failed_remote_check_keeps_the_original_push_failure() {
        let remote = FakeRemote {
            push_error: Some("authentication failed".into()),
            contains: Err("remote unavailable".into()),
            ..FakeRemote::default()
        };
        let decision = pushed_through(Some(&remote));
        assert_eq!(
            decision.report,
            report(
                PushResult::Failed,
                "origin",
                Some("authentication failed"),
                None
            )
        );
        assert_eq!(decision.kind, EventKind::PushFailed);
        assert_eq!(decision.payload["error"], "authentication failed");
        assert_eq!(decision.check_error.as_deref(), Some("remote unavailable"));
    }

    #[test]
    fn no_push_skips_without_looking_at_the_remote_and_names_the_configured_remote() {
        let decision = pushed_through(None);
        assert_eq!(
            decision.report,
            report(PushResult::Skipped, "origin", None, Some("--no-push"))
        );
        assert_eq!(decision.kind, EventKind::PushSkipped);
        assert_eq!(
            decision.payload,
            json!({"remote": "origin", "branch": "main", "commit": HEAD, "reason": "--no-push"})
        );
        assert_eq!(
            serde_json::to_value(&decision.report).unwrap(),
            json!({"outcome": "skipped", "remote": "origin", "branch": "main", "error": null, "reason": "--no-push"})
        );
        // `--no-push` names the remote `[repository]` names.
        let upstream = decide_push(
            None,
            || configured(Some("upstream"), None),
            &PushGrant(()),
            &LandingBranch::main(),
            &sha(HEAD),
        );
        assert_eq!(upstream.report.remote, "upstream");
        assert_eq!(upstream.report.reason.as_deref(), Some("--no-push"));
    }

    #[test]
    fn a_missing_default_origin_skips_the_push() {
        let remote = FakeRemote {
            remotes: Vec::new(),
            ..FakeRemote::default()
        };
        let decision = pushed_through(Some(&remote));
        assert_eq!(
            decision.report,
            report(
                PushResult::Skipped,
                "origin",
                None,
                Some("the repository has no remote origin")
            )
        );
        assert_eq!(decision.kind, EventKind::PushSkipped);
        assert!(remote.pushes.borrow().is_empty());
    }

    #[test]
    fn the_push_follows_the_repository_table() {
        // remote = "upstream": pushed there, not to origin.
        let remote = FakeRemote {
            config: Ok(configured(Some("upstream"), None)),
            remotes: vec!["origin", "upstream"],
            ..FakeRemote::default()
        };
        let decision = pushed_through(Some(&remote));
        assert_eq!(
            decision.report,
            report(PushResult::Pushed, "upstream", None, None)
        );
        assert_eq!(decision.payload["remote"], "upstream");
        assert_eq!(*remote.pushes.borrow(), ["upstream main"]);

        // push = false: skipped with its own reason, the remote untouched.
        let remote = FakeRemote {
            config: Ok(configured(None, Some(false))),
            ..FakeRemote::default()
        };
        let decision = pushed_through(Some(&remote));
        assert_eq!(
            decision.report,
            report(
                PushResult::Skipped,
                "origin",
                None,
                Some("push = false in dagq.toml")
            )
        );
        assert_eq!(
            decision.payload,
            json!({"remote": "origin", "branch": "main", "commit": HEAD, "reason": "push = false in dagq.toml"})
        );
        assert!(remote.pushes.borrow().is_empty());

        // A configured remote that is missing fails the push (unlike a
        // missing default origin, which skips it).
        let remote = FakeRemote {
            config: Ok(configured(Some("upstream"), None)),
            ..FakeRemote::default()
        };
        let decision = pushed_through(Some(&remote));
        assert_eq!(decision.report.outcome, PushResult::Failed);
        assert_eq!(decision.report.remote, "upstream");
        let error = decision.report.error.clone().unwrap();
        assert_eq!(error, missing_remote("upstream"));
        assert!(
            error.contains("the remote upstream") && error.contains("[repository]"),
            "{error}"
        );
        assert_eq!(decision.kind, EventKind::PushFailed);
        assert_eq!(decision.payload["code"], "push_failed");
        assert_eq!(decision.payload["branch"], "main");
        assert!(remote.pushes.borrow().is_empty());
    }

    #[test]
    fn an_unreadable_repository_table_or_remote_list_fails_the_push() {
        let remote = FakeRemote {
            config: Err("dagq.toml:2: value of remote".into()),
            ..FakeRemote::default()
        };
        let decision = pushed_through(Some(&remote));
        assert_eq!(
            decision.report,
            report(
                PushResult::Failed,
                "origin",
                Some("dagq.toml:2: value of remote"),
                None
            )
        );
        assert_eq!(decision.kind, EventKind::PushFailed);
        assert!(remote.pushes.borrow().is_empty());

        let remote = FakeRemote {
            remote_error: Some("git remote failed".into()),
            ..FakeRemote::default()
        };
        let decision = pushed_through(Some(&remote));
        assert_eq!(
            decision.report,
            report(
                PushResult::Failed,
                "origin",
                Some("git remote failed"),
                None
            )
        );
        assert!(remote.pushes.borrow().is_empty());
    }

    #[test]
    fn the_push_goes_to_the_landing_branch_the_landing_began_on() {
        let remote = FakeRemote::default();
        let trunk = LandingBranch {
            name: "trunk".to_owned(),
            ..LandingBranch::main()
        };
        let decision = decide_push(
            Some(&remote),
            RepositoryConfig::default,
            &PushGrant(()),
            &trunk,
            &sha(HEAD),
        );
        assert_eq!(decision.report.branch.as_deref(), Some("trunk"));
        assert_eq!(decision.payload["branch"], "trunk");
        assert_eq!(*remote.pushes.borrow(), ["origin trunk"]);
    }

    const MIGRATION_MAIN: &str = "3333333333333333333333333333333333333333";

    /// A run of dagq's source that added `added` and changed `changed`,
    /// whose rebased tree has `tree`.
    fn renumbering(added: &[&str], changed: &[&str], tree: &[&str]) -> FakeRepository {
        let owned = |paths: &[&str]| paths.iter().map(|path| (*path).to_owned()).collect();
        FakeRepository {
            added: owned(added),
            changed: owned(changed),
            tree: owned(tree),
            ..FakeRepository::sound()
        }
    }

    fn plan_of(repository: &FakeRepository) -> RenumberPlan {
        plan_renumber(repository, &sha(MIGRATION_MAIN), &sha(HEAD)).unwrap()
    }

    fn blocked(plan: RenumberPlan) -> (String, Value) {
        match plan {
            RenumberPlan::Blocked(Renumbering::Blocked { reason, detail }) => (reason, detail),
            other => panic!("not blocked: {other:?}"),
        }
    }

    #[test]
    fn a_migration_whose_number_main_took_moves_to_the_next_free_number() {
        let repository = renumbering(
            &["migrations/0002_asks.sql", "asks.txt"],
            &["migrations/0002_asks.sql", "asks.txt"],
            &[
                "migrations/0001_first.sql",
                "migrations/0002_goals.sql",
                "migrations/0002_asks.sql",
            ],
        );
        let RenumberPlan::Move {
            collision,
            old,
            new,
            old_digits,
        } = plan_of(&repository)
        else {
            panic!("not moved");
        };
        assert_eq!(old, "migrations/0002_asks.sql");
        assert_eq!(new, "migrations/0003_asks.sql");
        assert_eq!(old_digits, "0002");
        assert_eq!(collision.next_digits, "0003");
        assert_eq!(collision.taken, ["migrations/0002_asks.sql"]);
    }

    #[test]
    fn a_batch_member_with_a_migration_collision_takes_ordinary_landing() {
        let dir = Path::new(DIR);
        let files = MemoryFiles::default();
        write_receipt(&files, dir, "passed");
        let run = run_in(dir, RunStatus::Integrating);
        let task = task(&[]);
        let source = sha(HEAD);
        let prefix = sha(MIGRATION_MAIN);
        let stacked = sha("4444444444444444444444444444444444444444");
        let own = "migrations/0002_asks.sql";
        let taken = "migrations/0002_goals.sql";

        for (repo, can_stack) in [
            // A free number and a clean merge can share the batch.
            (renumbering(&[own], &[own], &[own]), true),
            // These otherwise admissible members would collide with the prefix.
            (renumbering(&[own], &[own], &[taken, own]), false),
            (
                FakeRepository {
                    contents: vec![("notes.md".into(), "the 0002 migration".into())],
                    ..renumbering(&[own], &[own, "notes.md"], &[taken, own])
                },
                false,
            ),
        ] {
            let repo = FakeRepository {
                merged_head: Some(stacked.clone()),
                ..repo
            };
            let mut judgment = json!({"verdict": "pass"});
            binding::put(&mut judgment, &binding::bind(&repo, BASE, HEAD).unwrap());
            let event = landing_event(event_kind::REVIEW_FINISHED, judgment);
            assert_eq!(
                batch::can_verify(
                    &repo,
                    &files,
                    &run,
                    &task,
                    &[event],
                    (&prefix, &source, &stacked)
                )
                .unwrap(),
                can_stack,
            );
            assert_eq!(
                repo.calls.into_inner(),
                [format!("added_paths {prefix} {stacked}")],
            );
        }
    }

    #[test]
    fn migrations_without_a_collision_are_left_alone() {
        // Nothing added under migrations/, or a file that is not a migration.
        let none = renumbering(&["src/lib.rs", "migrations/README.md"], &[], &[]);
        assert!(matches!(plan_of(&none), RenumberPlan::Unchanged));
        // A free number.
        let free = renumbering(
            &["migrations/0002_goals.sql"],
            &["migrations/0002_goals.sql"],
            &["migrations/0001_first.sql", "migrations/0002_goals.sql"],
        );
        assert!(matches!(plan_of(&free), RenumberPlan::Unchanged));
        // A run that renames a migration main had keeps its number: judged on
        // the rebased tree, where no other file has it.
        let renamed = renumbering(
            &["migrations/0001_initial.sql"],
            &["migrations/0001_first.sql", "migrations/0001_initial.sql"],
            &["migrations/0001_initial.sql", "migrations/0002_goals.sql"],
        );
        assert!(matches!(plan_of(&renamed), RenumberPlan::Unchanged));
        // Outside dagq's source (ADR-t614-1) the repository's own
        // migrations are not looked at, a taken number or not.
        let elsewhere = FakeRepository {
            dagq_source: false,
            ..renumbering(
                &["migrations/0002_asks.sql"],
                &["migrations/0002_asks.sql"],
                &["migrations/0002_goals.sql", "migrations/0002_asks.sql"],
            )
        };
        assert!(matches!(plan_of(&elsewhere), RenumberPlan::Unchanged));
    }

    #[test]
    fn a_migration_mentioned_by_the_runs_other_changes_is_left_to_a_session() {
        let repository = FakeRepository {
            contents: vec![
                ("notes.md".into(), "migration 0002 adds asks\n".into()),
                // Not a file the run changed.
                ("old.md".into(), "0002\n".into()),
            ],
            ..renumbering(
                &["migrations/0002_asks.sql", "notes.md"],
                &["migrations/0002_asks.sql", "notes.md"],
                &[
                    "migrations/0001_first.sql",
                    "migrations/0002_goals.sql",
                    "migrations/0002_asks.sql",
                ],
            )
        };
        let (reason, detail) = blocked(plan_of(&repository));
        assert_eq!(
            reason,
            format!(
                "the run's other changes mention 0002 (notes.md); main {MIGRATION_MAIN} already has the number of migrations/0002_asks.sql, and the next free number is 0003: renumber the run's migrations from 0003 (git mv), update what refers to their numbers, rerun the verification commands, and rewrite the receipt with the new head"
            )
        );
        assert_eq!(
            detail,
            json!({
                "main": MIGRATION_MAIN,
                "head": HEAD,
                "migrations": ["migrations/0002_asks.sql"],
                "taken": ["migrations/0002_asks.sql"],
                "next_number": "0003",
                "referring": ["notes.md"],
            })
        );
    }

    #[test]
    fn a_run_adding_two_migrations_is_left_to_a_session() {
        let repository = renumbering(
            &["migrations/0002_a.sql", "migrations/0003_b.sql"],
            &["migrations/0002_a.sql", "migrations/0003_b.sql"],
            &[
                "migrations/0001_first.sql",
                "migrations/0002_goals.sql",
                "migrations/0002_a.sql",
                "migrations/0003_b.sql",
            ],
        );
        let (reason, detail) = blocked(plan_of(&repository));
        assert!(
            reason.starts_with(
                "the run adds 2 migrations, which are not renumbered mechanically; main "
            ),
            "{reason}"
        );
        assert!(
            reason.contains(
                "already has the number of migrations/0002_a.sql, and the next free number is 0003"
            ),
            "{reason}"
        );
        assert_eq!(
            detail,
            json!({
                "main": MIGRATION_MAIN,
                "head": HEAD,
                "migrations": ["migrations/0002_a.sql", "migrations/0003_b.sql"],
                "taken": ["migrations/0002_a.sql"],
                "next_number": "0003",
            })
        );
    }

    #[test]
    fn a_refused_renumbering_commit_names_what_git_said() {
        let repository = renumbering(
            &["migrations/0002_asks.sql"],
            &["migrations/0002_asks.sql"],
            &["migrations/0002_goals.sql", "migrations/0002_asks.sql"],
        );
        let RenumberPlan::Move {
            collision,
            old,
            new,
            ..
        } = plan_of(&repository)
        else {
            panic!("not moved");
        };
        let Renumbering::Blocked { reason, detail } = collision.refused(
            &old,
            &new,
            "error: gpg failed to sign the data\n\nfatal: failed to write commit object\n",
        ) else {
            panic!("not blocked");
        };
        let gist = "error: gpg failed to sign the data; fatal: failed to write commit object";
        assert!(
            reason.starts_with(&format!(
                "git refused the commit moving migrations/0002_asks.sql to migrations/0003_asks.sql ({gist}); "
            )) && reason.contains("the next free number is 0003"),
            "{reason}"
        );
        assert_eq!(detail["commit_error"], gist);
        assert_eq!(detail["next_number"], "0003");
    }

    fn checked(code: Option<i32>, signal: Option<i32>, output: &str) -> Checked {
        judge_command(
            &Toolchains::this_repository(),
            "cargo test",
            code,
            signal,
            None,
            output,
            Path::new("/runs/run/integrate-1-verify-2.log"),
        )
    }

    #[test]
    fn a_failed_verification_command_is_classified_from_its_exit_and_log() {
        // Passed: no failure, no tests.
        let passed = checked(Some(0), None, "test result: ok\n");
        assert_eq!((passed.exit_code, passed.failure.is_none()), (0, true));
        assert_eq!(passed.tests_json(), Value::Null);
        // A build error, named by its first error line and where it is.
        let build = checked(
            Some(101),
            None,
            "   Compiling dagq\nerror[E0063]: missing field `finding_id` in initializer of `NewAsk`\n  --> src/recovery.rs:12:5\nerror: could not compile `dagq`\n",
        );
        let evidence = "error[E0063]: missing field `finding_id` in initializer of `NewAsk` --> src/recovery.rs:12:5";
        assert_eq!(
            build.failure.as_ref().unwrap().to_json(),
            json!({"class": "build_error", "evidence": evidence})
        );
        assert_eq!(build.tests_json(), Value::Null);
        // A kill has no exit code: 128, and the signal says why.
        let killed = checked(None, Some(15), "");
        assert_eq!((killed.exit_code, killed.signal), (128, Some(15)));
        assert_eq!(
            killed.failure.as_ref().unwrap().to_json(),
            json!({"class": "killed", "evidence": "killed by signal 15 (SIGTERM)"})
        );
        // Nothing in the log: unknown.
        let unknown = checked(Some(1), None, "");
        assert_eq!(
            unknown.failure.as_ref().unwrap().to_json(),
            json!({"class": "unknown", "evidence": "exit 1 with an empty log"})
        );
        // Killed at its limit: a timeout, whatever the log says.
        let timed_out = judge_command(
            &Toolchains::this_repository(),
            "cargo test",
            None,
            Some(9),
            Some(verify_failure::CommandTimedOut { limit_secs: 60 }),
            "",
            Path::new("/log"),
        );
        assert_eq!(
            timed_out.failure.as_ref().unwrap().class,
            verify_failure::FailureClass::Timeout
        );
        assert_eq!(timed_out.tests_json(), json!([]));
    }

    #[test]
    fn a_failed_test_is_named_and_the_verdict_carries_it() {
        let failed = checked(
            Some(101),
            None,
            "test a::passes ... ok\ntest runtime_claim::waits ... FAILED\ntest b::breaks ... FAILED\n\nfailures:\n    runtime_claim::waits\n    b::breaks\n\ntest result: FAILED. 1 passed; 2 failed\n",
        );
        let failure = failed.failure.clone().unwrap();
        assert_eq!(failure.class, verify_failure::FailureClass::TestFailure);
        assert_eq!(
            failed.tests_json(),
            json!(["runtime_claim::waits", "b::breaks"])
        );
        assert_eq!(failed.omitted_json(), json!(0));
        let Verdict::Deferred { reason, detail } =
            verification_failed(&sha(BASE), &sha(HEAD), "cargo test", 2, &failed, &failure)
        else {
            panic!("not deferred");
        };
        assert_eq!(
            reason,
            format!(
                "verification command \"cargo test\" exited with 101 after the rebase onto {BASE} (test_failure: {}); see /runs/run/integrate-1-verify-2.log",
                failure.evidence
            )
        );
        assert_eq!(
            detail,
            json!({
                "code": "verification_failed",
                "index": 2,
                "main": BASE,
                "head": HEAD,
                "command": "cargo test",
                "exit_code": 101,
                "signal": null,
                "failure": failure.to_json(),
                "failed_tests": ["runtime_claim::waits", "b::breaks"],
                "failed_tests_omitted": 0,
                "flaky_tests": [],
                "limit_secs": null,
            })
        );
    }

    /// ADR-t4950-1: a command the verifier killed at its limit after tests
    /// had failed in time is the code's `test_failure`, not the host's
    /// `timeout`: not retried, its event names the tests and the limit,
    /// and its verdict resumes the worker with them. The retry of a
    /// command that only ran out of time reads its log alike.
    #[test]
    fn a_command_killed_at_its_limit_after_failed_tests_goes_back_to_the_worker() {
        let files = MemoryFiles::default();
        let dir = Path::new(DIR);
        let task_run = run(dir);
        let queue = crate::application::port_fakes::SupervisorsAndEvents::default();
        let mut verifier = ScriptedVerifier::new(Vec::new(), false);
        verifier.limit_secs = Some(3600);
        let toolchains = Toolchains::this_repository();
        let planned = landing_verification::Planned {
            command: "sh scripts/landing-it.sh".to_owned(),
            replaces: Vec::new(),
        };
        let step = VerifyStep {
            verifier: &verifier,
            container: None,
            load_average: || None,
            files: &files,
            run: &task_run,
            worktree: dir,
            run_env: &[],
            toolchains: &toolchains,
            planned: &planned,
            extra_env: &[],
            attempt: 1,
            steps: None,
        };
        let failed = "        PASS [   0.1s] (1/9) dagq::it a::ok\n\
        FAIL [   4.9s] (2/9) dagq::it auth::denies\n\
     TIMEOUT [ 600.0s] (3/9) dagq::it slow::hangs\n\
        FAIL [   1.2s] (4/9) dagq::it auth::allows\n";
        let log = integrate_verify_log(dir, 1, 1);
        files.write(&log, failed.as_bytes()).unwrap();
        let first = step.run(&queue, 1, &log, None).unwrap();
        let failure = first.failure.clone().expect("failed");
        assert_eq!(failure.class, verify_failure::FailureClass::TestFailure);
        // Not the host's: the landing does not retry it nor hold the run.
        assert!(!failure.class.is_environmental());
        assert_eq!(
            failure.evidence,
            "FAIL [   4.9s] (2/9) dagq::it auth::denies"
        );
        let events = queue.events.borrow().clone();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].kind, EventKind::VerificationCommand);
        let payload = &events[0].payload;
        assert_eq!(payload["failure"], failure.to_json());
        assert_eq!(
            payload["failed_tests"],
            json!(["auth::denies", "auth::allows"])
        );
        assert_eq!(payload["failed_tests_omitted"], 0);
        assert_eq!(payload["limit_secs"], 3600);
        assert_eq!(
            (payload["exit_code"].clone(), payload["signal"].clone()),
            (json!(128), json!(9))
        );
        let Verdict::Deferred { reason, detail } = verification_failed(
            &sha(BASE),
            &sha(HEAD),
            planned.command.as_str(),
            1,
            &first,
            &failure,
        ) else {
            panic!("not deferred");
        };
        assert_eq!(detail["code"], "verification_failed");
        assert_eq!(detail["limit_secs"], 3600);
        assert_eq!(
            detail["failed_tests"],
            json!(["auth::denies", "auth::allows"])
        );
        assert!(
            reason.ends_with(
                ". It ran past its 3600 s limit and was killed, after these tests had failed (not out of time): auth::denies, auth::allows"
            ),
            "{reason}"
        );

        // Only out of time: the host's timeout, retried; the retry that
        // shows a failed test is the code's as the first run would be.
        let hung = "     TIMEOUT [ 600.0s] (3/9) dagq::it slow::hangs\n";
        let log = integrate_verify_log(dir, 2, 1);
        files.write(&log, hung.as_bytes()).unwrap();
        let timed_out = step.run(&queue, 1, &log, None).unwrap();
        let failure = timed_out.failure.clone().expect("failed");
        assert_eq!(
            failure,
            verify_failure::CommandTimedOut { limit_secs: 3600 }.failure()
        );
        assert!(failure.class.is_environmental());
        assert_eq!(queue.events.borrow()[1].payload["limit_secs"], 3600);
        assert_eq!(timed_out.tests_json(), json!(["slow::hangs"]));
        let retry_log = integrate_retry_log(dir, 2, 1);
        files.write(&retry_log, failed.as_bytes()).unwrap();
        let retried = step.run(&queue, 1, &retry_log, Some(&timed_out)).unwrap();
        assert_eq!(
            retried.failure.as_ref().map(|failure| failure.class),
            Some(verify_failure::FailureClass::TestFailure)
        );
        let payload = queue.events.borrow()[2].payload.clone();
        assert_eq!(payload["retry"], true);
        assert_eq!(payload["retry_of"]["failure"]["class"], "timeout");
        assert_eq!(
            payload["failed_tests"],
            json!(["auth::denies", "auth::allows"])
        );
        // A failure not killed at the limit names no limit.
        assert_eq!(checked(Some(101), None, "").limit_secs, None);
    }

    #[test]
    fn a_command_failing_on_the_host_again_holds_the_run_for_a_person() {
        let first = checked(None, Some(15), "");
        let last = judge_command(
            &Toolchains::this_repository(),
            "cargo test",
            None,
            Some(15),
            None,
            "",
            Path::new("/runs/run/integrate-1-verify-2-retry.log"),
        );
        let task_run = run(Path::new(DIR));
        let Verdict::Held { reason, detail } = held(
            &task_run,
            &sha(BASE),
            &sha(HEAD),
            "cargo test",
            2,
            &last,
            Some(&first),
            None,
        ) else {
            panic!("not held");
        };
        assert_eq!(
            reason,
            format!(
                "verification command \"cargo test\" failed on the host after the rebase onto {BASE} (killed: killed by signal 15 (SIGTERM)); see /runs/run/integrate-1-verify-2.log and /runs/run/integrate-1-verify-2-retry.log. it failed so again when retried once; fix the host (free disk space, a lighter load), then land it with dagq integrate 7; no session is resumed"
            )
        );
        let killed = json!({"class": "killed", "evidence": "killed by signal 15 (SIGTERM)"});
        assert_eq!(
            detail,
            json!({
                "code": "verification_environment",
                "index": 2,
                "main": BASE,
                "head": HEAD,
                "command": "cargo test",
                "exit_code": 128,
                "signal": 15,
                "failure": killed,
                "log_path": "/runs/run/integrate-1-verify-2-retry.log",
                "retried": true,
                "first_failure": killed,
                "first_log_path": "/runs/run/integrate-1-verify-2.log",
                "disk": null,
            })
        );
    }

    /// ADR-t3344-1: a verification a handoff's drain does not do again,
    /// a host's failure or only flaky tests, is handed over with what
    /// failed, where its log is and why it was not verified again.
    #[test]
    fn a_verification_not_done_again_while_handing_off_is_handed_over() {
        let killed = checked(None, Some(15), "");
        let flaky = checked(
            Some(100),
            None,
            "  TRY 1 FAIL [   0.010s] (───) dagq::it runtime_x::flaky\n\
             \x20 TRY 2 PASS [   0.007s] (1/1) dagq::it runtime_x::flaky\n\
             \x20FLKY-FL 2/2 [   0.007s] (1/1) dagq::it runtime_x::flaky\n\
             error: test run failed\n",
        );
        for (last, class) in [(&killed, "killed"), (&flaky, "flaky")] {
            let failure = last.failure.clone().expect("failed");
            assert_eq!(failure.class.as_str(), class);
            let Verdict::HandedOver { reason, detail } =
                handed_over(&sha(BASE), &sha(HEAD), "cargo test", (2, 1), last, &failure)
            else {
                panic!("not handed over");
            };
            assert_eq!(
                reason,
                format!(
                    "verification command \"cargo test\" failed after the rebase onto {BASE} ({class}); see /runs/run/integrate-1-verify-2.log. It would be verified once more, but the supervisor drains for a handoff: the landing is handed over, its lease kept, and the next process lands it again"
                )
            );
            assert_eq!(
                detail,
                json!({
                    "main": BASE,
                    "head": HEAD,
                    "attempt": 1,
                    "index": 2,
                    "command": "cargo test",
                    "exit_code": last.exit_code,
                    "failure": failure.to_json(),
                    "failed_tests": last.tests_json(),
                    "flaky_tests": last.flaky_tests,
                    "log_path": "/runs/run/integrate-1-verify-2.log",
                })
            );
        }
        assert_eq!(flaky.flaky_tests, ["runtime_x::flaky"]);
        assert!(killed.flaky_tests.is_empty());
    }

    /// Only a supervisor that drains for a handoff and is not asked to
    /// stop hands a landing over ([`Integration::handing_off`]): none, one
    /// not set, or one a stop came over (ADR-t3344-1 decision 3) verifies
    /// again, the stop read as it is set, before the pass sets the flag.
    #[test]
    fn only_a_set_handoff_flag_with_no_stop_hands_a_landing_over() {
        use std::sync::atomic::{AtomicBool, Ordering};
        assert!(!drains_for_handoff(None));
        for (handing_off, stop, hands_over) in [
            (false, false, false),
            (false, true, false),
            (true, false, true),
            (true, true, false),
        ] {
            let (handing_off, stop) = (AtomicBool::new(handing_off), AtomicBool::new(stop));
            let drain = HandoffDrain {
                handing_off: &handing_off,
                stop: &stop,
            };
            assert_eq!(drains_for_handoff(Some(drain)), hands_over);
        }
        let (handing_off, stop) = (AtomicBool::new(true), AtomicBool::new(false));
        let drain = HandoffDrain {
            handing_off: &handing_off,
            stop: &stop,
        };
        assert!(drains_for_handoff(Some(drain)));
        stop.store(true, Ordering::SeqCst);
        assert!(!drains_for_handoff(Some(drain)));
    }

    /// A person's `integrate` (task 638) and the retry of a command that
    /// failed on a full disk refuse below what a landing's verification
    /// needs as the supervisor judges it (ADR-t4473-1): the reserve with no
    /// run moving or no expectation, else the reserve and the expectation,
    /// with no factor. With room or no reading it lands.
    #[test]
    fn a_persons_integrate_refuses_below_what_a_landing_needs_only() {
        const GIB: u64 = 1 << 30;
        let state = |free: Option<u64>, expected: Option<u64>, moving| DiskState {
            free,
            total: 0,
            expected,
            integrator_expected: expected,
            runs: vec![
                crate::domain::disk::DiskRun {
                    stage: crate::domain::disk::DiskStage::Worker,
                    current: None
                };
                moving
            ],
            moving,
            landing_current: None,
            landing_waiting: false,
            relaxed: None,
        };
        let share = DiskConfig::default().reserve;
        let refusal = |state: DiskState| {
            let short = landing_short(&state, share).expect("short");
            disk_refusal(&RunId::new("r").unwrap(), short)
        };
        // The reserve (5 GiB of an unread volume) with nothing moving.
        let error = refusal(state(Some(GIB / 2), Some(8 * GIB), 0));
        assert!(
            error.contains("not enough free disk space to land run r"),
            "{error}"
        );
        assert!(error.contains("0.5 GiB free"), "{error}");
        assert!(error.contains("below the 5.0 GiB"), "{error}");
        assert!(error.contains("the reserve of 5.0 GiB"), "{error}");
        assert!(error.contains("and at least 5.0 GiB,"), "{error}");
        assert!(error.contains("dagq doctor"), "{error}");
        assert!(error.contains("not approved and is unchanged"), "{error}");
        assert!(!error.contains("factor"), "{error}");
        // With runs moving: the reserve and the expectation.
        let error = refusal(state(Some(12 * GIB), Some(8 * GIB), 2));
        assert!(error.contains("below the 13.0 GiB"), "{error}");
        assert!(error.contains("reservation from 8.0 GiB"), "{error}");
        let error = refusal(state(Some(GIB), None, 2));
        assert!(error.contains("reservation from none yet"), "{error}");
        // Room, nothing moving, no reading.
        assert!(landing_short(&state(Some(13 * GIB), Some(8 * GIB), 2), share).is_none());
        assert!(landing_short(&state(Some(5 * GIB), Some(8 * GIB), 0), share).is_none());
        assert!(landing_short(&state(Some(5 * GIB), None, 3), share).is_none());
        assert!(landing_short(&state(None, Some(8 * GIB), 2), share).is_none());
    }

    /// Start and disk-full retry use the same landing requirement: reserve
    /// alone with no other active run; otherwise the unused I, relaxed alike.
    #[test]
    fn the_supervisors_retry_uses_the_landings_progress_rule() {
        use crate::domain::{EventId, EventKind, RunId};
        const GIB: u64 = 1 << 30;
        let now = 1_793_664_000_000;
        let queue = crate::application::port_fakes::SupervisorsAndEvents::default();
        *queue.events.borrow_mut() = vec![RunEvent {
            id: EventId::new(1),
            task_id: None,
            goal_id: None,
            run_id: Some(RunId::new("00000000-0000-4000-8000-000000000001").unwrap()),
            kind: EventKind::BuildOutputsRemoved.as_str().into(),
            payload: json!({"bytes": 8 * GIB}),
            created_at: "2026-11-02T00:00:00Z".into(),
            actor: None,
        }];
        let config = DiskConfig::default();
        let short = |free: u64, moving: usize| {
            disk_short(
                &queue,
                &config,
                Some(free),
                None,
                Some(vec![
                    DiskRun {
                        stage: DiskStage::Worker,
                        current: None
                    };
                    moving
                ]),
                now,
                (None, None),
            )
            .unwrap()
        };
        // No other run active: the landing and its retry both need only reserve.
        assert!(short(5 * GIB, 0).is_none());
        assert_eq!(short(5 * GIB - 1, 0).map(|short| short.need), Some(5 * GIB));
        // Another run moving: the reserve and the expectation.
        let one = short(12 * GIB, 1).expect("short");
        assert_eq!((one.need, one.expected), (13 * GIB, Some(8 * GIB)));
        assert!(short(13 * GIB, 1).is_none());
        // Under a relaxation in effect (ADR-t4727-1) the expectation it
        // names stands for the samples'; past its time it relaxes nothing.
        // The reserve is kept.
        let relaxed = |at: &str| RunEvent {
            id: EventId::new(2),
            run_id: None,
            kind: EventKind::DiskRelaxed.as_str().into(),
            payload: json!({"expected_bytes": 2 * GIB, "for_secs": 3600, "reason": "r"}),
            created_at: at.into(),
            ..queue.events.borrow()[0].clone()
        };
        let relaxation = relaxed("2026-11-03T00:00:00Z");
        queue.events.borrow_mut().push(relaxation);
        assert!(short(7 * GIB, 1).is_none());
        let under = short(7 * GIB - 1, 1).expect("short");
        assert_eq!((under.need, under.expected), (7 * GIB, Some(2 * GIB)));
        assert_eq!(under.relaxed.map(|relaxed| relaxed.event_id), Some(2));
        let run = RunId::new("00000000-0000-4000-8000-000000000001").unwrap();
        assert!(disk_refusal(&run, under).contains("relaxed until"));
        assert_eq!(short(5 * GIB - 1, 0).map(|short| short.need), Some(5 * GIB));
        let past = relaxed("2026-11-02T22:00:00Z");
        queue.events.borrow_mut()[1] = past;
        assert_eq!(short(12 * GIB, 1).map(|short| short.need), Some(13 * GIB));
    }

    #[test]
    fn human_integration_uses_the_fixed_expectation_and_relaxation_without_samples() {
        use crate::domain::disk::DiskEstimate;
        for phase in [
            "environment_placement",
            "environment_image_pull",
            "environment_preparation",
            "environment_result_return",
            "resume",
            "revise",
        ] {
            assert_eq!(
                crate::domain::disk::disk_usage(
                    crate::domain::run_phase::Phase::from_name(phase).unwrap(),
                    true,
                    true
                )
                .stage,
                DiskStage::Worker
            );
        }
        assert_eq!(
            crate::domain::disk::disk_usage(crate::domain::run_phase::Phase::Review, true, true)
                .stage,
            DiskStage::Waiting
        );
        assert_eq!(
            crate::domain::disk::disk_usage(crate::domain::run_phase::Phase::Landing, true, true)
                .stage,
            DiskStage::Integrator
        );
        const GIB: u64 = 1 << 30;
        let config = DiskConfig {
            estimate: DiskEstimate::Fixed,
            run_size: Some(20 * GIB),
            ..DiskConfig::default()
        };
        let queue = crate::application::port_fakes::SupervisorsAndEvents::default();
        let now = 1_793_664_000_000;
        let short = |free, moving| {
            disk_short(
                &queue,
                &config,
                Some(free),
                None,
                Some(vec![
                    DiskRun {
                        stage: DiskStage::Worker,
                        current: None
                    };
                    moving
                ]),
                now,
                (None, None),
            )
            .unwrap()
        };
        assert!(short(25 * GIB, 1).is_none());
        let held = short(25 * GIB - 1, 1).unwrap();
        assert_eq!(held.need, 25 * GIB);
        assert_eq!(held.expected, Some(20 * GIB));
        assert_eq!(held.needs.integrator_expected, Some(20 * GIB));
        let separate = DiskConfig {
            landing_size: Some(30 * GIB),
            ..config
        };
        let measured = disk_short(
            &queue,
            &separate,
            Some(21 * GIB - 1),
            None,
            Some(vec![DiskRun {
                stage: DiskStage::Worker,
                current: None,
            }]),
            now,
            (None, Some(14 * GIB)),
        )
        .unwrap()
        .unwrap();
        assert_eq!(measured.need, 21 * GIB);
        assert_eq!(measured.needs.estimate, DiskEstimate::Fixed);
        assert!(disk_refusal(&RunId::new("r").unwrap(), measured).contains("I 30.0 GiB"));
        assert!(
            disk_short(
                &queue,
                &separate,
                Some(21 * GIB),
                None,
                Some(vec![DiskRun {
                    stage: DiskStage::Worker,
                    current: None
                }]),
                now,
                (None, Some(14 * GIB))
            )
            .unwrap()
            .is_none()
        );
        assert!(short(5 * GIB, 0).is_none());
        queue.events.borrow_mut().push(RunEvent {
            id: crate::domain::EventId::new(1),
            task_id: None,
            goal_id: None,
            run_id: None,
            kind: EventKind::DiskRelaxed.as_str().into(),
            payload: json!({"expected_bytes": 2 * GIB, "for_secs": 3600, "reason": "r"}),
            created_at: "2026-11-03T00:00:00Z".into(),
            actor: None,
        });
        assert!(short(7 * GIB, 1).is_none());
        assert_eq!(short(7 * GIB - 1, 1).unwrap().expected, Some(2 * GIB));
        assert_eq!(short(5 * GIB - 1, 0).unwrap().need, 5 * GIB);
    }

    #[test]
    fn a_command_failing_on_a_full_disk_is_held_without_a_retry() {
        let last = checked(Some(101), None, "No space left on device (os error 28)\n");
        let short = DiskShort {
            free: 1 << 30,
            need: 10 << 30,
            reserve: 5 << 30,
            expected: Some(5 << 30),
            relaxed: None,
            needs: crate::domain::disk::DiskNeeds::default(),
        };
        let Verdict::Held { reason, detail } = held(
            &run(Path::new(DIR)),
            &sha(BASE),
            &sha(HEAD),
            "cargo test",
            1,
            &last,
            None,
            Some(short),
        ) else {
            panic!("not held");
        };
        assert!(
            reason.contains("; see /runs/run/integrate-1-verify-2.log. it was not retried: 1.0 GiB free, below the 10.0 GiB a landing's verification needs; free disk space (dagq doctor lists the runs and their worktrees), then land it with dagq integrate 7"),
            "{reason}"
        );
        assert_eq!(detail["retried"], false);
        assert_eq!(detail["first_failure"], Value::Null);
        assert_eq!(detail["first_log_path"], Value::Null);
        assert_eq!(detail["disk"], short.to_json());
        assert_eq!(detail["code"], "verification_environment");
        // Without a classified failure, the class is unknown.
        let passed = checked(Some(0), None, "");
        let Verdict::Held { detail, .. } = held(
            &run(Path::new(DIR)),
            &sha(BASE),
            &sha(HEAD),
            "cargo test",
            1,
            &passed,
            None,
            None,
        ) else {
            panic!("not held");
        };
        assert_eq!(
            detail["failure"],
            json!({"class": "unknown", "evidence": ""})
        );
    }

    /// Integrate's verification logs are numbered per attempt; a run
    /// directory with the name used before (`integrate-verify-N.log`) is
    /// still read, as the attempt before the numbered ones, and the review
    /// names the latest attempt's.
    #[test]
    fn integrate_logs_are_kept_per_attempt_and_old_names_are_read() {
        use crate::application::review::review_logs_hint;
        let (files, run_dir) = (MemoryFiles::default(), Path::new(DIR));
        assert_eq!(next_integrate_attempt(&files, run_dir), 1);
        assert_eq!(integrate_logs(&files, run_dir), (vec![], vec![]));
        assert_eq!(
            review_logs_hint(&files, Some(DIR)),
            format!(
                "{DIR}/integrate-<attempt>-verify-N.log (one set per integrate attempt, written when integrate runs the verification commands after its rebase); none yet"
            )
        );
        assert_eq!(review_logs_hint(&files, None), "(no run directory)");
        for name in [
            "integrate-verify-1.log",
            "integrate-verify-2.log",
            "verify-1.log",
            "integrate-x-verify-1.log",
            "integrate-verify-y.log",
            "notes.txt",
        ] {
            files.write(&run_dir.join(name), name.as_bytes()).unwrap();
        }
        assert_eq!(
            integrate_logs(&files, run_dir),
            (
                vec![
                    run_dir.join("integrate-verify-1.log"),
                    run_dir.join("integrate-verify-2.log")
                ],
                vec![]
            )
        );
        assert_eq!(next_integrate_attempt(&files, run_dir), 1);
        assert_eq!(
            integrate_verify_log(run_dir, 1, 2),
            run_dir.join("integrate-1-verify-2.log")
        );
        assert_eq!(
            integrate_retry_log(run_dir, 1, 2),
            run_dir.join("integrate-1-verify-2-retry.log")
        );
        for name in [
            "integrate-1-verify-1.log",
            "integrate-10-verify-2.log",
            "integrate-10-verify-10.log",
            "integrate-2-verify-1.log",
        ] {
            files.write(&run_dir.join(name), name.as_bytes()).unwrap();
        }
        let (latest, earlier) = integrate_logs(&files, run_dir);
        assert_eq!(
            latest,
            vec![
                run_dir.join("integrate-10-verify-2.log"),
                run_dir.join("integrate-10-verify-10.log")
            ]
        );
        assert_eq!(
            earlier,
            vec![
                run_dir.join("integrate-verify-1.log"),
                run_dir.join("integrate-verify-2.log"),
                run_dir.join("integrate-1-verify-1.log"),
                run_dir.join("integrate-2-verify-1.log")
            ]
        );
        assert_eq!(next_integrate_attempt(&files, run_dir), 11);
        assert!(review_logs_hint(&files, Some(DIR)).ends_with(&format!(
            "latest attempt: {}, {}",
            run_dir.join("integrate-10-verify-2.log").display(),
            run_dir.join("integrate-10-verify-10.log").display()
        )));
        assert_eq!(next_integrate_attempt(&files, &run_dir.join("missing")), 1);
    }
}

#[cfg(test)]
mod sccache_tests {
    use super::*;
    use crate::application::sccache::LookedAt;
    use crate::domain::sccache::REFUSED_ERROR_LOG;

    fn run_env() -> Vec<(String, String)> {
        vec![
            ("RUSTC_WRAPPER".to_owned(), "sccache".to_owned()),
            ("CARGO_BUILD_JOBS".to_owned(), "4".to_owned()),
        ]
    }

    fn get<'a>(env: &'a [(String, String)], key: &str) -> Option<&'a str> {
        env.iter()
            .find(|(name, _)| name == key)
            .map(|(_, value)| value.as_str())
    }

    /// The landing's verification on the host has the run's short
    /// temporary files' directory, made for it, as its `TMPDIR`, over the
    /// one `[run.env]` names; the rest of the `[run.env]` stays.
    #[test]
    fn the_verification_on_the_host_keeps_its_temporary_files_in_the_runs_short_directory() {
        let files = crate::application::memory_files::MemoryFiles::default();
        let run = RunId::new("7804dfef-4b2c-4c3a-98db-7353826b7849").unwrap();
        let mut env = run_env();
        env.push(("TMPDIR".to_owned(), "/tmp/person".to_owned()));
        let tmp = host_verification_tmpdir(&files, &run, &mut env).unwrap();
        assert_eq!(get(&env, "TMPDIR"), Some("/tmp/dagq-v-7804dfef4b2c"));
        assert_eq!(env.iter().filter(|(key, _)| key == "TMPDIR").count(), 1);
        assert_eq!(get(&env, "CARGO_BUILD_JOBS"), Some("4"));
        assert!(files.is_dir(Path::new("/tmp/dagq-v-7804dfef4b2c")));
        drop(tmp);
    }

    /// At the verification's end its `TMPDIR` goes when nothing is left in
    /// it, and stays, for the run's cleanup to count, when something is.
    #[test]
    fn the_verification_tmpdir_goes_at_its_end_only_when_empty() {
        let files = crate::application::memory_files::MemoryFiles::default();
        let run = RunId::new("7804dfef-4b2c-4c3a-98db-7353826b7849").unwrap();
        let dir = super::super::host_verification_tmp_path(&run);
        drop(host_verification_tmpdir(&files, &run, &mut Vec::new()).unwrap());
        assert!(!files.is_dir(&dir));
        let tmp = host_verification_tmpdir(&files, &run, &mut Vec::new()).unwrap();
        files.write(&dir.join("fixture"), b"x").unwrap();
        drop(tmp);
        assert!(files.is_dir(&dir));
        assert!(files.is_file(&dir.join("fixture")));
    }

    /// However deep the queue's directory, the verification's `TMPDIR` is
    /// as short, and nowhere under the queue's `runs/`: a socket its tests
    /// make there fits in `SUN_LEN`, and a test looking above it for a
    /// `runs/` finds none.
    #[test]
    fn the_verification_tmpdir_is_short_and_outside_runs_however_deep_the_queue() {
        let files = crate::application::memory_files::MemoryFiles::default();
        let deep = format!("/{}/runs/r", ["deep"; 40].join("/"));
        let run = RunId::new("2f1e0c3b-8a9d-4e7f-b6c5-d4e3f2a1b0c9").unwrap();
        let mut env = vec![("TMPDIR".to_owned(), format!("{deep}/tmp"))];
        let _tmp = host_verification_tmpdir(&files, &run, &mut env).unwrap();
        let tmp = Path::new(get(&env, "TMPDIR").unwrap());
        assert!(
            tmp.as_os_str().len() <= "/tmp/dagq-v-".len() + 12,
            "{tmp:?}"
        );
        assert!(!tmp.starts_with(&deep));
        assert!(tmp.components().all(|part| part.as_os_str() != "runs"));
        assert!(files.is_dir(tmp));
    }

    /// What stands in the place of the verification's `TMPDIR` (a file, a
    /// link) is removed, not followed; a directory there is kept.
    #[test]
    fn the_verification_tmpdir_replaces_what_is_not_a_directory() {
        let files = crate::application::memory_files::MemoryFiles::default();
        let run = RunId::new("run-with-a-file-there").unwrap();
        let tmp = super::super::host_verification_tmp_path(&run);
        assert_eq!(tmp, Path::new("/tmp/dagq-v-runwithafile"));
        files.write(&tmp, b"x").unwrap();
        let mut env = Vec::new();
        let first = host_verification_tmpdir(&files, &run, &mut env).unwrap();
        assert!(files.is_dir(&tmp));
        let _again = host_verification_tmpdir(&files, &run, &mut env).unwrap();
        assert!(files.is_dir(&tmp));
        drop(first);
    }

    #[test]
    fn the_verification_is_refused_the_servers_start_and_guarded_when_it_listens() {
        let run_dir = Path::new("/q/runs/r");
        let mut env = run_env();
        let server = LookedAt::new(Ok(true), true);
        assert_eq!(guard_verification(Some(&server), run_dir, &mut env), None);
        assert_eq!(
            get(&env, "RUSTC_WRAPPER"),
            Some("/q/runs/r/dagq-rustc-wrapper")
        );
        assert_eq!(get(&env, "DAGQ_SCCACHE_PROGRAM"), Some("sccache"));
        assert_eq!(get(&env, "SCCACHE_ERROR_LOG"), Some(REFUSED_ERROR_LOG));
        assert_eq!(get(&env, "CARGO_BUILD_JOBS"), Some("4"));

        for server in [
            LookedAt::new(Ok(false), true),
            LookedAt::new(Ok(true), false),
        ] {
            let mut env = run_env();
            let (port, why) = guard_verification(Some(&server), run_dir, &mut env).unwrap();
            assert_eq!(port, crate::domain::sccache::DEFAULT_PORT);
            assert!(!why.is_empty());
            assert_eq!(get(&env, "RUSTC_WRAPPER"), None);
            assert_eq!(get(&env, "SCCACHE_ERROR_LOG"), Some(REFUSED_ERROR_LOG));
        }

        // Nothing to look with, or no sccache named: as it is.
        let mut env = run_env();
        assert_eq!(guard_verification(None, run_dir, &mut env), None);
        assert_eq!(env, run_env());
        let mut other = vec![("CARGO_BUILD_JOBS".to_owned(), "4".to_owned())];
        let server = LookedAt::new(Ok(false), true);
        assert_eq!(guard_verification(Some(&server), run_dir, &mut other), None);
        assert_eq!(server.looks.get(), 0);
    }
}
