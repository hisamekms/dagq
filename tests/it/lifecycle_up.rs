//! `up` against fakes for launchd and process signals: the idempotent
//! start in launchd mode (and the refusal of the retired in-cmux mode), the
//! preflights (the `[run.env]` programs, Claude Code's folder trust), the
//! pruning of dead registrations, the runs it reports, and the inbox it
//! does not open (ADR-t2159-1 decision 3). No cmux is on the fixture's
//! path. The real launchd path is `tests/e2e.rs`.

use crate::common;
use dagq::domain::LeaseToken;

use common::lifecycle::*;
use dagq::domain::recovery::ProcessInfo;

use dagq::{
    VERSION,
    application::TaskStore,
    domain::{NewTask, SessionRole, TaskAction},
    infrastructure::{adapters::GitRepository, location::QueueLocation, sqlite::SqliteQueue},
    lifecycle::{self, INBOX_ROLE, PLANNER_ROLE},
};
use rusqlite::Connection;
use serde_json::{Value, json};
use std::{fs, time::Duration};

/// A program the `[run.env]` of `dagq.toml` names that the PATH of `up`
/// does not find stops `up` before it starts a supervisor (ADR-0049
/// decision 9); found, `up` reports where.
#[test]
fn up_refuses_a_run_env_program_its_path_does_not_find() {
    let mut fixture = fixture();
    let bin = fixture.repo.parent().unwrap().join("tools");
    fs::create_dir(&bin).unwrap();
    fs::write(
        fixture.repo.join("dagq.toml"),
        "[run.env]\nRUSTC_WRAPPER = 'sccache'\n",
    )
    .unwrap();
    let launchd = FakeLaunchd::new(&fixture.location.db);
    let processes = FakeProcesses::default();
    let error = format!("{:#}", try_up(&fixture, &launchd, &processes).unwrap_err());
    assert!(
        error.contains("RUSTC_WRAPPER = \"sccache\"")
            && error.contains("PATH: /usr/bin:/bin")
            && error.contains("the supervisor was not started"),
        "{error}"
    );
    assert!(
        SqliteQueue::open(&fixture.location.db)
            .unwrap()
            .supervisors()
            .unwrap()
            .is_empty()
    );
    let tool = bin.join("sccache");
    crate::common::template::script(&tool, "#!/bin/sh\n");

    fixture.environment.path = format!("{}:/usr/bin:/bin", bin.display());
    let report = up(&fixture, &launchd, &processes);
    assert_eq!(report["supervisor"]["outcome"], "started", "{report}");
    assert_eq!(
        report["run_env"]["programs"][0]["resolved"],
        tool.to_str().unwrap(),
        "{report}"
    );
}

/// A `dagq.toml` and a `host.toml` that still have the removed resource
/// broker's tables do not stop `up` (ADR-t2125-1): they are not read, no
/// podman is looked for on a PATH without one, and the report says nothing
/// of a broker.
#[test]
fn up_starts_with_the_removed_brokers_tables_left_in_place() {
    let mut fixture = fixture();
    let bin = fixture.repo.parent().unwrap().join("tools");
    fs::create_dir(&bin).unwrap();
    fs::write(
        fixture.repo.join("dagq.toml"),
        "[broker]\nmode = \"required\"\n[broker.package]\nfetch = [\"cargo\", \"fetch\"]\n",
    )
    .unwrap();
    fs::write(
        fixture.location.queue_dir.join("host.toml"),
        "[broker]\nmode = \"preferred\"\npodman = \"/nonexistent/podman\"\n",
    )
    .unwrap();
    fixture.environment.path = format!("{}:/nonexistent", bin.display());
    let launchd = FakeLaunchd::new(&fixture.location.db);
    let processes = FakeProcesses::default();
    let report = dagq::compose::OneShot::system()
        .up(
            &fixture.location,
            &fixture.repo,
            &launchd,
            &processes,
            &fixture.environment,
            &fixture.options,
        )
        .unwrap();
    assert_eq!(report["supervisor"]["outcome"], "started", "{report}");
    assert_eq!(report.get("broker"), None, "{report}");
}

/// A mistake in the language stops `up` before it starts anything; a
/// language set is in `up`'s report (ADR-t616-2). The inbox's prompt gets
/// it from `dagq inbox` (`lifecycle_inbox`).
#[test]
fn up_refuses_a_wrong_language_and_reports_a_right_one() {
    let mut fixture = fixture();
    let config = fixture.repo.parent().unwrap().join("config.toml");
    fs::write(&config, "[language]\ntag = 'ja_JP'\n").unwrap();
    fixture.environment.user_config = Some(config.clone());
    let launchd = FakeLaunchd::new(&fixture.location.db);
    let processes = FakeProcesses::default();
    let error = format!("{:#}", try_up(&fixture, &launchd, &processes).unwrap_err());
    assert!(
        error.contains("config.toml:2")
            && error.contains("BCP 47")
            && error.contains("the supervisor was not started"),
        "{error}"
    );
    assert!(launchd.installs.lock().unwrap().is_empty());

    fs::write(&config, "[language]\ntag = 'ja'\n").unwrap();
    let report = up(&fixture, &launchd, &processes);
    assert_eq!(
        report["language"],
        json!({"tag": "ja", "source": "user"}),
        "{report}"
    );
}

