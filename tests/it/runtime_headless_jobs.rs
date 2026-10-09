//! Runtime tests: the processes of the headless jobs (task 443). A job's
//! pid is recorded in `headless_jobs`; a supervisor that takes over from
//! one that died stops the job it left before starting its own, never a
//! process that took the pid later, and a job's timeout stops what the job
//! started too, whatever the job's kind (an agent's or a program's).
use crate::runtime_support;
use dagq::application::review_programs::SnapshotProgram;
use dagq::application::supervise::{
    JobEnds, JobFailed, JobPorts, JobSubject, PROGRAM_OUTPUT_TAIL, ProgramEnd, start_program_job,
    start_review_program,
};
use dagq::domain::review_programs::{ProgramRun, ReviewProgram};
use dagq::domain::{EventKind, LeaseToken};
use dagq::infrastructure::adapters::SystemProcesses;
use dagq::infrastructure::review_programs::HostPrograms;
use std::ffi::OsString;

use runtime_support::*;

/// The start of `pid`'s process as the runtime reads it.
fn process_start(pid: u32) -> String {
    let out = Command::new("ps")
        .env("LC_ALL", "C")
        .args(["-o", "lstart=", "-p", &pid.to_string()])
        .bounded_output()
        .unwrap();
    String::from_utf8(out.stdout).unwrap().trim().to_owned()
}

/// Start `script` under `sh` as a process whose parent is gone, the way a
/// job outlives the supervisor that started it; its pid.
fn orphan(script: &str) -> u32 {
    let out = Command::new("/bin/sh")
        .args([
            "-c",
            &format!("/bin/sh -c '{script}' </dev/null >/dev/null 2>&1 & echo $!"),
        ])
        .bounded_output()
        .unwrap();
    String::from_utf8(out.stdout)
        .unwrap()
        .trim()
        .parse()
        .unwrap()
}

/// The processes whose parent is `pid`.
fn children(pid: u32) -> Vec<u32> {
    let out = Command::new("ps")
        .args(["-A", "-o", "pid=,ppid="])
        .bounded_output()
        .unwrap();
    String::from_utf8(out.stdout)
        .unwrap()
        .lines()
        .filter_map(|line| {
            let mut fields = line.split_whitespace();
            let child: u32 = fields.next()?.parse().ok()?;
            let parent: u32 = fields.next()?.parse().ok()?;
            (parent == pid).then_some(child)
        })
        .collect()
}

fn kill(pid: u32) {
    // SAFETY: kill(2) takes no pointer.
    unsafe { libc::kill(pid as libc::pid_t, libc::SIGKILL) };
}

/// A `headless_jobs` row: kind, run, attempt, pid, supervisor, outcome.
type JobRow = (String, Option<String>, i64, u32, String, Option<String>);

fn job_rows(db: &Path) -> Vec<JobRow> {
    Connection::open(db)
        .unwrap()
        .prepare(
            "SELECT kind, run_id, attempt, pid, supervisor_token, outcome FROM headless_jobs ORDER BY id",
        )
        .unwrap()
        .query_map([], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?))
        })
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap()
}

