//! The sizes of the runs moving now, measured off the loop (ADR-t4542-1),
//! for what they are expected to take yet ([`DiskState::remaining`]):
//! as the runs that wait are measured for the host's sample
//! ([`super::host_metrics`]), the loop lists the runs it counts as moving
//! (no walk), and a thread of its own measures their worktree and
//! `TMPDIR`s with [`RunFiles::tree_size`]'s count (hard links once, no link
//! followed), one walk at a time, every
//! [`MOVING_SIZE_INTERVAL_SECS`](crate::domain::disk::MOVING_SIZE_INTERVAL_SECS)
//! and as soon as a run moves that has no measure of its present
//! directories. The thread puts each run's size and when its measure
//! started (so a slow walk ages it) in a state shared with the loop, which only reads it
//! ([`crate::domain::disk::trusted_size`]). Nothing is removed.
//!
//! [`DiskState::remaining`]: crate::domain::disk::DiskState::remaining
//! [`RunFiles::tree_size`]: crate::application::RunFiles::tree_size

use super::*;
use crate::application::{RUN_TMP_DIR, host_verification_tmp_path};
use crate::domain::disk::{
    MOVING_SIZE_INTERVAL_SECS, MovingWalk, MovingWalkInputs, SizeMeasure, moving_remaining,
    moving_walk, trusted_size,
};
use std::collections::HashMap;
use std::sync::Mutex;

/// Measures the bytes on disk of the tree at a path as
/// [`RunFiles::tree_size`](crate::application::RunFiles::tree_size) does:
/// `Ok(None)` for no directory there.
pub type MeasureTree = Arc<dyn Fn(&Path) -> std::io::Result<Option<u64>> + Send + Sync>;

/// What a moving run is measured of: the run, its worktree on the host
/// (`None` for a run in a container, which builds in its machine's volume,
/// or one with no worktree: never measured), and its `TMPDIR`s, that of its
/// worker turns and that of its verification on the host. A measure of
/// another target is not this run's size now.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(in crate::application::supervise) struct MovingTarget {
    pub(in crate::application::supervise) run: RunId,
    pub(in crate::application::supervise) worktree: Option<PathBuf>,
    pub(in crate::application::supervise) tmp: Vec<PathBuf>,
}

impl MovingTarget {
    /// The target of `run`, whose run directory is under `runs_dir`.
    pub(in crate::application::supervise) fn of(run: &TaskRun, runs_dir: &Path) -> Self {
        let worktree = (!run.in_container())
            .then(|| run.worktree_path().map(PathBuf::from))
            .flatten();
        Self {
            run: run.id().clone(),
            worktree,
            tmp: vec![
                runs_dir.join(run.id().as_str()).join(RUN_TMP_DIR),
                host_verification_tmp_path(run.id()),
            ],
        }
    }

    /// Its worktree and `TMPDIR`s together; `None` when the worktree is
    /// none or not there, or a directory could not be measured. A `TMPDIR`
    /// not there is 0 bytes.
    fn measure(&self, measure: &MeasureTree) -> Option<u64> {
        let mut bytes = measure(self.worktree.as_deref()?).ok()??;
        for dir in &self.tmp {
            bytes = bytes.saturating_add(measure(dir).ok()?.unwrap_or(0));
        }
        Some(bytes)
    }
}

/// A run's latest measure, as the walk's thread put it: `at` is when the
/// walk of its target started.
#[derive(Debug, Clone)]
struct Measured {
    target: MovingTarget,
    bytes: Option<u64>,
    at: Instant,
}

/// The latest measure of each moving run, written by the walk's thread and
/// read by the loop.
type Shared = Arc<Mutex<HashMap<RunId, Measured>>>;

/// The walk of the moving runs and what the walks measured.
#[derive(Default)]
pub(in crate::application::supervise) struct MovingSizes {
    measured: Shared,
    /// The walk running now.
    walking: Option<thread::JoinHandle<()>>,
    /// When the last walk started.
    started: Option<Instant>,
}