#[test]
fn up_starts_the_agent_once_and_reuses_it_after_and_opens_no_inbox() {
    let fixture = fixture();
    let launchd = FakeLaunchd::new(&fixture.location.db);
    let processes = FakeProcesses::default();
    let root = GitRepository::inspect(&fixture.repo).unwrap().root;

    let first = up(&fixture, &launchd, &processes);
    assert_eq!(first["supervisor"]["outcome"], "started", "{first}");
    assert_eq!(first["supervisor"]["mode"], "launchd");
    assert_eq!(first["supervisor"]["workspace_id"], Value::Null);
    assert_eq!(first["supervisor"]["pid"], json!(std::process::id()));
    assert_eq!(
        first["supervisor"]["plist"],
        json!(fixture.location.launch_agent)
    );
    assert_eq!(
        first["supervisor"]["log_dir"],
        json!(fixture.location.log_dir)
    );
    // `up` opens no planner (ADR-0041 decision 6) and no inbox
    // (ADR-t2159-1 decision 3), and the report names no other session.
    let keys: Vec<&String> = first.as_object().unwrap().keys().collect();
    assert_eq!(
        keys,
        [
            "doctor",
            "inbox",
            "language",
            "migrated",
            "pruned_supervisors",
            "repository",
            "retired_sessions",
            "supervisor",
        ]
    );
    assert_eq!(
        first["inbox"],
        json!({"outcome": "not_opened", "next": lifecycle::INBOX_NEXT}),
        "{first}"
    );
    assert!(lifecycle::INBOX_NEXT.contains("`dagq inbox`"));
    // What the preflight resolved (ADR-t615-1).
    assert_eq!(
        first["repository"],
        json!({
            "branch": "main", "branch_source": "main", "remote": "origin",
            "remote_source": "default", "remote_exists": false, "push": true,
        })
    );
    assert_eq!(first["retired_sessions"], 0);
    assert_eq!(first["pruned_supervisors"], json!([]));
    assert_eq!(first["doctor"]["unfinished_runs"], json!([]));
    assert_eq!(first["doctor"]["awaiting_integration"], json!([]));
    assert_eq!(first["doctor"]["needs_session"], json!([]));

    // The agent definition launchd received.
    let installs = launchd.installs.lock().unwrap();
    assert_eq!(installs.len(), 1);
    let (label, path, contents) = &installs[0];
    assert_eq!(label, &fixture.location.label);
    assert!(label.starts_with("com.dagq."));
    assert_eq!(path, &fixture.location.launch_agent);
    assert_eq!(
        path,
        &fixture
            ._dir
            .path()
            .join("home/Library/LaunchAgents")
            .join(format!("{label}.plist"))
    );
    let db = fixture.location.db.canonicalize().unwrap();
    // The planners the runtime opens load the plugin `up` was given.
    let plugin_dir = fixture
        .options
        .plugin_dir
        .as_ref()
        .unwrap()
        .canonicalize()
        .unwrap();
    let string = |text: &str| format!("<string>{text}</string>");
    assert!(contents.contains(&format!("<key>Label</key>\n\t{}", string(label))));
    let arguments = [
        "/opt/bin/dagq",
        "--db",
        db.to_str().unwrap(),
        "supervise",
        "--log-dir",
        fixture.location.log_dir.to_str().unwrap(),
        "--cmux",
        fixture.options.cmux.to_str().unwrap(),
        "--claude",
        fixture.options.claude.to_str().unwrap(),
        // Fixed like `--claude` (ADR-t813-2).
        "--codex",
        fixture.options.codex.to_str().unwrap(),
        // What the start mark records (ADR-0051 decision 10).
        "--mode",
        "launchd",
        "--plugin-dir",
        plugin_dir.to_str().unwrap(),
        // Given to `up`, so passed on (task 698).
        "--parallel",
        "2",
    ]
    .iter()
    .map(|argument| format!("\t\t{}\n", string(argument)))
    .collect::<String>();
    assert!(
        contents.contains(&format!(
            "<key>ProgramArguments</key>\n\t<array>\n{arguments}\t</array>"
        )),
        "{contents}"
    );
    assert!(contents.contains(&format!(
        "<key>WorkingDirectory</key>\n\t{}",
        string(root.to_str().unwrap())
    )));
    assert!(contents.contains(
        "<key>EnvironmentVariables</key>\n\t<dict>\n\t\t<key>PATH</key>\n\t\t<string>/usr/bin:/bin:/home/u/.local/bin</string>\n\t</dict>"
    ));
    // The supervisor calls no cmux, so the agent carries no cmux variable.
    assert!(!contents.contains("CMUX_"));
    assert!(contents.contains("<key>KeepAlive</key>\n\t<true/>"));
    assert!(contents.contains("<key>RunAtLoad</key>\n\t<true/>"));
    let launchd_log = fixture.location.log_dir.join("launchd.log");
    assert!(contents.contains(&format!(
        "<key>StandardOutPath</key>\n\t{}",
        string(launchd_log.to_str().unwrap())
    )));
    drop(installs);

    // Nothing of the inbox is recorded: a person opens it.
    let queue = SqliteQueue::open(&fixture.location.db).unwrap();
    assert_eq!(queue.session_workspace(SessionRole::Inbox).unwrap(), None);
    assert_eq!(queue.session_workspace(SessionRole::Planner).unwrap(), None);
    assert!(queue.latest_event_of("inbox_opened").unwrap().is_none());

    let second = up(&fixture, &launchd, &processes);
    assert_eq!(second["supervisor"]["outcome"], "reused", "{second}");
    assert_eq!(second["supervisor"]["mode"], "launchd");
    assert_eq!(second["supervisor"]["pid"], json!(std::process::id()));
    assert_eq!(second["inbox"], first["inbox"], "{second}");
    assert_eq!(second["pruned_supervisors"], json!([]));
    assert_eq!(launchd.installs.lock().unwrap().len(), 1);
    assert!(launchd.uninstalls.lock().unwrap().is_empty());
    assert_eq!(
        SqliteQueue::open(&fixture.location.db)
            .unwrap()
            .supervisors()
            .unwrap()
            .len(),
        1
    );
}

#[test]
fn up_fails_when_the_started_supervisor_never_registers() {
    let fixture = fixture();
    let mut launchd = FakeLaunchd::new(&fixture.location.db);
    launchd.registers_on_install = false;
    let mut options = fixture.options.clone();
    options.startup_timeout = Duration::from_millis(100);
    let error = lifecycle::up(
        &fixture.location,
        &fixture.repo,
        &launchd,
        &FakeProcesses::default(),
        &fixture.environment,
        &options,
    )
    .unwrap_err();
    let message = format!("{error:#}");
    assert!(message.contains("did not register within 0s"), "{message}");
    assert!(message.contains("launchd.log"), "{message}");
    // The agent stays loaded for inspection.
    assert!(*launchd.loaded.lock().unwrap());
}

