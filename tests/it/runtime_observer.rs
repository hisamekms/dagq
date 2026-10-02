//! Runtime tests: The observer.
use crate::{common, runtime_support};
use dagq::domain::EventKind;
use dagq::domain::headless_job::JobAccess;
use dagq::infrastructure::adapters::ClaudeCode;

use runtime_support::*;

/// The observer's provider double: the headless job is a shell script in the
/// observation's directory, with the environment `observe` gives the agent.
struct ObserverProvider {
    script: String,
}

impl AgentProvider for ObserverProvider {
    fn preflight(&self) -> Result<()> {
        Ok(())
    }
    fn command(&self, _: &TaskRun, _: &str) -> Result<CommandSpec> {
        bail!("the observer has no run")
    }
    fn resume_command(&self, _: &TaskRun) -> Result<CommandSpec> {
        bail!("the observer has no run")
    }
    fn headless_command(&self, cwd: &Path, prompt: &str, access: JobAccess) -> Result<CommandSpec> {
        assert!(prompt.contains("You are the observer"), "{prompt}");
        assert_eq!(access, JobAccess::QueueCli);
        let mut command = CommandSpec::new("/bin/sh");
        command.current_dir(cwd).arg("-c").arg(&self.script);
        Ok(command)
    }
    /// The script's `$0`, which it writes to `mcp.txt` when asked to.
    fn without_mcp(&self, command: &mut CommandSpec) {
        command.option_args(["no-mcp"]);
    }
    /// `$MODEL`, which the script may write down (ADR-0079 decision 7).
    fn select_model(&self, command: &mut CommandSpec, model: &str, effort: &str) {
        command.env("MODEL", format!("{model} {effort}"));
    }
    fn review_command(&self, _: &TaskRun, _: &str, _: JobAccess) -> Result<CommandSpec> {
        bail!("the observer reviews no run")
    }
}

// The composition supplies the signals of the same provider that starts
// the job. These shell doubles emit Claude's diagnostics.
fn observe(
    db: &Path,
    provider: &dyn AgentProvider,
    options: &dagq::observer::ObserveOptions,
) -> Result<Value> {
    // The job's `dagq` goes to the queue's service (goal 82's stage (3)),
    // which the fixture stops.
    common::service::serve(db);
    dagq::observer::observe(
        db,
        provider,
        &ClaudeCode {
            executable: "/unused".into(),
        },
        options,
    )
}

