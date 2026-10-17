//! `setup-review`: a fake coding agent writes the review's settings in the
//! temporary checkout, and they reach the main checkout's working tree only
//! when approved; nothing is committed, the queue does not change, and the
//! temporary checkout is gone either way. A generation that changes
//! another file is refused, and so is one over a file with changes not
//! committed. `init` recommends the settings and writes no `dagq.toml`, and
//! only a person and the inbox run `setup-review`.

use crate::common;

use common::{Bounded, KillOnDrop, STEP_LIMIT, WithoutActor, shell_path, within};

use dagq::infrastructure::git_binary::git_executable;
use serde_json::Value;
use std::{
    fs,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    process::{Command, Output},
};
use tempfile::TempDir;

const CONFIG: &str = "[run.env]\nA = \"1\"\n";

fn git(repo: &Path, args: &[&str]) -> String {
    let result = Command::new(git_executable().unwrap())
        .arg("-C")
        .arg(repo)
        .args(args)
        .bounded_output()
        .unwrap();
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    String::from_utf8(result.stdout).unwrap()
}

/// A repository whose one commit has `dagq.toml` ([`CONFIG`]) and a
/// README, and a queue for it beside it, initialized.
struct Fixture {
    dir: TempDir,
    repo: PathBuf,
    db: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path().join("repo");
        fs::create_dir(&repo).unwrap();
        git(&repo, &["init", "-q", "-b", "main"]);
        git(&repo, &["config", "user.name", "test"]);
        git(&repo, &["config", "user.email", "test@example.invalid"]);
        fs::write(repo.join("dagq.toml"), CONFIG).unwrap();
        fs::write(repo.join("README.md"), "# A project\n").unwrap();
        git(&repo, &["add", "."]);
        git(&repo, &["commit", "-q", "-m", "seed"]);
        let db = dir.path().join("queue").join("queue.db");
        let fixture = Self { dir, repo, db };
        fixture.ok(&[], &["init"]);
        fixture
    }

    /// `dagq --db <queue> args` in the repository as `env`'s actor (the
    /// user without).
    fn command(&self, env: &[(&str, &str)], args: &[&str]) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_dagq"));
        command
            .without_actor_env()
            .current_dir(&self.repo)
            .env("XDG_CONFIG_HOME", self.dir.path().join("config"))
            .envs(env.iter().copied())
            .arg("--db")
            .arg(&self.db)
            .args(args);
        command
    }

    /// Run [`Self::command`], its stdin no terminal.
    fn invoke(&self, env: &[(&str, &str)], args: &[&str]) -> Output {
        self.command(env, args).bounded_output().unwrap()
    }

    fn ok(&self, env: &[(&str, &str)], args: &[&str]) -> Value {
        let output = self.invoke(env, args);
        assert!(
            output.status.success(),
            "{args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        serde_json::from_slice(&output.stdout).unwrap()
    }

    fn error(&self, env: &[(&str, &str)], args: &[&str]) -> Value {
        let output = self.invoke(env, args);
        assert!(!output.status.success(), "{env:?} {args:?} succeeded");
        serde_json::from_slice(&output.stderr).unwrap()
    }

    /// A fake `claude` that runs `body` in its working directory and
    /// answers as a headless Claude job does.
    fn agent(&self, name: &str, body: &str) -> String {
        let path = self.dir.path().join(name);
        fs::write(
            &path,
            format!(
                "#!/bin/sh\ncat >/dev/null\n{body}\necho '{{\"type\":\"result\",\"result\":\"done\"}}'\n"
            ),
        )
        .unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
        path.to_str().unwrap().to_owned()
    }

    fn events(&self) -> i64 {
        rusqlite::Connection::open(&self.db)
            .unwrap()
            .query_row("SELECT count(*) FROM run_events", [], |row| row.get(0))
            .unwrap()
    }

    fn commits(&self) -> String {
        git(&self.repo, &["rev-list", "--all"])
    }

    /// Whether the temporary checkouts are all gone: no directory under
    /// the queue's and no worktree but the main checkout.
    fn places_gone(&self) -> bool {
        let places = self.db.parent().unwrap().join("setup-review");
        let empty = fs::read_dir(&places).map_or(true, |mut dir| dir.next().is_none());
        let worktrees = git(&self.repo, &["worktree", "list", "--porcelain"]);
        empty && worktrees.matches("worktree ").count() == 1
    }
}

