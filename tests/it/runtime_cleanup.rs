//! Runtime tests: the cleanup of ended runs' worktrees off the supervisor's
//! loop, the branches of worktrees already gone, and worktrees a rebind
//! left pointing at an old repository (task 405), and the runners of
//! ended runs (task 696).
use crate::{common, runtime_support};

use dagq::{application::RunFiles, runtime::RunFilesPort};
use runtime_support::*;
use std::{io, sync::Condvar};

/// A worker script that commits, leaves build outputs in its worktree and
/// fails.
const BUILDING_AGENT: &str = "commit work; mkdir -p target/debug; \
     head -c 65536 /dev/zero > target/debug/big; receipt \"$(git rev-parse HEAD)\"; exit 7";

/// Supervisor options that sweep the ended runs on every pass.
fn sweeping_options() -> SuperviseOptions {
    SuperviseOptions {
        sweep_interval: Duration::ZERO,
        ..supervise_options(4, true)
    }
}

/// The payloads of a run's events of `kind`.
fn payloads_of(queue: &SqliteQueue, run: &TaskRun, kind: &str) -> Vec<Value> {
    queue
        .run_events(run.id())
        .unwrap()
        .into_iter()
        .filter(|e| e.kind == kind)
        .map(|e| e.payload)
        .collect()
}

/// Whether the branch exists in `repo`.
fn branch_exists(repo: &Path, branch: &str) -> bool {
    Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["rev-parse", "--verify", "--quiet"])
        .arg(format!("refs/heads/{branch}"))
        .bounded_output()
        .unwrap()
        .status
        .success()
}

/// The first measure of a `target/` under a run worktree waits here until
/// the test opens the gate: a slow cleanup.
#[derive(Default)]
struct Gate {
    /// The directory whose measure waits (or waited).
    held: Option<PathBuf>,
    open: bool,
}

/// The local run files, but for the [`Gate`] on measuring build outputs.
#[derive(Clone, Default)]
struct GatedFiles {
    gate: Arc<(Mutex<Gate>, Condvar)>,
}

impl GatedFiles {
    /// Wait for a measure to be held; the directory.
    fn held(&self) -> PathBuf {
        let (lock, changed) = &*self.gate;
        let gate = lock.lock().unwrap();
        let (gate, _) = changed
            .wait_timeout_while(gate, common::STEP_LIMIT, |gate| gate.held.is_none())
            .unwrap();
        gate.held.clone().expect("no cleanup reached the gate")
    }
    fn open(&self) {
        let (lock, changed) = &*self.gate;
        lock.lock().unwrap().open = true;
        changed.notify_all();
    }
    fn wait(&self, dir: &Path) {
        if !dir.ends_with("worktree/target") {
            return;
        }
        let (lock, changed) = &*self.gate;
        let mut gate = lock.lock().unwrap();
        if gate.held.is_some() {
            return;
        }
        gate.held = Some(dir.to_owned());
        changed.notify_all();
        let _ = changed
            .wait_timeout_while(gate, common::STEP_LIMIT, |gate| !gate.open)
            .unwrap();
    }
    fn options(&self, options: SuperviseOptions) -> SuperviseOptions {
        SuperviseOptions {
            files: Some(RunFilesPort(Arc::new(self.clone()))),
            ..options
        }
    }
}