/// A supervisor died while the review of its run ran (the stub job never
/// ends). The supervisor that adopts the run stops that job, the process
/// the job started with it, before it starts its own review, and records
/// `headless_job_stopped` with the pid, the kind and the run, and a
/// program job of the review (ADR-t1895-1 decision 1) the same way; only
/// its own review runs then, and the run lands. A row of the dead
/// supervisor whose
/// pid runs another process now (a pid used again) is closed without a
/// signal, and one whose process is gone is only closed.
#[test]
fn an_adopter_stops_the_review_a_dead_supervisor_left_and_only_its_own_runs() {
    let (_dir, repo, db) = fixture();
    let base = git_out(&repo, &["rev-parse", "main"]);
    let backend = Arc::new(TestWorkspace::new(&db, false, IDLE_AGENT));
    let run = start_run_under_dead_supervisor(&repo, &db, &backend, "dead-supervisor");
    let idle = run.idle_marker_path().unwrap();
    wait_until(&db, Duration::from_secs(20), |_| idle.is_file());
    let head = git_out(
        Path::new(run.worktree_path().unwrap()),
        &["rev-parse", "HEAD"],
    );
    // The dead supervisor validated the run and started its review, whose
    // job never ends and has a process of its own.
    let conn = Connection::open(&db).unwrap();
    conn.execute(
        "UPDATE task_runs SET status='awaiting_integration', result_commit=?2 WHERE id=?1",
        rusqlite::params![run.id(), head],
    )
    .unwrap();
    let mut queue = SqliteQueue::open(&db).unwrap();
    for (kind, payload) in [
        (
            EventKind::ValidationFinished,
            json!({"status": "awaiting_integration"}),
        ),
        (EventKind::ReviewStarted, json!({"attempt": 1})),
    ] {
        queue.record_runtime_event(run.id(), kind, payload).unwrap();
    }
    let old_job = orphan("sleep 120 & wait");
    let deadline = Instant::now() + Duration::from_secs(10);
    let old_child = loop {
        if let Some(&child) = children(old_job).first() {
            break child;
        }
        assert!(
            Instant::now() < deadline,
            "the old job's child never started"
        );
        thread::sleep(Duration::from_millis(20));
    };
    // Another process that took the pid of a job of the dead supervisor.
    let other = orphan("sleep 120");
    let insert = |kind: &str, pid: u32, start: &str| {
        conn.execute(
            "INSERT INTO headless_jobs(kind, run_id, attempt, pid, process_start, supervisor_token, started_at)
             VALUES (?1, ?2, 1, ?3, ?4, 'dead-supervisor', unixepoch())",
            rusqlite::params![kind, run.id(), pid, start],
        )
        .unwrap();
    };
    insert("review", old_job, &process_start(old_job));
    let program_job = orphan("sleep 120 & wait");
    conn.execute(
        "INSERT INTO headless_jobs(kind, label, run_id, attempt, pid, process_start, supervisor_token, started_at, provider)
         VALUES ('review_program', 'fmt', ?1, 1, ?2, ?3, 'dead-supervisor', unixepoch(), 'none')",
        rusqlite::params![run.id(), program_job, process_start(program_job)],
    )
    .unwrap();
    insert("recovery", other, "Thu Jan  1 00:00:00 1970");
    insert("recovery", u32::MAX / 2, "Thu Jan  1 00:00:00 1970");
    // A supervisor whose heartbeat went stale while its process lives (a
    // host that just woke up): its job is left to it.
    let asleep = orphan("sleep 120");
    let asleep_job = orphan("sleep 120");
    conn.execute(
        "INSERT INTO supervisors(token, pid, parallel, heartbeat_at) VALUES ('asleep', ?1, 1, unixepoch() - 600)",
        [asleep],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO headless_jobs(kind, proposal_id, attempt, pid, process_start, supervisor_token, started_at)
         VALUES ('plan_review', 7, 1, ?1, ?2, 'asleep', unixepoch())",
        rusqlite::params![asleep_job, process_start(asleep_job)],
    )
    .unwrap();
    age_lease(&db, &run, 31);
    let reviewer = TestReviewer::new(&[verdict("pass", &[], "fine")]);
    let outcome = supervise_reviewed(&db, &repo, &backend, &reviewer);
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    assert_eq!(outcome["runs"][0]["status"], "integrated", "{outcome}");

    // The old job and its child are gone; the other process runs on.
    assert!(!running(old_job));
    assert!(!running(old_child));
    assert!(!running(program_job));
    assert!(running(other));
    assert!(running(asleep_job));
    for pid in [other, asleep, asleep_job] {
        kill(pid);
    }
    let detail = queue.show(TaskId::new(1)).unwrap();
    assert_landed(&repo, &detail.runs[0], "test task", &base);
    let stopped = payloads(&detail, "headless_job_stopped");
    assert_eq!(stopped.len(), 2, "{:?}", event_kinds(&detail));
    assert_eq!(stopped[1]["pid"], program_job);
    assert_eq!(stopped[1]["kind"], "review_program");
    assert_eq!(stopped[1]["label"], "fmt");
    assert_eq!(stopped[0]["pid"], old_job);
    assert_eq!(stopped[0]["kind"], "review");
    assert_eq!(stopped[0]["run_id"], run.id().as_str());
    assert_eq!(stopped[0]["attempt"], 1);
    assert_eq!(stopped[0]["supervisor"], "dead-supervisor");
    assert_eq!(stopped[0]["descendants"], json!([old_child]));
    // The old job was stopped before the adopter's review started, and only
    // that one review ran.
    let at = |kind: &str, attempt: i64| {
        detail
            .events
            .iter()
            .position(|e| e.kind == kind && e.payload["attempt"] == attempt)
            .unwrap()
    };
    assert!(at("headless_job_stopped", 1) < at("review_started", 2));
    assert_eq!(reviewer.prompts().len(), 1);
    let rows = job_rows(&db);
    assert_eq!(rows.len(), 6, "{rows:?}");
    let outcomes: Vec<_> = rows.iter().map(|r| r.5.as_deref()).collect();
    assert_eq!(
        outcomes,
        [
            Some("taken_over"),
            Some("taken_over"),
            Some("not_the_job"),
            Some("gone"),
            None,
            Some("ended")
        ]
    );
    let (kind, run_id, attempt, _, token, _) = &rows[5];
    assert_eq!(kind, "review");
    assert_eq!(run_id.as_deref(), Some(run.id().as_str()));
    assert_eq!(*attempt, 2);
    assert_ne!(token, "dead-supervisor");
}

