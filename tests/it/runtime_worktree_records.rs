//! Runtime tests: the names of the records Git keeps in `.git/worktrees`
//! for the runs' worktrees and the scratches. Git names a record after
//! the last component of the worktree's path, which is the run's own
//! (`<runs>/<id>/worktree-<id>`) or the scratch's, so no add takes over the
//! name of a record that went or races another add for one; a worktree
//! made before (at `<runs>/<id>/worktree`) is read and removed as before;
//! and a scratch leaves no record once it is removed, wherever its making
//! stopped.
use crate::common::{self, Bounded};
use crate::runtime_support::git as in_repo;
use dagq::domain::{Provider, RunId, RunRecord, RunStatus, TaskId, TaskRun};
use dagq::infrastructure::adapters::GitRepository;
use dagq::infrastructure::git_binary::git_executable;
use std::{
    collections::BTreeSet,
    fs,
    path::{Path, PathBuf},
    process::Command,
    sync::{
        Barrier,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::{Duration, Instant},
};

const RUN_A: &str = "1a2b3c4d-0000-4000-8000-00000000000a";
const RUN_B: &str = "1a2b3c4d-0000-4000-8000-00000000000b";
const RUN_C: &str = "1a2b3c4d-0000-4000-8000-00000000000c";

/// How long the adds made at once may take, and the pruners prune.
const AT_ONCE: Duration = Duration::from_secs(60);

/// A repository with one commit on `main` in `dir`, and Git on it.
fn repository(dir: &Path) -> (PathBuf, GitRepository) {
    let repo = dir.join("repo");
    fs::create_dir(&repo).unwrap();
    common::template::repository(&repo, "worktree records\n");
    let git = GitRepository::inspect(&repo).unwrap();
    (repo, git)
}

/// The run `id` on `dagq/<id>` at main's head, its worktree where the
/// supervisor makes it, `<runs>/<id>/worktree-<id>`.
fn worktree_run(git: &GitRepository, runs: &Path, id: &str) -> TaskRun {
    run_with_worktree(git, runs, id, &format!("worktree-{id}"))
}

/// The run `id` on `dagq/<id>` at main's head, its worktree at
/// `<runs>/<id>/<name>`.
fn run_with_worktree(git: &GitRepository, runs: &Path, id: &str, name: &str) -> TaskRun {
    let run_dir = runs.join(id);
    fs::create_dir_all(&run_dir).unwrap();
    let text = |path: PathBuf| Some(path.to_str().unwrap().to_owned());
    TaskRun::restore(RunRecord {
        id: RunId::new(id).unwrap(),
        task_id: TaskId::new(1),
        status: RunStatus::Starting,
        requested_provider: Provider::Claude,
        actual_provider: Provider::Claude,
        worker_mode: dagq::domain::worker::WorkerMode::Headless,
        base_commit: git.main_head().unwrap(),
        branch: Some(format!("dagq/{id}")),
        worktree_path: text(run_dir.join(name)),
        workspace_id: None,
        receipt_path: text(run_dir.join("receipt.json")),
        log_path: text(run_dir.join("log")),
        result_commit: None,
        repo_path: None,
        run_dir: text(run_dir),
        last_error: None,
        workspace_closed_at: None,
        created_at: String::new(),
    })
    .unwrap()
}

fn worktree_of(run: &TaskRun) -> PathBuf {
    PathBuf::from(run.worktree_path().unwrap())
}

/// The name of the record in `.git/worktrees` the worktree at `path`
/// points at.
fn record_name(path: &Path) -> String {
    let link = fs::read_to_string(path.join(".git")).unwrap();
    let record = Path::new(link.trim().strip_prefix("gitdir: ").unwrap());
    record.file_name().unwrap().to_string_lossy().into_owned()
}

/// The branch `git` reads the worktree at `path` on.
fn branch_at(git: &GitRepository, path: &Path) -> Option<String> {
    git.current_branch(path).unwrap()
}

fn on(id: &str) -> Option<String> {
    Some(format!("refs/heads/dagq/{id}"))
}

/// `git worktree list --porcelain` of `repo`.
fn listed(repo: &Path) -> String {
    let output = Command::new(git_executable().unwrap())
        .arg("-C")
        .arg(repo)
        .args(["worktree", "list", "--porcelain"])
        .bounded_output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");
    String::from_utf8(output.stdout).unwrap()
}

/// Each run's worktree has a record named by its run, as its path is. One
/// run's record going while its directory stays (as a prune or a removal
/// that failed half way leaves it) leaves no name a later run takes over:
/// the later run's record has its own name, the directory left still
/// points at no record of another run, and the run that stays still reads
/// its own branch. The recheck's scratch has a name no run's has.
#[test]
fn each_run_worktree_has_a_record_of_its_own_name_that_no_later_run_takes_over() {
    let _test = common::test();
    let dir = tempfile::tempdir().unwrap();
    let (repo, git) = repository(dir.path());
    let runs = dir.path().join("runs");
    let (a, b) = (
        worktree_run(&git, &runs, RUN_A),
        worktree_run(&git, &runs, RUN_B),
    );
    git.create_worktree(&a).unwrap();
    git.create_worktree(&b).unwrap();
    assert_eq!(record_name(&worktree_of(&a)), format!("worktree-{RUN_A}"));
    assert_eq!(record_name(&worktree_of(&b)), format!("worktree-{RUN_B}"));
    // A's record goes; its directory, and the `.git` in it naming the
    // record, stay.
    let lost = repo
        .join(".git/worktrees")
        .join(format!("worktree-{RUN_A}"));
    fs::remove_dir_all(&lost).unwrap();
    assert!(worktree_of(&a).join(".git").is_file());
    let c = worktree_run(&git, &runs, RUN_C);
    git.create_worktree(&c).unwrap();
    assert_eq!(record_name(&worktree_of(&c)), format!("worktree-{RUN_C}"));
    assert!(!lost.exists(), "a later add took the name A's .git names");
    assert_eq!(branch_at(&git, &worktree_of(&b)), on(RUN_B));
    assert_eq!(branch_at(&git, &worktree_of(&c)), on(RUN_C));
    let scratch = crate::runtime_support::recheck_scratch(&dir.path().join("recheck"));
    let head = git.main_head().unwrap();
    git.checkout_scratch(&scratch, head.as_str()).unwrap();
    assert_eq!(
        record_name(&scratch),
        scratch.file_name().unwrap().to_str().unwrap()
    );
    assert_eq!(branch_at(&git, &worktree_of(&b)), on(RUN_B));
}

/// Worktrees added at once in one repository, as two supervisors provision
/// their runs while others prune, each get a record and a branch of their
/// own: an add that met another's record half written, or a prune, is
/// run again.
#[test]
fn worktrees_added_at_once_do_not_race_for_a_record_name() {
    let _test = common::test();
    let dir = tempfile::tempdir().unwrap();
    let (repo, git) = repository(dir.path());
    let runs = dir.path().join("runs");
    let ids: Vec<String> = (0..8)
        .map(|n| format!("1a2b3c4d-0000-4000-8000-0000000001{n:02}"))
        .collect();
    let made: Vec<TaskRun> = ids.iter().map(|id| worktree_run(&git, &runs, id)).collect();
    let head = git.main_head().unwrap();
    let scratches: Vec<PathBuf> = (0..4)
        .map(|n| crate::runtime_support::recheck_scratch(&dir.path().join(format!("q{n}/recheck"))))
        .collect();
    // Every thread's Git first, so none waits at the barrier for one that
    // never starts.
    let adders: Vec<GitRepository> = (0..made.len() + scratches.len())
        .map(|_| GitRepository::inspect(&repo).unwrap())
        .collect();
    let pruners: Vec<GitRepository> = (0..2)
        .map(|_| GitRepository::inspect(&repo).unwrap())
        .collect();
    let barrier = Barrier::new(adders.len());
    let adding = AtomicBool::new(true);
    let deadline = Instant::now() + AT_ONCE;
    let (barrier, adding) = (&barrier, &adding);
    let results = {
        let _waiting = common::within(AT_ONCE * 2, "the worktrees added at once");
        thread::scope(|scope| {
            let mut adders = adders.into_iter();
            let mut handles = Vec::new();
            for run in &made {
                let git = adders.next().unwrap();
                handles.push(scope.spawn(move || {
                    barrier.wait();
                    git.create_worktree(run).map(|_| ())
                }));
            }
            for scratch in &scratches {
                let (git, head) = (adders.next().unwrap(), head.clone());
                handles.push(scope.spawn(move || {
                    barrier.wait();
                    git.checkout_scratch(scratch, head.as_str())
                }));
            }
            let pruners: Vec<_> = pruners
                .into_iter()
                .map(|git| {
                    scope.spawn(move || {
                        while adding.load(Ordering::SeqCst) && Instant::now() < deadline {
                            let _ = git.prune_worktrees();
                        }
                    })
                })
                .collect();
            let results: Vec<_> = handles.into_iter().map(|handle| handle.join()).collect();
            adding.store(false, Ordering::SeqCst);
            for pruner in pruners {
                pruner.join().unwrap();
            }
            results
        })
    };
    for result in results {
        result.unwrap().unwrap();
    }
    let listing = listed(&repo);
    assert_eq!(
        listing.matches("\nworktree ").count() + 1,
        1 + made.len() + scratches.len(),
        "{listing}"
    );
    assert!(!listing.contains("locked"), "{listing}");
    let mut names: Vec<String> = made
        .iter()
        .map(|run| record_name(&worktree_of(run)))
        .collect();
    names.extend(scratches.iter().map(|scratch| record_name(scratch)));
    let unique: BTreeSet<&String> = names.iter().collect();
    assert_eq!(unique.len(), names.len(), "{names:?}");
    for (run, id) in made.iter().zip(&ids) {
        assert_eq!(branch_at(&git, &worktree_of(run)), on(id));
    }
}

/// A run's worktree added before the records had names of their own (`git
/// worktree add` at `<runs>/<id>/worktree`, so a record called `worktree`)
/// is read, repaired and removed as before, and its record's name is not
/// one a new run's takes.
#[test]
fn a_worktree_added_under_the_old_name_is_read_and_removed_as_before() {
    let _test = common::test();
    let dir = tempfile::tempdir().unwrap();
    let (repo, git) = repository(dir.path());
    let runs = dir.path().join("runs");
    let old = run_with_worktree(&git, &runs, RUN_A, "worktree");
    let path = worktree_of(&old);
    let head = git.main_head().unwrap();
    in_repo(
        &repo,
        &[
            "worktree",
            "add",
            "-q",
            "-b",
            &format!("dagq/{RUN_A}"),
            path.to_str().unwrap(),
            head.as_str(),
        ],
    );
    assert_eq!(record_name(&path), "worktree");
    let fresh = worktree_run(&git, &runs, RUN_B);
    git.create_worktree(&fresh).unwrap();
    assert_eq!(
        record_name(&worktree_of(&fresh)),
        format!("worktree-{RUN_B}")
    );
    // Resume and the landing's checks read it where it always was.
    assert_eq!(branch_at(&git, &path), on(RUN_A));
    assert_eq!(git.head(&path).unwrap(), head);
    assert_eq!(git.status(&path).unwrap(), "");
    git.repair_worktree(&path).unwrap();
    // The cleanup removes it and its branch.
    git.remove_worktree_and_branch(&path, &format!("dagq/{RUN_A}"))
        .unwrap();
    assert!(!path.exists());
    assert!(!repo.join(".git/worktrees/worktree").exists());
    // And its gone record's name is not taken over by the next run.
    let next = worktree_run(&git, &runs, RUN_C);
    git.create_worktree(&next).unwrap();
    assert_eq!(
        record_name(&worktree_of(&next)),
        format!("worktree-{RUN_C}")
    );
    assert_eq!(branch_at(&git, &worktree_of(&fresh)), on(RUN_B));
}

/// A scratch leaves no record in `.git/worktrees` once it is removed,
/// wherever its making or its use stopped: made and used, made again over
/// what a stop left (a directory with no `.git`, a record whose directory
/// went), or removed with no worktree there.
#[test]
fn a_removed_scratch_leaves_no_record_wherever_it_stopped() {
    let _test = common::test();
    let dir = tempfile::tempdir().unwrap();
    let (repo, git) = repository(dir.path());
    let head = git.main_head().unwrap();
    // As Git lists it, its links resolved (`/var` is `/private/var`).
    let root = dir.path().canonicalize().unwrap();
    let scratch = root.join("satisfied").join(RUN_A);
    let no_record = |what: &str| {
        let listing = listed(&repo);
        assert!(
            !listing.contains(scratch.to_str().unwrap()),
            "{what}: {listing}"
        );
        assert!(
            !repo.join(".git/worktrees").join(RUN_A).exists(),
            "{what}: the record is left"
        );
    };
    // Made and used, then removed.
    git.fresh_scratch(&scratch, head.as_str()).unwrap();
    assert_eq!(record_name(&scratch), RUN_A);
    git.remove_scratch(&scratch).unwrap();
    assert!(!scratch.exists());
    no_record("made and removed");
    // A stop left a directory with no `.git`: made again over it.
    fs::create_dir_all(scratch.join("left")).unwrap();
    git.fresh_scratch(&scratch, head.as_str()).unwrap();
    git.remove_scratch(&scratch).unwrap();
    no_record("made over a directory left");
    // A stop left its record with its directory gone.
    git.fresh_scratch(&scratch, head.as_str()).unwrap();
    fs::remove_dir_all(&scratch).unwrap();
    git.remove_scratch(&scratch).unwrap();
    no_record("a record whose directory went");
    // Nothing there at all.
    git.remove_scratch(&scratch).unwrap();
    no_record("nothing there");
    // The recheck's scratch, kept between rechecks, goes the same way.
    let recheck = crate::runtime_support::recheck_scratch(&root.join("recheck"));
    git.checkout_scratch(&recheck, head.as_str()).unwrap();
    git.checkout_scratch(&recheck, head.as_str()).unwrap();
    git.remove_scratch(&recheck).unwrap();
    let listing = listed(&repo);
    assert!(!listing.contains(recheck.to_str().unwrap()), "{listing}");
    assert_eq!(listing.matches("worktree ").count(), 1, "{listing}");
}

/// The recheck scratches of two queues of one repository whose
/// directories have the same name (`a/queue` and `b/queue`) have names of
/// their own, so one's record going leaves no name the other takes.
#[test]
fn the_recheck_scratches_of_queues_named_alike_have_records_of_their_own() {
    let _test = common::test();
    let dir = tempfile::tempdir().unwrap();
    let (repo, git) = repository(dir.path());
    let head = git.main_head().unwrap();
    let scratches: Vec<PathBuf> = ["a", "b"]
        .iter()
        .map(|queue| {
            let queue = dir.path().join(queue).join("queue");
            fs::create_dir_all(&queue).unwrap();
            crate::runtime_support::recheck_scratch(&queue.join("recheck"))
        })
        .collect();
    for scratch in &scratches {
        git.checkout_scratch(scratch, head.as_str()).unwrap();
        assert_eq!(
            record_name(scratch),
            scratch.file_name().unwrap().to_str().unwrap()
        );
    }
    assert_ne!(record_name(&scratches[0]), record_name(&scratches[1]));
    // A's record goes while its directory stays; B's add again takes no
    // name of A's.
    let lost = repo.join(".git/worktrees").join(record_name(&scratches[0]));
    fs::remove_dir_all(&lost).unwrap();
    git.fresh_scratch(&scratches[1], head.as_str()).unwrap();
    assert!(!lost.exists(), "B took the name A's .git names");
    assert_eq!(git.head(&scratches[1]).unwrap(), head);
}

/// What `git worktree add` leaves when a prune took its record before it
/// read it back (`could not find created worktree`): its directory with
/// the `.git` it wrote, and a record naming that `.git`. The clearing
/// before the retry takes both away, so the run's add makes its worktree
/// under its own name; it keeps a directory with more than the `.git`, a
/// record naming another worktree, and a record an add is still making;
/// and nothing there, as a prune that came first leaves it, is no error.
#[test]
fn what_an_add_a_prune_raced_left_is_cleared_for_the_retry() {
    let _test = common::test();
    let dir = tempfile::tempdir().unwrap();
    let (repo, git) = repository(dir.path());
    let runs = dir.path().join("runs");
    let run = worktree_run(&git, &runs, RUN_A);
    let path = worktree_of(&run);
    let name = format!("worktree-{RUN_A}");
    let record = repo.join(".git/worktrees").join(&name);
    // As Git writes it, its links resolved (`/var` is `/private/var`).
    let dotgit = runs
        .canonicalize()
        .unwrap()
        .join(RUN_A)
        .join(&name)
        .join(".git");
    let leave = |gitdir: &str| {
        fs::create_dir_all(&path).unwrap();
        fs::write(path.join(".git"), format!("gitdir: {}\n", record.display())).unwrap();
        fs::create_dir_all(&record).unwrap();
        fs::write(record.join("gitdir"), gitdir).unwrap();
    };
    // Another worktree's record, and a directory with more than its `.git`,
    // stay.
    leave("/elsewhere/worktree/.git\n");
    fs::write(path.join("work"), "kept\n").unwrap();
    git.clear_failed_add(&path).unwrap();
    assert!(path.join("work").exists());
    assert!(record.join("gitdir").exists());
    fs::remove_file(path.join("work")).unwrap();
    // A record with no `gitdir` that an add is still making stays.
    fs::remove_file(record.join("gitdir")).unwrap();
    fs::write(record.join("locked"), "initializing\n").unwrap();
    git.clear_failed_add(&path).unwrap();
    assert!(record.exists());
    fs::remove_dir_all(&record).unwrap();
    // What the failed add left goes, and the run's add then makes its own.
    leave(&format!("{}\n", dotgit.display()));
    git.clear_failed_add(&path).unwrap();
    assert!(!path.exists());
    assert!(!record.exists());
    git.create_worktree(&run).unwrap();
    assert_eq!(record_name(&path), name);
    assert_eq!(branch_at(&git, &path), on(RUN_A));
    // Nothing there is no error.
    git.clear_failed_add(&runs.join(RUN_B).join("worktree-b"))
        .unwrap();
}