/// `up` may run from a linked worktree: trust is still read at the main
/// checkout's root, the key Claude Code uses for every worktree.
#[test]
fn up_from_a_linked_worktree_checks_the_trust_of_the_main_checkout() {
    let mut fixture = fixture();
    let worktree = fixture._dir.path().join("linked");
    git(
        &fixture.repo,
        &[
            "worktree",
            "add",
            "-q",
            worktree.to_str().unwrap(),
            "-b",
            "linked",
        ],
    );
    fixture.repo = worktree;
    let launchd = FakeLaunchd::new(&fixture.location.db);
    let processes = FakeProcesses::default();
    let result = up(&fixture, &launchd, &processes);
    assert_eq!(result["supervisor"]["outcome"], "started", "{result}");
}

/// Run worktrees take Claude Code's folder trust from the repository root,
/// so an untrusted root would stop every run session at the trust dialog:
/// `up` refuses before it starts anything and says how to trust the root.
#[test]
fn up_fails_when_claude_code_has_not_trusted_the_repository() {
    let mut fixture = fixture();
    let config = fixture.environment.claude_config.clone().unwrap();
    let root = fixture.repo.canonicalize().unwrap();
    let cases = [
        // No config at all, no config path, another project trusted only,
        // and the root recorded with the dialog not accepted.
        None,
        Some(None),
        Some(Some(
            json!({"projects": {"/elsewhere": {"hasTrustDialogAccepted": true}}}),
        )),
        Some(Some(
            json!({"projects": {root.to_str().unwrap(): {"hasTrustDialogAccepted": false}}}),
        )),
    ];
    for case in cases {
        match &case {
            None => fixture.environment.claude_config = None,
            Some(written) => {
                fixture.environment.claude_config = Some(config.clone());
                match written {
                    Some(value) => fs::write(&config, value.to_string()).unwrap(),
                    None => {
                        let _ = fs::remove_file(&config);
                    }
                }
            }
        }
        let launchd = FakeLaunchd::new(&fixture.location.db);
        let processes = FakeProcesses::default();
        let message = format!("{:#}", try_up(&fixture, &launchd, &processes).unwrap_err());
        assert!(
            message.starts_with(&format!(
                "Claude Code has not trusted the repository {}",
                root.display()
            )),
            "{case:?}: {message}"
        );
        assert!(message.contains("Yes, I trust this folder"), "{message}");
        assert!(launchd.installs.lock().unwrap().is_empty());
    }

    // A config that cannot be parsed is an error of its own, not a trust verdict.
    fixture.environment.claude_config = Some(config.clone());
    fs::write(&config, "not json").unwrap();
    let launchd = FakeLaunchd::new(&fixture.location.db);
    let processes = FakeProcesses::default();
    let message = format!("{:#}", try_up(&fixture, &launchd, &processes).unwrap_err());
    assert!(message.contains("parse Claude Code config"), "{message}");
}

/// The in-cmux mode is retired (ADR-t1433-4): `up --in-cmux` is refused
/// with the reason and the way to move a supervisor running in a cmux
/// workspace to launchd, before anything is checked, started or opened.
#[test]
fn up_refuses_the_in_cmux_mode_with_the_way_to_launchd_and_touches_nothing() {
    let mut fixture = fixture();
    fixture.options.in_cmux = true;
    let launchd = FakeLaunchd::new(&fixture.location.db);
    let processes = FakeProcesses::default();
    let message = format!("{:#}", try_up(&fixture, &launchd, &processes).unwrap_err());
    assert_eq!(message, lifecycle::IN_CMUX_RETIRED);
    assert!(message.contains("the in-cmux mode is retired"), "{message}");
    assert!(
        message.contains("`down --wait` and then `up` without --in-cmux"),
        "{message}"
    );
    assert!(launchd.installs.lock().unwrap().is_empty());
    assert!(launchd.uninstalls.lock().unwrap().is_empty());
    assert!(!fixture.location.launch_agent.exists());
    assert!(
        SqliteQueue::open(&fixture.location.db)
            .unwrap()
            .supervisors()
            .unwrap()
            .is_empty()
    );
}

/// The launchd supervisor calls no cmux, so `up` starts it without asking
/// cmux anything and gives it no cmux socket password: the agent carries no
/// `CMUX_*` variable (ADR-t1433-4 decision 1, ADR-t2159-1 decision 1).
#[test]
fn up_starts_the_launchd_supervisor_with_no_cmux_check_or_password() {
    let mut fixture = fixture();
    fixture.options.no_claude = true;
    fixture.options.plugin_dir = None;
    let launchd = FakeLaunchd::new(&fixture.location.db);
    let processes = FakeProcesses::default();
    let report = up(&fixture, &launchd, &processes);
    assert_eq!(report["supervisor"]["outcome"], "started", "{report}");
    assert_eq!(report["supervisor"]["mode"], "launchd", "{report}");
    assert_eq!(report["supervisor"]["workspace_id"], Value::Null);
    let installs = launchd.installs.lock().unwrap();
    assert_eq!(installs.len(), 1);
    assert!(!installs[0].2.contains("CMUX_"), "{}", installs[0].2);
    assert!(
        installs[0]
            .2
            .contains("<string>--mode</string>\n\t\t<string>launchd</string>"),
        "{}",
        installs[0].2
    );
    let queue = SqliteQueue::open(&fixture.location.db).unwrap();
    let supervisors = queue.supervisors().unwrap();
    assert_eq!(supervisors.len(), 1);
    assert_eq!(
        supervisors[0].mode,
        Some(dagq::domain::SupervisorMode::Launchd)
    );
    assert_eq!(supervisors[0].workspace_id, None);
    assert_eq!(
        queue.session_workspace(SessionRole::Supervisor).unwrap(),
        None
    );
}

