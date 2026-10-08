//! The landing recheck (ADR-0068): after this supervisor lands a run, or
//! whenever main differs from the main the last recheck finished against
//! (a direct `integrate`, a landing before a handoff or a restart, a push
//! outside dagq; ADR-t1310-1), the runs that wait to land are checked
//! against the new main off the loop,
//! one recheck at a time: `git merge-tree` for a conflict, then the
//! `[recheck] command` of `dagq.toml` on main's tree with the run merged
//! in, in one scratch worktree with one target directory of the queue's,
//! when that tree differs from main in a path `[recheck] paths` names (or
//! there are none, ADR-t2032-1).
//! A run found no longer landing is parked for a resume at once, whatever
//! asks it waits on; a run this supervisor holds in a slot is parked when
//! it would land. A run found still landing records that (ADR-t1311-1).
//! Either way the run's open asks show the latest finding.

use super::*;
use crate::domain::EventKind;
use crate::domain::recheck::{
    self, HELD, LANDING_RECHECK_CLEAN, LANDING_RECHECK_FAILED, Landed, RecheckConfig,
    RecheckFailure,
};
use crate::domain::sccache::{CheckReason, GuardLook};

/// Where the recheck keeps its scratch worktree and its target directory,
/// under the queue's directory.
const RECHECK_DIR: &str = "recheck";

/// The lock file in [`RECHECK_DIR`] the queue's supervisors share: one
/// recheck at a time uses the scratch worktree and the target.
const RECHECK_LOCK: &str = "lock";

/// How many of the latest `landing_recheck_finished` tell who checked a
/// main already.
const CHECKED_LOOKBACK: usize = 20;

