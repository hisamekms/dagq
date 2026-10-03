//! Runtime tests: the throughput review (ADR-t996-1).
use crate::{common, runtime_support};
use dagq::domain::throughput_review::{HOUR_MS, ReviewMode, window};
use dagq::throughput_review::{PROMPT_LIMIT, ReviewOptions};

use runtime_support::*;

/// `review` with the queue's service running, which the job's `dagq`
/// reaches in client mode (goal 82's stage (3)); the fixture stops it.
pub(crate) fn review(
    db: &Path,
    provider: &dyn AgentProvider,
    options: &ReviewOptions,
) -> Result<Value> {
    common::service::serve(db);
    dagq::throughput_review::review(db, provider, options)
}

/// The review's provider double: the headless job is a shell script in the
/// review's directory, with the environment the command gives the agent.
/// Like Claude's `claude -p`, it takes the prompt as an argument, so a
/// prompt past the host's `ARG_MAX` fails to start (task 1099).
struct ReviewProvider {
    script: String,
    /// The program run instead of `/bin/sh`: one that does not exist
    /// fails to start.
    program: &'static str,
}

impl ReviewProvider {
    fn new(script: &str) -> Self {
        Self {
            script: script.into(),
            program: "/bin/sh",
        }
    }
}

impl AgentProvider for ReviewProvider {
    fn preflight(&self) -> Result<()> {
        Ok(())
    }
    fn command(&self, _: &TaskRun, _: &str) -> Result<CommandSpec> {
        bail!("the throughput review has no run")
    }
    fn resume_command(&self, _: &TaskRun) -> Result<CommandSpec> {
        bail!("the throughput review has no run")
    }
    fn headless_command(&self, cwd: &Path, prompt: &str, access: JobAccess) -> Result<CommandSpec> {
        assert!(
            prompt.contains("You are the throughput review job"),
            "{prompt}"
        );
        assert!(prompt.contains("Raising throughput: the weekly review"));
        assert_eq!(access, JobAccess::QueueCli);
        assert!(prompt.len() <= PROMPT_LIMIT, "{}", prompt.len());
        let mut command = CommandSpec::new(self.program);
        command
            .current_dir(cwd)
            .arg("-c")
            .arg(&self.script)
            .arg("--")
            .arg(prompt);
        Ok(command)
    }
    fn job_reply(&self, stdout: &str) -> String {
        if stdout.starts_with('{') {
            // A structured provider must receive stdout alone: diagnostics
            // on stderr would make this JSON invalid.
            serde_json::from_str::<Value>(stdout).unwrap()["reply"]
                .as_str()
                .unwrap()
                .to_owned()
        } else {
            stdout.to_owned()
        }
    }
    /// The script's `$0`, which it writes to `mcp.txt`.
    fn without_mcp(&self, command: &mut CommandSpec) {
        command.option_args(["no-mcp"]);
    }
    fn review_command(&self, _: &TaskRun, _: &str, _: JobAccess) -> Result<CommandSpec> {
        bail!("the throughput review reviews no run")
    }
}

/// 2026-09-29T04:25:00Z: the hours, days and weeks are UTC's here.
const AT: i64 = 1_790_655_900;

pub(crate) fn utc(_: i64) -> i64 {
    0
}

pub(crate) fn options(mode: ReviewMode) -> ReviewOptions {
    ReviewOptions {
        mode,
        at: Some(AT),
        dry_run: false,
        timeout: Duration::from_secs(60),
        dagq: PathBuf::from(env!("CARGO_BIN_EXE_dagq")),
        user_config: None,
        utc_offset: Some(0),
        launch: None,
        switchable: false,
        unavailable: None,
    }
}

pub(crate) fn queue_events(db: &Path, kind: &str) -> Vec<Value> {
    Connection::open(db)
        .unwrap()
        .prepare("SELECT payload FROM run_events WHERE kind=?1 ORDER BY id")
        .unwrap()
        .query_map([kind], |r| r.get::<_, String>(0))
        .unwrap()
        .map(|p| serde_json::from_str(&p.unwrap()).unwrap())
        .collect()
}