fn observe_options(mode: dagq::observer::ObserveMode) -> dagq::observer::ObserveOptions {
    dagq::observer::ObserveOptions {
        mode,
        cmux: None,
        since: None,
        dry_run: false,
        timeout: Duration::from_secs(60),
        dagq: PathBuf::from(env!("CARGO_BIN_EXE_dagq")),
        user_config: None,
    }
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

/// Without `[roles.observer]` the agent is given no model or effort; with
/// it in the bound checkout's `dagq.toml`, it is given its model and
/// effort. `observe_started` and the span record which (ADR-0079 decision
/// 7).
#[test]
fn the_observer_takes_its_role_table_and_records_what_it_started_with() {
    use dagq::observer::ObserveMode;
    let (_dir, repo, db) = fixture();
    let provider = ObserverProvider {
        script: r#"printf '%s' "${MODEL-none}" > model.txt"#.into(),
    };
    // A new task each time, so the observation is not skipped.
    let run = |mode| {
        add_ready_task(&mut SqliteQueue::open(&db).unwrap(), "more", &[]);
        let report = observe(&db, &provider, &observe_options(mode)).unwrap();
        assert_eq!(report["outcome"], "succeeded", "{report}");
        let dir = PathBuf::from(report["dir"].as_str().unwrap());
        fs::read_to_string(dir.join("model.txt")).unwrap()
    };
    assert_eq!(run(ObserveMode::Hourly), "none");
    let mut queue = SqliteQueue::open(&db).unwrap();
    let common = repo.join(".git").canonicalize().unwrap();
    queue.bind_repository(common.to_str().unwrap()).unwrap();
    fs::write(
        repo.join("dagq.toml"),
        "[roles.observer]\nmodel = \"claude-sonnet-5\"\neffort = \"low\"\n",
    )
    .unwrap();
    assert_eq!(run(ObserveMode::Daily), "claude-sonnet-5 low");
    let started: Vec<Value> = queue_events(&db, "observe_started")
        .into_iter()
        .map(|payload| payload["launch"].clone())
        .collect();
    let configured = json!({"role": "observer", "provider": "claude", "model": "claude-sonnet-5", "effort": "low",
                            "source": "dagq.toml"});
    assert_eq!(
        started,
        [
            json!({"role": "observer", "provider": "claude", "model": null, "effort": null, "source": "default"}),
            configured.clone(),
        ]
    );
    let spans: Vec<Value> = queue_events(&db, "session_opened")
        .into_iter()
        .filter(|payload| payload["kind"] == "observer")
        .map(|payload| payload["launch"].clone())
        .collect();
    assert_eq!(spans, started);
}

#[test]
fn observe_records_findings_and_a_blocked_ask_and_advances_the_cursor() {
    use dagq::observer::{ObserveMode, read_cursor};
    let (_dir, _repo, db) = fixture();
    // `dagq` is first on PATH and goes to the queue's service with the
    // observer's token; the state changes the prompt forbids are refused,
    // by the service or for having no use case in it.
    let provider = ObserverProvider {
        script: r#"
set -e
printf '%s' "$DAGQ_ROLE" > role.txt
printf '%s' "$0" > mcp.txt
q() { dagq "$@" > /dev/null; }
q finding record --kind stall --task 1 --summary 'task 1 waits for a slot' --evidence 1
q finding record --kind stall --task 1 --summary 'task 1 waits for a slot' --evidence 1 --evidence 2
q finding record --kind capacity --queue --subject idle_slots --summary 'slots idle'
q ask --kind blocked --because recovery_failed --finding 2 --question 'slots idle while task 1 is ready' --option 'leave it' --cmux /usr/bin/true
q ask --kind blocked --because recovery_failed --finding 2 --question 'the same alert again' --cmux /usr/bin/true
if q ready 1 2> ready.err; then exit 3; fi
if q ask --kind decide --because recovery_failed --task 1 --question 'decide?' --cmux /usr/bin/true 2> ask.err; then exit 4; fi
if q goal ready 1 2> goal.err; then exit 5; fi
if q note --task 1 --text 'seen' 2> note.err; then exit 6; fi
if q goal add --draft 'claim faster' 2> draft.err; then exit 7; fi
echo 'recorded 2 findings, updated 1, wrote 1 ask'
echo 'observer diagnostic' >&2
"#
        .into(),
    };
    assert_eq!(read_cursor(&db).unwrap(), None);
    let first = observe(&db, &provider, &observe_options(ObserveMode::Hourly)).unwrap();
    assert_eq!(first["outcome"], "succeeded", "{first}");
    assert_eq!(
        (
            &first["findings_recorded"],
            &first["findings_updated"],
            &first["asks"]
        ),
        (&json!(2), &json!(1), &json!(1))
    );
    assert_eq!(first["since"], Value::Null);
    let cursor = first["cursor"].as_i64().unwrap();
    assert!(cursor >= 0);
    assert_eq!(read_cursor(&db).unwrap(), Some(EventId::new(cursor)));
    let dir = PathBuf::from(first["dir"].as_str().unwrap());
    assert_eq!(
        dir.parent().unwrap(),
        db.canonicalize()
            .unwrap()
            .parent()
            .unwrap()
            .join("observer")
    );
    assert_eq!(
        fs::read_to_string(dir.join("role.txt")).unwrap(),
        "observer"
    );
    // The agent was started without MCP servers.
    assert_eq!(fs::read_to_string(dir.join("mcp.txt")).unwrap(), "no-mcp");
    // The service refuses the ask and the note for the observer's principal,
    // and the planning commands are none of its use cases.
    for (denied, code) in [
        ("ready.err", "no_use_case"),
        ("ask.err", "authorization_denied"),
        ("goal.err", "no_use_case"),
        ("note.err", "authorization_denied"),
        ("draft.err", "no_use_case"),
    ] {
        let error: Value =
            serde_json::from_str(&fs::read_to_string(dir.join(denied)).unwrap()).unwrap();
        assert_eq!(error["queue_service"]["code"], code, "{denied}: {error}");
    }
    let refusals = queue_events(&db, "authorization_denied");
    assert_eq!(refusals.len(), 2, "{refusals:?}");
    assert!(
        fs::read_to_string(dir.join("output.out"))
            .unwrap()
            .contains("recorded 2 findings")
    );
    assert!(
        fs::read_to_string(dir.join("prompt.md"))
            .unwrap()
            .contains("\"stats\"")
    );
    assert_eq!(
        fs::read_to_string(dir.join("output.err")).unwrap(),
        "observer diagnostic\n"
    );
    assert!(!dir.join("output.log").exists());
    // Nothing changed state: the task is still ready and no goal was added.
    let mut queue = SqliteQueue::open(&db).unwrap();
    assert_eq!(
        queue.show(TaskId::new(1)).unwrap().task.status(),
        TaskStatus::Ready
    );
    assert!(queue.show_goal(GoalId::new(1)).is_err());
    let asks = queue
        .asks(dagq::infrastructure::asks::AskQuery::default())
        .unwrap();
    assert_eq!(asks.len(), 1);
    assert_eq!(asks[0].kind.as_str(), "blocked");
    assert_eq!(asks[0].task_id, None);
    assert_eq!(asks[0].asked_by, "observer");
    assert_eq!(asks[0].finding_id, Some(dagq::domain::FindingId::new(2)));
    let findings = queue
        .findings(&dagq::domain::FindingQuery::default())
        .unwrap();
    assert_eq!(findings.len(), 2);
    let stall = findings.iter().find(|f| f.finding.kind == "stall").unwrap();
    assert_eq!(stall.finding.occurrences, 2);
    assert_eq!(stall.finding.recorded_by, "observer");
    assert_eq!(queue_events(&db, "observe_started").len(), 1);
    assert_eq!(
        queue_events(&db, "observe_finished"),
        std::slice::from_ref(&first)
    );

    // Nothing but the observer's own events and the KPIs' bookkeeping
    // (ADR-0051 decision 24) since: the next observation starts no agent
    // and records a skipped finish.
    queue
        .record_queue_event(
            EventKind::CandidatesSampled,
            json!({"candidates": 1, "free_slots": 2, "ready": 1}),
        )
        .unwrap();
    let failing = ObserverProvider {
        script: "exit 7".into(),
    };
    let skipped = observe(&db, &failing, &observe_options(ObserveMode::Hourly)).unwrap();
    assert_eq!(skipped["outcome"], "skipped", "{skipped}");
    assert_eq!(skipped["since"], cursor);
    assert_eq!(skipped["dir"], Value::Null);
    assert_eq!(queue_events(&db, "observe_started").len(), 1);
    assert_eq!(queue_events(&db, "observe_finished").len(), 2);
    assert_eq!(read_cursor(&db).unwrap(), Some(EventId::new(cursor)));
    // An event of someone else ends the quiet.
    queue
        .record_queue_event(EventKind::StallConfigLoaded, json!({}))
        .unwrap();

    // The next observation reads past the saved cursor; a failed one keeps it.
    let second = observe(&db, &failing, &observe_options(ObserveMode::Hourly)).unwrap();
    assert_eq!(second["since"], cursor);
    assert_eq!(second["outcome"], "failed");
    assert_eq!(second["exit_code"], 7);
    assert_eq!(second["cursor_saved"], false);
    assert_eq!(read_cursor(&db).unwrap(), Some(EventId::new(cursor)));
    // After a failed one the next runs its agent again, quiet or not.
    let retried = observe(&db, &failing, &observe_options(ObserveMode::Hourly)).unwrap();
    assert_eq!(retried["outcome"], "failed", "{retried}");

    // A dry run only returns the prompt.
    let dry = observe(
        &db,
        &failing,
        &dagq::observer::ObserveOptions {
            dry_run: true,
            since: Some(EventId::new(0)),
            ..observe_options(ObserveMode::Daily)
        },
    )
    .unwrap();
    assert_eq!(dry["dry_run"], true);
    assert_eq!(dry["since"], 0);
    let prompt = dry["prompt"].as_str().unwrap();
    assert!(prompt.contains("daily observation"), "{prompt}");
    assert!(prompt.contains("ask --kind blocked"), "{prompt}");
    assert!(prompt.contains("finding record --kind"), "{prompt}");
    assert!(prompt.contains("--finding ID"), "{prompt}");
    assert!(!prompt.contains("goal add --draft"), "{prompt}");
    assert!(prompt.contains("\"findings\""), "{prompt}");
    assert!(
        prompt.contains("slots idle while task 1 is ready"),
        "{prompt}"
    );
    assert!(prompt.contains("task 1 waits for a slot"), "{prompt}");
    assert!(prompt.contains("idle_slots"), "{prompt}");
    assert_eq!(queue_events(&db, "observe_started").len(), 3);
    // No language set: no instruction; the user's language: its
    // instruction closes the prompt (ADR-t616-2).
    assert!(!prompt.contains("Language:"), "{prompt}");
    let config = db.parent().unwrap().join("config.toml");
    fs::write(&config, "[language]\ntag = \"ja\"\n").unwrap();
    let dry = observe(
        &db,
        &failing,
        &dagq::observer::ObserveOptions {
            dry_run: true,
            since: Some(EventId::new(0)),
            user_config: Some(config),
            ..observe_options(ObserveMode::Daily)
        },
    )
    .unwrap();
    let prompt = dry["prompt"].as_str().unwrap();
    assert!(
        prompt.ends_with(&dagq::domain::language::instruction("ja")),
        "{prompt}"
    );

    // An agent that cannot start is an error outcome, not a failed observe.
    let broken = observe(
        &db,
        &TestProvider {
            script: String::new(),
            db: db.clone(),
        },
        &observe_options(ObserveMode::Daily),
    )
    .unwrap();
    assert_eq!(broken["outcome"], "error");
    assert!(
        broken["error"]
            .as_str()
            .unwrap()
            .contains("no headless execution")
    );
    // The daily one reads the last 24 hours and leaves the cursor alone.
    assert_eq!(broken["since"], 0);
    assert_eq!(read_cursor(&db).unwrap(), Some(EventId::new(cursor)));

    // `observe --history` lists them newest first with what each read and
    // wrote; the observer may read it too.
    let output = {
        use crate::common::{Bounded, WithoutActor};
        std::process::Command::new(env!("CARGO_BIN_EXE_dagq"))
            .without_actor_env()
            .args(["--db", db.to_str().unwrap(), "observe", "--history"])
            .env("DAGQ_ROLE", "observer")
            .bounded_output()
            .unwrap()
    };
    assert!(output.status.success(), "{output:?}");
    let history: Value = serde_json::from_slice(&output.stdout).unwrap();
    let observations = history["observations"].as_array().unwrap();
    assert_eq!(
        observations
            .iter()
            .map(|o| (o["mode"].as_str().unwrap(), o["outcome"].as_str().unwrap()))
            .collect::<Vec<_>>(),
        [
            ("daily", "error"),
            ("hourly", "failed"),
            ("hourly", "failed"),
            ("hourly", "skipped"),
            ("hourly", "succeeded"),
        ]
    );
    let quiet = &observations[3];
    assert_eq!(quiet["skipped"], true);
    assert_eq!(quiet["started_at"], quiet["finished_at"]);
    assert_eq!(quiet["input"], json!({"since": cursor, "through": cursor}));
    let first_entry = &observations[4];
    assert_eq!(first_entry["skipped"], false);
    assert_eq!(
        first_entry["input"],
        json!({"since": null, "through": cursor})
    );
    assert_eq!(
        first_entry["findings"],
        json!({
            "recorded": 2,
            "updated": 1,
            "closed": 0,
            "recorded_ids": [1, 2],
            "updated_ids": [1],
            "closed_ids": [],
        })
    );
    assert_eq!(
        first_entry["asks"],
        json!({"count": 1, "ids": [asks[0].id]})
    );
    assert_eq!(first_entry["dir"], first["dir"]);
    assert!(first_entry["duration_secs"].is_u64());
    assert!(
        first_entry["started_at"].as_str().unwrap() <= first_entry["finished_at"].as_str().unwrap()
    );
    let limited = {
        use crate::common::{Bounded, WithoutActor};
        std::process::Command::new(env!("CARGO_BIN_EXE_dagq"))
            .without_actor_env()
            .args([
                "--db",
                db.to_str().unwrap(),
                "observe",
                "--history",
                "--limit",
                "1",
            ])
            .bounded_output()
            .unwrap()
    };
    let limited: Value = serde_json::from_slice(&limited.stdout).unwrap();
    assert_eq!(limited["observations"].as_array().unwrap().len(), 1);
}

/// What a process the test started through the runtime wrote to `file`
/// (renamed into place whole), once it is there.
fn written_to(file: &Path) -> String {
    let _waiting = common::within(
        common::STEP_LIMIT,
        format!("{} to be written", file.display()),
    );
    loop {
        if let Ok(text) = fs::read_to_string(file) {
            return text.trim().to_owned();
        }
        thread::sleep(Duration::from_millis(50));
    }
}

/// Whether `pid` is gone within a few seconds (a killed orphan is reaped by
/// the system, not at once).
fn gone(pid: u32) -> bool {
    let deadline = Instant::now() + Duration::from_secs(10);
    while dagq::infrastructure::adapters::process_alive(pid) {
        if Instant::now() >= deadline {
            return false;
        }
        thread::sleep(Duration::from_millis(50));
    }
    true
}

/// An observer agent stopped at a login that ran out joins the queue's
/// authentication ask as the observer job (task 438): `auth_required` on
/// the queue, and `observe_finished` names the wall and the ask. A second
/// one changes nothing.
#[test]
fn an_observer_at_a_login_that_ran_out_joins_the_authentication_ask() {
    use dagq::observer::ObserveMode;
    let (_dir, _repo, db) = fixture();
    let logged_out = ObserverProvider {
        script: "printf 'Invalid API key \u{b7} Please run /login\\n'; exit 1".into(),
    };
    let first = observe(&db, &logged_out, &observe_options(ObserveMode::Daily)).unwrap();
    assert_eq!(first["outcome"], "failed", "{first}");
    assert_eq!(first["wall"], "authentication", "{first}");
    let queue = SqliteQueue::open(&db).unwrap();
    let asks = queue
        .asks(AskQuery {
            open: true,
            ..Default::default()
        })
        .unwrap();
    assert_eq!(asks.len(), 1, "{asks:?}");
    assert_eq!(asks[0].kind, AskKind::QueueHold);
    assert_eq!(
        asks[0].reason_category,
        dagq::domain::AskReason::Authentication
    );
    assert_eq!(asks[0].affected, ["observer job"]);
    assert_eq!(first["hold_ask_id"], json!(asks[0].id));
    let required = queue_events(&db, "auth_required");
    assert_eq!(required.len(), 1, "{required:?}");
    assert_eq!(required[0]["job"], "observer");
    // An explicitly missing executable has the same hold behavior as None.
    let mut missing = observe_options(ObserveMode::Daily);
    missing.cmux = Some(db.parent().unwrap().join("missing-cmux"));
    let second = observe(&db, &logged_out, &missing).unwrap();
    assert_eq!(second["hold_ask_id"], json!(asks[0].id), "{second}");
    assert_eq!(queue_events(&db, "auth_required").len(), 1);
    // A failure at no wall is no hold.
    let failing = ObserverProvider {
        script: "exit 7".into(),
    };
    let other = observe(&db, &failing, &observe_options(ObserveMode::Daily)).unwrap();
    assert_eq!(other["wall"], Value::Null, "{other}");
}

/// The agent past its timeout is killed with what it started: its Bash
/// child does not outlive the observation (task 245).
#[test]
fn observe_kills_an_agent_past_its_timeout_with_its_children() {
    use dagq::observer::ObserveMode;
    let (_dir, _repo, db) = fixture();
    let child_pid = db.parent().unwrap().join("child.pid");
    let slow = ObserverProvider {
        script: format!(
            "sleep 60 & echo $! > '{}.tmp' && mv '{0}.tmp' '{0}'; wait",
            child_pid.display()
        ),
    };
    let started = Instant::now();
    let outcome = observe(
        &db,
        &slow,
        &dagq::observer::ObserveOptions {
            timeout: Duration::from_secs(5),
            ..observe_options(ObserveMode::Hourly)
        },
    )
    .unwrap();
    assert!(started.elapsed() < Duration::from_secs(30));
    assert_eq!(outcome["outcome"], "error");
    assert!(
        outcome["error"]
            .as_str()
            .unwrap()
            .contains("did not finish")
    );
    assert_eq!(outcome["cursor_saved"], false);
    let child: u32 = written_to(&child_pid).parse().unwrap();
    assert!(
        gone(child),
        "the agent's child {child} outlived the timeout"
    );
}

/// A Claude Code stand-in for the supervisor's observer: `--version` for
/// the preflight, and in print mode (`-p`) a finding through the queue CLI it
/// finds first on PATH.
fn observer_claude_stub(db: &Path) -> PathBuf {
    let stub = db.parent().unwrap().join("claude-observer-stub");
    crate::common::template::script(
        &stub,
        r#"#!/bin/sh
if [ "$1" = "-p" ]; then
  mode=hourly
  case "$*" in *"daily observation"*) mode=daily ;; esac
  exec dagq finding record --goal 1 --kind observed --subject "$mode" --summary "observed by $DAGQ_ROLE"
fi
printf 'test provider\n'
"#,
    );

    stub
}

