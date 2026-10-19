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

use super::*;
use crate::domain::EventKind;
use crate::domain::recheck::{
    self, CheckedBy, HELD, LANDING_RECHECK_CLEAN, LANDING_RECHECK_FAILED, Landed, RecheckConfig,
    RecheckFailure, SKIPPED,
};
use crate::domain::sccache::{CheckReason, GuardLook};
use crate::domain::toolchain::{BuildDir, ProfileRegistry};
use crate::domain::verify_failure::{self, CommandTimedOut};

/// Where the recheck keeps its scratch worktree and the build outputs of
/// the declarations it runs with, under the queue's directory.
const RECHECK_DIR: &str = "recheck";

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
pub(super) fn build_dirs(
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
pub(super) fn clear_targets<R>(
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
fn look_before_lock(
    due: bool,
    rechecks: &Rechecks,
    main: &CommitSha,
    mut targets: Vec<Target>,
    finished: impl FnOnce() -> Result<Option<RunEvent>>,
    token: &str,
) -> Result<Look> {
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
    fn failed(&mut self, run_id: &RunId, failure: &RecheckFailure, action: Option<&'static str>) {
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
    /// `command`, how long it took and the supervisor that ran it.
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
    handle: Option<thread::JoinHandle<Vec<Checked>>>,
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
        if self.landing.rechecks.running.is_none() && !self.registration.draining {
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
            self.registration.token.as_str(),
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
                );
            judged.push((target, settled));
        }
        let finished = self
            .queue
            .latest_events_of(recheck::LANDING_RECHECK_FINISHED, CHECKED_LOOKBACK)?;
        let checked =
            recheck::checked_by(&finished, main.as_str(), self.registration.token.as_str());
        let targets = left_to_check(judged, checked);
        if targets.is_empty() {
            self.landing.rechecks.idle = Some((main, seen));
            return Ok(Start::Nothing);
        }
        let landed = self.landing_at(&main)?;
        self.start(landed, main, targets, Box::new((lock, turn)))?;
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
                    if lease.token == self.registration.token
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
        lock: Box<dyn std::any::Any + Send>,
    ) -> Result<()> {
        let record_on = record_on(landed.as_ref(), &targets);
        // A command whose program [run.env] cannot find would fail on every
        // run (ADR-0049 decision 9): only the merge is checked then.
        let config = if self.landing.run_env_missing {
            RecheckConfig::default()
        } else {
            self.verifier.recheck_config()?
        };
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
        let repository = self.repository.clone();
        let verifier = self.verifier.clone();
        let files = self.files.clone();
        let on = main.clone();
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
    fn apply_recheck(&mut self, watch: &RecheckWatch, found: Vec<Checked>) -> Result<()> {
        let mut tally = Tally::default();
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
                Finding::Failed(failure) => failure,
            };
            let action = match self.record_recheck_failure(watch, &target, &failure) {
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
            tally.failed(&target.run_id, &failure, action);
        }
        let payload = tally.finished(
            &watch.main,
            watch.landed.as_ref(),
            watch.command.as_deref(),
            self.generators.clock.since(watch.started).as_secs(),
            &self.registration.token,
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
                .is_some_and(|lease| lease.token != self.registration.token)
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
    /// host failure or not. Every way its open asks get the finding.
    /// `None` when the run moved on since it was checked (another head,
    /// another status, or a lease of another process).
    fn record_recheck_failure(
        &mut self,
        watch: &RecheckWatch,
        target: &Target,
        failure: &RecheckFailure,
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
        if failure.is_environmental() {
            if lease.is_some_and(|lease| lease.token != self.registration.token) {
                return Ok(None);
            }
            payload["action"] = json!(SKIPPED);
            payload["reason"] = json!(reason);
            payload["status"] = json!(run.status());
            self.queue
                .record_runtime_event(run.id(), EventKind::LandingRecheckFailed, payload)?;
            let note = if matches!(failure, RecheckFailure::ContainerRefused { .. }) {
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
            Some(lease) if lease.token == self.registration.token => {
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
            Some(&self.registration.token),
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
}

/// Check each target against `main`, one after another (ADR-0068 decision
/// 2): `git merge-tree` first; a clean merge, when there is a `command`
/// and the merged tree differs from main in a path of `[recheck] paths`
/// (or there are none, ADR-t2032-1), is committed on top of main and the
/// command run on it, its output in `recheck-<main>.log` of the run
/// directory. A run on the host has it checked out in the scratch worktree
/// under `dir` and run there in `/bin/sh` with the environment of
/// [`recheck_env`], once for each of the build runs of the declarations
/// that apply (checked out again before each, and no further once one
/// fails; [`crate::domain::toolchain::Toolchains::build_runs`]). A run in
/// a container has it run in a verification's
/// container, as `in_container` says ([`run_in_container`]).
fn recheck_runs(recheck: &Recheck<'_>, targets: Vec<Target>) -> Vec<Checked> {
    targets
        .into_iter()
        .map(|target| {
            let mut removed = None;
            let finding = recheck_run(recheck, &target, &mut removed)
                .unwrap_or_else(|error| Finding::Error(format!("{error:#}")));
            (target, finding, removed)
        })
        .collect()
}

/// `removed` is set to the port of the sccache whose wrapper a container's
/// command was run without.
fn recheck_run(
    recheck: &Recheck<'_>,
    target: &Target,
    removed: &mut Option<u16>,
) -> Result<Finding> {
    let Recheck {
        verifier,
        files,
        main,
        config,
        dir,
        look,
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
        Err(paths) => return Ok(Finding::Failed(RecheckFailure::Conflict { paths })),
    };
    let Some(command) = config.command.as_deref() else {
        return Ok(Finding::Clean {
            skipped_by_paths: false,
        });
    };
    if !config.paths.is_empty()
        && !recheck::runs_command(
            &config.paths,
            &repository.changed_paths(main.as_str(), &tree)?,
        )
    {
        return Ok(Finding::Clean {
            skipped_by_paths: true,
        });
    }
    let refused = |why: String| {
        Ok(Finding::Failed(RecheckFailure::ContainerRefused {
            command: command.to_owned(),
            why,
        }))
    };
    // Refused before anything is made for it.
    let in_container = match (&target.container, recheck.in_container) {
        (None, _) => None,
        (Some(_), Some(InContainer::Refused(why))) => return refused(why.clone()),
        (Some(_), None) => return refused("no container was looked at".to_owned()),
        (Some(run), Some(InContainer::Run { containers, tokens })) => {
            Some((run, &**containers, &**tokens))
        }
    };
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
    let log = target
        .run_dir
        .join(format!("recheck-{}.log", &main.as_str()[..12]));
    let status = match in_container {
        None => {
            let scratch = dir.join("worktree");
            files.create_dir_all(dir)?;
            let runs = toolchains.build_runs(|| repository.tree_paths(merged.as_str()))?;
            let mut status = None;
            // Each run on the merged tree as committed, until one fails.
            for outputs in &runs {
                repository.checkout_scratch(&scratch, merged.as_str())?;
                for output in outputs {
                    files.create_dir_all(&output.dir_under(dir))?;
                }
                let env = recheck_env(verifier.run_env(&target.run_dir)?, dir, outputs, look)?;
                let ran = verifier.run_to_log(command, &scratch, &env, &log);
                let passed = matches!(&ran, Ok(exit) if exit.success);
                status = Some(ran);
                if !passed {
                    break;
                }
            }
            status.context("no run of the recheck's command")?
        }
        Some((run, containers, tokens)) => {
            let ran = run_in_container(
                &InRecheck {
                    containers,
                    tokens,
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
    let mut env = verifier.run_env(run_dir)?;
    let wrapper = crate::domain::run_container::without_sccache_wrapper(&mut env).map(|t| t.port);
    let line = verification_line(containers, &prepared, run.id(), command, &env)?;
    let status = verifier.run_to_log(&line, run_dir, &[], log);
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
            kind: recheck::LANDING_RECHECK_FINISHED.to_owned(),
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
        let mut tally = Tally::default();
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
        let (r1, r2, r3, r4) = (
            RunId::new("r1").unwrap(),
            RunId::new("r2").unwrap(),
            RunId::new("r3").unwrap(),
            RunId::new("r4").unwrap(),
        );
        tally.failed(&r1, &conflict, Some(recheck::RESUMED));
        tally.failed(
            &r2,
            &check(verify_failure::FailureClass::TestFailure),
            Some(HELD),
        );
        tally.failed(&r3, &conflict, None);
        tally.failed(
            &r4,
            &check(verify_failure::FailureClass::DiskFull),
            Some(SKIPPED),
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
                ],
                "supervisor": "me",
                "checked": 7,
                "clean": 2,
                "command_skipped": 1,
                "conflicts": 2,
                "check_failed": 1,
                "environment": 1,
                "errors": 1,
                "resumed": 1,
                "held": 1,
            })
        );
        let none = Tally::default().finished(&main, None, None, 0, &LeaseToken::new("me"));
        assert_eq!(none["landed_run_id"], Value::Null);
        assert_eq!(none["landed_task_id"], Value::Null);
        assert_eq!(none["checked"], 0);
        assert_eq!(none["failed_runs"], json!([]));
    }
}