/// An `XDG_CONFIG_HOME` exported by the invoking shell goes into the agent,
/// so the launchd-run supervisor reads the `config.toml` and `host.toml`
/// under it like the `up` that checked them (task 749); unset, the plist
/// carries PATH only
/// (`up_starts_the_agent_and_the_sessions_once_and_reuses_them_after`).
#[test]
fn up_stores_the_exported_config_home_in_the_plist() {
    let mut fixture = fixture();
    fixture.environment.config_home = Some("/home/u/my config & <co>".into());
    let launchd = FakeLaunchd::new(&fixture.location.db);
    let processes = FakeProcesses::default();
    let report = up(&fixture, &launchd, &processes);
    assert_eq!(report["supervisor"]["outcome"], "started", "{report}");
    let installs = launchd.installs.lock().unwrap();
    assert_eq!(installs.len(), 1);
    let contents = &installs[0].2;
    assert!(
        contents.contains(
            "<key>EnvironmentVariables</key>\n\t<dict>\n\t\t<key>PATH</key>\n\t\t<string>/usr/bin:/bin:/home/u/.local/bin</string>\n\t\t<key>XDG_CONFIG_HOME</key>\n\t\t<string>/home/u/my config &amp; &lt;co&gt;</string>\n\t</dict>"
        ),
        "{contents}"
    );
    assert!(!contents.contains("CMUX_"));
}

/// Two registrations whose processes are gone, one whose process lives and
/// holds a lease: `up` drops the dead ones only and reuses the live one.
#[test]
fn up_prunes_dead_registrations_and_keeps_live_ones_and_leases() {
    let fixture = fixture();
    let mut queue = SqliteQueue::open(&fixture.location.db).unwrap();
    let dead = [dead_pid(), dead_pid()];
    queue
        .register_supervisor(&LeaseToken::new("dead-1"), dead[0], 4, VERSION)
        .unwrap();
    queue
        .register_supervisor(&LeaseToken::new("dead-2"), dead[1], 1, VERSION)
        .unwrap();
    queue
        .register_supervisor(&LeaseToken::new("live"), std::process::id(), 3, VERSION)
        .unwrap();
    let task = queue
        .add(NewTask {
            title: "held".into(),
            description: String::new(),
            acceptance: String::new(),
            verification_commands: vec![],
            required_evidence: Vec::new(),
            paths: Vec::new(),
            priority: Default::default(),
            change: None,
            dependencies: vec![],
            goal_dependencies: Vec::new(),
            goal_id: None,
            context: String::new(),
            provider: None,
            worker_mode: Some(dagq::domain::worker::WorkerMode::Interactive),
            wait_for_build: false,
        })
        .unwrap();
    queue
        .transition(task.id(), TaskAction::BypassReview)
        .unwrap();
    let repository = GitRepository::inspect(&fixture.repo).unwrap();
    queue
        .claim_for_supervisor(&repository.main_head().unwrap(), &LeaseToken::new("live"))
        .unwrap();
    let leases_before = queue.run_leases().unwrap();
    assert_eq!(leases_before.len(), 1);

    let launchd = FakeLaunchd::new(&fixture.location.db);
    let processes = FakeProcesses::default();
    processes.dead.lock().unwrap().extend(dead);
    let report = up(&fixture, &launchd, &processes);
    let pruned = report["pruned_supervisors"].as_array().unwrap();
    assert_eq!(pruned.len(), 2, "{report}");
    assert_eq!(pruned[0], json!({"token": "dead-1", "pid": dead[0]}));
    assert_eq!(pruned[1], json!({"token": "dead-2", "pid": dead[1]}));
    assert_eq!(report["supervisor"]["outcome"], "reused");
    assert_eq!(report["supervisor"]["token"], "live");
    // Nobody started this one through `up`, so it has no mode and no
    // workspace, and `up` does not claim one for it.
    assert_eq!(report["supervisor"]["mode"], Value::Null, "{report}");
    assert_eq!(report["supervisor"]["workspace_id"], Value::Null);
    assert_eq!(remaining_mode(&queue, "live"), None);
    assert!(launchd.installs.lock().unwrap().is_empty());
    let remaining = queue.supervisors().unwrap();
    assert_eq!(remaining.len(), 1);
    assert_eq!(remaining[0].token, "live");
    assert_eq!(remaining[0].parallel, 3);
    let leases_after = queue.run_leases().unwrap();
    assert_eq!(leases_after.len(), 1);
    assert_eq!(leases_after[0].run_id, leases_before[0].run_id);
    assert_eq!(leases_after[0].token, "live");
    // The claimed run is reported as unfinished with a live lease.
    let unfinished = report["doctor"]["unfinished_runs"].as_array().unwrap();
    assert_eq!(unfinished.len(), 1);
    assert_eq!(unfinished[0]["run_id"], json!(leases_before[0].run_id));
    assert_eq!(unfinished[0]["task_id"], json!(task.id()));
    assert_eq!(unfinished[0]["status"], "claimed");
    assert_eq!(unfinished[0]["lease_stale"], false);

    // A live registration that stopped heartbeating is neither pruned nor reused.
    Connection::open(&fixture.location.db)
        .unwrap()
        .execute("UPDATE supervisors SET heartbeat_at=1700000000", [])
        .unwrap();
    let report = up(&fixture, &launchd, &processes);
    assert_eq!(report["supervisor"]["outcome"], "started", "{report}");
    assert_eq!(report["pruned_supervisors"], json!([]));
    assert_eq!(queue.supervisors().unwrap().len(), 2);
}

