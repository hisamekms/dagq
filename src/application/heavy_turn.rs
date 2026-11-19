//! The turns of the host's heavy jobs across the processes of a queue
//! (ADR-t2719-1): the automatic update's job holds the queue's turn
//! exclusively while it builds and runs its e2e gate, and each landing
//! check (a landing's verification, a direct `integrate`'s included, and
//! the landing recheck) holds it shared while it runs. A job that waits for
//! its turn says so by holding the queue's waiting lock. The supervisor
//! reserves that turn only after the landing-first admission decision
//! (ADR-t5283-4). The reservation covers the gap between spawn and the
//! child's acquisition of the lock, including for direct integrations.

use super::{ProcessControl, RunFiles};
use crate::domain::heavy_turn::{self, HeavyJob, HeavyLook};
use anyhow::{Context, Result};
use std::io;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// A held turn: it goes when the guard drops.
pub type Turn = Box<dyn std::any::Any + Send>;

/// How often a landing check that has to start looks again while it waits.
pub const CHECK_POLL: Duration = Duration::from_millis(200);

/// Where a queue keeps its turns: `<queue dir>/heavy/`.
#[derive(Debug, Clone)]
pub struct HeavyTurn {
    /// Held exclusively by the update's gate, shared by the landing checks.
    pub turn: PathBuf,
    /// Held exclusively by the update's job while it waits for its turn.
    pub waiting: PathBuf,
}

/// A candidate waiting for the next opportunity, shared by the queue's
/// supervisors. The base preserves the original comparison even when a
/// supervisor whose build has no named commit is replaced.
#[derive(serde::Serialize, serde::Deserialize)]
pub struct PendingUpdate {
    pub since: i64,
    pub base: Option<String>,
}

/// The locks the turns take: the run files' locks, shared across
/// processes.
trait Locks {
    /// A shared lock without waiting; `None` while an exclusive holder has
    /// it.
    fn shared(&self, path: &Path) -> io::Result<Option<Turn>>;
    /// An exclusive lock without waiting; `None` while any holder has it.
    fn exclusive(&self, path: &Path) -> io::Result<Option<Turn>>;
    /// An exclusive lock, waiting while any holder has it.
    fn wait_exclusive(&self, path: &Path) -> io::Result<Turn>;
}

/// [`Locks`] of [`RunFiles`].
struct FileLocks<'a>(&'a dyn RunFiles);

impl Locks for FileLocks<'_> {
    fn shared(&self, path: &Path) -> io::Result<Option<Turn>> {
        self.0.try_lock_shared(path)
    }
    fn exclusive(&self, path: &Path) -> io::Result<Option<Turn>> {
        self.0.try_lock(path)
    }
    fn wait_exclusive(&self, path: &Path) -> io::Result<Turn> {
        self.0.lock(path)
    }
}

impl HeavyTurn {
    pub fn under(queue_dir: &Path) -> Self {
        let dir = queue_dir.join("heavy");
        Self {
            turn: dir.join("turn"),
            waiting: dir.join("waiting"),
        }
    }

    /// Serialize update selection across supervisors. This lock alone
    /// does not reserve the heavy turn or interfere with landings.
    pub fn try_schedule_update(&self, files: &dyn RunFiles) -> Result<Option<Turn>> {
        self.make_dir(files)?;
        Ok(files.try_lock(&self.turn.with_file_name("update-schedule"))?)
    }

    /// Read under the scheduling lock so the same wait survives new heads
    /// and replacement supervisors.
    pub fn pending_update(&self, files: &dyn RunFiles) -> Result<Option<PendingUpdate>> {
        read_state(files, &self.turn.with_file_name("update-wait"))
    }

    /// Write once on discovering a candidate, using Unix seconds from the
    /// injected clock. The caller holds the update scheduling lock.
    pub fn begin_update_wait(&self, files: &dyn RunFiles, pending: &PendingUpdate) -> Result<()> {
        write_state(files, &self.turn.with_file_name("update-wait"), pending)
    }

    pub fn clear_update_wait(&self, files: &dyn RunFiles) -> Result<()> {
        clear_state(files, &self.turn.with_file_name("update-wait"))
    }

