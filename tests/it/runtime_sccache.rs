//! Runtime tests: the sccache server of a `[run.env]` whose `RUSTC_WRAPPER`
//! is sccache (ADR-t1215-1). A stub `sccache` writes each call's arguments,
//! `SCCACHE_IDLE_TIMEOUT` and `PATH`; the server's port is a loopback port
//! the test listens on once the stub was called, or never. The supervisor
//! looks at the port without running sccache, starts the server outside
//! the sandbox with `SCCACHE_IDLE_TIMEOUT=0`, and a Codex turn or review
//! whose server is not confirmed runs without `RUSTC_WRAPPER`. One whose
//! server is confirmed compiles through the guard (`dagq` as
//! `RUSTC_WRAPPER`, ADR-t2008-1), which no sccache in the sandbox gets past
//! to start a server when the server stops during the turn.
use crate::common;
use crate::runtime_codex::{FINISH, codex_fixture, detail};
use crate::runtime_support;

use dagq::{
    domain::sccache::{GUARD_NAME, REFUSED_ERROR_LOG, SccacheTarget},
    runtime::SccacheOptions,
};
use runtime_support::*;
use std::net::{Ipv4Addr, TcpListener};

/// Only the fixture's wrapper identity is needed here. Signals and liveness
/// still refer to the real child; process enumeration is deliberately stubbed.
struct WrapperProcesses;

struct StopOnDrop(Arc<std::sync::atomic::AtomicBool>);
impl Drop for StopOnDrop {
    fn drop(&mut self) {
        self.0.store(true, Ordering::SeqCst);
    }
}

struct ListenerGuard {
    done: Arc<std::sync::atomic::AtomicBool>,
    thread: Option<thread::JoinHandle<()>>,
}
impl Drop for ListenerGuard {
    fn drop(&mut self) {
        self.done.store(true, Ordering::SeqCst);
        joined(self.thread.take().unwrap(), "the stub server controller");
    }
}

impl dagq::application::ProcessControl for WrapperProcesses {
    fn alive(&self, pid: u32) -> bool {
        dagq::infrastructure::adapters::SystemProcesses.alive(pid)
    }
    fn terminate(&self, pid: u32) -> anyhow::Result<()> {
        dagq::infrastructure::adapters::SystemProcesses.terminate(pid)
    }
    fn interrupt(&self, pid: u32) -> anyhow::Result<()> {
        dagq::infrastructure::adapters::SystemProcesses.interrupt(pid)
    }
    fn kill(&self, pid: u32) -> anyhow::Result<()> {
        dagq::infrastructure::adapters::SystemProcesses.kill(pid)
    }
    fn start_identity(&self, pid: u32) -> Option<String> {
        self.alive(pid).then(|| format!("fixture-wrapper-{pid}"))
    }
    fn list(&self) -> anyhow::Result<Vec<dagq::domain::recovery::ProcessInfo>> {
        Ok(Vec::new())
    }
    fn executables(&self) -> anyhow::Result<Vec<dagq::domain::disk::ProcessExecutable>> {
        Ok(Vec::new())
    }
}

/// Explicit programs, rather than process-wide PATH changes: cargo test can
/// run these fixtures concurrently. The server identity is a fixed stub row.
fn process_tools(dir: &Path) -> (PathBuf, PathBuf) {
    let lsof = dir.join("stub-lsof");
    let ps = dir.join("stub-ps");
    common::template::script(&lsof, "#!/bin/sh\necho 4242\n");
    common::template::script(
        &ps,
        "#!/bin/sh\ncase \"$*\" in *ppid=*) echo '1 Mon Oct 5 12:00:00 2026 sccache --server';; *) echo init;; esac\n",
    );
    (lsof, ps)
}

/// A loopback port nothing listens on now.
fn free_port() -> u16 {
    TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

/// A stub `sccache` in `<dir>/bin` (failing while `sccache-fails` is
/// there), and the `PATH` the `[run.env]` gives. A compile (a first
/// argument that is no flag) behaves as sccache 0.18's client
/// (`connect_or_start_server` in its `src/commands.rs`): without a server
/// (`server-up` beside it) it re-executes itself with
/// `SCCACHE_START_SERVER=1`, a server that opens `SCCACHE_ERROR_LOG` before
/// it listens (`refused` or `started` in `server-starts.log`), says
/// `sccache: error:` when the start failed, and runs the compiler with
/// `VIA_SCCACHE=1` otherwise.
fn stub_sccache(dir: &Path, name: &str) -> (PathBuf, String) {
    let bin = dir.join("bin");
    fs::create_dir_all(&bin).unwrap();
    let program = bin.join(name);
    fs::write(
        &program,
        r#"#!/bin/sh
D="${0%/*}"
if [ "${SCCACHE_START_SERVER-}" = 1 ]; then
  if [ -n "${SCCACHE_ERROR_LOG-}" ] && ! ( : >> "$SCCACHE_ERROR_LOG" ) 2>/dev/null; then
    echo refused >> "$D/server-starts.log"; exit 1
  fi
  echo started >> "$D/server-starts.log"; exit 0
fi
printf '%s idle=%s path=%s\n' "$*" "${SCCACHE_IDLE_TIMEOUT-unset}" "$PATH" >> "$D/sccache-calls.log"
case "$1" in
  -*) ;;
  *)
    if [ ! -f "$D/server-up" ]; then
      SCCACHE_START_SERVER=1 "$0" || {
        echo 'sccache: error: Timed out waiting for server startup. Maybe the remote service is unreachable?' >&2
        exit 2
      }
    fi
    VIA_SCCACHE=1 exec "$@" ;;
esac
[ -f "$D/sccache-fails" ] && { echo 'sccache stub refused' >&2; exit 2; }
[ "$1" = --show-stats ] && echo '{"stats":{"compile_requests":0,"compile_fails":0,"compilations":0}}'
exit 0
"#,
    )
    .unwrap();
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(&program, fs::Permissions::from_mode(0o755)).unwrap();
    let path = format!("{}:/usr/bin:/bin", bin.display());
    (program, path)
}