/// Record `per_hour[i]` landings of task 1 in each of the hours that end at
/// the last whole hour before [`AT`], the last hour last.
pub(crate) fn land(db: &Path, per_hour: &[usize]) {
    let end = window(ReviewMode::Hourly, AT * 1000, 0).end_ms;
    let connection = Connection::open(db).unwrap();
    for (index, count) in per_hour.iter().enumerate() {
        let hours_back = i64::try_from(per_hour.len() - index).unwrap();
        let start = end - hours_back * HOUR_MS;
        for n in 0..*count {
            let at = start + i64::try_from(n).unwrap() * 60_000 + 1000;
            connection
                .execute(
                    "INSERT INTO run_events(task_id,kind,payload,created_at) VALUES (1,'run_integrated','{}',?1)",
                    [dagq::domain::transcript::millis_text(at)],
                )
                .unwrap();
        }
    }
}

/// The agent prints its review after trying what the queue refuses it.
const REVIEWER: &str = r#"
printf '%s' "$DAGQ_ROLE" > role.txt
printf '%s' "$0" > mcp.txt
q() { dagq "$@" > /dev/null; }
if q note --task 1 --text 'seen' 2> note.err; then exit 3; fi
if q finding record --kind throughput --queue --summary 's' 2> finding.err; then exit 4; fi
if q mark 'faster' 2> mark.err; then exit 5; fi
if q ready 1 2> ready.err; then exit 6; fi
dagq kpi > kpi.json || exit 7
printf '## Conclusion\n- landings rose to 10 in the hour\n- nothing to do\n\n## Details\nthe numbers\n'
printf '```next_move\n{"summary": "split the e2e", "why": "verify is the constraint"}\n```\n'
"#;

#[test]
fn an_hour_no_rule_meets_starts_no_agent() {
    let (_dir, _repo, db) = fixture();
    land(&db, &[4; 28]);
    let provider = ReviewProvider::new("exit 9");
    let skipped = review(&db, &provider, &options(ReviewMode::Hourly)).unwrap();
    assert_eq!(skipped["outcome"], "skipped", "{skipped}");
    assert_eq!(skipped["period"], "2026-09-29T03");
    assert_eq!(skipped["hourly"]["landings"], 4);
    assert_eq!(skipped["hourly"]["triggered"], false);
    // Its pid and its parent's, by which a supervisor that exec'd reaps it.
    assert_eq!(skipped["pid"], json!(std::process::id()));
    assert_eq!(
        skipped["parent_pid"],
        json!(std::os::unix::process::parent_id())
    );
    assert!(queue_events(&db, "throughput_review_started").is_empty());
    assert_eq!(queue_events(&db, "throughput_review_finished").len(), 1);
    assert!(queue_events(&db, "throughput_review_reported").is_empty());
    // A dry run shows the prompt whatever the rules found.
    let dry = review(
        &db,
        &provider,
        &ReviewOptions {
            dry_run: true,
            ..options(ReviewMode::Hourly)
        },
    )
    .unwrap();
    assert!(
        dry["prompt"]
            .as_str()
            .unwrap()
            .contains("hourly review of the hour 2026-09-29T03")
    );
    assert_eq!(queue_events(&db, "throughput_review_finished").len(), 1);
}

