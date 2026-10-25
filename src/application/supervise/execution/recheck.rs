//! The landing recheck (ADR-0068): after this supervisor lands a run, or
//! whenever main differs from the main the last recheck finished against
//! (a direct `integrate`, a landing before a handoff or a restart, a push
//! outside dagq; ADR-t1310-1), the runs that wait to land are checked
//! against the new main off the loop,
//! one recheck at a time: `git merge-tree` for a conflict, then the
//! `[recheck] command` of `dagq.toml` on main's tree with the run merged
//! in, in one scratch worktree of the queue's with the build output of
//! each declared profile that applies there ([`recheck_env`]),
//! when that tree differs from main in a path `[recheck] paths` names (or
//! there are none, ADR-t2032-1).
//! A run found no longer landing is parked for a resume at once, whatever
//! asks it waits on; a run this supervisor holds in a slot is parked when
//! it would land. A command that failed on the host (a full disk, a kill,
//! its time limit; ADR-t639-1) judges nothing: the run keeps waiting to
//! land, neither parked nor held. A run found still landing records that
//! (ADR-t1311-1). Either way the run's open asks show the latest finding.
//! The command of a run whose worker works in a container runs in a
//! verification's container, as `integrate`'s verification does, never on
//! the host; with no such container it is not run and judges nothing, as
//! a command that failed on the host (ADR-t2424-1).
//! Before any command of a pass runs on the host, the recheck checks that
//! it can write the build outputs of all of them ([`check_writable`]); when it cannot,
//! the pass runs no command on the host and records nothing on the runs
//! that would have run one (ADR-0068 decision 2), and the queue's
//! `landing_recheck_target_unwritable` tells the inbox ([`TargetCheck`]).
//! A pass whose command failed a run on the host for the code runs the
//! command once more on main's tree alone ([`check_main`]); when main
//! fails it too, the runs that failed so are neither parked nor held, and
//! the queue tells the inbox once (ADR-t2141-1).

use super::*;
use crate::domain::EventKind;
use crate::domain::recheck::{
    self, CheckedBy, HELD, LANDING_RECHECK_CLEAN, LANDING_RECHECK_FAILED,
    LANDING_RECHECK_MAIN_KINDS, Landed, MainCheck, RecheckConfig, RecheckFailure, SKIPPED,
    TargetCheck,
};
use crate::domain::sccache::{CheckReason, GuardLook};
use crate::domain::toolchain::{BuildDir, ProfileRegistry};
use crate::domain::verify_failure::{self, CommandTimedOut};

/// Where the recheck keeps its scratch worktree and the build outputs of
/// the declarations it runs with, under the queue's directory.
const RECHECK_DIR: &str = "recheck";

/// The scratch worktree of the recheck's directory `dir` (its links
/// resolved): named after the queue it is in
/// ([`crate::domain::recheck_scratch_name`]), whose record in
/// `.git/worktrees` Git names after it.
fn scratch_worktree(dir: &Path) -> PathBuf {
    dir.join(crate::domain::recheck_scratch_name(
        dir.parent().unwrap_or(dir),
    ))
}

/// The lock file in [`RECHECK_DIR`] the queue's supervisors share: one
/// recheck at a time uses the scratch worktree and the build outputs, and
/// the cleanup for room clears them only under it ([`clear_targets`]).
const RECHECK_LOCK: &str = "lock";

/// The build outputs in the recheck's directory of the queue at
/// `queue_dir`: its entries that are the directory of a declaration of a
/// profile of `profiles` ([`ProfileRegistry::holds_build_dir`]), whatever
/// the repository declares now, by name. None when the directory cannot
/// be read (there is none before the first recheck). Published to host
/// operation's cleanup for room, which clears them through
/// [`clear_targets`].
pub(in crate::application::supervise) fn build_dirs(
    files: &dyn RunFiles,
    queue_dir: &Path,
    profiles: &ProfileRegistry,
) -> Vec<String> {
    let mut dirs: Vec<String> = files
        .read_dir(&queue_dir.join(RECHECK_DIR))
        .unwrap_or_default()
        .iter()
        .filter_map(|entry| entry.file_name()?.to_str().map(str::to_owned))
        .filter(|name| profiles.holds_build_dir(name))
        .collect();
    dirs.sort();
    dirs
}

/// Run `clear` on each of the build outputs `names` ([`build_dirs`]) in
/// the recheck's directory of the queue at `queue_dir` that is a
/// directory, only while no recheck uses them: the recheck lock is taken
/// once, without waiting, and held until the last `clear` returns, so no
/// recheck starts meanwhile (it tries again on a later pass, as when
/// another supervisor's recheck holds the lock) and they are cleared
/// together or left together. `None` with no such directory, or while a
/// recheck holds the lock. Published to host operation's cleanup for
/// room, which clears the declarations' build outputs through it.
pub(in crate::application::supervise) fn clear_targets<R>(
    files: &dyn RunFiles,
    queue_dir: &Path,
    names: &[String],
    clear: impl FnMut(&Path) -> R,
) -> Result<Option<Vec<R>>> {
    let dir = queue_dir.join(RECHECK_DIR);
    let targets: Vec<PathBuf> = names
        .iter()
        .map(|name| dir.join(name))
        .filter(|target| files.is_dir(target))
        .collect();
    if targets.is_empty() {
        return Ok(None);
    }
    let lock = dir.join(RECHECK_LOCK);
    let Some(_lock) = files
        .try_lock(&lock)
        .with_context(|| format!("lock {}", lock.display()))?
    else {
        info!(
            "the recheck build outputs in {} are in use: left for the next cleanup",
            dir.display()
        );
        return Ok(None);
    };
    Ok(Some(
        targets.iter().map(PathBuf::as_path).map(clear).collect(),
    ))
}

/// How many of the latest `landing_recheck_finished` tell who checked a
/// main already.
const CHECKED_LOOKBACK: usize = 20;

/// What this process and the latest `landing_recheck_finished` say of a
/// recheck of main before the queue's recheck lock is taken
/// ([`look_before_lock`]).
#[derive(Debug)]
enum Look {
    /// This supervisor checked main as it is: remembered as checked.
    Checked,
    /// Nothing to check: remembered with the runs that wait now, so that a
    /// run that starts waiting later is looked at.
    Idle,
    /// These runs are checked, unless the lock or the runs' own records say
    /// otherwise.
    Check(Vec<Target>),
}

/// Whether main is the one this process last checked, and no landing of
/// its own made a recheck `due`: an unmoved main costs no query.
fn unmoved(due: bool, rechecks: &Rechecks, main: &CommitSha) -> bool {
    !due && rechecks.checked.as_ref() == Some(main)
}

/// Whether to check main, given whether this supervisor's landing made a
/// recheck `due`, what `rechecks` remembers, the runs that wait to land
/// (`targets`), the latest `landing_recheck_finished` of the queue
/// (`finished`, read only when no landing is due and what this process
/// remembers does not settle it) and this supervisor's `token` (ADR-t1310-1), once main
/// moved ([`unmoved`]). A main looked at before with nothing to check is
/// looked at for the runs that started waiting since and nobody leases
/// (one this supervisor holds to land had its passed review's conflict
/// precheck against main as it is, ADR-0027 decision 4); a main this
/// supervisor checked is settled, and one another supervisor checked is
/// settled unless this one holds runs to land, which that one could not
/// see. No other supervisor's registration counts: none is elected.
/// With no run waiting, nothing is checked, so neither the check turn nor
/// the recheck lock is taken: the cleanup for room, which clears the
/// recheck's build outputs only under that lock, is not kept from them.
fn look_before_lock(
    due: bool,
    rechecks: &Rechecks,
    main: &CommitSha,
    mut targets: Vec<Target>,
    finished: impl FnOnce() -> Result<Option<RunEvent>>,
    token: &str,
) -> Result<Look> {
    if targets.is_empty() {
        return Ok(Look::Idle);
    }
    if let Some((idle, before)) = &rechecks.idle
        && idle == main
    {
        targets.retain(|target| {
            !target.held && !before.contains(&(target.run_id.clone(), target.head.clone()))
        });
        if targets.is_empty() {
            return Ok(Look::Idle);
        }
    }
    if !due
        && let Some(event) = finished()?
        && event.payload["main"] == main.as_str()
    {
        if event.payload["supervisor"] == token {
            return Ok(Look::Checked);
        }
        if !targets.iter().any(|target| target.held) {
            return Ok(Look::Idle);
        }
    }
    Ok(Look::Check(targets))
}

/// The runs a recheck checks under the queue's lock, of `targets` each
/// with whether it is settled already (its head contains main, or was
/// judged against it, [`recheck::judged_against`]), and who checked main
/// already: none when this supervisor did (a move seen before its own
/// landing's step was read), and only those this one holds in its slots
/// when another did.
fn left_to_check(targets: Vec<(Target, bool)>, checked: CheckedBy) -> Vec<Target> {
    targets
        .into_iter()
        .filter(|(target, settled)| {
            !settled
                && match checked {
                    CheckedBy::This => false,
                    CheckedBy::Other => target.held,
                    CheckedBy::No => true,
                }
        })
        .map(|(target, _)| target)
        .collect()
}

/// The `[recheck]` this supervisor's recheck uses (`config`, read only
/// when [run.env] finds the command's program): a command whose program
/// [run.env] cannot find (`run_env_missing`) would fail on every run
/// (ADR-0049 decision 9), so only the merge is checked then. Whether it
/// has a command decides whether a clean recheck recorded with no command
/// settles a run ([`recheck::judged_against`]).
fn recheck_config(
    run_env_missing: bool,
    config: impl FnOnce() -> Result<RecheckConfig>,
) -> Result<RecheckConfig> {
    if run_env_missing {
        Ok(RecheckConfig::default())
    } else {
        config()
    }
}

/// Where `landing_recheck_finished` goes: the run of the dagq landing
/// that moved main, or, when none did, the first run checked.
fn record_on(landed: Option<&Landed>, targets: &[Target]) -> RunId {
    match landed {
        Some(landed) => landed.run_id.clone(),
        None => targets[0].run_id.clone(),
    }
}

/// The counts of one recheck's findings, and the runs found no longer
/// landing with what followed, for its `landing_recheck_finished`.
#[derive(Debug, Default)]
struct Tally {
    checked: u64,
    clean: u64,
    /// Of `clean`, the runs whose diff touched none of `[recheck] paths`,
    /// so the command was not run on them (ADR-t2032-1).
    command_skipped: u64,
    conflicts: u64,
    check_failed: u64,
    /// Of the failed commands, those that failed on the host or found no
    /// container to run in ([`RecheckFailure::is_environmental`]): counted
    /// apart from `check_failed`, they resumed and held nothing.
    environment: u64,
    errors: u64,
    resumed: u64,
    held: u64,
    /// The runs whose command would have run but did not, since the
    /// build output could not be written ([`Finding::Unwritable`]): none
    /// was recorded, apart from `checked`.
    target_unwritable: u64,
    /// What the command found on main alone ([`MainCheck::outcome`]);
    /// `None` when the pass did not check main ([`recheck_runs`]).
    main_check: Option<&'static str>,
    /// Of `check_failed`, the runs not resumed nor held because the
    /// command failed on main alone too ([`recheck::kept_by_main`]).
    main_failed: u64,
    failed_runs: Vec<Value>,
}

impl Tally {
    /// A run found still landing.
    fn clean(&mut self, skipped_by_paths: bool) {
        self.checked += 1;
        self.clean += 1;
        if skipped_by_paths {
            self.command_skipped += 1;
        }
    }