impl RunFiles for GatedFiles {
    fn create_dir_all(&self, dir: &Path) -> io::Result<()> {
        LocalRunFiles.create_dir_all(dir)
    }
    fn create_new_dir(&self, dir: &Path) -> io::Result<()> {
        LocalRunFiles.create_new_dir(dir)
    }
    fn write(&self, path: &Path, contents: &[u8]) -> io::Result<()> {
        LocalRunFiles.write(path, contents)
    }
    fn copy(&self, from: &Path, to: &Path) -> io::Result<()> {
        LocalRunFiles.copy(from, to)
    }
    fn read(&self, path: &Path) -> io::Result<Vec<u8>> {
        LocalRunFiles.read(path)
    }
    fn read_to_string(&self, path: &Path) -> io::Result<String> {
        LocalRunFiles.read_to_string(path)
    }
    fn modified(&self, path: &Path) -> io::Result<SystemTime> {
        LocalRunFiles.modified(path)
    }
    fn read_stamped(&self, path: &Path) -> Result<Option<(SystemTime, Vec<u8>)>> {
        LocalRunFiles.read_stamped(path)
    }
    fn is_file(&self, path: &Path) -> bool {
        LocalRunFiles.is_file(path)
    }
    fn is_dir(&self, path: &Path) -> bool {
        LocalRunFiles.is_dir(path)
    }
    fn exists(&self, path: &Path) -> bool {
        LocalRunFiles.exists(path)
    }
    fn read_dir(&self, dir: &Path) -> io::Result<Vec<PathBuf>> {
        LocalRunFiles.read_dir(dir)
    }
    fn rename(&self, from: &Path, to: &Path) -> io::Result<()> {
        LocalRunFiles.rename(from, to)
    }
    fn remove_file(&self, path: &Path) -> io::Result<()> {
        LocalRunFiles.remove_file(path)
    }
    fn tree_size(&self, dir: &Path) -> io::Result<Option<u64>> {
        self.wait(dir);
        LocalRunFiles.tree_size(dir)
    }
    fn remove_dir_all(&self, dir: &Path) -> io::Result<()> {
        LocalRunFiles.remove_dir_all(dir)
    }
    fn append_line(&self, path: &Path, line: &str) -> io::Result<()> {
        LocalRunFiles.append_line(path, line)
    }
    fn canonicalize(&self, path: &Path) -> io::Result<PathBuf> {
        LocalRunFiles.canonicalize(path)
    }
    fn write_fenced(&self, path: &Path, text: &str, info: &str, body: &Path) -> Result<()> {
        LocalRunFiles.write_fenced(path, text, info, body)
    }
    fn now(&self) -> SystemTime {
        LocalRunFiles.now()
    }
}

/// Task 405: the build outputs of an ended run are measured and removed
/// off the loop. While that cleanup is held, the supervisor claims the
/// next task and watches its run to its end; the cleanup is recorded once
/// it is done, with the payload of task 376, and once only.
#[test]
fn a_slow_cleanup_does_not_hold_up_the_loop() {
    let (_dir, repo, db) = fixture();
    {
        let mut queue = SqliteQueue::open(&db).unwrap();
        add_ready_task(&mut queue, "second task", &[]);
    }
    let backend = Arc::new(TestWorkspace::new(&db, false, BUILDING_AGENT));
    let files = GatedFiles::default();
    // One slot: task 1 is claimed first, task 2 once its run ended.
    let options = files.options(supervise_options(1, true));
    let supervisor = {
        let (db, repo, backend) = (db.clone(), repo.clone(), backend.clone());
        thread::spawn(move || supervise_with(&db, &repo, &backend, &options))
    };
    let held = files.held();
    let first = SqliteQueue::open(&db)
        .unwrap()
        .show(TaskId::new(1))
        .unwrap()
        .runs[0]
        .clone();
    assert_eq!(
        held,
        Path::new(first.worktree_path().unwrap()).join("target")
    );
    // The loop goes on: task 2 is claimed and its run watched to its end
    // while the cleanup of task 1's run is held.
    wait_until(&db, Duration::from_secs(60), |queue| {
        queue
            .show(TaskId::new(2))
            .unwrap()
            .runs
            .first()
            .is_some_and(|run| run.status() == RunStatus::Failed)
    });
    let mut queue = SqliteQueue::open(&db).unwrap();
    assert!(payloads_of(&queue, &first, "build_outputs_removed").is_empty());
    assert!(held.join("debug/big").is_file());

    files.open();
    let outcome = joined(supervisor, "the supervisor to finish").unwrap();
    backend.join();
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    assert!(!held.exists());
    let removed = payloads_of(&queue, &first, "build_outputs_removed");
    assert_eq!(
        removed,
        [
            json!({"paths": [held.to_string_lossy()], "bytes": removed[0]["bytes"], "by": "supervisor"})
        ]
    );
    assert!(removed[0]["bytes"].as_u64().unwrap() >= 65536);
    let second = queue.show(TaskId::new(2)).unwrap().runs[0].clone();
    assert_eq!(
        payloads_of(&queue, &second, "build_outputs_removed").len(),
        1
    );
    assert!(payloads_of(&queue, &first, "cleanup_failed").is_empty());
}

