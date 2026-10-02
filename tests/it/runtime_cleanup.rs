//! Runtime tests: the cleanup of ended runs' worktrees off the supervisor's
//! loop, the branches of worktrees already gone, and worktrees a rebind
//! left pointing at an old repository (task 405), the runners of ended
//! runs (task 696), the build outputs of the runs left waiting for an
//! answer, a landing or a resume (task 1289), and the temporary files
//! directories of the runs whose task is over (task 1290).
use crate::{common, runtime_support};
use dagq::infrastructure::git_binary::git_executable;

use dagq::domain::disk::DiskConfig;
use dagq::{application::RunFiles, runtime::RunFilesPort};
use runtime_support::*;
use std::{io, sync::Condvar, sync::atomic::AtomicBool};

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
    Command::new(git_executable().expect("git executable"))
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
    fn open_agent_dir(&self, dir: &Path) -> io::Result<dagq::application::AgentDir> {
        LocalRunFiles.open_agent_dir(dir)
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
            json!({"paths": [held.to_string_lossy()], "bytes": removed[0]["bytes"], "by": "supervisor", "reason": "run_ended"})
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
    fn open_agent_dir(&self, dir: &Path) -> io::Result<dagq::application::AgentDir> {
        LocalRunFiles.open_agent_dir(dir)
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

/// A run whose review returned `concern`: `awaiting_integration` with no
/// lease and no live session, its `approve_landing` ask open. Its worktree
/// then gets build outputs, and an uncommitted source with `uncommitted`;
/// returns the run and the ask.
fn run_awaiting_an_answer(
    db: &Path,
    repo: &Path,
    backend: &TestWorkspace,
    uncommitted: bool,
) -> (TaskRun, dagq::domain::Ask) {
    let reviewer = TestReviewer::new(&[verdict("concern", &["a finding"], "first")]);
    let outcome = supervise_reviewed(db, repo, backend, &reviewer);
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    let mut queue = SqliteQueue::open(db).unwrap();
    let run = queue.show(TaskId::new(1)).unwrap().runs[0].clone();
    assert_eq!(run.status(), RunStatus::AwaitingIntegration);
    assert!(queue.run_lease(run.id()).unwrap().is_none());
    let ask = queue.asks(Default::default()).unwrap()[0].clone();
    assert_eq!(ask.kind, AskKind::ApproveLanding);
    let worktree = Path::new(run.worktree_path().unwrap());
    for dir in ["target/debug", "llvm-cov-target/debug"] {
        fs::create_dir_all(worktree.join(dir)).unwrap();
        fs::write(worktree.join(dir).join("big"), vec![0u8; 65536]).unwrap();
    }
    if uncommitted {
        fs::write(worktree.join("uncommitted.rs"), "fn main() {}\n").unwrap();
    }
    (run, ask)
}

/// What a cleanup must leave of a run whose build outputs it removed.
fn assert_kept_but_the_build_outputs(repo: &Path, run: &TaskRun) {
    let worktree = Path::new(run.worktree_path().unwrap());
    assert!(!worktree.join("target").exists());
    assert!(!worktree.join("llvm-cov-target").exists());
    assert!(worktree.join("uncommitted.rs").is_file());
    assert!(worktree.join("change.txt").is_file());
    assert!(branch_exists(repo, run.branch().unwrap()));
    let run_dir = Path::new(run.run_dir().unwrap());
    assert!(run_dir.join("receipt.json").is_file());
}

/// Task 1289: the build outputs of a run waiting for a person's answer go
/// on the cleanup's usual sweep, whatever the free space, recorded as
/// `build_outputs_removed` with the reason `awaiting_answer` and the ask;
/// not while a live lease or a live session holds the run. The worktree,
/// its branch and uncommitted source, the receipt and the runner stay, and
/// no `auto_repaired` is recorded.
#[test]
fn the_build_outputs_of_a_run_waiting_for_an_answer_go_on_the_sweep() {
    let (_dir, repo, db) = fixture();
    let backend = TestWorkspace::new(&db, false, IDLE_AGENT);
    let (run, ask) = run_awaiting_an_answer(&db, &repo, &backend, true);
    let queue = SqliteQueue::open(&db).unwrap();
    let raw = Connection::open(&db).unwrap();
    let candidate = |queue: &SqliteQueue| {
        queue
            .ended_run_worktrees()
            .unwrap()
            .into_iter()
            .find(|w| w.run_id == *run.id())
            .map(|w| w.cleanup)
    };
    assert_eq!(
        candidate(&queue),
        Some(dagq::application::WorktreeCleanup::AwaitingAnswer(ask.id))
    );
    // A live lease (a supervisor working on it, or a run waiting in its
    // session, ADR-0071) or a live session keeps it out.
    raw.execute(
        "INSERT INTO run_leases(run_id,token,pid,heartbeat_at) VALUES (?1,'live',?2,unixepoch()+100000)",
        rusqlite::params![run.id(), std::process::id()],
    )
    .unwrap();
    assert_eq!(candidate(&queue), None);
    raw.execute("DELETE FROM run_leases WHERE run_id=?1", [run.id()])
        .unwrap();
    raw.execute(
        "INSERT OR REPLACE INTO run_processes(run_id,role,pid,heartbeat_at,exited_at) VALUES (?1,'agent',?2,unixepoch()+100000,NULL)",
        rusqlite::params![run.id(), std::process::id()],
    )
    .unwrap();
    assert_eq!(candidate(&queue), None);
    raw.execute(
        "UPDATE run_processes SET exited_at=unixepoch() WHERE run_id=?1 AND role='agent'",
        [run.id()],
    )
    .unwrap();
    let runner = Path::new(run.run_dir().unwrap()).join("runner");
    fs::write(&runner, "binary").unwrap();

    let outcome = supervise_with(&db, &repo, &backend, &sweeping_options()).unwrap();
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    assert_kept_but_the_build_outputs(&repo, &run);
    assert!(runner.is_file());
    let removed = payloads_of(&queue, &run, "build_outputs_removed");
    assert_eq!(removed.len(), 1, "{removed:?}");
    assert_eq!(removed[0]["reason"], "awaiting_answer");
    assert_eq!(removed[0]["ask_id"], json!(ask.id));
    assert_eq!(removed[0]["paths"].as_array().unwrap().len(), 2);
    assert!(removed[0]["bytes"].as_u64().unwrap() >= 2 * 65536);
    assert!(
        queue
            .all_events()
            .unwrap()
            .iter()
            .all(|event| event.kind != "auto_repaired")
    );
    assert!(queue.read_ask(ask.id).unwrap().is_open());
    assert_eq!(
        queue.run(run.id()).unwrap().status(),
        RunStatus::AwaitingIntegration
    );
}

/// Task 1289: an answer that comes while the cleanup clears the run's
/// build outputs is applied, but the landing waits until the job has
/// passed the run; then the run lands.
#[test]
fn a_landing_waits_for_the_cleanup_of_its_build_outputs() {
    let (_dir, repo, db) = fixture();
    let backend = Arc::new(TestWorkspace::new(&db, false, IDLE_AGENT));
    // Nothing uncommitted: the landing would defer a dirty worktree.
    let (run, ask) = run_awaiting_an_answer(&db, &repo, &backend, false);
    let files = GatedFiles::default();
    let stop = Arc::new(AtomicBool::new(false));
    let options = files.options(SuperviseOptions {
        stop: stop.clone(),
        ..SuperviseOptions {
            sweep_interval: Duration::ZERO,
            ..supervise_options(4, false)
        }
    });
    let passes = options.passes.clone();
    let supervisor = {
        let (db, repo, backend) = (db.clone(), repo.clone(), backend.clone());
        thread::spawn(move || {
            supervise_reviewed_with(&db, &repo, &backend, &TestReviewer::new(&[]), &options)
        })
    };
    let held = files.held();
    assert_eq!(held, Path::new(run.worktree_path().unwrap()).join("target"));
    let mut queue = SqliteQueue::open(&db).unwrap();
    queue.answer(ask.id, "land").unwrap();
    wait_until(&db, Duration::from_secs(30), |queue| {
        queue.read_ask(ask.id).unwrap().closed_at.is_some()
    });
    await_passes(&passes, SOME_PASSES);
    assert!(payloads_of(&queue, &run, "landing_queued").len() == 1);
    assert!(payloads_of(&queue, &run, "integration_started").is_empty());
    assert_eq!(
        queue.run(run.id()).unwrap().status(),
        RunStatus::AwaitingIntegration
    );
    assert!(queue.run_lease(run.id()).unwrap().is_none());

    files.open();
    wait_until(&db, Duration::from_secs(60), |queue| {
        queue.run(run.id()).unwrap().status() == RunStatus::Integrated
    });
    stop.store(true, Ordering::SeqCst);
    let outcome = joined(supervisor, "the supervisor to finish");
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    // The landing began once the job had passed the run (the event is
    // recorded when the loop joins the job, which may come after).
    assert!(!held.exists());
    let removed = payloads_of(&queue, &run, "build_outputs_removed");
    assert_eq!(removed.len(), 1, "{removed:?}");
    assert_eq!(removed[0]["reason"], "awaiting_answer");
}

const GIB: u64 = 1 << 30;

/// The build outputs whose presence makes the idle-run test's disk short.
static IDLE_TARGET: Mutex<Option<PathBuf>> = Mutex::new(None);
static IDLE_SHORT: AtomicBool = AtomicBool::new(false);

fn short_while_idle_target(_: &Path) -> Option<u64> {
    let target = IDLE_TARGET
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    Some(
        if IDLE_SHORT.load(Ordering::SeqCst) && target.as_ref().is_some_and(|dir| dir.exists()) {
            1
        } else {
            4 * GIB
        },
    )
}

/// Task 1289: the build outputs of a run nobody works on that waits for no
/// answer (here its ask closed unanswered) stay while there is room, and
/// go only in a cleanup for disk space: `build_outputs_removed` with the
/// reason `disk_space`, counted in `auto_repaired` (`disk_cleanup`).
#[test]
fn the_build_outputs_of_an_idle_run_go_only_for_disk_space() {
    let (_dir, repo, db) = fixture();
    let backend = TestWorkspace::new(&db, false, IDLE_AGENT);
    let (run, ask) = run_awaiting_an_answer(&db, &repo, &backend, true);
    let mut queue = SqliteQueue::open(&db).unwrap();
    queue.answer(ask.id, "withdrawn").unwrap();
    queue.close_ask(ask.id).unwrap();
    let target = Path::new(run.worktree_path().unwrap()).join("target");
    *IDLE_TARGET.lock().unwrap() = Some(target.clone());
    assert_eq!(
        queue
            .ended_run_worktrees()
            .unwrap()
            .iter()
            .find(|w| w.run_id == *run.id())
            .map(|w| w.cleanup),
        Some(dagq::application::WorktreeCleanup::Idle)
    );
    let options = SuperviseOptions {
        disk: Some(DiskConfig {
            min_free_bytes: Some(GIB),
            ..DiskConfig::default()
        }),
        free_space: short_while_idle_target,
        ..sweeping_options()
    };
    // Room: they stay.
    IDLE_SHORT.store(false, Ordering::SeqCst);
    let outcome = supervise_with(&db, &repo, &backend, &options).unwrap();
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    assert!(target.join("debug/big").is_file());
    assert!(payloads_of(&queue, &run, "build_outputs_removed").is_empty());

    // Short: they go, for disk space.
    IDLE_SHORT.store(true, Ordering::SeqCst);
    let outcome = supervise_with(&db, &repo, &backend, &options).unwrap();
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    assert_kept_but_the_build_outputs(&repo, &run);
    let removed = payloads_of(&queue, &run, "build_outputs_removed");
    assert_eq!(removed.len(), 1, "{removed:?}");
    assert_eq!(removed[0]["reason"], "disk_space");
    assert!(removed[0].get("ask_id").is_none());
    let repaired: Vec<Value> = queue
        .all_events()
        .unwrap()
        .into_iter()
        .filter(|event| event.kind == "auto_repaired")
        .map(|event| event.payload)
        .collect();
    assert_eq!(repaired.len(), 1, "{repaired:?}");
    assert_eq!(repaired[0]["repair"], "disk_cleanup");
    assert_eq!(repaired[0]["bytes"], removed[0]["bytes"]);
    assert_eq!(repaired[0]["detail"]["runs"], json!([run.id().as_str()]));
    assert_eq!(
        queue.run(run.id()).unwrap().status(),
        RunStatus::AwaitingIntegration
    );
}

/// Task 1290: the temporary files directory a run's Codex turns got as
/// their `TMPDIR` (`tmp` in its run directory) stays while its task goes
/// on, a resume may go on in it, and goes once the task is canceled,
/// recorded as `run_tmp_removed` with its bytes; the rest of the run
/// directory (the receipt, the logs) stays.
#[test]
fn a_runs_tmp_dir_goes_only_once_its_task_is_over() {
    let (_dir, repo, db) = fixture();
    let agent = "mkdir -p ../tmp/target/debug; head -c 32768 /dev/zero > ../tmp/target/debug/big; \
         echo log > ../kept.log; "
        .to_owned()
        + BUILDING_AGENT;
    let backend = TestWorkspace::new(&db, false, &agent);
    supervise(&db, &repo, &backend).unwrap();
    backend.join();
    let mut queue = SqliteQueue::open(&db).unwrap();
    let run = queue.show(TaskId::new(1)).unwrap().runs[0].clone();
    let run_dir = PathBuf::from(run.run_dir().unwrap());
    let tmp = run_dir.join("tmp");
    // The run failed and its task goes on: its build outputs went, its
    // temporary files stay.
    assert_eq!(payloads_of(&queue, &run, "build_outputs_removed").len(), 1);
    assert!(tmp.join("target/debug/big").is_file());
    assert!(payloads_of(&queue, &run, "run_tmp_removed").is_empty());

    queue
        .transition(TaskId::new(1), TaskAction::Cancel)
        .unwrap();
    supervise_with(&db, &repo, &backend, &sweeping_options()).unwrap();
    backend.join();
    assert!(!tmp.exists());
    let removed = payloads_of(&queue, &run, "run_tmp_removed");
    assert_eq!(removed.len(), 1, "{removed:?}");
    assert_eq!(removed[0]["paths"], json!([tmp.to_string_lossy()]));
    assert!(
        removed[0]["bytes"].as_u64().unwrap() >= 32768,
        "{removed:?}"
    );
    assert_eq!(removed[0]["by"], "supervisor");
    assert_eq!(removed[0]["reason"], "task_canceled");
    assert!(run_dir.join("receipt.json").is_file());
    assert!(run_dir.join("kept.log").is_file());
    // Not recorded again on a later sweep.
    supervise_with(&db, &repo, &backend, &sweeping_options()).unwrap();
    backend.join();
    assert_eq!(payloads_of(&queue, &run, "run_tmp_removed").len(), 1);
}

/// State belongs to the single matrix test below, including under cargo test.
struct DrainDisk {
    db: PathBuf,
    outputs: Vec<PathBuf>,
    enough: bool,
    stop: Arc<AtomicBool>,
    stop_requested: bool,
    stopping_passes: usize,
}

static DRAIN_DISK: Mutex<Option<DrainDisk>> = Mutex::new(None);
static DRAIN_READS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

fn drain_free_space(_: &Path) -> Option<u64> {
    DRAIN_READS.fetch_add(1, Ordering::SeqCst);
    let mut state = DRAIN_DISK.lock().unwrap();
    let state = state.as_mut()?;
    // Request stop from a known point in a pass. The next reading follows
    // poll_cleanup(ending=true), including when an empty loop then exits.
    if state.stop_requested {
        state.stop.store(true, Ordering::SeqCst);
        state.stopping_passes += 1;
    }
    let queued = SqliteQueue::open(&state.db)
        .unwrap()
        .latest_event_of("landing_queued")
        .unwrap()
        .is_some();
    Some(
        if queued && (!state.enough || state.outputs.iter().any(|path| path.exists())) {
            1
        } else {
            1 << 40
        },
    )
}

/// Task 648: stop and handoff both finish a disk cleanup, retain a landing
/// lease until the fresh reading, then either land or return it. Ordinary
/// cleanup still stops at the current worktree.
#[test]
fn draining_finishes_disk_cleanup_before_deciding_a_landing() {
    for handoff in [false, true] {
        for disk in [None, Some(false), Some(true)] {
            let (_dir, repo, db) = fixture();
            let mut queue = SqliteQueue::open(&db).unwrap();
            add_ready_task(&mut queue, "second cleanup", &[]);
            add_ready_task(&mut queue, "third cleanup", &[]);
            let backend = TestWorkspace::new(&db, false, BUILDING_AGENT);
            supervise(&db, &repo, &backend).unwrap();
            backend.join();
            let runs: Vec<_> = (1..=3)
                .map(|id| queue.show(TaskId::new(id)).unwrap().runs[0].clone())
                .collect();
            let outputs: Vec<_> = runs
                .iter()
                .map(|run| {
                    let target = Path::new(run.worktree_path().unwrap()).join("target");
                    fs::create_dir_all(&target).unwrap();
                    fs::write(target.join("left"), vec![0; 4096]).unwrap();
                    target
                })
                .collect();
            if disk.is_some() {
                queue
                    .transition(TaskId::new(3), TaskAction::Cancel)
                    .unwrap();
                add_ready_task(&mut queue, "landing", &[]);
            }
            let files = GatedFiles::default();
            let stop = Arc::new(AtomicBool::new(false));
            *DRAIN_DISK.lock().unwrap() = Some(DrainDisk {
                db: db.clone(),
                outputs: outputs.clone(),
                enough: disk == Some(true),
                stop: stop.clone(),
                stop_requested: false,
                stopping_passes: 0,
            });
            let options = files.options(SuperviseOptions {
                stop: stop.clone(),
                disk: Some(dagq::domain::disk::DiskConfig {
                    min_free_bytes: Some(1 << 30),
                    ..Default::default()
                }),
                free_space: drain_free_space,
                ..supervise_options(1, false)
            });
            let backend = Arc::new(TestWorkspace::new(&db, false, IDLE_AGENT));
            let supervisor = {
                let (db, repo, backend) = (db.clone(), repo.clone(), backend.clone());
                thread::spawn(move || {
                    let reviewer = TestReviewer::new(&[verdict("pass", &[], "meets acceptance")]);
                    runtime::supervise_with_reviewer(
                        &db,
                        &repo,
                        &*backend,
                        &claude_stub(&db),
                        &reviewer,
                        Path::new(env!("CARGO_BIN_EXE_dagq")),
                        &options,
                    )
                })
            };
            let held = files.held();
            let landing = disk.map(|_| {
                wait_until(&db, common::STEP_LIMIT, |q| {
                    q.latest_event_of("landing_queued").unwrap().is_some()
                });
                queue.show(TaskId::new(4)).unwrap().runs[0].clone()
            });
            // Let check_disk promote the running job before asking to end.
            let reads = DRAIN_READS.load(Ordering::SeqCst);
            wait_until(&db, common::STEP_LIMIT, |_| {
                DRAIN_READS.load(Ordering::SeqCst) >= reads + 3
            });
            if handoff {
                let registration = queue.supervisors().unwrap().pop().unwrap();
                assert!(
                    queue
                        .request_handoff(&registration.token, "/next/dagq")
                        .unwrap()
                );
            } else {
                DRAIN_DISK.lock().unwrap().as_mut().unwrap().stop_requested = true;
            }
            // Multiple full passes after the request, while cleanup is gated.
            let reads = DRAIN_READS.load(Ordering::SeqCst);
            if disk.is_some() || handoff {
                wait_until(&db, common::STEP_LIMIT, |_| {
                    DRAIN_READS.load(Ordering::SeqCst) >= reads + 4
                });
            } else {
                // With no slots a stop leaves the loop and joins the job.
                wait_until(&db, common::STEP_LIMIT, |_| {
                    DRAIN_DISK.lock().unwrap().as_ref().unwrap().stopping_passes >= 2
                });
            }
            assert!(!supervisor.is_finished());
            if let Some(run) = &landing {
                assert!(queue.run_lease(run.id()).unwrap().is_some());
                assert_eq!(
                    queue.run(run.id()).unwrap().status(),
                    RunStatus::AwaitingIntegration
                );
                assert!(payloads_of(&queue, run, "integration_started").is_empty());
            }
            files.open();
            let outcome = joined(supervisor, "the cleanup and drain to finish").unwrap();
            backend.join();
            assert_eq!(outcome["errors"], json!([]), "{outcome}");
            for (run, output) in runs.iter().zip(&outputs) {
                let cleaned = disk.is_some() || *output == held;
                assert_eq!(
                    !output.exists(),
                    cleaned,
                    "handoff={handoff}, disk={disk:?}"
                );
                if disk.is_some() && run.task_id() == TaskId::new(3) {
                    assert_eq!(payloads_of(&queue, run, "worktree_removed").len(), 1);
                } else {
                    assert_eq!(
                        payloads_of(&queue, run, "build_outputs_removed").len(),
                        if cleaned { 2 } else { 1 }
                    );
                }
            }
            if let Some(run) = &landing {
                assert!(queue.run_lease(run.id()).unwrap().is_none());
                assert_eq!(
                    queue.run(run.id()).unwrap().status(),
                    if disk == Some(true) {
                        RunStatus::Integrated
                    } else {
                        RunStatus::AwaitingIntegration
                    }
                );
                let repair = queue.latest_event_of("auto_repaired").unwrap().unwrap();
                assert_eq!(repair.payload["repair"], "disk_cleanup");
                assert_eq!(
                    repair.payload["detail"]["runs"].as_array().unwrap().len(),
                    3
                );
            }
        }
    }
}