    /// No landing can pass the waiting probe between spawn and publication
    /// of the reservation. The child replaces it with the exclusive turn,
    /// or liveness makes it ignorable if startup fails before then.
    pub fn start_update(
        &self,
        files: &dyn RunFiles,
        processes: &dyn ProcessControl,
        spawn: impl FnOnce() -> Result<u32>,
    ) -> Result<bool> {
        let Some(_waiting) = files.try_lock(&self.waiting)? else {
            return Ok(false);
        };
        if self.update_reserved(files, processes)? || files.try_lock_shared(&self.turn)?.is_none() {
            return Ok(false);
        }
        let pid = spawn()?;
        write_state(
            files,
            &self.turn.with_file_name("update-reservation"),
            &(pid, processes.start_identity(pid)),
        )?;
        Ok(true)
    }

    fn update_reserved(
        &self,
        files: &dyn RunFiles,
        processes: &dyn ProcessControl,
    ) -> Result<bool> {
        let Some((pid, start)) = read_state::<(u32, Option<String>)>(
            files,
            &self.turn.with_file_name("update-reservation"),
        )?
        else {
            return Ok(false);
        };
        Ok(processes.alive(pid)
            && start.is_none_or(|start| {
                processes
                    .start_identity(pid)
                    .is_none_or(|current| start == current)
            }))
    }

    fn make_dir(&self, files: &dyn RunFiles) -> Result<()> {
        if let Some(dir) = self.turn.parent() {
            files
                .create_dir_all(dir)
                .with_context(|| format!("create {}", dir.display()))?;
        }
        Ok(())
    }

    /// Take a landing check's turn without waiting: the turn shared, or
    /// what it waits for (the update's gate running or waiting for its
    /// turn).
    pub fn try_check(
        &self,
        files: &dyn RunFiles,
        processes: &dyn ProcessControl,
    ) -> Result<Result<Turn, HeavyJob>> {
        self.make_dir(files)?;
        self.try_check_on(&FileLocks(files), || self.update_reserved(files, processes))
    }

    /// [`Self::try_check`] on `locks`. The probe of the waiting lock is held
    /// until the turn is taken, so a job cannot start to wait between the
    /// probe and the turn: once a job waits, no check starts.
    fn try_check_on(
        &self,
        locks: &dyn Locks,
        reserved: impl FnOnce() -> Result<bool>,
    ) -> Result<Result<Turn, HeavyJob>> {
        let probe = locks
            .shared(&self.waiting)
            .with_context(|| format!("lock {}", self.waiting.display()))?;
        if probe.is_some() && reserved()? {
            return Ok(Err(HeavyJob::UpdateGate));
        }
        let turn = match &probe {
            Some(_) => locks
                .shared(&self.turn)
                .with_context(|| format!("lock {}", self.turn.display()))?,
            None => None,
        };
        let look = HeavyLook {
            update_gate_running: probe.is_some() && turn.is_none(),
            update_gate_waiting: probe.is_none(),
            landing_checks_running: false,
        };
        drop(probe);
        Ok(match (heavy_turn::landing_check_waits_for(look), turn) {
            (None, Some(turn)) => Ok(turn),
            (what, _) => Err(what.unwrap_or(HeavyJob::UpdateGate)),
        })
    }

    /// Take a landing check's turn, waiting while the update's gate runs or
    /// waits; `on_wait` is told once what it waits for.
    pub fn check(
        &self,
        files: &dyn RunFiles,
        processes: &dyn ProcessControl,
        poll: Duration,
        on_wait: impl FnOnce(HeavyJob),
    ) -> Result<Turn> {
        let mut on_wait = Some(on_wait);
        loop {
            match self.try_check(files, processes)? {
                Ok(turn) => return Ok(turn),
                Err(what) => {
                    if let Some(tell) = on_wait.take() {
                        tell(what);
                    }
                    std::thread::sleep(poll);
                }
            }
        }
    }

    /// Take the update's gate: hold the waiting lock, so no new landing
    /// check starts, until the landing checks running end and the turn is
    /// held exclusively; `on_wait` is told once what it waits for.
    pub fn gate(&self, files: &dyn RunFiles, on_wait: impl FnOnce(HeavyJob)) -> Result<Turn> {
        self.make_dir(files)?;
        self.gate_on(&FileLocks(files), on_wait, || Ok(()))
    }