/// Task 405: a run leased while the cleanup is on another worktree (a
/// claim of it) is left alone when the cleanup reaches it.
#[test]
fn a_run_leased_during_the_cleanup_keeps_its_worktree() {
    let (_dir, repo, db) = fixture();
    {
        let mut queue = SqliteQueue::open(&db).unwrap();
        add_ready_task(&mut queue, "second task", &[]);
    }
    let backend = TestWorkspace::new(&db, false, BUILDING_AGENT);
    supervise(&db, &repo, &backend).unwrap();
    backend.join();
    let mut queue = SqliteQueue::open(&db).unwrap();
    let runs: Vec<TaskRun> = (1..=2)
        .map(|task| queue.show(TaskId::new(task)).unwrap().runs[0].clone())
        .collect();
    // Task 696: the runners of the ended runs went with their build outputs.
    let runner = |run: &TaskRun| Path::new(run.run_dir().unwrap()).join("runner");
    for run in &runs {
        assert!(!runner(run).exists());
        assert!(
            Path::new(run.run_dir().unwrap())
                .join("receipt.json")
                .is_file()
        );
    }
    // Both build again, and have a runner again.
    for run in &runs {
        let dir = Path::new(run.worktree_path().unwrap()).join("target/debug");
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("again"), "x").unwrap();
        fs::write(runner(run), "binary").unwrap();
    }
    let files = GatedFiles::default();
    let options = files.options(sweeping_options());
    let supervisor = {
        let (db, repo) = (db.clone(), repo.clone());
        let backend = TestWorkspace::new(&db, false, BUILDING_AGENT);
        thread::spawn(move || supervise_with(&db, &repo, &backend, &options))
    };
    let held = files.held();
    let (cleaned, leased) = if held.starts_with(runs[0].worktree_path().unwrap()) {
        (&runs[0], &runs[1])
    } else {
        (&runs[1], &runs[0])
    };
    // A live supervisor leases the other run meanwhile.
    Connection::open(&db)
        .unwrap()
        .execute(
            "INSERT INTO run_leases(run_id,token,pid,heartbeat_at) VALUES (?1,'live',?2,unixepoch()+100000)",
            rusqlite::params![leased.id(), std::process::id()],
        )
        .unwrap();
    files.open();
    joined(supervisor, "the supervisor to finish").unwrap();
    assert!(!held.exists());
    assert_eq!(
        payloads_of(&queue, cleaned, "build_outputs_removed").len(),
        2
    );
    let kept = Path::new(leased.worktree_path().unwrap()).join("target/debug/again");
    assert!(kept.is_file());
    // The leased run's runner is kept; the other's is gone.
    assert!(runner(leased).is_file());
    assert!(!runner(cleaned).exists());
    assert_eq!(
        payloads_of(&queue, leased, "build_outputs_removed").len(),
        1
    );
}

/// Task 405: once its task is canceled, a run whose worktree directory is
/// already gone loses the branch left behind, recorded as
/// `worktree_removed` with no bytes.
#[test]
fn a_canceled_task_loses_the_branch_of_a_worktree_already_gone() {
    let (_dir, repo, db) = fixture();
    let backend = TestWorkspace::new(&db, false, BUILDING_AGENT);
    supervise(&db, &repo, &backend).unwrap();
    backend.join();
    let mut queue = SqliteQueue::open(&db).unwrap();
    let run = queue.show(TaskId::new(1)).unwrap().runs[0].clone();
    let worktree = Path::new(run.worktree_path().unwrap());
    fs::remove_dir_all(worktree).unwrap();
    let branch = run.branch().unwrap();
    assert!(branch_exists(&repo, branch));
    queue
        .transition(TaskId::new(1), TaskAction::Cancel)
        .unwrap();

    supervise_with(&db, &repo, &backend, &sweeping_options()).unwrap();
    assert!(!branch_exists(&repo, branch));
    let removed = payloads_of(&queue, &run, "worktree_removed");
    assert_eq!(
        removed,
        [json!({
            "path": run.worktree_path().unwrap(),
            "branch": branch,
            "bytes": 0,
            "by": "supervisor",
            "reason": "task_canceled",
            "worktree_missing": true,
        })]
    );
    assert!(payloads_of(&queue, &run, "cleanup_failed").is_empty());
    // Nothing is left: another sweep records nothing.
    supervise_with(&db, &repo, &backend, &sweeping_options()).unwrap();
    assert_eq!(payloads_of(&queue, &run, "worktree_removed").len(), 1);
}