#[test]
fn supervisor_starts_the_observer_on_its_interval_without_a_run_slot() {
    let (_dir, repo, db) = fixture();
    // The observer's `dagq` goes to the queue's service.
    common::service::serve(&db);
    {
        let mut queue = SqliteQueue::open(&db).unwrap();
        queue
            .transition(TaskId::new(1), TaskAction::Cancel)
            .unwrap();
        queue
            .add_goal(NewGoal {
                title: "observed".into(),
                description: String::new(),
                acceptance: String::new(),
                constraints: String::new(),
                doc: None,
                draft: false,
            })
            .unwrap();
    }
    let backend = TestWorkspace::new(&db, false, VALID_AGENT);
    let options = SuperviseOptions {
        observe_interval: Duration::from_secs(3600),
        observe_daily: true,
        update: dagq::application::supervise::UpdateSettings {
            cmux: Some(PathBuf::from("/usr/bin/true")),
            ..Default::default()
        },
        ..SuperviseOptions::new(1, true)
    };
    let runner = db.parent().unwrap().join("observer-runner");
    let arguments = db.parent().unwrap().join("observer-arguments");
    crate::common::template::script(
        &runner,
        format!(
            "#!/bin/sh\nprintf '%s\\n' \"$@\" > \"${{0%/*}}/observer-arguments\"\nexec '{}' \"$@\"\n",
            env!("CARGO_BIN_EXE_dagq")
        ),
    );
    let supervise_observed = || {
        runtime::supervise(
            &db,
            &repo,
            &backend,
            &observer_claude_stub(&db),
            &runner,
            &options,
        )
        .unwrap()
    };
    // Nothing was ever observed: the daily observation is due, then the
    // hourly one; `--once` waits for each before it exits.
    let outcome = supervise_observed();
    assert_eq!(outcome["runs"], json!([]));
    let arguments = fs::read_to_string(&arguments).unwrap();
    assert!(
        arguments.contains("observe\n--cmux\n/usr/bin/true\n"),
        "{arguments}"
    );
    let finished = queue_events(&db, "observe_finished");
    assert_eq!(
        finished
            .iter()
            .map(|f| (f["mode"].as_str().unwrap(), f["outcome"].as_str().unwrap()))
            .collect::<Vec<_>>(),
        [("daily", "succeeded"), ("hourly", "succeeded")]
    );
    assert!(finished.iter().all(|f| f["findings_recorded"] == 1));
    let mut findings = SqliteQueue::open(&db)
        .unwrap()
        .findings(&dagq::domain::FindingQuery {
            target: Some(dagq::domain::FindingTarget::Goal(GoalId::new(1))),
            ..Default::default()
        })
        .unwrap();
    findings.sort_by_key(|f| f.finding.id);
    assert_eq!(
        findings
            .iter()
            .map(|f| (f.finding.subject.as_str(), f.finding.summary.as_str()))
            .collect::<Vec<_>>(),
        [
            ("daily", "observed by observer"),
            ("hourly", "observed by observer")
        ]
    );
    assert!(dagq::observer::read_cursor(&db).unwrap().is_some());
    // Within the interval nothing is due again, even for another supervisor.
    supervise_observed();
    assert_eq!(queue_events(&db, "observe_started").len(), 2);
    // An interval of 0 disables the observer.
    let disabled = SuperviseOptions {
        observe_interval: Duration::ZERO,
        ..options.clone()
    };
    fs::remove_file(db.parent().unwrap().join("observer/cursor")).unwrap();
    Connection::open(&db)
        .unwrap()
        .execute(
            "UPDATE run_events SET created_at='2000-01-01T00:00:00.000Z' WHERE kind LIKE 'observe_%'",
            [],
        )
        .unwrap();
    runtime::supervise(
        &db,
        &repo,
        &backend,
        &observer_claude_stub(&db),
        Path::new(env!("CARGO_BIN_EXE_dagq")),
        &disabled,
    )
    .unwrap();
    assert_eq!(queue_events(&db, "observe_started").len(), 2);
    // Once the interval passed, the next pass observes again.
    supervise_observed();
    assert_eq!(queue_events(&db, "observe_started").len(), 4);
}