/// A registration that is alive but no longer heartbeating is neither
/// pruned nor reused, and it may start heartbeating again while `up` waits
/// for the supervisor it just started under launchd. `up` must not take it
/// for the one it started: the mode it writes would land on a supervisor
/// `up` did not start, and the report would name the wrong pid and token.
/// The choice itself is the unit test
/// `application::lifecycle::tests::the_started_supervisor_is_the_first_fresh_registration_not_seen_before`.
#[test]
fn up_does_not_mistake_a_silent_supervisor_for_the_one_it_started() {
    let fixture = fixture();
    let mut queue = SqliteQueue::open(&fixture.location.db).unwrap();
    // Registered before `up`, alive, last heartbeat far in the past, and
    // first in `started_at` order.
    queue
        .register_supervisor(&LeaseToken::new("silent"), std::process::id(), 4, VERSION)
        .unwrap();
    Connection::open(&fixture.location.db)
        .unwrap()
        .execute("UPDATE supervisors SET heartbeat_at=1700000000", [])
        .unwrap();
    let launchd = FakeLaunchd {
        heartbeats_existing_on_install: true,
        ..FakeLaunchd::new(&fixture.location.db)
    };
    let processes = FakeProcesses::default();

    let report = up(&fixture, &launchd, &processes);
    assert_eq!(report["supervisor"]["outcome"], "started", "{report}");
    assert_eq!(report["supervisor"]["mode"], "launchd");
    assert_ne!(report["supervisor"]["token"], "silent", "{report}");
    // The silent registration heartbeats again but is untouched; the new
    // one carries the mode `up` wrote.
    assert_eq!(remaining_mode(&queue, "silent"), None);
    let registrations = queue.supervisors().unwrap();
    assert_eq!(registrations.len(), 2, "{registrations:?}");
    let started = registrations
        .iter()
        .find(|registration| registration.token != "silent")
        .expect("the started supervisor is registered");
    assert_eq!(started.mode, Some(dagq::domain::SupervisorMode::Launchd));
    assert_eq!(report["supervisor"]["token"], started.token.as_str());
}

/// Task 330: the rows of supervisors whose process is gone do not outlive
/// `up`, whatever wrote them. A dead pid goes whether or not its row has a
/// `binary_version` (one registered before the column existed has none,
/// as the row a `down --wait` of 2026-09-25 left had). A pid alive again
/// after the row stopped heartbeating goes too when it is not the process
/// that registered: one of another user (not among this user's
/// processes) or one that started after the registration. A silent
/// process that started before its registration is kept, and so is every
/// row when the processes cannot be listed.
#[test]
fn up_prunes_rows_whose_pid_is_dead_or_taken_by_another_process() {
    let fixture = fixture();
    let mut queue = SqliteQueue::open(&fixture.location.db).unwrap();
    let dead = dead_pid();
    let (foreign, younger, silent) = (4_000_001, 4_000_002, 4_000_003);
    for (token, pid) in [
        ("dead-unversioned", dead),
        ("foreign", foreign),
        ("younger", younger),
        ("silent", silent),
    ] {
        queue
            .register_supervisor(&LeaseToken::new(token), pid, 1, VERSION)
            .unwrap();
    }
    let registered_at = 1_700_000_000;
    let db = Connection::open(&fixture.location.db).unwrap();
    db.execute(
        "UPDATE supervisors SET started_at=?1, heartbeat_at=?1",
        [registered_at],
    )
    .unwrap();
    db.execute(
        "UPDATE supervisors SET binary_version=NULL WHERE token='dead-unversioned'",
        [],
    )
    .unwrap();
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let listed = |pid, elapsed_secs| ProcessInfo {
        pid,
        ppid: 1,
        elapsed_secs,
        command: "dagq supervise".into(),
        cwd: None,
        cpu_ms: None,
    };

    let launchd = FakeLaunchd::new(&fixture.location.db);
    let processes = FakeProcesses::default();
    processes.dead.lock().unwrap().insert(dead);
    // The listing fails: only the dead pid goes.
    let report = up(&fixture, &launchd, &processes);
    assert_eq!(
        report["pruned_supervisors"],
        json!([{"token": "dead-unversioned", "pid": dead}]),
        "{report}"
    );
    let tokens = |queue: &SqliteQueue| -> Vec<String> {
        let mut tokens: Vec<String> = queue
            .supervisors()
            .unwrap()
            .into_iter()
            .map(|registration| registration.token.into_string())
            .filter(|token| ["foreign", "younger", "silent"].contains(&token.as_str()))
            .collect();
        tokens.sort();
        tokens
    };
    assert_eq!(tokens(&queue), ["foreign", "silent", "younger"]);

    // `younger` started a minute ago, long after its row; `silent` before.
    *processes.listed.lock().unwrap() = Some(vec![
        listed(younger, 60),
        listed(silent, now - registered_at as u64 + 60),
    ]);
    let report = up(&fixture, &launchd, &processes);
    assert_eq!(
        report["pruned_supervisors"],
        json!([
            {"token": "foreign", "pid": foreign, "reason": "pid_reused"},
            {"token": "younger", "pid": younger, "reason": "pid_reused"},
        ]),
        "{report}"
    );
    assert_eq!(tokens(&queue), ["silent"]);
    let stops: Vec<Value> = db
        .prepare("SELECT payload FROM run_events WHERE kind='supervisor_stopped' ORDER BY id")
        .unwrap()
        .query_map([], |row| row.get::<_, String>(0))
        .unwrap()
        .map(|payload| serde_json::from_str(&payload.unwrap()).unwrap())
        .collect();
    assert_eq!(stops.len(), 3, "{stops:?}");
    assert!(stops.iter().all(|stop| stop["outcome"] == "pruned"));
    assert_eq!(stops[0]["supervisor"], "dead-unversioned");
    assert_eq!(stops[0]["dagq_version"], Value::Null);
}