impl MovingSizes {
    /// Reap the walk once it ended, forget the runs no longer moving, and
    /// start a walk of `moving` in a thread of its own when [`moving_walk`]
    /// says so (a run of `moving` has no measure of its present target
    /// when its measure is missing or of another target).
    /// `at` is the loop's monotonic time; the thread reads `clock` for when
    /// it measured each run.
    pub(in crate::application::supervise) fn pass(
        &mut self,
        moving: &[MovingTarget],
        measure: &MeasureTree,
        clock: &Arc<dyn crate::application::Clock>,
        at: Instant,
    ) {
        if let Some(walking) = self.walking.take() {
            if walking.is_finished() {
                if walking.join().is_err() {
                    warn!("the measure of the runs moving panicked");
                }
            } else {
                self.walking = Some(walking);
            }
        }
        let unmeasured = {
            let mut measured = lock(&self.measured);
            measured.retain(|run, _| moving.iter().any(|target| target.run == *run));
            moving.iter().any(|target| {
                measured
                    .get(&target.run)
                    .is_none_or(|measured| measured.target != *target)
            })
        };
        let walk = moving_walk(&MovingWalkInputs {
            since_started: self
                .started
                .map(|started| at.saturating_duration_since(started)),
            walking: self.walking.is_some(),
            moving: !moving.is_empty(),
            unmeasured,
        });
        if walk == MovingWalk::Wait {
            return;
        }
        self.started = Some(at);
        let (targets, measure, clock, shared) = (
            moving.to_vec(),
            measure.clone(),
            clock.clone(),
            self.measured.clone(),
        );
        self.walking = Some(spawn_traced(move || {
            for target in targets {
                // When its walk started, so that the time the walk takes
                // counts toward the measure's age: a walk stuck past the
                // stale bound leaves a value that is stale already.
                let at = clock.monotonic();
                let bytes = target.measure(&measure);
                lock(&shared).insert(target.run.clone(), Measured { target, bytes, at });
            }
        }));
    }

    /// Wait for the walk still running, as the loop ends, until `deadline`
    /// on `clock` when one is given ([`super::finished_by`]); one still
    /// running then is left.
    pub(in crate::application::supervise) fn finish(
        &mut self,
        deadline: &super::EndBy,
        clock: &dyn crate::application::Clock,
    ) {
        if let Some(walking) = self.walking.take() {
            if !super::finished_by(&walking, deadline, clock) {
                warn!(
                    "the measure of the runs moving still runs as the supervisor ends; it is left"
                );
            } else if walking.join().is_err() {
                warn!("the measure of the runs moving panicked");
            }
        }
    }

    /// What the runs of `moving` are expected to take yet, together, at
    /// `now` on the loop's monotonic clock: for each, `expected` less the
    /// size the claims trust ([`trusted_size`]), unmeasured ones counted
    /// whole ([`moving_remaining`]).
    pub(in crate::application::supervise) fn remaining(
        &self,
        moving: &[MovingTarget],
        expected: Option<u64>,
        now: Instant,
    ) -> u64 {
        let measured = lock(&self.measured);
        let interval = Duration::from_secs(MOVING_SIZE_INTERVAL_SECS);
        moving_remaining(
            expected,
            moving.iter().map(|target| {
                let measure = measured.get(&target.run).map(|measured| SizeMeasure {
                    bytes: measured.bytes,
                    age: now.saturating_duration_since(measured.at),
                    same_target: measured.target == *target,
                });
                trusted_size(measure.as_ref(), interval)
            }),
        )
    }
}