/// A stub `rustc` beside `program`: it logs `via=<VIA_SCCACHE or direct>`
/// and its arguments to `rustc-calls.log`, prints `rustc out`, and fails
/// with a compiler's error while `rustc-fails` is there.
fn stub_rustc(program: &Path) -> PathBuf {
    let rustc = program.with_file_name("rustc");
    common::template::script(
        &rustc,
        r#"#!/bin/sh
D="${0%/*}"
printf 'via=%s %s\n' "${VIA_SCCACHE:-direct}" "$*" >> "$D/rustc-calls.log"
echo 'rustc out'
[ -f "$D/rustc-fails" ] && { echo 'error[E0425]: cannot find value' >&2; exit 1; }
exit 0
"#,
    );
    rustc
}

fn lines(path: &Path) -> Vec<String> {
    fs::read_to_string(path)
        .unwrap_or_default()
        .lines()
        .map(str::to_owned)
        .collect()
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
    let (lsof, ps) = process_tools(dir);
    options.processes = Some(runtime::ProcessesPort(
        backend.wrapper_processes.clone().unwrap(),
    ));
    options.sccache = Some(SccacheOptions {
        lsof,
        ps,
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
    backend.wrapper_processes = Some(Arc::new(WrapperProcesses));
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
        calls(&program)
            .into_iter()
            .filter(|call| call.starts_with("--start-server"))
            .collect::<Vec<_>>(),
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
    assert_eq!(started["pid"], 4242, "{started}");
    assert_eq!(started["started_at"], "Mon Oct 5 12:00:00 2026");
    assert_eq!(started["parent_pid"], 1);
    assert_eq!(started["command"], "sccache --server");
    assert!(started.get("pid_error").is_none(), "{started}");
    assert!(queue_events(&db, "sccache_server_start_failed").is_empty());
    // The turns and the review compiled through the guard in the run's
    // directory, a link to dagq (ADR-t2008-1), and were given the refusal
    // of the server's start.
    let detail = detail(&db);
    assert!(payloads(&detail, "sccache_wrapper_removed").is_empty());
    assert_eq!(
        payloads(&detail, "review_started")[0]["launch"]["provider"],
        "codex"
    );
    let run = &detail.runs[0];
    let run_dir = Path::new(run.run_dir().unwrap());
    let guard = run_dir.join(GUARD_NAME);
    assert_eq!(
        fs::read_link(&guard).unwrap(),
        Path::new(env!("CARGO_BIN_EXE_dagq"))
    );
    let review = fs::read_to_string(dir.path().join("codex-review-wrapper.log")).unwrap();
    assert_eq!(review.trim(), guard.display().to_string());
    let turns = fs::read_to_string(run_dir.join("stub-wrapper.log")).unwrap();
    assert_eq!(turns.trim(), guard.display().to_string());
    for log in [
        dir.path().join("codex-review-error-log.log"),
        run_dir.join("stub-error-log.log"),
    ] {
        assert_eq!(lines(&log), [REFUSED_ERROR_LOG], "{}", log.display());
    }
}

#[test]
fn turns_and_reviews_without_a_confirmed_server_run_without_the_wrapper() {
    let (dir, repo, db, mut backend, codex) = codex_fixture();
    backend.wrapper_processes = Some(Arc::new(WrapperProcesses));
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
    // Neither may start a server by an sccache of its own (ADR-t2008-1).
    for log in [
        dir.path().join("codex-review-error-log.log"),
        Path::new(run.run_dir().unwrap()).join("stub-error-log.log"),
    ] {
        assert_eq!(lines(&log), [REFUSED_ERROR_LOG], "{}", log.display());
    }
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

#[test]
fn stopping_the_server_on_the_host_lets_the_supervisor_clear_restart_attention() {
    let (dir, repo, db) = fixture();
    SqliteQueue::open(&db)
        .unwrap()
        .transition(TaskId::new(1), TaskAction::Cancel)
        .unwrap();
    let (program, path) = stub_sccache(dir.path(), "sccache");
    common::template::script(
        &program,
        r#"#!/bin/sh
printf '%s idle=%s path=%s\n' "$*" "$SCCACHE_IDLE_TIMEOUT" "$PATH" >> "${0%/*}/sccache-calls.log"
case "$1" in
--show-stats) echo '{"stats":{"compile_requests":0,"compile_fails":0,"compilations":0}}';;
--stop-server) echo 'stop refused' >&2; exit 2;;
--start-server) touch "${0%/*}/start-request";;
esac
"#,
    );
    let bin = program.parent().unwrap();
    let lsof = bin.join("stub-lsof");
    let ps = bin.join("stub-ps");
    common::template::script(
        &lsof,
        "#!/bin/sh\nif [ -f \"${0%/*}/start-request\" ]; then echo 4343; else echo 4242; fi\n",
    );
    common::template::script(
        &ps,
        "#!/bin/sh\ncase \"$*\" in\n*ppid=*) if [ \"$2\" = 4242 ]; then echo '99 Mon Oct 5 12:00:00 2026 sccache --server'; else echo '1 Mon Oct 5 12:01:00 2026 sccache --server'; fi;;\n*) if [ \"$2\" = 99 ]; then echo '/usr/bin/sandbox-exec -p fixture'; else echo init; fi;;\nesac\n",
    );
    let old_listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
    let port = old_listener.local_addr().unwrap().port();
    configure(&repo, &program, port, &path);
    let backend = TestWorkspace::new(&db, false, VALID_AGENT);
    let mut options = supervise_options(1, true);
    options.sccache = Some(SccacheOptions {
        lsof,
        ps,
        start_timeout: Duration::from_secs(120),
    });
    options.processes = Some(runtime::ProcessesPort(Arc::new(WrapperProcesses)));
    assert_eq!(
        supervise_with(&db, &repo, &backend, &options).unwrap()["errors"],
        json!([])
    );
    assert_eq!(queue_events(&db, "sccache_server_restart_failed").len(), 1);
    assert!(queue_events(&db, "sccache_server_started").is_empty());
    let health = || {
        dagq::application::health::status(
            &SqliteQueue::open(&db).unwrap(),
            &WrapperProcesses,
            &SystemClock,
            None,
        )
        .unwrap()
    };
    assert!(health()["attention"].as_array().unwrap().iter().any(|a| {
        a["kind"] == "sccache_server_restart_failed"
            && a["next"] == "stop sccache on the host; the supervisor starts it"
    }));

    // Follow that instruction: the person stops the old listener and starts
    // nothing. The next supervisor keeps the fault and starts its replacement.
    drop(old_listener);
    let request = bin.join("start-request");
    thread::scope(|scope| {
        let server = scope.spawn(|| {
            let _waiting = common::within(common::STEP_LIMIT, "the supervisor's sccache start");
            let deadline = Instant::now() + common::STEP_LIMIT;
            while !request.exists() && Instant::now() < deadline {
                thread::sleep(Duration::from_millis(5));
            }
            assert!(
                request.exists(),
                "the supervisor did not request a replacement"
            );
            TcpListener::bind((Ipv4Addr::LOCALHOST, port)).unwrap()
        });
        assert_eq!(
            supervise_with(&db, &repo, &backend, &options).unwrap()["errors"],
            json!([])
        );
        let _joining = common::within(common::STEP_LIMIT, "the replacement listener to return");
        let _listener = server.join().unwrap();
        let started = queue_events(&db, "sccache_server_started");
        assert_eq!(started.len(), 1);
        assert_eq!(started[0]["reason"], "restart");
        assert_eq!(started[0]["pid"], 4343);
        assert_eq!(started[0]["replaced"]["pid"], 4242);
        assert_eq!(started[0]["idle_timeout"], "0");
        assert!(
            health()["attention"]
                .as_array()
                .unwrap()
                .iter()
                .all(|a| { a["kind"] != "sccache_server_restart_failed" })
        );
    });
    assert_eq!(
        calls(&program)
            .into_iter()
            .filter(|call| !call.starts_with("--show-stats"))
            .collect::<Vec<_>>(),
        [
            format!("--stop-server idle=0 path={path}"),
            format!("--start-server idle=0 path={path}")
        ]
    );
}

