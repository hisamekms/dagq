//! A provider's executable kept as its link and found again by its name
//! when the path given is gone (ADR-t2079-1): Claude Code's update removes
//! the version (`versions/<v>`) a link pointed to. The supervisor's entry
//! (`dagq supervise` as a process) keeps a link and finds a gone path
//! again on PATH; a runner's turn and a runtime planner's turn, real
//! wrapper processes, start with the `claude` on their PATH once the
//! version they were given is gone, and record it. The resolution itself
//! (links, cmux's shims, the version read through a link) and the
//! executor's single retry are unit tests of `infrastructure::adapters`,
//! `infrastructure::codex` and `application::actor_executor`.
use crate::common;
use crate::plan_review::{PlanWorkspace, StubReviewer, add, submit, supervise_with};
use crate::planner_headless::{headless_fixture, queue_events};
use crate::runtime_background_process::{background_fixture, supervise_real};
use crate::runtime_support;

use common::cli::{invoke, ok};
use common::{Bounded, WithoutActor, service::OwnedByTest};
use dagq::domain::{Priority, ProposalStatus, TaskId, worker::ProviderCheck};
use dagq::infrastructure::git_binary::git_executable;
use runtime_support::headless::{FINISH, detail};
use runtime_support::*;

/// This process's PATH without the directories that hold a `claude`, so
/// that nothing the test starts finds the host's Claude Code.
fn path_without_claude() -> String {
    let path = std::env::var_os("PATH").unwrap_or_default();
    let dirs: Vec<PathBuf> = std::env::split_paths(&path)
        .filter(|dir| !dir.join("claude").exists())
        .collect();
    std::env::join_paths(dirs).unwrap().into_string().unwrap()
}

/// `bin` first, then [`path_without_claude`].
fn path_with(bin: &Path) -> String {
    format!("{}:{}", bin.display(), path_without_claude())
}

/// `bin/claude`, a link to `target` as `~/.local/bin/claude` is to the
/// version Claude Code installed.
fn claude_link(bin: &Path, target: &Path) -> PathBuf {
    fs::create_dir_all(bin).unwrap();
    let link = bin.join("claude");
    std::os::unix::fs::symlink(target, &link).unwrap();
    link
}

/// Acceptance (2): the supervisor given Claude Code as its link (to the
/// version its update installed) gives its runner the link as `--claude`,
/// never the version, and the run lands.
#[test]
fn the_supervisor_gives_its_runner_the_claude_link() {
    let (dir, repo, db, backend) = runtime_support::headless::headless_fixture(&[]);
    set_turns(dir.path(), FINISH);
    let version = dir.path().join("share/claude/versions/2.2.0");
    fs::create_dir_all(version.parent().unwrap()).unwrap();
    fs::copy(claude_stub(&db), &version).unwrap();
    let link = claude_link(&dir.path().join("bin"), &version);
    let reviewer = TestReviewer::new(&[verdict("pass", &[], "fine")]);
    let outcome = runtime::supervise_with_reviewer(
        &db,
        &repo,
        &backend,
        &link,
        &reviewer,
        Path::new(env!("CARGO_BIN_EXE_dagq")),
        &supervise_options(4, true),
    )
    .unwrap();
    backend.join();
    assert_eq!(outcome["runs"][0]["status"], "integrated", "{outcome}");
    let launched = backend.launched.lock().unwrap().clone();
    assert_eq!(launched.len(), 1, "{launched:?}");
    let command = &launched[0].command;
    assert!(
        command.contains(&format!("'--claude' '{}'", link.display())),
        "{command}"
    );
    assert!(!command.contains("versions/"), "{command}");
}

