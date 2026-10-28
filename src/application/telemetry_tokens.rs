//! The supervisor's issue and revocation of the telemetry token of a run's
//! executor (ADR-t1662-3 decision 11, docs/design/measurement.md "送る口"):
//! issued at the claim, at each resume and when a supervisor adopts the
//! run (each revokes the one before), under the lease (the supervisor's
//! token) of the supervisor that issues it, and again when a supervisor
//! starts an executor on a run that holds none issued under its lease
//! ([`credential_for`]: a run that waited with no lease had its token
//! revoked once the supervisor that issued it ended). Revoked once the run
//! ended, and once that lease was lost: by the supervisor that lost it,
//! when it sees its heartbeat fail or the run's lease gone or taken, and by
//! the sweep of any supervisor, which reads the lease from the queue
//! ([`run_token_due`]). A revocation for a lost lease takes only the tokens
//! issued under it, never the one the adopting supervisor issued since.
//! Only the control side calls these, through [`TelemetryTokens`]; no
//! command of an AI actor's does. The executor of the run (its session
//! wrapper, and the review and recovery jobs and the landing's and the
//! e2e's verification on it) finds the token's file by the run
//! ([`TelemetryTokens::telemetry_credential`]), outside the worktree and
//! the run directory, and no agent is given it.
//!
//! The run's events record each issue (`telemetry_token_issued`) and each
//! revocation (`telemetry_token_revoked`), each only once it succeeded: an
//! issue that failed is no revocation, and a revocation that failed left a
//! token that may still authenticate, which [`TelemetryTokens`] still lists
//! with its lease and a later look revokes, recording it then: the sweep
//! for a token it finds due, and the supervisor's own [`Retries`] for one
//! the sweep would keep (the token before a resume, under the same live
//! lease). They outlive the exec of a handoff, which keeps the registration
//! and every lease ([`Retries::save`], [`Retries::take`]). A token is
//! telemetry, so nothing here changes a run's outcome: a failure is logged.
//!
//! A target of no run (ADR-t1662-3 decisions 10 and 11) is issued its
//! token by the queue service, at the request of the control side that
//! starts its agent (ADR-t3102-1 decision 2, docs/design/queue-service.md
//! "principalとtoken"), through [`TargetTelemetry`]: the supervisor as it
//! opens a planner of the runtime's and as it starts a plan review, a goal
//! review or an eval job, and the observation's and the throughput
//! review's own commands once their start is recorded. A plan or goal
//! review's and a command's are revoked by the one that asked for them at
//! the target's end (the job's, the
//! command's finish, or a launch that closed its planner), and the
//! supervisor's sweep revokes the tokens of
//! the targets the queue says ended or whose runner is gone
//! ([`sweep_targets`]): a closed planner, a job's finished row or a job
//! whose supervisor is gone, a command's finish, its process gone or past
//! the time it ends by. The queue service refuses a token of a target so
//! ended at once, revoked yet or not. The queue's events record their issue
//! and revocation, each only once it succeeded. Their file is under the
//! queue's `service/`, outside the planner's directory and the job's, and
//! no agent is given it.
//!
//! The control side also registers each session it starts, or learns of,
//! to its target ([`register_session`], ADR-t1662-3 decision 10): the
//! queue service records a report only for a session registered to the
//! token's target. A session the runtime names ahead (Claude's
//! `--session-id`, a worker's by its run and its session starts) is
//! registered before its agent starts; one its agent names (a Codex
//! thread) once its events or its job's output name it, and what the
//! executor sent before is refused and sent again. A registration is
//! telemetry too: one that failed is logged and changes no outcome.

use std::path::{Path, PathBuf};

use anyhow::Result;
use serde::{Deserialize, Serialize};
use serde_json::json;
use tracing::warn;

use super::queue_service::TelemetryTokens;
use super::{EventStore, RunFiles};
use crate::domain::session_step::StepTarget;
use crate::domain::session_step::TargetKind;
use crate::domain::telemetry::{
    HeldTelemetry, KeptRun, RunTokenState, TargetFacts, TokenRevocation, kept_steps_due, run_ended,
    run_token_due, target_token_due,
};
use crate::domain::{EventKind, LeaseToken, RunEvent, RunId, TaskId};

/// Where the issue and the revocation are recorded: the run's events, and
/// the queue's for a target of no run. Every [`EventStore`] is one.
pub trait TokenLog {
    /// Record `kind` with `payload` on `run`.
    fn record_token_event(
        &self,
        run: &RunId,
        kind: EventKind,
        payload: serde_json::Value,
    ) -> Result<()>;
    /// Record `kind` with `payload` on the queue (a target of no run's).
    fn record_queue_token_event(&self, kind: EventKind, payload: serde_json::Value) -> Result<()>;
}

impl<T: EventStore + ?Sized> TokenLog for T {
    fn record_token_event(
        &self,
        run: &RunId,
        kind: EventKind,
        payload: serde_json::Value,
    ) -> Result<()> {
        self.record_runtime_event(run, kind, payload)
    }
    fn record_queue_token_event(&self, kind: EventKind, payload: serde_json::Value) -> Result<()> {
        self.record_queue_event(kind, payload).map(drop)
    }
}

/// The issue and the revocation of the token of a target of no run by the
/// control side that starts its agent: the tokens of the queue at `db`,
/// issued under `lease` (the supervisor's token; `None` from a command,
/// whose token the target's end alone ends).
#[derive(Clone, Copy)]
pub struct TargetTelemetry<'a> {
    pub tokens: &'a dyn TelemetryTokens,
    pub db: &'a Path,
    pub lease: Option<&'a LeaseToken>,
}

impl TargetTelemetry<'_> {
    /// Issue the token of `target`, a target of no run, as its agent is
    /// started (a command's only while the target holds none, the
    /// supervisor's revoking any it held), and record it on the queue
    /// (`telemetry_token_issued`) once it was issued. The file its executor
    /// reads it from; `None` when it could not be issued: the target goes
    /// on without telemetry, as its outcome does not hang on it.
    pub fn issue<L: TokenLog + ?Sized>(&self, log: &L, target: &StepTarget) -> Option<PathBuf> {
        match self
            .tokens
            .issue_telemetry(self.db, target, None, self.lease)
        {
            Ok(file) => {
                record_queue(
                    log,
                    EventKind::TelemetryTokenIssued,
                    json!({"target": target}),
                );
                Some(file)
            }
            Err(error) => {
                warn!(
                    "the telemetry token of {} {} could not be issued; its executor sends no telemetry: {error:#}",
                    target.kind.as_str(),
                    target.id
                );
                None
            }
        }
    }

    /// Register `session_id` as `target`'s ([`register_session`]): before
    /// its agent starts for a session named ahead, once it is read for one
    /// its agent named. `None` registers nothing.
    pub fn register(&self, target: &StepTarget, session_id: Option<&str>) {
        if let Some(session_id) = session_id {
            register_session(self.tokens, self.db, target, session_id);
        }
    }

    /// Revoke the token of `target` at its end ([`revoke_target`]). One
    /// that cannot be revoked is logged: the service refuses it from the
    /// end on, and the sweep revokes it ([`sweep_targets`]).
    pub fn end<L: TokenLog + ?Sized>(&self, log: &L, target: &StepTarget, why: TokenRevocation) {
        if let Err(error) = revoke_target(self.tokens, log, self.db, target, why) {
            warn!(
                "the telemetry token of {} {} could not be revoked at its end; the sweep tries again: {error:#}",
                target.kind.as_str(),
                target.id
            );
        }
    }
}

/// Register `session_id` as `target`'s in the queue at `db`, as the
/// control side starts or learns of the session: whether it is
/// registered. One that failed (the queue locked, a session of another
/// target) is logged: the executor's reports of it are refused, and the
/// target's outcome does not change.
pub fn register_session(
    tokens: &dyn TelemetryTokens,
    db: &Path,
    target: &StepTarget,
    session_id: &str,
) -> bool {
    try_register(tokens, db, target, session_id).is_ok()
}

/// [`register_session`]: what the registration returned, logged when it
/// failed.
fn try_register(
    tokens: &dyn TelemetryTokens,
    db: &Path,
    target: &StepTarget,
    session_id: &str,
) -> Result<()> {
    tokens
        .register_telemetry_session(db, target, session_id)
        .inspect_err(|error| {
            warn!(
                "session {session_id} could not be registered to {} {}; its executor's reports are refused: {error:#}",
                target.kind.as_str(),
                target.id
            );
        })
}