#[test]
fn an_hour_a_rule_meets_is_reviewed_saved_and_told_to_the_inbox_and_the_job_only_reads() {
    let (_dir, _repo, db) = fixture();
    let mut hours = vec![4; 27];
    hours.push(10);
    land(&db, &hours);
    let provider = ReviewProvider::new(REVIEWER);
    let done = review(&db, &provider, &options(ReviewMode::Hourly)).unwrap();
    assert_eq!(done["outcome"], "succeeded", "{done}");
    assert_eq!(
        queue_events(&db, "throughput_review_finished"),
        vec![done.clone()]
    );
    assert_eq!(done["reasons"], json!(["deviation"]));
    let dir = PathBuf::from(done["dir"].as_str().unwrap());
    let reviews = db
        .canonicalize()
        .unwrap()
        .parent()
        .unwrap()
        .join("reports/reviews");
    assert_eq!(dir, reviews.join("hourly-2026-09-29T03"));
    assert_eq!(
        fs::read_to_string(dir.join("role.txt")).unwrap(),
        "throughput-review-job"
    );
    assert_eq!(fs::read_to_string(dir.join("mcp.txt")).unwrap(), "no-mcp");
    // The service refuses the note and the finding for the job's principal,
    // and a mark and `ready` are none of its use cases.
    for (denied, code) in [
        ("note.err", "authorization_denied"),
        ("finding.err", "authorization_denied"),
        ("mark.err", "no_use_case"),
        ("ready.err", "no_use_case"),
    ] {
        let error: Value =
            serde_json::from_str(&fs::read_to_string(dir.join(denied)).unwrap()).unwrap();
        assert_eq!(error["queue_service"]["code"], code, "{denied}: {error}");
    }
    // It read the KPIs.
    assert!(!fs::read_to_string(dir.join("kpi.json")).unwrap().is_empty());
    let input: Value =
        serde_json::from_str(&fs::read_to_string(dir.join("input.json")).unwrap()).unwrap();
    assert_eq!(input["landings"]["total"], 10);
    assert_eq!(input["landings"]["by_hour"].as_array().unwrap().len(), 26);
    for key in [
        "kpi",
        "stats",
        "claim_deferred",
        "asks",
        "timelines",
        "hourly",
        "health",
    ] {
        assert!(!input[key].is_null(), "{key}: {input}");
    }
    let saved = fs::read_to_string(dir.join("review.md")).unwrap();
    assert!(saved.contains("## Details"), "{saved}");
    assert!(!saved.contains("split the e2e"), "{saved}");
    let summary: Value =
        serde_json::from_str(&fs::read_to_string(dir.join("review.json")).unwrap()).unwrap();
    assert_eq!(
        summary["conclusion"],
        json!(["- landings rose to 10 in the hour", "- nothing to do"])
    );
    // Only the weekly review proposes: the hourly one's block is left out.
    assert_eq!(summary["finding_id"], Value::Null);
    // Its start records its session id and what it was launched with, the
    // provider included (task 1062).
    let started = queue_events(&db, "throughput_review_started");
    assert_eq!(started.len(), 1);
    assert!(started[0]["session_id"].is_string(), "{}", started[0]);
    assert_eq!(
        started[0]["launch"],
        json!({"role": "throughput_review", "provider": "claude", "model": null,
               "effort": null, "source": "default"})
    );
    // Its start opens its session's span and its finish, naming the same
    // session, closes it (task 1086); `stats` counts it under its kind.
    let session = &started[0]["session_id"];
    assert_eq!(
        queue_events(&db, "throughput_review_finished")[0]["session_id"],
        *session
    );
    let opened = queue_events(&db, "session_opened");
    assert_eq!(opened.len(), 1, "{opened:?}");
    assert_eq!(opened[0]["kind"], "throughput_review");
    assert_eq!(opened[0]["session_id"], *session);
    assert_eq!(opened[0]["mode"], "hourly");
    assert_eq!(opened[0]["period"], "2026-09-29T03");
    assert_eq!(opened[0]["cwd"], json!(dir));
    assert_eq!(opened[0]["launch"], started[0]["launch"]);
    let closed = queue_events(&db, "session_closed");
    assert_eq!(closed.len(), 1, "{closed:?}");
    assert_eq!(closed[0]["kind"], "throughput_review");
    assert_eq!(closed[0]["session_id"], *session);
    assert_eq!(closed[0]["reason"], "job_finished");
    let stats = crate::common::cli::ok(&db, &["stats", "--full"]);
    assert_eq!(
        stats["sessions"]["by_kind"]["throughput_review"]["count"], 1,
        "{stats}"
    );
    // Nothing changed state but the review's own record.
    let mut queue = SqliteQueue::open(&db).unwrap();
    assert_eq!(
        queue.show(TaskId::new(1)).unwrap().task.status(),
        TaskStatus::Ready
    );
    assert!(
        queue
            .findings(&dagq::domain::FindingQuery::default())
            .unwrap()
            .is_empty()
    );
    // The inbox's watch gets the notice with its conclusion.
    let attention = dagq::compose::events(&db, EventId::new(0), 100, false).unwrap();
    let notice = attention["events"]
        .as_array()
        .unwrap()
        .iter()
        .find(|event| event["kind"] == "throughput_review_reported")
        .unwrap_or_else(|| panic!("{attention}"))
        .clone();
    assert_eq!(notice["next"], "report the review");
    assert_eq!(notice["period"], "2026-09-29T03");
    assert_eq!(notice["mode"], "hourly");
    assert_eq!(notice["conclusion"][0], "- landings rose to 10 in the hour");
    assert_eq!(notice["path"], json!(dir.join("review.md")));
    // The skipped and started events are no attention.
    assert!(
        attention["events"]
            .as_array()
            .unwrap()
            .iter()
            .all(|event| event["kind"] != "throughput_review_finished")
    );
}