/// Acceptance (2): the supervisor given Claude Code as its link gives the
/// planner it opens for a revise the link as `--claude`, never the
/// version (the planner's wrapper is parked: only its command matters).
#[test]
fn the_supervisor_gives_its_planner_the_claude_link() {
    let mut fx = crate::plan_review::fixture();
    let queue_dir = fx.db.parent().unwrap().to_owned();
    let version = queue_dir.join("share/claude/versions/2.2.0");
    fs::create_dir_all(version.parent().unwrap()).unwrap();
    fs::copy(&fx.claude, &version).unwrap();
    fx.claude = claude_link(&queue_dir.join("bin"), &version);
    let mut queue = SqliteQueue::open(&fx.db).unwrap();
    let task = add(&mut queue, "change", &[TaskId::new(1)], Priority::Normal);
    submit(&mut queue, &[task], None);
    let reviewer = StubReviewer::new(&[json!(
        {"verdict": "revise", "reasons": ["split it"], "summary": "not yet"}
    )]);
    let backend = PlanWorkspace::default();
    let settings = crate::plan_review::options(1, Duration::from_secs(3600));
    let deadline = Instant::now() + Duration::from_secs(60);
    while backend.background.launched().is_empty() {
        assert!(Instant::now() < deadline, "no planner was opened");
        supervise_with(&fx, &backend, &reviewer, &settings);
    }
    let command = &backend.background.launched()[0].1;
    assert!(command.contains("'planner-session'"), "{command}");
    assert!(
        command.contains(&format!("'--claude' '{}'", fx.claude.display())),
        "{command}"
    );
    assert!(!command.contains("versions/"), "{command}");
}

/// Acceptance (3): a runner given a version of Claude Code that an update
/// removed after its first turn starts its next turn (the review's
/// revise) with the `claude` on its PATH, records that it did, and the
/// run lands with neither a failed start nor a switch of provider.
#[test]
fn a_runner_whose_claude_version_is_gone_starts_its_next_turn_with_the_claude_on_path() {
    let version_turns = "versions/2.0.0";
    let (dir, repo, db, guard, _leftovers) = background_fixture("");
    let version = dir.path().join(version_turns);
    fs::create_dir_all(version.parent().unwrap()).unwrap();
    fs::copy(&guard, &version).unwrap();
    // The version an update installs in place of it.
    let update = dir.path().join("versions/2.0.1");
    fs::copy(&guard, &update).unwrap();
    let bin = dir.path().join("path-bin");
    let link = claude_link(&bin, &update);
    set_turns(
        dir.path(),
        &format!(
            r#"case "$TURN" in
1) rm -f {}; {FINISH} ;;
*) {FINISH} ;;
esac"#,
            common::shell_path(&version)
        ),
    );
    // The runner's PATH, as `[run.env]` gives it to the wrapper.
    fs::write(
        repo.join("dagq.toml"),
        format!("[run.env]\nPATH = \"{}\"\n", path_with(&bin)),
    )
    .unwrap();
    git_out(&repo, &["add", "dagq.toml"]);
    git_out(&repo, &["commit", "-qm", "the runner's PATH"]);
    let base = git_out(&repo, &["rev-parse", "main"]);
    let reviewer = Arc::new(TestReviewer::new(&[
        verdict("revise", &["once more"], "again"),
        verdict("pass", &[], "fine"),
    ]));
    let supervisor = supervise_real(&db, &repo, &version, reviewer, Default::default());
    let outcome = joined(supervisor, "the supervisor thread to return").unwrap();
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    let detail = detail(&db);
    let run = &detail.runs[0];
    assert_landed_run(run, &repo, &base);
    assert_eq!(stub_calls(run).len(), 2, "{:?}", stub_calls(run));
    let relocated = payloads(&detail, "provider_executable_relocated");
    assert_eq!(relocated.len(), 1, "{relocated:?}");
    assert_eq!(relocated[0]["from"], json!(version.to_str().unwrap()));
    assert_eq!(relocated[0]["to"], json!(link.to_str().unwrap()));
    assert_eq!(
        relocated[0]["actor_id"],
        json!(format!("worker:{}", run.id()))
    );
    assert!(payloads(&detail, "provider_switched").is_empty());
    let failed: Vec<_> = payloads(&detail, "turn_finished")
        .into_iter()
        .filter(|turn| turn["failure"] == "launch")
        .collect();
    assert!(failed.is_empty(), "{failed:?}");
}