#[test]
fn up_reports_runs_that_wait_for_a_person_or_the_supervisor() {
    let fixture = fixture();
    let mut queue = SqliteQueue::open(&fixture.location.db).unwrap();
    let repository = GitRepository::inspect(&fixture.repo).unwrap();
    let mut claim = |title: &str| {
        let task = queue
            .add(NewTask {
                title: title.into(),
                description: String::new(),
                acceptance: String::new(),
                verification_commands: vec![],
                required_evidence: Vec::new(),
                paths: Vec::new(),
                priority: Default::default(),
                change: None,
                dependencies: vec![],
                goal_dependencies: Vec::new(),
                goal_id: None,
                context: String::new(),
                provider: None,
                worker_mode: Some(dagq::domain::worker::WorkerMode::Interactive),
                wait_for_build: false,
            })
            .unwrap();
        queue
            .transition(task.id(), TaskAction::BypassReview)
            .unwrap();
        let dagq::domain::ClaimOutcome::Claimed { run } = queue
            .claim_for_supervisor(&repository.main_head().unwrap(), &LeaseToken::new("gone"))
            .unwrap()
        else {
            panic!()
        };
        run
    };
    let awaiting = claim("awaiting");
    let parked = claim("parked");
    let orphan = claim("orphan");
    let raw = Connection::open(&fixture.location.db).unwrap();
    raw.execute(
        "UPDATE task_runs SET status='awaiting_integration' WHERE id=?1",
        [&awaiting.id()],
    )
    .unwrap();
    raw.execute(
        "UPDATE task_runs SET status='needs_session', last_error='rebase conflicted' WHERE id=?1",
        [&parked.id()],
    )
    .unwrap();
    raw.execute(
        "DELETE FROM run_leases WHERE run_id IN (?1, ?2)",
        [&awaiting.id(), &parked.id()],
    )
    .unwrap();
    // The orphan keeps a lease whose owner is dead.
    let dead = dead_pid();
    raw.execute(
        "UPDATE run_leases SET pid=?2 WHERE run_id=?1",
        rusqlite::params![orphan.id(), dead],
    )
    .unwrap();
    drop(raw);

    let processes = FakeProcesses::default();
    processes.dead.lock().unwrap().insert(dead);
    let report = up(
        &fixture,
        &FakeLaunchd::new(&fixture.location.db),
        &processes,
    );
    assert_eq!(
        report["doctor"]["awaiting_integration"],
        json!([{"run_id": awaiting.id(), "task_id": awaiting.task_id(), "last_error": null}])
    );
    assert_eq!(
        report["doctor"]["needs_session"],
        json!([{"run_id": parked.id(), "task_id": parked.task_id(), "last_error": "rebase conflicted"}])
    );
    assert_eq!(
        report["doctor"]["unfinished_runs"],
        json!([{"run_id": orphan.id(), "task_id": orphan.task_id(), "status": "claimed", "lease_stale": true}])
    );
}

/// A workspace the queue recorded for a role `up` no longer opens (the
/// inbox's an earlier `up` opened, ADR-t2159-1 decision 6; the maintainer
/// ADR-0024 retired, the resident planner ADR-0041 decision 6 retired) is
/// forgotten by `up` without cmux; the workspaces themselves are left open
/// for a person to close. Only the supervisor's record of the retired
/// in-cmux mode is kept, for `down`.
#[test]
fn up_forgets_the_inbox_and_the_retired_workspaces_and_opens_no_session() {
    let mut fixture = fixture();
    fixture.environment.role = Some("worker".into());
    fixture.environment.queue = Some(fixture.location.db.clone());
    let raw = Connection::open(&fixture.location.db).unwrap();
    raw.execute(
        "INSERT INTO session_workspaces(role,workspace_id) VALUES ('retired',?1),('planner',?2),
             ('inbox',?3),('supervisor',?4)",
        [
            "01234567-89ab-4def-8123-0000000000ee",
            "01234567-89ab-4def-8123-0000000000ef",
            "01234567-89ab-4def-8123-0000000000f0",
            "01234567-89ab-4def-8123-0000000000f1",
        ],
    )
    .unwrap();
    drop(raw);
    let launchd = FakeLaunchd::new(&fixture.location.db);
    let processes = FakeProcesses::default();
    let report = up(&fixture, &launchd, &processes);
    assert_eq!(report["supervisor"]["outcome"], "started");
    assert_eq!(report["inbox"]["outcome"], "not_opened", "{report}");
    assert_eq!(report.get("planner"), None, "{report}");
    assert_eq!(report["retired_sessions"], 3, "{report}");
    let raw = Connection::open(&fixture.location.db).unwrap();
    let roles: Vec<String> = raw
        .prepare("SELECT role FROM session_workspaces ORDER BY role")
        .unwrap()
        .query_map([], |row| row.get(0))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap();
    assert_eq!(roles, ["supervisor"]);
    // Forgetting is idempotent.
    assert_eq!(
        SqliteQueue::open(&fixture.location.db)
            .unwrap()
            .forget_retired_session_workspaces()
            .unwrap(),
        0
    );
    let second = up(&fixture, &launchd, &processes);
    assert_eq!(second["retired_sessions"], 0, "{second}");
}

/// `up` opens no inbox from any session, the inbox's own of this queue or
/// of another included, nor a planner: it says to open the inbox with
/// `dagq inbox`. One fixture: the first `up` starts the supervisor and the
/// others reuse it, each from another session's environment.
#[test]
fn up_opens_no_inbox_from_any_session() {
    let mut fixture = fixture();
    let launchd = FakeLaunchd::new(&fixture.location.db);
    let processes = FakeProcesses::default();
    let this = fixture.location.db.clone();
    let other = fixture._dir.path().join("elsewhere.db");
    for (role, queue) in [
        (INBOX_ROLE, this.clone()),
        (INBOX_ROLE, other),
        (PLANNER_ROLE, this),
    ] {
        fixture.environment.role = Some(role.into());
        fixture.environment.queue = Some(queue.clone());
        let report = up(&fixture, &launchd, &processes);
        assert_eq!(
            report["inbox"],
            json!({"outcome": "not_opened", "next": lifecycle::INBOX_NEXT}),
            "{role} {queue:?}: {report}"
        );
        assert_eq!(report.get("planner"), None, "{report}");
    }
    assert_eq!(launchd.installs.lock().unwrap().len(), 1);
    let queue = SqliteQueue::open(&fixture.location.db).unwrap();
    assert_eq!(queue.session_workspace(SessionRole::Inbox).unwrap(), None);
    assert!(queue.latest_event_of("inbox_opened").unwrap().is_none());
}