#[test]
fn the_weekly_review_records_its_next_move_as_a_finding_marked_for_a_proposal() {
    let (_dir, _repo, db) = fixture();
    let provider = ReviewProvider::new(REVIEWER);
    let done = review(&db, &provider, &options(ReviewMode::Weekly)).unwrap();
    assert_eq!(done["outcome"], "succeeded", "{done}");
    assert_eq!(done["period"], "2026-W39");
    let findings = SqliteQueue::open(&db)
        .unwrap()
        .findings(&dagq::domain::FindingQuery::default())
        .unwrap();
    assert_eq!(findings.len(), 1);
    let finding = &findings[0].finding;
    assert_eq!(finding.kind, "throughput");
    assert_eq!(finding.subject, "weekly/2026-W39");
    assert_eq!(finding.summary, "split the e2e");
    assert_eq!(
        finding.propose_reason.as_deref(),
        Some("verify is the constraint")
    );
    assert_eq!(finding.recorded_by, "supervisor");
    assert_eq!(done["finding_id"], json!(finding.id));
    let reported = queue_events(&db, "throughput_review_reported");
    assert_eq!(reported[0]["finding_id"], json!(finding.id));
    let input: Value = serde_json::from_str(
        &fs::read_to_string(PathBuf::from(done["dir"].as_str().unwrap()).join("input.json"))
            .unwrap(),
    )
    .unwrap();
    assert_eq!(input["landings"]["by_day"].as_array().unwrap().len(), 7);
    assert_eq!(input["hourly"], Value::Null);
}

/// The inbox's attention events: the compact form `watch` reads.
pub(crate) fn attentions(db: &Path) -> Vec<Value> {
    dagq::compose::events(db, EventId::new(0), 100, false).unwrap()["events"]
        .as_array()
        .unwrap()
        .clone()
}