/// Acceptance (3) on a runtime planner: its wrapper, given a version of
/// Claude Code that is gone, starts its turn with the `claude` on its
/// PATH, records it on the queue with the planner as the actor, and the
/// planner submits.
#[test]
fn a_planner_whose_claude_is_gone_starts_with_the_claude_on_path() {
    planner_with_claude_gone(true);
}

/// Acceptance (3): with no `claude` on its PATH either, the planner's turn
/// fails to start as before (`launch`) and nothing is recorded as found
/// again.
#[test]
fn a_planner_whose_claude_is_gone_and_not_on_path_fails_to_start_as_before() {
    planner_with_claude_gone(false);
}

/// A runtime planner opened for a revise whose version of Claude Code is
/// removed as its wrapper starts, with `claude` on the wrapper's PATH when
/// `found`; until the planner submitted, or its turn failed to start.
fn planner_with_claude_gone(found: bool) {
    let mut fx = headless_fixture(
        "\"$DAGQ\" --db \"$DB\" submit --proposal 1 >> \"$RUN_DIR/submit.log\" 2>&1; say \"turn $TURN submitted\"",
    );
    let queue_dir = fx.db.parent().unwrap().to_owned();
    // The supervisor starts with this version, which is removed as the
    // planner's wrapper starts and put back for each later pass; the
    // update installs 2.1.1, which `bin/claude` points to.
    let version = queue_dir.join("versions/2.1.0");
    fs::create_dir_all(version.parent().unwrap()).unwrap();
    let installed = fs::read(&fx.claude).unwrap();
    let update = queue_dir.join("versions/2.1.1");
    fs::copy(&fx.claude, &update).unwrap();
    let bin = queue_dir.join("path-bin");
    let link = claude_link(&bin, &update);
    let install = || {
        fs::write(&version, &installed).unwrap();
        fs::set_permissions(
            &version,
            std::os::unix::fs::PermissionsExt::from_mode(0o755),
        )
        .unwrap();
    };
    install();
    fx.claude = version.clone();
    let mut queue = SqliteQueue::open(&fx.db).unwrap();
    let task = add(&mut queue, "change", &[TaskId::new(1)], Priority::Normal);
    let proposal = submit(&mut queue, &[task], None);
    let reviewer = StubReviewer::new(&[
        json!({"verdict": "revise", "reasons": ["split it"], "summary": "not yet"}),
        json!({"verdict": "pass", "reasons": [], "summary": "ok"}),
    ]);
    let mut backend = PlanWorkspace::running();
    backend.background.path = Some(if found {
        path_with(&bin)
    } else {
        path_without_claude()
    });
    backend.background.remove_before_launch = Some(version.clone());
    let settings = crate::plan_review::options(1, Duration::from_secs(3600));
    let deadline = Instant::now() + Duration::from_secs(120);
    let done = || {
        if found {
            !queue_events(&fx.db, "planner_closed").is_empty()
                && SqliteQueue::open(&fx.db)
                    .unwrap()
                    .show_proposal(proposal)
                    .unwrap()
                    .status()
                    == ProposalStatus::Accepted
        } else {
            queue_events(&fx.db, "turn_finished")
                .iter()
                .any(|turn| turn["failure"] == "launch")
        }
    };
    loop {
        assert!(
            Instant::now() < deadline,
            "found {found}: {:?}\n{}",
            queue_events(&fx.db, "turn_finished"),
            crate::planner_headless::diagnose_planner(&fx.db)
        );
        // Not before the planner's turn started, which must find the
        // version gone.
        let launched = !backend.background.launched().is_empty();
        if launched && queue_events(&fx.db, "turn_started").is_empty() {
            thread::sleep(Duration::from_millis(50));
            continue;
        }
        install();
        supervise_with(&fx, &backend, &reviewer, &settings);
        if done() {
            break;
        }
        thread::sleep(Duration::from_millis(50));
    }
    let relocated = queue_events(&fx.db, "provider_executable_relocated");
    if found {
        assert_eq!(
            relocated.len(),
            1,
            "{relocated:?}\n{:?}\n{}",
            queue_events(&fx.db, "turn_started"),
            crate::planner_headless::diagnose_planner(&fx.db)
        );
        assert_eq!(relocated[0]["from"], json!(version.to_str().unwrap()));
        assert_eq!(relocated[0]["to"], json!(link.to_str().unwrap()));
        assert_eq!(relocated[0]["role"], "planner");
        assert_eq!(relocated[0]["actor_id"], "planner:1");
    } else {
        assert!(relocated.is_empty(), "{relocated:?}");
    }
}