/// The shared state, whatever a panicking walk left of it.
fn lock(shared: &Shared) -> std::sync::MutexGuard<'_, HashMap<RunId, Measured>> {
    shared
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    const GIB: u64 = 1 << 30;

    /// A clock the test moves.
    struct Moved(Mutex<Instant>);

    impl crate::application::Clock for Moved {
        fn system_time(&self) -> std::time::SystemTime {
            std::time::SystemTime::UNIX_EPOCH
        }
        fn monotonic(&self) -> Instant {
            *self.0.lock().unwrap()
        }
    }

    fn target(run: &str, worktree: Option<&str>) -> MovingTarget {
        MovingTarget {
            run: RunId::new(run).unwrap(),
            worktree: worktree.map(PathBuf::from),
            tmp: vec![PathBuf::from(format!("/q/runs/{run}/tmp"))],
        }
    }

    /// How long a test waits for a walk, or a walk for the test to let it
    /// go on, before it fails.
    const LIMIT: Duration = Duration::from_secs(10);

    /// Wait for the walk running to end and join it, failing `test` past
    /// [`LIMIT`]: a thread of the test joins it and says so on a channel
    /// the test waits on with `recv_timeout`.
    fn wait(sizes: &mut MovingSizes, test: &str) {
        let Some(walking) = sizes.walking.take() else {
            return;
        };
        let (joined, ended) = std::sync::mpsc::channel();
        let joiner = thread::spawn(move || {
            let _ = joined.send(walking.join().is_ok());
        });
        match ended.recv_timeout(LIMIT) {
            Ok(ended_well) => assert!(ended_well, "{test}: the walk of the moving runs panicked"),
            Err(_) => panic!("{test}: the walk of the moving runs did not end within {LIMIT:?}"),
        }
        // It sent its last: it ends now.
        joiner.join().unwrap();
    }

    /// The worktree and the `TMPDIR`s are added; a `TMPDIR` not there is
    /// 0, a worktree not there, none or one that fails is no size, as is a
    /// `TMPDIR` that fails.
    #[test]
    fn a_runs_size_is_its_worktree_and_tmpdirs_and_none_when_one_fails() {
        let measure: MeasureTree = Arc::new(|dir: &Path| match dir.to_str() {
            Some("/w") => Ok(Some(5)),
            Some("/t") => Ok(Some(2)),
            Some("/gone") | Some("/t-gone") => Ok(None),
            _ => Err(std::io::Error::from_raw_os_error(libc::EMFILE)),
        });
        let of = |worktree: Option<&str>, tmp: &[&str]| {
            MovingTarget {
                run: RunId::new("r").unwrap(),
                worktree: worktree.map(PathBuf::from),
                tmp: tmp.iter().map(PathBuf::from).collect(),
            }
            .measure(&measure)
        };
        assert_eq!(of(Some("/w"), &["/t", "/t-gone"]), Some(7));
        assert_eq!(of(Some("/gone"), &["/t"]), None);
        assert_eq!(of(None, &["/t"]), None);
        assert_eq!(of(Some("/fails"), &["/t"]), None);
        assert_eq!(of(Some("/w"), &["/fails"]), None);
    }

    /// A run in a container is measured of nothing; one on the host of its
    /// worktree and both its `TMPDIR`s.
    #[test]
    fn a_run_in_a_container_has_no_worktree_to_measure() {
        use crate::domain::{CommitSha, Provider, RunRecord, RunStatus};
        let run = |id: &str, worktree: &str| {
            TaskRun::restore(RunRecord {
                id: RunId::new(id).unwrap(),
                task_id: TaskId::new(7),
                status: RunStatus::Running,
                requested_provider: Provider::Claude,
                actual_provider: Provider::Claude,
                worker_mode: crate::domain::worker::Worker::default_mode(Provider::Claude),
                base_commit: CommitSha::parse("a".repeat(40), "base").unwrap(),
                branch: None,
                worktree_path: Some(worktree.to_owned()),
                workspace_id: None,
                receipt_path: None,
                log_path: None,
                result_commit: None,
                repo_path: None,
                run_dir: None,
                last_error: None,
                workspace_closed_at: None,
                created_at: String::new(),
            })
            .unwrap()
        };
        let runs = Path::new("/q/runs");
        let host = MovingTarget::of(&run("h", "/q/runs/h/worktree"), runs);
        assert_eq!(host.worktree, Some(PathBuf::from("/q/runs/h/worktree")));
        assert_eq!(
            host.tmp,
            [
                PathBuf::from("/q/runs/h/tmp"),
                host_verification_tmp_path(&RunId::new("h").unwrap())
            ]
        );
        let clone = MovingTarget::of(&run("c", "/q/clones/c"), runs);
        assert_eq!(clone.worktree, None);
    }

    /// Acceptance 1 and 2 of the task: the walk runs off the loop's
    /// thread, one at a time; a run just claimed counts whole until its
    /// first measure, then the expectation less its size, 0 past it. A
    /// new run starts a walk before the interval; with the same runs all
    /// measured the next starts at the interval, not 1 ms before. A measure grown
    /// stale, of another worktree, or that failed counts whole again.
    #[test]
    fn the_moving_runs_are_measured_off_the_loop_one_walk_at_a_time() {
        const TEST: &str = "the_moving_runs_are_measured_off_the_loop_one_walk_at_a_time";
        let loop_thread = thread::current().id();
        let walks = Arc::new(AtomicUsize::new(0));
        let (go, gate) = std::sync::mpsc::channel::<()>();
        let gate = Mutex::new(gate);
        let sizes: Arc<Mutex<HashMap<String, std::io::Result<Option<u64>>>>> = Arc::default();
        let measure: MeasureTree = {
            let (walks, sizes) = (walks.clone(), sizes.clone());
            Arc::new(move |dir: &Path| {
                assert_ne!(thread::current().id(), loop_thread);
                let dir = dir.to_str().unwrap().to_owned();
                if dir.ends_with("tmp") || dir.contains("dagq-v-") {
                    return Ok(None);
                }
                walks.fetch_add(1, Ordering::SeqCst);
                if gate.lock().unwrap().recv_timeout(LIMIT).is_err() {
                    panic!("{TEST}: the walk of {dir} was not let go on within {LIMIT:?}");
                }
                match sizes.lock().unwrap().get(&dir) {
                    Some(Ok(size)) => Ok(*size),
                    _ => Err(std::io::Error::from_raw_os_error(libc::EMFILE)),
                }
            })
        };
        sizes.lock().unwrap().insert("/a".into(), Ok(Some(4 * GIB)));
        sizes
            .lock()
            .unwrap()
            .insert("/b".into(), Ok(Some(12 * GIB)));
        let start = Instant::now();
        let clock = Arc::new(Moved(Mutex::new(start)));
        let dyn_clock: Arc<dyn crate::application::Clock> = clock.clone();
        let expected = Some(10 * GIB);
        let mut moving = MovingSizes::default();
        let a = [target("a", Some("/a"))];
        moving.pass(&a, &measure, &dyn_clock, start);
        // Just claimed and not yet measured: the expectation whole.
        assert_eq!(moving.remaining(&a, expected, start), 10 * GIB);
        // A second pass while the walk runs starts none.
        let ab = [target("a", Some("/a")), target("b", Some("/b"))];
        moving.pass(&ab, &measure, &dyn_clock, start);
        go.send(()).unwrap();
        wait(&mut moving, TEST);
        assert_eq!(walks.load(Ordering::SeqCst), 1);
        assert_eq!(moving.remaining(&a, expected, start), 6 * GIB);
        // `b` has no measure: a walk starts before the interval.
        moving.pass(&ab, &measure, &dyn_clock, start);
        assert_eq!(moving.remaining(&ab, expected, start), 16 * GIB);
        go.send(()).unwrap();
        go.send(()).unwrap();
        wait(&mut moving, TEST);
        assert_eq!(walks.load(Ordering::SeqCst), 3);
        assert_eq!(moving.remaining(&ab, expected, start), 6 * GIB);
        // All measured, within the interval: no walk.
        let interval = Duration::from_secs(MOVING_SIZE_INTERVAL_SECS);
        moving.pass(&ab, &measure, &dyn_clock, start + interval / 2);
        assert!(moving.walking.is_none());
        // Stale past the stale intervals: whole again.
        let stale = start
            + interval * crate::domain::disk::MOVING_SIZE_STALE_INTERVALS
            + Duration::from_secs(1);
        assert_eq!(moving.remaining(&ab, expected, stale), 20 * GIB);
        // The same runs, all measured: 1 ms before the interval no walk,
        // at the interval one walks them again.
        moving.pass(
            &ab,
            &measure,
            &dyn_clock,
            start + interval - Duration::from_millis(1),
        );
        assert!(moving.walking.is_none());
        moving.pass(&ab, &measure, &dyn_clock, start + interval);
        assert!(moving.walking.is_some());
        go.send(()).unwrap();
        go.send(()).unwrap();
        wait(&mut moving, TEST);
        assert_eq!(walks.load(Ordering::SeqCst), 5);
        assert_eq!(moving.remaining(&ab, expected, start), 6 * GIB);
        // Another worktree of `a`: whole, and walked at once.
        let moved = [target("a", Some("/a2")), target("b", Some("/b"))];
        assert_eq!(moving.remaining(&moved, expected, start), 10 * GIB);
        *clock.0.lock().unwrap() = start + interval;
        sizes.lock().unwrap().insert("/b".into(), Ok(Some(GIB)));
        moving.pass(&moved, &measure, &dyn_clock, start + interval);
        go.send(()).unwrap();
        go.send(()).unwrap();
        wait(&mut moving, TEST);
        // `/a2` failed: whole; `b` is now 1 GiB.
        assert_eq!(
            moving.remaining(&moved, expected, start + interval),
            19 * GIB
        );
        // A run no longer moving is forgotten; with none moving no walk.
        moving.pass(&[], &measure, &dyn_clock, start + interval * 2);
        assert!(moving.walking.is_none());
        assert!(lock(&moving.measured).is_empty());
        // No expectation: 0 whatever is measured.
        assert_eq!(moving.remaining(&moved, None, start), 0);
    }

    /// A measure is as old as the start of its run's walk: a walk whose
    /// `TMPDIR` takes past the stale bound after the worktree was measured
    /// leaves a value already stale, which counts the expectation whole.
    #[test]
    fn a_measure_is_as_old_as_the_start_of_its_runs_walk() {
        const TEST: &str = "a_measure_is_as_old_as_the_start_of_its_runs_walk";
        let start = Instant::now();
        let interval = Duration::from_secs(MOVING_SIZE_INTERVAL_SECS);
        let stuck = start
            + interval * crate::domain::disk::MOVING_SIZE_STALE_INTERVALS
            + Duration::from_secs(1);
        let clock = Arc::new(Moved(Mutex::new(start)));
        let measure: MeasureTree = {
            let clock = clock.clone();
            Arc::new(move |dir: &Path| {
                if dir == Path::new("/a") {
                    return Ok(Some(10 * GIB));
                }
                // The TMPDIR's walk takes past the stale bound.
                *clock.0.lock().unwrap() = stuck;
                Ok(None)
            })
        };
        let dyn_clock: Arc<dyn crate::application::Clock> = clock.clone();
        let a = [target("a", Some("/a"))];
        let mut moving = MovingSizes::default();
        moving.pass(&a, &measure, &dyn_clock, start);
        wait(&mut moving, TEST);
        // At the start of the walk the value would be trusted (nothing
        // left to take); once the walk ended it is stale already.
        assert_eq!(moving.remaining(&a, Some(10 * GIB), start), 0);
        assert_eq!(moving.remaining(&a, Some(10 * GIB), stuck), 10 * GIB);
    }
}