/// Register to `run` each of its sessions its `events` tell of
/// ([`crate::domain::telemetry::run_sessions`]) that is not in
/// `registered`, the sessions this process registered already, which
/// takes each one registered now and each one another target holds (never
/// the run's, so not asked again). At the claim, a resume, an adoption, a
/// switch or a renewal before the next turn starts, and at every pass for
/// a thread a turn or a job's end named since.
pub fn register_run_sessions(
    tokens: &dyn TelemetryTokens,
    db: &Path,
    run: &RunId,
    events: &[RunEvent],
    registered: &mut std::collections::HashSet<String>,
) {
    let target = StepTarget::run(run);
    for session in crate::domain::telemetry::run_sessions(run.as_str(), events) {
        if registered.contains(&session) {
            continue;
        }
        let done = match try_register(tokens, db, &target, &session) {
            Ok(()) => true,
            Err(error) => matches!(
                error.downcast_ref::<crate::domain::session_step::StepError>(),
                Some(crate::domain::session_step::StepError::SessionOfAnotherTarget)
            ),
        };
        if done {
            registered.insert(session);
        }
    }
}

/// Revoke the token of `target`, a target of no run, for `why`, and record
/// it on the queue (`telemetry_token_revoked`) once the revocation
/// succeeded and there was one. Whether there was one; an error records
/// nothing and may leave the token, still listed for the sweep.
pub fn revoke_target<L: TokenLog + ?Sized>(
    tokens: &dyn TelemetryTokens,
    log: &L,
    db: &Path,
    target: &StepTarget,
    why: TokenRevocation,
) -> Result<bool> {
    let revoked = tokens.revoke_telemetry(db, target, None)?;
    if revoked {
        record_queue(
            log,
            EventKind::TelemetryTokenRevoked,
            json!({"target": target, "reason": why.as_str()}),
        );
    }
    Ok(revoked)
}

/// The sweep of the tokens of the targets of no run: each listed whose
/// target `facts` reads as ended, or whose runner is gone by `alive`, at
/// `now_ms` ([`target_token_due`]), is revoked ([`revoke_target`]): the
/// targets revoked. One whose facts cannot be read, and one whose
/// revocation failed, stay listed for the next sweep. A run's token is
/// left to [`sweep`].
pub fn sweep_targets<L: TokenLog + ?Sized>(
    tokens: &dyn TelemetryTokens,
    log: &L,
    db: &Path,
    now_ms: i64,
    alive: &dyn Fn(u32) -> bool,
) -> Vec<StepTarget> {
    let held = match tokens.held_telemetry(db) {
        Ok(held) => held,
        Err(error) => {
            warn!(
                "the telemetry tokens could not be listed; the next sweep looks again: {error:#}"
            );
            return Vec::new();
        }
    };
    let mut revoked = Vec::new();
    for HeldTelemetry { target, .. } in held {
        if target.run_id().is_some() || revoked.contains(&target) {
            continue;
        }
        let (kind, id) = (target.kind.as_str(), &target.id);
        let facts = match tokens.target_facts(db, &target) {
            Ok(facts) => facts,
            Err(error) => {
                warn!("{kind} {id} could not be read for its telemetry token: {error:#}");
                continue;
            }
        };
        let Some(why) = target_token_due(target.kind, &facts, now_ms, alive) else {
            continue;
        };
        match revoke_target(tokens, log, db, &target, why) {
            Ok(true) => revoked.push(target),
            Ok(false) => {}
            Err(error) => {
                warn!(
                    "the telemetry token of {kind} {id} could not be revoked; the next sweep tries again: {error:#}"
                );
            }
        }
    }
    revoked
}

/// The revocations of one supervisor's that failed and that no sweep would
/// make: tried again on its next sweeps ([`sweep`]) until they succeed, or
/// until the run's token is no longer its to end. The process an exec hands
/// the registration to takes them over ([`Self::save`]).
#[derive(Debug, Default, Serialize, Deserialize)]
pub struct Retries {
    pending: Vec<Retry>,
}

/// What [`Retries::save`] writes: the revocations, and the registration
/// they are of.
#[derive(Serialize, Deserialize)]
struct Saved {
    token: LeaseToken,
    retries: Retries,
}

/// A revocation [`Retries`] holds.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum Retry {
    /// A resume or an adoption whose revocation of the token before
    /// failed: under the same live lease, [`run_token_due`] keeps that
    /// token. Once it is revoked, the run is issued another.
    Reissue { run: RunId, task: TaskId },
    /// A lost lease whose revocation failed while this supervisor goes on.
    LeaseLost(RunId),
}

impl Retry {
    fn run(&self) -> &RunId {
        match self {
            Self::Reissue { run, .. } | Self::LeaseLost(run) => run,
        }
    }
}

impl Retries {
    /// Whether a revocation of `run`'s is held.
    pub fn holds(&self, run: &RunId) -> bool {
        self.pending.iter().any(|retry| retry.run() == run)
    }

    fn forget(&mut self, run: &RunId) {
        self.pending.retain(|retry| retry.run() != run);
    }

    fn hold(&mut self, retry: Retry) {
        self.forget(retry.run());
        self.pending.push(retry);
    }

    /// The file under `queue_dir` the supervisor of the registration `token`
    /// keeps its revocations in across the exec of a handoff.
    pub fn file(queue_dir: &Path, token: &LeaseToken) -> PathBuf {
        let name: String = token
            .as_str()
            .chars()
            .filter(|c| c.is_ascii_alphanumeric() || *c == '-')
            .collect();
        queue_dir.join(format!("telemetry-retries-{name}.json"))
    }

    /// Keep these in `file` for the process an exec hands the registration
    /// `token` to (ADR-0045 decision 10): it keeps every lease, so no sweep
    /// would make them, and it takes them over ([`Self::take`]). Nothing is
    /// written while none is held.
    pub fn save(&self, files: &dyn RunFiles, file: &Path, token: &LeaseToken) -> Result<()> {
        if self.pending.is_empty() {
            return Ok(());
        }
        let saved = Saved {
            token: token.clone(),
            retries: Self {
                pending: self.pending.clone(),
            },
        };
        files.write(file, &serde_json::to_vec(&saved)?)?;
        Ok(())
    }

    /// The revocations the process that held the registration `token`
    /// before the exec kept in `file`, which is removed: none without the
    /// file, when it does not parse, or when another registration wrote it.
    pub fn take(files: &dyn RunFiles, file: &Path, token: &LeaseToken) -> Self {
        if !files.is_file(file) {
            return Self::default();
        }
        let bytes = files.read(file);
        if let Err(error) = files.remove_file(file) {
            warn!("{} could not be removed: {error}", file.display());
        }
        bytes
            .ok()
            .and_then(|bytes| serde_json::from_slice::<Saved>(&bytes).ok())
            .filter(|saved| saved.token == *token)
            .map(|saved| saved.retries)
            .unwrap_or_default()
    }
}

/// Issue the telemetry token of `run` of `task` under `lease` at its claim,
/// a resume or its adoption: every one it held is revoked first and, if
/// there was one, recorded (`reissued`), then the new one is issued and
/// recorded. The file the executor reads it from, `None` when it could not
/// be issued: the run goes on without telemetry. A revocation that failed
/// issues none, so the token before, still of the same run, is kept and
/// listed, and `retries` holds it: the next sweep revokes it and issues
/// another.
pub fn issue_run<L: TokenLog + ?Sized>(
    tokens: &dyn TelemetryTokens,
    log: &L,
    db: &Path,
    (run, task): (&RunId, TaskId),
    lease: &LeaseToken,
    retries: &mut Retries,
) -> Option<PathBuf> {
    retries.forget(run);
    let target = StepTarget::run(run);
    if let Err(error) = revoke_run(tokens, log, db, run, TokenRevocation::Reissued, None) {
        warn!(run_id = %run, "the telemetry token of run {run} could not be revoked to issue another; it keeps the one it held until the next sweep revokes it: {error:#}");
        retries.hold(Retry::Reissue {
            run: run.clone(),
            task,
        });
        return None;
    }
    match tokens.issue_telemetry(db, &target, Some(task), Some(lease)) {
        Ok(file) => {
            record(
                log,
                run,
                EventKind::TelemetryTokenIssued,
                json!({"target": target}),
            );
            Some(file)
        }
        Err(error) => {
            warn!(run_id = %run, "the telemetry token of run {run} could not be issued; its executor sends no telemetry: {error:#}");
            None
        }
    }
}