    /// A run Git or the command could not judge.
    fn error(&mut self) {
        self.checked += 1;
        self.errors += 1;
    }

    /// A run found no longer landing for `failure`, or whose command
    /// failed on the host, and the `action` its record took
    /// ([`recheck::RESUMED`], [`HELD`] or [`SKIPPED`]); `None` when it was
    /// not recorded (the run moved on, or the record failed).
    /// `kept_by_main` when main's failure kept it from a resume.
    fn failed(
        &mut self,
        run_id: &RunId,
        failure: &RecheckFailure,
        action: Option<&'static str>,
        kept_by_main: bool,
    ) {
        self.checked += 1;
        match failure {
            RecheckFailure::Conflict { .. } => self.conflicts += 1,
            _ if failure.is_environmental() => self.environment += 1,
            RecheckFailure::CheckFailed { .. } | RecheckFailure::ContainerRefused { .. } => {
                self.check_failed += 1;
            }
        }
        let Some(action) = action else {
            return;
        };
        match action {
            HELD => self.held += 1,
            SKIPPED if kept_by_main => self.main_failed += 1,
            SKIPPED => {}
            _ => self.resumed += 1,
        }
        self.failed_runs.push(json!({
            "run_id": run_id,
            "code": failure.code(),
            "action": action,
        }));
    }

    /// The payload of `landing_recheck_finished` against `main` after the
    /// dagq landing `landed` (`None` when main moved without one), with
    /// `command`, how long it took and the supervisor that ran it, and,
    /// when the pass ran no command for its build output
    /// ([`Finding::Unwritable`]), `command_skipped_reason`
    /// [`recheck::TARGET_UNWRITABLE`].
    fn finished(
        &self,
        main: &CommitSha,
        landed: Option<&Landed>,
        command: Option<&str>,
        duration_secs: u64,
        supervisor: &LeaseToken,
    ) -> Value {
        let mut payload = json!({
            "main": main,
            "landed_run_id": landed.map(|l| &l.run_id),
            "landed_task_id": landed.map(|l| l.task_id),
            "command": command,
            "duration_secs": duration_secs,
            "failed_runs": self.failed_runs,
            "supervisor": supervisor,
        });
        if let (Some(payload), Value::Object(counts)) = (payload.as_object_mut(), self.counts()) {
            payload.extend(counts);
        }
        if self.target_unwritable > 0 {
            payload["command_skipped_reason"] = json!(recheck::TARGET_UNWRITABLE);
        }
        payload
    }

    /// The counts alone, as the log shows them.
    fn counts(&self) -> Value {
        json!({
            "checked": self.checked,
            "clean": self.clean,
            "command_skipped": self.command_skipped,
            "conflicts": self.conflicts,
            "check_failed": self.check_failed,
            "environment": self.environment,
            "errors": self.errors,
            "resumed": self.resumed,
            "held": self.held,
            "target_unwritable": self.target_unwritable,
            "main_check": self.main_check,
            "main_failed": self.main_failed,
        })
    }
}

/// What came of trying to start a recheck.
enum Start {
    Started,
    /// Nothing to check.
    Nothing,
    /// Not now: a run lands, or another supervisor's recheck holds the
    /// lock (`locked_out`).
    Later {
        locked_out: bool,
    },
}

/// One waiting run to check: the head it would land and its run directory
/// (where the command's log goes).
#[derive(Debug, Clone)]
pub(super) struct Target {
    run_id: RunId,
    head: CommitSha,
    run_dir: PathBuf,
    /// Held in a slot of this supervisor to land (not waiting unleased).
    held: bool,
    /// The run when its worker works in a container: the git of the
    /// recheck handles its commits guarded (ADR-t2179-1 decision 8), and
    /// its command runs in a verification's container (ADR-t2424-1).
    container: Option<TaskRun>,
}

/// Where the commands of the runs in a container run (ADR-t2424-1): in a
/// verification's container of their own, or, when `dagq.toml` does not
/// set the verification to the container or the container cannot be used,
/// nowhere, for `Refused`'s reason; never on the host instead.
#[derive(Clone)]
enum InContainer {
    Run {
        containers: Arc<dyn crate::application::run_container::VerificationContainers>,
        tokens: Arc<dyn crate::application::queue_broker::VerificationTokens>,
    },
    Refused(String),
}

/// One run the recheck checked: what it found, and the port of the
/// sccache whose wrapper was taken out of the command's environment in a
/// container (a run in a container whose command ran).
type Checked = (Target, Finding, Option<u16>);

/// What a recheck's thread found: each run's [`Checked`], its write check
/// of the build outputs, `None` when no command was to run on the host,
/// and the command on main alone, `None` when the pass did not check it
/// ([`check_main`]).
type Found = (Vec<Checked>, Option<TargetCheck>, Option<MainCheck>);

/// What the recheck found for one run.
#[derive(Debug)]
pub(super) enum Finding {
    /// Still lands; `skipped_by_paths` when the run's diff touches none of
    /// `[recheck] paths`, so the command was not run (ADR-t2032-1).
    Clean {
        skipped_by_paths: bool,
    },
    Failed(RecheckFailure),
    /// Git or the command could not be run: nothing is recorded on the run.
    Error(String),
    /// The command would have run, but the pass's build output could not
    /// be written ([`check_writable`]), so no command ran: nothing is
    /// recorded on the run (ADR-0068 decision 2).
    Unwritable,
}

/// A recheck in progress on its thread.
pub(super) struct RecheckWatch {
    /// The dagq landing that moved main; `None` when none did.
    landed: Option<Landed>,
    /// Where `landing_recheck_finished` goes: the landed run, or the first
    /// run checked when no dagq landing moved main.
    record_on: RunId,
    main: CommitSha,
    command: Option<String>,
    started: Instant,
    handle: Option<thread::JoinHandle<Found>>,
    /// The queue's recheck lock and the recheck's heavy turn
    /// (ADR-t2719-1), held until the recheck is recorded.
    _lock: Box<dyn std::any::Any + Send>,
}

/// The supervisor's rechecks: the one running, whether a landing of this
/// supervisor's waits for the next (landings during a recheck are checked
/// after it, against the newest main, once), and what this process knows
/// already (ADR-t1310-1): a main with a finished recheck, so an unmoved
/// main costs no query, and a main with nothing to check for the runs that
/// waited then (`idle`), so a run that starts waiting later is looked at.
#[derive(Default)]
pub(super) struct Rechecks {
    running: Option<RecheckWatch>,
    due: bool,
    /// The due recheck waits for another supervisor's to free the lock.
    locked_out: bool,
    checked: Option<CommitSha>,
    idle: Option<(CommitSha, Vec<(RunId, CommitSha)>)>,
}

impl Rechecks {
    /// Whether a recheck runs, or a due one waits for the lock another
    /// supervisor's holds: the loop waits for it like a job.
    pub(super) const fn running(&self) -> bool {
        self.running.is_some() || self.locked_out
    }
}

impl Phase {
    /// Whether the run waits in this slot to land: for the integration
    /// slot, or for its session's `/exit` before it lands.
    fn waits_to_land(&self) -> bool {
        match self {
            // Its e2e (ADR-t1233-2) comes before the slot.
            Phase::AwaitingSlot | Phase::AwaitingE2e | Phase::E2e(_) => true,
            Phase::Exiting(watch) => matches!(watch.then, AfterExit::Land),
            _ => false,
        }
    }
}