#[test]
fn the_supervisors_poll_and_failed_start_retry_use_its_monotonic_clock() {
    let (dir, repo, db) = fixture();
    SqliteQueue::open(&db)
        .unwrap()
        .transition(TaskId::new(1), TaskAction::Cancel)
        .unwrap();
    let (program, path) = stub_sccache(dir.path(), "sccache");
    fs::write(program.with_file_name("sccache-fails"), "").unwrap();
    configure(&repo, &program, free_port(), &path);
    let (mut options, ahead) = supervise_options_ahead(1, false);
    // Starting ahead of the host also catches timestamps stored from the
    // real clock and later compared with the injected one.
    ahead.by(Duration::from_secs(3600));
    let (lsof, ps) = process_tools(dir.path());
    options.sccache = Some(SccacheOptions {
        lsof,
        ps,
        start_timeout: Duration::from_secs(120),
    });
    options.processes = Some(runtime::ProcessesPort(Arc::new(WrapperProcesses)));
    let backend = TestWorkspace::new(&db, false, VALID_AGENT);
    let stop = options.stop.clone();
    thread::scope(|scope| {
        let supervisor = scope.spawn(|| supervise_with(&db, &repo, &backend, &options));
        let _stop = StopOnDrop(stop.clone());
        wait_until(&db, common::STEP_LIMIT, |_| {
            !queue_events(&db, "sccache_server_start_failed").is_empty()
        });
        assert_eq!(calls(&program).len(), 1);
        // The 10-second poll is due, but the 60-second retry still waits.
        ahead.by(Duration::from_secs(10));
        await_passes(&options.passes, SOME_PASSES);
        assert_eq!(calls(&program).len(), 1);
        // No real 60-second sleep: the injected clock releases the retry.
        ahead.by(Duration::from_secs(50));
        await_passes(&options.passes, SOME_PASSES);
        assert_eq!(calls(&program).len(), 2);
        stop.store(true, Ordering::SeqCst);
        let _waiting = common::within(common::STEP_LIMIT, "the sccache supervisor to stop");
        let outcome = supervisor.join().unwrap().unwrap();
        assert_eq!(outcome["errors"], json!([]));
    });
    assert_eq!(
        calls(&program),
        vec![format!("--start-server idle=0 path={path}"); 2]
    );
    // Repeating the same failed start still records its error only once.
    assert_eq!(queue_events(&db, "sccache_server_start_failed").len(), 1);
}