#[test]
fn observe_reads_again_what_others_wrote_while_its_agent_ran() {
    use dagq::observer::ObserveMode;
    let (_dir, _repo, db) = fixture();
    // Someone else's note lands after the input was read, before the finish.
    let noting = ObserverProvider {
        script:
            // A person's note, on the queue itself.
            format!(
                "env -u DAGQ_ROLE -u DAGQ_SERVICE_SOCKET -u DAGQ_SERVICE_CREDENTIAL_FILE dagq --db \"{}\" note --task 1 --text 'meanwhile' > /dev/null",
                db.display()
            ),
    };
    let quiet = ObserverProvider {
        script: "true".into(),
    };
    let first = observe(&db, &noting, &observe_options(ObserveMode::Hourly)).unwrap();
    assert_eq!(first["outcome"], "succeeded", "{first}");
    let second = observe(&db, &quiet, &observe_options(ObserveMode::Hourly)).unwrap();
    assert_eq!(second["outcome"], "succeeded", "{second}");
    let third = observe(&db, &quiet, &observe_options(ObserveMode::Hourly)).unwrap();
    assert_eq!(third["outcome"], "skipped", "{third}");
    // A cursor given by hand reads whatever happened.
    let forced = observe(
        &db,
        &quiet,
        &dagq::observer::ObserveOptions {
            since: Some(EventId::new(0)),
            ..observe_options(ObserveMode::Hourly)
        },
    )
    .unwrap();
    assert_eq!(forced["outcome"], "succeeded", "{forced}");
}