    /// The admitted child exchanges its reservation for the actual turn.
    pub fn job_gate(
        &self,
        files: &dyn RunFiles,
        pid: u32,
        on_wait: impl FnOnce(HeavyJob),
    ) -> Result<Turn> {
        self.make_dir(files)?;
        self.gate_on(&FileLocks(files), on_wait, || {
            let path = self.turn.with_file_name("update-reservation");
            if read_state::<(u32, Option<String>)>(files, &path)?
                .is_some_and(|(owner, _)| owner == pid)
            {
                clear_state(files, &path)?;
            }
            Ok(())
        })
    }

    /// [`Self::gate`] on `locks`.
    fn gate_on(
        &self,
        locks: &dyn Locks,
        on_wait: impl FnOnce(HeavyJob),
        acquired: impl FnOnce() -> Result<()>,
    ) -> Result<Turn> {
        let waiting = locks
            .wait_exclusive(&self.waiting)
            .with_context(|| format!("lock {}", self.waiting.display()))?;
        let free = locks
            .exclusive(&self.turn)
            .with_context(|| format!("lock {}", self.turn.display()))?;
        let look = HeavyLook {
            landing_checks_running: free.is_none(),
            ..HeavyLook::default()
        };
        let turn = match (heavy_turn::update_gate_waits_for(look), free) {
            (None, Some(turn)) => turn,
            (what, _) => {
                on_wait(what.unwrap_or(HeavyJob::LandingCheck));
                locks
                    .wait_exclusive(&self.turn)
                    .with_context(|| format!("lock {}", self.turn.display()))?
            }
        };
        acquired()?;
        drop(waiting);
        Ok(turn)
    }
}

fn read_state<T: serde::de::DeserializeOwned>(
    files: &dyn RunFiles,
    path: &Path,
) -> Result<Option<T>> {
    match files.read(path) {
        Ok(bytes) => Ok(serde_json::from_slice(&bytes)?),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error).with_context(|| format!("read {}", path.display())),
    }
}

/// Each state file has a single writer at a time under its corresponding
/// lock. Rename keeps a terminated writer from leaving a partial record.
fn write_state(files: &dyn RunFiles, path: &Path, value: &impl serde::Serialize) -> Result<()> {
    let staging = path.with_extension("tmp");
    files.write(&staging, &serde_json::to_vec(value)?)?;
    Ok(files.rename(&staging, path)?)
}