/// A review that times out is stopped with what it started: the process
/// its shell left in the background does not outlive the job, and the
/// job's row is closed as stopped. Its timeout is `[review.jobs]
/// agent_timeout_secs` of the repository's `dagq.toml`, not the provider's
/// (ADR-t1895-1 decision 1).
#[test]
fn a_timed_out_review_stops_the_processes_it_started() {
    let (_dir, repo, db) = fixture();
    fs::write(
        repo.join("dagq.toml"),
        "[review.jobs]\nagent_timeout_secs = 1\n",
    )
    .unwrap();
    git(&repo, &["add", "dagq.toml"]);
    git(&repo, &["commit", "-m", "review jobs"]);
    let backend = TestWorkspace::new(&db, false, IDLE_AGENT);
    // Beside the queue, whose own name has a quote.
    let pid_file = db.parent().unwrap().join("review-child.pid");
    let mut reviewer = TestReviewer::new(&[format!(
        "sleep 120 & echo $! > {}; wait",
        shell_path(&pid_file)
    )]);
    // The provider's own timeout is far longer.
    reviewer.timeout = Duration::from_secs(60);
    let outcome = supervise_reviewed(&db, &repo, &backend, &reviewer);
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    let child: u32 = fs::read_to_string(&pid_file)
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    let detail = SqliteQueue::open(&db)
        .unwrap()
        .show(TaskId::new(1))
        .unwrap();
    let failed = payloads(&detail, "review_failed");
    assert!(
        failed[0]["error"]
            .as_str()
            .unwrap()
            .contains("did not finish within 1 seconds"),
        "{failed:?}"
    );
    let deadline = Instant::now() + Duration::from_secs(5);
    while running(child) && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(20));
    }
    assert!(!running(child), "the review's child {child} outlived it");
    let rows = job_rows(&db);
    assert_eq!(rows.len(), 1, "{rows:?}");
    assert_eq!(rows[0].0, "review");
    assert_eq!(rows[0].2, 1);
    assert_eq!(rows[0].5.as_deref(), Some("stopped"));
}

/// A program job's `headless_jobs` row: kind, label, provider, outcome,
/// supervisor.
type ProgramRow = (String, Option<String>, String, Option<String>, String);

/// Poll `job` until it ends, at most 20 seconds.
fn ended(job: &mut dagq::application::supervise::HeadlessJob) -> Result<String, JobFailed> {
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        if let Some(end) = job.poll_output(&LocalRunFiles).unwrap() {
            return end;
        }
        assert!(Instant::now() < deadline, "the program job never ended");
        thread::sleep(Duration::from_millis(20));
    }
}