#[test]
fn a_preparation_failure_is_finished_once_and_told_to_the_inbox() {
    let (_dir, _repo, db) = fixture();
    let root = dagq::throughput_review::reviews_dir(&db.canonicalize().unwrap());
    fs::create_dir_all(root.parent().unwrap()).unwrap();
    // A file where the reviews directory belongs fails deterministically,
    // including when the test user could override directory permissions.
    fs::write(&root, "not a directory").unwrap();
    let provider = ReviewProvider::new("exit 9");
    let mut opts = options(ReviewMode::Daily);
    opts.dry_run = true;
    assert_eq!(review(&db, &provider, &opts).unwrap()["dry_run"], true);
    assert!(queue_events(&db, "throughput_review_finished").is_empty());
    opts.dry_run = false;
    let done = review(&db, &provider, &opts).unwrap();
    assert_eq!(done["outcome"], "error", "{done}");
    assert_eq!(done["mode"], "daily");
    assert_eq!(done["period"], "2026-09-28");
    assert_eq!(done["exit_code"], Value::Null);
    assert_eq!(done["pid"], json!(std::process::id()));
    assert_eq!(
        done["parent_pid"],
        json!(std::os::unix::process::parent_id())
    );
    let error = done["error"].as_str().unwrap();
    assert!(
        error.contains(&format!("create {}:", root.display())),
        "{error}"
    );
    assert_eq!(
        queue_events(&db, "throughput_review_finished"),
        vec![done.clone()]
    );
    assert!(queue_events(&db, "throughput_review_started").is_empty());
    assert!(queue_events(&db, "throughput_review_reported").is_empty());
    let notices: Vec<_> = attentions(&db)
        .into_iter()
        .filter(|event| event["kind"] == "throughput_review_finished")
        .collect();
    assert_eq!(notices.len(), 1, "{notices:?}");
    assert_eq!(notices[0]["next"], "check the failed review");
    assert_eq!(notices[0]["reason"], done["error"]);
    assert!(
        SqliteQueue::open(&db)
            .unwrap()
            .asks(dagq::infrastructure::asks::AskQuery::default())
            .unwrap()
            .is_empty()
    );
}

#[test]
fn a_failed_review_is_recorded_and_told_to_the_inbox_as_a_notice() {
    let (_dir, _repo, db) = fixture();
    let provider = ReviewProvider::new("echo broken >&2; exit 2");
    let failed = review(&db, &provider, &options(ReviewMode::Daily)).unwrap();
    assert_eq!(failed["outcome"], "failed", "{failed}");
    assert_eq!(failed["exit_code"], 2);
    assert_eq!(failed["period"], "2026-09-28");
    let dir = PathBuf::from(failed["dir"].as_str().unwrap());
    assert!(
        fs::read_to_string(dir.join("output.err"))
            .unwrap()
            .contains("broken")
    );
    assert!(!dir.join("review.md").exists());
    assert!(queue_events(&db, "throughput_review_reported").is_empty());
    assert_eq!(
        queue_events(&db, "throughput_review_finished")[0]["outcome"],
        "failed"
    );
    // A job that cannot start ends `error` (task 1099: its prompt was past
    // `ARG_MAX`).
    let unstartable = ReviewProvider {
        program: "/nonexistent/claude",
        ..ReviewProvider::new("exit 0")
    };
    let error = review(&db, &unstartable, &options(ReviewMode::Weekly)).unwrap();
    assert_eq!(error["outcome"], "error", "{error}");
    assert!(
        error["error"].as_str().unwrap().contains("start"),
        "{error}"
    );
    assert_eq!(
        queue_events(&db, "throughput_review_finished"),
        vec![failed, error]
    );
    // Both reach the inbox as notices that ask nothing (task 1099).
    let notices: Vec<Value> = attentions(&db)
        .into_iter()
        .filter(|event| event["kind"] == "throughput_review_finished")
        .collect();
    assert_eq!(notices.len(), 2, "{notices:?}");
    for (notice, (mode, period, outcome)) in notices.iter().zip([
        ("daily", "2026-09-28", "failed"),
        ("weekly", "2026-W39", "error"),
    ]) {
        assert_eq!(notice["next"], "check the failed review", "{notice}");
        assert_eq!(notice["mode"], mode);
        assert_eq!(notice["period"], period);
        assert_eq!(notice["outcome"], outcome);
        assert!(notice["dir"].as_str().unwrap().contains("reports/reviews/"));
    }
    assert!(notices[1]["reason"].as_str().unwrap().contains("start"));
    assert!(
        SqliteQueue::open(&db)
            .unwrap()
            .asks(dagq::infrastructure::asks::AskQuery::default())
            .unwrap()
            .is_empty()
    );
}

