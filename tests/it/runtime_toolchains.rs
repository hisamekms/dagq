//! Runtime tests: the toolchain profiles a repository declares in
//! `[toolchains.<name>]` of `dagq.toml` read its verification's output
//! (ADR-t2268-1). A second profile, made up in `tests/common` and
//! registered beside the built-in one, classifies, names its tests and gives its retry env
//! with no other code; the runtime's own host marks hold whatever is
//! declared, and an output no declared profile reads is `unknown`, as is
//! one of a repository that declares none, dagq's source too. The
//! host's Rust toolchain is recorded at a claim where the Rust profile is
//! declared (the KPIs' `toolchain` axis is `cli_read`'s).
use crate::runtime_support;

use crate::common::toolchain::PROFILES;
use runtime_support::*;

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

/// `tests/common`'s `within` line as a mark of the project's own.
const WITHIN_MARK: &str = "[verify_marks.within]\nclass = \"timeout\"\nline = \"test {test} timed out: *did not happen within*\"\n";

/// A command that fails with `within`'s line.
const WITHIN: &str =
    "echo 'test it_x::waits timed out: the run to land did not happen within 20s'; exit 101";

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

/// Commit a `Cargo.toml` that makes `repo` dagq's source.
fn make_dagq_source(repo: &Path) {
    fs::write(repo.join("Cargo.toml"), "[package]\nname = \"dagq\"\n").unwrap();
    git(repo, &["add", "Cargo.toml"]);
    git(repo, &["commit", "-q", "-m", "dagq"]);
}

/// On one repository that is dagq's source, first without a declaration,
/// then declaring only `within`'s mark, then the made-up profile and the
/// mark: a command that exits 124, 128 + a signal, or fails on a full disk
/// is retried once and held for a person when it fails so again
/// (ADR-t639-1 decisions 1 to 4, ADR-t2268-1 decision 4); so is one that
/// prints `within`'s line once the mark is declared, which names its test.
#[test]
fn the_host_failures_are_retried_and_held_whatever_is_declared() {
    let (_dir, db, repo) = awaiting();
    make_dagq_source(&repo);
    let one_shot = with_made_up();
    let host = [
        ("echo boom; exit 124", "timeout"),
        ("exit 137", "killed"),
        ("echo 'write: No space left on device'; exit 1", "disk_full"),
    ];
    let made_up_and_mark = format!("{MADE_UP_ONLY}{WITHIN_MARK}");
    for (declared, commands) in [
        (None, &host[..]),
        (Some(WITHIN_MARK), &[][..]),
        (Some(made_up_and_mark.as_str()), &host[..]),
    ] {
        if let Some(toml) = declared {
            declare(&repo, toml);
        }
        let within = declared.map(|_| &(WITHIN, "timeout"));
        for &(command, class) in commands.iter().chain(within) {
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
            if command == WITHIN {
                let verifications = integration_verifications(&detail);
                for verification in &verifications[verifications.len() - 2..] {
                    assert_eq!(verification["failed_tests"], json!(["it_x::waits"]));
                }
            }
        }
    }
}

/// A repository that declares only the made-up profile: cargo's marks and
/// `within`'s line, which it does not declare as a mark, mean nothing
/// there (`unknown`, no tests named).
#[test]
fn an_output_only_another_profile_could_read_is_unknown() {
    let (_dir, db, repo) = awaiting();
    declare(&repo, MADE_UP_ONLY);
    let one_shot = with_made_up();
    set_commands(
        &db,
        json!([
            "echo 'error[E0425]: cannot find value `x`'; echo 'test a::b ... FAILED'; echo 'test a::b timed out: x did not happen within 1s'; exit 101"
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

/// A repository that declares no profile, dagq's source too (ADR-t2268-1
/// decision 3): `doctor` shows none and no warning, and cargo's and
/// nextest's marks, a test passed on its retry and `within`'s line mean
/// nothing (`unknown`, no tests named, no flaky retry).
#[test]
fn a_repository_that_declares_none_reads_no_profile_even_as_dagqs_source() {
    let (_dir, db, repo) = awaiting();
    make_dagq_source(&repo);
    let one_shot = with_made_up();
    assert_eq!(
        one_shot.doctor(&db, false, None).unwrap()["toolchains"],
        json!({"declared": [], "marks": []})
    );
    set_commands(
        &db,
        json!([
            "echo 'error[E0425]: cannot find value `x`'; echo '        FAIL [   0.1s] dagq::it a::b'; echo '  FLAKY 2/2 [   0.1s] dagq::it a::b'; echo 'test a::b timed out: x did not happen within 1s'; exit 101"
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
    assert!(payloads(&detail, "integration_retried").is_empty());
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

/// `doctor` and `status` show the declared profiles and their paths and the
/// project's marks, and
/// `doctor` a profile the binary does not build in as an error (the
/// undeclared view is
/// [`a_repository_that_declares_none_reads_no_profile_even_as_dagqs_source`]'s).
#[test]
fn doctor_and_status_show_the_declared_profiles() {
    let (_dir, db, repo) = awaiting();
    declare(
        &repo,
        &format!("{MADE_UP_ONLY}[toolchains.core]\nprofile = \"rust\"\n{WITHIN_MARK}"),
    );
    let declared = json!({
        "declared": [
            {"name": "tool", "profile": "made-up", "paths": ["tool/**"]},
            {"name": "core", "profile": "rust", "paths": ["**"]},
        ],
        "marks": [{
            "name": "within",
            "class": "timeout",
            "line": "test {test} timed out: *did not happen within*",
        }],
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

/// A repository that is not dagq's source and declares the Rust profile:
/// the supervisor's claim records the host's `rustc` release and host,
/// from the declaration and not the source judgement. The cases of the
/// judgement (no Rust profile resolved, even in dagq's source) are the unit
/// test's of `domain::toolchain`.
#[test]
fn a_claim_records_the_rust_toolchain_where_the_profile_is_declared() {
    warm_dagq();
    let (_dir, repo, db) = fixture();
    declare(&repo, "[toolchains.rust]\nprofile = \"rust\"\n");
    let backend = TestWorkspace::new(&db, false, VALID_AGENT);
    let options = SuperviseOptions {
        profiles: &PROFILES,
        ..supervise_options(1, true)
    };
    let outcome = supervise_with(&db, &repo, &backend, &options).unwrap();
    backend.join();
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    let claimed = payloads(&show(&db), "run_claimed")[0].clone();
    assert!(
        claimed["rustc_release"]
            .as_str()
            .is_some_and(|release| !release.is_empty()),
        "{claimed}"
    );
    assert!(claimed["rustc_host"].is_string(), "{claimed}");
}