#[test]
fn failed_restarts_in_one_supervisor_keep_retrying_but_notify_once_per_fault() {
    let (dir, repo, db) = fixture();
    SqliteQueue::open(&db)
        .unwrap()
        .transition(TaskId::new(1), TaskAction::Cancel)
        .unwrap();
    let (program, path) = stub_sccache(dir.path(), "sccache");
    let bin = program.parent().unwrap();
    common::template::script(
        &program,
        r#"#!/bin/sh
printf '%s idle=%s path=%s\n' "$*" "${SCCACHE_IDLE_TIMEOUT-unset}" "$PATH" >> "${0%/*}/sccache-calls.log"
case "$1" in
--show-stats) echo '{"stats":{"compile_requests":0,"compile_fails":0,"compilations":0}}';;
--stop-server)
    [ -f "${0%/*}/fail-stop" ] && { echo 'stop refused' >&2; exit 2; }
    touch "${0%/*}/stop-request";;
--start-server) echo 'replacement refused' >&2; exit 2;;
esac
"#,
    );
    fs::write(bin.join("fail-stop"), "").unwrap();
    let (lsof, ps) = process_tools(bin);
    common::template::script(
        &ps,
        "#!/bin/sh\ncase \"$*\" in *ppid=*) echo '99 Mon Oct 5 12:00:00 2026 sccache --server';; *) echo '/usr/bin/sandbox-exec -p fixture';; esac\n",
    );
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
    let port = listener.local_addr().unwrap().port();
    configure(&repo, &program, port, &path);
    let done = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let ended = done.clone();
    let server_dir = bin.to_path_buf();
    let controller = thread::spawn(move || {
        let deadline = Instant::now() + common::STEP_LIMIT;
        let mut listener = Some(listener);
        while !ended.load(Ordering::SeqCst) && Instant::now() < deadline {
            if server_dir.join("stop-request").exists() {
                listener.take();
            }
            thread::sleep(Duration::from_millis(5));
        }
    });
    let _controller = ListenerGuard {
        done,
        thread: Some(controller),
    };
    let (mut options, ahead) = supervise_options_ahead(1, false);
    ahead.by(Duration::from_secs(3600));
    options.sccache = Some(SccacheOptions {
        lsof,
        ps,
        start_timeout: Duration::from_secs(120),
    });
    options.processes = Some(runtime::ProcessesPort(Arc::new(WrapperProcesses)));
    let backend = TestWorkspace::new(&db, false, VALID_AGENT);
    let control_calls = || {
        calls(&program)
            .into_iter()
            .filter(|call| !call.starts_with("--show-stats"))
            .collect::<Vec<_>>()
    };
    thread::scope(|scope| {
        let supervisor = scope.spawn(|| supervise_with(&db, &repo, &backend, &options));
        let _stop = StopOnDrop(options.stop.clone());
        wait_until(&db, common::STEP_LIMIT, |_| {
            !queue_events(&db, "sccache_server_restart_failed").is_empty()
        });
        for retry in 1..=3 {
            ahead.by(Duration::from_secs(60));
            await_passes(&options.passes, SOME_PASSES);
            assert_eq!(control_calls().len(), retry + 1);
            assert_eq!(queue_events(&db, "sccache_server_restart_failed").len(), 1);
            assert_eq!(queue_events(&db, "sccache_server_unhealthy").len(), 1);
        }
        // The stop succeeds now, but replacement fails with a different error.
        // Later retries run through the absent-server branch in this same supervisor.
        fs::remove_file(bin.join("fail-stop")).unwrap();
        ahead.by(Duration::from_secs(60));
        await_passes(&options.passes, SOME_PASSES);
        assert_eq!(queue_events(&db, "sccache_server_restart_failed").len(), 2);
        for retry in 1..=3 {
            ahead.by(Duration::from_secs(60));
            await_passes(&options.passes, SOME_PASSES);
            assert_eq!(
                control_calls()
                    .iter()
                    .filter(|call| call.starts_with("--start-server"))
                    .count(),
                retry + 1
            );
            assert_eq!(queue_events(&db, "sccache_server_restart_failed").len(), 2);
            assert_eq!(queue_events(&db, "sccache_server_unhealthy").len(), 1);
        }
        options.stop.store(true, Ordering::SeqCst);
        let _waiting = common::within(common::STEP_LIMIT, "the sccache retry supervisor to stop");
        assert_eq!(supervisor.join().unwrap().unwrap()["errors"], json!([]));
    });
    let failures = queue_events(&db, "sccache_server_restart_failed");
    assert!(
        failures[0]["error"]
            .as_str()
            .unwrap()
            .contains("stop refused")
    );
    assert!(
        failures[1]["error"]
            .as_str()
            .unwrap()
            .contains("replacement refused")
    );
    assert!(
        failures
            .iter()
            .all(|failure| dagq::domain::wakes_inbox("sccache_server_restart_failed", failure))
    );
    assert_eq!(queue_events(&db, "supervisor_started").len(), 1);
    assert_eq!(queue_events(&db, "sccache_server_start_failed").len(), 1);
    assert!(queue_events(&db, "sccache_server_started").is_empty());
    assert_eq!(
        control_calls()
            .iter()
            .filter(|call| call.starts_with("--stop-server"))
            .count(),
        5
    );
}