/// ADR-t649-1: an alert that grew past its threshold with time alone, with
/// no event, starts the agent even when nothing else happened; the same
/// alerts again, or an older observation that kept none while there are
/// none, leave the observation skipped. After an older observation that
/// kept no alerts, any alert starts it.
#[test]
fn observe_starts_again_for_an_alert_that_time_alone_raised() {
    use dagq::domain::{AskKind, AskReason, NewAsk};
    use dagq::observer::ObserveMode;
    let (_dir, _repo, db) = fixture();
    let mut queue = SqliteQueue::open(&db).unwrap();
    let ask = queue
        .ask(NewAsk {
            topics: Vec::new(),
            kind: AskKind::Blocked,
            task_id: None,
            run_id: None,
            question: "is it stuck?".into(),
            options: Vec::new(),
            asked_by: "observer".into(),
            reason_category: AskReason::Scope,
            finding_id: None,
        })
        .unwrap()
        .ask;
    let quiet = ObserverProvider {
        script: "true".into(),
    };
    let unanswered = |payload: &Value| {
        payload["alerts"]
            .as_array()
            .unwrap()
            .iter()
            .any(|alert| alert["kind"] == "ask_unanswered")
    };
    let first = observe(&db, &quiet, &observe_options(ObserveMode::Hourly)).unwrap();
    assert_eq!(first["outcome"], "succeeded", "{first}");
    assert!(!unanswered(&first), "{first}");
    // An older observation that kept no alerts, while there is none.
    queue
        .record_queue_event(
            EventKind::ObserveFinished,
            json!({"mode": "hourly", "outcome": "succeeded"}),
        )
        .unwrap();
    let quiet_again = observe(&db, &quiet, &observe_options(ObserveMode::Hourly)).unwrap();
    assert_eq!(quiet_again["outcome"], "skipped", "{quiet_again}");

    // Two hours pass with the ask open: `ask_unanswered` is raised with no
    // event recorded.
    Connection::open(&db)
        .unwrap()
        .execute(
            "UPDATE run_events SET created_at=strftime('%Y-%m-%dT%H:%M:%fZ','now','-2 hours')
             WHERE kind='ask_opened'",
            [],
        )
        .unwrap();
    let raised = observe(&db, &quiet, &observe_options(ObserveMode::Hourly)).unwrap();
    assert_eq!(raised["outcome"], "succeeded", "{raised}");
    let alert = raised["alerts"]
        .as_array()
        .unwrap()
        .iter()
        .find(|alert| alert["kind"] == "ask_unanswered")
        .unwrap_or_else(|| panic!("{raised}"));
    assert_eq!(alert["ask_id"], ask.id.to_string());
    // The alert stays but is no longer new: skipped.
    let same = observe(&db, &quiet, &observe_options(ObserveMode::Hourly)).unwrap();
    assert_eq!(same["outcome"], "skipped", "{same}");
    assert!(unanswered(&same), "{same}");

    // After an older observation that kept no alerts, any alert is new.
    queue
        .record_queue_event(
            EventKind::ObserveFinished,
            json!({"mode": "hourly", "outcome": "succeeded"}),
        )
        .unwrap();
    let after_old = observe(&db, &quiet, &observe_options(ObserveMode::Hourly)).unwrap();
    assert_eq!(after_old["outcome"], "succeeded", "{after_old}");
    let settled = observe(&db, &quiet, &observe_options(ObserveMode::Hourly)).unwrap();
    assert_eq!(settled["outcome"], "skipped", "{settled}");
    assert_eq!(queue_events(&db, "observe_started").len(), 3);
}

/// ADR-0051 decisions 24 and 25: the observer reads the KPIs judged
/// against the checkout's `[kpi]` targets and the improvements running
/// against their limit, and a breach it records again under one subject
/// stays one `kpi` finding.
#[test]
fn observe_reads_the_kpis_and_the_improvements_and_keeps_one_kpi_finding_per_subject() {
    use dagq::observer::ObserveMode;
    let (_dir, repo, db) = fixture();
    fs::write(
        repo.join("dagq.toml"),
        "[kpi]\nmax_improvement_proposals = 3\n[kpi.targets.\"lead_time\"]\nmax = 60\n",
    )
    .unwrap();
    SqliteQueue::open(&db)
        .unwrap()
        .bind_repository(repo.join(".git").to_str().unwrap())
        .unwrap();

    let dry = observe(
        &db,
        &ObserverProvider {
            script: "exit 1".into(),
        },
        &dagq::observer::ObserveOptions {
            dry_run: true,
            ..observe_options(ObserveMode::Hourly)
        },
    )
    .unwrap();
    let prompt = dry["prompt"].as_str().unwrap();
    assert!(prompt.contains("Reading the KPIs"), "{prompt}");
    assert!(
        prompt.contains("--kind <finding_kind> --queue --subject"),
        "{prompt}"
    );

    let provider = ObserverProvider {
        script: r#"
set -e
q() { dagq "$@" > /dev/null; }
q finding record --kind kpi --queue --subject 'lead_time/all' --summary 'lead time 90s over 60s for 3 days' --evidence 1
q finding record --kind kpi --queue --subject 'lead_time/all' --summary 'lead time 95s over 60s for 4 days' --evidence 2
"#
        .into(),
    };
    let done = observe(&db, &provider, &observe_options(ObserveMode::Hourly)).unwrap();
    assert_eq!(done["outcome"], "succeeded", "{done}");
    let dir = PathBuf::from(done["dir"].as_str().unwrap());
    let input: Value =
        serde_json::from_str(&fs::read_to_string(dir.join("input.json")).unwrap()).unwrap();
    let targets = input["kpi"]["targets"].as_array().unwrap();
    assert_eq!(
        targets
            .iter()
            .map(|t| (t["period"].clone(), t["kpi"].clone(), t["max"].clone()))
            .collect::<Vec<_>>(),
        [
            (json!("day"), json!("lead_time"), json!(60.0)),
            (json!("week"), json!("lead_time"), json!(60.0)),
        ],
        "{input}"
    );
    // No landing: nothing is judged, so nothing is in breach.
    assert!(
        targets.iter().all(|t| t["state"] == "not_judged"),
        "{input}"
    );
    assert_eq!(input["kpi"]["breaches"], json!([]));
    assert_eq!(input["kpi"]["trend"]["day"].as_array().unwrap().len(), 7);
    assert_eq!(
        input["improvements"],
        json!({"running": 0, "limit": 3, "reached": false, "waiting": []})
    );

    let findings = SqliteQueue::open(&db)
        .unwrap()
        .findings(&dagq::domain::FindingQuery {
            kinds: vec!["kpi".into()],
            ..dagq::domain::FindingQuery::default()
        })
        .unwrap();
    assert_eq!(findings.len(), 1, "{findings:?}");
    assert_eq!(findings[0].finding.subject, "lead_time/all");
    assert_eq!(findings[0].finding.evidence.len(), 2);
}