/// A program job of a run's review (ADR-t1895-1 decision 1) runs as a
/// child process in a group of its own, on the common path of every job:
/// it is recorded as `review_program` with the program's name and no
/// provider; one that ends gives its stdout and its row ends as `ended`;
/// one past its timeout is stopped with the processes it started, and its
/// row ends as `stopped`.
#[test]
fn a_program_job_is_recorded_and_its_timeout_stops_what_it_started() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("q.db");
    let queue = SqliteQueue::init(&db).unwrap();
    let ends = JobEnds::default();
    let token = LeaseToken::new("program-supervisor");
    let ports = JobPorts {
        store: &queue,
        processes: Arc::new(SystemProcesses),
        supervisor_token: &token,
        clock: &SystemClock,
        ends: &ends,
    };
    let run = RunId::new("run-1").unwrap();
    let output = |name: &str| {
        (
            dir.path().join(format!("{name}.out")),
            dir.path().join(format!("{name}.err")),
        )
    };
    let start = |script: &str, name: &str, timeout: Duration| {
        let mut program = CommandSpec::new("/bin/sh");
        program.args(["-c", script]);
        start_program_job(
            &ports,
            &process::LocalSpawner,
            &program,
            output(name),
            JobSubject::review_program(&run, 1, name),
            timeout,
        )
        .unwrap()
    };
    let mut done = start("echo formatted", "fmt", Duration::from_secs(60));
    assert_eq!(ended(&mut done).unwrap(), "formatted\n");
    // A child, and a grandchild whose parent (a subshell) exits at once:
    // it leaves the job's descendants but stays in the job's process
    // group, so only the group's stop reaches it.
    let pid_file = dir.path().join("child.pid");
    let orphan_file = dir.path().join("orphan.pid");
    let mut slow = start(
        &format!(
            "(sleep 120 & echo $! > {}); sleep 120 & echo $! > {}; wait",
            shell_path(&orphan_file),
            shell_path(&pid_file)
        ),
        "slow",
        Duration::from_secs(2),
    );
    let deadline = Instant::now() + Duration::from_secs(10);
    let read_pid =
        |file: &Path| -> Option<u32> { fs::read_to_string(file).ok()?.trim().parse().ok() };
    while (read_pid(&pid_file).is_none() || read_pid(&orphan_file).is_none())
        && Instant::now() < deadline
    {
        thread::sleep(Duration::from_millis(20));
    }
    let orphan = read_pid(&orphan_file).expect("the program's grandchild never started");
    let failed = ended(&mut slow).unwrap_err();
    assert!(matches!(failed, JobFailed::TimedOut(_)), "{failed:?}");
    assert!(
        failed
            .error()
            .contains("review program did not finish within 2 seconds"),
        "{failed:?}"
    );
    let child: u32 = fs::read_to_string(&pid_file)
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    while running(child) && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(20));
    }
    assert!(!running(child), "the program's child {child} outlived it");
    while running(orphan) && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(20));
    }
    assert!(
        !running(orphan),
        "the program's grandchild {orphan} outside its descendants outlived its group"
    );
    assert!(!running(slow.pid()));
    let unwritten: Vec<_> = ends.write(&queue).into_iter().map(|(id, _)| id).collect();
    assert!(unwritten.is_empty(), "{unwritten:?}");
    let rows: Vec<ProgramRow> =
        Connection::open(&db)
            .unwrap()
            .prepare(
                "SELECT kind, label, provider, outcome, supervisor_token FROM headless_jobs ORDER BY id",
            )
            .unwrap()
            .query_map([], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?))
            })
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
    let row = |label: &str, outcome: &str| {
        (
            "review_program".to_owned(),
            Some(label.to_owned()),
            "none".to_owned(),
            Some(outcome.to_owned()),
            "program-supervisor".to_owned(),
        )
    };
    assert_eq!(rows, [row("fmt", "ended"), row("slow", "stopped")]);
}

/// Poll the program job `job` until it ends, at most 20 seconds.
fn program_ended(job: &mut dagq::application::supervise::HeadlessJob) -> ProgramEnd {
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        if let Some(end) = job.poll_program(&LocalRunFiles).unwrap() {
            return end;
        }
        assert!(Instant::now() < deadline, "the program job never ended");
        thread::sleep(Duration::from_millis(20));
    }
}