/// The file the executor of `run` of `task` reads its token from, as the
/// supervisor holding `lease` starts it on the run (a review, a recovery
/// job, the landing's or the e2e's verification): the token issued under
/// `lease` while it holds, else another issued now ([`issue_run`]). A run
/// that waited with no lease had its token revoked once the supervisor that
/// issued it ended, and one adopted holds the adopter's. `None` when none
/// could be issued.
pub fn credential_for<L: TokenLog + ?Sized>(
    tokens: &dyn TelemetryTokens,
    log: &L,
    db: &Path,
    (run, task): (&RunId, TaskId),
    lease: &LeaseToken,
    retries: &mut Retries,
) -> Option<PathBuf> {
    let target = StepTarget::run(run);
    let own = HeldTelemetry {
        target: target.clone(),
        issued_under: Some(lease.clone()),
    };
    // A revocation held for a resume tries again first.
    if !retries.holds(run)
        && tokens
            .held_telemetry(db)
            .is_ok_and(|held| held.contains(&own))
        && let Some(file) = tokens.telemetry_credential(db, &target)
    {
        return Some(file);
    }
    issue_run(tokens, log, db, (run, task), lease, retries)
}

/// Revoke the telemetry token of `run` for `why`, only the ones issued
/// under `under` when it is given (a lost lease's), and record it once the
/// revocation succeeded and there was one. Whether there was one; an error
/// records nothing and may leave the token, still listed for the sweep.
pub fn revoke_run<L: TokenLog + ?Sized>(
    tokens: &dyn TelemetryTokens,
    log: &L,
    db: &Path,
    run: &RunId,
    why: TokenRevocation,
    under: Option<&LeaseToken>,
) -> Result<bool> {
    let target = StepTarget::run(run);
    let revoked = tokens.revoke_telemetry(db, &target, under)?;
    if revoked {
        record(
            log,
            run,
            EventKind::TelemetryTokenRevoked,
            json!({"target": target, "reason": why.as_str()}),
        );
    }
    Ok(revoked)
}

/// Revoke the telemetry token of `run` issued under `lease`, which its
/// supervisor lost ([`revoke_run`]); a token another supervisor issued
/// since is left as it is. One that cannot be revoked is held in `retries`
/// for the next sweep, in place of any revocation held of the run.
pub fn revoke_lost<L: TokenLog + ?Sized>(
    tokens: &dyn TelemetryTokens,
    log: &L,
    db: &Path,
    run: &RunId,
    lease: &LeaseToken,
    retries: &mut Retries,
) {
    retries.forget(run);
    if let Err(error) = revoke_run(
        tokens,
        log,
        db,
        run,
        TokenRevocation::LeaseLost,
        Some(lease),
    ) {
        warn!(run_id = %run, "the telemetry token of run {run} could not be revoked with its lost lease; the next sweep tries again: {error:#}");
        retries.hold(Retry::LeaseLost(run.clone()));
    }
}

/// The sweep of the supervisor holding `lease`: first the revocations
/// `retries` holds, a resume's revoked and the run issued another while
/// the run is still its own ([`run_token_due`] finds nothing due of it under
/// `lease`), and a resume's whose run ended or went to another left to the
/// rest; then it revokes the run tokens of the list that are due by what
/// `states` reads of their run and the lease they were issued under: the
/// runs revoked there. A lost lease's revocation takes only the tokens
/// issued under it. A run whose state cannot be read, and a token whose
/// revocation failed, stay in the list for the next sweep. Targets of no
/// run are left alone.
pub fn sweep<L: TokenLog + ?Sized>(
    tokens: &dyn TelemetryTokens,
    log: &L,
    db: &Path,
    lease: &LeaseToken,
    retries: &mut Retries,
    states: &dyn Fn(&RunId, Option<&LeaseToken>) -> Result<RunTokenState>,
) -> Vec<RunId> {
    for retry in std::mem::take(&mut retries.pending) {
        match retry {
            Retry::LeaseLost(run) => revoke_lost(tokens, log, db, &run, lease, retries),
            Retry::Reissue { run, task } => match states(&run, Some(lease)) {
                Ok(state) if run_token_due(Some(lease), &state).is_none() => {
                    issue_run(tokens, log, db, (&run, task), lease, retries);
                }
                // Due by the list below, or the token of the supervisor
                // that holds the run now.
                Ok(_) => {}
                Err(error) => {
                    warn!(run_id = %run, "the state of run {run} could not be read for its telemetry token: {error:#}");
                    retries.hold(Retry::Reissue { run, task });
                }
            },
        }
    }
    let held = match tokens.held_telemetry(db) {
        Ok(held) => held,
        Err(error) => {
            warn!(
                "the telemetry tokens could not be listed; the next sweep looks again: {error:#}"
            );
            return Vec::new();
        }
    };
    let mut revoked = Vec::new();
    for token in held {
        let Some(run) = token.target.run_id() else {
            continue;
        };
        let issued_under = token.issued_under.as_ref();
        let state = match states(&run, issued_under) {
            Ok(state) => state,
            Err(error) => {
                warn!(run_id = %run, "the state of run {run} could not be read for its telemetry token: {error:#}");
                continue;
            }
        };
        let Some(why) = run_token_due(issued_under, &state) else {
            continue;
        };
        let under = (why == TokenRevocation::LeaseLost)
            .then_some(issued_under)
            .flatten();
        match revoke_run(tokens, log, db, &run, why, under) {
            Ok(true) => revoked.push(run),
            Ok(false) => {}
            Err(error) => {
                warn!(run_id = %run, "the telemetry token of run {run} could not be revoked; the next sweep tries again: {error:#}");
            }
        }
    }
    revoked
}

/// The sweep of the files the session wrappers kept their targets' steps
/// in ([`TelemetryTokens::kept_steps`]): each whose target ended by the
/// queue, a run's by what `run` reads of it ([`run_ended`]: a `failed` or
/// `interrupted` run's stays while recovery can put it back, so the
/// wrapper of its resume goes on numbering its sessions) and a target of
/// no run's by its facts, or that names none and is old
/// ([`kept_steps_due`]), is removed: the files removed. Its token does not
/// decide, so a file a wrapper wrote again after the end, or of a run
/// whose token went with a lost lease, goes at the next pass.
/// A target that cannot be read, and a removal that failed, stay for the
/// next pass.
pub fn sweep_kept_steps(
    tokens: &dyn TelemetryTokens,
    db: &Path,
    run: &dyn Fn(&RunId) -> Result<KeptRun>,
) -> Vec<PathBuf> {
    let kept = match tokens.kept_steps(db) {
        Ok(kept) => kept,
        Err(error) => {
            warn!(
                "the wrappers' kept steps could not be listed; the next pass looks again: {error:#}"
            );
            return Vec::new();
        }
    };
    let ended = |target: &StepTarget, age_ms: i64| {
        let ended = match (target.kind, target.run_id()) {
            (TargetKind::Run, Some(id)) => run(&id).map(|facts| run_ended(facts, age_ms)),
            // A run's file naming no run's ID is of no run there is.
            (TargetKind::Run, None) => Ok(true),
            _ => tokens
                .target_facts(db, target)
                .map(|facts| facts == TargetFacts::Ended),
        };
        ended
            .inspect_err(|error| {
                warn!(
                    "{} {} could not be read for its kept steps: {error:#}",
                    target.kind.as_str(),
                    target.id
                );
            })
            .unwrap_or(false)
    };
    let mut removed = Vec::new();
    for file in kept {
        if !kept_steps_due(&file, ended) {
            continue;
        }
        match tokens.forget_steps(db, &file.file) {
            Ok(()) => removed.push(file.file),
            Err(error) => warn!(
                "{} could not be removed; the next pass tries again: {error:#}",
                file.file.display()
            ),
        }
    }
    removed
}

/// Record `kind` on the queue; a record that cannot be written is logged.
fn record_queue<L: TokenLog + ?Sized>(log: &L, kind: EventKind, payload: serde_json::Value) {
    if let Err(error) = log.record_queue_token_event(kind, payload) {
        warn!(
            "{} could not be recorded on the queue: {error:#}",
            kind.as_str()
        );
    }
}