/// A repository with one commit, a queue, and stubs of `cmux` and of a
/// Claude Code installed as Claude Code installs itself: `bin/claude`, a
/// link to `share/claude/versions/2.2.0`. The path of a version an update
/// removed, `share/claude/versions/2.1.0`, is not there.
struct Entry {
    dir: tempfile::TempDir,
    db: PathBuf,
    repo: PathBuf,
    cmux: PathBuf,
    bin: PathBuf,
}

impl Entry {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("queue").join("queue.db");
        ok(&db, &["init"]);
        let repo = dir.path().join("repo");
        fs::create_dir(&repo).unwrap();
        for args in [
            &["init", "-q", "-b", "main"][..],
            &[
                "-c",
                "user.name=t",
                "-c",
                "user.email=t@example.invalid",
                "commit",
                "-q",
                "--allow-empty",
                "-m",
                "seed",
            ],
        ] {
            assert!(
                Command::new(git_executable().expect("git executable"))
                    .arg("-C")
                    .arg(&repo)
                    .args(args)
                    .bounded_status()
                    .unwrap()
                    .success()
            );
        }
        let cmux = dir.path().join("cmux");
        common::template::script(&cmux, "#!/bin/sh\nprintf 'PONG\\n'\n");
        let version = dir.path().join("share/claude/versions/2.2.0");
        fs::create_dir_all(version.parent().unwrap()).unwrap();
        // Every call says how it was started (`$0`) and its first argument.
        common::template::script(
            &version,
            format!(
                "#!/bin/sh\nprintf '%s %s\\n' \"$0\" \"$1\" >> {}\nprintf '2.2.0 (Claude Code)\\n'\n",
                common::shell_path(dir.path().join("claude-calls"))
            ),
        );
        let bin = dir.path().join("bin");
        claude_link(&bin, &version);
        Self {
            dir,
            db,
            repo,
            cmux,
            bin,
        }
    }

    fn gone(&self) -> PathBuf {
        self.dir.path().join("share/claude/versions/2.1.0")
    }

    /// `dagq supervise` with `claude` (and `extra`) on `path`, its stderr
    /// in `log`.
    fn supervise(&self, claude: &Path, extra: &[&str], path: &str, log: &Path) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_dagq"));
        command
            .without_actor_env()
            .owned_by_test()
            .env("PATH", path)
            .arg("--db")
            .arg(&self.db)
            .arg("supervise")
            .args(if extra.contains(&"--observe-interval") {
                &[][..]
            } else {
                &["--observe-interval", "0"][..]
            })
            .arg("--repo")
            .arg(&self.repo)
            .arg("--cmux")
            .arg(&self.cmux)
            .arg("--claude")
            .arg(claude)
            .args(extra)
            .args(["--tick-ms", "100", "--idle-poll-ms", "100"])
            .stdout(std::process::Stdio::null())
            .stderr(fs::File::create(log).unwrap());
        command
    }

    /// The calls of Claude Code (`bin/claude`'s version): how each was
    /// started and its first argument.
    fn claude_calls(&self) -> Vec<String> {
        fs::read_to_string(self.dir.path().join("claude-calls"))
            .unwrap_or_default()
            .lines()
            .map(str::to_owned)
            .collect()
    }

    /// Run the supervisor until it registered and `until` holds, then stop
    /// it; its registration's Claude Code and what it logged.
    fn registered_claude(
        &self,
        claude: &Path,
        extra: &[&str],
        path: &str,
        until: impl Fn() -> bool,
    ) -> (ProviderCheck, String) {
        let log = self.dir.path().join("supervise.log");
        let mut supervisor = common::KillOnDrop::new(
            self.supervise(claude, extra, path, &log).spawn().unwrap(),
            "the supervisor",
        );
        let registered = || SqliteQueue::open(&self.db).unwrap().supervisors().unwrap();
        let started = Instant::now();
        while registered().is_empty() || !until() {
            assert!(
                supervisor.child().try_wait().unwrap().is_none(),
                "the supervisor exited: {}",
                fs::read_to_string(&log).unwrap()
            );
            assert!(
                started.elapsed().as_secs() < 30,
                "the supervisor never registered, or what it waits for never came: {:?}",
                self.claude_calls()
            );
            thread::sleep(Duration::from_millis(100));
        }
        let claude = registered()[0]
            .providers
            .clone()
            .unwrap()
            .into_iter()
            .find(|check| check.provider == dagq::domain::Provider::Claude)
            .unwrap();
        unsafe { libc::kill(supervisor.child().id() as i32, libc::SIGINT) };
        let exit = {
            let _waiting = common::within(common::STEP_LIMIT, "the supervisor to drain on SIGINT");
            supervisor.child().wait().unwrap()
        };
        assert!(exit.success());
        (claude, fs::read_to_string(&log).unwrap())
    }
}