impl Supervisor<'_> {
    /// Note that `run` landed: the waiting runs are checked against the
    /// main it moved (ADR-0068 decision 1), named by the landing main is at
    /// when the recheck starts.
    pub(super) fn note_landed(&mut self, run: &TaskRun) {
        tracing::debug!(run_id = %run.id(), "run {} landed: a landing recheck is due", run.id());
        self.landing.rechecks.due = true;
    }

    /// Apply the recheck that finished, and start one when this
    /// supervisor's landing is due or main moved since the last recheck
    /// finished, unless this pass drains. An error is logged: the recheck
    /// is an aid, and the landing checks every run again anyway. Whether a
    /// recheck was applied: the next pass resumes the runs it parked.
    pub(super) fn recheck_pass(&mut self) -> bool {
        let mut applied = false;
        if self
            .landing
            .rechecks
            .running
            .as_ref()
            .is_some_and(|watch| watch.handle.as_ref().is_none_or(|h| h.is_finished()))
            && let Some(mut watch) = self.landing.rechecks.running.take()
        {
            let found = watch
                .handle
                .take()
                .map(|handle| handle.join())
                .transpose()
                .unwrap_or_else(|_| {
                    warn!("the landing recheck thread panicked");
                    None
                })
                .unwrap_or_default();
            match self.apply_recheck(&watch, found) {
                Ok(()) => self.landing.rechecks.checked = Some(watch.main.clone()),
                Err(error) => {
                    warn!(error = %format_args!("{error:#}"), "the landing recheck against main {} could not be recorded: {error:#}", watch.main);
                }
            }
            // The queue's recheck lock goes with the watch, after its
            // `landing_recheck_finished` is recorded.
            drop(watch);
            applied = true;
        }
        self.landing.rechecks.locked_out = false;
        if self.landing.rechecks.running.is_none() && !self.registration.draining() {
            let due = std::mem::take(&mut self.landing.rechecks.due);
            match self.try_recheck(due) {
                Ok(Start::Started | Start::Nothing) => {}
                // A landing in progress, or another supervisor's recheck:
                // the landing's recheck stays due for the next pass.
                Ok(Start::Later { locked_out }) => {
                    self.landing.rechecks.due = due;
                    self.landing.rechecks.locked_out = due && locked_out;
                }
                Err(error) => {
                    warn!(error = %format_args!("{error:#}"), "the landing recheck could not start: {error:#}");
                }
            }
        }
        applied
    }

    /// Start a recheck after this supervisor's landing (`due`), or, with
    /// none due, when main differs from the main of the latest
    /// `landing_recheck_finished` (ADR-t1310-1): main moved by a direct
    /// `integrate`, by a landing whose recheck a handoff or a restart
    /// dropped, or outside dagq. Read from the events, so it holds across
    /// an exec and a restart. The landing is the one main is at, whatever
    /// made the recheck due. Finding nothing to check holds only for the
    /// runs that waited then: a run that starts waiting later is looked at
    /// against the same main. The supervisors of the queue share one lock
    /// on the recheck (its scratch worktree and build outputs): a recheck starts
    /// only under it, and who checked the same main already is read under
    /// it, so it is not checked twice. Another supervisor's recheck of main
    /// settles only the runs it could see: the runs this one holds to land
    /// are checked still. Nothing starts while a run lands,
    /// and the waiting runs whose head already contains main are left out:
    /// they land as they are.
    fn try_recheck(&mut self, due: bool) -> Result<Start> {
        let main = self.repository.main_head()?;
        if unmoved(due, &self.landing.rechecks, &main) {
            return Ok(Start::Nothing);
        }
        let targets = self.recheck_targets()?;
        let seen: Vec<(RunId, CommitSha)> = targets
            .iter()
            .map(|target| (target.run_id.clone(), target.head.clone()))
            .collect();
        let queue = &self.queue;
        let finished = || queue.latest_event_of(recheck::LANDING_RECHECK_FINISHED);
        let targets = match look_before_lock(
            due,
            &self.landing.rechecks,
            &main,
            targets,
            finished,
            self.registration.token().as_str(),
        )? {
            Look::Checked => {
                self.landing.rechecks.checked = Some(main);
                return Ok(Start::Nothing);
            }
            Look::Idle => {
                self.landing.rechecks.idle = Some((main, seen));
                return Ok(Start::Nothing);
            }
            Look::Check(targets) => targets,
        };
        // It takes turns with the automatic update's build and e2e
        // (ADR-t2719-1), holding its turn until it is recorded: the due
        // recheck waits, as for a landing.
        let Some(turn) =
            self.take_check_turn(&format!("the landing recheck against main {main}"))?
        else {
            return Ok(Start::Later { locked_out: false });
        };
        let dir = self.recheck_dir()?;
        self.files.create_dir_all(&dir)?;
        let Some(lock) = self.files.try_lock(&dir.join(RECHECK_LOCK))? else {
            return Ok(Start::Later { locked_out: true });
        };
        // A landing in progress, this supervisor's or a direct
        // `integrate`'s, may have moved main before its `run_integrated`:
        // looked at once it ended, so the recheck names it.
        if !self
            .queue
            .runs_with_status(RunStatus::Integrating)?
            .is_empty()
        {
            return Ok(Start::Later { locked_out: false });
        }
        let config = recheck_config(self.landing.run_env_missing, || {
            self.verifier.recheck_config()
        })?;
        let runs_command = config.command.is_some();
        let mut judged = Vec::with_capacity(targets.len());
        for target in targets {
            let settled = self
                .repository
                .is_ancestor(main.as_str(), target.head.as_str())
                .unwrap_or(false)
                || recheck::judged_against(
                    &self.queue.run_events(&target.run_id)?,
                    main.as_str(),
                    target.head.as_str(),
                    runs_command,
                );
            judged.push((target, settled));
        }
        let finished = self
            .queue
            .latest_events_of(recheck::LANDING_RECHECK_FINISHED, CHECKED_LOOKBACK)?;
        let checked =
            recheck::checked_by(&finished, main.as_str(), self.registration.token().as_str());
        let targets = left_to_check(judged, checked);
        if targets.is_empty() {
            self.landing.rechecks.idle = Some((main, seen));
            return Ok(Start::Nothing);
        }
        let landed = self.landing_at(&main)?;
        self.start(landed, main, targets, config, Box::new((lock, turn)))?;
        Ok(Start::Started)
    }

    /// The queue's recheck directory: its scratch worktree, the profiles'
    /// build outputs and its lock.
    fn recheck_dir(&self) -> Result<PathBuf> {
        Ok(self
            .layout
            .db
            .parent()
            .context("queue database has no directory")?
            .join(RECHECK_DIR))
    }

    /// The dagq landing whose commit `main` is ([`recheck::landing_at`]).
    fn landing_at(&self, main: &CommitSha) -> Result<Option<Landed>> {
        let latest = self
            .queue
            .latest_event_of(EventKind::RunIntegrated.as_str())?;
        Ok(recheck::landing_at(latest.as_ref(), main.as_str()))
    }

    /// The runs a recheck looks at (ADR-0068 decision 1): those awaiting
    /// integration with a reviewed head that nobody leases (waiting for an
    /// ask's answer or for a recover), and those this supervisor holds in
    /// a slot to land. Runs in review or revise, and runs another process
    /// leases, are left to their own checks.
    fn recheck_targets(&mut self) -> Result<Vec<Target>> {
        let mut targets = Vec::new();
        for run in self
            .queue
            .runs_with_status(RunStatus::AwaitingIntegration)?
        {
            let (Some(head), Some(run_dir)) = (run.result_commit(), run.run_dir()) else {
                continue;
            };
            let held = match self.queue.run_lease(run.id())? {
                None => false,
                Some(lease)
                    if lease.token == *self.registration.token()
                        && self.claim.slots.iter().any(|slot| {
                            slot.run.id() == run.id() && slot.phase.waits_to_land()
                        }) =>
                {
                    true
                }
                Some(_) => continue,
            };
            targets.push(Target {
                run_id: run.id().clone(),
                head: head.clone(),
                run_dir: PathBuf::from(run_dir),
                held,
                container: run.in_container().then(|| run.clone()),
            });
        }
        Ok(targets)
    }

    fn start(
        &mut self,
        landed: Option<Landed>,
        main: CommitSha,
        targets: Vec<Target>,
        config: RecheckConfig,
        lock: Box<dyn std::any::Any + Send>,
    ) -> Result<()> {
        let record_on = record_on(landed.as_ref(), &targets);
        let command = config.command.clone();
        let dir = self.recheck_dir()?;
        // The command may not start the sccache server (ADR-t2086-1): its
        // guard lives in the recheck's directory.
        let look = match &command {
            Some(_) => {
                self.files.create_dir_all(&dir)?;
                let look = self.sccache_look(CheckReason::BeforeRecheck, &dir);
                self.record_wrapper_removed(&record_on, &look, json!({"job": "recheck"}));
                look
            }
            None => GuardLook::NotConfigured,
        };
        info!(
            "landing recheck of {} waiting run(s) against main {main} after {}{}",
            targets.len(),
            landed.as_ref().map_or_else(
                || "main moved without a dagq landing".to_owned(),
                |landed| format!("run {} (task {}) landed", landed.run_id, landed.task_id)
            ),
            command
                .as_ref()
                .map(|c| format!("; command {c:?}"))
                .unwrap_or_default()
        );
        let in_container = (command.is_some()
            && targets.iter().any(|target| target.container.is_some()))
        .then(|| self.recheck_container());
        let main_failed_open = command.is_some()
            && self
                .queue
                .latest_queue_event(&LANDING_RECHECK_MAIN_KINDS)?
                .is_some_and(|event| event.kind == EventKind::LandingRecheckMainFailed);
        let repository = self.repository.clone();
        let verifier = self.verifier.clone();
        let files = self.files.clone();
        let on = main.clone();
        let probe = write_probe(&self.generators.ids.uuid());
        let handle = spawn_traced(move || {
            recheck_runs(
                &Recheck {
                    repository: &*repository,
                    verifier: &*verifier,
                    files: &*files,
                    main: &on,
                    config: &config,
                    dir: &dir,
                    look: &look,
                    in_container: in_container.as_ref(),
                    probe: &probe,
                    main_failed_open,
                },
                targets,
            )
        });
        self.landing.rechecks.running = Some(RecheckWatch {
            landed,
            record_on,
            main,
            command,
            started: self.generators.clock.monotonic(),
            handle: Some(handle),
            _lock: lock,
        });
        Ok(())
    }

    /// Where the commands of the runs in a container run ([`InContainer`]):
    /// in a verification's container when `dagq.toml` sets the verification
    /// to the container and it can be used now, refused otherwise.
    fn recheck_container(&self) -> InContainer {
        match container_refusal(self.verifier.runtimes(), |target| {
            self.container.availability(target)
        }) {
            Some(why) => InContainer::Refused(why),
            None => InContainer::Run {
                containers: self.verification_containers.clone(),
                tokens: self.verification_tokens.clone(),
            },
        }
    }

    /// Record what a recheck found: each finding, failed or clean, on its
    /// run (and its open asks), and `landing_recheck_finished` with the counts on the run
    /// whose landing it followed (or the first run it checked).
    fn apply_recheck(&mut self, watch: &RecheckWatch, (found, writes, main): Found) -> Result<()> {
        let mut tally = Tally {
            main_check: main.as_ref().map(MainCheck::outcome),
            ..Tally::default()
        };
        for (target, finding, removed) in found {
            if let Some(port) = removed {
                self.with_env(|env| {
                    crate::application::sccache::record_wrapper_removed(
                        &*env.queue,
                        &target.run_id,
                        env.generators.clock.now(),
                        "supervisor",
                        port,
                        "the landing recheck's command runs in a container, which has no protected cache to compile with",
                        json!({"job": "recheck", "runtime": "container"}),
                    );
                });
            }
            let failure = match finding {
                Finding::Clean { skipped_by_paths } => {
                    tally.clean(skipped_by_paths);
                    match self.record_recheck_clean(watch, &target, skipped_by_paths) {
                        Ok(true) => {}
                        Ok(false) => {
                            info!(run_id = %target.run_id, "run {}: the landing recheck found it still landing on main {}, but it moved on meanwhile", target.run_id, watch.main);
                        }
                        Err(error) => {
                            warn!(run_id = %target.run_id, error = %format_args!("{error:#}"), "run {}: the landing recheck's clean finding could not be recorded: {error:#}", target.run_id);
                        }
                    }
                    continue;
                }
                Finding::Error(error) => {
                    tally.error();
                    warn!(run_id = %target.run_id, error = %error, "run {}: the landing recheck against main {} could not judge it: {error}", target.run_id, watch.main);
                    continue;
                }
                Finding::Unwritable => {
                    tally.target_unwritable += 1;
                    continue;
                }
                Finding::Failed(failure) => failure,
            };
            self.pause_at(PausePoint::RecheckFailure, &target.run_id);
            let main_log = main.as_ref().and_then(MainCheck::failed_log).filter(|_| {
                recheck::kept_by_main(&failure, target.container.is_none(), main.as_ref())
            });
            let action = match self.record_recheck_failure(watch, &target, &failure, main_log) {
                Ok(Some(action)) => Some(action),
                Ok(None) => {
                    info!(run_id = %target.run_id, "run {}: the landing recheck found it no longer landing on main {}, but it moved on meanwhile", target.run_id, watch.main);
                    None
                }
                Err(error) => {
                    warn!(run_id = %target.run_id, error = %format_args!("{error:#}"), "run {}: the landing recheck's finding could not be recorded: {error:#}", target.run_id);
                    None
                }
            };
            tally.failed(&target.run_id, &failure, action, main_log.is_some());
        }
        let payload = tally.finished(
            &watch.main,
            watch.landed.as_ref(),
            watch.command.as_deref(),
            self.generators.clock.since(watch.started).as_secs(),
            self.registration.token(),
        );
        info!(
            "landing recheck against main {} finished: {}",
            watch.main,
            tally.counts()
        );
        self.queue.record_runtime_event(
            &watch.record_on,
            EventKind::LandingRecheckFinished,
            payload,
        )?;
        if let Some(writes) = &writes {
            self.record_target_check(writes, &watch.main, tally.target_unwritable)?;
        }
        if let (Some(main), Some(command)) = (&main, watch.command.as_deref())
            && let Err(error) = self.record_main_check(watch, main, command, tally.main_failed)
        {
            warn!(error = %format_args!("{error:#}"), "the landing recheck's command on main {} alone could not be recorded: {error:#}", watch.main);
        }
        if main.is_some() {
            self.remove_stale_main_logs(&watch.main);
        }
        Ok(())
    }

    /// Remove the logs of earlier checks of main from the recheck's
    /// directory once the check of `main` wrote its own ([`check_main`]),
    /// as [`recheck::stale_main_logs`] says from what the queue points at
    /// now: kept are that log, the attention's while main's failure is
    /// the latest record of it, and those the open asks point at. Under
    /// the recheck lock the watch holds, and only warned of when it
    /// fails: the pass is judged and recorded already.
    fn remove_stale_main_logs(&self, main: &CommitSha) {
        let removed = || -> Result<()> {
            let dir = self.recheck_dir()?;
            let names: Vec<String> = self
                .files
                .read_dir(&dir)
                .with_context(|| format!("read {}", dir.display()))?
                .iter()
                .filter_map(|entry| entry.file_name()?.to_str().map(str::to_owned))
                .collect();
            let latest = self.queue.latest_queue_event(&LANDING_RECHECK_MAIN_KINDS)?;
            let asks = self.queue.asks(AskQuery::default())?;
            let asked: Vec<&str> = asks
                .iter()
                .filter_map(|ask| recheck::asked_main_log(&ask.question))
                .collect();
            let written = recheck::main_log_name(main);
            for name in recheck::stale_main_logs(
                &names,
                &written,
                recheck::attention_main_log(latest.as_ref()),
                &asked,
            ) {
                let log = dir.join(name);
                if let Err(error) = self.files.remove_file(&log) {
                    warn!(error = %error, "the landing recheck could not remove the log {} of an earlier check of main: {error}", log.display());
                }
            }
            Ok(())
        };
        if let Err(error) = removed() {
            warn!(error = %format_args!("{error:#}"), "the landing recheck could not remove the logs of earlier checks of main: {error:#}");
        }
    }

    /// Record on the queue what the command found on main alone
    /// (ADR-t2141-1) when it differs from the latest record of it
    /// ([`MainCheck::transition`]): a failure is the inbox's attention
    /// once, until a pass on main alone ends it. `failed_runs` is how many
    /// runs main's failure kept from a resume.
    fn record_main_check(
        &mut self,
        watch: &RecheckWatch,
        main: &MainCheck,
        command: &str,
        failed_runs: u64,
    ) -> Result<()> {
        match main {
            MainCheck::Failed { log_path, .. } => {
                warn!(
                    "the landing recheck's command {command:?} fails on main {} alone too (see {log_path}): {failed_runs} run(s) that failed it keep waiting to land instead of a resume",
                    watch.main
                );
            }
            MainCheck::Unjudged(why) => {
                warn!(
                    "the landing recheck's command {command:?} could not be judged on main {} alone: {why}; the runs that failed it are handled as before",
                    watch.main
                );
            }
            MainCheck::Passed { .. } => {}
        }
        let last = self.queue.latest_queue_event(&LANDING_RECHECK_MAIN_KINDS)?;
        if let Some((kind, mut payload)) = main.transition(
            last.as_ref().map(|e| &e.kind),
            &watch.main,
            command,
            failed_runs,
        ) {
            payload["supervisor"] = json!(self.registration.token());
            self.queue.record_queue_event(kind, payload)?;
        }
        Ok(())
    }

    /// Record on the queue what the pass's write check of its build output
    /// found, where it differs from the latest record of [`recheck::TARGET_KINDS`]
    /// ([`TargetCheck::transition`]): the attention once while it stays
    /// unwritable, its end once it is written again.
    fn record_target_check(
        &mut self,
        writes: &TargetCheck,
        main: &CommitSha,
        unrecorded: u64,
    ) -> Result<()> {
        if let TargetCheck::Unwritable { path, error } = writes {
            warn!(
                "the landing recheck against main {main} cannot write {path} ({error}): it ran no command, and the {unrecorded} run(s) it would have run one on keep waiting to land with nothing recorded"
            );
        }
        let last = self.queue.latest_queue_event(&recheck::TARGET_KINDS)?;
        if let Some((kind, mut payload)) =
            writes.transition(last.as_ref().map(|event| &event.kind), main, unrecorded)
        {
            payload["supervisor"] = json!(self.registration.token());
            self.queue.record_queue_event(kind, payload)?;
            if kind == EventKind::LandingRecheckTargetWritable {
                info!("the landing recheck writes its build output again; its commands run");
            }
        }
        Ok(())
    }

    /// Record on the run that it still lands on the main checked
    /// (ADR-t1311-1): `landing_recheck_clean`, and the finding as the
    /// recheck's paragraph of its open asks, as one with no command run
    /// when `skipped_by_paths`. Whether it was recorded: not when the run
    /// moved on since it was checked (another head, another status, or a
    /// lease of another process).
    fn record_recheck_clean(
        &mut self,
        watch: &RecheckWatch,
        target: &Target,
        skipped_by_paths: bool,
    ) -> Result<bool> {
        let run = self.queue.run(&target.run_id)?;
        if run.status() != RunStatus::AwaitingIntegration
            || run.result_commit() != Some(&target.head)
            || self
                .queue
                .run_lease(run.id())?
                .is_some_and(|lease| lease.token != *self.registration.token())
        {
            return Ok(false);
        }
        self.queue.record_runtime_event(
            run.id(),
            EventKind::LandingRecheckClean,
            recheck::clean_payload(
                watch.landed.as_ref(),
                &watch.main,
                &target.head,
                watch.command.as_deref(),
                skipped_by_paths,
            ),
        )?;
        let command = watch.command.as_deref().filter(|_| !skipped_by_paths);
        let note = recheck::clean_ask_note(watch.landed.as_ref(), &watch.main, command);
        for ask in self
            .queue
            .note_on_asks(run.id(), &note, LANDING_RECHECK_CLEAN)?
        {
            info!(run_id = %run.id(), ask_id = %ask.id, "run {}: the landing recheck's clean finding is now in its {} ask {}", run.id(), ask.kind.as_str(), ask.id);
        }
        Ok(true)
    }

    /// Record `failure` on the run (ADR-0068 decisions 3 and 4): a run
    /// nobody leases is parked for a resume in the same transaction; one
    /// this supervisor holds in a slot records it as held and is parked
    /// when it would land. A command that failed on the host does neither
    /// (ADR-t639-1), nor does one with no container to run in
    /// (ADR-t2424-1): the finding is recorded as [`SKIPPED`] and the run
    /// keeps waiting to land as it was, held by integrate's control of a
    /// host failure or not. Nor does a failure the command met on main
    /// alone too (`main_log`, the log of main's check; ADR-t2141-1),
    /// recorded as [`SKIPPED`] with `main_failed: true` and
    /// `main_log_path`, using no resume. Every way its open asks get the
    /// finding. `None` when the run moved on since it was checked (another
    /// head, another status, or a lease of another process).
    fn record_recheck_failure(
        &mut self,
        watch: &RecheckWatch,
        target: &Target,
        failure: &RecheckFailure,
        main_log: Option<&str>,
    ) -> Result<Option<&'static str>> {
        let run = self.queue.run(&target.run_id)?;
        if run.status() != RunStatus::AwaitingIntegration
            || run.result_commit() != Some(&target.head)
        {
            return Ok(None);
        }
        let reason = failure.reason(watch.landed.as_ref(), &watch.main);
        let mut payload = failure.payload(watch.landed.as_ref(), &watch.main, &target.head);
        let lease = self.queue.run_lease(run.id())?;
        if failure.is_environmental() || main_log.is_some() {
            if lease.is_some_and(|lease| lease.token != *self.registration.token()) {
                return Ok(None);
            }
            payload["action"] = json!(SKIPPED);
            payload["reason"] = json!(reason);
            payload["status"] = json!(run.status());
            if let Some(main_log) = main_log {
                payload["main_failed"] = json!(true);
                payload["main_log_path"] = json!(main_log);
            }
            self.queue
                .record_runtime_event(run.id(), EventKind::LandingRecheckFailed, payload)?;
            let note = if let Some(main_log) = main_log {
                warn!(run_id = %run.id(), "run {}: {reason}; the command fails on main alone too (see {main_log}), so the run keeps waiting to land", run.id());
                recheck::main_failed_ask_note(&reason, main_log)
            } else if matches!(failure, RecheckFailure::ContainerRefused { .. }) {
                warn!(run_id = %run.id(), "run {}: {reason}; nothing judged the code, so the run keeps waiting to land", run.id());
                recheck::refused_ask_note(&reason)
            } else {
                warn!(run_id = %run.id(), "run {}: {reason}; the host kept the command from judging the code, so the run keeps waiting to land", run.id());
                recheck::skipped_ask_note(&reason)
            };
            for ask in self
                .queue
                .note_on_asks(run.id(), &note, LANDING_RECHECK_FAILED)?
            {
                info!(run_id = %run.id(), ask_id = %ask.id, "run {}: the landing recheck's finding was added to its {} ask {}", run.id(), ask.kind.as_str(), ask.id);
            }
            return Ok(Some(SKIPPED));
        }
        let action = match lease {
            None => match self
                .queue
                .park_rechecked(run.id(), None, &reason, payload)?
            {
                Some(_) => recheck::RESUMED,
                None => return Ok(None),
            },
            Some(lease) if lease.token == *self.registration.token() => {
                payload["action"] = json!(HELD);
                payload["reason"] = json!(reason);
                self.queue.record_runtime_event(
                    run.id(),
                    EventKind::LandingRecheckFailed,
                    payload,
                )?;
                HELD
            }
            Some(_) => return Ok(None),
        };
        warn!(run_id = %run.id(), "run {}: {reason}; {}", run.id(), if action == HELD {
            "it is parked for a resume instead of landing"
        } else {
            "it is resumed without waiting for its asks"
        });
        let note = recheck::ask_note(&reason, action == recheck::RESUMED);
        for ask in self
            .queue
            .note_on_asks(run.id(), &note, LANDING_RECHECK_FAILED)?
        {
            info!(run_id = %run.id(), ask_id = %ask.id, "run {}: the landing recheck's finding was added to its {} ask {}", run.id(), ask.kind.as_str(), ask.id);
        }
        Ok(Some(action))
    }

    /// Park `run`, which this supervisor holds to land onto `main`, when
    /// the latest recheck found its head not landing on that main
    /// (ADR-0068 decision 3): the landing would only fail the same way.
    /// The parked run, or `None` to land it.
    pub(super) fn park_held_by_recheck(
        &mut self,
        run: &TaskRun,
        main: &CommitSha,
    ) -> Result<Option<TaskRun>> {
        let Some(head) = run.result_commit() else {
            return Ok(None);
        };
        let events = self.queue.run_events(run.id())?;
        let Some(held) = recheck::held_against(&events, main.as_str(), head.as_str()) else {
            return Ok(None);
        };
        let reason = held.payload["reason"]
            .as_str()
            .unwrap_or("the landing recheck found that the run no longer lands on main")
            .to_owned();
        let mut payload = held.payload.clone();
        if let Some(fields) = payload.as_object_mut() {
            for key in ["action", "reason", "status"] {
                fields.remove(key);
            }
        }
        payload["repeat"] = json!(true);
        let parked = self.queue.park_rechecked(
            run.id(),
            Some(self.registration.token()),
            &reason,
            payload,
        )?;
        if parked.is_some() {
            info!(run_id = %run.id(), "run {}: parked for a resume instead of landing onto main {main}: {reason}", run.id());
        }
        Ok(parked)
    }
}

