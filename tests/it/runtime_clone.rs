//! Runtime tests: a run in a container works in a clone of its own, which
//! the control side runs no git and no program in (ADR-t2113-2,
//! ADR-t2179-1). The container is faked: the test makes the worker's
//! commits in the clone and writes the run branch's bundle and the run's
//! state file as the container's executor would, with real git. What the
//! supervisor does around them (the claim, the validation, the review, the
//! resume) goes through the queue and the same application functions.
use crate::runtime_support;

use crate::common::service::OwnedByTest;
use dagq::application::{CloneFacts, integrate::check_receipt, run_git};
use dagq::domain::{
    ClaimOutcome, LeaseToken, RunPlan, podman_machine::MachineMounts, provider_switch::WorkerRoute,
    run_clone, validation::Validation, worker::Worker,
};
use dagq::infrastructure::git_binary::git_executable;
use dagq::infrastructure::location::runs_dir;
use runtime_support::*;
use std::os::unix::fs::{MetadataExt, PermissionsExt, symlink};

/// A run claimed as the supervisor claims one for a worker set to the
/// container, its clone made by the control side.
struct ContainerRun {
    task: TaskId,
    run: TaskRun,
    token: LeaseToken,
    clone: PathBuf,
    run_dir: PathBuf,
    base: String,
}

impl ContainerRun {
    fn branch(&self) -> String {
        format!("dagq/{}", self.run.id())
    }

    fn reload(&mut self, db: &Path) {
        self.run = SqliteQueue::open(db).unwrap().run(self.run.id()).unwrap();
    }
}