/// Who checked a main already.
enum Checked {
    No,
    ByThis,
    ByOther,
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
}

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
    handle: Option<thread::JoinHandle<Vec<(Target, Finding)>>>,
    /// The queue's recheck lock, held until the recheck is recorded.
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
        if self.landing.rechecks.running.is_none() && !self.draining {
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
    /// on the recheck (its scratch worktree and target): a recheck starts
    /// only under it, and who checked the same main already is read under
    /// it, so it is not checked twice. Another supervisor's recheck of main
    /// settles only the runs it could see: the runs this one holds to land
    /// are checked still. Nothing starts while a run lands,
    /// and the waiting runs whose head already contains main are left out:
    /// they land as they are.
    fn try_recheck(&mut self, due: bool) -> Result<Start> {
        let main = self.repository.main_head()?;
        if !due && self.landing.rechecks.checked.as_ref() == Some(&main) {
            return Ok(Start::Nothing);
        }
        let mut targets = self.recheck_targets()?;
        let seen: Vec<(RunId, CommitSha)> = targets
            .iter()
            .map(|target| (target.run_id.clone(), target.head.clone()))
            .collect();
        // Looked at this main before with nothing to check: of the runs
        // that wait since, only those nobody leases are looked at. One this
        // supervisor holds to land had its passed review's conflict
        // precheck against main as it is (ADR-0027 decision 4).
        if let Some((idle, before)) = &self.landing.rechecks.idle
            && *idle == main
        {
            targets.retain(|target| {
                !target.held && !before.contains(&(target.run_id.clone(), target.head.clone()))
            });
            if targets.is_empty() {
                self.landing.rechecks.idle = Some((main, seen));
                return Ok(Start::Nothing);
            }
        }
        if !due
            && let Some(event) = self
                .queue
                .latest_event_of(recheck::LANDING_RECHECK_FINISHED)?
            && event.payload["main"] == main.as_str()
        {
            // This supervisor checked main as it is.
            if event.payload["supervisor"] == self.token.as_str() {
                self.landing.rechecks.checked = Some(main);
                return Ok(Start::Nothing);
            }
            // Another supervisor did: it could not see the runs this one
            // holds to land, which are checked below (`Checked::ByOther`).
            if !targets.iter().any(|target| target.held) {
                self.landing.rechecks.idle = Some((main, seen));
                return Ok(Start::Nothing);
            }
        }
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
        let mut left = Vec::with_capacity(targets.len());
        for target in targets {
            if !self
                .repository
                .is_ancestor(main.as_str(), target.head.as_str())
                .unwrap_or(false)
                && !self.prechecked(&target, &main)?
            {
                left.push(target);
            }
        }
        let mut targets = left;
        match self.checked_against(&main)? {
            // This supervisor checked it already (a move seen before its
            // own landing's step was read).
            Checked::ByThis => targets.clear(),
            // Another supervisor did: the runs it could not see, those
            // this one holds in its slots, are left.
            Checked::ByOther => targets.retain(|target| target.held),
            Checked::No => {}
        }
        if targets.is_empty() {
            self.landing.rechecks.idle = Some((main, seen));
            return Ok(Start::Nothing);
        }
        let landed = self.landing_at(&main)?;
        self.start(landed, main, targets, lock)?;
        Ok(Start::Started)
    }

    /// The queue's recheck directory: its scratch worktree, its target
    /// directory and its lock.
    fn recheck_dir(&self) -> Result<PathBuf> {
        Ok(self
            .layout
            .db
            .parent()
            .context("queue database has no directory")?
            .join(RECHECK_DIR))
    }

    /// The dagq landing whose commit `main` is: the newest `run_integrated`,
    /// when it landed that commit.
    fn landing_at(&self, main: &CommitSha) -> Result<Option<Landed>> {
        let Some(event) = self
            .queue
            .latest_event_of(EventKind::RunIntegrated.as_str())?
        else {
            return Ok(None);
        };
        let (Some(run_id), Some(task_id)) = (event.run_id, event.task_id) else {
            return Ok(None);
        };
        Ok((event.payload["result_commit"] == main.as_str()
            || event.payload["commit"] == main.as_str())
        .then_some(Landed { run_id, task_id }))
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
                    if lease.token == self.token
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
            });
        }
        Ok(targets)
    }

    /// Whether the run's head was judged against `main` already: by its
    /// passed review's conflict precheck (ADR-0027 decision 4), by its
    /// landing that parked it, or by a recheck, failed or clean. What that
    /// found is handled (a request to the session, an ask, a resume, or a
    /// landing that parks the run), and is not judged again.
    fn prechecked(&self, target: &Target, main: &CommitSha) -> Result<bool> {
        use crate::domain::event_kind::{CONFLICT_PRECHECK, INTEGRATION_DEFERRED};
        Ok(self.queue.run_events(&target.run_id)?.iter().any(|event| {
            matches!(
                event.kind.as_str(),
                CONFLICT_PRECHECK
                    | INTEGRATION_DEFERRED
                    | LANDING_RECHECK_FAILED
                    | LANDING_RECHECK_CLEAN
            ) && event.payload["main"] == main.as_str()
                && event.payload["head"] == target.head.as_str()
        }))
    }

    /// Who checked `main` already, from the recent `landing_recheck_finished`.
    fn checked_against(&self, main: &CommitSha) -> Result<Checked> {
        let mut checked = Checked::No;
        for event in self
            .queue
            .latest_events_of(recheck::LANDING_RECHECK_FINISHED, CHECKED_LOOKBACK)?
            .into_iter()
            .filter(|event| event.payload["main"] == main.as_str())
        {
            if event.payload["supervisor"] == self.token.as_str() {
                return Ok(Checked::ByThis);
            }
            checked = Checked::ByOther;
        }
        Ok(checked)
    }

    fn start(
        &mut self,
        landed: Option<Landed>,
        main: CommitSha,
        targets: Vec<Target>,
        lock: Box<dyn std::any::Any + Send>,
    ) -> Result<()> {
        let record_on = match &landed {
            Some(landed) => landed.run_id.clone(),
            None => targets[0].run_id.clone(),
        };
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
        // guard lives beside the recheck's target directory.
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
        let repository = self.repository.clone();
        let verifier = self.verifier.clone();
        let files = self.files.clone();
        let on = main.clone();
        let handle = spawn_traced(move || {
            recheck_runs(
                &*repository,
                &*verifier,
                &*files,
                &on,
                &config,
                &dir,
                &look,
                targets,
            )
        });
        self.landing.rechecks.running = Some(RecheckWatch {
            landed,
            record_on,
            main,
            command,
            started: Instant::now(),
            handle: Some(handle),
            _lock: lock,
        });
        Ok(())
    }

    /// Record what a recheck found: each finding, failed or clean, on its
    /// run (and its open asks), and `landing_recheck_finished` with the counts on the run
    /// whose landing it followed (or the first run it checked).
    fn apply_recheck(&mut self, watch: &RecheckWatch, found: Vec<(Target, Finding)>) -> Result<()> {
        let mut counts = json!({
            "checked": found.len(),
            "clean": 0,
            // Of `clean`, the runs whose diff touched none of `[recheck]
            // paths`, so the command was not run on them (ADR-t2032-1).
            "command_skipped": 0,
            "conflicts": 0,
            "check_failed": 0,
            "errors": 0,
            "resumed": 0,
            "held": 0,
        });
        let mut failed_runs = Vec::new();
        for (target, finding) in found {
            let failure = match finding {
                Finding::Clean { skipped_by_paths } => {
                    bump(&mut counts, "clean");
                    if skipped_by_paths {
                        bump(&mut counts, "command_skipped");
                    }
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
                    bump(&mut counts, "errors");
                    warn!(run_id = %target.run_id, error = %error, "run {}: the landing recheck against main {} could not judge it: {error}", target.run_id, watch.main);
                    continue;
                }
                Finding::Failed(failure) => failure,
            };
            bump(
                &mut counts,
                match failure {
                    RecheckFailure::Conflict { .. } => "conflicts",
                    RecheckFailure::CheckFailed { .. } => "check_failed",
                },
            );
            match self.record_recheck_failure(watch, &target, &failure) {
                Ok(Some(action)) => {
                    bump(&mut counts, action);
                    failed_runs.push(json!({
                        "run_id": target.run_id,
                        "code": failure.code(),
                        "action": action,
                    }));
                }
                Ok(None) => {
                    info!(run_id = %target.run_id, "run {}: the landing recheck found it no longer landing on main {}, but it moved on meanwhile", target.run_id, watch.main);
                }
                Err(error) => {
                    warn!(run_id = %target.run_id, error = %format_args!("{error:#}"), "run {}: the landing recheck's finding could not be recorded: {error:#}", target.run_id);
                }
            }
        }
        let mut payload = json!({
            "main": watch.main,
            "landed_run_id": watch.landed.as_ref().map(|l| &l.run_id),
            "landed_task_id": watch.landed.as_ref().map(|l| l.task_id),
            "command": watch.command,
            "duration_secs": watch.started.elapsed().as_secs(),
            "failed_runs": failed_runs,
            "supervisor": self.token,
        });
        if let (Some(payload), Some(counts)) = (payload.as_object_mut(), counts.as_object()) {
            payload.extend(counts.clone());
        }
        info!(
            "landing recheck against main {} finished: {counts}",
            watch.main
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
                .is_some_and(|lease| lease.token != self.token)
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
    /// when it would land. Either way its open asks get the finding. `None`
    /// when the run moved on since it was checked (another head, another
    /// status, or a lease of another process).
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
        let action = match self.queue.run_lease(run.id())? {
            None => match self
                .queue
                .park_rechecked(run.id(), None, &reason, payload)?
            {
                Some(_) => recheck::RESUMED,
                None => return Ok(None),
            },
            Some(lease) if lease.token == self.token => {
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
        let parked = self
            .queue
            .park_rechecked(run.id(), Some(&self.token), &reason, payload)?;
        if parked.is_some() {
            info!(run_id = %run.id(), "run {}: parked for a resume instead of landing onto main {main}: {reason}", run.id());
        }
        Ok(parked)
    }
}

fn bump(counts: &mut Value, key: &str) {
    counts[key] = json!(counts[key].as_u64().unwrap_or(0) + 1);
}

/// Check each target against `main`, one after another (ADR-0068 decision
/// 2): `git merge-tree` first; a clean merge, when there is a `command`
/// and the merged tree differs from main in a path of `[recheck] paths`
/// (or there are none, ADR-t2032-1), is committed on top of main, checked out in the scratch worktree under
/// `dir` and the command run there in `/bin/sh` with the run's `[run.env]`
/// (refused the sccache server's start and guarded as `look` says,
/// ADR-t2086-1) and `CARGO_TARGET_DIR` set to the one target directory
/// under `dir`, its output in `recheck-<main>.log` of the run directory.
#[allow(clippy::too_many_arguments)]
fn recheck_runs(
    repository: &dyn Repository,
    verifier: &dyn Verifier,
    files: &dyn RunFiles,
    main: &CommitSha,
    config: &RecheckConfig,
    dir: &Path,
    look: &GuardLook,
    targets: Vec<Target>,
) -> Vec<(Target, Finding)> {
    targets
        .into_iter()
        .map(|target| {
            let finding = recheck_run(
                repository, verifier, files, main, config, dir, look, &target,
            )
            .unwrap_or_else(|error| Finding::Error(format!("{error:#}")));
            (target, finding)
        })
        .collect()
}

#[allow(clippy::too_many_arguments)]
fn recheck_run(
    repository: &dyn Repository,
    verifier: &dyn Verifier,
    files: &dyn RunFiles,
    main: &CommitSha,
    config: &RecheckConfig,
    dir: &Path,
    look: &GuardLook,
    target: &Target,
) -> Result<Finding> {
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
    let merged = repository.commit_tree(
        &tree,
        main.as_str(),
        &[format!(
            "dagq landing recheck of run {} on main {main}",
            target.run_id
        )],
    )?;
    let scratch = dir.join("worktree");
    files.create_dir_all(dir)?;
    repository.checkout_scratch(&scratch, merged.as_str())?;
    let env = recheck_env(verifier.run_env(&target.run_dir)?, dir, look)?;
    let log = target
        .run_dir
        .join(format!("recheck-{}.log", &main.as_str()[..12]));
    let exit = verifier.run_to_log(command, &scratch, &env, &log)?;
    if exit.success {
        return Ok(Finding::Clean {
            skipped_by_paths: false,
        });
    }
    let output = files.read_to_string(&log).unwrap_or_default();
    Ok(Finding::Failed(RecheckFailure::CheckFailed {
        command: command.to_owned(),
        exit_code: exit.code.unwrap_or(128),
        log_path: path_text(&log)?,
        output_tail: tail(&output, 2000).to_owned(),
    }))
}

/// The command's environment: the run's `[run.env]` with the recheck's one
/// target directory under `dir` as `CARGO_TARGET_DIR`, refused the sccache
/// server's start and guarded as `look` says (ADR-t2086-1).
fn recheck_env(
    mut env: Vec<(String, String)>,
    dir: &Path,
    look: &GuardLook,
) -> Result<Vec<(String, String)>> {
    env.retain(|(key, _)| key != "CARGO_TARGET_DIR");
    env.push((
        "CARGO_TARGET_DIR".to_owned(),
        path_text(&dir.join("target"))?,
    ));
    look.apply(&mut env);
    Ok(env)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::sccache::REFUSED_ERROR_LOG;

    #[test]
    fn the_recheck_command_is_refused_the_servers_start_and_guarded_as_looked() {
        let dir = Path::new("/q/recheck");
        let run_env = || {
            vec![
                ("RUSTC_WRAPPER".to_owned(), "sccache".to_owned()),
                ("CARGO_TARGET_DIR".to_owned(), "/elsewhere".to_owned()),
            ]
        };
        let pairs = |items: &[(&str, &str)]| -> Vec<(String, String)> {
            items
                .iter()
                .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
                .collect()
        };
        let guard = GuardLook::Guard("/q/recheck/dagq-rustc-wrapper".into());
        assert_eq!(
            recheck_env(run_env(), dir, &guard).unwrap(),
            pairs(&[
                ("CARGO_TARGET_DIR", "/q/recheck/target"),
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
            recheck_env(run_env(), dir, &unconfirmed).unwrap(),
            pairs(&[
                ("CARGO_TARGET_DIR", "/q/recheck/target"),
                ("SCCACHE_ERROR_LOG", REFUSED_ERROR_LOG),
            ])
        );
        assert_eq!(
            recheck_env(run_env(), dir, &GuardLook::NotConfigured).unwrap(),
            pairs(&[
                ("RUSTC_WRAPPER", "sccache"),
                ("CARGO_TARGET_DIR", "/q/recheck/target"),
            ])
        );
    }
}
