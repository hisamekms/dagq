//! Runtime tests: the toolchain profiles a repository declares in
//! `[toolchains.<name>]` of `dagq.toml` read its verification's output
//! (ADR-t2268-1). A second profile, made up here and registered beside
//! the built-in one, classifies, names its tests and gives its retry env
//! with no other code; the runtime's own host marks hold whatever is
//! declared, and an output no declared profile reads is `unknown`.
use crate::runtime_support;

use dagq::domain::toolchain::{
    ProfileRegistry, ToolCodeClass, ToolHostClass, ToolchainProfile, rust,
};
use runtime_support::*;

/// A made-up tool: `BROKEN <test>` failed, `WOBBLY <test>` passed on its
/// retry, `HUNG <test>` ran out of time; its retry runs with
/// `MADE_FLAKY=accept`.
struct Made;

static MADE: Made = Made;

/// The built-in profiles and the made-up one.
static PROFILES: ProfileRegistry = ProfileRegistry::new(&[&rust::RUST, &MADE]);

fn named<'a>(line: &'a str, mark: &str) -> Option<&'a str> {
    line.trim().strip_prefix(mark)?.strip_prefix(' ')
}

impl ToolchainProfile for Made {
    fn name(&self) -> &'static str {
        "made-up"
    }
    fn killed_line(&self, _: &[&str]) -> Option<usize> {
        None
    }
    fn failed_tests(&self, text: &str) -> Vec<(usize, String)> {
        text.lines()
            .enumerate()
            .filter_map(|(index, line)| named(line, "BROKEN").map(|t| (index, t.to_owned())))
            .collect()
    }
    fn flaky_tests(&self, text: &str) -> Vec<String> {
        text.lines()
            .filter_map(|line| named(line, "WOBBLY").map(str::to_owned))
            .collect()
    }
    fn flaky_line(&self, lines: &[&str]) -> Option<usize> {
        lines
            .iter()
            .position(|line| named(line, "WOBBLY").is_some())
    }
    fn test_host_line(&self, lines: &[&str]) -> Option<(ToolHostClass, usize)> {
        lines
            .iter()
            .position(|line| named(line, "HUNG").is_some())
            .map(|at| (ToolHostClass::Timeout, at))
    }
    fn code_failure(&self, _: &str, lines: &[&str]) -> Option<(ToolCodeClass, String)> {
        lines
            .iter()
            .find(|line| named(line, "BROKEN").is_some())
            .map(|line| (ToolCodeClass::TestFailure, (*line).to_owned()))
    }
    fn flaky_retry_env(&self) -> &'static [(&'static str, &'static str)] {
        &[("MADE_FLAKY", "accept")]
    }
}

/// The one-shot entry points with the made-up profile registered.
fn with_made_up() -> runtime::OneShot {
    runtime::OneShot {
        profiles: &PROFILES,
        ..runtime::OneShot::system()
    }
}

/// A run awaiting integration, its repository, and the queue.
fn awaiting() -> (Fixture, PathBuf, PathBuf) {
    let (dir, db, detail) = run_agent(
        "echo a > a.txt && git add a.txt && git commit -q -m a; receipt \"$(git rev-parse HEAD)\"",
    );
    assert_eq!(detail.runs[0].status(), RunStatus::AwaitingIntegration);
    let repo = Path::new(&db).parent().unwrap().join("repo's directory");
    (dir, db, repo)
}

/// Commit `toml` as main's `dagq.toml`.
fn declare(repo: &Path, toml: &str) {
    fs::write(repo.join("dagq.toml"), toml).unwrap();
    git(repo, &["add", "dagq.toml"]);
    git(repo, &["commit", "-q", "-m", "toolchains"]);
}

const MADE_UP_ONLY: &str = "[toolchains.tool]\nprofile = \"made-up\"\npaths = [\"tool/**\"]\n";

fn set_commands(db: &Path, commands: Value) {
    Connection::open(db)
        .unwrap()
        .execute(
            "UPDATE tasks SET verification_commands=?1 WHERE id=1",
            [commands.to_string()],
        )
        .unwrap();
}

fn show(db: &Path) -> dagq::domain::TaskDetail {
    SqliteQueue::open(db).unwrap().show(TaskId::new(1)).unwrap()
}

fn integrate_with(one_shot: &runtime::OneShot, db: &Path, repo: &Path) -> Value {
    one_shot
        .integrate(db, IntegrateTarget::Task(TaskId::new(1)), repo, None)
        .unwrap()
}