/// An observer that does what the prompt asks of the KPIs' breaches: it
/// reads `kpi.breaches` from the prompt's inputs and records each breach
/// with its evidence as a finding of its `finding_kind` and `subject`.
struct BreachRecorder;

impl AgentProvider for BreachRecorder {
    fn preflight(&self) -> Result<()> {
        Ok(())
    }
    fn command(&self, _: &TaskRun, _: &str) -> Result<CommandSpec> {
        bail!("the observer has no run")
    }
    fn resume_command(&self, _: &TaskRun) -> Result<CommandSpec> {
        bail!("the observer has no run")
    }
    fn headless_command(&self, cwd: &Path, prompt: &str, _: JobAccess) -> Result<CommandSpec> {
        let start = prompt.rfind("```json\n").unwrap() + "```json\n".len();
        let end = prompt.rfind("\n```").unwrap();
        let input: Value = serde_json::from_str(&prompt[start..end]).unwrap();
        fs::write(cwd.join("seen.json"), input["kpi"].to_string()).unwrap();
        let mut script = String::from("set -e\n");
        for breach in input["kpi"]["breaches"].as_array().unwrap() {
            let Some(evidence) = breach["evidence_event_id"].as_i64() else {
                continue;
            };
            script.push_str(&format!(
                "dagq finding record --kind {} --queue --subject '{}' --summary 'value {} since {}' --evidence {evidence} > /dev/null\n",
                breach["finding_kind"].as_str().unwrap(),
                breach["subject"].as_str().unwrap(),
                breach["value"],
                breach["breach_since"],
            ));
        }
        let mut command = CommandSpec::new("/bin/sh");
        command.current_dir(cwd).arg("-c").arg(script);
        Ok(command)
    }
    fn without_mcp(&self, _: &mut CommandSpec) {}
    fn select_model(&self, _: &mut CommandSpec, _: &str, _: &str) {}
    fn review_command(&self, _: &TaskRun, _: &str, _: JobAccess) -> Result<CommandSpec> {
        bail!("the observer reviews no run")
    }
}

/// ADR-0070 decisions 4 and 5: a forecast that missed its p90 is scored
/// in the observer's input (`kpi.forecast`), the target on its p90 hit
/// rate goes into breach, and the breach is recorded as a `forecast`
/// finding, not a `kpi` one.
#[test]
fn observe_reads_the_forecast_errors_and_a_forecast_breach_becomes_a_forecast_finding() {
    use dagq::observer::ObserveMode;
    let (_dir, repo, db) = fixture();
    SqliteQueue::open(&db)
        .unwrap()
        .bind_repository(repo.join(".git").to_str().unwrap())
        .unwrap();
    // The queue's host.toml wins over the host-wide one of whoever runs the
    // tests: one sample judges a day, and one day off target is a breach.
    fs::write(
        db.parent().unwrap().join("host.toml"),
        "[kpi]\nmin_samples = 1\nbreach_periods = 1\nbreach_weeks = 1\n\
         [kpi.targets.\"forecast.p90_hit_rate\"]\nmin = 0.75\n",
    )
    .unwrap();
    // Two days ago, a snapshot gave task 1 a p50 of 1 hour and a p90 of 2;
    // it completed 4 hours later: late past its p90.
    let conn = Connection::open(&db).unwrap();
    conn.execute(
        "INSERT INTO run_events(kind, payload, created_at) VALUES ('forecast_recorded',
           json_object('at_secs', CAST(strftime('%s','now','-40 hours') AS INTEGER), 'method', 1,
             'tasks', json_array(json_object('id', 1, 'p50_secs', 3600, 'p90_secs', 7200)),
             'goals', json_array()),
           strftime('%Y-%m-%dT%H:%M:%fZ','now','-40 hours'))",
        [],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO run_events(task_id, kind, payload, created_at) VALUES (1, 'task_status_changed',
           json_object('from', 'in_progress', 'to', 'completed'),
           strftime('%Y-%m-%dT%H:%M:%fZ','now','-36 hours'))",
        [],
    )
    .unwrap();
    drop(conn);
    let started = SqliteQueue::open(&db)
        .unwrap()
        .record_kpi_breach(
            EventKind::KpiBreachStarted,
            json!({"period": "day", "kpi": "forecast.p90_hit_rate", "stratum": "all"}),
            None,
        )
        .unwrap();
    assert!(started.is_some());

    let done = observe(&db, &BreachRecorder, &observe_options(ObserveMode::Hourly)).unwrap();
    assert_eq!(done["outcome"], "succeeded", "{done}");
    let dir = PathBuf::from(done["dir"].as_str().unwrap());
    let kpi: Value =
        serde_json::from_str(&fs::read_to_string(dir.join("seen.json")).unwrap()).unwrap();

    // The scoring is in the input: the day of the finish, one sample, the
    // p90 missed and the p50 4 times short (3 hours late over 1).
    let scored = kpi["forecast"]["day"].as_array().unwrap();
    assert_eq!(scored.len(), 1, "{kpi}");
    assert_eq!(scored[0]["details"]["samples"], 1, "{kpi}");
    let kpis = &scored[0]["kpis"];
    assert_eq!(kpis["forecast.p90_hit_rate"]["all"]["value"], 0.0, "{kpi}");
    assert_eq!(kpis["forecast.late_rate"]["all"]["value"], 1.0, "{kpi}");
    assert_eq!(
        kpis["forecast.p50_error"]["all"]["median"],
        3.0 * 3600.0,
        "{kpi}"
    );
    assert_eq!(
        kpis["forecast.p50_error_ratio"]["marks=0"]["median"], 3.0,
        "{kpi}"
    );

    let breach = kpi["breaches"]
        .as_array()
        .unwrap()
        .iter()
        .find(|breach| breach["period"] == "day")
        .unwrap_or_else(|| panic!("no breach of the days: {kpi}"));
    assert_eq!(breach["finding_kind"], "forecast");
    assert_eq!(breach["subject"], "p90_hit_rate/all");
    assert_eq!(breach["value"], 0.0);
    assert!(breach["evidence_event_id"].is_i64(), "{breach}");

    let queue = SqliteQueue::open(&db).unwrap();
    let findings = queue
        .findings(&dagq::domain::FindingQuery {
            kinds: vec!["forecast".into()],
            ..dagq::domain::FindingQuery::default()
        })
        .unwrap();
    assert_eq!(findings.len(), 1, "{findings:?}");
    assert_eq!(findings[0].finding.subject, "p90_hit_rate/all");
    let kpi_findings = queue
        .findings(&dagq::domain::FindingQuery {
            kinds: vec!["kpi".into()],
            ..dagq::domain::FindingQuery::default()
        })
        .unwrap();
    assert!(kpi_findings.is_empty(), "{kpi_findings:?}");
}