/// Task 405: a worktree whose `.git` still points at the repository's
/// common directory before the queue was rebound is repaired, then
/// removed with its branch.
#[test]
fn a_worktree_left_pointing_at_an_old_repository_is_repaired_and_removed() {
    let (_dir, repo, db) = fixture();
    let backend = TestWorkspace::new(&db, false, BUILDING_AGENT);
    supervise(&db, &repo, &backend).unwrap();
    backend.join();
    let mut queue = SqliteQueue::open(&db).unwrap();
    let run = queue.show(TaskId::new(1)).unwrap().runs[0].clone();
    let worktree = Path::new(run.worktree_path().unwrap());
    // As if the repository had moved: the same worktree record, under a
    // common directory that is gone.
    let gitfile = fs::read_to_string(worktree.join(".git")).unwrap();
    let admin = gitfile.trim().strip_prefix("gitdir: ").unwrap();
    let id = Path::new(admin).file_name().unwrap().to_string_lossy();
    fs::write(
        worktree.join(".git"),
        format!("gitdir: /nonexistent/old repository/.git/worktrees/{id}\n"),
    )
    .unwrap();
    let branch = run.branch().unwrap();
    queue
        .transition(TaskId::new(1), TaskAction::Cancel)
        .unwrap();

    supervise_with(&db, &repo, &backend, &sweeping_options()).unwrap();
    assert!(!worktree.exists());
    assert!(!branch_exists(&repo, branch));
    let removed = payloads_of(&queue, &run, "worktree_removed");
    assert_eq!(removed.len(), 1, "{removed:?}");
    assert_eq!(removed[0]["reason"], "task_canceled");
    assert_eq!(removed[0]["repaired"], true);
    assert!(removed[0]["bytes"].as_u64().unwrap() > 0);
    assert!(payloads_of(&queue, &run, "cleanup_failed").is_empty());
}

/// Run files whose removal of a directory under a `raced` directory finds
/// it gone (another cleanup removed it first), and under a `denied` one
/// fails.
struct RacyFiles;

impl RunFiles for RacyFiles {
    fn create_dir_all(&self, dir: &Path) -> io::Result<()> {
        LocalRunFiles.create_dir_all(dir)
    }
    fn create_new_dir(&self, dir: &Path) -> io::Result<()> {
        LocalRunFiles.create_new_dir(dir)
    }
    fn write(&self, path: &Path, contents: &[u8]) -> io::Result<()> {
        LocalRunFiles.write(path, contents)
    }
    fn copy(&self, from: &Path, to: &Path) -> io::Result<()> {
        LocalRunFiles.copy(from, to)
    }
    fn read(&self, path: &Path) -> io::Result<Vec<u8>> {
        LocalRunFiles.read(path)
    }
    fn read_to_string(&self, path: &Path) -> io::Result<String> {
        LocalRunFiles.read_to_string(path)
    }
    fn modified(&self, path: &Path) -> io::Result<SystemTime> {
        LocalRunFiles.modified(path)
    }
    fn read_stamped(&self, path: &Path) -> Result<Option<(SystemTime, Vec<u8>)>> {
        LocalRunFiles.read_stamped(path)
    }
    fn is_file(&self, path: &Path) -> bool {
        LocalRunFiles.is_file(path)
    }
    fn is_dir(&self, path: &Path) -> bool {
        LocalRunFiles.is_dir(path)
    }
    fn exists(&self, path: &Path) -> bool {
        LocalRunFiles.exists(path)
    }
    fn read_dir(&self, dir: &Path) -> io::Result<Vec<PathBuf>> {
        LocalRunFiles.read_dir(dir)
    }
    fn rename(&self, from: &Path, to: &Path) -> io::Result<()> {
        LocalRunFiles.rename(from, to)
    }
    fn remove_file(&self, path: &Path) -> io::Result<()> {
        LocalRunFiles.remove_file(path)
    }
    fn tree_size(&self, dir: &Path) -> io::Result<Option<u64>> {
        LocalRunFiles.tree_size(dir)
    }
    fn remove_dir_all(&self, dir: &Path) -> io::Result<()> {
        match dir
            .parent()
            .and_then(Path::file_name)
            .and_then(|name| name.to_str())
        {
            // Someone else removes it between its measure and its removal.
            Some("raced") => {
                LocalRunFiles.remove_dir_all(dir)?;
                Err(io::ErrorKind::NotFound.into())
            }
            Some("denied") => Err(io::ErrorKind::PermissionDenied.into()),
            _ => LocalRunFiles.remove_dir_all(dir),
        }
    }
    fn append_line(&self, path: &Path, line: &str) -> io::Result<()> {
        LocalRunFiles.append_line(path, line)
    }
    fn canonicalize(&self, path: &Path) -> io::Result<PathBuf> {
        LocalRunFiles.canonicalize(path)
    }
    fn write_fenced(&self, path: &Path, text: &str, info: &str, body: &Path) -> Result<()> {
        LocalRunFiles.write_fenced(path, text, info, body)
    }
    fn now(&self) -> SystemTime {
        LocalRunFiles.now()
    }
}