#[test]
fn the_supervisor_restarts_a_sandboxed_server_and_carries_a_failed_replacement_forward() {
    let (dir, repo, db, mut backend, codex) = codex_fixture();
    backend.wrapper_processes = Some(Arc::new(WrapperProcesses));
    let (program, path) = stub_sccache(dir.path(), "sccache");
    let bin = program.parent().unwrap();
    common::template::script(
        &program,
        "#!/bin/sh\nprintf '%s idle=%s path=%s\\n' \"$*\" \"${SCCACHE_IDLE_TIMEOUT-unset}\" \"$PATH\" >> \"${0%/*}/sccache-calls.log\"\ncase \"$1\" in\n--show-stats) echo '{\"stats\":{\"compile_requests\":0,\"compile_fails\":0,\"compilations\":0}}';;\n--stop-server) touch \"${0%/*}/stop-request\";;\n--start-server) [ -f \"${0%/*}/fail-start\" ] && { echo 'replacement refused' >&2; exit 2; }; touch \"${0%/*}/start-request\";;\nesac\n",
    );
    let lsof = bin.join("stub-lsof");
    let ps = bin.join("stub-ps");
    common::template::script(
        &lsof,
        "#!/bin/sh\nif [ -f \"${0%/*}/old-server\" ]; then echo 4242; elif [ -f \"${0%/*}/new-server\" ]; then echo 4343; fi\n",
    );
    common::template::script(
        &ps,
        "#!/bin/sh\ncase \"$*\" in\n*ppid=*) if [ \"$2\" = 4242 ]; then echo '99 Mon Oct 5 12:00:00 2026 sccache --server'; else echo '1 Mon Oct 5 12:01:00 2026 sccache --server'; fi;;\n*) if [ \"$2\" = 99 ]; then echo '/usr/bin/sandbox-exec -p fixture'; else echo init; fi;;\nesac\n",
    );
    fs::write(bin.join("old-server"), "").unwrap();
    fs::write(bin.join("fail-start"), "").unwrap();
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
    let port = listener.local_addr().unwrap().port();
    configure(&repo, &program, port, &path);
    backend.sccache = Some(SccacheTarget {
        program: program.display().to_string(),
        port,
    });
    backend.inherited_env = vec![("RUSTC_WRAPPER".into(), program.display().to_string())];
    set_turns(dir.path(), FINISH);
    // The fixture models server lifetime with a listener. It starts no
    // daemon; Drop joins the bounded controller even when an assertion fails.
    let done = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let ended = done.clone();
    let server_dir = bin.to_path_buf();
    let controller = thread::spawn(move || {
        let deadline = Instant::now() + common::STEP_LIMIT;
        let mut listener = Some(listener);
        while !ended.load(std::sync::atomic::Ordering::SeqCst) && Instant::now() < deadline {
            if server_dir.join("stop-request").exists() {
                listener.take();
                fs::remove_file(server_dir.join("stop-request")).unwrap();
                fs::remove_file(server_dir.join("old-server")).unwrap();
            }
            if server_dir.join("start-request").exists() && listener.is_none() {
                listener = Some(TcpListener::bind((Ipv4Addr::LOCALHOST, port)).unwrap());
                fs::write(server_dir.join("new-server"), "").unwrap();
            }
            thread::sleep(Duration::from_millis(5));
        }
    });
    let _controller = ListenerGuard {
        done,
        thread: Some(controller),
    };
    let mut options = supervise_options(1, true);
    options.codex = codex;
    options.no_claude = true;
    options.codex_home = Some(dir.path().join(CODEX_HOME));
    options.processes = Some(runtime::ProcessesPort(
        backend.wrapper_processes.clone().unwrap(),
    ));
    options.sccache = Some(SccacheOptions {
        lsof,
        ps,
        start_timeout: Duration::from_secs(120),
    });
    let reviewer = TestReviewer::new(&[]);
    let supervise = || {
        runtime::supervise_with_reviewer(
            &db,
            &repo,
            &backend,
            &claude_stub(&db),
            &reviewer,
            Path::new(env!("CARGO_BIN_EXE_dagq")),
            &options,
        )
        .unwrap()
    };
    let first = supervise();
    backend.join();
    assert_eq!(first["errors"], json!([]));
    assert_eq!(first["runs"][0]["status"], "integrated");
    let supervisor = queue_events(&db, "supervisor_started")[0]["supervisor"].clone();
    let failure = queue_events(&db, "sccache_server_restart_failed");
    assert_eq!(failure.len(), 1);
    assert_eq!(failure[0]["supervisor"], supervisor);
    assert_eq!(failure[0]["supervisor_pid"], std::process::id());
    assert_eq!(failure[0]["reason"], "sandboxed");
    assert_eq!(failure[0]["pid"], 4242);
    assert_eq!(failure[0]["program"], program.display().to_string());
    let health = dagq::application::health::status(
        &SqliteQueue::open(&db).unwrap(),
        &WrapperProcesses,
        &SystemClock,
        None,
    )
    .unwrap();
    let attention = health["attention"]
        .as_array()
        .unwrap()
        .iter()
        .find(|entry| entry["kind"] == "sccache_server_restart_failed")
        .unwrap();
    assert_eq!(
        attention["next"],
        "stop sccache on the host; the supervisor starts it"
    );
    let not_queue_rows: i64 = Connection::open(&db).unwrap().query_row(
        "SELECT count(*) FROM run_events WHERE kind LIKE 'sccache_server_%' AND (task_id IS NOT NULL OR run_id IS NOT NULL)",
        [], |row| row.get(0),
    ).unwrap();
    assert_eq!(not_queue_rows, 0);
    let control_calls = || {
        calls(&program)
            .into_iter()
            .filter(|call| !call.starts_with("--show-stats"))
            .collect::<Vec<_>>()
    };
    // Before-worker and before-review checks wait after the failure, rather
    // than trying another start immediately in the same supervisor.
    assert_eq!(
        control_calls(),
        [
            format!("--stop-server idle=0 path={path}"),
            format!("--start-server idle=0 path={path}")
        ]
    );
    assert!(queue_events(&db, "sccache_server_started").is_empty());
    // A new supervisor keeps the persisted fault while the server is absent.
    // A second failed start takes ensure_sccache's missing-server retry branch.
    assert_eq!(supervise()["errors"], json!([]));
    assert_eq!(queue_events(&db, "sccache_server_restart_failed").len(), 1);
    assert_eq!(
        queue_events(&db, "sccache_server_start_failed")[0]["reason"],
        "restart"
    );
    fs::remove_file(bin.join("fail-start")).unwrap();
    assert_eq!(supervise()["errors"], json!([]));
    let started = queue_events(&db, "sccache_server_started");
    assert_eq!(started.len(), 1);
    let latest_supervisor =
        queue_events(&db, "supervisor_started").pop().unwrap()["supervisor"].clone();
    assert_eq!(started[0]["reason"], "restart");
    assert_eq!(started[0]["supervisor"], latest_supervisor);
    assert_eq!(started[0]["supervisor_pid"], std::process::id());
    assert_eq!(started[0]["idle_timeout"], "0");
    assert_eq!(started[0]["pid"], 4343);
    assert_eq!(control_calls().len(), 4); // one stop, three starts
    let health = dagq::application::health::status(
        &SqliteQueue::open(&db).unwrap(),
        &WrapperProcesses,
        &SystemClock,
        None,
    )
    .unwrap();
    assert!(
        health["attention"]
            .as_array()
            .unwrap()
            .iter()
            .all(|entry| entry["kind"] != "sccache_server_restart_failed")
    );
}