/// Why the commands of the runs in a container have no container to run
/// in (ADR-t2424-1), given the runtimes `dagq.toml` sets (`runtimes`) and
/// whether the container runtime can run a target (`available`): the
/// runtimes cannot be read, the verification is set to the host, or its
/// container cannot be used. `None` when it can run them.
fn container_refusal(
    runtimes: Result<crate::domain::container_runtime::RuntimeConfig>,
    available: impl Fn(
        crate::domain::container_runtime::RuntimeTarget,
    ) -> crate::domain::container_runtime::Availability,
) -> Option<String> {
    use crate::domain::container_runtime::{ContainerCheck, Runtime, RuntimeTarget};
    let runtimes = match runtimes {
        Ok(runtimes) => runtimes,
        Err(error) => {
            return Some(format!(
                "the runtimes of dagq.toml cannot be read: {error:#}"
            ));
        }
    };
    if runtimes.verification == Runtime::Host {
        return Some(
            "dagq.toml sets the verification's runtime to the host, whose verification does not run the code of a run in a container".to_owned(),
        );
    }
    ContainerCheck::of(&runtimes, available)
        .why(RuntimeTarget::Verification)
        .map(|why| format!("the verification's container cannot be used: {why}"))
}

/// What a recheck checks its targets with, on its thread.
struct Recheck<'a> {
    repository: &'a dyn Repository,
    verifier: &'a dyn Verifier,
    files: &'a dyn RunFiles,
    main: &'a CommitSha,
    config: &'a RecheckConfig,
    dir: &'a Path,
    look: &'a GuardLook,
    /// Where the commands of the runs in a container run; `None` when no
    /// command of such a run is to run.
    in_container: Option<&'a InContainer>,
    /// The name of the file the pass's write check creates and removes in
    /// each build output ([`write_probe`]).
    probe: &'a str,
    /// The latest record of the command on main alone is a failure (the
    /// inbox's attention stands): main is checked in a pass that ran the
    /// command on a run on the host even when none failed it, so a fixed
    /// main ends the attention.
    main_failed_open: bool,
}