/// A program review runs on the host as a program job (ADR-t1895-2): in
/// the run's worktree, from the script's text at the landing branch's
/// commit (not the worktree's copy), with only the narrowed environment
/// (no cmux socket password, no `CMUX_*`, nothing that reaches the queue
/// service or a broker); its end gives the exit status and the end of its
/// output, and one past its time limit is stopped with its process group.
#[test]
fn a_review_program_runs_narrowed_in_the_worktree_and_is_stopped_past_its_limit() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("q.db");
    let queue = SqliteQueue::init(&db).unwrap();
    let ends = JobEnds::default();
    let token = LeaseToken::new("program-supervisor");
    let ports = JobPorts {
        store: &queue,
        processes: Arc::new(SystemProcesses),
        supervisor_token: &token,
        clock: &SystemClock,
        ends: &ends,
    };
    let worktree = dir.path().join("worktree");
    fs::create_dir_all(worktree.join("scripts")).unwrap();
    fs::write(
        worktree.join("scripts/check.sh"),
        "#!/bin/sh\necho the worker's check\n",
    )
    .unwrap();
    let run_dir = dir.path().join("run");
    fs::create_dir_all(&run_dir).unwrap();
    let scratch = dir.path().join("scratch");
    let mut inherited: Vec<(OsString, OsString)> = std::env::vars_os()
        .filter(|(name, _)| name == "PATH" || name == "HOME")
        .collect();
    for (name, value) in [
        ("CMUX_SOCKET_PASSWORD", "socket-password"),
        ("CMUX_SOCKET_PATH", "/tmp/cmux.sock"),
        ("DAGQ_SERVICE_SOCKET", "/q/service.sock"),
        ("DAGQ_SERVICE_CREDENTIAL_FILE", "/q/credential"),
        ("DAGQ_BROKER_URL", "http://127.0.0.1:1"),
        ("DAGQ_BROKER_TOKEN_FILE", "/q/broker-token"),
        ("DAGQ_QUEUE", "/q/queue.db"),
        ("GH_TOKEN", "gh"),
        ("LC_ALL", "C"),
    ] {
        inherited.push((name.into(), value.into()));
    }
    let backend = HostPrograms::inheriting(inherited);
    let run = RunId::new("run-1").unwrap();
    let start = |name: &str, run_as: ProgramRun, script: Option<&str>, timeout: Option<u64>| {
        let program = SnapshotProgram {
            program: ReviewProgram {
                name: name.to_owned(),
                run: run_as,
                paths: vec!["**".to_owned()],
                timeout_secs: timeout,
            },
            matched: vec!["a".to_owned()],
            script: script.map(str::to_owned),
        };
        start_review_program(
            &ports,
            &process::LocalSpawner,
            &backend,
            &program,
            &worktree,
            (&run_dir, &scratch),
            (&run, 1),
            Duration::from_secs(60),
        )
        .unwrap()
    };
    let mut check = start(
        "check",
        ProgramRun::Script {
            path: "scripts/check.sh".to_owned(),
            args: vec!["arg".to_owned()],
        },
        Some("#!/bin/sh\necho main\\'s check \"$1\"; pwd -P; env | sort; echo oops >&2; exit 3\n"),
        None,
    );
    let end = program_ended(&mut check);
    assert_eq!(end.exit.as_ref().and_then(|exit| exit.code), Some(3));
    let mut lines = end.stdout_tail.lines();
    assert_eq!(lines.next(), Some("main's check arg"));
    assert_eq!(
        lines.next().map(PathBuf::from),
        Some(worktree.canonicalize().unwrap())
    );
    let names: Vec<&str> = lines
        .filter_map(|line| line.split_once('=').map(|(name, _)| name))
        .collect();
    // What the shell itself exports aside.
    let shells = ["PWD", "OLDPWD", "SHLVL", "_"];
    for name in names.iter().filter(|name| !shells.contains(name)) {
        assert!(["PATH", "HOME", "LC_ALL"].contains(name), "{name} given");
    }
    assert!(names.contains(&"LC_ALL"), "{names:?}");
    assert_eq!(end.stderr_tail, "oops\n");
    // Past its own limit: stopped with what it started in its group.
    let pid_file = dir.path().join("child.pid");
    let mut slow = start(
        "slow",
        ProgramRun::Command(vec![
            "/bin/sh".to_owned(),
            "-c".to_owned(),
            format!(
                "seq 1 2000; (sleep 120 & echo $! > {}); sleep 120",
                shell_path(&pid_file)
            ),
        ]),
        None,
        Some(3),
    );
    let end = program_ended(&mut slow);
    assert_eq!(end.exit, None);
    assert_eq!(end.stdout_tail.len(), PROGRAM_OUTPUT_TAIL);
    assert!(
        end.stdout_tail.ends_with("1999\n2000\n"),
        "{}",
        end.stdout_tail
    );
    let child: u32 = fs::read_to_string(&pid_file)
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    while running(child) && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(20));
    }
    assert!(
        !running(child),
        "the program's grandchild {child} outlived its group"
    );
    assert!(ends.write(&queue).is_empty());
    let outcomes: Vec<(String, Option<String>)> = Connection::open(&db)
        .unwrap()
        .prepare("SELECT label, outcome FROM headless_jobs ORDER BY id")
        .unwrap()
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap();
    assert_eq!(
        outcomes,
        [
            ("check".to_owned(), Some("ended".to_owned())),
            ("slow".to_owned(), Some("stopped".to_owned()))
        ]
    );
}