/// The guard of `dir`: a link to dagq by its name, as the wrapper and the
/// supervisor make it in a run's directory.
fn guard_link(dir: &Path) -> PathBuf {
    let guard = dir.join(GUARD_NAME);
    std::os::unix::fs::symlink(env!("CARGO_BIN_EXE_dagq"), &guard).unwrap();
    guard
}

/// Run `guard` as cargo runs `RUSTC_WRAPPER`, with `rustc` and its
/// arguments, for the sccache `program` on `port`. The environment does
/// not refuse the server's start: the guard does.
fn compile(guard: &Path, program: &Path, port: u16, rustc: &Path) -> std::process::Output {
    let _running = common::within(common::STEP_LIMIT, "the guard's compile");
    std::process::Command::new(guard)
        .arg(rustc)
        .args(["--crate-name", "stub", "-"])
        .env("DAGQ_SCCACHE_PROGRAM", program)
        .env("SCCACHE_SERVER_PORT", port.to_string())
        .env_remove("SCCACHE_ERROR_LOG")
        .output()
        .unwrap()
}

/// The sccache calls that were compiles (no flag first).
fn compiles(program: &Path) -> Vec<String> {
    calls(program)
        .into_iter()
        .filter(|call| !call.starts_with('-'))
        .collect()
}