/// What came of a run's check before its command: found already (a
/// conflict, no command, a diff off `[recheck] paths`, no container to run
/// in), or its command is to run.
enum Prepared {
    Found(Finding),
    Command(ToRun),
}

/// A run whose command is to run.
struct ToRun {
    /// Main's tree with the run merged in, committed on top of main.
    merged: CommitSha,
    /// The profiles the repository declares, which read the output.
    toolchains: crate::domain::toolchain::Toolchains,
    /// The build outputs of each run of the command on the host
    /// ([`crate::domain::toolchain::Toolchains::build_runs`]); `None` for a
    /// run in a container, which builds in its container.
    host: Option<Vec<Vec<BuildDir>>>,
}

/// The build outputs under `dir` of every run of `prepared` whose command
/// is to run on the host, each once, in the order the runs come: what a
/// pass checks ([`pass_writes`]) before any of its commands runs. None
/// when no command is to run on the host (no command, every run in
/// conflict or off `[recheck] paths`, only runs in a container) or no
/// declaration applies.
fn host_outputs(prepared: &[Result<Prepared>], dir: &Path) -> Vec<PathBuf> {
    let mut outputs: Vec<PathBuf> = Vec::new();
    for prepared in prepared {
        let Ok(Prepared::Command(ToRun {
            host: Some(runs), ..
        })) = prepared
        else {
            continue;
        };
        for output in runs.iter().flatten().map(|output| output.dir_under(dir)) {
            if !outputs.contains(&output) {
                outputs.push(output);
            }
        }
    }
    outputs
}

/// The write check of a pass over `outputs` ([`host_outputs`]) with
/// `check`, until one is unwritable: `None` with none to check. Once one
/// is unwritable, no command of the pass runs on the host; a run in a
/// container builds in its container, not here, and is neither checked
/// nor stopped.
fn pass_writes(outputs: &[PathBuf], check: impl Fn(&Path) -> TargetCheck) -> Option<TargetCheck> {
    let mut found = None;
    for output in outputs {
        let checked = check(output);
        let writable = checked == TargetCheck::Writable;
        found = Some(checked);
        if !writable {
            break;
        }
    }
    found
}

/// The name of the file a pass's write check ([`check_writable`]) creates
/// and removes in a build output, unique to the pass by `id`, so it never
/// names what a command or an earlier pass left there.
fn write_probe(id: &str) -> String {
    format!(".dagq-write-check-{id}")
}

/// Whether the recheck can write in the build output `dir` (`<queue
/// dir>/recheck/target` for the Rust profile): it is made if missing, and
/// the file `probe` ([`write_probe`]) is created in it only where nothing
/// of that name is ([`RunFiles::create_new_file`]: an existing file or
/// link is neither written nor followed), then removed; only a file this
/// check created is removed. A command that cannot write where
/// it builds fails on every run whatever the run's code, as the cargo of a
/// supervisor started in another's sandbox did with `Operation not
/// permitted` on `target/debug/deps`. Unwritable names the path that
/// failed and the OS's error.
fn check_writable(files: &dyn RunFiles, dir: &Path, probe: &str) -> TargetCheck {
    let unwritable = |path: &Path, error: std::io::Error| TargetCheck::Unwritable {
        path: path.display().to_string(),
        error: error.to_string(),
    };
    if let Err(error) = files.create_dir_all(dir) {
        return unwritable(dir, error);
    }
    let probe = dir.join(probe);
    if let Err(error) = files.create_new_file(&probe, b"dagq") {
        return unwritable(&probe, error);
    }
    if let Err(error) = files.remove_file(&probe) {
        return unwritable(&probe, error);
    }
    TargetCheck::Writable
}

/// Check each target against `main` (ADR-0068 decision 2), in two steps.
/// First each one up to its command ([`prepare_run`]): `git merge-tree`;
/// a clean merge, when there is a `command` and the merged tree differs
/// from main in a path of `[recheck] paths` (or there are none,
/// ADR-t2032-1), is committed on top of main. Then, before any command
/// runs, the build outputs of every run whose command runs on the host
/// are checked ([`pass_writes`]): when one cannot be written, no command
/// runs on the host and those runs are [`Finding::Unwritable`]. Then the
/// commands, one after another ([`run_command`]), each output in
/// `recheck-<main>.log` of the run directory. Once every target is
/// checked, a pass whose command failed a run on the host for the code
/// ([`recheck::checks_main`]) runs it on main alone ([`check_main`]) with
/// the first such run's `[run.env]`, and the findings are recorded as it
/// says ([`recheck::kept_by_main`]); while main's failure is the inbox's
/// attention, a pass whose command passed on a run on the host checks
/// main too, with that run's `[run.env]`.
fn recheck_runs(recheck: &Recheck<'_>, targets: Vec<Target>) -> Found {
    let prepared: Vec<Result<Prepared>> = targets
        .iter()
        .map(|target| prepare_run(recheck, target))
        .collect();
    let writes = pass_writes(&host_outputs(&prepared, recheck.dir), |output| {
        check_writable(recheck.files, output, recheck.probe)
    });
    let unwritable = matches!(writes, Some(TargetCheck::Unwritable { .. }));
    let checked = targets
        .into_iter()
        .zip(prepared)
        .map(|(target, prepared)| {
            let mut removed = None;
            let finding = match prepared {
                Ok(Prepared::Found(finding)) => Ok(finding),
                Ok(Prepared::Command(ToRun { host: Some(_), .. })) if unwritable => {
                    Ok(Finding::Unwritable)
                }
                Ok(Prepared::Command(to_run)) => {
                    run_command(recheck, &target, to_run, &mut removed)
                }
                Err(error) => Err(error),
            }
            .unwrap_or_else(|error| Finding::Error(format!("{error:#}")));
            (target, finding, removed)
        })
        .collect::<Vec<Checked>>();
    let failed = checked
        .iter()
        .find_map(|(target, finding, _)| match finding {
            Finding::Failed(failure)
                if recheck::checks_main(failure, target.container.is_none()) =>
            {
                Some(target)
            }
            _ => None,
        });
    // With the attention open, a pass whose command passed on a run on the
    // host checks main too, so a fixed main ends it.
    let passed = || {
        checked.iter().find_map(|(target, finding, _)| {
            (target.container.is_none()
                && matches!(
                    finding,
                    Finding::Clean {
                        skipped_by_paths: false
                    }
                ))
            .then_some(target)
        })
    };
    let first = failed.or_else(|| {
        (recheck.main_failed_open && recheck.config.command.is_some())
            .then(passed)
            .flatten()
    });
    let main = first.map(|first| check_main(recheck, first));
    (checked, writes, main)
}

/// Run the command on main's tree alone, no run merged in (ADR-t2141-1),
/// as on a run on the host ([`run_on_host`]): in the same scratch
/// worktree and build outputs, with the `[run.env]` of `first` (the first
/// run that failed it, or that passed it while main's failure is the
/// inbox's attention), its output in `main-<main's first 12>.log` of the
/// recheck's directory. Git or the command not run, its time limit or a
/// failure on the host judge nothing ([`MainCheck::Unjudged`]).
/// One log per main would pile up as main moves, so once the pass is
/// recorded the logs of earlier checks are removed
/// ([`Supervisor::remove_stale_main_logs`]), but for those somebody is
/// pointed at ([`recheck::stale_main_logs`]): the one written, the
/// attention's while main's failure is the latest record of it, and
/// those the open asks point at. The asks are needed apart from the
/// attention: a failure after a failure records no event, so the event
/// may point at an older main than a run's asks do, and those asks keep
/// pointing there when the run is not recorded on a later pass.
fn check_main(recheck: &Recheck<'_>, first: &Target) -> MainCheck {
    let checked = || -> Result<MainCheck> {
        let command = recheck
            .config
            .command
            .as_deref()
            .context("no recheck command")?;
        let main = recheck.main;
        let toolchains = recheck.verifier.toolchains()?;
        let runs = toolchains.build_runs(|| recheck.repository.tree_paths(main.as_str()))?;
        let log = recheck.dir.join(recheck::main_log_name(main));
        let log_path = path_text(&log)?;
        let exit = match run_on_host(recheck, &runs, main, None, &first.run_dir, command, &log)? {
            Ok(exit) if exit.success => return Ok(MainCheck::Passed { log_path }),
            Ok(exit) => exit,
            Err(error) => return Ok(MainCheck::Unjudged(format!("{error:#}"))),
        };
        let output = recheck
            .files
            .read(&log)
            .map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
            .unwrap_or_default();
        let failure =
            verify_failure::classify(&toolchains, command, exit.code, exit.signal, &output);
        if failure.class.is_environmental() {
            return Ok(MainCheck::Unjudged(format!(
                "it failed on the host ({}: {}); see {log_path}",
                failure.class.as_str(),
                failure.evidence
            )));
        }
        Ok(MainCheck::Failed {
            exit_code: exit.code.unwrap_or(128),
            log_path,
            failure,
        })
    };
    checked().unwrap_or_else(|error| MainCheck::Unjudged(format!("{error:#}")))
}

/// Run `command` on the host on `commit` in the scratch worktree under the
/// recheck's directory, with the environment of [`recheck_env`] from the
/// `[run.env]` of `run_dir`, once for each of `runs` (the build outputs
/// of each run, [`crate::domain::toolchain::Toolchains::build_runs`]),
/// checked out again before each and no further once one fails, its
/// output in `log`, as `run`'s process when it checks a run's merge. The
/// outer `Err` when Git or the setup failed; the inner one is the
/// command's own (its time limit included).
fn run_on_host(
    recheck: &Recheck<'_>,
    runs: &[Vec<BuildDir>],
    commit: &CommitSha,
    run: Option<&RunId>,
    run_dir: &Path,
    command: &str,
    log: &Path,
) -> Result<Result<crate::application::Exit>> {
    let Recheck {
        verifier,
        files,
        dir,
        look,
        ..
    } = *recheck;
    files.create_dir_all(dir)?;
    let resolved = files
        .canonicalize(dir)
        .with_context(|| format!("resolve {}", dir.display()))?;
    let scratch = scratch_worktree(&resolved);
    // A scratch left under another name goes with its record, so no
    // directory left names a record another takes: the old layout's
    // `worktree`, or one named after the queue's path before it moved.
    for left in files.read_dir(&resolved).unwrap_or_default() {
        let Some(name) = left.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        if left != scratch
            && (name == crate::domain::LEGACY_WORKTREE_NAME
                || name.starts_with(crate::domain::RECHECK_SCRATCH_PREFIX))
        {
            recheck.repository.remove_scratch(&left)?;
        }
    }
    let mut status = None;
    // Each run on the tree as committed, until one fails.
    for outputs in runs {
        recheck
            .repository
            .checkout_scratch(&scratch, commit.as_str())?;
        for output in outputs {
            files.create_dir_all(&output.dir_under(dir))?;
        }
        let env = recheck_env(verifier.run_env(run_dir)?, dir, outputs, look)?;
        // A run's merge is its run's process while it runs; main's own
        // check is no run's.
        let ran = match run {
            Some(run) => verifier.run_to_log_for(run, command, &scratch, &env, log),
            None => verifier.run_to_log(command, &scratch, &env, log),
        };
        let passed = matches!(&ran, Ok(exit) if exit.success);
        status = Some(ran);
        if !passed {
            break;
        }
    }
    status.context("no run of the recheck's command")
}