/// Claim `task` at main's head in `repo` and make its clone, as the
/// supervisor provisions a run whose worker runs in the container.
fn claim_in_container(db: &Path, repo: &Path, task: TaskId) -> ContainerRun {
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
fn plan_in_container(db: &Path, repo: &Path, task: TaskId) -> ContainerRun {
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
fn in_clone(clone: &Path, args: &[&str]) -> String {
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
fn commit(run: &ContainerRun, files: &[(&str, &str)], message: &str) -> String {
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
fn turn_end(run: &ContainerRun) {
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

fn write_state(run: &ContainerRun, branch: Option<&str>, head: &str, status: &str) {
    fs::write(
        run.run_dir.join(run_clone::RUN_STATE_FILE),
        json!({"branch": branch, "head": head, "status": status, "rebase_in_progress": false,
               "turn_ended": true})
        .to_string(),
    )
    .unwrap();
}

fn write_receipt(run: &ContainerRun, commit: &str) {
    fs::write(
        run.run_dir.join("receipt.json"),
        json!({
            "run_id": run.run.id(),
            "result": "succeeded",
            "commit": commit,
            "tests": {"status": "passed", "evidence_or_reason": "unit"},
            "e2e": {"status": "not_applicable", "evidence_or_reason": "the runtime runs it"},
            "subagent_review": {"status": "not_applicable", "evidence_or_reason": "small"},
            "summary": "done",
        })
        .to_string(),
    )
    .unwrap();
}

/// The receipt's validation as the supervisor's, through the same check.
fn check(db: &Path, repo: &Path, run: &ContainerRun) -> Result<CommitSha, (ReasonCode, String)> {
    let task = SqliteQueue::open(db).unwrap().show(run.task).unwrap().task;
    let run = SqliteQueue::open(db).unwrap().run(run.run.id()).unwrap();
    match check_receipt(
        &GitRepository::inspect(repo).unwrap(),
        &LocalRunFiles,
        &task,
        &run,
        &[],
    )
    .unwrap()
    {
        Ok(accepted) => Ok(accepted.commit),
        Err(rejection) => Err((rejection.code, rejection.reason)),
    }
}

/// The session went idle after its receipt: validate it and, accepted,
/// leave the run awaiting integration with no lease, for a person's
/// integrate.
fn validate(db: &Path, repo: &Path, run: &mut ContainerRun) -> CommitSha {
    let mut queue = SqliteQueue::open(db).unwrap();
    if queue.run(run.run.id()).unwrap().status() != RunStatus::Validating {
        queue
            .finish_supervision_live(run.run.id(), &run.token)
            .unwrap();
    }
    let commit = check(db, repo, run).unwrap_or_else(|why| panic!("rejected: {why:?}"));
    queue
        .finish_validation(
            run.run.id(),
            &run.token,
            &Validation {
                accepted: true,
                result_commit: Some(commit.clone()),
                reason: None,
                code: None,
                receipt: serde_json::from_str(
                    &fs::read_to_string(run.run_dir.join("receipt.json")).unwrap(),
                )
                .unwrap(),
                evidence_missing: Vec::new(),
                spike_result_missing: Vec::new(),
                scope_violation: Vec::new(),
                allowed_paths: Vec::new(),
                e2e_requirement: None,
                already_satisfied: false,
                load: Default::default(),
            },
        )
        .unwrap();
    queue.release_lease(run.run.id(), &run.token).unwrap();
    run.reload(db);
    assert_eq!(run.run.status(), RunStatus::AwaitingIntegration);
    commit
}

/// A process the control side starts in the run's landing worktree (the
/// review's job or program, the e2e) with the environment it gets there;
/// like an agent's CLI, it runs git in its working directory. With a
/// global git configuration `global`, the control side that starts it
/// reads that configuration too ([`guard_env_in_child`]).
fn start_in_tree(repo: &Path, run: &ContainerRun, global: Option<&Path>) -> PathBuf {
    let repository = GitRepository::inspect(repo).unwrap();
    let tree = run_git::review_tree(&repository, &run.run).unwrap();
    let env = match global {
        None => run_git::tree_env(&repository, &run.run, &tree).unwrap(),
        Some(global) => guard_env_in_child(repo, &tree, global),
    };
    let extra: Vec<(&str, &Path)> = global
        .map(|g| ("GIT_CONFIG_GLOBAL", g))
        .into_iter()
        .collect();
    let output = Command::new("/bin/sh")
        .arg("-c")
        .arg(
            "git status --porcelain >/dev/null; git diff HEAD~1 >/dev/null; \
             git log -p -1 >/dev/null; git diff --stat HEAD~1 >/dev/null; exit 0",
        )
        .current_dir(&tree)
        .envs(env)
        .envs(extra)
        .bounded_output()
        .unwrap();
    assert!(output.status.success());
    tree
}

/// Where [`guard_env_child_process`] reads what to do.
const GUARD_ENV_FOR: &str = "DAGQ_TEST_GUARD_ENV_FOR";

/// The guard's environment for a process in `tree`, as a control side
/// whose global git configuration is `global` makes it: read in another
/// process of this test binary that has that configuration
/// ([`guard_env_child_process`]), since this one cannot change its own.
fn guard_env_in_child(repo: &Path, tree: &Path, global: &Path) -> Vec<(String, String)> {
    let output = Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "runtime_clone::guard_env_child_process",
            "--nocapture",
            "--test-threads=1",
        ])
        .env(
            GUARD_ENV_FOR,
            format!("{}\n{}", repo.display(), tree.display()),
        )
        .env("GIT_CONFIG_GLOBAL", global)
        .bounded_output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    let line = stdout
        .lines()
        .find_map(|line| line.split_once("guard-env: ").map(|(_, env)| env))
        .unwrap_or_else(|| panic!("{stdout}{}", String::from_utf8_lossy(&output.stderr)));
    serde_json::from_str(line).unwrap()
}

/// Not a test of its own: the other process of [`guard_env_in_child`],
/// which prints the guard's environment when it is asked to and does
/// nothing otherwise.
#[test]
fn guard_env_child_process() {
    let Ok(asked) = std::env::var(GUARD_ENV_FOR) else {
        return;
    };
    let (repo, tree) = asked.split_once('\n').unwrap();
    let env = GitRepository::inspect(Path::new(repo))
        .unwrap()
        .guard_env_in(Path::new(tree))
        .unwrap();
    println!("guard-env: {}", serde_json::to_string(&env).unwrap());
}

/// A script that leaves a mark named by its first argument in `marks`,
/// and otherwise does what git asks of the program it stands for (a clean
/// or smudge filter passes its input through, a textconv prints its file)
/// so a git that runs it goes on.
fn marker(dir: &Path, marks: &Path) -> PathBuf {
    let script = dir.join("mark");
    common::template::script(
        &script,
        format!(
            "#!/bin/sh\nmkdir -p '{marks}'\n: > '{marks}'/\"${{1:-unnamed}}\"\n\
             case \"$1\" in\n*-clean|*-smudge|clean|smudge) cat ;;\n\
             *-textconv|textconv) cat \"$2\" ;;\nesac\nexit 0\n",
            marks = marks.display(),
        ),
    );
    script
}

fn marks(dir: &Path) -> Vec<String> {
    let mut found: Vec<String> = fs::read_dir(dir)
        .map(|entries| {
            entries
                .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
                .collect()
        })
        .unwrap_or_default();
    found.sort();
    found
}

use crate::common;

/// AC 1: the clone shares no object file with the repository (no hard
/// link, no alternates), names no remote and holds no credential helper.
#[test]
fn a_container_runs_clone_shares_no_object_and_reaches_no_upstream() {
    let (_fixture, repo, db) = fixture();
    git(
        &repo,
        &[
            "remote",
            "add",
            "origin",
            "https://example.invalid/upstream.git",
        ],
    );
    git(&repo, &["config", "credential.helper", "store"]);
    let run = claim_in_container(&db, &repo, TaskId::new(1));
    let git_dir = run.clone.join(".git");
    assert!(git_dir.is_dir(), "the clone has its own git directory");
    assert!(!git_dir.join("objects/info/alternates").exists());
    let inodes = |dir: &Path| -> HashMap<(u64, u64), PathBuf> {
        let mut found = HashMap::new();
        let mut pending = vec![dir.to_path_buf()];
        while let Some(dir) = pending.pop() {
            for entry in fs::read_dir(&dir).unwrap() {
                let path = entry.unwrap().path();
                let meta = fs::symlink_metadata(&path).unwrap();
                if meta.is_dir() {
                    pending.push(path);
                } else {
                    found.insert((meta.dev(), meta.ino()), path);
                }
            }
        }
        found
    };
    let control = inodes(&repo.join(".git/objects"));
    let cloned = inodes(&git_dir.join("objects"));
    assert!(!cloned.is_empty());
    for (inode, path) in &cloned {
        assert!(
            !control.contains_key(inode),
            "{} shares its file with {}",
            path.display(),
            control[inode].display()
        );
        assert_eq!(fs::metadata(path).unwrap().nlink(), 1, "{}", path.display());
    }
    let config = fs::read_to_string(git_dir.join("config")).unwrap();
    assert!(!config.contains("[remote"), "{config}");
    assert!(!config.contains("credential"), "{config}");
    assert!(!config.contains("example.invalid/upstream"), "{config}");
    assert_eq!(in_clone(&run.clone, &["remote"]), "");
    // The run branch at the base, main's ref beside it, the control side's
    // run branch at the base too.
    assert_eq!(
        in_clone(&run.clone, &["symbolic-ref", "HEAD"]),
        format!("refs/heads/{}", run.branch())
    );
    assert_eq!(in_clone(&run.clone, &["rev-parse", "HEAD"]), run.base);
    assert_eq!(
        in_clone(&run.clone, &["rev-parse", run_clone::CLONE_MAIN_REF]),
        run.base
    );
    assert_eq!(in_clone(&run.clone, &["status", "--porcelain"]), "");
    assert_eq!(git_out(&repo, &["rev-parse", &run.branch()]), run.base);
}

/// AC 3 and 7: a worker's commit goes from the bundle through the
/// receipt's validation, the review in the landing worktree and the
/// landing's checks to main, and everything the run left goes.
#[test]
fn a_container_run_lands_from_its_bundle_through_its_landing_worktree() {
    let (_fixture, repo, db) = fixture();
    let mut run = claim_in_container(&db, &repo, TaskId::new(1));
    let head = commit(&run, &[("feature.txt", "feature\n")], "feature");
    turn_end(&run);
    write_receipt(&run, &head);
    // Before validation nothing of the run's commit is in the repository.
    assert_eq!(git_out(&repo, &["rev-parse", &run.branch()]), run.base);
    assert_eq!(validate(&db, &repo, &mut run).as_str(), head);
    assert_eq!(git_out(&repo, &["rev-parse", &run.branch()]), head);
    // The run's facts as the supervisor reads them between turns.
    let repository = GitRepository::inspect(&repo).unwrap();
    assert_eq!(run_git::head(&repository, &run.run).unwrap().as_str(), head);
    assert_eq!(
        run_git::session_head(&repository, &run.run).unwrap(),
        run_git::SessionHead {
            head: sha(&head),
            clean: true,
            last_turn: true,
        }
    );
    // The review's processes work in the landing worktree.
    let tree = start_in_tree(&repo, &run, None);
    assert_eq!(Some(tree.clone()), run.run.landing_path());
    assert_eq!(git_out(&tree, &["rev-parse", "HEAD"]), head);
    let outcome = integrate(&db, run.task.as_i64(), &repo).unwrap();
    assert_eq!(outcome["outcome"], "integrated", "{outcome}");
    assert_eq!(
        git_out(&repo, &["show", "main:feature.txt"]),
        "feature",
        "the worker's change is on main"
    );
    assert!(repo.join("feature.txt").exists());
    // Landed, the run leaves nothing.
    assert!(!run.clone.exists());
    assert!(!tree.exists());
    assert!(git_out(&repo, &["branch", "--list", &run.branch()]).is_empty());
    for name in [
        run_clone::RUN_BUNDLE_FILE,
        run_clone::RUN_STATE_FILE,
        run_clone::MAIN_BUNDLE_FILE,
    ] {
        assert!(!run.run_dir.join(name).exists(), "{name}");
    }
    assert!(!git_out(&repo, &["worktree", "list"]).contains("landing"));
}

/// AC 3: the receipt's checks hold for a container run as for a host one,
/// and a bundle or state file that cannot be used sends the run back to
/// its session, never read from the clone instead.
#[test]
fn a_container_runs_receipt_is_checked_from_its_bundle_and_state() {
    let (_fixture, repo, db) = fixture();
    let mut run = claim_in_container(&db, &repo, TaskId::new(1));
    SqliteQueue::open(&db)
        .unwrap()
        .finish_supervision_live(run.run.id(), &run.token)
        .unwrap();
    run.reload(&db);
    #[track_caller]
    fn rejected_in(db: &Path, repo: &Path, run: &ContainerRun) -> (ReasonCode, String) {
        match check(db, repo, run) {
            Ok(commit) => panic!("accepted {commit}"),
            Err(rejection) => rejection,
        }
    }
    macro_rules! rejected {
        ($run:expr) => {
            rejected_in(&db, &repo, $run)
        };
    }
    // No receipt, no commit, no bundle: as on the host.
    assert_eq!(rejected!(&run).0, ReasonCode::ReceiptMissing);
    write_receipt(&run, &run.base);
    let (code, reason) = rejected!(&run);
    assert_eq!(code, ReasonCode::CommitMismatch, "{reason}");
    assert!(reason.contains("state file"), "{reason}");
    assert_eq!(run.run.base_commit().as_str(), run.base);
    turn_end(&run);
    let (code, reason) = rejected!(&run);
    assert_eq!(code, ReasonCode::CommitMismatch);
    assert!(reason.contains("no commit was made"), "{reason}");

    let head = commit(&run, &[("feature.txt", "feature\n")], "feature");
    write_receipt(&run, &head);
    // A commit the bundle does not carry yet: the state still names the
    // last turn's head.
    let (code, reason) = rejected!(&run);
    assert_eq!(code, ReasonCode::CommitMismatch);
    assert!(reason.contains("receipt commit"), "{reason}");
    // The state names a head the bundle does not hold.
    turn_end(&run);
    write_state(
        &run,
        Some(&format!("refs/heads/{}", run.branch())),
        &run.base,
        "",
    );
    let (code, reason) = rejected!(&run);
    assert_eq!(code, ReasonCode::CommitMismatch);
    assert!(reason.contains("names"), "{reason}");
    // Uncommitted work and another branch, as the state says.
    turn_end(&run);
    write_state(
        &run,
        Some(&format!("refs/heads/{}", run.branch())),
        &head,
        "?? notes.txt\n",
    );
    assert_eq!(rejected!(&run).0, ReasonCode::WorktreeDirty);
    write_state(&run, Some("refs/heads/other"), &head, "");
    let (code, reason) = rejected!(&run);
    assert_eq!(code, ReasonCode::CommitMismatch);
    assert!(reason.contains("refs/heads/other"), "{reason}");
    // A bundle with another ref beside the run branch.
    turn_end(&run);
    let bundle = run.run_dir.join(run_clone::RUN_BUNDLE_FILE);
    in_clone(&run.clone, &["branch", "extra"]);
    in_clone(
        &run.clone,
        &[
            "bundle",
            "create",
            "-q",
            bundle.to_str().unwrap(),
            &format!("refs/heads/{}", run.branch()),
            "refs/heads/extra",
            &format!("^{}", run.base),
        ],
    );
    let (code, reason) = rejected!(&run);
    assert_eq!(code, ReasonCode::CommitMismatch);
    assert!(reason.contains("2 refs"), "{reason}");
    assert!(git_out(&repo, &["branch", "--list", "extra"]).is_empty());
    // A bundle that is gone, a symlink, or not a bundle.
    fs::remove_file(&bundle).unwrap();
    let (code, reason) = rejected!(&run);
    assert_eq!(code, ReasonCode::CommitMismatch);
    assert!(reason.contains("missing"), "{reason}");
    let elsewhere = run.run_dir.join("elsewhere.bundle");
    in_clone(
        &run.clone,
        &[
            "bundle",
            "create",
            "-q",
            elsewhere.to_str().unwrap(),
            &format!("refs/heads/{}", run.branch()),
            &format!("^{}", run.base),
        ],
    );
    symlink(&elsewhere, &bundle).unwrap();
    let (code, reason) = rejected!(&run);
    assert_eq!(code, ReasonCode::CommitMismatch);
    assert!(reason.contains("symlink"), "{reason}");
    fs::remove_file(&bundle).unwrap();
    fs::write(&bundle, "not a bundle").unwrap();
    let (code, reason) = rejected!(&run);
    assert_eq!(code, ReasonCode::CommitMismatch);
    assert!(reason.contains("bundle"), "{reason}");
    // A bundle whose base the repository does not have: history of its own.
    in_clone(&run.clone, &["checkout", "-q", "--orphan", "alone"]);
    in_clone(&run.clone, &["commit", "-q", "-m", "alone"]);
    let alone = in_clone(&run.clone, &["rev-parse", "HEAD"]);
    in_clone(
        &run.clone,
        &["commit", "-q", "--allow-empty", "-m", "on top"],
    );
    in_clone(&run.clone, &["branch", "-f", &run.branch(), "HEAD"]);
    in_clone(
        &run.clone,
        &[
            "bundle",
            "create",
            "-q",
            bundle.to_str().unwrap(),
            &format!("refs/heads/{}", run.branch()),
            &format!("^{alone}"),
        ],
    );
    let (code, reason) = rejected!(&run);
    assert_eq!(code, ReasonCode::CommitMismatch);
    assert!(reason.contains("imported"), "{reason}");
    in_clone(&run.clone, &["branch", "-f", &run.branch(), &head]);
    in_clone(&run.clone, &["checkout", "-q", &run.branch()]);
    // A state file that is a symlink is not read either.
    turn_end(&run);
    let state = run.run_dir.join(run_clone::RUN_STATE_FILE);
    let real = run.run_dir.join("state-elsewhere.json");
    fs::rename(&state, &real).unwrap();
    symlink(&real, &state).unwrap();
    let (code, reason) = rejected!(&run);
    assert_eq!(code, ReasonCode::CommitMismatch);
    assert!(reason.contains("symlink"), "{reason}");
    fs::remove_file(&state).unwrap();
    // Sound again, it is accepted with the imported commit.
    turn_end(&run);
    assert_eq!(check(&db, &repo, &run).unwrap().as_str(), head);
}

/// AC 3: the paths, the evidence, a Spike's result and the e2e decide the
/// container run's receipt as the host's.
#[test]
fn a_container_runs_paths_evidence_and_e2e_are_judged_as_on_the_host() {
    let (_fixture, repo, db) = fixture();
    let mut queue = SqliteQueue::open(&db).unwrap();
    let task = queue
        .add(NewTask {
            paths: vec!["src/**".into()],
            required_evidence: vec![EvidenceCheck::SubagentReview],
            worker_mode: Some(dagq::domain::worker::WorkerMode::Headless),
            ..common::queue::new_task("scoped")
        })
        .unwrap()
        .id();
    queue.transition(task, TaskAction::BypassReview).unwrap();
    let mut run = claim_in_container(&db, &repo, task);
    SqliteQueue::open(&db)
        .unwrap()
        .finish_supervision_live(run.run.id(), &run.token)
        .unwrap();
    run.reload(&db);
    let head = commit(&run, &[("outside.txt", "x\n")], "outside");
    turn_end(&run);
    write_receipt(&run, &head);
    // The receipt's subagent review is not_applicable: required, it is
    // missing; the path outside the task's comes first.
    let (code, _) = check(&db, &repo, &run).unwrap_err();
    assert_eq!(code, ReasonCode::ScopeViolation);
    in_clone(&run.clone, &["rm", "-q", "outside.txt"]);
    let head = commit(&run, &[("src/inside.rs", "x\n")], "inside");
    turn_end(&run);
    write_receipt(&run, &head);
    let (code, _) = check(&db, &repo, &run).unwrap_err();
    assert_eq!(code, ReasonCode::EvidenceMissing);
    let mut receipt: Value =
        serde_json::from_str(&fs::read_to_string(run.run_dir.join("receipt.json")).unwrap())
            .unwrap();
    receipt["subagent_review"] = json!({"status": "passed", "evidence_or_reason": "reviewed"});
    fs::write(run.run_dir.join("receipt.json"), receipt.to_string()).unwrap();
    assert_eq!(check(&db, &repo, &run).unwrap().as_str(), head);
    // Whether the e2e is required is read from the imported diff.
    let task = SqliteQueue::open(&db).unwrap().show(task).unwrap().task;
    let current = SqliteQueue::open(&db).unwrap().run(run.run.id()).unwrap();
    let accepted = check_receipt(
        &GitRepository::inspect(&repo).unwrap(),
        &LocalRunFiles,
        &task,
        &current,
        &["src/**".to_owned()],
    )
    .unwrap()
    .unwrap_or_else(|rejection| panic!("{}", rejection.reason));
    assert!(
        accepted.e2e_requirement.required,
        "{:?}",
        accepted.e2e_requirement
    );
}

/// The landing directory, the queue's directory and the landing worktree's
/// path of `run`.
fn landing_paths(db: &Path, run: &ContainerRun) -> (PathBuf, PathBuf, PathBuf) {
    let tree = run.run.landing_path().unwrap();
    let landing = tree.parent().unwrap().to_path_buf();
    (runs_dir(db).parent().unwrap().to_path_buf(), landing, tree)
}

/// An approved container run whose resume resolved it at a new head after
/// its landing worktree was made at the reviewed commit: the resume's end
/// as the supervisor records it ([`SqliteQueue::finish_approved_resume`])
/// makes that head the run's `result_commit`, the commit the e2e before
/// its landing records, and the landing worktree the e2e and the landing
/// run in is made again at it rather than reused at the reviewed commit.
/// The run lands that head.
#[test]
fn an_approved_container_runs_resolved_head_lands_from_a_new_landing_worktree() {
    let (_fixture, repo, db) = fixture();
    let mut run = claim_in_container(&db, &repo, TaskId::new(1));
    let reviewed = commit(&run, &[("shared.txt", "the run's\n")], "run");
    turn_end(&run);
    write_receipt(&run, &reviewed);
    validate(&db, &repo, &mut run);
    let tree = start_in_tree(&repo, &run, None);
    assert_eq!(git_out(&tree, &["rev-parse", "HEAD"]), reviewed);
    fs::write(repo.join("shared.txt"), "main's\n").unwrap();
    git(&repo, &["add", "shared.txt"]);
    git(&repo, &["commit", "-q", "-m", "main's"]);
    let main = git_out(&repo, &["rev-parse", "main"]);
    let outcome = integrate(&db, run.task.as_i64(), &repo).unwrap();
    assert_eq!(outcome["outcome"], "needs_session", "{outcome}");
    // The landing worktree is still there, at the reviewed commit.
    assert_eq!(git_out(&tree, &["rev-parse", "HEAD"]), reviewed);
    let repository = GitRepository::inspect(&repo).unwrap();
    run.reload(&db);
    run_git::hand_main(&repository, &run.run, &sha(&main)).unwrap();
    let bundle = run.run_dir.join(run_clone::MAIN_BUNDLE_FILE);
    in_clone(
        &run.clone,
        &[
            "fetch",
            "-q",
            bundle.to_str().unwrap(),
            &format!("+refs/heads/main:{}", run_clone::CLONE_MAIN_REF),
        ],
    );
    // The resumed session rebases onto main in the clone, resolving the
    // conflict, and the turn's end hands the new head over.
    in_clone(
        &run.clone,
        &["reset", "-q", "--hard", run_clone::CLONE_MAIN_REF],
    );
    let resolved = commit(&run, &[("shared.txt", "both\n")], "run, on main");
    turn_end(&run);
    write_receipt(&run, &resolved);
    let mut queue = SqliteQueue::open(&db).unwrap();
    let (_, attempt) = queue
        .begin_resume(
            run.run.id(),
            &run.token,
            &sha(&main),
            None,
            Default::default(),
        )
        .unwrap()
        .unwrap();
    assert_eq!(
        run_git::head(&repository, &run.run).unwrap().as_str(),
        resolved
    );
    let resumed = queue
        .finish_approved_resume(
            run.run.id(),
            &run.token,
            &sha(&resolved),
            json!({"attempt": attempt, "outcome": "resolved", "head": resolved, "approved": true}),
        )
        .unwrap();
    assert_eq!(resumed.result_commit().unwrap().as_str(), resolved);
    run.reload(&db);
    // The e2e's tree (`review_tree`, as e2e.rs takes it for the commit it
    // records, the run's `result_commit`) is at the resolved head.
    let anew = start_in_tree(&repo, &run, None);
    assert_eq!(Some(anew.clone()), run.run.landing_path());
    assert_eq!(git_out(&anew, &["rev-parse", "HEAD"]), resolved);
    assert_eq!(git_out(&anew, &["status", "--porcelain"]), "");
    // Asked again at that commit, it is reused as it is.
    let marker = anew.join("untracked-marker");
    fs::write(&marker, "").unwrap();
    let again = run_git::review_tree(&repository, &run.run).unwrap();
    assert_eq!(again, anew);
    assert!(marker.exists());
    fs::remove_file(&marker).unwrap();
    queue.release_lease(run.run.id(), &run.token).unwrap();
    let outcome = integrate(&db, run.task.as_i64(), &repo).unwrap();
    assert_eq!(outcome["outcome"], "integrated", "{outcome}");
    assert_eq!(git_out(&repo, &["show", "main:shared.txt"]), "both");
}

/// AC 5: the landing worktree is the control side's alone, outside every
/// directory the machine mounts; something at its path, a symlink or a file
/// on the way to it or a landing directory others may write keeps the run
/// from landing and waits for a person; a sound landing directory is
/// shared by the runs' landing worktrees.
#[test]
fn a_landing_worktree_is_made_only_in_the_control_sides_own_directory() {
    let (_fixture, repo, db) = fixture();
    let mut queue = SqliteQueue::open(&db).unwrap();
    let second = add_ready_task(&mut queue, "second", &[]);
    let mut run = claim_in_container(&db, &repo, TaskId::new(1));
    let (queue_dir, landing, tree) = landing_paths(&db, &run);
    let mounts = MachineMounts::of_queue(&queue_dir);
    for dir in mounts.dirs() {
        assert!(
            !tree.starts_with(dir) && !dir.starts_with(&tree),
            "{}",
            dir.display()
        );
    }
    let head = commit(&run, &[("feature.txt", "feature\n")], "feature");
    turn_end(&run);
    write_receipt(&run, &head);
    validate(&db, &repo, &mut run);
    let held = |why: &str| {
        let outcome = integrate(&db, run.task.as_i64(), &repo).unwrap();
        assert_eq!(outcome["outcome"], "held", "{why}: {outcome}");
        let current = SqliteQueue::open(&db).unwrap().run(run.run.id()).unwrap();
        assert_eq!(current.status(), RunStatus::AwaitingIntegration, "{why}");
        assert!(
            current
                .last_error()
                .unwrap_or_default()
                .contains("not landed"),
            "{why}: {:?}",
            current.last_error()
        );
        assert_eq!(git_out(&repo, &["rev-parse", "main"]), run.base, "{why}");
    };
    // Something at the landing worktree's path.
    fs::create_dir_all(&landing).unwrap();
    fs::set_permissions(&landing, fs::Permissions::from_mode(0o700)).unwrap();
    fs::write(&tree, "a file").unwrap();
    held("a file at the path");
    fs::remove_file(&tree).unwrap();
    fs::create_dir(&tree).unwrap();
    held("a directory at the path");
    fs::remove_dir(&tree).unwrap();
    let elsewhere = queue_dir.join("elsewhere");
    fs::create_dir(&elsewhere).unwrap();
    symlink(&elsewhere, &tree).unwrap();
    held("a symlink at the path");
    fs::remove_file(&tree).unwrap();
    assert_eq!(fs::read_dir(&elsewhere).unwrap().count(), 0);
    // On the way to it: the landing directory a symlink, or a file.
    fs::remove_dir(&landing).unwrap();
    symlink(&elsewhere, &landing).unwrap();
    held("a symlinked landing directory");
    fs::remove_file(&landing).unwrap();
    assert_eq!(fs::read_dir(&elsewhere).unwrap().count(), 0);
    fs::write(&landing, "a file").unwrap();
    held("a file for the landing directory");
    fs::remove_file(&landing).unwrap();
    // A landing directory its group may write.
    fs::create_dir(&landing).unwrap();
    fs::set_permissions(&landing, fs::Permissions::from_mode(0o770)).unwrap();
    held("a landing directory its group writes");
    fs::set_permissions(&landing, fs::Permissions::from_mode(0o700)).unwrap();
    // The queue's directory and a sound landing directory there already
    // are not refused, and two runs land one after the other under it.
    let outcome = integrate(&db, run.task.as_i64(), &repo).unwrap();
    assert_eq!(outcome["outcome"], "integrated", "{outcome}");
    let mut next = claim_in_container(&db, &repo, second);
    let head = commit(&next, &[("second.txt", "second\n")], "second");
    turn_end(&next);
    write_receipt(&next, &head);
    validate(&db, &repo, &mut next);
    let outcome = integrate(&db, next.task.as_i64(), &repo).unwrap();
    assert_eq!(outcome["outcome"], "integrated", "{outcome}");
    assert_eq!(git_out(&repo, &["show", "main:second.txt"]), "second");
    assert_eq!(
        fs::metadata(&landing).unwrap().permissions().mode() & 0o777,
        0o700
    );
}

/// AC 5: what the worker does to the run directory and the clone while
/// its session lives through the review changes neither the landing
/// worktree nor what lands, and no symlink it plants has the control side
/// write or read elsewhere; a resume's new commit lands from a landing
/// worktree made anew.
#[test]
fn what_the_worker_writes_during_the_review_does_not_reach_the_landing() {
    let (_fixture, repo, db) = fixture();
    let mut run = claim_in_container(&db, &repo, TaskId::new(1));
    let head = commit(&run, &[("feature.txt", "reviewed\n")], "reviewed");
    turn_end(&run);
    write_receipt(&run, &head);
    validate(&db, &repo, &mut run);
    let tree = start_in_tree(&repo, &run, None);
    // The live session goes on: a new commit, a new bundle and state, and a
    // symlink where main's bundle goes, to a file of the host.
    let host_file = run.run_dir.parent().unwrap().join("host-file");
    fs::write(&host_file, "the host's\n").unwrap();
    let later = commit(&run, &[("feature.txt", "after the review\n")], "later");
    turn_end(&run);
    let main_bundle = run.run_dir.join(run_clone::MAIN_BUNDLE_FILE);
    symlink(&host_file, &main_bundle).unwrap();
    // A commit on main makes the control side hand main to the clone.
    fs::write(repo.join("main.txt"), "main\n").unwrap();
    git(&repo, &["add", "main.txt"]);
    git(&repo, &["commit", "-q", "-m", "main moved"]);
    let main = git_out(&repo, &["rev-parse", "main"]);
    let repository = GitRepository::inspect(&repo).unwrap();
    run_git::hand_main(&repository, &run.run, &sha(&main)).unwrap();
    assert_eq!(fs::read_to_string(&host_file).unwrap(), "the host's\n");
    assert!(
        !fs::symlink_metadata(&main_bundle)
            .unwrap()
            .file_type()
            .is_symlink()
    );
    // Reading the facts again leaves the landing worktree as it was.
    assert_eq!(
        run_git::head(&repository, &run.run).unwrap().as_str(),
        later
    );
    assert_eq!(git_out(&tree, &["rev-parse", "HEAD"]), head);
    let outcome = integrate(&db, run.task.as_i64(), &repo).unwrap();
    assert_eq!(outcome["outcome"], "integrated", "{outcome}");
    assert_eq!(git_out(&repo, &["show", "main:feature.txt"]), "reviewed");
    assert_eq!(fs::read_to_string(&host_file).unwrap(), "the host's\n");
}

/// AC 5 and 6: a container run whose landing conflicts goes back to its
/// session naming the clone's main ref, gets main's bundle, rebases on it
/// in the clone, and its new commit lands from a landing worktree made
/// anew.
#[test]
fn a_conflicting_container_run_rebases_on_mains_bundle_and_lands_again() {
    let (_fixture, repo, db) = fixture();
    let mut run = claim_in_container(&db, &repo, TaskId::new(1));
    let head = commit(&run, &[("shared.txt", "the run's\n")], "run");
    turn_end(&run);
    write_receipt(&run, &head);
    validate(&db, &repo, &mut run);
    let tree = start_in_tree(&repo, &run, None);
    fs::write(repo.join("shared.txt"), "main's\n").unwrap();
    git(&repo, &["add", "shared.txt"]);
    git(&repo, &["commit", "-q", "-m", "main's"]);
    let main = git_out(&repo, &["rev-parse", "main"]);
    let outcome = integrate(&db, run.task.as_i64(), &repo).unwrap();
    assert_eq!(outcome["outcome"], "needs_session", "{outcome}");
    let reason = outcome["reason"].as_str().unwrap();
    assert!(reason.contains("shared.txt"), "{reason}");
    assert!(
        reason.contains(&format!(
            "git rebase {}, which holds {main}",
            run_clone::CLONE_MAIN_REF
        )),
        "{reason}"
    );
    // The landing worktree's rebase was given up.
    assert_eq!(git_out(&tree, &["rev-parse", "HEAD"]), head);
    assert_eq!(git_out(&tree, &["status", "--porcelain"]), "");
    // Before the resume, the control side hands main to the clone, whose
    // executor fetches it into the clone's main ref.
    let repository = GitRepository::inspect(&repo).unwrap();
    run.reload(&db);
    run_git::hand_main(&repository, &run.run, &sha(&main)).unwrap();
    let bundle = run.run_dir.join(run_clone::MAIN_BUNDLE_FILE);
    in_clone(
        &run.clone,
        &[
            "fetch",
            "-q",
            bundle.to_str().unwrap(),
            &format!("+refs/heads/main:{}", run_clone::CLONE_MAIN_REF),
        ],
    );
    assert_eq!(
        in_clone(&run.clone, &["rev-parse", run_clone::CLONE_MAIN_REF]),
        main
    );
    // The resumed worker rebases onto it and resolves the conflict.
    let rebase = Command::new("git")
        .arg("-C")
        .arg(&run.clone)
        .args([
            "-c",
            "core.hooksPath=/dev/null",
            "rebase",
            run_clone::CLONE_MAIN_REF,
        ])
        .bounded_output()
        .unwrap();
    assert!(!rebase.status.success());
    fs::write(run.clone.join("shared.txt"), "both\n").unwrap();
    in_clone(&run.clone, &["add", "shared.txt"]);
    let resolved = Command::new("git")
        .arg("-C")
        .arg(&run.clone)
        .args(["-c", "core.hooksPath=/dev/null", "-c", "core.editor=true"])
        .args(["rebase", "--continue"])
        .env("GIT_EDITOR", "true")
        .bounded_output()
        .unwrap();
    assert!(
        resolved.status.success(),
        "{}",
        String::from_utf8_lossy(&resolved.stderr)
    );
    let head = in_clone(&run.clone, &["rev-parse", "HEAD"]);
    turn_end(&run);
    write_receipt(&run, &head);
    let skipped = SqliteQueue::open(&db)
        .unwrap()
        .skip_resume(run.run.id(), &run.token, &sha(&head), &sha(&main), false)
        .unwrap();
    assert!(skipped.is_some());
    validate(&db, &repo, &mut run);
    // The new validation took the old landing worktree away.
    assert!(!tree.exists());
    let new_tree = start_in_tree(&repo, &run, None);
    assert_eq!(git_out(&new_tree, &["rev-parse", "HEAD"]), head);
    let outcome = integrate(&db, run.task.as_i64(), &repo).unwrap();
    assert_eq!(outcome["outcome"], "integrated", "{outcome}");
    assert_eq!(git_out(&repo, &["show", "main:shared.txt"]), "both");
}

/// AC 4: whatever the worker writes into the clone's configuration, hooks
/// and attributes, the control side's reading, validation, landing, review,
/// head reads and cleanup run none of it.
#[test]
fn nothing_a_worker_plants_in_its_clone_runs_on_the_control_side() {
    let (fixture, repo, db) = fixture();
    let dir = fixture.dir.path().to_path_buf();
    let marks_dir = dir.join("marks");
    let mark = marker(&dir, &marks_dir);
    let mut run = claim_in_container(&db, &repo, TaskId::new(1));
    let head = commit(
        &run,
        &[
            ("feature.txt", "feature\n"),
            (
                ".gitattributes",
                "* filter=planted diff=planted merge=planted\n",
            ),
        ],
        "feature",
    );
    turn_end(&run);
    write_receipt(&run, &head);
    // Planted after the turn, as a worker may at any time.
    let m = |name: &str| format!("{} {name}", mark.display());
    let included = dir.join("included.gitconfig");
    fs::write(
        &included,
        format!(
            "[core]\n\tfsmonitor = {}\n\tsshCommand = {}\n",
            m("include-fsmonitor"),
            m("ssh")
        ),
    )
    .unwrap();
    let hooks = dir.join("planted-hooks");
    fs::create_dir(&hooks).unwrap();
    let git_dir = run.clone.join(".git");
    fs::create_dir_all(git_dir.join("hooks")).unwrap();
    for hook in [
        "post-checkout",
        "post-merge",
        "post-rewrite",
        "reference-transaction",
        "pre-commit",
        "post-commit",
        "pre-rebase",
        "post-index-change",
    ] {
        for (dir, name) in [
            (&hooks, format!("hookspath-{hook}")),
            (&git_dir.join("hooks"), format!("hooks-{hook}")),
        ] {
            common::template::script(&dir.join(hook), format!("#!/bin/sh\n{}\n", m(&name)));
        }
    }
    let mut config = fs::OpenOptions::new()
        .append(true)
        .open(git_dir.join("config"))
        .unwrap();
    use std::io::Write as _;
    write!(
        config,
        "[core]\n\tfsmonitor = {fs}\n\thooksPath = {hooks}\n\tpager = {pager}\n\
         [filter \"planted\"]\n\tclean = {clean}\n\tsmudge = {smudge}\n\tprocess = {process}\n\trequired = true\n\
         [diff \"planted\"]\n\ttextconv = {textconv}\n\tcommand = {command}\n\
         [merge \"planted\"]\n\tdriver = {driver}\n\
         [diff]\n\texternal = {external}\n\
         [include]\n\tpath = {included}\n",
        fs = m("fsmonitor"),
        hooks = hooks.display(),
        pager = m("pager"),
        clean = m("clean"),
        smudge = m("smudge"),
        process = m("process"),
        textconv = m("textconv"),
        command = m("command"),
        driver = m("driver"),
        external = m("external"),
        included = included.display(),
    )
    .unwrap();
    fs::create_dir_all(git_dir.join("info")).unwrap();
    fs::write(
        git_dir.join("info/attributes"),
        "* filter=planted diff=planted\n",
    )
    .unwrap();
    // Every way the control side reads the run.
    validate(&db, &repo, &mut run);
    let repository = GitRepository::inspect(&repo).unwrap();
    run_git::head(&repository, &run.run).unwrap();
    run_git::session_head(&repository, &run.run).unwrap();
    run_git::status(&repository, &run.run).unwrap();
    run_git::rebase_in_progress(&repository, &run.run).unwrap();
    dagq::application::review::run_commits(&repository, &run.run).unwrap();
    dagq::application::inherit::carried_head(&run.run, &LocalRunFiles, &repository, &NoContainers)
        .unwrap();
    dagq::application::inherit::released_work(&run.run, &LocalRunFiles, &repository, &NoContainers)
        .unwrap();
    // The review's job and program and the e2e in the landing worktree.
    start_in_tree(&repo, &run, None);
    // The clone path itself is refused to git, whoever asks.
    assert!(repository.head(&run.clone).is_err());
    assert!(repository.status(&run.clone).is_err());
    assert!(repository.tracks(&run.clone, "feature.txt").is_err());
    let outcome = integrate(&db, run.task.as_i64(), &repo).unwrap();
    assert_eq!(outcome["outcome"], "integrated", "{outcome}");
    assert!(!run.clone.exists());
    assert_eq!(marks(&marks_dir), Vec::<String>::new());
    // The marker is live: git that reads such a configuration runs it.
    let proof = dir.join("proof");
    fs::create_dir(&proof).unwrap();
    git(&proof, &["init", "-q", "--template="]);
    git(&proof, &["config", "core.fsmonitor", &m("proof")]);
    let _ = Command::new("git")
        .arg("-C")
        .arg(&proof)
        .arg("status")
        .bounded_output();
    assert_eq!(marks(&marks_dir), ["proof"]);
}

/// A held run in a container released to a new run (it has no session) is
/// read from the facts its executor wrote, with no git in its clone
/// (container-runtime.md「git」): while they record uncommitted work, a
/// rebase stopped half way or a head its bundle does not hold, which the
/// control side cannot save, the release is refused and changes nothing;
/// clean, the head imported from its bundle is kept under
/// `refs/dagq/runs/<run-id>` and carried over, though nothing validated it.
#[test]
fn a_held_container_run_is_released_to_a_new_run_only_with_nothing_unsaved_in_its_clone() {
    let (_fixture, repo, db) = fixture();
    let run = claim_in_container(&db, &repo, TaskId::new(1));
    let head = commit(&run, &[("feature.txt", "feature\n")], "feature");
    turn_end(&run);
    let mut queue = SqliteQueue::open(&db).unwrap();
    queue
        .finish_supervision_live(run.run.id(), &run.token)
        .unwrap();
    queue.release_lease(run.run.id(), &run.token).unwrap();
    let as_user = |args: &[&str]| crate::common::cli::invoke_with(&[], &db, args);
    let held = as_user(&["hold", "1", "--reason", "the login expired"]);
    assert!(
        held.status.success(),
        "{}",
        String::from_utf8_lossy(&held.stderr)
    );
    assert_eq!(
        serde_json::from_slice::<Value>(&held.stdout).unwrap()["state"],
        "held"
    );
    let branch = format!("refs/heads/{}", run.branch());
    for (state_head, status, rebase) in [
        (&head, " M feature.txt", false),
        (&head, "", true),
        (&run.base, "", false),
    ] {
        fs::write(
            run.run_dir.join(run_clone::RUN_STATE_FILE),
            json!({"branch": branch, "head": state_head, "status": status,
                   "rebase_in_progress": rebase, "turn_ended": true})
            .to_string(),
        )
        .unwrap();
        let refused = as_user(&["release", "1"]);
        assert!(!refused.status.success(), "{status:?} {rebase}");
        let stderr = String::from_utf8_lossy(&refused.stderr);
        assert!(stderr.contains("cannot be saved"), "{stderr}");
        let detail = queue.show(run.task).unwrap();
        assert!(queue.task_hold(run.task).unwrap().is_some());
        assert_eq!(detail.runs[0].status(), RunStatus::Validating);
        assert_eq!(detail.task.status(), TaskStatus::InProgress);
        assert!(payloads(&detail, "task_released").is_empty());
    }
    turn_end(&run);
    let released = as_user(&["release", "1"]);
    assert!(
        released.status.success(),
        "{}",
        String::from_utf8_lossy(&released.stderr)
    );
    let released: Value = serde_json::from_slice(&released.stdout).unwrap();
    assert_eq!(released["decision"], "retry_inherit", "{released}");
    assert_eq!(released["inherit"]["head"], json!(head));
    assert!(released["inherit"].get("saved").is_none(), "{released}");
    assert_eq!(
        git_out(
            &repo,
            &["rev-parse", &format!("refs/dagq/runs/{}", run.run.id())]
        ),
        head
    );
    let detail = queue.show(run.task).unwrap();
    assert_eq!(detail.task.status(), TaskStatus::Ready);
    assert_eq!(detail.runs[0].status(), RunStatus::Failed);
}

/// A held container run is released with the head its clone's last turn
/// left, as its executor's facts say, past the commit its validation
/// accepted: that head is carried over and kept.
#[test]
fn a_held_container_run_is_released_with_the_head_its_last_turn_left_past_its_validated_commit() {
    let (_fixture, repo, db) = fixture();
    let mut run = claim_in_container(&db, &repo, TaskId::new(1));
    let validated = commit(&run, &[("feature.txt", "feature\n")], "feature");
    turn_end(&run);
    write_receipt(&run, &validated);
    assert_eq!(validate(&db, &repo, &mut run).as_str(), validated);
    let later = commit(&run, &[("later.txt", "later\n")], "later");
    turn_end(&run);
    let as_user = |args: &[&str]| crate::common::cli::invoke_with(&[], &db, args);
    let held = as_user(&["hold", "1", "--reason", "the login expired"]);
    assert!(
        held.status.success(),
        "{}",
        String::from_utf8_lossy(&held.stderr)
    );
    let released = as_user(&["release", "1", "--provider", "codex"]);
    assert!(
        released.status.success(),
        "{}",
        String::from_utf8_lossy(&released.stderr)
    );
    let released: Value = serde_json::from_slice(&released.stdout).unwrap();
    assert_eq!(released["decision"], "retry_inherit", "{released}");
    assert_eq!(released["inherit"]["head"], json!(later), "{released}");
    assert_eq!(
        git_out(
            &repo,
            &["rev-parse", &format!("refs/dagq/runs/{}", run.run.id())]
        ),
        later
    );
}

/// A turn of `run` through its container's executor (`dagq relay turn`),
/// run on the host as the fake podman of `run_container` runs it: `agent`
/// is a shell script the executor runs in the clone. The executor's exit
/// code, `None` when it was killed with its "container".
fn relay_turn(run: &ContainerRun, agent: &str) -> Option<i32> {
    fs::write(
        run.run_dir.join(dagq::domain::run_container::TOKEN_FILE),
        "relay-token\n",
    )
    .unwrap();
    let text = |path: &Path| path.to_str().unwrap().to_owned();
    let output = Command::new(env!("CARGO_BIN_EXE_dagq"))
        .without_actor_env()
        .owned_by_test()
        .env(dagq::domain::actor::RUN_ID_ENV, run.run.id().as_str())
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_AUTHOR_NAME", "worker")
        .env("GIT_AUTHOR_EMAIL", "worker@example.invalid")
        .env("GIT_COMMITTER_NAME", "worker")
        .env("GIT_COMMITTER_EMAIL", "worker@example.invalid")
        .args(["relay", "turn", "--run-dir", &text(&run.run_dir)])
        .args(["--clone", &text(&run.clone), "--base", &run.base])
        .args(["--branch", &run.branch(), "--", "sh", "-c", agent])
        .current_dir(&run.clone)
        .bounded_output()
        .unwrap();
    output.status.code()
}

/// The run's state as its executor last wrote it.
fn run_state(run: &ContainerRun) -> run_clone::RunState {
    run_clone::RunState::parse(
        &fs::read_to_string(run.run_dir.join(run_clone::RUN_STATE_FILE)).unwrap(),
    )
    .unwrap()
}

/// The executor's turn (`dagq relay turn`) marks the state its last turn's
/// end wrote before the agent runs, the other facts as they were, and the
/// turn's end writes the state of that turn; with no state yet it marks
/// nothing. A mark that cannot be written runs no agent and leaves the
/// state. Before the agent runs, a state that cannot be read, or a symlink
/// in its place (not what it points at), is gone, and a directory in its
/// place is left. (What a turn's start does with what it finds is
/// `run_clone::turn_start`'s, whose unit test holds each case.)
#[test]
fn a_container_turn_marks_its_last_state_before_its_agent_runs_and_writes_it_anew_at_its_end() {
    let (_fixture, repo, db) = fixture();
    let run = claim_in_container(&db, &repo, TaskId::new(1));
    let state = run.run_dir.join(run_clone::RUN_STATE_FILE);
    let none = format!("[ ! -e {0} ] && [ ! -L {0} ]", shell_path(&state));
    assert_eq!(relay_turn(&run, &none), Some(0));
    let ended = run_state(&run);
    assert_eq!(ended.turn_ended, Some(true));

    let seen = run.run_dir.join("seen-state.json");
    let copy = format!(
        "cp {} {} && echo b > b.txt",
        shell_path(&state),
        shell_path(&seen)
    );
    assert_eq!(relay_turn(&run, &copy), Some(0));
    let marked = run_clone::RunState::parse(&fs::read_to_string(&seen).unwrap()).unwrap();
    assert_eq!(
        marked,
        run_clone::RunState {
            turn_ended: Some(false),
            ..ended
        }
    );
    let after = run_state(&run);
    assert_eq!(after.status, "?? b.txt\n");
    assert_eq!(after.turn_ended, Some(true));

    // The run directory taken from the executor: no mark, no agent.
    fs::set_permissions(&run.run_dir, fs::Permissions::from_mode(0o555)).unwrap();
    let refused = relay_turn(&run, ": > ran.txt");
    fs::set_permissions(&run.run_dir, fs::Permissions::from_mode(0o755)).unwrap();
    assert_ne!(refused, Some(0));
    assert!(!run.clone.join("ran.txt").exists());
    assert_eq!(run_state(&run), after);

    fs::write(&state, b"\xff not a state").unwrap();
    assert_eq!(relay_turn(&run, &none), Some(0));
    assert_eq!(run_state(&run).turn_ended, Some(true));

    let elsewhere = run.run_dir.join("state-elsewhere.json");
    fs::rename(&state, &elsewhere).unwrap();
    let pointed = fs::read(&elsewhere).unwrap();
    symlink(&elsewhere, &state).unwrap();
    assert_eq!(relay_turn(&run, &none), Some(0));
    assert_eq!(fs::read(&elsewhere).unwrap(), pointed);
    assert!(fs::symlink_metadata(&state).unwrap().is_file());

    fs::remove_file(&state).unwrap();
    fs::create_dir(&state).unwrap();
    let left = format!("[ -d {} ]", shell_path(&state));
    assert_eq!(relay_turn(&run, &left), Some(0));
    assert!(state.is_dir());
}

/// The executor's turn writes the clone's uncommitted work (the staged
/// and unstaged diffs, the untracked files with their execute bits) of
/// the generation of the state it writes after it, one past the last; and
/// a restore handed for a clone that changed since its snapshot (here an
/// untracked file the snapshot does not hold) is answered as failed, runs
/// no agent and leaves the clone as it was, also on the next turn.
#[test]
fn a_container_turn_writes_its_work_and_refuses_to_restore_over_a_clone_that_changed() {
    use dagq::domain::clone_work::{
        self, CloneWork, RESTORE_RESULT_FILE, RUN_RESTORE_FILE, RUN_WORK_FILE, RestoreRequest,
    };
    let (_fixture, repo, db) = fixture();
    let run = claim_in_container(&db, &repo, TaskId::new(1));
    assert_eq!(relay_turn(&run, "true"), Some(0));
    assert_eq!(run_state(&run).generation, Some(1));
    let work = "echo staged > staged.txt && git add staged.txt && echo more >> seed.txt && printf '#!/bin/sh\\n' > run.sh && chmod +x run.sh";
    assert_eq!(relay_turn(&run, work), Some(0));
    let state = run_state(&run);
    assert_eq!(state.generation, Some(2));
    let (written, contents): (CloneWork, _) =
        clone_work::unpack(&fs::read(run.run_dir.join(RUN_WORK_FILE)).unwrap()).unwrap();
    assert_eq!(written.generation, 2);
    assert_eq!(written.head, state.head);
    let index = String::from_utf8_lossy(&contents[0]).into_owned();
    let unstaged = String::from_utf8_lossy(&contents[1]).into_owned();
    assert!(index.contains("staged.txt"), "{index}");
    assert!(unstaged.contains("+more"), "{unstaged}");
    let script = written
        .parts
        .iter()
        .find(|part| part.path.as_deref() == Some("run.sh"));
    assert!(script.is_some_and(|part| part.executable), "{written:?}");

    // A restore of the work as the turn left it, before the worker added
    // a file of its own: refused, the clone untouched, no agent.
    let request = RestoreRequest {
        snapshot_id: "replan-1-g2".into(),
        attempt: 1,
        base: run.base.clone(),
        branch: run.branch(),
        head: written.head.clone(),
        remade: false,
        parts: written.parts.clone(),
    };
    fs::write(
        run.run_dir.join(RUN_RESTORE_FILE),
        clone_work::pack(&request, &contents),
    )
    .unwrap();
    fs::write(run.clone.join("stray.txt"), "stray\n").unwrap();
    let ran = run.run_dir.join("agent-ran");
    let agent = format!(": > {}", shell_path(&ran));
    for _ in 0..2 {
        assert_ne!(relay_turn(&run, &agent), Some(0));
        assert!(!ran.exists());
        let answered: Value =
            serde_json::from_slice(&fs::read(run.run_dir.join(RESTORE_RESULT_FILE)).unwrap())
                .unwrap();
        assert_eq!(answered["snapshot_id"], "replan-1-g2");
        assert!(
            answered["failed"]
                .as_str()
                .is_some_and(|why| why.contains("changed since the park")),
            "{answered}"
        );
        assert_eq!(
            fs::read_to_string(run.clone.join("stray.txt")).unwrap(),
            "stray\n"
        );
        assert!(in_clone(&run.clone, &["diff", "--cached", "--name-only"]).contains("staged.txt"));
    }
}

/// A queue laid out as a host's (its directory names the runs'
/// containers), with the fake podman's containers of
/// `runtime_support::containers`: a facts-only turn runs the container's
/// executor (`dagq relay turn --facts-only`) on the host, with real git.
fn container_queue() -> (
    tempfile::TempDir,
    PathBuf,
    PathBuf,
    dagq::infrastructure::run_container::PodmanRunContainers,
) {
    let dir = tempfile::tempdir().unwrap();
    let (repo, db) = containers::queue_fixture(dir.path());
    let fake = containers::containers(&dir.path().join("podman"), &db, &repo);
    (dir, repo, db, fake)
}

/// A turn of `run` that commits `text` to `feature.txt` and ends: its
/// executor's facts of the turn's end, as [`turn_end`] writes them.
fn ended_turn(run: &ContainerRun, text: &str) -> String {
    let head = commit(run, &[("feature.txt", text)], text);
    turn_end(run);
    head
}

/// A turn of `run` that commits `text` and is cut off with its container
/// before its executor writes the turn's end: the state an earlier turn's
/// end wrote is left marked as its start marks it (`turn_ended: false`,
/// what `relay turn` does, held by
/// `a_container_turn_marks_its_last_state_before_its_agent_runs_and_writes_it_anew_at_its_end`),
/// and none is written when there was none. The head it left.
fn cut_off_turn(run: &ContainerRun, text: &str) -> String {
    // The run token's copy goes only with a stop's removal of the
    // container: a turn cut off with it leaves the copy behind.
    fs::write(
        run.run_dir.join(dagq::domain::run_container::TOKEN_FILE),
        "run-token\n",
    )
    .unwrap();
    let state = run.run_dir.join(run_clone::RUN_STATE_FILE);
    if state.exists() {
        let marked = run_clone::RunState {
            turn_ended: Some(false),
            ..run_state(run)
        };
        fs::write(&state, serde_json::to_vec(&marked).unwrap()).unwrap();
    }
    commit(run, &[("feature.txt", text)], text)
}

/// Task 1's run in a container whose last turn committed and was cut off
/// with its container, then recovered (`interrupted`, no lease) and held:
/// with `first_ended`, a first turn's end recorded clean facts at its own
/// commit and the cut-off turn's start marked them (`turn_ended: false`);
/// else the cut-off turn was its first and no state is there. The head the
/// cut-off turn left.
fn held_cut_off_run(db: &Path, repo: &Path, first_ended: bool) -> (ContainerRun, String) {
    let run = claim_in_container(db, repo, TaskId::new(1));
    let state = run.run_dir.join(run_clone::RUN_STATE_FILE);
    if first_ended {
        let first = ended_turn(&run, "feature");
        let latest = cut_off_turn(&run, "more");
        let left = run_state(&run);
        assert_eq!(
            (left.head.as_str(), left.turn_ended),
            (first.as_str(), Some(false))
        );
        recover(db, &run);
        hold(db);
        (run, latest)
    } else {
        let latest = cut_off_turn(&run, "feature");
        assert!(!state.exists());
        recover(db, &run);
        hold(db);
        (run, latest)
    }
}

/// The run recovered as its dead session's supervisor recovers it:
/// `interrupted`, with no lease.
fn recover(db: &Path, run: &ContainerRun) {
    let mut queue = SqliteQueue::open(db).unwrap();
    queue.release_lease(run.run.id(), &run.token).unwrap();
    let recovered = queue.recover_run(run.run.id(), 0, json!({})).unwrap();
    assert_eq!(recovered.status(), RunStatus::Interrupted);
}

/// Task 1 held by a person, in this process.
fn hold(db: &Path) {
    let mut queue = SqliteQueue::open(db).unwrap();
    let actor = dagq::domain::ActorContext::user();
    dagq::application::commands::holds::Holds::new(&mut queue, &actor, &dagq::domain::StaticPolicy)
        .hold(
            &dagq::domain::task_hold::HoldTarget::Task(TaskId::new(1)),
            "the login expired",
            None,
        )
        .unwrap();
}

/// A person's `release 1 --provider codex` in this process, the run's
/// facts-only turn run in `containers`.
fn release_with(
    db: &Path,
    containers: &dyn dagq::application::run_container::RunContainers,
) -> Result<Value> {
    use dagq::domain::task_hold::{HoldTarget, ReleaseChoice};
    let mut queue = SqliteQueue::open(db).unwrap();
    let actor = dagq::domain::ActorContext::user();
    let released = dagq::application::commands::holds::Holds::new(
        &mut queue,
        &actor,
        &dagq::domain::StaticPolicy,
    )
    .release(
        &HoldTarget::Task(TaskId::new(1)),
        ReleaseChoice {
            provider: Some(dagq::domain::Provider::Codex),
            model: None,
        },
        &dagq::infrastructure::adapters::RunRepositories { containers },
    )?;
    Ok(serde_json::to_value(released).unwrap())
}

/// Whether the ref that keeps the carried head of `run` is there.
fn kept_ref(repo: &Path, run: &ContainerRun) -> Option<String> {
    let kept = format!("refs/dagq/runs/{}", run.run.id());
    Command::new(git_executable().expect("git executable"))
        .arg("-C")
        .arg(repo)
        .args(["rev-parse", "--verify", "-q", &kept])
        .bounded_status()
        .unwrap()
        .success()
        .then(|| git_out(repo, &["rev-parse", &kept]))
}

/// The release of the run of [`held_cut_off_run`] carries over the head
/// a facts-only turn found in its clone, the cut-off turn's commit, not
/// an older head: the executor wrote the facts anew (`turn_ended: true`)
/// in the one facts-only turn, the head is kept, and the task is ready
/// with its hold gone.
fn released_with_the_facts_only_turn_s_head(first_ended: bool) {
    let (dir, repo, db, fake) = container_queue();
    let (run, latest) = held_cut_off_run(&db, &repo, first_ended);
    let released = release_with(&db, &fake).unwrap();
    assert_eq!(released["decision"], "retry_inherit", "{released}");
    assert_eq!(released["inherit"]["head"], json!(latest), "{released}");
    assert!(released["inherit"].get("saved").is_none(), "{released}");
    assert_eq!(kept_ref(&repo, &run), Some(latest.clone()));
    let refreshed = run_state(&run);
    assert_eq!(
        (refreshed.head.as_str(), refreshed.turn_ended),
        (latest.as_str(), Some(true))
    );
    assert_eq!(facts_turns(dir.path()), 1);
    facts_turn_was_narrow(dir.path(), &run);
    let mut queue = SqliteQueue::open(&db).unwrap();
    assert_eq!(
        queue.show(run.task).unwrap().task.status(),
        TaskStatus::Ready
    );
    assert!(queue.task_hold(run.task).unwrap().is_none());
}

/// The facts-only turn of `run` (the last container the fake podman of
/// the queue in `dir` ran) had only the machine and the image made ready,
/// with no relay of the run towards the queue broker; mounted only the
/// clone and the run directory, with no socket and no variable; and found
/// no run token's copy left in the run directory.
#[track_caller]
fn facts_turn_was_narrow(dir: &Path, run: &ContainerRun) {
    let state = dir.join("podman");
    assert_eq!(
        containers::machine_calls(&state),
        [format!("image {}", run.run.id())]
    );
    let args: Vec<String> = fs::read_to_string(state.join("run-args"))
        .unwrap()
        .lines()
        .map(str::to_owned)
        .collect();
    let values = |flag: &str| {
        args.windows(2)
            .filter(|pair| pair[0] == flag)
            .map(|pair| pair[1].clone())
            .collect::<Vec<_>>()
    };
    // The paths as the run records them, which may be the canonical ones.
    let id = run.run.id();
    let volumes = values("--volume");
    let mounted = |volume: &String, dir: &str| {
        volume
            .split_once(':')
            .is_some_and(|(from, to)| from == to && from.ends_with(&format!("/{dir}/{id}")))
    };
    assert!(
        volumes.len() == 2 && mounted(&volumes[0], "clones") && mounted(&volumes[1], "runs"),
        "{volumes:?}"
    );
    assert!(values("--env").is_empty(), "{args:?}");
    assert!(args.iter().any(|arg| arg == "--facts-only"), "{args:?}");
    assert!(
        !run.run_dir
            .join(dagq::domain::run_container::TOKEN_FILE)
            .exists()
    );
}

/// The facts-only turns the fake podman of the queue in `dir` ran.
fn facts_turns(dir: &Path) -> usize {
    containers::calls(&dir.join("podman"))
        .into_iter()
        .filter(|call| call.contains("relay turn --facts-only"))
        .count()
}

/// Acceptance (a): a held, interrupted container run whose last turn was
/// cut off with its container, its facts marked `turn_ended: false` at an
/// earlier turn's head, is released with the head of the cut-off turn.
#[test]
fn a_held_container_run_whose_last_turn_was_cut_off_is_released_with_the_head_its_facts_only_turn_found()
 {
    released_with_the_facts_only_turn_s_head(true);
}

/// Acceptance (a): so is one whose only turn was cut off before any state
/// was written.
#[test]
fn a_held_container_run_with_no_state_is_released_with_the_head_its_facts_only_turn_found() {
    released_with_the_facts_only_turn_s_head(false);
}

/// Acceptance (b): when the facts-only turn does not run (no machine, no
/// image), the release of such a run is refused with why and changes
/// nothing: the hold stays, the task keeps its worker and status, the run
/// its status, and no head is carried over or kept.
#[test]
fn a_cut_off_container_run_whose_facts_only_turn_does_not_run_is_not_released() {
    let (_dir, repo, db, _fake) = container_queue();
    let (run, _latest) = held_cut_off_run(&db, &repo, true);
    let mut queue = SqliteQueue::open(&db).unwrap();
    let before = queue.show(run.task).unwrap();
    let why = format!("{:#}", release_with(&db, &NoContainers).unwrap_err());
    for part in [
        "the hold stays",
        "the facts-only turn that would write them anew did not run",
        "this test runs no facts-only turn",
    ] {
        assert!(why.contains(part), "{part}: {why}");
    }
    let detail = queue.show(run.task).unwrap();
    assert!(queue.task_hold(run.task).unwrap().is_some());
    assert_eq!(detail.task.worker(), before.task.worker());
    assert_eq!(detail.task.status(), TaskStatus::InProgress);
    assert_eq!(detail.runs.len(), 1);
    assert_eq!(detail.runs[0].status(), RunStatus::Interrupted);
    assert!(payloads(&detail, "task_released").is_empty());
    assert_eq!(kept_ref(&repo, &run), None);
    assert_eq!(run_state(&run).turn_ended, Some(false));
}

/// Acceptance (f): a person's `ready --inherit` of an interrupted container
/// run whose last turn was cut off carries over the head a facts-only turn
/// found, as the release does: through the entry both the carried head and
/// a release's read of the run's work go through
/// (`inherit::container_head`), whichever of them `ready --inherit` reads.
#[test]
fn ready_inherit_of_a_cut_off_container_run_carries_the_head_its_facts_only_turn_found() {
    let (_dir, repo, db, fake) = container_queue();
    let run = claim_in_container(&db, &repo, TaskId::new(1));
    ended_turn(&run, "feature");
    let latest = cut_off_turn(&run, "more");
    recover(&db, &run);
    let mut queue = SqliteQueue::open(&db).unwrap();
    let inbox = dagq::domain::ActorContext::instance(dagq::domain::ActorRole::Inbox, 1);
    let inherited = dagq::application::commands::planning::Planning::new(
        &mut queue,
        &inbox,
        &dagq::domain::StaticPolicy,
    )
    .ready_inheriting(
        TaskId::new(1),
        "the turn's work is in its clone",
        &dagq::infrastructure::adapters::RunRepositories { containers: &fake },
    )
    .unwrap();
    assert_eq!(inherited.head.as_str(), latest);
    assert_eq!(kept_ref(&repo, &run), Some(latest.clone()));
    assert_eq!(run_state(&run).turn_ended, Some(true));
    let detail = SqliteQueue::open(&db).unwrap().show(run.task).unwrap();
    assert_eq!(detail.task.status(), TaskStatus::Ready);
}

/// A container run of task 1 whose first turn ended at its base and whose
/// second committed and was cut off with its container, its session gone
/// (a dead wrapper, no lease), triaged by a supervisor whose recovery job
/// repairs it with `action`. Its facts say the base with no bundle until
/// a facts-only turn writes them anew. The run's events, its task, the
/// cut-off turn's head and the queue's directory.
fn triaged_cut_off_run(
    action: &str,
) -> (
    Vec<dagq::domain::RunEvent>,
    dagq::domain::TaskDetail,
    String,
    ContainerRun,
    tempfile::TempDir,
    PathBuf,
    PathBuf,
) {
    let (dir, repo, db, fake) = container_queue();
    // The next run's worker is in a container that cannot be used: the
    // task a repair readies is not claimed again.
    fs::write(
        repo.join("dagq.toml"),
        "[roles.worker]\nruntime = \"container\"\n",
    )
    .unwrap();
    git(&repo, &["add", "dagq.toml"]);
    git(&repo, &["commit", "-qm", "the worker in the container"]);
    let mut run = plan_in_container(&db, &repo, TaskId::new(1));
    let mut queue = SqliteQueue::open(&db).unwrap();
    let token = run.token.as_str().to_owned();
    let dead = dead_pid();
    let handle = dagq::domain::background_wrapper::BackgroundHandle::new(
        dead,
        &format!("gone {}", run.run.id()),
    );
    record_background_start(&db, &mut queue, &run.run, &token, &handle.to_string());
    run.reload(&db);
    GitRepository::inspect(&repo)
        .unwrap()
        .create_clone(&run.run, None)
        .unwrap();
    turn_end(&run);
    assert_eq!(run_state(&run).head, run.base);
    let latest = cut_off_turn(&run, "feature");
    assert!(!run.run_dir.join(run_clone::RUN_BUNDLE_FILE).exists());
    queue
        .register_wrapper(run.run.id(), &run.token, dead)
        .unwrap();
    queue.register_agent(run.run.id(), dead, dead).unwrap();
    Connection::open(&db)
        .unwrap()
        .execute("DELETE FROM run_leases", [])
        .unwrap();
    let backend = TestWorkspace::new(&db, false, "exit 7");
    let reviewer = TestReviewer::new(&[verdict("pass", &[], "unused")])
        .with_triages(&[repair(json!({"action": action}), "the machine restarted")]);
    let mut options = supervise_options(1, true);
    options.run_containers = Some(runtime::RunContainersPort(Arc::new(fake)));
    options.container = Some(runtime::ContainerRuntimePort(Arc::new(Unusable)));
    let outcome = supervise_reviewed_with(&db, &repo, &backend, &reviewer, &options);
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    let detail = queue.show(TaskId::new(1)).unwrap();
    assert_eq!(detail.runs[0].id(), run.run.id());
    let events = queue.run_events(run.run.id()).unwrap();
    let started = events
        .iter()
        .find(|event| event.kind.as_str() == "triage_started")
        .unwrap();
    assert_eq!(started.payload["status"], "interrupted");
    assert_eq!(run_state(&run).turn_ended, Some(true));
    assert_eq!(facts_turns(dir.path()), 1, "{action}");
    facts_turn_was_narrow(dir.path(), &run);
    (events, detail, latest, run, dir, repo, db)
}

/// A container runtime that cannot be used.
struct Unusable;

impl dagq::application::ContainerRuntime for Unusable {
    fn availability(
        &self,
        _: dagq::domain::container_runtime::RuntimeTarget,
    ) -> dagq::domain::container_runtime::Availability {
        dagq::domain::container_runtime::Availability::Unavailable("not in this test".to_owned())
    }
}

/// The payloads of the events of `kind` among `events`.
fn of_kind(events: &[dagq::domain::RunEvent], kind: &str) -> Vec<Value> {
    events
        .iter()
        .filter(|event| event.kind.as_str() == kind)
        .map(|event| event.payload.clone())
        .collect()
}

/// Acceptance (c): the triage of a cut-off container run reads its own
/// commits from facts a facts-only turn wrote anew, so a `retry` from
/// scratch is refused (its branch holds the cut-off turn's commit) and
/// asked of a person instead.
#[test]
fn the_triage_of_a_cut_off_container_run_refuses_a_retry_from_scratch() {
    let (events, detail, _latest, _run, _dir, _repo, db) = triaged_cut_off_run("retry");
    assert_eq!(detail.runs.len(), 1);
    let finished = of_kind(&events, "triage_finished");
    assert_eq!(finished[0]["action"], "ask", "{finished:?}");
    let ask = SqliteQueue::open(&db)
        .unwrap()
        .read_ask(AskId::new(finished[0]["ask_id"].as_i64().unwrap()))
        .unwrap();
    assert!(
        ask.question.contains("its branch holds commits of its own"),
        "{}",
        ask.question
    );
}

/// Acceptance (c): its `retry_inherit` carries the cut-off turn's commit
/// over and keeps it, its own commits read from the facts written anew.
#[test]
fn the_triage_of_a_cut_off_container_run_carries_the_cut_off_turn_s_commit_over() {
    let (events, detail, latest, run, _dir, repo, _db) = triaged_cut_off_run("retry_inherit");
    let finished = of_kind(&events, "triage_finished");
    assert_eq!(finished[0]["action"], "retry_inherit", "{finished:?}");
    assert_eq!(finished[0]["inherit"]["head"], json!(latest));
    assert_eq!(
        of_kind(&events, "auto_repaired")[0]["conditions"]["own_commits"],
        true
    );
    assert_eq!(kept_ref(&repo, &run), Some(latest.clone()));
    assert_eq!(detail.task.status(), TaskStatus::Ready);
}

/// Configuration of `scope` that defines the drivers `name` names, each
/// marking `name-<key>` when it runs.
fn drivers(name: &str, mark: &Path) -> String {
    let m = |key: &str| format!("{} {name}-{key}", mark.display());
    format!(
        "[filter \"{name}\"]\n\tclean = {}\n\tsmudge = {}\n\tprocess = {}\n\
         [diff \"{name}\"]\n\ttextconv = {}\n\tcommand = {}\n\
         [merge \"{name}\"]\n\tdriver = {}\n",
        m("clean"),
        m("smudge"),
        m("process"),
        m("textconv"),
        m("command"),
        m("driver"),
    )
}

/// The worker's change that names the tracked hooks and the drivers the
/// control side's configuration defines: `.githooks` with every hook
/// calling `scripts/hook.sh`, and `.gitattributes` selecting the
/// repository's and the global configuration's drivers.
fn tracked_hooks(mark: &Path) -> Vec<(String, String)> {
    let mut files = vec![
        (
            "scripts/hook.sh".to_owned(),
            format!("#!/bin/sh\n{} tracked-$1\n", mark.display()),
        ),
        (
            ".gitattributes".to_owned(),
            "*.r filter=repo diff=repo merge=repo\n*.g filter=glob diff=glob merge=glob\n"
                .to_owned(),
        ),
        ("a.r".to_owned(), "repo\n".to_owned()),
        ("a.g".to_owned(), "glob\n".to_owned()),
    ];
    for hook in [
        "post-checkout",
        "post-merge",
        "post-rewrite",
        "reference-transaction",
        "pre-commit",
        "prepare-commit-msg",
        "commit-msg",
        "post-commit",
        "pre-rebase",
        "pre-push",
    ] {
        files.push((
            format!(".githooks/{hook}"),
            format!("#!/bin/sh\nsh \"$(git rev-parse --show-toplevel)/scripts/hook.sh\" {hook}\n"),
        ));
    }
    files
}

/// The repository as this one is set up: `core.hooksPath=.githooks`, and
/// drivers in the repository's and the global configuration.
fn configure_drivers(repo: &Path, dir: &Path, mark: &Path) -> PathBuf {
    git(repo, &["config", "core.hooksPath", ".githooks"]);
    let config = repo.join(".git/config");
    let mut text = fs::read_to_string(&config).unwrap();
    text.push_str(&drivers("repo", mark));
    fs::write(&config, text).unwrap();
    let global = dir.join("global.gitconfig");
    fs::write(&global, drivers("glob", mark)).unwrap();
    global
}

/// `dagq integrate` of `task` in another process whose global git
/// configuration is `global`.
fn integrate_with_global(db: &Path, repo: &Path, task: TaskId, global: &Path) -> Value {
    let output = Command::new(env!("CARGO_BIN_EXE_dagq"))
        .without_actor_env()
        .owned_by_test()
        .env("GIT_CONFIG_GLOBAL", global)
        .arg("--db")
        .arg(db)
        .args(["integrate", &task.to_string(), "--no-push", "--repo"])
        .arg(repo)
        .current_dir(repo)
        .bounded_output()
        .unwrap();
    serde_json::from_slice(&output.stdout).unwrap_or_else(|_| {
        panic!(
            "integrate: {}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        )
    })
}

/// The test's own git in the main checkout, as a person's: none of the
/// repository's hooks and drivers, which would mark.
fn git_unfiltered(repo: &Path, args: &[&str]) {
    let mut all = vec![
        "-c",
        "core.hooksPath=/dev/null",
        "-c",
        "filter.repo.clean=",
        "-c",
        "filter.repo.smudge=",
        "-c",
        "filter.repo.process=",
    ];
    all.extend_from_slice(args);
    git(repo, &all);
}

/// A repository that is dagq's source, whose migrations are renumbered on
/// landing (ADR-0067 decision 3).
fn migration_source(repo: &Path) {
    fs::write(repo.join("Cargo.toml"), "[package]\nname = \"dagq\"\n").unwrap();
    fs::create_dir(repo.join("migrations")).unwrap();
    fs::write(repo.join("migrations/0001_first.sql"), "-- first\n").unwrap();
    git(repo, &["add", "."]);
    git(repo, &["commit", "-q", "-m", "first migration"]);
}

/// AC 4: the tracked hooks and the drivers the control side's
/// configuration defines (the repository's and the global), selected by a
/// container run's commit, run in none of the import, the validation, the
/// landing worktree, the rebase (and its abort on a conflict), the
/// renumbering of a migration, main's fast-forward, the cleanup and the
/// review's processes in the landing worktree.
#[test]
fn a_container_runs_commit_runs_no_tracked_hook_and_no_driver_on_the_control_side() {
    let (fixture, repo, db) = fixture();
    let dir = fixture.dir.path().to_path_buf();
    let marks_dir = dir.join("marks");
    let mark = marker(&dir, &marks_dir);
    migration_source(&repo);
    let global = configure_drivers(&repo, &dir, &mark);
    let mut queue = SqliteQueue::open(&db).unwrap();
    let conflicting = add_ready_task(&mut queue, "conflicting", &[]);
    let mut run = claim_in_container(&db, &repo, TaskId::new(1));
    let mut files = tracked_hooks(&mark);
    files.push(("migrations/0002_run.sql".into(), "-- run\n".into()));
    let files: Vec<(&str, &str)> = files
        .iter()
        .map(|(p, t)| (p.as_str(), t.as_str()))
        .collect();
    let head = commit(&run, &files, "hooks and drivers");
    turn_end(&run);
    write_receipt(&run, &head);
    // Main takes the migration's number meanwhile.
    fs::write(repo.join("migrations/0002_main.sql"), "-- main\n").unwrap();
    git_unfiltered(&repo, &["add", "migrations"]);
    git_unfiltered(&repo, &["commit", "-q", "-m", "main's migration"]);
    validate(&db, &repo, &mut run);
    start_in_tree(&repo, &run, Some(&global));
    let outcome = integrate_with_global(&db, &repo, run.task, &global);
    assert_eq!(outcome["outcome"], "integrated", "{outcome}");
    assert_eq!(
        git_out(&repo, &["show", "main:migrations/0003_run.sql"]),
        "-- run"
    );
    assert!(repo.join(".githooks/post-merge").exists());
    assert!(!run.clone.exists());
    assert_eq!(marks(&marks_dir), Vec::<String>::new());

    // A run whose landing conflicts on a file whose merge driver the
    // repository defines: the rebase stops and is given up, and still no
    // hook or driver runs.
    let mut next = claim_in_container(&db, &repo, conflicting);
    let head = commit(
        &next,
        &[("a.r", "the run's\n"), ("a.g", "the run's\n")],
        "conflicting",
    );
    turn_end(&next);
    write_receipt(&next, &head);
    fs::write(repo.join("a.r"), "main's\n").unwrap();
    git_unfiltered(&repo, &["add", "a.r"]);
    git_unfiltered(&repo, &["commit", "-q", "-m", "main's a.r"]);
    validate(&db, &repo, &mut next);
    start_in_tree(&repo, &next, Some(&global));
    let outcome = integrate_with_global(&db, &repo, next.task, &global);
    assert_eq!(outcome["outcome"], "needs_session", "{outcome}");
    assert!(
        outcome["reason"].as_str().unwrap().contains("a.r"),
        "{outcome}"
    );
    assert_eq!(marks(&marks_dir), Vec::<String>::new());
    // The cleanup at the run's end runs none either.
    let repository = GitRepository::inspect(&repo).unwrap();
    next.reload(&db);
    run_git::remove(&repository, &next.run, &next.branch()).unwrap();
    assert!(!next.clone.exists());
    assert_eq!(marks(&marks_dir), Vec::<String>::new());
}

/// AC 4: the same commit landed from a run on the host runs the tracked
/// hooks as before: the test above is not passing for want of a hook that
/// could run.
#[test]
fn a_host_runs_landing_of_the_same_commit_runs_the_tracked_hooks() {
    let (fixture, repo, db) = fixture();
    let dir = fixture.dir.path().to_path_buf();
    let marks_dir = dir.join("marks");
    let mark = marker(&dir, &marks_dir);
    git(&repo, &["config", "core.hooksPath", ".githooks"]);
    let global = dir.join("global.gitconfig");
    fs::write(&global, "").unwrap();
    let mut queue = SqliteQueue::open(&db).unwrap();
    let common_dir = GitRepository::inspect(&repo).unwrap().common_dir;
    queue.bind_repository(common_dir.to_str().unwrap()).unwrap();
    let base = git_out(&repo, &["rev-parse", "main"]);
    let token = LeaseToken::new("host");
    let ClaimOutcome::Claimed { run } = queue
        .claim_for_supervisor_in_order(
            &sha(&base),
            &token,
            &[TaskId::new(1)],
            None,
            &Default::default(),
            &WorkerRoute::direct(&[Worker::CLAUDE_HEADLESS]),
        )
        .unwrap()
    else {
        panic!("not claimed")
    };
    let runs = runs_dir(&db);
    let run_dir = runs.join(run.id().as_str());
    let worktree = run_dir.join("worktree");
    fs::create_dir_all(&run_dir).unwrap();
    let text = |path: &Path| path.to_str().unwrap().to_owned();
    queue
        .plan_run(
            run.id(),
            &token,
            &RunPlan {
                repo_path: text(&repo),
                run_dir: text(&run_dir),
                branch: format!("dagq/{}", run.id()),
                worktree_path: text(&worktree),
                receipt_path: text(&run_dir.join("receipt.json")),
                log_path: text(&run_dir.join("log")),
            },
        )
        .unwrap();
    queue
        .workspace_created(run.id(), &token, "background:1:1")
        .unwrap();
    let run = queue.run(run.id()).unwrap();
    assert!(!run.in_container());
    GitRepository::inspect(&repo)
        .unwrap()
        .create_worktree(&run)
        .unwrap();
    for (path, body) in tracked_hooks(&mark) {
        let path = worktree.join(path);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, body).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
    }
    git(
        &worktree,
        &["-c", "core.hooksPath=/dev/null", "add", "--all"],
    );
    git(
        &worktree,
        &[
            "-c",
            "core.hooksPath=/dev/null",
            "commit",
            "-q",
            "-m",
            "hooks",
        ],
    );
    let head = git_out(&worktree, &["rev-parse", "HEAD"]);
    fs::write(repo.join("main.txt"), "main\n").unwrap();
    git(
        &repo,
        &["-c", "core.hooksPath=/dev/null", "add", "main.txt"],
    );
    git(
        &repo,
        &[
            "-c",
            "core.hooksPath=/dev/null",
            "commit",
            "-q",
            "-m",
            "main moved",
        ],
    );
    fs::write(
        run_dir.join("receipt.json"),
        json!({
            "run_id": run.id(),
            "result": "succeeded",
            "commit": head,
            "tests": {"status": "passed", "evidence_or_reason": "unit"},
            "e2e": {"status": "not_applicable", "evidence_or_reason": "the runtime runs it"},
            "subagent_review": {"status": "not_applicable", "evidence_or_reason": "small"},
            "summary": "done",
        })
        .to_string(),
    )
    .unwrap();
    queue.finish_supervision_live(run.id(), &token).unwrap();
    queue
        .finish_validation(
            run.id(),
            &token,
            &Validation {
                accepted: true,
                result_commit: Some(sha(&head)),
                reason: None,
                code: None,
                receipt: serde_json::from_str(
                    &fs::read_to_string(run_dir.join("receipt.json")).unwrap(),
                )
                .unwrap(),
                evidence_missing: Vec::new(),
                spike_result_missing: Vec::new(),
                scope_violation: Vec::new(),
                allowed_paths: Vec::new(),
                e2e_requirement: None,
                already_satisfied: false,
                load: Default::default(),
            },
        )
        .unwrap();
    queue.release_lease(run.id(), &token).unwrap();
    let _ = fs::remove_dir_all(&marks_dir);
    let outcome = integrate_with_global(&db, &repo, TaskId::new(1), &global);
    assert_eq!(outcome["outcome"], "integrated", "{outcome}");
    let found = marks(&marks_dir);
    assert!(
        found.iter().any(|mark| mark.starts_with("tracked-")),
        "a tracked hook ran on the host's landing: {found:?}"
    );
}

/// AC 7: removing what a container run left follows no symlink the worker
/// planted, in its clone or its run directory, and runs no git in the
/// clone.
#[test]
fn a_container_runs_cleanup_follows_no_symlink_the_worker_planted() {
    let (fixture, repo, db) = fixture();
    let dir = fixture.dir.path().to_path_buf();
    let kept = dir.join("host-dir");
    fs::create_dir(&kept).unwrap();
    fs::write(kept.join("keep.txt"), "keep\n").unwrap();
    let kept_file = dir.join("host-file");
    fs::write(&kept_file, "keep\n").unwrap();
    let mut run = claim_in_container(&db, &repo, TaskId::new(1));
    let head = commit(&run, &[("feature.txt", "x\n")], "feature");
    turn_end(&run);
    write_receipt(&run, &head);
    validate(&db, &repo, &mut run);
    start_in_tree(&repo, &run, None);
    symlink(&kept, run.clone.join("into-host")).unwrap();
    symlink(&kept_file, run.clone.join("host-file")).unwrap();
    fs::remove_file(run.run_dir.join(run_clone::RUN_STATE_FILE)).unwrap();
    symlink(&kept_file, run.run_dir.join(run_clone::RUN_STATE_FILE)).unwrap();
    symlink(&kept, run.run_dir.join(run_clone::MAIN_BUNDLE_FILE)).unwrap();
    let repository = GitRepository::inspect(&repo).unwrap();
    run_git::remove(&repository, &run.run, &run.branch()).unwrap();
    assert!(!run.clone.exists());
    assert!(!run.run.landing_path().unwrap().exists());
    assert!(git_out(&repo, &["branch", "--list", &run.branch()]).is_empty());
    for name in [
        run_clone::RUN_STATE_FILE,
        run_clone::MAIN_BUNDLE_FILE,
        run_clone::RUN_BUNDLE_FILE,
    ] {
        assert!(
            fs::symlink_metadata(run.run_dir.join(name)).is_err(),
            "{name}"
        );
    }
    assert_eq!(fs::read_to_string(kept.join("keep.txt")).unwrap(), "keep\n");
    assert_eq!(fs::read_to_string(&kept_file).unwrap(), "keep\n");
    // Again, with everything gone already: nothing to do.
    run_git::remove(&repository, &run.run, &run.branch()).unwrap();
    let facts: Option<CloneFacts> = None;
    assert!(facts.is_none());
}

/// A clone whose making fails half way (a fetch of a ref the repository
/// does not have) leaves nothing at its path, and the run's next
/// provisioning makes it; a clone an earlier attempt left is removed and
/// made again while no worker was started for the run, and kept as it is
/// once one was.
#[test]
fn a_clone_left_half_made_is_removed_and_made_again_before_the_worker_starts() {
    let (_fixture, repo, db) = fixture();
    let run = plan_in_container(&db, &repo, TaskId::new(1));
    let repository = GitRepository::inspect(&repo).unwrap();
    let error = repository
        .create_clone(&run.run, Some("refs/dagq/runs/missing"))
        .unwrap_err();
    assert!(
        format!("{error:#}").contains("refs/dagq/runs/missing"),
        "{error:#}"
    );
    assert!(
        fs::symlink_metadata(&run.clone).is_err(),
        "the half made clone is left"
    );
    repository.create_clone(&run.run, None).unwrap();
    assert_eq!(in_clone(&run.clone, &["rev-parse", "HEAD"]), run.base);
    // What an earlier attempt left, as a stopped supervisor leaves it.
    fs::remove_dir_all(&run.clone).unwrap();
    fs::create_dir_all(run.clone.join(".git")).unwrap();
    fs::write(run.clone.join("left.txt"), "left\n").unwrap();
    repository.create_clone(&run.run, None).unwrap();
    assert!(!run.clone.join("left.txt").exists());
    assert_eq!(in_clone(&run.clone, &["rev-parse", "HEAD"]), run.base);
    assert_eq!(in_clone(&run.clone, &["status", "--porcelain"]), "");
    // Once a worker was started on the clone, it is not the provisioning's
    // to remove.
    let mut queue = SqliteQueue::open(&db).unwrap();
    queue
        .workspace_created(run.run.id(), &run.token, "background:1:1")
        .unwrap();
    let started = queue.run(run.run.id()).unwrap();
    fs::write(run.clone.join("worker.txt"), "worker\n").unwrap();
    let error = repository.create_clone(&started, None).unwrap_err();
    assert!(
        format!("{error:#}").contains("is there already"),
        "{error:#}"
    );
    assert!(run.clone.join("worker.txt").exists());
}

/// Where [`remove_unguarded_child_process`] reads what to remove.
const REMOVE_UNGUARDED: &str = "DAGQ_TEST_REMOVE_UNGUARDED";

/// The line [`remove_unguarded_child_process`] writes to the git log
/// before it removes anything.
const REMOVAL_STARTS: &str = "--- removal";

/// Not a test of its own: the control side of
/// [`what_needs_no_git_is_removed_when_the_guard_does_not_hold`], whose git
/// is the test's, which logs every git it runs: it removes the run's
/// landing worktree and then what the run left, and prints the results.
#[test]
fn remove_unguarded_child_process() {
    let Ok(asked) = std::env::var(REMOVE_UNGUARDED) else {
        return;
    };
    let [db, repo, run, log]: [&str; 4] = asked.split('\n').collect::<Vec<_>>().try_into().unwrap();
    let run = SqliteQueue::open(Path::new(db))
        .unwrap()
        .run(&dagq::domain::RunId::new(run).unwrap())
        .unwrap();
    let repository = GitRepository::inspect(Path::new(repo)).unwrap();
    let mut file = fs::OpenOptions::new().append(true).open(log).unwrap();
    std::io::Write::write_all(&mut file, format!("{REMOVAL_STARTS}\n").as_bytes()).unwrap();
    let landing = repository.remove_landing_worktree(&run);
    let removed = run_git::remove(&repository, &run, run.branch().unwrap());
    for (name, result) in [("landing", landing), ("remove", removed)] {
        match result {
            Ok(()) => println!("{name}: ok"),
            Err(error) => println!("{name}: {error:#}"),
        }
    }
}

/// When the guard of the control side's git does not hold, the clone, the
/// landing worktree's directory and the bundles and state file of the run's
/// directory are still removed as files, following no symlink, while the
/// run branch and the landing worktree's registration are left for a
/// later cleanup with no git run on them unguarded, and the removal says
/// so; once the guard holds, they go through the guarded git.
#[test]
fn what_needs_no_git_is_removed_when_the_guard_does_not_hold() {
    let (fixture, repo, db) = fixture();
    let dir = fixture.dir.path().to_path_buf();
    let kept = dir.join("host-dir");
    fs::create_dir(&kept).unwrap();
    fs::write(kept.join("keep.txt"), "keep\n").unwrap();
    let mut run = claim_in_container(&db, &repo, TaskId::new(1));
    let head = commit(&run, &[("feature.txt", "x\n")], "feature");
    turn_end(&run);
    write_receipt(&run, &head);
    validate(&db, &repo, &mut run);
    let tree = start_in_tree(&repo, &run, None);
    symlink(&kept, run.clone.join("into-host")).unwrap();
    symlink(&kept, run.run_dir.join(run_clone::MAIN_BUNDLE_FILE)).unwrap();
    // A git that reads the guarded configuration back without the guard's
    // overrides, so that the guard does not hold, and logs every command
    // with whether the guard's environment was given to it.
    let bin = dir.join("bin");
    fs::create_dir(&bin).unwrap();
    let log = dir.join("git.log");
    fs::write(&log, "").unwrap();
    let real = dagq::infrastructure::git_binary::git_executable().unwrap();
    common::template::script(
        &bin.join("git"),
        format!(
            "#!/bin/sh\necho \"${{GIT_CONFIG_COUNT:-unguarded}} $*\" >> {log}\n\
             case \" $* \" in *' config --list -z '*) [ -n \"$GIT_CONFIG_COUNT\" ] && exit 0 ;; esac\n\
             exec {real} \"$@\"\n",
            log = common::shell_path(&log),
            real = common::shell_path(&real),
        ),
    );
    let output = Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "runtime_clone::remove_unguarded_child_process",
            "--nocapture",
            "--test-threads=1",
        ])
        // An inherited guard would hide an unguarded git from the log.
        .env_remove("GIT_CONFIG_COUNT")
        .env(
            REMOVE_UNGUARDED,
            format!(
                "{}\n{}\n{}\n{}",
                db.display(),
                repo.display(),
                run.run.id(),
                log.display()
            ),
        )
        .env(
            "PATH",
            format!("{}:{}", bin.display(), std::env::var("PATH").unwrap()),
        )
        .bounded_output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    let result = |name: &str| -> String {
        stdout
            .lines()
            .find_map(|line| line.split_once(&format!("{name}: ")).map(|(_, why)| why))
            .unwrap_or_else(|| panic!("{stdout}{}", String::from_utf8_lossy(&output.stderr)))
            .to_owned()
    };
    for name in ["landing", "remove"] {
        let why = result(name);
        assert!(why.contains("left for a later cleanup"), "{name}: {why}");
        assert!(why.contains("does not hold"), "{name}: {why}");
    }
    assert!(result("remove").contains(&run.branch()));
    // The files went, and nothing a symlink pointed at.
    assert!(fs::symlink_metadata(&run.clone).is_err());
    assert!(fs::symlink_metadata(&tree).is_err());
    for name in [
        run_clone::RUN_STATE_FILE,
        run_clone::MAIN_BUNDLE_FILE,
        run_clone::RUN_BUNDLE_FILE,
    ] {
        assert!(
            fs::symlink_metadata(run.run_dir.join(name)).is_err(),
            "{name}"
        );
    }
    assert_eq!(fs::read_to_string(kept.join("keep.txt")).unwrap(), "keep\n");
    // No git ran unguarded but the guard's own reading of the names.
    let logged = fs::read_to_string(&log).unwrap();
    let (_, removal) = logged.split_once(REMOVAL_STARTS).unwrap();
    for line in removal.lines().filter(|line| !line.is_empty()) {
        assert!(
            !line.starts_with("unguarded") || line.contains("config --list --name-only"),
            "git ran unguarded: {line}\n{logged}"
        );
        assert!(
            !line.contains(" worktree ") && !line.contains(" branch "),
            "git ran on what it leaves: {line}"
        );
    }
    // The run branch and the registration are left ...
    assert!(!git_out(&repo, &["branch", "--list", &run.branch()]).is_empty());
    let registered = || {
        git_out(&repo, &["worktree", "list", "--porcelain"])
            .lines()
            .any(|line| line.starts_with("worktree ") && line.ends_with(run.run.id().as_str()))
    };
    assert!(registered());
    // ... until a cleanup with the guard in place removes them.
    let repository = GitRepository::inspect(&repo).unwrap();
    run_git::remove(&repository, &run.run, &run.branch()).unwrap();
    assert!(git_out(&repo, &["branch", "--list", &run.branch()]).is_empty());
    assert!(!registered());
}
