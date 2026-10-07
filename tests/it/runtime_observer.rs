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
pub(crate) fn observe(
    db: &Path,
    provider: &dyn AgentProvider,
    options: &dagq::application::observer::ObserveOptions,
) -> Result<Value> {
    // The job's `dagq` goes to the queue's service (goal 82's stage (3)),
    // which the fixture stops.
    common::service::serve(db);
    dagq::compose::observe(
        db,
        provider,
        Some(&ClaudeCode {
            executable: "/unused".into(),
        }),
        options,
    )
}

pub(crate) fn observe_options(
    mode: dagq::application::observer::ObserveMode,
) -> dagq::application::observer::ObserveOptions {
    dagq::application::observer::ObserveOptions {
        mode,
        since: None,
        dry_run: false,
        timeout: Duration::from_secs(60),
        dagq: PathBuf::from(env!("CARGO_BIN_EXE_dagq")),
        user_config: None,
        prompt_limit: dagq::application::observer::PROMPT_LIMIT,
        launch: None,
        switchable: false,
        fallback: true,
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

/// Without `[roles.observer]` the agent is given no model or effort; with
/// it in the bound checkout's `dagq.toml`, it is given its model and
/// effort. `observe_started` and the span record which (ADR-0079 decision
/// 7).
#[test]
fn the_observer_takes_its_role_table_and_records_what_it_started_with() {
    use dagq::application::observer::ObserveMode;
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
    use dagq::{application::observer::ObserveMode, infrastructure::observer::read_cursor};
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
q ask --kind blocked --because recovery_failed --finding 2 --question 'slots idle while task 1 is ready' --option 'restart the supervisor' --recommend 'restart the supervisor' --confidence high --cmux /usr/bin/true
q ask --kind blocked --because recovery_failed --finding 2 --question 'the same alert again' --recommend propose --cmux /usr/bin/true
if q ask --kind blocked --because scope --finding 1 --question 'wait for a slot?' --option wait --cmux /usr/bin/true 2> blocked.err; then exit 8; fi
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
    // The stall's reading stayed on its finding: a finding without an ask
    // (ADR-t451-1 decision 2); the capacity finding has its blocked ask.
    assert_eq!(first["findings_without_ask"], 1, "{first}");
    assert_eq!(first["without_ask_finding_ids"], json!([1]), "{first}");
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
    // A blocked ask without --recommend is refused with the reason, not
    // as a denial.
    let blocked = fs::read_to_string(dir.join("blocked.err")).unwrap();
    assert!(
        blocked.contains("a blocked ask needs --recommend"),
        "{blocked}"
    );
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
            .contains("### stats: ")
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
    assert_eq!(
        asks[0].recommendation.as_deref(),
        Some("restart the supervisor")
    );
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
        &dagq::application::observer::ObserveOptions {
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
    assert!(prompt.contains("### findings: "), "{prompt}");
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
        &dagq::application::observer::ObserveOptions {
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
    use dagq::application::observer::ObserveMode;
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
    // A second observation at the same wall joins the same ask.
    let second = observe(&db, &logged_out, &observe_options(ObserveMode::Daily)).unwrap();
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
    use dagq::application::observer::ObserveMode;
    let (_dir, _repo, db) = fixture();
    let child_pid = db.parent().unwrap().join("child.pid");
    let slow = ObserverProvider {
        script: format!(
            "sleep 60 & echo $! > {tmp} && mv {tmp} {pid}; wait",
            tmp = shell_path(child_pid.with_extension("pid.tmp")),
            pid = shell_path(&child_pid)
        ),
    };
    let started = Instant::now();
    let outcome = observe(
        &db,
        &slow,
        &dagq::application::observer::ObserveOptions {
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
  # The prompt comes on stdin (task 1560).
  case "$(cat)" in *"daily observation"*) mode=daily ;; esac
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
                priority: Default::default(),
                title: "observed".into(),
                description: String::new(),
                acceptance: String::new(),
                constraints: String::new(),
                doc: None,
                draft: false,
                tags: Vec::new(),
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
            "#!/bin/sh\nprintf '%s\\n' \"$@\" > \"${{0%/*}}/observer-arguments\"\nexec {} \"$@\"\n",
            shell_path(env!("CARGO_BIN_EXE_dagq"))
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
        arguments.contains("observe\n--claude\n") && !arguments.contains("--cmux"),
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
    assert!(
        dagq::infrastructure::observer::read_cursor(&db)
            .unwrap()
            .is_some()
    );
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
    use dagq::application::observer::ObserveMode;
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
        &dagq::application::observer::ObserveOptions {
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
    use dagq::application::observer::ObserveMode;
    use dagq::domain::{AskKind, AskReason, NewAsk};
    let (_dir, _repo, db) = fixture();
    let mut queue = SqliteQueue::open(&db).unwrap();
    let ask = queue
        .ask(NewAsk {
            recommendation: None,
            confidence: None,
            topics: Vec::new(),
            kind: AskKind::Blocked,
            task_id: None,
            run_id: None,
            question: "is it stuck?".into(),
            options: Vec::new(),
            asked_by: "observer".into(),
            reason_category: AskReason::Scope,
            finding_id: None,
            request_id: None,
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
    use dagq::application::observer::ObserveMode;
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
        &dagq::application::observer::ObserveOptions {
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
        // The whole input is beside the prompt, which carries each breach.
        let input: Value =
            serde_json::from_str(&fs::read_to_string(cwd.join("input.json")).unwrap()).unwrap();
        for breach in input["kpi"]["breaches"].as_array().unwrap() {
            assert!(
                prompt.contains(&format!("\"subject\":{}", breach["subject"])),
                "{prompt}"
            );
        }
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
    use dagq::application::observer::ObserveMode;
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
    use dagq::application::observer::ObserveMode;
    use dagq::domain::{FindingStatus, FindingTarget, NewFinding};
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

/// The observer calls no cmux (ADR-t1433-1): `observe --cmux`, as a
/// supervisor of an older binary passes it, is accepted and ignored, and
/// the cmux it names is never run.
#[test]
fn observe_accepts_its_cmux_and_never_runs_it() {
    let (_dir, _repo, db) = fixture();
    let cmux = db.parent().unwrap().join("selected-cmux");
    let calls = db.parent().unwrap().join("cmux-calls");
    crate::common::template::script(
        &cmux,
        r#"#!/bin/sh
printf '%s\n' "$*" >> "${0%/*}/cmux-calls"
"#,
    );
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
    assert!(!calls.exists(), "the observer ran cmux");
}

#[test]
fn observer_walls_read_both_streams_and_ignore_successful_output() {
    use dagq::application::observer::ObserveMode;
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
        fn idle_hook(&self, _: &[u8]) -> IdleHook {
            unreachable!()
        }
    }
    let (_dir, _repo, db) = fixture();
    let provider = ObserverProvider {
        script: "printf 'provider stdout'; printf 'provider stderr' >&2; exit 1".into(),
    };
    let done = dagq::compose::observe(
        &db,
        &provider,
        Some(&Signals),
        &observe_options(dagq::application::observer::ObserveMode::Daily),
    )
    .unwrap();
    assert_eq!(done["wall"], "usage_limit", "{done}");
    assert!(done["hold_ask_id"].is_number());
}

/// Task 1567 (ADR-t1566-1): an input past the prompt's limit, its required
/// open asks included, leaves the rest out with the command that reads
/// it, which the observer's agent runs as the observer (`dagq` only,
/// `JobAccess::QueueCli`, `DAGQ_ROLE=observer` through the queue's
/// service) to get the whole of what was left out; `observe_started` and
/// `observe --history` record the prompt's bytes per section.
#[test]
fn the_observer_reads_what_its_prompt_left_out_and_the_prompt_bytes_are_recorded() {
    use dagq::application::observer::ObserveMode;
    let (_dir, _repo, db) = fixture();
    let question = "q".repeat(2_000);
    // One open ask per task.
    let mut queue = SqliteQueue::open(&db).unwrap();
    for n in 1..=12 {
        if n > 1 {
            add_ready_task(&mut queue, "asked", &[]);
        }
        crate::common::cli::ok(
            &db,
            &[
                "ask",
                "--kind",
                "decide",
                "--because",
                "scope",
                "--task",
                &n.to_string(),
                "--question",
                &question,
                "--option",
                "a",
                "--cmux",
                "/usr/bin/true",
            ],
        );
    }
    // How big the asks are, and the prompt with everything left out, from
    // dry runs.
    let dry_run = |prompt_limit| {
        observe(
            &db,
            &ObserverProvider {
                script: String::new(),
            },
            &dagq::application::observer::ObserveOptions {
                dry_run: true,
                prompt_limit,
                ..observe_options(ObserveMode::Hourly)
            },
        )
        .unwrap()
    };
    let dry = dry_run(dagq::application::observer::PROMPT_LIMIT);
    let least = dry_run(1)["prompt_bytes"].as_u64().unwrap() as usize;
    let section = |sections: &Value, name: &str| {
        sections
            .as_array()
            .unwrap()
            .iter()
            .find(|section| section["name"] == name)
            .cloned()
            .unwrap()
    };
    assert_eq!(
        section(&dry["prompt_sections"], "open_asks")["omitted"],
        0,
        "{dry}"
    );
    let asks_bytes = section(&dry["prompt_sections"], "open_asks")["bytes"]
        .as_u64()
        .unwrap() as usize;
    // Room for about half the asks: the required section is cut too, and
    // every other section left out.
    let limit = least + asks_bytes / 2 + dagq::application::observer::LANGUAGE_RESERVE;
    let provider = ObserverProvider {
        script: r#"
set -e
read=$(grep -o 'dagq observe --input [0-9-]* --section open_asks --offset [0-9]*' prompt.md | head -n 1)
printf '%s' "$read" > read.txt
$read > left_out.json
"#
        .into(),
    };
    let report = observe(
        &db,
        &provider,
        &dagq::application::observer::ObserveOptions {
            prompt_limit: limit,
            ..observe_options(ObserveMode::Hourly)
        },
    )
    .unwrap();
    assert_eq!(report["outcome"], "succeeded", "{report}");
    let dir = PathBuf::from(report["dir"].as_str().unwrap());
    let prompt = fs::read_to_string(dir.join("prompt.md")).unwrap();
    assert!(prompt.len() <= limit, "{} > {limit}", prompt.len());
    let read = fs::read_to_string(dir.join("read.txt")).unwrap();
    // The read is a `dagq` command, the only tool Claude's observer has.
    assert!(read.starts_with("dagq observe --input "), "{read}");
    let claude = ClaudeCode {
        executable: "claude".into(),
    }
    .headless_command(&dir, "observe", dagq::application::observer::ACCESS)
    .unwrap();
    let args: Vec<_> = claude.get_args().collect();
    assert!(args.iter().any(|arg| *arg == "Bash(dagq:*)"), "{args:?}");
    let started = queue_events(&db, "observe_started").pop().unwrap();
    let asks = section(&started["prompt_sections"], "open_asks");
    let (kept, omitted) = (
        asks["kept"].as_u64().unwrap() as usize,
        asks["omitted"].as_u64().unwrap() as usize,
    );
    assert!(omitted > 0 && kept + omitted == 12, "{asks}");
    assert!(
        prompt.contains(&format!(
            "Left out {omitted} items (from offset {kept}): read them with `{read}`"
        )),
        "{prompt}"
    );
    // The agent read every ask left out, whole, as the observer.
    let left_out: Value =
        serde_json::from_str(&fs::read_to_string(dir.join("left_out.json")).unwrap()).unwrap();
    assert_eq!(left_out["total"], 12, "{left_out}");
    assert_eq!(left_out["offset"], kept, "{left_out}");
    let items = left_out["items"].as_array().unwrap();
    assert_eq!(
        items.len(),
        omitted.min(dagq::application::observer::INPUT_PAGE)
    );
    // Newest first, so the left-out ones are the oldest asks.
    assert_eq!(items[0]["id"], 12 - kept as i64, "{left_out}");
    assert!(items.iter().all(|ask| ask["question"] == question.as_str()));
    assert!(queue_events(&db, "authorization_denied").is_empty());
    // The bytes recorded are the prompt's, section by section.
    assert_eq!(started["prompt_bytes"], prompt.len());
    assert_eq!(started["prompt_limit"], limit);
    let sum: u64 = started["prompt_sections"]
        .as_array()
        .unwrap()
        .iter()
        .map(|section| section["bytes"].as_u64().unwrap())
        .sum();
    assert_eq!(sum as usize, prompt.len());
    let history =
        dagq::application::observer::history(&SqliteQueue::open(&db).unwrap(), 1).unwrap();
    let latest = &history["observations"][0];
    assert_eq!(latest["prompt_bytes"], prompt.len(), "{history}");
    assert_eq!(latest["prompt_sections"], started["prompt_sections"]);
}

/// `observe_finished` counts the observations that ended `error` or
/// `failed` in a row, whatever their mode; a success or a skip ends the
/// run of them, and `observe --history` gives the count (task 1574).
#[test]
fn observe_counts_its_failures_in_a_row_and_a_success_or_a_skip_ends_them() {
    use dagq::application::observer::{ObserveMode, history};
    let (_dir, _repo, db) = fixture();
    let failing = ObserverProvider {
        script: "exit 3".into(),
    };
    let succeeding = ObserverProvider {
        script: "exit 0".into(),
    };
    let unavailable = dagq::application::observer::ObserveOptions {
        unavailable: Some("--no-claude and Codex is not usable".into()),
        ..observe_options(ObserveMode::Hourly)
    };
    let mut counts = Vec::new();
    let mut run = |provider: &ObserverProvider,
                   options: &dagq::application::observer::ObserveOptions,
                   outcome: &str| {
        let done = observe(&db, provider, options).unwrap();
        assert_eq!(done["outcome"], outcome, "{done}");
        counts.push(done["consecutive_failures"].as_i64().unwrap());
    };
    run(&failing, &observe_options(ObserveMode::Hourly), "failed");
    run(&failing, &observe_options(ObserveMode::Daily), "failed");
    run(&succeeding, &unavailable, "error");
    run(
        &succeeding,
        &observe_options(ObserveMode::Hourly),
        "succeeded",
    );
    run(&failing, &observe_options(ObserveMode::Daily), "failed");
    // Nothing happened since the hourly one but the observer's own: a
    // skip, which ends the daily one's run of failures.
    run(
        &succeeding,
        &observe_options(ObserveMode::Hourly),
        "skipped",
    );
    add_ready_task(&mut SqliteQueue::open(&db).unwrap(), "more", &[]);
    run(&failing, &observe_options(ObserveMode::Hourly), "failed");
    assert_eq!(counts, [1, 2, 3, 0, 1, 0, 1]);
    assert_eq!(
        queue_events(&db, "observe_finished")
            .iter()
            .map(|finished| finished["consecutive_failures"].as_i64().unwrap())
            .collect::<Vec<_>>(),
        counts
    );
    let listed = history(&SqliteQueue::open(&db).unwrap(), 10).unwrap();
    let listed = listed["observations"]
        .as_array()
        .unwrap()
        .iter()
        .map(|entry| entry["consecutive_failures"].as_i64().unwrap())
        .rev()
        .collect::<Vec<_>>();
    assert_eq!(listed, counts);
}

/// The walls of a Claude observer: what Claude prints there, the wall the
/// observation finishes with, the reason Claude cannot be used and the
/// queue event of a job that joins the hold ask.
const CLAUDE_WALLS: [(&str, &str, &str, &str); 2] = [
    (
        "Invalid API key \u{b7} Please run /login",
        "authentication",
        "authentication",
        "auth_required",
    ),
    (
        "Claude AI usage limit reached",
        "usage_limit",
        "usage_limit",
        "usage_limited",
    ),
];

/// A Claude Code stand-in for the supervisor's observer that stops at the
/// wall Claude reports with `line`.
fn walled_claude(db: &Path, line: &str) -> PathBuf {
    let stub = db.parent().unwrap().join("claude-walled-stub");
    crate::common::template::script(
        &stub,
        format!(
            "#!/bin/sh\nif [ \"$1\" = \"-p\" ]; then\n  cat > /dev/null\n  printf '%s\\n' '{line}'\n  exit 1\nfi\nprintf 'test provider\\n'\n"
        ),
    );
    stub
}

/// One `--once` pass of a supervisor of the fixture, its task cancelled,
/// whose `[roles.observer]` names Claude with `[provider_fallback] jobs =
/// false`, observing hourly through the child `observe` with `claude`.
fn supervise_claude_observer(db: &Path, repo: &Path, claude: &Path) {
    crate::observer_codex::roles(
        db,
        repo,
        "[roles.observer]\nprovider = \"claude\"\n\n[provider_fallback]\njobs = false\n",
    );
    common::service::serve(db);
    SqliteQueue::open(db)
        .unwrap()
        .transition(TaskId::new(1), TaskAction::Cancel)
        .unwrap();
    let backend = TestWorkspace::new(db, false, VALID_AGENT);
    let options = SuperviseOptions {
        observe_interval: Duration::from_secs(3600),
        observe_daily: false,
        ..supervise_options(1, true)
    };
    runtime::supervise(
        db,
        repo,
        &backend,
        claude,
        Path::new(env!("CARGO_BIN_EXE_dagq")),
        &options,
    )
    .unwrap();
}

/// The open hold asks of the queue.
fn hold_asks(db: &Path) -> Vec<dagq::domain::Ask> {
    SqliteQueue::open(db)
        .unwrap()
        .asks(AskQuery {
            open: true,
            ..Default::default()
        })
        .unwrap()
        .into_iter()
        .filter(|ask| ask.kind == AskKind::QueueHold)
        .collect()
}

/// With `[provider_fallback] jobs = false`, a Claude observer stopped at
/// a login that ran out or at the usage limit is added to the hold ask of
/// its wall twice: by the child `observe` (`observer::hold_wall`) and by
/// the supervisor that reads its finish (`Supervisor::hold_unusable` to
/// `raise_job_wall`). Whichever comes first, the queue records
/// `auth_required` / `usage_limited` once and the ask lists the observer
/// job once. The queue's join itself is
/// `runtime_queue_hold_detect::runs_and_jobs_join_the_one_ask_of_their_wall`'s.
#[test]
fn a_claude_observer_at_its_wall_joins_the_hold_ask_once_from_the_child_and_the_supervisor() {
    use dagq::application::observer::ObserveMode;
    for (line, wall, reason, event) in CLAUDE_WALLS {
        // The child first: it opens the ask and records the event; the
        // supervisor that reads its finish then finds the observer in it.
        let (_dir, repo, db) = fixture();
        supervise_claude_observer(&db, &repo, &walled_claude(&db, line));
        let finished = queue_events(&db, "observe_finished");
        assert_eq!(finished.len(), 1, "{wall}: {finished:?}");
        assert_eq!(finished[0]["wall"], wall, "{finished:?}");
        // The finish the supervisor reads says Claude cannot be used, so
        // it raises the observer at the wall too.
        assert_eq!(
            finished[0]["provider_unusable"],
            json!({"provider": "claude", "reason": reason}),
            "{finished:?}"
        );
        let asks = hold_asks(&db);
        assert_eq!(asks.len(), 1, "{wall}: {asks:?}");
        assert_eq!(finished[0]["hold_ask_id"], json!(asks[0].id));
        assert_eq!(
            asks[0].affected,
            ["observer job"],
            "{wall}, the child first"
        );
        let recorded = queue_events(&db, event);
        assert_eq!(recorded.len(), 1, "{wall}, the child first: {recorded:?}");
        assert_eq!(recorded[0]["job"], "observer");
        assert_eq!(recorded[0]["ask_id"], json!(asks[0].id));
        // The child's record: the supervisor's carries the job's error.
        assert!(recorded[0].get("error").is_none(), "{recorded:?}");
        // It waits for Claude: no observation starts on Codex.
        assert_eq!(queue_events(&db, "observe_started").len(), 1);

        // The supervisor first: the child's hold cannot be written, so the
        // supervisor opens the ask and records the event; the next child
        // at the wall then finds the observer in it.
        let (_dir, repo, db) = fixture();
        Connection::open(&db)
            .unwrap()
            .execute_batch(
                "CREATE TRIGGER child_hold_fails BEFORE INSERT ON asks
                 WHEN NOT EXISTS (SELECT 1 FROM run_events WHERE kind = 'observe_finished')
                 BEGIN SELECT RAISE(ABORT, 'the child cannot write its hold'); END;",
            )
            .unwrap();
        supervise_claude_observer(&db, &repo, &walled_claude(&db, line));
        let finished = queue_events(&db, "observe_finished");
        assert_eq!(finished.len(), 1, "{wall}: {finished:?}");
        assert_eq!(finished[0]["wall"], wall, "{finished:?}");
        assert_eq!(finished[0]["hold_ask_id"], Value::Null, "{finished:?}");
        let asks = hold_asks(&db);
        assert_eq!(asks.len(), 1, "{wall}: {asks:?}");
        assert_eq!(
            asks[0].affected,
            ["observer job"],
            "{wall}, the supervisor first"
        );
        let recorded = queue_events(&db, event);
        assert_eq!(
            recorded.len(),
            1,
            "{wall}, the supervisor first: {recorded:?}"
        );
        assert_eq!(recorded[0]["job"], "observer");
        assert_eq!(recorded[0]["ask_id"], json!(asks[0].id));
        assert!(recorded[0].get("error").is_some(), "{recorded:?}");
        Connection::open(&db)
            .unwrap()
            .execute_batch("DROP TRIGGER child_hold_fails;")
            .unwrap();
        let walled = ObserverProvider {
            script: format!("printf '%s\\n' '{line}'; exit 1"),
        };
        let child = observe(
            &db,
            &walled,
            &dagq::application::observer::ObserveOptions {
                switchable: true,
                fallback: false,
                ..observe_options(ObserveMode::Daily)
            },
        )
        .unwrap();
        assert_eq!(child["wall"], wall, "{child}");
        assert_eq!(child["hold_ask_id"], json!(asks[0].id), "{child}");
        let after = hold_asks(&db);
        assert_eq!(after.len(), 1, "{wall}: {after:?}");
        assert_eq!(
            after[0].affected,
            ["observer job"],
            "{wall}, the child after the supervisor"
        );
        let recorded = queue_events(&db, event);
        assert_eq!(
            recorded.len(),
            1,
            "{wall}, the child after the supervisor: {recorded:?}"
        );
    }
}