/// Record `per_day` marks with long labels on each of the days from
/// 2026-09-21 to 2026-09-28 (UTC): `kpi` lists each period's marks, so the
/// review's inputs grow to a production day's MBs.
fn mark_the_days(db: &Path, per_day: usize) {
    let mut connection = Connection::open(db).unwrap();
    let batch = connection.transaction().unwrap();
    let start = window(ReviewMode::Daily, AT * 1000, 0).start_ms - 7 * 24 * HOUR_MS;
    for day in 0..8 {
        for n in 0..per_day {
            let at = start + day * 24 * HOUR_MS + i64::try_from(n).unwrap() * 60_000;
            batch
                .execute(
                    "INSERT INTO run_events(kind,payload,created_at) VALUES ('mark_recorded',?1,?2)",
                    [
                        json!({"label": format!("mark {day}/{n} {}", "x".repeat(800)), "note": null, "at": null, "by": "human"})
                            .to_string(),
                        dagq::domain::transcript::millis_text(at),
                    ],
                )
                .unwrap();
        }
    }
    batch.commit().unwrap();
}

#[test]
fn the_daily_and_weekly_reviews_of_inputs_of_mbs_start_their_agent_with_a_small_prompt() {
    let (_dir, _repo, db) = fixture();
    mark_the_days(&db, 150);
    let provider = ReviewProvider::new(REVIEWER);
    for mode in [ReviewMode::Daily, ReviewMode::Weekly] {
        let done = review(&db, &provider, &options(mode)).unwrap();
        assert_eq!(done["outcome"], "succeeded", "{done}");
        let dir = PathBuf::from(done["dir"].as_str().unwrap());
        // The agent started: it wrote its role.
        assert_eq!(
            fs::read_to_string(dir.join("role.txt")).unwrap(),
            "throughput-review-job"
        );
        let input = fs::read_to_string(dir.join("input.json")).unwrap();
        // The workers' health and the disk of the reviewed period, in the
        // prompt too (task 1371).
        let health = &serde_json::from_str::<Value>(&input).unwrap()["health"];
        assert!(health["routes"].is_object(), "{}: {health}", mode.as_str());
        assert!(health.get("disk").is_some(), "{}: {health}", mode.as_str());
        assert!(health["period"].is_string(), "{}: {health}", mode.as_str());
        assert!(
            input.len() > 1024 * 1024,
            "{}: {}",
            mode.as_str(),
            input.len()
        );
        let prompt = fs::read_to_string(dir.join("prompt.md")).unwrap();
        assert!(
            prompt.len() <= PROMPT_LIMIT,
            "{}: {}",
            mode.as_str(),
            prompt.len()
        );
        assert!(prompt.contains(&dir.join("input.json").display().to_string()));
        assert!(prompt.contains("\"health\""), "{}", mode.as_str());
    }
    assert_eq!(queue_events(&db, "throughput_review_reported").len(), 2);
}

/// A Claude Code stand-in for the supervisor's review: `--version` for the
/// preflight, and in print mode (`-p`, its prompt on stdin) the review, or
/// a failure when `fail` is set.
pub(crate) fn review_claude_stub(db: &Path, fail: bool) -> PathBuf {
    let stub = db.parent().unwrap().join("claude-review-stub");
    let review = if fail {
        "exit 1"
    } else {
        "printf '## Conclusion\\n- reviewed by %s\\n' \"$DAGQ_ROLE\"; exit 0"
    };
    crate::common::template::script(
        &stub,
        format!(
            "#!/bin/sh\n[ \"$1\" = -p ] && case \"$(cat)\" in *\"You are the throughput review job\"*) {review} ;; esac\nprintf 'test provider\\n'\n"
        ),
    );

    stub
}

fn now_secs() -> i64 {
    i64::try_from(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs(),
    )
    .unwrap()
}

/// The latest finished period of each mode at the unix second `at`, in
/// UTC as the tests' supervisor reads it.
fn periods_at(at: i64) -> Vec<(String, String)> {
    ReviewMode::ALL
        .iter()
        .map(|mode| (mode.as_str().to_owned(), window(*mode, at * 1000, 0).label))
        .collect()
}