/// Record `kind` on `run`; a record that cannot be written is logged.
fn record<L: TokenLog + ?Sized>(log: &L, run: &RunId, kind: EventKind, payload: serde_json::Value) {
    if let Err(error) = log.record_token_event(run, kind, payload) {
        warn!(run_id = %run, "{} could not be recorded on run {run}: {error:#}", kind.as_str());
    }
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::collections::BTreeMap;
    use std::sync::Mutex;

    use anyhow::anyhow;

    use super::*;
    use crate::domain::RunStatus;
    use crate::domain::session_step::TargetKind;
    use crate::domain::telemetry::{Beat, HeldTelemetry, LeaseView, TargetFacts};

    /// The tokens held, each target with the lease it was issued under and
    /// how many times it was issued; a failure of a revocation or of an
    /// issue is injected by count, and leaves the tokens as they were.
    #[derive(Default)]
    struct Tokens {
        held: Mutex<BTreeMap<StepTarget, (Option<String>, u32)>>,
        issues: Mutex<u32>,
        failing_revocations: Mutex<u32>,
        failing_issues: Mutex<u32>,
        /// What the queue holds of each target of no run by its id; one
        /// not here has ended, and `unreadable` cannot be read.
        targets: Mutex<BTreeMap<String, TargetFacts>>,
        /// The sessions registered, each with its target, and how many
        /// registrations were asked.
        owners: Mutex<BTreeMap<String, StepTarget>>,
        registrations: Mutex<u32>,
        failing_registrations: Mutex<u32>,
        /// The wrappers' kept files there are.
        kept: Mutex<Vec<crate::domain::telemetry::KeptStepsFile>>,
    }

    impl Tokens {
        fn fail(count: &Mutex<u32>) -> bool {
            let mut left = count.lock().unwrap();
            let failing = *left > 0;
            *left = left.saturating_sub(1);
            failing
        }
        /// The lease and the number of the issue of `target`'s token.
        fn held(&self, target: &StepTarget) -> Option<(Option<String>, u32)> {
            self.held.lock().unwrap().get(target).cloned()
        }
    }

    impl TelemetryTokens for Tokens {
        fn issue_telemetry(
            &self,
            _: &Path,
            target: &StepTarget,
            _: Option<TaskId>,
            issued_under: Option<&LeaseToken>,
        ) -> Result<PathBuf> {
            if Self::fail(&self.failing_issues) {
                return Err(anyhow!("the token's random bytes could not be drawn"));
            }
            let mut issues = self.issues.lock().unwrap();
            *issues += 1;
            self.held.lock().unwrap().insert(
                target.clone(),
                (issued_under.map(|lease| lease.as_str().to_owned()), *issues),
            );
            Ok(format!("/q/service/credentials/{}", target.id).into())
        }
        fn revoke_telemetry(
            &self,
            _: &Path,
            target: &StepTarget,
            issued_under: Option<&LeaseToken>,
        ) -> Result<bool> {
            if Self::fail(&self.failing_revocations) {
                return Err(anyhow!("the token's file could not be read"));
            }
            let mut held = self.held.lock().unwrap();
            let matches = held.get(target).is_some_and(|(lease, _)| {
                issued_under.is_none_or(|under| lease.as_deref() == Some(under.as_str()))
            });
            Ok(matches && held.remove(target).is_some())
        }
        fn telemetry_credential(&self, _: &Path, target: &StepTarget) -> Option<PathBuf> {
            self.held(target)
                .map(|_| format!("/q/service/credentials/{}", target.id).into())
        }
        fn kept_steps(&self, _: &Path) -> Result<Vec<crate::domain::telemetry::KeptStepsFile>> {
            Ok(self.kept.lock().unwrap().clone())
        }
        fn forget_steps(&self, _: &Path, file: &Path) -> Result<()> {
            self.kept.lock().unwrap().retain(|kept| kept.file != file);
            Ok(())
        }
        fn held_telemetry(&self, _: &Path) -> Result<Vec<HeldTelemetry>> {
            Ok(self
                .held
                .lock()
                .unwrap()
                .iter()
                .map(|(target, (lease, _))| HeldTelemetry {
                    target: target.clone(),
                    issued_under: lease.as_deref().map(LeaseToken::new),
                })
                .collect())
        }
        fn register_telemetry_session(
            &self,
            _: &Path,
            target: &StepTarget,
            session_id: &str,
        ) -> Result<()> {
            *self.registrations.lock().unwrap() += 1;
            if Self::fail(&self.failing_registrations) {
                return Err(anyhow!("the queue is locked"));
            }
            let mut owners = self.owners.lock().unwrap();
            match owners.get(session_id) {
                Some(owner) if owner != target => {
                    Err(crate::domain::session_step::StepError::SessionOfAnotherTarget.into())
                }
                _ => {
                    owners.insert(session_id.to_owned(), target.clone());
                    Ok(())
                }
            }
        }
        fn target_facts(&self, _: &Path, target: &StepTarget) -> Result<TargetFacts> {
            if target.id == "404" {
                return Err(anyhow!("the queue is locked"));
            }
            Ok(self
                .targets
                .lock()
                .unwrap()
                .get(&target.id)
                .cloned()
                .unwrap_or(TargetFacts::Ended))
        }
    }

    /// The events written, as (run, kind, reason); none on a run the
    /// queue does not have.
    #[derive(Default)]
    struct Log {
        gone: Vec<String>,
        written: RefCell<Vec<(String, &'static str, Option<String>)>>,
    }

    impl TokenLog for Log {
        fn record_token_event(
            &self,
            run: &RunId,
            kind: EventKind,
            payload: serde_json::Value,
        ) -> Result<()> {
            if self.gone.iter().any(|gone| gone == run.as_str()) {
                return Err(anyhow!("run {run} does not exist"));
            }
            self.written.borrow_mut().push((
                run.to_string(),
                kind.as_str(),
                payload["reason"].as_str().map(str::to_owned),
            ));
            Ok(())
        }
        /// Written as on a run named `queue:<kind>:<id>` of its target.
        fn record_queue_token_event(
            &self,
            kind: EventKind,
            payload: serde_json::Value,
        ) -> Result<()> {
            self.written.borrow_mut().push((
                format!(
                    "queue:{}:{}",
                    payload["target"]["kind"].as_str().unwrap(),
                    payload["target"]["id"].as_str().unwrap()
                ),
                kind.as_str(),
                payload["reason"].as_str().map(str::to_owned),
            ));
            Ok(())
        }
    }

    /// The runs as the sweep reads them: each run's status and lease
    /// (`stale` when its supervisor stopped beating), the live
    /// supervisors, and `unreadable`, which cannot be read.
    /// A run's status and its lease (whose, and whether stale).
    type RunRead = (Option<RunStatus>, Option<(&'static str, bool)>);

    #[derive(Default)]
    struct States {
        runs: BTreeMap<String, RunRead>,
        alive: Vec<&'static str>,
    }

    impl States {
        fn read(&self, run: &RunId, issuer: Option<&LeaseToken>) -> Result<RunTokenState> {
            if run.as_str() == "unreadable" {
                return Err(anyhow!("the queue is locked"));
            }
            let (status, lease) = self.runs.get(run.as_str()).copied().unwrap_or((None, None));
            Ok(RunTokenState {
                status,
                lease: lease.map(|(token, stale)| LeaseView {
                    token: LeaseToken::new(token),
                    stale,
                }),
                issuer_alive: issuer.is_none_or(|issuer| self.alive.contains(&issuer.as_str())),
            })
        }
    }

    fn run(id: &str) -> RunId {
        RunId::new(id).unwrap()
    }

    fn lease(token: &str) -> LeaseToken {
        LeaseToken::new(token)
    }

    fn written(
        run: &str,
        kind: &'static str,
        reason: Option<&str>,
    ) -> (String, &'static str, Option<String>) {
        (run.to_owned(), kind, reason.map(str::to_owned))
    }

    const DB: &str = "/q/queue.db";

    fn issue(tokens: &Tokens, log: &Log, id: &str, under: &str) -> Option<PathBuf> {
        issue_held(tokens, log, id, under, &mut Retries::default())
    }

    /// [`issue`] by the supervisor holding `retries`.
    fn issue_held(
        tokens: &Tokens,
        log: &Log,
        id: &str,
        under: &str,
        retries: &mut Retries,
    ) -> Option<PathBuf> {
        issue_run(
            tokens,
            log,
            Path::new(DB),
            (&run(id), TaskId::new(2)),
            &lease(under),
            retries,
        )
    }

    /// The executor's file as the supervisor holding `under` starts it.
    fn credential(
        tokens: &Tokens,
        log: &Log,
        id: &str,
        under: &str,
        retries: &mut Retries,
    ) -> Option<PathBuf> {
        credential_for(
            tokens,
            log,
            Path::new(DB),
            (&run(id), TaskId::new(2)),
            &lease(under),
            retries,
        )
    }

    /// The sweep of a supervisor that holds no revocation and no run.
    fn sweep_with(tokens: &Tokens, log: &Log, states: &States) -> Vec<RunId> {
        sweep_as(tokens, log, "sweeper", &mut Retries::default(), states)
    }

    /// The sweep of the supervisor holding `under` and `retries`.
    fn sweep_as(
        tokens: &Tokens,
        log: &Log,
        under: &str,
        retries: &mut Retries,
        states: &States,
    ) -> Vec<RunId> {
        let mut revoked = sweep(
            tokens,
            log,
            Path::new(DB),
            &lease(under),
            retries,
            &|run, issuer| states.read(run, issuer),
        );
        revoked.sort();
        revoked
    }

    /// Run `id` running under the live lease `under`.
    fn running(id: &str, under: &'static str) -> States {
        States {
            runs: [(
                id.to_owned(),
                (Some(RunStatus::Running), Some((under, false))),
            )]
            .into(),
            alive: vec![under],
        }
    }

    /// ADR-t1662-3 decision 11: the claim issues the run's token under the
    /// supervisor's lease, and a resume issues another once the one before
    /// was revoked; each issue and the revocation are recorded once they
    /// succeeded.
    #[test]
    fn a_claim_and_a_resume_issue_a_runs_token_and_record_each_once_it_held() {
        let tokens = Tokens::default();
        let log = Log::default();
        let target = StepTarget::run(&run("r1"));
        let file = issue(&tokens, &log, "r1", "a");
        assert_eq!(file, Some(PathBuf::from("/q/service/credentials/r1")));
        assert_eq!(tokens.telemetry_credential(Path::new(DB), &target), file);
        issue(&tokens, &log, "r1", "a").unwrap();
        assert_eq!(tokens.held(&target), Some((Some("a".to_owned()), 2)));
        assert_eq!(
            *log.written.borrow(),
            [
                written("r1", "telemetry_token_issued", None),
                written("r1", "telemetry_token_revoked", Some("reissued")),
                written("r1", "telemetry_token_issued", None),
            ]
        );
    }

    /// An issue that fails is recorded as nothing, neither an issue nor a
    /// revocation; a resume whose revocation of the one before fails
    /// records nothing and issues none, so the run keeps the token it had.
    #[test]
    fn an_issue_that_fails_or_whose_revocation_fails_records_nothing_of_it() {
        let tokens = Tokens::default();
        let log = Log::default();
        let target = StepTarget::run(&run("r1"));
        *tokens.failing_issues.lock().unwrap() = 1;
        assert_eq!(issue(&tokens, &log, "r1", "a"), None);
        assert_eq!(*log.written.borrow(), []);
        assert_eq!(tokens.held_telemetry(Path::new(DB)).unwrap(), []);

        issue(&tokens, &log, "r1", "a").unwrap();
        *tokens.failing_revocations.lock().unwrap() = 1;
        assert_eq!(issue(&tokens, &log, "r1", "a"), None);
        assert_eq!(tokens.held(&target), Some((Some("a".to_owned()), 1)));
        // A resume that revoked the one before and failed to issue records
        // the revocation only: no token holds.
        *tokens.failing_issues.lock().unwrap() = 1;
        assert_eq!(issue(&tokens, &log, "r1", "a"), None);
        assert_eq!(tokens.held(&target), None);
        assert_eq!(
            *log.written.borrow(),
            [
                written("r1", "telemetry_token_issued", None),
                written("r1", "telemetry_token_revoked", Some("reissued")),
            ]
        );
    }

    /// The sweep revokes the tokens of the runs that ended or are not in
    /// the queue, and records each once it was revoked; a revocation that
    /// fails records nothing and leaves the token listed, and the next
    /// sweep revokes it and records it then. A running run's token under a
    /// live lease, one whose run cannot be read and a target of no run's
    /// stay.
    #[test]
    fn a_revocation_that_fails_is_recorded_only_once_a_later_sweep_revoked_it() {
        let tokens = Tokens::default();
        let log = Log {
            gone: vec!["gone".to_owned()],
            ..Log::default()
        };
        let states = States {
            runs: [
                ("r1".to_owned(), (Some(RunStatus::Integrated), None)),
                (
                    "running".to_owned(),
                    (Some(RunStatus::Running), Some(("a", false))),
                ),
            ]
            .into(),
            alive: vec!["a"],
        };
        let planner = StepTarget {
            kind: TargetKind::Planner,
            id: "3".into(),
        };
        for id in ["r1", "running", "unreadable", "gone"] {
            issue(&tokens, &Log::default(), id, "a").unwrap();
        }
        tokens
            .issue_telemetry(Path::new(DB), &planner, None, None)
            .unwrap();
        // The first revocation (of `gone`, the first of the list) and the
        // second (of `r1`) fail.
        *tokens.failing_revocations.lock().unwrap() = 2;
        assert!(sweep_with(&tokens, &log, &states).is_empty());
        assert_eq!(*log.written.borrow(), []);
        assert_eq!(tokens.held_telemetry(Path::new(DB)).unwrap().len(), 5);

        assert_eq!(sweep_with(&tokens, &log, &states), [run("gone"), run("r1")]);
        // `gone` is not in the queue: its revocation has no run to be
        // recorded on.
        assert_eq!(
            *log.written.borrow(),
            [written("r1", "telemetry_token_revoked", Some("run_ended"))]
        );
        let left: Vec<StepTarget> = tokens
            .held_telemetry(Path::new(DB))
            .unwrap()
            .into_iter()
            .map(|held| held.target)
            .collect();
        assert_eq!(
            left,
            [
                StepTarget::run(&run("running")),
                StepTarget::run(&run("unreadable")),
                planner,
            ]
        );
        assert!(sweep_with(&tokens, &log, &states).is_empty());
    }

    /// A token whose lease was lost while its run goes on is revoked by
    /// the sweep of any supervisor, even after the revocation of the one
    /// that lost it failed: its lease taken by another supervisor, still
    /// its own but stale (that supervisor died), or gone with its
    /// supervisor. A run waiting with no lease whose issuer lives keeps it.
    #[test]
    fn a_lost_lease_is_revoked_by_a_later_sweep_whatever_the_runs_status() {
        let tokens = Tokens::default();
        let log = Log::default();
        for id in ["taken", "stale", "orphan", "waiting"] {
            issue(&tokens, &Log::default(), id, "a").unwrap();
        }
        let states = |alive: Vec<&'static str>| States {
            runs: [
                (
                    "taken".to_owned(),
                    (Some(RunStatus::Running), Some(("b", false))),
                ),
                (
                    "stale".to_owned(),
                    (Some(RunStatus::Running), Some(("a", true))),
                ),
                ("orphan".to_owned(), (Some(RunStatus::NeedsSession), None)),
                (
                    "waiting".to_owned(),
                    (Some(RunStatus::AwaitingIntegration), None),
                ),
            ]
            .into(),
            alive,
        };
        assert_eq!(
            sweep_with(&tokens, &log, &states(vec!["a"])),
            [run("stale"), run("taken")]
        );
        assert_eq!(
            sweep_with(&tokens, &log, &states(vec![])),
            [run("orphan"), run("waiting")]
        );
        assert!(
            log.written
                .borrow()
                .iter()
                .all(|(_, kind, why)| *kind == "telemetry_token_revoked"
                    && why.as_deref() == Some("lease_lost"))
        );
        assert_eq!(log.written.borrow().len(), 4);
    }

    /// A revocation for a lost lease takes only the token issued under
    /// it: once the adopting supervisor issued the run another, the late
    /// revocation of the one that lost the lease revokes and records
    /// nothing, and the sweep leaves the adopter's token while its lease
    /// holds.
    #[test]
    fn a_lost_lease_never_revokes_the_token_the_adopter_issued() {
        let tokens = Tokens::default();
        let log = Log::default();
        let target = StepTarget::run(&run("r1"));
        issue(&tokens, &log, "r1", "a").unwrap();
        // B adopts the run and issues it another; A sees its loss late.
        issue(&tokens, &log, "r1", "b").unwrap();
        let mut retries = Retries::default();
        revoke_lost(
            &tokens,
            &log,
            Path::new(DB),
            &run("r1"),
            &lease("a"),
            &mut retries,
        );
        assert!(!retries.holds(&run("r1")));
        assert_eq!(tokens.held(&target), Some((Some("b".to_owned()), 2)));
        let states = States {
            runs: [(
                "r1".to_owned(),
                (Some(RunStatus::Running), Some(("b", false))),
            )]
            .into(),
            alive: vec!["b"],
        };
        assert!(sweep_with(&tokens, &log, &states).is_empty());
        assert_eq!(
            *log.written.borrow(),
            [
                written("r1", "telemetry_token_issued", None),
                written("r1", "telemetry_token_revoked", Some("reissued")),
                written("r1", "telemetry_token_issued", None),
            ]
        );
    }
    /// A resume whose revocation of the token before fails records nothing
    /// and issues none, and the supervisor holds it: the token before still
    /// authenticates under the same live lease, which no sweep finds due.
    /// Its next sweep tries again; one that fails again still records
    /// nothing, and the one that revokes it records it and issues the run
    /// another.
    #[test]
    fn a_resumes_revocation_that_fails_is_tried_again_by_its_next_sweep() {
        let tokens = Tokens::default();
        let log = Log::default();
        let target = StepTarget::run(&run("r1"));
        let mut retries = Retries::default();
        let states = running("r1", "a");
        issue_held(&tokens, &log, "r1", "a", &mut retries).unwrap();
        *tokens.failing_revocations.lock().unwrap() = 1;
        assert_eq!(issue_held(&tokens, &log, "r1", "a", &mut retries), None);
        assert!(retries.holds(&run("r1")));
        // Another supervisor's sweep keeps it: its lease lives.
        assert!(sweep_with(&tokens, &log, &states).is_empty());
        *tokens.failing_revocations.lock().unwrap() = 1;
        assert!(sweep_as(&tokens, &log, "a", &mut retries, &states).is_empty());
        assert!(retries.holds(&run("r1")));
        assert_eq!(tokens.held(&target), Some((Some("a".to_owned()), 1)));
        assert_eq!(
            *log.written.borrow(),
            [written("r1", "telemetry_token_issued", None)]
        );

        assert!(sweep_as(&tokens, &log, "a", &mut retries, &states).is_empty());
        assert!(!retries.holds(&run("r1")));
        assert_eq!(tokens.held(&target), Some((Some("a".to_owned()), 2)));
        assert!(sweep_as(&tokens, &log, "a", &mut retries, &states).is_empty());
        assert_eq!(
            *log.written.borrow(),
            [
                written("r1", "telemetry_token_issued", None),
                written("r1", "telemetry_token_revoked", Some("reissued")),
                written("r1", "telemetry_token_issued", None),
            ]
        );
    }

    /// A resume's revocation held for a run that ended, or that another
    /// supervisor took, issues none: the token before is due by the list
    /// and revoked for that, under its own lease.
    #[test]
    fn a_held_resume_whose_run_ended_or_went_issues_none() {
        let tokens = Tokens::default();
        let log = Log::default();
        let mut retries = Retries::default();
        for id in ["ended", "taken"] {
            issue_held(&tokens, &log, id, "a", &mut retries).unwrap();
            *tokens.failing_revocations.lock().unwrap() = 1;
            assert_eq!(issue_held(&tokens, &log, id, "a", &mut retries), None);
        }
        let states = States {
            runs: [
                ("ended".to_owned(), (Some(RunStatus::Integrated), None)),
                (
                    "taken".to_owned(),
                    (Some(RunStatus::Running), Some(("b", false))),
                ),
            ]
            .into(),
            alive: vec!["a", "b"],
        };
        assert_eq!(
            sweep_as(&tokens, &log, "a", &mut retries, &states),
            [run("ended"), run("taken")]
        );
        assert!(!retries.holds(&run("ended")) && !retries.holds(&run("taken")));
        assert_eq!(tokens.held_telemetry(Path::new(DB)).unwrap(), []);
        assert_eq!(*tokens.issues.lock().unwrap(), 2);
        let mut reasons: Vec<Option<String>> = log
            .written
            .borrow()
            .iter()
            .filter(|(_, kind, _)| *kind == "telemetry_token_revoked")
            .map(|(_, _, why)| why.clone())
            .collect();
        reasons.sort();
        assert_eq!(
            reasons,
            [Some("lease_lost".to_owned()), Some("run_ended".to_owned())]
        );
    }

    /// A lost lease whose revocation fails is held, recorded as nothing,
    /// and revoked and recorded by the supervisor's next sweep, even while
    /// the queue still shows the run as its own.
    #[test]
    fn a_lost_leases_revocation_that_fails_is_held_until_a_sweep_revoked_it() {
        let tokens = Tokens::default();
        let log = Log::default();
        let mut retries = Retries::default();
        issue_held(&tokens, &log, "r1", "a", &mut retries).unwrap();
        *tokens.failing_revocations.lock().unwrap() = 1;
        revoke_lost(
            &tokens,
            &log,
            Path::new(DB),
            &run("r1"),
            &lease("a"),
            &mut retries,
        );
        assert!(retries.holds(&run("r1")));
        assert_eq!(log.written.borrow().len(), 1);
        assert!(sweep_as(&tokens, &log, "a", &mut retries, &running("r1", "a")).is_empty());
        assert!(!retries.holds(&run("r1")));
        assert_eq!(tokens.held_telemetry(Path::new(DB)).unwrap(), []);
        assert_eq!(
            log.written.borrow().last(),
            Some(&written(
                "r1",
                "telemetry_token_revoked",
                Some("lease_lost")
            ))
        );
    }

    /// An executor a supervisor starts on a run finds the run's token issued
    /// under that supervisor's lease, the same file with nothing issued
    /// again; a run that holds none (its token revoked once the supervisor
    /// that issued it ended) or only another lease's is issued one under
    /// this lease first, the other revoked. A resume's revocation held for
    /// the run is tried again first.
    #[test]
    fn an_executor_finds_the_token_of_its_supervisors_lease_issued_when_it_was_not() {
        let tokens = Tokens::default();
        let log = Log::default();
        let target = StepTarget::run(&run("r1"));
        let mut retries = Retries::default();
        let file = issue_held(&tokens, &log, "r1", "a", &mut retries);
        assert_eq!(credential(&tokens, &log, "r1", "a", &mut retries), file);
        assert_eq!(tokens.held(&target), Some((Some("a".to_owned()), 1)));

        // b reviews it after a ended and the sweep revoked a's token.
        assert!(
            revoke_run(
                &tokens,
                &log,
                Path::new(DB),
                &run("r1"),
                TokenRevocation::LeaseLost,
                Some(&lease("a")),
            )
            .unwrap()
        );
        assert_eq!(credential(&tokens, &log, "r1", "b", &mut retries), file);
        assert_eq!(tokens.held(&target), Some((Some("b".to_owned()), 2)));
        // c starts one while b's token holds: b's is revoked.
        assert_eq!(credential(&tokens, &log, "r1", "c", &mut retries), file);
        assert_eq!(tokens.held(&target), Some((Some("c".to_owned()), 3)));

        // A resume of c's whose revocation failed: the start of the next
        // executor tries it again.
        *tokens.failing_revocations.lock().unwrap() = 1;
        assert_eq!(issue_held(&tokens, &log, "r1", "c", &mut retries), None);
        assert_eq!(credential(&tokens, &log, "r1", "c", &mut retries), file);
        assert!(!retries.holds(&run("r1")));
        assert_eq!(tokens.held(&target), Some((Some("c".to_owned()), 4)));
        assert_eq!(
            *log.written.borrow(),
            [
                written("r1", "telemetry_token_issued", None),
                written("r1", "telemetry_token_revoked", Some("lease_lost")),
                written("r1", "telemetry_token_issued", None),
                written("r1", "telemetry_token_revoked", Some("reissued")),
                written("r1", "telemetry_token_issued", None),
                written("r1", "telemetry_token_revoked", Some("reissued")),
                written("r1", "telemetry_token_issued", None),
            ]
        );
    }
    /// The revocations held outlive the exec of a handoff: kept in the
    /// registration's file, taken over (and the file removed) by the
    /// process that keeps the registration, whose next sweep revokes the
    /// token before the resume, records it and issues another. Another
    /// registration takes nothing of them, and nothing held writes nothing.
    #[test]
    fn held_revocations_outlive_the_exec_of_a_handoff() {
        use crate::application::memory_files::MemoryFiles;
        let tokens = Tokens::default();
        let log = Log::default();
        let files = MemoryFiles::default();
        let target = StepTarget::run(&run("r1"));
        let file = Retries::file(Path::new("/q"), &lease("a"));
        assert_eq!(file, Path::new("/q/telemetry-retries-a.json"));
        let mut before = Retries::default();
        before.save(&files, &file, &lease("a")).unwrap();
        assert!(!files.is_file(&file));

        issue_held(&tokens, &log, "r1", "a", &mut before).unwrap();
        *tokens.failing_revocations.lock().unwrap() = 1;
        assert_eq!(issue_held(&tokens, &log, "r1", "a", &mut before), None);
        before.save(&files, &file, &lease("a")).unwrap();
        assert!(!Retries::take(&files, &file, &lease("b")).holds(&run("r1")));
        assert!(!files.is_file(&file));
        before.save(&files, &file, &lease("a")).unwrap();
        drop(before);

        // The process after the exec: the same registration, the same lease.
        let mut after = Retries::take(&files, &file, &lease("a"));
        assert!(after.holds(&run("r1")));
        assert!(!files.is_file(&file));
        assert_eq!(tokens.held(&target), Some((Some("a".to_owned()), 1)));
        assert!(sweep_as(&tokens, &log, "a", &mut after, &running("r1", "a")).is_empty());
        assert!(!after.holds(&run("r1")));
        assert_eq!(tokens.held(&target), Some((Some("a".to_owned()), 2)));
        assert_eq!(
            *log.written.borrow(),
            [
                written("r1", "telemetry_token_issued", None),
                written("r1", "telemetry_token_revoked", Some("reissued")),
                written("r1", "telemetry_token_issued", None),
            ]
        );
    }

    fn target(kind: TargetKind, id: i64) -> StepTarget {
        StepTarget::of(kind, id)
    }

    /// ADR-t1662-3 decision 11: the control side issues a target of no
    /// run its token as it starts its agent and revokes it at its end, each
    /// recorded on the queue only once it succeeded; a revocation that
    /// failed records nothing and leaves the token for the sweep.
    #[test]
    fn a_target_of_no_run_is_issued_at_its_start_and_revoked_at_its_end() {
        let (tokens, log) = (Tokens::default(), Log::default());
        let telemetry = TargetTelemetry {
            tokens: &tokens,
            db: Path::new(DB),
            lease: Some(&lease("a")),
        };
        let review = target(TargetKind::PlanReview, 4);
        assert!(telemetry.issue(&log, &review).is_some());
        assert_eq!(tokens.held(&review), Some((Some("a".to_owned()), 1)));
        *tokens.failing_issues.lock().unwrap() = 1;
        let observer = target(TargetKind::Observer, 8);
        assert_eq!(
            TargetTelemetry {
                lease: None,
                ..telemetry
            }
            .issue(&log, &observer),
            None
        );
        *tokens.failing_revocations.lock().unwrap() = 1;
        telemetry.end(&log, &review, TokenRevocation::JobEnded);
        assert!(tokens.held(&review).is_some());
        telemetry.end(&log, &review, TokenRevocation::JobEnded);
        assert_eq!(tokens.held(&review), None);
        // Nothing left to revoke records nothing.
        telemetry.end(&log, &review, TokenRevocation::JobEnded);
        assert_eq!(
            *log.written.borrow(),
            [
                written("queue:plan_review:4", "telemetry_token_issued", None),
                written(
                    "queue:plan_review:4",
                    "telemetry_token_revoked",
                    Some("job_ended")
                ),
            ]
        );
    }

    /// The sweep revokes the tokens of the targets of no run whose end the
    /// queue holds, or whose runner is gone, and keeps the rest, a run's
    /// among them; one that cannot be read, or whose revocation failed,
    /// stays for the next sweep, and is recorded only once revoked.
    #[test]
    fn the_sweep_revokes_the_tokens_of_targets_that_ended_or_whose_runner_is_gone() {
        let (tokens, log) = (Tokens::default(), Log::default());
        let telemetry = TargetTelemetry {
            tokens: &tokens,
            db: Path::new(DB),
            lease: None,
        };
        let planner = target(TargetKind::Planner, 1);
        let closed = target(TargetKind::Planner, 2);
        let review = target(TargetKind::GoalReview, 3);
        let orphan = target(TargetKind::PlanReview, 5);
        let command = target(TargetKind::ThroughputReview, 6);
        let killed = target(TargetKind::Observer, 7);
        let unreadable = target(TargetKind::EvalJob, 404);
        for each in [
            &planner,
            &closed,
            &review,
            &orphan,
            &command,
            &killed,
            &unreadable,
        ] {
            telemetry.issue(&log, each).unwrap();
        }
        issue(&tokens, &log, "r1", "a").unwrap();
        log.written.borrow_mut().clear();
        let beat = |pid| Some(Beat { pid, age_secs: 1 });
        tokens.targets.lock().unwrap().extend([
            ("1".to_owned(), TargetFacts::OpenPlanner),
            (
                "3".to_owned(),
                TargetFacts::SupervisedJob {
                    supervisor: beat(10),
                },
            ),
            (
                "5".to_owned(),
                TargetFacts::SupervisedJob {
                    supervisor: beat(11),
                },
            ),
            (
                "6".to_owned(),
                TargetFacts::StartedCommand {
                    ends_by_ms: 2_000,
                    pid: Some(10),
                },
            ),
            (
                "7".to_owned(),
                TargetFacts::StartedCommand {
                    ends_by_ms: 2_000,
                    pid: Some(12),
                },
            ),
        ]);
        // Supervisor 11 and command 12 died without a word to the queue.
        let alive = |pid: u32| pid == 10;
        *tokens.failing_revocations.lock().unwrap() = 1;
        let swept = |tokens: &Tokens| sweep_targets(tokens, &log, Path::new(DB), 1_000, &alive);
        assert_eq!(swept(&tokens), [orphan.clone(), killed.clone()]);
        assert_eq!(swept(&tokens), std::slice::from_ref(&closed));
        assert!(swept(&tokens).is_empty());
        for kept in [&planner, &review, &command, &unreadable] {
            assert!(tokens.held(kept).is_some(), "{kept:?}");
        }
        assert!(tokens.held(&StepTarget::run(&run("r1"))).is_some());
        assert_eq!(
            *log.written.borrow(),
            [
                written(
                    "queue:plan_review:5",
                    "telemetry_token_revoked",
                    Some("job_ended")
                ),
                written(
                    "queue:observer:7",
                    "telemetry_token_revoked",
                    Some("job_ended")
                ),
                written(
                    "queue:planner:2",
                    "telemetry_token_revoked",
                    Some("planner_closed")
                ),
            ]
        );
    }

    /// ADR-t1662-3 decision 10: the control side registers a run's
    /// sessions as its events tell of them, each once by this process
    /// (asked of the queue once), a renewal's and a thread a turn named
    /// later included; one that failed to register is asked again at the
    /// next look, and none changes what the caller goes on with.
    #[test]
    fn a_runs_sessions_are_registered_to_it_once_each_and_a_failure_again_later() {
        use crate::domain::{EventId, turn::session_name};
        let tokens = Tokens::default();
        let run = run("r1");
        let event = |kind: EventKind, payload: serde_json::Value| RunEvent {
            id: EventId::new(1),
            task_id: Some(TaskId::new(1)),
            goal_id: None,
            run_id: Some(run.clone()),
            kind: kind.into(),
            payload,
            created_at: String::new(),
            actor: None,
        };
        let mut registered = std::collections::HashSet::new();
        // The claim: the session its wrapper names by the run.
        register_run_sessions(&tokens, Path::new(DB), &run, &[], &mut registered);
        register_run_sessions(&tokens, Path::new(DB), &run, &[], &mut registered);
        assert_eq!(*tokens.registrations.lock().unwrap(), 1);
        // A renewal, then a Codex thread a turn named, the first asked
        // when the queue was locked.
        let mut events = vec![event(EventKind::SessionRenewed, serde_json::json!({}))];
        *tokens.failing_registrations.lock().unwrap() = 1;
        register_run_sessions(&tokens, Path::new(DB), &run, &events, &mut registered);
        assert!(!registered.contains(&session_name("r1", 1)));
        events.push(event(
            EventKind::TurnSessionIdentified,
            serde_json::json!({"session_id": "thread-1"}),
        ));
        register_run_sessions(&tokens, Path::new(DB), &run, &events, &mut registered);
        let owners = tokens.owners.lock().unwrap().clone();
        let of_run = StepTarget::run(&run);
        assert_eq!(
            owners,
            BTreeMap::from([
                ("r1".to_owned(), of_run.clone()),
                (session_name("r1", 1), of_run.clone()),
                ("thread-1".to_owned(), of_run),
            ])
        );
        assert_eq!(*tokens.registrations.lock().unwrap(), 4);
        // A thread another target holds is not taken, and not asked again.
        tokens
            .owners
            .lock()
            .unwrap()
            .insert("planners".to_owned(), target(TargetKind::Planner, 3));
        events.push(event(
            EventKind::TurnSessionIdentified,
            serde_json::json!({"session_id": "planners"}),
        ));
        register_run_sessions(&tokens, Path::new(DB), &run, &events, &mut registered);
        register_run_sessions(&tokens, Path::new(DB), &run, &events, &mut registered);
        assert_eq!(*tokens.registrations.lock().unwrap(), 5);
        assert_eq!(
            tokens.owners.lock().unwrap().get("planners"),
            Some(&target(TargetKind::Planner, 3))
        );
    }

    /// A target of no run's session is registered to it alone: again to
    /// the same target changes nothing, to another one is refused and
    /// logged, and `None` (a session its agent names, not read yet)
    /// registers nothing.
    #[test]
    fn a_target_of_no_runs_session_is_registered_to_it_and_refused_to_another() {
        let tokens = Tokens::default();
        let telemetry = TargetTelemetry {
            tokens: &tokens,
            db: Path::new(DB),
            lease: None,
        };
        let planner = target(TargetKind::Planner, 3);
        let review = target(TargetKind::GoalReview, 5);
        telemetry.register(&planner, Some("p-1"));
        telemetry.register(&planner, Some("p-1"));
        telemetry.register(&review, None);
        assert!(!register_session(&tokens, Path::new(DB), &review, "p-1"));
        assert_eq!(
            *tokens.owners.lock().unwrap(),
            BTreeMap::from([("p-1".to_owned(), planner)])
        );
        assert_eq!(*tokens.registrations.lock().unwrap(), 3);
    }

    /// The sweep removes a wrapper's kept file once the queue says its
    /// target ended, whether or not it holds a token (one gone with a lost
    /// lease, the run ended since), and again at the next pass when a
    /// wrapper wrote it after the end; a live target's, one that cannot be
    /// read and a young file naming no target stay.
    #[test]
    fn kept_steps_go_once_their_target_ended_whatever_its_token() {
        use crate::domain::telemetry::{KeptStepsFile, UNREAD_KEPT_STEPS_MS};
        let (tokens, log) = (Tokens::default(), Log::default());
        // r1's token went with a lost lease and was never issued again.
        issue(&tokens, &log, "r1", "a").unwrap();
        revoke_lost(
            &tokens,
            &log,
            Path::new(DB),
            &run("r1"),
            &lease("a"),
            &mut Retries::default(),
        );
        assert!(tokens.held_telemetry(Path::new(DB)).unwrap().is_empty());
        tokens
            .targets
            .lock()
            .unwrap()
            .insert("3".to_owned(), TargetFacts::OpenPlanner);
        let file = |name: &str, target: Option<StepTarget>, age_ms| KeptStepsFile {
            file: format!("/q/service/steps/{name}").into(),
            target,
            age_ms,
        };
        let r1 = file("r1.json", Some(StepTarget::run(&run("r1"))), 0);
        let kept = vec![
            r1.clone(),
            file("r2.json", Some(StepTarget::run(&run("r2"))), 0),
            file("gone.json", Some(StepTarget::run(&run("gone"))), 0),
            file(
                "unreadable.json",
                Some(StepTarget::run(&run("unreadable"))),
                0,
            ),
            file("p3.json", Some(target(TargetKind::Planner, 3)), 0),
            file("p4.json", Some(target(TargetKind::Planner, 4)), 0),
            file("p404.json", Some(target(TargetKind::Planner, 404)), 0),
            file("young.json.tmp", None, UNREAD_KEPT_STEPS_MS - 1),
            file("old.json.tmp", None, UNREAD_KEPT_STEPS_MS),
        ];
        *tokens.kept.lock().unwrap() = kept;
        let status = |run: &RunId| -> Result<KeptRun> {
            let status = match run.as_str() {
                "r1" => Some(RunStatus::Integrated),
                "r2" => Some(RunStatus::Running),
                "unreadable" => return Err(anyhow!("the queue is locked")),
                _ => None,
            };
            Ok(KeptRun {
                status,
                revivable: true,
            })
        };
        let names = |files: Vec<PathBuf>| -> Vec<String> {
            files
                .iter()
                .map(|file| file.file_name().unwrap().to_string_lossy().into_owned())
                .collect()
        };
        assert_eq!(
            names(sweep_kept_steps(&tokens, Path::new(DB), &status)),
            ["r1.json", "gone.json", "p4.json", "old.json.tmp"]
        );
        let left: Vec<PathBuf> = tokens
            .kept
            .lock()
            .unwrap()
            .iter()
            .map(|kept| kept.file.clone())
            .collect();
        assert_eq!(
            names(left),
            [
                "r2.json",
                "unreadable.json",
                "p3.json",
                "p404.json",
                "young.json.tmp"
            ]
        );
        // A wrapper's last write after the end: gone at the next pass.
        tokens.kept.lock().unwrap().push(r1);
        assert_eq!(
            names(sweep_kept_steps(&tokens, Path::new(DB), &status)),
            ["r1.json"]
        );
    }

    /// An interrupted run's kept file outlives the sweep while recovery can
    /// put the run back, so the wrapper of its resume goes on numbering
    /// its sessions: also once its task went back to `ready` with no later
    /// run, as a triage under way may still resume it. It goes at the sweep
    /// after a later run of its task takes its place, or after its task
    /// ended, and a failed run's alike.
    #[test]
    fn an_interrupted_runs_kept_steps_stay_until_recovery_can_no_longer_bring_it_back() {
        use crate::domain::TaskStatus;
        use crate::domain::telemetry::KeptStepsFile;
        let tokens = Tokens::default();
        let file = |name: &str| KeptStepsFile {
            file: format!("/q/service/steps/{name}.json").into(),
            target: Some(StepTarget::run(&run(name))),
            age_ms: 0,
        };
        *tokens.kept.lock().unwrap() = vec![file("r1"), file("r2")];
        let names = |files: Vec<PathBuf>| -> Vec<String> {
            files
                .iter()
                .map(|file| file.file_stem().unwrap().to_string_lossy().into_owned())
                .collect()
        };
        // Each run's task: its latest run and its status.
        let tasks = RefCell::new(BTreeMap::from([
            ("r1", ("r1", TaskStatus::InProgress)),
            ("r2", ("r2", TaskStatus::InProgress)),
        ]));
        let facts = |id: &RunId| -> Result<KeptRun> {
            let (latest, task) = tasks.borrow()[id.as_str()];
            let status = if id.as_str() == "r1" {
                RunStatus::Interrupted
            } else {
                RunStatus::Failed
            };
            Ok(KeptRun::of(
                id,
                Some(status),
                Some(&run(latest)),
                Some(task),
            ))
        };
        // r1 interrupted and r2 failed, each the latest run of a task in
        // progress: both stay, pass after pass.
        for _ in 0..2 {
            assert!(sweep_kept_steps(&tokens, Path::new(DB), &facts).is_empty());
        }
        // Both tasks back to ready with no later run: a triage under way
        // may still resume them, so both stay.
        for task in tasks.borrow_mut().values_mut() {
            task.1 = TaskStatus::Ready;
        }
        assert!(sweep_kept_steps(&tokens, Path::new(DB), &facts).is_empty());
        assert_eq!(tokens.kept.lock().unwrap().len(), 2);
        // A claim of r1's task started a later run: r1's goes.
        tasks
            .borrow_mut()
            .insert("r1", ("r3", TaskStatus::InProgress));
        assert_eq!(
            names(sweep_kept_steps(&tokens, Path::new(DB), &facts)),
            ["r1"]
        );
        // r2's task was canceled: r2's goes.
        tasks
            .borrow_mut()
            .insert("r2", ("r2", TaskStatus::Canceled));
        assert_eq!(
            names(sweep_kept_steps(&tokens, Path::new(DB), &facts)),
            ["r2"]
        );
        assert!(tokens.kept.lock().unwrap().is_empty());
    }
}