#[test]
fn observe_counts_the_findings_the_observer_closed_and_no_other_close() {
    use dagq::domain::{FindingStatus, FindingTarget, NewFinding};
    use dagq::observer::ObserveMode;
    let (_dir, _repo, db) = fixture();
    let record = |queue: &mut SqliteQueue, subject: &str| {
        queue
            .record_finding(NewFinding {
                kind: "capacity".into(),
                target: FindingTarget::Queue,
                subject: subject.into(),
                summary: format!("{subject} again"),
                detail: None,
                impact: None,
                evidence: Vec::new(),
                propose: None,
                by: "observer".into(),
            })
            .unwrap()
            .finding
            .id
    };
    let (reopened, resolved) = {
        let mut queue = SqliteQueue::open(&db).unwrap();
        let reopened = record(&mut queue, "reopened");
        queue
            .set_finding_status(reopened, FindingStatus::Resolved, "fixed", "human")
            .unwrap();
        (reopened, record(&mut queue, "resolved"))
    };
    // The observer resolves one finding and reopens another by recording
    // it again (`to: open`); a person dismisses the one it records.
    let provider = ObserverProvider {
        script: format!(
            r#"
set -e
q() {{ dagq "$@" > /dev/null; }}
q finding resolve {resolved} --reason 'slots are busy again'
q finding record --kind capacity --queue --subject reopened --summary 'reopened again' --evidence 1
q finding record --kind capacity --queue --subject dismissed --summary 'dismissed later'
env -u DAGQ_ROLE -u DAGQ_SERVICE_SOCKET -u DAGQ_SERVICE_CREDENTIAL_FILE dagq --db "{db}" finding dismiss {dismissed} --reason 'not a problem' > /dev/null
"#,
            dismissed = resolved.as_i64() + 1,
            db = db.display()
        ),
    };
    let finished = observe(&db, &provider, &observe_options(ObserveMode::Hourly)).unwrap();
    assert_eq!(finished["outcome"], "succeeded", "{finished}");
    assert_eq!(finished["findings_closed"], 1, "{finished}");
    assert_eq!(finished["closed_finding_ids"], json!([resolved]));
    assert_eq!(finished["updated_finding_ids"], json!([reopened]));
    let changes = queue_events(&db, "finding_status_changed");
    assert!(
        changes
            .iter()
            .any(|c| c["by"] == "observer" && c["to"] == "open"),
        "{changes:?}"
    );
    assert!(
        changes
            .iter()
            .any(|c| c["by"] == "human" && c["to"] == "dismissed"),
        "{changes:?}"
    );

    let output = {
        use crate::common::{Bounded, WithoutActor};
        std::process::Command::new(env!("CARGO_BIN_EXE_dagq"))
            .without_actor_env()
            .args(["--db", db.to_str().unwrap(), "observe", "--history"])
            .bounded_output()
            .unwrap()
    };
    assert!(output.status.success(), "{output:?}");
    let history: Value = serde_json::from_slice(&output.stdout).unwrap();
    let findings = &history["observations"][0]["findings"];
    assert_eq!(
        (&findings["closed"], &findings["closed_ids"]),
        (&json!(1), &json!([resolved]))
    );
}

/// A supervisor (not `--once`) started on a fixture where the hourly
/// observation is due, whose agent starts a child and waits; the thread,
/// and the pids of the agent and its child once both run.
fn supervisor_observing(db: &Path, repo: &Path) -> (thread::JoinHandle<Result<Value>>, u32, u32) {
    {
        let mut queue = SqliteQueue::open(db).unwrap();
        queue
            .transition(TaskId::new(1), TaskAction::Cancel)
            .unwrap();
    }
    let pids = db.parent().unwrap().join("observer.pids");
    let stub = db.parent().unwrap().join("claude-observer-stub");
    crate::common::template::script(
        &stub,
        r#"#!/bin/sh
if [ "$1" = "-p" ]; then
  sleep 60 &
  echo "$$ $!" > "${0%/*}/observer.pids.tmp" && mv "${0%/*}/observer.pids.tmp" "${0%/*}/observer.pids"
  wait
  exit 0
fi
printf 'test provider\n'
"#,
    );

    let backend = TestWorkspace::new(db, false, VALID_AGENT);
    let options = SuperviseOptions {
        observe_interval: Duration::from_secs(3600),
        observe_daily: false,
        update: dagq::application::supervise::UpdateSettings {
            cmux: Some(PathBuf::from("/usr/bin/true")),
            ..Default::default()
        },
        ..SuperviseOptions::new(1, false)
    };
    let supervisor = {
        let (db, repo) = (db.to_path_buf(), repo.to_path_buf());
        thread::spawn(move || {
            runtime::supervise(
                &db,
                &repo,
                &backend,
                &stub,
                Path::new(env!("CARGO_BIN_EXE_dagq")),
                &options,
            )
        })
    };
    let written = written_to(&pids);
    let (agent, child) = written.split_once(' ').unwrap();
    assert_eq!(queue_events(db, "observe_started").len(), 1);
    (supervisor, agent.parse().unwrap(), child.parse().unwrap())
}