/// A run's check up to its command: what was found without one, or what
/// the command needs. Nothing runs or is made on the host but the merged
/// commit.
fn prepare_run(recheck: &Recheck<'_>, target: &Target) -> Result<Prepared> {
    let Recheck {
        verifier,
        main,
        config,
        ..
    } = *recheck;
    let guarded;
    let repository = if target.container.is_some() {
        guarded = recheck.repository.guarded()?;
        &*guarded
    } else {
        recheck.repository
    };
    let tree = match repository.merged_tree(main.as_str(), target.head.as_str())? {
        Ok(tree) => tree,
        Err(paths) => {
            return Ok(Prepared::Found(Finding::Failed(RecheckFailure::Conflict {
                paths,
            })));
        }
    };
    let Some(command) = config.command.as_deref() else {
        return Ok(Prepared::Found(Finding::Clean {
            skipped_by_paths: false,
        }));
    };
    if !config.paths.is_empty()
        && !recheck::runs_command(
            &config.paths,
            &repository.changed_paths(main.as_str(), &tree)?,
        )
    {
        return Ok(Prepared::Found(Finding::Clean {
            skipped_by_paths: true,
        }));
    }
    // Refused before anything is made for it.
    let refusal = match (&target.container, recheck.in_container) {
        (Some(_), Some(InContainer::Refused(why))) => Some(why.clone()),
        (Some(_), None) => Some("no container was looked at".to_owned()),
        _ => None,
    };
    if let Some(why) = refusal {
        return Ok(Prepared::Found(Finding::Failed(
            RecheckFailure::ContainerRefused {
                command: command.to_owned(),
                why,
            },
        )));
    }
    let merged = repository.commit_tree(
        &tree,
        main.as_str(),
        &[format!(
            "dagq landing recheck of run {} on main {main}",
            target.run_id
        )],
    )?;
    // The profiles the repository declares read the output and, on the
    // host, place the build output (ADR-t2268-1), resolved before the
    // command so a mistake in them does not cost it.
    let toolchains = verifier.toolchains()?;
    let host = match target.container {
        Some(_) => None,
        None => Some(toolchains.build_runs(|| repository.tree_paths(merged.as_str()))?),
    };
    Ok(Prepared::Command(ToRun {
        merged,
        toolchains,
        host,
    }))
}

/// Run the command of `target` as `to_run` says ([`prepare_run`]). A run
/// on the host has the merged commit checked out in the scratch worktree
/// under `dir` and the command run there in `/bin/sh` with the environment
/// of [`recheck_env`], once for each of its build runs (checked out again
/// before each, and no further once one fails). A run in a container has
/// it run in a verification's container, as `in_container` says
/// ([`run_in_container`]). `removed` is set to the port of the sccache
/// whose wrapper a container's command was run without.
fn run_command(
    recheck: &Recheck<'_>,
    target: &Target,
    to_run: ToRun,
    removed: &mut Option<u16>,
) -> Result<Finding> {
    let Recheck {
        verifier,
        files,
        main,
        config,
        ..
    } = *recheck;
    let command = config
        .command
        .as_deref()
        .context("no command to run in the recheck")?;
    let ToRun {
        merged,
        toolchains,
        host,
    } = to_run;
    let refused = |why: String| {
        Ok(Finding::Failed(RecheckFailure::ContainerRefused {
            command: command.to_owned(),
            why,
        }))
    };
    let log = target
        .run_dir
        .join(format!("recheck-{}.log", &main.as_str()[..12]));
    let status = match (host, &target.container, recheck.in_container) {
        (Some(runs), _, _) => run_on_host(
            recheck,
            &runs,
            &merged,
            Some(&target.run_id),
            &target.run_dir,
            command,
            &log,
        )?,
        (None, Some(run), Some(InContainer::Run { containers, tokens })) => {
            let ran = run_in_container(
                &InRecheck {
                    containers: &**containers,
                    tokens: &**tokens,
                    verifier,
                },
                run,
                &merged,
                command,
                &log,
                removed,
            );
            match ran {
                // Its limit is the command's, as on the host.
                Err(error) if error.downcast_ref::<CommandTimedOut>().is_none() => {
                    return refused(format!("{error:#}"));
                }
                // podman made or started no container: the command did not
                // run, so nothing was judged.
                Ok(exit) if crate::domain::run_container::podman_failed(exit.code) => {
                    return refused(format!(
                        "podman did not start the container ({exit}); see {}",
                        log.display()
                    ));
                }
                status => status,
            }
        }
        (None, _, _) => return refused("no container was looked at".to_owned()),
    };
    let (code, signal, timed_out) = match status {
        Ok(exit) if exit.success => {
            return Ok(Finding::Clean {
                skipped_by_paths: false,
            });
        }
        Ok(exit) => (exit.code, exit.signal, None),
        Err(error) => match error.downcast_ref::<CommandTimedOut>() {
            // Killed at the limit (SIGKILL): integrate's `timeout`.
            Some(timed_out) => (None, Some(9), Some(*timed_out)),
            None => return Err(error),
        },
    };
    // Lossy: a log cut off by a kill or a full disk may end mid-character,
    // and its marks still count.
    let output = files
        .read(&log)
        .map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
        .unwrap_or_default();
    let failure = match timed_out {
        Some(timed_out) => timed_out.failure(),
        None => verify_failure::classify(&toolchains, command, code, signal, &output),
    };
    Ok(Finding::Failed(RecheckFailure::CheckFailed {
        command: command.to_owned(),
        exit_code: code.unwrap_or(128),
        log_path: path_text(&log)?,
        output_tail: tail(&output, 2000).to_owned(),
        failure,
    }))
}

/// Run `command` of the run `run` in a container in the dagq machine on a
/// clone of `merged`, main with the run merged in (ADR-t2424-1), as
/// `integrate`'s verification runs its commands: with a token of the
/// recheck's own that reaches the registries alone, the run's `[run.env]`
/// without the sccache wrapper (no protected cache, ADR-t2114-4 decision
/// 4), no cache of the workers' or the host's and no materials (the
/// verification's empty staged directory in their place, so nothing is
/// written in the run directory but the log), its output in `log`. What
/// the container left and the token go when it ends, however it ends.
/// `Err` when it could not be made ready or started. `removed` is set to
/// the port of the sccache whose wrapper the command ran without.
fn run_in_container(
    with: &InRecheck<'_>,
    run: &TaskRun,
    merged: &CommitSha,
    command: &str,
    log: &Path,
    removed: &mut Option<u16>,
) -> Result<crate::application::Exit> {
    let InRecheck {
        containers,
        tokens,
        verifier,
    } = *with;
    use crate::application::queue_broker::{TokenFor, VerificationToken};
    use crate::application::run_container::{PreparedVerification, verification_line};
    let run_dir = Path::new(run.run_dir().context("missing run directory")?);
    let token = VerificationToken::issue(tokens, run.id(), TokenFor::Recheck)?;
    let prepared = containers
        .prepare(run, merged, &token.file)
        .context("make the verification's container ready")?;
    let prepared = PreparedVerification {
        materials: prepared.staged.clone(),
        ..prepared
    };
    let finished = Finished {
        containers,
        prepared: &prepared,
        run: run.id(),
    };
    // Its memory is reserved before its command as a verification's, and
    // forgotten when what it left goes (ADR-t3163-1 decision 1).
    containers
        .reserve_memory(run, &prepared, &mut |_, note| {
            if note != crate::domain::machine_memory::MemoryNote::Still {
                info!(run_id = %run.id(), "run {}: its landing recheck's container and the dagq machine's memory: {note:?}", run.id());
            }
        })
        .context("reserve the dagq machine's memory for the landing recheck's container")?;
    let mut env = verifier.run_env(run_dir)?;
    let wrapper = crate::domain::run_container::without_sccache_wrapper(&mut env).map(|t| t.port);
    let line = verification_line(containers, &prepared, run.id(), command, &env)?;
    let status = verifier.run_to_log_for(run.id(), &line, run_dir, &[], log);
    // Only a command that ran went without the wrapper.
    let ran = match &status {
        Ok(exit) => !crate::domain::run_container::podman_failed(exit.code),
        Err(error) => error.downcast_ref::<CommandTimedOut>().is_some(),
    };
    if ran {
        *removed = wrapper;
    }
    drop(finished);
    drop(token);
    status
}

/// What runs a recheck's command in a container.
struct InRecheck<'a> {
    containers: &'a dyn crate::application::run_container::VerificationContainers,
    tokens: &'a dyn crate::application::queue_broker::VerificationTokens,
    verifier: &'a dyn Verifier,
}

/// Removes what a recheck's container left when it goes.
struct Finished<'a> {
    containers: &'a dyn crate::application::run_container::VerificationContainers,
    prepared: &'a crate::application::run_container::PreparedVerification,
    run: &'a RunId,
}

impl Drop for Finished<'_> {
    fn drop(&mut self) {
        if let Err(error) = self.containers.finish(self.prepared) {
            warn!(run_id = %self.run, "run {}: what the landing recheck's container {} left could not all be removed: {error:#}", self.run, self.prepared.id);
        }
    }
}

