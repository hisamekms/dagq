//! Runtime tests: the processes of the headless jobs (task 443). A job's
//! pid is recorded in `headless_jobs`; a supervisor that takes over from
//! one that died stops the job it left before starting its own, never a
//! process that took the pid later, and a job's timeout stops what the job
//! started too.
use crate::runtime_support;

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
/// `headless_job_stopped` with the pid, the kind and the run; only its own
/// review runs then, and the run lands. A row of the dead supervisor whose
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
            "validation_finished",
            json!({"status": "awaiting_integration"}),
        ),
        ("review_started", json!({"attempt": 1})),
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
    assert!(running(other));
    assert!(running(asleep_job));
    for pid in [other, asleep, asleep_job] {
        kill(pid);
    }
    let detail = queue.show(TaskId::new(1)).unwrap();
    assert_landed(&repo, &detail.runs[0], "test task", &base);
    let stopped = payloads(&detail, "headless_job_stopped");
    assert_eq!(stopped.len(), 1, "{:?}", event_kinds(&detail));
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
    assert_eq!(rows.len(), 5, "{rows:?}");
    let outcomes: Vec<_> = rows.iter().map(|r| r.5.as_deref()).collect();
    assert_eq!(
        outcomes,
        [
            Some("taken_over"),
            Some("not_the_job"),
            Some("gone"),
            None,
            Some("ended")
        ]
    );
    let (kind, run_id, attempt, _, token, _) = &rows[4];
    assert_eq!(kind, "review");
    assert_eq!(run_id.as_deref(), Some(run.id().as_str()));
    assert_eq!(*attempt, 2);
    assert_ne!(token, "dead-supervisor");
}

/// A review that times out is stopped with what it started: the process
/// its shell left in the background does not outlive the job, and the
/// job's row is closed as stopped.
#[test]
fn a_timed_out_review_stops_the_processes_it_started() {
    let (_dir, repo, db) = fixture();
    let backend = TestWorkspace::new(&db, false, IDLE_AGENT);
    // Beside the queue, whose own name has a quote.
    let pid_file = db.parent().unwrap().join("review-child.pid");
    let mut reviewer = TestReviewer::new(&[format!(
        "sleep 120 & echo $! > '{}'; wait",
        pid_file.display()
    )]);
    reviewer.timeout = Duration::from_secs(1);
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