/// `up` and `down` record what they did through tracing (ADR-0033
/// decision 2, task 255): one record of the command's target each, with
/// the outcome and the report, and a failed `up` with its error.
#[test]
fn up_and_down_record_what_they_did_through_tracing() {
    use dagq::infrastructure::telemetry::Telemetry;
    let fixture = fixture();
    let launchd = FakeLaunchd::new(&fixture.location.db);
    let processes = FakeProcesses::default();
    let (telemetry, captured) = Telemetry::capture();
    // With one dispatcher registered, tracing asks the default of the
    // thread that first reaches a callsite; another test reaching the
    // command's callsite first, outside any scope, would switch it off for
    // good. A second one makes every callsite ask both.
    let _second = tracing::Dispatch::new(tracing_subscriber::registry());
    tracing::callsite::rebuild_interest_cache();
    let (first, report) = telemetry.in_scope(|| {
        let first = up(&fixture, &launchd, &processes);
        processes
            .dead
            .lock()
            .unwrap()
            .insert(first["supervisor"]["pid"].as_u64().unwrap() as u32);
        let report = down(&fixture, &launchd, &processes, false, false);
        let mut options = fixture.options.clone();
        options.claude = fixture._dir.path().join("missing-claude");
        lifecycle::up(
            &fixture.location,
            &fixture.repo,
            &launchd,
            &processes,
            &fixture.environment,
            &options,
        )
        .unwrap_err();
        (first, report)
    });
    let records: Vec<Value> = captured
        .records()
        .into_iter()
        .filter(|record| record["target"] == lifecycle::COMMAND_TARGET)
        .collect();
    assert_eq!(records.len(), 3, "{records:?}");
    assert_eq!(records[0]["level"], "INFO");
    assert_eq!(records[0]["message"], "dagq up finished: started");
    assert_eq!(records[0]["fields"]["command"], "up");
    assert_eq!(records[0]["fields"]["outcome"], "started");
    assert_eq!(records[0]["fields"]["inbox"], "not_opened");
    let traced: Value =
        serde_json::from_str(records[0]["fields"]["report"].as_str().unwrap()).unwrap();
    assert_eq!(traced, first);
    assert_eq!(records[1]["message"], "dagq down finished: not_running");
    assert_eq!(records[1]["fields"]["command"], "down");
    let traced: Value =
        serde_json::from_str(records[1]["fields"]["report"].as_str().unwrap()).unwrap();
    assert_eq!(traced, report);
    assert_eq!(records[2]["level"], "WARN");
    assert_eq!(records[2]["fields"]["command"], "up");
    let error = records[2]["fields"]["error"].as_str().unwrap();
    assert!(error.contains("missing-claude"), "{error}");
    assert!(
        records[2]["message"]
            .as_str()
            .unwrap()
            .starts_with("dagq up failed: "),
        "{records:?}"
    );
}

/// `up` requires Claude Code and an initialized queue, and no cmux: the
/// fixture's PATH and `--cmux` name none, and the supervisor starts.
#[test]
fn up_requires_claude_and_an_initialized_queue_but_no_cmux() {
    let fixture = fixture();
    assert!(!fixture.options.cmux.exists());
    let launchd = FakeLaunchd::new(&fixture.location.db);
    let processes = FakeProcesses::default();
    let mut options = fixture.options.clone();
    options.claude = fixture._dir.path().join("missing-claude");
    let error = lifecycle::up(
        &fixture.location,
        &fixture.repo,
        &launchd,
        &processes,
        &fixture.environment,
        &options,
    )
    .unwrap_err();
    assert!(format!("{error:#}").contains("missing-claude"), "{error:#}");
    let uninitialized =
        QueueLocation::explicit_in(&fixture._dir.path().join("nowhere.db"), fixture._dir.path());
    let error = lifecycle::up(
        &uninitialized,
        &fixture.repo,
        &launchd,
        &processes,
        &fixture.environment,
        &fixture.options,
    )
    .unwrap_err();
    assert!(format!("{error:#}").contains("queue must already be initialized"));
    assert!(launchd.installs.lock().unwrap().is_empty());

    let report = up(&fixture, &launchd, &processes);
    assert_eq!(report["supervisor"]["outcome"], "started", "{report}");
    assert_eq!(report["inbox"]["outcome"], "not_opened", "{report}");
}

/// `up --max-load` reaches the supervisor it starts (task 577): a given
/// value, 0 (the hold off) included, is passed as `supervise --max-load`,
/// and none is left out so the supervisor resolves its default from the
/// host's cores (task 623).
#[test]
fn up_passes_max_load_to_the_supervisor_only_when_given() {
    // launchd mode: the agent definition's arguments.
    let launchd_arguments = |max_load: Option<f64>| {
        let mut fixture = fixture();
        fixture.options.max_load = max_load;
        let launchd = FakeLaunchd::new(&fixture.location.db);
        up(&fixture, &launchd, &FakeProcesses::default());
        let installs = launchd.installs.lock().unwrap();
        installs[0].2.clone()
    };
    let contents = launchd_arguments(Some(0.0));
    assert!(
        contents.contains("\t\t<string>--max-load</string>\n\t\t<string>0</string>\n"),
        "{contents}"
    );
    let contents = launchd_arguments(Some(8.5));
    assert!(
        contents.contains("\t\t<string>--max-load</string>\n\t\t<string>8.5</string>\n"),
        "{contents}"
    );
    // Below 0 is the hold off too, passed as 0 so it does not read as a flag.
    let contents = launchd_arguments(Some(-1.0));
    assert!(
        contents.contains("\t\t<string>--max-load</string>\n\t\t<string>0</string>\n"),
        "{contents}"
    );
    let contents = launchd_arguments(None);
    assert!(!contents.contains("--max-load"), "{contents}");
}