/// The command's environment on the host in one of its runs: the run's
/// `[run.env]` with each of `outputs` (the build outputs of that run of
/// the declarations that apply to the merged tree,
/// [`crate::domain::toolchain::Toolchains::build_runs`])
/// pointed at its directory under `dir` over the `[run.env]` value of its
/// variable, refused the sccache server's start and guarded as `look` says
/// (ADR-t2086-1). With no output (a repository that declares profiles of
/// which none applies) nothing is added: the command builds where
/// `[run.env]` says.
fn recheck_env(
    mut env: Vec<(String, String)>,
    dir: &Path,
    outputs: &[BuildDir],
    look: &GuardLook,
) -> Result<Vec<(String, String)>> {
    for output in outputs {
        env.retain(|(key, _)| key != output.env);
        env.push((output.env.to_owned(), path_text(&output.dir_under(dir))?));
    }
    look.apply(&mut env);
    Ok(env)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::sccache::REFUSED_ERROR_LOG;
    use crate::domain::toolchain::{fake, rust};

    fn pairs(items: &[(&str, &str)]) -> Vec<(String, String)> {
        items
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect()
    }

    /// The write check makes the build output, creates its file in it and
    /// removes it, leaving nothing behind. A file already there under the
    /// probe's name is neither written nor removed: the check found it
    /// cannot create its own.
    #[test]
    fn a_writable_build_output_is_checked_and_left_as_it_was() {
        use crate::application::memory_files::MemoryFiles;
        let files = MemoryFiles::default();
        let target = Path::new("/q/recheck/target");
        let probe = write_probe("pass-1");
        assert_eq!(
            check_writable(&files, target, &probe),
            TargetCheck::Writable
        );
        assert!(!files.exists(&target.join(&probe)));
        let left = target.join(write_probe("pass-2"));
        files.put(&left, std::time::SystemTime::UNIX_EPOCH, "left");
        let found = check_writable(&files, target, &write_probe("pass-2"));
        assert!(
            matches!(&found, TargetCheck::Unwritable { path, .. } if Path::new(path) == left),
            "{found:?}"
        );
        assert_eq!(files.bytes(&left).unwrap(), b"left");
        assert_ne!(write_probe("pass-1"), write_probe("pass-2"));
    }

    /// The build outputs a pass checks are those of the runs whose command
    /// runs on the host, each once, in the runs' order: none for a run
    /// found without a command (a conflict, no command, a diff off
    /// `[recheck] paths`), a run in a container or one that could not be
    /// prepared, so a pass whose runs all go without a command checks
    /// nothing.
    #[test]
    fn a_pass_checks_the_build_outputs_of_the_runs_whose_command_runs_on_the_host() {
        let dir = Path::new("/q/recheck");
        let core = rust::BUILD_OUTPUT.declared("core", "rust");
        let rust = rust::BUILD_OUTPUT.declared("rust", "rust");
        let command = |host: Option<Vec<Vec<BuildDir>>>| {
            Ok(Prepared::Command(ToRun {
                merged: CommitSha::parse("a".repeat(40), "commit").unwrap(),
                toolchains: crate::domain::toolchain::Toolchains::none(),
                host,
            }))
        };
        let without_command = || {
            vec![
                Ok(Prepared::Found(Finding::Clean {
                    skipped_by_paths: false,
                })),
                Ok(Prepared::Found(Finding::Clean {
                    skipped_by_paths: true,
                })),
                Ok(Prepared::Found(Finding::Failed(RecheckFailure::Conflict {
                    paths: vec!["change.txt".into()],
                }))),
                command(None),
                Err(anyhow::anyhow!("git failed")),
            ]
        };
        assert!(host_outputs(&without_command(), dir).is_empty());
        let mut prepared = without_command();
        // A run with no declaration that applies comes first: the outputs
        // of the runs after it are checked before its command runs.
        prepared.push(command(Some(vec![vec![]])));
        prepared.push(command(Some(vec![vec![core.clone()], vec![rust.clone()]])));
        prepared.push(command(Some(vec![vec![rust]])));
        assert_eq!(
            host_outputs(&prepared, dir),
            [dir.join("target-core"), dir.join("target")]
        );
    }

    /// The outputs are checked in order until one is unwritable; with none
    /// to check, nothing is found.
    #[test]
    fn a_pass_stops_checking_at_its_first_unwritable_build_output() {
        let checked = std::cell::RefCell::new(Vec::new());
        let target = PathBuf::from("/q/recheck/target");
        let other = PathBuf::from("/q/recheck/target-other");
        let last = PathBuf::from("/q/recheck/target-last");
        let unwritable = TargetCheck::Unwritable {
            path: other.display().to_string(),
            error: "Operation not permitted (os error 1)".into(),
        };
        let check = |output: &Path| {
            checked.borrow_mut().push(output.to_owned());
            if output == other {
                unwritable.clone()
            } else {
                TargetCheck::Writable
            }
        };
        assert_eq!(pass_writes(&[], check), None);
        assert_eq!(
            pass_writes(std::slice::from_ref(&target), check),
            Some(TargetCheck::Writable)
        );
        checked.borrow_mut().clear();
        assert_eq!(
            pass_writes(&[target.clone(), other.clone(), last], check),
            Some(unwritable.clone())
        );
        assert_eq!(*checked.borrow(), [target, other]);
    }

    #[test]
    fn the_recheck_command_is_refused_the_servers_start_and_guarded_as_looked() {
        let dir = Path::new("/q/recheck");
        let rust = rust::BUILD_OUTPUT.declared("rust", "rust");
        let rust = [rust];
        let run_env = || pairs(&[("RUSTC_WRAPPER", "sccache"), (rust[0].env, "/elsewhere")]);
        let guard = GuardLook::Guard("/q/recheck/dagq-rustc-wrapper".into());
        assert_eq!(
            recheck_env(run_env(), dir, &rust, &guard).unwrap(),
            pairs(&[
                (rust[0].env, "/q/recheck/target"),
                ("SCCACHE_ERROR_LOG", REFUSED_ERROR_LOG),
                ("RUSTC_WRAPPER", "/q/recheck/dagq-rustc-wrapper"),
                ("DAGQ_SCCACHE_PROGRAM", "sccache"),
            ])
        );
        let unconfirmed = GuardLook::Unconfirmed {
            port: 4226,
            why: "no sccache server listens on port 4226".into(),
        };
        assert_eq!(
            recheck_env(run_env(), dir, &rust, &unconfirmed).unwrap(),
            pairs(&[
                (rust[0].env, "/q/recheck/target"),
                ("SCCACHE_ERROR_LOG", REFUSED_ERROR_LOG),
            ])
        );
        assert_eq!(
            recheck_env(run_env(), dir, &rust, &GuardLook::NotConfigured).unwrap(),
            pairs(&[
                ("RUSTC_WRAPPER", "sccache"),
                (rust[0].env, "/q/recheck/target"),
            ])
        );
    }

    /// The build outputs are those of the declarations (ADR-t2268-1): none
    /// adds nothing and leaves `[run.env]`'s value; a declaration of a
    /// second profile adds its own variable and directory and leaves the
    /// Rust one alone; each declaration has its own directory.
    #[test]
    fn the_recheck_env_points_each_declarations_variable_at_its_directory() {
        let dir = Path::new("/q/recheck");
        let rust = rust::BUILD_OUTPUT;
        let web = fake::BUILD_OUTPUT.declared("web", "fake");
        let run_env = || pairs(&[(rust.env, "/elsewhere")]);
        let look = GuardLook::NotConfigured;
        assert_eq!(recheck_env(run_env(), dir, &[], &look).unwrap(), run_env());
        assert_eq!(
            recheck_env(run_env(), dir, std::slice::from_ref(&web), &look).unwrap(),
            pairs(&[
                (rust.env, "/elsewhere"),
                ("FAKE_OUT", "/q/recheck/fake-out-web")
            ])
        );
        assert_eq!(
            recheck_env(run_env(), dir, &[rust.declared("core", "rust"), web], &look).unwrap(),
            pairs(&[
                (rust.env, "/q/recheck/target-core"),
                ("FAKE_OUT", "/q/recheck/fake-out-web"),
            ])
        );
    }

    /// The commands of the runs in a container have a container only when
    /// `dagq.toml` sets the verification to the container and it can be
    /// used; otherwise the refusal says why (ADR-t2424-1).
    #[test]
    fn a_run_in_a_container_has_a_container_only_when_the_verification_can_use_one() {
        use crate::domain::container_runtime::{Availability, Runtime, RuntimeConfig};
        let config = |verification| {
            Ok(RuntimeConfig {
                worker: Runtime::Container,
                verification,
            })
        };
        let usable = |_| Availability::Available;
        let unusable = |_| Availability::Unavailable("the dagq machine is not running".to_owned());
        assert_eq!(container_refusal(config(Runtime::Container), usable), None);
        let host = container_refusal(config(Runtime::Host), usable).unwrap();
        assert!(
            host.contains("sets the verification's runtime to the host"),
            "{host}"
        );
        let down = container_refusal(config(Runtime::Container), unusable).unwrap();
        assert!(
            down.ends_with("cannot be used: the dagq machine is not running"),
            "{down}"
        );
        let unread = container_refusal(Err(anyhow::anyhow!("bad toml")), usable).unwrap();
        assert!(unread.ends_with("cannot be read: bad toml"), "{unread}");
    }

    fn sha(c: char) -> CommitSha {
        CommitSha::parse(c.to_string().repeat(40), "commit").unwrap()
    }

    fn target(run: &str, head: char, held: bool) -> Target {
        Target {
            run_id: RunId::new(run).unwrap(),
            head: sha(head),
            run_dir: PathBuf::from(format!("/runs/{run}")),
            held,
            container: None,
        }
    }

    fn finished(main: &CommitSha, supervisor: &str) -> RunEvent {
        RunEvent {
            id: crate::domain::EventId::new(1),
            task_id: None,
            goal_id: None,
            run_id: None,
            kind: recheck::LANDING_RECHECK_FINISHED.into(),
            payload: json!({"main": main, "supervisor": supervisor}),
            created_at: String::new(),
            actor: None,
        }
    }

    /// The latest `landing_recheck_finished` of a look that must not read
    /// it.
    fn unread() -> Result<Option<RunEvent>> {
        unreachable!("the finished recheck is read")
    }

    fn runs(targets: &[Target]) -> Vec<&str> {
        targets.iter().map(|t| t.run_id.as_str()).collect()
    }

    fn checks(look: Look) -> Vec<String> {
        match look {
            Look::Check(targets) => runs(&targets).into_iter().map(str::to_owned).collect(),
            other => panic!("{other:?}"),
        }
    }

    /// Main that moved since the last finished recheck (a direct
    /// `integrate`, a push outside dagq, a landing before this supervisor
    /// started) is checked on the next pass, whatever other supervisor is
    /// registered: the decision reads no registration.
    #[test]
    fn a_main_that_moved_since_the_last_recheck_is_checked() {
        let main = sha('a');
        let none = Rechecks::default();
        let waiting = vec![target("r1", 'b', false)];
        assert!(!unmoved(false, &none, &main));
        assert_eq!(
            checks(
                look_before_lock(false, &none, &main, waiting.clone(), || Ok(None), "me").unwrap()
            ),
            ["r1"]
        );
        let earlier = finished(&sha('c'), "me");
        assert_eq!(
            checks(
                look_before_lock(
                    false,
                    &none,
                    &main,
                    waiting.clone(),
                    || Ok(Some(earlier.clone())),
                    "me"
                )
                .unwrap()
            ),
            ["r1"]
        );
        let by_other = finished(&sha('c'), "another");
        assert_eq!(
            checks(
                look_before_lock(
                    false,
                    &none,
                    &main,
                    waiting,
                    || Ok(Some(by_other.clone())),
                    "me"
                )
                .unwrap()
            ),
            ["r1"]
        );
    }

    /// A main already checked is not checked again, pass after pass and
    /// supervisor after supervisor: this process remembers it, this
    /// supervisor's finished recheck settles it, and another supervisor's
    /// settles it unless this one holds runs to land. This supervisor's
    /// own landing makes it due whatever was checked.
    #[test]
    fn a_main_already_checked_is_not_checked_again() {
        let main = sha('a');
        let checked = Rechecks {
            checked: Some(main.clone()),
            ..Rechecks::default()
        };
        assert!(unmoved(false, &checked, &main));
        assert!(!unmoved(true, &checked, &main));
        assert!(!unmoved(false, &checked, &sha('c')));
        let none = Rechecks::default();
        let waiting = vec![target("r1", 'b', false)];
        let mine = finished(&main, "me");
        assert!(matches!(
            look_before_lock(
                false,
                &none,
                &main,
                waiting.clone(),
                || Ok(Some(mine.clone())),
                "me"
            )
            .unwrap(),
            Look::Checked
        ));
        let other = finished(&main, "another");
        assert!(matches!(
            look_before_lock(
                false,
                &none,
                &main,
                waiting.clone(),
                || Ok(Some(other.clone())),
                "me"
            )
            .unwrap(),
            Look::Idle
        ));
        let held = vec![target("r1", 'b', false), target("r2", 'c', true)];
        assert_eq!(
            checks(
                look_before_lock(false, &none, &main, held, || Ok(Some(other.clone())), "me")
                    .unwrap()
            ),
            ["r1", "r2"]
        );
        // A due recheck does not read the finished one, nor does a look
        // that what this process remembers settles.
        assert_eq!(
            checks(look_before_lock(true, &none, &main, waiting, unread, "me").unwrap()),
            ["r1"]
        );
    }

    /// With no run waiting to land, main is not checked, due or not, and
    /// the finished recheck is not read: the recheck lock is left free.
    #[test]
    fn a_main_with_no_run_waiting_is_not_checked() {
        let main = sha('a');
        let none = Rechecks::default();
        for due in [false, true] {
            assert!(matches!(
                look_before_lock(due, &none, &main, Vec::new(), unread, "me").unwrap(),
                Look::Idle
            ));
        }
    }

    /// A main looked at with nothing to check settles only the runs that
    /// waited then: a run that starts waiting later is checked against the
    /// same main by the same supervisor, unless this one holds it to land.
    #[test]
    fn a_run_that_starts_waiting_after_main_moved_is_checked() {
        let main = sha('a');
        let idle = Rechecks {
            idle: Some((main.clone(), vec![(RunId::new("r1").unwrap(), sha('b'))])),
            ..Rechecks::default()
        };
        let same = vec![target("r1", 'b', false)];
        assert!(matches!(
            look_before_lock(false, &idle, &main, same, unread, "me").unwrap(),
            Look::Idle
        ));
        let later = vec![
            target("r1", 'b', false),
            target("r2", 'c', false),
            target("r3", 'd', true),
        ];
        assert_eq!(
            checks(
                look_before_lock(false, &idle, &main, later.clone(), || Ok(None), "me").unwrap()
            ),
            ["r2"]
        );
        // A new head of the same run is looked at too.
        let rebased = vec![target("r1", 'e', false)];
        assert_eq!(
            checks(look_before_lock(false, &idle, &main, rebased, || Ok(None), "me").unwrap()),
            ["r1"]
        );
        // Idle on another main settles nothing.
        assert_eq!(
            checks(look_before_lock(false, &idle, &sha('f'), later, || Ok(None), "me").unwrap()),
            ["r1", "r2", "r3"]
        );
    }

    /// Under the lock, the runs whose head contains main or was judged
    /// against it are left out, and so is every run when this supervisor
    /// checked main, or all but those it holds when another one did.
    #[test]
    fn the_runs_left_to_check_under_the_lock() {
        let judged = || {
            vec![
                (target("r1", 'b', false), false),
                (target("r2", 'c', false), true),
                (target("r3", 'd', true), false),
            ]
        };
        assert_eq!(runs(&left_to_check(judged(), CheckedBy::No)), ["r1", "r3"]);
        assert_eq!(runs(&left_to_check(judged(), CheckedBy::Other)), ["r3"]);
        assert!(left_to_check(judged(), CheckedBy::This).is_empty());
    }

    /// A run this supervisor holds in its slot, whose only judgement for
    /// main and its head is another supervisor's clean with no command,
    /// after that supervisor checked main: (a) with no paths-skip mark
    /// and a command with no `[recheck] paths`, (b) with paths the merged
    /// diff touches, it is checked and the command runs; (c) with paths it
    /// touches none of, it is checked and recorded clean skipped by the
    /// paths; (d) with no command here, or its program missing, it is
    /// settled; (e) a clean skipped by the paths settles it for a
    /// supervisor with a command.
    #[test]
    fn another_supervisors_clean_with_no_command_settles_a_held_run_by_its_paths_mark() {
        let (main, head) = (sha('a'), sha('b'));
        let clean = |skipped_by_paths: bool| RunEvent {
            id: crate::domain::EventId::new(1),
            task_id: None,
            goal_id: None,
            run_id: None,
            kind: LANDING_RECHECK_CLEAN.into(),
            payload: recheck::clean_payload(None, &main, &head, None, skipped_by_paths),
            created_at: String::new(),
            actor: None,
        };
        let config = |command: Option<&str>, paths: &[&str]| RecheckConfig {
            command: command.map(str::to_owned),
            paths: paths.iter().map(|p| (*p).to_owned()).collect(),
        };
        // What a recheck of the held run does: nothing (settled), the
        // command, or a clean skipped by the paths (`git merge-tree` is
        // clean here).
        let checks = |event: &RunEvent, env_missing: bool, config: RecheckConfig| {
            let config = recheck_config(env_missing, || Ok(config)).unwrap();
            let settled = recheck::judged_against(
                std::slice::from_ref(event),
                main.as_str(),
                head.as_str(),
                config.command.is_some(),
            );
            let left = left_to_check(vec![(target("r1", 'b', true), settled)], CheckedBy::Other);
            if left.is_empty() {
                return "settled";
            }
            match config.command {
                None => "merge only",
                Some(_) if recheck::runs_command(&config.paths, &["src/lib.rs".to_owned()]) => {
                    "command"
                }
                Some(_) => "skipped by paths",
            }
        };
        let unmarked = clean(false);
        assert_eq!(
            checks(&unmarked, false, config(Some("make"), &[])),
            "command"
        );
        assert_eq!(
            checks(&unmarked, false, config(Some("make"), &["src/**"])),
            "command"
        );
        assert_eq!(
            checks(&unmarked, false, config(Some("make"), &["docs/**"])),
            "skipped by paths"
        );
        assert_eq!(checks(&unmarked, false, config(None, &[])), "settled");
        assert_eq!(
            checks(&unmarked, true, config(Some("make"), &[])),
            "settled"
        );
        let by_paths = clean(true);
        assert_eq!(
            by_paths.payload["command_skipped"],
            recheck::SKIPPED_BY_PATHS
        );
        assert_eq!(
            checks(&by_paths, false, config(Some("make"), &[])),
            "settled"
        );
        assert_eq!(
            checks(&by_paths, false, config(Some("make"), &["src/**"])),
            "settled"
        );
    }

    /// The config is not read when [run.env] cannot find the command's
    /// program: the recheck has no command then.
    #[test]
    fn a_missing_program_has_no_command_and_reads_no_config() {
        let set = RecheckConfig {
            command: Some("make".to_owned()),
            paths: vec!["src/**".to_owned()],
        };
        assert_eq!(
            recheck_config(true, || unreachable!("the config is read")).unwrap(),
            RecheckConfig::default()
        );
        assert_eq!(recheck_config(false, || Ok(set.clone())).unwrap(), set);
    }

    /// `landing_recheck_finished` goes on the run whose landing moved main,
    /// or on the first run checked when no dagq landing did.
    #[test]
    fn the_finished_recheck_goes_on_the_landing_or_the_first_run() {
        let targets = [target("r1", 'b', false), target("r2", 'c', false)];
        let landed = Landed {
            run_id: RunId::new("landed").unwrap(),
            task_id: crate::domain::TaskId::new(2),
        };
        assert_eq!(record_on(Some(&landed), &targets).as_str(), "landed");
        assert_eq!(record_on(None, &targets).as_str(), "r1");
    }

    /// `landing_recheck_finished` counts every finding, the clean ones
    /// skipped by `[recheck] paths` among them, and lists the failures
    /// recorded with what followed; a failure not recorded (the run moved
    /// on) is counted but not listed.
    #[test]
    fn the_finished_recheck_counts_its_findings() {
        let mut tally = Tally {
            main_check: Some("failed"),
            ..Tally::default()
        };
        tally.clean(false);
        tally.clean(true);
        tally.error();
        let conflict = RecheckFailure::Conflict {
            paths: vec!["change.txt".into()],
        };
        let check = |class| RecheckFailure::CheckFailed {
            command: "exit 1".into(),
            exit_code: 1,
            log_path: "/r/log".into(),
            output_tail: String::new(),
            failure: verify_failure::VerifyFailure {
                class,
                evidence: String::new(),
                profile: None,
            },
        };
        let (r1, r2, r3, r4, r5) = (
            RunId::new("r1").unwrap(),
            RunId::new("r2").unwrap(),
            RunId::new("r3").unwrap(),
            RunId::new("r4").unwrap(),
            RunId::new("r5").unwrap(),
        );
        tally.failed(&r1, &conflict, Some(recheck::RESUMED), false);
        tally.failed(
            &r2,
            &check(verify_failure::FailureClass::TestFailure),
            Some(HELD),
            false,
        );
        tally.failed(&r3, &conflict, None, false);
        tally.failed(
            &r4,
            &check(verify_failure::FailureClass::DiskFull),
            Some(SKIPPED),
            false,
        );
        // Kept from its resume by main's failure (ADR-t2141-1).
        tally.failed(
            &r5,
            &check(verify_failure::FailureClass::BuildError),
            Some(SKIPPED),
            true,
        );
        let landed = Landed {
            run_id: RunId::new("landed").unwrap(),
            task_id: crate::domain::TaskId::new(2),
        };
        let main = sha('a');
        assert_eq!(
            tally.finished(
                &main,
                Some(&landed),
                Some("exit 1"),
                3,
                &LeaseToken::new("me")
            ),
            json!({
                "main": main,
                "landed_run_id": "landed",
                "landed_task_id": 2,
                "command": "exit 1",
                "duration_secs": 3,
                "failed_runs": [
                    {"run_id": "r1", "code": "rebase_conflict", "action": "resumed"},
                    {"run_id": "r2", "code": "verification_failed", "action": "held"},
                    {"run_id": "r4", "code": "verification_failed", "action": "skipped"},
                    {"run_id": "r5", "code": "verification_failed", "action": "skipped"},
                ],
                "supervisor": "me",
                "checked": 8,
                "clean": 2,
                "command_skipped": 1,
                "conflicts": 2,
                "check_failed": 2,
                "environment": 1,
                "errors": 1,
                "resumed": 1,
                "held": 1,
                "target_unwritable": 0,
                "main_check": "failed",
                "main_failed": 1,
            })
        );
        let none = Tally::default().finished(&main, None, None, 0, &LeaseToken::new("me"));
        assert_eq!(none["landed_run_id"], Value::Null);
        assert_eq!(none["landed_task_id"], Value::Null);
        assert_eq!(none["checked"], 0);
        assert_eq!(none["failed_runs"], json!([]));
        assert!(none.get("command_skipped_reason").is_none());
        assert_eq!(none["main_check"], Value::Null);
        assert_eq!(none["main_failed"], 0);
        // Runs left unrecorded for an unwritable build output are counted
        // apart from `checked`, with why no command ran.
        let unwritable = Tally {
            target_unwritable: 2,
            ..Tally::default()
        }
        .finished(&main, None, Some("exit 1"), 0, &LeaseToken::new("me"));
        assert_eq!(unwritable["checked"], 0);
        assert_eq!(unwritable["target_unwritable"], 2);
        assert_eq!(unwritable["command_skipped_reason"], "target_unwritable");
    }
}