fn clear_state(files: &dyn RunFiles, path: &Path) -> Result<()> {
    match files.remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error).with_context(|| format!("remove {}", path.display())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::sync::{Arc, Mutex};

    /// A lock's holders in the model: shared ones counted, or one
    /// exclusive.
    #[derive(Default, Clone, Copy)]
    struct Held {
        shared: usize,
        exclusive: bool,
    }

    type Table = Arc<Mutex<HashMap<PathBuf, Held>>>;

    /// A held lock of the model, given back when it drops.
    struct Guard {
        table: Table,
        path: PathBuf,
        exclusive: bool,
    }

    impl Drop for Guard {
        fn drop(&mut self) {
            let mut table = self.table.lock().unwrap();
            let held = table.entry(self.path.clone()).or_default();
            if self.exclusive {
                held.exclusive = false;
            } else {
                held.shared -= 1;
            }
        }
    }

    /// What another process does at the moment a check asks for the turn.
    type BeforeTurn = Box<dyn FnOnce(&Model) + Send>;

    /// The locks in memory, granted as flock grants them, with no waiting:
    /// a wait for a held lock is recorded and fails. `before_turn` runs as
    /// a check asks for the turn.
    #[derive(Default)]
    struct Model {
        table: Table,
        before_turn: Mutex<Option<BeforeTurn>>,
        /// The locks a wait found held, in order.
        waits: Mutex<Vec<PathBuf>>,
    }

    impl Model {
        fn held(&self, path: &Path) -> Held {
            self.table
                .lock()
                .unwrap()
                .get(path)
                .copied()
                .unwrap_or_default()
        }

        fn take(&self, path: &Path, exclusive: bool) -> Option<Turn> {
            let mut table = self.table.lock().unwrap();
            let held = table.entry(path.to_owned()).or_default();
            if held.exclusive || (exclusive && held.shared > 0) {
                return None;
            }
            if exclusive {
                held.exclusive = true;
            } else {
                held.shared += 1;
            }
            Some(Box::new(Guard {
                table: self.table.clone(),
                path: path.to_owned(),
                exclusive,
            }))
        }
    }

    impl Locks for Model {
        fn shared(&self, path: &Path) -> io::Result<Option<Turn>> {
            if path.ends_with("turn") {
                let before = self.before_turn.lock().unwrap().take();
                if let Some(before) = before {
                    before(self);
                }
            }
            Ok(self.take(path, false))
        }
        fn exclusive(&self, path: &Path) -> io::Result<Option<Turn>> {
            Ok(self.take(path, true))
        }
        fn wait_exclusive(&self, path: &Path) -> io::Result<Turn> {
            self.take(path, true).ok_or_else(|| {
                self.waits.lock().unwrap().push(path.to_owned());
                io::Error::from(io::ErrorKind::WouldBlock)
            })
        }
    }

    fn turns() -> HeavyTurn {
        HeavyTurn::under(Path::new("/queue"))
    }

    /// A check runs; a second check probes the waiting lock, and a job
    /// tries to begin to wait before the second asks for the turn. The job
    /// cannot take the waiting lock while the probe is held, so no check
    /// starts after a job began to wait.
    #[test]
    fn a_job_cannot_begin_to_wait_between_a_checks_probe_and_its_turn() {
        let turns = turns();
        let model = Model::default();
        let first = turns.try_check_on(&model, || Ok(false)).unwrap().unwrap();
        let job_took_waiting = Arc::new(Mutex::new(None));
        let took = job_took_waiting.clone();
        let waiting = turns.waiting.clone();
        *model.before_turn.lock().unwrap() = Some(Box::new(move |model: &Model| {
            *took.lock().unwrap() = Some(model.exclusive(&waiting).unwrap().is_some());
        }));
        let second = turns.try_check_on(&model, || Ok(false)).unwrap().unwrap();
        assert_eq!(*job_took_waiting.lock().unwrap(), Some(false));
        assert_eq!(model.held(&turns.waiting).shared, 0, "the probe went");
        assert_eq!(model.held(&turns.turn).shared, 2);
        drop((first, second));
        assert_eq!(model.held(&turns.turn).shared, 0);
    }

    #[test]
    fn a_check_waits_for_a_job_waiting_or_holding_its_turn() {
        let turns = turns();
        let model = Model::default();
        let waiting = model.exclusive(&turns.waiting).unwrap().unwrap();
        assert_eq!(
            turns.try_check_on(&model, || Ok(false)).unwrap().err(),
            Some(HeavyJob::UpdateGate)
        );
        assert_eq!(model.held(&turns.turn).shared, 0);
        drop(waiting);
        let gate = turns
            .gate_on(&model, |_| panic!("free"), || Ok(()))
            .unwrap();
        assert!(!model.held(&turns.waiting).exclusive);
        assert_eq!(
            turns.try_check_on(&model, || Ok(false)).unwrap().err(),
            Some(HeavyJob::UpdateGate)
        );
        drop(gate);
        assert!(turns.try_check_on(&model, || Ok(false)).unwrap().is_ok());
    }

    /// A gate that finds a check running is told so and waits for the turn
    /// holding the waiting lock, which keeps new checks out meanwhile.
    #[test]
    fn a_gate_waits_for_the_checks_running_holding_the_waiting_lock() {
        let turns = turns();
        let model = Model::default();
        let check = turns.try_check_on(&model, || Ok(false)).unwrap().unwrap();
        let mut told = None;
        let waiting_held = Arc::new(Mutex::new(None));
        let held = waiting_held.clone();
        let waited = turns.gate_on(
            &model,
            |what| {
                told = Some(what);
                *held.lock().unwrap() = Some(model.held(&turns.waiting).exclusive);
            },
            || Ok(()),
        );
        assert!(waited.is_err(), "the model does not wait");
        assert_eq!(told, Some(HeavyJob::LandingCheck));
        assert_eq!(*waiting_held.lock().unwrap(), Some(true));
        assert_eq!(*model.waits.lock().unwrap(), vec![turns.turn.clone()]);
        assert!(!model.held(&turns.waiting).exclusive, "given back");
        drop(check);
    }
}