fn joined(supervisor: thread::JoinHandle<Result<Value>>) -> Result<Value> {
    let _waiting = common::within(common::STEP_LIMIT, "the supervisor thread to return");
    supervisor.join().unwrap()
}

/// A supervisor whose loop ends on an error kills the observer it started
/// and what that started: `dagq observe`, its agent and the agent's child
/// (task 245).
#[test]
fn a_failing_supervisor_leaves_no_observer_process() {
    let (_dir, repo, db) = fixture();
    let (supervisor, agent, child) = supervisor_observing(&db, &repo);
    // The heartbeat fails from now on: the loop ends on an error.
    Connection::open(&db)
        .unwrap()
        .execute_batch(
            "CREATE TRIGGER fail_heartbeat BEFORE UPDATE ON supervisors
             BEGIN SELECT RAISE(ABORT, 'heartbeat refused by the test'); END;",
        )
        .unwrap();
    let error = format!("{:#}", joined(supervisor).unwrap_err());
    assert!(error.contains("heartbeat failed"), "{error}");
    assert!(
        gone(agent),
        "the observer's agent {agent} outlived the supervisor"
    );
    assert!(
        gone(child),
        "the agent's child {child} outlived the supervisor"
    );
}

/// A supervisor that hands off (execs another binary) stops the observer
/// with what it started, not `dagq observe` alone (task 245).
#[test]
fn a_handoff_leaves_no_observer_process() {
    let (_dir, repo, db) = fixture();
    let (supervisor, agent, child) = supervisor_observing(&db, &repo);
    let queue = SqliteQueue::open(&db).unwrap();
    let token = queue.supervisors().unwrap()[0].token.clone();
    assert!(queue.request_handoff(&token, "/next/dagq").unwrap());
    let outcome = joined(supervisor).unwrap();
    assert_eq!(outcome["outcome"], "handoff", "{outcome}");
    assert!(
        gone(agent),
        "the observer's agent {agent} outlived the handoff"
    );
    assert!(
        gone(child),
        "the agent's child {child} outlived the handoff"
    );
}

#[test]
fn observe_uses_only_the_selected_cmux_for_workspace_listing() {
    use dagq::observer::ObserveMode;
    let (_dir, _repo, db) = fixture();
    let cmux = db.parent().unwrap().join("selected-cmux");
    let calls = db.parent().unwrap().join("cmux-calls");
    crate::common::template::script(
        &cmux,
        r#"#!/bin/sh
printf '%s\n' "$*" >> "${0%/*}/cmux-calls"
case "$4" in
  list-windows) echo '[{"id":"test-window"}]' ;;
  workspace) echo '{"workspaces":[]}' ;;
esac
"#,
    );
    let provider = ObserverProvider {
        script: "exit 0".into(),
    };
    let mut options = observe_options(ObserveMode::Daily);
    options.dry_run = true;
    let without = observe(&db, &provider, &options).unwrap();
    assert!(!calls.exists());
    options.cmux = Some(db.parent().unwrap().join("missing-cmux"));
    let missing = observe(&db, &provider, &options).unwrap();
    assert_eq!(missing["dry_run"], without["dry_run"]);
    assert!(
        !missing["prompt"]
            .as_str()
            .unwrap()
            .contains("workspace_mismatch")
    );
    assert!(!calls.exists());
    options.cmux = Some(cmux.clone());
    observe(&db, &provider, &options).unwrap();
    assert!(
        fs::read_to_string(&calls)
            .unwrap()
            .contains("workspace list")
    );
    fs::remove_file(&calls).unwrap();
    // Exercise CLI parsing and forwarding as well as the library entry point.
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_dagq"))
        .without_actor_env()
        .args([
            "--db",
            db.to_str().unwrap(),
            "observe",
            "--dry-run",
            "--daily",
            "--cmux",
            cmux.to_str().unwrap(),
        ])
        .bounded_output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        fs::read_to_string(&calls)
            .unwrap()
            .contains("workspace list")
    );
}

#[test]
fn observer_walls_read_both_streams_and_ignore_successful_output() {
    use dagq::observer::ObserveMode;
    for (diagnostic, wall) in [
        ("Invalid API key · Please run /login", "authentication"),
        ("You've hit your limit · resets 5pm", "usage_limit"),
    ] {
        for redirect in ["", " >&2"] {
            let (_dir, _repo, db) = fixture();
            let provider = ObserverProvider {
                script: format!("cat <<'DIAGNOSTIC'{redirect}\n{diagnostic}\nDIAGNOSTIC\nexit 1"),
            };
            let done = observe(&db, &provider, &observe_options(ObserveMode::Daily)).unwrap();
            assert_eq!(done["wall"], wall, "{done}");
            assert!(done["hold_ask_id"].is_number(), "{done}");
            let provider = ObserverProvider {
                script: provider.script.replace("exit 1", "exit 0"),
            };
            let done = observe(&db, &provider, &observe_options(ObserveMode::Daily)).unwrap();
            assert_eq!(done["outcome"], "succeeded");
            assert_eq!(done["wall"], Value::Null);
        }
    }
}

#[test]
fn observer_uses_the_supplied_providers_failure_signals() {
    use dagq::application::{AgentSignals, IdleHook};
    use dagq::domain::headless_job::JobFailure;
    struct Signals;
    impl AgentSignals for Signals {
        fn job_failure(&self, output: &str) -> JobFailure {
            assert_eq!(output, "provider stdout\nprovider stderr");
            JobFailure::UsageLimit
        }
        fn detect_prompt(&self, _: &str) -> Option<&'static str> {
            unreachable!()
        }
        fn screen_excerpt(&self, _: &str) -> String {
            unreachable!()
        }
        fn idle_hook(&self, _: &[u8]) -> IdleHook {
            unreachable!()
        }
        fn input_ready(&self, _: &str) -> bool {
            unreachable!()
        }
        fn input_pending(&self, _: &str, _: &str) -> bool {
            unreachable!()
        }
        fn working(&self, _: &str) -> bool {
            unreachable!()
        }
    }
    let (_dir, _repo, db) = fixture();
    let provider = ObserverProvider {
        script: "printf 'provider stdout'; printf 'provider stderr' >&2; exit 1".into(),
    };
    let done = dagq::observer::observe(
        &db,
        &provider,
        &Signals,
        &observe_options(dagq::observer::ObserveMode::Daily),
    )
    .unwrap();
    assert_eq!(done["wall"], "usage_limit", "{done}");
    assert!(done["hold_ask_id"].is_number());
}