#[test]
fn the_guard_compiles_through_a_server_that_listens() {
    let dir = tempfile::tempdir().unwrap();
    let (program, _) = stub_sccache(dir.path(), "sccache");
    let rustc = stub_rustc(&program);
    let guard = guard_link(dir.path());
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
    let port = listener.local_addr().unwrap().port();
    fs::write(program.with_file_name("server-up"), "").unwrap();

    let output = compile(&guard, &program, port, &rustc);
    assert!(output.status.success(), "{output:?}");
    assert_eq!(String::from_utf8_lossy(&output.stdout), "rustc out\n");
    assert_eq!(compiles(&program).len(), 1);
    assert_eq!(
        lines(&program.with_file_name("rustc-calls.log")),
        ["via=1 --crate-name stub -"]
    );
    assert!(lines(&program.with_file_name("server-starts.log")).is_empty());

    // The compiler's own failure through the server is the compile's:
    // said by the compiler, and not run again.
    fs::write(program.with_file_name("rustc-fails"), "").unwrap();
    let output = compile(&guard, &program, port, &rustc);
    assert_eq!(output.status.code(), Some(1), "{output:?}");
    assert!(String::from_utf8_lossy(&output.stderr).contains("error[E0425]"));
    assert_eq!(lines(&program.with_file_name("rustc-calls.log")).len(), 2);
    drop(listener);
}

#[test]
fn a_server_stopped_before_the_compile_is_not_started_and_the_compiler_runs() {
    let dir = tempfile::tempdir().unwrap();
    let (program, _) = stub_sccache(dir.path(), "sccache");
    let rustc = stub_rustc(&program);
    let guard = guard_link(dir.path());
    // The server stopped during the turn: nothing listens.
    let port = free_port();

    let output = compile(&guard, &program, port, &rustc);
    assert!(output.status.success(), "{output:?}");
    assert_eq!(String::from_utf8_lossy(&output.stdout), "rustc out\n");
    assert!(calls(&program).is_empty(), "{:?}", calls(&program));
    assert_eq!(
        lines(&program.with_file_name("rustc-calls.log")),
        ["via=direct --crate-name stub -"]
    );
    assert!(lines(&program.with_file_name("server-starts.log")).is_empty());
}

#[test]
fn a_server_that_stops_after_the_guards_look_is_refused_its_start() {
    let dir = tempfile::tempdir().unwrap();
    let (program, _) = stub_sccache(dir.path(), "sccache");
    let rustc = stub_rustc(&program);
    let guard = guard_link(dir.path());
    // The port still answers the guard's look, but the client finds no
    // server: it stopped in between.
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
    let port = listener.local_addr().unwrap().port();

    let output = compile(&guard, &program, port, &rustc);
    assert!(output.status.success(), "{output:?}");
    // Only the compiler's output: the client's error was not the compile's.
    assert_eq!(String::from_utf8_lossy(&output.stdout), "rustc out\n");
    assert!(
        !String::from_utf8_lossy(&output.stderr).contains("sccache"),
        "{output:?}"
    );
    assert_eq!(compiles(&program).len(), 1);
    // The client tried to start a server, which exited before it listened.
    assert_eq!(
        lines(&program.with_file_name("server-starts.log")),
        ["refused"]
    );
    assert_eq!(
        lines(&program.with_file_name("rustc-calls.log")),
        ["via=direct --crate-name stub -"]
    );
    drop(listener);
}

/// Wait (bounded) for `path` to hold at least `count` lines.
fn await_lines(path: &Path, count: usize, what: &str) {
    let _waiting = common::within(common::STEP_LIMIT, what);
    while lines(path).len() < count {
        thread::sleep(Duration::from_millis(10));
    }
}

/// Wait (bounded) for `path` to be there.
fn await_path(path: &Path, what: &str) {
    let _waiting = common::within(common::STEP_LIMIT, what);
    while !path.exists() {
        thread::sleep(Duration::from_millis(10));
    }
}