/// Acceptance (2) and (4): `dagq supervise` keeps the link it is given as
/// the Claude Code its runners, planners and jobs are given, never the
/// version it points at; given a version an update removed (an argv a
/// binary before registered), it finds `claude` on PATH, starts and logs
/// that it did; with none on PATH it fails at its entry as before; with
/// `--no-claude` it resolves nothing.
#[test]
fn the_supervisors_entry_keeps_a_link_and_finds_a_gone_claude_again_on_path() {
    let entry = Entry::new();
    let link = entry.bin.join("claude");
    let on_path = path_with(&entry.bin);

    // With the observer due at once, the job the supervisor starts gets
    // the link as `--claude` and starts its agent by it.
    common::service::serve(&entry.db);
    let agent_called = || {
        entry
            .claude_calls()
            .iter()
            .any(|call| !call.ends_with(" --version"))
    };
    let (claude, _) = entry.registered_claude(
        &link,
        &["--observe-interval", "3600"],
        &on_path,
        agent_called,
    );
    assert_eq!(claude.executable, link.to_str().unwrap());
    assert!(claude.found);
    assert!(!claude.executable.contains("versions"), "{claude:?}");
    let calls = entry.claude_calls();
    assert!(
        calls
            .iter()
            .all(|call| call.starts_with(&format!("{} ", link.display()))),
        "{calls:?}"
    );

    let gone = entry.gone();
    let (claude, log) = entry.registered_claude(&gone, &[], &on_path, || true);
    assert_eq!(claude.executable, link.to_str().unwrap());
    assert!(claude.found);
    assert!(
        log.contains(&format!(
            "{} is gone; claude found again on PATH at {}",
            gone.display(),
            link.display()
        )),
        "{log}"
    );

    let log = entry.dir.path().join("refused.log");
    let output = {
        let _waiting = common::within(common::STEP_LIMIT, "the supervisor to refuse");
        entry
            .supervise(&gone, &[], &path_without_claude(), &log)
            .status()
            .unwrap()
    };
    assert!(!output.success());
    let refused = fs::read_to_string(&log).unwrap();
    assert!(refused.contains("2.1.0"), "{refused}");
    assert!(
        SqliteQueue::open(&entry.db)
            .unwrap()
            .supervisors()
            .unwrap()
            .is_empty()
    );

    let (claude, log) = entry.registered_claude(&gone, &["--no-claude"], &on_path, || true);
    assert_eq!(claude.executable, gone.to_str().unwrap());
    assert!(!log.contains("found again"), "{log}");
    // `invoke` reads the queue as the person does: the supervisors are gone.
    assert!(invoke(&entry.db, &["status"]).status.success());
}