/// On one repository, first without a declaration and then declaring only
/// the made-up profile: a command that exits 124, 128 + a signal, or fails
/// on a full disk is retried once and held for a person when it fails so
/// again (ADR-t639-1 decisions 1 to 4).
#[test]
fn the_host_failures_are_retried_and_held_whatever_is_declared() {
    let (_dir, db, repo) = awaiting();
    let one_shot = with_made_up();
    for declared in [None, Some(MADE_UP_ONLY)] {
        if let Some(toml) = declared {
            declare(&repo, toml);
        }
        for (command, class) in [
            ("echo boom; exit 124", "timeout"),
            ("exit 137", "killed"),
            ("echo 'write: No space left on device'; exit 1", "disk_full"),
        ] {
            set_commands(&db, json!([command]));
            let outcome = integrate_with(&one_shot, &db, &repo);
            assert_eq!(
                outcome["outcome"], "held",
                "{declared:?} {command}: {outcome}"
            );
            let detail = show(&db);
            let held = payloads(&detail, "integration_held");
            let held = held.last().unwrap();
            assert_eq!(held["code"], "verification_environment");
            assert_eq!(held["retried"], true, "{declared:?} {held}");
            assert_eq!(held["failure"]["class"], class, "{declared:?}");
            assert_eq!(held["first_failure"]["class"], class, "{declared:?}");
            assert_eq!(detail.runs[0].status(), RunStatus::AwaitingIntegration);
            assert!(payloads(&detail, "resume_started").is_empty());
        }
    }
}

/// A repository that declares only the made-up profile: cargo's marks mean
/// nothing there (`unknown`, no tests named).
#[test]
fn an_output_only_another_profile_could_read_is_unknown() {
    let (_dir, db, repo) = awaiting();
    declare(&repo, MADE_UP_ONLY);
    let one_shot = with_made_up();
    set_commands(
        &db,
        json!([
            "echo 'error[E0425]: cannot find value `x`'; echo 'test a::b ... FAILED'; exit 101"
        ]),
    );
    let outcome = integrate_with(&one_shot, &db, &repo);
    assert_eq!(outcome["outcome"], "needs_session", "{outcome}");
    let detail = show(&db);
    let deferred = payloads(&detail, "integration_deferred");
    assert_eq!(
        deferred[0]["failure"]["class"], "unknown",
        "{}",
        deferred[0]
    );
    assert_eq!(deferred[0]["failed_tests"], Value::Null);
}

/// A repository that declares only the made-up profile: its marks read the
/// output, and its flaky retry runs with its env and without nextest's.
#[test]
fn the_declared_profile_gives_its_retry_env() {
    let one_shot = with_made_up();
    let (_dir, db, repo) = awaiting();
    declare(&repo, MADE_UP_ONLY);
    set_commands(
        &db,
        json!([
            "true",
            "[ -z \"$NEXTEST_FLAKY_RESULT\" ] || exit 7; if [ \"$MADE_FLAKY\" = accept ]; then exit 0; fi; echo 'BROKEN m::n'; echo 'WOBBLY m::n'; exit 1"
        ]),
    );
    let outcome = integrate_with(&one_shot, &db, &repo);
    assert_eq!(outcome["outcome"], "integrated", "{outcome}");
    let detail = show(&db);
    let first = integration_verifications(&detail)[1];
    assert_eq!(first["failure"]["class"], "flaky");
    assert_eq!(first["failed_tests"], json!(["m::n"]));
    assert_eq!(first["flaky_tests"], json!(["m::n"]));
    let retried = payloads(&detail, "integration_retried");
    assert_eq!(retried.len(), 1, "{:?}", event_kinds(&detail));
    assert_eq!(retried[0]["profile"], "made-up");
    assert_eq!(retried[0]["retry_env"], json!({"MADE_FLAKY": "accept"}));
}

/// `doctor` and `status` show the declared profiles and their paths, and
/// `doctor` a profile the binary does not build in as an error (the
/// undeclared view is the unit tests' of `domain::toolchain`).
#[test]
fn doctor_and_status_show_the_declared_profiles() {
    let (_dir, db, repo) = awaiting();
    declare(
        &repo,
        &format!("{MADE_UP_ONLY}[toolchains.core]\nprofile = \"rust\"\n"),
    );
    let declared = json!({
        "declared": [
            {"name": "tool", "profile": "made-up", "paths": ["tool/**"]},
            {"name": "core", "profile": "rust", "paths": ["**"]},
        ],
        "implicit": null,
        "warning": null,
    });
    let one_shot = with_made_up();
    assert_eq!(
        one_shot.doctor(&db, false, None).unwrap()["toolchains"],
        declared
    );
    assert_eq!(
        one_shot.status_for(&db, None).unwrap()["toolchains"],
        declared
    );
    // The binary's own registry does not have the made-up profile.
    let error = runtime::doctor(&db, false).unwrap()["toolchains"]["error"]
        .as_str()
        .unwrap()
        .to_owned();
    assert!(
        error.contains("[toolchains.tool]: unknown profile \"made-up\"; the profiles are rust"),
        "{error}"
    );
}