/// What the fake agent writes: a built-in's table added to `dagq.toml`, a
/// program's table, the rules of `builtin:tests` and the program script.
const GENERATE: &str = r#"mkdir -p .dagq/review/programs
cat >> dagq.toml <<'EOF'

[review.subagents."builtin:tests"]
enabled = true

[review.programs.check-docs]
script = ".dagq/review/programs/check-docs.sh"
paths = ["docs/**"]
EOF
printf 'Tests live in t/.\n' > .dagq/review/builtin-tests.md
printf '#!/bin/sh\nroot=$(git rev-parse --show-toplevel) || exit 2\nexit 0\n' > .dagq/review/programs/check-docs.sh
chmod 0777 .dagq/review/programs/check-docs.sh
chmod 0666 .dagq/review/builtin-tests.md"#;

#[test]
fn the_generated_settings_reach_the_working_tree_only_when_approved() {
    let fixture = Fixture::new();
    let agent = fixture.agent("claude-generate", GENERATE);
    let commits = fixture.commits();
    let events = fixture.events();

    // No terminal to ask on and no flag: shown, not applied.
    let shown = fixture.ok(&[], &["setup-review", "--claude", &agent]);
    assert_eq!(shown["approved"], false, "{shown}");
    assert_eq!(shown["applied"], serde_json::json!([]));
    assert_eq!(shown["updated"], true);
    let changed: Vec<&str> = shown["changes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|change| change["path"].as_str().unwrap())
        .collect();
    assert_eq!(
        changed,
        [
            ".dagq/review/builtin-tests.md",
            ".dagq/review/programs/check-docs.sh",
            "dagq.toml"
        ]
    );
    assert!(
        shown["diff"]
            .as_str()
            .unwrap()
            .contains("+[review.subagents.\"builtin:tests\"]")
    );
    assert_eq!(shown["checks"]["config"], "read");
    assert_eq!(git(&fixture.repo, &["status", "--porcelain"]), "");
    assert!(!fixture.repo.join(".dagq").exists());
    assert!(fixture.places_gone());

    // Approved by the flag: written to the working tree, not committed.
    let applied = fixture.ok(&[], &["setup-review", "--yes", "--claude", &agent]);
    assert_eq!(applied["approved"], true, "{applied}");
    assert_eq!(applied["applied"].as_array().unwrap().len(), 3);
    let config = fs::read_to_string(fixture.repo.join("dagq.toml")).unwrap();
    assert!(config.starts_with(CONFIG) && config.contains("[review.programs.check-docs]"));
    assert_eq!(
        fs::read_to_string(fixture.repo.join(".dagq/review/builtin-tests.md")).unwrap(),
        "Tests live in t/.\n"
    );
    // Only the executable bit of what the job wrote is taken.
    let mode = |path: &str| {
        fs::metadata(fixture.repo.join(path))
            .unwrap()
            .permissions()
            .mode()
            & 0o7777
    };
    assert_eq!(mode(".dagq/review/programs/check-docs.sh"), 0o755);
    assert_eq!(mode(".dagq/review/builtin-tests.md"), 0o644);
    assert_eq!(
        git(
            &fixture.repo,
            &["status", "--porcelain", "--untracked-files=all"]
        ),
        " M dagq.toml\n?? .dagq/review/builtin-tests.md\n?? .dagq/review/programs/check-docs.sh\n"
    );
    assert_eq!(fixture.commits(), commits);
    assert_eq!(fixture.events(), events);
    assert!(fixture.places_gone());
}

#[test]
fn a_generation_outside_the_settings_is_not_applied() {
    let fixture = Fixture::new();
    let outside = fixture.agent(
        "claude-outside",
        &format!("{GENERATE}\nprintf 'x\\n' > README.md"),
    );
    let error = fixture.error(&[], &["setup-review", "--yes", "--claude", &outside]);
    assert!(
        error["error"]
            .as_str()
            .unwrap()
            .contains("outside dagq.toml and .dagq/review/, so nothing is applied: README.md"),
        "{error}"
    );
    assert_eq!(git(&fixture.repo, &["status", "--porcelain"]), "");
    assert!(fixture.places_gone());
}

/// A job that rewrites the temporary checkout's `.git` beside the settings
/// is refused whole, also when approved beforehand: `.git` is a path
/// outside what it may change, read without being followed.
#[test]
fn a_generation_that_rewrites_git_is_not_applied() {
    let fixture = Fixture::new();
    let rewriting = fixture.agent(
        "claude-git",
        &format!("{GENERATE}\nprintf 'gitdir: /elsewhere\\n' > .git"),
    );
    let error = fixture.error(&[], &["setup-review", "--yes", "--claude", &rewriting]);
    assert!(
        error["error"]
            .as_str()
            .unwrap()
            .contains("outside dagq.toml and .dagq/review/, so nothing is applied: .git"),
        "{error}"
    );
    assert_eq!(git(&fixture.repo, &["status", "--porcelain"]), "");
    assert!(!fixture.repo.join(".dagq").exists());
    assert!(fixture.places_gone());
}

/// A link to a file outside the place is not copied in.
#[test]
fn a_generated_link_is_not_applied() {
    let fixture = Fixture::new();
    let secret = fixture.dir.path().join("secret");
    fs::write(&secret, "outside\n").unwrap();
    let linked = fixture.agent(
        "claude-link",
        &format!(
            "mkdir -p .dagq/review && ln -s {} .dagq/review/builtin-security.md",
            shell_path(&secret)
        ),
    );
    let error = fixture.error(&[], &["setup-review", "--yes", "--claude", &linked]);
    assert!(
        error["error"]
            .as_str()
            .unwrap()
            .contains("is not a regular file"),
        "{error}"
    );
    assert_eq!(git(&fixture.repo, &["status", "--porcelain"]), "");
    assert!(!fixture.repo.join(".dagq").exists());
    assert!(fixture.places_gone());
}

/// A rules file the person is writing is not overwritten.
#[test]
fn a_generation_over_uncommitted_changes_is_not_applied() {
    let fixture = Fixture::new();
    fs::create_dir_all(fixture.repo.join(".dagq/review")).unwrap();
    fs::write(fixture.repo.join(".dagq/review/builtin-tests.md"), "mine\n").unwrap();
    let generate = fixture.agent("claude-generate", GENERATE);
    let error = fixture.error(&[], &["setup-review", "--yes", "--claude", &generate]);
    assert!(
        error["error"]
            .as_str()
            .unwrap()
            .contains("changes not committed to .dagq/review/builtin-tests.md"),
        "{error}"
    );
    assert_eq!(
        fs::read_to_string(fixture.repo.join(".dagq/review/builtin-tests.md")).unwrap(),
        "mine\n"
    );
    assert_eq!(
        fs::read_to_string(fixture.repo.join("dagq.toml")).unwrap(),
        CONFIG
    );
    assert!(fixture.places_gone());
}

#[test]
fn init_recommends_the_settings_and_only_a_person_and_the_inbox_set_up_the_review() {
    let fixture = Fixture::new();
    git(&fixture.repo, &["rm", "-q", "dagq.toml"]);
    git(&fixture.repo, &["commit", "-q", "-m", "no settings"]);
    let init = fixture.ok(&[], &["init"]);
    assert_eq!(init["recommended"]["setup"], "dagq setup-review", "{init}");
    assert_eq!(
        init["recommended"]["builtins"][2],
        serde_json::json!({"name": "builtin:tests", "enabled_by_default": false})
    );
    assert!(!fixture.repo.join("dagq.toml").exists());

    let agent = fixture.agent("claude-generate", GENERATE);
    for env in [
        vec![("DAGQ_ROLE", "planner"), ("DAGQ_ACTOR_ID", "planner:1")],
        vec![
            ("DAGQ_ROLE", "worker"),
            ("DAGQ_ACTOR_ID", "worker:r1"),
            ("DAGQ_RUN_ID", "r1"),
            ("DAGQ_TASK_ID", "1"),
        ],
        vec![("DAGQ_ROLE", "review-job")],
        vec![("DAGQ_ROLE", "setup-review-job")],
    ] {
        let error = fixture.error(&env, &["setup-review", "--claude", &agent]);
        assert_eq!(error["denied"]["capability"], "review.setup", "{env:?}");
        assert_eq!(error["denied"]["reason"], "not granted", "{env:?}");
    }
    // Approving beforehand is a person's own: the inbox is refused it.
    let error = fixture.error(
        &[("DAGQ_ROLE", "inbox"), ("DAGQ_ACTOR_ID", "inbox")],
        &["setup-review", "--yes", "--claude", &agent],
    );
    assert_eq!(error["denied"]["capability"], "review.setup_approve");
    assert_eq!(error["denied"]["reason"], "not granted");
    assert!(!fixture.repo.join(".dagq").exists());
    let inbox = fixture.ok(
        &[("DAGQ_ROLE", "inbox"), ("DAGQ_ACTOR_ID", "inbox")],
        &["setup-review", "--claude", &agent],
    );
    assert_eq!(inbox["approved"], false, "{inbox}");
    assert_eq!(inbox["updated"], false);
}

/// A repository that ignores `scratch/` and `.dagq/`.
fn ignoring() -> Fixture {
    let fixture = Fixture::new();
    fs::write(fixture.repo.join(".gitignore"), "scratch/\n.dagq/\n").unwrap();
    git(&fixture.repo, &["add", ".gitignore"]);
    git(&fixture.repo, &["commit", "-q", "-m", "ignore"]);
    fixture
}

/// What the job writes is found even where the repository ignores it: an
/// ignored file outside the settings is refused.
#[test]
fn an_ignored_file_outside_the_settings_is_refused() {
    let fixture = ignoring();
    let scratch = fixture.agent(
        "claude-scratch",
        &format!("{GENERATE}\nmkdir -p scratch && printf 'x\\n' > scratch/x"),
    );
    let error = fixture.error(&[], &["setup-review", "--yes", "--claude", &scratch]);
    assert!(
        error["error"]
            .as_str()
            .unwrap()
            .contains("so nothing is applied: scratch/x"),
        "{error}"
    );
    assert!(!fixture.repo.join(".dagq").exists());
    assert!(fixture.places_gone());
}

/// Settings under an ignored `.dagq/` are changes, and applied, also when
/// that ignored directory holds other files of the checkout already: only
/// the paths written themselves are checked for changes not committed.
#[test]
fn ignored_settings_are_applied() {
    let fixture = ignoring();
    fs::create_dir(fixture.repo.join(".dagq")).unwrap();
    fs::write(fixture.repo.join(".dagq/notes.txt"), "mine\n").unwrap();
    let generate = fixture.agent("claude-generate", GENERATE);
    let applied = fixture.ok(&[], &["setup-review", "--yes", "--claude", &generate]);
    assert_eq!(applied["applied"].as_array().unwrap().len(), 3, "{applied}");
    assert_eq!(
        fs::read_to_string(fixture.repo.join(".dagq/review/builtin-tests.md")).unwrap(),
        "Tests live in t/.\n"
    );
    assert_eq!(
        fs::read_to_string(fixture.repo.join(".dagq/notes.txt")).unwrap(),
        "mine\n"
    );
    assert!(fixture.places_gone());
}

/// A file the checkout already has at a target, ignored and untracked,
/// stops the apply and is not overwritten (a tracked file's change is
/// `a_hidden_or_filtered_change_of_a_tracked_target_is_not_overwritten`'s).
#[test]
fn an_ignored_file_at_a_target_is_not_overwritten() {
    let fixture = ignoring();
    fs::create_dir_all(fixture.repo.join(".dagq/review")).unwrap();
    fs::write(fixture.repo.join(".dagq/review/builtin-tests.md"), "mine\n").unwrap();
    let generate = fixture.agent("claude-generate", GENERATE);
    let error = fixture.error(&[], &["setup-review", "--yes", "--claude", &generate]);
    assert!(
        error["error"]
            .as_str()
            .unwrap()
            .contains("changes not committed to .dagq/review/builtin-tests.md"),
        "{error}"
    );
    assert_eq!(
        fs::read_to_string(fixture.repo.join(".dagq/review/builtin-tests.md")).unwrap(),
        "mine\n"
    );
    assert_eq!(
        fs::read_to_string(fixture.repo.join("dagq.toml")).unwrap(),
        CONFIG
    );
    assert!(fixture.places_gone());
}

/// A tracked target is checked from its bytes, not by Git's status: an
/// edit the index's skip-worktree flag hides is not overwritten, and the
/// clean filter the repository names for it does not run on that edited
/// file (of the committed size, which Git's status would hash). The other
/// index flags and kinds of change are the unit tests'
/// (`infrastructure::setup_review::dirty`).
#[test]
fn a_hidden_or_filtered_change_of_a_tracked_target_is_not_overwritten() {
    let fixture = Fixture::new();
    let cleaned = fixture.dir.path().join("cleaned");
    git(
        &fixture.repo,
        &[
            "config",
            "filter.mark.clean",
            &format!("touch {}; cat", shell_path(&cleaned)),
        ],
    );
    fs::write(
        fixture.repo.join(".gitattributes"),
        "dagq.toml filter=mark\n",
    )
    .unwrap();
    git(&fixture.repo, &["add", ".gitattributes"]);
    git(&fixture.repo, &["commit", "-q", "-m", "attributes"]);
    let generate = fixture.agent("claude-generate", GENERATE);
    let edited = "[run.env]\nA = \"2\"\n";
    git(
        &fixture.repo,
        &["update-index", "--skip-worktree", "dagq.toml"],
    );
    fs::write(fixture.repo.join("dagq.toml"), edited).unwrap();
    let _ = fs::remove_file(&cleaned);
    let error = fixture.error(&[], &["setup-review", "--yes", "--claude", &generate]);
    assert!(
        error["error"]
            .as_str()
            .unwrap()
            .contains("changes not committed to dagq.toml"),
        "{error}"
    );
    assert!(!cleaned.exists(), "the clean filter ran");
    assert_eq!(
        fs::read_to_string(fixture.repo.join("dagq.toml")).unwrap(),
        edited
    );
    assert!(!fixture.repo.join(".dagq").exists());
    assert!(fixture.places_gone());
}

/// No filter or hook of the repository runs, neither when the temporary
/// checkout is made nor on what the job wrote, and the diff the person
/// sees is of the bytes that would be written: a smudge filter
/// `.gitattributes` names for `*.md` would have left a mark as the place
/// was checked out, a clean filter would have upper-cased the rules, and
/// the hooks of a checkout and of a ref's update would have left theirs.
/// The place still has the commit's files as committed: an executable
/// script and a link.
#[test]
fn the_repositorys_filters_and_hooks_do_not_run() {
    let fixture = Fixture::new();
    let (cleaned, smudged, hooked) = (
        fixture.dir.path().join("cleaned"),
        fixture.dir.path().join("smudged"),
        fixture.dir.path().join("hooked"),
    );
    git(
        &fixture.repo,
        &[
            "config",
            "filter.shout.clean",
            &format!("touch {}; tr a-z A-Z", shell_path(&cleaned)),
        ],
    );
    git(
        &fixture.repo,
        &[
            "config",
            "filter.shout.smudge",
            &format!("touch {}; cat", shell_path(&smudged)),
        ],
    );
    fs::write(fixture.repo.join(".gitattributes"), "*.md filter=shout\n").unwrap();
    fs::create_dir(fixture.repo.join("tools")).unwrap();
    fs::write(fixture.repo.join("tools/run.sh"), "#!/bin/sh\n").unwrap();
    fs::set_permissions(
        fixture.repo.join("tools/run.sh"),
        fs::Permissions::from_mode(0o755),
    )
    .unwrap();
    std::os::unix::fs::symlink("README.md", fixture.repo.join("link.md")).unwrap();
    git(
        &fixture.repo,
        &["add", ".gitattributes", "tools/run.sh", "link.md"],
    );
    git(&fixture.repo, &["commit", "-q", "-m", "attributes"]);
    let hooks = fixture.repo.join(".git/hooks");
    for hook in ["post-checkout", "reference-transaction"] {
        fs::write(
            hooks.join(hook),
            format!("#!/bin/sh\ntouch {}\n", shell_path(&hooked)),
        )
        .unwrap();
        fs::set_permissions(hooks.join(hook), fs::Permissions::from_mode(0o755)).unwrap();
    }
    for mark in [&cleaned, &smudged, &hooked] {
        let _ = fs::remove_file(mark);
    }
    let generate = fixture.agent(
        "claude-generate",
        &format!(
            "{GENERATE}\ntest -x tools/run.sh && test -L link.md && cp README.md .dagq/review/builtin-security.md"
        ),
    );
    let shown = fixture.ok(&[], &["setup-review", "--claude", &generate]);
    let diff = shown["diff"].as_str().unwrap();
    assert!(diff.contains("+Tests live in t/.\n"), "{diff}");
    assert!(!diff.contains("TESTS LIVE"), "{diff}");
    assert!(diff.contains("+# A project\n"), "{diff}");
    for mark in [&cleaned, &smudged, &hooked] {
        assert!(!mark.exists(), "{} ran", mark.display());
    }
    assert!(fixture.places_gone());
    assert!(!hooked.exists(), "a hook ran as the place was removed");
}

/// A link in the checkout on the way to a target is not written through.
#[test]
fn a_linked_target_in_the_checkout_is_not_written_through() {
    let fixture = Fixture::new();
    let outside = fixture.dir.path().join("outside");
    fs::create_dir(&outside).unwrap();
    std::os::unix::fs::symlink(&outside, fixture.repo.join(".dagq")).unwrap();
    let generate = fixture.agent("claude-generate", GENERATE);
    let error = fixture.error(&[], &["setup-review", "--yes", "--claude", &generate]);
    assert!(
        error["error"]
            .as_str()
            .unwrap()
            .contains("is a symbolic link in the checkout"),
        "{error}"
    );
    assert_eq!(fs::read_dir(&outside).unwrap().count(), 0);
    assert_eq!(
        fs::read_to_string(fixture.repo.join("dagq.toml")).unwrap(),
        CONFIG
    );
    assert!(fixture.places_gone());
}

/// An interrupt while the job runs stops it, writes nothing and still
/// removes the temporary checkout.
#[test]
fn an_interrupt_stops_the_job_and_removes_the_place() {
    let fixture = Fixture::new();
    let started = fixture.dir.path().join("started");
    // The job waits to be stopped, and ends by itself once `dagq` is gone
    // or after a minute.
    let agent = fixture.agent(
        "claude-slow",
        &format!(
            "{GENERATE}\n: > {}\ni=0\nwhile [ $i -lt 600 ] && kill -0 \"$PPID\" 2>/dev/null; do sleep 0.1; i=$((i + 1)); done",
            shell_path(&started)
        ),
    );
    let mut child = KillOnDrop::new(
        fixture
            .command(&[], &["setup-review", "--yes", "--claude", &agent])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .unwrap(),
        "dagq setup-review",
    );
    {
        let _waiting = within(STEP_LIMIT, "the job to start");
        while !started.exists() {
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
    }
    let pid = i32::try_from(child.child().id()).unwrap();
    // SAFETY: signals the child this test started, by its pid.
    assert_eq!(unsafe { libc::kill(pid, libc::SIGINT) }, 0);
    let output = {
        let _waiting = within(STEP_LIMIT, "dagq setup-review to exit after the interrupt");
        child.wait_with_output().unwrap()
    };
    assert!(!output.status.success());
    let error: Value = serde_json::from_slice(&output.stderr).unwrap();
    assert!(
        error["error"].as_str().unwrap().contains("was interrupted"),
        "{error}"
    );
    assert!(!fixture.repo.join(".dagq").exists());
    assert_eq!(
        fs::read_to_string(fixture.repo.join("dagq.toml")).unwrap(),
        CONFIG
    );
    assert!(fixture.places_gone());
}
