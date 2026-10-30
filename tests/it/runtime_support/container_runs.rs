//! The runs of a worker set to the container as the runtime tests of its
//! clone make them (`runtime_clone`, `runtime_clone_facts`): claimed and
//! planned as the supervisor does, the worker's commits made in the clone
//! and the turn's end written as the container's executor writes it, with
//! real git.
use super::*;
use dagq::domain::{
    ClaimOutcome, LeaseToken, RunPlan, provider_switch::WorkerRoute, run_clone, worker::Worker,
};
use dagq::infrastructure::location::runs_dir;

/// A run claimed as the supervisor claims one for a worker set to the
/// container, its clone made by the control side.
pub struct ContainerRun {
    pub task: TaskId,
    pub run: TaskRun,
    pub token: LeaseToken,
    pub clone: PathBuf,
    pub run_dir: PathBuf,
    pub base: String,
}

impl ContainerRun {
    pub fn branch(&self) -> String {
        format!("dagq/{}", self.run.id())
    }

    pub fn reload(&mut self, db: &Path) {
        self.run = SqliteQueue::open(db).unwrap().run(self.run.id()).unwrap();
    }
}

/// Claim `task` at main's head in `repo` and make its clone, as the
/// supervisor provisions a run whose worker runs in the container.
pub fn claim_in_container(db: &Path, repo: &Path, task: TaskId) -> ContainerRun {
    let mut run = plan_in_container(db, repo, task);
    let mut queue = SqliteQueue::open(db).unwrap();
    queue
        .workspace_created(run.run.id(), &run.token, "background:1:1")
        .unwrap();
    run.run = queue.run(run.run.id()).unwrap();
    GitRepository::inspect(repo)
        .unwrap()
        .create_clone(&run.run, None)
        .unwrap();
    run
}

/// Claim `task` and plan its paths, as the supervisor does before it makes
/// the clone: no clone yet, and no worker started.
pub fn plan_in_container(db: &Path, repo: &Path, task: TaskId) -> ContainerRun {
    let mut queue = SqliteQueue::open(db).unwrap();
    let common_dir = GitRepository::inspect(repo).unwrap().common_dir;
    queue.bind_repository(common_dir.to_str().unwrap()).unwrap();
    let base = git_out(repo, &["rev-parse", "main"]);
    let token = LeaseToken::new(format!("container-{task}"));
    let ClaimOutcome::Claimed { run } = queue
        .claim_for_supervisor_in_order(
            &sha(&base),
            &token,
            &[task],
            None,
            &Default::default(),
            &WorkerRoute::direct(&[Worker::CLAUDE_HEADLESS]),
        )
        .unwrap()
    else {
        panic!("task {task} was not claimed")
    };
    let runs = runs_dir(db);
    let run_dir = runs.join(run.id().as_str());
    let clone = run_clone::clone_path(&runs, run.id());
    fs::create_dir_all(&run_dir).unwrap();
    let text = |path: &Path| path.to_str().unwrap().to_owned();
    queue
        .plan_run(
            run.id(),
            &token,
            &RunPlan {
                repo_path: text(repo),
                run_dir: text(&run_dir),
                branch: format!("dagq/{}", run.id()),
                worktree_path: text(&clone),
                receipt_path: text(&run_dir.join("receipt.json")),
                log_path: text(&run_dir.join("log")),
            },
        )
        .unwrap();
    let run = queue.run(run.id()).unwrap();
    assert!(run.in_container());
    ContainerRun {
        task,
        run,
        token,
        clone,
        run_dir,
        base,
    }
}

/// Git in the clone, as the worker and the executor run it in the
/// container: nothing the worker planted in its configuration is read.
pub fn in_clone(clone: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .args([
            "-c",
            "core.hooksPath=/dev/null",
            "-c",
            "core.fsmonitor=false",
        ])
        .arg("-C")
        .arg(clone)
        .args(args)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_AUTHOR_NAME", "worker")
        .env("GIT_AUTHOR_EMAIL", "worker@example.invalid")
        .env("GIT_COMMITTER_NAME", "worker")
        .env("GIT_COMMITTER_EMAIL", "worker@example.invalid")
        .bounded_output()
        .unwrap();
    assert!(
        output.status.success(),
        "git {args:?} in the clone: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap().trim().to_owned()
}

/// The worker writes `files` and commits them on the run branch; the head.
pub fn commit(run: &ContainerRun, files: &[(&str, &str)], message: &str) -> String {
    for (path, text) in files {
        let path = run.clone.join(path);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, text).unwrap();
    }
    in_clone(&run.clone, &["add", "--all"]);
    in_clone(&run.clone, &["commit", "-q", "-m", message]);
    in_clone(&run.clone, &["rev-parse", "HEAD"])
}

/// The end of a turn as the container's executor ends it: the run branch's
/// bundle on top of the base (none without a new commit) and the run's
/// state, in the run directory.
pub fn turn_end(run: &ContainerRun) {
    let head = in_clone(&run.clone, &["rev-parse", "HEAD"]);
    let bundle = run.run_dir.join(run_clone::RUN_BUNDLE_FILE);
    let _ = fs::remove_file(&bundle);
    if head != run.base {
        in_clone(
            &run.clone,
            &[
                "bundle",
                "create",
                "-q",
                bundle.to_str().unwrap(),
                &format!("refs/heads/{}", run.branch()),
                &format!("^{}", run.base),
            ],
        );
    }
    write_state(
        run,
        Some(&format!("refs/heads/{}", run.branch())),
        &head,
        "",
    );
}

pub fn write_state(run: &ContainerRun, branch: Option<&str>, head: &str, status: &str) {
    fs::write(
        run.run_dir.join(run_clone::RUN_STATE_FILE),
        json!({"branch": branch, "head": head, "status": status, "rebase_in_progress": false,
               "turn_ended": true})
        .to_string(),
    )
    .unwrap();
}

/// The run's state as its executor last wrote it.
pub fn run_state(run: &ContainerRun) -> run_clone::RunState {
    run_clone::RunState::parse(
        &fs::read_to_string(run.run_dir.join(run_clone::RUN_STATE_FILE)).unwrap(),
    )
    .unwrap()
}