/// The finished reviews, oldest first, as (mode, period, outcome).
fn finished_reviews(db: &Path) -> Vec<(String, String, String)> {
    queue_events(db, "throughput_review_finished")
        .iter()
        .map(|f| {
            (
                f["mode"].as_str().unwrap().to_owned(),
                f["period"].as_str().unwrap().to_owned(),
                f["outcome"].as_str().unwrap().to_owned(),
            )
        })
        .collect()
}

/// `finished` has succeeded once for each mode, never twice for a period,
/// and only for a period that was the latest finished one at one of the
/// seconds `seen` (a supervisor's now lies between the first and the last
/// of them, so a period that turned meanwhile is among them).
fn reviewed_once_each(finished: &[(String, String, String)], seen: &[i64]) {
    let current: Vec<(String, String)> = seen.iter().flat_map(|at| periods_at(*at)).collect();
    let mut periods = Vec::new();
    for (mode, period, outcome) in finished {
        assert_eq!(outcome, "succeeded", "{finished:?}");
        let key = (mode.clone(), period.clone());
        assert!(current.contains(&key), "{key:?} was not due: {current:?}");
        assert!(!periods.contains(&key), "{key:?} was reviewed twice");
        periods.push(key);
    }
    for mode in ReviewMode::ALL {
        assert!(
            finished.iter().any(|(was, _, _)| was == mode.as_str()),
            "no {} review: {finished:?}",
            mode.as_str()
        );
    }
}

fn review_outcomes(db: &Path) -> Vec<(String, String)> {
    queue_events(db, "throughput_review_finished")
        .iter()
        .map(|f| {
            (
                f["mode"].as_str().unwrap().to_owned(),
                f["outcome"].as_str().unwrap().to_owned(),
            )
        })
        .collect()
}

#[test]
fn the_supervisor_starts_each_review_due_once_without_a_run_slot() {
    let (_dir, repo, db) = fixture();
    SqliteQueue::open(&db)
        .unwrap()
        .transition(TaskId::new(1), TaskAction::Cancel)
        .unwrap();
    let backend = TestWorkspace::new(&db, false, VALID_AGENT);
    // Read before the supervisor's clock starts, so its every now lies
    // after it.
    let before = now_secs();
    let options = SuperviseOptions {
        throughput_review: true,
        utc_offset: utc,
        ..supervise_options(1, true)
    };
    let supervise_reviewing = || {
        runtime::supervise(
            &db,
            &repo,
            &backend,
            &review_claude_stub(&db, false),
            Path::new(env!("CARGO_BIN_EXE_dagq")),
            &options,
        )
        .unwrap()
    };
    // Nothing was ever reviewed: the hour, the day and the week are due,
    // the hour first; `--once` waits for each. The hour of an idle queue
    // landed nothing, which is a rule of its own (ADR-t996-1 decision 2).
    let outcome = supervise_reviewing();
    let after = now_secs();
    assert_eq!(outcome["runs"], json!([]));
    let first = finished_reviews(&db);
    assert_eq!(first[0].0, "hourly", "{first:?}");
    reviewed_once_each(&first, &[before, after]);
    let reported = queue_events(&db, "throughput_review_reported");
    assert_eq!(reported.len(), first.len());
    for notice in &reported {
        assert_eq!(
            notice["conclusion"],
            json!(["- reviewed by throughput-review-job"])
        );
        if notice["mode"] == "hourly" {
            assert_eq!(notice["reasons"], json!(["no_landing"]));
        }
    }
    // Within the same periods nothing is due again, even for another
    // supervisor: only a period that began since is.
    supervise_reviewing();
    let end = now_secs();
    let second = finished_reviews(&db);
    assert_eq!(second[..first.len()], first[..]);
    reviewed_once_each(&second, &[before, after, end]);
    // Unless an hour, a day or a week turned while the test ran, that is
    // one review of each.
    if periods_at(before) == periods_at(end) {
        let modes: Vec<&str> = second.iter().map(|(mode, _, _)| mode.as_str()).collect();
        assert_eq!(modes, ["hourly", "daily", "weekly"]);
    }
    // Off, none starts.
    let disabled = SuperviseOptions {
        throughput_review: false,
        ..options.clone()
    };
    Connection::open(&db)
        .unwrap()
        .execute(
            "DELETE FROM run_events WHERE kind LIKE 'throughput_review_%'",
            [],
        )
        .unwrap();
    runtime::supervise(
        &db,
        &repo,
        &backend,
        &review_claude_stub(&db, false),
        Path::new(env!("CARGO_BIN_EXE_dagq")),
        &disabled,
    )
    .unwrap();
    assert!(review_outcomes(&db).is_empty());
}