/// Have the turn or job ask the test to stop the server (`stop`), wait
/// until it did (`stopped`; the wait ends with the test), and compile
/// through its `RUSTC_WRAPPER` as cargo does, as `name`, its status to
/// `compile-status.log`.
fn compile_after(dir: &Path, rustc: &Path, stop: &str, stopped: &str, name: &str) -> String {
    format!(
        ": > {stop}\n{wait}\n\"$RUSTC_WRAPPER\" {rustc} --crate-name {name} > /dev/null 2>&1\necho $? >> {status}\n",
        stop = common::shell_path(dir.join(stop)),
        wait = common::await_path(dir.join(stopped)),
        rustc = common::shell_path(rustc),
        status = common::shell_path(dir.join("compile-status.log")),
    )
}

#[test]
fn a_server_that_stops_during_a_turn_or_a_review_is_not_started_from_the_sandbox() {
    let (dir, repo, db, mut backend, codex) = codex_fixture();
    backend.wrapper_processes = Some(Arc::new(WrapperProcesses));
    let (program, path) = stub_sccache(dir.path(), "sccache");
    let rustc = stub_rustc(&program);
    let port = free_port();
    configure(&repo, &program, port, &path);
    backend.sccache = Some(SccacheTarget {
        program: program.display().to_string(),
        port,
    });
    backend.inherited_env = vec![
        ("RUSTC_WRAPPER".into(), program.display().to_string()),
        ("SCCACHE_SERVER_PORT".into(), port.to_string()),
    ];
    // The turn: the server stops (nothing listens any more), then it
    // compiles. The review: the server stops after the guard's look (the
    // port still answers, the client finds no server), then it compiles.
    set_turns(
        dir.path(),
        &format!(
            "{}{FINISH}",
            compile_after(
                dir.path(),
                &rustc,
                "stop-listener",
                "listener-stopped",
                "turn"
            )
        ),
    );
    fs::write(
        dir.path().join("codex-review-hook.sh"),
        compile_after(
            dir.path(),
            &rustc,
            "stop-server",
            "server-stopped",
            "review",
        ),
    )
    .unwrap();

    // The host's server, as the supervisor's starts make it listen.
    let done = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let controller = {
        let (done, dir, program) = (done.clone(), dir.path().to_path_buf(), program.clone());
        thread::spawn(move || {
            let up = program.with_file_name("server-up");
            let starts = |count| {
                let _waiting = common::within(common::STEP_LIMIT, "a start of the server");
                while calls(&program)
                    .iter()
                    .filter(|call| call.starts_with("--start-server"))
                    .count()
                    < count
                {
                    thread::sleep(Duration::from_millis(10));
                }
            };
            starts(1);
            let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, port)).unwrap();
            fs::write(&up, "").unwrap();
            await_path(&dir.join("stop-listener"), "the turn to ask for the stop");
            drop(listener);
            fs::remove_file(&up).unwrap();
            fs::write(dir.join("listener-stopped"), "").unwrap();
            // Started again only after the turn compiled, by the
            // supervisor's next look or its look before the review.
            await_lines(&dir.join("compile-status.log"), 1, "the turn's compile");
            starts(2);
            let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, port)).unwrap();
            fs::write(&up, "").unwrap();
            await_path(&dir.join("stop-server"), "the review to ask for the stop");
            fs::remove_file(&up).unwrap();
            fs::write(dir.join("server-stopped"), "").unwrap();
            while !done.load(Ordering::SeqCst) {
                thread::sleep(Duration::from_millis(10));
            }
            drop(listener);
        })
    };
    let stop = StopOnDrop(done);
    let result = supervise_codex(dir.path(), &repo, &db, &backend, codex);
    drop(stop);
    joined(controller, "the stub server controller");
    assert_eq!(result["runs"][0]["status"], "integrated", "{result}");

    // Both compiled, by the compiler itself, uncached and correct.
    assert_eq!(lines(&dir.path().join("compile-status.log")), ["0", "0"]);
    assert_eq!(
        lines(&program.with_file_name("rustc-calls.log")),
        [
            "via=direct --crate-name turn",
            "via=direct --crate-name review"
        ]
    );
    // The turn's guard found nothing listening and ran no sccache; the
    // review's sccache found no server and its start was refused. No
    // server was started from the sandbox; every start was the
    // supervisor's.
    assert_eq!(compiles(&program).len(), 1, "{:?}", calls(&program));
    assert!(compiles(&program)[0].contains("--crate-name review"));
    assert_eq!(
        lines(&program.with_file_name("server-starts.log")),
        ["refused"]
    );
    for call in calls(&program)
        .iter()
        .filter(|call| call.starts_with("--start-server"))
    {
        assert_eq!(*call, format!("--start-server idle=0 path={path}"));
    }
    // Both had their server confirmed when they started: no wrapper was
    // removed, and both ran through the guard.
    let detail = detail(&db);
    assert!(payloads(&detail, "sccache_wrapper_removed").is_empty());
    let guard = Path::new(detail.runs[0].run_dir().unwrap()).join(GUARD_NAME);
    let review = fs::read_to_string(dir.path().join("codex-review-wrapper.log")).unwrap();
    assert_eq!(review.trim(), guard.display().to_string());
}
