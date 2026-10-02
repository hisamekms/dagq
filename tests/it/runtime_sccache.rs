//! Runtime tests: the sccache server of a `[run.env]` whose `RUSTC_WRAPPER`
//! is sccache (ADR-t1215-1). A stub `sccache` writes each call's arguments,
//! `SCCACHE_IDLE_TIMEOUT` and `PATH`; the server's port is a loopback port
//! the test listens on once the stub was called, or never. The supervisor
//! looks at the port without running sccache, starts the server outside
//! the sandbox with `SCCACHE_IDLE_TIMEOUT=0`, and a Codex turn or review
//! whose server is not confirmed runs without `RUSTC_WRAPPER`.
use crate::common;
use crate::runtime_codex::{FINISH, codex_fixture, detail};
use crate::runtime_support;

use dagq::{domain::sccache::SccacheTarget, runtime::SccacheOptions};
use runtime_support::*;
use std::net::{Ipv4Addr, TcpListener};

/// A loopback port nothing listens on now.
fn free_port() -> u16 {
    TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

/// A stub `sccache` in `<dir>/bin` (failing while `sccache-fails` is
/// there), and the `PATH` the `[run.env]` gives.
fn stub_sccache(dir: &Path, name: &str) -> (PathBuf, String) {
    let bin = dir.join("bin");
    fs::create_dir_all(&bin).unwrap();
    let program = bin.join(name);
    fs::write(
        &program,
        "#!/bin/sh\nprintf '%s idle=%s path=%s\\n' \"$*\" \"${SCCACHE_IDLE_TIMEOUT-unset}\" \"$PATH\" >> \"${0%/*}/sccache-calls.log\"\n[ -f \"${0%/*}/sccache-fails\" ] && { echo 'sccache stub refused' >&2; exit 2; }\nexit 0\n",
    )
    .unwrap();
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(&program, fs::Permissions::from_mode(0o755)).unwrap();
    let path = format!("{}:/usr/bin:/bin", bin.display());
    (program, path)
}

fn calls(program: &Path) -> Vec<String> {
    fs::read_to_string(program.with_file_name("sccache-calls.log"))
        .unwrap_or_default()
        .lines()
        .map(str::to_owned)
        .collect()
}

fn queue_events(db: &Path, kind: &str) -> Vec<Value> {
    Connection::open(db)
        .unwrap()
        .prepare("SELECT payload FROM run_events WHERE kind=?1 ORDER BY id")
        .unwrap()
        .query_map([kind], |r| r.get::<_, String>(0))
        .unwrap()
        .map(|p| serde_json::from_str(&p.unwrap()).unwrap())
        .collect()
}

/// The `[run.env]` naming `program` as `RUSTC_WRAPPER` on `port`, and the
/// review on Codex, committed.
fn configure(repo: &Path, program: &Path, port: u16, path: &str) {
    fs::write(
        repo.join("dagq.toml"),
        format!(
            "[run.env]\nRUSTC_WRAPPER = '{}'\nSCCACHE_SERVER_PORT = '{port}'\nPATH = '{path}'\n\n[roles.review]\nprovider = 'codex'\n",
            program.display()
        ),
    )
    .unwrap();
    git(repo, &["add", "dagq.toml"]);
    git(repo, &["commit", "-m", "sccache as the wrapper"]);
}

/// Supervise the Codex fixture's task to its end with the sccache server
/// kept.
fn supervise_codex(
    dir: &Path,
    repo: &Path,
    db: &Path,
    backend: &TestWorkspace,
    codex: PathBuf,
) -> Value {
    let mut options = supervise_options(1, true);
    options.codex = codex;
    options.no_claude = true;
    options.codex_home = Some(dir.join(CODEX_HOME));
    // Long enough for the stub to start on a loaded host: a start killed
    // at its limit never runs the stub (it then records `did not finish`).
    options.sccache = Some(SccacheOptions {
        start_timeout: Duration::from_secs(120),
    });
    let reviewer = TestReviewer::new(&[]);
    let result = runtime::supervise_with_reviewer(
        db,
        repo,
        backend,
        &claude_stub(db),
        &reviewer,
        Path::new(env!("CARGO_BIN_EXE_dagq")),
        &options,
    )
    .unwrap();
    backend.join();
    assert_eq!(result["errors"], json!([]), "{result}");
    result
}

#[test]
fn the_supervisor_starts_a_missing_server_outside_the_sandbox_and_records_it() {
    let (dir, repo, db, mut backend, codex) = codex_fixture();
    let (program, path) = stub_sccache(dir.path(), "sccache");
    let port = free_port();
    configure(&repo, &program, port, &path);
    backend.sccache = Some(SccacheTarget {
        program: program.display().to_string(),
        port,
    });
    // The wrapper's environment names it, as the workspace's [run.env]
    // does: each turn inherits it unless the turn removes it.
    backend.inherited_env = vec![("RUSTC_WRAPPER".into(), program.display().to_string())];
    set_turns(dir.path(), FINISH);
    // The server listens once the stub was called to start it.
    let log = program.with_file_name("sccache-calls.log");
    let server = thread::spawn(move || {
        let _waiting = common::within(common::STEP_LIMIT, "the stub sccache to be called");
        while !log.exists() {
            thread::sleep(Duration::from_millis(10));
        }
        TcpListener::bind((Ipv4Addr::LOCALHOST, port)).unwrap()
    });
    let result = supervise_codex(dir.path(), &repo, &db, &backend, codex);
    let _listener = joined(server, "the server's listener");
    assert_eq!(result["runs"][0]["status"], "integrated", "{result}");

    // Started once, at the supervisor's start, with no idle timeout and
    // the [run.env]'s PATH; every later look started nothing.
    assert_eq!(
        calls(&program),
        [format!("--start-server idle=0 path={path}")]
    );
    let started = queue_events(&db, "sccache_server_started");
    assert_eq!(started.len(), 1, "{started:?}");
    let started = &started[0];
    assert_eq!(started["reason"], "startup");
    assert_eq!(started["port"], port);
    assert_eq!(started["idle_timeout"], "0");
    assert_eq!(started["program"], program.display().to_string());
    assert!(started["supervisor"].is_string(), "{started}");
    assert!(started["supervisor_pid"].is_u64(), "{started}");
    assert!(started["at"].is_i64(), "{started}");
    // lsof names this process (the listener's); a host without lsof says
    // why there is no pid.
    if started["pid"].is_null() {
        assert!(
            started["pid_error"]
                .as_str()
                .unwrap()
                .contains("lsof could not be run"),
            "{started}"
        );
    } else {
        assert_eq!(started["pid"], std::process::id(), "{started}");
        assert!(started.get("pid_error").is_none(), "{started}");
    }
    assert!(queue_events(&db, "sccache_server_start_failed").is_empty());
    // The turns and the review kept RUSTC_WRAPPER.
    let detail = detail(&db);
    assert!(payloads(&detail, "sccache_wrapper_removed").is_empty());
    assert_eq!(
        payloads(&detail, "review_started")[0]["launch"]["provider"],
        "codex"
    );
    let review = fs::read_to_string(dir.path().join("codex-review-wrapper.log")).unwrap();
    assert_eq!(review.trim(), program.display().to_string());
    let run = &detail.runs[0];
    let turns =
        fs::read_to_string(Path::new(run.run_dir().unwrap()).join("stub-wrapper.log")).unwrap();
    assert_eq!(turns.trim(), program.display().to_string());
}

#[test]
fn turns_and_reviews_without_a_confirmed_server_run_without_the_wrapper() {
    let (dir, repo, db, mut backend, codex) = codex_fixture();
    let (program, path) = stub_sccache(dir.path(), "sccache");
    fs::write(program.with_file_name("sccache-fails"), "").unwrap();
    let port = free_port();
    configure(&repo, &program, port, &path);
    backend.sccache = Some(SccacheTarget {
        program: program.display().to_string(),
        port,
    });
    // The wrapper's environment names it, as the workspace's [run.env]
    // does: each turn inherits it unless the turn removes it.
    backend.inherited_env = vec![("RUSTC_WRAPPER".into(), program.display().to_string())];
    set_turns(dir.path(), FINISH);
    let result = supervise_codex(dir.path(), &repo, &db, &backend, codex);
    assert_eq!(result["runs"][0]["status"], "integrated", "{result}");

    // Only starts, which failed (once, unless the run outlasted the wait
    // before a retry): nothing else ran sccache, so no server was started
    // with another environment.
    let calls = calls(&program);
    assert!(
        !calls.is_empty(),
        "{:?}",
        queue_events(&db, "sccache_server_start_failed")
    );
    for call in &calls {
        assert_eq!(*call, format!("--start-server idle=0 path={path}"));
    }
    assert!(queue_events(&db, "sccache_server_started").is_empty());
    let failed = queue_events(&db, "sccache_server_start_failed");
    assert_eq!(failed.len(), 1, "{failed:?}");
    assert_eq!(failed[0]["reason"], "startup");
    assert_eq!(failed[0]["port"], port);
    assert_eq!(failed[0]["idle_timeout"], "0");
    assert!(
        failed[0]["error"]
            .as_str()
            .unwrap()
            .contains("sccache stub refused"),
        "{}",
        failed[0]
    );

    // The turn (by its wrapper) and the review (by the supervisor) ran
    // without RUSTC_WRAPPER, and said so.
    let detail = detail(&db);
    let removed = payloads(&detail, "sccache_wrapper_removed");
    assert_eq!(removed.len(), 2, "{removed:?}");
    assert_eq!(removed[0]["by"], "wrapper");
    assert_eq!(removed[0]["turn"], 1);
    assert_eq!(removed[0]["port"], port);
    assert!(
        removed[0]["reason"]
            .as_str()
            .unwrap()
            .contains("no sccache server listens"),
        "{}",
        removed[0]
    );
    assert_eq!(removed[1]["by"], "supervisor");
    assert_eq!(removed[1]["job"], "review");
    assert_eq!(removed[1]["attempt"], 1);
    let run = &detail.runs[0];
    let turns =
        fs::read_to_string(Path::new(run.run_dir().unwrap()).join("stub-wrapper.log")).unwrap();
    // It inherited RUSTC_WRAPPER (`inherited_env`) and ran without it.
    assert_eq!(turns.trim(), "unset");
    let review = fs::read_to_string(dir.path().join("codex-review-wrapper.log")).unwrap();
    assert_eq!(review.trim(), "unset");
}

#[test]
fn a_wrapper_other_than_sccache_is_left_alone() {
    let (fixture, repo, db) = fixture();
    let (program, path) = stub_sccache(fixture.dir.path(), "rustc-wrapper");
    let port = free_port();
    configure(&repo, &program, port, &path);
    SqliteQueue::open(&db)
        .unwrap()
        .transition(TaskId::new(1), TaskAction::Cancel)
        .unwrap();
    let backend = TestWorkspace::new(&db, false, VALID_AGENT);
    let mut options = supervise_options(1, true);
    options.sccache = Some(SccacheOptions::default());
    let result = supervise_with(&db, &repo, &backend, &options).unwrap();
    assert_eq!(result["errors"], json!([]), "{result}");
    assert!(calls(&program).is_empty());
    for kind in [
        "sccache_server_started",
        "sccache_server_start_failed",
        "sccache_wrapper_removed",
    ] {
        assert!(queue_events(&db, kind).is_empty(), "{kind}");
    }
}