#[test]
fn a_failing_review_stops_no_claim_nor_landing() {
    let (_dir, repo, db) = fixture();
    let backend = TestWorkspace::new(&db, false, VALID_AGENT);
    let reviewer = TestReviewer::new(&[verdict("pass", &[], "meets the acceptance")]);
    let options = SuperviseOptions {
        throughput_review: true,
        utc_offset: utc,
        ..supervise_options(1, true)
    };
    let outcome = runtime::supervise_with_reviewer(
        &db,
        &repo,
        &backend,
        &review_claude_stub(&db, true),
        &reviewer,
        Path::new(env!("CARGO_BIN_EXE_dagq")),
        &options,
    )
    .unwrap();
    backend.join();
    assert_eq!(outcome["runs"][0]["status"], "integrated", "{outcome}");
    assert_eq!(
        SqliteQueue::open(&db)
            .unwrap()
            .show(TaskId::new(1))
            .unwrap()
            .task
            .status(),
        TaskStatus::Completed
    );
    let outcomes = review_outcomes(&db);
    assert!(
        outcomes
            .iter()
            .any(|(mode, outcome)| mode == "weekly" && outcome == "failed"),
        "{outcomes:?}"
    );
    assert!(queue_events(&db, "throughput_review_reported").is_empty());
    // Each failure reaches the inbox as a notice, and none as an ask.
    let notices = attentions(&db)
        .into_iter()
        .filter(|event| event["next"] == "check the failed review")
        .count();
    assert_eq!(
        notices,
        outcomes
            .iter()
            .filter(|(_, outcome)| outcome == "failed")
            .count()
    );
    assert!(
        SqliteQueue::open(&db)
            .unwrap()
            .asks(dagq::infrastructure::asks::AskQuery::default())
            .unwrap()
            .is_empty()
    );
}

#[test]
fn the_reply_uses_only_stdout_for_text_and_structured_providers() {
    let reply = "## Conclusion\n- unchanged\n\n## Details\nOnly the reply.";
    for output in [reply.to_owned(), json!({"reply": reply}).to_string()] {
        let (_dir, _repo, db) = fixture();
        let script = format!("cat <<'REPLY'\n{output}\nREPLY\necho 'stderr diagnostic' >&2");
        let done = review(
            &db,
            &ReviewProvider::new(&script),
            &options(ReviewMode::Daily),
        )
        .unwrap();
        assert_eq!(done["outcome"], "succeeded", "{done}");
        let dir = PathBuf::from(done["dir"].as_str().unwrap());
        assert_eq!(
            fs::read_to_string(dir.join("output.out")).unwrap(),
            format!("{output}\n")
        );
        assert_eq!(
            fs::read_to_string(dir.join("output.err")).unwrap(),
            "stderr diagnostic\n"
        );
        assert_eq!(
            fs::read_to_string(dir.join("review.md")).unwrap(),
            format!("{reply}\n")
        );
        let saved: Value =
            serde_json::from_str(&fs::read_to_string(dir.join("review.json")).unwrap()).unwrap();
        assert_eq!(saved["conclusion"], json!(["- unchanged"]));
        assert!(!dir.join("output.log").exists());
    }
}