/// Task 1100: a scratchpad another cleanup removed between its measure and
/// its removal is no failure, and one that cannot be removed under a root
/// records `cleanup_failed` while what was removed under another root is
/// recorded as `scratchpad_removed`; a later sweep tries it again.
#[test]
fn a_scratchpad_gone_meanwhile_is_no_failure_and_one_failing_root_keeps_the_others() {
    let (dir, repo, db) = fixture();
    let backend = TestWorkspace::new(&db, false, BUILDING_AGENT);
    supervise(&db, &repo, &backend).unwrap();
    backend.join();
    let mut queue = SqliteQueue::open(&db).unwrap();
    let run = queue.show(TaskId::new(1)).unwrap().runs[0].clone();
    queue
        .transition(TaskId::new(1), TaskAction::Cancel)
        .unwrap();
    let roots: Vec<PathBuf> = ["raced", "denied", "kept"]
        .iter()
        .map(|name| dir.path().join(name))
        .collect();
    let scratchpads: Vec<PathBuf> = roots
        .iter()
        .map(|root| scratchpad_of(root, run.worktree_path().unwrap()))
        .collect();
    for scratchpad in &scratchpads {
        fs::create_dir_all(scratchpad.join("session/scratchpad")).unwrap();
        fs::write(scratchpad.join("session/scratchpad/notes"), "x").unwrap();
    }
    let options = SuperviseOptions {
        files: Some(RunFilesPort(Arc::new(RacyFiles))),
        scratchpad_roots: Some(roots.clone()),
        ..sweeping_options()
    };
    supervise_with(&db, &repo, &backend, &options).unwrap();
    backend.join();

    assert!(!scratchpads[0].exists());
    assert!(scratchpads[1].is_dir());
    assert!(!scratchpads[2].exists());
    let removed = payloads_of(&queue, &run, "scratchpad_removed");
    assert_eq!(removed.len(), 1, "{removed:?}");
    assert_eq!(
        removed[0]["paths"],
        json!([scratchpads[2].to_string_lossy()])
    );
    let failed = payloads_of(&queue, &run, "cleanup_failed");
    assert_eq!(failed.len(), 1, "{failed:?}");
    assert_eq!(failed[0]["path"], scratchpads[1].to_string_lossy().as_ref());
    assert!(
        failed[0]["message"]
            .as_str()
            .unwrap()
            .starts_with("scratchpad "),
        "{failed:?}"
    );

    // Once it can be removed, the next sweep does.
    let options = SuperviseOptions {
        scratchpad_roots: Some(roots),
        ..sweeping_options()
    };
    supervise_with(&db, &repo, &backend, &options).unwrap();
    backend.join();
    assert!(!scratchpads[1].exists());
    let removed = payloads_of(&queue, &run, "scratchpad_removed");
    assert_eq!(removed.len(), 2, "{removed:?}");
    assert_eq!(
        removed[1]["paths"],
        json!([scratchpads[1].to_string_lossy()])
    );
}