/// `up --parallel` and `--max-waiting` reach the supervisor it starts only
/// when given (task 698): one not given is left out, so the
/// supervisor follows `[supervisor]` of `dagq.toml` instead of a default
/// baked into its arguments.
#[test]
fn up_passes_parallel_and_max_waiting_only_when_given() {
    let launchd_arguments = |parallel: Option<u16>, max_waiting: Option<u16>| {
        let mut fixture = fixture();
        fixture.options.parallel = parallel;
        fixture.options.max_waiting = max_waiting;
        let launchd = FakeLaunchd::new(&fixture.location.db);
        up(&fixture, &launchd, &FakeProcesses::default());
        let installs = launchd.installs.lock().unwrap();
        installs[0].2.clone()
    };
    let contents = launchd_arguments(None, None);
    assert!(!contents.contains("--parallel"), "{contents}");
    assert!(!contents.contains("--max-waiting"), "{contents}");
    let contents = launchd_arguments(Some(4), Some(0));
    assert!(
        contents.contains("\t\t<string>--parallel</string>\n\t\t<string>4</string>\n"),
        "{contents}"
    );
    assert!(
        contents.contains("\t\t<string>--max-waiting</string>\n\t\t<string>0</string>\n"),
        "{contents}"
    );
}

/// `up --runtime-planners` reaches the supervisor it starts only when given
/// (task 941), as `--parallel` does.
#[test]
fn up_passes_runtime_planners_only_when_given() {
    let launchd_arguments = |runtime_planners: Option<u16>| {
        let mut fixture = fixture();
        fixture.options.runtime_planners = runtime_planners;
        let launchd = FakeLaunchd::new(&fixture.location.db);
        up(&fixture, &launchd, &FakeProcesses::default());
        let installs = launchd.installs.lock().unwrap();
        installs[0].2.clone()
    };
    let contents = launchd_arguments(None);
    assert!(!contents.contains("--runtime-planners"), "{contents}");
    let contents = launchd_arguments(Some(2));
    assert!(
        contents.contains("\t\t<string>--runtime-planners</string>\n\t\t<string>2</string>\n"),
        "{contents}"
    );
}

#[test]
fn no_claude_up_needs_no_claude_plugin_or_trust_and_opens_no_inbox() {
    let mut fixture = fixture();
    fixture.options.no_claude = true;
    fixture.options.claude = fixture.repo.join("missing-claude");
    fixture.options.plugin_dir = None;
    fixture.environment.claude_config = None;
    let launchd = FakeLaunchd::new(&fixture.location.db);
    let report = up(&fixture, &launchd, &FakeProcesses::default());
    assert_eq!(report["supervisor"]["outcome"], "started");
    assert_eq!(report["inbox"]["outcome"], "skipped");
    assert_eq!(report["inbox"]["reason"], "provider_disabled");
    assert!(
        launchd.installs.lock().unwrap()[0]
            .2
            .contains("--no-claude")
    );
}

#[test]
fn no_claude_up_refuses_to_reuse_a_supervisor_with_another_policy() {
    let mut fixture = fixture();
    let launchd = FakeLaunchd::new(&fixture.location.db);
    let processes = FakeProcesses::default();
    up(&fixture, &launchd, &processes);
    fixture.options.no_claude = true;
    let error = format!("{:#}", try_up(&fixture, &launchd, &processes).unwrap_err());
    assert!(error.contains("different --no-claude policy"), "{error}");
    assert_eq!(launchd.installs.lock().unwrap().len(), 1);
}

/// ADR-t2079-1: `up` given Claude Code and Codex as their links (to the
/// versions their updates install) registers the supervisor with the
/// links, never the versions they point at, which an update removes.
#[test]
fn up_registers_the_supervisor_with_the_provider_links_not_their_versions() {
    let mut fixture = fixture();
    let versions = fixture._dir.path().join("share/claude/versions");
    fs::create_dir_all(&versions).unwrap();
    fs::copy(&fixture.options.claude, versions.join("2.2.0")).unwrap();
    let codex_version = fixture._dir.path().join("codex/releases/0.2.0/bin/codex");
    fs::create_dir_all(codex_version.parent().unwrap()).unwrap();
    common::template::script(&codex_version, "#!/bin/sh\nprintf 'codex-cli 0.2.0\\n'\n");
    let bin = fixture._dir.path().join("bin");
    fs::create_dir_all(&bin).unwrap();
    std::os::unix::fs::symlink(versions.join("2.2.0"), bin.join("claude")).unwrap();
    std::os::unix::fs::symlink(&codex_version, bin.join("codex")).unwrap();
    // As the command resolves what it is given (`src/main.rs`).
    fixture.options.claude =
        dagq::infrastructure::adapters::claude_at_entry(&bin.join("claude")).unwrap();
    fixture.options.codex =
        dagq::infrastructure::codex::codex_at_entry(&bin.join("codex")).unwrap();
    assert_eq!(fixture.options.claude, bin.join("claude"));
    assert_eq!(fixture.options.codex, bin.join("codex"));
    let launchd = FakeLaunchd::new(&fixture.location.db);
    let processes = FakeProcesses::default();
    let report = up(&fixture, &launchd, &processes);
    assert_eq!(report["supervisor"]["outcome"], "started", "{report}");
    let installs = launchd.installs.lock().unwrap();
    let contents = &installs[0].2;
    for (flag, link) in [
        ("--claude", bin.join("claude")),
        ("--codex", bin.join("codex")),
    ] {
        assert!(
            contents.contains(&format!(
                "<string>{flag}</string>\n\t\t<string>{}</string>",
                link.display()
            )),
            "{contents}"
        );
    }
    assert!(!contents.contains("versions/"), "{contents}");
    assert!(!contents.contains("releases/"), "{contents}");
}
